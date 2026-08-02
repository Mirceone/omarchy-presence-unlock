//! Apple Continuity decoding: the one vendor-specific decoder in the tree.
//!
//! A new vendor decoder lives beside this module and is wired in through
//! [`crate::profile::Profile`]; nothing else needs to change.

use thiserror::Error;

pub const COMPANY_ID: u16 = 0x004c;
pub const NEARBY_INFO_TYPE: u8 = 0x10;
pub const WATCH_LOCKED: u8 = 0x20;
pub const AUTO_UNLOCK_ENABLED: u8 = 0x80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NearbyInfo {
    pub watch_locked: bool,
    pub auto_unlock_enabled: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("truncated Apple Continuity TLV")]
    Truncated,
    #[error("Nearby Info is absent")]
    MissingNearbyInfo,
    #[error("Nearby Info is too short")]
    ShortNearbyInfo,
    #[error("duplicate Nearby Info")]
    DuplicateNearbyInfo,
}

/// Decode Apple's public-facing, reverse-engineered Continuity TLV framing.
/// A Nearby Info payload carries a status byte at index 1.
///
/// # Errors
///
/// Returns an error if the payload is malformed or carries no unambiguous Nearby Info TLV.
pub fn parse_nearby_info(data: &[u8]) -> Result<NearbyInfo, ParseError> {
    let mut offset = 0;
    let mut result = None;
    while offset < data.len() {
        if data.len() - offset < 2 {
            return Err(ParseError::Truncated);
        }
        let kind = data[offset];
        let length = usize::from(data[offset + 1]);
        let start = offset + 2;
        let end = start.checked_add(length).ok_or(ParseError::Truncated)?;
        let payload = data.get(start..end).ok_or(ParseError::Truncated)?;
        if kind == NEARBY_INFO_TYPE {
            if payload.len() < 3 {
                return Err(ParseError::ShortNearbyInfo);
            }
            if result.is_some() {
                return Err(ParseError::DuplicateNearbyInfo);
            }
            let flags = payload[1];
            result = Some(NearbyInfo {
                watch_locked: flags & WATCH_LOCKED != 0,
                auto_unlock_enabled: flags & AUTO_UNLOCK_ENABLED != 0,
            });
        }
        offset = end;
    }
    result.ok_or(ParseError::MissingNearbyInfo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nearby_info_from_a_tlv_sequence() {
        let data = [0x07, 1, 4, 0x10, 3, 0, 0x80, 0];
        assert_eq!(
            parse_nearby_info(&data),
            Ok(NearbyInfo {
                watch_locked: false,
                auto_unlock_enabled: true
            })
        );
    }

    #[test]
    fn rejects_truncated_data() {
        assert_eq!(parse_nearby_info(&[0x10, 5, 0]), Err(ParseError::Truncated));
    }

    #[test]
    fn rejects_a_duplicate_nearby_info() {
        let data = [0x10, 3, 0, 0x80, 0, 0x10, 3, 0, 0x00, 0];
        assert_eq!(
            parse_nearby_info(&data),
            Err(ParseError::DuplicateNearbyInfo)
        );
    }
}
