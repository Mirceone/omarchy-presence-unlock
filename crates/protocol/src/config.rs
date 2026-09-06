//! On-disk configuration, shared by the daemon and the CLI.
//!
//! The file shape ([`ConfigFile`]) and validated runtime shape ([`Settings`])
//! are separate so parsing never exposes unvalidated device identities to the daemon.

use crate::{
    ble::parse_address,
    identity::Identity,
    paths,
    presence::{DeviceSpec, MultiDeviceAuth, Policy},
    profile::Profile,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::{fmt, fs, io, path::Path};
use thiserror::Error;
use uuid::Uuid;

pub const CURRENT_SCHEMA: u8 = 4;

/// The literal contents of `config.toml`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub schema_version: u8,
    pub adapter: Option<String>,
    /// `any` (default), `all`, or `at-least:<n>`.
    #[serde(default)]
    pub multi_device_auth: Option<String>,
    #[serde(default, rename = "device")]
    pub devices: Vec<DeviceEntry>,
}

/// One `[[device]]` table.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceEntry {
    /// Stable name used in `status` output and log lines.
    pub id: String,
    /// Canonical id from the compile-time [`Profile`] registry.
    pub profile: String,
    /// Identity criteria. At least one is required; all that are set must hold.
    pub irk_base64: Option<String>,
    pub address: Option<String>,
    pub service_uuid: Option<String>,
    pub name_prefix: Option<String>,
    /// Per-device policy overrides.
    pub threshold_dbm: Option<i16>,
    pub minimum_samples: Option<u8>,
    pub sample_window_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
}

/// Redacts the IRK: a stray `{:?}` must never write the long-term secret to the journal.
impl fmt::Debug for DeviceEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceEntry")
            .field("id", &self.id)
            .field("profile", &self.profile)
            .field(
                "irk_base64",
                &self.irk_base64.as_ref().map(|_| "<redacted>"),
            )
            .field("address", &self.address)
            .field("service_uuid", &self.service_uuid)
            .field("name_prefix", &self.name_prefix)
            .field("threshold_dbm", &self.threshold_dbm)
            .field("minimum_samples", &self.minimum_samples)
            .field("sample_window_ms", &self.sample_window_ms)
            .field("freshness_ms", &self.freshness_ms)
            .finish()
    }
}

impl fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigFile")
            .field("schema_version", &self.schema_version)
            .field("adapter", &self.adapter)
            .field("multi_device_auth", &self.multi_device_auth)
            .field("devices", &self.devices)
            .finish()
    }
}

/// Validated configuration the daemon runs on.
#[derive(Debug)]
pub struct Settings {
    pub adapter: Option<String>,
    pub multi_device_auth: MultiDeviceAuth,
    pub devices: Vec<DeviceSpec>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("XDG_CONFIG_HOME or HOME is required")]
    NoConfigDir,
    #[error("cannot read config: {0}")]
    Read(#[from] io::Error),
    #[error("invalid config: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("unsupported schema version {0}; this build requires version {CURRENT_SCHEMA}")]
    Schema(u8),
    #[error("device {0}: IRK must decode to exactly 16 bytes")]
    IrkLength(String),
    #[error("device {0}: IRK is invalid base64")]
    IrkBase64(String),
    #[error("device {0}: {1}")]
    Device(String, &'static str),
    #[error("device {0}: unsupported profile {1}")]
    UnsupportedProfile(String, String),
    #[error("duplicate device id {0}")]
    DuplicateDevice(String),
    #[error("no devices are configured; run `omarchy-presence-unlock enroll`")]
    NoDevices,
    #[error("unsupported multi-device authentication rule {0}; use any, all, or at-least:<n>")]
    MultiDeviceAuth(String),
    #[error(
        "at-least:{required} needs {required} enrolled devices but {configured} are configured; run `omarchy-presence-unlock multi-device-auth any`"
    )]
    MultiDeviceAuthExceedsDeviceCount { required: u8, configured: usize },
}

impl ConfigFile {
    /// # Errors
    ///
    /// Returns an error when the file is missing, unreadable, or malformed.
    pub fn load() -> Result<Self, ConfigError> {
        let path = paths::config_path().ok_or(ConfigError::NoConfigDir)?;
        Self::from_path(&path)
    }

