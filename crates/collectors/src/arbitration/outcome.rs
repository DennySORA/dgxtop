//! Step 9: the explainable result — canonical value, per-candidate
//! dispositions, hysteresis memo and source events.

use dgxtop_core::canonical::{
    CandidateDisposition, CandidateExplanation, CanonicalValue, CollectorEvent, Explanation,
    PolicySnapshot, RejectReason, SelectionMemo, SwitchExplanation,
};
use dgxtop_core::identity::{MetricKey, SourceId};
use dgxtop_core::metric::MetricDescriptor;
use dgxtop_core::quality::{Freshness, Namespace, Quality, SampleKind, SelectionState};

use super::consistency::Consistency;
use super::ranking::Ranking;
use super::{Assessed, Decision, Outcome, Selection};
use crate::reasons;
use crate::values::{bounded_candidate_ids, history_eligible};

pub(super) struct Parts<'r, 'c, 'a> {
    pub key: &'r MetricKey,
    pub descriptor: &'static MetricDescriptor,
    pub cycle_id: u64,
    pub now_boot_ns: u64,
    pub policy: &'r PolicySnapshot,
    pub memo: Option<&'r SelectionMemo>,
    pub assessed: &'r [Assessed<'c, 'a>],
    pub ranking: &'r Ranking,
    pub consistency: &'r Consistency,
    pub decision: Decision,
    pub notes: Vec<String>,
    pub dropped: Vec<SourceId>,
}

pub(super) fn build(parts: Parts<'_, '_, '_>) -> Outcome {
    let state = selection_state(&parts.decision);
    let memo = next_memo(&parts);
    Outcome {
        canonical: canonical_value(&parts, state),
        explanation: explanation(&parts, state, memo.as_ref()),
        events: events(&parts),
        selection: selection(&parts),
        has_namespace_evidence: parts
            .assessed
            .iter()
            .any(|entry| entry.rejection() != Some(&RejectReason::DifferentNamespace)),
        memo,
        dropped_candidates: parts.dropped,
    }
}

fn selection_state(decision: &Decision) -> SelectionState {
    match decision {
        Decision::Selected { degraded: true, .. } => SelectionState::Degraded,
        Decision::Selected { .. } => SelectionState::Selected,
        Decision::Conflict => SelectionState::Conflict,
        Decision::Unavailable { .. } => SelectionState::Unavailable,
    }
}

fn canonical_value(parts: &Parts<'_, '_, '_>, state: SelectionState) -> CanonicalValue {
    match &parts.decision {
        Decision::Selected {
            index, degraded, ..
        } => selected_value(parts, *index, *degraded, state),
        Decision::Conflict => conflict_value(parts),
        Decision::Unavailable { pinned } => unavailable_value(parts, *pinned),
    }
}

fn selected_value(
    parts: &Parts<'_, '_, '_>,
    index: usize,
    degraded: bool,
    state: SelectionState,
) -> CanonicalValue {
    let entry = &parts.assessed[index];
    let observation = entry.candidate.observation;
    let degradation = degraded.then_some(reasons::PREFERRED_SOURCE_FAILED);
    let reason_codes = reasons::bounded(
        observation
            .reason_codes
            .iter()
            .map(String::as_str)
            .chain(degradation),
    );
    CanonicalValue {
        key: parts.key.clone(),
        cycle_id: parts.cycle_id,
        source_id: Some(observation.provenance.source_id.clone()),
        source_generation: Some(observation.provenance.generation),
        unit: parts.descriptor.unit,
        value: entry.value(),
        sample_kind: observation.sample_kind,
        selection_state: state,
        quality: observation.quality,
        freshness: entry.assessment.freshness,
        measurement_freshness: entry.assessment.measurement_freshness,
        observed_boot_ns: observation.device_sample_boot_ns,
        read_finished_boot_ns: observation.read_finished_boot_ns,
        policy_revision: parts.policy.revision,
        candidate_source_ids: candidate_ids(parts),
        reason_codes,
        eligible_for_history: history_eligible(
            parts.descriptor,
            observation.sample_kind,
            parts.policy.namespace,
        ),
    }
}

