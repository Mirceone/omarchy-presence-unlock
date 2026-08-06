//! Enrollment: comment-preserving edits to the `[[device]]` array in config.toml.

use crate::atomic::write_atomic;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use omarchy_watch_unlock_protocol::{
    ble::parse_address,
    config::{CURRENT_SCHEMA, ConfigFile},
    paths,
};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};
use uuid::Uuid;

/// The identity criteria a device may be enrolled with. At least one is required.
#[derive(Default)]
pub struct Criteria {
    pub irk_base64: Option<String>,
    pub address: Option<String>,
    pub service_uuid: Option<String>,
    pub name_prefix: Option<String>,
}

impl Criteria {
    fn is_empty(&self) -> bool {
        self.irk_base64.is_none()
            && self.address.is_none()
            && self.service_uuid.is_none()
            && self.name_prefix.is_none()
    }

    /// Rejects unusable criteria before anything is written: a config that fails
    /// to resolve leaves the daemon refusing to start.
    fn validate(&self) -> Result<(), String> {
        if self.is_empty() {
            return Err(
                "a device needs at least one of --irk, --address, --service-uuid, or --name-prefix"
                    .into(),
            );
        }
        if let Some(irk) = &self.irk_base64 {
            let raw = STANDARD
                .decode(irk.trim())
                .map_err(|_| "IRK is not valid base64".to_string())?;
            if raw.len() != 16 {
                return Err("IRK must decode to exactly 16 bytes".into());
            }
        }
        if let Some(address) = &self.address {
            parse_address(address).map_err(|error| error.to_string())?;
        }
        if let Some(uuid) = &self.service_uuid {
            Uuid::parse_str(uuid).map_err(|_| format!("invalid service UUID: {uuid}"))?;
        }
        Ok(())
    }
}

pub struct Overrides {
    pub threshold_dbm: Option<i16>,
    pub minimum_samples: Option<u8>,
    pub freshness_ms: Option<u64>,
}

fn config_path() -> Result<PathBuf, String> {
    paths::config_path().ok_or_else(|| "XDG_CONFIG_HOME or HOME is required".to_string())
}

/// Reads config.toml for editing, or starts a fresh schema-2 document.
fn open() -> Result<DocumentMut, String> {
    let path = config_path()?;
    if !path.exists() {
        let mut document = DocumentMut::new();
        document["schema_version"] = value(i64::from(CURRENT_SCHEMA));
        document["unlock_backend"] = value("disabled");
        return Ok(document);
    }
    fs::read_to_string(&path)
        .map_err(|error| error.to_string())?
        .parse::<DocumentMut>()
        .map_err(|error| format!("cannot edit {}: {error}", path.display()))
}

/// Writes config.toml 0600 through a temp file, after checking it still resolves.
fn save(document: &DocumentMut) -> Result<(), String> {
    let text = document.to_string();
    ConfigFile::parse(&text)
        .map_err(|error| format!("refusing to write an unusable config: {error}"))?
        .resolve()
        .map_err(|error| format!("refusing to write an unusable config: {error}"))?;
    let directory = paths::config_dir().ok_or("XDG_CONFIG_HOME or HOME is required")?;
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    write_atomic(&config_path()?, &text, 0o600)
}

/// Migrates configuration through schema 3 while preserving comments.
///
/// Schema 1's top-level Watch becomes a device; schema 2's `kind` field becomes
/// the canonical profile id resolved through the compile-time registry.
fn migrate(document: &mut DocumentMut) -> Result<(), String> {
    let schema = document
        .get("schema_version")
        .and_then(Item::as_integer)
        .unwrap_or(i64::from(CURRENT_SCHEMA));
    if schema < 2
        && let Some(irk) = document
            .get("irk_base64")
            .and_then(Item::as_str)
            .map(str::to_owned)
    {
        let threshold = document
            .get("unlock_threshold_dbm")
            .and_then(Item::as_integer);
        document.remove("irk_base64");
        document.remove("unlock_threshold_dbm");
        let table = upsert(document, "watch")?;
        table["profile"] = value("apple-continuity");
        table["irk_base64"] = value(irk);
        if let Some(threshold) = threshold {
            table["threshold_dbm"] = value(threshold);
        }
    }
    if schema < 3 {
        for table in device_array(document)?.iter_mut() {
            let legacy = table.get("kind").and_then(Item::as_str).map(str::to_owned);
            if let Some(legacy) = legacy {
                let canonical = omarchy_watch_unlock_protocol::profile::find(&legacy)
                    .map_or(legacy.as_str(), |profile| profile.id());
                table["profile"] = value(canonical);
                table.remove("kind");
            }
        }
    }
    document["schema_version"] = value(i64::from(CURRENT_SCHEMA));
    Ok(())
}

