//! Preflight checks. Every failure names the command that fixes it.

use crate::client;
use omarchy_presence_unlock_protocol::{
    config::{Backend, ConfigFile},
    paths::{config_path, current_socket_path},
    wire,
};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::PathBuf,
    process::Command,
    time::Duration,
};

const SERVICE_MARKER: &str = "// omarchy-presence-unlock:service";
const VIEW_MARKER: &str = "// omarchy-presence-unlock:view";
const PAM_POLICY: &str = "/etc/pam.d/omarchy-lock-presence";
const UPDATE_HOOK: &str = ".config/omarchy/hooks/post-update.d/omarchy-presence-unlock";

struct QuattroIntegration {
    plugin_id: String,
    service: PathBuf,
    view: PathBuf,
}

/// # Errors
///
/// Returns the first problem found, rendered for the terminal.
pub fn doctor() -> Result<(), String> {
    let path = config_path().ok_or("XDG_CONFIG_HOME or HOME is required")?;
    let config = ConfigFile::from_path(&path).map_err(|error| error.to_string())?;
    let settings = config.resolve().map_err(|error| error.to_string())?;

    let mode = fs::metadata(&path)
        .map_err(|error| error.to_string())?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(format!(
            "{} must be mode 0600 (is {mode:o})",
            path.display()
        ));
    }

    let quattro = quattro_integration();
    match &settings.backend {
        Backend::Quattro => validate_quattro(quattro.as_ref())?,
        Backend::Disabled => {}
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

    let socket = current_socket_path();
    let metadata = fs::metadata(&socket).map_err(|_| {
        format!(
            "presenced socket is absent at {}; run `systemctl --user restart presenced` (service is {})",
            socket.display(),
            user_service_state()
        )
    })?;
    let uid = nix::unistd::Uid::effective().as_raw();
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(format!(
            "presenced socket at {} is not a private socket owned by uid {uid}",
            socket.display()
        ));
    }

    let reported = client::request_lines(wire::REQ_STATUS, Duration::from_millis(200))?;
    let devices = reported
        .iter()
        .filter(|line| line.starts_with("DEVICE "))
        .count();
    if devices != settings.devices.len() {
        return Err(format!(
            "config lists {} device(s) but presenced is running {devices}; restart presenced",
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
    if let Some(integration) = quattro {
        println!("ok: active Quattro plugin {}", integration.plugin_id);
        println!("ok: presence PAM policy {PAM_POLICY}");
        if let Some(hook) = update_hook_path() {
            println!("ok: Omarchy post-update rebase hook {}", hook.display());
        }
    }
    println!(
        "ok: presenced {}, private socket {}",
        user_service_state(),
        socket.display()
    );
    for device in &settings.devices {
        println!("  {} ({})", device.id, device.profile.id());
    }
    Ok(())
}

fn quattro_integration() -> Option<QuattroIntegration> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let plugin_id = "presence.lock".to_string();
    let directory = home.join(".config/omarchy/plugins").join(&plugin_id);
    Some(QuattroIntegration {
        plugin_id,
        service: directory.join("Service.qml"),
        view: directory.join("LockView.qml"),
    })
}
fn update_hook_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(UPDATE_HOOK))
}

fn validate_quattro(integration: Option<&QuattroIntegration>) -> Result<(), String> {
    let integration = integration.ok_or(
        "unlock backend is disabled and the Quattro plugin path cannot be resolved; run `omarchy-presence-unlock setup-omarchy`",
    )?;
    let service = fs::read_to_string(&integration.service).map_err(|_| {
        format!(
            "unlock backend is disabled but {} is missing; run `omarchy-presence-unlock setup-omarchy`",
            integration.service.display()
        )
    })?;
    let view = fs::read_to_string(&integration.view).map_err(|_| {
        format!(
            "Quattro presence view {} is missing; run `omarchy-presence-unlock setup-omarchy`",
            integration.view.display()
        )
    })?;
    if !service.contains(SERVICE_MARKER) || !view.contains(VIEW_MARKER) {
        return Err(
            "the active Quattro clone does not contain the current presence integration; rerun `omarchy-presence-unlock setup-omarchy`"
                .into(),
        );
    }
    if !std::path::Path::new(PAM_POLICY).is_file() {
        return Err(format!(
            "presence PAM policy is missing at {PAM_POLICY}; rerun `omarchy-presence-unlock setup-omarchy`"
        ));
    }
    let hook = update_hook_path().ok_or(
        "HOME is required to locate the Omarchy post-update hook; rerun `omarchy-presence-unlock setup-omarchy`",
    )?;
    let hook_metadata = fs::metadata(&hook).map_err(|_| {
        format!(
            "Omarchy post-update hook is missing at {}; rerun `omarchy-presence-unlock setup-omarchy`",
            hook.display()
        )
    })?;
    if !hook_metadata.is_file() || hook_metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "Omarchy post-update hook at {} is not executable; rerun `omarchy-presence-unlock setup-omarchy`",
            hook.display()
        ));
    }
    if !plugin_is_enabled(&integration.plugin_id) {
        return Err(format!(
            "Quattro plugin {} is not enabled; run `omarchy plugin enable {}`",
            integration.plugin_id, integration.plugin_id
        ));
    }
    Ok(())
}

fn plugin_is_enabled(plugin_id: &str) -> bool {
    let Ok(output) = Command::new("omarchy")
        .args(["plugin", "list", "--json"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let needle = format!("\"id\":\"{plugin_id}\"");
    listing
        .find(&needle)
        .and_then(|start| listing[start..].split_once('}').map(|(object, _)| object))
        .is_some_and(|object| object.contains("\"enabled\":true"))
}

fn user_service_state() -> String {
    Command::new("systemctl")
        .args(["--user", "is-active", "presenced.service"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or_else(|| "unknown".into(), |state| state.trim().to_string())
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

#[cfg(test)]
mod tests {

    #[test]
    fn compact_plugin_listing_detects_only_the_enabled_target() {
        let listing =
            r#"[{"id":"presence.lock","enabled":true},{"id":"omarchy.lock","enabled":false}]"#;
        let needle = "\"id\":\"presence.lock\"";
        let enabled = listing
            .find(needle)
            .and_then(|start| listing[start..].split_once('}').map(|(object, _)| object))
            .is_some_and(|object| object.contains("\"enabled\":true"));
        assert!(enabled);
        assert!(!listing.contains("\"id\":\"bob.lock\""));
    }
}
