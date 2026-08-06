//! Full-screen rendering for the interactive wizard.
//!
//! Every screen is a [`Frame`]: a complete list of lines, rebuilt from scratch
//! whenever anything changes and painted from the top-left corner. Nothing here
//! ever scrolls or appends, so a long operation can update its own checklist in
//! place without the flicker a clear-and-reprint would cause — the paint
//! overwrites each line and clears whatever the previous, possibly longer,
//! frame left below it.
//!
//! Key semantics are uniform and are the reason this module owns input as well
//! as output:
//!
//! * `Enter` selects or continues.
//! * `Esc` cancels or backs out of the current screen only.
//! * `Ctrl+C` leaves the whole wizard, wherever it is pressed.

use crate::interrupt;
use console::{Key, Term, style};
use std::fmt::Write as _;
use std::time::Duration;

/// Shown on the screens that are the wizard itself rather than one flow.
pub const APP_TITLE: &str = "Omarchy Presence Unlock";

const HOME: &str = "\x1b[H";
const CLEAR_LINE: &str = "\x1b[K";
const CLEAR_BELOW: &str = "\x1b[J";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

/// Key hints, spelled exactly as the keys this module binds.
pub const NAV_EXIT: &str = "↑↓ navigate   Enter select   Esc exit";
pub const NAV_BACK: &str = "↑↓ navigate   Enter select   Esc back";
pub const NAV_SELECT: &str = "↑↓ navigate   Enter select";
pub const NAV_CANCEL: &str = "Esc cancel";
pub const NAV_FINISH_SCAN: &str = "Esc finish scan";
pub const NAV_RETURN: &str = "Press Esc to return";
pub const NAV_INPUT: &str = "Enter confirm   Esc cancel";

/// The state of one entry in a progress checklist.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Finished successfully.
    Done,
    /// Happening right now.
    Active,
    /// Not started yet.
    Pending,
    /// Attempted and failed.
    Failed,
}

impl Mark {
    fn render(self, text: &str) -> String {
        match self {
            Self::Done => format!("  {} {text}", style("✓").green()),
            Self::Active => format!("  {} {text}", style("◉").cyan()),
            Self::Pending => format!("  {} {}", style("○").dim(), style(text).dim()),
            Self::Failed => format!("  {} {text}", style("✗").red()),
        }
    }
}

/// One screen, built line by line and painted in a single write.
///
/// Frames compare by value so a caller repainting on a timer can skip the
/// write when nothing moved.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    width: usize,
    lines: Vec<String>,
    /// One-based row and column for the terminal cursor. Absent means the
    /// screen takes no typing and the cursor stays hidden.
    cursor: Option<(usize, usize)>,
}

impl Frame {
    #[must_use]
    fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            cursor: None,
        }
    }

    /// The screen title, with an optional step indicator pushed to the right
    /// margin so a multi-step flow always says where the user is.
    pub fn title(&mut self, title: &str, step: Option<&str>) {
        let mut rendered = style(title).bold().to_string();
        if let Some(step) = step {
            let used = title.chars().count() + step.chars().count();
            let gap = self.width.saturating_sub(used).max(2);
            let _ = write!(rendered, "{:gap$}{}", "", style(step).dim());
        }
        self.lines.push(rendered);
    }

    pub fn blank(&mut self) {
        self.lines.push(String::new());
    }

    pub fn line(&mut self, text: impl Into<String>) {
        self.lines.push(text.into());
    }

    /// Secondary text: hints, notes, and key legends.
    pub fn dim(&mut self, text: &str) {
        self.lines.push(style(text).dim().to_string());
    }

    pub fn warn(&mut self, text: &str) {
        self.lines.push(style(text).yellow().to_string());
    }

    /// A checklist row, indented to sit under a heading.
    pub fn mark(&mut self, mark: Mark, text: &str) {
        self.lines.push(mark.render(text));
    }

    /// A `name  value` row of a details block. The name column is fixed so
    /// stacked rows line up whatever they hold.
    pub fn field(&mut self, name: &str, value: &str) {
        self.lines.push(format!("  {name:<10} {value}"));
    }

    pub fn bullet(&mut self, text: &str) {
        self.lines.push(format!("  • {text}"));
    }

    /// A numbered instruction, as used by the on-device step lists.
    pub fn step(&mut self, number: usize, text: &str) {
        self.lines.push(format!("  {number}. {text}"));
    }

    /// Puts the terminal cursor `column` characters into the line just added,
    /// which is what makes a text prompt look like one.
    fn cursor_here(&mut self, column: usize) {
        self.cursor = Some((self.lines.len(), column + 1));
    }

    /// The frame as the user sees it, with styling stripped, so a test can
    /// assert on the layout a mockup specifies rather than on escape codes.
    #[cfg(test)]
    #[must_use]
    pub fn plain(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| console::strip_ansi_codes(line).trim_end().to_string())
            .collect()
    }
}

