use bytes::Bytes;
use std::collections::VecDeque;

const DEFAULT_MAX_LINES: usize = 50;
/// Hard-wrap lines wider than this. Replay sends one Data frame per line, and
/// the protocol decoder rejects payloads > 1 MiB, so an unbounded partial (e.g.
/// `base64` on BSD emits a single multi-MB line) would break reconnect.
const MAX_LINE_BYTES: usize = 4096;

pub struct ScrollbackBuffer {
    lines: VecDeque<Bytes>,
    partial: Vec<u8>,
    max_lines: usize,
}

impl ScrollbackBuffer {
    pub fn new() -> Self {
        Self { lines: VecDeque::new(), partial: Vec::new(), max_lines: DEFAULT_MAX_LINES }
    }

    /// Scan a chunk of PTY output, splitting on newlines and keeping the last N lines.
    pub fn push(&mut self, data: &[u8]) {
        for &b in data {
            self.partial.push(b);
            if b == b'\n' || self.partial.len() >= MAX_LINE_BYTES {
                self.flush_partial();
            }
        }
    }

    fn flush_partial(&mut self) {
        let line = Bytes::from(std::mem::take(&mut self.partial));
        self.lines.push_back(line);
        if self.lines.len() > self.max_lines {
            self.lines.pop_front();
        }
    }

    pub fn lines(&self) -> &VecDeque<Bytes> {
        &self.lines
    }

    /// Iterator over the stored complete lines followed by the in-progress
    /// partial line (if any, without adding a trailing newline).
    ///
    /// Used for reconnect replay: a shell prompt is a line without `\n` and
    /// therefore lives forever in `partial`, never in `lines`. Replaying only
    /// `lines` would drop the current prompt from the new client's view.
    pub fn lines_and_partial(&self) -> impl Iterator<Item = Bytes> + '_ {
        self.lines
            .iter()
            .cloned()
            .chain((!self.partial.is_empty()).then(|| Bytes::copy_from_slice(&self.partial)))
    }

    /// Clear stored lines (keeps partial accumulator intact).
    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

impl Default for ScrollbackBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initially_empty() {
        let sb = ScrollbackBuffer::new();
        assert!(sb.lines().is_empty());
    }

    #[test]
    fn single_complete_line() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"hello world\n");
        assert_eq!(sb.lines().len(), 1);
        assert_eq!(sb.lines()[0].as_ref(), b"hello world\n");
    }

    #[test]
    fn multiple_lines_in_one_chunk() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"line1\nline2\nline3\n");
        assert_eq!(sb.lines().len(), 3);
        assert_eq!(sb.lines()[0].as_ref(), b"line1\n");
        assert_eq!(sb.lines()[1].as_ref(), b"line2\n");
        assert_eq!(sb.lines()[2].as_ref(), b"line3\n");
    }

    #[test]
    fn partial_line_not_flushed() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"incomplete");
        assert!(sb.lines().is_empty());
    }

    #[test]
    fn partial_line_completed_in_next_chunk() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"hel");
        sb.push(b"lo\n");
        assert_eq!(sb.lines().len(), 1);
        assert_eq!(sb.lines()[0].as_ref(), b"hello\n");
    }

    #[test]
    fn caps_at_max_lines() {
        let mut sb = ScrollbackBuffer::new();
        for i in 0..60 {
            sb.push(format!("line {i}\n").as_bytes());
        }
        assert_eq!(sb.lines().len(), DEFAULT_MAX_LINES);
        // Oldest lines dropped, newest kept
        assert_eq!(sb.lines()[0].as_ref(), b"line 10\n");
        assert_eq!(sb.lines()[49].as_ref(), b"line 59\n");
    }

    #[test]
    fn clear_keeps_partial() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"line1\npartial");
        assert_eq!(sb.lines().len(), 1);
        sb.clear();
        assert!(sb.lines().is_empty());
        // Partial should still be accumulating
        sb.push(b" continued\n");
        assert_eq!(sb.lines().len(), 1);
        assert_eq!(sb.lines()[0].as_ref(), b"partial continued\n");
    }

    #[test]
    fn mixed_complete_and_partial() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"line1\nline2\npart");
        assert_eq!(sb.lines().len(), 2);
        sb.push(b"ial3\nline4\n");
        assert_eq!(sb.lines().len(), 4);
        assert_eq!(sb.lines()[2].as_ref(), b"partial3\n");
        assert_eq!(sb.lines()[3].as_ref(), b"line4\n");
    }

    #[test]
    fn long_line_hard_wrapped() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(&vec![b'A'; MAX_LINE_BYTES * 3]);
        sb.push(b"tail\n");
        assert_eq!(sb.lines().len(), 4);
        assert_eq!(sb.lines()[0].len(), MAX_LINE_BYTES);
        assert_eq!(sb.lines()[2].len(), MAX_LINE_BYTES);
        assert_eq!(sb.lines()[3].as_ref(), b"tail\n");
        assert!(sb.lines().iter().all(|l| l.len() <= MAX_LINE_BYTES));
    }

    #[test]
    fn empty_lines_preserved() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\n\n\n");
        assert_eq!(sb.lines().len(), 3);
        assert_eq!(sb.lines()[0].as_ref(), b"\n");
    }

    #[test]
    fn lines_and_partial_yields_prompt() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"line1\nline2\n$ ");
        let replayed: Vec<_> = sb.lines_and_partial().collect();
        assert_eq!(replayed.len(), 3);
        assert_eq!(replayed[0].as_ref(), b"line1\n");
        assert_eq!(replayed[1].as_ref(), b"line2\n");
        assert_eq!(replayed[2].as_ref(), b"$ ");
    }

    #[test]
    fn lines_and_partial_empty_partial() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"only\n");
        let replayed: Vec<_> = sb.lines_and_partial().collect();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].as_ref(), b"only\n");
    }

    #[test]
    fn lines_and_partial_only_partial() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"typing...");
        let replayed: Vec<_> = sb.lines_and_partial().collect();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].as_ref(), b"typing...");
    }
}
