//! All series plus the global gap index; mutated only by the state actor
//! (under the write lock) and pinned by history queries (under a short read
//! lock).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dgxtop_core::budget::{ByteLedger, Partition};
use dgxtop_core::canonical::GapRecord;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::{CanonicalUnit, MetricDescriptor};
pub(crate) use dgxtop_core::ports::{CutAppend, GaugeAppend, HistoryAppends, IntervalAppend};

use super::gaps::{GapEntry, GapInsert, GapList, ReasonInterner};
use super::grid::MinuteGrid;
use super::output::{ArchiveRow, DropReason, HistoryReport};
use super::records::{CounterSample, GaugeBucket};
use super::segment::SegmentIdentity;
use super::series::{Series, gauge_record, series_overhead_bytes};
use super::slots::{SlotHandle, SlotTable};

/// Gap entries kept for keys without a series and host-wide gaps.
const MAX_GLOBAL_GAPS: usize = 1024;
/// Clock discontinuities remembered for query results.
const MAX_DISCONTINUITIES: usize = 256;
/// Evictions one admission may perform before giving up (bounds lock time).
const MAX_EVICTIONS_PER_REQUEST: usize = 64;
/// Segment metadata bytes reserved before a new segment is created.
const SEGMENT_HEADROOM_BYTES: u64 = 512;

/// Retention and admission settings of the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistorySettings {
    pub raw_history_ns: u64,
    pub aggregate_retention_ns: u64,
    pub max_series: usize,
    /// Minutes older than `now - grace` are finalized; fresh samples are
    /// never older than this, so finalized buckets never change.
    pub finalize_grace_ns: u64,
    pub gap_merge_tolerance_ns: u64,
    pub archive_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppendKind {
    Gauge,
    Interval,
}

fn record_drop(report: &mut HistoryReport, kind: AppendKind, reason: DropReason) {
    match kind {
        AppendKind::Gauge => report.gauge_drops.record(reason),
        AppendKind::Interval => report.interval_drops.record(reason),
    }
}

pub(crate) struct HistoryState {
    pub(super) revision: u64,
    pub(super) slots: SlotTable<Series>,
    pub(super) by_key: BTreeMap<MetricKey, SlotHandle>,
    pub(super) global_gaps: GapList,
    pub(super) discontinuities: VecDeque<u64>,
    /// The boot's minutes, fixed by the first boot-to-UTC pairing.
    grid: Option<MinuteGrid>,
    reasons: ReasonInterner,
    settings: HistorySettings,
    ledger: ByteLedger,
}

impl HistoryState {
    pub(crate) fn new(settings: HistorySettings, ledger: &ByteLedger) -> Self {
        Self {
            revision: 0,
            slots: SlotTable::new(),
            by_key: BTreeMap::new(),
            global_gaps: GapList::new(ledger, MAX_GLOBAL_GAPS),
            discontinuities: VecDeque::new(),
            grid: None,
            reasons: ReasonInterner::new(ledger),
            settings,
            ledger: ledger.clone(),
        }
    }

    /// Fixes the boot's minutes (D-82); a later grid is ignored, because
    /// finished minutes are never cut again. Minutes wait for it: none is
    /// finalized before, so the first finalization already cuts on it.
    pub(crate) fn set_minute_grid(&mut self, grid: MinuteGrid) {
        if self.grid.is_some() {
            return;
        }
        self.grid = Some(grid);
        for handle in self.slots.handles() {
            if let Some(series) = self.slots.get_mut(handle) {
                series.adopt_grid(grid);
            }
        }
    }

    /// The boot's minutes, once known.
    pub(crate) fn minute_grid(&self) -> Option<MinuteGrid> {
        self.grid
    }

    /// Start of the minute holding `time_ns`; boot 0 while the grid is
    /// unknown (nothing is old or settled yet).
    fn minute_floor(&self, time_ns: u64) -> u64 {
        self.grid.map_or(0, |grid| grid.floor(time_ns))
    }

