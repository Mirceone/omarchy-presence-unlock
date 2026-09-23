use nix::{
    poll::{PollFd, PollFlags, PollTimeout, poll},
    unistd::Uid,
};
use omarchy_presence_unlock_protocol::{paths, wire};
use pam::{PamHandle, PamItemType, PamModule, PamReturnCode, export_pam_module, get_item};
use socket2::{Domain, SockAddr, Socket, Type};
use std::{
    ffi::{CStr, c_uint},
    fs::File,
    io::{ErrorKind, Read, Write},
    os::{
        fd::AsFd,
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::Path,
    time::{Duration, Instant},
};

/// The complete control-socket exchange is bounded so a stalled daemon cannot
/// wedge the lock-screen authentication conversation.
const EXCHANGE_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_REPLY_BYTES: usize = 64;
const PASSWD_PATH: &str = "/etc/passwd";

struct PresencePam;
export_pam_module!(PresencePam);

impl PamModule for PresencePam {
    fn authenticate(handle: &PamHandle, args: Vec<&CStr>, _: c_uint) -> PamReturnCode {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| authenticate(handle, &args)));
        outcome.unwrap_or(PamReturnCode::Service_Err)
    }
}

/// The handle is an opaque zero-sized type, so its address *is* the handle:
/// taking it by value hands PAM a pointer to our stack. Clippy cannot see that.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn authenticate(handle: &PamHandle, _args: &[&CStr]) -> PamReturnCode {
    // Quickshell runs PAM in a forked subprocess. Avoid NSS/getpwnam here:
    // glibc NSS state inherited from the multithreaded shell is not safe to
    // reuse after fork and has crashed inside libc. PAM_USER is checked against
    // the local, root-owned passwd file before choosing the euid's socket.
    let Some(user) = pam_user(handle) else {
        return PamReturnCode::Service_Err;
    };
    let uid = Uid::effective().as_raw();
    if !local_account_matches_euid(user, uid) {
        return PamReturnCode::Service_Err;
    }

    let path = paths::socket_path(uid);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return PamReturnCode::Authinfo_Unavail;
    };
    if !socket_metadata_is_safe(&metadata, uid) {
        return PamReturnCode::Authinfo_Unavail;
    }
    match exchange(&path) {
        Some(reply) => reply_result(reply),
        None => PamReturnCode::Authinfo_Unavail,
    }
}

/// `pam::get_item` exposes `PAM_USER` as an opaque C pointer. The pointer remains
/// valid for the PAM transaction, which outlives this synchronous call.
#[allow(unsafe_code)]
fn pam_user(handle: &PamHandle) -> Option<&CStr> {
    let item = get_item(handle, PamItemType::User).ok()?;
    let ptr = std::ptr::from_ref(item).cast::<std::ffi::c_char>();
    // SAFETY: PAM_USER is specified by Linux-PAM as a NUL-terminated C string.
    Some(unsafe { CStr::from_ptr(ptr) })
}

/// Checks the PAM principal against the local account database without NSS.
/// A malformed file, duplicate account name, root, or a uid mismatch fails
/// closed. The intentionally narrow contract is local Omarchy accounts only.
fn local_account_matches_euid(user: &CStr, euid: u32) -> bool {
    if euid == 0 {
        return false;
    }
    let Ok(mut file) = File::open(PASSWD_PATH) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !passwd_metadata_is_safe(&metadata) {
        return false;
    }
    let mut passwd = Vec::new();
    if file.read_to_end(&mut passwd).is_err() {
        return false;
    }
    account_uid(&passwd, user.to_bytes()) == Some(euid)
}

fn passwd_metadata_is_safe(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0
}

fn account_uid(passwd: &[u8], user: &[u8]) -> Option<u32> {
    let mut result = None;
    for line in passwd.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split(|byte| *byte == b':');
        let (
            Some(name),
            Some(_password),
            Some(uid),
            Some(_gid),
            Some(_gecos),
            Some(_home),
            Some(_shell),
        ) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        )
        else {
            return None;
        };
        if name.is_empty() || fields.next().is_some() {
            return None;
        }
        let Ok(uid) = std::str::from_utf8(uid).ok()?.parse::<u32>() else {
            return None;
        };
        if name == user && result.replace(uid).is_some() {
            return None;
        }
    }
    result
}

