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
//!
//! The menu runs full-screen: it takes over the terminal's alternate screen
//! buffer for the duration and hands the shell back exactly as it found it.

use crate::{client, devices, doctor, enrollment, pairing, setup};
use console::style;
use dialoguer::{
    Confirm, Input, Select,
    theme::{ColorfulTheme, Theme},
};
use omarchy_watch_unlock_protocol::{config::ConfigFile, wire};
use std::{
    fmt,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use tokio::signal::unix::{SignalKind, signal};

const UNLOCKD: &str = "omarchy-watch-unlockd";

/// `DECSET`/`DECRST` 1049 — switch to and from the terminal's alternate
/// screen buffer. Supported by every terminal this app can plausibly run
/// under; unsupported ones ignore the sequence and simply render inline.
const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";

/// Whether an action left output on screen that the user still needs to
/// read. Cancelled prompts leave nothing behind, so the menu repaints
/// immediately instead of demanding a keystroke to dismiss a blank screen.
type Action = Result<bool, String>;

/// `ColorfulTheme` hard-codes a `"{prefix} {title}"` layout for every prompt
/// header, so no field value can pull the title flush left — even an empty
/// `prompt_prefix` leaves the separating space behind. This wrapper keeps the
/// colorful theme for everything else and rewrites only the three headers,
/// dropping the prefix and its space so titles start at column 0.
struct MenuTheme(ColorfulTheme);

impl Theme for MenuTheme {
    fn format_prompt(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        if !prompt.is_empty() {
            write!(f, "{} ", self.0.prompt_style.apply_to(prompt))?;
        }
        write!(f, "{}", self.0.prompt_suffix)
    }

    fn format_input_prompt(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        default: Option<&str>,
    ) -> fmt::Result {
        if !prompt.is_empty() {
            write!(f, "{} ", self.0.prompt_style.apply_to(prompt))?;
        }
        match default {
            Some(default) => write!(
                f,
                "{} {} ",
                self.0.hint_style.apply_to(&format!("({default})")),
                self.0.prompt_suffix
            ),
            None => write!(f, "{} ", self.0.prompt_suffix),
        }
    }

    fn format_confirm_prompt(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        default: Option<bool>,
    ) -> fmt::Result {
        if !prompt.is_empty() {
            write!(f, "{} ", self.0.prompt_style.apply_to(prompt))?;
        }
        let hint = self.0.hint_style.apply_to("(y/n)");
        let suffix = &self.0.prompt_suffix;
        match default {
            None => write!(f, "{hint} {suffix}"),
            Some(true) => write!(
                f,
                "{hint} {suffix} {}",
                self.0.defaults_style.apply_to("yes")
            ),
            Some(false) => write!(
                f,
                "{hint} {suffix} {}",
                self.0.defaults_style.apply_to("no")
            ),
        }
    }

    fn format_error(&self, f: &mut dyn fmt::Write, err: &str) -> fmt::Result {
        self.0.format_error(f, err)
    }

    fn format_input_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        sel: &str,
    ) -> fmt::Result {
        self.0.format_input_prompt_selection(f, prompt, sel)
    }

    fn format_confirm_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        selection: Option<bool>,
    ) -> fmt::Result {
        self.0.format_confirm_prompt_selection(f, prompt, selection)
    }

    fn format_select_prompt_item(
        &self,
        f: &mut dyn fmt::Write,
        text: &str,
        active: bool,
    ) -> fmt::Result {
        self.0.format_select_prompt_item(f, text, active)
    }

    fn format_select_prompt_selection(
        &self,
        f: &mut dyn fmt::Write,
        prompt: &str,
        sel: &str,
    ) -> fmt::Result {
        self.0.format_select_prompt_selection(f, prompt, sel)
    }
}

fn theme() -> MenuTheme {
    MenuTheme(ColorfulTheme {
        active_item_prefix: style("→".to_string()).for_stderr().green(),
        ..ColorfulTheme::default()
    })
}

