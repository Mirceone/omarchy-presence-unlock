//! Installing and removing the presence unlock integration.
//!
//! The integration is a clone of Omarchy's own lock plugin with presence
//! authentication added, enabled in place of the built-in one. See [`lock`]
//! for why a clone, and for the anchors that generate it.
//!
//! Each piece owns one surface: [`lock`] the plugin, [`binding`] the Hyprland
//! gesture, [`menu`] the Omarchy menu row, [`service`] the daemon's units and
//! the PAM policy they answer for, and [`paths`] the locations they share.

mod binding;
mod lock;
mod menu;
mod paths;
mod service;
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
/// Ordered so a machine is never left half integrated. The clone is rendered
/// before anything is written, because an Omarchy release this version cannot
/// patch must leave the session exactly as it was found.
///
/// # Errors
///
/// Returns an error when the integration cannot be applied.
pub fn apply() -> Result<(), String> {
    let plugin_id = lock::plugin_id()?;
    service::policy_installed()?;
    let clone = lock::render_clone(&plugin_id)?;

    lock::install(&plugin_id, &clone)?;
    binding::install()?;
    menu::install()?;
    binding::reload_hyprland()?;

    // After enabling, so a first install does not restart a shell that is
    // about to load the plugin anyway.
    if let Err(error) = lock::reload_code(&clone) {
        eprintln!("warning: could not reload the Omarchy shell: {error}");
    }
    // Best-effort: a dev checkout with no installed units must stay able to
    // run setup, and a failure here costs the service, not the lock
    // integration that has already landed.
    if let Err(error) = service::enable() {
        eprintln!("warning: could not arm the presence service: {error}");
    }

    println!(
        "Enabled {plugin_id}, a clone of {} with presence unlock, and installed the Alt hold binding.",
        lock::STOCK_PLUGIN_ID
    );
    Ok(())
}
