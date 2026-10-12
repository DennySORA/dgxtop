//! MetricCell mapping: one committed sample plus its arbitration explanation
//! becomes one displayable cell. A cell carries a value only when a source
//! was selected; every other state is explained in text and never shown as 0.

use dgxtop_core::canonical::{
    CandidateDisposition, CandidateExplanation, CanonicalSample, CatalogSnapshot, Explanation,
    RejectReason, SnapshotEntry,
};
use dgxtop_core::metric::{CanonicalUnit, descriptor};
use dgxtop_core::quality::{Freshness, ReadStatus, SelectionState};
use dgxtop_core::view::{CellState, ConflictView, LastGoodView, MetricCell};

use super::labels::{adapter_prefix, age_ms, humanize_scope, label_text};

/// Conflicting readings listed per cell (series keep at most 4 candidates).
const MAX_CONFLICT_ENTRIES: usize = 8;

/// Reason-code fragments meaning "the read itself failed". Reason codes are
/// upper snake case free text (no shared core constants yet), so matching is
/// by fragment.
const READ_FAILURE_MARKERS: [&str; 5] = [
    "TIMEOUT",
    "READ_FAILED",
    "READ_ERROR",
    "IO_ERROR",
    "REMOVED",
];
/// Reason-code fragments meaning "a counter-derived value awaits its next sample".
const WARMUP_MARKERS: [&str; 5] = ["WARMUP", "WARM_UP", "RESET", "FIRST_SAMPLE", "NO_BASELINE"];

/// Builds the cell for one committed sample. `catalog` resolves the adapter
/// of the selected source for the "package 0 · hwmon" label.
pub fn build_metric_cell(
    entry: &SnapshotEntry,
    explanation: Option<&Explanation>,
    catalog: &CatalogSnapshot,
    now_boot_ns: u64,
) -> MetricCell {
    let sample = &entry.sample;
    let state = aged(cell_state(sample, explanation), entry, now_boot_ns);
    let has_value = state.has_value();
    MetricCell {
        value: if has_value { sample.value } else { None },
        unit: sample.unit,
        state,
        kind: sample.sample_kind,
        quality: sample.quality,
        age_ms: Some(age_ms(now_boot_ns, sample.sample_time_boot_ns())),
        source_label: if has_value {
            selected_source_label(sample, explanation, catalog)
        } else {
            None
        },
        conflict: if state == CellState::Conflict {
            conflict_view(explanation)
        } else {
            None
        },
        last_good: if has_value {
            None
        } else {
            last_good_view(entry, now_boot_ns)
        },
    }
}

/// A value nobody refreshed is stale once its deadline passed: a hung
/// backend commits nothing, so the commit-time verdict would otherwise stay
/// "ok" with an ever-growing age (AT24).
fn aged(state: CellState, entry: &SnapshotEntry, now_boot_ns: u64) -> CellState {
    let expired = entry
        .stale_after_boot_ns
        .is_some_and(|deadline| now_boot_ns >= deadline);
    if state.has_value() && expired {
        CellState::Stale
    } else {
        state
    }
}

/// Canonical unit of a metric id from the catalog; ids used by the builders
/// are all registered (checked by tests), `count` is only a defensive default.
pub(crate) fn canonical_unit_of(metric_id: &str) -> CanonicalUnit {
    descriptor(metric_id).map_or(CanonicalUnit::Count, |entry| entry.unit)
}

/// Cell for a series that has no committed sample at all.
pub(crate) fn missing_cell(metric_id: &str) -> MetricCell {
    MetricCell::missing(canonical_unit_of(metric_id), CellState::Unavailable)
}

fn cell_state(sample: &CanonicalSample, explanation: Option<&Explanation>) -> CellState {
    match sample.selection_state {
        SelectionState::Selected if sample.value.is_some() => CellState::Ok,
        SelectionState::Degraded if sample.value.is_some() => CellState::Degraded,
        SelectionState::Conflict => CellState::Conflict,
        _ => unavailable_state(sample, explanation),
    }
}

