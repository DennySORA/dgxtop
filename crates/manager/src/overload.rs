//! Bounded overload control (RUNTIME.md §7).
//!
//! `normal → pressured → throttled → recovering → normal`:
//! * pressured: any controlled queue ≥ 75 % (or collection service time above
//!   the effective period) for `runtime.pressure_samples` consecutive samples
//!   and at least 3 s;
//! * throttled: managed memory ≥ 90 % or the ingress gate persistently full;
//!   while it persists the fast-group multiplier escalates ×2 → ×4;
//! * recovery: all queues < 50 %, memory < 75 % and service time normal for
//!   `runtime.recovery_ms`, stepping down at most one level per 30 s.
//!
//! Upward moves need sustained evidence and downward moves need a full quiet
//! window per level, so a noisy signal cannot flap the state every tick.

use dgxtop_core::clock::{NANOS_PER_SECOND, ms_to_ns};
use dgxtop_core::config::RuntimeConfig;
use dgxtop_core::view::OverloadState;

/// Sampling cadence of the controller.
pub(crate) const OVERLOAD_SAMPLE_MS: u64 = 1000;
const PRESSURE_RATIO: f64 = 0.75;
const MEMORY_CRITICAL_RATIO: f64 = 0.90;
/// The ingress gate counts as "full" from here (a maximum batch no longer fits).
const INGRESS_FULL_RATIO: f64 = 0.95;
const HEALTHY_QUEUE_RATIO: f64 = 0.50;
const HEALTHY_MEMORY_RATIO: f64 = 0.75;
const MIN_PRESSURE_NS: u64 = 3 * NANOS_PER_SECOND;
const MIN_LEVEL_STEP_NS: u64 = 30 * NANOS_PER_SECOND;
const PRESSURED_MAX_FPS: u32 = 2;
const FIRST_MULTIPLIER: u32 = 2;
const MAX_MULTIPLIER: u32 = 4;

/// One sample of the controlled signals (ratios in `[0, 1]`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct OverloadSignals {
    /// Highest utilization of the Manager's own queues (commands, ingestion
    /// inbox, collector backlogs).
    pub(crate) queue_utilization: f64,
    pub(crate) ingress_utilization: f64,
    pub(crate) memory_utilization: f64,
    /// Collection service time p95 above the effective fast period.
    pub(crate) service_overrun: bool,
}

/// A visible state or multiplier change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OverloadTransition {
    pub(crate) from: OverloadState,
    pub(crate) to: OverloadState,
    pub(crate) multiplier: u32,
}

#[derive(Debug, Default)]
struct Streak {
    count: u32,
    since_ns: Option<u64>,
}

impl Streak {
    fn observe(&mut self, active: bool, now_ns: u64) {
        if active {
            self.count = self.count.saturating_add(1);
            self.since_ns.get_or_insert(now_ns);
        } else {
            *self = Self::default();
        }
    }

    fn sustained(&self, samples: u32, min_ns: u64, now_ns: u64) -> bool {
        self.count >= samples
            && self
                .since_ns
                .is_some_and(|since| now_ns.saturating_sub(since) >= min_ns)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug)]
pub(crate) struct OverloadController {
    state: OverloadState,
    multiplier: u32,
    pressure_samples: u32,
    recovery_ns: u64,
    pressure: Streak,
    ingress_full: Streak,
    escalation: Streak,
    healthy_since_ns: Option<u64>,
    last_change_ns: u64,
}

impl OverloadController {
    pub(crate) fn new(runtime: &RuntimeConfig, now_ns: u64) -> Self {
        Self {
            state: OverloadState::Normal,
            multiplier: 1,
            pressure_samples: runtime.pressure_samples.max(1),
            recovery_ns: ms_to_ns(u64::from(runtime.recovery_ms)),
            pressure: Streak::default(),
            ingress_full: Streak::default(),
            escalation: Streak::default(),
            healthy_since_ns: None,
            last_change_ns: now_ns,
        }
    }

    pub(crate) fn state(&self) -> OverloadState {
        self.state
    }

    /// Multiplier applied to throttleable fast read groups.
    pub(crate) fn multiplier(&self) -> u32 {
        self.multiplier
    }

    /// Projection rate cap for the current state.
    pub(crate) fn max_fps(&self, configured: u32) -> u32 {
        match self.state {
            OverloadState::Normal => configured.max(1),
            _ => configured.clamp(1, PRESSURED_MAX_FPS),
        }
    }

    /// Metadata (discovery) is deferred outside the normal state.
    pub(crate) fn defer_metadata(&self) -> bool {
        self.state != OverloadState::Normal
    }

    /// History queries are rate limited while throttled.
    pub(crate) fn limit_history(&self) -> bool {
        self.state == OverloadState::Throttled
    }

