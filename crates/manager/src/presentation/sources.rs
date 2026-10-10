//! Sources module projection: arbitration evidence per explained series
//! (candidates, dispositions, freshness, origin, policy) plus unclassified
//! diagnostics channels. The list is filtered, windowed around the selected
//! series and bounded in candidates.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;

use dgxtop_core::canonical::{
    CandidateDisposition, CandidateExplanation, CatalogSnapshot, Explanation, ImmutableSnapshot,
    RejectReason, SwitchExplanation,
};
use dgxtop_core::identity::{MetricKey, SourceId};
use dgxtop_core::metric::{CanonicalUnit, descriptor, normalize};
use dgxtop_core::naming::entity;
use dgxtop_core::observation::{Observation, SourceDescriptor};
use dgxtop_core::quality::{Freshness, Quality};
use dgxtop_core::view::{
    CandidateView, HostStatus, IntervalView, SessionState, SourceMetricView, SourcesView,
};

use super::cell::canonical_unit_of;
use super::gpu_directory::{GpuDirectory, is_gpu_entity};
use super::labels::{
    TextNeedle, age_ms, filter_text, humanize_scope, label_text, native_text, note_text,
    origin_text, read_status_label,
};
use super::reader::MetricReader;

/// Explained series listed at once.
pub(crate) const MAX_SOURCE_METRICS: usize = 48;
/// Candidate rows across the listed series.
pub(crate) const MAX_METRIC_CANDIDATES: usize = 160;
/// Unclassified diagnostic channels listed at once.
pub(crate) const MAX_UNCLASSIFIED: usize = 40;
const MAX_CANDIDATES_PER_METRIC: usize = 8;
const MAX_HOSTS: usize = 16;
const MAX_INTERVALS: usize = 64;
const MAX_SUSPECT_REASONS: usize = 8;
const SENSOR_METRIC_PREFIX: &str = "sensor.";
const UNPROVEN_PURPOSE: &str = "unclassified: purpose unproven";
const DIAGNOSTICS_ONLY: &str = "unclassified: diagnostics only";
const NOT_READ_YET: &str = "not read yet";

pub(crate) struct SourcesInputs<'a> {
    pub(crate) reader: &'a MetricReader<'a>,
    pub(crate) directory: &'a GpuDirectory<'a>,
    pub(crate) session: &'a SessionState,
    pub(crate) hosts: &'a [HostStatus],
    pub(crate) intervals: &'a [IntervalView],
}

struct ListedMetric<'a> {
    selector: String,
    label: String,
    explanation: &'a Explanation,
}

pub(crate) fn build_sources_view(inputs: &SourcesInputs<'_>) -> SourcesView {
    let reader = inputs.reader;
    let filter = TextNeedle::new(&inputs.session.filter);
    let explanations = all_explanations(reader.snapshot());
    let (sensor, classified): (Vec<&Explanation>, Vec<&Explanation>) = explanations
        .iter()
        .partition(|explanation| is_sensor_metric(&explanation.key));
    let listing = listed_metrics(&classified, &filter, reader.catalog(), inputs.directory);
    let wanted = inputs
        .session
        .selected_entity_id
        .as_ref()
        .map(|id| id.as_str());
    let selected = listing
        .iter()
        .position(|listed| Some(listed.selector.as_str()) == wanted);
    let costs: Vec<usize> = listing
        .iter()
        .map(|listed| {
            listed
                .explanation
                .candidates
                .len()
                .clamp(1, MAX_CANDIDATES_PER_METRIC)
        })
        .collect();
    let window = window_around(
        &costs,
        selected.unwrap_or(0),
        MAX_SOURCE_METRICS,
        MAX_METRIC_CANDIDATES,
    );
    let now = reader.now_boot_ns();
    SourcesView {
        metrics: listing
            .get(window)
            .unwrap_or_default()
            .iter()
            .map(|listed| metric_view(listed, now))
            .collect(),
        hosts: inputs
            .hosts
            .iter()
            .take(MAX_HOSTS)
            .map(bounded_host)
            .collect(),
        intervals: inputs
            .intervals
            .iter()
            .take(MAX_INTERVALS)
            .map(bounded_interval)
            .collect(),
        policy_revision: explanations
            .iter()
            .map(|explanation| explanation.policy_revision)
            .max()
            .unwrap_or(0),
        selected: selected.map(|index| listing[index].selector.clone()),
        filter: filter_text(&inputs.session.filter),
        unclassified: unclassified_views(&sensor, reader, &filter),
    }
}

