//! Building the user's presence lock plugin out of Omarchy's stock lock.
//!
//! Omarchy's own answer to "I want a different lock screen" is a clone: the
//! stock plugin copied into `~/.config/omarchy/plugins/`, enabled in place of
//! the built-in one. That is the path taken here. The clone is generated from
//! whatever lock the running Omarchy ships, so upstream fixes arrive with an
//! `omarchy update` plus one `omarchy-presence-unlock setup`, and nothing this
//! project writes ever lands in a package-owned file.
//!
//! Every edit is anchored on an exact run of stock lines. An anchor that is
//! missing, or present more than once, fails the whole render: an Omarchy
//! release that moves this code must stop setup loudly rather than quietly
//! produce a lock screen with half an authenticator in it.

use super::paths;
use crate::atomic::write_atomic;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

/// The built-in this clone replaces. Recorded as `clonedFrom` so the shell
/// routes the `lock` IPC target here and restores the built-in on removal.
pub(super) const STOCK_PLUGIN_ID: &str = "omarchy.lock";

/// Marks generated code inside the clone. Setup rewrites the clone from stock
/// every run, so this is what tells `doctor` that the enabled lock is the
/// presence one rather than an untouched or hand-made clone.
pub(super) const PRESENCE_MARKER: &str = "// omarchy-presence-unlock:presence";

/// Files copied out of the stock lock. Everything else it ships comes along
/// verbatim; these three are the ones this module rewrites.
const SERVICE_FILE: &str = "Service.qml";
const VIEW_FILE: &str = "LockView.qml";
const MANIFEST_FILE: &str = "manifest.json";

/// Presence state, the hold timer, its PAM context, and the IPC surface.
///
/// Anchored on the last of the stock lock's own properties so the whole block
/// lands inside the root `Item`, where QML order does not matter.
const SERVICE_STATE_ANCHOR: &str = "  property bool strandedLockResolved: false\n";

