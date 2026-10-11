//! CPU package and platform power.
//!
//! Candidates per power key, arbitrated together (origin de-duplication,
//! conflict, hysteresis):
//! * powercap energy counters: ΔµJ / dt = W over the actual interval
//!   (`derived`, with an exact joule `CounterInterval`); the derived
//!   candidate is retained under its energy source so other batches still
//!   see it;
//! * hwmon power channels mapped to `cpu.power.package` / `.platform`;
//! * GB10 profile formula: `cluster:p` + `cluster:e` when both are valid and
//!   time-aligned (`derived`, `FORMULA_GB10_CLUSTER_SUM`, both inputs
//!   listed); one component missing yields no derived value.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::clock::ms_to_ns;
use dgxtop_core::identity::{MetricKey, OriginRelation, SourceId};
use dgxtop_core::metric::{self, CanonicalUnit, NativeUnit, Scalar, ids, native};
use dgxtop_core::naming::entity;
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::{Freshness, RawKind, ReadStatus};

use crate::shared::arbitration::{self, Candidate, CatalogMapping, Deadlines};
use crate::shared::context::{Record, Session, carries_formula};
use crate::shared::counter_inputs::{self, Step};
use crate::shared::output::Subject;
use crate::shared::reasons;
use crate::shared::values::{self, Evidence};

const POWER_METRICS: [&str; 2] = [ids::CPU_POWER_PACKAGE, ids::CPU_POWER_PLATFORM];

/// Energy counter and the power metric derived from it.
const ENERGY_TO_POWER: [(&str, &str); 2] = [
    (ids::CPU_ENERGY_PACKAGE, ids::CPU_POWER_PACKAGE),
    (ids::CPU_ENERGY_PLATFORM, ids::CPU_POWER_PLATFORM),
];

const CLUSTER_P: &str = "cluster:p";
const CLUSTER_E: &str = "cluster:e";

/// One collector-built candidate.
#[derive(Debug, Clone)]
struct DerivedCandidate {
    observation: Observation,
    profile_rank: u16,
    deadlines: Deadlines,
    input_source_ids: Vec<SourceId>,
}

impl DerivedCandidate {
    fn candidate(&self) -> Candidate<'_> {
        Candidate {
            observation: &self.observation,
            profile_rank: self.profile_rank,
            raw_kind: RawKind::Gauge,
            mapping: CatalogMapping::Derived,
            deadlines: self.deadlines,
            input_source_ids: self.input_source_ids.clone(),
            previous_value: None,
        }
    }
}

/// Work of one job, per power key.
#[derive(Debug, Default)]
struct PowerWork {
    derived: BTreeMap<MetricKey, Vec<DerivedCandidate>>,
    notes: BTreeMap<MetricKey, Vec<String>>,
    evidence: BTreeMap<MetricKey, Vec<Evidence>>,
    /// Energy sources whose retained derived candidate is replaced or
    /// withdrawn by this job's reading.
    superseded: BTreeSet<SourceId>,
    /// Power keys with new formula evidence.
    pending: BTreeSet<MetricKey>,
}

impl PowerWork {
    fn note(&mut self, key: &MetricKey, code: &str) {
        self.notes
            .entry(key.clone())
            .or_default()
            .push(code.to_owned());
    }
}

pub(super) fn publish(session: &mut Session<'_>) {
    let mut work = PowerWork::default();
    derive_from_energy(session, &mut work);
    derive_from_clusters(session, &mut work);
    let mut keys = work.pending.clone();
    keys.extend(
        session
            .pending_keys(|key| POWER_METRICS.contains(&key.metric_id.as_str()))
            .into_iter()
            .cloned(),
    );
    for key in keys {
        arbitrate_power(session, &key, &work);
    }
}

// ----- energy counters ------------------------------------------------------

fn derive_from_energy(session: &mut Session<'_>, work: &mut PowerWork) {
    for (energy_metric, power_metric) in ENERGY_TO_POWER {
        for key in session.pending_keys(|key| key.metric_id.as_str() == energy_metric) {
            let Ok(power_key) = values::with_metric(key, power_metric) else {
                continue;
            };
            for record in session.records(key) {
                if record.is_new() {
                    energy_step(session, work, &power_key, record);
                }
            }
        }
    }
}

