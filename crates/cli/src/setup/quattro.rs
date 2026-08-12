use crate::{atomic::write_atomic, devices};
use omarchy_presence_unlock_protocol::paths;
use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

const SERVICE_MARKER: &str = "// omarchy-presence-unlock:service";
const VIEW_MARKER: &str = "// omarchy-presence-unlock:view";
const PLUGIN_ID: &str = "presence.lock";
const STOCK_PLUGIN_ID: &str = "omarchy.lock";
const PAM_POLICY: &str = "/etc/pam.d/omarchy-lock-presence";
const UPDATE_HOOK: &str = "#!/bin/sh\nexec /usr/bin/omarchy-presence-unlock setup-omarchy\n";

const SERVICE_STATE_ANCHOR: &str = "  property bool lockRequested: false";
const SERVICE_RESET_ANCHOR: &str = "    fingerprintRetryTimer.stop()";
const SERVICE_VIEW_STATE_ANCHOR: &str = "        fingerprintConfigured: root.fingerprintConfigured";
const SERVICE_VIEW_EVENT_ANCHOR: &str = "        onWakeRequested: root.runWake()";
const SERVICE_PAM_ANCHOR: &str = "  PamContext {\n    id: fingerprintPam";
const SERVICE_FILE_ANCHOR: &str = "  FileView {\n    path: \"/etc/pam.d/omarchy-lock-password\"";

const VIEW_PROPERTY_ANCHOR: &str = "  property bool fingerprintConfigured: false";
const VIEW_INPUT_ANCHOR: &str =
    "  onInputEnabledChanged: {\n    if (inputEnabled) Qt.callLater(forcePasswordFocus)\n  }";
const VIEW_KEYS_ANCHOR: &str =
    "        Keys.onPressed: function(event) {\n          root.wakeRequested()";
const VIEW_KEYS_END_ANCHOR: &str = "        }\n      }\n\n      Text {";
const VIEW_RIGHT_MARGIN_ANCHOR: &str =
    "        anchors.rightMargin: inputField.borderRight + 18 + root.fingerprintReserve";
const VIEW_LEFT_MARGIN_ANCHOR: &str =
    "        anchors.leftMargin: inputField.borderLeft + 18 + root.fingerprintReserve";
const VIEW_ICON_ANCHOR: &str =
    "      // Fingerprint hint pinned inside the field's right edge when a sensor is";

const SERVICE_STATE: &str = r"  // omarchy-presence-unlock:service
  property bool presenceAuthenticating: false
  property bool presenceConfigured: false

  function startPresenceAuth() {
    if (!lockRequested || !sessionLock.secure || !presenceConfigured) return
    if (presenceAuthenticating || presencePam.active) return
    presenceAuthenticating = true
    if (!presencePam.start()) presenceAuthenticating = false
  }";

const SERVICE_PAM: &str = r#"  PamContext {
    id: presencePam
    config: "omarchy-lock-presence"
    user: root.userName

    onCompleted: function(result) {
      root.presenceAuthenticating = false
      if (root.lockRequested && result === PamResult.Success) root.finishUnlock()
    }

    onError: function(error) {
      root.presenceAuthenticating = false
    }
  }

"#;

const SERVICE_POLICY: &str = r#"  FileView {
    path: "/etc/pam.d/omarchy-lock-presence"
    watchChanges: true
    printErrors: false
    onLoaded: root.presenceConfigured = true
    onLoadFailed: root.presenceConfigured = false
    onFileChanged: reload()
  }

"#;

const VIEW_TIMER: &str = r"  // omarchy-presence-unlock:view
  property bool presenceConfigured: false
  signal presenceRequested()

  Timer {
    id: presenceHoldTimer
    interval: 400
    repeat: false
    onTriggered: {
      if (root.inputEnabled && passwordInput.activeFocus) root.presenceRequested()
    }
  }";

const VIEW_KEY_PRESS: &str = r"        Keys.onPressed: function(event) {
          root.wakeRequested()
          if (event.key === Qt.Key_Alt) {
            if (!event.isAutoRepeat) presenceHoldTimer.start()
            event.accepted = true
            return
          }
          presenceHoldTimer.stop()";

