//! On-disk configuration, shared by the daemon and the CLI.
//!
//! The file shape ([`ConfigFile`]) and the runtime shape ([`Settings`]) are
//! deliberately separate: the file is versioned, tolerant, and migrated; the
//! runtime shape is validated, secret-free at the string level, and is what the
//! daemon actually runs on.

use crate::{
    ble::parse_address,
    identity::Identity,
    paths,
    presence::{DeviceSpec, Policy, Quorum},
    profile::Profile,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::{fmt, fs, io, path::Path};
use thiserror::Error;
use uuid::Uuid;

pub const CURRENT_SCHEMA: u8 = 3;

/// The literal contents of `config.toml`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub schema_version: u8,
    pub adapter: Option<String>,
    /// `any` (default), `all`, or `at-least:<n>`.
    #[serde(default)]
    pub quorum: Option<String>,
    #[serde(default = "default_backend")]
    pub unlock_backend: String,
    /// Argv for `unlock_backend = "command"`.
    #[serde(default)]
    pub unlock_command: Vec<String>,
    /// Process name for `unlock_backend = "process-signal"`.
    pub unlock_process: Option<String>,
    /// `SIGUSR1` (default) or `SIGUSR2` for `unlock_backend = "process-signal"`.
    pub unlock_signal: Option<String>,
    #[serde(default, rename = "device")]
    pub devices: Vec<DeviceEntry>,

    // Schema 1 compatibility. A v1 file describes exactly one Apple Watch.
    pub irk_base64: Option<String>,
    pub unlock_threshold_dbm: Option<i16>,
}

/// One `[[device]]` table.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceEntry {
    /// Stable name used in `status` output and log lines.
    pub id: String,
    /// Canonical id from the compile-time [`Profile`] registry.
    #[serde(alias = "kind")]
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
            .field("quorum", &self.quorum)
            .field("unlock_backend", &self.unlock_backend)
            .field("unlock_command", &self.unlock_command)
            .field("unlock_process", &self.unlock_process)
            .field("unlock_signal", &self.unlock_signal)
            .field("devices", &self.devices)
            .field(
                "irk_base64",
                &self.irk_base64.as_ref().map(|_| "<redacted>"),
            )
            .field("unlock_threshold_dbm", &self.unlock_threshold_dbm)
            .finish()
    }
}

fn default_backend() -> String {
    "disabled".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    Usr1,
    Usr2,
}

/// What releases the lock screen once a confirmation is authorised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Nothing is wired up; `CONFIRM` is refused.
    Disabled,
    /// Signal a process owned by this user, matched by `/proc/<pid>/comm`.
    /// Hyprlock's supported unlock path is `SIGUSR1`.
    ProcessSignal { process: String, signal: SignalKind },
    /// Run a command; a zero exit status means the session was unlocked.
    /// Covers `loginctl unlock-session`, swaylock forks, and anything scriptable.
    Command(Vec<String>),
}

impl Backend {
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        *self != Self::Disabled
    }
}

