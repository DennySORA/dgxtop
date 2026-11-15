//! The headless frontend of `--snapshot-json` (D-140): it lets the readers
//! run until the live state holds what a panel would show a moment after
//! the monitor started, then takes the document of that moment from the
//! committed snapshot and the catalog.
//!
//! Three things have to have happened: a second reading of every counter (a
//! rate is the difference of two readings), a first reading of every read
//! group, and the start of every adapter host. The read groups keep
//! absolute slots, so a group read every five seconds may be first read
//! five seconds after its catalog; a snapshot therefore reads the slow
//! groups as often as the configuration allows ([`read_slow_groups_soon`])
//! and waits until the committed metrics have stopped growing for one such
//! period.

use std::sync::Arc;
use std::time::Duration;

use dgxtop_core::canonical::ImmutableSnapshot;
use dgxtop_core::clock::{TimeProvider, ms_to_ns};
use dgxtop_core::config::SamplingConfig;
use dgxtop_core::ports::{CatalogReader, SnapshotReader};
use dgxtop_core::quality::Namespace;
use dgxtop_core::snapshot_report::{ReportContext, SnapshotReport};
use dgxtop_core::view::HostStatus;
use dgxtop_manager::presentation::HostFacts;

/// Longest wait for the first commit: hosts start and discover within a
/// second or two, so past this nothing is being read.
const FIRST_COMMIT_BUDGET: Duration = Duration::from_secs(10);
/// Longest wait after the first commit for the readings to settle: a host
/// that keeps starting or metrics that keep appearing do not hold the
/// document back for ever.
const SETTLE_BUDGET: Duration = Duration::from_secs(10);
/// Sampling intervals read after the first commit: the second reading
/// gives every rate.
const RATE_INTERVALS: u64 = 2;
/// Time for a reading to be collected and committed.
const COMMIT_MARGIN: Duration = Duration::from_millis(300);
/// Cadence of the wait.
const POLL: Duration = Duration::from_millis(50);
/// The period a snapshot reads the slow groups at: the shortest one the
/// configuration allows.
const SLOW_PERIOD_MS: u32 = 1000;
const NANOS_PER_MILLI: i128 = 1_000_000;

/// Reads the capacity and slow-health groups every [`SLOW_PERIOD_MS`] in
/// this run, where the configuration reads them more rarely: their first
/// reading then comes within that period of their catalog. Returns the
/// longest period a read group is left with.
pub(super) fn read_slow_groups_soon(sampling: &mut SamplingConfig) -> u32 {
    sampling.capacity_interval_ms = sampling.capacity_interval_ms.min(SLOW_PERIOD_MS);
    sampling.slow_health_interval_ms = sampling.slow_health_interval_ms.min(SLOW_PERIOD_MS);
    sampling
        .capacity_interval_ms
        .max(sampling.slow_health_interval_ms)
}

/// Why no document was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WaitError {
    /// Nothing was committed within [`FIRST_COMMIT_BUDGET`].
    NoData,
    /// The run was asked to stop (a signal, or a fail-stop).
    Stopped,
}

impl WaitError {
    pub(super) const fn message(self) -> &'static str {
        match self {
            Self::NoData => "snapshot: no reading was committed within 10 s",
            Self::Stopped => "snapshot: stopped before the readings were complete",
        }
    }
}

/// What the wait reads and how it passes time.
pub(super) struct Waiting<'a> {
    pub(super) snapshots: &'a dyn SnapshotReader,
    pub(super) clock: &'a dyn TimeProvider,
    pub(super) interval_ms: u32,
    /// The longest period of a read group in this run.
    pub(super) slow_period_ms: u32,
    /// An adapter host is still starting.
    pub(super) starting: &'a dyn Fn() -> bool,
    /// The run was asked to stop.
    pub(super) stopped: &'a dyn Fn() -> bool,
    /// Lets `Duration` pass (a sleep; a test advances its clock).
    pub(super) pause: &'a mut dyn FnMut(Duration),
}

