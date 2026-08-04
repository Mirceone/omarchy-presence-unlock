//! Guided first-run setup.
//!
//! Collapses the README quick-start (stop the daemon, enroll, restart, pick a
//! backend, wire the lock screen, verify) into one linear flow over the same
//! internals every other subcommand uses. Triggered by bare
//! `omarchy-watch-unlock` in a terminal, or explicitly with `init`; every
//! other subcommand is unaffected, so scripts and agents never hit a prompt.

use crate::{devices, doctor, enrollment, setup};
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
use std::process::Command;

const UNLOCKD: &str = "omarchy-watch-unlockd";

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl --user {} exited with {status}",
            args.join(" ")
        ))
    }
}

fn ask_text(prompt: &str, default: Option<&str>) -> Result<String, String> {
    let theme = theme();
    let input = Input::with_theme(&theme).with_prompt(prompt);
    let input = match default {
        Some(default) => input.default(default.to_string()),
        None => input,
    };
    input.interact_text().map_err(|error| error.to_string())
}

/// Stops the daemon (it otherwise holds the adapter in a continuous scan),
/// runs the Watch-tested enrollment provider, and restarts the daemon
/// regardless of whether enrollment succeeded.
fn enroll_apple_watch() -> Result<(), String> {
    let id = ask_text("Device id", Some("watch"))?;

    println!("Stopping {UNLOCKD} so enrollment can use the adapter...");
    systemctl(&["stop", UNLOCKD])?;
    println!(
        "On the Watch: Settings > Bluetooth > Health Devices, then select this PC when it appears."
    );
    let result = enrollment::enroll(
        "apple-watch",
        &enrollment::Request {
            adapter: None,
            timeout_secs: 300,
            id: &id,
            save: true,
        },
    );
    println!("Restarting {UNLOCKD}...");
    systemctl(&["start", UNLOCKD])?;
    result
}

/// Proximity-only devices assert nothing about their own lock state, so a
/// fixed address is enough; `devices --scan-secs` in another terminal finds
/// it without holding up this prompt.
fn enroll_other_device() -> Result<(), String> {
    println!(
        "Run `omarchy-watch-unlock devices` in another terminal to find the address, then come back here."
    );
    let id = ask_text("Device id", None)?;
    let address = ask_text("Address (AA:BB:CC:DD:EE:FF)", None)?;
    devices::add(
        &id,
        "presence",
        &devices::Criteria {
            address: Some(address),
            ..devices::Criteria::default()
        },
        &devices::Overrides {
            threshold_dbm: None,
            minimum_samples: None,
            freshness_ms: None,
        },
    )
}

fn choose_backend() -> Result<(), String> {
    const OPTIONS: [&str; 4] = [
        "Hyprlock Alt+Enter confirmation (recommended)",
        "Signal another lock screen process",
        "Run a custom unlock command",
        "Skip for now",
    ];
    let choice = Select::with_theme(&theme())
        .with_prompt("Unlock backend")
        .items(OPTIONS)
        .default(0)
        .interact()
        .map_err(|error| error.to_string())?;
    match choice {
        0 => devices::set_backend("hyprlock-confirm", None, None, &[]),
        1 => {
            let process = ask_text("Process name (matched against /proc/<pid>/comm)", None)?;
            devices::set_backend("process-signal", Some(&process), None, &[])
        }
        2 => {
            let command = ask_text("Command, e.g. loginctl unlock-session", None)?;
            let argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
            devices::set_backend("command", None, None, &argv)
        }
        _ => Ok(()),
    }
}

/// # Errors
///
/// Returns the first step that fails; already-completed steps (an enrolled
/// device, a chosen backend) are left in place, so re-running `init` resumes
/// rather than starting over.
pub fn run() -> Result<(), String> {
    println!("Omarchy Watch Unlock setup\n");

    let device_kind = Select::with_theme(&theme())
        .with_prompt("What are you enrolling?")
        .items(["Apple Watch", "Other BLE device (phone, fob, band...)"])
        .default(0)
        .interact()
        .map_err(|error| error.to_string())?;
    if device_kind == 0 {
        enroll_apple_watch()?;
    } else {
        enroll_other_device()?;
    }

    choose_backend()?;

    if Confirm::with_theme(&theme())
        .with_prompt("Install the lock-screen integration now?")
        .default(true)
        .interact()
        .map_err(|error| error.to_string())?
    {
        setup::setup_omarchy()?;
    }

    println!("Restarting {UNLOCKD}...");
    systemctl(&["restart", UNLOCKD])?;
    println!();
    doctor::doctor()?;
    Ok(())
}
