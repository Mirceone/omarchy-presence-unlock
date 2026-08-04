//! The interactive menu: the primary way to operate this app without
//! memorizing subcommands (tracks #1, "Unified interactive menu as the
//! primary CLI control surface").
//!
//! Every action here is a thin front-end over the same internals the
//! equivalent subcommand uses — `devices`, `enrollment`, `setup`, `doctor`,
//! `client` — so nothing here has logic the non-interactive CLI lacks.
//! Bare `omarchy-watch-unlock` in a terminal, or `init` explicitly, opens
//! this menu; every other subcommand is unaffected, so scripts and agents
//! never hit a prompt.

use crate::{client, devices, doctor, enrollment, setup};
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
use omarchy_watch_unlock_protocol::{config::ConfigFile, wire};
use std::{process::Command, sync::mpsc, thread, time::Duration};

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

/// Best-effort: an unreadable or not-yet-created config just means "nothing
/// enrolled yet", which is the correct display on a fresh install.
fn enrolled_devices() -> Vec<(String, String)> {
    ConfigFile::load()
        .ok()
        .and_then(|config| config.resolve().ok())
        .map(|settings| {
            settings
                .devices
                .into_iter()
                .map(|device| (device.id, device.profile.id().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Stops the daemon (it otherwise holds the adapter in a continuous scan),
/// runs the named enrollment provider, restarts the daemon regardless of
/// outcome, then reports `doctor` so the result is visible without a
/// separate command.
fn enroll_via_provider(provider_id: &str) -> Result<(), String> {
    let id = ask_text("Device id", Some("watch"))?;

    println!("Stopping {UNLOCKD} so enrollment can use the adapter...");
    systemctl(&["stop", UNLOCKD])?;
    println!("Follow the on-device pairing prompt if one appears.");
    let result = enrollment::enroll(
        provider_id,
        &enrollment::Request {
            adapter: None,
            timeout_secs: 300,
            id: &id,
            save: true,
        },
    );
    println!("Restarting {UNLOCKD}...");
    systemctl(&["start", UNLOCKD])?;
    result?;
    println!();
    doctor::doctor()
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
    )?;
    println!();
    doctor::doctor()
}

/// Lists every registered enrollment provider — Apple Watch today, whatever
/// else the compile-time registry grows tomorrow — plus a manual fallback
/// for devices with no guided provider (the generic `presence` profile).
fn enroll_menu() -> Result<(), String> {
    let mut items: Vec<String> = enrollment::PROVIDERS
        .iter()
        .map(|provider| format!("{} — {}", provider.id(), provider.description()))
        .collect();
    items.push(
        "Other BLE device (phone, fob, band...) — manual address, no guided provider".into(),
    );

    let Some(choice) = Select::with_theme(&theme())
        .with_prompt("Enroll which device?")
        .items(&items)
        .default(0)
        .interact_opt()
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };

    if choice < enrollment::PROVIDERS.len() {
        enroll_via_provider(enrollment::PROVIDERS[choice].id())
    } else {
        enroll_other_device()
    }
}

/// Lists enrolled devices and lets one be removed. Esc/`q` backs out at
/// either level without changing anything.
fn manage_devices() -> Result<(), String> {
    loop {
        let devices = enrolled_devices();
        if devices.is_empty() {
            println!("No devices enrolled yet.");
            return Ok(());
        }

        let mut items: Vec<String> = devices
            .iter()
            .map(|(id, profile)| format!("Remove {id} ({profile})"))
            .collect();
        items.push("Back".into());
        let back = items.len() - 1;

        let Some(choice) = Select::with_theme(&theme())
            .with_prompt("Manage devices")
            .items(&items)
            .default(back)
            .interact_opt()
            .map_err(|error| error.to_string())?
        else {
            return Ok(());
        };
        if choice == back {
            return Ok(());
        }

        let (id, _) = &devices[choice];
        let confirmed = Confirm::with_theme(&theme())
            .with_prompt(format!("Remove {id}?"))
            .default(false)
            .interact()
            .map_err(|error| error.to_string())?;
        if confirmed {
            devices::remove(id)?;
        }
    }
}

fn choose_backend() -> Result<(), String> {
    const OPTIONS: [&str; 4] = [
        "Hyprlock Alt+Enter confirmation (recommended)",
        "Signal another lock screen process",
        "Run a custom unlock command",
        "Disable",
    ];
    let Some(choice) = Select::with_theme(&theme())
        .with_prompt("Unlock backend")
        .items(OPTIONS)
        .default(0)
        .interact_opt()
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
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
        _ => devices::set_backend("disabled", None, None, &[]),
    }
}

fn choose_quorum() -> Result<(), String> {
    const OPTIONS: [&str; 3] = [
        "any — any single enrolled device suffices (default)",
        "all — every enrolled device must be present",
        "at-least:<n> — a minimum count must be present",
    ];
    let Some(choice) = Select::with_theme(&theme())
        .with_prompt("Quorum")
        .items(OPTIONS)
        .default(0)
        .interact_opt()
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let expression = match choice {
        0 => "any".to_string(),
        1 => "all".to_string(),
        _ => {
            let count = ask_text("Minimum device count", None)?;
            format!("at-least:{}", count.trim())
        }
    };
    devices::set_quorum(&expression)
}

/// Refreshes the daemon's per-device and aggregate decision once a second
/// until any key is pressed. The read runs on its own thread so the refresh
/// loop can poll it with a timeout instead of blocking on stdin.
fn live_status() -> Result<(), String> {
    let term = console::Term::stdout();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = console::Term::stdout().read_key();
        let _ = tx.send(());
    });

    loop {
        term.clear_screen().map_err(|error| error.to_string())?;
        println!("Live status — press any key to return\n");
        match client::request_lines(wire::REQ_STATUS, Duration::from_millis(200)) {
            Ok(lines) if lines.is_empty() => println!("(no response)"),
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
            }
            Err(error) => println!("error: {error}"),
        }
        if rx.recv_timeout(Duration::from_secs(1)).is_ok() {
            return Ok(());
        }
    }
}

const MAIN_MENU: [&str; 7] = [
    "Enroll a device",
    "Manage enrolled devices",
    "Choose unlock backend",
    "Set quorum",
    "Install lock-screen integration",
    "Run diagnostics",
    "View live status",
];

/// # Errors
///
/// Returns an error only when the menu itself cannot run (not a terminal, or
/// the terminal driver fails); an action that fails is reported and the menu
/// keeps looping.
pub fn run() -> Result<(), String> {
    println!("Omarchy Watch Unlock\n");
    loop {
        let Some(choice) = Select::with_theme(&theme())
            .with_prompt("Menu (Esc to exit)")
            .items(MAIN_MENU)
            .default(0)
            .interact_opt()
            .map_err(|error| error.to_string())?
        else {
            return Ok(());
        };
        let result = match choice {
            0 => enroll_menu(),
            1 => manage_devices(),
            2 => choose_backend(),
            3 => choose_quorum(),
            4 => setup::setup_omarchy(),
            5 => {
                println!();
                doctor::doctor()
            }
            _ => live_status(),
        };
        if let Err(error) = result {
            eprintln!("error: {error}");
        }
        println!();
    }
}
