//! The control socket: the only way anything outside the daemon reaches policy.

use crate::{
    clock::boottime_ms,
    unlock::{UnlockError, Unlocker},
};
use nix::unistd::Uid;
use omarchy_watch_unlock_protocol::{Decision, Fleet, wire};
use std::{fs, io, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Semaphore},
};

/// The longest valid request is 10 bytes; anything larger is not a client of ours.
const MAX_REQUEST_BYTES: u64 = 64;
/// Concurrent control-socket connections. Over-limit clients are dropped, not queued.
const MAX_CONCURRENT_CLIENTS: usize = 8;

/// Everything a control-socket request may touch.
pub struct Service {
    pub fleet: Mutex<Fleet>,
    pub unlocker: Option<Arc<dyn Unlocker>>,
}

impl Service {
    #[must_use]
    pub fn new(fleet: Fleet, unlocker: Option<Arc<dyn Unlocker>>) -> Arc<Self> {
        Arc::new(Self {
            fleet: Mutex::new(fleet),
            unlocker,
        })
    }
}

/// # Errors
///
/// Returns socket binding, permissions, or accept errors.
pub async fn serve(socket: PathBuf, service: Arc<Service>) -> io::Result<()> {
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
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
        wire::REQ_CONFIRM => confirm(service).await,
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

/// Authorises, consumes the authorisation, then releases the lock screen.
///
/// The lock-screen check comes first so a confirmation sent while nothing is
/// locked does not burn the authorisation the user is about to need.
async fn confirm(service: &Service) -> String {
    let Some(unlocker) = service.unlocker.clone() else {
        return wire::deny(wire::DENY_BACKEND);
    };
    if unlocker.locked() == Some(false) {
        return wire::deny(wire::DENY_NOT_LOCKED);
    }
    if !service.fleet.lock().await.take_authorization(boottime_ms()) {
        return wire::deny(wire::DENY_NOT_ELIGIBLE);
    }
    // A command backend forks and waits; never on the reactor.
    match tokio::task::spawn_blocking(move || unlocker.unlock()).await {
        Ok(Ok(())) => wire::RESP_ALLOW.into(),
        Ok(Err(UnlockError::NotLocked)) => wire::deny(wire::DENY_NOT_LOCKED),
        Ok(Err(error)) => {
            eprintln!("unlock failed: {error}");
            wire::deny(wire::DENY_UNLOCK_FAILED)
        }
        Err(error) => {
            eprintln!("unlock task panicked: {error}");
            wire::deny(wire::DENY_UNLOCK_FAILED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omarchy_watch_unlock_protocol::{
        Advertisement, DeviceSpec, Identity, Policy, Quorum, profile::PRESENCE,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Recorder {
        locked: Option<bool>,
        calls: AtomicUsize,
    }

    impl Unlocker for Recorder {
        fn describe(&self) -> String {
            "recorder".into()
        }
        fn locked(&self) -> Option<bool> {
            self.locked
        }
        fn unlock(&self) -> Result<(), UnlockError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn fleet() -> Fleet {
        Fleet::new(
            vec![DeviceSpec {
                id: "fob".into(),
                identity: Identity::from_address([1, 2, 3, 4, 5, 6]),
                profile: PRESENCE,
                policy: Policy::default(),
            }],
            Quorum::Any,
        )
    }

    fn present(fleet: &mut Fleet) {
        let now = boottime_ms();
        fleet.observe(now, &Advertisement::new([1, 2, 3, 4, 5, 6], -50));
        fleet.observe(now + 100, &Advertisement::new([1, 2, 3, 4, 5, 6], -50));
    }

    #[tokio::test]
    async fn an_unknown_request_is_a_protocol_denial() {
        let service = Service::new(fleet(), None);
        assert_eq!(
            respond("HELLO\n", &service).await,
            wire::deny(wire::DENY_PROTOCOL)
        );
    }

    #[tokio::test]
    async fn confirm_without_a_backend_is_refused() {
        let service = Service::new(fleet(), None);
        present(&mut *service.fleet.lock().await);
        assert_eq!(
            respond(wire::REQ_CONFIRM, &service).await,
            wire::deny(wire::DENY_BACKEND)
        );
    }

    #[tokio::test]
    async fn confirm_does_not_consume_an_authorization_when_nothing_is_locked() {
        let recorder = Arc::new(Recorder {
            locked: Some(false),
            calls: AtomicUsize::new(0),
        });
        let service = Service::new(fleet(), Some(recorder.clone()));
        present(&mut *service.fleet.lock().await);
        assert_eq!(
            respond(wire::REQ_CONFIRM, &service).await,
            wire::deny(wire::DENY_NOT_LOCKED)
        );
        assert_eq!(recorder.calls.load(Ordering::SeqCst), 0);
        // The authorisation survived, so the next confirmation can still use it.
        assert_eq!(respond(wire::REQ_CHECK, &service).await, wire::RESP_ALLOW);
    }

    #[tokio::test]
    async fn one_authorization_releases_exactly_one_lock_screen() {
        let recorder = Arc::new(Recorder {
            locked: Some(true),
            calls: AtomicUsize::new(0),
        });
        let service = Service::new(fleet(), Some(recorder.clone()));
        present(&mut *service.fleet.lock().await);
        assert_eq!(respond(wire::REQ_CONFIRM, &service).await, wire::RESP_ALLOW);
        assert_eq!(
            respond(wire::REQ_CONFIRM, &service).await,
            wire::deny(wire::DENY_NOT_ELIGIBLE)
        );
        assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn status_lists_every_device_then_the_aggregate() {
        let service = Service::new(fleet(), None);
        present(&mut *service.fleet.lock().await);
        let response = respond(wire::REQ_STATUS, &service).await;
        let lines: Vec<_> = response.lines().collect();
        assert_eq!(lines[0], "DEVICE fob presence ALLOW rssi=-50");
        assert_eq!(lines[1], "ALLOW");
        assert_eq!(lines[2], "END");
    }
}
