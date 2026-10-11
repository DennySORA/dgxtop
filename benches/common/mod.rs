//! Plumbing shared by the dgxtop benches (included with `#[path]` by each
//! `harness = false` bench): arguments, paths, environment, the evidence
//! directory, schema validation and the console summary.
//!
//! Every bench includes this file and uses a different subset of it, hence
//! the module-wide `dead_code` allowance.
#![allow(dead_code)]

pub mod headless;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dgxtop::perf::environment::{EnvironmentDetails, probe};
use dgxtop::perf::evidence::{EvidenceWriter, ReportPaths, report_paths};
use dgxtop::perf::report::{
    AssembledReport, BenchMode, ReportInputs, RunRecord, acceptance_blockers, assemble, run_gates,
};
use dgxtop::perf::scenarios::{REF_PROFILE, ScenarioProfile, Scenarios};
use dgxtop::perf::verdict::VerdictStatus;
use dgxtop::perf::{Budgets, Environment, ReportKind, SoakResult};
use dgxtop::version::BuildInfo;
use dgxtop_core::config::{Config, DEFAULT_CONFIG_JSON, remove_retired_keys};
use serde::Serialize;
use serde_json::{Value, json};

/// A bench failure with a message for the console.
pub type BenchResult<T> = Result<T, String>;

/// Command line of a bench (`cargo bench --features bench-support --bench <name> -- [args]`).
#[derive(Debug, Clone)]
pub struct BenchArgs {
    pub mode: BenchMode,
    pub out: Option<PathBuf>,
    pub soak_results: Option<PathBuf>,
}

/// Parses `--smoke` (default) / `--full`, `--out PATH` and, where allowed,
/// `--soak-results PATH`. `DGXTOP_BENCH_MODE=full|smoke` selects the mode
/// too; an explicit flag wins. Cargo's own `--bench` flag is ignored.
pub fn parse_args(bench: &str, accepts_soak: bool) -> BenchResult<BenchArgs> {
    let mut mode = match std::env::var("DGXTOP_BENCH_MODE").ok().as_deref() {
        None | Some("") | Some("smoke") => BenchMode::Smoke,
        Some("full") => BenchMode::Full,
        Some(other) => {
            return Err(format!(
                "DGXTOP_BENCH_MODE must be smoke or full (got {other})"
            ));
        }
    };
    let mut out = None;
    let mut soak_results = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--bench" => {}
            "--smoke" => mode = BenchMode::Smoke,
            "--full" => mode = BenchMode::Full,
            "--out" => out = Some(PathBuf::from(arguments.next().ok_or("--out needs a path")?)),
            "--soak-results" if accepts_soak => {
                soak_results = Some(PathBuf::from(
                    arguments.next().ok_or("--soak-results needs a path")?,
                ));
            }
            "--help" | "-h" => {
                println!("{}", usage(bench, accepts_soak));
                std::process::exit(0);
            }
            other => {
                if let Some(path) = other.strip_prefix("--out=") {
                    out = Some(PathBuf::from(path));
                } else {
                    return Err(format!(
                        "unknown argument {other:?}\n{}",
                        usage(bench, accepts_soak)
                    ));
                }
            }
        }
    }
    Ok(BenchArgs {
        mode,
        out,
        soak_results,
    })
}

fn usage(bench: &str, accepts_soak: bool) -> String {
    let soak = if accepts_soak {
        " [--soak-results SOAK.json]"
    } else {
        ""
    };
    format!(
        "usage: cargo bench -p dgxtop --features bench-support --bench {bench} -- [--smoke|--full] [--out REPORT.json]{soak}\n\
         default: --smoke (seconds, never acceptance); report: target/perf/{bench}-<mode>.json"
    )
}

/// `spec/dgxtop-v2` of this checkout.
pub fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/dgxtop-v2")
}

/// Cargo's target directory (parent of the benches' scratch directory).
pub fn target_dir() -> PathBuf {
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"));
    scratch
        .parent()
        .map_or_else(|| scratch.to_path_buf(), Path::to_path_buf)
}

/// Private run directory below the target scratch dir, removed on drop.
#[derive(Debug)]
pub struct RunDirectory {
    path: PathBuf,
}

