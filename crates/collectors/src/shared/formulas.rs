//! Building blocks shared by collectors: pass-through arbitration of pending
//! keys and formulas over arbitrated, time-aligned inputs.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::clock::ms_to_ns;
use dgxtop_core::identity::MetricKey;
use dgxtop_core::metric::Scalar;
use dgxtop_core::quality::Freshness;

use crate::shared::arbitration::Selection;
use crate::shared::context::Session;
use crate::shared::counter_inputs::is_fresh;
use crate::shared::reasons;
use crate::shared::values::{self, Evidence};

/// Selections already computed in this job, by key (`None`: no value).
pub(crate) type Selections = BTreeMap<MetricKey, Option<Selection>>;

/// Arbitrates and publishes every key with new evidence whose metric id is
/// accepted by `wanted`; remembers the selections for formulas.
pub(crate) fn publish_pending(
    session: &mut Session<'_>,
    wanted: impl Fn(&str) -> bool,
    selections: &mut Selections,
) {
    for key in session.pending_keys(|key| wanted(key.metric_id.as_str())) {
        let candidates = session.candidates(key);
        let selection = session.arbitrate_and_publish(key, candidates, Vec::new());
        selections.insert(key.clone(), selection);
    }
}

/// Selection of `key` for a formula: the one published in this job when
/// available, else a quiet arbitration over its current candidates.
pub(crate) fn selection_of(
    session: &mut Session<'_>,
    key: &MetricKey,
    selections: &mut Selections,
) -> Option<Selection> {
    if let Some(selection) = selections.get(key) {
        return selection.clone();
    }
    let candidates = session.candidates(key);
    let selection = if candidates.is_empty() {
        None
    } else {
        session
            .arbitrate(key, candidates, Vec::new())
            .and_then(|outcome| outcome.selection)
    };
    selections.insert(key.clone(), selection.clone());
    selection
}

/// Selected inputs of one formula, in the order of `keys`, sampled within
/// the alignment tolerance of each other.
pub(crate) fn aligned_inputs(
    session: &mut Session<'_>,
    keys: &[&MetricKey],
    selections: &mut Selections,
) -> Result<Vec<Selection>, &'static str> {
    let mut inputs = Vec::with_capacity(keys.len());
    for key in keys {
        inputs.push(selection_of(session, key, selections).ok_or(reasons::MISSING_INPUT)?);
    }
    let times = inputs.iter().map(Selection::sample_time_boot_ns);
    let earliest = times.clone().min().unwrap_or(0);
    let latest = times.max().unwrap_or(0);
    let tolerance = ms_to_ns(u64::from(session.policy().alignment_tolerance_ms));
    if latest - earliest > tolerance {
        return Err(reasons::INPUTS_NOT_ALIGNED);
    }
    Ok(inputs)
}

/// Whether any input key has a record delivered by this plan.
pub(crate) fn any_new(session: &Session<'_>, keys: &[&MetricKey]) -> bool {
    keys.iter().any(|key| session.has_new(key))
}

/// Newest record of each input key, for the provenance of an unavailable
/// formula value, and the freshness of the latest read among them.
pub(crate) fn input_evidence(
    session: &Session<'_>,
    keys: &[&MetricKey],
) -> (Vec<Evidence>, Freshness) {
    let mut evidence = Vec::new();
    let mut freshness = Freshness::Unknown;
    for key in keys {
        let newest = session
            .records(key)
            .into_iter()
            .max_by_key(|record| record.observation.read_finished_boot_ns);
        if let Some(record) = newest {
            evidence.push(Evidence::of(record.observation));
            freshness = match (freshness, is_fresh(session, record)) {
                (Freshness::Fresh, _) | (_, true) => Freshness::Fresh,
                _ => Freshness::Stale,
            };
        }
    }
    (evidence, freshness)
}

/// Publishes a formula result: a derived value with its input evidence, or
/// an unavailable value with the reason.
pub(crate) fn publish_formula(
    session: &mut Session<'_>,
    key: MetricKey,
    input_keys: &[&MetricKey],
    result: Result<(Scalar, Vec<Evidence>), &'static str>,
) {
    let context = session.context();
    let value = match result {
        Ok((value, evidence)) => values::derived(context, key, value, &evidence, &[]),
        Err(code) => {
            let (evidence, freshness) = input_evidence(session, input_keys);
            values::unavailable(context, key, &evidence, &[code], freshness)
        }
    };
    if let Some(value) = value {
        session.publish_value(value);
    }
}

/// Keys of a formula result: one per host/boot/entity/scope that has a
/// record of any input metric.
pub(crate) fn result_keys(session: &Session<'_>, inputs: &[&str], result: &str) -> Vec<MetricKey> {
    session
        .keys()
        .into_iter()
        .filter(|key| inputs.contains(&key.metric_id.as_str()))
        .filter_map(|key| values::with_metric(key, result).ok())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `result = minuend - subtrahend` over arbitrated inputs; a negative result
/// is unavailable with `negative` (never clamped to 0).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Difference {
    pub minuend: &'static str,
    pub subtrahend: &'static str,
    pub result: &'static str,
    pub negative: &'static str,
}

pub(crate) fn publish_differences(
    session: &mut Session<'_>,
    difference: Difference,
    selections: &mut Selections,
) {
    for result_key in result_keys(
        session,
        &[difference.minuend, difference.subtrahend],
        difference.result,
    ) {
        let (Ok(minuend), Ok(subtrahend)) = (
            values::with_metric(&result_key, difference.minuend),
            values::with_metric(&result_key, difference.subtrahend),
        ) else {
            continue;
        };
        if !any_new(session, &[&minuend, &subtrahend]) {
            continue;
        }
        let result =
            aligned_inputs(session, &[&subtrahend, &minuend], selections).and_then(|inputs| {
                let evidence: Vec<Evidence> = inputs.iter().map(Evidence::of_selection).collect();
                values::difference(inputs[1].value, inputs[0].value)
                    .map(|value| (value, evidence))
                    .ok_or(difference.negative)
            });
        publish_formula(session, result_key, &[&minuend, &subtrahend], result);
    }
}
