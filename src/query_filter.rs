//! `QueryFilter`: strips terminal capability queries from replayed output.
//!
//! Programs fingerprint the terminal at startup -- DA1/DA2 (`CSI c`),
//! XTVERSION (`CSI > q`), DECRQM (`CSI ? Pd $ p`), DSR (`CSI 6 n`), OSC color
//! queries (`OSC 11 ; ? ST`), XTGETTCAP, kitty keyboard/graphics probes -- and
//! consume the answers the terminal types back. Those query bytes then sit
//! in the session's `History` / `ScrollbackBuffer`. Replaying them on
//! reconnect makes the terminal answer a second time, and with the original
//! asker long gone the answers land in whatever now owns the foreground:
//! `>|ghostty 1.3.162;22;52c2026;2$y` typed into the shell prompt.
//!
//! The filter runs only on replayed bytes. The live relay must pass queries
//! through untouched -- there the asker is still waiting for its answer.
//!
//! Streaming: `History` hands the replay over as chunks split at arbitrary
//! byte offsets, so a sequence may straddle two calls. Bytes of an
//! undecided sequence are held back until the sequence completes (or gives
//! up) and `finish()` flushes whatever is still pending.

/// Parameter/intermediate byte cap for a single CSI sequence. Anything
/// longer is not a query; it is flushed raw.
const MAX_CSI_BYTES: usize = 64;

/// Cap on an OSC/DCS/APC body held back while deciding. A query body is a
/// few bytes; past this the sequence is flushed raw and the rest streams
/// through untouched (a sixel image, a clipboard write, a hyperlink).
const MAX_STRING_BYTES: usize = 4096;

/// XTWINOPS report requests (`CSI Ps t`) that make the terminal reply.
const XTWINOPS_REPORTS: &[&[u8]] = &[b"11", b"13", b"14", b"15", b"16", b"18", b"19", b"20", b"21"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    /// OSC / DCS / APC body: held back until BEL or ST (`ESC \`).
    StringBody,
    /// Saw ESC inside a string body -- `\` terminates, anything else doesn't.
    StringEsc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringKind {
    Osc,
    Dcs,
    Apc,
}

#[derive(Debug, Default)]
pub struct QueryFilter {
    state: State,
    kind: Option<StringKind>,
    /// Bytes of the sequence currently being classified, `ESC` included.
    pending: Vec<u8>,
}

impl QueryFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scrub one chunk. Output may lag input (a sequence still being
    /// classified is held back) or lead it (a previously held sequence
    /// turned out not to be a query); the concatenation over all chunks plus
    /// `finish()` equals the input minus the queries.
    pub fn scrub(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len());
        for &b in chunk {
            self.step(b, &mut out);
        }
        out
    }

    /// Flush a sequence still pending at end of stream, raw.
    pub fn finish(self) -> Vec<u8> {
        self.pending
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => {
                if b == 0x1b {
                    self.pending.push(b);
                    self.state = State::Esc;
                } else {
                    out.push(b);
                }
            }
            State::Esc => match b {
                b'[' => {
                    self.pending.push(b);
                    self.state = State::Csi;
                }
                b']' | b'P' | b'_' => {
                    self.pending.push(b);
                    self.kind = Some(match b {
                        b']' => StringKind::Osc,
                        b'P' => StringKind::Dcs,
                        _ => StringKind::Apc,
                    });
                    self.state = State::StringBody;
                }
                // DECID: obsolete "identify terminal", answered like DA1.
                b'Z' => self.drop(),
                _ => {
                    self.flush_raw(out);
                    // The byte after a lone ESC could itself be an ESC.
                    self.step(b, out);
                }
            },
            State::Csi => {
                if self.pending.len() >= MAX_CSI_BYTES {
                    self.flush_raw(out);
                    self.step(b, out);
                    return;
                }
                match b {
                    0x20..=0x3f => self.pending.push(b),
                    0x40..=0x7e => {
                        if is_csi_query(&self.pending[2..], b) {
                            self.drop();
                        } else {
                            self.pending.push(b);
                            self.flush_raw(out);
                        }
                    }
                    _ => {
                        self.flush_raw(out);
                        self.step(b, out);
                    }
                }
            }
            State::StringBody => match b {
                0x1b => {
                    self.pending.push(b);
                    self.state = State::StringEsc;
                }
                0x07 => {
                    self.pending.push(b);
                    self.end_string(out);
                }
                _ => {
                    self.pending.push(b);
                    if self.pending.len() > MAX_STRING_BYTES || !self.string_may_be_query() {
                        self.flush_raw(out);
                    }
                }
            },
            State::StringEsc => {
                if b == b'\\' {
                    self.pending.push(b);
                    self.end_string(out);
                } else {
                    // A new sequence began inside an unterminated body: give
                    // up on the body and classify the new one.
                    let esc = self.pending.pop();
                    self.flush_raw(out);
                    self.pending.extend(esc);
                    self.state = State::Esc;
                    self.step(b, out);
                }
            }
        }
    }

    /// Early exit for string bodies that can never be a query, so large
    /// payloads (sixel, clipboard writes) are not held back.
    fn string_may_be_query(&self) -> bool {
        let body = &self.pending[2..];
        match self.kind {
            Some(StringKind::Dcs) => {
                body.len() < 2 || body.starts_with(b"$q") || body.starts_with(b"+q")
            }
            Some(StringKind::Apc) => {
                if body.is_empty() {
                    return true;
                }
                if body[0] != b'G' {
                    return false;
                }
                // Control data ends at `;`; a query is decided by then.
                match body.iter().position(|&c| c == b';') {
                    Some(end) => kitty_is_query(&body[1..end]),
                    None => true,
                }
            }
            _ => true,
        }
    }

    fn end_string(&mut self, out: &mut Vec<u8>) {
        let body = strip_terminator(&self.pending[2..]);
        let query = match self.kind {
            Some(StringKind::Osc) => body.rsplit(|&c| c == b';').next() == Some(b"?"),
            Some(StringKind::Dcs) => body.starts_with(b"$q") || body.starts_with(b"+q"),
            Some(StringKind::Apc) => {
                let control = body.strip_prefix(b"G").unwrap_or(b"");
                let end = control.iter().position(|&c| c == b';').unwrap_or(control.len());
                kitty_is_query(&control[..end])
            }
            None => false,
        };
        if query {
            self.drop();
        } else {
            self.flush_raw(out);
        }
    }

    fn drop(&mut self) {
        self.pending.clear();
        self.kind = None;
        self.state = State::Ground;
    }

    fn flush_raw(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.pending);
        self.kind = None;
        self.state = State::Ground;
    }
}