/// Key hints, spelled from what `Select` actually binds: arrows navigate,
/// Enter and Space both commit, Esc backs out.
const NAV_EXIT: &str = "↑↓ navigate   ENTER select   ESC exit";
const NAV_BACK: &str = "↑↓ navigate   ENTER select   ESC back";
const NAV_CHOICE: &str = "↑↓ navigate   ENTER/SPACE select   ESC cancel";

/// A menu header: bold title, the keys that work here, then a blank line.
/// Printed by us rather than passed to `Select` as a prompt, because
/// `Select` counts the lines it renders in order to clear them on redraw
/// and a multi-line prompt would corrupt that arithmetic.
fn header(title: &str, keys: &str) {
    println!("{}", style(title).bold());
    println!("  {}", style(keys).dim());
    println!();
}

/// Radio labels for a pick-one menu. The filled dot marks the value the
/// config currently holds, so the menu shows the live setting instead of
/// only a cursor position.
fn radios(labels: &[&str], current: Option<usize>) -> Vec<String> {
    labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let mark = if Some(index) == current { '●' } else { '○' };
            format!("({mark}) {label}")
        })
        .collect()
}

/// Lines up a plain item, such as `Back`, with the labels beside it: the
/// radio column is exactly four columns wide.
fn aligned(label: &str) -> String {
    format!("    {label}")
}

/// Owns the alternate screen buffer for as long as the menu runs. `Drop`
/// covers the ordinary returns and any `?` on the way out; the SIGINT path
/// is handled separately by [`restore_on_interrupt`], because a signal
/// death never unwinds.
struct AltScreen {
    term: console::Term,
}

impl AltScreen {
    fn enter() -> Result<Self, String> {
        let term = console::Term::stdout();
        term.write_str(ENTER_ALT_SCREEN)
            .map_err(|error| error.to_string())?;
        Ok(Self { term })
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        restore(&self.term);
    }
}

/// Leaving the alternate buffer restores the shell's scrollback verbatim.
/// The cursor is shown first because a prompt interrupted mid-draw may have
/// hidden it, and that state outlives the buffer switch.
fn restore(term: &console::Term) {
    let _ = term.show_cursor();
    let _ = term.write_str(LEAVE_ALT_SCREEN);
    let _ = term.flush();
}

/// Set while a long operation — a scan, an enrollment — owns the terminal.
/// Ctrl+C then means "stop this operation", not "quit": these are the only
/// places the menu makes the user wait, so the interrupt they reach for
/// should end the wait and keep whatever was found rather than tear down the
/// app. [`run_cancellable`] owns both flags; nothing else writes them.
static CANCELLABLE: AtomicBool = AtomicBool::new(false);
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Runs a long operation with Esc, `q`, and Ctrl+C wired to cancel it rather
/// than quit the app: the signal handler flips [`CANCEL_REQUESTED`] instead of
/// exiting while [`CANCELLABLE`] is set, and this polls the keyboard for the
/// same intent, which a signal alone cannot express.
///
/// The work runs on a worker thread so this one can poll. Polling stops the
/// moment the worker finishes, so no keystroke meant for the next menu is
/// swallowed.
fn run_cancellable<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    CANCEL_REQUESTED.store(false, Ordering::Relaxed);
    CANCELLABLE.store(true, Ordering::Relaxed);

    let (done, result) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(work());
    });

    let mut outcome = None;
    while outcome.is_none() {
        if crate::keys::cancel_key_pressed(Duration::from_millis(100)) {
            CANCEL_REQUESTED.store(true, Ordering::Relaxed);
        }
        outcome = match result.try_recv() {
            Ok(value) => Some(value),
            Err(mpsc::TryRecvError::Empty) => None,
            // The sender is dropped without a value only when the worker
            // panicked. Waiting on a channel nothing will ever fill would
            // hang the menu, so the panic surfaces here instead.
            Err(mpsc::TryRecvError::Disconnected) => {
                panic!("cancellable operation panicked")
            }
        };
    }

    CANCELLABLE.store(false, Ordering::Relaxed);
    // The loop exits only once `outcome` holds the worker's result.
    outcome.expect("worker result")
}

