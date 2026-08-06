//! The daemon: a `BlueZ` scanner, a presence fleet, and a control socket.
//!
//! ```text
//! scan (BlueZ)  ->  Advertisement  ->  Fleet (per-device policy + quorum)
//!                                          ^
//!                   control socket  -------+---->  Unlocker (lock screen)
//! ```
//!
//! Each arrow is a trait or a plain data type, so a transport, a device class,
//! and a lock screen can each be replaced without touching the other two.

pub mod clock;
pub mod control;
pub mod scan;
pub mod unlock;

pub use clock::boottime_ms;
pub use control::{Service, serve};
pub use scan::{ScanError, scan};
pub use unlock::{UnlockError, Unlocker};

pub use omarchy_presence_unlock_protocol::config::{Backend, ConfigError, ConfigFile, Settings};
pub use omarchy_presence_unlock_protocol::{Fleet, Quorum};