impl RunDirectory {
    pub fn create(bench: &str) -> BenchResult<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "dgxtop-bench-{bench}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("run directory {}: {error}", path.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("run directory permissions: {error}"))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spec inputs every bench reads: scenarios (REF profile + seed) and budgets.
pub struct SpecInputs {
    pub seed: u64,
    pub profile: ScenarioProfile,
    pub budgets: Budgets,
}

pub fn load_spec() -> BenchResult<SpecInputs> {
    let scenarios = Scenarios::load(&spec_dir().join("bench/scenarios.json"))
        .map_err(|error| error.to_string())?;
    let profile = scenarios
        .profile(REF_PROFILE)
        .map_err(|error| error.to_string())?
        .clone();
    let budgets = Budgets::load(&spec_dir().join("bench/budgets.json"))
        .map_err(|error| format!("bench/budgets.json: {error}"))?;
    Ok(SpecInputs {
        seed: scenarios.seed,
        profile,
        budgets,
    })
}

/// The configuration every bench runs with: the REF fixed configuration,
/// the spec's default document (`examples/config.default.json`). The
/// built-in defaults differ from it by the history archive (D-51).
pub fn reference_config() -> BenchResult<Config> {
    Config::reference().map_err(|error| format!("embedded default configuration: {error}"))
}

/// The spec's default document, the REF configuration as the spec wrote it.
fn reference_document() -> Option<Value> {
    serde_json::from_str(DEFAULT_CONFIG_JSON).ok()
}

/// Whether `config` is exactly the REF default document, apart from the
/// keys dgxtop retired and never writes (D-88).
pub fn is_reference_config(config: &Config) -> bool {
    let default = reference_document().map(|mut document| {
        remove_retired_keys(&mut document);
        document
    });
    serde_json::to_value(config).ok() == default
}

/// Environment of this run.
pub fn environment(
    terminal: &str,
    background: &str,
) -> BenchResult<(Environment, EnvironmentDetails)> {
    probe(&target_dir(), &BuildInfo::current(), terminal, background)
        .map_err(|error| error.to_string())
}

/// Reads real soak results (`[{...fixed...}, {...churn...}]`) measured by a
/// soak harness; they are copied into the evidence directory.
pub fn load_soak_results(path: &Path) -> BenchResult<(Vec<SoakResult>, Vec<u8>)> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("soak results {}: {error}", path.display()))?;
    let soak: Vec<SoakResult> = serde_json::from_slice(&bytes)
        .map_err(|error| format!("soak results {}: {error}", path.display()))?;
    let mut scenarios: Vec<String> = soak
        .iter()
        .map(|entry| format!("{:?}", entry.scenario))
        .collect();
    scenarios.sort();
    if scenarios != ["Churn", "Fixed"] {
        return Err(format!(
            "soak results must hold exactly one fixed and one churn run (got {scenarios:?})"
        ));
    }
    Ok((soak, bytes))
}

/// Everything a bench hands over to be written.
pub struct BenchOutcome {
    pub bench: &'static str,
    pub mode: BenchMode,
    pub runs: Vec<RunRecord>,
    pub soak: Option<(Vec<SoakResult>, Vec<u8>)>,
    /// Bench-specific summary sections (raw numbers, notes).
    pub details: Value,
    pub extra_blockers: Vec<String>,
    pub notes: Vec<String>,
}

/// Writes evidence, assembles and schema-validates the report, writes it and
/// prints the console summary. Returns the report path.
pub fn finish(
    outcome: BenchOutcome,
    spec: &SpecInputs,
    config: &Config,
    environment: (Environment, EnvironmentDetails),
    evidence: &mut EvidenceWriter,
    paths: &ReportPaths,
) -> BenchResult<PathBuf> {
    let (environment, details) = environment;
    let io = |error: std::io::Error| format!("evidence: {error}");
    // The spec's evaluator compares the evidence with its default document,
    // which still carries the keys D-88 retired.
    match reference_document().filter(|_| is_reference_config(config)) {
        Some(document) => evidence.write_json("config.json", &document),
        None => evidence.write_json("config.json", config),
    }
    .map_err(io)?;
    let config_sha256 = evidence
        .files()
        .iter()
        .find(|file| file.path == "config.json")
        .map(|file| file.sha256.clone())
        .ok_or("config evidence missing")?;
    let mut extra_blockers = outcome.extra_blockers.clone();
    if !is_reference_config(config) {
        extra_blockers.push("configuration differs from the REF default".to_owned());
    }
    let soak = match &outcome.soak {
        Some((soak, bytes)) => {
            evidence
                .write_bytes("soak-results.json", bytes)
                .map_err(io)?;
            Some(soak.clone())
        }
        None => None,
    };
    let mut inputs = ReportInputs {
        mode: outcome.mode,
        workload: spec.profile.workload(),
        runs: outcome.runs.clone(),
        soak,
        provenance: details.build.clone(),
        config_sha256,
        config_path: "config.json".to_owned(),
        environment: environment.clone(),
        evidence_files: Vec::new(),
        extra_blockers,
    };
    let blockers = acceptance_blockers(&inputs);
    let summary = summary_document(&outcome, &spec.budgets, &blockers, &environment, &details);
    evidence.write_json("summary.json", &summary).map_err(io)?;
    inputs.evidence_files = evidence.files().to_vec();
    let assembled = assemble(inputs);
    let document = serde_json::to_value(&assembled.report).map_err(|error| error.to_string())?;
    validate_against_schema(&document)?;
    write_report(&paths.report, &document)?;
    print_console(&outcome, &spec.budgets, &assembled, paths);
    Ok(paths.report.clone())
}

