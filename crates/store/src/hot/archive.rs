//! Bounded queue of finished history rows for the optional archive.
//!
//! Rows (finalized minute buckets, bridges and settled gaps) are queued only
//! when the archive is enabled, accounted in the `reserve` partition (the
//! archive journal's budget) and handed out as bounded `ArchiveJournal`s.
//! Every journal carries the segment row of every segment its buckets and
//! bridges reference (the sink upserts them), so a journal never depends on
//! an earlier one. When the queue is full the oldest rows are dropped and the
//! dropped time is reported to the archive as an `archive_overflow` gap: the
//! HotStore is never blocked by the archive.

use std::collections::{BTreeSet, VecDeque};

use dgxtop_core::budget::{ByteLedger, Partition};
use dgxtop_core::canonical::GapRecord;
use dgxtop_core::history::HistoryMethod;
use dgxtop_core::identity::{BootId, HostId};
use dgxtop_core::metric::ScalarView;
use dgxtop_core::ports::{ArchiveJournal, ArchivedBridge, ArchivedBucket, ArchivedSegment};

use super::wall_clock::{WallClock, format_utc};
use crate::history::accounted::Account;
use crate::history::output::ArchiveRow;
use crate::history::records::BUCKET_NS;
use crate::history::segment::SegmentMeta;

/// Most bytes queued for the archive (`queues.archive_bytes` upper bound).
pub(crate) const MAX_QUEUED_BYTES: u64 = 8 * 1024 * 1024;
/// Accounted bytes of one queued row besides its strings.
const ROW_BYTES: u64 = 256;
/// Accounted bytes of one segment row added to a journal.
const SEGMENT_ROW_BYTES: u64 = 384;

struct QueuedRow {
    row: ArchiveRow,
    bytes: u64,
}

fn row_bytes(row: &ArchiveRow) -> u64 {
    match row {
        ArchiveRow::Gap(gap) => {
            ROW_BYTES.saturating_add(u64::try_from(gap.reason.len()).unwrap_or(u64::MAX))
        }
        ArchiveRow::GaugeBucket { .. }
        | ArchiveRow::CounterBucket { .. }
        | ArchiveRow::Bridge { .. } => ROW_BYTES,
    }
}

fn row_identity(row: &ArchiveRow) -> Option<(&HostId, &BootId)> {
    match row {
        ArchiveRow::GaugeBucket { segment, .. }
        | ArchiveRow::CounterBucket { segment, .. }
        | ArchiveRow::Bridge { segment, .. } => Some((&segment.key.host_id, &segment.key.boot_id)),
        ArchiveRow::Gap(gap) => gap.key.as_ref().map(|key| (&key.host_id, &key.boot_id)),
    }
}

/// Pending archive rows.
pub(crate) struct ArchiveQueue {
    rows: VecDeque<QueuedRow>,
    account: Account,
    /// Time range of rows dropped since the last journal.
    overflow: Option<(u64, u64)>,
}

fn row_span(row: &ArchiveRow) -> (u64, u64) {
    match row {
        ArchiveRow::GaugeBucket { bucket, .. } => {
            (bucket.start_ns, bucket.start_ns.saturating_add(BUCKET_NS))
        }
        ArchiveRow::CounterBucket { bucket, .. } => {
            (bucket.start_ns, bucket.start_ns.saturating_add(BUCKET_NS))
        }
        ArchiveRow::Bridge { bridge, .. } => (bridge.start_ns, bridge.end_ns),
        ArchiveRow::Gap(gap) => (gap.start_boot_ns, gap.end_boot_ns),
    }
}

/// Journal content collected from the queue front.
struct JournalRows {
    segment_ids: BTreeSet<uuid::Uuid>,
    segments: Vec<ArchivedSegment>,
    buckets: Vec<ArchivedBucket>,
    bridges: Vec<ArchivedBridge>,
    gaps: Vec<GapRecord>,
    bytes: u64,
}

impl ArchiveQueue {
    pub(crate) fn new(ledger: &ByteLedger) -> Self {
        Self {
            rows: VecDeque::new(),
            account: Account::new(ledger, Partition::Reserve),
            overflow: None,
        }
    }

