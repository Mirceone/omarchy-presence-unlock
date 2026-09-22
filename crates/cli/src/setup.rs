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
/// Ordered so a machine is never left half integrated. Everything that can
/// be checked up front is: the clone is rendered and the Hyprland error
/// baseline taken before anything is written. If a later step still fails on
/// a first install, the lock, binding and menu are removed again so the stock
/// lock is back in charge. A rerun over a working integration is not rolled
/// back, because that would remove what was already working.
///
/// # Errors
///
/// Returns an error when the integration cannot be applied.
pub fn apply() -> Result<(), String> {
    let plugin_id = lock::plugin_id()?;
    service::policy_installed()?;
    let clone = lock::render_clone(&plugin_id)?;
    let first_install = !lock::is_installed()?;
    let baseline = binding::config_errors()?;

    lock::install(&plugin_id, &clone)?;
    let integrated = binding::install()
        .and_then(|()| menu::install())
        .and_then(|()| binding::reload_hyprland(&baseline));
    if let Err(error) = integrated {
        if !first_install {
            return Err(error);
        }
        let undone = lock::remove()
            .and_then(|()| binding::remove())
            .and_then(|()| menu::remove())
            .and_then(|()| binding::reload_hyprland(&baseline));
        return Err(match undone {
            Ok(()) => format!("{error}\nSetup was undone; the stock lock is active."),
            Err(undo) => format!(
                "{error}\nUndoing setup also failed ({undo}); run `omarchy-presence-unlock uninstall`."
            ),
        });
    }

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
