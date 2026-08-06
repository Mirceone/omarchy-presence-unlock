use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

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
}