/// Most specific explanation for a value-less series: unsupported (every
/// candidate), permission denied, read failed, stale, warming up, else
/// plain unavailable (no eligible candidate).
fn unavailable_state(sample: &CanonicalSample, explanation: Option<&Explanation>) -> CellState {
    let candidates: &[CandidateExplanation] =
        explanation.map_or(&[], |explained| explained.candidates.as_slice());
    let explanation_codes = explanation
        .into_iter()
        .flat_map(|explained| explained.reason_codes.iter());
    let reasons = ReasonSignals::scan(sample.reason_codes.iter().chain(explanation_codes));
    let any_status = |wanted: &[ReadStatus]| {
        candidates
            .iter()
            .any(|candidate| wanted.contains(&effective_read_status(candidate)))
    };
    if all_unsupported(candidates) || (candidates.is_empty() && reasons.unsupported) {
        CellState::Unsupported
    } else if any_status(&[ReadStatus::PermissionDenied]) || reasons.permission_denied {
        CellState::PermissionDenied
    } else if any_status(&[ReadStatus::Timeout, ReadStatus::Error, ReadStatus::Removed])
        || reasons.read_failed
    {
        CellState::ReadFailed
    } else if candidates.iter().any(is_stale) || is_stale_sample(sample) || reasons.stale {
        CellState::Stale
    } else if reasons.warmup {
        CellState::Warmup
    } else {
        CellState::Unavailable
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReasonSignals {
    unsupported: bool,
    permission_denied: bool,
    read_failed: bool,
    stale: bool,
    warmup: bool,
}

impl ReasonSignals {
    fn scan<'r>(codes: impl IntoIterator<Item = &'r String>) -> Self {
        let mut signals = Self::default();
        for code in codes {
            let code = code.to_ascii_uppercase();
            let has = |markers: &[&str]| markers.iter().any(|marker| code.contains(marker));
            signals.unsupported |= has(&["UNSUPPORTED", "NOT_SUPPORTED"]);
            signals.permission_denied |= has(&["PERMISSION"]);
            signals.read_failed |= has(&READ_FAILURE_MARKERS);
            signals.stale |= has(&["STALE"]);
            signals.warmup |= has(&WARMUP_MARKERS);
        }
        signals
    }
}

fn effective_read_status(candidate: &CandidateExplanation) -> ReadStatus {
    match &candidate.disposition {
        CandidateDisposition::Rejected(RejectReason::ReadFailed { status }) => *status,
        _ => candidate.read_status,
    }
}

fn all_unsupported(candidates: &[CandidateExplanation]) -> bool {
    !candidates.is_empty()
        && candidates
            .iter()
            .all(|candidate| effective_read_status(candidate) == ReadStatus::Unsupported)
}

fn is_stale(candidate: &CandidateExplanation) -> bool {
    candidate.freshness == Freshness::Stale
        || candidate.measurement_freshness == Freshness::Stale
        || matches!(
            candidate.disposition,
            CandidateDisposition::Rejected(RejectReason::Stale)
        )
}

fn is_stale_sample(sample: &CanonicalSample) -> bool {
    sample.freshness == Freshness::Stale || sample.measurement_freshness == Freshness::Stale
}

/// "package 0 · hwmon": scope of the series plus the selected source's adapter.
fn selected_source_label(
    sample: &CanonicalSample,
    explanation: Option<&Explanation>,
    catalog: &CatalogSnapshot,
) -> Option<String> {
    let source_id = sample.source_id.as_ref()?;
    let adapter = catalog
        .source(source_id)
        .map(|source| source.adapter_id.as_str())
        .or_else(|| {
            explanation?
                .candidates
                .iter()
                .find(|candidate| &candidate.source_id == source_id)
                .map(|candidate| candidate.adapter_id.as_str())
        })
        .unwrap_or_else(|| adapter_prefix(source_id.as_str()));
    Some(format!(
        "{} · {}",
        humanize_scope(sample.key.scope.as_str()),
        label_text(adapter)
    ))
}

/// Both readings of a conflict ("hwmon 68°C / thermal 42°C"); two sources
/// of one adapter are told apart by their full source id.
fn conflict_view(explanation: Option<&Explanation>) -> Option<ConflictView> {
    let conflicting: Vec<&CandidateExplanation> = explanation?
        .candidates
        .iter()
        .filter(|candidate| {
            candidate.disposition == CandidateDisposition::Conflicting && candidate.value.is_some()
        })
        .take(MAX_CONFLICT_ENTRIES)
        .collect();
    let entries: Vec<_> = conflicting
        .iter()
        .filter_map(|candidate| {
            let shared_adapter = conflicting
                .iter()
                .filter(|other| other.adapter_id == candidate.adapter_id)
                .count()
                > 1;
            let label = if shared_adapter {
                label_text(candidate.source_id.as_str())
            } else {
                label_text(candidate.adapter_id.as_str())
            };
            Some((label, candidate.value?))
        })
        .collect();
    (!entries.is_empty()).then_some(ConflictView { entries })
}

fn last_good_view(entry: &SnapshotEntry, now_boot_ns: u64) -> Option<LastGoodView> {
    let last_good = entry
        .last_good
        .as_ref()
        .filter(|last_good| last_good.unit == entry.sample.unit)?;
    Some(LastGoodView {
        value: last_good.value,
        age_ms: age_ms(now_boot_ns, last_good.sample_boot_ns),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{candidate, explanation_for, sample};
    use dgxtop_core::quality::Quality;

    const NOW: u64 = 50_000_000_000;

    fn cell(entry: &SnapshotEntry, explanation: Option<&Explanation>) -> MetricCell {
        build_metric_cell(entry, explanation, &CatalogSnapshot::default(), NOW)
    }

    #[test]
    fn true_zero_is_a_value_not_missing() {
        let entry = sample("cpu.utilization.total", "cpu", "host")
            .f64(0.0)
            .build();
        let built = cell(&entry, None);
        assert_eq!(built.state, CellState::Ok);
        assert_eq!(built.value_f64(), Some(0.0));
        assert_eq!(built.unit, CanonicalUnit::Percent);
        assert_eq!(built.source_label.as_deref(), Some("host · procfs"));
    }

    #[test]
    fn degraded_keeps_value_and_age_comes_from_sample_time() {
        let entry = sample("cpu.temperature.package", "cpu.package.0", "package:0")
            .f64(68.0)
            .state(SelectionState::Degraded)
            .source("hwmon:coretemp.0:temp1")
            .read_at(NOW - 1_500_000_000)
            .build();
        let built = cell(&entry, None);
        assert_eq!(built.state, CellState::Degraded);
        assert_eq!(built.value_f64(), Some(68.0));
        assert_eq!(built.age_ms, Some(1500));
        assert_eq!(built.source_label.as_deref(), Some("package 0 · hwmon"));
    }

    #[test]
    fn selected_without_value_is_never_shown_as_value() {
        let mut entry = sample("memory.used", "memory", "system").f64(1.0).build();
        entry.sample.value = None;
        let built = cell(&entry, None);
        assert_eq!(built.state, CellState::Unavailable);
        assert_eq!(built.value, None);
    }

    #[test]
    fn stale_and_warmup_and_plain_unavailable() {
        let entry = sample("cpu.temperature.package", "cpu.package.0", "package:0")
            .unavailable()
            .build();
        let key = entry.sample.key.clone();
        let stale = explanation_for(
            &key,
            vec![
                candidate("hwmon:x:temp1", "hwmon")
                    .freshness(Freshness::Stale)
                    .disposition(CandidateDisposition::Rejected(RejectReason::Stale))
                    .build(),
            ],
        );
        let built = cell(&entry, Some(&stale));
        assert_eq!(built.state, CellState::Stale);
        assert_eq!(built.state.label(), "stale");

        let warmup = sample("disk.read.bytes_per_second", "blockdev:8:0", "device")
            .unavailable()
            .reason("WARMUP")
            .build();
        assert_eq!(cell(&warmup, None).state, CellState::Warmup);
        let reset = sample("network.rx.bytes_per_second", "netif:ns:2", "interface")
            .unavailable()
            .reason("COUNTER_RESET")
            .build();
        assert_eq!(cell(&reset, None).state, CellState::Warmup);

        let suspect_only = explanation_for(
            &key,
            vec![
                candidate("hwmon:x:temp1", "hwmon")
                    .quality(Quality::Suspect)
                    .disposition(CandidateDisposition::Rejected(
                        RejectReason::QualityNotEligible {
                            quality: Quality::Suspect,
                        },
                    ))
                    .build(),
            ],
        );
        assert_eq!(
            cell(&entry, Some(&suspect_only)).state,
            CellState::Unavailable
        );
        let unsupported_reason = sample("gpu.fan.speed", "gpu:GPU-a", "gpu")
            .unavailable()
            .reason("NVML_NOT_SUPPORTED")
            .build();
        assert_eq!(
            cell(&unsupported_reason, None).state,
            CellState::Unsupported
        );
    }

    #[test]
    fn conflict_lists_both_readings_without_a_value() {
        let entry = sample("cpu.temperature.package", "cpu.package.0", "package:0")
            .state(SelectionState::Conflict)
            .build();
        let explanation = explanation_for(
            &entry.sample.key,
            vec![
                candidate("hwmon:coretemp:temp1", "hwmon")
                    .value(68.0)
                    .disposition(CandidateDisposition::Conflicting)
                    .build(),
                candidate("thermal:zone2:temp", "thermal")
                    .value(42.0)
                    .disposition(CandidateDisposition::Conflicting)
                    .build(),
            ],
        );
        let built = cell(&entry, Some(&explanation));
        assert_eq!(built.state, CellState::Conflict);
        assert_eq!(built.value, None);
        assert_eq!(built.source_label, None);
        let entries = built.conflict.unwrap().entries;
        let labels: Vec<(&str, f64)> = entries
            .iter()
            .map(|(label, value)| (label.as_str(), value.to_f64_lossy()))
            .collect();
        assert_eq!(labels, vec![("hwmon", 68.0), ("thermal", 42.0)]);
    }

    #[test]
    fn last_good_annotates_missing_values_only() {
        let missing = sample("gpu.temperature", "gpu:GPU-a", "gpu")
            .unavailable()
            .reason("TIMEOUT")
            .last_good(61.0, NOW - 12_000_000_000)
            .build();
        let built = cell(&missing, None);
        assert_eq!(built.state, CellState::ReadFailed);
        assert_eq!(built.value, None);
        let last_good = built.last_good.unwrap();
        assert_eq!(last_good.value.to_f64_lossy(), 61.0);
        assert_eq!(last_good.age_ms, 12_000);

        let present = sample("gpu.temperature", "gpu:GPU-a", "gpu")
            .f64(62.0)
            .last_good(61.0, NOW)
            .build();
        assert!(cell(&present, None).last_good.is_none());

        let mut wrong_unit = missing.clone();
        if let Some(last_good) = wrong_unit.last_good.as_mut() {
            last_good.unit = CanonicalUnit::Watts;
        }
        assert!(cell(&wrong_unit, None).last_good.is_none());
    }

    #[test]
    fn a_value_past_its_stale_deadline_is_stale_and_kept_only_as_last_good() {
        // A backend that stopped answering commits nothing: its last value
        // ages past the deadline stamped at commit (AT24).
        let mut entry = sample("gpu.temperature", "gpu:GPU-a", "gpu")
            .f64(64.0)
            .read_at(NOW - 4_000_000_000)
            .last_good(64.0, NOW - 4_000_000_000)
            .stale_after(NOW - 3_100_000_000)
            .build();
        let built = cell(&entry, None);
        assert_eq!(built.state, CellState::Stale);
        assert_eq!(built.value, None);
        let last_good = built
            .last_good
            .expect("the old reading stays visible as last good");
        assert_eq!(last_good.value.to_f64_lossy(), 64.0);
        assert_eq!(last_good.age_ms, 4_000);
        // `now >= deadline` is stale; before it the value is current.
        entry.stale_after_boot_ns = Some(NOW);
        assert_eq!(cell(&entry, None).state, CellState::Stale);
        entry.stale_after_boot_ns = Some(NOW + 1);
        assert_eq!(cell(&entry, None).state, CellState::Ok);
    }

    #[test]
    fn missing_cells_use_catalog_units() {
        let built = missing_cell("gpu.power.draw");
        assert_eq!(built.unit, CanonicalUnit::Watts);
        assert_eq!(built.state, CellState::Unavailable);
        assert_eq!(built.value, None);
        assert_eq!(canonical_unit_of("not.registered"), CanonicalUnit::Count);
    }
}
