//! One derivation each for the control socket, the config file, and installed data.
//!
//! Every consumer must agree on these paths: the PAM module cannot trust the
//! caller's environment, so a daemon that derives its socket from
//! `$XDG_RUNTIME_DIR` would serve a path PAM never probes.

use nix::unistd::Uid;
use std::{env, path::PathBuf};

/// Where packaging installs read-only data assets.
pub const DEFAULT_DATADIR: &str = "/usr/share/omarchy-presence-unlock";

/// Canonical control-socket directory for `uid`.
///
/// Derived from the uid, never from `$XDG_RUNTIME_DIR`.
#[must_use]
pub fn socket_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/omarchy-presence-unlock"))
}

#[must_use]
pub fn socket_path(uid: u32) -> PathBuf {
    socket_dir(uid).join("control.sock")
}

#[must_use]
pub fn current_socket_dir() -> PathBuf {
    socket_dir(Uid::current().as_raw())
}

#[must_use]
pub fn current_socket_path() -> PathBuf {
    socket_path(Uid::current().as_raw())
}

/// `$OPU_DATADIR` if set, otherwise [`DEFAULT_DATADIR`].
#[must_use]
pub fn datadir() -> PathBuf {
    env::var_os("OPU_DATADIR").map_or_else(|| PathBuf::from(DEFAULT_DATADIR), PathBuf::from)
}

/// The PAM policy template shipped by packaging.
#[must_use]
pub fn pam_policy_source() -> PathBuf {
    datadir().join("omarchy-lock-presence.pam")
}

/// `$XDG_CONFIG_HOME/omarchy-presence-unlock`, else `$HOME/.config/omarchy-presence-unlock`.
/// `None` when neither variable is set.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|base| base.join("omarchy-presence-unlock"))
}

#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.toml"))
}

/// Where `BlueZ` keeps bonding records.
pub const BLUEZ_STORAGE_DIR: &str = "/var/lib/bluetooth";

/// Directory holding every bond record for one adapter.
///
/// `adapter` is an uppercase colon-separated address. `BlueZ` prefixes the
/// directory with `static-` when the adapter's own address is LE-random
/// (`btd_adapter_get_storage_dir`, bluez src/adapter.c:565-577).
#[must_use]
pub fn bluez_adapter_dir(adapter: &str, adapter_is_random: bool) -> PathBuf {
    let directory = if adapter_is_random {
        format!("static-{adapter}")
    } else {
        adapter.to_owned()
    };
    PathBuf::from(BLUEZ_STORAGE_DIR).join(directory)
}

/// Path of a bonded device's `info` file.
///
/// `device` must be the peer's *identity* address, never a resolvable private one.
#[must_use]
pub fn bluez_device_info(adapter: &str, adapter_is_random: bool, device: &str) -> PathBuf {
    bluez_adapter_dir(adapter, adapter_is_random)
        .join(device)
        .join("info")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_uid_derived_and_matches_the_pam_module() {
        assert_eq!(
            socket_path(1000),
            PathBuf::from("/run/user/1000/omarchy-presence-unlock/control.sock")
        );
        assert_eq!(socket_path(1000), socket_dir(1000).join("control.sock"));
    }

    #[test]
    fn datadir_defaults_when_the_override_is_absent() {
        // SAFETY-free: this test only reads when OPU_DATADIR is unset in the harness.
        if env::var_os("OPU_DATADIR").is_none() {
            assert_eq!(datadir(), PathBuf::from(DEFAULT_DATADIR));
            assert_eq!(
                pam_policy_source(),
                PathBuf::from(DEFAULT_DATADIR).join("omarchy-lock-presence.pam")
            );
        }
    }

    #[test]
    fn bluez_info_path_follows_the_adapter_storage_dir_rules() {
        assert_eq!(
            bluez_device_info("00:1A:7D:DA:71:05", false, "AA:BB:CC:DD:EE:FF"),
            PathBuf::from("/var/lib/bluetooth/00:1A:7D:DA:71:05/AA:BB:CC:DD:EE:FF/info")
        );
        // An LE-random adapter address prefixes the adapter directory only.
        assert_eq!(
            bluez_device_info("C0:1A:7D:DA:71:05", true, "AA:BB:CC:DD:EE:FF"),
            PathBuf::from("/var/lib/bluetooth/static-C0:1A:7D:DA:71:05/AA:BB:CC:DD:EE:FF/info")
        );
    }
}
