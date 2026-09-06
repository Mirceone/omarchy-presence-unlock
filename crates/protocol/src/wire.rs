//! The control-socket wire protocol, owned in one place.
//!
//! The daemon, the CLI, and the PAM module all speak these exact bytes. Retyping
//! them per crate lets a typo degrade silently to `DENY protocol` with no compile error.
//!
//! Requests are one line. `CHECK` answers with one line; `STATUS` answers with
//! zero or more `DEVICE` lines followed by [`RESP_END`], so a client always
//! knows when to stop reading.

use std::fmt::Write as _;

pub const REQ_CHECK: &str = "CHECK 1\n";
pub const REQ_STATUS: &str = "STATUS 1\n";
pub const RESP_ALLOW: &str = "ALLOW\n";
pub const RESP_END: &str = "END\n";

/// Renders a refusal. `reason` is a stable machine-readable token.
#[must_use]
pub fn deny(reason: &str) -> String {
    format!("DENY {reason}\n")
}

pub const DENY_PROTOCOL: &str = "protocol";
/// No configured device has produced a qualifying advertisement.
pub const DENY_NO_DEVICE: &str = "no-device";
/// The last qualifying advertisement is older than the freshness window.
pub const DENY_STALE: &str = "stale";
/// Fresh, but fewer qualifying advertisements than the policy requires.
pub const DENY_INSUFFICIENT_SAMPLES: &str = "insufficient-samples";
/// A matched device recently asserted a state that refuses authorisation.
pub const DENY_DEVICE_LOCKED: &str = "device-locked";
/// Some devices qualify, but fewer than the configured multi-device rule requires.
pub const DENY_MULTI_DEVICE_AUTH: &str = "multi-device-auth";

/// One `STATUS` row: `DEVICE <id> <profile> <ALLOW|DENY reason> rssi=<dbm|->`.
#[must_use]
pub fn device_status(id: &str, profile: &str, decision: &str, rssi: Option<i16>) -> String {
    let mut line = String::with_capacity(48);
    let _ = write!(line, "DEVICE {id} {profile} {decision} rssi=");
    match rssi {
        Some(dbm) => {
            let _ = writeln!(line, "{dbm}");
        }
        None => line.push_str("-\n"),
    }
    line
}

/// A parsed `STATUS` device row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRow<'a> {
    pub id: &'a str,
    pub profile: &'a str,
    pub allowed: bool,
    pub reason: Option<&'a str>,
    pub rssi: Option<i16>,
}

/// Parses one `STATUS` device row.
#[must_use]
pub fn parse_device_status(line: &str) -> Option<DeviceRow<'_>> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let mut fields = line.split_ascii_whitespace();
    if fields.next()? != "DEVICE" {
        return None;
    }
    let id = fields.next()?;
    let profile = fields.next()?;
    let (allowed, reason, rssi) = match fields.next()? {
        "ALLOW" => (true, None, fields.next()?),
        "DENY" => (false, Some(fields.next()?), fields.next()?),
        _ => return None,
    };
    if fields.next().is_some() {
        return None;
    }
    let rssi = rssi.strip_prefix("rssi=")?;
    let rssi = if rssi == "-" {
        None
    } else {
        Some(rssi.parse().ok()?)
    };
    Some(DeviceRow {
        id,
        profile,
        allowed,
        reason,
        rssi,
    })
}

/// A parsed aggregate decision line. Distinct from [`crate::Decision`], which
/// is the daemon's own verdict: this is what a client read off the wire, and
/// its reason is borrowed from that line rather than a known static token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate<'a> {
    Allow,
    Deny(&'a str),
}

/// Parses a bare aggregate line: `ALLOW`, or `DENY <reason>`. Anything else,
/// including a trailing field, is not an aggregate line.
#[must_use]
pub fn parse_decision(line: &str) -> Option<Aggregate<'_>> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let mut fields = line.split_ascii_whitespace();
    let decision = match fields.next()? {
        "ALLOW" if fields.next().is_none() => Aggregate::Allow,
        "DENY" => Aggregate::Deny(fields.next()?),
        _ => return None,
    };
    if fields.next().is_some() {
        return None;
    }
    Some(decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_responses_are_newline_terminated() {
        for message in [REQ_CHECK, REQ_STATUS, RESP_ALLOW, RESP_END] {
            assert!(message.ends_with('\n'), "{message:?} must be a full line");
        }
        assert_eq!(deny(DENY_PROTOCOL), "DENY protocol\n");
    }

    #[test]
    fn a_status_row_is_one_line_with_or_without_an_rssi() {
        assert_eq!(
            device_status("watch", "apple-continuity", "ALLOW", Some(-61)),
            "DEVICE watch apple-continuity ALLOW rssi=-61\n"
        );
        assert_eq!(
            device_status("fob", "presence", "DENY no-device", None),
            "DEVICE fob presence DENY no-device rssi=-\n"
        );
    }

    #[test]
    fn parsing_status_rows_round_trips_allowed_and_denied_devices() {
        assert_eq!(
            parse_device_status(&device_status(
                "watch",
                "apple-continuity",
                "ALLOW",
                Some(-54)
            )),
            Some(DeviceRow {
                id: "watch",
                profile: "apple-continuity",
                allowed: true,
                reason: None,
                rssi: Some(-54),
            })
        );
        assert_eq!(
            parse_device_status(&device_status(
                "watch",
                "apple-continuity",
                "DENY device-locked",
                Some(-54),
            )),
            Some(DeviceRow {
                id: "watch",
                profile: "apple-continuity",
                allowed: false,
                reason: Some("device-locked"),
                rssi: Some(-54),
            })
        );
        assert_eq!(
            parse_device_status(&device_status("fob", "presence", "DENY no-device", None)),
            Some(DeviceRow {
                id: "fob",
                profile: "presence",
                allowed: false,
                reason: Some("no-device"),
                rssi: None,
            })
        );
    }

    #[test]
    fn parsers_reject_malformed_protocol_lines() {
        for line in [
            "",
            "DEVICE watch apple-continuity ALLOW",
            "DEVICE watch apple-continuity ALLOW rssi=-54 extra",
            "DEVICE watch apple-continuity DENY rssi=-54",
            "DEVICE watch apple-continuity DENY device-locked rssi=nearby",
            "DEVICE watch apple-continuity MAYBE rssi=-54",
            "STATUS watch apple-continuity ALLOW rssi=-54",
        ] {
            assert_eq!(parse_device_status(line), None, "{line:?}");
        }
        for line in [
            "",
            "ALLOW no-device",
            "DENY",
            "DENY no-device extra",
            "MAYBE",
        ] {
            assert_eq!(parse_decision(line), None, "{line:?}");
        }
    }

    #[test]
    fn parsing_decisions_preserves_the_aggregate_reason() {
        assert_eq!(parse_decision("ALLOW\n"), Some(Aggregate::Allow));
        assert_eq!(
            parse_decision("DENY device-locked\n"),
            Some(Aggregate::Deny(DENY_DEVICE_LOCKED))
        );
    }
}