    pub(crate) fn set_finalize_grace(&mut self, grace_ns: u64) {
        self.settings.finalize_grace_ns = grace_ns;
    }

    /// Snapshot revision reported by queries; follows every publish.
    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }

    /// Records a gap reported from outside the history (Manager or store).
    pub(crate) fn record_external_gap(&mut self, gap: GapRecord) -> HistoryReport {
        let mut report = HistoryReport::default();
        self.note_gap(gap, &mut report);
        self.summarize(&mut report);
        report
    }

    pub(crate) fn series_keys(&self) -> Vec<MetricKey> {
        self.by_key.keys().cloned().collect()
    }

    pub(crate) fn note_discontinuity(&mut self, boot_ns: u64) {
        if self.discontinuities.len() >= MAX_DISCONTINUITIES {
            self.discontinuities.pop_front();
        }
        self.discontinuities.push_back(boot_ns);
    }

    /// Applies one committed batch and finalizes the touched series.
    pub(crate) fn apply(
        &mut self,
        appends: HistoryAppends,
        now_ns: u64,
        revision: u64,
    ) -> HistoryReport {
        self.revision = revision;
        let mut report = HistoryReport::default();
        let mut touched = BTreeSet::new();
        for append in appends.gauges {
            if let Some(handle) = self.apply_gauge(append, now_ns, &mut report) {
                touched.insert(handle);
            }
        }
        for cut in appends.cuts {
            if let Some(handle) = self.apply_cut(cut, now_ns, &mut report) {
                touched.insert(handle);
            }
        }
        for interval in appends.intervals {
            if let Some(handle) = self.apply_interval(interval, now_ns, &mut report) {
                touched.insert(handle);
            }
        }
        let horizon = now_ns.saturating_sub(self.settings.finalize_grace_ns);
        for handle in touched {
            self.finalize(handle, horizon, &mut report);
        }
        self.summarize(&mut report);
        report
    }

    fn apply_gauge(
        &mut self,
        append: GaugeAppend,
        now_ns: u64,
        report: &mut HistoryReport,
    ) -> Option<SlotHandle> {
        let kind = AppendKind::Gauge;
        if append.time_ns > now_ns {
            record_drop(report, kind, DropReason::Future);
            return None;
        }
        let handle = self.series_for(
            &append.key,
            append.descriptor,
            append.time_ns,
            now_ns,
            kind,
            report,
        )?;
        let admitted = self.slots.get(handle)?.admit_sample(append.time_ns);
        if let Err(reason) = admitted {
            record_drop(report, kind, reason);
            return None;
        }
        let gap = (append.time_ns, append.stale_deadline_ns);
        if !self.reserve_for_append(handle, Some((&append.identity, append.unit)), kind, report) {
            self.drop_with_gap(handle, gap, DropReason::Capacity, kind, report);
            return None;
        }
        let series = self.slots.get_mut(handle)?;
        let segment = match series.segment(append.identity, append.unit, append.time_ns) {
            Ok(segment) => segment,
            Err(reason) => {
                self.drop_with_gap(handle, gap, reason, kind, report);
                return None;
            }
        };
        let record = gauge_record(
            append.time_ns,
            append.stale_deadline_ns,
            Some(append.value),
            segment,
        );
        match series.append_sample(record, now_ns) {
            Ok(()) => Some(handle),
            Err(reason) => {
                self.drop_with_gap(handle, gap, reason, kind, report);
                None
            }
        }
    }

    /// A missing value ends the previous step and leaves a visible gap; it
    /// never refreshes or extends the last good value.
    fn apply_cut(
        &mut self,
        cut: CutAppend,
        now_ns: u64,
        report: &mut HistoryReport,
    ) -> Option<SlotHandle> {
        let handle = *self.by_key.get(&cut.key)?;
        let series = self.slots.get(handle)?;
        if cut.time_ns > now_ns || series.admit_sample(cut.time_ns).is_err() {
            return None;
        }
        let gap = GapRecord {
            key: Some(cut.key.clone()),
            adapter_id: None,
            start_boot_ns: cut.time_ns,
            end_boot_ns: cut.gap_end_ns.max(cut.time_ns.saturating_add(1)),
            reason: cut.reason,
            affected_count: 1,
        };
        self.note_gap(gap, report);
        if !self.slots.get(handle)?.cut_needed() {
            return Some(handle);
        }
        if !self.reserve_for_append(handle, None, AppendKind::Gauge, report) {
            report.gauge_drops.record(DropReason::Capacity);
            return Some(handle);
        }
        let series = self.slots.get_mut(handle)?;
        let record = gauge_record(cut.time_ns, cut.time_ns, None, series.last_gauge_segment());
        if series.append_sample(record, now_ns).is_err() {
            report.gauge_drops.record(DropReason::Capacity);
        }
        Some(handle)
    }

    fn apply_interval(
        &mut self,
        append: IntervalAppend,
        now_ns: u64,
        report: &mut HistoryReport,
    ) -> Option<SlotHandle> {
        let kind = AppendKind::Interval;
        if append.end_ns > now_ns {
            record_drop(report, kind, DropReason::Future);
            return None;
        }
        let handle = self.series_for(
            &append.key,
            append.descriptor,
            append.start_ns,
            now_ns,
            kind,
            report,
        )?;
        let series = self.slots.get_mut(handle)?;
        series.reach_back_to(append.start_ns);
        let admitted = series.admit_interval(append.start_ns, append.end_ns);
        if let Err(reason) = admitted {
            record_drop(report, kind, reason);
            return None;
        }
        let gap = (append.start_ns, append.end_ns);
        if !self.reserve_for_append(handle, Some((&append.identity, append.unit)), kind, report) {
            self.drop_with_gap(handle, gap, DropReason::Capacity, kind, report);
            return None;
        }
        let series = self.slots.get_mut(handle)?;
        let segment = match series.segment(append.identity, append.unit, append.start_ns) {
            Ok(segment) => segment,
            Err(reason) => {
                self.drop_with_gap(handle, gap, reason, kind, report);
                return None;
            }
        };
        let record = CounterSample {
            start_ns: append.start_ns,
            end_ns: append.end_ns,
            delta: append.delta,
            segment,
        };
        match series.append_interval(record, now_ns) {
            Ok(()) => Some(handle),
            Err(reason) => {
                self.drop_with_gap(handle, gap, reason, kind, report);
                None
            }
        }
    }

    /// Makes room for one raw record (and a new segment when needed),
    /// evicting the oldest evictable data first.
    fn reserve_for_append(
        &mut self,
        handle: SlotHandle,
        segment: Option<(&SegmentIdentity, CanonicalUnit)>,
        kind: AppendKind,
        report: &mut HistoryReport,
    ) -> bool {
        let Some(series) = self.slots.get(handle) else {
            return false;
        };
        let raw_cost = match kind {
            AppendKind::Gauge => series.gauge.raw.cost_of_pushes(1),
            AppendKind::Interval => series.counter.raw.cost_of_pushes(1),
        };
        let segment_cost = match segment {
            Some((identity, unit)) if series.needs_new_segment(identity, unit) => {
                SEGMENT_HEADROOM_BYTES
            }
            _ => 0,
        };
        self.ensure_headroom(Partition::RawHistory, raw_cost, report)
            && self.ensure_headroom(Partition::Aggregates, segment_cost, report)
    }

    fn series_for(
        &mut self,
        key: &MetricKey,
        descriptor: &'static MetricDescriptor,
        time_ns: u64,
        now_ns: u64,
        kind: AppendKind,
        report: &mut HistoryReport,
    ) -> Option<SlotHandle> {
        if let Some(handle) = self.by_key.get(key) {
            return Some(*handle);
        }
        if self.slots.live() >= self.settings.max_series {
            self.reclaim_empty_series(now_ns);
        }
        if self.slots.live() >= self.settings.max_series {
            record_drop(report, kind, DropReason::SeriesLimit);
            self.record_series_gap(key, time_ns, now_ns, DropReason::SeriesLimit, report);
            return None;
        }
        if !self.ensure_headroom(Partition::Aggregates, series_overhead_bytes(key), report) {
            record_drop(report, kind, DropReason::Capacity);
            return None;
        }
        let created = Series::new(
            key.clone(),
            descriptor,
            (time_ns, now_ns),
            self.grid,
            &self.ledger,
        );
        let Ok(series) = created else {
            record_drop(report, kind, DropReason::Capacity);
            return None;
        };
        let handle = self.slots.insert(series)?;
        self.by_key.insert(key.clone(), handle);
        Some(handle)
    }

    fn record_series_gap(
        &mut self,
        key: &MetricKey,
        time_ns: u64,
        now_ns: u64,
        reason: DropReason,
        report: &mut HistoryReport,
    ) {
        let Some(code) = reason.gap_reason() else {
            return;
        };
        let gap = GapRecord {
            key: Some(key.clone()),
            adapter_id: None,
            start_boot_ns: time_ns,
            end_boot_ns: now_ns.max(time_ns.saturating_add(1)),
            reason: code.to_owned(),
            affected_count: 1,
        };
        self.note_gap(gap, report);
    }

    fn drop_with_gap(
        &mut self,
        handle: SlotHandle,
        (start_ns, end_ns): (u64, u64),
        reason: DropReason,
        kind: AppendKind,
        report: &mut HistoryReport,
    ) {
        record_drop(report, kind, reason);
        if let Some(code) = reason.gap_reason() {
            self.record_loss_gap(handle, (start_ns, end_ns), code, report);
        }
    }

    /// Records lost history time of one series as a visible gap.
    fn record_loss_gap(
        &mut self,
        handle: SlotHandle,
        (start_ns, end_ns): (u64, u64),
        reason: &str,
        report: &mut HistoryReport,
    ) {
        let Some(key) = self.slots.get(handle).map(|series| series.key.clone()) else {
            return;
        };
        let gap = GapRecord {
            key: Some(key),
            adapter_id: None,
            start_boot_ns: start_ns,
            end_boot_ns: end_ns.max(start_ns.saturating_add(1)),
            reason: reason.to_owned(),
            affected_count: 1,
        };
        self.note_gap(gap, report);
    }

    /// Records a gap and reports it when it became visible; only a new
    /// entry counts as a new gap (a merged repeat extends an existing one).
    fn note_gap(&mut self, gap: GapRecord, report: &mut HistoryReport) {
        match self.record_gap(&gap, report) {
            GapInsert::Added { .. } => {
                report.gaps_added = report.gaps_added.saturating_add(1);
                report.new_gaps.push(gap);
            }
            GapInsert::Merged => report.new_gaps.push(gap),
            GapInsert::Rejected => {}
        }
    }

    /// Adds a gap to its series (or the global list).
    fn record_gap(&mut self, gap: &GapRecord, report: &mut HistoryReport) -> GapInsert {
        let entry = GapEntry {
            key: gap.key.clone(),
            adapter_id: gap.adapter_id.clone(),
            start_ns: gap.start_boot_ns,
            end_ns: gap.end_boot_ns,
            reason: self.reasons.intern(&gap.reason),
            affected_count: gap.affected_count.max(1),
            settled: false,
        };
        let tolerance = self.settings.gap_merge_tolerance_ns;
        let handle = gap
            .key
            .as_ref()
            .and_then(|key| self.by_key.get(key).copied());
        let cost = match handle.and_then(|handle| self.slots.get(handle)) {
            Some(series) => series.gaps.growth_cost(),
            None => self.global_gaps.growth_cost(),
        };
        self.ensure_headroom(Partition::Aggregates, cost, report);
        match handle.and_then(|handle| self.slots.get_mut(handle)) {
            Some(series) => series.gaps.insert(entry, tolerance),
            None => self.global_gaps.insert(entry, tolerance),
        }
    }

    fn finalize(&mut self, handle: SlotHandle, horizon_ns: u64, report: &mut HistoryReport) {
        let Some(series) = self.slots.get(handle) else {
            return;
        };
        let Some(plan) = series.plan_finalization(horizon_ns) else {
            return;
        };
        let cost = series.finalization_cost(&plan);
        self.ensure_headroom(Partition::Aggregates, cost, report);
        let Some(series) = self.slots.get_mut(handle) else {
            return;
        };
        let lost = series.commit_finalization(plan, report);
        for range in lost {
            self.record_loss_gap(handle, range, "history_capacity", report);
        }
    }

    fn available(&self, partition: Partition) -> u64 {
        self.ledger
            .limits()
            .limit(partition)
            .saturating_sub(self.ledger.used(partition))
    }

    /// Evicts the oldest evictable data of `partition` until `bytes` fit.
    /// Chunks pinned by a running query stay accounted, so eviction may need
    /// several rounds; it stops after a bounded number of evictions.
    fn ensure_headroom(
        &mut self,
        partition: Partition,
        bytes: u64,
        report: &mut HistoryReport,
    ) -> bool {
        if bytes == 0 {
            return true;
        }
        for _ in 0..MAX_EVICTIONS_PER_REQUEST {
            if self.available(partition) >= bytes {
                return true;
            }
            let evicted = match partition {
                Partition::RawHistory => self.evict_raw_once(report),
                Partition::Aggregates => self.evict_rollup_once(report),
                _ => false,
            };
            if !evicted {
                break;
            }
        }
        self.available(partition) >= bytes
    }

    fn evict_raw_once(&mut self, report: &mut HistoryReport) -> bool {
        let victim = self
            .slots
            .handles()
            .into_iter()
            .filter_map(|handle| Some((self.slots.get(handle)?.oldest_evictable_raw_ns()?, handle)))
            .min();
        let Some((_, handle)) = victim else {
            return false;
        };
        let Some(series) = self.slots.get_mut(handle) else {
            return false;
        };
        let dropped = series.evict_oldest_raw();
        report.evicted_records = report
            .evicted_records
            .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
        dropped > 0
    }

    fn evict_rollup_once(&mut self, report: &mut HistoryReport) -> bool {
        let victim = self
            .slots
            .handles()
            .into_iter()
            .filter_map(|handle| Some((self.slots.get(handle)?.oldest_rollup_ns()?, handle)))
            .min();
        let global_oldest = self.global_gaps.oldest_start();
        let series_first = match (victim, global_oldest) {
            (Some((series_start, _)), Some(global_start)) => series_start <= global_start,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if !series_first {
            let dropped = self.global_gaps.drop_oldest();
            if dropped {
                report.evicted_records = report.evicted_records.saturating_add(1);
            }
            return dropped;
        }
        let Some((_, handle)) = victim else {
            return false;
        };
        let Some(series) = self.slots.get_mut(handle) else {
            return false;
        };
        let Some(eviction) = series.evict_oldest_rollup() else {
            return false;
        };
        report.evicted_records = report
            .evicted_records
            .saturating_add(u64::try_from(eviction.records).unwrap_or(u64::MAX));
        report.truncated = true;
        if eviction.to_ns > eviction.from_ns {
            let key = series.key.clone();
            let gap = GapRecord {
                key: Some(key),
                adapter_id: None,
                start_boot_ns: eviction.from_ns,
                end_boot_ns: eviction.to_ns,
                reason: "evicted".to_owned(),
                affected_count: 1,
            };
            self.note_gap(gap, report);
        }
        eviction.records > 0
    }

    /// Overload relief: raw history older than `keep_ns` is dropped (only
    /// minutes that already have rollups).
    pub(crate) fn shed_raw(&mut self, now_ns: u64, keep_ns: u64) -> HistoryReport {
        let mut report = HistoryReport::default();
        let target = now_ns.saturating_sub(keep_ns);
        for handle in self.slots.handles() {
            if let Some(series) = self.slots.get_mut(handle) {
                let dropped = series.advance_raw_floors(target);
                report.evicted_records = report
                    .evicted_records
                    .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
            }
        }
        self.summarize(&mut report);
        report
    }

    /// Retention, finalization, segment retirement and gap settlement.
    pub(crate) fn maintain(&mut self, now_ns: u64) -> HistoryReport {
        let mut report = HistoryReport::default();
        let horizon = now_ns.saturating_sub(self.settings.finalize_grace_ns);
        let raw_target = now_ns.saturating_sub(self.settings.raw_history_ns);
        let rollup_target = now_ns.saturating_sub(self.settings.aggregate_retention_ns);
        let settle_horizon = self.minute_floor(horizon);
        let archive = self.settings.archive_enabled;
        for handle in self.slots.handles() {
            self.finalize(handle, horizon, &mut report);
            let Some(series) = self.slots.get_mut(handle) else {
                continue;
            };
            series.advance_raw_floors(raw_target);
            series.advance_rollup_floors(rollup_target);
            series.retire_unreferenced_segments();
            if archive {
                for entry in series.gaps.settle_before(settle_horizon) {
                    report.archive_rows.push(ArchiveRow::Gap(entry.to_record()));
                }
            }
        }
        let rollup_floor = self.minute_floor(rollup_target);
        self.global_gaps.drop_ending_by(rollup_floor);
        if archive {
            for entry in self.global_gaps.settle_before(settle_horizon) {
                report.archive_rows.push(ArchiveRow::Gap(entry.to_record()));
            }
        }
        self.reclaim_empty_series(now_ns);
        self.summarize(&mut report);
        report
    }

    /// Removes series whose data aged out completely; their slot is freed
    /// with a new generation so the old identity never resolves again.
    fn reclaim_empty_series(&mut self, now_ns: u64) {
        let idle_after = self.settings.raw_history_ns;
        let removable: Vec<(SlotHandle, MetricKey)> = self
            .slots
            .handles()
            .into_iter()
            .filter_map(|handle| {
                let series = self.slots.get(handle)?;
                let idle = series.last_activity_ns.saturating_add(idle_after) <= now_ns;
                (idle && series.holds_no_data()).then(|| (handle, series.key.clone()))
            })
            .collect();
        for (handle, key) in removable {
            self.slots.remove(handle);
            self.by_key.remove(&key);
        }
    }

    fn summarize(&self, report: &mut HistoryReport) {
        report.series_count = u32::try_from(self.slots.live()).unwrap_or(u32::MAX);
        report.truncated |= self.slots.values().any(|series| series.capacity_truncated);
    }

    pub(crate) fn handle_of(&self, key: &MetricKey) -> Option<SlotHandle> {
        self.by_key.get(key).copied()
    }

    /// The held series that is `series` in this boot, if any.
    pub(crate) fn key_in_this_boot(&self, series: &SeriesInEveryBoot) -> Option<MetricKey> {
        self.by_key
            .keys()
            .find(|key| SeriesInEveryBoot::from(*key) == *series)
            .cloned()
    }

    /// `key`'s minutes not finalized yet, up to `now_ns` (D-82).
    pub(crate) fn provisional_minutes(&self, key: &MetricKey, now_ns: u64) -> Vec<GaugeBucket> {
        self.by_key
            .get(key)
            .and_then(|handle| self.slots.get(*handle))
            .map(|series| series.provisional_gauge_buckets(now_ns))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::history::HistoryMethod;
    use dgxtop_core::metric::{Scalar, descriptor, ids};
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::{metric_key, source_id};

    const SECOND: u64 = 1_000_000_000;

    fn settings() -> HistorySettings {
        HistorySettings {
            raw_history_ns: 300 * SECOND,
            aggregate_retention_ns: 24 * 3600 * SECOND,
            max_series: 2,
            finalize_grace_ns: 5 * SECOND,
            gap_merge_tolerance_ns: SECOND,
            archive_enabled: true,
        }
    }

    fn state(ledger: &ByteLedger) -> HistoryState {
        let mut state = HistoryState::new(settings(), ledger);
        state.set_minute_grid(MinuteGrid::BOOT);
        state
    }

    fn gauge(entity: &str, seconds: u64, value: f64) -> GaugeAppend {
        GaugeAppend {
            key: metric_key(entity, ids::GPU_TEMPERATURE, "gpu"),
            descriptor: descriptor(ids::GPU_TEMPERATURE).unwrap(),
            identity: SegmentIdentity {
                source_id: source_id("nvml:0:temp"),
                source_generation: 1,
                policy_revision: 1,
                sample_kind: SampleKind::Measured,
                method: HistoryMethod::TimeWeightedStep,
            },
            unit: CanonicalUnit::DegC,
            time_ns: seconds * SECOND,
            stale_deadline_ns: (seconds + 3) * SECOND,
            value: Scalar::from_f64(value).unwrap(),
        }
    }

    #[test]
    fn series_limit_rejects_new_series_visibly() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut state = state(&ledger);
        let appends = HistoryAppends {
            gauges: vec![
                gauge("gpu:a", 1, 1.0),
                gauge("gpu:b", 1, 1.0),
                gauge("gpu:c", 1, 1.0),
            ],
            ..HistoryAppends::default()
        };
        let report = state.apply(appends, 2 * SECOND, 1);
        assert_eq!(report.series_count, 2);
        assert_eq!(report.gauge_drops.series_limit, 1);
        assert_eq!(report.new_gaps.len(), 1);
        assert_eq!(report.new_gaps[0].reason, "series_capacity");
        assert_eq!(state.series_keys().len(), 2);
    }

    #[test]
    fn future_late_and_duplicate_samples_are_dropped() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut state = state(&ledger);
        let report = state.apply(
            HistoryAppends {
                gauges: vec![
                    gauge("gpu:a", 100, 1.0),
                    gauge("gpu:a", 100, 2.0),
                    gauge("gpu:a", 500, 3.0),
                ],
                ..HistoryAppends::default()
            },
            200 * SECOND,
            1,
        );
        assert_eq!(report.gauge_drops.out_of_order, 1);
        assert_eq!(report.gauge_drops.future, 1);
        state.maintain(400 * SECOND);
        let report = state.apply(
            HistoryAppends {
                gauges: vec![gauge("gpu:a", 150, 1.0)],
                ..HistoryAppends::default()
            },
            400 * SECOND,
            2,
        );
        assert_eq!(report.gauge_drops.late, 1);
    }

    #[test]
    fn empty_series_are_reclaimed_with_a_new_generation() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut state = state(&ledger);
        state.apply(
            HistoryAppends {
                gauges: vec![gauge("gpu:a", 1, 1.0)],
                ..HistoryAppends::default()
            },
            2 * SECOND,
            1,
        );
        let key = metric_key("gpu:a", ids::GPU_TEMPERATURE, "gpu");
        let old_handle = state.handle_of(&key).unwrap();
        let far = 2 * 24 * 3600 * SECOND;
        state.maintain(far);
        state.maintain(far + 3600 * SECOND);
        assert!(state.handle_of(&key).is_none());
        let late = far / SECOND + 3600;
        state.apply(
            HistoryAppends {
                gauges: vec![gauge("gpu:b", late, 1.0)],
                ..HistoryAppends::default()
            },
            (late + 1) * SECOND,
            2,
        );
        let new_handle = state
            .handle_of(&metric_key("gpu:b", ids::GPU_TEMPERATURE, "gpu"))
            .unwrap();
        assert_ne!(old_handle, new_handle);
        assert!(state.slots.get(old_handle).is_none());
    }
}