fn conflict_value(parts: &Parts<'_, '_, '_>) -> CanonicalValue {
    let read_finished = parts
        .consistency
        .conflicting
        .iter()
        .map(|index| {
            parts.assessed[*index]
                .candidate
                .observation
                .read_finished_boot_ns
        })
        .max()
        .unwrap_or(parts.now_boot_ns);
    let read_time_only = parts
        .consistency
        .notes
        .contains(&reasons::ALIGNED_BY_READ_TIME)
        .then_some(reasons::ALIGNED_BY_READ_TIME);
    let codes = std::iter::once(reasons::SOURCE_DISAGREEMENT).chain(read_time_only);
    no_value(
        parts,
        SelectionState::Conflict,
        Freshness::Fresh,
        read_finished,
        reasons::bounded(codes),
    )
}

fn unavailable_value(parts: &Parts<'_, '_, '_>, pinned: bool) -> CanonicalValue {
    let codes = if pinned {
        pinned_codes(parts)
    } else {
        unavailable_codes(parts)
    };
    let in_namespace = || {
        parts
            .assessed
            .iter()
            .filter(|entry| entry.rejection() != Some(&RejectReason::DifferentNamespace))
    };
    let read_finished = in_namespace()
        .map(|entry| entry.candidate.observation.read_finished_boot_ns)
        .max()
        .unwrap_or(parts.now_boot_ns);
    let freshness = latest_successful_read(in_namespace());
    no_value(
        parts,
        SelectionState::Unavailable,
        freshness,
        read_finished,
        codes,
    )
}

fn pinned_codes(parts: &Parts<'_, '_, '_>) -> Vec<String> {
    let pinned_rejection = parts
        .policy
        .pinned_sources
        .get(parts.key)
        .and_then(|pinned| {
            parts
                .assessed
                .iter()
                .find(|entry| entry.candidate.source_id() == pinned)
        })
        .and_then(|entry| {
            entry.rejection().map(|reason| {
                reasons::reject_code(reason, entry.candidate.observation.native_unit.as_str())
            })
        });
    reasons::bounded(std::iter::once(reasons::PINNED_SOURCE_UNAVAILABLE).chain(pinned_rejection))
}

fn unavailable_codes(parts: &Parts<'_, '_, '_>) -> Vec<String> {
    let rejections = policy_order(parts).into_iter().filter_map(|index| {
        let entry = &parts.assessed[index];
        entry.rejection().map(|reason| {
            reasons::reject_code(reason, entry.candidate.observation.native_unit.as_str())
        })
    });
    reasons::bounded(
        std::iter::once(reasons::NO_ELIGIBLE_CANDIDATE)
            .chain(rejections)
            .map(str::to_owned)
            .chain(parts.notes.iter().cloned()),
    )
}

/// Freshness of the latest successful read (the canonical meaning of
/// `freshness` when no value is selected).
fn latest_successful_read<'e, 'c: 'e, 'a: 'c>(
    entries: impl Iterator<Item = &'e Assessed<'c, 'a>>,
) -> Freshness {
    let mut saw_success = false;
    for entry in entries {
        if entry.candidate.observation.read_status.is_ok() {
            if entry.assessment.freshness == Freshness::Fresh {
                return Freshness::Fresh;
            }
            saw_success = true;
        }
    }
    if saw_success {
        Freshness::Stale
    } else {
        Freshness::Unknown
    }
}

fn no_value(
    parts: &Parts<'_, '_, '_>,
    state: SelectionState,
    freshness: Freshness,
    read_finished_boot_ns: u64,
    reason_codes: Vec<String>,
) -> CanonicalValue {
    CanonicalValue {
        key: parts.key.clone(),
        cycle_id: parts.cycle_id,
        source_id: None,
        source_generation: None,
        unit: parts.descriptor.unit,
        value: None,
        sample_kind: namespace_kind(parts),
        selection_state: state,
        quality: Quality::Unknown,
        freshness,
        measurement_freshness: Freshness::Unknown,
        observed_boot_ns: None,
        read_finished_boot_ns,
        policy_revision: parts.policy.revision,
        candidate_source_ids: candidate_ids(parts),
        reason_codes,
        eligible_for_history: false,
    }
}

/// Sample kind for a value-less result: the policy-preferred candidate of
/// the policy namespace, else the namespace's own default kind.
fn namespace_kind(parts: &Parts<'_, '_, '_>) -> SampleKind {
    policy_order(parts)
        .into_iter()
        .map(|index| &parts.assessed[index])
        .find(|entry| entry.rejection() != Some(&RejectReason::DifferentNamespace))
        .map_or_else(
            || match parts.policy.namespace {
                Namespace::Live => SampleKind::Measured,
                Namespace::Replay => SampleKind::Replay,
            },
            |entry| entry.candidate.observation.sample_kind,
        )
}

