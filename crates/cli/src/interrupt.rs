//! One place that knows the user wants out.
//!
//! Two things can say so and they are not interchangeable. Esc means "stop
//! this operation" and never reaches here — [`crate::keys`] handles it, because
//! a signal cannot express it. Ctrl+C means "leave the app", and reaches here
//! twice over: as SIGINT normally, and as a plain byte while the key poller
//! holds the terminal raw with `ISIG` cleared. Both funnel into
//! [`request_quit`] so the two spellings cannot drift apart.
//!
//! Quitting is a request rather than an exit. An operation in flight owns
//! Bluetooth state — an adapter left pairable, a half-built bond — that only
//! its own unwind puts back, so the request sets a flag the operation polls
//! and the app leaves through ordinary returns. A second Ctrl+C gives up on
//! that and exits, because an operation that will not stop must not be able
//! to trap the user.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use tokio::signal::unix::{SignalKind, signal};

/// Exit status follows the 128 + signal convention, so the numbers are needed
/// as well as the `SignalKind`s.
const SIGINT: i32 = nix::libc::SIGINT;
const SIGTERM: i32 = nix::libc::SIGTERM;

/// Set once the user has asked to leave. Long operations poll it and unwind;
/// doubling as the `cancel` flag the non-interactive CLI hands to enrollment,
/// where quitting and cancelling are the same thing because the operation is
/// the whole process.
pub static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Runs immediately before an exit that skips destructors, for whatever the
/// caller must put back by hand.
static ON_QUIT: OnceLock<fn()> = OnceLock::new();

/// Records that the user wants to leave.
///
/// The first call only sets the flag: whatever is in flight gets to unwind,
/// which is what returns the adapter to its previous state. A second call
/// means that unwind is not happening, so this one does not return.
pub fn request_quit() {
    if QUIT_REQUESTED.swap(true, Ordering::Relaxed) {
        force_quit(SIGINT);
    }
}

/// Whether the user has asked to leave.
#[must_use]
pub fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::Relaxed)
}

fn force_quit(signum: i32) -> ! {
    if let Some(on_quit) = ON_QUIT.get() {
        on_quit();
    }
    std::process::exit(128 + signum);
}

/// Leaves the app when a terminal read was cut short by Ctrl+C.
///
/// `console` answers Ctrl+C at a prompt by re-raising SIGINT *and* reporting
/// the read as interrupted; the handler and this call therefore race, and both
/// must land on the same outcome — run `on_quit`, then exit 128 + SIGINT — or
/// the exit status would depend on which one won.
///
/// Exiting outright is safe only because prompts are where nothing is in
/// flight. An operation that owns adapter state is instead unwound by its own
/// cancellation flag, and never gets here.
pub fn exit_if_interrupted(error: &std::io::Error) {
    if error.kind() == std::io::ErrorKind::Interrupted {
        force_quit(SIGINT);
    }
}

/// Installs the SIGINT and SIGTERM handlers. `on_quit` runs immediately
/// before an exit that skips destructors, for callers that must put the
/// terminal back or restore a service they suspended.
///
/// `console` does not hand Ctrl+C back as a key: it re-raises SIGINT at the
/// process, and the default disposition kills us outright without unwinding.
/// The workspace forbids `unsafe`, which rules out installing a handler
/// directly, but `tokio`'s signal driver is already a dependency and does it
/// in safe code.
///
/// SIGTERM never asks, it only tells: a logout or a `kill` has no interactive
/// user to hand a half-finished operation back to.
///
/// Returns only once the handlers are installed. `signal()` registers with
/// the OS when the stream is built rather than when it is first awaited, so
/// the wait is what closes the startup window in which a fast Ctrl+C would
/// still hit the default disposition and strand the terminal.
///
/// Call at most once per process.
pub fn install(on_quit: fn()) {
    let _ = ON_QUIT.set(on_quit);
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
            let (Ok(mut interrupt), Ok(mut terminate)) = (
                signal(SignalKind::interrupt()),
                signal(SignalKind::terminate()),
            ) else {
                drop(registered);
                return;
            };
            drop(registered);
            // Repeated signals must keep working, so this loops rather than
            // handling one and falling out.
            loop {
                tokio::select! {
                    received = interrupt.recv() => match received {
                        Some(()) => request_quit(),
                        None => break,
                    },
                    received = terminate.recv() => match received {
                        Some(()) => force_quit(SIGTERM),
                        None => break,
                    },
                }
            }
        });
    });
    // Errs as soon as the sender is dropped, which every path above does
    // immediately after registration succeeds or is abandoned.
    let _ = wait.recv();
}
