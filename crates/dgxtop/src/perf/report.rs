//! Assembling `performance-report.schema.json` documents honestly.
//!
//! The schema has no field for "partially measured", so every bench run is
//! recorded as [`RunRecord`]s holding only what was measured, and
//! [`assemble`] decides the report `kind`:
//!
//! * `measured` only for a complete acceptance-grade report: full mode,
//!   every REF metric of all three runs measured from samples, real soak
//!   results, the REF workload and a clean, known source commit;
//! * `synthetic_example` otherwise, with every reason listed as a blocker.
//!   Unmeasured fields then hold `0` placeholders inside a document the
//!   schema itself declares not to be evidence (the evaluator rejects it),
//!   and the bench summary marks each of them `not_measured`.
//!
//! Duration and sample-count minimums are left to the evaluator's gates: a
//! short or thin *measured* run is reported as measured and fails there.

use std::collections::BTreeMap;

use serde::Serialize;

use super::environment::BuildProvenance;
use super::verdict::{BudgetVerdict, VerdictStatus};
use super::{
    Budgets, Environment, EvidenceFile, PerformanceReport, ReportKind, RunMetrics, RunResult,
    SampleCounts, SoakResult, SoakScenario, Workload,
};

/// Runs a report must contain (schema `runs`: exactly three).
pub const REQUIRED_RUNS: usize = 3;

/// The schema's per-run metrics, in schema order.
pub const REF_METRICS: [&str; 14] = [
    "cpu_mean_one_core_percent",
    "rss_tree_peak_mib",
    "managed_peak_mib",
    "command_p95_ms",
    "command_p99_ms",
    "frame_p95_ms",
    "frame_p99_ms",
    "projection_p95_ms",
    "history_p95_ms",
    "coverage_percent",
    "unexplained_drops",
    "adaptive_throttle_events",
    "crashes",
    "deadlocks",
];

/// How long and how thoroughly a bench measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchMode {
    /// Seconds-long run for CI and development; never acceptance evidence.
    Smoke,
    /// The `bench/scenarios.json` duration (REF: warm-up + 1800 s × 3).
    Full,
}

impl BenchMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Full => "full",
        }
    }
}

/// One measured metric with the number of samples behind it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MetricValue {
    pub value: f64,
    pub samples: u64,
}

/// What one run measured; absent metrics were not measured by this bench.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunRecord {
    pub duration_seconds: f64,
    pub commands: u64,
    pub frames: u64,
    pub history_queries: u64,
    pub metrics: BTreeMap<&'static str, MetricValue>,
}

impl RunRecord {
    /// Records a REF metric measured from `samples` samples. A non-finite
    /// value or an unknown name is rejected rather than stored.
    pub fn set(&mut self, metric: &'static str, value: f64, samples: u64) -> Result<(), String> {
        if !REF_METRICS.contains(&metric) {
            return Err(format!("{metric} is not a REF report metric"));
        }
        if !value.is_finite() || value < 0.0 {
            return Err(format!(
                "{metric}={value} is not a finite non-negative measurement"
            ));
        }
        self.metrics.insert(metric, MetricValue { value, samples });
        Ok(())
    }

    pub fn metric(&self, name: &str) -> Option<MetricValue> {
        self.metrics.get(name).copied()
    }

    pub fn missing_metrics(&self) -> Vec<&'static str> {
        REF_METRICS
            .iter()
            .copied()
            .filter(|name| !self.metrics.contains_key(name))
            .collect()
    }

    /// Schema run; unmeasured metrics become `0` placeholders.
    pub fn to_schema(&self) -> RunResult {
        let value = |name: &str| self.metric(name).map_or(0.0, |metric| metric.value);
        RunResult {
            duration_seconds: self.duration_seconds.max(0.0),
            sample_counts: SampleCounts {
                commands: self.commands,
                frames: self.frames,
                history_queries: self.history_queries,
            },
            metrics: RunMetrics {
                cpu_mean_one_core_percent: value("cpu_mean_one_core_percent"),
                rss_tree_peak_mib: value("rss_tree_peak_mib"),
                managed_peak_mib: value("managed_peak_mib"),
                command_p95_ms: value("command_p95_ms"),
                command_p99_ms: value("command_p99_ms"),
                frame_p95_ms: value("frame_p95_ms"),
                frame_p99_ms: value("frame_p99_ms"),
                projection_p95_ms: value("projection_p95_ms"),
                history_p95_ms: value("history_p95_ms"),
                coverage_percent: value("coverage_percent"),
                unexplained_drops: value("unexplained_drops"),
                adaptive_throttle_events: value("adaptive_throttle_events"),
                crashes: value("crashes"),
                deadlocks: value("deadlocks"),
            },
        }
    }
}