const VIEW_KEY_RELEASE: &str = r"        }

        Keys.onReleased: function(event) {
          if (event.key === Qt.Key_Alt) {
            presenceHoldTimer.stop()
            event.accepted = true
          }
        }

        onActiveFocusChanged: {
          if (!activeFocus) presenceHoldTimer.stop()
        }
      }

      Text {";

const PRESENCE_ICON: &str = r#"      Text {
        id: presenceIcon
        objectName: "presenceIndicator"
        anchors.left: parent.left
        anchors.leftMargin: inputField.borderLeft + 18
        anchors.verticalCenter: parent.verticalCenter
        visible: root.presenceConfigured
        text: "󰂯"
        color: Color.lock.placeholder
        font.family: Style.font.family
        font.pixelSize: Math.round(root.fieldFontSize * 1.1)
        horizontalAlignment: Text.AlignHCenter
        verticalAlignment: Text.AlignVCenter
      }

"#;

fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

fn plugins_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/omarchy/plugins"))
}

fn plugin_dir() -> Result<PathBuf, String> {
    Ok(plugins_dir()?.join(PLUGIN_ID))
}

fn legacy_plugin() -> Result<Option<(String, PathBuf)>, String> {
    let Some(username) = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .ok()
        .filter(|username| username != "presence")
    else {
        return Ok(None);
    };
    let id = format!("{username}.lock");
    Ok(Some((id.clone(), plugins_dir()?.join(id))))
}

fn stock_plugin_dir() -> PathBuf {
    env::var_os("OMARCHY_PATH")
        .map_or_else(|| PathBuf::from("/usr/share/omarchy"), PathBuf::from)
        .join("shell/plugins/lock")
}

fn state_dir() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_STATE_HOME") {
        Some(path) => PathBuf::from(path),
        None => home_dir()?.join(".local/state"),
    };
    Ok(base.join("omarchy-presence-unlock"))
}
fn update_hook_path() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/omarchy/hooks/post-update.d/omarchy-presence-unlock"))
}

fn install_update_hook() -> Result<(), String> {
    let hook = update_hook_path()?;
    fs::create_dir_all(hook.parent().ok_or("invalid update hook path")?)
        .map_err(|error| error.to_string())?;
    write_atomic(&hook, UPDATE_HOOK, 0o755)
}

fn install_policy(source: &Path) -> Result<(), String> {
    let installed = Path::new(PAM_POLICY);
    let current = fs::read(source).ok() == fs::read(installed).ok()
        && fs::metadata(installed)
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o777 == 0o644);
    if current {
        return Ok(());
    }
    super::run(Command::new("sudo").args([
        "install",
        "-m",
        "0644",
        source.to_str().ok_or("invalid policy path")?,
        PAM_POLICY,
    ]))
}

fn replace_once(
    text: &mut String,
    anchor: &str,
    replacement: &str,
    file: &str,
) -> Result<(), String> {
    if !text.contains(anchor) {
        return Err(format!(
            "unsupported Omarchy {file} layout; no changes written (missing anchor: {anchor:?})"
        ));
    }
    *text = text.replacen(anchor, replacement, 1);
    Ok(())
}

fn patch_service(source: &str) -> Result<String, String> {
    if source.contains(SERVICE_MARKER) {
        return Ok(source.to_string());
    }
    if source.contains("id: presencePam") {
        return Err(
            "stock Omarchy Service.qml already contains an unsupported presence integration".into(),
        );
    }

    let mut output = source.to_string();
    replace_once(
        &mut output,
        SERVICE_STATE_ANCHOR,
        &format!("{SERVICE_STATE_ANCHOR}\n{SERVICE_STATE}"),
        "Service.qml",
    )?;
    replace_once(
        &mut output,
        SERVICE_RESET_ANCHOR,
        "    fingerprintRetryTimer.stop()\n    presenceAuthenticating = false\n    if (presencePam.active) presencePam.abort()",
        "Service.qml",
    )?;
    replace_once(
        &mut output,
        SERVICE_VIEW_STATE_ANCHOR,
        "        fingerprintConfigured: root.fingerprintConfigured\n        presenceConfigured: root.presenceConfigured",
        "Service.qml",
    )?;
    replace_once(
        &mut output,
        SERVICE_VIEW_EVENT_ANCHOR,
        "        onWakeRequested: root.runWake()\n        onPresenceRequested: root.startPresenceAuth()",
        "Service.qml",
    )?;
    replace_once(
        &mut output,
        SERVICE_PAM_ANCHOR,
        &format!("{SERVICE_PAM}{SERVICE_PAM_ANCHOR}"),
        "Service.qml",
    )?;
    replace_once(
        &mut output,
        SERVICE_FILE_ANCHOR,
        &format!("{SERVICE_POLICY}{SERVICE_FILE_ANCHOR}"),
        "Service.qml",
    )?;
    Ok(output)
}

