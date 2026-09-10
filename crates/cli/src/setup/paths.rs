//! The user-owned locations the integration writes to.
//!
//! Gathered in one place because setup and teardown must agree on every one
//! of them: a path that only removal knows about is a file left behind, and
//! one that only setup knows about is a file nothing can clean up.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub(super) fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

pub(super) fn plugins_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/omarchy/plugins"))
}

/// Where the presence lock clone lives, named for this user.
pub(super) fn plugin_dir() -> Result<PathBuf, String> {
    Ok(plugins_dir()?.join(super::lock::plugin_id()?))
}

pub(super) fn bindings_path() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/hypr/bindings.lua"))
}

pub(super) fn menu_extensions_path() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/omarchy/extensions/omarchy-menu.jsonc"))
}

pub(super) fn state_dir() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_STATE_HOME") {
        Some(path) => PathBuf::from(path),
        None => home_dir()?.join(".local/state"),
    };
    Ok(base.join("omarchy-presence-unlock"))
}

/// Whether anything is at `path`, including a symlink with no target.
pub(super) fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

pub(super) fn remove(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path).map_err(|error| error.to_string())
    } else if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(|error| error.to_string())
    } else {
        Err(format!(
            "unsupported filesystem entry at {}",
            path.display()
        ))
    }
}

/// Removes a path that may legitimately already be gone.
pub(super) fn remove_if_present(path: &Path) -> Result<(), String> {
    if exists(path) { remove(path) } else { Ok(()) }
}
