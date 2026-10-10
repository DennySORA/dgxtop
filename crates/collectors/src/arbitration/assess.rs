//! Steps 1–4 for one candidate: semantic mapping, normalization, hard
//! validation and freshness. A failure here excludes the candidate from
//! canonical state but keeps it as evidence in the explanation.

use dgxtop_core::canonical::{PolicySnapshot, RejectReason};
use dgxtop_core::identity::{MetricKey, OriginId, OriginRelation};
use dgxtop_core::metric::{CanonicalUnit, MetricDescriptor, Scalar, conversion_for};
use dgxtop_core::naming;
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::{Freshness, Namespace, RawKind};

use super::{Candidate, CatalogMapping, Deadlines};
use crate::reasons;

/// Metric id prefix of unclassified channels (diagnostics only).
const SENSOR_PREFIX: &str = "sensor.";

/// Evaluation of one candidate.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Assessment {
    /// Canonical value when the native unit converts (kept even when the
    /// candidate is rejected later, as evidence).
    pub value: Option<Scalar>,
    pub conversion_id: Option<&'static str>,
    pub freshness: Freshness,
    pub measurement_freshness: Freshness,
    /// Normalized value when eligible, else why it is not.
    pub verdict: Result<Scalar, RejectReason>,
    pub suspect_reasons: Vec<String>,
}

/// Evaluates `candidate` for `key`.
pub(crate) fn assess(
    candidate: &Candidate<'_>,
    key: &MetricKey,
    descriptor: &MetricDescriptor,
    policy: &PolicySnapshot,
    now_boot_ns: u64,
) -> Assessment {
    let observation = candidate.observation;
    let normalized = normalize(observation, descriptor.unit);
    let freshness = read_freshness(candidate.deadlines, now_boot_ns);
    let measurement_freshness =
        measurement_freshness(observation, candidate.deadlines, now_boot_ns);
    // The unit is checked before the quality grade: a reading whose unit is
    // unverified cannot be interpreted at all, whatever grade it carries.
    let verdict = check_source(candidate, key, policy)
        .and_then(|()| normalized.clone().map(|(value, _)| value))
        .and_then(|value| check_quality(observation).map(|()| value))
        .and_then(|value| check_freshness(freshness, measurement_freshness).map(|()| value));
    let (value, conversion_id) = match normalized {
        Ok((value, id)) => (Some(value), Some(id)),
        Err(_) => (None, None),
    };
    Assessment {
        value,
        conversion_id,
        freshness,
        measurement_freshness,
        verdict,
        suspect_reasons: suspect_reasons(candidate),
    }
}

/// Mapping, namespace, policy exclusion, read status, driver fault and
/// quality: everything except unit and freshness. Shared with formulas and
/// counters, which validate their inputs the same way.
pub(crate) fn check_admission(
    candidate: &Candidate<'_>,
    key: &MetricKey,
    policy: &PolicySnapshot,
) -> Result<(), RejectReason> {
    check_source(candidate, key, policy)?;
    check_quality(candidate.observation)
}

/// Mapping, namespace, policy exclusion, driver fault and read status.
fn check_source(
    candidate: &Candidate<'_>,
    key: &MetricKey,
    policy: &PolicySnapshot,
) -> Result<(), RejectReason> {
    check_mapping(candidate, key)?;
    check_namespace(candidate.observation, policy.namespace)?;
    check_policy(candidate, policy)?;
    check_fault(candidate.observation)?;
    check_read(candidate.observation)
}

fn check_mapping(candidate: &Candidate<'_>, key: &MetricKey) -> Result<(), RejectReason> {
    if key.metric_id.as_str().starts_with(SENSOR_PREFIX) {
        return Err(RejectReason::UnprovenPurpose);
    }
    if candidate.observation.key != *key {
        return Err(RejectReason::DifferentScope);
    }
    match candidate.mapping {
        CatalogMapping::Matches | CatalogMapping::NotCatalogued | CatalogMapping::Derived => Ok(()),
        CatalogMapping::KeyMismatch => Err(RejectReason::DifferentScope),
        CatalogMapping::UnprovenPurpose => Err(RejectReason::UnprovenPurpose),
    }
}

/// A record belongs to the policy namespace only if its kind does; a replay
/// adapter can never contribute live kinds, whatever it claims.
pub(crate) fn check_namespace(
    observation: &Observation,
    policy: Namespace,
) -> Result<(), RejectReason> {
    let kind_namespace = observation.sample_kind.namespace();
    let from_replay = naming::is_replay_adapter(observation.provenance.adapter_id.as_str());
    let mislabeled = from_replay && kind_namespace == Namespace::Live;
    if mislabeled || kind_namespace != policy {
        Err(RejectReason::DifferentNamespace)
    } else {
        Ok(())
    }
}

