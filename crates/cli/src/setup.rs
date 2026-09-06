//! Installing and removing the presence companion for Omarchy's stock lock.

mod quattro;
mod teardown;

pub use teardown::{Enrollment, Step, packaged, remaining_system_files, uninstall};

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
/// Returns an error when the required Quattro integration cannot be applied.
pub fn setup_omarchy() -> Result<(), String> {
    quattro::setup()
}
