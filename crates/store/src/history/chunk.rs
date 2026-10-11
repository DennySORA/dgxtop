//! Append-only logs of fixed-width records split into immutable chunks.
//!
//! The writer appends into one open chunk; full chunks are sealed into `Arc`s
//! that readers pin without copying. Each sealed chunk owns the byte permit
//! taken before its records were allocated, so a chunk evicted while a slow
//! query still pins it stays accounted until the last `Arc` is dropped.

use std::collections::VecDeque;
use std::sync::Arc;

use dgxtop_core::budget::{ByteLedger, BytePermit, CapacityError, Partition};

use super::records::TimeSpan;
use dgxtop_core::budget::accounted::bytes_of;

/// Records per chunk.
pub(crate) const CHUNK_RECORDS: usize = 64;
/// Arc header, boxed-slice pointer, permit and spine slot of one chunk.
const CHUNK_OVERHEAD_BYTES: u64 = 128;

/// Bytes reserved for one chunk of `T`.
pub(crate) fn chunk_bytes<T>() -> u64 {
    bytes_of::<T>(CHUNK_RECORDS).saturating_add(CHUNK_OVERHEAD_BYTES)
}

/// Immutable, shareable run of records with its own byte permit.
pub(crate) struct SealedChunk<T> {
    records: Box<[T]>,
    _permit: Option<BytePermit>,
}

impl<T> SealedChunk<T> {
    pub(crate) fn records(&self) -> &[T] {
        &self.records
    }
}

/// Writer-side append-only log.
pub(crate) struct ChunkLog<T> {
    sealed: VecDeque<Arc<SealedChunk<T>>>,
    open: Vec<T>,
    open_permit: Option<BytePermit>,
    ledger: ByteLedger,
    partition: Partition,
}

impl<T: TimeSpan> ChunkLog<T> {
    pub(crate) fn new(ledger: &ByteLedger, partition: Partition) -> Self {
        Self {
            sealed: VecDeque::new(),
            open: Vec::new(),
            open_permit: None,
            ledger: ledger.clone(),
            partition,
        }
    }

    /// Bytes that pushing `count` more records has to reserve.
    pub(crate) fn cost_of_pushes(&self, count: usize) -> u64 {
        let free = self.open.capacity().saturating_sub(self.open.len());
        if count <= free {
            return 0;
        }
        let chunks = (count - free).div_ceil(CHUNK_RECORDS);
        chunk_bytes::<T>().saturating_mul(u64::try_from(chunks).unwrap_or(u64::MAX))
    }

    /// Appends one record, reserving a new chunk first when the open one is
    /// full. On failure the log is unchanged.
    pub(crate) fn try_push(&mut self, record: T) -> Result<(), CapacityError> {
        if self.open.len() >= self.open.capacity() {
            let permit = self
                .ledger
                .try_reserve(self.partition, chunk_bytes::<T>())?;
            self.seal_open();
            self.open = Vec::with_capacity(CHUNK_RECORDS);
            self.open_permit = Some(permit);
        }
        self.open.push(record);
        Ok(())
    }

    fn seal_open(&mut self) {
        let records = std::mem::take(&mut self.open);
        let permit = self.open_permit.take();
        if records.is_empty() {
            return;
        }
        self.sealed.push_back(Arc::new(SealedChunk {
            records: records.into_boxed_slice(),
            _permit: permit,
        }));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.open.is_empty() && self.sealed.is_empty()
    }

    /// Oldest physically retained record.
    pub(crate) fn first(&self) -> Option<&T> {
        self.sealed
            .front()
            .and_then(|chunk| chunk.records.first())
            .or_else(|| self.open.first())
    }

    /// Oldest physically retained chunk (sealed, else the open one).
    pub(crate) fn oldest_chunk(&self) -> Option<&[T]> {
        match self.sealed.front() {
            Some(chunk) => Some(chunk.records()),
            None if !self.open.is_empty() => Some(&self.open),
            None => None,
        }
    }

    /// Drops chunks from the front while `behind(last record of chunk)`
    /// holds; the open chunk is released too once every record is behind.
    /// Returns the number of records dropped.
    pub(crate) fn drop_front_while(&mut self, behind: impl Fn(&T) -> bool) -> usize {
        let mut dropped = 0usize;
        while let Some(chunk) = self.sealed.front() {
            let all_behind = chunk.records.last().is_some_and(&behind);
            if !all_behind {
                return dropped;
            }
            dropped = dropped.saturating_add(chunk.records.len());
            self.sealed.pop_front();
        }
        if self.open.last().is_some_and(&behind) {
            dropped = dropped.saturating_add(self.open.len());
            self.open = Vec::new();
            self.open_permit = None;
        }
        dropped
    }