    /// # Errors
    ///
    /// Returns an error when `path` is unreadable or malformed.
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&fs::read_to_string(path)?)
    }

    /// # Errors
    ///
    /// Returns an error for malformed TOML or a schema version other than the current one.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)?;
        if config.schema_version != CURRENT_SCHEMA {
            return Err(ConfigError::Schema(config.schema_version));
        }
        Ok(config)
    }

    /// Validates and lowers the file into runtime [`Settings`].
    ///
    /// # Errors
    ///
    /// Returns an error for an unusable device or multi-device authentication rule.
    pub fn resolve(&self) -> Result<Settings, ConfigError> {
        if self.devices.is_empty() {
            return Err(ConfigError::NoDevices);
        }
        let mut devices = Vec::with_capacity(self.devices.len());
        for entry in &self.devices {
            if devices.iter().any(|spec: &DeviceSpec| spec.id == entry.id) {
                return Err(ConfigError::DuplicateDevice(entry.id.clone()));
            }
            devices.push(entry.resolve()?);
        }
        let multi_device_auth = self.multi_device_auth()?;
        if let MultiDeviceAuth::AtLeast(required) = multi_device_auth
            && usize::from(required) > devices.len()
        {
            return Err(ConfigError::MultiDeviceAuthExceedsDeviceCount {
                required,
                configured: devices.len(),
            });
        }
        Ok(Settings {
            adapter: self.adapter.clone(),
            multi_device_auth,
            devices,
        })
    }

    /// # Errors
    ///
    /// Returns an error for a multi-device authentication rule this build does not understand.
    pub fn multi_device_auth(&self) -> Result<MultiDeviceAuth, ConfigError> {
        let Some(text) = &self.multi_device_auth else {
            return Ok(MultiDeviceAuth::Any);
        };
        match text.as_str() {
            "any" => Ok(MultiDeviceAuth::Any),
            "all" => Ok(MultiDeviceAuth::All),
            other => other
                .strip_prefix("at-least:")
                .and_then(|n| n.parse::<u8>().ok())
                .filter(|n| *n > 0)
                .map(MultiDeviceAuth::AtLeast)
                .ok_or_else(|| ConfigError::MultiDeviceAuth(other.into())),
        }
    }
}

impl DeviceEntry {
    fn profile(&self) -> Result<&'static Profile, ConfigError> {
        crate::profile::find(&self.profile)
            .ok_or_else(|| ConfigError::UnsupportedProfile(self.id.clone(), self.profile.clone()))
    }

    fn identity(&self) -> Result<Identity, ConfigError> {
        let irk = self
            .irk_base64
            .as_ref()
            .map(|text| decode_irk(&self.id, text))
            .transpose()?;
        let address = self
            .address
            .as_ref()
            .map(|text| {
                parse_address(text)
                    .map_err(|_| ConfigError::Device(self.id.clone(), "invalid address"))
            })
            .transpose()?;
        let service_uuid = self
            .service_uuid
            .as_ref()
            .map(|text| {
                Uuid::parse_str(text)
                    .map_err(|_| ConfigError::Device(self.id.clone(), "invalid service UUID"))
            })
            .transpose()?;
        let identity = Identity {
            irk: irk.as_ref().map(crate::irk::IrkMatcher::new),
            address,
            service_uuid,
            name_prefix: self.name_prefix.clone(),
        };
        if identity.is_empty() {
            return Err(ConfigError::Device(
                self.id.clone(),
                "needs at least one of irk_base64, address, service_uuid, or name_prefix",
            ));
        }
        Ok(identity)
    }

    fn policy(&self) -> Policy {
        let default = Policy::default();
        Policy {
            threshold_dbm: self.threshold_dbm.unwrap_or(default.threshold_dbm),
            minimum_samples: self
                .minimum_samples
                .unwrap_or(default.minimum_samples)
                .max(1),
            sample_window_ms: self.sample_window_ms.unwrap_or(default.sample_window_ms),
            freshness_ms: self.freshness_ms.unwrap_or(default.freshness_ms),
        }
    }

    /// # Errors
    ///
    /// Returns an error for an unknown profile or unusable identity criteria.
    pub fn resolve(&self) -> Result<DeviceSpec, ConfigError> {
        Ok(DeviceSpec {
            id: self.id.clone(),
            identity: self.identity()?,
            profile: self.profile()?,
            policy: self.policy(),
        })
    }
}

