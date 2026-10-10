//! QualityPolicyService: the versioned arbitration policy handed to
//! collectors (`PolicySnapshot`).
//!
//! The snapshot combines `config.quality`, the sampling stale factor, the
//! effective period of every scheduled read group (`naming::period_key`) plus
//! the discovery period of every adapter (`naming::static_period_key`, used for
//! static metadata sources) and the namespace accepted into canonical state.
//! The revision only moves when the content changes, so collectors and the
//! store see a new `policy_revision` exactly when arbitration inputs differ.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use dgxtop_core::canonical::PolicySnapshot;
use dgxtop_core::config::Config;
use dgxtop_core::quality::Namespace;

/// Engineering default for the disagreement tolerance of non-temperature
/// gauges, in percent of magnitude. Schema v2 has no field for it, so it is a
/// documented constant rather than a hidden heuristic.
pub(crate) const RELATIVE_TOLERANCE_PERCENT: f64 = 10.0;

#[derive(Debug)]
pub(crate) struct QualityPolicyService {
    current: Arc<PolicySnapshot>,
}

impl QualityPolicyService {
    pub(crate) fn new(
        config: &Config,
        periods: BTreeMap<String, u32>,
        namespace: Namespace,
    ) -> Self {
        Self {
            current: Arc::new(build(config, periods, namespace, 1)),
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<PolicySnapshot> {
        Arc::clone(&self.current)
    }

    #[cfg(test)]
    pub(crate) fn revision(&self) -> u32 {
        self.current.revision
    }

    /// Rebuilds the policy; returns whether a new revision was published.
    pub(crate) fn update(&mut self, config: &Config, periods: BTreeMap<String, u32>) -> bool {
        let candidate = build(
            config,
            periods,
            self.current.namespace,
            self.current.revision,
        );
        if candidate == *self.current {
            return false;
        }
        let revision = self.current.revision.saturating_add(1);
        self.current = Arc::new(PolicySnapshot {
            revision,
            ..candidate
        });
        true
    }
}

fn build(
    config: &Config,
    periods: BTreeMap<String, u32>,
    namespace: Namespace,
    revision: u32,
) -> PolicySnapshot {
    let quality = &config.quality;
    PolicySnapshot {
        revision,
        switch_successes: quality.switch_successes,
        switch_min_hold_ms: quality.switch_min_hold_ms,
        temperature_disagreement_deg_c: quality.temperature_disagreement_deg_c,
        relative_tolerance_percent: RELATIVE_TOLERANCE_PERCENT,
        alignment_tolerance_ms: quality.alignment_tolerance_ms,
        excluded_sources: quality
            .excluded_sources
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        pinned_sources: quality
            .pinned_sources
            .iter()
            .map(|pin| (pin.key.clone(), pin.source_id.clone()))
            .collect(),
        stale_factor: config.sampling.stale_factor,
        effective_periods_ms: periods,
        namespace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn periods(fast: u32) -> BTreeMap<String, u32> {
        BTreeMap::from([
            ("procfs/cpu".to_owned(), fast),
            ("procfs/discovery".to_owned(), 10_000),
        ])
    }

    #[test]
    fn revision_moves_only_on_content_change() {
        let config = Config::defaults().unwrap();
        let mut policy = QualityPolicyService::new(&config, periods(1000), Namespace::Live);
        assert!(!policy.update(&config, periods(1000)));
        assert_eq!(policy.revision(), 1);
        let mut stricter = config.clone();
        stricter.quality.temperature_disagreement_deg_c = 4.0;
        assert!(policy.update(&stricter, periods(1000)));
        assert_eq!(policy.revision(), 2);
        // Overload multiplier changes the effective periods: new revision.
        assert!(policy.update(&stricter, periods(2000)));
        assert_eq!(policy.revision(), 3);
        assert_eq!(policy.snapshot().namespace, Namespace::Live);
        assert_eq!(
            policy.snapshot().effective_periods_ms.get("procfs/cpu"),
            Some(&2000)
        );
    }
}
