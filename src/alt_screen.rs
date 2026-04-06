/// Tracks whether the terminal is in alternate screen mode by scanning
/// PTY output for standard escape sequences.
///
/// Sequences tracked:
/// - `\x1b[?1049h` / `\x1b[?1049l` (standard smcup/rmcup)
/// - `\x1b[?47h` / `\x1b[?47l` (legacy)
/// - `\x1b[?1047h` / `\x1b[?1047l` (legacy)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Normal,
    Esc,
    Bracket,
    Question,
    Param,
}

pub struct AltScreenTracker {
    state: ScanState,
    param_buf: [u8; 8],
    param_len: usize,
    in_alt: bool,
}

impl Default for AltScreenTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl AltScreenTracker {
    pub fn new() -> Self {
        Self { state: ScanState::Normal, param_buf: [0; 8], param_len: 0, in_alt: false }
    }

    pub fn in_alternate_screen(&self) -> bool {
        self.in_alt
    }

    /// Scan a chunk of PTY output bytes, updating alternate screen state.
    pub fn scan(&mut self, data: &[u8]) {
        for &b in data {
            match self.state {
                ScanState::Normal => {
                    if b == 0x1b {
                        self.state = ScanState::Esc;
                    }
                }
                ScanState::Esc => {
                    if b == b'[' {
                        self.state = ScanState::Bracket;
                    } else {
                        self.state = ScanState::Normal;
                    }
                }
                ScanState::Bracket => {
                    if b == b'?' {
                        self.state = ScanState::Question;
                    } else {
                        self.state = ScanState::Normal;
                    }
                }
                ScanState::Question => {
                    if b.is_ascii_digit() {
                        self.param_buf[0] = b;
                        self.param_len = 1;
                        self.state = ScanState::Param;
                    } else {
                        self.state = ScanState::Normal;
                    }
                }
                ScanState::Param => {
                    if b.is_ascii_digit() {
                        if self.param_len < self.param_buf.len() {
                            self.param_buf[self.param_len] = b;
                            self.param_len += 1;
                        } else {
                            // Param too long -- can't be one of ours
                            self.state = ScanState::Normal;
                            self.param_len = 0;
                        }
                    } else if b == b';' {
                        self.check_param(true);
                        self.param_len = 0;
                        // Stay in Param state for next param
                        self.state = ScanState::Question;
                    } else if b == b'h' {
                        self.check_param(true);
                        self.param_len = 0;
                        self.state = ScanState::Normal;
                    } else if b == b'l' {
                        self.check_param(false);
                        self.param_len = 0;
                        self.state = ScanState::Normal;
                    } else {
                        self.param_len = 0;
                        self.state = ScanState::Normal;
                    }
                }
            }
        }
    }

    fn check_param(&mut self, entering: bool) {
        let param = &self.param_buf[..self.param_len];
        if param == b"1049" || param == b"1047" || param == b"47" {
            self.in_alt = entering;
        }
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
}