    fn note_overflow(&mut self, (start, end): (u64, u64)) {
        self.overflow = Some(match self.overflow {
            Some((first, last)) => (first.min(start), last.max(end)),
            None => (start, end),
        });
    }

    fn drop_oldest(&mut self) -> bool {
        let Some(oldest) = self.rows.pop_front() else {
            return false;
        };
        self.account
            .shrink_to(self.account.bytes().saturating_sub(oldest.bytes));
        self.note_overflow(row_span(&oldest.row));
        true
    }

    /// Queues rows; returns how many rows were dropped to stay bounded.
    pub(crate) fn enqueue(&mut self, rows: Vec<ArchiveRow>) -> u64 {
        let mut dropped = 0u64;
        for row in rows {
            let bytes = row_bytes(&row);
            loop {
                let fits = self.account.bytes().saturating_add(bytes) <= MAX_QUEUED_BYTES;
                if fits && self.account.try_grow(bytes).is_ok() {
                    self.rows.push_back(QueuedRow { row, bytes });
                    break;
                }
                if !self.drop_oldest() {
                    self.note_overflow(row_span(&row));
                    dropped = dropped.saturating_add(1);
                    break;
                }
                dropped = dropped.saturating_add(1);
            }
        }
        dropped
    }

    /// Takes up to `max_bytes` of rows (at least one) of a single host/boot.
    /// Returns `None` when nothing is queued or no boot-to-UTC mapping is
    /// known yet (rows stay queued).
    pub(crate) fn take(
        &mut self,
        max_bytes: u64,
        clock: &WallClock,
        revision: u64,
    ) -> Option<ArchiveJournal> {
        if !clock.is_known() || (self.rows.is_empty() && self.overflow.is_none()) {
            return None;
        }
        let (host_id, boot_id) = self
            .rows
            .iter()
            .find_map(|queued| row_identity(&queued.row))
            .map(|(host, boot)| (host.to_string(), boot.to_string()))
            .unwrap_or_default();
        let mut collected = JournalRows {
            segment_ids: BTreeSet::new(),
            segments: Vec::new(),
            buckets: Vec::new(),
            bridges: Vec::new(),
            gaps: Vec::new(),
            bytes: 0,
        };
        let mut taken_bytes = 0u64;
        while let Some(front) = self.rows.front() {
            let same_boot = row_identity(&front.row)
                .is_none_or(|(host, boot)| host.as_str() == host_id && boot.as_str() == boot_id);
            let first = collected.bytes == 0;
            if !same_boot || (!first && collected.bytes.saturating_add(front.bytes) > max_bytes) {
                break;
            }
            let Some(queued) = self.rows.pop_front() else {
                break;
            };
            taken_bytes = taken_bytes.saturating_add(queued.bytes);
            collect_row(queued, clock, &mut collected);
        }
        if let Some((start, end)) = self.overflow.take() {
            collected.gaps.push(GapRecord {
                key: None,
                adapter_id: None,
                start_boot_ns: start,
                end_boot_ns: end.max(start.saturating_add(1)),
                reason: "archive_overflow".to_owned(),
                affected_count: 1,
            });
        }
        let permit = self
            .account
            .split_off(taken_bytes.min(self.account.bytes()));
        Some(ArchiveJournal {
            revision,
            host_id,
            boot_id,
            segments: collected.segments,
            buckets: collected.buckets,
            bridges: collected.bridges,
            gaps: collected.gaps,
            permit,
        })
    }
}

fn utc_text(clock: &WallClock, boot_ns: u64) -> String {
    clock
        .utc_of(boot_ns)
        .and_then(format_utc)
        .unwrap_or_default()
}

