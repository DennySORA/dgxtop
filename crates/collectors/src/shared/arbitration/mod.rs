//! Pure source arbitration for one `MetricKey` (ARCHITECTURE.md §5.4,
//! `contracts/INTERFACES.md` §3).
//!
//! Steps: (1) semantic mapping, (2) normalization, (3) hard validation,
//! (4) freshness, (5) origin de-duplication, (6) ordering, (7) inconsistency
//! check, (8) hysteresis and (9) an explainable result. The engine performs no
//! I/O, holds no state and is deterministic: the outcome does not depend on
//! the order in which candidates are offered.
//!
//! Bounds: at most [`MAX_CANDIDATES_PER_SERIES`] candidates per key, therefore
//! at most 6 pair comparisons, and `O(C log C)` ordering.

mod assess;
mod consistency;
mod hysteresis;
mod outcome;
mod ranking;

use std::cmp::Reverse;

use dgxtop_core::canonical::{
    CanonicalValue, Explanation, PolicySnapshot, RejectReason, SelectionMemo,
};
use dgxtop_core::identity::{MetricKey, SourceId};
use dgxtop_core::metric::{MetricDescriptor, Scalar};
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::{Quality, RawKind, SampleKind};

pub(crate) use assess::{Assessment, assess, check_admission, check_namespace};

use self::consistency::Consistency;
use self::ranking::Ranking;

/// Hard cap on candidates per series (RUNTIME.md §2).
pub const MAX_CANDIDATES_PER_SERIES: usize = 4;

/// How the catalog relates a candidate's source to the arbitrated key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogMapping {
    /// The catalog maps the source to exactly this key.
    Matches,
    /// The source is not catalogued (process records, replay): defaults apply.
    NotCatalogued,
    /// The catalog maps the source to a different key.
    KeyMismatch,
    /// The catalog lists the source without a proven purpose (`metric: None`).
    UnprovenPurpose,
    /// Output of a collector formula over catalogued inputs.
    Derived,
}

/// Freshness deadlines resolved for one candidate (`now >= deadline` is stale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    pub read_boot_ns: u64,
    /// Deadline of the hardware measurement when the source reports a device
    /// timestamp; `None` means the measurement freshness is unknown.
    pub measurement_boot_ns: Option<u64>,
}

/// One candidate offered to the arbitrator.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub observation: &'a Observation,
    /// Policy order from the profile (lower first); not a probability.
    pub profile_rank: u16,
    pub raw_kind: RawKind,
    pub mapping: CatalogMapping,
    pub deadlines: Deadlines,
    /// Inputs of a derived candidate, listed as candidate source ids.
    pub input_source_ids: Vec<SourceId>,
    /// Value of the previous, older reading of the same source (diagnostics).
    pub previous_value: Option<Scalar>,
}

impl<'a> Candidate<'a> {
    pub fn source_id(&self) -> &'a SourceId {
        &self.observation.provenance.source_id
    }
}

/// Everything the arbitrator needs for one key.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub key: &'a MetricKey,
    pub descriptor: &'static MetricDescriptor,
    pub cycle_id: u64,
    pub now_boot_ns: u64,
    pub policy: &'a PolicySnapshot,
    /// Committed hysteresis memo of this key.
    pub memo: Option<&'a SelectionMemo>,
    pub candidates: Vec<Candidate<'a>>,
    /// `limits.max_candidates_per_series`; capped at [`MAX_CANDIDATES_PER_SERIES`].
    pub max_candidates: usize,
    /// Collector notes (e.g. a formula input warming up), shown in the
    /// explanation and, when no value results, in the canonical reasons.
    pub notes: Vec<String>,
}

/// The chosen value, for formulas built on top of an arbitrated key.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub source_id: SourceId,
    pub generation: u32,
    pub value: Scalar,
    pub quality: Quality,
    pub sample_kind: SampleKind,
    pub read_finished_boot_ns: u64,
    pub observed_boot_ns: Option<u64>,
}

impl Selection {
    /// Time the value is attributed to: device time if known, else read time.
    pub fn sample_time_boot_ns(&self) -> u64 {
        self.observed_boot_ns.unwrap_or(self.read_finished_boot_ns)
    }
}