/// Decodes a macOS Remote IRK.
///
/// Apple's stored representation is byte-reversed relative to the key order
/// `BlueZ`'s `ah()` expects, so the bytes are reversed here, once, for everyone.
fn decode_irk(id: &str, text: &str) -> Result<[u8; 16], ConfigError> {
    let raw = STANDARD
        .decode(text.trim())
        .map_err(|_| ConfigError::IrkBase64(id.into()))?;
    let mut irk: [u8; 16] = raw
        .try_into()
        .map_err(|_| ConfigError::IrkLength(id.into()))?;
    irk.reverse();
    Ok(irk)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IRK: &str = "c3VwZXItc2VjcmV0LWtleQ==";

    fn current_config() -> String {
        format!(
            r#"
schema_version = 4
multi_device_auth = "all"

[[device]]
id = "watch"
profile = "apple-continuity"
irk_base64 = "{IRK}"
threshold_dbm = -60

[[device]]
id = "phone"
profile = "presence"
address = "AA:BB:CC:DD:EE:FF"
"#
        )
    }

    #[test]
    fn current_schema_resolves_profiles_through_the_registry() {
        let settings = ConfigFile::parse(&current_config())
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(settings.multi_device_auth, MultiDeviceAuth::All);
        assert_eq!(settings.devices.len(), 2);
        assert_eq!(settings.devices[0].id, "watch");
        assert_eq!(
            settings.devices[0].profile,
            crate::profile::APPLE_CONTINUITY
        );
        assert_eq!(settings.devices[0].policy.threshold_dbm, -60);
        assert_eq!(
            settings.devices[0].policy.freshness_ms,
            Policy::default().freshness_ms
        );
        assert_eq!(settings.devices[1].profile, crate::profile::PRESENCE);
        assert_eq!(
            settings.devices[1].policy.threshold_dbm,
            Policy::default().threshold_dbm
        );
    }

    #[test]
    fn current_schema_rejects_an_unregistered_profile() {
        let text = "schema_version = 4\n[[device]]\nid = \"x\"\nprofile = \"untrusted-plugin\"\naddress = \"AA:BB:CC:DD:EE:FF\"\n";
        assert!(matches!(
            ConfigFile::parse(text).unwrap().resolve(),
            Err(ConfigError::UnsupportedProfile(_, _))
        ));
    }

    #[test]
    fn debug_never_prints_the_irk() {
        let rendered = format!("{:?}", ConfigFile::parse(&current_config()).unwrap());
        assert!(!rendered.contains("c3VwZXI"), "IRK leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn a_device_without_identity_criteria_is_rejected() {
        let text = "schema_version = 4\n[[device]]\nid = \"ghost\"\nprofile = \"presence\"\n";
        assert!(matches!(
            ConfigFile::parse(text).unwrap().resolve(),
            Err(ConfigError::Device(..))
        ));
    }

    #[test]
    fn duplicate_device_ids_are_rejected() {
        let text = format!(
            "schema_version = 4\n[[device]]\nid = \"a\"\nprofile = \"apple-continuity\"\nirk_base64 = \"{IRK}\"\n\n[[device]]\nid = \"a\"\nprofile = \"presence\"\naddress = \"AA:BB:CC:DD:EE:FF\"\n"
        );
        assert!(matches!(
            ConfigFile::parse(&text).unwrap().resolve(),
            Err(ConfigError::DuplicateDevice(_))
        ));
    }

    #[test]
    fn multi_device_auth_expressions_parse() {
        let mut config = ConfigFile::parse(&current_config()).unwrap();
        config.multi_device_auth = Some("any".into());
        assert_eq!(config.multi_device_auth().unwrap(), MultiDeviceAuth::Any);
        config.multi_device_auth = Some("at-least:2".into());
        assert_eq!(
            config.multi_device_auth().unwrap(),
            MultiDeviceAuth::AtLeast(2)
        );
        for invalid in ["at-least:0", "most"] {
            config.multi_device_auth = Some(invalid.into());
            assert!(matches!(
                config.multi_device_auth(),
                Err(ConfigError::MultiDeviceAuth(_))
            ));
        }
    }

    #[test]
    fn an_at_least_rule_exceeding_enrolled_devices_is_rejected() {
        let mut config = ConfigFile::parse(&current_config()).unwrap();
        config.multi_device_auth = Some("at-least:3".into());
        let error = config.resolve().unwrap_err();
        assert_eq!(
            error.to_string(),
            "at-least:3 needs 3 enrolled devices but 2 are configured; run `omarchy-presence-unlock multi-device-auth any`"
        );
    }

    #[test]
    fn deprecated_schema_versions_are_refused() {
        for version in [1, 2, 3, 5] {
            assert!(matches!(
                ConfigFile::parse(&format!("schema_version = {version}\n")),
                Err(ConfigError::Schema(found)) if found == version
            ));
        }
    }

    #[test]
    fn an_irk_of_the_wrong_length_is_refused() {
        let text = "schema_version = 4\n[[device]]\nid = \"w\"\nprofile = \"apple-continuity\"\nirk_base64 = \"AAAA\"\n";
        assert!(matches!(
            ConfigFile::parse(text).unwrap().resolve(),
            Err(ConfigError::IrkLength(_))
        ));
    }
}