    pub(crate) fn sample(
        &mut self,
        now_ns: u64,
        signals: OverloadSignals,
    ) -> Option<OverloadTransition> {
        let previous = (self.state, self.multiplier);
        // Any controlled queue, the ingress byte gate included (RUNTIME.md §7).
        let busiest_queue = signals.queue_utilization.max(signals.ingress_utilization);
        let pressure_now = busiest_queue >= PRESSURE_RATIO || signals.service_overrun;
        let memory_critical = signals.memory_utilization >= MEMORY_CRITICAL_RATIO;
        let healthy = signals.queue_utilization < HEALTHY_QUEUE_RATIO
            && signals.ingress_utilization < HEALTHY_QUEUE_RATIO
            && signals.memory_utilization < HEALTHY_MEMORY_RATIO
            && !signals.service_overrun;

        self.pressure.observe(pressure_now, now_ns);
        self.ingress_full
            .observe(signals.ingress_utilization >= INGRESS_FULL_RATIO, now_ns);
        let throttle_now = memory_critical || self.ingress_full.count >= self.pressure_samples;
        if healthy {
            self.healthy_since_ns.get_or_insert(now_ns);
        } else {
            self.healthy_since_ns = None;
        }

        if throttle_now {
            self.escalate(now_ns);
        } else {
            self.escalation.reset();
            if self
                .pressure
                .sustained(self.pressure_samples, MIN_PRESSURE_NS, now_ns)
            {
                if matches!(
                    self.state,
                    OverloadState::Normal | OverloadState::Recovering
                ) {
                    self.enter(OverloadState::Pressured, 1, now_ns);
                }
            } else if self.can_step_down(now_ns) {
                self.step_down(now_ns);
            }
        }

        ((self.state, self.multiplier) != previous).then_some(OverloadTransition {
            from: previous.0,
            to: self.state,
            multiplier: self.multiplier,
        })
    }

    fn escalate(&mut self, now_ns: u64) {
        if self.state != OverloadState::Throttled {
            self.enter(OverloadState::Throttled, FIRST_MULTIPLIER, now_ns);
            return;
        }
        self.escalation.observe(true, now_ns);
        if self.multiplier < MAX_MULTIPLIER
            && self
                .escalation
                .sustained(self.pressure_samples, MIN_PRESSURE_NS, now_ns)
        {
            self.enter(OverloadState::Throttled, MAX_MULTIPLIER, now_ns);
        }
    }

    fn can_step_down(&self, now_ns: u64) -> bool {
        if self.state == OverloadState::Normal {
            return false;
        }
        let window = self.recovery_ns.max(MIN_LEVEL_STEP_NS);
        self.healthy_since_ns
            .is_some_and(|since| now_ns.saturating_sub(since.max(self.last_change_ns)) >= window)
    }

    fn step_down(&mut self, now_ns: u64) {
        match (self.state, self.multiplier) {
            (OverloadState::Throttled, multiplier) if multiplier > FIRST_MULTIPLIER => {
                self.enter(OverloadState::Throttled, FIRST_MULTIPLIER, now_ns);
            }
            (OverloadState::Throttled | OverloadState::Pressured, _) => {
                self.enter(OverloadState::Recovering, 1, now_ns);
            }
            (OverloadState::Recovering, _) => self.enter(OverloadState::Normal, 1, now_ns),
            (OverloadState::Normal, _) => {}
        }
    }

