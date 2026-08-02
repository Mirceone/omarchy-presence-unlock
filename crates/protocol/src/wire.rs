//! The control-socket wire protocol, owned in one place.
//!
//! The daemon, the CLI, and the PAM module all speak these exact bytes. Retyping
//! them per crate lets a typo degrade silently to `DENY protocol` with no compile error.
//!
//! Requests are one line. `CHECK` and `CONFIRM` answer with exactly one line;
//! `STATUS` answers with zero or more `DEVICE` lines followed by [`RESP_END`],
//! so a client always knows when to stop reading.

use std::fmt::Write as _;

pub const REQ_CHECK: &str = "CHECK 1\n";
pub const REQ_CONFIRM: &str = "CONFIRM 1\n";
pub const REQ_STATUS: &str = "STATUS 1\n";
pub const RESP_ALLOW: &str = "ALLOW\n";
pub const RESP_END: &str = "END\n";

/// Renders a refusal. `reason` is a stable machine-readable token.
#[must_use]
pub fn deny(reason: &str) -> String {
    format!("DENY {reason}\n")
}

pub const DENY_BACKEND: &str = "backend";
pub const DENY_PROTOCOL: &str = "protocol";
pub const DENY_NOT_LOCKED: &str = "not-locked";
pub const DENY_NOT_ELIGIBLE: &str = "not-eligible";
pub const DENY_UNLOCK_FAILED: &str = "unlock-failed";
/// No configured device has produced a qualifying advertisement.
pub const DENY_NO_DEVICE: &str = "no-device";
/// The last qualifying advertisement is older than the freshness window.
pub const DENY_STALE: &str = "stale";
/// Fresh, but fewer qualifying advertisements than the policy requires.
pub const DENY_INSUFFICIENT_SAMPLES: &str = "insufficient-samples";
/// Some devices qualify, but fewer than the configured quorum.
pub const DENY_QUORUM: &str = "quorum";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_responses_are_newline_terminated() {
        for message in [REQ_CHECK, REQ_CONFIRM, REQ_STATUS, RESP_ALLOW, RESP_END] {
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
}