/// Candidate indices by profile rank, then source id.
fn policy_order(parts: &Parts<'_, '_, '_>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..parts.assessed.len()).collect();
    order.sort_by(|left, right| {
        let (left, right) = (&parts.assessed[*left], &parts.assessed[*right]);
        (left.candidate.profile_rank, left.candidate.source_id())
            .cmp(&(right.candidate.profile_rank, right.candidate.source_id()))
    });
    order
}

fn candidate_ids(parts: &Parts<'_, '_, '_>) -> Vec<SourceId> {
    bounded_candidate_ids(parts.assessed.iter().flat_map(|entry| {
        std::iter::once(entry.candidate.source_id()).chain(entry.candidate.input_source_ids.iter())
    }))
}

fn explanation(
    parts: &Parts<'_, '_, '_>,
    state: SelectionState,
    memo: Option<&SelectionMemo>,
) -> Explanation {
    let selected_source = match &parts.decision {
        Decision::Selected { index, .. } => {
            Some(parts.assessed[*index].candidate.source_id().clone())
        }
        Decision::Conflict | Decision::Unavailable { .. } => None,
    };
    let compared = parts.consistency.comparisons > 0;
    Explanation {
        key: parts.key.clone(),
        policy_revision: parts.policy.revision,
        selection_state: state,
        selected_source,
        pinned_source: parts.policy.pinned_sources.get(parts.key).cloned(),
        candidates: display_order(parts)
            .into_iter()
            .map(|index| candidate_explanation(parts, index))
            .collect(),
        conflict_spread: parts.consistency.spread.filter(|_| compared),
        tolerance: parts.consistency.tolerance.filter(|_| compared),
        switch: memo.and_then(|memo| switch_explanation(memo, parts.policy)),
        reason_codes: explanation_codes(parts),
    }
}

