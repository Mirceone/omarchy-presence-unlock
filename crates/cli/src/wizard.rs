//! The interactive menu: the primary way to operate this app without
//! memorizing subcommands (tracks #1, "Unified interactive menu as the
//! primary CLI control surface").
//!
//! Every action here is a thin front-end over the same internals the
//! equivalent subcommand uses — `devices`, `enrollment`, `setup`, `doctor`,
//! `client` — so nothing here has logic the non-interactive CLI lacks.
//! Bare `omarchy-presence-unlock` in a terminal, or `init` explicitly, opens
//! this menu; every other subcommand is unaffected, so scripts and agents
//! never hit a prompt.
//!
//! The menu runs full-screen: it takes over the terminal's alternate screen
//! buffer for the duration and hands the shell back exactly as it found it.
//! Screens are painted by [`crate::ui`], never printed, so a long operation
//! can revise its own checklist in place.
//!
//! Enrollment is a short guided flow that closes itself. `Enter` advances,
//! `Esc` backs out of the current screen, and `Ctrl+C` leaves the wizard —
//! nothing here asks for `Ctrl+C` as the ordinary way to dismiss a finished
//! screen. The two enrollment routes are deliberately not described alike:
//! an Apple Watch really pairs and enrolls an identity key, while any other
//! device is only located and remembered by address.

