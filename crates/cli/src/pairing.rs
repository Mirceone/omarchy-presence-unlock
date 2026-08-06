//! Live LE discovery, interactive bonding, and IRK extraction from `BlueZ`'s
//! own bond record.
//!
//! `BlueZ` never exposes a peer's Identity Resolving Key over D-Bus, so the only
//! way to enroll a rotating device from Linux is to bond with it and read the
//! `[IdentityResolvingKey]` group out of `/var/lib/bluetooth/.../info`, which
//! needs root. This is experimental: an Apple Watch is only known to distribute
//! its IRK when *it* initiates pairing, so a Linux-central bond may complete
//! without yielding a key.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bluer::{
    Adapter, AdapterEvent, Address, AddressType, DiscoveryFilter, DiscoveryTransport, Session,
    Uuid,
    adv::{Advertisement, Type as AdvertisementType},
    agent::{Agent, DisplayPasskey, ReqError, RequestAuthorization, RequestConfirmation},
    gatt::local::{
        Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
        CharacteristicRead, Service,
    },
};
use futures_util::{FutureExt as _, StreamExt};
use omarchy_presence_unlock_protocol::{
    ble::parse_address,
    irk::{BluezIrkError, IrkMatcher, parse_bluez_info_irk},
    paths,
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use crate::devices::{self, Criteria, Overrides};
use crate::enrollment::{Cleanup, Phase, Progress, Sink};

/// How long a bond attempt may take before the `pair()` future is dropped,
/// which is what issues `CancelPairing` — there is no cancel method.
const PAIR_TIMEOUT: Duration = Duration::from_secs(90);

/// A device seen advertising during the scan window.
pub struct Candidate {
    pub address: Address,
    pub alias: Option<String>,
    pub rssi: i16,
    pub paired: bool,
}

/// Deliberately not `Runtime::new()`: that enables every driver, including
/// the signal driver. A second signal driver registers the same global
/// self-pipe with a second reactor, and both then broadcast the same SIGINT
/// — turning one Ctrl+C into two notifications for whoever is listening.
/// `BlueZ` work needs IO and timers, never signals.
fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| error.to_string())
}

/// `pair` is a CLI-only entry point, so the wizard's SIGINT handler is never
/// installed alongside it and this runtime can own signals — unlike
/// [`runtime`], which is shared with the menu and must not.
fn signal_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

async fn open_adapter(session: &Session, name: Option<&str>) -> Result<Adapter, String> {
    let adapter = match name {
        Some(name) => session.adapter(name).map_err(|error| error.to_string())?,
        None => session
            .default_adapter()
            .await
            .map_err(|error| error.to_string())?,
    };
    if !adapter
        .is_powered()
        .await
        .map_err(|error| error.to_string())?
    {
        return Err(format!(
            "Bluetooth adapter {} is not powered",
            adapter.name()
        ));
    }
    Ok(adapter)
}

/// Completes once `cancel` is set. Polled rather than awaited on
/// `signal::ctrl_c()` because this runtime deliberately has no signal driver.
async fn cancelled(cancel: &AtomicBool) {
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    loop {
        ticker.tick().await;
        if cancel.load(Ordering::Relaxed) {
            return;
        }
    }
}

/// Runs one LE discovery session and returns everything that actually
/// advertised, strongest first.
///
/// `discover_devices_with_changes` replays `BlueZ`'s whole device cache as
/// `DeviceAdded` before any radio traffic arrives, and re-emits it on every
/// property change. Keeping only devices with a live RSSI is precisely the
/// cached-versus-present distinction; deduping by address handles the re-emits.
///
/// `found` tracks the running total, so a caller drawing a scan screen can
/// show it climbing rather than only the number the window ended on.
async fn scan(
    adapter: &Adapter,
    window: Duration,
    cancel: &AtomicBool,
    found: &AtomicUsize,
) -> Result<Vec<Candidate>, String> {
    // Must precede discover_devices*, or BlueZ answers DiscoveryActive.
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..DiscoveryFilter::default()
        })
        .await
        .map_err(|error| error.to_string())?;

    let deadline = Instant::now() + window;
    let mut seen: HashMap<Address, Candidate> = HashMap::new();
    // BlueZ can end our discovery session as soon as it starts — reliably so
    // in the moments after another client's session is torn down, which is
    // exactly when the menu scans, having just stopped the daemon. The stream
    // replays the device cache before it dies, so a single attempt looks like
    // a successful scan while reporting nothing the radio actually heard.
    // Restarting until the window is spent is what keeps this a real scan.
    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        let mut events = adapter
            .discover_devices_with_changes()
            .await
            .map_err(|error| error.to_string())?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Inner value is true when the session died and is worth restarting.
        let outcome = tokio::time::timeout(remaining, async {
            // The ticker is what makes `cancel` responsive: without it a quiet
            // radio would park the loop on `next()` until the window ran out.
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    event = events.next() => {
                        let Some(event) = event else { return true };
                        match event {
                            AdapterEvent::DeviceAdded(address) => {
                                let Ok(device) = adapter.device(address) else {
                                    continue;
                                };
                                let Ok(Some(rssi)) = device.rssi().await else {
                                    continue;
                                };
                                seen.insert(
                                    address,
                                    Candidate {
                                        address,
                                        alias: device.alias().await.ok(),
                                        rssi,
                                        paired: device.is_paired().await.unwrap_or(false),
                                    },
                                );
                                found.store(seen.len(), Ordering::Relaxed);
                            }
                            AdapterEvent::DeviceRemoved(address) => {
                                seen.remove(&address);
                                found.store(seen.len(), Ordering::Relaxed);
                            }
                            AdapterEvent::PropertyChanged(_) => {}
                        }
                    }
                    _ = ticker.tick() => {
                        if cancel.load(Ordering::Relaxed) {
                            return false;
                        }
                    }
                }
            }
        })
        .await;
        // Dropping the stream ends the discovery session; the controller must
        // be free before a connection can be established.
        drop(events);
        // Elapsed window or a cancel both mean stop; only a dead session retries.
        if outcome != Ok(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let mut candidates: Vec<Candidate> = seen.into_values().collect();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.rssi));
    Ok(candidates)
}

