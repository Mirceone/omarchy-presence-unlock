//! Cancel-key polling for long menu operations.
//!
//! `console::Term::read_key` blocks forever, so a background reader would still
//! be holding stdin when the operation finishes and would swallow the next
//! menu selection. Everything here is bounded by the caller's timeout and
//! touches stdin only while that call is on the stack.

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::termios::{
    SetArg, SpecialCharacterIndices, Termios, cfmakeraw, tcgetattr, tcsetattr,
};
use nix::unistd::read;
use std::io::{IsTerminal, stdin};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::time::{Duration, Instant};

const ESC: u8 = 0x1b;
const CTRL_C: u8 = 0x03;

/// Reinstates the entry-time terminal settings on drop, which covers every
/// early return and any panic between here and the end of the wait.
struct RawMode<'fd> {
    fd: BorrowedFd<'fd>,
    saved: Termios,
}

impl<'fd> RawMode<'fd> {
    fn enter(fd: BorrowedFd<'fd>) -> Option<Self> {
        let saved = tcgetattr(fd).ok()?;
        let mut raw = saved.clone();
        // Canonical mode withholds Esc until Enter, and ISIG would turn Ctrl+C
        // into a signal for the wizard's handler instead of a readable byte.
        cfmakeraw(&mut raw);
        // Only the input side needs to be raw: cfmakeraw also clears OPOST
        // (and with it ONLCR), and this guard is held while another thread
        // prints, so every newline would lose its carriage return and
        // staircase the output.
        raw.output_flags = saved.output_flags;
        // cfmakeraw asks for one byte minimum, which would let read() block
        // past the deadline. Zero makes it take only what poll announced.
        raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
        raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
        tcsetattr(fd, SetArg::TCSANOW, &raw).ok()?;
        Some(Self { fd, saved })
    }
}

impl Drop for RawMode<'_> {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, SetArg::TCSANOW, &self.saved);
    }
}

/// How long a trailing `ESC` is held back, waiting to see whether it is the
/// start of a sequence. Terminals emit the bytes of a CSI or SS3 sequence
/// back to back, so a tail that has not arrived within this window is not
/// coming. Long enough to survive a link that splits the write, short enough
/// that a deliberate Esc still ends the wait promptly.
const ESC_TAIL: Duration = Duration::from_millis(50);

/// What a run of input bytes asks for.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Esc: stop the operation.
    Cancel,
    /// Ctrl+C: stop the operation, then leave the app.
    Quit,
    /// Nothing here means anything; discard it and keep waiting.
    Idle,
    /// The bytes end in an `ESC` with nothing after it yet, which is either a
    /// real Esc press or the head of a sequence still in flight.
    Undecided,
}

/// Esc only cancels on its own: arrow, function and navigation keys arrive as
/// `ESC [ …` (CSI) or `ESC O …` (SS3), so cancelling on the leading byte would
/// abort the operation on an idle keypress.
///
/// A `read` is not guaranteed to deliver a whole sequence — an arrow key can
/// land as `ESC` now and `[A` a moment later — so a trailing `ESC` cannot be
/// judged from the bytes in hand at all, and says so rather than guessing.
///
/// `q` is deliberately not a cancel key. Every prompt advertises Esc, and the
/// sudo password prompt shares this terminal, so a `q` in a passphrase used
/// to abort the enrollment behind it.
fn verdict(chunk: &[u8]) -> Verdict {
    for (i, &byte) in chunk.iter().enumerate() {
        match byte {
            CTRL_C => return Verdict::Quit,
            ESC => match chunk.get(i + 1) {
                Some(b'[' | b'O') => {}
                Some(_) => return Verdict::Cancel,
                None => return Verdict::Undecided,
            },
            _ => {}
        }
    }
    Verdict::Idle
}

/// What the user asked for during the wait.
///
/// `cfmakeraw` clears `ISIG`, so while this is polling, Ctrl+C is delivered as
/// a plain byte instead of a signal. That makes this the *other* half of the
/// app's interrupt handling, and the reason [`Press::Quit`] exists: without
/// it, the one key that means "leave" would mean "go back" for as long as an
/// operation held the terminal.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Press {
    /// Nothing worth acting on arrived.
    Idle,
    /// Esc: stop this operation and stay in the app.
    Cancel,
    /// Ctrl+C: stop this operation, then leave the app.
    Quit,
}

/// Waits up to `timeout` for a key that means something to a long operation.
#[must_use]
pub fn wait_for_press(timeout: Duration) -> Press {
    let stdin = stdin();
    if !stdin.is_terminal() {
        return Press::Idle;
    }
    let fd = stdin.as_fd();
    let Some(_raw) = RawMode::enter(fd) else {
        return Press::Idle;
    };

    let start = Instant::now();
    // Holds a single held-back `ESC` between reads, and the tail that decides
    // it. Nothing longer is ever carried: every byte before a trailing `ESC`
    // has already been judged.
    let mut buf = [0u8; 32];
    let mut held = 0usize;
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        // A held-back Esc gets the shorter of its own window and whatever is
        // left of the caller's budget, so this still cannot overrun.
        let wait = if held == 0 {
            remaining
        } else {
            remaining.min(ESC_TAIL)
        };
        let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
        let wait = PollTimeout::try_from(wait).unwrap_or(PollTimeout::MAX);
        match poll(&mut fds, wait) {
            // No tail followed, so the Esc was pressed on its own.
            Ok(0) if held > 0 => return Press::Cancel,
            Ok(0) => return Press::Idle,
            Ok(_) => {}
            // Some other signal cut the wait short; `remaining` still bounds
            // the retry, so this cannot outlive the caller's timeout.
            Err(Errno::EINTR) => continue,
            Err(_) => return Press::Idle,
        }

        // Zero bytes on a readable descriptor means hangup, not a slow typist.
        let Ok(count @ 1..) = read(fd.as_raw_fd(), &mut buf[held..]) else {
            return Press::Idle;
        };
        match verdict(&buf[..held + count]) {
            Verdict::Cancel => return Press::Cancel,
            Verdict::Quit => return Press::Quit,
            Verdict::Idle => held = 0,
            Verdict::Undecided => {
                buf[0] = ESC;
                held = 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Verdict, verdict};

    #[test]
    fn a_lone_escape_is_undecided_until_its_tail_arrives() {
        assert_eq!(verdict(b"\x1b"), Verdict::Undecided);
        assert_eq!(verdict(b"\x1b["), Verdict::Idle);
    }

    #[test]
    fn an_arrow_key_never_cancels_however_it_is_split() {
        assert_eq!(verdict(b"\x1b[A"), Verdict::Idle);
        assert_eq!(verdict(b"\x1bOP"), Verdict::Idle);
    }

    #[test]
    fn a_standalone_escape_press_cancels() {
        assert_eq!(verdict(b"\x1bx"), Verdict::Cancel);
    }

    #[test]
    fn ctrl_c_quits_rather_than_cancelling() {
        assert_eq!(verdict(b"\x03"), Verdict::Quit);
    }

    #[test]
    fn q_is_an_ordinary_key() {
        assert_eq!(verdict(b"q"), Verdict::Idle);
        assert_eq!(verdict(b"Q"), Verdict::Idle);
    }

    #[test]
    fn ordinary_keys_are_ignored() {
        assert_eq!(verdict(b"x"), Verdict::Idle);
        assert_eq!(verdict(b""), Verdict::Idle);
    }
}
