//! Tracks the DEC private modes (`CSI ? Pm h` / `CSI ? Pm l`) that programs
//! switch on the terminal: mouse reporting, focus events, bracketed paste and
//! friends. These modes live in the client's terminal, not in the PTY, so a
//! client that misses the bytes that changed one keeps a terminal that
//! disagrees with the program -- mouse reports typed at a shell prompt, or a
//! TUI that gets no mouse at all.
//!
//! `PrivateModeScanner` is the shared parser. `TermModeTracker` folds its
//! events into the set of modes a terminal attached to the stream should have.

/// Most mode numbers accepted in one `CSI ? ... h|l`. Real programs send a
/// handful; a longer list is dropped whole rather than applied in part.
const MAX_MODES_PER_SEQUENCE: usize = 16;

/// A terminal state change that `PrivateModeScanner` found in a byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeEvent {
    /// One mode number from a `CSI ? Pm h` (set) or `CSI ? Pm l` (reset).
    Changed { mode: u16, is_set: bool },
    /// RIS (`ESC c`): the terminal returns every mode to its default.
    FullReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Ground,
    Esc,
    Csi,
    Params,
}

/// Streaming parser for private-mode changes. Sequences may be split across
/// `scan` calls at any byte.
pub struct PrivateModeScanner {
    state: ScanState,
    /// Mode numbers of the sequence being parsed. Whether they are set or
    /// reset is only known at the `h` / `l` terminator.
    modes: [u16; MAX_MODES_PER_SEQUENCE],
    mode_count: usize,
    /// Digits of the mode number being read; `None` between numbers.
    partial_mode: Option<u16>,
}

impl Default for PrivateModeScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl PrivateModeScanner {
    pub fn new() -> Self {
        Self {
            state: ScanState::Ground,
            modes: [0; MAX_MODES_PER_SEQUENCE],
            mode_count: 0,
            partial_mode: None,
        }
    }

    /// Discard a partially parsed sequence.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Scans a chunk of terminal output and reports each mode change in
    /// stream order. Example input: `"\x1b[?1000;1006h"`
    pub fn scan(&mut self, data: &[u8], mut on_event_fn: impl FnMut(ModeEvent)) {
        for &byte in data {
            self.state = match (self.state, byte) {
                (_, 0x1b) => ScanState::Esc,
                (ScanState::Esc, b'[') => ScanState::Csi,
                (ScanState::Esc, b'c') => {
                    on_event_fn(ModeEvent::FullReset);
                    ScanState::Ground
                }
                (ScanState::Csi, b'?') => {
                    self.mode_count = 0;
                    self.partial_mode = None;
                    ScanState::Params
                }
                (ScanState::Params, b'0'..=b'9') => self.push_digit(byte - b'0'),
                (ScanState::Params, b';') => self.end_mode_number(),
                (ScanState::Params, b'h' | b'l') => {
                    if self.end_mode_number() == ScanState::Params {
                        let is_set = byte == b'h';
                        for &mode in &self.modes[..self.mode_count] {
                            on_event_fn(ModeEvent::Changed { mode, is_set });
                        }
                    }
                    ScanState::Ground
                }
                _ => ScanState::Ground,
            };
        }
    }

    /// Returns the next state: `Ground` drops a sequence whose mode number
    /// does not fit a `u16` (no real mode is that large).
    fn push_digit(&mut self, digit: u8) -> ScanState {
        let widened = self
            .partial_mode
            .unwrap_or(0)
            .checked_mul(10)
            .and_then(|m| m.checked_add(digit.into()));
        self.partial_mode = widened;
        if widened.is_some() { ScanState::Params } else { ScanState::Ground }
    }

    /// Returns the next state: `Ground` drops a sequence with more mode
    /// numbers than `MAX_MODES_PER_SEQUENCE`.
    fn end_mode_number(&mut self) -> ScanState {
        let Some(mode) = self.partial_mode.take() else {
            return ScanState::Params;
        };
        if self.mode_count == MAX_MODES_PER_SEQUENCE {
            return ScanState::Ground;
        }
        self.modes[self.mode_count] = mode;
        self.mode_count += 1;
        ScanState::Params
    }
}

struct TrackedMode {
    number: u16,
    is_set_by_default: bool,
}

const fn off_by_default(number: u16) -> TrackedMode {
    TrackedMode { number, is_set_by_default: false }
}

const fn on_by_default(number: u16) -> TrackedMode {
    TrackedMode { number, is_set_by_default: true }
}

