//! Privileged Linux Bluetooth management-event capture.
//!
//! `BlueZ` persists a peer IRK after bonding, but the kernel reports it earlier and
//! without a filesystem race through `MGMT_EV_NEW_IRK`. The public command starts
//! a narrowly scoped, sudo-elevated copy of this binary; key bytes travel only
//! through its piped stdout and are never written to the terminal.

use btmgmt::{Client, event::Event, packet::ControllerIndex};
use futures_util::StreamExt;
use std::{
    io::{IsTerminal as _, Write as _},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Lines},
    process::{Child, ChildStdout, Command},
};

// Clippy 1.88 predates this lint, while newer Clippy versions suggest `from_mins`,
// which is newer than this project's MSRV.
#[allow(unknown_lints, clippy::duration_suboptimal_units)]
const START_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
pub struct CapturedIrk {
    pub random_address: String,
    pub identity_address: String,
    pub key: [u8; 16],
}

pub struct Monitor {
    _child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Monitor {
    /// Starts the privileged helper and waits until its management socket is bound.
    pub async fn start(adapter_index: u16) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("cannot locate this executable: {error}"))?;
        let mut child = Command::new("sudo")
            .arg("--")
            .arg(executable)
            .arg("mgmt-monitor")
            .arg("--adapter-index")
            .arg(adapter_index.to_string())
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("cannot start privileged IRK monitor: {error}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "privileged IRK monitor has no stdout pipe".to_string())?;
        let mut lines = BufReader::new(stdout).lines();
        let status = tokio::time::timeout(START_TIMEOUT, lines.next_line())
            .await
            .map_err(|_| "timed out starting the privileged IRK monitor".to_string())?
            .map_err(|error| format!("cannot read privileged IRK monitor: {error}"))?
            .ok_or_else(|| "privileged IRK monitor exited before becoming ready".to_string())?;
        if status != "READY" {
            return Err(format!("privileged IRK monitor failed: {status}"));
        }
        Ok(Self {
            _child: child,
            lines,
        })
    }

    /// Waits for the next well-formed IRK event, ignoring unrelated helper output.
    pub async fn next_irk(&mut self) -> Result<CapturedIrk, String> {
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|error| format!("cannot read privileged IRK monitor: {error}"))?
                .ok_or_else(|| {
                    "privileged IRK monitor stopped before receiving an IRK".to_string()
                })?;
            if let Some(capture) = parse_capture(&line) {
                return Ok(capture);
            }
        }
    }
}

fn parse_capture(line: &str) -> Option<CapturedIrk> {
    let mut fields = line.split_whitespace();
    if fields.next()? != "IRK" {
        return None;
    }
    let random_address = fields.next()?.to_owned();
    let identity_address = fields.next()?.to_owned();
    let key_hex = fields.next()?;
    if fields.next().is_some() || key_hex.len() != 32 {
        return None;
    }
    let mut key = [0_u8; 16];
    for (slot, octets) in key.iter_mut().zip(key_hex.as_bytes().chunks_exact(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(octets).ok()?, 16).ok()?;
    }
    Some(CapturedIrk {
        random_address,
        identity_address,
        key,
    })
}

/// Runs the root-only management event loop used by [`Monitor`].
pub fn run_helper(adapter_index: u16) -> Result<(), String> {
    if std::io::stdout().is_terminal() {
        return Err("the internal IRK monitor requires a private stdout pipe".to_string());
    }
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let client = Client::open()
            .map_err(|error| format!("cannot open Bluetooth management socket: {error}"))?;
        let mut events = client.events().await;
        println!("READY");
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        while let Some((index, event)) = events.next().await {
            if index != ControllerIndex::ControllerId(adapter_index) {
                continue;
            }
            if let Event::NewIdentityResolvingKey(received) = event {
                let key = received.key();
                println!(
                    "IRK {} {} {}",
                    received.random_address(),
                    key.address(),
                    encode_hex(key.value())
                );
                std::io::stdout()
                    .flush()
                    .map_err(|error| error.to_string())?;
            }
        }
        Err("Bluetooth management event stream ended".to_string())
    })
}

fn encode_hex(value: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(32);
    for octet in value {
        write!(encoded, "{octet:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_complete_capture_messages() {
        let expected = CapturedIrk {
            random_address: "70:81:94:0D:FB:AA".into(),
            identity_address: "AA:BB:CC:DD:EE:FF".into(),
            key: [0xab; 16],
        };
        assert_eq!(
            parse_capture(
                "IRK 70:81:94:0D:FB:AA AA:BB:CC:DD:EE:FF abababababababababababababababab"
            ),
            Some(expected)
        );
        assert_eq!(parse_capture("READY"), None);
        assert_eq!(parse_capture("IRK missing fields"), None);
        assert_eq!(
            parse_capture("IRK 70:81:94:0D:FB:AA AA:BB:CC:DD:EE:FF xyz"),
            None
        );
    }
}
