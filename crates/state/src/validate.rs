//! Commit validation. Everything here runs before any state is touched, so a
//! rejected batch leaves no trace (`INTERFACES.md` §8).

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::canonical::{AckStatus, CollectionBatch, CommitGuard, MAX_CANDIDATE_IDS};
use dgxtop_core::identity::{CollectorId, MetricKey};
use dgxtop_core::metric::{CanonicalUnit, METRIC_CATALOG, MetricDescriptor, ScalarView};
use dgxtop_core::quality::{Namespace, RawKind};

/// Most canonical values, explanations or counter intervals in one batch.
pub(crate) const MAX_BATCH_ITEMS: usize = 16_384;

/// Why a batch was not committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rejection {
    pub status: AckStatus,
    pub reason: String,
}

impl Rejection {
    pub(crate) fn invalid(reason: impl Into<String>) -> Self {
        Self {
            status: AckStatus::RejectedInvalid,
            reason: reason.into(),
        }
    }

    fn stale_epoch(reason: impl Into<String>) -> Self {
        Self {
            status: AckStatus::RejectedStaleEpoch,
            reason: reason.into(),
        }
    }

    fn stale_base(reason: impl Into<String>) -> Self {
        Self {
            status: AckStatus::RejectedStaleBase,
            reason: reason.into(),
        }
    }

    pub(crate) fn capacity(reason: impl Into<String>) -> Self {
        Self {
            status: AckStatus::RejectedCapacity,
            reason: reason.into(),
        }
    }

    pub(crate) fn conflict(reason: impl Into<String>) -> Self {
        Self {
            status: AckStatus::Conflict,
            reason: reason.into(),
        }
    }
}

/// O(log n) lookup into the compile-time metric catalog.
pub(crate) struct DescriptorIndex {
    by_id: BTreeMap<&'static str, &'static MetricDescriptor>,
}

impl DescriptorIndex {
    pub(crate) fn new() -> Self {
        Self {
            by_id: METRIC_CATALOG
                .iter()
                .map(|entry| (entry.id, entry))
                .collect(),
        }
    }

    pub(crate) fn get(&self, metric_id: &str) -> Option<&'static MetricDescriptor> {
        self.by_id.get(metric_id).copied()
    }
}

/// Epochs the store has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EpochState {
    /// Current epoch announced by the Manager (`StateWriter::set_epoch`).
    pub announced: Option<u64>,
    /// Highest epoch of a committed batch; older epochs are stale.
    pub highest_committed: Option<u64>,
}

/// Epoch and base checks of the commit handshake.
pub(crate) fn check_guard(
    batch: &CollectionBatch,
    guard: &CommitGuard,
    epochs: &EpochState,
    committed_state_revision: u64,
) -> Result<(), Rejection> {
    if guard.collector_id != batch.collector_id {
        return Err(Rejection::invalid(format!(
            "guard collector {} does not match batch collector {}",
            guard.collector_id, batch.collector_id
        )));
    }
    if batch.epoch != guard.expected_epoch {
        return Err(Rejection::stale_epoch(format!(
            "batch epoch {} differs from expected epoch {}",
            batch.epoch, guard.expected_epoch
        )));
    }
    if let Some(announced) = epochs.announced
        && guard.expected_epoch != announced
    {
        return Err(Rejection::stale_epoch(format!(
            "expected epoch {} differs from current epoch {announced}",
            guard.expected_epoch
        )));
    }
    if let Some(highest) = epochs.highest_committed
        && batch.epoch < highest
    {
        return Err(Rejection::stale_epoch(format!(
            "batch epoch {} is older than committed epoch {highest}",
            batch.epoch
        )));
    }
    if guard.state_base_revision != batch.state_base_revision {
        return Err(Rejection::stale_base(format!(
            "guard base {} differs from batch base {}",
            guard.state_base_revision, batch.state_base_revision
        )));
    }
    if batch.state_base_revision != committed_state_revision {
        return Err(Rejection::stale_base(format!(
            "batch base {} but committed state revision of {} is {committed_state_revision}",
            batch.state_base_revision, batch.collector_id
        )));
    }
    if batch.state_base_revision == u64::MAX {
        return Err(Rejection::invalid("state revision exhausted"));
    }
    Ok(())
}

