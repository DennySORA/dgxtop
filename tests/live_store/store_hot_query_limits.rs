//! AT38: bounded queries (points, bytes, scan budget, deadline, cancel) and
//! concurrent queries while the HotStore keeps committing.

#[path = "store_hot_support.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dgxtop_core::budget::{ByteLedger, Partition, PartitionLimits};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::history::{HistoryMethod, HistoryPoint, HistoryStat};
use dgxtop_core::identity::BootId;
use dgxtop_core::ports::{CancelFlag, HistoryReader};
use dgxtop_core::quality::Namespace;
use dgxtop_core::testing::{boot_id, source_id};
use support::{Harness, SECOND, assert_well_formed, config, gauge, query, temperature_key};

/// Clock that moves forward on every reading, to pass deadlines mid-scan.
struct TickingClock {
    now: AtomicU64,
    step: u64,
}

impl TimeProvider for TickingClock {
    fn boot_id(&self) -> BootId {
        boot_id("fake-boot")
    }

    fn now_boot_ns(&self) -> u64 {
        self.now.fetch_add(self.step, Ordering::SeqCst)
    }

    fn now_utc_ns(&self) -> i128 {
        0
    }
}

fn filled(
    seconds: u64,
    settings: dgxtop_core::config::Config,
) -> (Harness, dgxtop_core::identity::MetricKey) {
    let mut harness = Harness::with_config(settings);
    let key = temperature_key("gpu:0");
    for second in 0..seconds {
        harness.commit(
            vec![gauge(&key, (second % 50) as f64, second * SECOND)],
            vec![],
            second * SECOND,
        );
    }
    harness.maintain(seconds * SECOND);
    (harness, key)
}

#[test]
fn at38_raw_results_are_truncated_and_paged_never_sampled() {
    let (harness, key) = filled(50, config());
    let full = harness
        .query(&key, (0, 50 * SECOND), 0, HistoryStat::Raw, 2000)
        .unwrap();
    assert!(!full.truncated);
    assert_eq!(full.next_start_boot_ns, None);
    assert_eq!(full.points.len(), 50);

    let mut pages: Vec<HistoryPoint> = Vec::new();
    let mut start = 0;
    let mut page_count = 0;
    loop {
        let page = harness
            .query(&key, (start, 50 * SECOND), 0, HistoryStat::Raw, 10)
            .unwrap();
        assert_well_formed(&page);
        assert!(page.points.len() <= 10);
        page_count += 1;
        pages.extend(page.points.iter().cloned());
        match page.next_start_boot_ns {
            Some(next) => {
                assert!(page.truncated);
                assert_eq!(next, page.points.last().unwrap().end_boot_ns);
                start = next;
            }
            None => break,
        }
    }
    assert_eq!(page_count, 5);
    assert_eq!(
        pages, full.points,
        "pages concatenate to the full raw result"
    );
}

#[test]
fn at38_aggregates_beyond_max_points_use_coarser_buckets() {
    let (harness, key) = filled(600, config());
    let fine = harness
        .query(&key, (0, 600 * SECOND), 1000, HistoryStat::Mean, 2000)
        .unwrap();
    let coarse = harness
        .query(&key, (0, 600 * SECOND), 1000, HistoryStat::Mean, 50)
        .unwrap();
    assert_well_formed(&coarse);
    assert!(!coarse.truncated);
    assert!(coarse.points.len() <= 50);
    assert_eq!(coarse.covered_ns, fine.covered_ns);
    let width = coarse.points[0].end_boot_ns - coarse.points[0].start_boot_ns;
    assert!(
        width > SECOND && width % (60 * SECOND) == 0,
        "coarser minute-aligned cells"
    );
    assert!(
        coarse
            .points
            .iter()
            .all(|point| point.method == HistoryMethod::TimeWeightedStep)
    );
}

#[test]
fn at38_scan_budget_returns_query_too_large_with_a_coarser_suggestion() {
    let mut settings = config();
    settings.store.max_query_scan_records = 1000;
    settings.store.raw_history_seconds = 3600;
    let (harness, key) = filled(3000, settings);
    let error = harness
        .query(&key, (0, 3000 * SECOND), 1000, HistoryStat::Mean, 2000)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::QueryTooLarge);
    assert!(
        error.message.contains("resolution_ms >= 60000"),
        "{}",
        error.message
    );
    assert!(!error.retryable);
    let raw = harness
        .query(&key, (0, 3000 * SECOND), 0, HistoryStat::Raw, 2000)
        .unwrap_err();
    assert_eq!(raw.code, ErrorCode::QueryTooLarge);
    let coarse = harness
        .query(&key, (0, 3000 * SECOND), 60_000, HistoryStat::Mean, 2000)
        .unwrap();
    assert_well_formed(&coarse);
    assert!(coarse.coverage_ratio > 0.99);
}

#[test]
fn at38_deadline_returns_timed_out() {
    let (harness, key) = filled(3000, {
        let mut settings = config();
        settings.store.raw_history_seconds = 3600;
        settings
    });
    let request = query(&key, (0, 3000 * SECOND), 0, HistoryStat::Raw, 2000);
    let passed = harness
        .query_with(
            &request,
            harness.clock.now_boot_ns(),
            &CancelFlag::new(),
            harness.clock.as_ref(),
        )
        .unwrap_err();
    assert_eq!(passed.code, ErrorCode::TimedOut);
    assert!(passed.retryable);
    assert_eq!(passed.request_id, Some(request.request_id));
    let ticking = TickingClock {
        now: AtomicU64::new(3000 * SECOND),
        step: 1_000_000,
    };
    let mean = query(&key, (0, 3000 * SECOND), 1000, HistoryStat::Mean, 2000);
    let midway = harness
        .query_with(
            &mean,
            3000 * SECOND + 3_000_000,
            &CancelFlag::new(),
            &ticking,
        )
        .unwrap_err();
    assert_eq!(midway.code, ErrorCode::TimedOut);
}

