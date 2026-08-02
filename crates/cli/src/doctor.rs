//! Preflight checks. Every failure names the command that fixes it.

use crate::client;
use omarchy_watch_unlock_protocol::{
    config::{Backend, ConfigFile},
    paths::{config_path, current_socket_path},
    wire,
};
use std::{fs, os::unix::fs::PermissionsExt, process::Command, time::Duration};

/// # Errors
///
/// Returns the first problem found, rendered for the terminal.
pub fn doctor() -> Result<(), String> {
    let path = config_path().ok_or("XDG_CONFIG_HOME or HOME is required")?;
    let config = ConfigFile::from_path(&path).map_err(|error| error.to_string())?;
    let settings = config.resolve().map_err(|error| error.to_string())?;

    let mode = fs::metadata(&path)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(format!(
            "{} must be mode 0600 (is {mode:o})",
            path.display()
        ));
    }

    match &settings.backend {
        Backend::Disabled => {
            return Err(
                "unlock backend is disabled; run setup-omarchy or `backend` to pick one".into(),
            );
        }
        Backend::ProcessSignal { process, .. } => {
            if !executable(process) {
                return Err(format!(
                    "unlock backend signals {process}, but {process} is not on PATH"
                ));
            }
        }
        Backend::Command(argv) => {
            let program = &argv[0];
            if !executable(program) {
                return Err(format!("unlock command {program} is not on PATH"));
            }
        }
    }

    if !current_socket_path().exists() {
        return Err("daemon socket is absent; start the user service".into());
    }
    // The socket exists, so the daemon can answer for itself: this proves the
    // running daemon parsed the same devices, not just that the file is valid.
    let reported = client::request_lines(wire::REQ_STATUS, Duration::from_millis(200))?;
    let devices = reported
        .iter()
        .filter(|line| line.starts_with("DEVICE "))
        .count();
    if devices != settings.devices.len() {
        return Err(format!(
            "config lists {} device(s) but the daemon is running {devices}; restart omarchy-watch-unlockd",
            settings.devices.len()
        ));
    }

    println!(
        "ok: schema {}, {} device(s), quorum {:?}, backend {}",
        config.schema_version,
        settings.devices.len(),
        settings.quorum,
        config.unlock_backend
    );
    for device in &settings.devices {
        println!("  {} ({})", device.id, device.profile.id());
    }
    Ok(())
}

/// True when `program` runs. `--version` is the one flag every lock screen and
/// session tool in scope accepts.
fn executable(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}