/// Criteria keys `apply_device` owns; cleared on every update so a re-enrollment
/// never inherits a stale AND-combined criterion from the previous one.
const CRITERIA_KEYS: [&str; 4] = ["irk_base64", "address", "service_uuid", "name_prefix"];
const POLICY_KEYS: [&str; 3] = ["threshold_dbm", "minimum_samples", "freshness_ms"];

/// The user-facing name of the config file, for error messages.
fn config_path_display() -> String {
    config_path().map_or_else(
        |_| "the config file".to_string(),
        |p| p.display().to_string(),
    )
}

fn wrong_device_shape(item: &Item) -> String {
    format!(
        "config key `device` must be a sequence of [[device]] tables, found {}; fix {} by hand",
        item.type_name(),
        config_path_display()
    )
}

/// Borrows the `[[device]]` array, creating it when absent.
///
/// A `device` key of any other shape is a hand-editing mistake the user must
/// resolve; the one exception is an empty inline `device = []`, whose intent is
/// unambiguous and which is replaced in place.
fn device_array(document: &mut DocumentMut) -> Result<&mut ArrayOfTables, String> {
    if document
        .get("device")
        .and_then(Item::as_array)
        .is_some_and(toml_edit::Array::is_empty)
    {
        // Drop the key outright: replacing the Item in place would keep the
        // inline value's decor and render as `device = []` again.
        document.remove("device");
    }
    let item = document
        .entry("device")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    if item.as_array_of_tables().is_none() {
        return Err(wrong_device_shape(item));
    }
    item.as_array_of_tables_mut()
        .ok_or_else(|| "`device` is not an array of tables".to_string())
}

/// Returns the `[[device]]` table with this id, appending one if absent.
fn upsert<'a>(document: &'a mut DocumentMut, id: &str) -> Result<&'a mut Table, String> {
    let devices = device_array(document)?;
    let existing = devices
        .iter()
        .position(|table| table.get("id").and_then(Item::as_str) == Some(id));
    let index = if let Some(index) = existing {
        index
    } else {
        let mut table = Table::new();
        table["id"] = value(id);
        devices.push(table);
        devices.len() - 1
    };
    devices
        .get_mut(index)
        .ok_or_else(|| "`device` array changed while being edited".to_string())
}

/// Writes one device as a full replacement of any table already carrying `id`.
fn apply_device(
    document: &mut DocumentMut,
    id: &str,
    profile: &str,
    criteria: &Criteria,
    overrides: &Overrides,
) -> Result<(), String> {
    criteria.validate()?;
    let table = upsert(document, id)?;
    table["profile"] = value(profile);
    table.remove("kind");
    for key in CRITERIA_KEYS.iter().chain(&POLICY_KEYS) {
        table.remove(key);
    }
    for (key, new) in [
        ("irk_base64", criteria.irk_base64.as_deref()),
        ("address", criteria.address.as_deref()),
        ("service_uuid", criteria.service_uuid.as_deref()),
        ("name_prefix", criteria.name_prefix.as_deref()),
    ] {
        if let Some(new) = new {
            table[key] = value(new);
        }
    }
    if let Some(threshold) = overrides.threshold_dbm {
        table["threshold_dbm"] = value(i64::from(threshold));
    }
    if let Some(samples) = overrides.minimum_samples {
        table["minimum_samples"] = value(i64::from(samples));
    }
    if let Some(freshness) = overrides.freshness_ms {
        table["freshness_ms"] = value(i64::try_from(freshness).unwrap_or(i64::MAX));
    }
    Ok(())
}