fn exchange(path: &Path) -> Option<Reply> {
    let deadline = Instant::now().checked_add(EXCHANGE_TIMEOUT)?;
    let mut stream = connect(path, deadline).ok()?;
    stream.set_nonblocking(true).ok()?;
    write_until_deadline(&mut stream, wire::REQ_CHECK.as_bytes(), deadline)?;
    read_reply_until_deadline(&mut stream, deadline)
}

/// `connect_timeout` puts `AF_UNIX` sockets into nonblocking mode and treats
/// Linux EAGAIN as `WouldBlock`, waiting with poll until this same deadline.
fn connect(path: &Path, deadline: Instant) -> std::io::Result<UnixStream> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect_timeout(&SockAddr::unix(path)?, remaining(deadline)?)?;
    Ok(std::os::fd::OwnedFd::from(socket).into())
}

fn write_until_deadline(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Option<()> {
    while !bytes.is_empty() {
        wait_for(stream, PollFlags::POLLOUT, deadline)?;
        match stream.write(bytes) {
            Ok(0) => return None,
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(_) => return None,
        }
    }
    Some(())
}

fn read_reply_until_deadline(stream: &mut UnixStream, deadline: Instant) -> Option<Reply> {
    let mut reply = [0; MAX_REPLY_BYTES];
    let mut used = 0;
    loop {
        wait_for(stream, PollFlags::POLLIN, deadline)?;
        match stream.read(&mut reply[used..]) {
            Ok(0) => return None,
            Ok(read) => {
                used += read;
                if let Some(newline) = reply[..used].iter().position(|byte| *byte == b'\n') {
                    return (newline + 1 == used)
                        .then(|| parse_reply(&reply[..used]))
                        .flatten();
                }
                if used == reply.len() {
                    return None;
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(_) => return None,
        }
    }
}

fn wait_for(stream: &UnixStream, interest: PollFlags, deadline: Instant) -> Option<()> {
    let mut fds = [PollFd::new(stream.as_fd(), interest)];
    let ready = poll(&mut fds, millis_until(deadline)?).ok()?;
    (ready > 0 && fds[0].revents()?.contains(interest)).then_some(())
}

fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            std::io::Error::new(ErrorKind::TimedOut, "presence socket exchange timed out")
        })
}

fn millis_until(deadline: Instant) -> Option<PollTimeout> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    let milliseconds = remaining.as_micros().div_ceil(1_000).max(1);
    PollTimeout::try_from(i32::try_from(milliseconds).ok()?).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    Allow,
    Deny,
}

fn parse_reply(reply: &[u8]) -> Option<Reply> {
    if reply == wire::RESP_ALLOW.as_bytes() {
        return Some(Reply::Allow);
    }
    let reason = reply.strip_prefix(b"DENY ")?.strip_suffix(b"\n")?;
    (!reason.is_empty() && reason.iter().all(u8::is_ascii_graphic)).then_some(Reply::Deny)
}

fn reply_result(reply: Reply) -> PamReturnCode {
    match reply {
        Reply::Allow => PamReturnCode::Success,
        Reply::Deny => PamReturnCode::Auth_Err,
    }
}