/// `console` does not hand Ctrl+C back as a key: it re-raises SIGINT at the
/// process, and the default disposition kills us outright without unwinding,
/// so [`AltScreen`]'s `Drop` never runs and the shell is left drawing into
/// the alternate buffer with a hidden cursor. The workspace forbids `unsafe`,
/// which rules out installing a handler directly, but `tokio`'s signal driver
/// is already a dependency and does it in safe code. Exit status follows the
/// 128 + signal convention, except during a cancellable operation.
///
/// Returns only once the handler is installed. `signal()` registers with the
/// OS when the stream is built rather than when it is first awaited, so the
/// wait is what closes the startup window in which a fast Ctrl+C would still
/// hit the default disposition and strand the terminal.
fn restore_on_interrupt() {
    let (registered, wait) = mpsc::channel::<()>();
    thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            drop(registered);
            return;
        };
        runtime.block_on(async move {
            let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
                drop(registered);
                return;
            };
            drop(registered);
            // Repeated interrupts must keep working, so this loops rather
            // than handling one signal and falling out.
            while interrupt.recv().await.is_some() {
                if CANCELLABLE.load(Ordering::Relaxed) {
                    CANCEL_REQUESTED.store(true, Ordering::Relaxed);
                    continue;
                }
                restore(&console::Term::stdout());
                std::process::exit(130);
            }
        });
    });
    // Errs as soon as the sender is dropped, which every path above does
    // immediately after registration succeeds or is abandoned.
    let _ = wait.recv();
}

/// Ctrl+C reaches us twice over: `console` re-raises SIGINT (caught above)
/// *and* returns the read as interrupted, which surfaces here. The two race,
/// so both must land on the same outcome — restore, then exit 128 + SIGINT —
/// or the exit status would depend on which one won.
fn exit_on_interrupt(error: &std::io::Error) {
    if error.kind() == std::io::ErrorKind::Interrupted {
        restore(&console::Term::stdout());
        std::process::exit(130);
    }
}

/// Turns a prompt failure into a message the menu can print and keep going —
/// except Ctrl+C, which exits cleanly rather than being reported as an error.
fn prompt_error(error: dialoguer::Error) -> String {
    let dialoguer::Error::IO(io) = error;
    exit_on_interrupt(&io);
    io.to_string()
}

/// Repaints the empty screen an action or menu is about to draw onto, so
/// only one thing is ever visible at a time.
fn clear(term: &console::Term) -> Result<(), String> {
    term.clear_screen().map_err(|error| error.to_string())
}

