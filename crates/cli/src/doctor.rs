//! Preflight checks. Every failure names the command that fixes it.

use crate::client;
use omarchy_presence_unlock_protocol::{
    config::ConfigFile,
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

const STOCK_PLUGIN_ID: &str = "omarchy.lock";
/// Written into the generated lock by `setup::lock`. Its absence means the
/// enabled lock is not this integration, whatever else is in place.
const PRESENCE_MARKER: &str = "// omarchy-presence-unlock:presence";
const BINDING_MARKER: &str = "-- omarchy-presence-unlock:start";
const PAM_POLICY: &str = "/etc/pam.d/omarchy-lock-presence";

struct LockIntegration {
    plugin_id: String,
    service: PathBuf,
    view: PathBuf,
    bindings: PathBuf,
}

/// One finished check, in the order it was run.
///
/// Checks are values rather than printed lines because two callers render
/// them differently: the command prints them, and the wizard paints them as a
/// checklist. A printing `doctor` could only ever serve the first.
pub struct Check {
    pub ok: bool,
    pub label: String,
}

/// Every check that ran, ending at the first failure.
///
/// Stopping is deliberate: a later check reads state an earlier failure
/// invalidates, so continuing would report consequences as separate problems.
#[must_use]
pub fn report() -> Vec<Check> {
    let mut checks = Vec::new();
    let result = collect(&mut checks);
    finish_report(checks, result)
}

fn finish_report(mut checks: Vec<Check>, result: Result<(), String>) -> Vec<Check> {
    if let Err(problem) = result {
        checks.push(Check {
            ok: false,
            label: problem,
        });
    }
    checks
}

/// # Errors
///
/// Returns the first problem found, rendered for the terminal.
pub fn doctor() -> Result<(), String> {
    let checks = report();
    for check in checks.iter().filter(|check| check.ok) {
        println!("ok: {}", check.label);
    }
    match checks.into_iter().find(|check| !check.ok) {
        Some(failure) => Err(failure.label),
        None => Ok(()),
    }
}

fn collect(checks: &mut Vec<Check>) -> Result<(), String> {
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
    pass(
        checks,
        format!(
            "schema {}, {} device(s), multi-device authentication {:?}",
            config.schema_version,
            settings.devices.len(),
            settings.multi_device_auth
        ),
    );

    let integration = lock_integration();
    validate_lock(integration.as_ref())?;
    let plugin_id = integration.map_or_else(
        || STOCK_PLUGIN_ID.to_string(),
        |integration| integration.plugin_id,
    );
    pass(
        checks,
        format!("presence lock {plugin_id}, cloned from {STOCK_PLUGIN_ID}"),
    );
    pass(checks, "Alt hold/release binding".to_string());
    pass(checks, format!("presence PAM policy {PAM_POLICY}"));

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
    pass(
        checks,
        format!(
            "presenced {}, private socket {}",
            user_service_state(),
            socket.display()
        ),
    );

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

    for device in &settings.devices {
        pass(
            checks,
            format!("device {} ({})", device.id, device.profile.id()),
        );
    }
    Ok(())
}

fn pass(checks: &mut Vec<Check>, label: String) {
    checks.push(Check { ok: true, label });
}

/// Where the generated lock should be, named the way setup names it.
fn lock_integration() -> Option<LockIntegration> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()?;
    let plugin_id = format!("{}.lock", user.trim());
    let directory = home.join(".config/omarchy/plugins").join(&plugin_id);
    Some(LockIntegration {
        plugin_id,
        service: directory.join("Service.qml"),
        view: directory.join("LockView.qml"),
        bindings: home.join(".config/hypr/bindings.lua"),
    })
}

