use bytes::Bytes;
use std::collections::VecDeque;

const DEFAULT_MAX_LINES: usize = 50;

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
            if b == b'\n' {
                let line = Bytes::from(std::mem::take(&mut self.partial));
                self.lines.push_back(line);
                if self.lines.len() > self.max_lines {
                    self.lines.pop_front();
                }
            }
        }
    }

    pub fn lines(&self) -> &VecDeque<Bytes> {
        &self.lines
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
    fn empty_lines_preserved() {
        let mut sb = ScrollbackBuffer::new();
        sb.push(b"\n\n\n");
        assert_eq!(sb.lines().len(), 3);
        assert_eq!(sb.lines()[0].as_ref(), b"\n");
    }
}