fn all_explanations(snapshot: &ImmutableSnapshot) -> Vec<&Explanation> {
    snapshot
        .domains
        .values()
        .flat_map(|domain| domain.explanations.values())
        .map(|explanation| explanation.as_ref())
        .collect()
}

fn is_sensor_metric(key: &MetricKey) -> bool {
    key.metric_id.as_str().starts_with(SENSOR_METRIC_PREFIX)
}

fn listed_metrics<'a>(
    explanations: &[&'a Explanation],
    filter: &TextNeedle,
    catalog: &CatalogSnapshot,
    directory: &GpuDirectory<'_>,
) -> Vec<ListedMetric<'a>> {
    let mut listing: Vec<ListedMetric<'a>> = explanations
        .iter()
        .map(|explanation| ListedMetric {
            selector: explanation.key.series_selector(),
            label: series_label(&explanation.key, catalog, directory),
            explanation,
        })
        .filter(|listed| {
            filter.matches(&listed.label)
                || filter.matches(&listed.selector)
                || listed
                    .explanation
                    .candidates
                    .iter()
                    .any(|candidate| filter.matches(candidate.source_id.as_str()))
        })
        .collect();
    listing.sort_by(|left, right| left.selector.cmp(&right.selector));
    listing
}

/// Grows a window outward from `center` while it stays within `max_items`
/// rows and `budget` candidate rows; the center row is always included.
pub(crate) fn window_around(
    costs: &[usize],
    center: usize,
    max_items: usize,
    budget: usize,
) -> Range<usize> {
    let Some(last) = costs.len().checked_sub(1) else {
        return 0..0;
    };
    let center = center.min(last);
    let (mut start, mut end) = (center, center + 1);
    let mut used = costs[center];
    loop {
        let mut grew = false;
        if end - start < max_items && end < costs.len() && used + costs[end] <= budget {
            used += costs[end];
            end += 1;
            grew = true;
        }
        if end - start < max_items && start > 0 && used + costs[start - 1] <= budget {
            used += costs[start - 1];
            start -= 1;
            grew = true;
        }
        if !grew {
            return start..end;
        }
    }
}

/// "CPU package temperature · package 0", "GPU temperature · gpu · GPU 1".
fn series_label(
    key: &MetricKey,
    catalog: &CatalogSnapshot,
    directory: &GpuDirectory<'_>,
) -> String {
    let metric =
        descriptor(key.metric_id.as_str()).map_or(key.metric_id.as_str(), |entry| entry.label);
    let scope = humanize_scope(key.scope.as_str());
    match entity_label(key, catalog, directory) {
        Some(entity_name) => label_text(&format!("{metric} · {scope} · {entity_name}")),
        None => label_text(&format!("{metric} · {scope}")),
    }
}

