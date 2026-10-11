//! Step 7: inconsistency check.
//!
//! Only comparable (non-counter), time-aligned candidates are compared: device
//! time when both report it, else read time (weaker evidence, flagged).
//! Different confirmed origins beyond tolerance are a conflict; aliases and
//! unknown origins only add diagnostic notes. Values are never averaged and
//! the lower one is never preferred. With at most four candidates there are
//! at most six pairs.

use std::collections::BTreeSet;

use dgxtop_core::canonical::PolicySnapshot;
use dgxtop_core::clock::ms_to_ns;
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::observation::Observation;
use dgxtop_core::quality::RawKind;

use super::Assessed;
use super::assess::confirmed_origin;
use super::ranking::Ranking;
use crate::shared::reasons;

/// Findings of the pairwise comparison.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Consistency {
    /// Candidate indices of independent origins that disagree.
    pub conflicting: BTreeSet<usize>,
    /// Largest spread among compared pairs (canonical unit).
    pub spread: Option<f64>,
    /// Tolerance applied to the pair with the largest spread.
    pub tolerance: Option<f64>,
    pub notes: Vec<&'static str>,
    pub comparisons: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    /// Same confirmed origin: diagnostic comparison only.
    Alias,
    /// Different confirmed origins: a disagreement is a conflict.
    Independent,
    /// At least one origin is unknown: uncertainty, never a vote.
    Unconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alignment {
    DeviceTime,
    ReadTime,
}

pub(crate) fn check(
    assessed: &[Assessed<'_, '_>],
    ranking: &Ranking,
    unit: CanonicalUnit,
    policy: &PolicySnapshot,
) -> Consistency {
    let comparer = Comparer {
        assessed,
        unit,
        policy,
        tolerance_ns: ms_to_ns(u64::from(policy.alignment_tolerance_ms)),
    };
    let mut report = Consistency::default();
    for (position, &left) in ranking.order.iter().enumerate() {
        for &right in &ranking.order[position + 1..] {
            if let Some(relation) = relation(ranking, assessed, left, right) {
                comparer.compare(&mut report, left, right, relation);
            }
        }
    }
    report.notes.sort_unstable();
    report.notes.dedup();
    report
}

struct Comparer<'r, 'c, 'a> {
    assessed: &'r [Assessed<'c, 'a>],
    unit: CanonicalUnit,
    policy: &'r PolicySnapshot,
    tolerance_ns: u64,
}

impl Comparer<'_, '_, '_> {
    fn compare(&self, report: &mut Consistency, left: usize, right: usize, relation: Relation) {
        let (first, second) = (&self.assessed[left], &self.assessed[right]);
        if !comparable(first) || !comparable(second) {
            return;
        }
        let Some(alignment) = alignment(
            first.candidate.observation,
            second.candidate.observation,
            self.tolerance_ns,
        ) else {
            return;
        };
        let (Some(a), Some(b)) = (first.value(), second.value()) else {
            return;
        };
        let (a, b) = (a.to_f64_lossy(), b.to_f64_lossy());
        let spread = (a - b).abs();
        let tolerance = tolerance_for(self.unit, a, b, self.policy);
        report.comparisons += 1;
        if report.spread.is_none_or(|largest| spread > largest) {
            report.spread = Some(spread);
            report.tolerance = Some(tolerance);
        }
        if spread <= tolerance {
            return;
        }
        match relation {
            Relation::Independent => {
                report.conflicting.insert(left);
                report.conflicting.insert(right);
                if alignment == Alignment::ReadTime {
                    report.notes.push(reasons::ALIGNED_BY_READ_TIME);
                }
            }
            Relation::Alias => report.notes.push(reasons::ALIAS_DISAGREEMENT),
            Relation::Unconfirmed => report.notes.push(reasons::UNCONFIRMED_ORIGIN_DISAGREEMENT),
        }
    }
}

/// Pairs worth comparing. An alias is compared only with the entry of its
/// own origin: comparing it with another origin would count that origin twice.
fn relation(
    ranking: &Ranking,
    assessed: &[Assessed<'_, '_>],
    left: usize,
    right: usize,
) -> Option<Relation> {
    let (left_alias, right_alias) = (ranking.alias_of[left], ranking.alias_of[right]);
    if left_alias.is_some() || right_alias.is_some() {
        let same_origin = left_alias == Some(right)
            || right_alias == Some(left)
            || (left_alias.is_some() && left_alias == right_alias);
        return same_origin.then_some(Relation::Alias);
    }
    let confirmed = confirmed_origin(assessed[left].candidate.observation).is_some()
        && confirmed_origin(assessed[right].candidate.observation).is_some();
    Some(if confirmed {
        Relation::Independent
    } else {
        Relation::Unconfirmed
    })
}

/// Counters are not compared: cumulative totals from different sources start
/// at different points and say nothing about agreement.
fn comparable(entry: &Assessed<'_, '_>) -> bool {
    entry.candidate.raw_kind != RawKind::Counter
}

fn alignment(first: &Observation, second: &Observation, tolerance_ns: u64) -> Option<Alignment> {
    match (first.device_sample_boot_ns, second.device_sample_boot_ns) {
        (Some(a), Some(b)) => (a.abs_diff(b) <= tolerance_ns).then_some(Alignment::DeviceTime),
        _ => (first
            .read_finished_boot_ns
            .abs_diff(second.read_finished_boot_ns)
            <= tolerance_ns)
            .then_some(Alignment::ReadTime),
    }
}

/// Temperatures use the absolute diagnostic tolerance; other gauges a
/// percentage of the larger magnitude.
fn tolerance_for(unit: CanonicalUnit, a: f64, b: f64, policy: &PolicySnapshot) -> f64 {
    if unit == CanonicalUnit::DegC {
        policy.temperature_disagreement_deg_c.max(0.0)
    } else {
        policy.relative_tolerance_percent.max(0.0) / 100.0 * a.abs().max(b.abs())
    }
}
