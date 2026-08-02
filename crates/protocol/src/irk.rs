//! Resolvable Private Address matching against a long-term Identity Resolving Key.

use crate::ble::Address;
use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BluezIrkError {
    #[error("this device's BlueZ bond record contains no IdentityResolvingKey")]
    Missing,
    #[error("the IdentityResolvingKey in the BlueZ bond record is malformed")]
    Malformed,
}

/// Extracts the remote IRK from the text of a `BlueZ` device `info` file.
///
/// Returns the 16 bytes in `BlueZ`'s own file order (SMP wire order, least
/// significant octet first) — the same order macOS exports, i.e. exactly what
/// `config` base64-encodes. `config::decode_irk` applies the single reversal
/// that turns them into an AES-128 key; do not reverse here.
///
/// # Errors
///
/// [`BluezIrkError::Missing`] when the section or its `Key=` is absent — the
/// "bonded but distributed no IRK" case, which callers must report distinctly.
/// [`BluezIrkError::Malformed`] for a value that is not exactly 32 hex digits.
pub fn parse_bluez_info_irk(info: &str) -> Result<[u8; 16], BluezIrkError> {
    let mut in_section = false;
    for line in info.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            if in_section {
                // The section ended without a Key=.
                return Err(BluezIrkError::Missing);
            }
            in_section = name == "IdentityResolvingKey";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "Key" {
            continue;
        }
        let hex = raw.trim();
        let hex = hex
            .strip_prefix("0x")
            .or_else(|| hex.strip_prefix("0X"))
            .unwrap_or(hex);
        if hex.len() != 32 {
            return Err(BluezIrkError::Malformed);
        }
        let mut irk = [0_u8; 16];
        for (slot, pair) in irk.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            let text = std::str::from_utf8(pair).map_err(|_| BluezIrkError::Malformed)?;
            *slot = u8::from_str_radix(text, 16).map_err(|_| BluezIrkError::Malformed)?;
        }
        return Ok(irk);
    }
    Err(BluezIrkError::Missing)
}

/// Matches Resolvable Private Addresses against a fixed IRK.
/// Holds the expanded AES-128 key schedule so hot-path matching does no key setup.
pub struct IrkMatcher {
    cipher: Aes128,
}

impl std::fmt::Debug for IrkMatcher {
    /// Never renders key material: the expanded schedule is as sensitive as the IRK.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IrkMatcher(<redacted>)")
    }
}

impl IrkMatcher {
    #[must_use]
    pub fn new(irk: &[u8; 16]) -> Self {
        Self {
            cipher: Aes128::new(GenericArray::from_slice(irk)),
        }
    }