fn check_policy(candidate: &Candidate<'_>, policy: &PolicySnapshot) -> Result<(), RejectReason> {
    let excluded = policy.excluded_sources.contains(candidate.source_id())
        || candidate
            .input_source_ids
            .iter()
            .any(|input| policy.excluded_sources.contains(input));
    if excluded {
        Err(RejectReason::Excluded)
    } else {
        Ok(())
    }
}

/// A driver fault excludes the reading; an alarm never does.
fn check_fault(observation: &Observation) -> Result<(), RejectReason> {
    if has_reason(observation, reasons::SENSOR_FAULT) {
        Err(RejectReason::DriverFault)
    } else {
        Ok(())
    }
}

fn check_read(observation: &Observation) -> Result<(), RejectReason> {
    if !observation.read_status.is_ok() {
        return Err(RejectReason::ReadFailed {
            status: observation.read_status,
        });
    }
    if observation.value.is_none() {
        return Err(RejectReason::InvalidValue);
    }
    Ok(())
}

/// `allow_suspect_as_canonical` is a fixed `false`: only verified/usable.
fn check_quality(observation: &Observation) -> Result<(), RejectReason> {
    if observation.quality.is_canonical_eligible() {
        Ok(())
    } else {
        Err(RejectReason::QualityNotEligible {
            quality: observation.quality,
        })
    }
}

/// Explicit, identified conversion; unknown or unverified units are never
/// guessed.
fn normalize(
    observation: &Observation,
    unit: CanonicalUnit,
) -> Result<(Scalar, &'static str), RejectReason> {
    let raw = observation.value.ok_or(RejectReason::InvalidValue)?;
    let conversion =
        conversion_for(observation.native_unit.as_str(), unit).ok_or(RejectReason::UnknownUnit)?;
    let value = conversion
        .apply(raw)
        .map_err(|_| RejectReason::InvalidValue)?;
    Ok((value, conversion.id))
}

fn read_freshness(deadlines: Deadlines, now_boot_ns: u64) -> Freshness {
    if now_boot_ns >= deadlines.read_boot_ns {
        Freshness::Stale
    } else {
        Freshness::Fresh
    }
}

/// Fresh only with a device timestamp inside its deadline; without a device
/// timestamp a successful read proves nothing about the measurement.
fn measurement_freshness(
    observation: &Observation,
    deadlines: Deadlines,
    now_boot_ns: u64,
) -> Freshness {
    match (
        observation.device_sample_boot_ns,
        deadlines.measurement_boot_ns,
    ) {
        (Some(_), Some(deadline)) if now_boot_ns >= deadline => Freshness::Stale,
        (Some(_), Some(_)) => Freshness::Fresh,
        _ => Freshness::Unknown,
    }
}

fn check_freshness(read: Freshness, measurement: Freshness) -> Result<(), RejectReason> {
    if read == Freshness::Stale || measurement == Freshness::Stale {
        Err(RejectReason::Stale)
    } else {
        Ok(())
    }
}

/// Diagnostic suspicions only: they never invalidate a reading.
fn suspect_reasons(candidate: &Candidate<'_>) -> Vec<String> {
    let observation = candidate.observation;
    let mut suspicions = Vec::new();
    if observation
        .device_sample_boot_ns
        .is_some_and(|device| device > observation.read_finished_boot_ns)
    {
        suspicions.push(reasons::DEVICE_TIME_AFTER_READ.to_owned());
    }
    let unchanged = candidate.raw_kind == RawKind::Gauge
        && observation.value.is_some()
        && candidate.previous_value == observation.value;
    if unchanged {
        suspicions.push(reasons::VALUE_UNCHANGED_SINCE_LAST_READ.to_owned());
    }
    suspicions
}

pub(crate) fn has_reason(observation: &Observation, code: &str) -> bool {
    observation.reason_codes.iter().any(|reason| reason == code)
}

/// Origin id when a verified profile/topology confirmed it.
pub(crate) fn confirmed_origin(observation: &Observation) -> Option<&OriginId> {
    match observation.provenance.origin_relation {
        OriginRelation::Confirmed => observation.provenance.origin_id.as_ref(),
        OriginRelation::Unknown => None,
    }
}