/// Body without its BEL or `ESC \` terminator.
fn strip_terminator(body: &[u8]) -> &[u8] {
    body.strip_suffix(b"\x1b\\").or_else(|| body.strip_suffix(b"\x07")).unwrap_or(body)
}

/// kitty graphics control data (`k=v,k=v`) requesting a query response.
fn kitty_is_query(control: &[u8]) -> bool {
    control.split(|&c| c == b',').any(|kv| kv == b"a=q")
}

/// Does `CSI <inner> <final>` make the terminal reply? `inner` is the
/// parameter and intermediate bytes.
fn is_csi_query(inner: &[u8], final_byte: u8) -> bool {
    let params_end = inner.iter().position(|b| (0x20..=0x2f).contains(b)).unwrap_or(inner.len());
    let (params, intermediates) = inner.split_at(params_end);
    let mut fields = params
        .strip_prefix(b"?")
        .or_else(|| params.strip_prefix(b">"))
        .or_else(|| params.strip_prefix(b"="))
        .unwrap_or(params)
        .split(|&c| c == b';');
    let first = fields.next().unwrap_or(b"");
    let second = fields.next().unwrap_or(b"");
    match (final_byte, intermediates) {
        // Primary/secondary/tertiary device attributes.
        (b'c', b"") => true,
        // Device status report (`5n`, `6n`, `?6n`, `?996n`, ...).
        (b'n', b"") => true,
        // DECRQM mode query.
        (b'p', b"$") => true,
        // XTVERSION.
        (b'q', b"") => params.starts_with(b">"),
        // kitty keyboard protocol query.
        (b'u', b"") => params == b"?",
        // XTWINOPS reports (text-area size, cell size, title, ...).
        (b't', b"") => XTWINOPS_REPORTS.contains(&first),
        // XTSMGRAPHICS read (`?Pi;1;...S`) / read-max (`?Pi;4;...S`).
        (b'S', b"") => params.starts_with(b"?") && matches!(second, b"1" | b"4"),
        _ => false,
    }
}