fn summary_document(
    outcome: &BenchOutcome,
    budgets: &Budgets,
    blockers: &[String],
    environment: &Environment,
    details: &EnvironmentDetails,
) -> Value {
    let kind = if blockers.is_empty() {
        "measured"
    } else {
        "synthetic_example"
    };
    let runs: Vec<Value> = outcome
        .runs
        .iter()
        .map(|run| {
            json!({
                "duration_seconds": run.duration_seconds,
                "sample_counts": {
                    "commands": run.commands,
                    "frames": run.frames,
                    "history_queries": run.history_queries,
                },
                "measured_metrics": run.metrics,
                "not_measured": run.missing_metrics(),
                "gates": run_gates(budgets, run),
            })
        })
        .collect();
    json!({
        "bench": outcome.bench,
        "mode": outcome.mode,
        "report_kind": kind,
        "acceptance": blockers.is_empty(),
        "acceptance_blockers": blockers,
        "placeholder_rule": "metrics listed under not_measured are 0 placeholders in the schema report and must not be read as results",
        "environment": environment,
        "environment_details": details,
        "runs": runs,
        "details": outcome.details,
        "notes": outcome.notes,
    })
}

/// Validates a report against `contracts/performance-report.schema.json`.
pub fn validate_against_schema(document: &Value) -> BenchResult<()> {
    let path = spec_dir().join("contracts/performance-report.schema.json");
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let schema: Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .map_err(|error| format!("report schema does not compile: {error}"))?;
    let errors: Vec<String> = validator
        .iter_errors(document)
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "report violates performance-report.schema.json: {errors:#?}"
        ))
    }
}

fn write_report(path: &Path, document: &Value) -> BenchResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))
}

fn print_console(
    outcome: &BenchOutcome,
    budgets: &Budgets,
    assembled: &AssembledReport,
    paths: &ReportPaths,
) {
    println!(
        "== dgxtop bench {} ({}) ==",
        outcome.bench,
        outcome.mode.label()
    );
    for (index, run) in outcome.runs.iter().enumerate() {
        println!(
            "run {}: {:.1} s, samples: commands {} frames {} history {}",
            index + 1,
            run.duration_seconds,
            run.commands,
            run.frames,
            run.history_queries
        );
        for gate in run_gates(budgets, run)
            .into_iter()
            .filter(|gate| gate_applies(gate, run))
        {
            let bound = gate.max.map_or_else(
                || gate.min.map_or_else(String::new, |min| format!(">= {min}")),
                |max| format!("<= {max}"),
            );
            println!(
                "    {:<32} {:>12.3} {:<12} {:?}",
                gate.metric,
                gate.measured.unwrap_or(f64::NAN),
                bound,
                gate.status
            );
        }
    }
    let kind = match assembled.report.kind {
        ReportKind::Measured => "measured",
        ReportKind::SyntheticExample => "synthetic_example (not acceptance evidence)",
    };
    println!("report kind: {kind}");
    for blocker in &assembled.blockers {
        println!("  blocker: {blocker}");
    }
    for note in &outcome.notes {
        println!("  note: {note}");
    }
    println!("report:   {}", paths.report.display());
    println!("evidence: {}", paths.evidence_dir.display());
}

/// Whether a gate concerns what this run measured: measured limits, sample
/// minimums of streams the run sampled, and the duration minimum only for an
/// end-to-end run (one that measured process-tree CPU). summary.json keeps
/// every gate; this only trims the console.
fn gate_applies(gate: &dgxtop::perf::verdict::BudgetVerdict, run: &RunRecord) -> bool {
    match gate.metric.as_str() {
        "minimum_duration_seconds" => run.metric("cpu_mean_one_core_percent").is_some(),
        "minimum_samples.commands" => run.commands > 0,
        "minimum_samples.frames" => run.frames > 0,
        "minimum_samples.history_queries" => run.history_queries > 0,
        _ => gate.status != VerdictStatus::NotMeasured,
    }
}

/// Output paths for `bench` honouring `--out`.
pub fn output_paths(bench: &str, args: &BenchArgs) -> ReportPaths {
    report_paths(&target_dir(), bench, args.mode, args.out.as_deref())
}

/// Opens the evidence directory of a report.
pub fn open_evidence(paths: &ReportPaths) -> BenchResult<EvidenceWriter> {
    EvidenceWriter::create(&paths.evidence_dir)
        .map_err(|error| format!("{}: {error}", paths.evidence_dir.display()))
}

/// Raw samples as an evidence file (`{"unit": "us", "samples": [...]}`).
#[derive(Serialize)]
pub struct RawSamples<'a> {
    pub stream: &'a str,
    pub unit: &'a str,
    pub definition: &'a str,
    pub samples: &'a [u64],
}

pub fn write_samples(
    evidence: &mut EvidenceWriter,
    relative: &str,
    samples: &RawSamples<'_>,
) -> BenchResult<()> {
    evidence
        .write_json(relative, samples)
        .map_err(|error| format!("evidence {relative}: {error}"))
}

/// Runs `main_body`, printing a failure and exiting non-zero on error.
pub fn run_bench(
    name: &str,
    main_body: impl FnOnce() -> BenchResult<PathBuf>,
) -> std::process::ExitCode {
    match main_body() {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bench {name} failed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
