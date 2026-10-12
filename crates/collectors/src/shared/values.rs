//! Construction rules shared by every canonical value a collector derives
//! (rates, differences, ratios, formula outputs): sample kind, quality,
//! timing, candidate lists and history eligibility.

use dgxtop_core::canonical::{CanonicalSample, CanonicalValue, MAX_CANDIDATE_IDS};
use dgxtop_core::identity::{EntityId, IdentityError, MetricId, MetricKey, Scope, SourceId};
use dgxtop_core::metric::{MetricDescriptor, Scalar, ScalarView, descriptor};
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::{Freshness, Namespace, Quality, SampleKind, SelectionState};

use crate::shared::reasons;

/// Per-job constants stamped on every value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValueContext {
    pub cycle_id: u64,
    pub policy_revision: u32,
    pub namespace: Namespace,
}

/// What one formula input contributes to the provenance of a derived value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Evidence {
    pub source_id: SourceId,
    pub generation: u32,
    pub quality: Quality,
    pub sample_kind: SampleKind,
    pub read_finished_boot_ns: u64,
    pub observed_boot_ns: Option<u64>,
}

impl Evidence {
    pub fn of(observation: &Observation) -> Self {
        Self {
            source_id: observation.provenance.source_id.clone(),
            generation: observation.provenance.generation,
            quality: observation.quality,
            sample_kind: observation.sample_kind,
            read_finished_boot_ns: observation.read_finished_boot_ns,
            observed_boot_ns: observation.device_sample_boot_ns,
        }
    }

    /// A value this job published, as an input; `None` without the
    /// source that read it.
    pub fn of_value(value: &CanonicalValue) -> Option<Self> {
        Some(Self {
            source_id: value.source_id.clone()?,
            generation: value.source_generation?,
            quality: value.quality,
            sample_kind: value.sample_kind,
            read_finished_boot_ns: value.read_finished_boot_ns,
            observed_boot_ns: value.observed_boot_ns,
        })
    }

    /// A committed sample of another job, as an input; `None` without the
    /// source that read it.
    pub fn of_sample(sample: &CanonicalSample) -> Option<Self> {
        Some(Self {
            source_id: sample.source_id.clone()?,
            generation: sample.source_generation?,
            quality: sample.quality,
            sample_kind: sample.sample_kind,
            read_finished_boot_ns: sample.read_finished_boot_ns,
            observed_boot_ns: sample.observed_boot_ns,
        })
    }

    pub fn of_selection(selection: &crate::shared::arbitration::Selection) -> Self {
        Self {
            source_id: selection.source_id.clone(),
            generation: selection.generation,
            quality: selection.quality,
            sample_kind: selection.sample_kind,
            read_finished_boot_ns: selection.read_finished_boot_ns,
            observed_boot_ns: selection.observed_boot_ns,
        }
    }
}

/// History holds only live measured/derived/estimated values of metrics the
/// catalog marks as history series; replay and simulated data never enter.
pub(crate) fn history_eligible(
    metric: &MetricDescriptor,
    kind: SampleKind,
    namespace: Namespace,
) -> bool {
    metric.history && namespace == Namespace::Live && kind.namespace() == Namespace::Live
}

/// A value computed by a formula keeps the namespace of its inputs: from live
/// inputs it is `derived`, from replay/simulated inputs it stays replay.
pub(crate) fn derived_kind(inputs: &[Evidence]) -> SampleKind {
    inputs
        .iter()
        .map(|input| input.sample_kind)
        .find(|kind| kind.namespace() == Namespace::Replay)
        .unwrap_or(SampleKind::Derived)
}

/// Worst quality grade among inputs (a formula is as good as its weakest input).
pub(crate) fn weakest_quality(inputs: &[Evidence]) -> Quality {
    inputs
        .iter()
        .map(|input| input.quality)
        .max_by_key(|quality| quality.rank())
        .unwrap_or(Quality::Unknown)
}

/// Deduplicated candidate ids, capped at the schema maximum.
pub(crate) fn bounded_candidate_ids<'s>(
    ids: impl IntoIterator<Item = &'s SourceId>,
) -> Vec<SourceId> {
    let mut kept: Vec<SourceId> = Vec::new();
    for id in ids {
        if kept.len() == MAX_CANDIDATE_IDS {
            break;
        }
        if !kept.contains(id) {
            kept.push(id.clone());
        }
    }
    kept
}

