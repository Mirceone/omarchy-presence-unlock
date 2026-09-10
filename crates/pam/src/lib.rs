use nix::unistd::Uid;
use omarchy_presence_unlock_protocol::{paths, wire};
use pam::{PamHandle, PamModule, PamReturnCode, export_pam_module};
use socket2::{Domain, SockAddr, Socket, Type};
use std::{
    ffi::{CStr, c_uint},
    io::{BufRead, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    path::Path,
    time::Duration,
};

/// Every blocking step gets the same budget. The module runs inside the lock
/// screen's authentication conversation, where a stall is indistinguishable
/// from a hung desktop.
const STEP_TIMEOUT: Duration = Duration::from_millis(100);

struct PresencePam;
export_pam_module!(PresencePam);

impl PamModule for PresencePam {
    fn authenticate(handle: &PamHandle, args: Vec<&CStr>, _: c_uint) -> PamReturnCode {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            authenticate(*handle, &args)
        }));
        outcome.unwrap_or(PamReturnCode::Service_Err)
    }
}

fn authenticate(_handle: PamHandle, args: &[&CStr]) -> PamReturnCode {
    // Quickshell runs PAM in a forked subprocess. Avoid NSS/getpwnam here:
    // glibc NSS state inherited from the multithreaded shell is not safe to
    // reuse after fork and has crashed inside libc. The socket is therefore
    // derived from the effective uid, which is only the account being
    // authenticated while the module runs unprivileged as that account.
    let uid = Uid::effective().as_raw();
    if !uid_is_authorised(uid, args) {
        return PamReturnCode::Service_Err;
    }
    let path = paths::socket_path(uid);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return PamReturnCode::Authinfo_Unavail;
    };
    if !socket_metadata_is_safe(&metadata, uid) {
        return PamReturnCode::Authinfo_Unavail;
    }
    let Ok(mut stream) = connect(&path) else {
        return PamReturnCode::Authinfo_Unavail;
    };
    let deadline = Some(STEP_TIMEOUT);
    if stream.set_read_timeout(deadline).is_err()
        || stream.set_write_timeout(deadline).is_err()
        || stream.write_all(wire::REQ_CHECK.as_bytes()).is_err()
    {
        return PamReturnCode::Authinfo_Unavail;
    }
    let mut reply = String::new();
    if std::io::BufReader::new(stream)
        .read_line(&mut reply)
        .is_err()
    {
        return PamReturnCode::Authinfo_Unavail;
    }
    reply_result(&reply)
}

/// Whether presence may speak for this uid.
///
/// The socket is chosen by effective uid, so a stack that runs the module with
/// an euid other than the account being authenticated would authorise the
/// wrong principal. Root never qualifies, and a stack that cannot guarantee
/// the euid states the expected one as `uid=N`.
fn uid_is_authorised(uid: u32, args: &[&CStr]) -> bool {
    if uid == 0 {
        return false;
    }
    args.iter()
        .filter_map(|arg| arg.to_str().ok()?.strip_prefix("uid="))
        .all(|expected| expected.parse() == Ok(uid))
}

/// `UnixStream::connect` blocks without bound, and a listen backlog saturated
/// by any same-uid process would otherwise wedge the conversation before the
/// read and write deadlines are ever set.
fn connect(path: &Path) -> std::io::Result<UnixStream> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect_timeout(&SockAddr::unix(path)?, STEP_TIMEOUT)?;
    Ok(std::os::fd::OwnedFd::from(socket).into())
}

fn reply_result(reply: &str) -> PamReturnCode {
    if !reply.ends_with('\n') {
        PamReturnCode::Authinfo_Unavail
    } else if reply == wire::RESP_ALLOW {
        PamReturnCode::Success
    } else {
        PamReturnCode::Auth_Err
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
    };

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
    fn requires_a_newline_terminated_reply_before_returning_a_verdict() {
        assert_eq!(reply_result(wire::RESP_ALLOW), PamReturnCode::Success);
        assert_eq!(
            reply_result(wire::RESP_ALLOW.strip_suffix('\n').unwrap()),
            PamReturnCode::Authinfo_Unavail
        );
        assert_eq!(reply_result("DENY\n"), PamReturnCode::Auth_Err);
    }

    #[test]
    fn presence_never_speaks_for_root_or_an_unexpected_account() {
        let uid = |text: &'static CStr| text;
        assert!(uid_is_authorised(1000, &[]));
        assert!(!uid_is_authorised(0, &[]));
        assert!(uid_is_authorised(1000, &[uid(c"uid=1000")]));
        assert!(!uid_is_authorised(1000, &[uid(c"uid=1001")]));
        assert!(!uid_is_authorised(1000, &[uid(c"uid=nonsense")]));
        assert!(!uid_is_authorised(0, &[uid(c"uid=0")]));
    }
}
