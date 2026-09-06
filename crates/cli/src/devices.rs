//! Enrollment: comment-preserving edits to the `[[device]]` array in config.toml.

use crate::atomic::write_atomic;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use omarchy_presence_unlock_protocol::{
    ble::parse_address,
    config::{CURRENT_SCHEMA, ConfigFile},
    paths,
};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};
use uuid::Uuid;

/// True when this text is a Bluetooth address rather than a name.
///
/// `BlueZ` synthesises an alias from the address for a peer it has no name
/// for, in dashed form, so this is what keeps `63-6a-1a-5c-e5-83` from being
/// treated as something a user would recognise.
#[must_use]
pub fn looks_like_address(text: &str) -> bool {
    let mut octets = 0;
    for field in text.split(['-', ':']) {
        if field.len() != 2 || !field.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        octets += 1;
    }
    octets == 6
}

/// Derives a usable device id from a name, so the id reads as the device
/// rather than as the flow that enrolled it. Anything that is not
/// alphanumeric collapses to a single dash.
fn slug(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The device id is the config's primary key, not a label: [`add`] upserts on
/// it, `status` prints it, and removal addresses devices by it. It is the
/// app's to choose — the user is never asked.
///
/// An entry already holding this address keeps its id, because that is the
/// same hardware being re-registered rather than a new device. Otherwise the
/// device's own name provides the id, falling back to the address, and a
/// numeric suffix is appended until it is unique so enrolling something new
/// can never silently replace something enrolled.
#[must_use]
pub fn derive_id(name: Option<&str>, address: Option<&str>) -> String {
    let entries = ConfigFile::load()
        .map(|config| config.devices)
        .unwrap_or_default();
    if let Some(address) = address
        && let Some(existing) = entries.iter().find(|entry| {
            entry
                .address
                .as_deref()
                .is_some_and(|known| known.eq_ignore_ascii_case(address))
        })
    {
        return existing.id.clone();
    }

    let taken: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
    let base = name
        .and_then(slug)
        .or_else(|| address.and_then(slug))
        .unwrap_or_else(|| "device".to_string());
    if !taken.contains(&base.as_str()) {
        return base;
    }
    // N enrolled ids can block at most N of the N+1 candidates in this range,
    // so one is always free and the fallback is unreachable.
    (2..=taken.len() + 2)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .unwrap_or(base)
}

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

/// Reads `config.toml` for editing, or starts a fresh current-schema document.
fn open() -> Result<DocumentMut, String> {
    let path = config_path()?;
    if !path.exists() {
        let mut document = DocumentMut::new();
        document["schema_version"] = value(i64::from(CURRENT_SCHEMA));
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

/// Sets how many enrolled devices must authorize an unlock.
///
/// # Errors
///
/// Returns an error for a rule this build does not understand.
pub fn set_multi_device_auth(expression: &str) -> Result<(), String> {
    let mut document = open()?;
    document["multi_device_auth"] = value(expression);
    save(&document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    /// The bug this guards: `BlueZ` reports an unnamed peer's alias as its own
    /// address, which was enrolled verbatim as `63-6a-1a-5c-e5-83`.
    #[test]
    fn an_address_is_never_mistaken_for_a_device_name() {
        assert!(looks_like_address("63:6A:1A:5C:E5:83"));
        assert!(looks_like_address("63-6a-1a-5c-e5-83"));
        assert!(!looks_like_address("Mirceone\u{2019}s iPhone"));
        assert!(!looks_like_address("Govee_H605C_427D"));
        assert!(!looks_like_address("63:6A:1A:5C:E5"));
        assert!(!looks_like_address(""));
    }

    /// The id is a config key, so a device name has to survive becoming one:
    /// an unusable id would be written and then fail to resolve.
    #[test]
    fn ids_are_derived_from_a_name_and_are_config_safe() {
        assert_eq!(slug("Pixel 10 Pro").as_deref(), Some("pixel-10-pro"));
        assert_eq!(
            slug("Mirceone\u{2019}s iPhone").as_deref(),
            Some("mirceone-s-iphone")
        );
        assert_eq!(
            slug("Mi Smart Band 8!!").as_deref(),
            Some("mi-smart-band-8")
        );
        assert_eq!(slug("---").as_deref(), None);
        assert_eq!(slug("").as_deref(), None);
    }

    #[test]
    fn upsert_updates_in_place_rather_than_appending_a_duplicate() {
        let mut doc = document(
            "schema_version = 4\n\n[[device]]\nid = \"watch\"\nprofile = \"apple-continuity\"\nirk_base64 = \"AAAA\"\n",
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
            "# hand written\nschema_version = 4\n\n[[device]]\nid = \"watch\"\nprofile = \"presence\"\naddress = \"AA:BB:CC:DD:EE:FF\"\n",
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
            "schema_version = 4\n\n[[device]]\nid = \"watch\"\nprofile = \"apple-continuity\"\nname_prefix = \"Apple\"\naddress = \"AA:BB:CC:DD:EE:FF\"\nthreshold_dbm = -70\n",
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
            let mut doc = document(&format!("schema_version = 4\n{bad}\n"));
            let error = upsert(&mut doc, "watch").unwrap_err();
            assert!(
                error.contains("must be a sequence of [[device]] tables"),
                "unexpected error for {bad}: {error}"
            );
        }
        // An empty inline array is unambiguous, and nothing is lost by replacing it.
        let mut doc = document("schema_version = 4\ndevice = []\n");
        upsert(&mut doc, "watch").unwrap()["profile"] = value("presence");
        assert!(doc.to_string().contains("[[device]]"));
    }
}