/// Entities already named by their scope (host, aggregate CPU, packages,
/// logical cores, memory, SoC) are not repeated.
fn entity_label(
    key: &MetricKey,
    catalog: &CatalogSnapshot,
    directory: &GpuDirectory<'_>,
) -> Option<String> {
    let id = key.entity_id.as_str();
    let implied = matches!(
        id,
        entity::HOST | entity::CPU_AGGREGATE | entity::MEMORY | entity::SOC
    ) || id.starts_with("cpu.core.")
        || id.starts_with("cpu.package.");
    if implied {
        return None;
    }
    if is_gpu_entity(id) {
        return Some(directory.label_of(&key.entity_id));
    }
    let display = catalog
        .entity(&key.entity_id)
        .map(|record| record.display_name.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(id);
    Some(label_text(display))
}

fn metric_view(listed: &ListedMetric<'_>, now_boot_ns: u64) -> SourceMetricView {
    let explanation = listed.explanation;
    let unit = canonical_unit_of(explanation.key.metric_id.as_str());
    let mut candidates: Vec<&CandidateExplanation> = explanation.candidates.iter().collect();
    candidates.sort_by(|left, right| {
        disposition_rank(&left.disposition)
            .cmp(&disposition_rank(&right.disposition))
            .then(left.profile_rank.cmp(&right.profile_rank))
            .then_with(|| left.source_id.cmp(&right.source_id))
    });
    SourceMetricView {
        key: explanation.key.clone(),
        label: listed.label.clone(),
        selection_state: explanation.selection_state,
        selected_source: explanation.selected_source.as_ref().map(source_text),
        pinned_source: explanation.pinned_source.as_ref().map(source_text),
        policy_revision: explanation.policy_revision,
        conflict_spread: explanation
            .conflict_spread
            .filter(|spread| spread.is_finite()),
        tolerance: explanation
            .tolerance
            .filter(|tolerance| tolerance.is_finite()),
        candidates: candidates
            .into_iter()
            .take(MAX_CANDIDATES_PER_METRIC)
            .map(|candidate| candidate_view(candidate, unit, now_boot_ns))
            .collect(),
        switch_note: switch_note(explanation.switch.as_ref()),
    }
}

fn source_text(source_id: &SourceId) -> String {
    label_text(source_id.as_str())
}

const fn disposition_rank(disposition: &CandidateDisposition) -> u8 {
    match disposition {
        CandidateDisposition::Selected => 0,
        CandidateDisposition::Conflicting => 1,
        CandidateDisposition::Standby => 2,
        CandidateDisposition::Alias { .. } => 3,
        CandidateDisposition::Rejected(_) => 4,
    }
}

/// "selected", "standby", "alias of X", "conflicting", "rejected: code".
pub(crate) fn disposition_text(disposition: &CandidateDisposition) -> String {
    match disposition {
        CandidateDisposition::Selected => "selected".to_owned(),
        CandidateDisposition::Standby => "standby".to_owned(),
        CandidateDisposition::Alias { of } => format!("alias of {}", source_text(of)),
        CandidateDisposition::Conflicting => "conflicting".to_owned(),
        CandidateDisposition::Rejected(reason) => rejected_text(reason),
    }
}

fn rejected_text(reason: &RejectReason) -> String {
    match reason {
        RejectReason::ReadFailed { status } => {
            format!(
                "rejected: {} ({})",
                reason.code(),
                read_status_label(*status)
            )
        }
        RejectReason::QualityNotEligible { quality } => {
            format!("rejected: {} ({})", reason.code(), quality_label(*quality))
        }
        _ => format!("rejected: {}", reason.code()),
    }
}

const fn quality_label(quality: Quality) -> &'static str {
    match quality {
        Quality::Verified => "verified",
        Quality::Usable => "usable",
        Quality::Suspect => "suspect",
        Quality::Invalid => "invalid",
        Quality::Unknown => "unknown",
    }
}

fn candidate_view(
    candidate: &CandidateExplanation,
    unit: CanonicalUnit,
    now_boot_ns: u64,
) -> CandidateView {
    CandidateView {
        source_id: source_text(&candidate.source_id),
        adapter_id: label_text(candidate.adapter_id.as_str()),
        value: candidate.value,
        unit: candidate.value.map(|_| unit),
        native: native_text(
            candidate.native_value,
            &candidate.native_unit,
            candidate.read_status,
        ),
        quality: candidate.quality,
        kind: candidate.sample_kind,
        freshness: candidate.freshness,
        measurement_freshness: candidate.measurement_freshness,
        age_ms: Some(age_ms(
            now_boot_ns,
            candidate
                .observed_boot_ns
                .unwrap_or(candidate.read_finished_boot_ns),
        )),
        origin: origin_text(&candidate.origin),
        rank: candidate.profile_rank,
        disposition: disposition_text(&candidate.disposition),
        suspect_reasons: candidate
            .suspect_reasons
            .iter()
            .take(MAX_SUSPECT_REASONS)
            .map(|reason| label_text(reason))
            .collect(),
    }
}

