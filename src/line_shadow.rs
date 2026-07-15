//! `LineShadow`: a cursor-state emulator over PTY output.
//!
//! Interprets a byte stream the way a terminal would, tracking only what a
//! reconnect cursor-restore needs: the cursor's on-screen column and the
//! active SGR (attribute/color) state. It deliberately stores no cell
//! contents -- on a clean auto-reconnect the client's screen already shows
//! the current line intact (the reconnect status line lives on the row
//! *below* and is erased on success); only the cursor position and SGR state
//! were disturbed. Replaying raw transcript bytes to "repaint" the line is
//! what caused visual artifacts: shell line editors emit relative cursor
//! moves, delete-char, and insert-char sequences that are only meaningful
//! against the cells that existed when first played.
//!
//! Column tracking emulates autowrap (DECAWM, including `\x1b[?7h/l`) and
//! wide characters so that `\r`, backspace, and CUB/CUF -- which operate on
//! on-screen columns -- land where the terminal's cursor actually is.
//! Vertical movement is tracked only as far as the column is concerned
//! (`\n` preserves the column; NEL/CNL/CPL reset it); rows are the client
//! terminal's business.

use std::fmt::Write;

use unicode_width::UnicodeWidthChar;

/// Degradation cap for pathological streams that endlessly stack SGR
/// attributes without ever resetting: past this many segments the state is
/// cleared (restore degrades to a bare `\x1b[0m`). Real shells reset SGR at
/// every prompt, so the list stays tiny in practice.
const MAX_SGR_SEGMENTS: usize = 64;

/// Parameter/intermediate byte cap for a single CSI sequence. Anything
/// longer is not a sequence we act on; it is consumed and ignored.
const MAX_CSI_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    /// `ESC ( / ) / * / + / # / %` etc. -- one following byte to consume.
    EscIntermediate,
    Csi,
    /// OSC / DCS / APC / PM / SOS body: consume until BEL or ST (`ESC \`).
    StringBody,
    /// Saw ESC inside a string body -- `\` terminates, anything else doesn't.
    StringEsc,
}

pub struct LineShadow {
    /// Terminal width in columns at the time the bytes were rendered.
    /// 0 = unknown: no wrap emulation, no clamping.
    cols: usize,
    /// Cursor column, 0-based. May equal `cols` transiently (cursor past the
    /// last column after filling a row, i.e. the wrap-pending position).
    col: usize,
    autowrap: bool,
    /// SGR parameter segments applied since the last full reset, in order.
    /// Replaying them verbatim reproduces the attribute state.
    sgr: Vec<String>,
    /// DECSC/DECRC (`ESC 7` / `ESC 8`) snapshot: (column, SGR state).
    saved: Option<(usize, Vec<String>)>,
    state: State,
    csi: Vec<u8>,
    csi_overflow: bool,
    /// In-progress UTF-8 sequence: buffered bytes and how many are expected.
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

impl LineShadow {
    pub fn new(cols: u16) -> Self {
        Self {
            cols: cols as usize,
            col: 0,
            autowrap: true,
            sgr: Vec::new(),
            saved: None,
            state: State::Ground,
            csi: Vec::new(),
            csi_overflow: false,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        }
    }

    pub fn cursor_col(&self) -> usize {
        self.col
    }

    /// Terminal bytes that move the cursor from column 0 back to the tracked
    /// column and re-establish the tracked SGR state. Assumes the cursor is
    /// already on the right row.
    pub fn restore_sequence(&self) -> String {
        let mut s = String::new();
        // CHA is 1-based; clamp the transient wrap-pending position (`col ==
        // cols`) back onto the last real column.
        let col = if self.cols > 0 { self.col.min(self.cols - 1) } else { self.col };
        if col == 0 {
            s.push('\r');
        } else {
            let _ = write!(s, "\x1b[{}G", col + 1);
        }
        s.push_str("\x1b[0m");
        for seg in &self.sgr {
            let _ = write!(s, "\x1b[{seg}m");
        }
        s
    }

    pub fn scan(&mut self, data: &[u8]) {
        for &b in data {
            self.step(b);
        }
    }

    fn step(&mut self, b: u8) {
        match self.state {
            State::Ground => self.ground(b),
            State::Esc => self.esc(b),
            State::EscIntermediate => self.state = State::Ground,
            State::Csi => self.csi_byte(b),
            State::StringBody => {
                if b == 0x07 {
                    self.state = State::Ground;
                } else if b == 0x1b {
                    self.state = State::StringEsc;
                }
            }
            State::StringEsc => {
                self.state = if b == b'\\' { State::Ground } else { State::StringBody };
            }
        }
    }

