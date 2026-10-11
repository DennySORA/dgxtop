//! Bounded queue of finished history rows for the optional archive.
//!
//! Rows (finalized minute buckets, bridges and settled gaps) are queued only
//! when the archive is enabled, and only those read back: rows of the
//! series the Overview keeps in its sizes, and gaps no series is named in
//! (D-82; a row of any other series would only wait for its age limit).
//! They are accounted in the `reserve` partition (the
//! archive journal's budget) and handed out as bounded `ArchiveJournal`s.
//! Every journal carries the segment row of every segment its buckets and
//! bridges reference (the sink upserts them), so a journal never depends on
//! an earlier one. When the queue is full the oldest rows are dropped and the
//! dropped time is reported to the archive as an `archive_overflow` gap: the
//! live state is never blocked by the archive. A journal also carries the
//! deletions the Overview's sizes allow (D-82), alone when no row waits;
//! a queued row such a deletion covers is dropped before it is written.

use std::collections::{BTreeSet, VecDeque};

use dgxtop_core::budget::{ByteLedger, Partition};
use dgxtop_core::canonical::GapRecord;
use dgxtop_core::history::{HistoryMethod, SeriesInEveryBoot};
use dgxtop_core::identity::{BootId, HostId};
use dgxtop_core::metric::ScalarView;
use dgxtop_core::overview_history::{ArchivePrune, is_overview_series};
use dgxtop_core::ports::{ArchiveJournal, ArchivedBridge, ArchivedBucket, ArchivedSegment};

use dgxtop_core::clock::format_utc_ns;

use crate::output::ArchiveRow;
use crate::records::BUCKET_NS;
use crate::segment::SegmentMeta;
use crate::wall_clock::WallClock;
use dgxtop_core::budget::accounted::Account;

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

/// Whether the archive keeps a row: one of an Overview series (the history
/// compresses those minutes at startup) or a gap no series is named in.
fn is_read_back(row: &ArchiveRow) -> bool {
    let key = match row {
        ArchiveRow::GaugeBucket { segment, .. }
        | ArchiveRow::CounterBucket { segment, .. }
        | ArchiveRow::Bridge { segment, .. } => &segment.key,
        ArchiveRow::Gap(gap) => match &gap.key {
            Some(key) => key,
            None => return true,
        },
    };
    is_overview_series(key.metric_id.as_str(), key.entity_id.as_str())
}

/// Whether `prune` covers a queued row: one of its series that starts
/// before the prune (an unmapped boot time is not covered).
fn is_pruned(row: &ArchiveRow, prune: &ArchivePrune, clock: &WallClock) -> bool {
    let key = match row {
        ArchiveRow::GaugeBucket { segment, .. }
        | ArchiveRow::CounterBucket { segment, .. }
        | ArchiveRow::Bridge { segment, .. } => &segment.key,
        ArchiveRow::Gap(gap) => match &gap.key {
            Some(key) => key,
            None => return false,
        },
    };
    if SeriesInEveryBoot::from(key) != prune.series {
        return false;
    }
    clock
        .utc_of(row_span(row).0)
        .is_some_and(|start| start < i128::from(prune.before_utc_ns))
}

/// Pending archive rows.
pub(crate) struct ArchiveQueue {
    rows: VecDeque<QueuedRow>,
    account: Account,
    /// Time range of rows dropped since the last journal.
    overflow: Option<(u64, u64)>,
    /// Host and boot of the latest row queued: a journal of deletions
    /// alone is stamped with them.
    identity: Option<(String, String)>,
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
            identity: None,
        }
    }

    /// Whether a host and boot are known to stamp a journal with.
    pub(crate) fn knows_identity(&self) -> bool {
        self.identity.is_some()
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

    /// Queues the rows the archive keeps; returns how many rows were
    /// dropped to stay bounded.
    pub(crate) fn enqueue(&mut self, rows: Vec<ArchiveRow>) -> u64 {
        let mut dropped = 0u64;
        for row in rows {
            if let Some((host, boot)) = row_identity(&row) {
                self.identity = Some((host.to_string(), boot.to_string()));
            }
            if !is_read_back(&row) {
                continue;
            }
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

    /// Drops the queued rows `prunes` would delete once written: a pruned
    /// series' rows that start before its prune, on the wall clock.
    pub(crate) fn drop_pruned(&mut self, prunes: &[ArchivePrune], clock: &WallClock) {
        if prunes.is_empty() {
            return;
        }
        let before = self.account.bytes();
        let mut released = 0u64;
        self.rows.retain(|queued| {
            let pruned = prunes
                .iter()
                .any(|prune| is_pruned(&queued.row, prune, clock));
            if pruned {
                released = released.saturating_add(queued.bytes);
            }
            !pruned
        });
        self.account.shrink_to(before.saturating_sub(released));
    }

    /// Takes up to `max_bytes` of rows (at least one) of a single host/boot,
    /// with `prunes`. Returns `None` when nothing is queued or no
    /// boot-to-UTC mapping is known yet (rows stay queued).
    pub(crate) fn take(
        &mut self,
        max_bytes: u64,
        clock: &WallClock,
        revision: u64,
        prunes: Vec<ArchivePrune>,
    ) -> Option<ArchiveJournal> {
        let nothing = self.rows.is_empty() && self.overflow.is_none() && prunes.is_empty();
        if !clock.is_known() || nothing {
            return None;
        }
        let (host_id, boot_id) = self
            .rows
            .iter()
            .find_map(|queued| row_identity(&queued.row))
            .map(|(host, boot)| (host.to_string(), boot.to_string()))
            .or_else(|| self.identity.clone())
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
            prunes,
            permit,
        })
    }
}