/// Adds the segment row the first time this journal references a segment.
fn ensure_segment(segment: &SegmentMeta, clock: &WallClock, collected: &mut JournalRows) {
    if !collected.segment_ids.insert(segment.uuid) {
        return;
    }
    collected.bytes = collected.bytes.saturating_add(SEGMENT_ROW_BYTES);
    collected.segments.push(ArchivedSegment {
        segment_id: segment.uuid.to_string(),
        key: segment.key.clone(),
        source_id: segment.identity.source_id.clone(),
        source_generation: segment.identity.source_generation,
        policy_revision: segment.identity.policy_revision,
        unit: segment.unit,
        sample_kind: segment.identity.sample_kind,
        started_utc: utc_text(clock, segment.started_ns),
    });
}

fn collect_row(queued: QueuedRow, clock: &WallClock, collected: &mut JournalRows) {
    collected.bytes = collected.bytes.saturating_add(queued.bytes);
    match queued.row {
        ArchiveRow::GaugeBucket { segment, bucket } => {
            ensure_segment(&segment, clock, collected);
            let end = bucket.start_ns.saturating_add(BUCKET_NS);
            let covered = bucket.covered_ns > 0;
            collected.buckets.push(ArchivedBucket {
                segment_id: segment.uuid.to_string(),
                start_boot_ns: bucket.start_ns,
                end_boot_ns: end,
                start_utc: utc_text(clock, bucket.start_ns),
                end_utc: utc_text(clock, end),
                covered_ns: bucket.covered_ns,
                weighted_sum: covered.then_some(bucket.weighted_sum),
                minimum: covered.then_some(bucket.minimum),
                maximum: covered.then_some(bucket.maximum),
                counter_delta: None,
                sample_count: bucket.sample_count,
                method: if covered {
                    HistoryMethod::TimeWeightedStep
                } else {
                    HistoryMethod::Gap
                },
                reason_code: None,
            });
        }
        ArchiveRow::CounterBucket { segment, bucket } => {
            ensure_segment(&segment, clock, collected);
            let end = bucket.start_ns.saturating_add(BUCKET_NS);
            let exact = bucket.total.integral();
            collected.buckets.push(ArchivedBucket {
                segment_id: segment.uuid.to_string(),
                start_boot_ns: bucket.start_ns,
                end_boot_ns: end,
                start_utc: utc_text(clock, bucket.start_ns),
                end_utc: utc_text(clock, end),
                covered_ns: bucket.covered_ns,
                weighted_sum: None,
                minimum: None,
                maximum: None,
                counter_delta: exact,
                sample_count: bucket.interval_count,
                method: HistoryMethod::CounterDelta,
                reason_code: exact
                    .is_none()
                    .then(|| "non_integral_counter_delta".to_owned()),
            });
        }
        ArchiveRow::Bridge { segment, bridge } => {
            // Bridges are archived only when integral: the archive column is
            // an exact decimal and a float delta must not pose as exact.
            if let ScalarView::U64(delta) = bridge.delta.view() {
                ensure_segment(&segment, clock, collected);
                collected.bridges.push(ArchivedBridge {
                    segment_id: segment.uuid.to_string(),
                    start_boot_ns: bridge.start_ns,
                    end_boot_ns: bridge.end_ns,
                    delta: u128::from(delta),
                });
            } else {
                collected.gaps.push(GapRecord {
                    key: Some(segment.key.clone()),
                    adapter_id: None,
                    start_boot_ns: bridge.start_ns,
                    end_boot_ns: bridge.end_ns,
                    reason: "non_integral_bridge".to_owned(),
                    affected_count: 1,
                });
            }
        }
        ArchiveRow::Gap(gap) => collected.gaps.push(gap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use dgxtop_core::metric::{CanonicalUnit, Scalar};
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::{metric_key, source_id};
    use uuid::Uuid;

    use crate::history::records::{CounterSample, GaugeBucket};
    use crate::history::segment::SegmentIdentity;

    fn segment() -> Arc<SegmentMeta> {
        Arc::new(SegmentMeta {
            uuid: Uuid::new_v4(),
            key: metric_key("gpu:0", "gpu.temperature", "gpu"),
            identity: SegmentIdentity {
                source_id: source_id("nvml:0:temp"),
                source_generation: 1,
                policy_revision: 1,
                sample_kind: SampleKind::Measured,
                method: HistoryMethod::TimeWeightedStep,
            },
            unit: CanonicalUnit::DegC,
            started_ns: 0,
            opens_series: true,
        })
    }

    fn bucket_row(segment: &Arc<SegmentMeta>, minute: u64) -> ArchiveRow {
        let mut bucket = GaugeBucket::empty(minute * BUCKET_NS, 0);
        bucket.add_step(50.0, BUCKET_NS);
        ArchiveRow::GaugeBucket {
            segment: Arc::clone(segment),
            bucket,
        }
    }

    fn clock() -> WallClock {
        let mut clock = WallClock::default();
        clock.observe(0, 1_790_000_000_000_000_000);
        clock
    }

    #[test]
    fn every_journal_carries_its_segments_and_is_accounted() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut queue = ArchiveQueue::new(&ledger);
        let segment = segment();
        assert_eq!(
            queue.enqueue(vec![bucket_row(&segment, 0), bucket_row(&segment, 1)]),
            0
        );
        assert!(
            queue.take(1 << 20, &WallClock::default(), 1).is_none(),
            "no UTC mapping yet"
        );
        let journal = queue.take(1 << 20, &clock(), 9).unwrap();
        assert_eq!(journal.revision, 9);
        assert_eq!(journal.segments.len(), 1);
        assert_eq!(journal.buckets.len(), 2);
        assert_eq!(
            journal.buckets[0].start_utc,
            "2026-09-21T14:13:20.000000000Z"
        );
        assert_eq!(journal.host_id, "test-host");
        assert!(
            journal
                .permit
                .as_ref()
                .is_some_and(|permit| permit.bytes() == 2 * ROW_BYTES)
        );
        assert!(queue.take(1 << 20, &clock(), 10).is_none());
        drop(journal);
        assert_eq!(ledger.used(Partition::Reserve), 0);
        queue.enqueue(vec![bucket_row(&segment, 2)]);
        let later = queue.take(1 << 20, &clock(), 11).unwrap();
        assert_eq!(
            later.segments.len(),
            1,
            "a later journal repeats the segment row"
        );
        assert_eq!(later.segments[0].segment_id, later.buckets[0].segment_id);
    }

    #[test]
    fn take_is_bounded_and_float_bridges_are_not_posed_as_exact() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut queue = ArchiveQueue::new(&ledger);
        let segment = segment();
        let bridge = |delta: Scalar| ArchiveRow::Bridge {
            segment: Arc::clone(&segment),
            bridge: CounterSample {
                start_ns: 59_500_000_000,
                end_ns: 60_500_000_000,
                delta,
                segment: 0,
            },
        };
        queue.enqueue(vec![
            bucket_row(&segment, 0),
            bridge(Scalar::from_u64(300)),
            bridge(Scalar::from_f64(1.5).unwrap()),
        ]);
        let first = queue.take(1, &clock(), 1).unwrap();
        assert_eq!(first.buckets.len(), 1);
        assert!(first.bridges.is_empty());
        let rest = queue.take(1 << 20, &clock(), 2).unwrap();
        assert_eq!(rest.bridges.len(), 1);
        assert_eq!(rest.bridges[0].delta, 300);
        assert_eq!(rest.gaps.len(), 1);
        assert_eq!(rest.gaps[0].reason, "non_integral_bridge");
    }

    #[test]
    fn full_queue_drops_oldest_and_reports_overflow() {
        let ledger = ByteLedger::new(dgxtop_core::budget::PartitionLimits::for_memory_limit_mib(
            1,
        ));
        let mut queue = ArchiveQueue::new(&ledger);
        let segment = segment();
        let rows: Vec<ArchiveRow> = (0..10_000)
            .map(|minute| bucket_row(&segment, minute))
            .collect();
        let dropped = queue.enqueue(rows);
        assert!(dropped > 0);
        assert!(ledger.used(Partition::Reserve) <= ledger.limits().limit(Partition::Reserve));
        let journal = queue.take(u64::MAX, &clock(), 1).unwrap();
        assert!(
            journal
                .gaps
                .iter()
                .any(|gap| gap.reason == "archive_overflow")
        );
    }
}
