//! Daemon lifecycle guards for screens that need exclusive adapter access.

use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

pub(super) const UNLOCKD: &str = "presenced";

pub(super) fn systemctl(args: &[&str]) -> Result<(), String> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl --user {} exited with {status}",
            args.join(" ")
        ))
    }
}

/// `systemctl stop` succeeds on an already-inactive unit, so its exit status
/// cannot say whether there was a daemon to put back afterwards.
pub(super) fn unlockd_is_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", UNLOCKD])
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Set while the menu holds the daemon stopped so an enrollment or a scan can
/// have the adapter to itself.
///
/// Putting it back cannot be a plain statement at the end of the operation:
/// a `?`, a worker failure, or the SIGINT handler's `exit` all skip past that
/// point and would strand the user's unlock daemon until they noticed and
/// restarted it by hand. [`DaemonPause`]'s `Drop` covers the unwinding exits
/// and the terminal-restoration path covers `exit`, which no `Drop` runs for;
/// this flag is what lets both share one restart.
static DAEMON_STOPPED: AtomicBool = AtomicBool::new(false);

/// Stops the daemon for as long as this is alive, if there was one running.
///
/// Silent, because the screens say what is happening. Call [`DaemonPause::resume`]
/// to learn whether the restart worked; `Drop` is the safety net for the paths
/// that never get that far.
pub(super) struct DaemonPause;

impl DaemonPause {
    pub(super) fn stop() -> Self {
        if unlockd_is_active() {
            DAEMON_STOPPED.store(true, Ordering::Relaxed);
            let _ = systemctl(&["stop", UNLOCKD]);
        }
        Self
    }

    /// Ends the pause here rather than at the end of the caller's scope, and
    /// reports whether the daemon was restarted. A false result means it was
    /// inactive before the operation, so screens must not claim it resumed.
    pub(super) fn resume(self) -> Result<bool, String> {
        let restarted = restart_unlockd();
        // Explicit: the guard is spent, and the `Drop` it triggers finds the
        // flag already cleared rather than issuing a second restart.
        drop(self);
        restarted
    }
}

impl Drop for DaemonPause {
    fn drop(&mut self) {
        let _ = restart_unlockd();
    }
}

/// Puts the daemon back if this menu stopped it, reporting whether there was
/// anything to put back. Idempotent by the `swap`: whichever of the `Drop`
/// and the SIGINT path runs first is the only one that acts, so an interrupt
/// during an already-running restart cannot issue a second one.
pub(super) fn restart_unlockd() -> Result<bool, String> {
    if DAEMON_STOPPED.swap(false, Ordering::Relaxed) {
        systemctl(&["start", UNLOCKD])
            .map(|()| true)
            .map_err(|error| format!("{UNLOCKD} did not restart: {error}"))
    } else {
        Ok(false)
    }
}