/// Result of arbitrating one key.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub canonical: CanonicalValue,
    pub explanation: Explanation,
    /// Next hysteresis memo (`None` clears it).
    pub memo: Option<SelectionMemo>,
    pub selection: Option<Selection>,
    /// Whether any candidate belongs to the policy namespace. Without such
    /// evidence the key must not be published in this namespace at all.
    pub has_namespace_evidence: bool,
    /// Candidates beyond the per-series limit (not evaluated).
    pub dropped_candidates: Vec<SourceId>,
}

/// One candidate with its evaluation.
#[derive(Debug, Clone)]
pub(crate) struct Assessed<'c, 'a> {
    pub candidate: &'c Candidate<'a>,
    pub assessment: Assessment,
}

impl Assessed<'_, '_> {
    pub fn value(&self) -> Option<Scalar> {
        self.assessment.verdict.as_ref().ok().copied()
    }

    pub fn rejection(&self) -> Option<&RejectReason> {
        self.assessment.verdict.as_ref().err()
    }

    pub fn is_eligible(&self) -> bool {
        self.assessment.verdict.is_ok()
    }
}

/// What the decision step concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Selected {
        index: usize,
        degraded: bool,
        pinned: bool,
        held_challenger: Option<usize>,
        memo: SelectionMemo,
    },
    Conflict,
    Unavailable {
        pinned: bool,
    },
}

/// Arbitrates one key. See the module documentation for the steps.
pub fn arbitrate(request: Request<'_>) -> Outcome {
    let Request {
        key,
        descriptor,
        cycle_id,
        now_boot_ns,
        policy,
        memo,
        candidates,
        max_candidates,
        notes,
    } = request;
    let pinned = policy.pinned_sources.get(key);
    let current = memo.and_then(|memo| memo.selected.as_ref());
    let (kept, dropped) = limit_candidates(candidates, max_candidates, pinned, current);
    let assessed: Vec<Assessed<'_, '_>> = kept
        .iter()
        .map(|candidate| Assessed {
            candidate,
            assessment: assess(candidate, key, descriptor, policy, now_boot_ns),
        })
        .collect();
    let ranking = ranking::rank(&assessed);
    let consistency = consistency::check(&assessed, &ranking, descriptor.unit, policy);
    let decision = decide(&DecisionInput {
        assessed: &assessed,
        ranking: &ranking,
        consistency: &consistency,
        pinned,
        memo,
        now_boot_ns,
        policy,
    });
    outcome::build(outcome::Parts {
        key,
        descriptor,
        cycle_id,
        now_boot_ns,
        policy,
        assessed: &assessed,
        ranking: &ranking,
        consistency: &consistency,
        decision,
        notes,
        dropped,
    })
}

/// Keeps at most `limit` candidates: the pinned source and the current
/// selection first, then by profile rank, newest read and source id. One
/// candidate per source id (the newest reading wins).
fn limit_candidates<'a>(
    mut candidates: Vec<Candidate<'a>>,
    max_candidates: usize,
    pinned: Option<&SourceId>,
    current: Option<&SourceId>,
) -> (Vec<Candidate<'a>>, Vec<SourceId>) {
    candidates.sort_by(|left, right| {
        left.source_id()
            .cmp(right.source_id())
            .then_with(|| reading_order(right.observation).cmp(&reading_order(left.observation)))
    });
    candidates.dedup_by(|later, earlier| later.source_id() == earlier.source_id());
    candidates.sort_by(|left, right| {
        retention_priority(left, pinned, current).cmp(&retention_priority(right, pinned, current))
    });
    let limit = max_candidates.clamp(1, MAX_CANDIDATES_PER_SERIES);
    let dropped = if candidates.len() > limit {
        candidates
            .split_off(limit)
            .iter()
            .map(|candidate| candidate.source_id().clone())
            .collect()
    } else {
        Vec::new()
    };
    candidates.sort_by(|left, right| left.source_id().cmp(right.source_id()));
    (candidates, dropped)
}

fn reading_order(observation: &Observation) -> (u64, u64, u32) {
    (
        observation.read_finished_boot_ns,
        observation.cycle_id,
        observation.provenance.generation,
    )
}