/// Soak entries for a report without soak measurements: zero-length runs
/// that the evaluator fails on its duration/warm-up minimums.
pub fn soak_not_run() -> Vec<SoakResult> {
    [SoakScenario::Fixed, SoakScenario::Churn]
        .into_iter()
        .map(|scenario| SoakResult {
            duration_seconds: 0.0,
            warmup_seconds: 0.0,
            rss_growth_after_warmup_mib: 0.0,
            fd_growth_after_warmup: 0.0,
            thread_growth_after_warmup: 0.0,
            crashes: 0.0,
            deadlocks: 0.0,
            unexplained_drops: 0.0,
            scenario,
        })
        .collect()
}

/// Everything a report is assembled from.
#[derive(Debug, Clone)]
pub struct ReportInputs {
    pub mode: BenchMode,
    pub workload: Workload,
    pub runs: Vec<RunRecord>,
    /// Real soak results (fixed + churn) measured by a soak harness.
    pub soak: Option<Vec<SoakResult>>,
    pub provenance: BuildProvenance,
    pub config_sha256: String,
    pub config_path: String,
    pub environment: Environment,
    pub evidence_files: Vec<EvidenceFile>,
    /// Bench-specific reasons the result is not acceptance-grade.
    pub extra_blockers: Vec<String>,
}

/// A schema document plus the reasons it is not `measured`.
#[derive(Debug, Clone)]
pub struct AssembledReport {
    pub report: PerformanceReport,
    pub blockers: Vec<String>,
}

pub fn assemble(inputs: ReportInputs) -> AssembledReport {
    let blockers = acceptance_blockers(&inputs);
    let kind = if blockers.is_empty() {
        ReportKind::Measured
    } else {
        ReportKind::SyntheticExample
    };
    let report = PerformanceReport {
        schema_version: 1,
        kind,
        profile: "REF".to_owned(),
        source_commit: inputs.provenance.report_commit(),
        config_sha256: inputs.config_sha256,
        environment: inputs.environment,
        evidence_files: inputs.evidence_files,
        runs: inputs.runs.iter().map(RunRecord::to_schema).collect(),
        soak_runs: inputs.soak.unwrap_or_else(soak_not_run),
        workload: inputs.workload,
        config_path: inputs.config_path,
    };
    AssembledReport { report, blockers }
}

/// Why `inputs` would not make a `measured` report (empty = it would).
pub fn acceptance_blockers(inputs: &ReportInputs) -> Vec<String> {
    let mut blockers = Vec::new();
    if inputs.mode == BenchMode::Smoke {
        blockers.push("smoke mode: shortened warm-up and measurement, never acceptance".to_owned());
    }
    if inputs.runs.len() != REQUIRED_RUNS {
        blockers.push(format!(
            "{} runs recorded; exactly {REQUIRED_RUNS} independent runs are required",
            inputs.runs.len()
        ));
    }
    for (index, run) in inputs.runs.iter().enumerate() {
        let missing = run.missing_metrics();
        if !missing.is_empty() {
            blockers.push(format!(
                "run {}: not measured: {}",
                index + 1,
                missing.join(", ")
            ));
        }
        let unsampled: Vec<&str> = run
            .metrics
            .iter()
            .filter(|(_, metric)| metric.samples == 0)
            .map(|(name, _)| *name)
            .collect();
        if !unsampled.is_empty() {
            blockers.push(format!(
                "run {}: no samples behind: {}",
                index + 1,
                unsampled.join(", ")
            ));
        }
    }
    if inputs.soak.is_none() {
        blockers.push(
            "soak_runs not measured (72 h fixed + churn soak, AT29); zero-length placeholders"
                .to_owned(),
        );
    }
    if !inputs.provenance.identifies_source() {
        blockers.push(format!(
            "source commit not identifiable (build commit {:?}{})",
            inputs.provenance.raw_commit,
            if inputs.provenance.dirty {
                ", dirty tree"
            } else {
                ""
            }
        ));
    }
    if inputs.workload != Workload::reference() {
        blockers.push("workload differs from the REF profile".to_owned());
    }
    blockers.extend(inputs.extra_blockers.iter().cloned());
    blockers
}