/// Content rules a batch must satisfy.
pub(crate) struct ContentRules<'a> {
    pub namespace: Namespace,
    pub descriptors: &'a DescriptorIndex,
    pub key_owners: &'a BTreeMap<MetricKey, CollectorId>,
    pub max_processes: usize,
    pub max_process_gpu_links: usize,
}

/// Descriptors resolved during validation, parallel to the batch vectors.
pub(crate) struct CheckedBatch {
    pub value_descriptors: Vec<&'static MetricDescriptor>,
    pub interval_descriptors: Vec<&'static MetricDescriptor>,
    pub new_keys: Vec<MetricKey>,
}

/// Unit of a counter interval for a series: the integral of a rate, or the
/// counter's own unit.
pub(crate) fn interval_unit(descriptor: &MetricDescriptor) -> Option<CanonicalUnit> {
    match descriptor.unit {
        CanonicalUnit::BytesPerSecond => Some(CanonicalUnit::Bytes),
        CanonicalUnit::CountPerSecond => Some(CanonicalUnit::Count),
        CanonicalUnit::Watts => Some(CanonicalUnit::Joules),
        unit if descriptor.kind == RawKind::Counter => Some(unit),
        _ => None,
    }
}

fn check_bounds(batch: &CollectionBatch) -> Result<(), Rejection> {
    let sizes = [
        ("canonical values", batch.canonical.len()),
        ("explanations", batch.explanations.len()),
        ("counter intervals", batch.counter_intervals.len()),
    ];
    for (name, size) in sizes {
        if size > MAX_BATCH_ITEMS {
            return Err(Rejection::invalid(format!(
                "{size} {name} exceed the batch limit of {MAX_BATCH_ITEMS}"
            )));
        }
    }
    Ok(())
}

fn check_values(
    batch: &CollectionBatch,
    rules: &ContentRules<'_>,
) -> Result<(Vec<&'static MetricDescriptor>, Vec<MetricKey>), Rejection> {
    let mut descriptors = Vec::with_capacity(batch.canonical.len());
    let mut seen = BTreeSet::new();
    let mut new_keys = Vec::new();
    for value in &batch.canonical {
        let selector = value.key.series_selector();
        value
            .validate()
            .map_err(|error| Rejection::invalid(format!("{selector}: {error}")))?;
        if value.sample_kind.namespace() != rules.namespace {
            return Err(Rejection::invalid(format!(
                "{selector}: {:?} value does not belong to the {:?} namespace",
                value.sample_kind, rules.namespace
            )));
        }
        let descriptor = rules
            .descriptors
            .get(value.key.metric_id.as_str())
            .ok_or_else(|| Rejection::invalid(format!("{selector}: unknown metric id")))?;
        if value.unit != descriptor.unit {
            return Err(Rejection::invalid(format!(
                "{selector}: unit {} does not match catalog unit {}",
                value.unit, descriptor.unit
            )));
        }
        if !seen.insert(&value.key) {
            return Err(Rejection::invalid(format!(
                "{selector}: key appears twice in the batch"
            )));
        }
        match rules.key_owners.get(&value.key) {
            Some(owner) if *owner != batch.collector_id => {
                return Err(Rejection::invalid(format!(
                    "{selector}: key is owned by collector {owner}"
                )));
            }
            Some(_) => {}
            None => new_keys.push(value.key.clone()),
        }
        descriptors.push(descriptor);
    }
    Ok((descriptors, new_keys))
}

