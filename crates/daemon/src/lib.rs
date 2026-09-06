//! The daemon: a `BlueZ` scanner, a presence fleet, and a control socket.
//!
//! ```text
//! scan (BlueZ)  ->  Advertisement  ->  Fleet (presence policy)
//!                                          ^
//!                   control socket  -------+
//! ```
//!
//! The scanner owns transport concerns while the fleet owns device policy.

pub mod clock;
pub mod control;
pub mod scan;

pub use clock::boottime_ms;
pub use control::{Service, serve};
pub use scan::{ScanError, scan};

pub use omarchy_presence_unlock_protocol::config::{ConfigError, ConfigFile, Settings};
pub use omarchy_presence_unlock_protocol::{Fleet, MultiDeviceAuth};
