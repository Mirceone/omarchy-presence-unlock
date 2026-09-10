//! The Hyprland keybinding that reports the unlock gesture.
//!
//! Hyprland is what can see a key while the session is locked, so the hold
//! starts here: the press and release edges are forwarded to the lock plugin,
//! which times them. The block is written between markers so a user's own
//! bindings file stays theirs, and removal takes back exactly what setup
//! added.

use super::paths;
use crate::atomic::write_atomic;
use std::{fs, process::Command};

pub(super) const BLOCK_START: &str = "-- omarchy-presence-unlock:start";
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
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let rendered = render(&source)?;
    if rendered == source {
        return Ok(());
    }
    fs::create_dir_all(path.parent().ok_or("invalid Hyprland bindings path")?)
        .map_err(|error| error.to_string())?;
    write_atomic(&path, &rendered, 0o644)
}

/// # Errors
///
/// Returns an error when the block is malformed or cannot be written back.
pub(super) fn remove() -> Result<(), String> {
    let path = paths::bindings_path()?;
    let Ok(source) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let (Some(start), Some(end)) = (source.find(BLOCK_START), source.find(BLOCK_END)) else {
        return Ok(());
    };
    if start > end {
        return Err(format!(
            "{} contains a malformed presence-unlock block; remove it by hand",
            path.display()
        ));
    }
    let mut rendered = source.clone();
    rendered.replace_range(start..end + BLOCK_END.len(), "");
    // The block was written with a blank line before it, which would otherwise
    // accumulate every time the integration is installed and removed.
    let trimmed = rendered.trim_end();
    write_atomic(&path, &format!("{trimmed}\n"), 0o644)
}

/// Applies the binding change, and refuses to call it applied if Hyprland
/// rejected the file.
///
/// # Errors
///
/// Returns an error when Hyprland reports a configuration error.
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
}