/// The modes kept in sync, in ascending order. Each has the same default in
/// every terminal, so asserting the default is safe on a terminal whose state
/// is unknown. Modes whose default is a user preference (cursor blink `12`,
/// alternate scroll `1007`) are left out for that reason. The alt screen
/// (`1049`) has its own tracker because entering it needs a repaint.
const TRACKED_MODES: [TrackedMode; 16] = [
    off_by_default(1),    // application cursor keys
    on_by_default(7),     // autowrap
    off_by_default(9),    // X10 mouse
    on_by_default(25),    // cursor visible
    off_by_default(1000), // mouse: button events
    off_by_default(1002), // mouse: drag events
    off_by_default(1003), // mouse: all motion
    off_by_default(1004), // focus in/out reports
    off_by_default(1005), // mouse encoding: UTF-8
    off_by_default(1006), // mouse encoding: SGR
    off_by_default(1015), // mouse encoding: urxvt
    off_by_default(1016), // mouse encoding: SGR pixels
    off_by_default(2004), // bracketed paste
    off_by_default(2026), // synchronized output
    off_by_default(2031), // color-scheme change reports
    off_by_default(2048), // in-band resize reports
];

/// The private modes a terminal rendering a PTY stream should have, derived
/// from every byte of that stream.
pub struct TermModeTracker {
    scanner: PrivateModeScanner,
    /// Parallel to `TRACKED_MODES`.
    is_set: [bool; TRACKED_MODES.len()],
}

impl Default for TermModeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TermModeTracker {
    pub fn new() -> Self {
        Self { scanner: PrivateModeScanner::new(), is_set: default_mode_states() }
    }

    /// Scan a chunk of PTY output bytes, updating the tracked modes.
    pub fn scan(&mut self, data: &[u8]) {
        let is_set = &mut self.is_set;
        self.scanner.scan(data, |event| match event {
            ModeEvent::FullReset => *is_set = default_mode_states(),
            ModeEvent::Changed { mode, is_set: is_mode_set } => {
                if let Some(tracked_idx) = TRACKED_MODES.iter().position(|t| t.number == mode) {
                    is_set[tracked_idx] = is_mode_set;
                }
            }
        });
    }

    /// Escape sequence that puts a terminal into the tracked state whatever
    /// state it was in: every tracked mode is named, either reset or set.
    pub fn sync_sequence(&self) -> String {
        let numbers_fn = |wanted: bool| {
            TRACKED_MODES
                .iter()
                .zip(self.is_set)
                .filter(|(_, is_set)| *is_set == wanted)
                .map(|(tracked, _)| tracked.number.to_string())
                .collect::<Vec<_>>()
                .join(";")
        };
        let mut sequence = String::new();
        for (numbers, terminator) in [(numbers_fn(false), 'l'), (numbers_fn(true), 'h')] {
            if !numbers.is_empty() {
                sequence.push_str(&format!("\x1b[?{numbers}{terminator}"));
            }
        }
        sequence
    }
}

fn default_mode_states() -> [bool; TRACKED_MODES.len()] {
    TRACKED_MODES.each_ref().map(|tracked| tracked.is_set_by_default)
}

