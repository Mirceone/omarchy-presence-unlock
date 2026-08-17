//! Lock-screen integration for Omarchy's Quattro plugin.

mod quattro;

use std::process::Command;

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
/// Returns an error when Quattro integration is unavailable or cannot be applied.
pub fn setup_omarchy() -> Result<(), String> {
    let commands = Command::new("omarchy")
        .args(["commands", "--all"])
        .output()
        .map_err(|error| error.to_string())?;
    if String::from_utf8_lossy(&commands.stdout).contains("omarchy plugin clone") {
        return quattro::setup();
    }
    Err("this Omarchy build does not support the required Quattro lock-screen integration".into())
}
