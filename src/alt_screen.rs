//! Tracks whether the terminal is in alternate screen mode by scanning
//! PTY output for standard escape sequences.
//!
//! Sequences tracked:
//! - `\x1b[?1049h` / `\x1b[?1049l` (standard smcup/rmcup)
//! - `\x1b[?47h` / `\x1b[?47l` (legacy)
//! - `\x1b[?1047h` / `\x1b[?1047l` (legacy)

use crate::term_modes::{ModeEvent, PrivateModeScanner};

pub struct AltScreenTracker {
    scanner: PrivateModeScanner,
    in_alt: bool,
}

impl Default for AltScreenTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl AltScreenTracker {
    pub fn new() -> Self {
        Self { scanner: PrivateModeScanner::new(), in_alt: false }
    }

    pub fn in_alternate_screen(&self) -> bool {
        self.in_alt
    }

    /// Scan a chunk of PTY output bytes, updating alternate screen state.
    pub fn scan(&mut self, data: &[u8]) {
        let in_alt = &mut self.in_alt;
        self.scanner.scan(data, |event| {
            // Standard `1049`, legacy `1047` / `47`.
            if let ModeEvent::Changed { mode: 1049 | 1047 | 47, is_set: entering } = event {
                if *in_alt != entering {
                    tracing::trace!(entering, "alt-screen transition");
                }
                *in_alt = entering;
            }
        });
    }

    /// Record an alt-screen leave that gritty itself wrote to the terminal
    /// (not scanned from the PTY stream). Set directly rather than fed through
    /// `scan`: a stream that was cut mid-CSI would swallow the synthesized
    /// sequence's ESC as sequence garbage. Any such partial parse is discarded.
    pub fn note_left(&mut self) {
        self.in_alt = false;
        self.scanner.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initially_not_in_alternate_screen() {
        let t = AltScreenTracker::new();
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn enter_standard_alternate_screen() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?1049h");
        assert!(t.in_alternate_screen());
    }

    #[test]
    fn leave_standard_alternate_screen() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?1049h");
        assert!(t.in_alternate_screen());
        t.scan(b"\x1b[?1049l");
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn legacy_47() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?47h");
        assert!(t.in_alternate_screen());
        t.scan(b"\x1b[?47l");
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn legacy_1047() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?1047h");
        assert!(t.in_alternate_screen());
        t.scan(b"\x1b[?1047l");
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn sequence_spanning_chunks() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?10");
        assert!(!t.in_alternate_screen());
        t.scan(b"49h");
        assert!(t.in_alternate_screen());
    }

    #[test]
    fn sequence_split_at_every_byte() {
        let seq = b"\x1b[?1049h";
        for split in 1..seq.len() {
            let mut t = AltScreenTracker::new();
            t.scan(&seq[..split]);
            t.scan(&seq[split..]);
            assert!(t.in_alternate_screen(), "failed splitting at byte {split}");
        }
    }

    #[test]
    fn interleaved_with_normal_data() {
        let mut t = AltScreenTracker::new();
        t.scan(b"hello world\x1b[?1049hsome TUI content");
        assert!(t.in_alternate_screen());
    }

    #[test]
    fn unrelated_csi_does_not_trigger() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?25h"); // show cursor
        assert!(!t.in_alternate_screen());
        t.scan(b"\x1b[2J"); // clear screen (not a ?-mode sequence)
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn multi_param_csi_with_alt_screen() {
        let mut t = AltScreenTracker::new();
        // Some terminals send multiple params separated by ;
        t.scan(b"\x1b[?1049;1h");
        assert!(t.in_alternate_screen());
    }

    #[test]
    fn multi_param_csi_reset_leaves_alt_screen() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?1049h");
        assert!(t.in_alternate_screen());
        // DECRST with the tracked mode *not* in the last position. The `;`
        // must not be treated as an enter -- the `l` terminator governs.
        t.scan(b"\x1b[?1049;2004l");
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn multi_param_csi_set_with_trailing_param() {
        let mut t = AltScreenTracker::new();
        // Tracked mode first, untracked trailing param, `h` terminator.
        t.scan(b"\x1b[?1049;2004h");
        assert!(t.in_alternate_screen());
    }

    #[test]
    fn multi_param_csi_untracked_modes_do_not_trigger() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?2004;1000h");
        assert!(!t.in_alternate_screen());
        t.scan(b"\x1b[?2004;1000l");
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn param_overflow_resets() {
        let mut t = AltScreenTracker::new();
        // More digits than the buffer can hold -- should reset gracefully
        t.scan(b"\x1b[?1234567890h");
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn multiple_transitions() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?1049h");
        assert!(t.in_alternate_screen());
        t.scan(b"\x1b[?1049l");
        assert!(!t.in_alternate_screen());
        t.scan(b"\x1b[?1049h");
        assert!(t.in_alternate_screen());
        t.scan(b"\x1b[?1049l");
        assert!(!t.in_alternate_screen());
    }

    #[test]
    fn incomplete_sequence_then_normal_data() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?10");
        t.scan(b"hello"); // not a valid continuation
        assert!(!t.in_alternate_screen());
        // Tracker should have reset -- a real sequence should still work
        t.scan(b"\x1b[?1049h");
        assert!(t.in_alternate_screen());
    }

    #[test]
    fn note_left_clears_state_and_discards_partial_sequence() {
        let mut t = AltScreenTracker::new();
        t.scan(b"\x1b[?1049h");
        t.scan(b"\x1b[?10"); // stream cut mid-CSI (disconnect boundary)
        t.note_left();
        assert!(!t.in_alternate_screen());
        // The partial sequence was discarded: its continuation is plain text.
        t.scan(b"49h");
        assert!(!t.in_alternate_screen());
    }
}
