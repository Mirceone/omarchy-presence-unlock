//! Transport-neutral view of one BLE advertisement.
//!
//! Nothing here knows about `BlueZ`. The daemon adapts whatever its scanner
//! yields into [`Advertisement`], which lets the policy layer be exercised from
//! plain unit tests and lets a second transport be added without touching policy.

use std::collections::{HashMap, HashSet};
use thiserror::Error;
use uuid::Uuid;

/// A six-byte BLE device address in `BlueZ`'s byte order (most significant first,
/// matching the `AA:BB:CC:DD:EE:FF` text form).
pub type Address = [u8; 6];

#[derive(Debug, Error, PartialEq, Eq)]
#[error("expected a Bluetooth address of the form AA:BB:CC:DD:EE:FF")]
pub struct AddressParseError;

/// Parses `AA:BB:CC:DD:EE:FF`, case-insensitive.
///
/// # Errors
///
/// Returns [`AddressParseError`] for any input that is not exactly six
/// colon-separated hex octets.
pub fn parse_address(text: &str) -> Result<Address, AddressParseError> {
    let mut address = [0_u8; 6];
    let mut octets = text.split(':');
    for slot in &mut address {
        let octet = octets.next().ok_or(AddressParseError)?;
        if octet.len() != 2 {
            return Err(AddressParseError);
        }
        *slot = u8::from_str_radix(octet, 16).map_err(|_| AddressParseError)?;
    }
    if octets.next().is_some() {
        return Err(AddressParseError);
    }
    Ok(address)
}

#[must_use]
pub fn format_address(address: &Address) -> String {
    let [o0, o1, o2, o3, o4, o5] = *address;
    format!("{o0:02X}:{o1:02X}:{o2:02X}:{o3:02X}:{o4:02X}:{o5:02X}")
}

/// One observed advertisement, borrowed from whatever the scanner already holds.
///
/// Borrowing rather than owning keeps the hot path allocation-free: an
/// advertisement arrives every ~250 ms per device and is discarded immediately
/// after the policy layer folds it into [`crate::presence::Eligibility`].
#[derive(Debug, Clone, Copy)]
pub struct Advertisement<'a> {
    pub address: Address,
    pub rssi: i16,
    pub name: Option<&'a str>,
    pub manufacturer_data: Option<&'a HashMap<u16, Vec<u8>>>,
    pub service_data: Option<&'a HashMap<Uuid, Vec<u8>>>,
    pub service_uuids: Option<&'a HashSet<Uuid>>,
}

impl<'a> Advertisement<'a> {
    /// An advertisement carrying only the two fields every transport provides.
    #[must_use]
    pub fn new(address: Address, rssi: i16) -> Self {
        Self {
            address,
            rssi,
            name: None,
            manufacturer_data: None,
            service_data: None,
            service_uuids: None,
        }
    }

    #[must_use]
    pub fn with_manufacturer_data(mut self, data: &'a HashMap<u16, Vec<u8>>) -> Self {
        self.manufacturer_data = Some(data);
        self
    }

    #[must_use]
    pub fn with_service_data(mut self, data: &'a HashMap<Uuid, Vec<u8>>) -> Self {
        self.service_data = Some(data);
        self
    }

    #[must_use]
    pub fn with_service_uuids(mut self, uuids: &'a HashSet<Uuid>) -> Self {
        self.service_uuids = Some(uuids);
        self
    }

    #[must_use]
    pub fn with_name(mut self, name: &'a str) -> Self {
        self.name = Some(name);
        self
    }

    #[must_use]
    pub fn manufacturer(&self, company: u16) -> Option<&'a [u8]> {
        self.manufacturer_data?.get(&company).map(Vec::as_slice)
    }

    #[must_use]
    pub fn service(&self, uuid: Uuid) -> Option<&'a [u8]> {
        self.service_data?.get(&uuid).map(Vec::as_slice)
    }
}

/// Which advertisement fields a fleet actually inspects.
///
/// Every field beyond the address and RSSI costs a D-Bus property read per
/// advertisement, so the scanner fetches only what some configured device needs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
// One flag per advertisement field, not a bool-parameter API: the lint's usual
// argument-confusion hazard does not apply to a named field set.
#[allow(clippy::struct_excessive_bools)]
pub struct Needs {
    pub manufacturer_data: bool,
    pub service_data: bool,
    pub service_uuids: bool,
    pub name: bool,
}

impl Needs {
    #[must_use]
    pub const fn nothing() -> Self {
        Self {
            manufacturer_data: false,
            service_data: false,
            service_uuids: false,
            name: false,
        }
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            manufacturer_data: self.manufacturer_data || other.manufacturer_data,
            service_data: self.service_data || other.service_data,
            service_uuids: self.service_uuids || other.service_uuids,
            name: self.name || other.name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_an_address_round_trip() {
        let address = parse_address("aa:bb:cc:11:22:33").unwrap();
        assert_eq!(address, [0xaa, 0xbb, 0xcc, 0x11, 0x22, 0x33]);
        assert_eq!(format_address(&address), "AA:BB:CC:11:22:33");
    }

    #[test]
    fn rejects_malformed_addresses() {
        for bad in ["", "aa:bb:cc:dd:ee", "aa:bb:cc:dd:ee:ff:00", "aabbccddeeff"] {
            assert_eq!(parse_address(bad), Err(AddressParseError), "{bad}");
        }
        // A five-octet prefix with a trailing separator must not pad to six.
        assert_eq!(parse_address("aa:bb:cc:dd:ee:"), Err(AddressParseError));
        // Non-hex must not be silently truncated.
        assert_eq!(parse_address("aa:bb:cc:dd:ee:zz"), Err(AddressParseError));
    }

    #[test]
    fn needs_union_accumulates_every_field() {
        let a = Needs {
            manufacturer_data: true,
            ..Needs::nothing()
        };
        let b = Needs {
            name: true,
            ..Needs::nothing()
        };
        let merged = a.union(b);
        assert!(merged.manufacturer_data && merged.name);
        assert!(!merged.service_data && !merged.service_uuids);
    }
}
