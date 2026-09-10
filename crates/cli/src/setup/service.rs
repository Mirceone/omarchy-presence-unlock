//! The presence daemon's user units, and the system half they depend on.

use super::paths;
use omarchy_presence_unlock_protocol::paths as protocol_paths;
use std::{path::Path, process::Command};

pub(super) const PAM_POLICY: &str = "/etc/pam.d/omarchy-lock-presence";

/// Whether the system half of the install is present.
///
/// The policy is a system file describing a system PAM module, identical for
/// every user, so whatever installed the module owns it too. Checking rather
/// than installing it is what keeps this whole command unprivileged.
///
/// # Errors
///
/// Returns an error naming the policy when it is absent.
pub(super) fn policy_installed() -> Result<(), String> {
    if Path::new(PAM_POLICY).is_file() {
        return Ok(());
    }
    Err(format!(
        "the presence PAM policy is missing at {PAM_POLICY}; reinstall the package, or rerun install.sh"
    ))
}

/// Arms the presence service, so setup leaves a machine that actually answers.
///
/// The path unit is what makes an unenrolled machine usable: the service
/// refuses to run with nothing configured, so it stays enabled but stopped
/// until a config appears. Only an already-enrolled machine is started here.
///
/// # Errors
///
/// Returns an error when systemd rejects one of the unit operations.
pub(super) fn enable() -> Result<(), String> {
    super::run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    super::run(Command::new("systemctl").args([
        "--user",
        "enable",
        "presenced.path",
        "presenced.service",
    ]))?;
    super::run(Command::new("systemctl").args(["--user", "start", "presenced.path"]))?;
    let enrolled = protocol_paths::config_path().is_some_and(|path| path.is_file());
    let unit = if enrolled { "restart" } else { "stop" };
    super::run(Command::new("systemctl").args(["--user", unit, "presenced.service"]))
}

/// Disables both units and stops what they started.
///
/// `disable --now` removes the enable symlinks under
/// `~/.config/systemd/user/`, which is the part of the service this user owns;
/// the unit files themselves live under `/usr` and are the package's.
///
/// The path unit goes first and on its own: it starts the service whenever a
/// config appears, so disabling both in one invocation makes systemd warn that
/// it is stopping a service whose trigger is still armed.
///
/// # Errors
///
/// Returns an error when systemd rejects one of the unit operations.
pub(super) fn stop() -> Result<(), String> {
    super::run(Command::new("systemctl").args(["--user", "disable", "--now", "presenced.path"]))?;
    super::run(Command::new("systemctl").args(["--user", "disable", "--now", "presenced.service"]))
}

/// Removes the runtime socket directory the daemon served from.
///
/// Stopping the daemon leaves the socket behind until the next reboot, and a
/// socket nobody serves is worse than none: the PAM module finds a path that
/// passes its ownership checks and then fails to connect, so a stale one turns
/// a removed feature into a connection error on every lock screen.
///
/// # Errors
///
/// Returns an error when the socket directory cannot be removed.
pub(super) fn remove_socket() -> Result<(), String> {
    paths::remove_if_present(&protocol_paths::current_socket_dir())
}
