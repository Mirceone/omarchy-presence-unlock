use crate::atomic::write_atomic;
use omarchy_presence_unlock_protocol::paths;
use std::{
    env, fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

pub(super) const PLUGIN_ID: &str = "presence.unlock";
const STOCK_PLUGIN_ID: &str = "omarchy.lock";
pub(super) const PAM_POLICY: &str = "/etc/pam.d/omarchy-lock-presence";
pub(super) const BINDING_START: &str = "-- omarchy-presence-unlock:start";
pub(super) const BINDING_END: &str = "-- omarchy-presence-unlock:end";
// The press edge ignores mods because Hyprland has not yet folded Alt into the
// modmask at Alt's own press. The release edge needs the target modmask plus
// `release`; the QML plugin, rather than Hyprland's `long_press`, owns the hold
// duration. Both Alt keys are bound so either hand works.
const BINDING_BLOCK: &str = r#"-- omarchy-presence-unlock:start
for _, presence_key in ipairs({ "ALT_L", "ALT_R" }) do
  o.bind(presence_key, "Presence unlock", "omarchy-shell -q presence-unlock hold", {
    locked = true,
    ignore_mods = true,
    non_consuming = true,
  })
  o.bind("ALT + " .. presence_key, "Presence unlock release", "omarchy-shell -q presence-unlock release", {
    locked = true,
    release = true,
    non_consuming = true,
  })
end
-- omarchy-presence-unlock:end"#;

pub(super) fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

fn plugins_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/omarchy/plugins"))
}

pub(super) fn plugin_dir() -> Result<PathBuf, String> {
    Ok(plugins_dir()?.join(PLUGIN_ID))
}

pub(super) fn bindings_path() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/hypr/bindings.lua"))
}

pub(super) fn state_dir() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_STATE_HOME") {
        Some(path) => PathBuf::from(path),
        None => home_dir()?.join(".local/state"),
    };
    Ok(base.join("omarchy-presence-unlock"))
}

pub(super) fn obsolete_update_hook() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".config/omarchy/hooks/post-update.d/omarchy-presence-unlock"))
}

pub(super) fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

pub(super) fn remove_path(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path).map_err(|error| error.to_string())
    } else if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(|error| error.to_string())
    } else {
        Err(format!(
            "unsupported filesystem entry at {}",
            path.display()
        ))
    }
}

fn archive(directory: &Path, name: &str) -> Result<(), String> {
    let state = state_dir()?;
    fs::create_dir_all(&state).map_err(|error| error.to_string())?;
    let destination = state.join(name);
    if path_exists(&destination) {
        remove_path(&destination)?;
    }
    fs::rename(directory, destination).map_err(|error| error.to_string())
}

/// Whether the system half of the install is present.
///
/// The policy is a system file describing a system PAM module, identical for
/// every user, so whatever installed the module owns it too. Checking rather
/// than installing it is what keeps this whole command unprivileged.
fn policy_installed() -> Result<(), String> {
    if Path::new(PAM_POLICY).is_file() {
        return Ok(());
    }
    Err(format!(
        "the presence PAM policy is missing at {PAM_POLICY}; reinstall the package, or rerun install.sh"
    ))
}

fn render_bindings(source: &str) -> Result<String, String> {
    let starts = source.match_indices(BINDING_START).collect::<Vec<_>>();
    let ends = source.match_indices(BINDING_END).collect::<Vec<_>>();
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => {
            let mut output = source.to_string();
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            if !output.is_empty() && !output.ends_with("\n\n") {
                output.push('\n');
            }
            output.push_str(BINDING_BLOCK);
            output.push('\n');
            Ok(output)
        }
        ([(start, _)], [(end, _)]) if start < end => {
            let end = end + BINDING_END.len();
            let mut output = source.to_string();
            output.replace_range(*start..end, BINDING_BLOCK);
            Ok(output)
        }
        _ => Err(format!(
            "{} contains an incomplete or duplicate presence-unlock block",
            bindings_path()?.display()
        )),
    }
}