/// The terminal the wizard paints onto.
pub struct Screen {
    term: Term,
}

impl Screen {
    #[must_use]
    pub fn new() -> Self {
        Self {
            term: Term::stdout(),
        }
    }

    /// A new empty frame sized to the terminal as it is right now, so a resize
    /// between screens is picked up without any resize plumbing.
    #[must_use]
    pub fn frame(&self) -> Frame {
        Frame::new(self.term.size().1 as usize)
    }

    /// Paints a frame from the top-left corner.
    ///
    /// Each line clears its own tail and the frame clears everything below it,
    /// so this both draws the new screen and erases the old one without the
    /// blank flash of a full clear.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal cannot be written to.
    pub fn draw(&self, frame: &Frame) -> Result<(), String> {
        let mut out = String::with_capacity(1024);
        out.push_str(HIDE_CURSOR);
        out.push_str(HOME);
        for line in &frame.lines {
            out.push_str(line);
            out.push_str(CLEAR_LINE);
            out.push('\n');
        }
        out.push_str(CLEAR_BELOW);
        if let Some((row, column)) = frame.cursor {
            let _ = write!(out, "\x1b[{row};{column}H{SHOW_CURSOR}");
        }
        self.term
            .write_str(&out)
            .and_then(|()| self.term.flush())
            .map_err(|error| error.to_string())
    }

    /// Paints a frame and leaves the cursor on the line below it, so a command
    /// that prints its own output can carry on from there.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal cannot be written to.
    pub fn draw_above_output(&self, frame: &Frame) -> Result<(), String> {
        self.draw(frame)?;
        self.term
            .write_str(SHOW_CURSOR)
            .map_err(|error| error.to_string())
    }
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads one key, turning Ctrl+C into leaving the wizard.
///
/// Ctrl+C reaches us twice over: `console` re-raises SIGINT — which the handler
/// installed by [`crate::interrupt`] catches — *and* reports the read as
/// interrupted, which surfaces here. Both must land on the same outcome or the
/// exit status would depend on which one won.
fn read_key(screen: &Screen) -> Result<Key, String> {
    match screen.term.read_key() {
        Ok(key) => Ok(key),
        Err(error) => {
            interrupt::exit_if_interrupted(&error);
            Err(error.to_string())
        }
    }
}

/// Waits for the key that dismisses a read-only screen.
///
/// # Errors
///
/// Returns an error when the terminal cannot be read.
pub fn wait_for_dismiss(screen: &Screen) -> Result<(), String> {
    loop {
        match read_key(screen)? {
            Key::Escape | Key::Enter | Key::Char(' ') => return Ok(()),
            _ => {}
        }
    }
}

/// Draws the lines under a menu that describe whichever item is highlighted.
pub type Detail<'a> = &'a dyn Fn(usize, &mut Frame);

/// A pick-one list rendered under `head` and above `footer`.
///
/// The caller builds everything above the list, because that part differs on
/// every screen; this owns the list, the cursor, and the keys.
pub struct Menu<'a> {
    head: Frame,
    items: Vec<String>,
    footer: &'a str,
    selected: usize,
    /// Called on every redraw, so the detail tracks the cursor.
    detail: Option<Detail<'a>>,
}

impl<'a> Menu<'a> {
    /// `head` must already carry the title and any body text.
    #[must_use]
    pub fn new(head: Frame, items: Vec<String>) -> Self {
        Self {
            head,
            items,
            footer: NAV_BACK,
            selected: 0,
            detail: None,
        }
    }

    #[must_use]
    pub fn footer(mut self, footer: &'a str) -> Self {
        self.footer = footer;
        self
    }

    /// Where the cursor starts. Out-of-range values fall back to the first item.
    #[must_use]
    pub fn selected(mut self, index: usize) -> Self {
        self.selected = if index < self.items.len() { index } else { 0 };
        self
    }

    #[must_use]
    pub fn detail(mut self, detail: Detail<'a>) -> Self {
        self.detail = Some(detail);
        self
    }

    fn render(&self) -> Frame {
        let mut frame = self.head.clone();
        frame.blank();
        for (index, item) in self.items.iter().enumerate() {
            if index == self.selected {
                frame.line(format!("  {} {}", style("→").green(), style(item).green()));
            } else {
                frame.line(format!("    {item}"));
            }
        }
        if let Some(detail) = self.detail {
            frame.blank();
            detail(self.selected, &mut frame);
        }
        frame.blank();
        frame.dim(self.footer);
        frame
    }

