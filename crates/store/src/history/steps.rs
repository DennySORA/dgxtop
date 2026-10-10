//! The time-weighted step model shared by rollup finalization and queries.
//!
//! A valid sample value `v_i` holds on `[t_i, min(t_next, stale_deadline_i,
//! limit))`. Missing time is never filled and a stale value is never carried
//! to the next successful sample. Cut markers (`value: None`) end the previous
//! step without starting a new one.

use dgxtop_core::metric::Scalar;

use super::chunk::{SliceIter, Slices};
use super::records::GaugeSample;

/// One valid step `[start, end)` of a gauge series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Step {
    pub start_ns: u64,
    pub end_ns: u64,
    pub value: Scalar,
    pub segment: u32,
    /// The value expired at its stale deadline before anything replaced it.
    pub ended_by_deadline: bool,
}

/// Raw gauge records whose evicted prefix is summarized by `carry`.
#[derive(Clone, Copy)]
pub(crate) struct RawGauge<'s, 'a> {
    pub slices: &'s Slices<'a, GaugeSample>,
    /// Newest record older than `floor_ns` (its step may cross the floor).
    pub carry: Option<GaugeSample>,
    /// Raw history is complete from this time on.
    pub floor_ns: u64,
}

/// Iterator over the steps that can overlap `[from, ..)`.
pub(crate) struct StepIter<'s, 'a> {
    records: SliceIter<'s, 'a, GaugeSample>,
    pending: Option<GaugeSample>,
    limit_ns: u64,
    consumed: usize,
}

impl<'s, 'a> StepIter<'s, 'a> {
    /// Starts at the newest record before `from_ns` (or the carry), so the
    /// step crossing `from_ns` is included. Steps never extend past
    /// `limit_ns` (the query clock), because the future is not covered.
    pub(crate) fn covering(raw: RawGauge<'s, 'a>, from_ns: u64, limit_ns: u64) -> Self {
        let bound = from_ns.max(raw.floor_ns);
        let slices = raw.slices;
        let first_retained = slices.seek_start(raw.floor_ns);
        let first_at_bound = slices.seek_start(bound);
        let retained_predecessor = slices
            .previous(first_at_bound)
            .filter(|position| *position >= first_retained)
            .and_then(|position| slices.get(position))
            .copied();
        let pending =
            retained_predecessor.or_else(|| raw.carry.filter(|carry| carry.time_ns < bound));
        Self {
            records: slices.iter_from(first_at_bound),
            pending,
            limit_ns,
            consumed: 0,
        }
    }

    /// Records read so far (scan accounting).
    pub(crate) fn consumed(&self) -> usize {
        self.consumed
    }

    fn take_record(&mut self) -> Option<GaugeSample> {
        let record = self.records.next().copied();
        if record.is_some() {
            self.consumed = self.consumed.saturating_add(1);
        }
        record
    }
}

impl Iterator for StepIter<'_, '_> {
    type Item = Step;

    fn next(&mut self) -> Option<Step> {
        loop {
            let record = match self.pending.take() {
                Some(record) => record,
                None => self.take_record()?,
            };
            let following = self.take_record();
            self.pending = following;
            let Some(value) = record.value else {
                continue;
            };
            let mut end = record.stale_deadline_ns;
            let mut ended_by_deadline = true;
            if let Some(next) = following
                && next.time_ns <= end
            {
                end = next.time_ns;
                ended_by_deadline = false;
            }
            if self.limit_ns < end {
                end = self.limit_ns;
                ended_by_deadline = false;
            }
            if end <= record.time_ns {
                continue;
            }
            return Some(Step {
                start_ns: record.time_ns,
                end_ns: end,
                value,
                segment: record.segment,
                ended_by_deadline,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::chunk::ChunkLog;
    use dgxtop_core::budget::{ByteLedger, Partition};

    fn record(time_ns: u64, deadline_ns: u64, value: Option<f64>) -> GaugeSample {
        GaugeSample {
            time_ns,
            stale_deadline_ns: deadline_ns,
            value: value.map(|v| Scalar::from_f64(v).unwrap()),
            segment: 0,
        }
    }

    fn log(records: &[GaugeSample]) -> ChunkLog<GaugeSample> {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut log = ChunkLog::new(&ledger, Partition::RawHistory);
        for record in records {
            log.try_push(*record).unwrap();
        }
        log
    }

    fn steps(
        log: &ChunkLog<GaugeSample>,
        carry: Option<GaugeSample>,
        floor: u64,
        from: u64,
        limit: u64,
    ) -> Vec<(u64, u64, f64, bool)> {
        let slices = log.slices();
        let raw = RawGauge {
            slices: &slices,
            carry,
            floor_ns: floor,
        };
        StepIter::covering(raw, from, limit)
            .map(|step| {
                (
                    step.start_ns,
                    step.end_ns,
                    step.value.to_f64_lossy(),
                    step.ended_by_deadline,
                )
            })
            .collect()
    }

    #[test]
    fn steps_end_at_next_sample_deadline_or_cut() {
        let log = log(&[
            record(0, 30, Some(1.0)),
            record(10, 40, Some(2.0)),
            record(100, 130, Some(3.0)),
            record(110, 140, None),
            record(120, 150, Some(4.0)),
        ]);
        assert_eq!(
            steps(&log, None, 0, 0, u64::MAX),
            vec![
                (0, 10, 1.0, false),
                (10, 40, 2.0, true),
                (100, 110, 3.0, false),
                (120, 150, 4.0, true),
            ]
        );
    }

    #[test]
    fn limit_caps_the_open_step() {
        let log = log(&[record(0, 30, Some(1.0))]);
        assert_eq!(steps(&log, None, 0, 0, 12), vec![(0, 12, 1.0, false)]);
        assert!(steps(&log, None, 0, 0, 0).is_empty());
    }

    #[test]
    fn predecessor_or_carry_provides_the_crossing_step() {
        let log_both = log(&[record(50, 80, Some(5.0)), record(70, 100, Some(6.0))]);
        assert_eq!(
            steps(&log_both, None, 0, 60, u64::MAX)[0],
            (50, 70, 5.0, false)
        );
        // The record at 50 is behind the floor (evicted) but still inside a
        // chunk: the carry, not the physical record, is the predecessor.
        let carry = record(50, 80, Some(5.0));
        assert_eq!(
            steps(&log_both, Some(carry), 60, 60, u64::MAX),
            vec![(50, 70, 5.0, false), (70, 100, 6.0, true)]
        );
        let log_after = log(&[record(70, 100, Some(6.0))]);
        let expiring_carry = record(45, 65, Some(9.0));
        assert_eq!(
            steps(&log_after, Some(expiring_carry), 60, 60, u64::MAX),
            vec![(45, 65, 9.0, true), (70, 100, 6.0, true)]
        );
    }
}
