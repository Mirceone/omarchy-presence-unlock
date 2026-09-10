//! Undoing the per-user half of the integration.
//!
//! Only the user's own half: the installed files are a package's, whichever
//! way they were installed, so `pacman -Rns` removes them. That is what keeps
//! this unprivileged and keeps one owner per file.
//!
//! Teardown is deliberately not atomic the way [`super::apply`] is. Setup
//! must leave the machine either integrated or untouched, because a half
//! integration is a lock screen that cannot authenticate. Removal is the
//! opposite: a step that fails must not strand the steps after it, or a user
//! trying to get rid of this is left with the parts that broke and no way to
//! reach the parts that would have worked. So every step runs, and each
//! reports for itself.

use super::{binding, lock, menu, paths, service};
use omarchy_presence_unlock_protocol::paths as protocol_paths;

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

fn forget_enrollment() -> Result<(), String> {
    let config = protocol_paths::config_path().ok_or("XDG_CONFIG_HOME or HOME is required")?;
    paths::remove_if_present(&config)
}

/// Undoes the integration, the service, and optionally the enrollment.
///
/// Every step is attempted whatever the ones before it did; the returned list
/// is what happened, in order.
pub fn uninstall(enrollment: Enrollment) -> Vec<Step> {
    let mut steps = Vec::new();

    steps.push(step(
        format!(
            "Removed the presence lock and restored {}",
            lock::STOCK_PLUGIN_ID
        ),
        lock::remove(),
    ));
    steps.push(step("Removed the Omarchy menu entry", menu::remove()));
    steps.push(step("Removed the Alt unlock binding", binding::remove()));
    steps.push(step("Reloaded Hyprland", binding::reload_hyprland()));
    steps.push(step(
        "Disabled and stopped the presence service",
        service::stop(),
    ));
    steps.push(step("Removed the control socket", service::remove_socket()));
    steps.push(step(
        "Removed retained state",
        paths::state_dir().and_then(|path| paths::remove_if_present(&path)),
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
        assert!(paths::remove_if_present(&absent).is_ok());
    }
}