fn install_binding() -> Result<(), String> {
    let path = bindings_path()?;
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let rendered = render_bindings(&source)?;
    if rendered == source {
        return Ok(());
    }
    fs::create_dir_all(path.parent().ok_or("invalid Hyprland bindings path")?)
        .map_err(|error| error.to_string())?;
    write_atomic(&path, &rendered, 0o644)
}

fn install_plugin(source: &Path, target: &Path) -> Result<(), String> {
    let source = fs::canonicalize(source).map_err(|error| error.to_string())?;
    if fs::canonicalize(target).is_ok_and(|current| current == source) {
        return Ok(());
    }

    let plugins = target.parent().ok_or("invalid plugin target path")?;
    fs::create_dir_all(plugins).map_err(|error| error.to_string())?;
    let suffix = std::process::id();
    let stage = plugins.join(format!(".{PLUGIN_ID}.stage-{suffix}"));
    let rollback = plugins.join(format!(".{PLUGIN_ID}.rollback-{suffix}"));
    for temporary in [&stage, &rollback] {
        if path_exists(temporary) {
            remove_path(temporary)?;
        }
    }
    symlink(&source, &stage).map_err(|error| error.to_string())?;

    let had_current = path_exists(target);
    if had_current && let Err(error) = fs::rename(target, &rollback) {
        let _ = remove_path(&stage);
        return Err(format!(
            "could not stage the previous {PLUGIN_ID} plugin: {error}"
        ));
    }
    if let Err(error) = fs::rename(&stage, target) {
        let _ = remove_path(&stage);
        if had_current {
            let _ = fs::rename(&rollback, target);
        }
        return Err(format!("could not install the {PLUGIN_ID} plugin: {error}"));
    }
    if had_current && let Err(error) = archive(&rollback, "previous-presence-plugin") {
        eprintln!(
            "warning: could not archive the previous {PLUGIN_ID} plugin: {error}; retained at {}",
            rollback.display()
        );
    }
    Ok(())
}

/// A fingerprint of the plugin code the shell would have to load.
///
/// Not a security digest: the only question is whether these bytes differ from
/// the ones a running shell already compiled.
fn plugin_fingerprint(source: &Path) -> Result<String, String> {
    use std::hash::{Hash as _, Hasher as _};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for asset in ["manifest.json", "Service.qml"] {
        fs::read(source.join(asset))
            .map_err(|error| format!("cannot read the {asset} plugin asset: {error}"))?
            .hash(&mut hasher);
    }
    Ok(format!("{:016x}", hasher.finish()))
}

fn fingerprint_path() -> Result<PathBuf, String> {
    Ok(state_dir()?.join("plugin.fingerprint"))
}

/// Restarts the Omarchy shell when the plugin code changed, and only then.
///
/// The plugin is symlinked from `/usr`, which the shell's file watcher does
/// not cover, and `rescanPlugins` serves QML it has already compiled. So an
/// upgraded plugin keeps running the old code until the shell restarts. A
/// restart is disruptive enough to be worth avoiding when nothing changed, and
/// silently running stale code is worse than a reload, so the applied
/// fingerprint is recorded and compared.
fn reload_plugin_code(source: &Path) -> Result<(), String> {
    let fingerprint = plugin_fingerprint(source)?;
    let stamp = fingerprint_path()?;
    if fs::read_to_string(&stamp).is_ok_and(|applied| applied.trim() == fingerprint) {
        return Ok(());
    }
    super::run(Command::new("omarchy").args(["restart", "shell"]))?;
    fs::create_dir_all(stamp.parent().ok_or("invalid state path")?)
        .map_err(|error| error.to_string())?;
    write_atomic(&stamp, &format!("{fingerprint}\n"), 0o644)
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

fn remove_obsolete_update_hook() -> Result<(), String> {
    let hook = obsolete_update_hook()?;
    if path_exists(&hook) {
        remove_path(&hook)?;
    }
    Ok(())
}

pub(super) fn reload_hyprland() -> Result<(), String> {
    super::run(Command::new("hyprctl").arg("reload"))?;
    let output = Command::new("hyprctl")
        .arg("configerrors")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "hyprctl configerrors exited with {}",
            output.status
        ));
    }
    let errors = String::from_utf8_lossy(&output.stdout);
    if errors.trim().is_empty() {
        Ok(())
    } else {
        Err(format!("Hyprland rejected the presence binding:\n{errors}"))
    }
}

