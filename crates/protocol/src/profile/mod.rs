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
    /// What this family is called in front of a user. Configuration and
    /// diagnostics use `id`; anything a person reads uses this.
    label: &'static str,
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
        label: &'static str,
        needs: Needs,
        attests_device_state: bool,
        evaluate: for<'a> fn(&Advertisement<'a>) -> Observation,
    ) -> Self {
        Self {
            id,
            label,
            needs,
            attests_device_state,
            evaluate,
        }
    }

    /// The name to show a user. `id` stays the machine-readable one.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
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
}

/// Audited profiles compiled into this release.
pub static PROFILES: [&Profile; 2] = [&apple_continuity::PROFILE, &presence::PROFILE];

/// Finds a profile by its canonical id.
#[must_use]
pub fn find(id: &str) -> Option<&'static Profile> {
    PROFILES.iter().copied().find(|profile| profile.id == id)
}

/// Generic proximity-only BLE profile.
pub static PRESENCE: &Profile = &presence::PROFILE;

/// Apple Continuity profile used by an unlocked, wrist-worn Watch.
pub static APPLE_CONTINUITY: &Profile = &apple_continuity::PROFILE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_resolves_only_canonical_ids() {
        assert_eq!(find("presence"), Some(PRESENCE));
        assert_eq!(find("apple-continuity"), Some(APPLE_CONTINUITY));
        assert_eq!(find("ble"), None);
        assert_eq!(find("apple-watch"), None);
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