    /// Returns true when `address` is an RPA generated from this IRK.
    /// The byte order matches `BlueZ`'s `Address::0` representation.
    #[must_use]
    pub fn matches(&self, address: &Address) -> bool {
        if address[0] >> 6 != 0b01 {
            return false;
        }
        let mut block = [0_u8; 16];
        block[13..].copy_from_slice(&address[..3]);
        self.cipher
            .encrypt_block(GenericArray::from_mut_slice(&mut block));
        block[13..] == address[3..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_resolvable_addresses() {
        let matcher = IrkMatcher::new(&[0x11; 16]);
        // Only addresses whose top two bits are 0b01 are Resolvable Private Addresses.
        assert!(!matcher.matches(&[0x00; 6]));
        assert!(!matcher.matches(&[0xff; 6]));
        assert!(!matcher.matches(&[0x80, 1, 2, 3, 4, 5]));
    }

    #[test]
    fn rejects_a_foreign_resolvable_address() {
        let matcher = IrkMatcher::new(&[0x11; 16]);
        // 0b01-prefixed, so it reaches the AES comparison, but its hash is not ours.
        assert!(!matcher.matches(&[0x40, 0x11, 0x22, 0x33, 0x44, 0x55]));
    }

    #[test]
    fn debug_never_prints_key_material() {
        assert_eq!(
            format!("{:?}", IrkMatcher::new(&[0xab; 16])),
            "IrkMatcher(<redacted>)"
        );
    }

    /// The kernel's own `test_ah()` vector, from `net/bluetooth/smp.c`:
    /// irk (little-endian, i.e. `BlueZ` file order), r = 94 81 70, hash = aa fb 0d.
    /// The RPA in `Address` (MSB-first) order is therefore 70:81:94:0D:FB:AA.
    const TEST_AH_KEY_HEX: &str = "9B7D390AA610103405ADC857A33402EC";
    const TEST_AH_RPA: Address = [0x70, 0x81, 0x94, 0x0d, 0xfb, 0xaa];

    fn info_file(key_line: &str) -> String {
        format!(
            "[General]\nName=Apple Watch\n\n[LongTermKey]\nKey=00112233445566778899AABBCCDDEEFF\n\n[IdentityResolvingKey]\n{key_line}\n\n[ConnectionParameters]\nMinInterval=6\n"
        )
    }

    #[test]
    fn parses_the_kernel_test_vector_and_resolves_its_rpa() {
        let parsed = parse_bluez_info_irk(&info_file(&format!("Key={TEST_AH_KEY_HEX}"))).unwrap();
        assert_eq!(
            parsed,
            [
                0x9b, 0x7d, 0x39, 0x0a, 0xa6, 0x10, 0x10, 0x34, 0x05, 0xad, 0xc8, 0x57, 0xa3, 0x34,
                0x02, 0xec
            ],
            "the file's bytes must be returned in file order, unreversed"
        );
        let mut aes_key = parsed;
        aes_key.reverse();
        assert!(IrkMatcher::new(&aes_key).matches(&TEST_AH_RPA));
    }

    #[test]
    fn tolerates_a_0x_prefix_and_lowercase_hex() {
        let expected = parse_bluez_info_irk(&info_file(&format!("Key={TEST_AH_KEY_HEX}"))).unwrap();
        let lowered = TEST_AH_KEY_HEX.to_lowercase();
        assert_eq!(
            parse_bluez_info_irk(&info_file(&format!("Key=0x{lowered}"))).unwrap(),
            expected
        );
        assert_eq!(
            parse_bluez_info_irk(&info_file(&format!("Key = {lowered}  "))).unwrap(),
            expected
        );
    }

    #[test]
    fn reports_a_bond_without_an_irk_as_missing() {
        let no_section =
            "[General]\nName=Speaker\n\n[LongTermKey]\nKey=00112233445566778899AABBCCDDEEFF\n";
        assert_eq!(
            parse_bluez_info_irk(no_section),
            Err(BluezIrkError::Missing)
        );
        assert_eq!(parse_bluez_info_irk(""), Err(BluezIrkError::Missing));
        // A section present but empty is still "no key distributed".
        assert_eq!(
            parse_bluez_info_irk("[IdentityResolvingKey]\n\n[General]\nName=x\n"),
            Err(BluezIrkError::Missing)
        );
    }

    #[test]
    fn a_key_in_another_section_is_not_mistaken_for_the_irk() {
        assert_eq!(
            parse_bluez_info_irk(&format!(
                "[LongTermKey]\nKey={TEST_AH_KEY_HEX}\n\n[General]\nName=x\n"
            )),
            Err(BluezIrkError::Missing)
        );
    }

    #[test]
    fn rejects_wrong_length_and_non_hex_keys() {
        for bad in [
            "9B7D390AA610103405ADC857A33402",     // 30
            "9B7D390AA610103405ADC857A33402E",    // 31
            "9B7D390AA610103405ADC857A33402ECAB", // 34
            "9B7D390AA610103405ADC857A33402EZ",   // non-hex
            "",
        ] {
            assert_eq!(
                parse_bluez_info_irk(&info_file(&format!("Key={bad}"))),
                Err(BluezIrkError::Malformed),
                "accepted {bad:?}"
            );
        }
    }
}