fn socket_metadata_is_safe(metadata: &std::fs::Metadata, uid: u32) -> bool {
    metadata.file_type().is_socket()
        && metadata.uid() == uid
        && metadata.mode().trailing_zeros() >= 6
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    #[test]
    fn real_pam_transaction_rejects_another_account_without_copying_the_handle() {
        // Opaque PAM handles must retain their address, even though the
        // binding's zero-sized type happens to implement Copy.
        let conversation = pam::ffi::pam_conv {
            conv: None,
            appdata_ptr: std::ptr::null_mut(),
        };
        let handle = pam::start("presence-unit-test", Some("root"), &conversation).unwrap();
        let result = PresencePam::authenticate(handle, Vec::new(), 0);
        pam::end(handle, result);
        assert_eq!(result, PamReturnCode::Service_Err);
    }

    #[test]
    fn accepts_only_private_sockets_owned_by_the_effective_user() {
        let dir = std::env::temp_dir().join(format!("opu-pam-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert!(socket_metadata_is_safe(
            &metadata,
            Uid::effective().as_raw()
        ));
        assert!(!socket_metadata_is_safe(
            &metadata,
            Uid::effective().as_raw().wrapping_add(1)
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(!socket_metadata_is_safe(
            &fs::metadata(&path).unwrap(),
            Uid::effective().as_raw()
        ));
        drop(listener);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn account_must_exactly_match_the_effective_uid() {
        let passwd = b"alice:x:1000:1000::/home/alice:/bin/sh\nbob:x:1000:1000::/home/bob:/bin/sh\nroot:x:0:0::/root:/bin/sh\n";
        assert_eq!(account_uid(passwd, b"alice"), Some(1000));
        assert_eq!(account_uid(passwd, b"bob"), Some(1000));
        assert_ne!(account_uid(passwd, b"bob"), Some(1001));
        assert_ne!(account_uid(passwd, b"root"), Some(1000));
        assert!(!local_account_matches_euid(c"root", 0));
    }

    #[test]
    fn malformed_or_ambiguous_passwd_records_fail_closed() {
        assert_eq!(
            account_uid(b"alice:x:not-a-uid:1::/:/bin/sh\n", b"alice"),
            None
        );
        assert_eq!(
            account_uid(
                b"alice:x:1000:1::/:/bin/sh\nalice:x:1000:1::/:/bin/sh\n",
                b"alice"
            ),
            None
        );
    }

    #[test]
    fn reply_mapping_accepts_allow_and_deny_but_rejects_malformed_or_overlong_lines() {
        assert_eq!(
            parse_reply(wire::RESP_ALLOW.as_bytes()).map(reply_result),
            Some(PamReturnCode::Success)
        );
        assert_eq!(
            parse_reply(b"DENY stale\n").map(reply_result),
            Some(PamReturnCode::Auth_Err)
        );
        assert_eq!(parse_reply(b"DENY \n"), None);
        assert_eq!(parse_reply(&[b'x'; MAX_REPLY_BYTES + 1]), None);
    }

    #[test]
    fn unterminated_and_slow_replies_fail_closed_within_one_deadline() {
        for response in [b"TOO-LONG\n".as_slice(), b"ALLOW".as_slice()] {
            let (path, server) = test_server(move |mut stream| {
                let mut request = [0; wire::REQ_CHECK.len()];
                stream.read_exact(&mut request).unwrap();
                stream.write_all(response).unwrap();
            });
            assert!(exchange(&path).is_none());
            server.join().unwrap();
            fs::remove_file(path).unwrap();
        }

        let (path, server) = test_server(|mut stream| {
            let mut request = [0; wire::REQ_CHECK.len()];
            stream.read_exact(&mut request).unwrap();
            let _ = stream.write_all(&[b'x'; MAX_REPLY_BYTES + 1]);
        });
        assert!(exchange(&path).is_none());
        server.join().unwrap();
        fs::remove_file(path).unwrap();

        let (path, server) = test_server(|mut stream| {
            let mut request = [0; wire::REQ_CHECK.len()];
            stream.read_exact(&mut request).unwrap();
            for byte in *b"ALLOW\n" {
                thread::sleep(EXCHANGE_TIMEOUT / 2);
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
            }
        });
        let started = Instant::now();
        assert!(exchange(&path).is_none());
        assert!(started.elapsed() < EXCHANGE_TIMEOUT * 3);
        server.join().unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn maps_complete_allow_and_deny_replies() {
        for (response, expected) in [
            (wire::RESP_ALLOW.as_bytes(), Reply::Allow),
            (b"DENY stale\n".as_slice(), Reply::Deny),
        ] {
            let (path, server) = test_server(move |mut stream| {
                let mut request = [0; wire::REQ_CHECK.len()];
                stream.read_exact(&mut request).unwrap();
                stream.write_all(response).unwrap();
            });
            assert_eq!(exchange(&path), Some(expected));
            server.join().unwrap();
            fs::remove_file(path).unwrap();
        }
    }

    fn test_server(
        handler: impl FnOnce(UnixStream) + Send + 'static,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let path =
            std::env::temp_dir().join(format!("opu-pam-{}-{}.sock", std::process::id(), uuid()));
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || handler(listener.accept().unwrap().0));
        (path, server)
    }

    fn uuid() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }
}