fn energy_step<'a>(
    session: &mut Session<'a>,
    work: &mut PowerWork,
    power_key: &MetricKey,
    record: Record<'a>,
) {
    let step = counter_inputs::step(session, record, Some(CanonicalUnit::Joules));
    if matches!(step, Step::Unchanged) {
        // Already consumed by an earlier job: its derived candidate stays.
        return;
    }
    work.pending.insert(power_key.clone());
    work.superseded.insert(record.source_id().clone());
    work.evidence
        .entry(power_key.clone())
        .or_default()
        .push(Evidence::of(record.observation));
    match step {
        Step::Delta {
            record,
            delta,
            conversion: Some(conversion),
        } => {
            counter_inputs::emit_interval(session, power_key, record, &delta, conversion);
            let wrapped = delta.wrapped.then_some(reasons::COUNTER_WRAPPED);
            let watts = delta
                .rate_per_second(conversion)
                .and_then(|rate| Scalar::from_f64(rate).ok());
            match watts.and_then(|watts| power_observation(record, power_key, Some(watts), wrapped))
            {
                Some(observation) => keep_derived(session, work, record, observation),
                None => {
                    work.note(power_key, reasons::VALUE_OVERFLOW);
                    session.forget(record.source_id(), power_key);
                }
            }
        }
        Step::Unusable { record, code } if !record.observation.read_status.is_ok() => {
            // A failed energy read makes the derived candidate fail too, so a
            // current selection falls back immediately.
            match power_observation(record, power_key, None, None) {
                Some(observation) => keep_derived(session, work, record, observation),
                None => work.note(power_key, code),
            }
        }
        Step::Unusable { record, code } => {
            work.note(power_key, code);
            session.forget(record.source_id(), power_key);
        }
        Step::Gap { record, gap } => {
            work.note(power_key, gap.reason_code());
            session.forget(record.source_id(), power_key);
        }
        Step::Delta { record, .. } => {
            work.note(power_key, reasons::UNKNOWN_UNIT);
            session.forget(record.source_id(), power_key);
        }
        Step::Unchanged => {}
    }
}

/// Watts derived from an energy record (`None`: the read failed).
fn power_observation(
    record: Record<'_>,
    power_key: &MetricKey,
    watts: Option<Scalar>,
    extra: Option<&str>,
) -> Option<Observation> {
    let source = record.observation;
    let mut observation = source.clone();
    observation.key = power_key.clone();
    observation.native_unit = NativeUnit::new(native::WATT).ok()?;
    observation.value = watts;
    observation.sample_kind = values::derived_kind(&[Evidence::of(source)]);
    observation.device_sample_boot_ns = None;
    let inherited = if watts.is_none() {
        source.reason_codes.clone()
    } else {
        Vec::new()
    };
    observation.reason_codes = reasons::bounded(
        std::iter::once(reasons::FORMULA_ENERGY_DELTA)
            .chain(extra)
            .map(str::to_owned)
            .chain(inherited),
    );
    if watts.is_some() {
        observation.read_status = ReadStatus::Ok;
    }
    Some(observation)
}

fn keep_derived(
    session: &mut Session<'_>,
    work: &mut PowerWork,
    record: Record<'_>,
    observation: Observation,
) {
    let derived = DerivedCandidate {
        profile_rank: rank_of(session, record.source_id()),
        deadlines: Deadlines {
            read_boot_ns: session.deadlines(record).read_boot_ns,
            measurement_boot_ns: None,
        },
        input_source_ids: Vec::new(),
        observation: observation.clone(),
    };
    session.retain(observation);
    work.derived
        .entry(derived.observation.key.clone())
        .or_default()
        .push(derived);
}

fn rank_of(session: &Session<'_>, source: &SourceId) -> u16 {
    session
        .source_descriptor(source)
        .map_or(0, |descriptor| descriptor.profile_rank)
}

// ----- GB10 cluster formula -------------------------------------------------