fn validate_lock(integration: Option<&LockIntegration>) -> Result<(), String> {
    let integration = integration.ok_or(
        "the lock integration path cannot be resolved; run `omarchy-presence-unlock setup`",
    )?;
    let plugin_id = &integration.plugin_id;

    for asset in [&integration.service, &integration.view] {
        let contents = fs::read_to_string(asset).map_err(|_| {
            format!(
                "the presence lock is missing {}; run `omarchy-presence-unlock setup`",
                asset.display()
            )
        })?;
        // Both halves are generated: the service authenticates, the view is
        // what tells the user the gesture exists. One without the other is a
        // stale clone from a partial install or an interrupted update.
        if !contents.contains(PRESENCE_MARKER) {
            return Err(format!(
                "{} carries no presence code; rerun `omarchy-presence-unlock setup`",
                asset.display()
            ));
        }
    }

    let bindings = fs::read_to_string(&integration.bindings).map_err(|_| {
        format!(
            "Hyprland bindings file {} is missing; run `omarchy-presence-unlock setup`",
            integration.bindings.display()
        )
    })?;
    for required in [
        BINDING_MARKER,
        "ALT_L",
        "ALT_R",
        "presence-unlock hold",
        "presence-unlock release",
        "ignore_mods = true",
        "release = true",
        "locked = true",
        "non_consuming = true",
    ] {
        if !bindings.contains(required) {
            return Err(format!(
                "the Alt presence binding is incomplete (missing {required:?}); rerun `omarchy-presence-unlock setup`"
            ));
        }
    }

    if !std::path::Path::new(PAM_POLICY).is_file() {
        return Err(format!(
            "presence PAM policy is missing at {PAM_POLICY}; rerun `omarchy-presence-unlock setup`"
        ));
    }
    if !plugin_is_enabled(plugin_id) {
        return Err(format!(
            "the presence lock {plugin_id} is not enabled; run `omarchy plugin enable {plugin_id}`"
        ));
    }
    // The clone replaces the built-in, so the shell reports the built-in as
    // disabled. Both enabled at once would mean the clone is not registered
    // as its replacement, and the wrong lock could answer.
    if plugin_is_enabled(STOCK_PLUGIN_ID) {
        return Err(format!(
            "both {plugin_id} and {STOCK_PLUGIN_ID} are enabled; rerun `omarchy-presence-unlock setup`"
        ));
    }
    if !presence_responds() {
        return Err(
            "the presence unlock IPC target is unavailable; run `omarchy restart shell` and retry"
                .into(),
        );
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
    output.status.success() && plugin_enabled_in(&output.stdout, plugin_id)
}

fn plugin_enabled_in(listing: &[u8], plugin_id: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(listing).is_ok_and(|value| {
        value.as_array().is_some_and(|plugins| {
            plugins.iter().any(|plugin| {
                plugin.get("id").and_then(serde_json::Value::as_str) == Some(plugin_id)
                    && plugin.get("enabled").and_then(serde_json::Value::as_bool) == Some(true)
            })
        })
    })
}

/// Whether the running lock answers the gesture the Alt binding sends.
fn presence_responds() -> bool {
    Command::new("omarchy-shell")
        .args(["presence-unlock", "ping"])
        .output()
        .is_ok_and(|output| output.status.success() && output.stdout.trim_ascii() == b"ok")
}

fn user_service_state() -> String {
    Command::new("systemctl")
        .args(["--user", "is-active", "presenced.service"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or_else(|| "unknown".into(), |state| state.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clone replaces the built-in, so a healthy machine reports exactly
    /// one of them enabled — which is what `validate_lock` asserts on.
    #[test]
    fn plugin_listing_detects_only_the_enabled_target() {
        let listing =
            br#"[{"id":"tester.lock","enabled":true},{"id":"omarchy.lock","enabled":false}]"#;
        assert!(plugin_enabled_in(listing, "tester.lock"));
        assert!(!plugin_enabled_in(listing, STOCK_PLUGIN_ID));
        assert!(!plugin_enabled_in(listing, "bob.lock"));
    }

    #[test]
    fn a_failed_report_keeps_preceding_completed_checks() {
        let checks = finish_report(
            vec![Check {
                ok: true,
                label: "configuration is valid".into(),
            }],
            Err("presenced socket is absent".into()),
        );

        assert!(checks[0].ok);
        assert_eq!(checks[0].label, "configuration is valid");
        assert!(!checks[1].ok);
        assert_eq!(checks[1].label, "presenced socket is absent");
    }
}