#[test]
fn at38_two_history_workers_query_while_commits_continue() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    let workers: Vec<_> = (0..2)
        .map(|worker| {
            let history = harness.history.clone();
            let clock = Arc::clone(&harness.clock);
            let key = key.clone();
            std::thread::spawn(move || {
                let stats = [
                    HistoryStat::Mean,
                    HistoryStat::Max,
                    HistoryStat::Raw,
                    HistoryStat::Min,
                ];
                let mut answered = 0u32;
                for round in 0..300u32 {
                    let stat = stats[usize::try_from((round + worker) % 4).unwrap()];
                    let request = query(
                        &key,
                        (0, 600 * SECOND),
                        if round % 2 == 0 { 0 } else { 60_000 },
                        stat,
                        500,
                    );
                    let result = history
                        .query(&request, u64::MAX, &CancelFlag::new(), clock.as_ref())
                        .unwrap();
                    assert_well_formed(&result);
                    answered += 1;
                }
                answered
            })
        })
        .collect();
    for second in 0..600u64 {
        let ack = harness.commit(
            vec![gauge(&key, 42.0, second * SECOND)],
            vec![],
            second * SECOND,
        );
        assert!(ack.status.is_committed());
        if second % 60 == 0 {
            harness.maintain(second * SECOND);
        }
    }
    for worker in workers {
        assert_eq!(worker.join().unwrap(), 300);
    }
    let final_result = harness
        .query(&key, (0, 600 * SECOND), 60_000, HistoryStat::Mean, 100)
        .unwrap();
    assert!(final_result.coverage_ratio > 0.99);
    assert_eq!(
        harness.ledger.used(Partition::HistoryQuery),
        0,
        "no query keeps memory after returning"
    );
}

#[test]
fn invalid_queries_are_validation_failures() {
    let (harness, key) = filled(10, config());
    let mut future_schema = query(&key, (0, SECOND), 0, HistoryStat::Mean, 10);
    future_schema.schema_version = 2;
    let cases = vec![
        query(&key, (5 * SECOND, 5 * SECOND), 0, HistoryStat::Mean, 10),
        query(&key, (0, SECOND), 50, HistoryStat::Mean, 10),
        query(&key, (0, SECOND), 0, HistoryStat::Mean, 0),
        query(&key, (0, SECOND), 0, HistoryStat::Mean, 2001),
        future_schema,
        query(&key, (0, 10 * SECOND), 0, HistoryStat::Total, 10),
    ];
    for request in cases {
        let error = harness
            .query_with(
                &request,
                u64::MAX,
                &CancelFlag::new(),
                harness.clock.as_ref(),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ValidationFailed, "{request:?}");
        assert_eq!(error.request_id, Some(request.request_id));
    }
    let mut capped = config();
    capped.store.max_history_points = 100;
    let (small, small_key) = filled(10, capped);
    let error = small
        .query(&small_key, (0, SECOND), 0, HistoryStat::Mean, 101)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::ValidationFailed);
}

#[test]
fn query_memory_is_reserved_up_front_and_released() {
    let ledger = ByteLedger::new(PartitionLimits::for_memory_limit_mib(1));
    let mut harness = Harness::build(config(), ledger.clone(), Namespace::Live, false);
    let key = temperature_key("gpu:0");
    for second in 0..20u64 {
        harness.commit(
            vec![gauge(&key, 1.0, second * SECOND)],
            vec![],
            second * SECOND,
        );
    }
    let error = harness
        .query(&key, (0, 20 * SECOND), 0, HistoryStat::Mean, 2000)
        .unwrap_err();
    // RUNTIME.md §5: a history query without memory capacity is Busy.
    assert_eq!(error.code, ErrorCode::Busy);
    assert!(error.retryable);
    let small = harness
        .query(&key, (0, 20 * SECOND), 0, HistoryStat::Mean, 10)
        .unwrap();
    assert!(!small.points.is_empty());
    assert_eq!(ledger.used(Partition::HistoryQuery), 0);
}

#[test]
fn source_filter_keeps_only_the_selected_source() {
    let (harness, key) = filled(10, config());
    let mut matching = query(&key, (0, 10 * SECOND), 0, HistoryStat::Mean, 100);
    matching.source_filter = Some(source_id("nvml:0:temp"));
    let result = harness
        .query_with(
            &matching,
            u64::MAX,
            &CancelFlag::new(),
            harness.clock.as_ref(),
        )
        .unwrap();
    assert_eq!(result.covered_ns, 10 * SECOND);
    assert!(
        result
            .points
            .iter()
            .all(|point| point.source_ids == vec![source_id("nvml:0:temp")])
    );
    let mut other = matching.clone();
    other.source_filter = Some(source_id("hwmon:other"));
    let filtered = harness
        .query_with(&other, u64::MAX, &CancelFlag::new(), harness.clock.as_ref())
        .unwrap();
    assert_eq!(filtered.covered_ns, 0);
    assert!(
        filtered
            .points
            .iter()
            .all(|point| point.reason.as_deref() == Some("source_filtered"))
    );
}
