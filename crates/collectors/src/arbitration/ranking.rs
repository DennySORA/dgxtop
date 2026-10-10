//! Steps 5–6: origin de-duplication and candidate ordering.
//!
//! Only confirmed identical `origin_id`s are aliases; equal values never
//! prove a shared origin and unknown origins never merge. Ordering is quality
//! grade, then profile rank, then newest read, then source id. There is no
//! global adapter ranking.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use dgxtop_core::identity::{OriginId, SourceId};

use super::Assessed;
use super::assess::confirmed_origin;

/// Ordered eligible candidates and their alias relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ranking {
    /// Eligible candidate indices, best first.
    pub order: Vec<usize>,
    /// `alias_of[i] == Some(r)`: candidate `i` shares the confirmed origin of
    /// representative `r` and adds no independent vote.
    pub alias_of: Vec<Option<usize>>,
}

impl Ranking {
    /// Eligible candidates that represent their origin, best first.
    pub fn representatives(&self) -> Vec<usize> {
        self.order
            .iter()
            .copied()
            .filter(|index| self.alias_of[*index].is_none())
            .collect()
    }
}

pub(crate) fn rank(assessed: &[Assessed<'_, '_>]) -> Ranking {
    let mut order: Vec<usize> = (0..assessed.len())
        .filter(|index| assessed[*index].is_eligible())
        .collect();
    order.sort_by(|left, right| order_key(&assessed[*left]).cmp(&order_key(&assessed[*right])));
    let mut alias_of = vec![None; assessed.len()];
    let mut representative_of_origin: BTreeMap<&OriginId, usize> = BTreeMap::new();
    for &index in &order {
        let Some(origin) = confirmed_origin(assessed[index].candidate.observation) else {
            continue;
        };
        match representative_of_origin.get(origin) {
            Some(&representative) => alias_of[index] = Some(representative),
            None => {
                representative_of_origin.insert(origin, index);
            }
        }
    }
    Ranking { order, alias_of }
}

fn order_key<'e>(entry: &'e Assessed<'_, '_>) -> (u8, u16, Reverse<u64>, &'e SourceId) {
    let observation = entry.candidate.observation;
    (
        observation.quality.rank(),
        entry.candidate.profile_rank,
        Reverse(observation.read_finished_boot_ns),
        entry.candidate.source_id(),
    )
}
