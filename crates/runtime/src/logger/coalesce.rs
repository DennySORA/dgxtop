//! Coalescing of identical log records.
//!
//! A failing source logs the same error every cycle; writing it once per 30 s
//! plus a "repeated N times" summary keeps the information while bounding the
//! file growth.

use std::collections::HashMap;

use dgxtop_core::ports::LogLevel;

/// Identity of a coalescable record.
type RecordKey = (&'static str, String);

#[derive(Debug)]
struct Window {
    started_boot_ns: u64,
    suppressed: u64,
    level: LogLevel,
    sequence: u64,
}

/// A summary line owed for suppressed repetitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub level: LogLevel,
    pub target: &'static str,
    pub message: String,
    pub suppressed: u64,
}

/// Whether the record itself must be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Write,
    Suppress,
}

/// Bounded table of recent `(target, message)` windows.
#[derive(Debug)]
pub(crate) struct Coalescer {
    window_ns: u64,
    capacity: usize,
    next_sequence: u64,
    windows: HashMap<RecordKey, Window>,
}

impl Coalescer {
    pub(crate) fn new(window_ns: u64, capacity: usize) -> Self {
        Self {
            window_ns,
            capacity: capacity.max(1),
            next_sequence: 0,
            windows: HashMap::new(),
        }
    }

    /// Decides about one record; may also return summaries that are due.
    pub(crate) fn offer(
        &mut self,
        level: LogLevel,
        target: &'static str,
        message: &str,
        boot_ns: u64,
    ) -> (Vec<Summary>, Verdict) {
        let key = (target, message.to_owned());
        if let Some(window) = self.windows.get_mut(&key) {
            if boot_ns.saturating_sub(window.started_boot_ns) < self.window_ns {
                window.suppressed = window.suppressed.saturating_add(1);
                return (Vec::new(), Verdict::Suppress);
            }
            let summary = take_summary(&key, window);
            window.started_boot_ns = boot_ns;
            window.level = level;
            return (summary.into_iter().collect(), Verdict::Write);
        }
        let mut summaries = Vec::new();
        if self.windows.len() >= self.capacity {
            summaries.extend(self.evict_oldest());
        }
        self.next_sequence += 1;
        self.windows.insert(
            key,
            Window {
                started_boot_ns: boot_ns,
                suppressed: 0,
                level,
                sequence: self.next_sequence,
            },
        );
        (summaries, Verdict::Write)
    }

    /// Summaries of windows that ended; ended windows are forgotten.
    pub(crate) fn due(&mut self, now_boot_ns: u64) -> Vec<Summary> {
        let window_ns = self.window_ns;
        let ended: Vec<RecordKey> = self
            .windows
            .iter()
            .filter(|(_, window)| now_boot_ns.saturating_sub(window.started_boot_ns) >= window_ns)
            .map(|(key, _)| key.clone())
            .collect();
        let mut summaries = Vec::new();
        for key in ended {
            if let Some(mut window) = self.windows.remove(&key) {
                summaries.extend(take_summary(&key, &mut window));
            }
        }
        summaries
    }

    /// Every pending summary (shutdown).
    pub(crate) fn drain(&mut self) -> Vec<Summary> {
        let windows = std::mem::take(&mut self.windows);
        windows
            .into_iter()
            .filter_map(|(key, mut window)| take_summary(&key, &mut window))
            .collect()
    }

    fn evict_oldest(&mut self) -> Option<Summary> {
        let oldest = self
            .windows
            .iter()
            .min_by_key(|(_, window)| window.sequence)
            .map(|(key, _)| key.clone())?;
        let mut window = self.windows.remove(&oldest)?;
        take_summary(&oldest, &mut window)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.windows.len()
    }
}

fn take_summary(key: &RecordKey, window: &mut Window) -> Option<Summary> {
    if window.suppressed == 0 {
        return None;
    }
    let suppressed = std::mem::take(&mut window.suppressed);
    Some(Summary {
        level: window.level,
        target: key.0,
        message: key.1.clone(),
        suppressed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: u64 = 30_000;

    #[test]
    fn repeats_inside_the_window_are_suppressed_and_summarized() {
        let mut coalescer = Coalescer::new(WINDOW, 8);
        assert_eq!(
            coalescer.offer(LogLevel::Warn, "nvml", "timeout", 0).1,
            Verdict::Write
        );
        for time in 1..5 {
            assert_eq!(
                coalescer.offer(LogLevel::Warn, "nvml", "timeout", time).1,
                Verdict::Suppress
            );
        }
        assert!(coalescer.due(WINDOW - 1).is_empty());
        let summaries = coalescer.due(WINDOW);
        assert_eq!(
            summaries,
            vec![Summary {
                level: LogLevel::Warn,
                target: "nvml",
                message: "timeout".into(),
                suppressed: 4
            }]
        );
        assert_eq!(coalescer.len(), 0);
        assert_eq!(
            coalescer
                .offer(LogLevel::Warn, "nvml", "timeout", WINDOW + 1)
                .1,
            Verdict::Write
        );
    }

    #[test]
    fn different_targets_or_messages_are_independent_and_bounded() {
        let mut coalescer = Coalescer::new(WINDOW, 2);
        assert_eq!(
            coalescer.offer(LogLevel::Info, "a", "m", 0).1,
            Verdict::Write
        );
        assert_eq!(
            coalescer.offer(LogLevel::Info, "b", "m", 0).1,
            Verdict::Write
        );
        coalescer.offer(LogLevel::Info, "a", "m", 1);
        // Third key evicts the oldest window ("a") and returns its summary.
        let (summaries, verdict) = coalescer.offer(LogLevel::Info, "a", "n", 2);
        assert_eq!(verdict, Verdict::Write);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].target, "a");
        assert_eq!(coalescer.len(), 2);
        coalescer.offer(LogLevel::Info, "b", "m", 3);
        assert_eq!(coalescer.drain().len(), 1);
    }
}