    /// Ordered view over every retained record.
    pub(crate) fn slices(&self) -> Slices<'_, T> {
        Slices::new(
            self.sealed
                .iter()
                .map(|chunk| chunk.records())
                .chain(std::iter::once(&self.open[..])),
        )
    }

    /// Pins the chunks overlapping `[from_ns, to_ns)` plus one neighbour on
    /// each side (predecessor and successor records). The open chunk is
    /// copied only when the pinned run reaches it, so the view never has a
    /// hole.
    pub(crate) fn pin_around(&self, from_ns: u64, to_ns: u64) -> LogPin<T> {
        let count = self.sealed.len();
        let first_overlap = self
            .sealed
            .iter()
            .position(|chunk| chunk.records.last().is_some_and(|r| r.end_ns() >= from_ns))
            .unwrap_or(count);
        let start = first_overlap.saturating_sub(1);
        let end = self
            .sealed
            .iter()
            .rposition(|chunk| chunk.records.first().is_some_and(|r| r.start_ns() < to_ns))
            .map_or(start, |last| last.saturating_add(2).min(count))
            .max(start.saturating_add(1).min(count));
        let chunks = self.sealed.range(start..end).cloned().collect();
        let tail = if end >= count {
            self.open.clone()
        } else {
            Vec::new()
        };
        LogPin { chunks, tail }
    }

    /// Bytes of the open chunk a pin would copy.
    pub(crate) fn open_bytes(&self) -> u64 {
        bytes_of::<T>(self.open.len())
    }
}

/// Reader-side pinned run of chunks plus a copy of the open chunk.
pub(crate) struct LogPin<T> {
    chunks: Vec<Arc<SealedChunk<T>>>,
    tail: Vec<T>,
}

impl<T: TimeSpan> LogPin<T> {
    pub(crate) fn slices(&self) -> Slices<'_, T> {
        Slices::new(
            self.chunks
                .iter()
                .map(|chunk| chunk.records())
                .chain(std::iter::once(&self.tail[..])),
        )
    }
}

/// Position of one record inside [`Slices`]; ordered like the records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Position {
    part: usize,
    index: usize,
}

/// Ordered list of record slices (sealed chunks then the open chunk).
pub(crate) struct Slices<'a, T> {
    parts: Vec<&'a [T]>,
}

impl<'a, T: TimeSpan> Slices<'a, T> {
    fn new(parts: impl Iterator<Item = &'a [T]>) -> Self {
        Self {
            parts: parts.filter(|part| !part.is_empty()).collect(),
        }
    }

    /// First position whose record does not satisfy `before` (which must be
    /// monotonic: true for a prefix of the records, false afterwards).
    fn seek_by(&self, before: impl Fn(&T) -> bool) -> Position {
        for (part, records) in self.parts.iter().enumerate() {
            if records.last().is_some_and(|last| !before(last)) {
                let index = records.partition_point(|record| before(record));
                return Position { part, index };
            }
        }
        self.end()
    }

    fn end(&self) -> Position {
        Position {
            part: self.parts.len(),
            index: 0,
        }
    }

    /// First record starting at or after `time_ns`.
    pub(crate) fn seek_start(&self, time_ns: u64) -> Position {
        self.seek_by(|record| record.start_ns() < time_ns)
    }

    /// First record ending after `time_ns`.
    pub(crate) fn seek_end_after(&self, time_ns: u64) -> Position {
        self.seek_by(|record| record.end_ns() <= time_ns)
    }

    pub(crate) fn get(&self, position: Position) -> Option<&'a T> {
        self.parts
            .get(position.part)
            .and_then(|records| records.get(position.index))
    }

    pub(crate) fn previous(&self, position: Position) -> Option<Position> {
        if position.index > 0 {
            return Some(Position {
                part: position.part,
                index: position.index - 1,
            });
        }
        let part = position.part.checked_sub(1)?;
        let records = self.parts.get(part)?;
        let index = records.len().checked_sub(1)?;
        Some(Position { part, index })
    }

    pub(crate) fn iter_from(&self, position: Position) -> SliceIter<'_, 'a, T> {
        SliceIter {
            parts: &self.parts,
            part: position.part,
            index: position.index,
        }
    }

    /// Number of records between two positions (`from <= to`).
    pub(crate) fn distance(&self, from: Position, to: Position) -> usize {
        if to <= from {
            return 0;
        }
        if from.part == to.part {
            return to.index - from.index;
        }
        let head = self
            .parts
            .get(from.part)
            .map_or(0, |records| records.len().saturating_sub(from.index));
        let middle: usize = self
            .parts
            .iter()
            .take(to.part)
            .skip(from.part.saturating_add(1))
            .map(|records| records.len())
            .sum();
        head.saturating_add(middle).saturating_add(to.index)
    }

    /// Records starting inside `[from_ns, to_ns)`.
    pub(crate) fn count_starting_in(&self, from_ns: u64, to_ns: u64) -> usize {
        self.distance(self.seek_start(from_ns), self.seek_start(to_ns))
    }

    /// Records ending inside `(from_ns, to_ns]`.
    pub(crate) fn count_ending_in(&self, from_ns: u64, to_ns: u64) -> usize {
        self.distance(self.seek_end_after(from_ns), self.seek_end_after(to_ns))
    }

    /// Newest record starting strictly before `time_ns`.
    pub(crate) fn last_starting_before(&self, time_ns: u64) -> Option<&'a T> {
        self.previous(self.seek_start(time_ns))
            .and_then(|position| self.get(position))
    }
}