/// Hysteresis progress of a pending source switch, if any.
fn switch_note(switch: Option<&SwitchExplanation>) -> Option<String> {
    let switch = switch?;
    let challenger = switch.challenger.as_ref()?;
    Some(note_text(&format!(
        "switch pending to {challenger}: {}/{} healthy reads, minimum hold {} ms",
        switch.challenger_streak, switch.required_streak, switch.min_hold_ms
    )))
}

fn bounded_host(status: &HostStatus) -> HostStatus {
    HostStatus {
        adapter_id: label_text(&status.adapter_id),
        detail: status.detail.as_deref().map(note_text),
        ..status.clone()
    }
}

fn bounded_interval(interval: &IntervalView) -> IntervalView {
    IntervalView {
        adapter_id: label_text(&interval.adapter_id),
        read_group: label_text(&interval.read_group),
        reason: note_text(&interval.reason),
        ..interval.clone()
    }
}

/// A diagnostics channel: an explained `sensor.*` candidate or a catalog
/// source without a proven purpose.
enum Unclassified<'a> {
    Explained(&'a CandidateExplanation, CanonicalUnit),
    Cataloged(&'a SourceDescriptor),
}

impl Unclassified<'_> {
    fn source_id(&self) -> &str {
        match self {
            Self::Explained(candidate, _) => candidate.source_id.as_str(),
            Self::Cataloged(source) => source.source_id.as_str(),
        }
    }

    fn adapter_id(&self) -> &str {
        match self {
            Self::Explained(candidate, _) => candidate.adapter_id.as_str(),
            Self::Cataloged(source) => source.adapter_id.as_str(),
        }
    }
}

fn unclassified_views(
    sensor: &[&Explanation],
    reader: &MetricReader<'_>,
    filter: &TextNeedle,
) -> Vec<CandidateView> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut channels: Vec<Unclassified<'_>> = Vec::new();
    for explanation in sensor {
        let unit = canonical_unit_of(explanation.key.metric_id.as_str());
        for candidate in &explanation.candidates {
            if seen.insert(candidate.source_id.as_str()) {
                channels.push(Unclassified::Explained(candidate, unit));
            }
        }
    }
    for source in reader.catalog().sources.values() {
        if is_unclassified_source(source) && seen.insert(source.source_id.as_str()) {
            channels.push(Unclassified::Cataloged(source));
        }
    }
    channels.retain(|channel| {
        filter.matches(channel.source_id()) || filter.matches(channel.adapter_id())
    });
    channels.sort_by(|left, right| left.source_id().cmp(right.source_id()));
    channels.truncate(MAX_UNCLASSIFIED);
    let now = reader.now_boot_ns();
    channels
        .iter()
        .map(|channel| match channel {
            Unclassified::Explained(candidate, unit) => candidate_view(candidate, *unit, now),
            Unclassified::Cataloged(source) => cataloged_view(source, reader),
        })
        .collect()
}

fn is_unclassified_source(source: &SourceDescriptor) -> bool {
    source.metric.as_ref().is_none_or(is_sensor_metric)
}

