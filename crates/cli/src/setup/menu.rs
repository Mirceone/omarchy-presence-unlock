//! The Omarchy menu rows for this integration.
//!
//! Fingerprint is the pattern followed here: one row under **Setup →
//! Security** that applies it, and one under **Remove → Security** that
//! takes it away. The setup row becomes the way in afterwards too — once the
//! integration is applied it opens the interactive CLI, which already knows
//! how to enroll, manage devices, and diagnose. A submenu mirroring that
//! surface would be a second copy to keep in sync.
//!
//! The menu extension file belongs to the user and is mostly comments in its
//! stock form, so it is edited as text between markers rather than reparsed
//! and rewritten, which would throw those comments away.

use super::paths;
use crate::atomic::write_atomic;
use std::fs;

pub(super) const BLOCK_START: &str = "// omarchy-presence-unlock:menu:start";
pub(super) const BLOCK_END: &str = "// omarchy-presence-unlock:menu:end";

pub(super) const SETUP_ENTRY_ID: &str = "setup.security.presence";
pub(super) const MANAGE_ENTRY_ID: &str = "setup.security.presence-manage";
pub(super) const REMOVE_ENTRY_ID: &str = "remove.security.presence";

const ICON: &str = "\u{f0990}";

/// Whether the lock plugin is installed, as a shell condition the menu can
/// evaluate. The rows are complementary on it, so exactly one appears under
/// Setup: apply it, or open it.
const APPLIED: &str = "[ -d \\\"$HOME/.config/omarchy/plugins/${USER}.lock\\\" ]";

fn row(id: &str, label: &str, description: &str, when: &str, command: &str) -> String {
    format!(
        "  \"{id}\": {{\"icon\":\"{ICON}\",\"label\":\"{label}\",\"description\":\"{description}\",\"when\":\"{when}\",\"action\":\"omarchy-launch-floating-terminal-with-presentation {command}\"}}"
    )
}

fn block() -> String {
    let installed = "command -v omarchy-presence-unlock >/dev/null";
    let rows = [
        row(
            SETUP_ENTRY_ID,
            "Presence Unlock",
            "Set up unlocking with a trusted Bluetooth device",
            &format!("{installed} && ! {APPLIED}"),
            "omarchy-presence-unlock setup",
        ),
        row(
            MANAGE_ENTRY_ID,
            "Presence Unlock",
            "Enroll a trusted device, manage presence unlock, and run diagnostics",
            &format!("{installed} && {APPLIED}"),
            "omarchy-presence-unlock",
        ),
        row(
            REMOVE_ENTRY_ID,
            "Presence Unlock",
            "Remove presence unlock and restore Omarchy's own lock screen",
            &format!("{installed} && {APPLIED}"),
            "omarchy-presence-unlock uninstall",
        ),
    ];
    format!("  {BLOCK_START}\n{}\n  {BLOCK_END}\n", rows.join(",\n"))
}

/// The stock file Omarchy ships when the user has none: an empty object,
/// documented in comments. Written only when nothing is there to edit.
const EMPTY_EXTENSIONS: &str = "{\n}\n";

/// Index of the last character that is neither whitespace nor comment.
///
/// JSONC means the byte before the closing brace is usually the end of a
/// comment, not the end of an entry, so "does the previous entry need a
/// comma" cannot be answered by looking at raw characters.
fn last_meaningful(source: &str) -> Option<(usize, char)> {
    let mut found = None;
    let mut chars = source.char_indices().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some((index, character)) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
                found = Some((index, character));
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                found = Some((index, character));
            }
            '/' if chars.peek().map(|(_, next)| *next) == Some('/') => {
                for (_, skipped) in chars.by_ref() {
                    if skipped == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek().map(|(_, next)| *next) == Some('*') => {
                let mut previous = ' ';
                for (_, skipped) in chars.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }
                    previous = skipped;
                }
            }
            character if character.is_whitespace() => {}
            character => found = Some((index, character)),
        }
    }
    found
}

/// Removes the managed block, and the comma that only existed to separate it.
pub(super) fn render_removal(source: &str) -> Result<String, String> {
    let (Some(start), Some(end)) = (source.find(BLOCK_START), source.find(BLOCK_END)) else {
        return Ok(source.to_string());
    };
    if start > end {
        return Err(
            "the presence menu block is malformed; remove it from omarchy-menu.jsonc by hand"
                .to_string(),
        );
    }

    let mut rendered = source.to_string();
    // Take the whole lines the markers sit on, so removal does not leave the
    // indentation they were written with behind.
    let line_start = rendered[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = rendered[end..]
        .find('\n')
        .map_or(rendered.len(), |index| end + index + 1);
    rendered.replace_range(line_start..line_end, "");

    // The entry before ours kept a comma only because ours followed it, so
    // it is now a trailing comma before the closing brace.
    if let Some(close) = rendered.rfind('}')
        && let Some((index, ',')) = last_meaningful(&rendered[..close])
    {
        rendered.remove(index);
    }
    Ok(rendered)
}

/// Appends the managed block as the last entry of the root object.
///
/// Always last, and always rewritten from scratch, so the comma bookkeeping
/// has one shape to get right instead of one per position the block could
/// have been left in by an earlier version.
pub(super) fn render_entry(source: &str) -> Result<String, String> {
    let mut rendered = render_removal(source)?;

    let close = rendered
        .rfind('}')
        .ok_or("omarchy-menu.jsonc has no closing brace; fix it by hand and rerun setup")?;
    let head = &rendered[..close];
    if let Some((index, character)) = last_meaningful(head)
        && character != '{'
        && character != ','
    {
        rendered.insert(index + character.len_utf8(), ',');
    }

    let close = rendered
        .rfind('}')
        .ok_or("omarchy-menu.jsonc has no closing brace; fix it by hand and rerun setup")?;
    let line_start = rendered[..close]
        .rfind('\n')
        .map_or(close, |index| index + 1);
    rendered.insert_str(line_start, &block());
    Ok(rendered)
}

/// # Errors
///
/// Returns an error when the menu extension cannot be read or written.
pub(super) fn install() -> Result<(), String> {
    let path = paths::menu_extensions_path()?;
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EMPTY_EXTENSIONS.to_string(),
        Err(error) => return Err(error.to_string()),
    };
    let rendered = render_entry(&source)?;
    if rendered == source && path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path.parent().ok_or("invalid menu extension path")?)
        .map_err(|error| error.to_string())?;
    write_atomic(&path, &rendered, 0o644)
}