/// Selected derived value. `inputs[0]` is the primary input whose source id
/// and generation identify the value; all inputs are listed as candidates.
/// `None` when the metric is unknown or there is no input.
pub(crate) fn derived(
    context: ValueContext,
    key: MetricKey,
    value: Scalar,
    inputs: &[Evidence],
    reason_codes: &[&str],
) -> Option<CanonicalValue> {
    let metric = descriptor(key.metric_id.as_str())?;
    let primary = inputs.first()?;
    let kind = derived_kind(inputs);
    Some(CanonicalValue {
        key,
        cycle_id: context.cycle_id,
        source_id: Some(primary.source_id.clone()),
        source_generation: Some(primary.generation),
        unit: metric.unit,
        value: Some(value),
        sample_kind: kind,
        selection_state: SelectionState::Selected,
        quality: weakest_quality(inputs),
        freshness: Freshness::Fresh,
        measurement_freshness: Freshness::Unknown,
        observed_boot_ns: None,
        read_finished_boot_ns: latest_read(inputs),
        policy_revision: context.policy_revision,
        candidate_source_ids: bounded_candidate_ids(inputs.iter().map(|input| &input.source_id)),
        reason_codes: reasons::bounded(reason_codes),
        eligible_for_history: history_eligible(metric, kind, context.namespace),
    })
}

/// Unavailable derived value with the reasons why no value exists.
pub(crate) fn unavailable(
    context: ValueContext,
    key: MetricKey,
    inputs: &[Evidence],
    reason_codes: &[&str],
    freshness: Freshness,
) -> Option<CanonicalValue> {
    let metric = descriptor(key.metric_id.as_str())?;
    let kind = if inputs.is_empty() {
        match context.namespace {
            Namespace::Live => SampleKind::Derived,
            Namespace::Replay => SampleKind::Replay,
        }
    } else {
        derived_kind(inputs)
    };
    Some(CanonicalValue {
        key,
        cycle_id: context.cycle_id,
        source_id: None,
        source_generation: None,
        unit: metric.unit,
        value: None,
        sample_kind: kind,
        selection_state: SelectionState::Unavailable,
        quality: Quality::Unknown,
        freshness,
        measurement_freshness: Freshness::Unknown,
        observed_boot_ns: None,
        read_finished_boot_ns: latest_read(inputs),
        policy_revision: context.policy_revision,
        candidate_source_ids: bounded_candidate_ids(inputs.iter().map(|input| &input.source_id)),
        reason_codes: reasons::bounded(reason_codes),
        eligible_for_history: false,
    })
}

fn latest_read(inputs: &[Evidence]) -> u64 {
    inputs
        .iter()
        .map(|input| input.read_finished_boot_ns)
        .max()
        .unwrap_or(0)
}

/// Same host/boot/entity/scope with another metric id.
pub(crate) fn with_metric(key: &MetricKey, metric_id: &str) -> Result<MetricKey, IdentityError> {
    Ok(MetricKey {
        metric_id: MetricId::new(metric_id)?,
        ..key.clone()
    })
}

/// Same host/boot with another entity, metric and scope.
pub(crate) fn with_series(
    key: &MetricKey,
    entity: EntityId,
    metric_id: &str,
    scope: Scope,
) -> Result<MetricKey, IdentityError> {
    Ok(MetricKey {
        host_id: key.host_id.clone(),
        boot_id: key.boot_id.clone(),
        entity_id: entity,
        metric_id: MetricId::new(metric_id)?,
        scope,
    })
}

/// `minuend - subtrahend` in exact arithmetic when both are `u64`; `None`
/// when the result would be negative (never clamped to 0).
pub(crate) fn difference(minuend: Scalar, subtrahend: Scalar) -> Option<Scalar> {
    match (minuend.view(), subtrahend.view()) {
        (ScalarView::U64(left), ScalarView::U64(right)) => {
            left.checked_sub(right).map(Scalar::from_u64)
        }
        _ => {
            let result = minuend.to_f64_lossy() - subtrahend.to_f64_lossy();
            if result < 0.0 {
                None
            } else {
                Scalar::from_f64(result).ok()
            }
        }
    }
}

