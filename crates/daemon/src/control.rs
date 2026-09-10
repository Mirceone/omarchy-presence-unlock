//! The control socket: the only way anything outside the daemon reaches policy.

use crate::clock::boottime_ms;
use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg},
    unistd::Uid,
};
use omarchy_presence_unlock_protocol::{Decision, Fleet, wire};
use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Semaphore},
};

/// The longest valid request is 10 bytes; anything larger is not a client of ours.
const MAX_REQUEST_BYTES: u64 = 64;
/// Concurrent control-socket connections. Over-limit clients are dropped, not queued.
const MAX_CONCURRENT_CLIENTS: usize = 8;

/// Presence state shared by the scanner and control socket.
pub struct Service {
    pub fleet: Mutex<Fleet>,
}

impl Service {
    #[must_use]
    pub fn new(fleet: Fleet) -> Arc<Self> {
        Arc::new(Self {
            fleet: Mutex::new(fleet),
        })
    }
}

/// # Errors
///
/// Returns `AddrInUse` when another daemon holds the sibling lock or answers on
/// the endpoint, and socket binding, permissions, or accept errors otherwise.
pub async fn serve(socket: PathBuf, service: Arc<Service>) -> io::Result<()> {
    let mut lock_path = socket.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    let _lock = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock).map_err(|(_, error)| {
        if error == Errno::EWOULDBLOCK {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "another presence daemon is starting or serving {}",
                    socket.display()
                ),
            )
        } else {
            io::Error::other(format!("cannot lock {}: {error}", lock_path.display()))
        }
    })?;

    remove_stale_socket(&socket).await?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let clients = Arc::new(Semaphore::new(MAX_CONCURRENT_CLIENTS));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = Arc::clone(&clients).try_acquire_owned() else {
            eprintln!(
                "control socket: dropping connection, {MAX_CONCURRENT_CLIENTS} already in flight"
            );
            drop(stream);
            continue;
        };
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            if let Err(error) = answer(stream, service).await {
                eprintln!("control socket request failed: {error}");
            }
            drop(permit);
        });
    }
}

async fn remove_stale_socket(socket: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "refusing to replace non-socket endpoint {}",
                socket.display()
            ),
        ));
    }

    match tokio::time::timeout(Duration::from_millis(100), UnixStream::connect(socket)).await {
        Ok(Ok(_)) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another presence daemon is already serving {}",
                socket.display()
            ),
        )),
        Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
            fs::remove_file(socket)
        }
        Ok(Err(error)) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot determine whether {} is stale: {error}",
                socket.display()
            ),
        )),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "timed out probing {}; leaving it in place",
                socket.display()
            ),
        )),
    }
}

async fn answer(stream: UnixStream, service: Arc<Service>) -> io::Result<()> {
    if stream.peer_cred()?.uid() != Uid::current().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "client UID differs from daemon UID",
        ));
    }
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    // Bounded so a same-uid process cannot stream us to death within the timeout.
    let mut reader = BufReader::new(reader.take(MAX_REQUEST_BYTES));
    tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request timeout"))??;
    let response = respond(&line, &service).await;
    writer.write_all(response.as_bytes()).await
}

async fn respond(request: &str, service: &Service) -> String {
    match request {
        wire::REQ_CHECK => render(service.fleet.lock().await.check(boottime_ms())),
        wire::REQ_STATUS => status(service).await,
        _ => wire::deny(wire::DENY_PROTOCOL),
    }
}

fn render(decision: Decision) -> String {
    match decision {
        Decision::Allow => wire::RESP_ALLOW.into(),
        Decision::Deny(reason) => wire::deny(reason),
    }
}

