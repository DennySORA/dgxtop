//! CLI suite: one-shot modes of the real binary in isolated HOME/XDG
//! directories (AT08 CLI precedence, AT18 legacy migration, AT20 version
//! metadata) and the installer (`installer`: AT18
//! rollback, AT20 integrity checks).

// Test helpers may unwrap: a broken fixture must fail loudly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod installer;

const BINARY: &str = env!("CARGO_BIN_EXE_dgxtop");

struct Home {
    root: PathBuf,
}

impl Home {
    fn new(label: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dgxtop-cli-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(BINARY)
            .args(arguments)
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .unwrap()
    }

    fn config_path(&self) -> PathBuf {
        self.root.join(".config/dgxtop/v2/config.json")
    }

    fn files(&self) -> Vec<PathBuf> {
        fn walk(directory: &Path, found: &mut Vec<PathBuf>) {
            if let Ok(entries) = std::fs::read_dir(directory) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        walk(&path, found);
                    } else {
                        found.push(path);
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.root, &mut found);
        found.sort();
        found
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_verbose_lists_build_metadata_and_sqlite_gate() {
    let home = Home::new("version");
    let output = home.run(&["--version", "--verbose"]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.starts_with(&format!("dgxtop {}\n", env!("CARGO_PKG_VERSION"))));
    for marker in [
        "commit: ",
        "toolchain: rustc 1.92.0",
        "target: ",
        "profile: ",
        "sqlite: 3.",
        "runtime gate >= 3.51.3: ok",
    ] {
        assert!(text.contains(marker), "missing {marker:?} in {text}");
    }
    let short = home.run(&["--version"]);
    assert_eq!(
        stdout(&short),
        format!("dgxtop {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn check_config_writes_nothing_and_shows_explicit_overrides_only() {
    let home = Home::new("check");
    let output = home.run(&["--check-config"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("effective: identical to the saved configuration"));
    let output = home.run(&["--check-config", "--interval", "0.5", "--no-gpu"]);
    let text = stdout(&output);
    assert!(
        text.contains("sampling.interval_ms: saved 1000 -> effective 500"),
        "{text}"
    );
    assert!(
        text.contains("sampling.gpu_enabled: saved true -> effective false"),
        "{text}"
    );
    assert!(
        home.files().is_empty(),
        "--check-config wrote {:?}",
        home.files()
    );
}

#[test]
fn invalid_command_line_values_exit_with_status_two() {
    let home = Home::new("invalid");
    for arguments in [
        vec!["--check-config", "--interval", "0.01"],
        vec!["--check-config", "--theme", "neon"],
        vec!["--check-config", "--config", "relative.json"],
    ] {
        let output = home.run(&arguments);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{arguments:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn saved_value_survives_absent_cli_flags() {
    let home = Home::new("precedence");
    let directory = home.root.join(".config/dgxtop/v2");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut document: serde_json::Value = serde_json::from_str(include_str!(
        "../../contracts/examples/config.reference.json"
    ))
    .unwrap();
    document["sampling"]["interval_ms"] = serde_json::json!(5000);
    std::fs::write(
        home.config_path(),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(home.config_path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let text = stdout(&home.run(&["--check-config"]));
    assert!(
        text.contains("fast 5000 ms"),
        "absent --interval overwrote the saved value: {text}"
    );
    // D-88, D-90: the REF document still holds retired keys: loaded,
    // each noted.
    for retired in [
        "store.raw_candidate_seconds is retired and ignored (D-88)",
        "store.max_query_scan_records is retired and ignored (D-90)",
        "runtime.max_history_result_bytes is retired and ignored (D-90)",
    ] {
        assert!(text.contains(&format!("note: {retired}")), "{text}");
    }
    let text = stdout(&home.run(&["--check-config", "-i", "2"]));
    assert!(
        text.contains("sampling.interval_ms: saved 5000 -> effective 2000"),
        "{text}"
    );
}

#[test]
fn invalid_saved_config_is_reported_with_location() {
    let home = Home::new("badconfig");
    let directory = home.root.join(".config/dgxtop/v2");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut document: serde_json::Value = serde_json::from_str(include_str!(
        "../../contracts/examples/config.reference.json"
    ))
    .unwrap();
    document["sampling"]["turbo"] = serde_json::json!(true);
    std::fs::write(
        home.config_path(),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(home.config_path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = home.run(&["--check-config"]);
    assert_eq!(output.status.code(), Some(2));
    let message = stderr(&output);
    assert!(
        message.contains("line") && message.contains("column"),
        "{message}"
    );
}

#[test]
fn future_schema_is_refused_and_never_rewritten() {
    let home = Home::new("future");
    let directory = home.root.join(".config/dgxtop/v2");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut document: serde_json::Value = serde_json::from_str(include_str!(
        "../../contracts/examples/config.reference.json"
    ))
    .unwrap();
    document["schema_version"] = serde_json::json!(3);
    let bytes = serde_json::to_vec_pretty(&document).unwrap();
    std::fs::write(home.config_path(), &bytes).unwrap();
    std::fs::set_permissions(home.config_path(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = home.run(&["--check-config"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("schema_version 3"),
        "{}",
        stderr(&output)
    );
    assert_eq!(std::fs::read(home.config_path()).unwrap(), bytes);
}

#[test]
fn legacy_import_dry_run_writes_nothing_and_apply_writes_private_v2_once() {
    let home = Home::new("legacy");
    let legacy = home.root.join("legacy.json");
    let legacy_bytes = br#"{"update_interval_secs":0.5,"color_theme":"green","gpu_enabled":false,"redline_threshold":85.0,"history_length":300,"network_max_visible":4,"mystery":1}"#;
    std::fs::write(&legacy, legacy_bytes).unwrap();
    std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o600)).unwrap();
    let legacy_arg = legacy.display().to_string();

    let output = home.run(&["--import-legacy", &legacy_arg, "--dry-run"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout(&output);
    for line in [
        "sampling.interval_ms = 500",
        "viewer.theme = green",
        "sampling.gpu_enabled = false",
        "store.raw_history_seconds = 150",
        "viewer.network_max_visible = 4",
        "viewer.redline_threshold_percent = 85",
        "unsupported",
        "dry run: nothing was written",
    ] {
        assert!(report.contains(line), "missing {line:?} in {report}");
    }
    assert!(!home.config_path().exists(), "dry run wrote a config");

    let output = home.run(&["--import-legacy", &legacy_arg, "--apply"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let mode = std::fs::metadata(home.config_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let directory_mode = std::fs::metadata(home.config_path().parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(directory_mode, 0o700);
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.config_path()).unwrap()).unwrap();
    assert_eq!(written["sampling"]["interval_ms"], 500);
    assert_eq!(written["viewer"]["theme"], "green");

    let again = home.run(&["--import-legacy", &legacy_arg, "--apply"]);
    assert!(
        !again.status.success(),
        "a second apply must refuse to overwrite v2"
    );
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        legacy_bytes,
        "legacy file modified"
    );
}

#[test]
fn import_legacy_requires_an_explicit_mode() {
    let home = Home::new("mode");
    let output = home.run(&["--import-legacy", "/nonexistent"]);
    assert!(!output.status.success());
    assert!(home.files().is_empty());
}