fn explanation_codes(parts: &Parts<'_, '_, '_>) -> Vec<String> {
    let decision_codes: Vec<&str> = match &parts.decision {
        Decision::Selected {
            degraded,
            held_challenger,
            ..
        } => [
            degraded.then_some(reasons::PREFERRED_SOURCE_FAILED),
            held_challenger.map(|_| reasons::HELD_BY_HYSTERESIS),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Decision::Conflict => vec![reasons::SOURCE_DISAGREEMENT],
        Decision::Unavailable { pinned: true } => vec![reasons::PINNED_SOURCE_UNAVAILABLE],
        Decision::Unavailable { pinned: false } => vec![reasons::NO_ELIGIBLE_CANDIDATE],
    };
    let limit = (!parts.dropped.is_empty()).then_some(reasons::CANDIDATE_LIMIT_EXCEEDED);
    reasons::bounded(
        decision_codes
            .into_iter()
            .chain(parts.consistency.notes.iter().copied())
            .chain(limit)
            .map(str::to_owned)
            .chain(parts.notes.iter().cloned()),
    )
}

/// Eligible candidates best-first, then rejected ones in policy order.
fn display_order(parts: &Parts<'_, '_, '_>) -> Vec<usize> {
    let rejected = policy_order(parts)
        .into_iter()
        .filter(|index| !parts.assessed[*index].is_eligible());
    parts
        .ranking
        .order
        .iter()
        .copied()
        .chain(rejected)
        .collect()
}

fn candidate_explanation(parts: &Parts<'_, '_, '_>, index: usize) -> CandidateExplanation {
    let entry = &parts.assessed[index];
    let observation = entry.candidate.observation;
    CandidateExplanation {
        source_id: observation.provenance.source_id.clone(),
        adapter_id: observation.provenance.adapter_id.clone(),
        origin: observation.provenance.origin(),
        profile_rank: entry.candidate.profile_rank,
        native_value: observation.value,
        native_unit: observation.native_unit.clone(),
        value: entry.assessment.value,
        conversion_id: entry.assessment.conversion_id.map(str::to_owned),
        read_status: observation.read_status,
        quality: observation.quality,
        sample_kind: observation.sample_kind,
        freshness: entry.assessment.freshness,
        measurement_freshness: entry.assessment.measurement_freshness,
        read_finished_boot_ns: observation.read_finished_boot_ns,
        observed_boot_ns: observation.device_sample_boot_ns,
        disposition: disposition(parts, index),
        suspect_reasons: entry.assessment.suspect_reasons.clone(),
    }
}

fn disposition(parts: &Parts<'_, '_, '_>, index: usize) -> CandidateDisposition {
    match &parts.decision {
        Decision::Selected {
            index: selected, ..
        } if *selected == index => {
            return CandidateDisposition::Selected;
        }
        Decision::Selected {
            held_challenger: Some(challenger),
            ..
        } if *challenger == index => {
            return CandidateDisposition::Rejected(RejectReason::Hysteresis);
        }
        Decision::Conflict if parts.consistency.conflicting.contains(&index) => {
            return CandidateDisposition::Conflicting;
        }
        _ => {}
    }
    let entry = &parts.assessed[index];
    match (entry.rejection(), parts.ranking.alias_of[index]) {
        (Some(reason), _) => CandidateDisposition::Rejected(reason.clone()),
        (None, Some(representative)) => CandidateDisposition::Alias {
            of: parts.assessed[representative].candidate.source_id().clone(),
        },
        (None, None) => CandidateDisposition::Standby,
    }
}

fn switch_explanation(memo: &SelectionMemo, policy: &PolicySnapshot) -> Option<SwitchExplanation> {
    memo.selected.as_ref().map(|_| SwitchExplanation {
        challenger: memo.challenger.clone(),
        challenger_streak: memo.challenger_streak,
        required_streak: policy.switch_successes.max(1),
        held_since_boot_ns: memo.selected_since_boot_ns,
        min_hold_ms: policy.switch_min_hold_ms,
    })
}

/// A round without a value (conflict or unavailable) ends the current
/// selection: the next valid round selects afresh, and nobody's winning
/// streak survives a round that withheld the value.
fn next_memo(parts: &Parts<'_, '_, '_>) -> Option<SelectionMemo> {
    match &parts.decision {
        Decision::Selected { memo, .. } => Some(memo.clone()),
        Decision::Conflict | Decision::Unavailable { .. } => None,
    }
}

fn events(parts: &Parts<'_, '_, '_>) -> Vec<CollectorEvent> {
    let previous = parts.memo.and_then(|memo| memo.selected.as_ref());
    let mut events = Vec::new();
    match &parts.decision {
        Decision::Selected { index, .. } => {
            let selected = parts.assessed[*index].candidate.source_id();
            if let Some(previous) = previous.filter(|previous| *previous != selected) {
                events.extend(unavailable_event(parts, previous));
                events.push(CollectorEvent::SourceChanged {
                    key: parts.key.clone(),
                    previous_source: Some(previous.clone()),
                    source_id: selected.clone(),
                    policy_revision: parts.policy.revision,
                });
            }
        }
        Decision::Conflict => {
            let mut source_ids: Vec<SourceId> = parts
                .consistency
                .conflicting
                .iter()
                .map(|index| parts.assessed[*index].candidate.source_id().clone())
                .collect();
            source_ids.sort();
            events.push(CollectorEvent::SourceConflict {
                key: parts.key.clone(),
                source_ids,
            });
        }
        Decision::Unavailable { .. } => {
            if let Some(previous) = previous {
                events.extend(unavailable_event(parts, previous));
            }
        }
    }
    events
}

/// `SourceUnavailable` for the previously selected source when it is present
/// and rejected this round.
fn unavailable_event(parts: &Parts<'_, '_, '_>, previous: &SourceId) -> Option<CollectorEvent> {
    let entry = parts
        .assessed
        .iter()
        .find(|entry| entry.candidate.source_id() == previous)?;
    let reason = entry.rejection()?;
    Some(CollectorEvent::SourceUnavailable {
        source_id: previous.clone(),
        reason: reasons::reject_code(reason, entry.candidate.observation.native_unit.as_str())
            .to_owned(),
    })
}

fn selection(parts: &Parts<'_, '_, '_>) -> Option<Selection> {
    let Decision::Selected { index, .. } = &parts.decision else {
        return None;
    };
    let entry = &parts.assessed[*index];
    let observation = entry.candidate.observation;
    Some(Selection {
        source_id: observation.provenance.source_id.clone(),
        generation: observation.provenance.generation,
        value: entry.value()?,
        quality: observation.quality,
        sample_kind: observation.sample_kind,
        read_finished_boot_ns: observation.read_finished_boot_ns,
        observed_boot_ns: observation.device_sample_boot_ns,
    })
}