fn print_candidates(candidates: &[Candidate]) {
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "{:>2}. {:<22} {}  {:>4} dBm  {}",
            index + 1,
            candidate.alias.as_deref().unwrap_or("(unknown)"),
            candidate.address,
            candidate.rssi,
            if candidate.paired {
                "(paired)"
            } else {
                "(not paired)"
            }
        );
    }
}

const NOTHING_FOUND: &str =
    "no advertising devices found; bring the device close and make sure it is awake";

/// Scans and returns everything currently advertising, strongest first.
///
/// Exposed as data rather than printed text so the interactive menu can offer
/// the results as a picker instead of asking for an address to be typed in.
/// An empty result is a fact about the room, not an error; only a caller with
/// nothing to show for it renders that as one.
///
/// Setting `cancel` ends the window early and returns whatever has been seen
/// so far, so a caller can cut a long wait short without losing the results.
/// `found` counts what has been seen while the window is still open.
///
/// # Errors
///
/// Returns an error when `BlueZ` is unreachable or the adapter is unusable.
pub fn discover(
    adapter_name: Option<&str>,
    scan_secs: u64,
    cancel: &AtomicBool,
    found: &AtomicUsize,
) -> Result<Vec<Candidate>, String> {
    runtime()?.block_on(async move {
        let session = Session::new().await.map_err(|error| error.to_string())?;
        let adapter = open_adapter(&session, adapter_name).await?;
        scan(&adapter, Duration::from_secs(scan_secs), cancel, found).await
    })
}

/// The name this computer advertises under, which is the name a user has to
/// recognise on the device they are pairing from.
///
/// # Errors
///
/// Returns an error when `BlueZ` is unreachable or the adapter is unusable.
pub fn adapter_alias(adapter_name: Option<&str>) -> Result<String, String> {
    runtime()?.block_on(async move {
        let session = Session::new().await.map_err(|error| error.to_string())?;
        let adapter = open_adapter(&session, adapter_name).await?;
        adapter.alias().await.map_err(|error| error.to_string())
    })
}

/// Scans and prints everything currently advertising.
///
/// # Errors
///
/// Returns an error when `BlueZ` is unreachable, the adapter is unusable, or
/// nothing advertised during the window.
pub fn list_advertising(adapter_name: Option<&str>, scan_secs: u64) -> Result<(), String> {
    let never = AtomicBool::new(false);
    let found = AtomicUsize::new(0);
    println!(
        "Scanning for {scan_secs}s on {}...",
        adapter_name.unwrap_or("the default adapter")
    );
    let candidates = discover(adapter_name, scan_secs, &never, &found)?;
    if candidates.is_empty() {
        return Err(NOTHING_FOUND.to_string());
    }
    print_candidates(&candidates);
    Ok(())
}

/// Reads one line from the terminal without blocking the reactor.
async fn prompt(message: &str) -> Result<String, String> {
    let message = message.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        print!("{message}");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => Err("cancelled".to_string()),
            Ok(_) => Ok(line),
            Err(error) => Err(error.to_string()),
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

/// An agent that answers pairing prompts from this terminal.
///
/// Leaving every handler `None` would make bluer advertise `NoInputNoOutput`,
/// and `BlueZ` would then just-works-accept with no prompt at all.
fn terminal_agent() -> Agent {
    Agent {
        request_default: true,
        request_confirmation: Some(Box::new(|request: RequestConfirmation| {
            Box::pin(async move {
                let answer = prompt(&format!(
                    "Confirm passkey {:06} for {}? [y/N] ",
                    request.passkey, request.device
                ))
                .await
                .map_err(|_| ReqError::Rejected)?;
                if matches!(answer.trim(), "y" | "Y") {
                    Ok(())
                } else {
                    Err(ReqError::Rejected)
                }
            })
        })),
        request_authorization: Some(Box::new(|request: RequestAuthorization| {
            Box::pin(async move {
                let answer = prompt(&format!(
                    "Allow incoming pairing from {}? [y/N] ",
                    request.device
                ))
                .await
                .map_err(|_| ReqError::Rejected)?;
                if matches!(answer.trim(), "y" | "Y") {
                    Ok(())
                } else {
                    Err(ReqError::Rejected)
                }
            })
        })),
        display_passkey: Some(Box::new(|request: DisplayPasskey| {
            Box::pin(async move {
                println!(
                    "Passkey for {}: {:06} ({} digits entered)",
                    request.device, request.passkey, request.entered
                );
                Ok(())
            })
        })),
        ..Default::default()
    }
}

/// Watch-tested `NoInputNoOutput`/Just Works profile.
///
/// Pairing is initiated locally with `Device1.Pair`, so no authorization prompt
/// is needed. Adding any callback would make `bluer` advertise a different I/O
/// capability and change the SMP exchange.
fn capture_agent() -> Agent {
    Agent {
        request_default: true,
        ..Default::default()
    }
}

fn select(candidates: &[Candidate], answer: &str) -> Result<usize, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Err("cancelled".into());
    }
    let index = answer
        .parse::<usize>()
        .map_err(|_| "invalid selection".to_string())?;
    if index == 0 || index > candidates.len() {
        return Err("invalid selection".into());
    }
    Ok(index - 1)
}

