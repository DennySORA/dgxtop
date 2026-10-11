//! What the state actor does with one [`StateJob`]: the composition root's
//! only translation from the Manager's state jobs to the live state's
//! writer, shared by the monitor and the integration rigs.

use dgxtop_core::canonical::StoreAck;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::ports::{StateJob, StoreWriter};
use dgxtop_state::StateWriter;

/// Applies `job` to the live state; a commit answers with its ack.
pub fn run_state_job(
    writer: &mut StateWriter,
    job: StateJob,
    clock: &dyn TimeProvider,
) -> Option<StoreAck> {
    match job {
        StateJob::Commit { batch, guard } => {
            Some(writer.commit_collection(*batch, &guard, clock.now_boot_ns()))
        }
        StateJob::Gap(gap) => {
            writer.record_gap(gap);
            None
        }
        StateJob::Maintain => {
            // The archive needs the boot/UTC pairing to stamp finished rows.
            writer.observe_wall_clock(clock.now_boot_ns(), clock.now_utc_ns());
            writer.maintain(clock.now_boot_ns());
            None
        }
        StateJob::ApplySampling(sampling) => {
            writer.set_sampling(&sampling);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use dgxtop_core::budget::ByteLedger;
    use dgxtop_core::canonical::{
        CanonicalValue, CollectionBatch, CollectorState, CommitGuard, GapRecord,
    };
    use dgxtop_core::config::Config;
    use dgxtop_core::identity::CollectorId;
    use dgxtop_core::metric::{CanonicalUnit, Scalar, ids};
    use dgxtop_core::ports::{ArchiveJournal, HistoryAppends, HistoryIngest, HistoryOutcome};
    use dgxtop_core::quality::{Freshness, Namespace, Quality, SampleKind, SelectionState};
    use dgxtop_core::sampling::SamplingTiming;
    use dgxtop_core::testing::{FakeClock, metric_key, source_id};
    use dgxtop_state::LiveState;
    use uuid::Uuid;

    use super::*;

    const SECOND: u64 = 1_000_000_000;

    /// What the state handed to the history that depends on its timing.
    #[derive(Default)]
    struct Timing {
        finalize_grace_ns: Vec<u64>,
        /// `(sample time, stale deadline)` of every gauge step.
        steps: Vec<(u64, u64)>,
    }

    #[derive(Clone, Default)]
    struct TimingHistory(Arc<Mutex<Timing>>);

    impl TimingHistory {
        fn lock(&self) -> std::sync::MutexGuard<'_, Timing> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl HistoryIngest for TimingHistory {
        fn ingest(&mut self, appends: HistoryAppends, _now: u64, _revision: u64) -> HistoryOutcome {
            self.lock().steps.extend(
                appends
                    .gauges
                    .iter()
                    .map(|gauge| (gauge.time_ns, gauge.stale_deadline_ns)),
            );
            HistoryOutcome::default()
        }

        fn record_gap(&mut self, _gap: GapRecord) -> HistoryOutcome {
            HistoryOutcome::default()
        }

        fn maintain(&mut self, _now: u64) -> HistoryOutcome {
            HistoryOutcome::default()
        }

        fn set_revision(&mut self, _revision: u64) {}

        fn set_finalize_grace(&mut self, grace_ns: u64) {
            self.lock().finalize_grace_ns.push(grace_ns);
        }

        fn observe_wall_clock(&mut self, _boot_ns: u64, _utc_ns: i128) {}

        fn take_archive_journal(&mut self, _max_bytes: u64) -> Option<ArchiveJournal> {
            None
        }
    }

    fn temperature_commit(at: u64) -> StateJob {
        let collector = CollectorId::new("gpu").unwrap();
        let value = CanonicalValue {
            key: metric_key("gpu:0", ids::GPU_TEMPERATURE, "gpu"),
            cycle_id: 1,
            source_id: Some(source_id("nvml:0:temp")),
            source_generation: Some(1),
            unit: CanonicalUnit::DegC,
            value: Some(Scalar::from_f64(50.0).unwrap()),
            sample_kind: SampleKind::Measured,
            selection_state: SelectionState::Selected,
            quality: Quality::Verified,
            freshness: Freshness::Fresh,
            measurement_freshness: Freshness::Unknown,
            observed_boot_ns: None,
            read_finished_boot_ns: at,
            policy_revision: 1,
            candidate_source_ids: Vec::new(),
            reason_codes: Vec::new(),
            eligible_for_history: true,
        };
        let batch = CollectionBatch {
            batch_id: Uuid::new_v4(),
            collector_id: collector.clone(),
            cycle_id: 1,
            epoch: 1,
            config_revision: 1,
            catalog_revision: 1,
            policy_revision: 1,
            state_base_revision: 0,
            canonical: vec![value],
            explanations: Vec::new(),
            counter_intervals: Vec::new(),
            inputs: Vec::new(),
            process_table: None,
            next_state: CollectorState::default(),
            failures: Vec::new(),
        };
        StateJob::Commit {
            batch: Box::new(batch),
            guard: CommitGuard {
                expected_epoch: 1,
                collector_id: collector,
                state_base_revision: 0,
            },
        }
    }

    /// A durable interval change reaches the state's stale windows and the
    /// history's finalize grace through the store job the Manager sends.
    #[test]
    fn an_applied_sampling_change_retimes_the_state_and_the_history() {
        let config = Config::defaults().unwrap();
        let history = TimingHistory::default();
        let (mut writer, _snapshots) = LiveState::new(
            &config.store,
            &config.sampling,
            ByteLedger::for_memory_limit_mib(64),
            Namespace::Live,
            false,
            Box::new(history.clone()),
        )
        .into_parts();
        let clock = FakeClock::new(10 * SECOND);
        let mut slower = config.sampling.clone();
        slower.interval_ms = 5_000;
        let expected = SamplingTiming::from_sampling(&slower);

        assert!(
            run_state_job(&mut writer, StateJob::ApplySampling(slower), clock.as_ref()).is_none()
        );
        let commit = temperature_commit(9 * SECOND);
        let ack = run_state_job(&mut writer, commit, clock.as_ref()).unwrap();
        assert!(ack.status.is_committed());

        let timing = history.lock();
        assert_eq!(timing.finalize_grace_ns, vec![expected.finalize_grace_ns]);
        let stale_after = u64::from(expected.stale_factor) * expected.fast_period_ns;
        assert_eq!(timing.steps, vec![(9 * SECOND, 9 * SECOND + stale_after)]);
    }
}
