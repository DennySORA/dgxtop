//! Bounded line assembly and rate limiting of a host's stderr.
//!
//! Host stderr is untrusted text: lines are cut at a fixed length, sanitized
//! before they reach the logger, and only a fixed number per window is
//! forwarded. Nothing here blocks or grows without bound.

use dgxtop_core::text::sanitize_message;

/// Longest line kept; the rest of a longer line is dropped.
pub const MAX_STDERR_LINE_BYTES: usize = 1024;

#[derive(Debug)]
pub(crate) struct StderrDrain {
    line: Vec<u8>,
    truncated: bool,
    window_ns: u64,
    lines_per_window: u32,
    window_start_ns: u64,
    lines_in_window: u32,
    suppressed: u64,
    suppressed_total: u64,
}

impl StderrDrain {
    pub(crate) fn new(lines_per_window: u32, window_ns: u64, now_ns: u64) -> Self {
        Self {
            line: Vec::new(),
            truncated: false,
            window_ns,
            lines_per_window: lines_per_window.max(1),
            window_start_ns: now_ns,
            lines_in_window: 0,
            suppressed: 0,
            suppressed_total: 0,
        }
    }

    /// Splits `bytes` into lines and forwards the allowed ones to `emit`.
    pub(crate) fn absorb(&mut self, bytes: &[u8], now_ns: u64, emit: &mut dyn FnMut(String)) {
        for byte in bytes {
            if *byte == b'\n' {
                self.finish_line(now_ns, emit);
            } else if self.line.len() < MAX_STDERR_LINE_BYTES {
                self.line.push(*byte);
            } else {
                self.truncated = true;
            }
        }
    }

    /// Forwards a final unterminated line (end of stream).
    pub(crate) fn finish(&mut self, now_ns: u64, emit: &mut dyn FnMut(String)) {
        if !self.line.is_empty() {
            self.finish_line(now_ns, emit);
        }
    }

    pub(crate) const fn suppressed_total(&self) -> u64 {
        self.suppressed_total
    }

    fn finish_line(&mut self, now_ns: u64, emit: &mut dyn FnMut(String)) {
        let raw = std::mem::take(&mut self.line);
        let truncated = std::mem::take(&mut self.truncated);
        if now_ns.saturating_sub(self.window_start_ns) >= self.window_ns {
            if self.suppressed > 0 {
                emit(format!(
                    "suppressed {} stderr lines in the last window",
                    self.suppressed
                ));
            }
            self.window_start_ns = now_ns;
            self.lines_in_window = 0;
            self.suppressed = 0;
        }
        if self.lines_in_window >= self.lines_per_window {
            self.suppressed += 1;
            self.suppressed_total += 1;
            return;
        }
        self.lines_in_window += 1;
        let mut text = sanitize_message(&String::from_utf8_lossy(&raw));
        if truncated {
            text.push_str(" [truncated]");
        }
        if !text.is_empty() {
            emit(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(drain: &mut StderrDrain, bytes: &[u8], now: u64) -> Vec<String> {
        let mut lines = Vec::new();
        drain.absorb(bytes, now, &mut |line| lines.push(line));
        lines
    }

    #[test]
    fn lines_split_across_reads_are_reassembled_and_sanitized() {
        let mut drain = StderrDrain::new(10, 1000, 0);
        assert!(collect(&mut drain, b"partial \x1b[31mred", 0).is_empty());
        assert_eq!(
            collect(&mut drain, b" line\nnext\n", 0),
            ["partial red line", "next"]
        );
        let mut tail = Vec::new();
        drain.absorb(b"tail", 0, &mut |line| tail.push(line));
        drain.finish(0, &mut |line| tail.push(line));
        assert_eq!(tail, ["tail"]);
    }

    #[test]
    fn long_lines_are_truncated_and_floods_are_rate_limited() {
        let mut drain = StderrDrain::new(2, 1000, 0);
        let long = vec![b'x'; 5000];
        let mut input = long.clone();
        input.push(b'\n');
        let lines = collect(&mut drain, &input, 0);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("[truncated]"));
        assert!(lines[0].len() <= MAX_STDERR_LINE_BYTES + 12);
        let flood = collect(&mut drain, b"a\nb\nc\nd\n", 10);
        assert_eq!(flood, ["a"]);
        assert_eq!(drain.suppressed_total(), 3);
        let next = collect(&mut drain, b"e\n", 1000);
        assert_eq!(next, ["suppressed 3 stderr lines in the last window", "e"]);
    }
}
