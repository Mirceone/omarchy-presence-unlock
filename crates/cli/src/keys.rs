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

/// Esc only cancels on its own: arrow, function and navigation keys arrive as
/// `ESC [ …` (CSI) or `ESC O …` (SS3), so cancelling on the leading byte would
/// abort the operation on an idle keypress. A chunk ending at the Esc is a real
/// Esc press, which terminals deliver as that single byte.
fn is_cancel_chunk(chunk: &[u8]) -> bool {
    chunk.iter().enumerate().any(|(i, &byte)| match byte {
        CTRL_C | b'q' | b'Q' => true,
        ESC => !matches!(chunk.get(i + 1), Some(b'[' | b'O')),
        _ => false,
    })
}

/// True when a cancel key (Esc, `q`, or Ctrl+C) is pressed within `timeout`.
/// False when the timeout expires with no such key.
#[must_use]
pub fn cancel_key_pressed(timeout: Duration) -> bool {
    let stdin = stdin();
    if !stdin.is_terminal() {
        return false;
    }
    let fd = stdin.as_fd();
    let Some(_raw) = RawMode::enter(fd) else {
        return false;
    };

    let start = Instant::now();
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
        let wait = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
        match poll(&mut fds, wait) {
            Ok(0) => return false,
            Ok(_) => {}
            // Some other signal cut the wait short; `remaining` still bounds
            // the retry, so this cannot outlive the caller's timeout.
            Err(Errno::EINTR) => continue,
            Err(_) => return false,
        }

        let mut buf = [0u8; 32];
        // Zero bytes on a readable descriptor means hangup, not a slow typist.
        let Ok(count @ 1..) = read(fd.as_raw_fd(), &mut buf) else {
            return false;
        };
        if is_cancel_chunk(&buf[..count]) {
            return true;
        }
    }
}