/// User-facing copy lives at the top of the generated block, in one place, so
/// changing what the lock screen says is a text edit and not a logic change.
const SERVICE_STATE_PATCH: &str = r#"  property bool strandedLockResolved: false

  // omarchy-presence-unlock:presence
  //
  // Wording for the presence hint under the password field. Edit freely: no
  // authentication decision reads these strings.
  readonly property string presenceIdleText: "Hold Alt to unlock with your trusted device"
  readonly property string presenceHoldingText: "Keep holding Alt to confirm unlock…"
  readonly property string presenceCheckingText: "Checking presence, authenticating…"
  readonly property string presenceRetryText: "Could not confirm presence. Hold Alt to retry."
  readonly property string presenceCancelledText: "Hold Alt a little longer to unlock."
  // The gesture, not Hyprland's long_press: the binding reports the press and
  // release edges and this timer decides when a hold counts as deliberate.
  readonly property int presenceHoldMs: 400
  property string presenceMessage: presenceIdleText
  property bool presenceAuthenticating: false
  property bool presenceConfigured: false

  function cancelPresence() {
    presenceHoldTimer.stop()
    presenceAuthenticating = false
    if (presencePam.active) presencePam.abort()
    presenceMessage = presenceIdleText
  }

  // Admission is checked here, before any PAM work: an IPC target every
  // session process can reach must not be able to start an authentication
  // attempt against an unlocked session or alongside a password in flight.
  function holdPresence() {
    if (!lockRequested || !sessionLock.secure) return "not-locked"
    if (authenticatingPassword || presenceAuthenticating || presencePam.active || presenceHoldTimer.running) return "busy"
    if (!presenceConfigured) {
      presenceMessage = presenceRetryText
      return "missing-pam"
    }
    presenceMessage = presenceHoldingText
    presenceHoldTimer.restart()
    runWake()
    return "holding"
  }

  function releasePresence() {
    if (presenceHoldTimer.running) {
      presenceHoldTimer.stop()
      presenceMessage = presenceCancelledText
    }
    return "ok"
  }

  Timer {
    id: presenceHoldTimer
    interval: root.presenceHoldMs

    onTriggered: {
      // The lock may have ended, or a password may have been submitted,
      // during the hold. Re-check rather than trusting the earlier decision.
      if (!root.lockRequested || !sessionLock.secure || root.authenticatingPassword) {
        root.cancelPresence()
        return
      }

      root.presenceMessage = root.presenceCheckingText
      root.presenceAuthenticating = true
      if (!presencePam.start()) {
        root.presenceAuthenticating = false
        root.presenceMessage = root.presenceRetryText
      }
    }
  }

  PamContext {
    id: presencePam
    config: "omarchy-lock-presence"
    user: root.userName

    onCompleted: function(result) {
      // An aborted attempt still completes. Only a result this lock is still
      // waiting for may unlock it.
      if (!root.presenceAuthenticating) return
      root.presenceAuthenticating = false
      if (!root.lockRequested || !sessionLock.secure || root.authenticatingPassword) return
      if (result === PamResult.Success) root.finishUnlock()
      else root.presenceMessage = root.presenceRetryText
    }

    onError: function(error) {
      root.presenceAuthenticating = false
      if (root.lockRequested) root.presenceMessage = root.presenceRetryText
    }
  }

  // Presence is offered only while its policy exists, the way the stock lock
  // gates password and fingerprint on theirs. Watched, so removing the
  // package mid-session takes the hint away instead of leaving a gesture that
  // can only fail.
  FileView {
    path: "/etc/pam.d/omarchy-lock-presence"
    watchChanges: true
    printErrors: false
    onLoaded: root.presenceConfigured = true
    onLoadFailed: {
      root.presenceConfigured = false
      root.cancelPresence()
    }
    onFileChanged: reload()
  }

  // The Alt binding's two edges. There is deliberately no method that
  // authenticates without the hold: the gesture is the user's intent, and
  // this target is reachable by anything in the session.
  IpcHandler {
    target: "presence-unlock"

    function hold(): string { return root.holdPresence() }
    function release(): string { return root.releasePresence() }
    function ping(): string { return "ok" }
  }
"#;

const SERVICE_AUTHENTICATING_ANCHOR: &str = "  readonly property bool authenticating: authenticatingPassword || fingerprintAuthenticating\n";

const SERVICE_AUTHENTICATING_PATCH: &str = "  readonly property bool authenticating: authenticatingPassword || fingerprintAuthenticating || presenceAuthenticating\n";

/// The stock reset is what ends every authenticator when the lock ends, so
/// presence joins it rather than tracking lock lifetime separately.
const SERVICE_RESET_ANCHOR: &str = "  function resetAuthenticationState() {\n";

const SERVICE_RESET_PATCH: &str = "  function resetAuthenticationState() {\n    cancelPresence()\n";

/// A typed password wins over a pending gesture: the user is already
/// authenticating another way, and two attempts must not race for the unlock.
const SERVICE_SUBMIT_ANCHOR: &str =
    "    if (!lockRequested || authenticatingPassword || password.length === 0) return\n";

const SERVICE_SUBMIT_PATCH: &str = "    if (!lockRequested || authenticatingPassword || password.length === 0) return\n    cancelPresence()\n";

const SERVICE_LOCK_VIEW_ANCHOR: &str = "        passwordText: root.enteredPassword\n";

const SERVICE_LOCK_VIEW_PATCH: &str = "        passwordText: root.enteredPassword\n        presenceMessage: root.presenceMessage\n        presenceConfigured: root.presenceConfigured\n";

/// The theme preview renders the same view with no live state, so it shows
/// the resting hint and never a hold or failure message.
const SERVICE_PREVIEW_ANCHOR: &str = "      passwordText: \"\"\n";