fn pause(term: &console::Term) {
    println!("\nPress any key to return to the menu.");
    if let Err(error) = term.read_key() {
        exit_on_interrupt(&error);
    }
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

fn ask_text(label: &str, default: Option<&str>) -> Result<String, String> {
    let theme = theme();
    let input = Input::with_theme(&theme).with_prompt(label);
    let input = match default {
        Some(default) => input.default(default.to_string()),
        None => input,
    };
    input.interact_text().map_err(prompt_error)
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

/// Which unlock-backend option the config currently holds, read from the raw
/// `config.toml` string rather than the resolved [`Settings`]:
/// `hyprlock-confirm` resolves into the same process-signal backend the
/// manual option produces, so only the unresolved name tells them apart.
fn current_backend() -> Option<usize> {
    match ConfigFile::load().ok()?.unlock_backend.as_str() {
        "hyprlock-confirm" => Some(0),
        "process-signal" => Some(1),
        "command" => Some(2),
        "disabled" => Some(3),
        _ => None,
    }
}

/// Which quorum option is in force. A loaded config with no `quorum` key runs
/// the documented `any` default; no config at all marks nothing.
fn current_quorum() -> Option<usize> {
    match ConfigFile::load().ok()?.quorum.as_deref().unwrap_or("any") {
        "any" => Some(0),
        "all" => Some(1),
        expression if expression.starts_with("at-least:") => Some(2),
        _ => None,
    }
}

/// The daemon reads its config once, at startup, so an edit made from this
/// menu does not take effect until it restarts — and until then `doctor`
/// correctly reports the running daemon disagreeing with the file. Every
/// config change here therefore ends with a restart.
///
/// Best-effort: a dev checkout with no installed unit must stay usable, and a
/// restart that fails never invalidates the change already written to disk.
fn reload_daemon() -> Result<(), String> {
    systemctl(&["restart", UNLOCKD])
        .map_err(|error| format!("config saved, but {UNLOCKD} did not restart: {error}"))
}

/// Renders a warning under a menu header. Menus that redraw in place clear
/// the screen on every pass, so anything printed at the moment it happened
/// would be wiped before it could be read; carrying it forward as a note is
/// what makes a failed restart visible instead of silently swallowed.
fn note(text: Option<&String>) {
    if let Some(text) = text {
        println!("{}\n", style(text).yellow());
    }
}

/// The privileged IRK monitor runs under `sudo` with an inherited stdin, and
/// [`run_cancellable`] polls that same terminal. A password prompt inside the
/// cancellable region would have its keystrokes split with the cancel poller,
/// and a `q` in the password would abort the enrollment. Priming the
/// credential cache here keeps any prompt outside that region.
///
/// Best-effort: a passwordless sudo, a declined prompt, or no sudo at all all
/// leave enrollment to succeed or fail on its own terms.
fn prime_sudo() {
    let _ = Command::new("sudo").arg("--validate").status();
}

/// Stops the daemon (it otherwise holds the adapter in a continuous scan),
/// runs the named enrollment provider, restarts the daemon if we were the
/// ones who stopped it, then reports `doctor` so the result is visible
/// without a separate command.
///
/// Both systemctl calls are best-effort. A dev checkout with no installed
/// unit must still be able to enroll, and a restart that fails must never
/// mask whether enrollment itself worked — that outcome is the reason the
/// user is here, so it is always the error that propagates.
fn enroll_via_provider(provider_id: &'static str) -> Action {
    let id = device_id(Some("watch"), None);

    println!("Stopping {UNLOCKD} so enrollment can use the adapter...");
    let stopped = systemctl(&["stop", UNLOCKD]);
    if let Err(error) = &stopped {
        println!("warning: could not stop {UNLOCKD} ({error}); continuing anyway.");
    }

    println!("Follow the on-device pairing prompt if one appears.");
    println!("Press Esc or Ctrl+C to stop waiting and return to the menu.");
    println!("Enrollment needs root for the kernel IRK monitor.");
    prime_sudo();
    let result = run_cancellable(move || {
        enrollment::enroll(
            provider_id,
            &enrollment::Request {
                adapter: None,
                timeout_secs: 300,
                id: &id,
                save: true,
                cancel: &CANCEL_REQUESTED,
            },
        )
    });

    if stopped.is_ok() {
        println!("Restarting {UNLOCKD}...");
        if let Err(error) = systemctl(&["start", UNLOCKD]) {
            eprintln!("warning: {UNLOCKD} did not restart: {error}");
        }
    }

    // Stopping the wait is a deliberate outcome, not a failure to report: the
    // menu redraws with nothing to dismiss.
    if CANCEL_REQUESTED.load(Ordering::Relaxed) {
        return Ok(false);
    }
    result?;
    println!();
    doctor::doctor()?;
    Ok(true)
}

/// Scan window for the in-menu picker: long enough for a phone, band, or fob
/// to advertise at least once, short enough that the menu does not feel hung.
const SCAN_SECS: u64 = 8;

/// Derives a usable device id from an advertised name, so picking a device
/// and pressing Enter is the whole flow. Anything that is not alphanumeric
/// collapses to a single dash.
fn slug(alias: &str) -> Option<String> {
    let mut out = String::with_capacity(alias.len());
    for character in alias.chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The device id is the config's primary key, not a label: `add` upserts on
/// it, `status` prints it, and removal addresses devices by it. It is the
/// app's to choose — the user is never asked.
///
/// An entry already holding this address keeps its id, because that is the
/// same hardware being re-registered rather than a new device. Otherwise the
/// advertised name provides the id, falling back to the address, and a
/// numeric suffix is appended until it is unique so enrolling something new
/// can never silently replace something enrolled.
fn device_id(alias: Option<&str>, address: Option<&str>) -> String {
    let entries = ConfigFile::load()
        .map(|config| config.devices)
        .unwrap_or_default();
    if let Some(address) = address
        && let Some(existing) = entries.iter().find(|entry| {
            entry
                .address
                .as_deref()
                .is_some_and(|known| known.eq_ignore_ascii_case(address))
        })
    {
        return existing.id.clone();
    }

    let taken: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
    let base = alias
        .and_then(slug)
        .or_else(|| address.and_then(slug))
        .unwrap_or_else(|| "device".to_string());
    if !taken.contains(&base.as_str()) {
        return base;
    }
    // N enrolled ids can block at most N of the N+1 candidates in this range,
    // so one is always free and the fallback is unreachable.
    (2..=taken.len() + 2)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .unwrap_or(base)
}

/// Runs a scan with Esc and Ctrl+C wired to end it early rather than quit.
fn scan_for_devices() -> Result<Vec<pairing::Candidate>, String> {
    run_cancellable(|| pairing::discover(None, SCAN_SECS, &CANCEL_REQUESTED))
}

/// Proximity-only devices assert nothing about their own lock state, so an
/// address is all that is needed — and the address comes from a scan run
/// right here rather than from a second terminal.
///
/// The daemon holds the adapter in a continuous scan, so it is stopped for
/// the window and restarted as soon as the results are in, well before the
/// user has finished picking.
fn enroll_other_device(term: &console::Term) -> Action {
    println!("Stopping {UNLOCKD} so the scan can use the adapter...");
    let stopped = systemctl(&["stop", UNLOCKD]);
    if let Err(error) = &stopped {
        println!("warning: could not stop {UNLOCKD} ({error}); continuing anyway.");
    }

    println!("Bring the device close and make sure it is awake.");
    println!("Press Esc or Ctrl+C to stop searching early and keep what was found.");
    let found = scan_for_devices();

    if stopped.is_ok()
        && let Err(error) = systemctl(&["start", UNLOCKD])
    {
        eprintln!("warning: {UNLOCKD} did not restart: {error}");
    }
    let candidates = match found {
        Ok(candidates) => candidates,
        // Stopping the search and finding nothing is a deliberate outcome,
        // not a failure to report.
        Err(_) if CANCEL_REQUESTED.load(Ordering::Relaxed) => return Ok(false),
        Err(error) => return Err(error),
    };

    clear(term)?;
    let mut items: Vec<String> = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{:<24} {}  {:>4} dBm{}",
                candidate.alias.as_deref().unwrap_or("(unknown)"),
                candidate.address,
                candidate.rssi,
                if candidate.paired { "  (paired)" } else { "" }
            )
        })
        .collect();
    items.push("Back".into());
    let back = items.len() - 1;

    header("Devices found — strongest first:", NAV_BACK);
    let Some(choice) = Select::with_theme(&theme())
        .items(&items)
        .default(0)
        .interact_opt()
        .map_err(prompt_error)?
    else {
        return Ok(false);
    };
    if choice == back {
        return Ok(false);
    }

    let candidate = &candidates[choice];
    let address = candidate.address.to_string();
    let id = device_id(candidate.alias.as_deref(), Some(&address));
    devices::add(
        &id,
        "presence",
        &devices::Criteria {
            address: Some(candidate.address.to_string()),
            ..devices::Criteria::default()
        },
        &devices::Overrides {
            threshold_dbm: None,
            minimum_samples: None,
            freshness_ms: None,
        },
    )?;
    if let Err(error) = reload_daemon() {
        eprintln!("warning: {error}");
    }
    println!();
    doctor::doctor()?;
    Ok(true)
}