fn derive_from_clusters(session: &mut Session<'_>, work: &mut PowerWork) {
    let mut entities: BTreeMap<&str, (Option<&MetricKey>, Option<&MetricKey>)> = BTreeMap::new();
    for key in session.keys() {
        if key.metric_id.as_str() != ids::CPU_POWER_CLUSTER {
            continue;
        }
        let slot = entities.entry(key.entity_id.as_str()).or_default();
        match key.scope.as_str() {
            CLUSTER_P => slot.0 = Some(key),
            CLUSTER_E => slot.1 = Some(key),
            _ => {}
        }
    }
    for (entity_name, (p_key, e_key)) in entities {
        let reference = p_key.or(e_key);
        let Some(reference) = reference else {
            continue;
        };
        let triggered = [p_key, e_key]
            .into_iter()
            .flatten()
            .any(|key| session.has_new(key) || session.is_expiring(key));
        let Some(package_key) = package_key(reference, entity_name) else {
            if triggered {
                session.failure(
                    reasons::FORMULA_ENTITY_UNMAPPED,
                    &format!("GB10 clusters on {entity_name} do not name a CPU package"),
                    Subject::key(reference),
                );
            }
            continue;
        };
        if triggered {
            work.pending.insert(package_key.clone());
        }
        match cluster_sum(session, p_key, e_key, &package_key) {
            Ok(derived) => work.derived.entry(package_key).or_default().push(derived),
            Err((code, evidence)) => {
                work.note(&package_key, reasons::FORMULA_GB10_CLUSTER_SUM);
                work.note(&package_key, code);
                work.evidence
                    .entry(package_key)
                    .or_default()
                    .extend(evidence);
            }
        }
    }
}

/// `cpu.package.N` entity -> `(cpu.package.N, cpu.power.package, package:N)`.
fn package_key(reference: &MetricKey, entity_name: &str) -> Option<MetricKey> {
    let index: u32 = entity_name.strip_prefix("cpu.package.")?.parse().ok()?;
    values::with_series(
        reference,
        entity::cpu_package(index).ok()?,
        ids::CPU_POWER_PACKAGE,
        entity::scope_package(index).ok()?,
    )
    .ok()
}

type ClusterFailure = (&'static str, Vec<Evidence>);

fn cluster_sum(
    session: &Session<'_>,
    p_key: Option<&MetricKey>,
    e_key: Option<&MetricKey>,
    package_key: &MetricKey,
) -> Result<DerivedCandidate, ClusterFailure> {
    let p = newest(session, p_key);
    let e = newest(session, e_key);
    let evidence: Vec<Evidence> = [p, e]
        .into_iter()
        .flatten()
        .map(|record| Evidence::of(record.observation))
        .collect();
    let (Some(p), Some(e)) = (p, e) else {
        return Err((reasons::MISSING_INPUT, evidence));
    };
    let p_watts = valid_watts(session, p).map_err(|code| (code, evidence.clone()))?;
    let e_watts = valid_watts(session, e).map_err(|code| (code, evidence.clone()))?;
    if !aligned(session, p, e) {
        return Err((reasons::INPUTS_NOT_ALIGNED, evidence));
    }
    let total = values::sum(p_watts, e_watts).ok_or((reasons::VALUE_OVERFLOW, evidence.clone()))?;
    let observation = sum_observation(p, e, package_key, total, &evidence)
        .ok_or((reasons::VALUE_OVERFLOW, evidence))?;
    let deadline = session
        .deadlines(p)
        .read_boot_ns
        .min(session.deadlines(e).read_boot_ns);
    Ok(DerivedCandidate {
        observation,
        profile_rank: rank_of(session, p.source_id()).max(rank_of(session, e.source_id())),
        deadlines: Deadlines {
            read_boot_ns: deadline,
            measurement_boot_ns: None,
        },
        input_source_ids: vec![p.source_id().clone(), e.source_id().clone()],
    })
}

fn newest<'a>(session: &Session<'a>, key: Option<&MetricKey>) -> Option<Record<'a>> {
    session.records(key?).into_iter().max_by_key(|record| {
        (
            record.observation.read_finished_boot_ns,
            record.source_id().clone(),
        )
    })
}