    fn enter(&mut self, state: OverloadState, multiplier: u32, now_ns: u64) {
        self.state = state;
        self.multiplier = multiplier;
        self.last_change_ns = now_ns;
        self.escalation.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::config::Config;

    const SECOND: u64 = NANOS_PER_SECOND;

    fn controller() -> OverloadController {
        OverloadController::new(&Config::defaults().unwrap().runtime, 0)
    }

    fn queue(ratio: f64) -> OverloadSignals {
        OverloadSignals {
            queue_utilization: ratio,
            ..OverloadSignals::default()
        }
    }

    fn memory(ratio: f64) -> OverloadSignals {
        OverloadSignals {
            memory_utilization: ratio,
            ..OverloadSignals::default()
        }
    }

    fn run(
        controller: &mut OverloadController,
        from_s: u64,
        to_s: u64,
        signals: OverloadSignals,
    ) -> Vec<OverloadTransition> {
        (from_s..to_s)
            .filter_map(|second| controller.sample(second * SECOND, signals))
            .collect()
    }

    #[test]
    fn pressure_needs_consecutive_samples_and_three_seconds() {
        let mut overload = controller();
        // Three samples span only 2 s: not yet.
        assert!(run(&mut overload, 0, 3, queue(0.8)).is_empty());
        assert_eq!(overload.state(), OverloadState::Normal);
        let transitions = run(&mut overload, 3, 4, queue(0.8));
        assert_eq!(transitions.len(), 1);
        assert_eq!(
            (transitions[0].from, transitions[0].to),
            (OverloadState::Normal, OverloadState::Pressured)
        );
        assert_eq!(overload.state(), OverloadState::Pressured);
        assert_eq!(overload.max_fps(4), 2);
        assert!(overload.defer_metadata());
        assert_eq!(overload.multiplier(), 1);
    }

    #[test]
    fn ingress_bytes_at_three_quarters_are_pressure_like_any_queue() {
        let mut overload = controller();
        let held = OverloadSignals {
            ingress_utilization: 0.75,
            ..OverloadSignals::default()
        };
        // RUNTIME.md §7: any queue's bytes >= 75 % for 3 samples and 3 s.
        assert!(run(&mut overload, 0, 3, held).is_empty());
        let transitions = run(&mut overload, 3, 4, held);
        assert_eq!(transitions.len(), 1, "{transitions:?}");
        assert_eq!(
            (transitions[0].from, transitions[0].to),
            (OverloadState::Normal, OverloadState::Pressured)
        );
        // Just below the boundary is not pressure.
        let mut calm = controller();
        let below = OverloadSignals {
            ingress_utilization: 0.749,
            ..OverloadSignals::default()
        };
        assert!(run(&mut calm, 0, 10, below).is_empty());
        assert_eq!(calm.state(), OverloadState::Normal);
    }

    #[test]
    fn alternating_spikes_never_flap() {
        let mut overload = controller();
        for second in 0..120u64 {
            let ratio = if second % 2 == 0 { 0.9 } else { 0.1 };
            assert!(overload.sample(second * SECOND, queue(ratio)).is_none());
        }
        assert_eq!(overload.state(), OverloadState::Normal);
    }

    #[test]
    fn memory_critical_throttles_then_escalates_and_recovers_one_level_per_30s() {
        let mut overload = controller();
        let first = overload.sample(0, memory(0.95)).unwrap();
        assert_eq!(first.to, OverloadState::Throttled);
        assert_eq!(overload.multiplier(), 2);
        assert!(overload.limit_history());
        // Persisting for >= 3 samples and >= 3 s escalates to x4.
        let escalations = run(&mut overload, 1, 6, memory(0.95));
        assert_eq!(escalations.len(), 1);
        assert_eq!(
            (escalations[0].to, escalations[0].multiplier),
            (OverloadState::Throttled, 4)
        );
        assert_eq!(overload.multiplier(), 4);
        // Healthy from t=10 s: x4 -> x2 at 40 s, -> recovering at 70 s,
        // -> normal at 100 s; never more than one level per 30 s.
        let mut steps = Vec::new();
        for second in 10..=110u64 {
            if let Some(step) = overload.sample(second * SECOND, OverloadSignals::default()) {
                steps.push((second, step));
            }
        }
        let summary: Vec<_> = steps
            .iter()
            .map(|(second, step)| (*second, step.to, step.multiplier))
            .collect();
        assert_eq!(
            summary,
            vec![
                (40, OverloadState::Throttled, 2),
                (70, OverloadState::Recovering, 1),
                (100, OverloadState::Normal, 1),
            ]
        );
    }

    #[test]
    fn unhealthy_sample_restarts_the_recovery_window() {
        let mut overload = controller();
        run(&mut overload, 0, 4, queue(0.8));
        assert_eq!(overload.state(), OverloadState::Pressured);
        // 29 healthy seconds, then a 60 % sample (not pressure, not healthy).
        assert!(run(&mut overload, 4, 33, OverloadSignals::default()).is_empty());
        assert!(overload.sample(33 * SECOND, queue(0.6)).is_none());
        assert!(run(&mut overload, 34, 63, OverloadSignals::default()).is_empty());
        let recovered = overload
            .sample(64 * SECOND, OverloadSignals::default())
            .unwrap();
        assert_eq!(recovered.to, OverloadState::Recovering);
        // Pressure while recovering goes back up without waiting.
        let back = run(&mut overload, 65, 70, queue(0.9));
        assert_eq!(back.last().unwrap().to, OverloadState::Pressured);
    }

    #[test]
    fn service_overrun_counts_as_pressure_and_blocks_recovery() {
        let mut overload = controller();
        let overrun = OverloadSignals {
            service_overrun: true,
            ..OverloadSignals::default()
        };
        run(&mut overload, 0, 4, overrun);
        assert_eq!(overload.state(), OverloadState::Pressured);
        assert_eq!(overload.max_fps(1), 1);
        assert!(run(&mut overload, 4, 120, overrun).is_empty());
        assert_eq!(overload.state(), OverloadState::Pressured);
    }
}
