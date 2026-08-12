use nix::unistd::Uid;
use omarchy_presence_unlock_protocol::{paths, wire};
use pam::{PamHandle, PamModule, PamReturnCode, export_pam_module};
use std::{
    ffi::{CStr, c_uint},
    io::{BufRead, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    time::Duration,
};

struct PresencePam;
export_pam_module!(PresencePam);

impl PamModule for PresencePam {
    fn authenticate(handle: &PamHandle, _: Vec<&CStr>, _: c_uint) -> PamReturnCode {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| authenticate(*handle)));
        outcome.unwrap_or(PamReturnCode::Service_Err)
    }
}

fn authenticate(_handle: PamHandle) -> PamReturnCode {
    // Quickshell runs PAM in a forked subprocess. Avoid NSS/getpwnam here:
    // glibc NSS state inherited from the multithreaded shell is not safe to
    // reuse after fork and has crashed inside libc. The daemon socket is
    // already derived from and strictly owned by this effective uid.
    let uid = Uid::effective();
    let path = paths::socket_path(uid.as_raw());
    let Ok(metadata) = std::fs::metadata(&path) else {
        return PamReturnCode::Authinfo_Unavail;
    };
    if !socket_metadata_is_safe(&metadata, uid.as_raw()) {
        return PamReturnCode::Authinfo_Unavail;
    }
    let Ok(mut stream) = UnixStream::connect(&path) else {
        return PamReturnCode::Authinfo_Unavail;
    };
    let deadline = Some(Duration::from_millis(100));
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
    if reply == wire::RESP_ALLOW {
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
}
