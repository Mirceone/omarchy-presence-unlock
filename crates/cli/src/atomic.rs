use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg},
};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

const LOCK_WAIT: Duration = Duration::from_secs(1);
const LOCK_RETRY: Duration = Duration::from_millis(25);

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replaces `path` through a unique sibling temp file and a rename, so a crash
/// mid-write can never leave a truncated file behind. The temp file is created
/// with `mode`, before its contents are written or it is renamed.
///
/// # Errors
///
/// Returns a rendered I/O error when staging, writing, chmod, or rename fails.
pub fn write_atomic(path: &Path, contents: &str, mode: u32) -> Result<(), String> {
    write_atomic_checked(path, contents, mode, || Ok(()))
}

fn write_atomic_checked(
    path: &Path,
    contents: &str,
    mode: u32,
    check: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let parent = path.parent().ok_or("invalid config path")?;
    let name = path.file_name().ok_or("invalid config path")?;
    let temporary = (0..128)
        .map(|_| {
            parent.join(format!(
                ".{}.tmp-{}-{}",
                name.to_string_lossy(),
                std::process::id(),
                TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed)
            ))
        })
        .find_map(|candidate| {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(mode)
                .open(&candidate)
            {
                Ok(file) => Some(Ok((candidate, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error.to_string())),
            }
        })
        .ok_or_else(|| {
            format!(
                "cannot create a unique temporary file beside {}",
                path.display()
            )
        })??;
    let (temporary, mut file) = temporary;
    let result = (|| {
        file.write_all(contents.as_bytes())
            .map_err(|error| error.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| error.to_string())?;
        drop(file);
        check()?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

#[derive(Debug, PartialEq, Eq)]
enum Snapshot {
    Missing,
    File {
        device: u64,
        inode: u64,
        contents: String,
    },
}

fn snapshot(path: &Path) -> Result<Snapshot, String> {
    match fs::File::open(path) {
        Ok(mut file) => {
            let mut contents = String::new();
            file.read_to_string(&mut contents)
                .map_err(|error| error.to_string())?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            Ok(Snapshot::File {
                device: metadata.dev(),
                inode: metadata.ino(),
                contents,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Snapshot::Missing),
        Err(error) => Err(error.to_string()),
    }
}

/// Serializes cooperating user-file updates, then verifies the target still has
/// the identity and content used for rendering before replacing it.
///
/// The final comparison and rename cannot be one atomic compare-and-swap against
/// uncooperative editors; such an editor can still race after the comparison.
///
/// `missing` is treated as the current source without creating a file unless
/// `render` changes it.
///
/// # Errors
///
/// Returns an actionable conflict when an external edit occurs during rendering,
/// or an error when the lock cannot be obtained promptly, reading, rendering,
/// and writing the file fails.
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

    let before = snapshot(path)?;
    let source = match &before {
        Snapshot::Missing => missing,
        Snapshot::File { contents, .. } => contents,
    };
    let rendered = render(source)?;

    if rendered != source {
        write_atomic_checked(path, &rendered, mode, || {
            if snapshot(path)? != before {
                return Err(format!(
                    "{} changed while rendering; retry the command without overwriting that edit",
                    path.display()
                ));
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "opu-atomic-{label}-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

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
    fn update_atomic_writes_a_unchanged_source() {
        let dir = test_dir("update");
        let target = dir.join("bindings.lua");
        fs::write(&target, "original\n").unwrap();

        update_atomic(&target, "", 0o644, |source| {
            assert_eq!(source, "original\n");
            Ok("managed\n".to_string())
        })
        .unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "managed\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn update_atomic_preserves_an_external_same_content_replacement() {
        let dir = test_dir("replace");
        let target = dir.join("bindings.lua");
        fs::write(&target, "original\n").unwrap();
        let replacement = dir.join("replacement.lua");

        let error = update_atomic(&target, "", 0o644, |_| {
            fs::write(&replacement, "original\n").unwrap();
            fs::rename(&replacement, &target).unwrap();
            Ok("managed\n".to_string())
        })
        .unwrap_err();

        assert!(error.contains(&target.display().to_string()));
        assert!(error.contains("changed while rendering"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn update_atomic_preserves_an_external_in_place_edit() {
        let dir = test_dir("edit");
        let target = dir.join("bindings.lua");
        fs::write(&target, "original\n").unwrap();

        let error = update_atomic(&target, "", 0o644, |_| {
            fs::write(&target, "external edit\n").unwrap();
            Ok("managed\n".to_string())
        })
        .unwrap_err();

        assert!(error.contains(&target.display().to_string()));
        assert_eq!(fs::read_to_string(&target).unwrap(), "external edit\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn update_atomic_preserves_an_externally_created_missing_target() {
        let dir = test_dir("create");
        let target = dir.join("bindings.lua");

        let error = update_atomic(&target, "missing\n", 0o644, |_| {
            fs::write(&target, "external create\n").unwrap();
            Ok("managed\n".to_string())
        })
        .unwrap_err();

        assert!(error.contains(&target.display().to_string()));
        assert_eq!(fs::read_to_string(&target).unwrap(), "external create\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn update_atomic_preserves_an_externally_removed_target() {
        let dir = test_dir("remove");
        let target = dir.join("bindings.lua");
        fs::write(&target, "original\n").unwrap();

        let error = update_atomic(&target, "", 0o644, |_| {
            fs::remove_file(&target).unwrap();
            Ok("managed\n".to_string())
        })
        .unwrap_err();

        assert!(error.contains(&target.display().to_string()));
        assert!(!target.exists());
        fs::remove_dir_all(dir).unwrap();
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