/// Reads a `BlueZ` bond record, elevating with `sudo` when the direct read fails.
///
/// The file also holds the LTK, so neither the text nor any part of it is ever
/// printed. The retry loop covers write-ordering surprises: `BlueZ` stores the
/// IRK in its New IRK handler, which has no ordering guarantee against `Paired`.
async fn read_bond_record(path: &Path) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if let Ok(text) = std::fs::read_to_string(path) {
            return Ok(text);
        }
        let output = Command::new("sudo")
            .arg("--")
            .arg("cat")
            .arg(path)
            .output()
            .map_err(|error| {
                format!(
                    "cannot read {}: {error}; reading BlueZ bond records requires root",
                    path.display()
                )
            })?;
        if output.status.success() {
            return String::from_utf8(output.stdout)
                .map_err(|_| format!("{} is not valid UTF-8", path.display()));
        }
        last = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }
    Err(format!(
        "cannot read {}: {last}; reading BlueZ bond records requires root",
        path.display()
    ))
}

/// Whether a self-check could be performed at all.
#[derive(Debug, PartialEq, Eq)]
enum Checked {
    /// The key resolved the address the device was advertising.
    Resolved,
    /// The device was using a fixed address, which no key resolves.
    NotApplicable,
}

/// Confirms the extracted key really resolves the address the device was using.
///
/// This is the one check that catches a byte-order regression against real
/// hardware. Only a resolvable private address can be checked at all, and the
/// caller decides whether that is worth saying out loud.
fn self_check(irk_file_order: &[u8; 16], scanned: Address) -> Result<Checked, String> {
    if scanned.0[0] >> 6 != 0b01 {
        return Ok(Checked::NotApplicable);
    }
    let mut aes_key = *irk_file_order;
    aes_key.reverse();
    if IrkMatcher::new(&aes_key).matches(&scanned.0) {
        Ok(Checked::Resolved)
    } else {
        Err(
            "the extracted key does not resolve the address this device was advertising; not saving"
                .into(),
        )
    }
}

async fn pair_flow(
    adapter_name: Option<&str>,
    scan_secs: u64,
    id: &str,
    save: bool,
) -> Result<(), String> {
    let session = Session::new().await.map_err(|error| error.to_string())?;
    let adapter = open_adapter(&session, adapter_name).await?;
    // Named binding: dropping the handle silently unregisters the agent.
    let _agent = session
        .register_agent(terminal_agent())
        .await
        .map_err(|error| error.to_string())?;

    println!("Scanning for {scan_secs}s on {}...", adapter.name());
    let candidates = scan(
        &adapter,
        Duration::from_secs(scan_secs),
        &AtomicBool::new(false),
        &AtomicUsize::new(0),
    )
    .await?;
    if candidates.is_empty() {
        return Err(NOTHING_FOUND.to_string());
    }
    print_candidates(&candidates);
    let answer = prompt(&format!(
        "Select a device [1-{}], or Enter to cancel: ",
        candidates.len()
    ))
    .await?;
    let candidate = &candidates[select(&candidates, &answer)?];

    // The scanned address may be an RPA, but its object path is the one that
    // exists in this bluetoothd session; BlueZ mutates the Address property on
    // bonding without ever rebuilding the path, so this handle stays valid.
    let device = adapter
        .device(candidate.address)
        .map_err(|error| error.to_string())?;
    if device.is_paired().await.map_err(|e| e.to_string())? {
        println!("already bonded; reading the stored key");
    } else {
        println!("Pairing with {}...", candidate.address);
        tokio::time::timeout(PAIR_TIMEOUT, device.pair())
            .await
            .map_err(|_| "pairing timed out after 90 seconds".to_string())?
            .map_err(|error| error.to_string())?;
        if !device.is_paired().await.map_err(|e| e.to_string())? {
            return Err("BlueZ returned from pairing but did not mark the device paired".into());
        }
    }

    extract_and_report(
        &adapter,
        &device,
        candidate.address,
        candidate.alias.as_deref(),
        id,
        save,
    )
    .await
}