/// Arms the presence service, so setup leaves a machine that actually answers.
///
/// The path unit is what makes an unenrolled machine usable: the service
/// refuses to run with nothing configured, so it stays enabled but stopped
/// until a config appears. Only an already-enrolled machine is started here.
///
/// Best-effort: a dev checkout with no installed units must stay able to run
/// setup, and a failure here costs the service, not the lock integration that
/// has already landed.
fn enable_service() -> Result<(), String> {
    super::run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    super::run(Command::new("systemctl").args([
        "--user",
        "enable",
        "presenced.path",
        "presenced.service",
    ]))?;
    super::run(Command::new("systemctl").args(["--user", "start", "presenced.path"]))?;
    let enrolled = paths::config_path().is_some_and(|path| path.is_file());
    let unit = if enrolled { "restart" } else { "stop" };
    super::run(Command::new("systemctl").args(["--user", unit, "presenced.service"]))
}

pub fn setup() -> Result<(), String> {
    let source = paths::shell_plugin_source();
    for required in ["manifest.json", "Service.qml"] {
        if !source.join(required).is_file() {
            return Err(format!(
                "presence plugin asset is missing at {}; set OPU_DATADIR or reinstall the package",
                source.join(required).display()
            ));
        }
    }
    super::run(
        Command::new("omarchy")
            .args(["plugin", "validate"])
            .arg(&source),
    )?;

    policy_installed()?;
    install_binding()?;
    install_plugin(&source, &plugin_dir()?)?;

    super::run(Command::new("omarchy-shell").args(["shell", "rescanPlugins"]))?;
    wait_for_plugin(PLUGIN_ID)?;
    super::run(Command::new("omarchy").args(["plugin", "enable", PLUGIN_ID]))?;

    super::run(Command::new("omarchy").args(["plugin", "enable", STOCK_PLUGIN_ID]))?;
    remove_obsolete_update_hook()?;
    reload_hyprland()?;
    // After enabling, so a first install does not restart a shell that is
    // about to load the plugin anyway.
    if let Err(error) = reload_plugin_code(&source) {
        eprintln!("warning: could not reload the Omarchy shell: {error}");
    }
    if let Err(error) = enable_service() {
        eprintln!("warning: could not arm the presence service: {error}");
    }

    println!(
        "Enabled {PLUGIN_ID} beside the stock {STOCK_PLUGIN_ID} lock and installed the Alt hold binding."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_is_appended_once() {
        let source = "o.bind(\"SUPER + T\", \"Terminal\", \"foot\")\n";
        let rendered = render_bindings(source).unwrap();
        assert!(rendered.contains(source));
        assert!(rendered.contains("\"ALT_L\""));
        assert!(rendered.contains("\"ALT_R\""));
        assert!(rendered.contains(
            "o.bind(presence_key, \"Presence unlock\", \"omarchy-shell -q presence-unlock hold\""
        ));
        assert!(rendered.contains(
            "o.bind(\"ALT + \" .. presence_key, \"Presence unlock release\", \"omarchy-shell -q presence-unlock release\""
        ));
        assert!(rendered.contains("ignore_mods = true"));
        assert!(rendered.contains("release = true"));
        assert!(!rendered.contains("long_press"));
        assert_eq!(render_bindings(&rendered).unwrap(), rendered);
    }

    #[test]
    fn stale_managed_binding_is_replaced() {
        let source = format!("before\n{BINDING_START}\nold binding\n{BINDING_END}\nafter\n");
        let rendered = render_bindings(&source).unwrap();
        assert!(!rendered.contains("old binding"));
        assert!(rendered.starts_with("before\n"));
        assert!(rendered.ends_with("\nafter\n"));
    }

    #[test]
    fn malformed_managed_binding_is_rejected() {
        assert!(render_bindings(BINDING_START).is_err());
        assert!(
            render_bindings(&format!("{BINDING_START}\n{BINDING_START}\n{BINDING_END}")).is_err()
        );
    }
}