fn patch_lock_view(source: &str) -> Result<String, String> {
    if source.contains(VIEW_MARKER) {
        return Ok(source.to_string());
    }
    if source.contains("signal presenceRequested()") || source.contains("id: presenceHoldTimer") {
        return Err(
            "stock Omarchy LockView.qml already contains an unsupported presence integration"
                .into(),
        );
    }

    let mut output = source.to_string();
    replace_once(
        &mut output,
        VIEW_PROPERTY_ANCHOR,
        &format!("{VIEW_PROPERTY_ANCHOR}\n{VIEW_TIMER}"),
        "LockView.qml",
    )?;
    replace_once(
        &mut output,
        VIEW_INPUT_ANCHOR,
        "  onInputEnabledChanged: {\n    if (inputEnabled) Qt.callLater(forcePasswordFocus)\n    else presenceHoldTimer.stop()\n  }",
        "LockView.qml",
    )?;
    replace_once(
        &mut output,
        VIEW_KEYS_ANCHOR,
        VIEW_KEY_PRESS,
        "LockView.qml",
    )?;
    replace_once(
        &mut output,
        VIEW_KEYS_END_ANCHOR,
        VIEW_KEY_RELEASE,
        "LockView.qml",
    )?;
    replace_once(
        &mut output,
        VIEW_RIGHT_MARGIN_ANCHOR,
        "        anchors.rightMargin: inputField.borderRight + 18 + root.authenticationReserve",
        "LockView.qml",
    )?;
    replace_once(
        &mut output,
        VIEW_LEFT_MARGIN_ANCHOR,
        "        anchors.leftMargin: inputField.borderLeft + 18 + root.authenticationReserve",
        "LockView.qml",
    )?;
    replace_once(
        &mut output,
        "  readonly property real fingerprintReserve: fingerprintConfigured ? Math.round(fingerprintIcon.implicitWidth + 12) : 0",
        "  readonly property real fingerprintReserve: fingerprintConfigured ? Math.round(fingerprintIcon.implicitWidth + 12) : 0\n  readonly property real presenceReserve: presenceConfigured ? Math.round(presenceIcon.implicitWidth + 12) : 0\n  readonly property real authenticationReserve: Math.max(fingerprintReserve, presenceReserve)",
        "LockView.qml",
    )?;
    replace_once(
        &mut output,
        VIEW_ICON_ANCHOR,
        &format!("{PRESENCE_ICON}{VIEW_ICON_ANCHOR}"),
        "LockView.qml",
    )?;
    Ok(output)
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let metadata = fs::metadata(&source_path).map_err(|error| error.to_string())?;
        if metadata.is_dir() {
            copy_tree(&source_path, &target_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &target_path).map_err(|error| error.to_string())?;
        } else {
            return Err(format!(
                "unsupported file in stock lock plugin: {}",
                source_path.display()
            ));
        }
    }
    Ok(())
}