/// Lists every registered enrollment provider — Apple Watch today, whatever
/// else the compile-time registry grows tomorrow — plus a manual fallback
/// for devices with no guided provider (the generic `presence` profile).
fn enroll_menu(term: &console::Term) -> Action {
    let mut items: Vec<String> = enrollment::PROVIDERS
        .iter()
        .map(|provider| format!("{} — {}", provider.id(), provider.description()))
        .collect();
    items.push("Other BLE device (phone, fob, band...) — scan and pick, no guided provider".into());
    items.push("Back".into());
    let back = items.len() - 1;

    header("Enroll which device:", NAV_BACK);
    let Some(choice) = Select::with_theme(&theme())
        .items(&items)
        .default(0)
        .interact_opt()
        .map_err(prompt_error)?
    else {
        return Ok(false);
    };

    if choice == back {
        Ok(false)
    } else if choice < enrollment::PROVIDERS.len() {
        enroll_via_provider(enrollment::PROVIDERS[choice].id())
    } else {
        enroll_other_device(term)
    }
}

/// Lists enrolled devices and lets one be removed. Esc/`q` backs out at
/// either level without changing anything. The refreshed list after a
/// removal is its own confirmation, so nothing is printed for it.
fn manage_devices(term: &console::Term) -> Action {
    let mut warning = None;
    loop {
        clear(term)?;
        let devices = enrolled_devices();
        if devices.is_empty() {
            println!("No devices enrolled yet.");
            return Ok(true);
        }

        let mut items: Vec<String> = devices
            .iter()
            .map(|(id, profile)| format!("Remove {id} ({profile})"))
            .collect();
        items.push("Back".into());
        let back = items.len() - 1;

        header("Manage devices:", NAV_BACK);
        note(warning.as_ref());
        let Some(choice) = Select::with_theme(&theme())
            .items(&items)
            .default(back)
            .interact_opt()
            .map_err(prompt_error)?
        else {
            return Ok(false);
        };
        if choice == back {
            return Ok(false);
        }

        let (id, _) = &devices[choice];
        let confirmed = Confirm::with_theme(&theme())
            .with_prompt(format!("Remove {id}?"))
            .default(false)
            .report(false)
            .interact()
            .map_err(prompt_error)?;
        if confirmed {
            devices::remove(id)?;
            warning = reload_daemon().err();
        }
    }
}