fn utc_text(clock: &WallClock, boot_ns: u64) -> String {
    clock
        .utc_of(boot_ns)
        .and_then(format_utc_ns)
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

    use dgxtop_core::metric::{CanonicalUnit, Scalar, ids};
    use dgxtop_core::naming::entity;
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::{metric_key, source_id};
    use uuid::Uuid;

    use crate::records::{CounterSample, GaugeBucket};
    use crate::segment::SegmentIdentity;

    fn segment() -> Arc<SegmentMeta> {
        segment_of("gpu:0", "gpu.temperature")
    }

    fn segment_of(entity: &str, metric: &str) -> Arc<SegmentMeta> {
        Arc::new(SegmentMeta {
            uuid: Uuid::new_v4(),
            key: metric_key(entity, metric, "gpu"),
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
            queue
                .take(1 << 20, &WallClock::default(), 1, Vec::new())
                .is_none(),
            "no UTC mapping yet"
        );
        let journal = queue.take(1 << 20, &clock(), 9, Vec::new()).unwrap();
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
        assert!(queue.take(1 << 20, &clock(), 10, Vec::new()).is_none());
        drop(journal);
        assert_eq!(ledger.used(Partition::Reserve), 0);
        queue.enqueue(vec![bucket_row(&segment, 2)]);
        let later = queue.take(1 << 20, &clock(), 11, Vec::new()).unwrap();
        assert_eq!(
            later.segments.len(),
            1,
            "a later journal repeats the segment row"
        );
        assert_eq!(later.segments[0].segment_id, later.buckets[0].segment_id);
    }

    fn gap(key: Option<dgxtop_core::identity::MetricKey>) -> ArchiveRow {
        ArchiveRow::Gap(GapRecord {
            key,
            adapter_id: None,
            start_boot_ns: 0,
            end_boot_ns: BUCKET_NS,
            reason: "collector_down".to_owned(),
            affected_count: 1,
        })
    }

    /// Only what is read back is archived (D-82): an Overview series'
    /// minutes and bridges, gaps keyed to one and gaps keyed to none; a
    /// CPU core or a metric the Overview does not chart is left out.
    #[test]
    fn only_the_overview_series_are_queued() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut queue = ArchiveQueue::new(&ledger);
        let kept = segment();
        let core = segment_of("cpu:3", ids::CPU_UTILIZATION_TOTAL);
        let whole_cpu = segment_of(entity::CPU_AGGREGATE, ids::CPU_UTILIZATION_TOTAL);
        let uncharted = segment_of(entity::CPU_AGGREGATE, ids::CPU_UTILIZATION_IOWAIT);
        let bridge = |segment: &Arc<SegmentMeta>| ArchiveRow::Bridge {
            segment: Arc::clone(segment),
            bridge: CounterSample {
                start_ns: 59_500_000_000,
                end_ns: 60_500_000_000,
                delta: Scalar::from_u64(300),
                segment: 0,
            },
        };
        let dropped = queue.enqueue(vec![
            bucket_row(&kept, 0),
            bucket_row(&core, 0),
            bucket_row(&whole_cpu, 0),
            bucket_row(&uncharted, 0),
            bridge(&kept),
            bridge(&core),
            gap(Some(kept.key.clone())),
            gap(Some(core.key.clone())),
            gap(None),
        ]);
        assert_eq!(dropped, 0, "left out is not dropped");
        assert_eq!(
            ledger.used(Partition::Reserve),
            2 * ROW_BYTES + ROW_BYTES + 2 * (ROW_BYTES + 14),
            "only the rows kept are accounted"
        );
        let journal = queue.take(1 << 20, &clock(), 1, Vec::new()).unwrap();
        let mut segments: Vec<&str> = journal
            .segments
            .iter()
            .map(|segment| segment.key.entity_id.as_str())
            .collect();
        segments.sort_unstable();
        assert_eq!(segments, vec![entity::CPU_AGGREGATE, "gpu:0"]);
        assert_eq!(journal.buckets.len(), 2);
        assert_eq!(journal.bridges.len(), 1);
        assert_eq!(journal.gaps.len(), 2);
        assert!(
            journal
                .gaps
                .iter()
                .all(|gap| gap.key.as_ref().is_none_or(|key| key == &kept.key))
        );
    }

    /// A queued row a prune covers is dropped before it is written (it
    /// would be deleted in the same step): the pruned series' rows before
    /// the prune go, its later rows and other series' rows stay.
    #[test]
    fn queued_rows_a_prune_covers_are_dropped() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut queue = ArchiveQueue::new(&ledger);
        let pruned = segment();
        let other = segment_of("gpu:1", "gpu.temperature");
        queue.enqueue(vec![
            bucket_row(&pruned, 0),
            bucket_row(&pruned, 1),
            gap(Some(pruned.key.clone())),
            bucket_row(&pruned, 2),
            bucket_row(&other, 0),
            gap(None),
        ]);
        let start_of_minute_2 = 1_790_000_000_000_000_000 + 2 * i64::try_from(BUCKET_NS).unwrap();
        let prune = ArchivePrune {
            series: dgxtop_core::history::SeriesInEveryBoot::from(&pruned.key),
            before_utc_ns: start_of_minute_2,
        };
        queue.drop_pruned(&[], &clock());
        assert_eq!(queue.rows.len(), 6, "no prune, nothing dropped");
        queue.drop_pruned(std::slice::from_ref(&prune), &WallClock::default());
        assert_eq!(queue.rows.len(), 6, "an unmapped boot time is kept");
        queue.drop_pruned(&[prune], &clock());
        assert_eq!(
            ledger.used(Partition::Reserve),
            2 * ROW_BYTES + (ROW_BYTES + 14),
            "what was dropped is released"
        );
        let journal = queue.take(1 << 20, &clock(), 1, Vec::new()).unwrap();
        let kept: Vec<(&str, u64)> = journal
            .buckets
            .iter()
            .map(|bucket| {
                let segment = journal
                    .segments
                    .iter()
                    .find(|segment| segment.segment_id == bucket.segment_id)
                    .unwrap();
                (
                    segment.key.entity_id.as_str(),
                    bucket.start_boot_ns / BUCKET_NS,
                )
            })
            .collect();
        assert_eq!(kept, vec![("gpu:0", 2), ("gpu:1", 0)]);
        assert_eq!(journal.gaps.len(), 1);
        assert!(journal.gaps[0].key.is_none());
    }

    /// Deletions the Overview's sizes allow travel in a journal of their
    /// own when no row waits, stamped with the latest host and boot (D-82).
    #[test]
    fn prunes_travel_alone_with_the_latest_identity() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut queue = ArchiveQueue::new(&ledger);
        let prune = ArchivePrune {
            series: dgxtop_core::history::SeriesInEveryBoot::from(&segment().key),
            before_utc_ns: 1_790_000_000_000_000_000,
        };
        assert!(!queue.knows_identity());
        assert!(queue.take(1 << 20, &clock(), 1, Vec::new()).is_none());
        queue.enqueue(vec![bucket_row(&segment(), 0)]);
        assert!(queue.knows_identity());
        let rows = queue.take(1 << 20, &clock(), 2, Vec::new()).unwrap();
        assert!(rows.prunes.is_empty());
        let alone = queue
            .take(1 << 20, &clock(), 3, vec![prune.clone()])
            .unwrap();
        assert_eq!(alone.prunes, vec![prune]);
        assert!(alone.buckets.is_empty());
        assert_eq!(
            (alone.host_id.as_str(), alone.boot_id.as_str()),
            ("test-host", "fake-boot")
        );
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
        let first = queue.take(1, &clock(), 1, Vec::new()).unwrap();
        assert_eq!(first.buckets.len(), 1);
        assert!(first.bridges.is_empty());
        let rest = queue.take(1 << 20, &clock(), 2, Vec::new()).unwrap();
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
        let journal = queue.take(u64::MAX, &clock(), 1, Vec::new()).unwrap();
        assert!(
            journal
                .gaps
                .iter()
                .any(|gap| gap.reason == "archive_overflow")
        );
    }
}