/// Iterator over [`Slices`] from a position.
pub(crate) struct SliceIter<'s, 'a, T> {
    parts: &'s [&'a [T]],
    part: usize,
    index: usize,
}

impl<'a, T> Iterator for SliceIter<'_, 'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        while let Some(records) = self.parts.get(self.part) {
            if let Some(record) = records.get(self.index) {
                self.index += 1;
                return Some(record);
            }
            self.part += 1;
            self.index = 0;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::records::GaugeSample;

    fn sample(time_ns: u64) -> GaugeSample {
        GaugeSample {
            time_ns,
            stale_deadline_ns: time_ns + 10,
            value: None,
            segment: 0,
        }
    }

    fn log_with(count: u64, ledger: &ByteLedger) -> ChunkLog<GaugeSample> {
        let mut log = ChunkLog::new(ledger, Partition::RawHistory);
        for time in 0..count {
            log.try_push(sample(time * 10)).unwrap();
        }
        log
    }

    #[test]
    fn push_reserves_whole_chunks_and_seals() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let log = log_with(130, &ledger);
        assert_eq!(log.sealed.len(), 2);
        assert_eq!(log.open.len(), 2);
        assert_eq!(
            ledger.used(Partition::RawHistory),
            3 * chunk_bytes::<GaugeSample>()
        );
        assert_eq!(log.cost_of_pushes(62), 0);
        assert_eq!(log.cost_of_pushes(63), chunk_bytes::<GaugeSample>());
        drop(log);
        assert_eq!(ledger.used(Partition::RawHistory), 0);
    }

    #[test]
    fn failed_reservation_leaves_log_unchanged() {
        let ledger = ByteLedger::new(dgxtop_core::budget::PartitionLimits::for_memory_limit_mib(
            0,
        ));
        let mut log = ChunkLog::<GaugeSample>::new(&ledger, Partition::RawHistory);
        assert!(log.try_push(sample(1)).is_err());
        assert!(log.is_empty());
    }

    #[test]
    fn evicted_chunk_stays_accounted_while_pinned() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut log = log_with(200, &ledger);
        let before = ledger.used(Partition::RawHistory);
        let pin = log.pin_around(0, 100);
        let dropped = log.drop_front_while(|last| last.time_ns < 700);
        assert_eq!(dropped, 64);
        assert_eq!(ledger.used(Partition::RawHistory), before);
        drop(pin);
        assert_eq!(
            ledger.used(Partition::RawHistory),
            before - chunk_bytes::<GaugeSample>()
        );
    }

    #[test]
    fn slices_seek_count_and_neighbours() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let log = log_with(150, &ledger);
        let slices = log.slices();
        let position = slices.seek_start(645);
        assert_eq!(slices.get(position).unwrap().time_ns, 650);
        assert_eq!(slices.last_starting_before(645).unwrap().time_ns, 640);
        assert_eq!(slices.last_starting_before(0), None);
        assert_eq!(slices.count_starting_in(0, 1500), 150);
        assert_eq!(slices.count_starting_in(630, 1290), 66);
        assert_eq!(slices.count_ending_in(0, 1490), 149);
        assert!(slices.get(slices.seek_start(10_000)).is_none());
        let collected: Vec<u64> = slices
            .iter_from(slices.seek_start(1460))
            .map(|record| record.time_ns)
            .collect();
        assert_eq!(collected, vec![1460, 1470, 1480, 1490]);
    }

    #[test]
    fn pin_includes_neighbours_and_never_has_holes() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let log = log_with(300, &ledger);
        let pin = log.pin_around(700, 710);
        let slices = pin.slices();
        // Window inside chunk 1 (640..1270): chunks 0..=2 pinned, no tail.
        assert_eq!(slices.get(slices.seek_start(0)).unwrap().time_ns, 0);
        assert!(slices.last_starting_before(10_000).unwrap().time_ns < 2000);
        let recent = log.pin_around(2950, 3000);
        let slices = recent.slices();
        assert_eq!(slices.last_starting_before(u64::MAX).unwrap().time_ns, 2990);
        assert_eq!(slices.last_starting_before(2950).unwrap().time_ns, 2940);
    }
}