/// Everything after a bond exists: find the identity address, read the key
/// `BlueZ` stored for it, verify it, and optionally enroll it.
///
/// `observed` is the address the peer was actually using on air — an RPA for any
/// privacy-enabled device, which is what makes the resolution self-check possible.
async fn extract_and_report(
    adapter: &Adapter,
    device: &bluer::Device,
    observed: Address,
    alias: Option<&str>,
    id: &str,
    save: bool,
) -> Result<(), String> {
    // The identity address, not device.address(), which still yields the RPA.
    let identity = device
        .remote_address()
        .await
        .map_err(|error| error.to_string())?;
    let path = paths::bluez_device_info(
        &adapter
            .address()
            .await
            .map_err(|error| error.to_string())?
            .to_string(),
        adapter.address_type().await.map_err(|e| e.to_string())? == AddressType::LeRandom,
        &identity.to_string(),
    );
    let record = read_bond_record(&path).await?;
    let irk = parse_bluez_info_irk(&record).map_err(|error| match error {
        BluezIrkError::Missing => {
            "the device bonded but distributed no IdentityResolvingKey; it cannot be used for presence unlock".to_string()
        }
        BluezIrkError::Malformed => error.to_string(),
    })?;
    if self_check(&irk, observed)? == Checked::NotApplicable {
        println!(
            "selected device was not using a resolvable private address; skipping resolution self-check"
        );
    }

    if !save {
        println!("IRK obtained and verified for {identity}; re-run with --save to enroll it");
        return Ok(());
    }
    if !alias.unwrap_or_default().contains("Watch") {
        println!("warning: this device does not look like an Apple Watch, but saving it as one");
    }
    // Base64 of the file-order bytes: config::decode_irk applies the single
    // reversal that turns them into an AES key. A second one here would break it.
    devices::add(
        id,
        "apple-continuity",
        &Criteria {
            irk_base64: Some(STANDARD.encode(irk)),
            ..Criteria::default()
        },
        &Overrides {
            threshold_dbm: None,
            minimum_samples: None,
            freshness_ms: None,
        },
    )?;
    println!("enrolled {id}; restart omarchy-presence-unlockd");
    Ok(())
}

/// Scans, bonds with a chosen device, and reports whether `BlueZ` obtained an
/// IRK for it. Writes an enrollment only when `save` is set, and only after the
/// key has been verified against the address the device was advertising.
///
/// # Errors
///
/// Returns an error for an unusable adapter, a cancelled or invalid selection, a
/// bond that fails or yields no IRK, or a bond record that cannot be read.
pub fn pair(
    adapter_name: Option<&str>,
    scan_secs: u64,
    id: &str,
    save: bool,
) -> Result<(), String> {
    signal_runtime()?.block_on(async move {
        // Ctrl-C unwinds through the same Drop paths that stop discovery and
        // unregister the agent.
        tokio::select! {
            result = pair_flow(adapter_name, scan_secs, id, save) => result,
            _ = tokio::signal::ctrl_c() => Err("cancelled".to_string()),
        }
    })
}

/// Standard services used by the Watch-tested Python capture component.
const HEART_RATE_SERVICE: Uuid = Uuid::from_u128(0x0000_180d_0000_1000_8000_0080_5f9b_34fb);
const HEART_RATE_MEASUREMENT: Uuid = Uuid::from_u128(0x0000_2a37_0000_1000_8000_0080_5f9b_34fb);
const BODY_SENSOR_LOCATION: Uuid = Uuid::from_u128(0x0000_2a38_0000_1000_8000_0080_5f9b_34fb);
const DEVICE_INFO_SERVICE: Uuid = Uuid::from_u128(0x0000_180a_0000_1000_8000_0080_5f9b_34fb);
const MANUFACTURER_NAME: Uuid = Uuid::from_u128(0x0000_2a29_0000_1000_8000_0080_5f9b_34fb);
const MODEL_NUMBER: Uuid = Uuid::from_u128(0x0000_2a24_0000_1000_8000_0080_5f9b_34fb);
const BATTERY_SERVICE: Uuid = Uuid::from_u128(0x0000_180f_0000_1000_8000_0080_5f9b_34fb);
const BATTERY_LEVEL: Uuid = Uuid::from_u128(0x0000_2a19_0000_1000_8000_0080_5f9b_34fb);
const PROTECTED_SERVICE: Uuid = Uuid::from_u128(0x2143_6587_09ba_dcfe_efcd_ab90_7856_3412);
const PROTECTED_CHARACTERISTIC: Uuid = Uuid::from_u128(0x1234_5678_90ab_cdef_fedc_ba09_8765_4321);
/// GAP appearance: generic Heart Rate Sensor, matching the proven component.
const APPEARANCE_HEART_RATE_SENSOR: u16 = 0x0340;
const IRK_WAIT: Duration = Duration::from_secs(8);