fn check_intervals(
    batch: &CollectionBatch,
    rules: &ContentRules<'_>,
) -> Result<Vec<&'static MetricDescriptor>, Rejection> {
    let mut descriptors = Vec::with_capacity(batch.counter_intervals.len());
    for interval in &batch.counter_intervals {
        let selector = interval.key.series_selector();
        let descriptor = rules
            .descriptors
            .get(interval.key.metric_id.as_str())
            .ok_or_else(|| {
                Rejection::invalid(format!(
                    "{selector}: unknown metric id for counter interval"
                ))
            })?;
        if interval_unit(descriptor) != Some(interval.unit) {
            return Err(Rejection::invalid(format!(
                "{selector}: counter interval unit {} does not integrate catalog unit {}",
                interval.unit, descriptor.unit
            )));
        }
        if interval.end_boot_ns <= interval.start_boot_ns {
            return Err(Rejection::invalid(format!(
                "{selector}: counter interval end must follow its start"
            )));
        }
        if let ScalarView::F64(delta) = interval.delta.view()
            && delta < 0.0
        {
            return Err(Rejection::invalid(format!(
                "{selector}: counter interval delta is negative"
            )));
        }
        descriptors.push(descriptor);
    }
    Ok(descriptors)
}

fn check_explanations(batch: &CollectionBatch, rules: &ContentRules<'_>) -> Result<(), Rejection> {
    for explanation in &batch.explanations {
        let selector = explanation.key.series_selector();
        if rules
            .descriptors
            .get(explanation.key.metric_id.as_str())
            .is_none()
        {
            return Err(Rejection::invalid(format!(
                "{selector}: explanation for unknown metric id"
            )));
        }
        if explanation.candidates.len() > MAX_CANDIDATE_IDS {
            return Err(Rejection::invalid(format!(
                "{selector}: too many candidates in explanation"
            )));
        }
    }
    Ok(())
}

fn check_process_table(batch: &CollectionBatch, rules: &ContentRules<'_>) -> Result<(), Rejection> {
    let Some(table) = &batch.process_table else {
        return Ok(());
    };
    if table.rows.len() > rules.max_processes {
        return Err(Rejection::invalid(format!(
            "process table has {} rows (limit {})",
            table.rows.len(),
            rules.max_processes
        )));
    }
    let links: usize = table.rows.iter().map(|row| row.gpu.len()).sum();
    if links > rules.max_process_gpu_links {
        return Err(Rejection::invalid(format!(
            "process table has {links} GPU links (limit {})",
            rules.max_process_gpu_links
        )));
    }
    Ok(())
}

/// Validates every value, interval, explanation and the process table.
pub(crate) fn check_contents(
    batch: &CollectionBatch,
    rules: &ContentRules<'_>,
) -> Result<CheckedBatch, Rejection> {
    check_bounds(batch)?;
    let (value_descriptors, new_keys) = check_values(batch, rules)?;
    let interval_descriptors = check_intervals(batch, rules)?;
    check_explanations(batch, rules)?;
    check_process_table(batch, rules)?;
    Ok(CheckedBatch {
        value_descriptors,
        interval_descriptors,
        new_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::ids;

    #[test]
    fn interval_units_integrate_rates_or_keep_counter_units() {
        let index = DescriptorIndex::new();
        let rate = index.get(ids::NET_RX_BPS).unwrap();
        assert_eq!(interval_unit(rate), Some(CanonicalUnit::Bytes));
        let power = index.get(ids::CPU_POWER_PACKAGE).unwrap();
        assert_eq!(interval_unit(power), Some(CanonicalUnit::Joules));
        let packets = index.get(ids::NET_RX_PACKETS).unwrap();
        assert_eq!(interval_unit(packets), Some(CanonicalUnit::Count));
        let temperature = index.get(ids::GPU_TEMPERATURE).unwrap();
        assert_eq!(interval_unit(temperature), None);
        assert!(index.get("no.such.metric").is_none());
    }
}
