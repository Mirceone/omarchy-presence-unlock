use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg},
};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const LOCK_WAIT: Duration = Duration::from_secs(1);
const LOCK_RETRY: Duration = Duration::from_millis(25);

/// Replaces `path` through a sibling temp file and a rename, so a crash mid-write
/// can never leave a truncated file behind. `mode` is applied before the rename,
/// so the final path is never briefly world-readable.
///
/// # Errors
///
/// Returns a rendered I/O error when the write, chmod, or rename fails.
pub fn write_atomic(path: &Path, contents: &str, mode: u32) -> Result<(), String> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, contents).map_err(|error| error.to_string())?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
        .map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// Serializes a user-file update so rendering cannot overwrite an edit made
/// after the caller's initial read.
///
/// `missing` is treated as the current source without creating a file unless
/// `render` changes it.
///
/// # Errors
///
/// Returns an error when the lock cannot be obtained promptly, or reading,
/// rendering, and writing the file fails.
pub fn update_atomic(
    path: &Path,
    missing: &str,
    mode: u32,
    render: impl FnOnce(&str) -> Result<String, String>,
) -> Result<(), String> {
    fs::create_dir_all(path.parent().ok_or("invalid config path")?)
        .map_err(|error| error.to_string())?;
    let mut unlocked = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path(path))
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + LOCK_WAIT;
    let _lock = loop {
        match Flock::lock(unlocked, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => break lock,
            Err((file, Errno::EWOULDBLOCK)) if Instant::now() < deadline => {
                unlocked = file;
                thread::sleep(LOCK_RETRY);
            }
            Err((_, Errno::EWOULDBLOCK)) => {
                return Err(format!(
                    "timed out waiting to update {}; retry the command",
                    path.display()
                ));
            }
            Err((_, error)) => return Err(format!("cannot lock {}: {error}", path.display())),
        }
    };

    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing.to_string(),
        Err(error) => return Err(error.to_string()),
    };
    let rendered = render(&source)?;
    if rendered != source {
        write_atomic(path, &rendered, mode)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn replaces_content_and_applies_the_mode() {
        let dir = env::temp_dir().join(format!("opu-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("config.toml");
        fs::write(&target, "original\n").unwrap();

        write_atomic(&target, "replaced\n", 0o600).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "replaced\n");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the secret must never be group/world readable");
        assert!(
            !dir.join("config.toml.new").exists(),
            "the temp file must be renamed away, not left behind"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_a_file_that_does_not_exist_yet() {
        let dir = env::temp_dir().join(format!("opu-atomic-new-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("bindings.lua");

        write_atomic(&target, "o.bind()\n", 0o644).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "o.bind()\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn locked_update_times_out_without_overwriting_the_current_content() {
        let dir = env::temp_dir().join(format!("opu-atomic-lock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("bindings.lua");
        fs::write(&target, "user edit\n").unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path(&target))
            .unwrap();
        let _lock = Flock::lock(lock, FlockArg::LockExclusive).unwrap();
        let writer_path = target.clone();
        let writer = thread::spawn(move || {
            update_atomic(&writer_path, "", 0o644, |_| Ok("setup edit\n".to_string()))
        });
        let error = writer
            .join()
            .expect("the concurrent writer must not panic")
            .expect_err("a concurrent update must not overwrite a locked file");

        assert!(error.contains(&target.display().to_string()));
        assert_eq!(fs::read_to_string(&target).unwrap(), "user edit\n");

        fs::remove_dir_all(&dir).unwrap();
    }
}