use crate::ui::{Frame, Mark, Menu, Screen};
use crate::{client, devices, doctor, enrollment, interrupt, pairing, setup, ui};
use enrollment::{Cleanup, Phase, Progress};
use omarchy_presence_unlock_protocol::{config::ConfigFile, wire};
use std::{
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const UNLOCKD: &str = "presenced";

/// `DECSET`/`DECRST` 1049 — switch to and from the terminal's alternate
/// screen buffer. Supported by every terminal this app can plausibly run
/// under; unsupported ones ignore the sequence and simply render inline.
const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";

/// Whether an action left output on screen that the user still needs to
/// read. Flows that end on their own screen leave nothing behind, so the menu
/// repaints immediately instead of demanding a keystroke to dismiss it.
type Action = Result<bool, String>;

/// Owns the alternate screen buffer for as long as the menu runs. `Drop`
/// covers the ordinary returns and any `?` on the way out; the SIGINT path
/// is handled separately by [`interrupt::install`], because a signal
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
/// The cursor is shown first because a screen painted without one leaves it
/// hidden, and that state outlives the buffer switch.
fn restore(term: &console::Term) {
    let _ = term.show_cursor();
    let _ = term.write_str(LEAVE_ALT_SCREEN);
    let _ = term.flush();
}

/// `install` takes a plain `fn`, and the handler has no terminal to hand it.
fn restore_terminal() {
    // Leaves the alternate buffer first, so anything the restart prints lands
    // in the shell's scrollback where it survives instead of being wiped.
    restore(&console::Term::stdout());
    match restart_unlockd() {
        Ok(true) => println!("Restarted {UNLOCKD}."),
        Ok(false) => {}
        Err(error) => eprintln!("warning: {error}"),
    }
}

/// Runs a long operation with Esc wired to stop it and Ctrl+C wired to leave
/// the app, repainting the screen between polls.
///
/// The cancel flag is created here and handed to `work`, so it lives exactly
/// as long as the operation it belongs to; nothing outside this call can see
/// or set it. Ctrl+C reaches it the same way Esc does — through this poll
/// loop — rather than by the signal handler reaching into the operation, so
/// there is one path from "user asked" to "operation stops".
///
/// The work runs on a worker thread so this one can poll and paint. Polling
/// stops the moment the worker finishes, so no keystroke meant for the next
/// screen is swallowed.
///
/// Returns the operation's result and whether it was stopped early.
fn run_cancellable<T: Send + 'static>(
    work: impl FnOnce(&AtomicBool) -> T + Send + 'static,
    mut repaint: impl FnMut(),
) -> (T, bool) {
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    let (done, result) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(work(&worker_cancel));
    });

    let mut outcome = None;
    while outcome.is_none() {
        repaint();
        match crate::keys::wait_for_press(Duration::from_millis(100)) {
            crate::keys::Press::Cancel => cancel.store(true, Ordering::Relaxed),
            // Leaving still unwinds the operation first: it owns adapter
            // state that only its own cleanup puts back.
            crate::keys::Press::Quit => interrupt::request_quit(),
            crate::keys::Press::Idle => {}
        }
        // Covers a SIGINT that landed between polls, when the terminal was
        // not raw and Ctrl+C was a signal rather than a byte.
        if interrupt::quit_requested() {
            cancel.store(true, Ordering::Relaxed);
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

    // The loop exits only once `outcome` holds the worker's result.
    (
        outcome.expect("worker result"),
        cancel.load(Ordering::Relaxed),
    )
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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

/// `systemctl stop` succeeds on an already-inactive unit, so its exit status
/// cannot say whether there was a daemon to put back afterwards.
fn unlockd_is_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", UNLOCKD])
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Set while the menu holds the daemon stopped so an enrollment or a scan can
/// have the adapter to itself.
///
/// Putting it back cannot be a plain statement at the end of the operation:
/// a `?`, a worker panic, or the SIGINT handler's `exit` all skip past that
/// point and would strand the user's unlock daemon until they noticed and
/// restarted it by hand. [`DaemonPause`]'s `Drop` covers the unwinding
/// exits and [`restore_terminal`] covers the `exit`, which no `Drop` runs
/// for; this flag is what lets both share one restart.
static DAEMON_STOPPED: AtomicBool = AtomicBool::new(false);

/// Stops the daemon for as long as this is alive, if there was one running.
///
/// Silent, because the screens say what is happening. Call [`DaemonPause::resume`]
/// to learn whether the restart worked; `Drop` is the safety net for the paths
/// that never get that far.
struct DaemonPause;

impl DaemonPause {
    fn stop() -> Self {
        if unlockd_is_active() {
            DAEMON_STOPPED.store(true, Ordering::Relaxed);
            let _ = systemctl(&["stop", UNLOCKD]);
        }
        Self
    }

    /// Ends the pause here rather than at the end of the caller's scope, and
    /// reports whether the daemon came up — which the screen that follows has
    /// to be able to state honestly.
    fn resume(self) -> Result<(), String> {
        let restarted = restart_unlockd().map(|_| ());
        // Explicit: the guard is spent, and the `Drop` it triggers finds the
        // flag already cleared rather than issuing a second restart.
        drop(self);
        restarted
    }
}

impl Drop for DaemonPause {
    fn drop(&mut self) {
        let _ = restart_unlockd();
    }
}

/// Puts the daemon back if this menu stopped it, reporting whether there was
/// anything to put back. Idempotent by the `swap`: whichever of the `Drop`
/// and the SIGINT path runs first is the only one that acts, so an interrupt
/// during an already-running restart cannot issue a second one.
fn restart_unlockd() -> Result<bool, String> {
    if DAEMON_STOPPED.swap(false, Ordering::Relaxed) {
        systemctl(&["start", UNLOCKD])
            .map(|()| true)
            .map_err(|error| format!("{UNLOCKD} did not restart: {error}"))
    } else {
        Ok(false)
    }
}

/// Best-effort: an unreadable or not-yet-created config just means "nothing
/// enrolled yet", which is the correct display on a fresh install.
fn enrolled_devices() -> Vec<(String, &'static str)> {
    ConfigFile::load()
        .ok()
        .and_then(|config| config.resolve().ok())
        .map(|settings| {
            settings
                .devices
                .into_iter()
                .map(|device| (device.id, device.profile.label()))
                .collect()
        })
        .unwrap_or_default()
}

/// Which unlock-backend option the config currently holds.
fn current_backend() -> Option<usize> {
    match ConfigFile::load().ok()?.unlock_backend.as_str() {
        "quattro" => Some(0),
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

/// Whether anything is wired up to actually release the lock screen. An
/// enrollment that reports success while nothing can act on it would be a lie
/// of omission, so every success screen states this.
fn unlock_state() -> &'static str {
    match ConfigFile::load() {
        Ok(config) if config.unlock_backend != "disabled" => "Ready",
        _ => "No unlock backend set",
    }
}

/// The adapter the daemon watches, which is the one an enrollment or a scan
/// started from this menu must also use. Enrolling on a different controller
/// than the daemon monitors reports success and then never fires, and the
/// symptom looks like broken hardware rather than a mismatch.
///
/// Best-effort: no config, or no `adapter` key, means `BlueZ`'s default —
/// exactly the fallback the daemon itself takes.
fn configured_adapter() -> Option<String> {
    ConfigFile::load().ok()?.adapter
}

/// The daemon reads its config once, at startup, so an edit made from this
/// menu does not take effect until it restarts — and until then `doctor`
/// correctly reports the running daemon disagreeing with the file. Every
/// config change here therefore ends with a restart.
///
/// Best-effort: a dev checkout with no installed unit must stay usable, and a
/// restart that fails never invalidates the change already written to disk.
/// `restart`, rather than `try-restart`, also starts the enabled service after
/// the first device is enrolled on a fresh installation.
fn reload_daemon() -> Result<(), String> {
    systemctl(&["restart", UNLOCKD])
        .map_err(|error| format!("config saved, but {UNLOCKD} did not restart: {error}"))
}

/// The privileged IRK monitor runs under `sudo` with an inherited stdin, and
/// [`run_cancellable`] polls that same terminal. A password prompt inside the
/// cancellable region would have its keystrokes split with the cancel poller,
/// losing characters from the passphrase. Priming the credential cache before
/// the flow starts keeps any prompt outside that region.
///
/// Returns without touching the screen when the cache is already warm, which
/// is the common case on a second attempt.
fn prime_sudo(screen: &Screen, title: &str, step: &str) -> Result<(), String> {
    let warm = Command::new("sudo")
        .args(["--non-interactive", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if warm {
        return Ok(());
    }
    let mut frame = screen.frame();
    frame.title(title, Some(step));
    frame.blank();
    frame.line("Secure pairing reads key material straight from the kernel,");
    frame.line("which needs administrator access.");
    frame.blank();
    screen.draw_above_output(&frame)?;
    // Best-effort: a declined prompt or no sudo at all leaves enrollment to
    // fail on its own terms, with a message about what it could not do.
    let _ = Command::new("sudo").arg("--validate").status();
    Ok(())
}

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

/// A screen with nothing to choose, shown until it is dismissed.
fn show(screen: &Screen, frame: Frame) -> Result<(), String> {
    let mut frame = frame;
    frame.blank();
    frame.dim(ui::NAV_RETURN);
    screen.draw(&frame)?;
    ui::wait_for_dismiss(screen)
}

/// A screen that says its piece and then moves on by itself.
fn flash(screen: &Screen, frame: &Frame) -> Result<(), String> {
    screen.draw(frame)?;
    thread::sleep(Duration::from_millis(1200));
    Ok(())
}

/// Reports a failure that has no recovery beyond going back.
fn problem(screen: &Screen, title: &str, error: &str) -> Action {
    let mut frame = screen.frame();
    frame.title(title, None);
    frame.blank();
    frame.warn(&ui::sentence(error));
    show(screen, frame)?;
    Ok(false)
}

// ---------------------------------------------------------------------------
// Guided enrollment
// ---------------------------------------------------------------------------

/// How long an enrollment waits for the device before giving up.
const ENROLL_TIMEOUT_SECS: u64 = 300;

/// What the live pairing screen knows. Written by the enrollment worker
/// through its progress sink and read by the repaint on this thread, which is
/// the only reason it needs a lock.
#[derive(Default)]
struct PairState {
    reached: Option<Phase>,
    advertising_as: Option<String>,
    device_name: Option<String>,
    cleanup: Vec<Cleanup>,
}

impl PairState {
    fn apply(&mut self, progress: Progress) {
        match progress {
            Progress::Phase(phase) => {
                match &phase {
                    Phase::Advertising(name) => self.advertising_as = Some(name.clone()),
                    Phase::Connected(name) => self.device_name.clone_from(name),
                    _ => {}
                }
                self.reached = Some(phase);
            }
            Progress::Cleanup(cleanup) => self.cleanup.push(cleanup),
        }
    }

    /// How far the flow got, as a number a checklist can compare against.
    /// Nothing reported yet ranks below the first phase.
    fn rank(&self) -> i16 {
        self.reached
            .as_ref()
            .map_or(-1, |phase| i16::from(phase.rank()))
    }
}

/// A checklist row that reads the same whatever its state.
fn milestone(frame: &mut Frame, rank: i16, done_at: i16, label: &str) {
    let mark = if rank >= done_at {
        Mark::Done
    } else if rank == done_at - 1 {
        Mark::Active
    } else {
        Mark::Pending
    };
    frame.mark(mark, label);
}

/// A checklist row whose wording changes with its state: an ellipsis while it
/// is happening, and the past tense once it has.
fn milestone_tensed(frame: &mut Frame, rank: i16, done_at: i16, doing: &str, done: &str) {
    if rank >= done_at {
        frame.mark(Mark::Done, done);
    } else if rank == done_at - 1 {
        frame.mark(Mark::Active, &format!("{doing}\u{2026}"));
    } else {
        frame.mark(Mark::Pending, doing);
    }
}

/// The live pairing screen.
///
/// Before the device connects the checklist is about this computer getting
/// ready, and the only useful thing to say is what to tap and how long is
/// left. Once it connects, that is all settled and the checklist becomes the
/// remaining security handshake.
fn pair_frame(
    screen: &Screen,
    provider: &enrollment::Provider,
    state: &PairState,
    advertised_as: &str,
    remaining: Duration,
) -> Frame {
    let label = provider.label();
    let rank = state.rank();
    let mut frame = screen.frame();
    frame.title(&format!("Pair {label}"), Some("Step 2 of 3"));
    frame.blank();
    milestone(&mut frame, rank, 0, "Bluetooth adapter ready");
    milestone(&mut frame, rank, 1, "Secure pairing monitor ready");
    if rank < 3 {
        let name = state.advertising_as.as_deref().unwrap_or(advertised_as);
        milestone(
            &mut frame,
            rank,
            2,
            &format!("Advertising as \u{201c}{name}\u{201d}"),
        );
        frame.mark(
            if rank >= 2 {
                Mark::Active
            } else {
                Mark::Pending
            },
            &format!("Waiting for {label}\u{2026}"),
        );
        frame.blank();
        frame.line(provider.guide().hint);
        frame.blank();
        frame.line(format!("Time remaining: {}", ui::countdown(remaining)));
    } else {
        frame.mark(Mark::Done, &format!("{label} connected"));
        milestone_tensed(
            &mut frame,
            rank,
            4,
            "Completing secure pairing",
            "Secure pairing completed",
        );
        milestone_tensed(
            &mut frame,
            rank,
            5,
            "Receiving device identity",
            "Device identity received",
        );
        milestone_tensed(
            &mut frame,
            rank,
            6,
            "Verifying enrollment",
            "Enrollment verified",
        );
    }
    frame.blank();
    frame.dim(ui::NAV_CANCEL);
    frame
}

/// What went wrong, in the user's terms, plus what to try — both chosen by how
/// far the flow got, because the phase reached is what names the step that
/// then failed.
fn failure_advice(label: &str, hint: &str, rank: i16) -> (String, Vec<String>) {
    let forget = format!("Remove this computer from the {label}\u{2019}s Bluetooth devices");
    let close = format!("Keep the {label} unlocked and close to the computer");
    let again = "Start pairing again".to_string();
    match rank {
        ..=-1 => (
            "The Bluetooth adapter could not be opened.".to_string(),
            vec![
                "Check that Bluetooth is switched on".to_string(),
                "Make sure the configured adapter exists".to_string(),
                again,
            ],
        ),
        0 => (
            "The secure pairing monitor could not be started.".to_string(),
            vec![
                "Secure pairing needs administrator access".to_string(),
                "Answer the password prompt, or configure sudo to allow it".to_string(),
                again,
            ],
        ),
        1 => (
            "This computer could not be made discoverable.".to_string(),
            vec![
                "Make sure no other application is using the adapter".to_string(),
                "Check that Bluetooth is switched on".to_string(),
                again,
            ],
        ),
        2 => (
            format!("The {label} never connected."),
            vec![hint.to_string(), close, again],
        ),
        3 => (
            format!("The {label} connected, but secure pairing did not complete."),
            vec![forget, close, again],
        ),
        4 => (
            format!("The {label} connected, but no device identity was received."),
            vec![forget, close, again],
        ),
        _ => (
            "The device identity could not be verified.".to_string(),
            vec![forget, again],
        ),
    }
}

/// The technical account, kept behind a menu entry so the ordinary failure
/// screen can stay in plain language.
fn pairing_details(
    screen: &Screen,
    state: &PairState,
    error: &str,
    daemon: &Result<(), String>,
) -> Result<(), String> {
    let mut frame = screen.frame();
    frame.title("Pairing details", None);
    frame.blank();
    frame.line("Stage");
    frame.line(format!(
        "  {}",
        state
            .reached
            .as_ref()
            .map_or("Opening the Bluetooth adapter", Phase::waiting_for)
    ));
    frame.blank();
    frame.line("Error");
    frame.line(format!("  {}", ui::sentence(error)));
    frame.blank();
    frame.line("Cleanup");
    for cleanup in &state.cleanup {
        frame.mark(
            if cleanup.ok { Mark::Done } else { Mark::Failed },
            cleanup.label,
        );
    }
    match daemon {
        Ok(()) => frame.mark(Mark::Done, "Unlock service resumed"),
        Err(error) => frame.mark(Mark::Failed, error),
    }
    show(screen, frame)
}

/// The instructions shown before anything is started.
///
/// Deliberately a separate screen: beginning the long operation and printing
/// the instructions while it is already running gives the user no moment to
/// read them, and no way to back out.
fn pair_instructions(
    screen: &Screen,
    provider: &enrollment::Provider,
    advertised_as: &str,
) -> Result<bool, String> {
    let label = provider.label();
    let mut head = screen.frame();
    head.title(&format!("Pair {label}"), Some("Step 1 of 3"));
    head.blank();
    head.line(provider.guide().summary);
    head.blank();
    head.line(format!("On your {label}:"));
    head.blank();
    for (index, step) in provider.guide().steps.iter().enumerate() {
        head.step(index + 1, &step.replace("{name}", advertised_as));
    }
    head.blank();
    head.line("The unlock service will pause during pairing and resume afterward.");

    let choice = Menu::new(head, vec!["Continue".into(), "Back".into()])
        .footer(ui::NAV_BACK)
        .run(screen)?;
    Ok(choice == Some(0))
}

/// Everything the success screen states, without the menu under it.
fn pair_success_frame(
    screen: &Screen,
    provider: &enrollment::Provider,
    id: &str,
    state: &PairState,
    daemon: &Result<(), String>,
) -> Frame {
    let label = provider.label();
    let mut frame = screen.frame();
    frame.title(&format!("{label} enrolled"), Some("Step 3 of 3"));
    frame.blank();
    frame.mark(Mark::Done, "Pairing completed");
    frame.mark(Mark::Done, "Device identity verified");
    match daemon {
        Ok(()) => frame.mark(Mark::Done, "Unlock service resumed"),
        Err(error) => frame.mark(Mark::Failed, error),
    }
    frame.blank();
    frame.line("Device");
    frame.field("Name", state.device_name.as_deref().unwrap_or(label));
    frame.field("ID", id);
    frame.field("Security", provider.profile().label());
    frame.field("Unlock", unlock_state());
    frame
}

/// Everything after a successful capture: what happened, what was enrolled,
/// and the two things worth doing next.
fn pair_success(
    screen: &Screen,
    provider: &enrollment::Provider,
    id: &str,
    state: &PairState,
    daemon: &Result<(), String>,
) -> Action {
    loop {
        let head = pair_success_frame(screen, provider, id, state, daemon);
        let choice = Menu::new(head, vec!["Done".into(), "View diagnostics".into()])
            .footer(ui::NAV_SELECT)
            .run(screen)?;
        match choice {
            Some(1) => diagnostics(screen)?,
            // Done, and Esc, both mean the flow is over.
            _ => return Ok(false),
        }
    }
}

/// Offers the ways out of a failed pairing. `true` asks for another attempt.
fn pair_failure(
    screen: &Screen,
    provider: &enrollment::Provider,
    state: &PairState,
    error: &str,
    daemon: &Result<(), String>,
) -> Result<bool, String> {
    let (headline, tips) = failure_advice(provider.label(), provider.guide().hint, state.rank());
    loop {
        let mut head = screen.frame();
        head.title(
            &format!("Couldn\u{2019}t enroll {}", provider.label()),
            None,
        );
        head.blank();
        head.line(headline.clone());
        head.blank();
        head.line("Try this:");
        head.blank();
        for tip in &tips {
            head.bullet(tip);
        }

        let choice = Menu::new(
            head,
            vec!["Try again".into(), "View details".into(), "Back".into()],
        )
        .footer(ui::NAV_SELECT)
        .run(screen)?;
        match choice {
            Some(0) => return Ok(true),
            Some(1) => pairing_details(screen, state, error, daemon)?,
            _ => return Ok(false),
        }
    }
}

/// One attempt: pause the daemon, run the capture, and paint its progress.
fn run_pairing(
    screen: &Screen,
    provider: &'static enrollment::Provider,
    id: &str,
    advertised_as: &str,
) -> (PairState, Result<(), String>, bool, Result<(), String>) {
    let state = Arc::new(Mutex::new(PairState::default()));
    let worker_state = Arc::clone(&state);
    let adapter = configured_adapter();
    let provider_id = provider.id();
    let worker_id = id.to_string();

    let pause = DaemonPause::stop();
    let started = Instant::now();
    let budget = Duration::from_secs(ENROLL_TIMEOUT_SECS);

    let mut painted: Option<Frame> = None;
    let (result, cancelled) = run_cancellable(
        move |cancel| {
            let sink = move |progress: Progress| {
                if let Ok(mut state) = worker_state.lock() {
                    state.apply(progress);
                }
            };
            enrollment::enroll(
                provider_id,
                &enrollment::Request {
                    adapter: adapter.as_deref(),
                    timeout_secs: ENROLL_TIMEOUT_SECS,
                    id: &worker_id,
                    save: true,
                    cancel,
                    progress: &sink,
                },
            )
        },
        || {
            let Ok(state) = state.lock() else { return };
            let frame = pair_frame(
                screen,
                provider,
                &state,
                advertised_as,
                budget.saturating_sub(started.elapsed()),
            );
            // Only a changed screen is written, so the countdown ticks once a
            // second instead of the poll repainting ten times over it.
            if painted.as_ref() != Some(&frame) {
                let _ = screen.draw(&frame);
                painted = Some(frame);
            }
        },
    );

    let mut daemon = pause.resume();
    // A fresh install enables but does not start an unconfigured daemon. Once
    // enrollment has written the first device, start it here; an already-active
    // daemon was resumed by the pause guard above and needs no second restart.
    if result.is_ok() && daemon.is_ok() && !unlockd_is_active() {
        daemon = reload_daemon();
    }
    // The worker is finished, so nothing else holds the lock; a poisoned lock
    // means the worker panicked, which `run_cancellable` has already turned
    // into a panic here.
    let state = Arc::try_unwrap(state)
        .unwrap_or_else(|_| unreachable!("the worker thread has ended"))
        .into_inner()
        .unwrap_or_default();
    (state, result, cancelled, daemon)
}

/// What a cancelled attempt left behind, which is the whole point of the
/// screen: the user stopped a security operation part-way and is owed a
/// statement about what state the machine is in.
///
/// Every line is a fact the flow reported, so a cleanup that failed says so
/// rather than being papered over with the reassuring version.
fn pair_cancelled_frame(screen: &Screen, state: &PairState, daemon: &Result<(), String>) -> Frame {
    let mut frame = screen.frame();
    frame.title("Pairing cancelled", None);
    frame.blank();
    frame.line("No device was enrolled.");
    for cleanup in &state.cleanup {
        if cleanup.ok {
            frame.line(format!("{}.", cleanup.label));
        } else {
            frame.warn(&format!("{} could not be undone.", cleanup.label));
        }
    }
    match daemon {
        Ok(()) => frame.line("The unlock service is running again."),
        Err(error) => frame.warn(error),
    }
    frame.blank();
    frame.dim("Returning to device enrollment\u{2026}");
    frame
}

/// The full guided flow for one provider, from instructions to outcome.
fn enroll_guided(screen: &Screen, provider: &'static enrollment::Provider) -> Action {
    let advertised_as = match pairing::adapter_alias(configured_adapter().as_deref()) {
        Ok(name) => name,
        Err(error) => {
            return problem(
                screen,
                &format!("Can\u{2019}t pair {}", provider.label()),
                &error,
            );
        }
    };
    let id = device_id(Some("watch"), None);

    loop {
        if !pair_instructions(screen, provider, &advertised_as)? {
            return Ok(false);
        }
        prime_sudo(screen, &format!("Pair {}", provider.label()), "Step 2 of 3")?;

        let (state, result, cancelled, daemon) = run_pairing(screen, provider, &id, &advertised_as);
        // Ctrl+C asked to leave, and the operation has now unwound; the caller
        // returns rather than painting another screen.
        if interrupt::quit_requested() {
            return Ok(false);
        }
        if cancelled {
            flash(screen, &pair_cancelled_frame(screen, &state, &daemon))?;
            return Ok(false);
        }
        match result {
            Ok(()) => return pair_success(screen, provider, &id, &state, &daemon),
            Err(error) => {
                if !pair_failure(screen, provider, &state, &error, &daemon)? {
                    return Ok(false);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Proximity devices
// ---------------------------------------------------------------------------

/// Scan window for the in-menu picker: long enough for a phone, band, or fob
/// to advertise at least once, short enough that the menu does not feel hung.
const SCAN_SECS: u64 = 8;

const FINDER_TITLE: &str = "Find a proximity device";

/// A plain-language reading of a signal strength, so the list can be judged
/// without knowing what a dBm is.
fn signal_quality(rssi: i16) -> &'static str {
    match rssi {
        -55..=0 => "Excellent",
        -70..=-56 => "Good",
        _ => "Weak",
    }
}

/// dBm with a typographic minus, matching the rest of the interface.
fn dbm(rssi: i16) -> String {
    format!("\u{2212}{} dBm", rssi.abs())
}

/// What to call a device in the picker.
///
/// `BlueZ` synthesises an alias from the address for anything that advertised
/// no name, so an alias is only a name when it is not that. Repeating the
/// address in the name column tells the user nothing the address line below
/// does not already say; a truncated address does at least tell two unnamed
/// devices apart.
fn candidate_name(candidate: &pairing::Candidate) -> String {
    let address = candidate.address.to_string();
    candidate
        .alias
        .as_deref()
        .filter(|alias| !alias.eq_ignore_ascii_case(&address.replace(':', "-")))
        .map_or_else(
            || {
                format!(
                    "Unknown \u{b7} {}\u{2026}",
                    &address[..8.min(address.len())]
                )
            },
            str::to_string,
        )
}

/// Runs one scan window with the screen counting what it finds.
///
/// Esc finishes the scan rather than cancelling it: everything already found
/// is kept and offered, which is why the key legend here says so.
fn run_scan(screen: &Screen) -> (Result<Vec<pairing::Candidate>, String>, bool) {
    let adapter = configured_adapter();
    let found = Arc::new(AtomicUsize::new(0));
    let worker_found = Arc::clone(&found);
    let started = Instant::now();
    let budget = Duration::from_secs(SCAN_SECS);
    let mut painted: Option<Frame> = None;

    run_cancellable(
        move |cancel| pairing::discover(adapter.as_deref(), SCAN_SECS, cancel, &worker_found),
        || {
            let mut frame = screen.frame();
            frame.title(FINDER_TITLE, Some("Step 1 of 2"));
            frame.blank();
            frame.line("Keep the device awake and close to the computer.");
            frame.blank();
            frame.mark(
                Mark::Active,
                "Scanning for nearby Bluetooth devices\u{2026}",
            );
            frame.blank();
            let count = found.load(Ordering::Relaxed);
            frame.line(format!(
                "Found: {count} device{}",
                if count == 1 { "" } else { "s" }
            ));
            frame.line(format!(
                "Time remaining: {}",
                ui::seconds(budget.saturating_sub(started.elapsed()))
            ));
            frame.blank();
            frame.dim(ui::NAV_FINISH_SCAN);
            if painted.as_ref() != Some(&frame) {
                let _ = screen.draw(&frame);
                painted = Some(frame);
            }
        },
    )
}

/// Everything the proximity success screen states, without the menu under it.
fn proximity_success_frame(
    screen: &Screen,
    id: &str,
    name: &str,
    rssi: i16,
    reloaded: &Result<(), String>,
) -> Frame {
    let mut frame = screen.frame();
    frame.title("Proximity device added", None);
    frame.blank();
    frame.mark(Mark::Done, &format!("{name} was added"));
    match reloaded {
        Ok(()) => frame.mark(Mark::Done, "Unlock service reloaded"),
        Err(error) => frame.mark(Mark::Failed, error),
    }
    frame.blank();
    frame.line("Device");
    frame.field("ID", id);
    frame.field("Signal", &dbm(rssi));
    frame.field("Mode", "Proximity only");
    frame.blank();
    frame.line("This device does not report whether it is itself unlocked.");
    frame
}

/// Confirms what was added and offers the one adjustment that matters for a
/// proximity-only device.
fn proximity_success(
    screen: &Screen,
    id: &str,
    name: &str,
    rssi: i16,
    reloaded: &Result<(), String>,
) -> Action {
    loop {
        let head = proximity_success_frame(screen, id, name, rssi, reloaded);
        let choice = Menu::new(head, vec!["Done".into(), "Adjust sensitivity".into()])
            .footer(ui::NAV_SELECT)
            .run(screen)?;
        match choice {
            Some(1) => adjust_sensitivity(screen, id, name, rssi)?,
            _ => return Ok(false),
        }
    }
}

/// How near a device has to be before it counts as present. Named for the
/// distance they describe rather than the number, which is what the user is
/// actually choosing between.
const SENSITIVITY: [(&str, i16); 4] = [
    ("Very close \u{2014} arm's length", -55),
    ("Nearby \u{2014} same desk", -65),
    ("Default \u{2014} same small room", -75),
    ("Generous \u{2014} anywhere in range", -85),
];

/// Radio labels for a pick-one menu. The filled dot marks the value the
/// config currently holds, so the menu shows the live setting instead of
/// only a cursor position.
fn radios(labels: impl IntoIterator<Item = String>, current: Option<usize>) -> Vec<String> {
    labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| {
            let mark = if Some(index) == current {
                '\u{25cf}'
            } else {
                '\u{25cb}'
            };
            format!("({mark}) {label}")
        })
        .collect()
}

/// Lines up a plain item, such as `Back`, with the radio labels beside it:
/// the radio column is exactly four columns wide.
fn aligned(label: &str) -> String {
    format!("    {label}")
}

/// Which preset the configured threshold corresponds to, if any.
fn current_sensitivity(id: &str) -> Option<usize> {
    let threshold = ConfigFile::load()
        .ok()?
        .devices
        .into_iter()
        .find(|device| device.id == id)?
        .threshold_dbm?;
    SENSITIVITY
        .iter()
        .position(|(_, preset)| *preset == threshold)
}

fn adjust_sensitivity(screen: &Screen, id: &str, name: &str, rssi: i16) -> Result<(), String> {
    let mut warning: Option<String> = None;
    loop {
        let current = current_sensitivity(id);
        let mut head = screen.frame();
        head.title("Adjust sensitivity", None);
        head.blank();
        head.line(format!("Unlock while {name} is at least this close."));
        head.line(format!("It measured {} during the scan.", dbm(rssi)));
        if let Some(warning) = &warning {
            head.blank();
            head.warn(warning);
        }

        let mut items = radios(
            SENSITIVITY
                .iter()
                .map(|(label, dbm_value)| format!("{label:<32} {}", dbm(*dbm_value))),
            current,
        );
        items.push(aligned("Back"));
        let back = items.len() - 1;

        let Some(choice) = Menu::new(head, items)
            .footer(ui::NAV_BACK)
            .selected(current.unwrap_or(back))
            .run(screen)?
        else {
            return Ok(());
        };
        if choice == back {
            return Ok(());
        }
        devices::set_threshold(id, SENSITIVITY[choice].1)?;
        warning = reload_daemon().err();
    }
}

/// Proximity-only devices assert nothing about their own lock state, so an
/// address is all that is needed — and the address comes from a scan run
/// right here rather than from a second terminal.
///
/// The daemon holds the adapter in a continuous scan, so it is stopped for
/// the window and restarted as soon as the results are in, well before the
/// user has finished picking.
fn find_proximity_device(screen: &Screen) -> Action {
    loop {
        let pause = DaemonPause::stop();
        let (found, _finished_early) = run_scan(screen);
        // The picker is not something to leave a user staring at after they
        // asked to quit.
        if interrupt::quit_requested() {
            return Ok(false);
        }
        let daemon = pause.resume();
        let candidates = found?;

        let mut items: Vec<String> = candidates
            .iter()
            .map(|candidate| {
                format!(
                    "{:<24} {:>9}   {}",
                    candidate_name(candidate),
                    dbm(candidate.rssi),
                    signal_quality(candidate.rssi)
                )
            })
            .collect();
        let scan_again = items.len();
        items.push("Scan again".into());
        items.push("Back".into());
        let back = items.len() - 1;

        let mut head = screen.frame();
        head.title(FINDER_TITLE, Some("Step 2 of 2"));
        head.blank();
        if candidates.is_empty() {
            head.line("Nothing was advertising nearby.");
        } else {
            head.line("Choose a nearby device:");
        }
        if let Err(error) = &daemon {
            head.blank();
            head.warn(error);
        }

        // Shows the full address only for whatever is highlighted: the list
        // stays readable, and the one address that matters is still visible.
        let detail = |index: usize, frame: &mut Frame| {
            let Some(candidate) = candidates.get(index) else {
                return;
            };
            frame.line(format!("Selected: {}", candidate_name(candidate)));
            frame.line(format!("Address: {}", candidate.address));
            frame.blank();
            frame.dim("Note: this device will provide proximity detection only.");
        };

        let Some(choice) = Menu::new(head, items).detail(&detail).run(screen)? else {
            return Ok(false);
        };
        if choice == back {
            return Ok(false);
        }
        if choice == scan_again {
            continue;
        }

        let candidate = &candidates[choice];
        let name = candidate_name(candidate);
        let address = candidate.address.to_string();
        let id = device_id(candidate.alias.as_deref(), Some(&address));
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
        return proximity_success(screen, &id, &name, candidate.rssi, &reload_daemon());
    }
}

// ---------------------------------------------------------------------------
// Manual identity key
// ---------------------------------------------------------------------------

/// The escape hatch for a key obtained elsewhere — from macOS, or from an
/// earlier `bond-info` — with no pairing involved.
fn enroll_manual_irk(screen: &Screen) -> Action {
    let id = device_id(Some("watch"), None);
    let mut warning: Option<String> = None;
    loop {
        let mut head = screen.frame();
        head.title("Enter an IRK manually", None);
        head.blank();
        head.line("Paste the base64 Identity Resolving Key for the device.");
        head.line("It is masked as you type, because it is key material.");
        if let Some(warning) = &warning {
            head.blank();
            head.warn(warning);
        }

        let Some(typed) = ui::input(screen, &head, "IRK: ", true)? else {
            return Ok(false);
        };
        let irk = typed.trim().to_string();
        if irk.is_empty() {
            warning = Some("An identity key is required.".to_string());
            continue;
        }
        match devices::add(
            &id,
            "apple-continuity",
            &devices::Criteria {
                irk_base64: Some(irk),
                ..devices::Criteria::default()
            },
            &devices::Overrides {
                threshold_dbm: None,
                minimum_samples: None,
                freshness_ms: None,
            },
        ) {
            Ok(()) => {}
            Err(error) => {
                warning = Some(ui::sentence(&error));
                continue;
            }
        }
        let reloaded = reload_daemon();

        let mut head = screen.frame();
        head.title("Device enrolled", None);
        head.blank();
        head.mark(Mark::Done, "Identity key stored");
        match &reloaded {
            Ok(()) => head.mark(Mark::Done, "Unlock service reloaded"),
            Err(error) => head.mark(Mark::Failed, error),
        }
        head.blank();
        head.line("Device");
        head.field("ID", &id);
        head.field("Security", "Apple Continuity");
        head.field("Unlock", unlock_state());
        head.blank();
        head.line("Nothing was paired: this key was taken at your word.");

        loop {
            let choice = Menu::new(head.clone(), vec!["Done".into(), "View diagnostics".into()])
                .footer(ui::NAV_SELECT)
                .run(screen)?;
            match choice {
                Some(1) => diagnostics(screen)?,
                _ => return Ok(false),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

/// The enrollment menu. Names what each route enrolls, never how it works:
/// the distinction that matters to a user is that a Watch really pairs and
/// anything else is only located.
fn enroll_menu(screen: &Screen) -> Action {
    let mut selected = 0;
    loop {
        let mut items: Vec<String> = enrollment::PROVIDERS
            .iter()
            .map(|provider| provider.label().to_string())
            .collect();
        let other = items.len();
        items.push("Other Bluetooth device".into());
        items.push("Enter an IRK manually".into());
        items.push("Back".into());
        let back = items.len() - 1;

        let mut head = screen.frame();
        head.title(ui::APP_TITLE, None);
        head.blank();
        head.line("Enroll a device");

        let Some(choice) = Menu::new(head, items).selected(selected).run(screen)? else {
            return Ok(false);
        };
        if choice == back {
            return Ok(false);
        }
        selected = choice;
        let outcome = if choice < enrollment::PROVIDERS.len() {
            enroll_guided(screen, enrollment::PROVIDERS[choice])
        } else if choice == other {
            find_proximity_device(screen)
        } else {
            enroll_manual_irk(screen)
        };
        if interrupt::quit_requested() {
            return Ok(false);
        }
        // Every flow ends on a screen it drew itself, so anything left here is
        // an error that never got that far.
        if let Err(error) = outcome {
            problem(screen, "Enrollment failed", &error)?;
        }
    }
}

/// Lists enrolled devices and lets one be removed. Esc backs out at either
/// level without changing anything. The refreshed list after a removal is its
/// own confirmation, so nothing is printed for it.
fn manage_devices(screen: &Screen) -> Action {
    let mut warning: Option<String> = None;
    loop {
        let devices = enrolled_devices();
        let mut head = screen.frame();
        head.title(ui::APP_TITLE, None);
        head.blank();
        head.line("Manage enrolled devices");
        if devices.is_empty() {
            head.blank();
            head.line("Nothing is enrolled yet.");
            show(screen, head)?;
            return Ok(false);
        }
        if let Some(warning) = &warning {
            head.blank();
            head.warn(warning);
        }

        let mut items: Vec<String> = devices
            .iter()
            .map(|(id, profile)| format!("{id:<24} {profile}"))
            .collect();
        items.push("Back".into());
        let back = items.len() - 1;

        let Some(choice) = Menu::new(head, items).selected(back).run(screen)? else {
            return Ok(false);
        };
        if choice == back {
            return Ok(false);
        }

        let (id, _) = &devices[choice];
        let mut head = screen.frame();
        head.title(&format!("Remove {id}?"), None);
        head.blank();
        head.line("The device stops unlocking this computer immediately.");
        let confirm = Menu::new(head, vec!["Keep it".into(), "Remove it".into()])
            .footer(ui::NAV_BACK)
            .run(screen)?;
        if confirm == Some(1) {
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
fn choose_backend(screen: &Screen) -> Action {
    const OPTIONS: [&str; 4] = [
        "Omarchy hold Alt for 400ms (recommended)",
        "Signal another lock screen process",
        "Run a custom unlock command",
        "Disable",
    ];
    let mut warning: Option<String> = None;
    loop {
        let current = current_backend();
        let mut head = screen.frame();
        head.title(ui::APP_TITLE, None);
        head.blank();
        head.line("Choose what releases the lock screen");
        if let Some(warning) = &warning {
            head.blank();
            head.warn(warning);
        }

        let mut items = radios(OPTIONS.iter().map(|option| (*option).to_string()), current);
        items.push(aligned("Back"));
        let back = items.len() - 1;

        let Some(choice) = Menu::new(head.clone(), items)
            .selected(current.unwrap_or(0))
            .run(screen)?
        else {
            return Ok(false);
        };
        if choice == back {
            return Ok(false);
        }
        match choice {
            0 => devices::set_backend("quattro", None, None, &[])?,
            1 => {
                let Some(process) = ui::input(
                    screen,
                    &head,
                    "Process name (matched against /proc/<pid>/comm): ",
                    false,
                )?
                else {
                    continue;
                };
                devices::set_backend("process-signal", Some(process.trim()), None, &[])?;
            }
            2 => {
                let Some(command) = ui::input(
                    screen,
                    &head,
                    "Command, e.g. sh -c 'loginctl unlock-session': ",
                    false,
                )?
                else {
                    continue;
                };
                match shell_words::split(&command) {
                    Ok(argv) if !argv.is_empty() => {
                        devices::set_backend("command", None, None, &argv)?;
                    }
                    Ok(_) => {
                        warning = Some("A command is required.".to_string());
                        continue;
                    }
                    Err(error) => {
                        warning = Some(format!("Could not parse the command: {error}"));
                        continue;
                    }
                }
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
fn choose_quorum(screen: &Screen) -> Action {
    const OPTIONS: [&str; 3] = [
        "Any single enrolled device is enough (default)",
        "Every enrolled device must be present",
        "At least a minimum number must be present",
    ];
    let mut warning: Option<String> = None;
    loop {
        let current = current_quorum();
        let mut head = screen.frame();
        head.title(ui::APP_TITLE, None);
        head.blank();
        head.line("Decide how many enrolled devices must be present");
        if let Some(warning) = &warning {
            head.blank();
            head.warn(warning);
        }

        let mut items = radios(OPTIONS.iter().map(|option| (*option).to_string()), current);
        items.push(aligned("Back"));
        let back = items.len() - 1;

        let Some(choice) = Menu::new(head.clone(), items)
            .selected(current.unwrap_or(0))
            .run(screen)?
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
                let Some(count) = ui::input(screen, &head, "Minimum device count: ", false)? else {
                    continue;
                };
                format!("at-least:{}", count.trim())
            }
            _ => unreachable!("index {choice} is past the option list"),
        };
        if let Err(error) = devices::set_quorum(&expression) {
            warning = Some(ui::sentence(&error));
            continue;
        }
        warning = reload_daemon().err();
    }
}

/// Runs `doctor` and shows exactly what it printed. Its output is arbitrary
/// length, so it goes below a painted header rather than into a frame.
fn diagnostics(screen: &Screen) -> Result<(), String> {
    let mut head = screen.frame();
    head.title("Diagnostics", None);
    head.blank();
    screen.draw_above_output(&head)?;
    if let Err(error) = doctor::doctor() {
        println!("{}", console::style(ui::sentence(&error)).yellow());
    }
    println!("\n{}", console::style(ui::NAV_RETURN).dim());
    ui::wait_for_dismiss(screen)
}

/// Same shape as [`diagnostics`]: a header, then whatever the installer says.
fn install_integration(screen: &Screen) -> Action {
    let mut head = screen.frame();
    head.title("Install lock-screen integration", None);
    head.blank();
    screen.draw_above_output(&head)?;
    let result = setup::setup_omarchy();
    if let Err(error) = &result {
        println!("{}", console::style(ui::sentence(error)).yellow());
    }
    println!("\n{}", console::style(ui::NAV_RETURN).dim());
    ui::wait_for_dismiss(screen)?;
    // Already reported on screen; the menu has nothing left to say about it.
    Ok(false)
}

/// Refreshes the daemon's per-device and aggregate decision once a second
/// until any key is pressed. The read runs on its own thread so the refresh
/// loop can poll it with a timeout instead of blocking on stdin.
fn live_status(screen: &Screen) -> Action {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Err(error) = console::Term::stdout().read_key() {
            interrupt::exit_if_interrupted(&error);
        }
        let _ = tx.send(());
    });

    loop {
        let mut frame = screen.frame();
        frame.title("Live status", None);
        frame.blank();
        match client::request_lines(wire::REQ_STATUS, Duration::from_millis(200)) {
            Ok(lines) if lines.is_empty() => frame.line("(no response)"),
            Ok(lines) => {
                for line in lines {
                    frame.line(line);
                }
            }
            Err(error) => frame.warn(&ui::sentence(&error)),
        }
        frame.blank();
        frame.dim("Press any key to return");
        screen.draw(&frame)?;
        if rx.recv_timeout(Duration::from_secs(1)).is_ok() {
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
    // Registered first: the handler must exist before the buffer switch it
    // is responsible for undoing.
    interrupt::install(restore_terminal);
    let _screen = AltScreen::enter()?;
    let screen = Screen::new();

    let exit = MAIN_MENU.len() - 1;
    let mut selected = 0;
    loop {
        let mut head = screen.frame();
        head.title(ui::APP_TITLE, None);
        head.blank();
        head.line("BLE proximity unlock for this computer");

        let Some(choice) = Menu::new(head, MAIN_MENU.iter().map(|&s| s.to_string()).collect())
            .footer(ui::NAV_EXIT)
            .selected(selected)
            .run(&screen)?
        else {
            return Ok(());
        };
        if choice == exit {
            return Ok(());
        }
        selected = choice;

        let result = match choice {
            0 => enroll_menu(&screen),
            1 => manage_devices(&screen),
            2 => choose_backend(&screen),
            3 => choose_quorum(&screen),
            4 => install_integration(&screen),
            5 => diagnostics(&screen).map(|()| false),
            _ => live_status(&screen),
        };
        // Ctrl+C during the action asked to leave, and the action has now
        // unwound. Returning rather than exiting is the point: `AltScreen`
        // and any suspended daemon are put back by their own `Drop`.
        if interrupt::quit_requested() {
            return Ok(());
        }
        if let Err(error) = result {
            problem(&screen, "Something went wrong", &error)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_quality_reads_the_bands_the_picker_shows() {
        assert_eq!(signal_quality(-42), "Excellent");
        assert_eq!(signal_quality(-55), "Excellent");
        assert_eq!(signal_quality(-56), "Good");
        assert_eq!(signal_quality(-70), "Good");
        assert_eq!(signal_quality(-71), "Weak");
        assert_eq!(signal_quality(-79), "Weak");
    }

    #[test]
    fn signal_strength_is_written_with_a_typographic_minus() {
        assert_eq!(dbm(-42), "\u{2212}42 dBm");
        assert_eq!(dbm(-100), "\u{2212}100 dBm");
    }

    #[test]
    fn advice_names_the_step_that_failed_rather_than_the_first_one() {
        let (headline, tips) = failure_advice("Apple Watch", "Open Health Devices.", 4);
        assert_eq!(
            headline,
            "The Apple Watch connected, but no device identity was received."
        );
        assert!(tips.iter().any(|tip| tip.contains("Bluetooth devices")));

        let (headline, tips) = failure_advice("Apple Watch", "Open Health Devices.", 2);
        assert_eq!(headline, "The Apple Watch never connected.");
        assert_eq!(tips[0], "Open Health Devices.");

        // Every stage before the device is involved blames a different thing,
        // because a user can only act on the one that actually failed.
        let headline = |rank| failure_advice("Apple Watch", "Open Health Devices.", rank).0;
        assert_eq!(headline(-1), "The Bluetooth adapter could not be opened.");
        assert_eq!(
            headline(0),
            "The secure pairing monitor could not be started."
        );
        assert_eq!(headline(1), "This computer could not be made discoverable.");
        assert_eq!(
            headline(3),
            "The Apple Watch connected, but secure pairing did not complete."
        );
        assert_eq!(headline(5), "The device identity could not be verified.");
    }

    #[test]
    fn a_pair_state_ranks_by_the_last_phase_it_was_told_about() {
        let mut state = PairState::default();
        assert_eq!(state.rank(), -1);
        state.apply(Progress::Phase(Phase::AdapterReady));
        assert_eq!(state.rank(), 0);
        state.apply(Progress::Phase(Phase::Advertising("kelvin".into())));
        assert_eq!(state.advertising_as.as_deref(), Some("kelvin"));
        state.apply(Progress::Phase(Phase::Connected(Some(
            "Apple Watch".into(),
        ))));
        assert_eq!(state.device_name.as_deref(), Some("Apple Watch"));
        assert_eq!(state.rank(), 3);
        state.apply(Progress::Cleanup(Cleanup {
            label: "Adapter settings restored",
            ok: true,
        }));
        // Cleanup is recorded without pretending the flow moved on.
        assert_eq!(state.rank(), 3);
        assert_eq!(state.cleanup.len(), 1);
    }

    #[test]
    fn an_unnamed_device_is_identified_by_the_head_of_its_address() {
        let candidate = pairing::Candidate {
            address: bluer::Address::new([0x7c, 0x91, 0x22, 0x0d, 0xfb, 0xaa]),
            alias: None,
            rssi: -79,
            paired: false,
        };
        assert_eq!(
            candidate_name(&candidate),
            "Unknown \u{b7} 7C:91:22\u{2026}"
        );

        let named = pairing::Candidate {
            alias: Some("Pixel 10 Pro".into()),
            ..candidate
        };
        assert_eq!(candidate_name(&named), "Pixel 10 Pro");

        // What BlueZ hands back for a device that advertised no name at all.
        let placeholder = pairing::Candidate {
            alias: Some("7C-91-22-0D-FB-AA".into()),
            ..candidate
        };
        assert_eq!(
            candidate_name(&placeholder),
            "Unknown \u{b7} 7C:91:22\u{2026}"
        );
    }

    #[test]
    fn ids_are_derived_from_a_name_and_are_config_safe() {
        assert_eq!(slug("Pixel 10 Pro").as_deref(), Some("pixel-10-pro"));
        assert_eq!(
            slug("Mi Smart Band 8!!").as_deref(),
            Some("mi-smart-band-8")
        );
        assert_eq!(slug("---").as_deref(), None);
        assert_eq!(slug("").as_deref(), None);
    }

    /// Everything below the title and above the key legend, which is the part
    /// a mockup pins down.
    fn body(frame: &Frame) -> Vec<String> {
        let lines = frame.plain();
        lines[1..lines.len() - 2]
            .iter()
            .filter(|line| !line.is_empty())
            .cloned()
            .collect()
    }

    fn at_phase(phases: &[Phase]) -> PairState {
        let mut state = PairState::default();
        for phase in phases {
            state.apply(Progress::Phase(phase.clone()));
        }
        state
    }

    /// The three states of the live pairing screen, exactly as specified: the
    /// advertising checklist collapses into "connected" the moment the Watch
    /// arrives, and each remaining row switches from future to past tense as
    /// it completes.
    #[test]
    fn the_live_pairing_checklist_advances_row_by_row() {
        let screen = Screen::new();
        let provider = enrollment::PROVIDERS[0];
        let render = |state: &PairState| {
            body(&pair_frame(
                &screen,
                provider,
                state,
                "mirceone-framework",
                Duration::from_secs(277),
            ))
        };

        let waiting = at_phase(&[
            Phase::AdapterReady,
            Phase::MonitorReady,
            Phase::Advertising("mirceone-framework".into()),
        ]);
        assert_eq!(
            render(&waiting),
            [
                "  ✓ Bluetooth adapter ready",
                "  ✓ Secure pairing monitor ready",
                "  ✓ Advertising as “mirceone-framework”",
                "  ◉ Waiting for Apple Watch…",
                "Open Settings → Bluetooth → Health Devices on the Watch.",
                "Time remaining: 04:37",
            ]
        );

        let mut connected = waiting;
        connected.apply(Progress::Phase(Phase::Connected(None)));
        assert_eq!(
            render(&connected),
            [
                "  ✓ Bluetooth adapter ready",
                "  ✓ Secure pairing monitor ready",
                "  ✓ Apple Watch connected",
                "  ◉ Completing secure pairing…",
                "  ○ Receiving device identity",
                "  ○ Verifying enrollment",
            ]
        );

        let mut verifying = connected;
        verifying.apply(Progress::Phase(Phase::Bonded));
        verifying.apply(Progress::Phase(Phase::IdentityReceived));
        assert_eq!(
            render(&verifying),
            [
                "  ✓ Bluetooth adapter ready",
                "  ✓ Secure pairing monitor ready",
                "  ✓ Apple Watch connected",
                "  ✓ Secure pairing completed",
                "  ✓ Device identity received",
                "  ◉ Verifying enrollment…",
            ]
        );
    }

    /// Before anything is reported the adapter row is the one in flight, not a
    /// finished one: a screen that opens claiming work already done would be
    /// lying for as long as `BlueZ` takes to answer.
    #[test]
    fn nothing_is_marked_done_before_it_is_reported() {
        let screen = Screen::new();
        assert_eq!(
            body(&pair_frame(
                &screen,
                enrollment::PROVIDERS[0],
                &PairState::default(),
                "mirceone-framework",
                Duration::from_secs(299),
            ))[..2],
            [
                "  ◉ Bluetooth adapter ready",
                "  ○ Secure pairing monitor ready",
            ]
        );
    }

    /// The picker's columns are what make a list of radios comparable at a
    /// glance, so they are pinned rather than left to drift.
    #[test]
    fn picker_rows_line_their_columns_up() {
        let row = |alias: Option<&str>, rssi: i16| {
            let candidate = pairing::Candidate {
                address: bluer::Address::new([0x74, 0x21, 0x9c, 0x8b, 0x12, 0xaf]),
                alias: alias.map(str::to_string),
                rssi,
                paired: false,
            };
            format!(
                "{:<24} {:>9}   {}",
                candidate_name(&candidate),
                dbm(candidate.rssi),
                signal_quality(candidate.rssi)
            )
        };
        assert_eq!(
            row(Some("Pixel 10 Pro"), -42),
            "Pixel 10 Pro               −42 dBm   Excellent"
        );
        assert_eq!(
            row(Some("Galaxy Buds"), -71),
            "Galaxy Buds                −71 dBm   Weak"
        );
        assert_eq!(row(None, -79), "Unknown · 74:21:9C…        −79 dBm   Weak");
    }

    /// A cancelled attempt has to account for the state it left the machine
    /// in, and may only claim the cleanup that was actually reported.
    #[test]
    fn cancelling_reports_only_the_cleanup_that_happened() {
        let screen = Screen::new();
        let mut state = PairState::default();
        state.apply(Progress::Cleanup(Cleanup {
            label: "Temporary Bluetooth device removed",
            ok: true,
        }));
        state.apply(Progress::Cleanup(Cleanup {
            label: "Adapter settings restored",
            ok: true,
        }));
        assert_eq!(
            pair_cancelled_frame(&screen, &state, &Ok(())).plain(),
            [
                "Pairing cancelled",
                "",
                "No device was enrolled.",
                "Temporary Bluetooth device removed.",
                "Adapter settings restored.",
                "The unlock service is running again.",
                "",
                "Returning to device enrollment…",
            ]
        );

        // A cleanup that failed, and a daemon that did not come back, must
        // both survive onto the screen rather than being smoothed over.
        let mut broken = PairState::default();
        broken.apply(Progress::Cleanup(Cleanup {
            label: "Adapter settings restored",
            ok: false,
        }));
        let lines =
            pair_cancelled_frame(&screen, &broken, &Err("unlockd did not restart".into())).plain();
        assert_eq!(lines[3], "Adapter settings restored could not be undone.");
        assert_eq!(lines[4], "unlockd did not restart");
    }

    /// The success screen is the only record of what was enrolled, so its
    /// fields are pinned to the device that was actually seen.
    #[test]
    fn the_success_screen_describes_the_device_that_was_enrolled() {
        let screen = Screen::new();
        let state = at_phase(&[Phase::Connected(Some("Apple Watch".into()))]);
        let lines =
            pair_success_frame(&screen, enrollment::PROVIDERS[0], "watch", &state, &Ok(())).plain();
        assert!(lines[0].starts_with("Apple Watch enrolled"));
        assert!(lines[0].ends_with("Step 3 of 3"));
        assert_eq!(
            lines[2..7],
            [
                "  ✓ Pairing completed",
                "  ✓ Device identity verified",
                "  ✓ Unlock service resumed",
                "",
                "Device",
            ]
        );
        assert_eq!(lines[7], "  Name       Apple Watch");
        assert_eq!(lines[8], "  ID         watch");
        assert_eq!(lines[9], "  Security   Apple Continuity");
    }

    /// A proximity device asserts nothing about its own lock state, and the
    /// screen that confirms it has to say so.
    #[test]
    fn the_proximity_screen_does_not_overclaim_what_was_added() {
        let screen = Screen::new();
        assert_eq!(
            proximity_success_frame(&screen, "pixel-10-pro", "Pixel 10 Pro", -42, &Ok(())).plain(),
            [
                "Proximity device added",
                "",
                "  ✓ Pixel 10 Pro was added",
                "  ✓ Unlock service reloaded",
                "",
                "Device",
                "  ID         pixel-10-pro",
                "  Signal     −42 dBm",
                "  Mode       Proximity only",
                "",
                "This device does not report whether it is itself unlocked.",
            ]
        );
    }
}