impl Waiting<'_> {
    /// Waits for the first commit, then until the readings are complete
    /// (module docs); the snapshot of that moment.
    pub(super) fn snapshot(self) -> Result<Arc<ImmutableSnapshot>, WaitError> {
        let Self {
            snapshots,
            clock,
            interval_ms,
            slow_period_ms,
            starting,
            stopped,
            pause,
        } = self;
        let give_up_ns = clock
            .now_boot_ns()
            .saturating_add(duration_ns(FIRST_COMMIT_BUDGET));
        while snapshots.latest().revision == 0 {
            if stopped() {
                return Err(WaitError::Stopped);
            }
            if clock.now_boot_ns() >= give_up_ns {
                return Err(WaitError::NoData);
            }
            pause(POLL);
        }
        let first_commit_ns = clock.now_boot_ns();
        let margin_ns = duration_ns(COMMIT_MARGIN);
        let rates_ns = first_commit_ns
            .saturating_add(ms_to_ns(u64::from(interval_ms)).saturating_mul(RATE_INTERVALS))
            .saturating_add(margin_ns);
        let quiet_ns = ms_to_ns(u64::from(slow_period_ms)).saturating_add(margin_ns);
        let settle_by_ns = first_commit_ns.saturating_add(duration_ns(SETTLE_BUDGET));
        let mut growth = Growth::default();
        loop {
            if stopped() {
                return Err(WaitError::Stopped);
            }
            let now_ns = clock.now_boot_ns();
            let snapshot = snapshots.latest();
            let quiet_since_ns = growth.observe(&snapshot, now_ns);
            let settled = !starting() && now_ns >= quiet_since_ns.saturating_add(quiet_ns);
            if (settled && now_ns >= rates_ns) || now_ns >= settle_by_ns {
                return Ok(snapshot);
            }
            pause(POLL);
        }
    }
}

/// Since when the committed metrics have not grown in number.
#[derive(Debug, Default)]
struct Growth {
    metrics: usize,
    since_ns: u64,
}