fn retention_priority<'c>(
    candidate: &'c Candidate<'_>,
    pinned: Option<&SourceId>,
    current: Option<&SourceId>,
) -> (bool, bool, u16, Reverse<u64>, &'c SourceId) {
    let source = candidate.source_id();
    (
        pinned != Some(source),
        current != Some(source),
        candidate.profile_rank,
        Reverse(candidate.observation.read_finished_boot_ns),
        source,
    )
}

struct DecisionInput<'r, 'c, 'a> {
    assessed: &'r [Assessed<'c, 'a>],
    ranking: &'r Ranking,
    consistency: &'r Consistency,
    pinned: Option<&'r SourceId>,
    memo: Option<&'r SelectionMemo>,
    now_boot_ns: u64,
    policy: &'r PolicySnapshot,
}

/// Steps 7–8: conflict wins over everything (a pin never hides it), then a
/// pin, then hysteresis among healthy candidates.
fn decide(input: &DecisionInput<'_, '_, '_>) -> Decision {
    if !input.consistency.conflicting.is_empty() {
        return Decision::Conflict;
    }
    if let Some(pinned) = input.pinned {
        return decide_pinned(input, pinned);
    }
    let Some(&best) = input.ranking.representatives().first() else {
        return Decision::Unavailable { pinned: false };
    };
    let current_index = input
        .memo
        .and_then(|memo| memo.selected.as_ref())
        .and_then(|current| index_of(input.assessed, current));
    let current_healthy = current_index.is_some_and(|index| input.assessed[index].is_eligible());
    let verdict = hysteresis::decide(&hysteresis::Input {
        memo: input.memo,
        best: input.assessed[best].candidate.source_id(),
        current_healthy,
        now_boot_ns: input.now_boot_ns,
        required_streak: input.policy.switch_successes.max(1),
        min_hold_ns: dgxtop_core::clock::ms_to_ns(u64::from(input.policy.switch_min_hold_ms)),
    });
    let index = index_of(input.assessed, &verdict.selected).unwrap_or(best);
    let held_challenger = verdict
        .held_challenger
        .as_ref()
        .and_then(|challenger| index_of(input.assessed, challenger));
    Decision::Selected {
        index,
        degraded: is_degraded(input.assessed, index),
        pinned: false,
        held_challenger,
        memo: verdict.memo,
    }
}

fn decide_pinned(input: &DecisionInput<'_, '_, '_>, pinned: &SourceId) -> Decision {
    match index_of(input.assessed, pinned) {
        Some(index) if input.assessed[index].is_eligible() => {
            let since = input
                .memo
                .filter(|memo| memo.selected.as_ref() == Some(pinned))
                .map_or(input.now_boot_ns, |memo| memo.selected_since_boot_ns);
            Decision::Selected {
                index,
                degraded: false,
                pinned: true,
                held_challenger: None,
                memo: hysteresis::settled(pinned.clone(), since),
            }
        }
        _ => Decision::Unavailable { pinned: true },
    }
}

fn index_of(assessed: &[Assessed<'_, '_>], source: &SourceId) -> Option<usize> {
    assessed
        .iter()
        .position(|entry| entry.candidate.source_id() == source)
}

/// Degraded: the policy-preferred source (lowest profile rank, then source
/// id, among sources that could serve the key) failed, so a fallback serves.
fn is_degraded(assessed: &[Assessed<'_, '_>], selected: usize) -> bool {
    let preferred = assessed
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.rejection().is_none_or(is_failure))
        .min_by(|(_, left), (_, right)| {
            (left.candidate.profile_rank, left.candidate.source_id())
                .cmp(&(right.candidate.profile_rank, right.candidate.source_id()))
        })
        .map(|(index, _)| index);
    preferred
        .is_some_and(|preferred| preferred != selected && assessed[preferred].rejection().is_some())
}

/// Rejections that mean "this source could not deliver", as opposed to
/// "this source does not serve this key here" (scope, namespace, policy).
pub(crate) fn is_failure(reason: &RejectReason) -> bool {
    matches!(
        reason,
        RejectReason::ReadFailed { .. }
            | RejectReason::DriverFault
            | RejectReason::QualityNotEligible { .. }
            | RejectReason::Stale
            | RejectReason::InvalidValue
            | RejectReason::UnknownUnit
    )
}

#[cfg(test)]
mod tests;
