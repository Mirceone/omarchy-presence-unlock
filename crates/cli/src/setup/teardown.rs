//! Undoing the per-user half of the integration.
//!
//! Only the user's own half: the installed files are a package's, whichever
//! way they were installed, so `pacman -Rns` removes them. That is what keeps
//! this unprivileged and keeps one owner per file.
//!
//! Teardown is deliberately not atomic the way [`super::quattro::setup`] is.
//! Setup must leave the machine either integrated or untouched, because a half
//! integration is a lock screen that cannot authenticate. Removal is the
//! opposite: a step that fails must not strand the steps after it, or a user
//! trying to get rid of this is left with the parts that broke and no way to
//! reach the parts that would have worked. So every step runs, and each
//! reports for itself.

use super::quattro;
use omarchy_presence_unlock_protocol::paths;
use std::{path::Path, process::Command};

/// One thing removal attempted, in the order attempted.
pub struct Step {
    pub label: String,
    pub ok: bool,
    /// Why it failed, for the one line a screen can show about it.
    pub detail: Option<String>,
}

/// What removal is allowed to take with it.
#[derive(Clone, Copy)]
pub enum Enrollment {
    /// Leave `config.toml` alone, so reinstalling picks the devices back up.
    Keep,
    /// Delete the enrolled devices and their keys.
    Forget,
}

fn step(label: impl Into<String>, outcome: Result<(), String>) -> Step {
    match outcome {
        Ok(()) => Step {
            label: label.into(),
            ok: true,
            detail: None,
        },
        Err(detail) => Step {
            label: label.into(),
            ok: false,
            detail: Some(detail),
        },
    }
}

/// Removes a path that may legitimately already be gone.
fn remove_if_present(path: &Path) -> Result<(), String> {
    if quattro::path_exists(path) {
        quattro::remove_path(path)
    } else {
        Ok(())
    }
}

fn disable_plugin() -> Result<(), String> {
    // Disabling before the directory goes away lets the shell drop the service
    // cleanly; removing it first would leave a plugin id enabled in shell.json
    // that resolves to nothing.
    super::run(Command::new("omarchy").args(["plugin", "disable", quattro::PLUGIN_ID]))
}

fn remove_binding() -> Result<(), String> {
    let path = quattro::bindings_path()?;
    let Ok(source) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let (Some(start), Some(end)) = (
        source.find(quattro::BINDING_START),
        source.find(quattro::BINDING_END),
    ) else {
        return Ok(());
    };
    if start > end {
        return Err(format!(
            "{} contains a malformed presence-unlock block; remove it by hand",
            path.display()
        ));
    }
    let mut rendered = source.clone();
    rendered.replace_range(start..end + quattro::BINDING_END.len(), "");
    // The block was written with a blank line before it, which would otherwise
    // accumulate every time the integration is installed and removed.
    let trimmed = rendered.trim_end();
    crate::atomic::write_atomic(&path, &format!("{trimmed}\n"), 0o644)
}

/// Disables both units and stops what they started.
///
/// `disable --now` removes the enable symlinks under
/// `~/.config/systemd/user/`, which is the part of the service this user owns;
/// the unit files themselves live under `/usr` and are reported instead.
///
/// The path unit goes first and on its own: it starts the service whenever a
/// config appears, so disabling both in one invocation makes systemd warn that
/// it is stopping a service whose trigger is still armed.
fn stop_service() -> Result<(), String> {
    super::run(Command::new("systemctl").args(["--user", "disable", "--now", "presenced.path"]))?;
    super::run(Command::new("systemctl").args(["--user", "disable", "--now", "presenced.service"]))
}

/// Removes the runtime socket directory the daemon served from.
///
/// Stopping the daemon leaves the socket behind until the next reboot, and a
/// socket nobody serves is worse than none: the PAM module finds a path that
/// passes its ownership checks and then fails to connect, so a stale one turns
/// a removed feature into a connection error on every lock screen.
fn remove_socket() -> Result<(), String> {
    remove_if_present(&paths::current_socket_dir())
}

fn forget_enrollment() -> Result<(), String> {
    let config = paths::config_path().ok_or("XDG_CONFIG_HOME or HOME is required")?;
    remove_if_present(&config)
}

/// Undoes the integration, the service, and optionally the enrollment.
///
/// Every step is attempted whatever the ones before it did; the returned list
/// is what happened, in order.
pub fn uninstall(enrollment: Enrollment) -> Vec<Step> {
    let mut steps = Vec::new();

    steps.push(step(
        format!("Disabled the {} plugin", quattro::PLUGIN_ID),
        disable_plugin(),
    ));
    steps.push(step(
        "Removed the companion plugin",
        quattro::plugin_dir().and_then(|path| remove_if_present(&path)),
    ));
    steps.push(step(
        "Reloaded the Omarchy shell plugins",
        super::run(Command::new("omarchy-shell").args(["shell", "rescanPlugins"])),
    ));
    steps.push(step("Removed the Alt unlock binding", remove_binding()));
    steps.push(step("Reloaded Hyprland", quattro::reload_hyprland()));
    steps.push(step(
        "Disabled and stopped the presence service",
        stop_service(),
    ));
    steps.push(step("Removed the control socket", remove_socket()));
    steps.push(step(
        "Removed retained state",
        quattro::state_dir().and_then(|path| remove_if_present(&path)),
    ));
    steps.push(step(
        "Removed the obsolete update hook",
        quattro::obsolete_update_hook().and_then(|path| remove_if_present(&path)),
    ));
    if matches!(enrollment, Enrollment::Forget) {
        steps.push(step("Forgot the enrolled devices", forget_enrollment()));
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step that failed has to carry its reason, because the screen that
    /// shows it is the only place a user learns what to finish by hand.
    #[test]
    fn a_failed_step_reports_why_and_a_successful_one_does_not() {
        let failed = step("Removed the PAM policy", Err("permission denied".into()));
        assert!(!failed.ok);
        assert_eq!(failed.detail.as_deref(), Some("permission denied"));
        let passed = step("Removed the PAM policy", Ok(()));
        assert!(passed.ok);
        assert!(passed.detail.is_none());
    }

    /// Removal must not fail over work that was already done, or a second
    /// attempt after a partial removal could never succeed.
    #[test]
    fn removing_an_absent_path_succeeds() {
        let absent = std::env::temp_dir().join(format!("opu-absent-{}", std::process::id()));
        assert!(remove_if_present(&absent).is_ok());
    }
}
