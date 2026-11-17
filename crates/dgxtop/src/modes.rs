//! One-shot command modes: `--version`, `--check-config`, `--import-legacy`.
//! None of them samples hardware; only `--import-legacy --apply` writes.

use std::io::Write;
use std::process::ExitCode;

use dgxtop_core::config::{LegacyImportReport, LegacyMappingStatus};
use dgxtop_core::ports::LoadOrigin;

use crate::cli::Cli;
use crate::startup::{self, StartupError};
use crate::version::{self, BuildInfo, RuntimeFacts};
use std::fmt::Write as _;

/// Exit status for invalid input or configuration.
pub const EXIT_INVALID: u8 = 2;
/// Exit status for runtime failures.
pub const EXIT_FAILURE: u8 = 1;

/// SQLite runtime facts for the verbose version report.
#[must_use]
pub fn sqlite_facts() -> RuntimeFacts {
    match dgxtop_store::sqlite::ensure_sqlite_version() {
        Ok(version) => RuntimeFacts {
            sqlite_version: version.runtime_text,
            sqlite_gate_ok: true,
        },
        Err(error) => RuntimeFacts {
            sqlite_version: error.to_string(),
            sqlite_gate_ok: false,
        },
    }
}

/// `--version` / `--version --verbose`.
pub fn print_version(verbose: bool) -> ExitCode {
    let build = BuildInfo::current();
    let text = if verbose {
        version::verbose(&build, &sqlite_facts())
    } else {
        format!("{}\n", version::short(&build))
    };
    print_stdout(&text)
}

/// `--check-config`: load, validate, show configured/effective values.
pub fn check_config(cli: &Cli) -> ExitCode {
    match render_check_config(cli) {
        Ok(text) => print_stdout(&text),
        Err(error) => fail(EXIT_INVALID, &error.to_string()),
    }
}

fn render_check_config(cli: &Cli) -> Result<String, StartupError> {
    let paths = startup::resolve_paths(cli)?;
    let (loaded, _repository) =
        startup::load_configuration(cli, &paths, startup::real_filesystem())?;
    let mut text = String::new();
    writeln!(text, "config file: {}", paths.config_file.display()).unwrap_or_default();
    match &loaded.load_origin {
        LoadOrigin::Defaults => {
            text.push_str("origin: built-in defaults (no file yet; nothing written)\n");
        }
        LoadOrigin::File { sha256, .. } => {
            writeln!(text, "origin: saved file (sha256 {sha256})").unwrap_or_default();
        }
    }
    writeln!(text, "revision: {}", loaded.saved.revision).unwrap_or_default();
    writeln!(text, "state dir: {}", paths.state_dir.display()).unwrap_or_default();
    for note in &paths.notes {
        writeln!(text, "note: {note}").unwrap_or_default();
    }
    for notice in startup::retired_key_notices(&loaded.saved) {
        writeln!(text, "note: {notice}").unwrap_or_default();
    }
    text.push_str("validation: ok\n");
    let differences = startup::effective_differences(&loaded.saved, &loaded.effective);
    if differences.is_empty() {
        text.push_str("effective: identical to the saved configuration\n");
    } else {
        text.push_str("command-line overrides (this run only, never persisted):\n");
        for (path, saved, effective) in differences {
            writeln!(text, "  {path}: saved {saved} -> effective {effective}").unwrap_or_default();
        }
    }
    let sampling = &loaded.effective.sampling;
    writeln!(text, "sampling: fast {} ms, capacity {} ms, slow health {} ms, discovery {} ms, read deadline {} ms",
        sampling.interval_ms,
        sampling.capacity_interval_ms,
        sampling.slow_health_interval_ms,
        sampling.discovery_interval_ms,
        sampling.read_deadline_ms).unwrap_or_default();
    text.push_str(
        "effective periods are max(configured, adapter minimum) and may be raised by overload control at runtime\n",
    );
    Ok(text)
}

/// `--import-legacy <path> --dry-run|--apply`.
pub fn import_legacy(cli: &Cli) -> ExitCode {
    let Some(legacy_path) = cli.import_legacy.as_deref() else {
        return fail(EXIT_INVALID, "--import-legacy requires a path");
    };
    let paths = match startup::resolve_paths(cli) {
        Ok(paths) => paths,
        Err(error) => return fail(EXIT_INVALID, &error.to_string()),
    };
    let filesystem = startup::real_filesystem();
    if cli.apply
        && let Err(error) = paths.ensure_directories(filesystem.as_ref())
    {
        return fail(EXIT_FAILURE, &error.to_string());
    }
    let repository =
        dgxtop_store::config::FileConfigRepository::new(paths.config_file.clone(), filesystem);
    match repository.import_legacy(legacy_path, cli.apply) {
        Ok(report) => {
            let mut text = render_import_report(&report);
            if cli.apply {
                writeln!(text, "written: {}", paths.config_file.display()).unwrap_or_default();
            } else {
                text.push_str(
                    "dry run: nothing was written; re-run with --apply to write the v2 file\n",
                );
            }
            writeln!(
                text,
                "legacy file left unchanged: {}",
                legacy_path.display()
            )
            .unwrap_or_default();
            print_stdout(&text)
        }
        Err(error) => fail(EXIT_FAILURE, &error.to_string()),
    }
}

fn render_import_report(report: &LegacyImportReport) -> String {
    let mut text = String::from("legacy -> v2 mapping:\n");
    for mapping in &report.mappings {
        let status = match mapping.status {
            LegacyMappingStatus::Mapped => "mapped",
            LegacyMappingStatus::MappedWithWarning => "mapped (warning)",
            LegacyMappingStatus::Rejected => "rejected",
            LegacyMappingStatus::Unsupported => "unsupported",
            LegacyMappingStatus::Absent => "absent",
        };
        let target = match (&mapping.target_field, &mapping.target_value) {
            (Some(field), Some(value)) => format!("{field} = {value}"),
            (Some(field), None) => format!("{field} (unchanged)"),
            _ => "-".to_owned(),
        };
        writeln!(
            text,
            "  {:<22} {:<12} -> {:<40} [{status}] {}",
            mapping.legacy_field, mapping.legacy_value, target, mapping.note
        )
        .unwrap_or_default();
    }
    for warning in &report.warnings {
        writeln!(text, "warning: {warning}").unwrap_or_default();
    }
    text
}

pub(crate) fn print_stdout(text: &str) -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return ExitCode::from(EXIT_FAILURE);
    }
    ExitCode::SUCCESS
}

/// Prints a sanitized error to stderr and returns the exit status.
pub fn fail(code: u8, message: &str) -> ExitCode {
    writeln!(
        std::io::stderr(),
        "dgxtop: {}",
        dgxtop_core::text::sanitize_message(message)
    )
    .unwrap_or_default();
    ExitCode::from(code)
}