/// Validated configuration the daemon runs on.
#[derive(Debug)]
pub struct Settings {
    pub adapter: Option<String>,
    pub quorum: Quorum,
    pub backend: Backend,
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
    #[error(
        "unsupported schema version {0}; this build understands versions 1 through {CURRENT_SCHEMA}"
    )]
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
    #[error("no devices are configured; run `omarchy-watch-unlock enroll`")]
    NoDevices,
    #[error("unsupported unlock backend: {0}")]
    Backend(String),
    #[error("unlock_backend = \"command\" requires a non-empty unlock_command")]
    MissingCommand,
    #[error("unsupported unlock signal: {0}")]
    Signal(String),
    #[error("unsupported quorum {0}; use any, all, or at-least:<n>")]
    Quorum(String),
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
    /// Returns an error for malformed TOML or an unknown schema version.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)?;
        if config.schema_version == 0 || config.schema_version > CURRENT_SCHEMA {
            return Err(ConfigError::Schema(config.schema_version));
        }
        Ok(config)
    }

    /// Validates and lowers the file into runtime [`Settings`].
    ///
    /// # Errors
    ///
    /// Returns an error for an unusable device, backend, or quorum.
    pub fn resolve(&self) -> Result<Settings, ConfigError> {
        let entries = self.device_entries();
        if entries.is_empty() {
            return Err(ConfigError::NoDevices);
        }
        let mut devices = Vec::with_capacity(entries.len());
        for entry in &entries {
            if devices.iter().any(|spec: &DeviceSpec| spec.id == entry.id) {
                return Err(ConfigError::DuplicateDevice(entry.id.clone()));
            }
            devices.push(entry.resolve()?);
        }
        Ok(Settings {
            adapter: self.adapter.clone(),
            quorum: self.quorum()?,
            backend: self.backend()?,
            devices,
        })
    }

    /// The configured devices, synthesising the schema-1 single-watch form.
    fn device_entries(&self) -> Vec<DeviceEntry> {
        if !self.devices.is_empty() {
            return self.devices.clone();
        }
        self.irk_base64
            .as_ref()
            .map(|irk| {
                vec![DeviceEntry {
                    id: "watch".into(),
                    profile: "apple-continuity".into(),
                    irk_base64: Some(irk.clone()),
                    address: None,
                    service_uuid: None,
                    name_prefix: None,
                    threshold_dbm: self.unlock_threshold_dbm,
                    minimum_samples: None,
                    sample_window_ms: None,
                    freshness_ms: None,
                }]
            })
            .unwrap_or_default()
    }

    /// # Errors
    ///
    /// Returns an error for a quorum expression this build does not understand.
    pub fn quorum(&self) -> Result<Quorum, ConfigError> {
        let Some(text) = &self.quorum else {
            return Ok(Quorum::Any);
        };
        match text.as_str() {
            "any" => Ok(Quorum::Any),
            "all" => Ok(Quorum::All),
            other => other
                .strip_prefix("at-least:")
                .and_then(|n| n.parse::<u8>().ok())
                .filter(|n| *n > 0)
                .map(Quorum::AtLeast)
                .ok_or_else(|| ConfigError::Quorum(other.into())),
        }
    }

    /// # Errors
    ///
    /// Returns an error for a backend not supported by this build, or for a
    /// backend whose required parameters are missing.
    pub fn backend(&self) -> Result<Backend, ConfigError> {
        match self.unlock_backend.as_str() {
            "disabled" => Ok(Backend::Disabled),
            // v0.1's automatic mode is deliberately migrated to confirmation:
            // old configuration must never preserve automatic unlock by accident.
            "hyprlock-confirm" | "hyprlock-signal" => Ok(Backend::ProcessSignal {
                process: "hyprlock".into(),
                signal: SignalKind::Usr1,
            }),
            "process-signal" => Ok(Backend::ProcessSignal {
                process: self.unlock_process.clone().ok_or_else(|| {
                    ConfigError::Backend("process-signal needs unlock_process".into())
                })?,
                signal: match self.unlock_signal.as_deref() {
                    None | Some("SIGUSR1") => SignalKind::Usr1,
                    Some("SIGUSR2") => SignalKind::Usr2,
                    Some(other) => return Err(ConfigError::Signal(other.into())),
                },
            }),
            "command" => {
                if self.unlock_command.is_empty() {
                    return Err(ConfigError::MissingCommand);
                }
                Ok(Backend::Command(self.unlock_command.clone()))
            }
            other => Err(ConfigError::Backend(other.into())),
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

    fn v2() -> String {
        format!(
            r#"
schema_version = 2
unlock_backend = "hyprlock-confirm"
quorum = "all"

[[device]]
id = "watch"
kind = "apple-watch"
irk_base64 = "{IRK}"
threshold_dbm = -60

[[device]]
id = "phone"
kind = "ble"
address = "AA:BB:CC:DD:EE:FF"
"#
        )
    }

    #[test]
    fn schema_three_resolves_profiles_through_the_registry() {
        let text = "schema_version = 3\nunlock_backend = \"disabled\"\n[[device]]\nid = \"fob\"\nprofile = \"presence\"\naddress = \"AA:BB:CC:DD:EE:FF\"\n";
        let settings = ConfigFile::parse(text).unwrap().resolve().unwrap();
        assert_eq!(settings.devices[0].profile, crate::profile::PRESENCE);
    }

    #[test]
    fn schema_three_rejects_an_unregistered_profile() {
        let text = "schema_version = 3\nunlock_backend = \"disabled\"\n[[device]]\nid = \"x\"\nprofile = \"untrusted-plugin\"\naddress = \"AA:BB:CC:DD:EE:FF\"\n";
        assert!(matches!(
            ConfigFile::parse(text).unwrap().resolve(),
            Err(ConfigError::UnsupportedProfile(_, _))
        ));
    }

    #[test]
    fn debug_never_prints_the_irk() {
        let rendered = format!("{:?}", ConfigFile::parse(&v2()).unwrap());
        assert!(!rendered.contains("c3VwZXI"), "IRK leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn resolves_multiple_devices_with_per_device_policy() {
        let settings = ConfigFile::parse(&v2()).unwrap().resolve().unwrap();
        assert_eq!(settings.quorum, Quorum::All);
        assert_eq!(settings.devices.len(), 2);
        assert_eq!(settings.devices[0].id, "watch");
        assert_eq!(
            settings.devices[0].profile,
            crate::profile::APPLE_CONTINUITY
        );
        assert_eq!(settings.devices[0].policy.threshold_dbm, -60);
        // Unset knobs fall back to the shared default rather than to zero.
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
    fn a_schema_1_file_becomes_a_single_apple_watch() {
        let text = format!(
            "schema_version = 1\nirk_base64 = \"{IRK}\"\nunlock_threshold_dbm = -55\nunlock_backend = \"hyprlock-confirm\"\n"
        );
        let settings = ConfigFile::parse(&text).unwrap().resolve().unwrap();
        assert_eq!(settings.devices.len(), 1);
        assert_eq!(settings.devices[0].id, "watch");
        assert_eq!(
            settings.devices[0].profile,
            crate::profile::APPLE_CONTINUITY
        );
        assert_eq!(settings.devices[0].policy.threshold_dbm, -55);
        assert_eq!(settings.quorum, Quorum::Any);
    }

    #[test]
    fn the_legacy_automatic_backend_maps_to_confirmation() {
        let signal = Backend::ProcessSignal {
            process: "hyprlock".into(),
            signal: SignalKind::Usr1,
        };
        let mut config = ConfigFile::parse(&v2()).unwrap();
        assert_eq!(config.backend().unwrap(), signal);
        config.unlock_backend = "hyprlock-signal".into();
        assert_eq!(config.backend().unwrap(), signal);
        config.unlock_backend = "disabled".into();
        assert_eq!(config.backend().unwrap(), Backend::Disabled);
        config.unlock_backend = "hyprlock-comfirm".into();
        assert!(matches!(config.backend(), Err(ConfigError::Backend(_))));
    }

    #[test]
    fn the_command_backend_requires_an_argv() {
        let mut config = ConfigFile::parse(&v2()).unwrap();
        config.unlock_backend = "command".into();
        assert!(matches!(config.backend(), Err(ConfigError::MissingCommand)));
        config.unlock_command = vec!["loginctl".into(), "unlock-session".into()];
        assert_eq!(
            config.backend().unwrap(),
            Backend::Command(vec!["loginctl".into(), "unlock-session".into()])
        );
    }

    #[test]
    fn a_device_without_identity_criteria_is_rejected() {
        let text = "schema_version = 2\n[[device]]\nid = \"ghost\"\nkind = \"ble\"\n";
        assert!(matches!(
            ConfigFile::parse(text).unwrap().resolve(),
            Err(ConfigError::Device(..))
        ));
    }

    #[test]
    fn duplicate_device_ids_are_rejected() {
        let text = format!(
            "schema_version = 2\n[[device]]\nid = \"a\"\nkind = \"apple-watch\"\nirk_base64 = \"{IRK}\"\n\n[[device]]\nid = \"a\"\nkind = \"ble\"\naddress = \"AA:BB:CC:DD:EE:FF\"\n"
        );
        assert!(matches!(
            ConfigFile::parse(&text).unwrap().resolve(),
            Err(ConfigError::DuplicateDevice(_))
        ));
    }

    #[test]
    fn quorum_expressions_parse() {
        let mut config = ConfigFile::parse(&v2()).unwrap();
        config.quorum = Some("any".into());
        assert_eq!(config.quorum().unwrap(), Quorum::Any);
        config.quorum = Some("at-least:2".into());
        assert_eq!(config.quorum().unwrap(), Quorum::AtLeast(2));
        config.quorum = Some("at-least:0".into());
        assert!(matches!(config.quorum(), Err(ConfigError::Quorum(_))));
        config.quorum = Some("most".into());
        assert!(matches!(config.quorum(), Err(ConfigError::Quorum(_))));
    }

    #[test]
    fn an_unknown_schema_version_is_refused() {
        assert!(matches!(
            ConfigFile::parse("schema_version = 4\n"),
            Err(ConfigError::Schema(4))
        ));
    }

    #[test]
    fn an_irk_of_the_wrong_length_is_refused() {
        let text = "schema_version = 2\n[[device]]\nid = \"w\"\nkind = \"apple-watch\"\nirk_base64 = \"AAAA\"\n";
        assert!(matches!(
            ConfigFile::parse(text).unwrap().resolve(),
            Err(ConfigError::IrkLength(_))
        ));
    }
}
