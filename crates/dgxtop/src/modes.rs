//! One-shot command modes: `--version`, `--check-config`, `--import-legacy`.
//! None of them samples hardware; only `--import-legacy --apply` writes.

use std::io::Write;
use std::process::ExitCode;

use dgxtop_core::config::{LegacyImportReport, LegacyMappingStatus};
use dgxtop_core::ports::LoadOrigin;

use crate::cli::Cli;
use crate::startup::{self, StartupError};
use crate::version::{self, BuildInfo, RuntimeFacts};

/// Exit status for invalid input or configuration.
pub const EXIT_INVALID: u8 = 2;
/// Exit status for runtime failures.
pub const EXIT_FAILURE: u8 = 1;

/// SQLite runtime facts for the verbose version report.
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
    text.push_str(&format!("config file: {}\n", paths.config_file.display()));
    match &loaded.load_origin {
        LoadOrigin::Defaults => {
            text.push_str("origin: built-in defaults (no file yet; nothing written)\n")
        }
        LoadOrigin::File { sha256, .. } => {
            text.push_str(&format!("origin: saved file (sha256 {sha256})\n"))
        }
    }
    text.push_str(&format!("revision: {}\n", loaded.saved.revision));
    text.push_str(&format!("state dir: {}\n", paths.state_dir.display()));
    for note in &paths.notes {
        text.push_str(&format!("note: {note}\n"));
    }
    for notice in startup::retired_key_notices(&loaded.saved) {
        text.push_str(&format!("note: {notice}\n"));
    }
    text.push_str("validation: ok\n");
    let differences = startup::effective_differences(&loaded.saved, &loaded.effective);
    if differences.is_empty() {
        text.push_str("effective: identical to the saved configuration\n");
    } else {
        text.push_str("command-line overrides (this run only, never persisted):\n");
        for (path, saved, effective) in differences {
            text.push_str(&format!(
                "  {path}: saved {saved} -> effective {effective}\n"
            ));
        }
    }
    let sampling = &loaded.effective.sampling;
    text.push_str(&format!(
        "sampling: fast {} ms, capacity {} ms, slow health {} ms, discovery {} ms, read deadline {} ms\n",
        sampling.interval_ms,
        sampling.capacity_interval_ms,
        sampling.slow_health_interval_ms,
        sampling.discovery_interval_ms,
        sampling.read_deadline_ms
    ));
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
                text.push_str(&format!("written: {}\n", paths.config_file.display()));
            } else {
                text.push_str(
                    "dry run: nothing was written; re-run with --apply to write the v2 file\n",
                );
            }
            text.push_str(&format!(
                "legacy file left unchanged: {}\n",
                legacy_path.display()
            ));
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
        text.push_str(&format!(
            "  {:<22} {:<12} -> {:<40} [{status}] {}\n",
            mapping.legacy_field, mapping.legacy_value, target, mapping.note
        ));
    }
    for warning in &report.warnings {
        text.push_str(&format!("warning: {warning}\n"));
    }
    text
}

fn print_stdout(text: &str) -> ExitCode {
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
    let _ = writeln!(
        std::io::stderr(),
        "dgxtop: {}",
        dgxtop_core::text::sanitize_message(message)
    );
    ExitCode::from(code)
}
