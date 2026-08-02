//! Pure protocol and policy code. This crate deliberately has no D-Bus or PAM dependency.
//!
//! Layering, innermost first:
//!
//! * [`ble`] — a transport-neutral advertisement. The only thing a scanner must produce.
//! * [`irk`], [`apple`] — primitives: address resolution, one vendor decoder.
//! * [`identity`] — "is this advertisement my device?"
//! * [`profile`] — "what does this device's advertisement assert?"
//! * [`presence`] — per-device evidence and the quorum over a fleet.
//! * [`config`] — the file format that builds a fleet; [`wire`], [`paths`] — the IPC contract.
//!
//! Supporting a new device class touches [`profile`] (and a decoder module) and
//! nothing else; supporting a new way of recognising one touches [`identity`].

pub mod apple;
pub mod ble;
pub mod identity;
pub mod irk;
pub mod paths;
pub mod presence;
pub mod profile;
pub mod wire;

#[cfg(feature = "config")]
pub mod config;

pub use ble::{Advertisement, Needs};
pub use identity::Identity;
pub use irk::IrkMatcher;
pub use presence::{
    Decision, Device, DeviceSpec, DeviceStatus, Eligibility, Fleet, Policy, Quorum,
};
pub use profile::{Observation, Profile};
