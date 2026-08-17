//! Lock-screen integration for Omarchy's Quattro plugin.

mod quattro;

use crate::atomic::write_atomic;
use std::{env, fs, io, path::PathBuf, process::Command};

fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command exited with {status}"))
    }
}

/// # Errors
///
/// Returns an error when neither integration is available, or when the chosen
/// integration cannot be applied.
pub fn setup_omarchy() -> Result<(), String> {
    remove_legacy_hyprlock_binding()?;
    let commands = Command::new("omarchy")
        .args(["commands", "--all"])
        .output()
        .map_err(|error| error.to_string())?;
    if String::from_utf8_lossy(&commands.stdout).contains("omarchy plugin clone") {
        return quattro::setup();
    }
    Err("this Omarchy build does not support the required Quattro lock-screen integration".into())
}

fn remove_legacy_hyprlock_binding() -> Result<(), String> {
    let bindings = home_dir()?.join(".config/hypr/bindings.lua");
    let binding_text = match fs::read_to_string(&bindings) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let updated = without_legacy_hyprlock_binding(&binding_text);
    if updated != binding_text {
        write_atomic(&bindings, &updated, 0o644)?;
    }
    Ok(())
}

fn without_legacy_hyprlock_binding(source: &str) -> String {
    source
        .replace(
            "\n-- omarchy-watch-unlock Alt+Enter confirmation.\no.bind(\"ALT + RETURN\", \"Watch unlock confirmation\", \"omarchy-watch-unlock confirm\", { locked = true })\n",
            "\n",
        )
        .replace(
            "\n-- omarchy-presence-unlock Alt+Enter confirmation\no.bind(\"ALT + RETURN\", \"Presence unlock confirmation\", \"omarchy-presence-unlock confirm\", { locked = true })\n",
            "\n",
        )
}

#[cfg(test)]
mod tests {
    use super::without_legacy_hyprlock_binding;

    #[test]
    fn removes_both_legacy_alt_enter_bindings() {
        let old = "\n-- omarchy-watch-unlock Alt+Enter confirmation.\no.bind(\"ALT + RETURN\", \"Watch unlock confirmation\", \"omarchy-watch-unlock confirm\", { locked = true })\n";
        let current = "\n-- omarchy-presence-unlock Alt+Enter confirmation\no.bind(\"ALT + RETURN\", \"Presence unlock confirmation\", \"omarchy-presence-unlock confirm\", { locked = true })\n";
        assert_eq!(
            without_legacy_hyprlock_binding(&format!("before{old}middle{current}after")),
            "before\nmiddle\nafter"
        );
    }
}