/// One-shot form of [`QueryFilter`] for a complete byte string.
pub fn strip_queries(bytes: &[u8]) -> Vec<u8> {
    let mut filter = QueryFilter::new();
    let mut out = filter.scrub(bytes);
    out.extend(filter.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIM_BURST: &[u8] = b"\x1b[>q\x1b[c\x1b[?2026$p\x1b[c";

    fn keeps(bytes: &[u8]) {
        assert_eq!(strip_queries(bytes), bytes, "should pass through: {bytes:?}");
    }

    fn drops(bytes: &[u8]) {
        assert_eq!(strip_queries(bytes), b"", "should be stripped: {bytes:?}");
    }

    #[test]
    fn plain_text_untouched() {
        keeps(b"hello\r\nworld $ ");
    }

    #[test]
    fn neovim_startup_burst_removed_from_surrounding_text() {
        let mut input = b"before ".to_vec();
        input.extend_from_slice(NVIM_BURST);
        input.extend_from_slice(b" after\r\n");
        assert_eq!(strip_queries(&input), b"before  after\r\n");
    }

    #[test]
    fn device_attributes_all_flavors() {
        for q in [b"\x1b[c".as_slice(), b"\x1b[0c", b"\x1b[>c", b"\x1b[>0c", b"\x1b[=c", b"\x1bZ"] {
            drops(q);
        }
    }

    #[test]
    fn device_status_reports() {
        for q in [b"\x1b[5n".as_slice(), b"\x1b[6n", b"\x1b[?6n", b"\x1b[?996n", b"\x1b[?26n"] {
            drops(q);
        }
    }

    #[test]
    fn mode_and_version_and_keyboard_queries() {
        for q in [b"\x1b[?2026$p".as_slice(), b"\x1b[4$p", b"\x1b[>q", b"\x1b[>0q", b"\x1b[?u"] {
            drops(q);
        }
    }

    #[test]
    fn window_and_graphics_reports() {
        for q in [b"\x1b[14t".as_slice(), b"\x1b[18t", b"\x1b[16t", b"\x1b[?1;1;0S", b"\x1b[?2;4S"]
        {
            drops(q);
        }
    }

    #[test]
    fn non_query_csi_kept() {
        for s in [
            b"\x1b[0m".as_slice(),
            b"\x1b[1;31m",
            b"\x1b[2J",
            b"\x1b[10;5H",
            b"\x1b[!p",    // DECSTR soft reset
            b"\x1b[2 q",   // DECSCUSR cursor style
            b"\x1b[u",     // SCORC restore cursor
            b"\x1b[=1u",   // kitty keyboard push flags
            b"\x1b[>1u",   // kitty keyboard set flags
            b"\x1b[2S",    // scroll up
            b"\x1b[22;0t", // push title
            b"\x1b[?1049h",
            b"\x1b[?2026h",
            b"\x1b[K",
        ] {
            keeps(s);
        }
    }

    #[test]
    fn osc_color_and_clipboard_queries() {
        for q in [
            b"\x1b]11;?\x07".as_slice(),
            b"\x1b]10;?\x1b\\",
            b"\x1b]4;1;?\x1b\\",
            b"\x1b]4;0;?;1;?\x07",
            b"\x1b]52;c;?\x07",
        ] {
            drops(q);
        }
    }

    #[test]
    fn non_query_osc_kept() {
        for s in [
            b"\x1b]0;title\x07".as_slice(),
            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
            b"\x1b]52;c;aGVsbG8=\x07",
            b"\x1b]11;rgb:0000/0000/0000\x07",
        ] {
            keeps(s);
        }
    }

    #[test]
    fn dcs_queries_dropped_and_sixel_kept() {
        drops(b"\x1bP$q m\x1b\\");
        drops(b"\x1bP+q544e;436f\x1b\\");
        keeps(b"\x1bPq#0;2;0;0;0#0~~~~-\x1b\\");
        keeps(b"\x1bP+p544e=1\x1b\\");
    }

    #[test]
    fn kitty_graphics_query_dropped_and_transmit_kept() {
        drops(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
        keeps(b"\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\");
        keeps(b"\x1b_Gi=1,a=d\x1b\\");
    }

    #[test]
    fn query_split_across_chunks_at_every_boundary() {
        let mut input = b"x".to_vec();
        input.extend_from_slice(NVIM_BURST);
        input.extend_from_slice(b"\x1b]11;?\x1b\\y");
        for cut in 0..=input.len() {
            let mut f = QueryFilter::new();
            let mut out = f.scrub(&input[..cut]);
            out.extend(f.scrub(&input[cut..]));
            out.extend(f.finish());
            assert_eq!(out, b"xy", "cut at {cut}");
        }
    }

    #[test]
    fn non_query_split_across_chunks_is_byte_exact() {
        let input = b"a\x1b[1;31mb\x1b]0;t\x07c\x1b(Bd";
        for cut in 0..=input.len() {
            let mut f = QueryFilter::new();
            let mut out = f.scrub(&input[..cut]);
            out.extend(f.scrub(&input[cut..]));
            out.extend(f.finish());
            assert_eq!(out, input, "cut at {cut}");
        }
    }

    #[test]
    fn finish_flushes_incomplete_sequence_raw() {
        let mut f = QueryFilter::new();
        assert_eq!(f.scrub(b"abc\x1b[?20"), b"abc");
        assert_eq!(f.finish(), b"\x1b[?20");
    }

    #[test]
    fn esc_followed_by_esc_restarts_classification() {
        assert_eq!(strip_queries(b"\x1b\x1b[c"), b"\x1b");
        assert_eq!(strip_queries(b"\x1b]0;t\x1b[c"), b"\x1b]0;t");
    }

    #[test]
    fn oversized_string_body_streams_through() {
        let mut input = b"\x1b]52;c;".to_vec();
        input.extend(std::iter::repeat_n(b'A', MAX_STRING_BYTES + 100));
        input.extend_from_slice(b"\x07tail");
        keeps(&input);
    }

    #[test]
    fn overlong_csi_streams_through() {
        let mut input = b"\x1b[".to_vec();
        input.extend(std::iter::repeat_n(b'1', MAX_CSI_BYTES + 10));
        input.extend_from_slice(b"c");
        keeps(&input);
    }
}
