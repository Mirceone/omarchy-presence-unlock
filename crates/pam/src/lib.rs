use nix::unistd::Uid;
use omarchy_watch_unlock_protocol::{paths, wire};
use pam::{PamHandle, PamModule, PamReturnCode, export_pam_module, get_user};
use std::{
    ffi::{CStr, c_uint},
    io::{BufRead, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    time::Duration,
};

struct WatchPam;
export_pam_module!(WatchPam);

impl PamModule for WatchPam {
    fn authenticate(handle: &PamHandle, _: Vec<&CStr>, _: c_uint) -> PamReturnCode {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| authenticate(*handle)));
        outcome.unwrap_or(PamReturnCode::Service_Err)
    }
}

fn authenticate(handle: PamHandle) -> PamReturnCode {
    let Ok(user) = get_user(&handle, None) else {
        return PamReturnCode::Authinfo_Unavail;
    };
    let uid = match nix::unistd::User::from_name(user) {
        Ok(Some(user)) => user.uid,
        _ => return PamReturnCode::Auth_Err,
    };
    if uid != Uid::effective() {
        return PamReturnCode::Auth_Err;
    }
    let path = paths::socket_path(uid.as_raw());
    let Ok(metadata) = std::fs::metadata(&path) else {
        return PamReturnCode::Authinfo_Unavail;
    };
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid.as_raw()
        || metadata.mode() & 0o077 != 0
    {
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