impl Growth {
    /// Notes `snapshot` seen at `now_ns`; the time of the last growth.
    fn observe(&mut self, snapshot: &ImmutableSnapshot, now_ns: u64) -> u64 {
        let metrics = snapshot
            .domains
            .values()
            .map(|domain| domain.samples.len())
            .sum();
        if metrics > self.metrics || self.since_ns == 0 {
            self.metrics = metrics;
            self.since_ns = now_ns;
        }
        self.since_ns
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// What the document says of this run, beside the snapshot's data.
pub(super) struct Reporting<'a> {
    pub(super) catalog: &'a dyn CatalogReader,
    pub(super) clock: &'a dyn TimeProvider,
    pub(super) facts: &'a HostFacts,
    pub(super) namespace: Namespace,
    pub(super) interval_ms: u32,
    pub(super) adapters: &'a [HostStatus],
}

impl Reporting<'_> {
    /// The document of `snapshot`, taken now.
    pub(super) fn report(&self, snapshot: &ImmutableSnapshot) -> SnapshotReport {
        let unix_ms = self.clock.now_utc_ns().max(0) / NANOS_PER_MILLI;
        SnapshotReport::of(
            snapshot,
            &self.catalog.latest(),
            &ReportContext {
                dgxtop_version: &self.facts.version,
                host_name: &self.facts.host_name,
                kernel: self.facts.kernel.as_deref(),
                namespace: self.namespace,
                interval_ms: self.interval_ms,
                now_boot_ns: self.clock.now_boot_ns(),
                now_unix_ms: u64::try_from(unix_ms).unwrap_or(u64::MAX),
                users: &self.facts.users,
                adapters: self.adapters,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use dgxtop_core::canonical::CatalogSnapshot;
    use dgxtop_core::config::Config;
    use dgxtop_core::metric::ids;
    use dgxtop_core::testing::{FakeClock, metric_key, snapshot_entry, snapshot_of};

    use super::*;

    const START_NS: u64 = 5_000_000_000;

    /// A live state whose committed metrics follow a script: `(after_ms,
    /// metrics)` steps on the time since the wait began; none before the
    /// first step.
    struct Scripted {
        clock: Arc<FakeClock>,
        steps: Vec<(u64, usize)>,
    }

    impl Scripted {
        fn elapsed_ms(&self) -> u64 {
            (self.clock.now_boot_ns() - START_NS) / 1_000_000
        }
    }

    impl SnapshotReader for Scripted {
        fn latest(&self) -> Arc<ImmutableSnapshot> {
            let elapsed = self.elapsed_ms();
            let metrics = self
                .steps
                .iter()
                .rev()
                .find(|(after, _)| elapsed >= *after)
                .map_or(0, |(_, metrics)| *metrics);
            if metrics == 0 {
                return Arc::new(ImmutableSnapshot::default());
            }
            let entries = (0..metrics)
                .map(|index| {
                    let key = metric_key(
                        &format!("cpu.core:{index}"),
                        ids::CPU_UTILIZATION_TOTAL,
                        "core",
                    );
                    ("cpu", snapshot_entry(&key, Some(1.0), START_NS))
                })
                .collect();
            Arc::new(snapshot_of(1 + elapsed / 100, entries))
        }
    }

    struct FixedCatalog(Arc<CatalogSnapshot>);

    impl CatalogReader for FixedCatalog {
        fn latest(&self) -> Arc<CatalogSnapshot> {
            Arc::clone(&self.0)
        }
    }

    /// One wait on a fake clock that every pause advances.
    struct Run {
        steps: Vec<(u64, usize)>,
        interval_ms: u32,
        /// Hosts are starting until this long after the wait began.
        hosts_up_after_ms: u64,
        stop_after_ms: Option<u64>,
    }

    impl Run {
        fn of(steps: &[(u64, usize)]) -> Self {
            Self {
                steps: steps.to_vec(),
                interval_ms: 1000,
                hosts_up_after_ms: 0,
                stop_after_ms: None,
            }
        }

        /// The metrics of the snapshot taken, or why none was; and the
        /// time the wait took.
        fn wait(self) -> (Result<usize, WaitError>, u64) {
            let clock = FakeClock::new(START_NS);
            let state = Scripted {
                clock: Arc::clone(&clock),
                steps: self.steps,
            };
            let elapsed_ms = || (clock.now_boot_ns() - START_NS) / 1_000_000;
            let starting = || elapsed_ms() < self.hosts_up_after_ms;
            let stopped = || {
                self.stop_after_ms
                    .is_some_and(|after| elapsed_ms() >= after)
            };
            let mut pause = |duration: Duration| clock.advance_ns(duration_ns(duration));
            let outcome = Waiting {
                snapshots: &state,
                clock: clock.as_ref(),
                interval_ms: self.interval_ms,
                slow_period_ms: SLOW_PERIOD_MS,
                starting: &starting,
                stopped: &stopped,
                pause: &mut pause,
            }
            .snapshot()
            .map(|snapshot| {
                snapshot
                    .domains
                    .values()
                    .map(|domain| domain.samples.len())
                    .sum()
            });
            (outcome, elapsed_ms())
        }
    }

    /// With every metric committed at once, the document is taken two
    /// intervals and a margin after the first commit: every rate has had
    /// its second reading.
    #[test]
    fn the_snapshot_waits_for_the_second_reading_of_every_rate() {
        let (outcome, elapsed_ms) = Run::of(&[(1200, 40)]).wait();
        assert_eq!(outcome, Ok(40));
        assert_eq!(elapsed_ms, 1200 + 2 * 1000 + 300);
        // At a faster interval the slow groups' period decides.
        let mut fast = Run::of(&[(400, 40)]);
        fast.interval_ms = 100;
        let (outcome, elapsed_ms) = fast.wait();
        assert_eq!(outcome, Ok(40));
        assert_eq!(elapsed_ms, 400 + 1000 + 300);
    }

    /// A group read for the first time later (a slow group at its slot, an
    /// adapter that discovers late) is waited for: the document is taken
    /// once the metrics have not grown for a slow period, and holds them.
    #[test]
    fn the_snapshot_waits_until_every_group_has_delivered() {
        let late_group = Run::of(&[(1000, 40), (3100, 71)]);
        let (outcome, elapsed_ms) = late_group.wait();
        assert_eq!(outcome, Ok(71), "the late group's metrics are in");
        assert_eq!(elapsed_ms, 3100 + 1000 + 300);
        // A host still starting holds the document back as well.
        let mut slow_host = Run::of(&[(1000, 40)]);
        slow_host.hosts_up_after_ms = 4000;
        let (outcome, elapsed_ms) = slow_host.wait();
        assert_eq!(outcome, Ok(40));
        assert_eq!(elapsed_ms, 4000);
    }

    /// The wait is bounded: readers that never commit end it at its
    /// budget with no document, metrics that keep appearing and a host
    /// that never starts end it ten seconds after the first commit with
    /// what there is, and a stop request ends it at once.
    #[test]
    fn the_wait_is_bounded_and_ends_on_a_stop() {
        let (outcome, elapsed_ms) = Run::of(&[]).wait();
        assert_eq!(outcome, Err(WaitError::NoData));
        assert_eq!(elapsed_ms, 10_000);
        assert!(WaitError::NoData.message().contains("10 s"));

        let growing: Vec<(u64, usize)> = (1..=40_u32)
            .map(|step| (u64::from(step) * 500, step as usize))
            .collect();
        let (outcome, elapsed_ms) = Run::of(&growing).wait();
        assert_eq!(outcome, Ok(21), "what was committed by then");
        assert_eq!(elapsed_ms, 500 + 10_000);
        let mut never_up = Run::of(&[(500, 40)]);
        never_up.hosts_up_after_ms = u64::MAX;
        let (outcome, elapsed_ms) = never_up.wait();
        assert_eq!(outcome, Ok(40));
        assert_eq!(elapsed_ms, 500 + 10_000);

        let mut before = Run::of(&[]);
        before.stop_after_ms = Some(700);
        assert_eq!(before.wait(), (Err(WaitError::Stopped), 700));
        let mut after = Run::of(&[(500, 40)]);
        after.stop_after_ms = Some(900);
        assert_eq!(after.wait(), (Err(WaitError::Stopped), 900));
    }

    /// A snapshot reads the slow groups every second where the
    /// configuration reads them more rarely, and leaves a faster setting
    /// and everything else as it is.
    #[test]
    fn a_snapshot_reads_the_slow_groups_within_a_second() {
        let mut sampling = Config::defaults().unwrap().sampling;
        let saved = sampling.clone();
        assert!(saved.capacity_interval_ms > SLOW_PERIOD_MS);
        assert_eq!(read_slow_groups_soon(&mut sampling), SLOW_PERIOD_MS);
        assert_eq!(sampling.capacity_interval_ms, SLOW_PERIOD_MS);
        assert_eq!(sampling.slow_health_interval_ms, SLOW_PERIOD_MS);
        assert_eq!(sampling.interval_ms, saved.interval_ms);
        assert_eq!(sampling.discovery_interval_ms, saved.discovery_interval_ms);
        assert_eq!(sampling.gpu_enabled, saved.gpu_enabled);
        assert_eq!(read_slow_groups_soon(&mut sampling), SLOW_PERIOD_MS);
    }

    /// The document carries the moment it was taken at, the run's facts and
    /// the catalog as it stands then.
    #[test]
    fn the_report_is_of_the_moment_it_is_taken_at() {
        let clock = FakeClock::new(START_NS);
        let catalog = FixedCatalog(Arc::new(CatalogSnapshot {
            revision: 9,
            ..CatalogSnapshot::default()
        }));
        let facts = HostFacts {
            host_name: "acme-node".to_owned(),
            kernel: Some("6.8.0-acme".to_owned()),
            version: "9.9.9".to_owned(),
            ..HostFacts::default()
        };
        let reporting = Reporting {
            catalog: &catalog,
            clock: clock.as_ref(),
            facts: &facts,
            namespace: Namespace::Live,
            interval_ms: 250,
            adapters: &[],
        };
        let snapshot = ImmutableSnapshot {
            revision: 4,
            ..ImmutableSnapshot::default()
        };
        let report = reporting.report(&snapshot);
        assert_eq!(report.snapshot_revision, 4);
        assert_eq!(report.uptime_ms, START_NS / 1_000_000);
        assert_eq!(report.taken_unix_ms, 1_790_000_000_000);
        assert_eq!(report.interval_ms, 250);
        assert_eq!(report.host.name, "acme-node");
        assert_eq!(report.host.kernel.as_deref(), Some("6.8.0-acme"));
        assert_eq!(report.dgxtop_version, "9.9.9");
        assert_eq!(report.namespace, Namespace::Live);
    }
}