const SERVICE_PREVIEW_PATCH: &str = "      passwordText: \"\"\n      presenceMessage: root.presenceIdleText\n      presenceConfigured: root.presenceConfigured\n";

const VIEW_PROPERTY_ANCHOR: &str = "  property bool syncingPasswordText: false\n";

const VIEW_PROPERTY_PATCH: &str = "  property bool syncingPasswordText: false\n  property string presenceMessage: \"\"\n  property bool presenceConfigured: false\n";

/// The tail of the fingerprint indicator and the field that contains it. The
/// hint belongs under the field, in the surrounding background item.
const VIEW_HINT_ANCHOR: &str = "        horizontalAlignment: Text.AlignHCenter\n        verticalAlignment: Text.AlignVCenter\n      }\n    }\n";

const VIEW_HINT_PATCH: &str = r#"        horizontalAlignment: Text.AlignHCenter
        verticalAlignment: Text.AlignVCenter
      }
    }

    // omarchy-presence-unlock:presence
    //
    // Sits under the field rather than inside it: the field's centered dots
    // are the stock layout, and a second line of text in there would push
    // them off centre the way the fingerprint icon is reserved against.
    Text {
      objectName: "presenceStatus"
      anchors.top: inputField.bottom
      anchors.topMargin: 14
      anchors.horizontalCenter: inputField.horizontalCenter
      width: Math.min(root.fieldWidth, parent.width - 24)
      text: root.presenceMessage
      visible: root.presenceConfigured && text.length > 0
      textFormat: Text.PlainText
      color: Color.lock.placeholder
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      horizontalAlignment: Text.AlignHCenter
      wrapMode: Text.WordWrap
    }
"#;

const SERVICE_PATCHES: &[(&str, &str)] = &[
    (SERVICE_STATE_ANCHOR, SERVICE_STATE_PATCH),
    (SERVICE_AUTHENTICATING_ANCHOR, SERVICE_AUTHENTICATING_PATCH),
    (SERVICE_RESET_ANCHOR, SERVICE_RESET_PATCH),
    (SERVICE_SUBMIT_ANCHOR, SERVICE_SUBMIT_PATCH),
    (SERVICE_LOCK_VIEW_ANCHOR, SERVICE_LOCK_VIEW_PATCH),
    (SERVICE_PREVIEW_ANCHOR, SERVICE_PREVIEW_PATCH),
];

const VIEW_PATCHES: &[(&str, &str)] = &[
    (VIEW_PROPERTY_ANCHOR, VIEW_PROPERTY_PATCH),
    (VIEW_HINT_ANCHOR, VIEW_HINT_PATCH),
];

/// `<username>.lock`, matching what `omarchy plugin clone omarchy.lock`
/// produces, so a user who already knows Omarchy's own workflow recognises
/// the plugin this installs.
pub(super) fn plugin_id() -> Result<String, String> {
    let user = env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .map_err(|_| "USER or LOGNAME is required to name the lock plugin".to_string())?;
    let user = user.trim();
    if user.is_empty()
        || !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "cannot build a plugin id from the user name {user:?}; set USER to a plain name"
        ));
    }
    Ok(format!("{user}.lock"))
}

/// Where the running Omarchy keeps the lock this clone is generated from.
///
/// `$OMARCHY_PATH` is what a dev checkout moves, and what `omarchy dev link`
/// points at, so the clone follows the tree the shell is actually loading.
pub(super) fn stock_source() -> PathBuf {
    let base = env::var_os("OMARCHY_PATH")
        .map_or_else(|| PathBuf::from("/usr/share/omarchy"), PathBuf::from);
    base.join("shell/plugins/lock")
}