fn encrypted_read(uuid: Uuid, value: &'static [u8]) -> Characteristic {
    Characteristic {
        uuid,
        read: Some(CharacteristicRead {
            read: true,
            encrypt_read: true,
            fun: Box::new(move |_| {
                let value = value.to_vec();
                async move { Ok(value) }.boxed()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn heart_rate_measurement() -> Characteristic {
    Characteristic {
        uuid: HEART_RATE_MEASUREMENT,
        read: Some(CharacteristicRead {
            read: true,
            encrypt_read: true,
            fun: Box::new(|_| async { Ok(vec![0, 72]) }.boxed()),
            ..Default::default()
        }),
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Fun(Box::new(|mut notifier| {
                async move {
                    let mut bpm = 60_u8;
                    while !notifier.is_stopped() {
                        if notifier.notify(vec![0, bpm]).await.is_err() {
                            break;
                        }
                        bpm = if bpm == 99 { 60 } else { bpm + 1 };
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                .boxed()
            })),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Reproduces the GATT shape that captured a Watch IRK in the Python proof.
///
/// Encryption on every readable value is a secondary trigger. The primary,
/// deterministic trigger is calling `Device1.Pair` as soon as the Watch connects.
fn capture_application() -> Application {
    Application {
        services: vec![
            Service {
                uuid: HEART_RATE_SERVICE,
                primary: true,
                characteristics: vec![
                    heart_rate_measurement(),
                    encrypted_read(BODY_SENSOR_LOCATION, &[1]),
                ],
                ..Default::default()
            },
            Service {
                uuid: DEVICE_INFO_SERVICE,
                primary: true,
                characteristics: vec![
                    encrypted_read(MANUFACTURER_NAME, b"Omarchy"),
                    encrypted_read(MODEL_NUMBER, b"Presence Unlock"),
                ],
                ..Default::default()
            },
            Service {
                uuid: BATTERY_SERVICE,
                primary: true,
                characteristics: vec![encrypted_read(BATTERY_LEVEL, &[100])],
                ..Default::default()
            },
            Service {
                uuid: PROTECTED_SERVICE,
                primary: true,
                characteristics: vec![encrypted_read(PROTECTED_CHARACTERISTIC, b"Protected Info")],
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

/// Every device `BlueZ` already had a live relationship with — connected,
/// bonded, or both — when the capture flow started.
///
/// A bonded-only snapshot is too narrow for either caller. The cancellation
/// cleanup disconnects and removes whatever is missing from this set, so an
/// unrelated unpaired connection predating the flow — or one another
/// application opens alongside it — would be torn down; and the connection
/// wait claims the first connected peer missing from it, which that same
/// unrelated connection would win.
///
/// Devices merely sitting in the discovery cache are deliberately absent: the
/// peer being enrolled has very likely been seen before, and excluding it
/// would leave the wait hanging until its timeout.
async fn established_before(adapter: &Adapter) -> Result<HashSet<Address>, String> {
    let mut established = HashSet::new();
    for address in adapter
        .device_addresses()
        .await
        .map_err(|error| error.to_string())?
    {
        if let Ok(device) = adapter.device(address)
            && (device.is_connected().await.unwrap_or(false)
                || device.is_paired().await.unwrap_or(false))
        {
            established.insert(address);
        }
    }
    Ok(established)
}

/// Finds the first peer that connects *during* the flow and has not bonded
/// yet. Anything already connected or bonded when the flow started is another
/// application's, not the device being enrolled.
async fn await_incoming_connection(
    adapter: &Adapter,
    established: &HashSet<Address>,
) -> Result<(bluer::Device, Address), String> {
    loop {
        for address in adapter
            .device_addresses()
            .await
            .map_err(|error| error.to_string())?
        {
            if established.contains(&address) {
                continue;
            }
            let Ok(device) = adapter.device(address) else {
                continue;
            };
            if device.is_connected().await.unwrap_or(false)
                && !device.is_paired().await.unwrap_or(false)
            {
                return Ok((device, address));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_until_paired(device: &bluer::Device) -> Result<(), String> {
    tokio::time::timeout(PAIR_TIMEOUT, async {
        loop {
            if device.is_paired().await.unwrap_or(false) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for incoming pairing to complete".to_string())
}

/// Drives bonding from this side as soon as the peer connects.
///
/// A peer that got there first answers `InProgress`/`AlreadyExists`, which is
/// success arriving out of order rather than a failure.
async fn initiate_security(device: &bluer::Device) -> Result<(), String> {
    match tokio::time::timeout(PAIR_TIMEOUT, device.pair()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            let text = error.to_string();
            if text.contains("InProgress") || text.contains("AlreadyExists") {
                wait_until_paired(device).await
            } else {
                Err(format!("pairing/security failed: {error}"))
            }
        }
        Err(_) => Err("pairing/security timed out".to_string()),
    }
}

/// Verifies the captured key against the address the kernel saw it used on,
/// then enrolls it when asked to.
fn save_capture(
    capture: &crate::enrollment::mgmt::CapturedIrk,
    id: &str,
    save: bool,
) -> Result<(), String> {
    let rpa = Address::new(
        parse_address(&capture.random_address)
            .map_err(|error| format!("kernel reported an invalid RPA: {error}"))?,
    );
    self_check(&capture.key, rpa)?;
    if !save {
        return Ok(());
    }
    devices::add(
        id,
        "apple-continuity",
        &Criteria {
            irk_base64: Some(STANDARD.encode(capture.key)),
            ..Criteria::default()
        },
        &Overrides {
            threshold_dbm: None,
            minimum_samples: None,
            freshness_ms: None,
        },
    )
}

/// Disconnects and forgets the peripheral this flow created, reporting whether
/// the machine was left as it was found.
async fn cleanup_capture_device(
    adapter: &Adapter,
    device: &bluer::Device,
    address: Address,
    progress: Sink<'_>,
) {
    tokio::time::sleep(Duration::from_secs(1)).await;
    if device.is_connected().await.unwrap_or(false) {
        let _ = device.disconnect().await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    progress(Progress::Cleanup(Cleanup {
        label: "Temporary Bluetooth device removed",
        ok: adapter.remove_device(address).await.is_ok(),
    }));
}

/// What one capture attempt is enrolling, and where it reports to.
struct Capture<'a> {
    id: &'a str,
    save: bool,
    progress: Sink<'a>,
}

async fn active_capture(
    session: &Session,
    adapter: &Adapter,
    established: &HashSet<Address>,
    name: &str,
    timeout_secs: u64,
    capture: &Capture<'_>,
) -> Result<(), String> {
    let progress = capture.progress;
    let adapter_index = adapter
        .name()
        .strip_prefix("hci")
        .ok_or_else(|| format!("cannot determine controller index from {}", adapter.name()))?
        .parse::<u16>()
        .map_err(|_| format!("cannot determine controller index from {}", adapter.name()))?;
    let mut monitor = crate::enrollment::mgmt::Monitor::start(adapter_index).await?;
    progress(Progress::Phase(Phase::MonitorReady));
    let _agent = session
        .register_agent(capture_agent())
        .await
        .map_err(|error| error.to_string())?;
    let _application = adapter
        .serve_gatt_application(capture_application())
        .await
        .map_err(|error| error.to_string())?;
    let _advertisement = adapter
        .advertise(Advertisement {
            advertisement_type: AdvertisementType::Peripheral,
            service_uuids: [HEART_RATE_SERVICE].into_iter().collect(),
            local_name: Some(name.to_owned()),
            appearance: Some(APPEARANCE_HEART_RATE_SENSOR),
            discoverable: Some(true),
            discoverable_timeout: Some(Duration::ZERO),
            ..Advertisement::default()
        })
        .await
        .map_err(|error| error.to_string())?;
    progress(Progress::Phase(Phase::Advertising(name.to_owned())));

    let (device, observed) = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        await_incoming_connection(adapter, established),
    )
    .await
    .map_err(|_| format!("no device connected within {timeout_secs} seconds"))??;
    progress(Progress::Phase(Phase::Connected(device.alias().await.ok())));
    let result = async {
        initiate_security(&device).await?;
        progress(Progress::Phase(Phase::Bonded));
        let captured = tokio::time::timeout(IRK_WAIT, monitor.next_irk())
            .await
            .map_err(|_| {
                "bonding completed, but the kernel produced no remote IRK".to_string()
            })??;
        progress(Progress::Phase(Phase::IdentityReceived));
        save_capture(&captured, capture.id, capture.save)?;
        progress(Progress::Phase(Phase::Verified));
        Ok(())
    }
    .await;
    cleanup_capture_device(adapter, &device, observed, progress).await;
    result
}

/// Undoes what the capture flow left behind when it is cancelled part-way.
///
/// Scoped to devices that gained a connection or a bond *during* the flow —
/// anything `established` already covers is someone else's and is left alone.
///
/// A connection a third party opens after `established` was taken is still
/// swept, and that is the deliberate side of the trade. The narrower rule —
/// clean only the peer [`await_incoming_connection`] handed back — would
/// leave a bond behind whenever a peer connects and bonds inside one 200ms
/// poll, and an abandoned bond holds keys. Losing a redundant link is the
/// cheaper failure.
async fn cleanup_new_capture_devices(
    adapter: &Adapter,
    established: &HashSet<Address>,
    progress: Sink<'_>,
) {
    let Ok(addresses) = adapter.device_addresses().await else {
        return;
    };
    for address in addresses {
        if established.contains(&address) {
            continue;
        }
        let Ok(device) = adapter.device(address) else {
            continue;
        };
        if device.is_connected().await.unwrap_or(false) || device.is_paired().await.unwrap_or(false)
        {
            cleanup_capture_device(adapter, &device, address, progress).await;
        }
    }
}

/// How long to keep retrying a `Pairable` write that `BlueZ` refuses as `Busy`.
const RESTORE_ATTEMPTS: u32 = 10;

/// Puts back the pairability settings the capture flow overrode.
///
/// Tearing down the advertisement queues a controller mode change, and `BlueZ`
/// answers `Busy` to a `Pairable` write until that lands — reproducibly so on an
/// adapter that was already pairable. Retrying is what tells that collision
/// apart from a failure worth reporting.
async fn restore_pairability(adapter: &Adapter, pairable: bool, timeout: u32) -> bluer::Result<()> {
    let mut attempts = 0;
    loop {
        let result = adapter
            .set_pairable_timeout(timeout)
            .await
            .and(adapter.set_pairable(pairable).await);
        attempts += 1;
        if result.is_ok() || attempts == RESTORE_ATTEMPTS {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn advertise_flow(
    adapter_name: Option<&str>,
    timeout_secs: u64,
    id: &str,
    save: bool,
    cancel: &AtomicBool,
    progress: Sink<'_>,
) -> Result<(), String> {
    let session = Session::new().await.map_err(|error| error.to_string())?;
    let adapter = open_adapter(&session, adapter_name).await?;
    progress(Progress::Phase(Phase::AdapterReady));
    let name = adapter.alias().await.map_err(|error| error.to_string())?;
    let established = established_before(&adapter).await?;
    let previous_pairable = adapter
        .is_pairable()
        .await
        .map_err(|error| error.to_string())?;
    let previous_timeout = adapter
        .pairable_timeout()
        .await
        .map_err(|error| error.to_string())?;
    adapter
        .set_pairable_timeout(0)
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = adapter.set_pairable(true).await {
        let _ = adapter.set_pairable_timeout(previous_timeout).await;
        return Err(error.to_string());
    }

    let capture = Capture { id, save, progress };

    let result = tokio::select! {
        result = active_capture(
            &session,
            &adapter,
            &established,
            &name,
            timeout_secs,
            &capture,
        ) => result,
        () = cancelled(cancel) => {
            cleanup_new_capture_devices(&adapter, &established, progress).await;
            Err("cancelled".to_string())
        },
    };
    progress(Progress::Cleanup(Cleanup {
        label: "Adapter settings restored",
        ok: restore_pairability(&adapter, previous_pairable, previous_timeout)
            .await
            .is_ok(),
    }));
    result
}

/// Captures a Watch IRK through the proven Linux-peripheral pairing flow.
///
/// # Errors
///
/// Returns an error for an unusable adapter, management-monitor failure, timeout,
/// rejected pairing, missing IRK, failed self-check, or failed enrollment.
pub(crate) fn capture_apple_watch(
    adapter_name: Option<&str>,
    timeout_secs: u64,
    id: &str,
    save: bool,
    cancel: &AtomicBool,
    progress: Sink<'_>,
) -> Result<(), String> {
    runtime()?.block_on(advertise_flow(
        adapter_name,
        timeout_secs,
        id,
        save,
        cancel,
        progress,
    ))
}

/// One `[Section]` of a `BlueZ` info file, with its keys in file order.
struct BondSection {
    name: String,
    entries: Vec<(String, String)>,
}

/// Splits a `BlueZ` info file into its sections. Values are kept verbatim; the
/// caller decides what may be printed.
fn bond_sections(info: &str) -> Vec<BondSection> {
    let mut sections: Vec<BondSection> = Vec::new();
    for line in info.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            sections.push(BondSection {
                name: name.to_owned(),
                entries: Vec::new(),
            });
        } else if let Some((key, value)) = line.split_once('=')
            && let Some(section) = sections.last_mut()
        {
            section
                .entries
                .push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    sections
}

/// `Key=` is the only field in a bond record that is key material. Everything
/// else — `Rand`, `EDiv`, `Authenticated`, `EncSize`, names — is metadata that
/// is safe to show and useful when diagnosing what a peer actually distributed.
fn is_secret(section: &str, key: &str) -> bool {
    key == "Key" && section.ends_with("Key")
}

/// Prints what `BlueZ` recorded for every bonded device on this adapter.
///
/// This answers the only question that matters after a bond: did the peer
/// distribute an `IdentityResolvingKey`? Key material stays hidden unless
/// `show_keys` is set.
///
/// # Errors
///
/// Returns an error when `BlueZ` is unreachable, the adapter is unusable, or no
/// bond record could be read.
pub fn bond_info(adapter_name: Option<&str>, show_keys: bool) -> Result<(), String> {
    runtime()?.block_on(async move {
        let session = Session::new().await.map_err(|error| error.to_string())?;
        let adapter = open_adapter(&session, adapter_name).await?;
        let adapter_address = adapter
            .address()
            .await
            .map_err(|error| error.to_string())?
            .to_string();
        let adapter_is_random =
            adapter.address_type().await.map_err(|e| e.to_string())? == AddressType::LeRandom;
        let storage = paths::bluez_adapter_dir(&adapter_address, adapter_is_random);
        println!("{storage}", storage = storage.display());

        // Enumerated from disk, not from bluetoothd: a record can exist on disk
        // while its D-Bus object is gone, and that is exactly the case worth
        // diagnosing after a bond that seemed to work.
        let identities = list_bond_dirs(&storage)?;
        if identities.is_empty() {
            return Err(format!(
                "no bond records under {}; nothing has completed pairing on this adapter",
                storage.display()
            ));
        }
        for identity in identities {
            let path = storage.join(&identity).join("info");
            println!("\n{identity}");
            println!("  {}", path.display());

            // One unreadable record must not hide the remaining bonds — finding
            // the interesting device among several is the whole point.
            let record = match read_bond_record(&path).await {
                Ok(record) => record,
                Err(error) => {
                    println!("  => {error}");
                    continue;
                }
            };
            for section in bond_sections(&record) {
                println!("  [{}]", section.name);
                for (key, value) in &section.entries {
                    if is_secret(&section.name, key) && !show_keys {
                        println!("    {key} = <redacted, {} chars; --show-keys to reveal>", value.len());
                    } else {
                        println!("    {key} = {value}");
                    }
                }
            }
            match parse_bluez_info_irk(&record) {
                Ok(irk) => {
                    println!("  => IRK distributed: this device can be enrolled for presence unlock.");
                    if show_keys {
                        println!(
                            "     omarchy-presence-unlock add-device watch --profile apple-continuity --irk {}",
                            STANDARD.encode(irk)
                        );
                    } else {
                        println!("     re-run with --show-keys to print the enrollment command.");
                    }
                }
                Err(BluezIrkError::Missing) => {
                    println!("  => no IdentityResolvingKey: this bond cannot resolve private addresses.");
                }
                Err(BluezIrkError::Malformed) => {
                    println!("  => the IdentityResolvingKey in this record is malformed.");
                }
            }
        }
        Ok(())
    })
}

/// True for a `BlueZ` bond directory name: an uppercase colon-separated address.
/// Filters out the sibling `cache`, `settings`, and `attributes` entries.
fn is_address_dir(name: &str) -> bool {
    name.len() == 17
        && name.split(':').count() == 6
        && name.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')
}

/// Lists the identity addresses `BlueZ` has bond records for, elevating when the
/// storage directory is not readable as this user.
fn list_bond_dirs(storage: &Path) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = if let Ok(entries) = std::fs::read_dir(storage) {
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| is_address_dir(name))
            .collect()
    } else {
        let output = Command::new("sudo")
            .arg("--")
            .arg("ls")
            .arg("-1")
            .arg(storage)
            .output()
            .map_err(|error| format!("cannot list {}: {error}", storage.display()))?;
        if !output.status.success() {
            return Err(format!(
                "cannot list {}: {}; reading BlueZ bond records requires root",
                storage.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|name| is_address_dir(name))
            .map(str::to_owned)
            .collect()
    };
    names.sort_unstable();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(count: usize) -> Vec<Candidate> {
        (0..count)
            .map(|index| Candidate {
                address: Address::new([0x40, 0, 0, 0, 0, u8::try_from(index).unwrap()]),
                alias: None,
                rssi: -50,
                paired: false,
            })
            .collect()
    }

    #[test]
    fn selection_accepts_only_a_one_based_index_in_range() {
        let list = candidates(3);
        assert_eq!(select(&list, "1\n"), Ok(0));
        assert_eq!(select(&list, " 3 "), Ok(2));
        assert_eq!(select(&list, ""), Err("cancelled".into()));
        assert_eq!(select(&list, "\n"), Err("cancelled".into()));
        assert_eq!(select(&list, "0"), Err("invalid selection".into()));
        assert_eq!(select(&list, "4"), Err("invalid selection".into()));
        assert_eq!(select(&list, "-1"), Err("invalid selection".into()));
        assert_eq!(select(&list, "two"), Err("invalid selection".into()));
    }

    #[test]
    fn only_address_named_directories_are_treated_as_bonds() {
        assert!(is_address_dir("AA:BB:CC:DD:EE:FF"));
        assert!(is_address_dir("10:B5:88:A1:52:66"));
        // BlueZ keeps these beside the bond directories.
        assert!(!is_address_dir("cache"));
        assert!(!is_address_dir("settings"));
        assert!(!is_address_dir("attributes"));
        assert!(!is_address_dir("AA:BB:CC:DD:EE"));
        assert!(!is_address_dir("AA:BB:CC:DD:EE:FF:00"));
        assert!(!is_address_dir("ZZ:BB:CC:DD:EE:FF"));
    }

    #[test]
    fn the_self_check_resolves_the_kernel_test_vector_and_rejects_a_foreign_rpa() {
        let irk = [
            0x9b, 0x7d, 0x39, 0x0a, 0xa6, 0x10, 0x10, 0x34, 0x05, 0xad, 0xc8, 0x57, 0xa3, 0x34,
            0x02, 0xec,
        ];
        let rpa = Address::new([0x70, 0x81, 0x94, 0x0d, 0xfb, 0xaa]);
        assert_eq!(self_check(&irk, rpa), Ok(Checked::Resolved));
        assert!(self_check(&irk, Address::new([0x70, 0x81, 0x94, 0x0d, 0xfb, 0xab])).is_err());
        // A public (non-resolvable) address cannot be checked, and must not fail.
        assert_eq!(
            self_check(&irk, Address::new([0x00, 0x1a, 0x7d, 0xda, 0x71, 0x05])),
            Ok(Checked::NotApplicable)
        );
    }

    #[test]
    fn bond_sections_keeps_every_group_and_entry_in_file_order() {
        let info = "[General]\nName=Apple Watch\nAddressType=static\n\n[LongTermKey]\nKey=AABB\nEncSize=16\n\n[IdentityResolvingKey]\nKey=CCDD\n";
        let sections = bond_sections(info);
        let names: Vec<&str> = sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["General", "LongTermKey", "IdentityResolvingKey"]);
        assert_eq!(
            sections[0].entries,
            [
                ("Name".to_string(), "Apple Watch".to_string()),
                ("AddressType".to_string(), "static".to_string())
            ]
        );
        assert_eq!(
            sections[2].entries,
            [("Key".to_string(), "CCDD".to_string())]
        );
    }

    #[test]
    fn only_key_material_is_treated_as_secret() {
        assert!(is_secret("IdentityResolvingKey", "Key"));
        assert!(is_secret("LongTermKey", "Key"));
        assert!(is_secret("PeripheralLongTermKey", "Key"));
        // Metadata is what makes the dump useful; it must not be redacted.
        assert!(!is_secret("LongTermKey", "Rand"));
        assert!(!is_secret("LongTermKey", "EDiv"));
        assert!(!is_secret("LongTermKey", "Authenticated"));
        assert!(!is_secret("General", "Name"));
        assert!(!is_secret("ConnectionParameters", "Key"));
    }
}