    /// Runs the list until something is chosen, or Esc backs out.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal cannot be read or written.
    pub fn run(mut self, screen: &Screen) -> Result<Option<usize>, String> {
        if self.items.is_empty() {
            return Ok(None);
        }
        let last = self.items.len() - 1;
        loop {
            screen.draw(&self.render())?;
            match read_key(screen)? {
                Key::ArrowUp | Key::Char('k') => {
                    self.selected = self.selected.checked_sub(1).unwrap_or(last);
                }
                Key::ArrowDown | Key::Char('j') | Key::Tab => {
                    self.selected = if self.selected == last {
                        0
                    } else {
                        self.selected + 1
                    };
                }
                Key::Home => self.selected = 0,
                Key::End => self.selected = last,
                Key::Enter | Key::Char(' ') => return Ok(Some(self.selected)),
                Key::Escape => return Ok(None),
                _ => {}
            }
        }
    }
}

/// Reads one line under `head`, echoing as it goes. `Esc` gives up.
///
/// `secret` masks the echo for key material, which is otherwise left on screen
/// for as long as the wizard runs.
///
/// # Errors
///
/// Returns an error when the terminal cannot be read or written.
pub fn input(
    screen: &Screen,
    head: &Frame,
    label: &str,
    secret: bool,
) -> Result<Option<String>, String> {
    let mut typed = String::new();
    loop {
        let shown = if secret {
            "•".repeat(typed.chars().count())
        } else {
            typed.clone()
        };
        let mut frame = head.clone();
        frame.blank();
        frame.line(format!("  {label}{shown}"));
        frame.cursor_here(2 + label.chars().count() + shown.chars().count());
        frame.blank();
        frame.dim(NAV_INPUT);
        screen.draw(&frame)?;
        match read_key(screen)? {
            Key::Char(character) => typed.push(character),
            Key::Backspace => {
                typed.pop();
            }
            Key::Enter => return Ok(Some(typed)),
            Key::Escape => return Ok(None),
            _ => {}
        }
    }
}

/// `mm:ss`, for a wait long enough that seconds alone read as a large number.
#[must_use]
pub fn countdown(remaining: Duration) -> String {
    let seconds = remaining.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// `N seconds`, for a wait short enough to be counted in them.
#[must_use]
pub fn seconds(remaining: Duration) -> String {
    let seconds = remaining.as_secs();
    if seconds == 1 {
        "1 second".to_string()
    } else {
        format!("{seconds} seconds")
    }
}

/// Upper-cases the first letter of a message written for a log line, so it can
/// be shown as a sentence in a details pane.
#[must_use]
pub fn sentence(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + characters.as_str()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_step_indicator_is_pushed_to_the_right_margin() {
        let mut frame = Frame::new(40);
        frame.title("Pair Apple Watch", Some("Step 1 of 3"));
        let rendered = console::strip_ansi_codes(&frame.lines[0]).to_string();
        assert_eq!(rendered.chars().count(), 40);
        assert!(rendered.starts_with("Pair Apple Watch"));
        assert!(rendered.ends_with("Step 1 of 3"));
    }

    #[test]
    fn a_title_wider_than_the_terminal_still_keeps_the_two_apart() {
        let mut frame = Frame::new(10);
        frame.title("Pair Apple Watch", Some("Step 1 of 3"));
        let rendered = console::strip_ansi_codes(&frame.lines[0]).to_string();
        assert_eq!(rendered, "Pair Apple Watch  Step 1 of 3");
    }

    #[test]
    fn countdowns_are_minutes_and_seconds() {
        assert_eq!(countdown(Duration::from_secs(277)), "04:37");
        assert_eq!(countdown(Duration::from_secs(0)), "00:00");
        assert_eq!(countdown(Duration::from_secs(605)), "10:05");
    }

    #[test]
    fn short_waits_are_counted_in_seconds_and_agree_with_themselves() {
        assert_eq!(seconds(Duration::from_secs(6)), "6 seconds");
        assert_eq!(seconds(Duration::from_secs(1)), "1 second");
        assert_eq!(seconds(Duration::from_secs(0)), "0 seconds");
    }

    #[test]
    fn a_log_message_reads_as_a_sentence() {
        assert_eq!(
            sentence("bonding completed, but the kernel produced no remote IRK"),
            "Bonding completed, but the kernel produced no remote IRK"
        );
        assert_eq!(sentence(""), "");
        assert_eq!(sentence("BlueZ is unreachable"), "BlueZ is unreachable");
    }
}
