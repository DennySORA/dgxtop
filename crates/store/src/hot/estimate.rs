//! Conservative byte estimates of the structures a commit publishes, used to
//! reserve `hot_state` / `catalog` bytes before allocating them.

use std::mem::size_of;

use dgxtop_core::canonical::{
    CandidateExplanation, CanonicalSample, CanonicalValue, CollectionBatch, CollectorState,
    DomainSnapshot, Explanation, GapRecord, ImmutableSnapshot, ProcessRow, ProcessTable,
    SnapshotEntry,
};
use dgxtop_core::identity::{MetricKey, SourceId};

/// Node overhead of one `BTreeMap` entry (amortized).
const MAP_NODE_BYTES: u64 = 64;
/// Header of one shared `Arc` allocation.
const ARC_HEADER_BYTES: u64 = 16;
/// Key ownership entry kept next to every snapshot key.
const OWNER_ENTRY_BYTES: u64 = 96;

fn bytes(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

fn key_bytes(key: &MetricKey) -> u64 {
    let strings = key.host_id.as_str().len()
        + key.boot_id.as_str().len()
        + key.entity_id.as_str().len()
        + key.metric_id.as_str().len()
        + key.scope.as_str().len();
    bytes(size_of::<MetricKey>() + strings).saturating_add(5 * ARC_HEADER_BYTES)
}

fn source_ids_bytes(ids: &[SourceId], capacity: usize) -> u64 {
    let strings: usize = ids.iter().map(|id| id.as_str().len()).sum();
    bytes(capacity.saturating_mul(size_of::<SourceId>()) + strings)
        .saturating_add(ARC_HEADER_BYTES.saturating_mul(bytes(ids.len())))
}

fn strings_bytes(strings: &[String], capacity: usize) -> u64 {
    let heap: usize = strings.iter().map(String::capacity).sum();
    bytes(capacity.saturating_mul(size_of::<String>()) + heap)
}

fn entry_bytes(
    key: &MetricKey,
    candidates: &[SourceId],
    candidate_capacity: usize,
    reasons: &[String],
    reason_capacity: usize,
) -> u64 {
    bytes(size_of::<SnapshotEntry>() + size_of::<MetricKey>())
        .saturating_add(MAP_NODE_BYTES)
        .saturating_add(OWNER_ENTRY_BYTES)
        .saturating_add(key_bytes(key))
        .saturating_add(source_ids_bytes(candidates, candidate_capacity))
        .saturating_add(strings_bytes(reasons, reason_capacity))
}

fn value_entry_bytes(value: &CanonicalValue) -> u64 {
    entry_bytes(
        &value.key,
        &value.candidate_source_ids,
        value.candidate_source_ids.capacity(),
        &value.reason_codes,
        value.reason_codes.capacity(),
    )
}

fn sample_entry_bytes(sample: &CanonicalSample) -> u64 {
    entry_bytes(
        &sample.key,
        &sample.candidate_source_ids,
        sample.candidate_source_ids.capacity(),
        &sample.reason_codes,
        sample.reason_codes.capacity(),
    )
}

fn candidate_bytes(candidate: &CandidateExplanation) -> u64 {
    let strings = candidate.source_id.as_str().len()
        + candidate.adapter_id.as_str().len()
        + candidate
            .origin
            .origin_id
            .as_ref()
            .map_or(0, |id| id.as_str().len())
        + candidate.native_unit.as_str().len()
        + candidate.conversion_id.as_ref().map_or(0, String::capacity);
    bytes(strings).saturating_add(strings_bytes(
        &candidate.suspect_reasons,
        candidate.suspect_reasons.capacity(),
    ))
}

pub(crate) fn explanation_bytes(explanation: &Explanation) -> u64 {
    let candidates = explanation.candidates.iter().fold(0u64, |sum, candidate| {
        sum.saturating_add(candidate_bytes(candidate))
    });
    bytes(size_of::<Explanation>() + size_of::<MetricKey>())
        .saturating_add(ARC_HEADER_BYTES + MAP_NODE_BYTES)
        .saturating_add(bytes(
            explanation
                .candidates
                .capacity()
                .saturating_mul(size_of::<CandidateExplanation>()),
        ))
        .saturating_add(candidates)
        .saturating_add(strings_bytes(
            &explanation.reason_codes,
            explanation.reason_codes.capacity(),
        ))
}

/// Bytes of one published domain snapshot.
pub(crate) fn domain_bytes(domain: &DomainSnapshot) -> u64 {
    let samples = domain.samples.values().fold(0u64, |sum, entry| {
        sum.saturating_add(sample_entry_bytes(&entry.sample))
    });
    let explanations = domain.explanations.values().fold(0u64, |sum, explanation| {
        sum.saturating_add(explanation_bytes(explanation))
    });
    bytes(size_of::<DomainSnapshot>())
        .saturating_add(ARC_HEADER_BYTES)
        .saturating_add(samples)
        .saturating_add(explanations)
}

/// Upper bound of the domain a batch produces: the previous domain plus
/// every value and explanation of the batch (as if all keys were new).
pub(crate) fn domain_upper_bound(
    previous: Option<&DomainSnapshot>,
    batch: &CollectionBatch,
) -> u64 {
    let base = previous.map_or_else(
        || bytes(size_of::<DomainSnapshot>()).saturating_add(ARC_HEADER_BYTES),
        domain_bytes,
    );
    let values = batch.canonical.iter().fold(0u64, |sum, value| {
        sum.saturating_add(value_entry_bytes(value))
    });
    let explanations = batch.explanations.iter().fold(0u64, |sum, explanation| {
        sum.saturating_add(explanation_bytes(explanation))
    });
    base.saturating_add(values).saturating_add(explanations)
}

fn process_row_bytes(row: &ProcessRow) -> u64 {
    bytes(
        row.name.capacity()
            + row.missing_reason.as_ref().map_or(0, String::capacity)
            + row
                .gpu
                .capacity()
                .saturating_mul(size_of::<dgxtop_core::canonical::ProcessGpuLink>()),
    )
    .saturating_add(key_bytes_of_process(row))
}

fn key_bytes_of_process(row: &ProcessRow) -> u64 {
    let strings = row.key.host_id.as_str().len()
        + row.key.boot_id.as_str().len()
        + row.key.pid_namespace.as_str().len();
    bytes(strings).saturating_add(3 * ARC_HEADER_BYTES)
}

/// Bytes of one published process table.
pub(crate) fn process_table_bytes(table: &ProcessTable) -> u64 {
    let rows = table
        .rows
        .iter()
        .fold(0u64, |sum, row| sum.saturating_add(process_row_bytes(row)));
    bytes(
        size_of::<ProcessTable>()
            + table
                .rows
                .capacity()
                .saturating_mul(size_of::<ProcessRow>()),
    )
    .saturating_add(ARC_HEADER_BYTES)
    .saturating_add(rows)
}

/// Bytes of the snapshot frame itself (maps of `Arc`s and health).
pub(crate) fn frame_bytes(domains: usize, collector_states: usize) -> u64 {
    let entries = bytes(domains.saturating_add(collector_states));
    bytes(size_of::<ImmutableSnapshot>())
        .saturating_add(ARC_HEADER_BYTES)
        .saturating_add(entries.saturating_mul(MAP_NODE_BYTES + 64))
        .saturating_add(1024)
}

/// Bytes of a committed collector state (its own entry estimate plus the
/// shared allocation itself).
pub(crate) fn collector_state_bytes(state: &CollectorState) -> u64 {
    bytes(size_of::<CollectorState>().saturating_add(state.estimated_bytes()))
        .saturating_add(ARC_HEADER_BYTES)
}

/// Bytes of a published recent-gap list.
pub(crate) fn gaps_bytes(gaps: &[GapRecord]) -> u64 {
    let heap: usize = gaps.iter().map(|gap| gap.reason.capacity()).sum();
    bytes(gaps.len().saturating_mul(size_of::<GapRecord>()) + heap).saturating_add(ARC_HEADER_BYTES)
}