/// Adds or updates one device. Updating replaces the whole device definition:
/// criteria are AND-combined, so a leftover key would silently stop it matching.
///
/// # Errors
///
/// Returns an error for unusable criteria, a malformed `device` key, or when the
/// resulting config would not resolve.
pub fn add(
    id: &str,
    profile: &str,
    criteria: &Criteria,
    overrides: &Overrides,
) -> Result<(), String> {
    let mut document = open()?;
    migrate(&mut document)?;
    apply_device(&mut document, id, profile, criteria, overrides)?;
    save(&document)
}

/// Retunes how close one enrolled device must be, leaving everything else
/// about it alone.
///
/// Unlike [`add`], this is an edit rather than a replacement: the identity
/// criteria are exactly what the device was enrolled with and must survive a
/// sensitivity change untouched.
///
/// # Errors
///
/// Returns an error when no such device is configured, or when the result
/// would not resolve.
pub fn set_threshold(id: &str, threshold_dbm: i16) -> Result<(), String> {
    let mut document = open()?;
    migrate(&mut document)?;
    let devices = device_array(&mut document)?;
    let table = devices
        .iter_mut()
        .find(|table| table.get("id").and_then(Item::as_str) == Some(id))
        .ok_or_else(|| format!("no device named {id} is configured"))?;
    table["threshold_dbm"] = value(i64::from(threshold_dbm));
    save(&document)
}

/// Removes one device by id.
///
/// # Errors
///
/// Returns an error when no such device is configured, or when removing it
/// would leave a config that cannot resolve.
pub fn remove(id: &str) -> Result<(), String> {
    let mut document = open()?;
    migrate(&mut document)?;
    if document.get("device").is_none() {
        return Err(format!("no device named {id} is configured"));
    }
    let devices = device_array(&mut document)?;
    let before = devices.len();
    devices.retain(|table| table.get("id").and_then(Item::as_str) != Some(id));
    if devices.len() == before {
        return Err(format!("no device named {id} is configured"));
    }
    save(&document)
}

/// Sets the quorum expression (`any`, `all`, or `at-least:<n>`).
///
/// # Errors
///
/// Returns an error for an expression this build does not understand.
pub fn set_quorum(expression: &str) -> Result<(), String> {
    let mut document = open()?;
    migrate(&mut document)?;
    document["quorum"] = value(expression);
    save(&document)
}

/// Validates a backend selection, then writes exactly the keys it needs.
///
/// Every backend key is cleared first: a stale `unlock_command` left behind by a
/// previous `command` backend would otherwise survive a switch.
fn apply_backend(
    document: &mut DocumentMut,
    name: &str,
    process: Option<&str>,
    signal: Option<&str>,
    command: &[String],
) -> Result<(), String> {
    // Validate before mutating so a rejected switch leaves the document untouched.
    if !matches!(
        name,
        "disabled" | "hyprlock-confirm" | "command" | "process-signal"
    ) {
        return Err(format!(
            "unknown backend {name}; use disabled, hyprlock-confirm, process-signal, or command"
        ));
    }
    if name == "command" && command.is_empty() {
        return Err(
            "backend command requires an argv, for example: backend command -- loginctl unlock-session"
                .into(),
        );
    }
    if name == "process-signal" {
        if process.is_none() {
            return Err(
                "backend process-signal requires --process <name>, for example --process swaylock"
                    .into(),
            );
        }
        if !matches!(signal, None | Some("SIGUSR1" | "SIGUSR2")) {
            return Err("unlock signal must be SIGUSR1 or SIGUSR2".into());
        }
    }
    document["unlock_backend"] = value(name);
    document.remove("unlock_command");
    document.remove("unlock_process");
    document.remove("unlock_signal");
    match name {
        "command" => {
            let mut argv = Array::new();
            for argument in command {
                argv.push(argument.as_str());
            }
            document["unlock_command"] = value(argv);
        }
        "process-signal" => {
            document["unlock_process"] = value(process.unwrap_or_default());
            document["unlock_signal"] = value(signal.unwrap_or("SIGUSR1"));
        }
        _ => {}
    }
    Ok(())
}