/// The cluster reading in watts when it passes every arbitration check.
fn valid_watts(session: &Session<'_>, record: Record<'_>) -> Result<Scalar, &'static str> {
    let descriptor = metric::descriptor(ids::CPU_POWER_CLUSTER).ok_or(reasons::UNKNOWN_UNIT)?;
    let candidate = session.candidate(record);
    arbitration::assess(
        &candidate,
        record.key(),
        descriptor,
        session.policy(),
        session.now(),
    )
    .verdict
    .map_err(|reason| reasons::reject_code(&reason, record.observation.native_unit.as_str()))
}

fn aligned(session: &Session<'_>, p: Record<'_>, e: Record<'_>) -> bool {
    let tolerance = ms_to_ns(u64::from(session.policy().alignment_tolerance_ms));
    let (p, e) = (p.observation, e.observation);
    match (p.device_sample_boot_ns, e.device_sample_boot_ns) {
        (Some(a), Some(b)) => a.abs_diff(b) <= tolerance,
        _ => p.read_finished_boot_ns.abs_diff(e.read_finished_boot_ns) <= tolerance,
    }
}

fn sum_observation(
    p: Record<'_>,
    e: Record<'_>,
    package_key: &MetricKey,
    total: Scalar,
    evidence: &[Evidence],
) -> Option<Observation> {
    let mut observation = p.observation.clone();
    observation.key = package_key.clone();
    observation.native_unit = NativeUnit::new(native::WATT).ok()?;
    observation.value = Some(total);
    observation.read_status = ReadStatus::Ok;
    observation.sample_kind = values::derived_kind(evidence);
    observation.quality = values::weakest_quality(evidence);
    observation.read_started_boot_ns = p
        .observation
        .read_started_boot_ns
        .min(e.observation.read_started_boot_ns);
    observation.read_finished_boot_ns = p
        .observation
        .read_finished_boot_ns
        .max(e.observation.read_finished_boot_ns);
    observation.device_sample_boot_ns = None;
    // A sum of two channels has no single physical origin.
    observation.provenance.origin_id = None;
    observation.provenance.origin_relation = OriginRelation::Unknown;
    observation.reason_codes = vec![reasons::FORMULA_GB10_CLUSTER_SUM.to_owned()];
    Some(observation)
}

// ----- arbitration ----------------------------------------------------------

fn arbitrate_power(session: &mut Session<'_>, key: &MetricKey, work: &PowerWork) {
    let mut candidates: Vec<Candidate<'_>> = session
        .records(key)
        .into_iter()
        .filter(|record| !work.superseded.contains(record.source_id()))
        .map(|record| power_candidate(session, record))
        .collect();
    if let Some(derived) = work.derived.get(key) {
        candidates.extend(derived.iter().map(DerivedCandidate::candidate));
    }
    let notes = work.notes.get(key).cloned().unwrap_or_default();
    if candidates.is_empty() {
        publish_without_candidates(session, key, work, &notes);
        return;
    }
    session.arbitrate_and_publish(key, candidates, notes);
}

/// A retained energy-derived candidate is recognised by its formula marker
/// and the energy mapping of its source; everything else is a plain record.
fn power_candidate<'a>(session: &Session<'a>, record: Record<'a>) -> Candidate<'a> {
    let mut candidate = session.candidate(record);
    let energy_source = session
        .source_descriptor(record.source_id())
        .and_then(|descriptor| descriptor.metric.as_ref())
        .is_some_and(|metric| {
            ENERGY_TO_POWER
                .iter()
                .any(|(energy, _)| metric.metric_id.as_str() == *energy)
        });
    if energy_source && carries_formula(record.observation, reasons::FORMULA_ENERGY_DELTA) {
        candidate.mapping = CatalogMapping::Derived;
        candidate.raw_kind = RawKind::Gauge;
        candidate.deadlines.measurement_boot_ns = None;
    }
    candidate
}

fn publish_without_candidates(
    session: &mut Session<'_>,
    key: &MetricKey,
    work: &PowerWork,
    notes: &[String],
) {
    if notes.is_empty() {
        return;
    }
    let codes: Vec<&str> = notes.iter().map(String::as_str).collect();
    let evidence = work.evidence.get(key).cloned().unwrap_or_default();
    if let Some(value) = values::unavailable(
        session.context(),
        key.clone(),
        &evidence,
        &codes,
        Freshness::Unknown,
    ) {
        session.publish_value(value);
    }
}
