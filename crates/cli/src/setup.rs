//! Lock-screen integration: Hyprlock keybinding, or Omarchy's Quattro plugin.

use crate::{atomic::write_atomic, devices};
use omarchy_presence_unlock_protocol::paths;
use std::{env, fs, path::PathBuf, process::Command};

fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command exited with {status}"))
    }
}

/// # Errors
///
/// Returns an error when neither integration is available, or when the chosen
/// integration cannot be applied.
pub fn setup_omarchy() -> Result<(), String> {
    let commands = Command::new("omarchy")
        .args(["commands", "--all"])
        .output()
        .map_err(|error| error.to_string())?;
    if String::from_utf8_lossy(&commands.stdout).contains("omarchy plugin clone") {
        return setup_quattro();
    }
    setup_hyprlock()
}

fn setup_hyprlock() -> Result<(), String> {
    if !Command::new("hyprlock")
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
    {
        return Err("this Omarchy build has neither Quattro plugins nor Hyprlock".into());
    }
    devices::set_backend("hyprlock-confirm", None, None, &[])?;
    let bindings = home_dir()?.join(".config/hypr/bindings.lua");
    let binding_marker = "-- omarchy-presence-unlock Alt+Enter confirmation";
    let binding = format!(
        "\n{binding_marker}\no.bind(\"ALT + RETURN\", \"Presence unlock confirmation\", \"omarchy-presence-unlock confirm\", {{ locked = true }})\n"
    );
    let binding_text = fs::read_to_string(&bindings).map_err(|error| error.to_string())?;
    if !binding_text.contains(binding_marker) {
        write_atomic(&bindings, &format!("{binding_text}{binding}"), 0o644)?;
    }
    run(Command::new("hyprctl").arg("reload"))?;
    let validation = Command::new("hyprctl")
        .arg("configerrors")
        .output()
        .map_err(|error| error.to_string())?;
    if !validation.status.success() {
        return Err(String::from_utf8_lossy(&validation.stderr)
            .trim()
            .to_string());
    }
    if !validation.stdout.is_empty() {
        eprintln!(
            "Hyprland config validation output:\n{}",
            String::from_utf8_lossy(&validation.stdout)
        );
    }
    println!(
        "Enabled Alt+Enter unlock confirmation for Hyprlock. Restart omarchy-presence-unlockd."
    );
    Ok(())
}

const PRESENCE_BLOCK: &str = r"
  property bool presenceAuthenticating: false
  property int presenceAttempts: 0
  readonly property bool presenceConfigured: true

  function startPresenceAuth() {
    if (!lockRequested || !sessionLock.secure || presenceAuthenticating || presencePam.active) return
    if (presenceAttempts >= 12) return
    presenceAttempts += 1
    presenceAuthenticating = true
    if (!presencePam.start()) presenceAuthenticating = false
  }
";

const PRESENCE_PAM: &str = r#"
  PamContext {
    id: presencePam
    config: "omarchy-lock-presence"
    user: root.userName
    onCompleted: function(result) {
      root.presenceAuthenticating = false
      if (!root.lockRequested) return
      if (result === PamResult.Success) root.finishUnlock()
      else presenceRetryTimer.restart()
    }
    onError: function(error) { root.presenceAuthenticating = false; if (root.lockRequested) presenceRetryTimer.restart() }
  }

  Timer {
    id: presenceRetryTimer
    interval: 250
    repeat: false
    onTriggered: root.startPresenceAuth()
  }
"#;

fn quattro_plugin_path() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/omarchy/plugins/local.lock/Service.qml"))
}

/// Every substitution `setup_quattro` performs, in the order it applies them.
///
/// `String::replacen` silently returns its input unchanged when the pattern is
/// absent, so all four must be verified before anything is written: a partial
/// patch installs a `PamContext` that is never invoked and QML referencing
/// properties that were never declared.
const QML_ANCHORS: [&str; 4] = [
    "  property bool lockRequested: false",
    "onWakeRequested: root.runWake()",
    "    fingerprintRetryTimer.stop()",
    "  Timer {\n    id: fingerprintRetryTimer",
];

fn missing_anchor(qml: &str) -> Option<&'static str> {
    QML_ANCHORS
        .iter()
        .find(|anchor| !qml.contains(**anchor))
        .copied()
}

fn setup_quattro() -> Result<(), String> {
    let target = quattro_plugin_path()?;
    if !target.exists() {
        run(Command::new("omarchy").args(["plugin", "clone", "omarchy.lock"]))?;
    }
    let mut qml = fs::read_to_string(&target).map_err(|e| e.to_string())?;
    if !qml.contains("id: presencePam") {
        if let Some(missing) = missing_anchor(&qml) {
            return Err(format!(
                "unsupported Omarchy lock plugin layout; no changes written (missing anchor: {missing:?})"
            ));
        }
        qml = qml.replacen(
            QML_ANCHORS[0],
            &format!("{}\n{PRESENCE_BLOCK}", QML_ANCHORS[0]),
            1,
        );
        qml = qml.replacen(
            QML_ANCHORS[1],
            "onWakeRequested: { root.runWake(); root.startPresenceAuth() }",
            1,
        );
        qml = qml.replacen(
            QML_ANCHORS[2],
            "    fingerprintRetryTimer.stop()\n    presenceRetryTimer.stop()\n    presenceAttempts = 0\n    if (presencePam.active) presencePam.abort()",
            1,
        );
        qml = qml.replacen(
            QML_ANCHORS[3],
            &format!("{PRESENCE_PAM}\n  Timer {{\n    id: fingerprintRetryTimer"),
            1,
        );
        if !qml.contains("id: presencePam") {
            return Err("unsupported Omarchy lock plugin layout; no changes written".into());
        }
        let backup = target.with_extension("qml.opu.bak");
        fs::copy(&target, &backup).map_err(|e| e.to_string())?;
        write_atomic(&target, &qml, 0o644)?;
        println!("Backed up the original plugin to {}", backup.display());
    }
    let policy = paths::pam_policy_source();
    if !policy.exists() {
        return Err(format!(
            "PAM policy template is missing at {}; set OPU_DATADIR or reinstall the package",
            policy.display()
        ));
    }
    run(Command::new("sudo").args([
        "install",
        "-m",
        "0644",
        policy.to_str().ok_or("invalid policy path")?,
        "/etc/pam.d/omarchy-lock-presence",
    ]))?;
    println!(
        "Installed local.lock presence adapter. Restart Omarchy Shell or log out/in to load it."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugin carrying every anchor `setup_quattro` rewrites.
    fn complete_plugin() -> String {
        format!(
            "Item {{\n{}\n  onWakeRequested: root.runWake()\n  function reset() {{\n{}\n  }}\n{}\n  }}\n}}\n",
            QML_ANCHORS[0], QML_ANCHORS[2], QML_ANCHORS[3]
        )
    }

    #[test]
    fn preflight_accepts_a_plugin_with_every_anchor() {
        assert_eq!(missing_anchor(&complete_plugin()), None);
    }

    #[test]
    fn preflight_names_the_first_missing_anchor() {
        let stub = "Item { property bool lockRequested: false }\n";
        assert_eq!(missing_anchor(stub), Some(QML_ANCHORS[0]));

        // A reformatted onWakeRequested must abort rather than write a half patch.
        let reformatted = complete_plugin().replace(QML_ANCHORS[1], "onWakeRequested: doWake()");
        assert_eq!(missing_anchor(&reformatted), Some(QML_ANCHORS[1]));
    }
}