/// Latest raw reading of a catalog-only channel (from committed collector
/// state), converted only through an explicit, identified unit conversion.
fn cataloged_view(source: &SourceDescriptor, reader: &MetricReader<'_>) -> CandidateView {
    let observation = latest_observation(reader.snapshot(), &source.source_id);
    let unit = source
        .metric
        .as_ref()
        .map(|key| canonical_unit_of(key.metric_id.as_str()));
    let value = observation
        .and_then(|observation| observation.value)
        .zip(unit)
        .and_then(|(raw, unit)| normalize(raw, source.native_unit.as_str(), unit).ok())
        .map(|(value, _)| value)
        .or_else(|| committed_value(source, reader));
    CandidateView {
        source_id: source_text(&source.source_id),
        adapter_id: label_text(source.adapter_id.as_str()),
        value,
        unit: value.and(unit),
        native: observation.map_or_else(
            || NOT_READ_YET.to_owned(),
            |observation| {
                native_text(
                    observation.value,
                    &observation.native_unit,
                    observation.read_status,
                )
            },
        ),
        quality: observation.map_or(Quality::Unknown, |observation| observation.quality),
        kind: source.sample_kind,
        freshness: Freshness::Unknown,
        measurement_freshness: Freshness::Unknown,
        age_ms: observation.map(|observation| {
            age_ms(
                reader.now_boot_ns(),
                observation
                    .device_sample_boot_ns
                    .unwrap_or(observation.read_finished_boot_ns),
            )
        }),
        origin: origin_text(&source.origin),
        rank: source.profile_rank,
        disposition: if source.metric.is_none() {
            UNPROVEN_PURPOSE.to_owned()
        } else {
            DIAGNOSTICS_ONLY.to_owned()
        },
        suspect_reasons: Vec::new(),
    }
}

fn latest_observation<'a>(
    snapshot: &'a ImmutableSnapshot,
    source_id: &SourceId,
) -> Option<&'a Observation> {
    snapshot
        .collector_states
        .values()
        .find_map(|(_, state)| state.candidates.get(source_id).map(Arc::as_ref))
}

