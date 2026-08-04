mod atomic;
mod client;
mod devices;
mod doctor;
mod enrollment;
mod pairing;
mod setup;
mod wizard;

use clap::{CommandFactory, Parser, Subcommand};
use devices::{Criteria, Overrides};
use omarchy_watch_unlock_protocol::wire;
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Parser)]
#[command(about = "BLE proximity unlock for Omarchy")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Enroll an Apple Watch from a macOS Remote IRK, read without echoing it.
    /// Use `enroll-device --provider apple-watch` for Linux-side enrollment.
    Enroll {
        /// Device id used in status output. Defaults to `watch`.
        #[arg(long, default_value = "watch")]
        id: String,
    },
    /// Enroll any BLE device by address, service UUID, or advertised name.
    #[command(allow_negative_numbers = true)]
    AddDevice {
        /// Device id used in status output.
        id: String,
        /// Built-in profile id, for example `presence` or `apple-continuity`.
        #[arg(long, default_value = "presence")]
        profile: String,
        /// Fixed address, as shown by `devices`. Useless for a device that
        /// rotates private addresses; use --irk for those.
        #[arg(long)]
        address: Option<String>,
        /// Base64 IRK, for a device that rotates resolvable private addresses.
        #[arg(long)]
        irk: Option<String>,
        /// Match any device advertising this service UUID.
        #[arg(long)]
        service_uuid: Option<String>,
        /// Match devices whose advertised name starts with this prefix.
        #[arg(long)]
        name_prefix: Option<String>,
        /// RSSI at or above which this device counts as near.
        #[arg(long)]
        threshold_dbm: Option<i16>,
        /// Qualifying advertisements required before this device is eligible.
        #[arg(long)]
        minimum_samples: Option<u8>,
        /// How long a qualifying advertisement stays valid.
        #[arg(long)]
        freshness_ms: Option<u64>,
    },
    /// Remove an enrolled device.
    RemoveDevice { id: String },
    /// How many enrolled devices must be present: any, all, or at-least:<n>.
    Quorum { expression: String },
    /// Choose what releases the lock screen.
    Backend {
        /// disabled, hyprlock-confirm, process-signal, or command.
        name: String,
        /// Process name for process-signal, matched against /proc/<pid>/comm.
        #[arg(long)]
        process: Option<String>,
        /// SIGUSR1 (default) or SIGUSR2, for process-signal.
        #[arg(long)]
        signal: Option<String>,
        /// Argv for `command`, for example: -- loginctl unlock-session.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Scan for advertising BLE devices (live, not the `BlueZ` cache).
    Devices {
        /// Bluetooth adapter name. Uses the `BlueZ` default adapter when omitted.
        #[arg(long)]
        adapter: Option<String>,
        /// Seconds to scan before showing the list.
        #[arg(long, default_value_t = 12)]
        scan_secs: u64,
    },
    /// Proof of concept: scan, pick a device, pair it, and read the IRK `BlueZ` stored.
    /// Experimental — Apple Watch bonding to a Linux central is unproven.
    Pair {
        /// Bluetooth adapter name. Uses the `BlueZ` default adapter when omitted.
        #[arg(long)]
        adapter: Option<String>,
        /// Seconds to scan before showing the list.
        #[arg(long, default_value_t = 12)]
        scan_secs: u64,
        /// Device id to write when --save is given.
        #[arg(long, default_value = "watch")]
        id: String,
        /// Save the extracted IRK as an apple-watch enrollment. Off by default:
        /// without it the command only reports whether an IRK could be obtained.
        #[arg(long)]
        save: bool,
    },
    /// Enroll a device through a built-in provider.
    EnrollDevice {
        /// Enrollment provider id. See `profiles`.
        #[arg(long, default_value = "apple-watch")]
        provider: String,
        /// Bluetooth adapter name. Uses the `BlueZ` default adapter when omitted.
        #[arg(long)]
        adapter: Option<String>,
        /// Seconds to wait for the device to complete enrollment.
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
        /// Device id to write when --save is given.
        #[arg(long, default_value = "watch")]
        id: String,
        /// Save the resulting credentials. Off by default.
        #[arg(long)]
        save: bool,
    },
    /// List device profiles and their enrollment providers.
    Profiles,
    /// Show what `BlueZ` recorded for each bonded device, including whether the
    /// peer distributed an IRK. Needs root to read the bond records.
    BondInfo {
        /// Bluetooth adapter name. Uses the `BlueZ` default adapter when omitted.
        #[arg(long)]
        adapter: Option<String>,
        /// Print key material, including the IRK. Off by default.
        #[arg(long)]
        show_keys: bool,
    },
    /// Internal privileged listener for kernel IRK events.
    #[command(hide = true)]
    MgmtMonitor {
        #[arg(long)]
        adapter_index: u16,
    },
    /// Check configuration, the daemon socket, and the lock-screen integration.
    Doctor,
    /// Print the daemon's per-device and aggregate decision.
    Status,
    /// Confirm an unlock request from a lock-screen keybinding.
    Confirm,
    /// Install the lock-screen integration for this Omarchy build.
    SetupOmarchy,
    /// Interactive menu: enroll a device, manage enrolled devices, choose the
    /// unlock backend, set quorum, wire the lock screen, run diagnostics, and
    /// watch live status. Also runs when no subcommand is given, in a
    /// terminal.
    Init,
}