fn patch_manifest(source: &str) -> Result<String, String> {
    let mut manifest: serde_json::Value =
        serde_json::from_str(source).map_err(|error| error.to_string())?;
    let object = manifest
        .as_object_mut()
        .ok_or("stock lock manifest is not a JSON object")?;
    object.insert("id".into(), serde_json::Value::String(PLUGIN_ID.into()));
    object.insert(
        "name".into(),
        serde_json::Value::String("Presence Lock".into()),
    );
    let omarchy = object
        .entry("omarchy")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !omarchy.is_object() {
        *omarchy = serde_json::Value::Object(serde_json::Map::new());
    }
    let omarchy = omarchy
        .as_object_mut()
        .ok_or("could not create Omarchy manifest metadata")?;
    omarchy.insert(
        "clonedFrom".into(),
        serde_json::Value::String(STOCK_PLUGIN_ID.into()),
    );
    omarchy.remove("clonePaths");
    let mut rendered =
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    rendered.push('\n');
    Ok(rendered)
}

fn prepare_plugin(stock: &Path, stage: &Path) -> Result<(), String> {
    for required in ["Service.qml", "LockView.qml", "manifest.json"] {
        if !stock.join(required).is_file() {
            return Err(format!(
                "stock Omarchy lock plugin is missing {}",
                stock.join(required).display()
            ));
        }
    }
    copy_tree(stock, stage)?;
    let service_path = stage.join("Service.qml");
    let view_path = stage.join("LockView.qml");
    let manifest_path = stage.join("manifest.json");
    let service =
        patch_service(&fs::read_to_string(&service_path).map_err(|error| error.to_string())?)?;
    let view =
        patch_lock_view(&fs::read_to_string(&view_path).map_err(|error| error.to_string())?)?;
    let manifest =
        patch_manifest(&fs::read_to_string(&manifest_path).map_err(|error| error.to_string())?)?;
    write_atomic(&service_path, &service, 0o644)?;
    write_atomic(&view_path, &view, 0o644)?;
    write_atomic(&manifest_path, &manifest, 0o644)
}

fn archive(directory: &Path, name: &str) -> Result<(), String> {
    let state = state_dir()?;
    fs::create_dir_all(&state).map_err(|error| error.to_string())?;
    let destination = state.join(name);
    if destination.exists() {
        fs::remove_dir_all(&destination).map_err(|error| error.to_string())?;
    }
    fs::rename(directory, destination).map_err(|error| error.to_string())
}

fn wait_for_plugin(plugin_id: &str) -> Result<(), String> {
    let needle = format!("\"id\":\"{plugin_id}\"");
    for _ in 0..40 {
        let output = Command::new("omarchy")
            .args(["plugin", "list", "--json"])
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(&needle) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "{plugin_id} was not discovered after rescanning plugins"
    ))
}

struct PluginSwap<'a> {
    stage: &'a Path,
    target: &'a Path,
    current_rollback: &'a Path,
    legacy_rollback: &'a Path,
}

fn restore_plugin(swap: &PluginSwap<'_>, legacy: Option<&(String, PathBuf)>) -> String {
    let mut errors = Vec::new();
    if swap.target.exists()
        && let Err(error) = fs::remove_dir_all(swap.target)
    {
        errors.push(format!("remove generated plugin: {error}"));
    }
    if swap.current_rollback.exists()
        && let Err(error) = fs::rename(swap.current_rollback, swap.target)
    {
        errors.push(format!("restore previous {PLUGIN_ID}: {error}"));
    }
    if let Some((_, legacy_path)) = legacy
        && swap.legacy_rollback.exists()
        && let Err(error) = fs::rename(swap.legacy_rollback, legacy_path)
    {
        errors.push(format!("restore legacy lock plugin: {error}"));
    }
    let _ = super::run(Command::new("omarchy-shell").args(["shell", "rescanPlugins"]));
    if swap.target.exists() {
        let _ = wait_for_plugin(PLUGIN_ID);
        let _ = super::run(Command::new("omarchy").args(["plugin", "enable", PLUGIN_ID]));
    } else if let Some((legacy_id, legacy_path)) = legacy
        && legacy_path.exists()
    {
        let _ = wait_for_plugin(legacy_id);
        let _ = super::run(Command::new("omarchy").args(["plugin", "enable", legacy_id.as_str()]));
    }
    if errors.is_empty() {
        "previous plugin restored".into()
    } else {
        format!("rollback errors: {}", errors.join("; "))
    }
}

