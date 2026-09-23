//! Worker coordination for screens that keep painting while an operation runs.

use crate::{interrupt, keys};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

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
/// A worker panic is reported through the same error surface as an operation
/// failure, so the screen owner can restore its terminal before showing it.
///
/// Returns the operation's result and whether it was stopped early.
pub(super) fn run_cancellable<T: Send + 'static>(
    work: impl FnOnce(&AtomicBool) -> Result<T, String> + Send + 'static,
    mut repaint: impl FnMut(),
) -> (Result<T, String>, bool) {
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);

    let (done, result) = mpsc::channel();
    thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(|| work(&worker_cancel)))
            .map_err(|_| "cancellable operation panicked".to_string())
            .and_then(|result| result);
        let _ = done.send(outcome);
    });

    let mut outcome = None;
    while outcome.is_none() {
        repaint();
        match keys::wait_for_press(Duration::from_millis(100)) {
            keys::Press::Cancel => cancel.store(true, Ordering::Relaxed),
            // Leaving still unwinds the operation first: it owns adapter
            // state that only its own cleanup puts back.
            keys::Press::Quit => interrupt::request_quit(),
            keys::Press::Idle | keys::Press::Pressed => {}
        }
        // Covers a SIGINT that landed between polls, when the terminal was
        // not raw and Ctrl+C was a signal rather than a byte.
        if interrupt::quit_requested() {
            cancel.store(true, Ordering::Relaxed);
        }
        outcome = match result.try_recv() {
            Ok(value) => Some(value),
            Err(mpsc::TryRecvError::Empty) => None,
            // The thread always sends an outcome, including panics caught
            // above; reaching this branch means the runtime lost the worker.
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err("cancellable operation ended without a result".into()))
            }
        };
    }

    (
        outcome.expect("worker result"),
        cancel.load(Ordering::Relaxed),
    )
}

/// Owns the status screen's reader until the screen has definitely released
/// stdin. Its polling interval bounds `Drop`, so every exit path joins it.
pub(super) struct LiveStatusReader {
    cancel: Arc<AtomicBool>,
    pressed: mpsc::Receiver<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl LiveStatusReader {
    pub(super) fn start() -> Self {
        Self::start_with_poller(|_| match keys::wait_for_press(Duration::from_millis(100)) {
            keys::Press::Quit => {
                interrupt::request_quit();
                true
            }
            keys::Press::Idle => false,
            keys::Press::Pressed | keys::Press::Cancel => true,
        })
    }

    fn start_with_poller(poll: impl Fn(&AtomicBool) -> bool + Send + 'static) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let reader_cancel = Arc::clone(&cancel);
        let (tx, pressed) = mpsc::channel();
        let thread = thread::spawn(move || {
            while !reader_cancel.load(Ordering::Relaxed) {
                if poll(&reader_cancel) {
                    let _ = tx.send(());
                    break;
                }
            }
        });
        Self {
            cancel,
            pressed,
            thread: Some(thread),
        }
    }

    pub(super) fn was_pressed(&self) -> bool {
        self.pressed.try_recv().is_ok()
    }
}

impl Drop for LiveStatusReader {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveStatusReader, run_cancellable};
    use std::{
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn a_worker_panic_is_returned_to_the_screen_owner() {
        let (result, cancelled) = run_cancellable(
            |_| -> Result<(), String> { panic!("worker failure") },
            || {},
        );

        assert_eq!(result.unwrap_err(), "cancellable operation panicked");
        assert!(!cancelled);
    }

    #[test]
    fn status_reader_releases_stdin_when_its_screen_exits_without_a_keypress() {
        let (entered, waiting) = mpsc::channel();
        let reader = LiveStatusReader::start_with_poller(move |_| {
            let _ = entered.send(());
            thread::sleep(Duration::from_millis(100));
            false
        });
        waiting
            .recv_timeout(Duration::from_secs(1))
            .expect("reader must begin waiting for input");
        let started = Instant::now();
        drop(reader);

        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
