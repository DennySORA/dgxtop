//! AT38: the history's reads are bounded by a deadline and run beside the
//! HotStore's commits. The Manager reads the history only through the
//! Overview's sizes (D-82); the spec's point, byte and scan caps belonged
//! to the query engine D-90 removed.

#[path = "store_hot_support.rs"]
mod support;

use std::sync::atomic::{AtomicU64, Ordering};

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::history::SeriesInEveryBoot;
use dgxtop_core::identity::BootId;
use dgxtop_core::overview_history::{OverviewAnswer, OverviewRequest};
use dgxtop_core::ports::{CancelFlag, OverviewReader};
use dgxtop_core::testing::boot_id;
use dgxtop_core::tiers::Tier;
use support::{EVERYTHING_NS, Harness, MINUTE, SECOND, gauge, temperature_key, utc_of};

/// Clock that moves forward on every reading, to pass a deadline while
/// a read runs.
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
        i128::from(utc_of(self.now.load(Ordering::SeqCst)))
    }
}

fn day_of_bars(key: &dgxtop_core::identity::MetricKey, at: u64) -> OverviewRequest {
    OverviewRequest::Bars {
        series: SeriesInEveryBoot::from(key),
        tier: Tier::OneMinute,
        from_utc_ns: utc_of(at.saturating_sub(EVERYTHING_NS)),
        end_utc_ns: utc_of(at + MINUTE),
        at_utc_ns: utc_of(at),
        parts_of: std::collections::BTreeSet::new(),
    }
}

#[test]
fn at38_deadline_returns_timed_out() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    for second in 0..300u64 {
        harness.commit(
            vec![gauge(&key, (second % 50) as f64, second * SECOND)],
            vec![],
            second * SECOND,
        );
    }
    let request = day_of_bars(&key, 300 * SECOND);
    let clock = harness.reader_clock();
    let passed = harness
        .read_with(
            &request,
            clock.now_boot_ns(),
            &CancelFlag::new(),
            clock.as_ref(),
        )
        .unwrap_err();
    assert_eq!(passed.code, ErrorCode::TimedOut);
    assert!(passed.retryable);
    let cancel = CancelFlag::new();
    cancel.cancel();
    let cancelled = harness
        .read_with(&request, u64::MAX, &cancel, clock.as_ref())
        .unwrap_err();
    assert_eq!(cancelled.code, ErrorCode::Cancelled);
    let ticking = TickingClock {
        now: AtomicU64::new(300 * SECOND),
        step: 1_000_000,
    };
    let in_time = harness
        .read_with(
            &request,
            300 * SECOND + 3_000_000,
            &CancelFlag::new(),
            &ticking,
        )
        .unwrap();
    assert!(matches!(in_time, OverviewAnswer::Bars(_)));
    let late = harness
        .read_with(&request, 300 * SECOND, &CancelFlag::new(), &ticking)
        .unwrap_err();
    assert_eq!(late.code, ErrorCode::TimedOut);
}

#[test]
fn at38_two_history_workers_query_while_commits_continue() {
    let mut harness = Harness::new();
    let key = temperature_key("gpu:0");
    let workers: Vec<_> = (0..2u32)
        .map(|worker| {
            let history = harness.history.clone();
            let clock = harness.reader_clock();
            let key = key.clone();
            std::thread::spawn(move || {
                let mut answered = 0u32;
                for round in 0..300u32 {
                    let request = if (round + worker) % 2 == 0 {
                        day_of_bars(&key, 600 * SECOND)
                    } else {
                        OverviewRequest::Windows {
                            series: SeriesInEveryBoot::from(&key),
                            spans_ns: vec![3600 * SECOND, EVERYTHING_NS],
                            at_utc_ns: utc_of(600 * SECOND),
                        }
                    };
                    history
                        .read(&request, u64::MAX, &CancelFlag::new(), clock.as_ref())
                        .unwrap();
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
    harness.clock.set_boot_ns(600 * SECOND);
    let held = harness.window(&key, EVERYTHING_NS);
    assert_eq!(
        held.covered_ns,
        600 * SECOND,
        "every commit reached the history"
    );
    assert_eq!(held.mean(), Some(42.0));
    let bars = harness.bars(&key, Tier::OneMinute, (0, 600 * SECOND));
    assert_eq!(bars.len(), 10);
}
