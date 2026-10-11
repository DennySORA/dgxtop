//! The history the REF history queries are served from. Replay data is never
//! history-eligible (ADR08), so after warm-up the bench preloads a private
//! live-namespace hot store with the realistic 24 h shape of
//! `perf::history_load` for exactly the history-capable series the replay
//! snapshot holds (same host/boot identity, so the Manager's queries hit
//! them), then keeps appending at the live 1 Hz cadence from a harness
//! thread whose CPU is accounted as harness overhead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dgxtop::live_store::LiveStore;
use dgxtop::perf::history_load::{HistoryFeeder, PreloadReport, PreloadShape, history_series};
use dgxtop::perf::threads::HarnessCpu;
use dgxtop_core::budget::{ByteLedger, Partition};
use dgxtop_core::canonical::ImmutableSnapshot;
use dgxtop_core::clock::{NANOS_PER_SECOND, TimeProvider};
use dgxtop_core::config::Config;
use dgxtop_core::ports::StoreWriter;
use dgxtop_core::quality::Namespace;
use dgxtop_history::HistoryIndex;
use dgxtop_state::StateWriter;

use crate::common::BenchResult;

/// Data before boot cannot exist; keep the preload this far after boot.
const BOOT_MARGIN_NS: u64 = 120 * NANOS_PER_SECOND;

pub struct ShadowHistory {
    pub index: HistoryIndex,
    pub ledger: ByteLedger,
    pub report: PreloadReport,
    pub preload_seconds: f64,
    stop: Arc<AtomicBool>,
    feeder: Option<JoinHandle<u64>>,
}

impl ShadowHistory {
    /// Preloads up to 24 h ending now and starts the 1 Hz feeder.
    pub fn start(
        snapshot: &ImmutableSnapshot,
        config: &Config,
        seed: u64,
        clock: Arc<dyn TimeProvider>,
        harness: &HarnessCpu,
    ) -> BenchResult<Self> {
        let keys = snapshot
            .domains
            .values()
            .flat_map(|domain| domain.samples.keys().cloned());
        let limit = usize::try_from(config.store.max_series).unwrap_or(512);
        let series = history_series(keys, limit);
        if series.is_empty() {
            return Err("the replay snapshot holds no history-capable series yet".to_owned());
        }
        let ledger = ByteLedger::for_memory_limit_mib(u64::from(config.store.memory_limit_mib));
        let (mut writer, _snapshots, index) = LiveStore::new(
            &config.store,
            &config.sampling,
            ledger.clone(),
            Namespace::Live,
            false,
        )
        .into_parts();
        let mut feeder = HistoryFeeder::new(series, seed, &config.sampling)
            .map_err(|error| error.to_string())?;
        let now = clock.now_boot_ns();
        let shape = PreloadShape::reference();
        let span = shape.span_ns.min(now.saturating_sub(BOOT_MARGIN_NS));
        let started = Instant::now();
        let report = feeder.preload(&mut writer, shape.with_span(span), now);
        let preload_seconds = started.elapsed().as_secs_f64();
        let stop = Arc::new(AtomicBool::new(false));
        let feeder_thread =
            spawn_feeder(feeder, writer, clock, Arc::clone(&stop), harness.clone())?;
        Ok(Self {
            index,
            ledger,
            report,
            preload_seconds,
            stop,
            feeder: Some(feeder_thread),
        })
    }

    /// Stops the feeder; returns how many appends it made.
    pub fn stop(mut self) -> u64 {
        self.stop.store(true, Ordering::Release);
        self.feeder
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or(0)
    }
}

/// History bytes a live process would hold for the shadow's data (raw
/// history and aggregates; the shadow's hot state duplicates the replay
/// store's and is left out).
pub fn history_bytes(ledger: &ByteLedger) -> u64 {
    [Partition::RawHistory, Partition::Aggregates]
        .into_iter()
        .map(|partition| ledger.used(partition))
        .sum()
}

fn spawn_feeder(
    mut feeder: HistoryFeeder,
    mut writer: StateWriter,
    clock: Arc<dyn TimeProvider>,
    stop: Arc<AtomicBool>,
    harness: HarnessCpu,
) -> BenchResult<JoinHandle<u64>> {
    std::thread::Builder::new()
        .name("bench-history-feeder".to_owned())
        .spawn(move || {
            let slot = harness.register();
            let mut appends = 0u64;
            let mut next = Instant::now() + Duration::from_secs(1);
            while !stop.load(Ordering::Acquire) {
                let now = Instant::now();
                if now < next {
                    std::thread::sleep((next - now).min(Duration::from_millis(50)));
                    continue;
                }
                next += Duration::from_secs(1);
                let at = clock.now_boot_ns();
                feeder.append(&mut writer, at, NANOS_PER_SECOND);
                writer.maintain(at);
                appends += 1;
                slot.refresh();
            }
            appends
        })
        .map_err(|error| error.to_string())
}