/// Pick-one menu: the filled radio is the backend currently in the config,
/// and the cursor starts there so the live setting is the default answer.
///
/// Applying a choice loops rather than returning, so the config is re-read
/// and the dot moves to what was just picked. The redrawn menu *is* the
/// confirmation — no message to dismiss. Only `Back`/Esc leaves.
fn choose_backend(term: &console::Term) -> Action {
    const OPTIONS: [&str; 4] = [
        "Hyprlock Alt+Enter confirmation (recommended)",
        "Signal another lock screen process",
        "Run a custom unlock command",
        "Disable",
    ];
    let mut warning = None;
    loop {
        clear(term)?;
        let current = current_backend();
        let mut items = radios(&OPTIONS, current);
        items.push(aligned("Back"));
        let back = items.len() - 1;

        header("Unlock backend:", NAV_CHOICE);
        note(warning.as_ref());
        let Some(choice) = Select::with_theme(&theme())
            .items(&items)
            .default(current.unwrap_or(0))
            .interact_opt()
            .map_err(prompt_error)?
        else {
            return Ok(false);
        };
        if choice == back {
            return Ok(false);
        }
        match choice {
            0 => devices::set_backend("hyprlock-confirm", None, None, &[])?,
            1 => {
                let process = ask_text("Process name (matched against /proc/<pid>/comm)", None)?;
                devices::set_backend("process-signal", Some(&process), None, &[])?;
            }
            2 => {
                let command = ask_text("Command, e.g. loginctl unlock-session", None)?;
                let argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
                devices::set_backend("command", None, None, &argv)?;
            }
            3 => devices::set_backend("disabled", None, None, &[])?,
            _ => unreachable!("index {choice} is past the option list"),
        }
        warning = reload_daemon().err();
    }
}

