//! Removing the user's integration, then the system-wide package.
//!
//! Authenticate before changing anything. All per-user cleanup steps are
//! attempted, but the package stays installed if any fail: removing the PAM
//! module while the presence lock is still active would leave a broken lock
//! integration and no installed command with which to retry cleanup.

use super::{binding, lock, menu, paths, service};
use omarchy_presence_unlock_protocol::paths as protocol_paths;
use std::process::Command;

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

/// Removes the integration, optionally the enrollment, and the installed package.
///
/// Package removal requires administrator access and affects every user.
/// Failed cleanup leaves the package installed so the user can retry.
pub fn uninstall(enrollment: Enrollment) -> Vec<Step> {
    let mut steps = vec![step(
        "Administrator access confirmed",
        super::run(Command::new("sudo").arg("--validate")),
    )];
    if !steps[0].ok {
        return steps;
    }

    steps.push(step(
        format!(
            "Removed the presence lock and restored {}",
            lock::STOCK_PLUGIN_ID
        ),
        lock::remove(),
    ));
    steps.push(step("Removed the Omarchy menu entry", menu::remove()));
    let baseline = binding::config_errors();
    steps.push(step("Removed the Alt unlock binding", binding::remove()));
    // Advisory: the block is already gone from bindings.lua, so Hyprland drops
    // it at its next reload, and there is no Hyprland to reload over SSH.
    // Neither may hold the package hostage.
    steps.push(
        match baseline.and_then(|before| binding::reload_hyprland(&before)) {
            Ok(()) => step("Reloaded Hyprland", Ok(())),
            Err(error) => Step {
                label: "Hyprland not reloaded; the binding stops at its next reload".into(),
                ok: true,
                detail: Some(error),
            },
        },
    );
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
    let removal = if steps.iter().all(|step| step.ok) {
        // Remove only our package, without retaining package-owned .pacsave
        // files or recursively removing dependencies used by other software.
        super::run(Command::new("sudo").args([
            "--non-interactive",
            "pacman",
            "--remove",
            "--nosave",
            "--noconfirm",
            "--",
            "omarchy-presence-unlock",
        ]))
    } else {
        Err("package kept installed because integration cleanup failed; fix the failed steps and rerun uninstall".into())
    };
    steps.push(step("Removed the installed application package", removal));
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