/// Selects the unlock backend and the parameters that backend requires.
///
/// # Errors
///
/// Returns an error when the backend or its parameters are unusable.
pub fn set_backend(
    name: &str,
    process: Option<&str>,
    signal: Option<&str>,
    command: &[String],
) -> Result<(), String> {
    let mut document = open()?;
    migrate(&mut document)?;
    apply_backend(&mut document, name, process, signal, command)?;
    save(&document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    #[test]
    fn migration_moves_a_schema_1_watch_into_the_device_array() {
        let mut doc = document(
            "schema_version = 1\nirk_base64 = \"AAAA\"\nunlock_threshold_dbm = -55\nunlock_backend = \"hyprlock-confirm\"\n",
        );
        migrate(&mut doc).unwrap();
        let text = doc.to_string();
        assert!(text.contains("schema_version = 3"));
        let reparsed = ConfigFile::parse(&text).unwrap();
        assert!(
            reparsed.irk_base64.is_none() && reparsed.unlock_threshold_dbm.is_none(),
            "schema-1 keys survived: {text}"
        );
        assert_eq!(reparsed.devices.len(), 1);
        assert!(text.contains("[[device]]"));
        assert!(text.contains("id = \"watch\""));
        assert!(text.contains("profile = \"apple-continuity\""));
        assert!(text.contains("threshold_dbm = -55"));
        // Unrelated keys are preserved.
        assert!(text.contains("unlock_backend = \"hyprlock-confirm\""));
    }

    #[test]
    fn migration_is_idempotent_and_canonicalizes_schema_2_profiles() {
        let mut doc = document(
            "schema_version = 2\n\n[[device]]\nid = \"watch\"\nkind = \"ble\"\naddress = \"AA:BB:CC:DD:EE:FF\"\n",
        );
        migrate(&mut doc).unwrap();
        migrate(&mut doc).unwrap();
        let text = doc.to_string();
        assert_eq!(text.matches("[[device]]").count(), 1);
        assert!(text.contains("schema_version = 3"));
        assert!(text.contains("profile = \"presence\""));
        assert!(!text.contains("kind ="));
    }

    #[test]
    fn upsert_updates_in_place_rather_than_appending_a_duplicate() {
        let mut doc = document(
            "schema_version = 3\n\n[[device]]\nid = \"watch\"\nprofile = \"apple-continuity\"\nirk_base64 = \"AAAA\"\n",
        );
        upsert(&mut doc, "watch").unwrap()["profile"] = value("presence");
        upsert(&mut doc, "fob").unwrap()["profile"] = value("presence");
        let text = doc.to_string();
        assert_eq!(text.matches("[[device]]").count(), 2);
        assert!(text.contains("id = \"fob\""));
        assert!(!text.contains("apple-watch"));
    }

    #[test]
    fn comments_survive_an_edit() {
        let mut doc = document(
            "# hand written\nschema_version = 3\n\n[[device]]\nid = \"watch\"\nprofile = \"presence\"\naddress = \"AA:BB:CC:DD:EE:FF\"\n",
        );
        upsert(&mut doc, "watch").unwrap()["threshold_dbm"] = value(-60);
        assert!(doc.to_string().contains("# hand written"));
    }

    #[test]
    fn criteria_must_be_present_and_well_formed() {
        assert!(Criteria::default().validate().is_err());
        assert!(
            Criteria {
                address: Some("nonsense".into()),
                ..Criteria::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Criteria {
                irk_base64: Some("AAAA".into()),
                ..Criteria::default()
            }
            .validate()
            .is_err(),
            "a short IRK must be refused before it is written"
        );
        assert!(
            Criteria {
                address: Some("AA:BB:CC:DD:EE:FF".into()),
                ..Criteria::default()
            }
            .validate()
            .is_ok()
        );
    }

    const IRK: &str = "m305CqYQEDQFrchXozQC7A==";

    fn no_overrides() -> Overrides {
        Overrides {
            threshold_dbm: None,
            minimum_samples: None,
            freshness_ms: None,
        }
    }

    #[test]
    fn updating_a_device_replaces_its_whole_definition() {
        let mut doc = document(
            "schema_version = 3\n\n[[device]]\nid = \"watch\"\nprofile = \"apple-continuity\"\nname_prefix = \"Apple\"\naddress = \"AA:BB:CC:DD:EE:FF\"\nthreshold_dbm = -70\n",
        );
        apply_device(
            &mut doc,
            "watch",
            "apple-continuity",
            &Criteria {
                irk_base64: Some(IRK.into()),
                ..Criteria::default()
            },
            &no_overrides(),
        )
        .unwrap();
        let text = doc.to_string();
        assert_eq!(
            text.matches("[[device]]").count(),
            1,
            "appended instead of replacing: {text}"
        );
        assert!(text.contains("irk_base64"));
        // Criteria are AND-combined, so any survivor would silently break matching.
        assert!(
            !text.contains("name_prefix"),
            "stale criterion survived: {text}"
        );
        assert!(
            !text.contains("address"),
            "stale criterion survived: {text}"
        );
        assert!(
            !text.contains("threshold_dbm"),
            "stale override survived: {text}"
        );
    }

    #[test]
    fn a_device_key_of_the_wrong_shape_is_an_error_not_a_panic() {
        for bad in ["device = \"x\"", "device = 5", "device = [1, 2]"] {
            let mut doc = document(&format!("schema_version = 3\n{bad}\n"));
            let error = upsert(&mut doc, "watch").unwrap_err();
            assert!(
                error.contains("must be a sequence of [[device]] tables"),
                "unexpected error for {bad}: {error}"
            );
        }
        // An empty inline array is unambiguous, and nothing is lost by replacing it.
        let mut doc = document("schema_version = 3\ndevice = []\n");
        upsert(&mut doc, "watch").unwrap()["profile"] = value("presence");
        assert!(doc.to_string().contains("[[device]]"));
    }

    #[test]
    fn switching_backends_never_leaves_the_previous_one_s_keys() {
        let mut doc = document(
            "schema_version = 3\nunlock_backend = \"command\"\nunlock_command = [\"loginctl\", \"unlock-session\"]\n",
        );
        apply_backend(&mut doc, "hyprlock-confirm", None, None, &[]).unwrap();
        let text = doc.to_string();
        assert!(text.contains("unlock_backend = \"hyprlock-confirm\""));
        assert!(
            !text.contains("unlock_command"),
            "stale argv survived: {text}"
        );
    }

    #[test]
    fn process_signal_writes_the_keys_the_config_parser_requires() {
        let mut doc = document("schema_version = 2\nunlock_backend = \"disabled\"\n");
        apply_backend(&mut doc, "process-signal", Some("swaylock"), None, &[]).unwrap();
        let text = doc.to_string();
        assert!(text.contains("unlock_process = \"swaylock\""));
        assert!(text.contains("unlock_signal = \"SIGUSR1\""));
        assert_eq!(
            ConfigFile::parse(&text).unwrap().backend().unwrap(),
            omarchy_watch_unlock_protocol::config::Backend::ProcessSignal {
                process: "swaylock".into(),
                signal: omarchy_watch_unlock_protocol::config::SignalKind::Usr1,
            }
        );
    }

    #[test]
    fn a_rejected_backend_leaves_the_document_untouched() {
        let before =
            "schema_version = 2\nunlock_backend = \"command\"\nunlock_command = [\"loginctl\"]\n";
        for (name, process, signal) in [
            ("process-signal", None, None),
            ("process-signal", Some("swaylock"), Some("SIGKILL")),
            ("command", None, None),
            ("nonsense", None, None),
        ] {
            let mut doc = document(before);
            assert!(
                apply_backend(&mut doc, name, process, signal, &[]).is_err(),
                "{name} was accepted"
            );
            assert_eq!(doc.to_string(), before, "{name} mutated the document");
        }
    }
}