    fn ground(&mut self, b: u8) {
        // A control byte interrupting a multibyte char abandons it.
        if b < 0x20 || b == 0x7f {
            self.utf8_need = 0;
            self.utf8_len = 0;
        }
        match b {
            0x1b => self.state = State::Esc,
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => {
                let next_stop = self.col / 8 * 8 + 8;
                self.col = if self.cols > 0 { next_stop.min(self.cols - 1) } else { next_stop };
            }
            // LF/VT/FF move rows, not columns.
            b'\n' | 0x0b | 0x0c => {}
            _ if b < 0x20 || b == 0x7f => {}
            _ => self.printable(b),
        }
    }

    fn printable(&mut self, b: u8) {
        if self.utf8_need > 0 {
            if b & 0xc0 == 0x80 {
                self.utf8[self.utf8_len] = b;
                self.utf8_len += 1;
                self.utf8_need -= 1;
                if self.utf8_need == 0 {
                    let w = std::str::from_utf8(&self.utf8[..self.utf8_len])
                        .ok()
                        .and_then(|s| s.chars().next())
                        .and_then(UnicodeWidthChar::width)
                        .unwrap_or(1);
                    self.utf8_len = 0;
                    self.advance(w);
                }
                return;
            }
            // Broken sequence (e.g. the transcript started mid-char after
            // history eviction): drop it and reprocess this byte fresh.
            self.utf8_need = 0;
            self.utf8_len = 0;
        }
        match b {
            0x20..=0x7e => self.advance(1),
            0xc2..=0xdf => self.start_utf8(b, 1),
            0xe0..=0xef => self.start_utf8(b, 2),
            0xf0..=0xf4 => self.start_utf8(b, 3),
            // Stray continuation or invalid lead byte: count one column, the
            // way a terminal renders a replacement glyph.
            _ => self.advance(1),
        }
    }

    fn start_utf8(&mut self, b: u8, need: usize) {
        self.utf8[0] = b;
        self.utf8_len = 1;
        self.utf8_need = need;
    }

    /// Advance the cursor by a printed glyph's width, emulating autowrap.
    fn advance(&mut self, w: usize) {
        if w == 0 {
            return;
        }
        if self.cols == 0 {
            self.col += w;
        } else if self.autowrap {
            if self.col + w > self.cols {
                self.col = 0;
            }
            self.col = (self.col + w).min(self.cols);
        } else {
            self.col = (self.col + w).min(self.cols - 1);
        }
    }

    fn esc(&mut self, b: u8) {
        self.state = State::Ground;
        match b {
            b'[' => {
                self.csi.clear();
                self.csi_overflow = false;
                self.state = State::Csi;
            }
            b']' | b'P' | b'X' | b'^' | b'_' => self.state = State::StringBody,
            b'7' => self.saved = Some((self.col, self.sgr.clone())),
            b'8' => {
                if let Some((col, sgr)) = self.saved.clone() {
                    self.col = col;
                    self.sgr = sgr;
                }
            }
            // RIS: full reset.
            b'c' => {
                self.col = 0;
                self.autowrap = true;
                self.sgr.clear();
                self.saved = None;
            }
            // NEL: next line, column 0.
            b'E' => self.col = 0,
            // Charset / alignment / encoding designators consume one byte.
            b'(' | b')' | b'*' | b'+' | b'#' | b'%' | b' ' => {
                self.state = State::EscIntermediate;
            }
            _ => {}
        }
    }