/// Applies one anchored edit, refusing anything it cannot place exactly once.
fn apply(source: &str, anchor: &str, patch: &str, file: &str) -> Result<String, String> {
    match source.matches(anchor).count() {
        1 => Ok(source.replacen(anchor, patch, 1)),
        0 => Err(format!(
            "this Omarchy release changed {file}: the presence integration cannot find\n  {}\nRun `omarchy-presence-unlock doctor` after updating this package.",
            anchor.trim_end()
        )),
        found => Err(format!(
            "this Omarchy release changed {file}: the presence anchor\n  {}\nmatches {found} places, so the edit is ambiguous.",
            anchor.trim_end()
        )),
    }
}

fn render(source: &str, patches: &[(&str, &str)], file: &str) -> Result<String, String> {
    if source.contains(PRESENCE_MARKER) {
        return Err(format!(
            "{file} already contains presence code; setup builds the clone from the stock lock, not from another clone"
        ));
    }
    let mut rendered = source.to_string();
    for (anchor, patch) in patches {
        rendered = apply(&rendered, anchor, patch, file)?;
    }
    Ok(rendered)
}

/// # Errors
///
/// Returns an error when the stock lock no longer matches an anchor.
pub(super) fn render_service(stock: &str) -> Result<String, String> {
    render(stock, SERVICE_PATCHES, SERVICE_FILE)
}

/// # Errors
///
/// Returns an error when the stock lock view no longer matches an anchor.
pub(super) fn render_view(stock: &str) -> Result<String, String> {
    render(stock, VIEW_PATCHES, VIEW_FILE)
}

/// Renames the manifest onto the clone and records what it replaces.
///
/// `clonedFrom` is the whole reason this is a supported arrangement: the
/// shell disables the built-in lock while this is enabled, and puts it back
/// when the clone goes away.
///
/// # Errors
///
/// Returns an error when the stock manifest cannot be read as an object.
pub(super) fn render_manifest(stock: &str, plugin_id: &str) -> Result<String, String> {
    let mut manifest = serde_json::from_str::<serde_json::Value>(stock)
        .map_err(|error| format!("the stock lock manifest is not valid JSON: {error}"))?;
    let object = manifest
        .as_object_mut()
        .ok_or("the stock lock manifest is not a JSON object")?;

    object.insert("id".into(), plugin_id.into());
    object.insert("name".into(), "Presence Lock Screen".into());
    object.insert(
        "description".into(),
        "Omarchy's lock screen with trusted-device presence unlock.".into(),
    );
    let omarchy = object
        .entry("omarchy")
        .or_insert_with(|| serde_json::json!({}));
    let omarchy = omarchy
        .as_object_mut()
        .ok_or("the stock lock manifest has a non-object `omarchy` section")?;
    omarchy.insert("clonedFrom".into(), STOCK_PLUGIN_ID.into());
    // Capabilities are stamped from first-party manifests; carrying the key
    // into a third-party clone would claim something the shell will not grant.
    omarchy.remove("capabilities");

    serde_json::to_string_pretty(&manifest)
        .map(|rendered| format!("{rendered}\n"))
        .map_err(|error| error.to_string())
}

/// One rendered clone, ready to be written as a directory.
pub(super) struct Clone {
    pub(super) files: Vec<(String, String)>,
    pub(super) copied: Vec<(PathBuf, String)>,
}

/// Reads the stock lock and renders the presence clone from it.
///
/// # Errors
///
/// Returns an error when the stock lock is missing or no longer patchable.
pub(super) fn render_clone(plugin_id: &str) -> Result<Clone, String> {
    let source = stock_source();
    let read = |name: &str| {
        fs::read_to_string(source.join(name)).map_err(|error| {
            format!(
                "cannot read the stock lock at {}: {error}; set OMARCHY_PATH if Omarchy lives elsewhere",
                source.join(name).display()
            )
        })
    };

    let files = vec![
        (
            MANIFEST_FILE.to_string(),
            render_manifest(&read(MANIFEST_FILE)?, plugin_id)?,
        ),
        (
            SERVICE_FILE.to_string(),
            render_service(&read(SERVICE_FILE)?)?,
        ),
        (VIEW_FILE.to_string(), render_view(&read(VIEW_FILE)?)?),
    ];

    // Anything else the stock lock ships (assets, helper QML) travels with it,
    // so a release that adds a file does not leave the clone missing it.
    let mut copied = Vec::new();
    for entry in fs::read_dir(&source).map_err(|error| {
        format!(
            "cannot read the stock lock at {}: {error}",
            source.display()
        )
    })? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if files.iter().any(|(rendered, _)| *rendered == name) {
            continue;
        }
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            return Err(format!(
                "the stock lock now ships the directory {name}; this package cannot clone it yet"
            ));
        }
        copied.push((entry.path(), name));
    }

    Ok(Clone { files, copied })
}

