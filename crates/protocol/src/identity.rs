//! How a configured device is recognised in an advertisement.
//!
//! An [`Identity`] is a conjunction of optional criteria: every criterion that is
//! set must hold. An identity with no criterion set matches nothing, so a
//! half-written configuration can never authorise every device in radio range.

use crate::{
    ble::{Address, Advertisement, Needs, format_address},
    irk::IrkMatcher,
};
use std::fmt;
use uuid::Uuid;

/// Whether an advertisement can be accepted or rejected from its address alone.
///
/// The scanner uses this to skip the D-Bus property reads for the overwhelming
/// majority of advertisements, which come from strangers' phones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every criterion is satisfied by the address; no payload is required.
    Match,
    /// Some criterion is contradicted by the address.
    Reject,
    /// The address is consistent, but payload fields must still be checked.
    Undecided,
}

#[derive(Default)]
pub struct Identity {
    /// The device rotates Resolvable Private Addresses derived from this IRK.
    pub irk: Option<IrkMatcher>,
    /// The device uses this fixed public or static random address.
    pub address: Option<Address>,
    /// The device advertises this service UUID.
    pub service_uuid: Option<Uuid>,
    /// The advertised name starts with this prefix.
    pub name_prefix: Option<String>,
}

/// Redacts nothing sensitive but keeps the IRK opaque; see [`IrkMatcher`].
impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("irk", &self.irk)
            .field("address", &self.address.as_ref().map(format_address))
            .field("service_uuid", &self.service_uuid)
            .field("name_prefix", &self.name_prefix)
            .finish()
    }
}

impl Identity {
    #[must_use]
    pub fn from_irk(irk: &[u8; 16]) -> Self {
        Self {
            irk: Some(IrkMatcher::new(irk)),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn from_address(address: Address) -> Self {
        Self {
            address: Some(address),
            ..Self::default()
        }
    }

    /// True when no criterion is set. Such an identity never matches.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.irk.is_none()
            && self.address.is_none()
            && self.service_uuid.is_none()
            && self.name_prefix.is_none()
    }

    /// Advertisement fields this identity has to read.
    #[must_use]
    pub fn needs(&self) -> Needs {
        Needs {
            manufacturer_data: false,
            service_data: false,
            service_uuids: self.service_uuid.is_some(),
            name: self.name_prefix.is_some(),
        }
    }

    /// Decides as much as the address allows.
    #[must_use]
    pub fn verdict(&self, address: &Address) -> Verdict {
        if self.is_empty() {
            return Verdict::Reject;
        }
        if let Some(irk) = &self.irk
            && !irk.matches(address)
        {
            return Verdict::Reject;
        }
        if let Some(expected) = &self.address
            && expected != address
        {
            return Verdict::Reject;
        }
        if self.service_uuid.is_some() || self.name_prefix.is_some() {
            Verdict::Undecided
        } else {
            Verdict::Match
        }
    }

    /// Full match, including the criteria that require advertisement payload.
    ///
    /// A payload criterion whose field the scanner did not fetch is treated as
    /// unmet: matching must never succeed on missing evidence.
    #[must_use]
    pub fn matches(&self, advertisement: &Advertisement<'_>) -> bool {
        match self.verdict(&advertisement.address) {
            Verdict::Reject => return false,
            Verdict::Match => return true,
            Verdict::Undecided => {}
        }
        if let Some(uuid) = &self.service_uuid
            && !advertisement
                .service_uuids
                .is_some_and(|uuids| uuids.contains(uuid))
        {
            return false;
        }
        if let Some(prefix) = &self.name_prefix
            && !advertisement
                .name
                .is_some_and(|name| name.starts_with(prefix.as_str()))
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const UUID: Uuid = Uuid::from_u128(0x0000_180f_0000_1000_8000_0080_5f9b_34fb);

    #[test]
    fn an_empty_identity_matches_nothing() {
        let identity = Identity::default();
        assert!(identity.is_empty());
        assert_eq!(identity.verdict(&[0x40; 6]), Verdict::Reject);
        assert!(!identity.matches(&Advertisement::new([0x40; 6], -50)));
    }

    #[test]
    fn a_fixed_address_decides_without_payload() {
        let identity = Identity::from_address([1, 2, 3, 4, 5, 6]);
        assert_eq!(identity.verdict(&[1, 2, 3, 4, 5, 6]), Verdict::Match);
        assert_eq!(identity.verdict(&[1, 2, 3, 4, 5, 7]), Verdict::Reject);
        assert_eq!(identity.needs(), Needs::nothing());
    }

    #[test]
    fn a_service_uuid_criterion_requires_the_payload() {
        let identity = Identity {
            service_uuid: Some(UUID),
            ..Identity::default()
        };
        assert_eq!(identity.verdict(&[1, 2, 3, 4, 5, 6]), Verdict::Undecided);
        assert!(identity.needs().service_uuids);

        // Absent payload must not match: no evidence is not a match.
        let bare = Advertisement::new([1, 2, 3, 4, 5, 6], -50);
        assert!(!identity.matches(&bare));

        let uuids = HashSet::from([UUID]);
        assert!(identity.matches(&bare.with_service_uuids(&uuids)));
    }

    #[test]
    fn criteria_are_conjunctive() {
        let identity = Identity {
            address: Some([1, 2, 3, 4, 5, 6]),
            name_prefix: Some("Pixel".into()),
            ..Identity::default()
        };
        let right = Advertisement::new([1, 2, 3, 4, 5, 6], -50).with_name("Pixel 9");
        let wrong_name = Advertisement::new([1, 2, 3, 4, 5, 6], -50).with_name("Galaxy");
        let wrong_address = Advertisement::new([9, 9, 9, 9, 9, 9], -50).with_name("Pixel 9");
        assert!(identity.matches(&right));
        assert!(!identity.matches(&wrong_name));
        assert!(!identity.matches(&wrong_address));
    }
}
