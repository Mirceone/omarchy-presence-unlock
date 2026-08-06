//! Lock-screen backends.
//!
//! Presence policy decides *whether* to unlock; an [`Unlocker`] knows *how*.
//! Adding support for another lock screen means adding an implementation here
//! and a name in [`omarchy_presence_unlock_protocol::config`]; nothing in the scan
//! or policy path changes.

use nix::{
    sys::signal::{Signal, kill},
    unistd::{Pid, Uid},
};
use omarchy_presence_unlock_protocol::config::{Backend, SignalKind};
use std::{fs, os::unix::fs::MetadataExt, process::Command, sync::Arc};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UnlockError {
    #[error("no lock screen is running")]
    NotLocked,
    #[error("{0}")]
    Failed(String),
}

pub trait Unlocker: Send + Sync + 'static {
    /// Human-readable backend description, for `status` and startup logs.
    fn describe(&self) -> String;

    /// Whether a lock screen is currently up. `None` when the backend cannot tell,
    /// in which case [`Unlocker::unlock`] is the only way to find out.
    fn locked(&self) -> Option<bool> {
        None
    }

    /// Releases the lock screen. Blocking: callers run it off the reactor.
    ///
    /// # Errors
    ///
    /// Returns [`UnlockError::NotLocked`] when no lock screen is running, and
    /// [`UnlockError::Failed`] when the release itself failed.
    fn unlock(&self) -> Result<(), UnlockError>;
}

/// Builds the configured backend, or `None` when unlocking is disabled.
#[must_use]
pub fn build(backend: &Backend) -> Option<Arc<dyn Unlocker>> {
    match backend {
        Backend::Disabled => None,
        Backend::ProcessSignal { process, signal } => Some(Arc::new(ProcessSignal {
            process: process.clone(),
            signal: match signal {
                SignalKind::Usr1 => Signal::SIGUSR1,
                SignalKind::Usr2 => Signal::SIGUSR2,
            },
        })),
        Backend::Command(argv) => Some(Arc::new(CommandUnlocker { argv: argv.clone() })),
    }
}

/// Signals a lock-screen process owned by this user.
///
/// Resolving the PID from procfs and signalling it directly removes both the
/// `pgrep`/`pkill` process spawns (which blocked the async worker and could
/// outlast the client deadline) and the window in which the lock screen exits
/// between the two.
pub struct ProcessSignal {
    process: String,
    signal: Signal,
}

impl ProcessSignal {
    /// Returns the PID of a process named `self.process` owned by this user.
    fn pid(&self) -> Option<Pid> {
        let uid = Uid::current().as_raw();
        // procfs reads are served from kernel memory; they never block on I/O.
        for entry in fs::read_dir("/proc").ok()?.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let path = entry.path();
            if !fs::metadata(&path).is_ok_and(|metadata| metadata.uid() == uid) {
                continue;
            }
            if fs::read_to_string(path.join("comm")).is_ok_and(|comm| comm.trim() == self.process) {
                return Some(Pid::from_raw(pid));
            }
        }
        None
    }
}

impl Unlocker for ProcessSignal {
    fn describe(&self) -> String {
        format!("signal {} to {}", self.signal, self.process)
    }

    fn locked(&self) -> Option<bool> {
        Some(self.pid().is_some())
    }

    fn unlock(&self) -> Result<(), UnlockError> {
        let pid = self.pid().ok_or(UnlockError::NotLocked)?;
        kill(pid, self.signal).map_err(|error| UnlockError::Failed(error.to_string()))
    }
}

/// Runs a command; a zero exit status means the session was unlocked.
pub struct CommandUnlocker {
    argv: Vec<String>,
}

impl Unlocker for CommandUnlocker {
    fn describe(&self) -> String {
        format!("run {}", self.argv.join(" "))
    }

    fn unlock(&self) -> Result<(), UnlockError> {
        let (program, arguments) = self
            .argv
            .split_first()
            .ok_or_else(|| UnlockError::Failed("empty unlock command".into()))?;
        let status = Command::new(program)
            .args(arguments)
            .status()
            .map_err(|error| UnlockError::Failed(format!("{program}: {error}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(UnlockError::Failed(format!(
                "{program} exited with {status}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_builds_no_unlocker() {
        assert!(build(&Backend::Disabled).is_none());
    }

    #[test]
    fn a_process_signal_backend_reports_whether_the_process_is_running() {
        let running = ProcessSignal {
            // This test binary is the one process we know is running as us.
            process: current_comm(),
            signal: Signal::SIGUSR1,
        };
        assert_eq!(running.locked(), Some(true));
        let absent = ProcessSignal {
            process: "definitely-not-a-running-process".into(),
            signal: Signal::SIGUSR1,
        };
        assert_eq!(absent.locked(), Some(false));
        assert!(matches!(absent.unlock(), Err(UnlockError::NotLocked)));
    }

    fn current_comm() -> String {
        fs::read_to_string("/proc/self/comm")
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn the_command_backend_reports_a_failing_exit_status() {
        let unlocker = CommandUnlocker {
            argv: vec!["false".into()],
        };
        assert!(matches!(unlocker.unlock(), Err(UnlockError::Failed(_))));
        let ok = CommandUnlocker {
            argv: vec!["true".into()],
        };
        assert!(ok.unlock().is_ok());
    }
}
