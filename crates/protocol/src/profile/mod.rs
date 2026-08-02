//! Built-in device profile registry.
//!
//! Identity answers "is this my device"; a profile answers "what does this
//! matched advertisement assert". Each supported device family owns one module
//! and exports one descriptor. Adding support means registering that descriptor;
//! the scanner, fleet, configuration resolver, and status output stay unchanged.

mod apple_continuity;
mod presence;

use crate::ble::{Advertisement, Needs};

/// A profile's reading of one advertisement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// The device asserts a state consistent with unlocking.
    Qualify,
    /// The device asserts a state that revokes accumulated evidence.
    Revoke,
    /// This advertisement carries no usable statement.
    Ignore,
}

/// Compile-time descriptor for one supported device family.
#[derive(Clone, Copy)]
pub struct Profile {
    id: &'static str,
    aliases: &'static [&'static str],
    needs: Needs,
    attests_device_state: bool,
    evaluate: for<'a> fn(&Advertisement<'a>) -> Observation,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("Profile").field(&self.id).finish()
    }
}

impl PartialEq for Profile {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Profile {}

impl Profile {
    pub(super) const fn new(
        id: &'static str,
        aliases: &'static [&'static str],
        needs: Needs,
        attests_device_state: bool,
        evaluate: for<'a> fn(&Advertisement<'a>) -> Observation,
    ) -> Self {
        Self {
            id,
            aliases,
            needs,
            attests_device_state,
            evaluate,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    pub const fn needs(&self) -> Needs {
        self.needs
    }

    /// True when the profile distinguishes locked from unlocked device state.
    #[must_use]
    pub const fn attests_device_state(&self) -> bool {
        self.attests_device_state
    }

    #[must_use]
    pub fn evaluate(&self, advertisement: &Advertisement<'_>) -> Observation {
        (self.evaluate)(advertisement)
    }

    fn accepts(&self, id: &str) -> bool {
        self.id == id || self.aliases.contains(&id)
    }
}

/// Audited profiles compiled into this release.
pub static PROFILES: [&Profile; 2] = [&apple_continuity::PROFILE, &presence::PROFILE];

/// Finds a profile by canonical id or a migration alias.
#[must_use]
pub fn find(id: &str) -> Option<&'static Profile> {
    PROFILES.iter().copied().find(|profile| profile.accepts(id))
}

/// Generic proximity-only BLE profile.
pub static PRESENCE: &Profile = &presence::PROFILE;

/// Apple Continuity profile used by an unlocked, wrist-worn Watch.
pub static APPLE_CONTINUITY: &Profile = &apple_continuity::PROFILE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_resolves_canonical_ids_and_migration_aliases() {
        assert_eq!(find("presence"), Some(PRESENCE));
        assert_eq!(find("ble"), Some(PRESENCE));
        assert_eq!(find("apple-continuity"), Some(APPLE_CONTINUITY));
        assert_eq!(find("apple-watch"), Some(APPLE_CONTINUITY));
        assert_eq!(find("unknown"), None);
    }

    #[test]
    fn canonical_profile_ids_are_unique() {
        for (index, profile) in PROFILES.iter().enumerate() {
            assert!(
                PROFILES[..index]
                    .iter()
                    .all(|other| other.id() != profile.id())
            );
        }
    }
}