fn enroll(id: &str) -> Result<(), String> {
    let irk = rpassword::prompt_password("Paste the macOS Remote IRK (base64): ")
        .map_err(|error| error.to_string())?;
    devices::add(
        id,
        "apple-continuity",
        &Criteria {
            irk_base64: Some(irk.trim().to_string()),
            ..Criteria::default()
        },
        &Overrides {
            threshold_dbm: None,
            minimum_samples: None,
            freshness_ms: None,
        },
    )?;
    println!("Enrolled {id}. Start with: systemctl --user enable --now omarchy-watch-unlockd");
    Ok(())
}

fn status() -> Result<(), String> {
    // Informational: any well-formed reply is a success, denial included.
    for line in client::request_lines(wire::REQ_STATUS, Duration::from_millis(200))? {
        println!("{line}");
    }
    Ok(())
}

fn confirm() -> Result<(), String> {
    // A refused unlock must be a nonzero exit so the keybinding and any wrapper
    // script can tell it apart from success. CONFIRM releases a lock screen, so it
    // gets a deadline well above the in-memory CHECK path.
    let response = client::request(wire::REQ_CONFIRM, Duration::from_secs(2))?;
    if response == wire::RESP_ALLOW {
        print!("{response}");
        Ok(())
    } else {
        Err(response.trim().to_string())
    }
}

fn main() {
    let Cli { command } = Cli::parse();
    let result = match command {
        None if std::io::stdin().is_terminal() => wizard::run(),
        None => {
            let _ = Cli::command().print_help();
            println!();
            Ok(())
        }
        Some(Commands::Enroll { id }) => enroll(&id),
        Some(Commands::AddDevice {
            id,
            profile,
            address,
            irk,
            service_uuid,
            name_prefix,
            threshold_dbm,
            minimum_samples,
            freshness_ms,
        }) => devices::add(
            &id,
            &profile,
            &Criteria {
                irk_base64: irk,
                address,
                service_uuid,
                name_prefix,
            },
            &Overrides {
                threshold_dbm,
                minimum_samples,
                freshness_ms,
            },
        ),
        Some(Commands::RemoveDevice { id }) => devices::remove(&id),
        Some(Commands::Quorum { expression }) => devices::set_quorum(&expression),
        Some(Commands::Backend {
            name,
            process,
            signal,
            command,
        }) => devices::set_backend(&name, process.as_deref(), signal.as_deref(), &command),
        Some(Commands::Devices { adapter, scan_secs }) => {
            pairing::list_advertising(adapter.as_deref(), scan_secs)
        }
        Some(Commands::Pair {
            adapter,
            scan_secs,
            id,
            save,
        }) => pairing::pair(adapter.as_deref(), scan_secs, &id, save),
        Some(Commands::EnrollDevice {
            provider,
            adapter,
            timeout_secs,
            id,
            save,
        }) => enrollment::enroll(
            &provider,
            &enrollment::Request {
                adapter: adapter.as_deref(),
                timeout_secs,
                id: &id,
                save,
            },
        ),
        Some(Commands::Profiles) => {
            enrollment::print_catalog();
            Ok(())
        }
        Some(Commands::BondInfo { adapter, show_keys }) => {
            pairing::bond_info(adapter.as_deref(), show_keys)
        }
        Some(Commands::MgmtMonitor { adapter_index }) => enrollment::run_mgmt_helper(adapter_index),
        Some(Commands::Doctor) => doctor::doctor(),
        Some(Commands::Status) => status(),
        Some(Commands::Confirm) => confirm(),
        Some(Commands::SetupOmarchy) => setup::setup_omarchy(),
        Some(Commands::Init) => wizard::run(),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