/// A `DEVICE` line per configured device, then the aggregate, then `END`.
async fn status(service: &Service) -> String {
    let now = boottime_ms();
    let fleet = service.fleet.lock().await;
    let mut response = String::new();
    for device in fleet.report(now) {
        let decision = match device.decision {
            Decision::Allow => "ALLOW".to_string(),
            Decision::Deny(reason) => format!("DENY {reason}"),
        };
        response.push_str(&wire::device_status(
            device.id,
            device.profile.id(),
            &decision,
            device.rssi,
        ));
    }
    response.push_str(&render(fleet.check(now)));
    response.push_str(wire::RESP_END);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use omarchy_presence_unlock_protocol::{
        Advertisement, DeviceSpec, Identity, MultiDeviceAuth, Policy, profile::PRESENCE,
    };

    fn fleet() -> Fleet {
        Fleet::new(
            vec![DeviceSpec {
                id: "fob".into(),
                identity: Identity::from_address([1, 2, 3, 4, 5, 6]),
                profile: PRESENCE,
                policy: Policy::default(),
            }],
            MultiDeviceAuth::Any,
        )
    }

    fn present(fleet: &mut Fleet) {
        let now = boottime_ms();
        fleet.observe(now, &Advertisement::new([1, 2, 3, 4, 5, 6], -50));
        fleet.observe(now + 100, &Advertisement::new([1, 2, 3, 4, 5, 6], -50));
    }

    #[tokio::test]
    async fn an_unknown_request_is_a_protocol_denial() {
        let service = Service::new(fleet());
        assert_eq!(
            respond("HELLO\n", &service).await,
            wire::deny(wire::DENY_PROTOCOL)
        );
    }

    #[tokio::test]
    async fn check_reports_current_presence_without_consuming_it() {
        let service = Service::new(fleet());
        present(&mut *service.fleet.lock().await);
        assert_eq!(respond(wire::REQ_CHECK, &service).await, wire::RESP_ALLOW);
        assert_eq!(respond(wire::REQ_CHECK, &service).await, wire::RESP_ALLOW);
    }

    #[tokio::test]
    async fn status_lists_every_device_then_the_aggregate() {
        let service = Service::new(fleet());
        present(&mut *service.fleet.lock().await);
        let response = respond(wire::REQ_STATUS, &service).await;
        let lines: Vec<_> = response.lines().collect();
        assert_eq!(lines[0], "DEVICE fob presence ALLOW rssi=-50");
        assert_eq!(lines[1], "ALLOW");
        assert_eq!(lines[2], "END");
    }

    #[tokio::test]
    async fn stale_socket_cleanup_refuses_regular_files_and_symlinks() {
        let dir = std::env::temp_dir().join(format!(
            "opu-control-nonsocket-{}-{}",
            std::process::id(),
            boottime_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("control.sock");
        fs::write(&socket, "do not remove").unwrap();
        assert_eq!(
            remove_stale_socket(&socket).await.unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        assert_eq!(fs::read_to_string(&socket).unwrap(), "do not remove");

        let target = dir.join("target");
        fs::write(&target, "also do not remove").unwrap();
        fs::remove_file(&socket).unwrap();
        std::os::unix::fs::symlink(&target, &socket).unwrap();
        assert_eq!(
            remove_stale_socket(&socket).await.unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        assert!(
            fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn concurrent_stale_socket_startup_keeps_the_lock_for_the_server_lifetime() {
        let dir = std::env::temp_dir().join(format!(
            "opu-control-{}-{}",
            std::process::id(),
            boottime_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("control.sock");
        let stale = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(stale);

        let mut first = tokio::spawn(serve(socket.clone(), Service::new(fleet())));
        let mut second = tokio::spawn(serve(socket.clone(), Service::new(fleet())));
        let (first_lost, result) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = &mut first => (true, result),
                result = &mut second => (false, result),
            }
        })
        .await
        .expect("one concurrent starter must promptly lose the lock");
        assert_eq!(
            result.unwrap().unwrap_err().kind(),
            io::ErrorKind::AddrInUse,
            "the loser must not unlink the winner's endpoint"
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if UnixStream::connect(&socket).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the lock holder must serve the cleaned stale endpoint");

        fs::remove_file(&socket).unwrap();
        let blocked = serve(socket.clone(), Service::new(fleet())).await;
        assert_eq!(blocked.unwrap_err().kind(), io::ErrorKind::AddrInUse);

        let winner = if first_lost { &mut second } else { &mut first };
        winner.abort();
        let _ = winner.await;

        let replacement = tokio::spawn(serve(socket.clone(), Service::new(fleet())));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if UnixStream::connect(&socket).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropping the server must release its sibling lock");
        replacement.abort();
        let _ = replacement.await;
        fs::remove_dir_all(dir).unwrap();
    }
}