/// Refuses to overwrite a lock plugin this program did not generate.
///
/// The id is the one Omarchy's own `plugin clone` would pick, so a user who
/// cloned the lock for their own reasons already owns this directory. Their
/// work is not this program's to replace.
fn claim_target(target: &Path) -> Result<(), String> {
    if !paths::exists(target) {
        return Ok(());
    }
    if fs::read_to_string(target.join(SERVICE_FILE)).is_ok_and(|s| s.contains(PRESENCE_MARKER)) {
        return Ok(());
    }
    Err(format!(
        "{} is a lock plugin this program did not create; remove it with `omarchy plugin remove {}` and rerun setup",
        target.display(),
        target
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
    ))
}

/// Writes the rendered clone into place, keeping the running one until the
/// replacement is complete.
///
/// A half-written lock plugin is a session that cannot authenticate, so the
/// new directory is built beside the current one and swapped in.
fn write_clone(plugin_id: &str, clone: &Clone) -> Result<(), String> {
    let target = paths::plugin_dir()?;
    claim_target(&target)?;

    let plugins = paths::plugins_dir()?;
    fs::create_dir_all(&plugins).map_err(|error| error.to_string())?;
    let suffix = std::process::id();
    let stage = plugins.join(format!(".{plugin_id}.stage-{suffix}"));
    let rollback = plugins.join(format!(".{plugin_id}.rollback-{suffix}"));
    for temporary in [&stage, &rollback] {
        paths::remove_if_present(temporary)?;
    }

    let build = || -> Result<(), String> {
        fs::create_dir(&stage).map_err(|error| error.to_string())?;
        for (name, contents) in &clone.files {
            write_atomic(&stage.join(name), contents, 0o644)?;
        }
        for (source, name) in &clone.copied {
            fs::copy(source, stage.join(name)).map_err(|error| {
                format!("cannot copy {} into the clone: {error}", source.display())
            })?;
        }
        Ok(())
    };
    if let Err(error) = build() {
        let _ = paths::remove_if_present(&stage);
        return Err(error);
    }

    let previous = paths::exists(&target);
    if previous && let Err(error) = fs::rename(&target, &rollback) {
        let _ = paths::remove_if_present(&stage);
        return Err(format!("could not stage the previous {plugin_id}: {error}"));
    }
    if let Err(error) = fs::rename(&stage, &target) {
        let _ = paths::remove_if_present(&stage);
        if previous {
            let _ = fs::rename(&rollback, &target);
        }
        return Err(format!("could not install {plugin_id}: {error}"));
    }
    if previous {
        paths::remove_if_present(&rollback)?;
    }
    Ok(())
}

