//! The Hyprland keybinding that reports the unlock gesture.
//!
//! Hyprland is what can see a key while the session is locked, so the hold
//! starts here: the press and release edges are forwarded to the lock plugin,
//! which times them. The block is written between markers so a user's own
//! bindings file stays theirs, and removal takes back exactly what setup
//! added.

use super::paths;
use crate::atomic::update_atomic;
use std::process::Command;

pub(crate) const BLOCK_START: &str = "-- omarchy-presence-unlock:start";
pub(super) const BLOCK_END: &str = "-- omarchy-presence-unlock:end";
// The press edge ignores mods because Hyprland has not yet folded Alt into the
// modmask at Alt's own press. The release edge needs the target modmask plus
// `release`; the lock plugin, rather than Hyprland's `long_press`, owns the
// hold duration. Both Alt keys are bound so either hand works.
const BLOCK: &str = r#"-- omarchy-presence-unlock:start
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

fn render(source: &str) -> Result<String, String> {
    let starts = source.match_indices(BLOCK_START).collect::<Vec<_>>();
    let ends = source.match_indices(BLOCK_END).collect::<Vec<_>>();
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => {
            let mut output = source.to_string();
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            if !output.is_empty() && !output.ends_with("\n\n") {
                output.push('\n');
            }
            output.push_str(BLOCK);
            output.push('\n');
            Ok(output)
        }
        ([(start, _)], [(end, _)]) if start < end => {
            let end = end + BLOCK_END.len();
            let mut output = source.to_string();
            output.replace_range(*start..end, BLOCK);
            Ok(output)
        }
        _ => Err(format!(
            "{} contains an incomplete or duplicate presence-unlock block",
            paths::bindings_path()?.display()
        )),
    }
}

/// # Errors
///
/// Returns an error when the bindings file cannot be read or written.
pub(super) fn install() -> Result<(), String> {
    let path = paths::bindings_path()?;
    update_atomic(&path, "", 0o644, render)
}

/// # Errors
///
/// Returns an error when the block is malformed or cannot be written back.
pub(super) fn remove() -> Result<(), String> {
    let path = paths::bindings_path()?;
    update_atomic(&path, "", 0o644, |source| {
        let (Some(start), Some(end)) = (source.find(BLOCK_START), source.find(BLOCK_END)) else {
            return Ok(source.to_string());
        };
        if start > end {
            return Err(format!(
                "{} contains a malformed presence-unlock block; remove it by hand",
                path.display()
            ));
        }
        let mut rendered = source.to_string();
        rendered.replace_range(start..end + BLOCK_END.len(), "");
        // The block was written with a blank line before it, which would otherwise
        // accumulate every time the integration is installed and removed.
        Ok(format!("{}\n", rendered.trim_end()))
    })
}

/// What Hyprland currently reports as wrong with the user's config.
///
/// # Errors
///
/// Returns an error when `hyprctl` cannot be run, e.g. outside a session.
pub(super) fn config_errors() -> Result<String, String> {
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
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Applies the binding change, and refuses to call it applied if Hyprland
/// reports an error that was not there before the edit.
///
/// `configerrors` covers the whole config, so an unrelated, pre-existing
/// mistake elsewhere must not be blamed on this binding.
///
/// # Errors
///
/// Returns an error when the reload fails or introduced a new error.
pub(super) fn reload_hyprland(baseline: &str) -> Result<(), String> {
    super::run(Command::new("hyprctl").arg("reload"))?;
    let after = config_errors()?;
    let introduced = new_errors(baseline, &after);
    if introduced.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Hyprland reported new configuration errors:\n{}",
            introduced.join("\n")
        ))
    }
}

fn new_errors<'a>(before: &str, after: &'a str) -> Vec<&'a str> {
    after
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !before.lines().any(|old| old.trim() == *line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both edges, both Alt keys, and the flags that make the gesture work
    /// on a locked session. `long_press` is deliberately absent: the hold is
    /// timed by the lock plugin.
    #[test]
    fn the_binding_is_appended_once_and_reports_both_edges() {
        let source = "o.bind(\"SUPER + T\", \"Terminal\", \"foot\")\n";
        let rendered = render(source).unwrap();

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
        assert_eq!(render(&rendered).unwrap(), rendered);
    }

    #[test]
    fn a_stale_managed_block_is_replaced_in_place() {
        let source = format!("before\n{BLOCK_START}\nold binding\n{BLOCK_END}\nafter\n");
        let rendered = render(&source).unwrap();

        assert!(!rendered.contains("old binding"));
        assert!(rendered.starts_with("before\n"));
        assert!(rendered.ends_with("\nafter\n"));
    }

    #[test]
    fn a_malformed_managed_block_is_rejected() {
        assert!(render(BLOCK_START).is_err());
        assert!(render(&format!("{BLOCK_START}\n{BLOCK_START}\n{BLOCK_END}")).is_err());
    }

    /// An error already present before the edit belongs to the user, not to
    /// this binding; only what the edit introduced may fail setup.
    #[test]
    fn only_errors_introduced_by_the_edit_count() {
        let before = "error in monitors.lua line 3\n";
        assert!(new_errors(before, "error in monitors.lua line 3\n").is_empty());
        assert_eq!(
            new_errors(
                before,
                "error in monitors.lua line 3\nerror in bindings.lua line 40\n"
            ),
            ["error in bindings.lua line 40"]
        );
    }
}