/// `left + right`, exact for `u64` (checked), finite `f64` otherwise.
pub(crate) fn sum(left: Scalar, right: Scalar) -> Option<Scalar> {
    match (left.view(), right.view()) {
        (ScalarView::U64(a), ScalarView::U64(b)) => a.checked_add(b).map(Scalar::from_u64),
        _ => Scalar::from_f64(left.to_f64_lossy() + right.to_f64_lossy()).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::metric::ids;
    use dgxtop_core::testing::{metric_key, source_id};

    fn evidence(source: &str, kind: SampleKind, quality: Quality, read: u64) -> Evidence {
        Evidence {
            source_id: source_id(source),
            generation: 1,
            quality,
            sample_kind: kind,
            read_finished_boot_ns: read,
            observed_boot_ns: None,
        }
    }

    fn context(namespace: Namespace) -> ValueContext {
        ValueContext {
            cycle_id: 7,
            policy_revision: 1,
            namespace,
        }
    }

    #[test]
    fn derived_value_from_live_inputs_is_history_eligible_and_valid() {
        let inputs = [
            evidence(
                "procfs:memory:available",
                SampleKind::Measured,
                Quality::Verified,
                9,
            ),
            evidence(
                "procfs:memory:total",
                SampleKind::Measured,
                Quality::Usable,
                10,
            ),
        ];
        let value = derived(
            context(Namespace::Live),
            metric_key("memory", ids::MEMORY_USED, "system"),
            Scalar::from_u64(42),
            &inputs,
            &[],
        )
        .unwrap();
        value.validate().unwrap();
        assert_eq!(value.sample_kind, SampleKind::Derived);
        assert_eq!(value.quality, Quality::Usable);
        assert_eq!(value.read_finished_boot_ns, 10);
        assert_eq!(value.source_id, Some(source_id("procfs:memory:available")));
        assert!(value.eligible_for_history);
        assert_eq!(value.candidate_source_ids.len(), 2);
    }

    #[test]
    fn derived_value_from_replay_inputs_stays_replay_and_out_of_history() {
        let inputs = [evidence(
            "replay:a",
            SampleKind::Replay,
            Quality::Verified,
            1,
        )];
        let value = derived(
            context(Namespace::Replay),
            metric_key("memory", ids::MEMORY_USED, "system"),
            Scalar::from_u64(1),
            &inputs,
            &[],
        )
        .unwrap();
        value.validate().unwrap();
        assert_eq!(value.sample_kind, SampleKind::Replay);
        assert!(!value.eligible_for_history);
    }

    #[test]
    fn unavailable_value_carries_reasons_but_no_value() {
        let value = unavailable(
            context(Namespace::Live),
            metric_key("memory", ids::MEMORY_USED, "system"),
            &[],
            &[reasons::MISSING_INPUT],
            Freshness::Unknown,
        )
        .unwrap();
        value.validate().unwrap();
        assert!(value.value.is_none());
        assert_eq!(value.reason_codes, vec![reasons::MISSING_INPUT.to_owned()]);
    }

    #[test]
    fn difference_is_exact_and_never_negative() {
        assert_eq!(
            difference(Scalar::from_u64(u64::MAX), Scalar::from_u64(1))
                .unwrap()
                .as_u64(),
            Some(u64::MAX - 1)
        );
        assert!(difference(Scalar::from_u64(1), Scalar::from_u64(2)).is_none());
        assert!(
            difference(
                Scalar::from_f64(1.0).unwrap(),
                Scalar::from_f64(1.5).unwrap()
            )
            .is_none()
        );
        assert_eq!(
            difference(Scalar::from_u64(0), Scalar::from_u64(0))
                .unwrap()
                .as_u64(),
            Some(0)
        );
    }

    #[test]
    fn sum_is_checked() {
        assert!(sum(Scalar::from_u64(u64::MAX), Scalar::from_u64(1)).is_none());
        let total = sum(
            Scalar::from_f64(8.5).unwrap(),
            Scalar::from_f64(1.2).unwrap(),
        )
        .unwrap();
        assert!((total.to_f64_lossy() - 9.7).abs() < 1e-12);
    }

    #[test]
    fn candidate_ids_are_deduplicated_and_capped() {
        let ids: Vec<SourceId> = (0..40)
            .map(|index| source_id(&format!("s{index}")))
            .collect();
        let repeated = ids.iter().chain(ids.iter());
        assert_eq!(bounded_candidate_ids(repeated).len(), MAX_CANDIDATE_IDS);
    }
}