/// Escape sequence that returns every tracked mode to its default. gritty
/// writes it when it hands the terminal back to the local shell.
pub fn default_mode_sequence() -> String {
    TermModeTracker::new().sync_sequence()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULTS: &str =
        "\x1b[?1;9;1000;1002;1003;1004;1005;1006;1015;1016;2004;2026;2031;2048l\x1b[?7;25h";

    fn events(chunks: &[&[u8]]) -> Vec<ModeEvent> {
        let mut scanner = PrivateModeScanner::new();
        let mut seen = Vec::new();
        for chunk in chunks {
            scanner.scan(chunk, |event| seen.push(event));
        }
        seen
    }

    fn changed(mode: u16, is_set: bool) -> ModeEvent {
        ModeEvent::Changed { mode, is_set }
    }

    #[test]
    fn scanner_reports_set_and_reset() {
        assert_eq!(events(&[b"\x1b[?1000h"]), [changed(1000, true)]);
        assert_eq!(events(&[b"ab\x1b[?25lcd"]), [changed(25, false)]);
    }

    #[test]
    fn scanner_applies_the_terminator_to_every_mode_in_the_sequence() {
        assert_eq!(
            events(&[b"\x1b[?1000;1006;2004l"]),
            [changed(1000, false), changed(1006, false), changed(2004, false)]
        );
    }

    #[test]
    fn scanner_resumes_a_sequence_split_at_any_byte() {
        let sequence = b"\x1b[?1003;1006h";
        for split_idx in 1..sequence.len() {
            let (head, tail) = sequence.split_at(split_idx);
            assert_eq!(
                events(&[head, tail]),
                [changed(1003, true), changed(1006, true)],
                "split at {split_idx}"
            );
        }
    }

    #[test]
    fn scanner_ignores_other_sequences() {
        // SGR, a non-private mode (insert mode `CSI 4 h`), a private-mode
        // query (DECRQM), and XTSAVE.
        assert_eq!(events(&[b"\x1b[31m\x1b[4h\x1b[?2026$p\x1b[?1000s"]), []);
    }

    #[test]
    fn scanner_skips_empty_mode_numbers() {
        assert_eq!(events(&[b"\x1b[?;25;h"]), [changed(25, true)]);
        assert_eq!(events(&[b"\x1b[?h"]), []);
    }

    #[test]
    fn scanner_restarts_at_an_escape_inside_a_sequence() {
        assert_eq!(events(&[b"\x1b[?1000\x1b[?1006h"]), [changed(1006, true)]);
        assert_eq!(events(&[b"\x1b\x1b[?25l"]), [changed(25, false)]);
    }

    #[test]
    fn scanner_drops_a_sequence_with_an_oversized_mode_number() {
        assert_eq!(events(&[b"\x1b[?65535h"]), [changed(65535, true)]);
        assert_eq!(events(&[b"\x1b[?65536h"]), []);
        assert_eq!(events(&[b"\x1b[?25;99999999h\x1b[?7l"]), [changed(7, false)]);
    }

    #[test]
    fn scanner_drops_a_sequence_with_too_many_modes() {
        let at_limit = format!("\x1b[?{}h", ["5"; MAX_MODES_PER_SEQUENCE].join(";"));
        assert_eq!(events(&[at_limit.as_bytes()]).len(), MAX_MODES_PER_SEQUENCE);
        let over_limit = format!("\x1b[?{}h", ["5"; MAX_MODES_PER_SEQUENCE + 1].join(";"));
        assert_eq!(events(&[over_limit.as_bytes()]), []);
    }

    #[test]
    fn scanner_reports_full_reset() {
        assert_eq!(events(&[b"\x1bc"]), [ModeEvent::FullReset]);
        // `c` as a CSI final byte is a device-attributes query, not RIS.
        assert_eq!(events(&[b"\x1b[c\x1b[?1c"]), []);
    }

    #[test]
    fn scanner_reset_discards_a_partial_sequence() {
        let mut scanner = PrivateModeScanner::new();
        let mut seen = Vec::new();
        scanner.scan(b"\x1b[?1000", |event| seen.push(event));
        scanner.reset();
        scanner.scan(b";25h", |event| seen.push(event));
        assert_eq!(seen, []);
    }

    #[test]
    fn tracker_starts_at_the_defaults() {
        assert_eq!(TermModeTracker::new().sync_sequence(), DEFAULTS);
        assert_eq!(default_mode_sequence(), DEFAULTS);
    }

    #[test]
    fn tracker_follows_set_and_reset() {
        let mut tracker = TermModeTracker::new();
        tracker.scan(b"\x1b[?1003;1006h\x1b[?25l");
        assert_eq!(
            tracker.sync_sequence(),
            "\x1b[?1;9;25;1000;1002;1004;1005;1015;1016;2004;2026;2031;2048l\x1b[?7;1003;1006h"
        );
        tracker.scan(b"\x1b[?1003;1006l\x1b[?25h");
        assert_eq!(tracker.sync_sequence(), DEFAULTS);
    }

    #[test]
    fn tracker_ignores_untracked_modes() {
        let mut tracker = TermModeTracker::new();
        tracker.scan(b"\x1b[?1049h\x1b[?12h");
        assert_eq!(tracker.sync_sequence(), DEFAULTS);
    }

    #[test]
    fn tracker_full_reset_restores_the_defaults() {
        let mut tracker = TermModeTracker::new();
        tracker.scan(b"\x1b[?1000;2004h\x1b[?7l\x1bc");
        assert_eq!(tracker.sync_sequence(), DEFAULTS);
    }

    #[test]
    fn sync_sequence_omits_an_empty_list() {
        let mut tracker = TermModeTracker::new();
        tracker.scan(b"\x1b[?7;25l");
        assert_eq!(
            tracker.sync_sequence(),
            "\x1b[?1;7;9;25;1000;1002;1003;1004;1005;1006;1015;1016;2004;2026;2031;2048l"
        );
    }

    #[test]
    fn tracked_modes_are_ascending_and_exclude_the_alt_screen() {
        assert!(TRACKED_MODES.windows(2).all(|pair| pair[0].number < pair[1].number));
        assert!(TRACKED_MODES.iter().all(|t| ![47, 1047, 1049].contains(&t.number)));
    }
}
