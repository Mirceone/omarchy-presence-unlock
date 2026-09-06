//! Installing and removing the presence companion for Omarchy's stock lock.

mod quattro;
mod teardown;

pub use teardown::{Enrollment, Step, uninstall};

use std::process::Command;

fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command exited with {status}"))
    }
}
/// Applies the desktop-side integration for the current user.
///
/// Unprivileged by design: the system half — the binaries, the PAM module and
/// its policy — belongs to whatever installed them, so everything here is work
/// this user owns in their own session.
///
/// # Errors
///
/// Returns an error when the integration cannot be applied.
pub fn apply() -> Result<(), String> {
    quattro::setup()
}