/// Pick-one menu over the quorum expression, with the configured value
/// marked. `at-least:<n>` matches on its prefix, whatever the count.
/// Updates in place exactly as [`choose_backend`] does.
fn choose_quorum(term: &console::Term) -> Action {
    const OPTIONS: [&str; 3] = [
        "any — any single enrolled device suffices (default)",
        "all — every enrolled device must be present",
        "at-least:<n> — a minimum count must be present",
    ];
    let mut warning = None;
    loop {
        clear(term)?;
        let current = current_quorum();
        let mut items = radios(&OPTIONS, current);
        items.push(aligned("Back"));
        let back = items.len() - 1;

        header("Quorum:", NAV_CHOICE);
        note(warning.as_ref());
        let Some(choice) = Select::with_theme(&theme())
            .items(&items)
            .default(current.unwrap_or(0))
            .interact_opt()
            .map_err(prompt_error)?
        else {
            return Ok(false);
        };
        if choice == back {
            return Ok(false);
        }
        let expression = match choice {
            0 => "any".to_string(),
            1 => "all".to_string(),
            2 => {
                let count = ask_text("Minimum device count", None)?;
                format!("at-least:{}", count.trim())
            }
            _ => unreachable!("index {choice} is past the option list"),
        };
        devices::set_quorum(&expression)?;
        warning = reload_daemon().err();
    }
}

/// Refreshes the daemon's per-device and aggregate decision once a second
/// until any key is pressed. The read runs on its own thread so the refresh
/// loop can poll it with a timeout instead of blocking on stdin.
fn live_status(term: &console::Term) -> Action {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Err(error) = console::Term::stdout().read_key() {
            exit_on_interrupt(&error);
        }
        let _ = tx.send(());
    });

    loop {
        clear(term)?;
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
            // The key that dismissed this view was already consumed, so the
            // caller must not ask for another one.
            return Ok(false);
        }
    }
}

const MAIN_MENU: [&str; 8] = [
    "Enroll a device",
    "Manage enrolled devices",
    "Choose unlock backend",
    "Set quorum",
    "Install lock-screen integration",
    "Run diagnostics",
    "View live status",
    "Exit",
];

/// # Errors
///
/// Returns an error only when the menu itself cannot run (not a terminal, or
/// the terminal driver fails); an action that fails is reported and the menu
/// keeps looping.
pub fn run() -> Result<(), String> {
    let term = console::Term::stdout();
    // Registered first: the handler must exist before the buffer switch it
    // is responsible for undoing.
    restore_on_interrupt();
    let _screen = AltScreen::enter()?;

    let exit = MAIN_MENU.len() - 1;
    let mut selected = 0;
    loop {
        clear(&term)?;
        header("Omarchy Watch Unlock", NAV_EXIT);
        let Some(choice) = Select::with_theme(&theme())
            .items(MAIN_MENU)
            .default(selected)
            .interact_opt()
            .map_err(prompt_error)?
        else {
            return Ok(());
        };
        if choice == exit {
            return Ok(());
        }
        selected = choice;

        clear(&term)?;
        let result = match choice {
            0 => enroll_menu(&term),
            1 => manage_devices(&term),
            2 => choose_backend(&term),
            3 => choose_quorum(&term),
            4 => setup::setup_omarchy().map(|()| true),
            5 => doctor::doctor().map(|()| true),
            _ => live_status(&term),
        };
        match result {
            Ok(false) => {}
            Ok(true) => pause(&term),
            Err(error) => {
                eprintln!("error: {error}");
                pause(&term);
            }
        }
    }
}
