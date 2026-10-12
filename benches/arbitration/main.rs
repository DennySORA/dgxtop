//! Arbitration micro benchmark: the real collectors (multi-source
//! arbitration, alias de-duplication, conflicts, counter deltas, process and
//! GPU joins) over the seeded REF workload, in process and single-threaded.
//!
//! Measured: wall time of `Collector::collect` per collector and per cycle
//! (all collectors back to back), after a warm-up, over three runs.
//! `bench/budgets.json` has no arbitration entry; collection is one stage of
//! `projection_p95_ms` (read finished → projection published), so the
//! per-cycle p95 is compared with that end-to-end budget only as a
//! necessary condition, never reported as a pass of it.

#[path = "../common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use common::{BenchOutcome, BenchResult, RawSamples};
use dgxtop::perf::fixture::FixtureCensus;
use dgxtop::perf::latency::{LatencySummary, duration_us, summarize_us};
use dgxtop::perf::pipeline::ReplayPipeline;
use dgxtop::perf::report::{BenchMode, RunRecord};
use dgxtop_adapters::replay::generate_ref_fixture;
use dgxtop_core::canonical::CollectionBatch;
use dgxtop_core::quality::SelectionState;
use serde::Serialize;
use serde_json::json;

const BENCH: &str = "arbitration";
const RUNS: usize = 3;
/// Boot time of the first cycle (any value well above zero).
const START_BOOT_NS: u64 = 1_000_000_000_000;

struct Sizes {
    warmup_cycles: u32,
    measured_cycles: u32,
}

const fn sizes(mode: BenchMode) -> Sizes {
    match mode {
        BenchMode::Smoke => Sizes {
            warmup_cycles: 20,
            measured_cycles: 120,
        },
        BenchMode::Full => Sizes {
            warmup_cycles: 120,
            measured_cycles: 1800,
        },
    }
}

/// Arbitration outcomes of the measured cycles (proof the work happened).
#[derive(Debug, Default, Serialize)]
struct OutcomeCounts {
    canonical: u64,
    selected: u64,
    degraded: u64,
    conflict: u64,
    unavailable: u64,
    explanations: u64,
    failures: u64,
}

impl OutcomeCounts {
    fn absorb(&mut self, batch: &CollectionBatch) {
        for value in &batch.canonical {
            self.canonical += 1;
            match value.selection_state {
                SelectionState::Selected => self.selected += 1,
                SelectionState::Degraded => self.degraded += 1,
                SelectionState::Conflict => self.conflict += 1,
                SelectionState::Unavailable => self.unavailable += 1,
            }
        }
        self.explanations += u64::try_from(batch.explanations.len()).unwrap_or(u64::MAX);
        self.failures += u64::try_from(batch.failures.len()).unwrap_or(u64::MAX);
    }
}

#[derive(Default)]
struct RunTimings {
    cycle_us: Vec<u64>,
    collector_us: BTreeMap<String, Vec<u64>>,
    outcomes: OutcomeCounts,
}

fn main() -> ExitCode {
    common::run_bench(BENCH, run)
}