fn activate_plugin(
    swap: &PluginSwap<'_>,
    legacy: Option<&(String, PathBuf)>,
) -> Result<(bool, bool), String> {
    let had_current = swap.target.exists();
    if had_current && let Err(error) = fs::rename(swap.target, swap.current_rollback) {
        let _ = fs::remove_dir_all(swap.stage);
        return Err(format!("could not stage current {PLUGIN_ID}: {error}"));
    }
    if let Some((_, legacy_path)) = legacy
        && let Err(error) = fs::rename(legacy_path, swap.legacy_rollback)
    {
        let _ = fs::remove_dir_all(swap.stage);
        let rollback = restore_plugin(swap, legacy);
        return Err(format!(
            "could not stage legacy lock plugin migration: {error}; {rollback}"
        ));
    }
    if let Err(error) = fs::rename(swap.stage, swap.target) {
        let _ = fs::remove_dir_all(swap.stage);
        let rollback = restore_plugin(swap, legacy);
        return Err(format!(
            "could not activate generated {PLUGIN_ID}: {error}; {rollback}"
        ));
    }

    let activated = super::run(Command::new("omarchy-shell").args(["shell", "rescanPlugins"]))
        .and_then(|()| wait_for_plugin(PLUGIN_ID))
        .and_then(|()| super::run(Command::new("omarchy").args(["plugin", "enable", PLUGIN_ID])));
    if let Err(error) = activated {
        let rollback = restore_plugin(swap, legacy);
        return Err(format!("could not enable {PLUGIN_ID}: {error}; {rollback}"));
    }
    Ok((had_current, legacy.is_some()))
}