/// # Errors
///
/// Returns an error when the menu extension cannot be rewritten.
pub(super) fn remove() -> Result<(), String> {
    let path = paths::menu_extensions_path()?;
    let Ok(source) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let rendered = render_removal(&source)?;
    if rendered == source {
        return Ok(());
    }
    write_atomic(&path, &rendered, 0o644)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(rendered: &str) -> serde_json::Value {
        // The menu file is JSONC; strip line comments the way its parser does
        // before asking whether what is left is valid JSON.
        let stripped = rendered
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        serde_json::from_str(&stripped).expect("rendered menu must be valid JSON")
    }

    /// The stock file is an object of nothing but comments, so the first
    /// entry must not inherit a comma from them. All three rows land, and
    /// the two Setup rows are complementary so only one is ever offered.
    #[test]
    fn the_rows_land_in_a_commented_stock_file() {
        let stock = "{\n  // Extend the menu with JSONC.\n  // \"personal\": {\"label\":\"Personal\"},\n}\n";
        let rendered = render_entry(stock).unwrap();

        let parsed = parse(&rendered);
        assert_eq!(parsed.as_object().unwrap().len(), 3);
        assert!(
            parsed[SETUP_ENTRY_ID]["action"]
                .as_str()
                .unwrap()
                .ends_with("omarchy-presence-unlock setup")
        );
        assert!(
            parsed[MANAGE_ENTRY_ID]["action"]
                .as_str()
                .unwrap()
                .ends_with("omarchy-presence-unlock")
        );
        assert!(
            parsed[REMOVE_ENTRY_ID]["action"]
                .as_str()
                .unwrap()
                .ends_with("omarchy-presence-unlock uninstall")
        );

        // Setting up and managing are the same row to a user: exactly one is
        // visible, decided by whether the integration is applied.
        let setup_when = parsed[SETUP_ENTRY_ID]["when"].as_str().unwrap();
        let manage_when = parsed[MANAGE_ENTRY_ID]["when"].as_str().unwrap();
        assert_eq!(setup_when.replace("&& ! ", "&& "), manage_when);
        // Removal is only offered once there is something to remove.
        assert_eq!(parsed[REMOVE_ENTRY_ID]["when"], manage_when);
        assert!(rendered.contains("// Extend the menu with JSONC."));
    }

    /// A user with their own entries must keep them, and the file must stay
    /// parseable — which is what the added comma is for.
    #[test]
    fn an_existing_entry_keeps_its_place_and_the_file_parses() {
        let existing = "{\n  \"personal\": {\"label\":\"Personal\"}\n}\n";
        let rendered = render_entry(existing).unwrap();

        let parsed = parse(&rendered);
        assert_eq!(parsed["personal"]["label"], "Personal");
        assert!(parsed[SETUP_ENTRY_ID].is_object());
    }

    /// Setup reruns on every update, and removal has to leave the file the
    /// way it was found.
    #[test]
    fn installing_twice_and_removing_restores_the_original() {
        let original = "{\n  \"personal\": {\"label\":\"Personal\"}\n}\n";
        let once = render_entry(original).unwrap();
        let twice = render_entry(&once).unwrap();
        assert_eq!(once, twice);

        let removed = render_removal(&twice).unwrap();
        assert_eq!(parse(&removed), parse(original));
        assert!(!removed.contains(BLOCK_START));
        assert!(removed.contains("\"personal\""));
    }

    /// A comma or brace inside a comment or a string is not the end of an
    /// entry, and treating it as one produces a file the shell cannot read.
    #[test]
    fn commas_and_braces_in_comments_and_strings_are_not_entries() {
        let tricky = "{\n  \"note\": {\"label\":\"a } and a , inside\"}\n  // trailing comment with , and }\n}\n";
        let rendered = render_entry(tricky).unwrap();

        let parsed = parse(&rendered);
        assert_eq!(parsed["note"]["label"], "a } and a , inside");
        assert!(parsed[SETUP_ENTRY_ID].is_object());
    }
}