    fn csi_byte(&mut self, b: u8) {
        match b {
            // Parameter and intermediate bytes.
            0x20..=0x3f => {
                if self.csi.len() < MAX_CSI_BYTES {
                    self.csi.push(b);
                } else {
                    self.csi_overflow = true;
                }
            }
            // Final byte: dispatch.
            0x40..=0x7e => {
                if !self.csi_overflow {
                    self.csi_dispatch(b);
                }
                self.state = State::Ground;
            }
            0x1b => self.state = State::Esc,
            // Other C0 controls inside a CSI: ignore.
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, final_byte: u8) {
        let raw = std::mem::take(&mut self.csi);
        // Sequences with intermediate bytes (0x20-0x2f) are never ones we
        // track; a private marker (`?`, `<`, `=`, `>`) only matters for
        // DECAWM.
        if raw.iter().any(|&b| (0x20..=0x2f).contains(&b)) {
            return;
        }
        let private = raw.first().is_some_and(|&b| (0x3c..=0x3f).contains(&b));
        let params = std::str::from_utf8(if private { &raw[1..] } else { &raw }).unwrap_or("");
        if private {
            // DECAWM (mode 7) is the only private mode that affects columns.
            if (final_byte == b'h' || final_byte == b'l')
                && raw.first() == Some(&b'?')
                && params.split(';').any(|p| p == "7")
            {
                self.autowrap = final_byte == b'h';
            }
            return;
        }
        match final_byte {
            b'm' => self.apply_sgr(params),
            // CHA / HPA: absolute column (1-based).
            b'G' | b'`' => self.set_col(nth_param(params, 0, 1).saturating_sub(1)),
            // CUF / HPR: right, clamped at the margin (no wrap).
            b'C' | b'a' => {
                let n = nth_param(params, 0, 1).max(1);
                let target = self.col.saturating_add(n);
                self.col = if self.cols > 0 { target.min(self.cols - 1) } else { target };
            }
            // CUB: left.
            b'D' => self.col = self.col.saturating_sub(nth_param(params, 0, 1).max(1)),
            // CUP / HVP: row;col -- only the column concerns us.
            b'H' | b'f' => self.set_col(nth_param(params, 1, 1).saturating_sub(1)),
            // CNL / CPL: next/previous line, column 0.
            b'E' | b'F' => self.col = 0,
            // Erases and in-line edits (J, K, P, X, @, L, M...) move cells,
            // not the cursor.
            _ => {}
        }
    }

    fn set_col(&mut self, col: usize) {
        self.col = if self.cols > 0 { col.min(self.cols - 1) } else { col };
    }

    /// Apply an SGR parameter string, maintaining the ordered segment list.
    /// Extended colors (`38;5;n`, `38;2;r;g;b`, and their colon forms) are
    /// kept as single segments so their numeric components -- which may be
    /// `0` -- are never mistaken for a reset.
    fn apply_sgr(&mut self, params: &str) {
        if params.is_empty() {
            self.sgr.clear();
            return;
        }
        let toks: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < toks.len() {
            let t = toks[i];
            // Colon subparameters (`38:5:196`) stay within one token; the
            // leading number identifies the segment.
            let n: Option<u32> =
                if t.is_empty() { Some(0) } else { t.split(':').next().unwrap_or("").parse().ok() };
            match n {
                Some(0) => {
                    self.sgr.clear();
                    i += 1;
                }
                Some(38 | 48 | 58) if !t.contains(':') => {
                    let take = match toks.get(i + 1).copied() {
                        Some("5") => 3,
                        Some("2") => 5,
                        _ => 1,
                    };
                    let end = (i + take).min(toks.len());
                    self.push_sgr(toks[i..end].join(";"));
                    i = end;
                }
                Some(_) => {
                    self.push_sgr(t.to_string());
                    i += 1;
                }
                // Unparsable token: skip it.
                None => i += 1,
            }
        }
    }

    fn push_sgr(&mut self, seg: String) {
        if self.sgr.len() >= MAX_SGR_SEGMENTS {
            // Pathological attribute stacking: degrade to a plain reset
            // rather than growing (and later replaying) without bound.
            self.sgr.clear();
        }
        self.sgr.push(seg);
    }
}

/// `idx`-th semicolon-separated numeric parameter, or `default` when absent,
/// empty, or unparsable. Colon subparameters are ignored past the leading
/// number.
fn nth_param(params: &str, idx: usize, default: usize) -> usize {
    params
        .split(';')
        .nth(idx)
        .filter(|t| !t.is_empty())
        .and_then(|t| t.split(':').next())
        .and_then(|t| t.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shadow(cols: u16, bytes: &[u8]) -> LineShadow {
        let mut s = LineShadow::new(cols);
        s.scan(bytes);
        s
    }

    #[test]
    fn plain_text_advances_column() {
        assert_eq!(shadow(80, b"hello").cursor_col(), 5);
    }

    #[test]
    fn carriage_return_resets_column() {
        assert_eq!(shadow(80, b"hello\r").cursor_col(), 0);
        assert_eq!(shadow(80, b"hello\rab").cursor_col(), 2);
    }

    #[test]
    fn newline_preserves_column() {
        // LF moves rows only; a bare `\n` mid-line keeps the column.
        assert_eq!(shadow(80, b"ab\ncd").cursor_col(), 4);
        assert_eq!(shadow(80, b"ab\r\ncd").cursor_col(), 2);
    }

    #[test]
    fn backspace_moves_left_saturating() {
        assert_eq!(shadow(80, b"abc\x08").cursor_col(), 2);
        assert_eq!(shadow(80, b"\x08\x08").cursor_col(), 0);
    }

    #[test]
    fn tab_advances_to_next_stop() {
        assert_eq!(shadow(80, b"\t").cursor_col(), 8);
        assert_eq!(shadow(80, b"abc\t").cursor_col(), 8);
        assert_eq!(shadow(80, b"12345678\t").cursor_col(), 16);
    }

    #[test]
    fn wide_char_counts_two_columns() {
        assert_eq!(shadow(80, "日本".as_bytes()).cursor_col(), 4);
        assert_eq!(shadow(80, "a日b".as_bytes()).cursor_col(), 4);
    }

    #[test]
    fn autowrap_wraps_at_width() {
        // 25 chars on a 10-col terminal: 10 + 10 + 5.
        assert_eq!(shadow(10, &[b'x'; 25]).cursor_col(), 5);
    }

    #[test]
    fn wide_char_wraps_early_at_odd_boundary() {
        // 3 cells used, a width-2 char doesn't fit in the last column: it
        // wraps to the next row and lands at column 2.
        assert_eq!(shadow(4, "abc日".as_bytes()).cursor_col(), 2);
    }

    #[test]
    fn autowrap_disabled_clamps_at_margin() {
        assert_eq!(shadow(5, b"\x1b[?7l0123456789").cursor_col(), 4);
        // Re-enabled: printing wraps again ("56789" fills the row and wraps).
        assert_eq!(shadow(5, b"\x1b[?7l01234\x1b[?7h56789").cursor_col(), 4);
    }

    #[test]
    fn unknown_width_never_wraps() {
        assert_eq!(shadow(0, &[b'x'; 300]).cursor_col(), 300);
    }

    #[test]
    fn cursor_forward_and_back() {
        assert_eq!(shadow(80, b"abc\x1b[2D").cursor_col(), 1);
        assert_eq!(shadow(80, b"abc\x1b[10C").cursor_col(), 13);
        // CUF clamps at the right margin instead of wrapping.
        assert_eq!(shadow(10, b"abc\x1b[100C").cursor_col(), 9);
        // Default parameter is 1.
        assert_eq!(shadow(80, b"abc\x1b[D").cursor_col(), 2);
    }

    #[test]
    fn absolute_column_and_cup() {
        assert_eq!(shadow(80, b"\x1b[12G").cursor_col(), 11);
        assert_eq!(shadow(80, b"\x1b[G").cursor_col(), 0);
        assert_eq!(shadow(80, b"\x1b[5;12H").cursor_col(), 11);
        assert_eq!(shadow(80, b"abc\x1b[H").cursor_col(), 0);
        // Clamped to the terminal width.
        assert_eq!(shadow(10, b"\x1b[99G").cursor_col(), 9);
    }

    #[test]
    fn cnl_cpl_nel_reset_column() {
        assert_eq!(shadow(80, b"abc\x1b[E").cursor_col(), 0);
        assert_eq!(shadow(80, b"abc\x1b[2F").cursor_col(), 0);
        assert_eq!(shadow(80, b"abc\x1bE").cursor_col(), 0);
    }

    #[test]
    fn erases_and_edits_do_not_move_cursor() {
        for seq in [&b"abc\x1b[K"[..], b"abc\x1b[2J", b"abc\x1b[3P", b"abc\x1b[2@", b"abc\x1b[X"] {
            assert_eq!(shadow(80, seq).cursor_col(), 3, "seq {:?}", seq);
        }
    }

    #[test]
    fn save_restore_cursor() {
        // Save at col 3, wander, restore.
        assert_eq!(shadow(80, b"abc\x1b7\rwander\x1b8").cursor_col(), 3);
        // DECSC also snapshots SGR state.
        let s = shadow(80, b"\x1b[31m\x1b7\x1b[0m\x1b8");
        assert_eq!(s.restore_sequence(), "\r\x1b[0m\x1b[31m");
    }

    #[test]
    fn osc_and_dcs_bodies_are_ignored() {
        assert_eq!(shadow(80, b"\x1b]0;window title\x07ab").cursor_col(), 2);
        assert_eq!(shadow(80, b"\x1b]133;A\x1b\\ab").cursor_col(), 2);
        assert_eq!(shadow(80, b"\x1bPsome dcs\x1b\\ab").cursor_col(), 2);
    }

    #[test]
    fn charset_designators_consume_one_byte() {
        assert_eq!(shadow(80, b"\x1b(Bab").cursor_col(), 2);
    }

    #[test]
    fn sgr_tracking_and_reset() {
        assert!(shadow(80, b"\x1b[1;32m$ \x1b[0m").sgr.is_empty());
        assert_eq!(shadow(80, b"\x1b[31m").sgr, vec!["31"]);
        assert_eq!(shadow(80, b"\x1b[1;31m").sgr, vec!["1", "31"]);
        assert_eq!(shadow(80, b"\x1b[1m\x1b[0m\x1b[4m").sgr, vec!["4"]);
        // Reset mid-sequence: `0;31` ends up just red.
        assert_eq!(shadow(80, b"\x1b[1m\x1b[0;31m").sgr, vec!["31"]);
        // Empty parameter = reset.
        assert!(shadow(80, b"\x1b[31m\x1b[m").sgr.is_empty());
    }

    #[test]
    fn extended_colors_are_single_segments() {
        assert_eq!(shadow(80, b"\x1b[38;5;196m").sgr, vec!["38;5;196"]);
        assert_eq!(shadow(80, b"\x1b[38;2;10;20;30m").sgr, vec!["38;2;10;20;30"]);
        // A zero color component is not a reset.
        assert_eq!(shadow(80, b"\x1b[38;2;0;255;0m").sgr, vec!["38;2;0;255;0"]);
        // Colon form stays one token.
        assert_eq!(shadow(80, b"\x1b[38:5:196m").sgr, vec!["38:5:196"]);
        // Trailing attribute after an extended color parses independently.
        assert_eq!(shadow(80, b"\x1b[38;5;196;1m").sgr, vec!["38;5;196", "1"]);
    }

    #[test]
    fn sgr_overflow_degrades_to_reset() {
        let mut s = LineShadow::new(80);
        for _ in 0..200 {
            s.scan(b"\x1b[1m");
        }
        assert!(s.sgr.len() <= MAX_SGR_SEGMENTS);
        assert!(s.restore_sequence().len() < 1024);
    }

    #[test]
    fn restore_sequence_at_column_zero() {
        assert_eq!(shadow(80, b"").restore_sequence(), "\r\x1b[0m");
        assert_eq!(shadow(80, b"abc\r").restore_sequence(), "\r\x1b[0m");
    }

    #[test]
    fn restore_sequence_mid_line_with_attributes() {
        // Prompt painted, dim attribute still active, cursor at col 2.
        let s = shadow(80, b"\x1b[2m$ ");
        assert_eq!(s.restore_sequence(), "\x1b[3G\x1b[0m\x1b[2m");
    }

    #[test]
    fn restore_clamps_wrap_pending_position() {
        // Row exactly filled: cursor sits past the last column; restore
        // targets the last real column.
        let s = shadow(5, b"01234");
        assert_eq!(s.cursor_col(), 5);
        assert_eq!(s.restore_sequence(), "\x1b[5G\x1b[0m");
    }

    #[test]
    fn editing_transcript_lands_on_final_column() {
        // A zle-style session: prompt, type "echo hi", left twice, insert a
        // blank (ICH, no cursor move), type "X" over it.
        let s = shadow(80, b"\x1b[1;32m$\x1b[0m echo hi\x1b[2D\x1b[@X");
        // "$ echo hi" = 9 cols, minus 2 left, plus the typed "X".
        assert_eq!(s.cursor_col(), 8);
    }

    #[test]
    fn sequence_split_at_every_byte() {
        let seq = "ab\x1b[38;5;196m日\x1b]0;t\x07\x1b[3D".as_bytes();
        let reference = shadow(80, seq);
        for split in 1..seq.len() {
            let mut s = LineShadow::new(80);
            s.scan(&seq[..split]);
            s.scan(&seq[split..]);
            assert_eq!(s.cursor_col(), reference.cursor_col(), "split at {split}");
            assert_eq!(s.sgr, reference.sgr, "split at {split}");
        }
    }

    #[test]
    fn truncated_head_does_not_panic_and_reanchors() {
        // Transcript starting mid-escape (history eviction boundary): the
        // stray tail may count as text, but a later \r re-anchors.
        let s = shadow(80, b"8;5;196mgarbage\rok");
        assert_eq!(s.cursor_col(), 2);
    }

    #[test]
    fn broken_utf8_recovers() {
        // Lead byte followed by a non-continuation: dropped, next byte fresh.
        assert_eq!(shadow(80, b"\xe6ab").cursor_col(), 2);
        // Stray continuation bytes count one column each.
        assert_eq!(shadow(80, b"\x80\x80").cursor_col(), 2);
    }

    #[test]
    fn ris_resets_everything() {
        let s = shadow(80, b"abc\x1b[31m\x1bc");
        assert_eq!(s.cursor_col(), 0);
        assert!(s.sgr.is_empty());
        assert_eq!(s.restore_sequence(), "\r\x1b[0m");
    }
}