pub fn setup() -> Result<(), String> {
    let plugins = plugins_dir()?;
    fs::create_dir_all(&plugins).map_err(|error| error.to_string())?;
    let target = plugin_dir()?;
    let legacy = legacy_plugin()?.filter(|(_, path)| path.exists());
    let suffix = std::process::id();
    let stage = plugins.join(format!(".presence.lock.stage-{suffix}"));
    let current_rollback = plugins.join(format!(".presence.lock.current-{suffix}"));
    let legacy_rollback = plugins.join(format!(".presence.lock.legacy-{suffix}"));
    for temporary in [&stage, &current_rollback, &legacy_rollback] {
        if temporary.exists() {
            fs::remove_dir_all(temporary).map_err(|error| error.to_string())?;
        }
    }

    let stock = stock_plugin_dir();
    if let Err(error) = prepare_plugin(&stock, &stage) {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    if let Err(error) = super::run(
        Command::new("omarchy")
            .args(["plugin", "validate"])
            .arg(&stage),
    ) {
        let _ = fs::remove_dir_all(&stage);
        return Err(format!("generated {PLUGIN_ID} failed validation: {error}"));
    }

    let policy = paths::pam_policy_source();
    if !policy.exists() {
        let _ = fs::remove_dir_all(&stage);
        return Err(format!(
            "PAM policy template is missing at {}; set OPU_DATADIR or reinstall the package",
            policy.display()
        ));
    }
    if let Err(error) = install_policy(&policy) {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    if let Err(error) = install_update_hook() {
        let _ = fs::remove_dir_all(&stage);
        return Err(format!(
            "could not install Omarchy post-update hook: {error}"
        ));
    }
    if let Err(error) = devices::set_backend("disabled", None, None, &[]) {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }

    let swap = PluginSwap {
        stage: &stage,
        target: &target,
        current_rollback: &current_rollback,
        legacy_rollback: &legacy_rollback,
    };
    let (had_current, migrated_legacy) = activate_plugin(&swap, legacy.as_ref())?;

    if had_current && let Err(error) = archive(&current_rollback, "previous-lock-plugin") {
        eprintln!(
            "warning: could not archive previous {PLUGIN_ID}: {error}; retained at {}",
            current_rollback.display()
        );
    }
    if migrated_legacy && let Err(error) = archive(&legacy_rollback, "legacy-lock-plugin") {
        eprintln!(
            "warning: could not archive legacy lock plugin: {error}; retained at {}",
            legacy_rollback.display()
        );
    }

    println!(
        "Built {PLUGIN_ID} from the current stock plugin at {} and enabled it. The prior plugin is retained under {}.",
        stock.display(),
        state_dir()?.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE_FIXTURE: &str = include_str!("fixtures/Service.qml");
    const VIEW_FIXTURE: &str = include_str!("fixtures/LockView.qml");

    #[test]
    fn stock_fixtures_receive_the_complete_integration() {
        let service = patch_service(SERVICE_FIXTURE).unwrap();
        let view = patch_lock_view(VIEW_FIXTURE).unwrap();
        for expected in [
            SERVICE_MARKER,
            "id: presencePam",
            "presencePam.abort()",
            "onPresenceRequested: root.startPresenceAuth()",
            "/etc/pam.d/omarchy-lock-presence",
            "if (root.lockRequested && result === PamResult.Success)",
        ] {
            assert!(service.contains(expected), "missing {expected}");
        }
        for expected in [
            VIEW_MARKER,
            "interval: 400",
            "event.key === Qt.Key_Alt",
            "Keys.onReleased",
            "presenceIndicator",
        ] {
            assert!(view.contains(expected), "missing {expected}");
        }
        assert!(
            view.contains("enabled: root.inputEnabled && !root.authenticatingPassword")
                || !VIEW_FIXTURE.contains("enabled:"),
            "presence integration must not disable password input"
        );
    }

    #[test]
    fn patching_is_idempotent() {
        let service = patch_service(SERVICE_FIXTURE).unwrap();
        let view = patch_lock_view(VIEW_FIXTURE).unwrap();
        assert_eq!(patch_service(&service).unwrap(), service);
        assert_eq!(patch_lock_view(&view).unwrap(), view);
    }

    #[test]
    fn prepared_plugin_uses_presence_id_and_current_stock_files() {
        let root = std::env::temp_dir().join(format!("opu-quattro-prepare-{}", std::process::id()));
        let stock = root.join("stock");
        let stage = root.join("stage");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&stock).unwrap();
        fs::write(stock.join("Service.qml"), SERVICE_FIXTURE).unwrap();
        fs::write(stock.join("LockView.qml"), VIEW_FIXTURE).unwrap();
        fs::write(
            stock.join("manifest.json"),
            r#"{"schemaVersion":1,"id":"omarchy.lock","name":"Lock Screen","version":"1.0.0","kinds":["service"],"entryPoints":{"service":"Service.qml"}}"#,
        )
        .unwrap();
        fs::write(stock.join("current-stock-asset"), "new\n").unwrap();

        prepare_plugin(&stock, &stage).unwrap();

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(stage.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["id"], PLUGIN_ID);
        assert_eq!(manifest["name"], "Presence Lock");
        assert_eq!(manifest["omarchy"]["clonedFrom"], STOCK_PLUGIN_ID);
        assert_eq!(
            fs::read_to_string(stage.join("current-stock-asset")).unwrap(),
            "new\n"
        );
        assert!(
            fs::read_to_string(stage.join("Service.qml"))
                .unwrap()
                .contains(SERVICE_MARKER)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_stock_layout_cannot_touch_the_installed_plugin() {
        let root = std::env::temp_dir().join(format!("opu-quattro-reject-{}", std::process::id()));
        let stock = root.join("stock");
        let target = root.join(PLUGIN_ID);
        let stage = root.join("stage");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&stock).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("sentinel"), "working\n").unwrap();
        fs::write(stock.join("Service.qml"), "Item {}\n").unwrap();
        fs::write(stock.join("LockView.qml"), VIEW_FIXTURE).unwrap();
        fs::write(stock.join("manifest.json"), "{}\n").unwrap();

        assert!(prepare_plugin(&stock, &stage).is_err());
        assert_eq!(
            fs::read_to_string(target.join("sentinel")).unwrap(),
            "working\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_layouts_are_rejected_before_writes() {
        assert!(patch_service("Item {}\n").is_err());
        assert!(patch_lock_view("Item {}\n").is_err());
    }
}