/// Every budget gate of one run: the limits, the minimum duration and the
/// minimum sample counts. Unmeasured limits are `not_measured`.
#[allow(clippy::cast_precision_loss)]
pub fn run_gates(budgets: &Budgets, run: &RunRecord) -> Vec<BudgetVerdict> {
    let mut gates: Vec<BudgetVerdict> = budgets
        .limits
        .keys()
        .filter_map(|name| {
            let metric = run.metric(name);
            budgets.verdict(
                name,
                metric.map(|metric| metric.value),
                metric.map_or(0, |metric| metric.samples),
            )
        })
        .collect();
    let minimum = |metric: String, required: f64, measured: f64| BudgetVerdict {
        metric,
        max: None,
        min: Some(required),
        unit: "minimum".to_owned(),
        measured: Some(measured),
        samples: 1,
        status: if measured >= required {
            VerdictStatus::Pass
        } else {
            VerdictStatus::Fail
        },
    };
    gates.push(minimum(
        "minimum_duration_seconds".to_owned(),
        budgets.minimum_duration_seconds,
        run.duration_seconds,
    ));
    for (stream, count) in [
        ("commands", run.commands),
        ("frames", run.frames),
        ("history_queries", run.history_queries),
    ] {
        if let Some(required) = budgets.minimum_samples.get(stream) {
            gates.push(minimum(
                format!("minimum_samples.{stream}"),
                *required as f64,
                count as f64,
            ));
        }
    }
    gates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::environment::NULL_COMMIT;
    use crate::version::BuildInfo;
    use std::path::Path;

    fn provenance(commit: &'static str) -> BuildProvenance {
        BuildProvenance::from_build(&BuildInfo {
            commit,
            ..BuildInfo::current()
        })
    }

    fn environment() -> Environment {
        Environment {
            cpu: "test".into(),
            architecture: "x86_64".into(),
            os_kernel: "Linux test".into(),
            logical_cpus: 4,
            ram_mib: 16384,
            rustc: "rustc test".into(),
            sqlite: "3.53".into(),
            gpu_driver: "none".into(),
            filesystem: "ext4".into(),
            terminal: "headless".into(),
            background_load: "idle".into(),
        }
    }

    fn complete_run() -> RunRecord {
        let mut run = RunRecord {
            duration_seconds: 1800.0,
            commands: 18_000,
            frames: 7_000,
            history_queries: 2_000,
            ..RunRecord::default()
        };
        for name in REF_METRICS {
            run.set(name, 1.0, 10).unwrap();
        }
        run
    }

    fn inputs(
        mode: BenchMode,
        runs: Vec<RunRecord>,
        soak: Option<Vec<SoakResult>>,
    ) -> ReportInputs {
        ReportInputs {
            mode,
            workload: Workload::reference(),
            runs,
            soak,
            provenance: provenance("b4b5a8d0123456789abcdef0123456789abcdef0"),
            config_sha256: "a".repeat(64),
            config_path: "config.json".into(),
            environment: environment(),
            evidence_files: vec![EvidenceFile {
                path: "summary.json".into(),
                sha256: "b".repeat(64),
            }],
            extra_blockers: Vec::new(),
        }
    }

    fn soak() -> Vec<SoakResult> {
        soak_not_run()
            .into_iter()
            .map(|entry| SoakResult {
                duration_seconds: 259_200.0,
                warmup_seconds: 86_400.0,
                ..entry
            })
            .collect()
    }

    #[test]
    fn only_a_complete_full_report_is_measured() {
        let runs = vec![complete_run(), complete_run(), complete_run()];
        let complete = assemble(inputs(BenchMode::Full, runs.clone(), Some(soak())));
        assert_eq!(
            complete.report.kind,
            ReportKind::Measured,
            "{:?}",
            complete.blockers
        );
        assert!(complete.blockers.is_empty());
        let smoke = assemble(inputs(BenchMode::Smoke, runs.clone(), Some(soak())));
        assert_eq!(smoke.report.kind, ReportKind::SyntheticExample);
        assert!(smoke.blockers[0].starts_with("smoke mode"));
        let no_soak = assemble(inputs(BenchMode::Full, runs, None));
        assert_eq!(no_soak.report.kind, ReportKind::SyntheticExample);
        assert_eq!(no_soak.report.soak_runs.len(), 2);
        assert!(
            no_soak
                .report
                .soak_runs
                .iter()
                .all(|soak| soak.duration_seconds == 0.0)
        );
    }

    #[test]
    fn missing_metrics_unsampled_metrics_and_unknown_commits_block_measured() {
        let mut partial = complete_run();
        partial.metrics.remove("frame_p95_ms");
        let mut unsampled = complete_run();
        unsampled.set("history_p95_ms", 3.0, 0).unwrap();
        let mut request = inputs(
            BenchMode::Full,
            vec![partial, unsampled, complete_run()],
            Some(soak()),
        );
        request.provenance = provenance("unknown");
        let assembled = assemble(request);
        assert_eq!(assembled.report.kind, ReportKind::SyntheticExample);
        assert_eq!(assembled.report.source_commit, NULL_COMMIT);
        let text = assembled.blockers.join("\n");
        assert!(text.contains("run 1: not measured: frame_p95_ms"), "{text}");
        assert!(
            text.contains("run 2: no samples behind: history_p95_ms"),
            "{text}"
        );
        assert!(text.contains("source commit not identifiable"), "{text}");
        assert_eq!(
            assembled.report.runs[0].metrics.frame_p95_ms, 0.0,
            "placeholder"
        );
    }

    #[test]
    fn only_exactly_three_runs_of_the_reference_workload_can_be_measured() {
        // Keeping only the best run (or adding a fourth) is never measured.
        for runs in [vec![complete_run()], vec![complete_run(); 4]] {
            let count = runs.len();
            let assembled = assemble(inputs(BenchMode::Full, runs, Some(soak())));
            assert_eq!(assembled.report.kind, ReportKind::SyntheticExample);
            assert!(
                assembled.blockers.iter().any(|blocker| blocker
                    == &format!(
                        "{count} runs recorded; exactly {REQUIRED_RUNS} independent runs are required"
                    )),
                "{:?}",
                assembled.blockers
            );
        }
        let mut lighter = inputs(BenchMode::Full, vec![complete_run(); 3], Some(soak()));
        lighter.workload.adapter_hosts -= 1;
        let assembled = assemble(lighter);
        assert_eq!(assembled.report.kind, ReportKind::SyntheticExample);
        assert_eq!(
            assembled.blockers,
            ["workload differs from the REF profile"]
        );
    }

    #[test]
    fn any_adaptive_throttle_event_fails_its_gate() {
        let budgets = Budgets::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches/budgets.json"),
        )
        .unwrap();
        let status = |events: f64| {
            let mut run = complete_run();
            run.set("adaptive_throttle_events", events, 10).unwrap();
            run_gates(&budgets, &run)
                .into_iter()
                .find(|gate| gate.metric == "adaptive_throttle_events")
                .map(|gate| gate.status)
                .unwrap()
        };
        assert_eq!(status(0.0), VerdictStatus::Pass);
        assert_eq!(status(1.0), VerdictStatus::Fail);
    }

    #[test]
    fn records_reject_unknown_or_non_finite_metrics() {
        let mut run = RunRecord::default();
        assert!(run.set("frames_per_second", 1.0, 1).is_err());
        assert!(run.set("frame_p95_ms", f64::INFINITY, 1).is_err());
        assert!(run.set("frame_p95_ms", -1.0, 1).is_err());
        assert_eq!(run.missing_metrics().len(), REF_METRICS.len());
    }

    #[test]
    fn gates_cover_limits_duration_and_sample_minimums() {
        let budgets = Budgets::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches/budgets.json"),
        )
        .unwrap();
        let mut run = RunRecord {
            duration_seconds: 20.0,
            commands: 200,
            ..RunRecord::default()
        };
        run.set("frame_p95_ms", 2.0, 80).unwrap();
        let gates = run_gates(&budgets, &run);
        let status = |name: &str| {
            gates
                .iter()
                .find(|gate| gate.metric == name)
                .map(|gate| gate.status)
                .unwrap()
        };
        assert_eq!(status("frame_p95_ms"), VerdictStatus::Pass);
        assert_eq!(
            status("cpu_mean_one_core_percent"),
            VerdictStatus::NotMeasured
        );
        assert_eq!(status("minimum_duration_seconds"), VerdictStatus::Fail);
        assert_eq!(status("minimum_samples.commands"), VerdictStatus::Fail);
        assert_eq!(
            gates.len(),
            budgets.limits.len() + 1 + budgets.minimum_samples.len()
        );
    }
}