/// A fingerprint of the lock code the shell would have to load.
///
/// Not a security digest: the only question is whether these bytes differ
/// from the ones a running shell already compiled.
fn fingerprint(clone: &Clone) -> String {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (name, contents) in &clone.files {
        name.hash(&mut hasher);
        contents.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Restarts the Omarchy shell when the lock code changed, and only then.
///
/// The lock is a `keepLoaded` service: the shell holds its instance across a
/// plugin rescan so a reload cannot destroy the lock client while the
/// compositor is still locked. The other side of that is that new lock code
/// only runs after a restart. A restart is disruptive enough to be worth
/// avoiding when nothing changed, and silently running stale code is worse
/// than a reload, so the applied fingerprint is recorded and compared.
pub(super) fn reload_code(clone: &Clone) -> Result<(), String> {
    let applied = fingerprint(clone);
    let stamp = paths::state_dir()?.join("lock.fingerprint");
    if fs::read_to_string(&stamp).is_ok_and(|recorded| recorded.trim() == applied) {
        return Ok(());
    }
    super::run(Command::new("omarchy").args(["restart", "shell"]))?;
    fs::create_dir_all(stamp.parent().ok_or("invalid state path")?)
        .map_err(|error| error.to_string())?;
    write_atomic(&stamp, &format!("{applied}\n"), 0o644)
}

fn wait_for_discovery(plugin_id: &str) -> Result<(), String> {
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

/// Installs the clone the way Omarchy documents installing a plugin by hand:
/// a directory under `~/.config/omarchy/plugins/`, a rescan, then enable.
///
/// Enabling is what stands the built-in lock down. The shell loads one lock
/// service, and `clonedFrom` tells it which one this replaces.
///
/// # Errors
///
/// Returns an error when the clone cannot be written, validated, discovered,
/// or enabled.
pub(super) fn install(plugin_id: &str, clone: &Clone) -> Result<(), String> {
    write_clone(plugin_id, clone)?;
    super::run(
        Command::new("omarchy")
            .args(["plugin", "validate"])
            .arg(paths::plugin_dir()?),
    )?;
    super::run(Command::new("omarchy-shell").args(["shell", "rescanPlugins"]))?;
    wait_for_discovery(plugin_id)?;
    super::run(Command::new("omarchy").args(["plugin", "enable", plugin_id]))
}

/// Hands the clone back to Omarchy's own plugin command.
///
/// `omarchy plugin remove` is the counterpart of the way it was installed:
/// it disables the plugin, deletes the checkout, rescans, and — because the
/// manifest records `clonedFrom` — restores the built-in lock. Doing those
/// by hand here would mean a second, divergent opinion about what removing a
/// plugin means, and a moment with no lock service at all if they disagree.
///
/// # Errors
///
/// Returns an error when the plugin command fails or a copy cannot be
/// removed.
pub(super) fn remove() -> Result<(), String> {
    let plugin_id = plugin_id()?;
    if paths::exists(&paths::plugin_dir()?) {
        super::run(Command::new("omarchy").args(["plugin", "remove", &plugin_id, "--yes"]))?;
    }

    // `omarchy plugin remove` keeps a backup of a plugin it did not clone
    // from git, so a user does not lose work. This one is generated from the
    // stock lock on every setup, so keeping it would only leave presence code
    // on a machine that asked to be rid of it.
    let prefix = format!(".{plugin_id}.bak.");
    let Ok(entries) = fs::read_dir(paths::plugins_dir()?) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            paths::remove(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOCK_SERVICE: &str = concat!(
        "Item {\n",
        "  property bool strandedLockResolved: false\n",
        "\n",
        "  readonly property bool authenticating: authenticatingPassword || fingerprintAuthenticating\n",
        "\n",
        "  function resetAuthenticationState() {\n",
        "    enteredPassword = \"\"\n",
        "  }\n",
        "\n",
        "  function submitPassword(value) {\n",
        "    if (!lockRequested || authenticatingPassword || password.length === 0) return\n",
        "  }\n",
        "\n",
        "      LockView {\n",
        "        passwordText: root.enteredPassword\n",
        "      }\n",
        "\n",
        "    LockView {\n",
        "      passwordText: \"\"\n",
        "    }\n",
        "}\n",
    );

    const STOCK_VIEW: &str = concat!(
        "Item {\n",
        "  property bool syncingPasswordText: false\n",
        "\n",
        "      Text {\n",
        "        id: fingerprintIcon\n",
        "        horizontalAlignment: Text.AlignHCenter\n",
        "        verticalAlignment: Text.AlignVCenter\n",
        "      }\n",
        "    }\n",
        "  }\n",
        "}\n",
    );

    /// The generated lock must start presence PAM behind the hold, and must
    /// not gain a second way to unlock that skips it.
    #[test]
    fn the_rendered_service_authenticates_only_through_the_hold() {
        let rendered = render_service(STOCK_SERVICE).unwrap();

        assert!(rendered.contains(PRESENCE_MARKER));
        assert!(rendered.contains("config: \"omarchy-lock-presence\""));
        assert!(rendered.contains("function hold(): string"));
        assert!(rendered.contains("function release(): string"));
        assert!(!rendered.contains("function unlock("));
        // finishUnlock() is reachable from exactly one presence path: a
        // successful PAM result.
        assert_eq!(rendered.matches("root.finishUnlock()").count(), 1);
        assert!(rendered.contains("if (result === PamResult.Success) root.finishUnlock()"));
        // The stock lock's own state stays wired to presence.
        assert!(rendered.contains("|| presenceAuthenticating"));
        assert!(rendered.contains("  function resetAuthenticationState() {\n    cancelPresence()"));
    }

    /// A hint the user can read is the whole point of the gesture, and it
    /// must reach both the live lock and the theme preview.
    #[test]
    fn the_rendered_view_shows_the_presence_hint() {
        let service = render_service(STOCK_SERVICE).unwrap();
        let view = render_view(STOCK_VIEW).unwrap();

        assert!(service.contains("presenceMessage: root.presenceMessage"));
        assert!(service.contains("presenceMessage: root.presenceIdleText"));
        assert!(view.contains("property string presenceMessage"));
        assert!(view.contains("objectName: \"presenceStatus\""));
        assert!(view.contains("text: root.presenceMessage"));
    }

    /// An Omarchy release that moves this code must stop setup, not produce a
    /// lock screen missing half its authenticator.
    #[test]
    fn a_moved_anchor_fails_the_render() {
        let moved = STOCK_SERVICE.replace(
            "  readonly property bool authenticating: authenticatingPassword || fingerprintAuthenticating\n",
            "  readonly property bool authenticating: authenticatingPassword\n",
        );
        let error = render_service(&moved).unwrap_err();
        assert!(error.contains("Service.qml"));
        assert!(error.contains("doctor"));

        let duplicated = format!("{STOCK_VIEW}{STOCK_VIEW}");
        assert!(render_view(&duplicated).is_err());
    }

    /// Rendering from an already-patched clone would nest the integration
    /// inside itself; setup always starts from the stock lock.
    #[test]
    fn rendering_from_a_clone_is_refused() {
        let rendered = render_service(STOCK_SERVICE).unwrap();
        assert!(render_service(&rendered).is_err());
    }

    /// `clonedFrom` is what makes the shell disable the built-in lock while
    /// this is enabled, and restore it when the clone is removed.
    #[test]
    fn the_manifest_replaces_the_stock_lock() {
        let stock = r#"{
          "schemaVersion": 1,
          "id": "omarchy.lock",
          "name": "Lock Screen",
          "omarchy": { "capabilities": ["authentication"] },
          "kinds": ["service"],
          "keepLoaded": true,
          "entryPoints": { "service": "Service.qml" }
        }"#;

        let rendered = render_manifest(stock, "tester.lock").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(parsed["id"], "tester.lock");
        assert_eq!(parsed["omarchy"]["clonedFrom"], "omarchy.lock");
        assert_eq!(parsed["entryPoints"]["service"], "Service.qml");
        assert_eq!(parsed["keepLoaded"], true);
        // Third-party manifests cannot claim first-party capabilities.
        assert!(parsed["omarchy"].get("capabilities").is_none());
    }
}