fn committed_value(
    source: &SourceDescriptor,
    reader: &MetricReader<'_>,
) -> Option<dgxtop_core::metric::Scalar> {
    let entry = reader.snapshot().sample(source.metric.as_ref()?)?;
    let sample = &entry.sample;
    (sample.source_id.as_ref() == Some(&source.source_id) && sample.selection_state.has_value())
        .then_some(sample.value)
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::index::SampleIndex;
    use crate::presentation::testkit::{Fixture, NOW, candidate, explanation_for, host_status};

    use dgxtop_core::identity::EntityId;
    use dgxtop_core::metric::Scalar;

    use dgxtop_core::quality::ReadStatus;
    use dgxtop_core::testing::{metric_key, source_id};
    use dgxtop_core::view::HostState;

    fn build(fixture: &Fixture) -> SourcesView {
        let reader = MetricReader::new(&fixture.snapshot, &fixture.catalog, &fixture.trends, NOW);
        let index = SampleIndex::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        build_sources_view(&SourcesInputs {
            reader: &reader,
            directory: &directory,
            session: &fixture.session,
            hosts: &fixture.hosts,
            intervals: &fixture.intervals,
        })
    }

    fn package_explanation() -> Explanation {
        let key = metric_key("cpu.package.0", "cpu.temperature.package", "package:0");
        let mut explanation = explanation_for(
            &key,
            vec![
                candidate("thermal:zone2:temp", "thermal")
                    .value(42.0)
                    .rank(2)
                    .disposition(CandidateDisposition::Rejected(RejectReason::ReadFailed {
                        status: ReadStatus::Timeout,
                    }))
                    .build(),
                candidate("hwmon:coretemp:temp1", "hwmon")
                    .value(68.0)
                    .native(Scalar::from_u64(68000), "mdegC")
                    .origin("sensor:package0")
                    .disposition(CandidateDisposition::Selected)
                    .suspect("constant_value")
                    .build(),
                candidate("hwmon:coretemp:temp1b", "hwmon")
                    .disposition(CandidateDisposition::Alias {
                        of: source_id("hwmon:coretemp:temp1"),
                    })
                    .build(),
                candidate("acpi:tz0:temp", "thermal").rank(5).build(),
            ],
        );
        explanation.switch = Some(SwitchExplanation {
            challenger: Some(source_id("acpi:tz0:temp")),
            challenger_streak: 1,
            required_streak: 3,
            held_since_boot_ns: 0,
            min_hold_ms: 5000,
        });
        explanation
    }

    #[test]
    fn candidates_show_dispositions_origin_and_evidence() {
        let mut fixture = Fixture::new();
        fixture.explain("cpu", package_explanation());
        fixture.hosts = vec![host_status("hwmon", HostState::Healthy, None)];
        fixture.intervals = vec![IntervalView {
            adapter_id: "hwmon".into(),
            read_group: "fast".into(),
            configured_ms: 1000,
            effective_ms: 1000,
            reason: "configured".into(),
        }];
        let view = build(&fixture);
        assert_eq!(view.metrics.len(), 1);
        let metric = &view.metrics[0];
        assert_eq!(metric.label, "CPU package temperature · package 0");
        assert_eq!(
            metric.selected_source.as_deref(),
            Some("hwmon:coretemp:temp1")
        );
        let dispositions: Vec<&str> = metric
            .candidates
            .iter()
            .map(|candidate| candidate.disposition.as_str())
            .collect();
        assert_eq!(
            dispositions,
            vec![
                "selected",
                "standby",
                "alias of hwmon:coretemp:temp1",
                "rejected: read_failed (timeout)",
            ]
        );
        let selected = &metric.candidates[0];
        assert_eq!(selected.native, "68000 mdegC");
        assert_eq!(selected.origin, "confirmed sensor:package0");
        assert_eq!(selected.unit, Some(CanonicalUnit::DegC));
        assert_eq!(selected.suspect_reasons, vec!["constant_value".to_owned()]);
        assert_eq!(selected.age_ms, Some(100));
        assert_eq!(metric.candidates[1].origin, "unknown");
        assert_eq!(metric.candidates[1].unit, None);
        assert_eq!(
            metric.switch_note.as_deref(),
            Some("switch pending to acpi:tz0:temp: 1/3 healthy reads, minimum hold 5000 ms")
        );
        assert_eq!(view.policy_revision, 1);
        assert_eq!(view.hosts.len(), 1);
        assert_eq!(view.intervals.len(), 1);
    }

    #[test]
    fn conflicting_and_quality_dispositions_are_named() {
        assert_eq!(
            disposition_text(&CandidateDisposition::Conflicting),
            "conflicting"
        );
        assert_eq!(
            disposition_text(&CandidateDisposition::Rejected(
                RejectReason::QualityNotEligible {
                    quality: Quality::Suspect,
                }
            )),
            "rejected: quality_not_eligible (suspect)"
        );
        assert_eq!(
            disposition_text(&CandidateDisposition::Rejected(RejectReason::Stale)),
            "rejected: stale"
        );
    }

    #[test]
    fn filter_selection_and_window_bound_the_list() {
        let mut fixture = Fixture::new();
        for core in 0..120_u32 {
            let key = metric_key(
                &format!("cpu.core.{core}"),
                "cpu.utilization.total",
                &format!("core:{core}"),
            );
            fixture.explain(
                "cpu",
                explanation_for(
                    &key,
                    vec![
                        candidate(&format!("procfs:stat:cpu{core}"), "procfs")
                            .disposition(CandidateDisposition::Selected)
                            .build(),
                    ],
                ),
            );
        }
        let all = build(&fixture);
        assert_eq!(all.metrics.len(), MAX_SOURCE_METRICS);
        assert_eq!(all.selected, None);

        let wanted =
            metric_key("cpu.core.77", "cpu.utilization.total", "core:77").series_selector();
        fixture.session.selected_entity_id = Some(EntityId::new(wanted.as_str()).unwrap());
        let around = build(&fixture);
        assert_eq!(around.selected.as_deref(), Some(wanted.as_str()));
        assert!(
            around
                .metrics
                .iter()
                .any(|metric| metric.key.series_selector() == wanted)
        );
        assert!(around.metrics.len() <= MAX_SOURCE_METRICS);

        fixture.session.filter = "CPU77".to_owned();
        let filtered = build(&fixture);
        assert_eq!(filtered.metrics.len(), 1);
        assert_eq!(filtered.filter, "CPU77");
        assert_eq!(filtered.selected.as_deref(), Some(wanted.as_str()));
    }

    #[test]
    fn empty_snapshot_yields_an_empty_bounded_view() {
        let view = build(&Fixture::new());
        assert!(view.metrics.is_empty());
        assert!(view.unclassified.is_empty());
        assert_eq!(view.policy_revision, 0);
    }
}