fn run() -> BenchResult<PathBuf> {
    let args = common::parse_args(BENCH, false)?;
    let inputs = common::load_inputs()?;
    let config = common::reference_config()?;
    let sizes = sizes(args.mode);
    let paths = common::output_paths(BENCH, &args);
    let environment = common::environment(
        "none (collectors only; no viewer)",
        "single-threaded in-process loop; reads untimed, collect() timed",
    )?;
    let profile = inputs.profile.replay_profile(0);
    let fixture = generate_ref_fixture(inputs.seed, &profile);
    let fixture_bytes = fixture.to_json_bytes().map_err(|error| error.to_string())?;
    let census = FixtureCensus::of(&fixture);
    let mut pipeline = ReplayPipeline::new(&fixture, profile.adapter_hosts, &config, START_BOOT_NS)
        .map_err(|error| error.to_string())?;
    drop(fixture);
    for _ in 0..sizes.warmup_cycles {
        run_cycle(&mut pipeline, None);
    }
    let mut evidence = common::open_evidence(&paths)?;
    let mut runs = Vec::new();
    let mut run_details = Vec::new();
    for index in 1..=RUNS {
        let started = Instant::now();
        let mut timings = RunTimings::default();
        for _ in 0..sizes.measured_cycles {
            run_cycle(&mut pipeline, Some(&mut timings));
        }
        let duration = started.elapsed().as_secs_f64();
        write_run_evidence(&mut evidence, index, &timings)?;
        run_details.push(RunDetail::from(timings));
        runs.push(RunRecord {
            duration_seconds: duration,
            ..RunRecord::default()
        });
    }
    let worst_cycle_p95 = run_details
        .iter()
        .filter_map(|detail| detail.cycle.as_ref().map(|summary| summary.p95_ms))
        .fold(f64::NAN, f64::max);
    let projection_budget = inputs
        .budgets
        .limits
        .get("projection_p95_ms")
        .and_then(|bound| bound.max);
    let outcome = BenchOutcome {
        bench: BENCH,
        mode: args.mode,
        runs,
        soak: None,
        details: json!({
            "fixture": {
                "seed": inputs.seed,
                "sha256": dgxtop_core::digest::sha256_hex(&fixture_bytes),
                "bytes": fixture_bytes.len(),
                "census": census,
                "simulated_latency_us": 0,
            },
            "warmup_cycles": sizes.warmup_cycles,
            "measured_cycles_per_run": sizes.measured_cycles,
            "runs": run_details,
            "component_check": {
                "stage": "collect() of every collector for one cycle",
                "end_to_end_budget": "projection_p95_ms",
                "end_to_end_max_ms": projection_budget,
                "worst_run_cycle_p95_ms": worst_cycle_p95,
                "necessary_condition_holds": projection_budget.map(|max| worst_cycle_p95 < max),
                "meaning": "collection alone must fit in the end-to-end budget; this is not a measurement of projection_p95_ms",
            },
        }),
        extra_blockers: vec![
            "micro benchmark: measures collector arbitration only; no REF report metric is measured here".to_owned(),
        ],
        notes: vec![
            format!(
                "worst per-cycle collect p95 {worst_cycle_p95:.3} ms (component of projection_p95_ms, budget {})",
                projection_budget.map_or_else(|| "undefined".to_owned(), |max| format!("{max} ms"))
            ),
            "fixture generated with simulated_latency_us=0: host read latency does not affect in-process collection".to_owned(),
        ],
    };
    common::finish(
        outcome,
        &inputs,
        &config,
        environment,
        &mut evidence,
        &paths,
    )
}

/// Reads one cycle (untimed) and runs every collector on it.
fn run_cycle(pipeline: &mut ReplayPipeline, mut timings: Option<&mut RunTimings>) {
    let batches = pipeline.read_next_cycle();
    let plans = pipeline.plans(&batches);
    let mut cycle_us = 0u64;
    for (collector, plan) in plans {
        let started = Instant::now();
        let batch = collector.collect(&plan);
        let elapsed = duration_us(started.elapsed());
        cycle_us = cycle_us.saturating_add(elapsed);
        if let Some(timings) = timings.as_deref_mut() {
            timings
                .collector_us
                .entry(collector.id().to_string())
                .or_default()
                .push(elapsed);
            timings.outcomes.absorb(&batch);
        }
        pipeline.adopt(&batch, plan.state_revision.saturating_add(1));
    }
    if let Some(timings) = timings {
        timings.cycle_us.push(cycle_us);
    }
}

#[derive(Serialize)]
struct RunDetail {
    cycle: Option<LatencySummary>,
    collectors: BTreeMap<String, Option<LatencySummary>>,
    outcomes: OutcomeCounts,
}

impl From<RunTimings> for RunDetail {
    fn from(timings: RunTimings) -> Self {
        Self {
            cycle: summarize_us(&timings.cycle_us),
            collectors: timings
                .collector_us
                .iter()
                .map(|(id, samples)| (id.clone(), summarize_us(samples)))
                .collect(),
            outcomes: timings.outcomes,
        }
    }
}

fn write_run_evidence(
    evidence: &mut dgxtop::perf::evidence::EvidenceWriter,
    index: usize,
    timings: &RunTimings,
) -> BenchResult<()> {
    common::write_samples(
        evidence,
        &format!("run{index}/collect_cycle_us.json"),
        &RawSamples {
            stream: "collect_cycle",
            unit: "us",
            definition: "sum of Collector::collect wall time of every collector for one sampling cycle",
            samples: &timings.cycle_us,
        },
    )?;
    evidence
        .write_json(
            &format!("run{index}/collect_per_collector_us.json"),
            &timings.collector_us,
        )
        .map_err(|error| format!("evidence: {error}"))
}
