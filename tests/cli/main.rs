//! CLI suite: one-shot modes of the real binary in isolated HOME/XDG
//! directories (AT08 CLI precedence, AT18 legacy migration, AT20 version
//! metadata) and the installer (`installer`: AT18
//! rollback, AT20 integrity checks).

// A test-only crate: its helpers may unwrap and panic, so a broken fixture
// fails loudly (`allow-*-in-tests` reaches the whole crate).
#![cfg(test)]

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
        std::fs::remove_dir_all(&self.root).unwrap_or_default();
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
        "toolchain: rustc 1.100.0",
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
        // The redline is retired (D-105): reported, never imported.
        "retired (D-105)",
        "unsupported",
        "dry run: nothing was written",
    ] {
        assert!(report.contains(line), "missing {line:?} in {report}");
    }
    assert!(
        !report.contains("viewer.redline_threshold_percent"),
        "{report}"
    );
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
    assert!(written["viewer"].get("redline_threshold_percent").is_none());

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
    assert_eq!(home.files(), [] as [PathBuf; 0]);
}

/// `--snapshot-json` of the replay demo (D-140): standard output carries
/// one JSON document and nothing else. It holds a rate, which only a second
/// reading gives, the GPU by its name and the GPU processes without their
/// command lines; the run keeps nothing, in the home or among the temporary
/// files.
#[test]
fn snapshot_json_prints_one_document_and_keeps_nothing() {
    let home = Home::new("snapshot");
    let temporary = home.root.join("tmp");
    std::fs::create_dir_all(&temporary).unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../adapters/fixtures/replay/demo.json");
    let output = Command::new(BINARY)
        .args(["--snapshot-json", "--interval", "0.2", "--replay"])
        .arg(&fixture)
        .env_clear()
        .env("HOME", &home.root)
        .env("TMPDIR", &temporary)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let document: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(document["format"], "dgxtop-snapshot/1");
    assert_eq!(document["dgxtop_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(document["namespace"], "replay");
    assert_eq!(document["interval_ms"], 200);
    assert_eq!(document["adapters"][0]["id"], "replay");
    assert_eq!(document["adapters"][0]["state"], "healthy");

    let metrics = document["metrics"].as_array().unwrap();
    let metric = |name: &str| {
        metrics
            .iter()
            .find(|metric| metric["metric"] == name)
            .unwrap_or_else(|| panic!("{name} missing"))
    };
    let read_rate = metric("disk.read.bytes_per_second");
    assert!(read_rate["value"].as_f64().unwrap() > 0.0, "{read_rate}");
    assert_eq!(read_rate["unit"], "bytes_per_second");
    assert_eq!(read_rate["kind"], "simulated");
    assert_eq!(read_rate["stale"], false);
    let temperature = metric("gpu.temperature");
    assert!(temperature["value"].is_number(), "{temperature}");
    assert!(
        temperature["entity"].as_str().unwrap().starts_with("gpu:"),
        "{temperature}"
    );
    let entities = document["entities"].as_array().unwrap();
    assert!(
        entities
            .iter()
            .any(|entity| entity["kind"] == "gpu" && entity["name"] == "Demo GPU 0"),
        "{entities:?}"
    );

    let processes = document["processes"].as_array().unwrap();
    let mut pids: Vec<i64> = processes
        .iter()
        .map(|process| process["pid"].as_i64().unwrap())
        .collect();
    pids.sort_unstable();
    assert_eq!(pids, vec![4242, 5150]);
    for process in processes {
        assert!(process.get("command").is_none(), "{process}");
        assert!(
            process["gpus"][0]["memory_bytes"].as_u64().unwrap() > 0,
            "{process}"
        );
    }
    assert_eq!(document["processes_truncated"], false);
    assert_eq!(document["redacted"], false);
    assert!(home.files().is_empty(), "{:?}", home.files());
    assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
}

/// `--snapshot-json --redact` of the replay demo (D-145): the document
/// says it is redacted and holds neither the GPU's UUID, nor the name of
/// an interface, a process or the host; a metric and a process still name
/// their GPU's entity, by its stand-in, and every figure is there. Without
/// `--snapshot-json` the flag is refused.
#[test]
fn snapshot_json_redact_leaves_out_what_names_the_host() {
    let home = Home::new("snapshot-redact");
    let temporary = home.root.join("tmp");
    std::fs::create_dir_all(&temporary).unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../adapters/fixtures/replay/demo.json");
    let run = |redact: bool| {
        let mut command = Command::new(BINARY);
        command.args(["--snapshot-json", "--interval", "0.2", "--replay"]);
        command.arg(&fixture);
        if redact {
            command.arg("--redact");
        }
        let output = command
            .env_clear()
            .env("HOME", &home.root)
            .env("TMPDIR", &temporary)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    };
    let plain = run(false);
    let text = run(true);
    let named = [
        "5e1d0a2c-0000-4000-8000-000000000001",
        "eno1",
        "python3",
        "torchrun",
    ];
    for name in named {
        assert!(plain.contains(name), "the demo holds {name}");
        assert!(!text.contains(name), "{name} is left");
    }
    let document: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(document["redacted"], true);
    assert_eq!(document["host"]["name"], "redacted");
    assert!(document["host"]["id"].is_null());
    let gpu = "gpu:GPU-redacted-0";
    let entities = document["entities"].as_array().unwrap();
    assert!(
        entities
            .iter()
            .any(|entity| entity["id"] == gpu && entity["name"] == "Demo GPU 0"),
        "{entities:?}"
    );
    let metrics = document["metrics"].as_array().unwrap();
    let temperature = metrics
        .iter()
        .find(|metric| metric["metric"] == "gpu.temperature")
        .unwrap();
    assert_eq!(temperature["entity"], gpu);
    assert!(temperature["value"].is_number(), "{temperature}");
    let processes = document["processes"].as_array().unwrap();
    assert_eq!(processes.len(), 2);
    for process in processes {
        assert_eq!(process["name"], "redacted");
        assert!(process["user"].is_null());
        assert_eq!(process["gpus"][0]["gpu"], gpu);
        assert!(process["gpus"][0]["memory_bytes"].as_u64().unwrap() > 0);
    }
    let plain: serde_json::Value = serde_json::from_str(&plain).unwrap();
    assert_eq!(
        plain["metrics"].as_array().unwrap().len(),
        metrics.len(),
        "every metric is kept"
    );

    let alone = home.run(&["--redact"]);
    assert_eq!(alone.status.code(), Some(2));
    assert_eq!(stdout(&alone), "");
}

/// A snapshot is a mode of its own: with another one-shot mode or a state
/// directory to keep it exits with status two and prints no document.
#[test]
fn snapshot_json_refuses_the_flags_of_a_kept_state() {
    let home = Home::new("snapshot-flags");
    for other in [
        vec!["--state-dir", "/abs/state"],
        vec!["--check-config"],
        vec!["--diagnostics-json"],
    ] {
        let mut arguments = vec!["--snapshot-json"];
        arguments.extend(&other);
        let output = home.run(&arguments);
        assert_eq!(output.status.code(), Some(2), "{other:?}");
        assert_eq!(stdout(&output), "", "{other:?}");
    }
    assert_eq!(home.files(), [] as [PathBuf; 0]);
}

/// A snapshot reads the saved configuration under the rules the monitor
/// trusts it by: a file others can write and an invalid one end the run
/// with status two before anything is read, told with the saved file's own
/// path; no document is printed and no temporary state is left behind.
#[test]
fn snapshot_json_refuses_an_untrusted_or_invalid_saved_configuration() {
    let home = Home::new("snapshot-config");
    let temporary = home.root.join("tmp");
    std::fs::create_dir_all(&temporary).unwrap();
    let config = home.config_path();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let run = || {
        Command::new(BINARY)
            .arg("--snapshot-json")
            .env_clear()
            .env("HOME", &home.root)
            .env("TMPDIR", &temporary)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .unwrap()
    };
    let refused = |output: &Output, reason: &str| {
        assert_eq!(output.status.code(), Some(2), "{}", stderr(output));
        assert_eq!(stdout(output), "");
        let message = stderr(output);
        assert!(message.contains(reason), "{message}");
        assert!(
            message.contains(config.to_str().unwrap()),
            "names the saved file: {message}"
        );
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
    };

    std::fs::write(&config, "{\n  oops\n}").unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    refused(&run(), "line 2");

    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o666)).unwrap();
    refused(&run(), "writable");
    // The saved file is only read.
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "{\n  oops\n}");
}
