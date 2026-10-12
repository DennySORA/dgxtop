//! Structural contracts that types alone cannot show, checked from the
//! repository files so a wiring regression fails `cargo test`, not only the
//! CI boundary script (AT01, AT05,
//! AT11, AT25, AT34, AT20, AT37).

use std::path::{Path, PathBuf};

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(repository().join(relative))
        .unwrap_or_else(|error| panic!("{relative}: {error}"))
}

/// Package names of a crate's production `[dependencies]` (dev and build
/// dependencies may assemble test doubles and are not production edges).
fn production_dependencies(crate_dir: &str) -> Vec<String> {
    let manifest = read(&format!("crates/{crate_dir}/Cargo.toml"));
    let mut in_dependencies = false;
    let mut names = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name = line
            .split(|c: char| c == '=' || c == '.' || c.is_whitespace())
            .next()
            .unwrap_or_default();
        names.push(name.to_owned());
    }
    names
}

/// Code lines (line comments removed) of every `.rs` file under `dir`.
fn code_lines(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut lines = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                for line in text.lines() {
                    let code = line.split("//").next().unwrap_or_default();
                    lines.push((path.clone(), code.to_owned()));
                }
            }
        }
    }
    lines
}

const IMPLEMENTATION_CRATES: [&str; 9] = [
    "runtime",
    "adapters",
    "collectors",
    "state",
    "history",
    "compression",
    "store",
    "manager",
    "ui",
];

/// Workspace crates a crate may use beside core (D-79): the history uses the
/// pure compression.
fn allowed_edges(crate_dir: &str) -> &'static [&'static str] {
    match crate_dir {
        "history" => &["dgxtop-compression"],
        _ => &[],
    }
}

#[test]
fn implementation_crates_depend_on_no_workspace_crate_but_core() {
    for crate_dir in IMPLEMENTATION_CRATES {
        let workspace_edges: Vec<String> = production_dependencies(crate_dir)
            .into_iter()
            .filter(|name| name.starts_with("dgxtop") && name != "dgxtop-core")
            .filter(|name| !allowed_edges(crate_dir).contains(&name.as_str()))
            .collect();
        assert!(
            workspace_edges.is_empty(),
            "crates/{crate_dir} depends on {workspace_edges:?}; only the composition root may"
        );
    }
}

#[test]
fn core_and_the_viewer_carry_no_system_or_driver_dependencies() {
    let forbidden_everywhere_but_infrastructure =
        ["libc", "nix", "nvml-wrapper", "rusqlite", "tokio"];
    let core = production_dependencies("core");
    for name in forbidden_everywhere_but_infrastructure
        .iter()
        .chain(["ratatui", "crossterm"].iter())
    {
        assert!(
            !core.contains(&(*name).to_owned()),
            "core depends on {name}"
        );
    }
    for crate_dir in [
        "ui",
        "collectors",
        "manager",
        "state",
        "history",
        "compression",
    ] {
        let dependencies = production_dependencies(crate_dir);
        for name in forbidden_everywhere_but_infrastructure {
            assert!(
                !dependencies.contains(&name.to_owned()),
                "crates/{crate_dir} depends on {name}"
            );
        }
    }
}

#[test]
fn the_store_keeps_only_sqlite_and_file_modules() {
    // D-79: the live state and the in-memory history are their own crates;
    // the store only executes SQLite and file I/O.
    let library = read("crates/store/src/lib.rs");
    let modules: Vec<&str> = library
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub mod "))
        .map(|rest| rest.trim_end_matches(';'))
        .collect();
    assert_eq!(
        modules,
        [
            "archive",
            "audit",
            "config",
            "fs",
            "history_tiers",
            // D-85: files of the state directory no build needs any more.
            "leftovers",
            "lock",
            "paths",
            "process_history",
            "sqlite"
        ]
    );
    assert!(
        !library
            .lines()
            .any(|line| line.trim_start().starts_with("mod ")),
        "crates/store/src/lib.rs declares a private module"
    );
}

#[test]
fn collectors_manager_and_viewer_never_touch_files_processes_or_sockets() {
    // Collectors only compute, the Manager reaches I/O through ports, and the
    // Controller/Viewer only translate input and draw (AT01, AT05, AT11);
    // the state, the history and the compression keep data in memory and
    // reach files through the store's ports (D-79).
    let forbidden = ["std::fs", "std::process", "std::net", "Command::new"];
    for crate_dir in [
        "collectors",
        "manager",
        "ui",
        "state",
        "history",
        "compression",
    ] {
        let source = repository().join("crates").join(crate_dir).join("src");
        for (path, line) in code_lines(&source) {
            for pattern in forbidden {
                assert!(
                    !line.contains(pattern),
                    "{} uses {pattern}: {line}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn only_the_supervisor_launches_processes_and_nothing_runs_nvidia_smi() {
    // AT34: no resident nvidia-smi; hosts are the same binary, no shell.
    // Runtime code only: build scripts run at compile time.
    let crates = repository().join("crates");
    for crate_entry in std::fs::read_dir(&crates).unwrap().flatten() {
        let source = crate_entry.path().join("src");
        if !source.is_dir() {
            continue;
        }
        for (path, line) in code_lines(&source) {
            let relative = path
                .strip_prefix(&crates)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert!(!line.contains("nvidia-smi"), "{relative}: {line}");
            if line.contains("Command::new") {
                assert_eq!(
                    relative, "runtime/src/supervisor/launch.rs",
                    "only the host launcher may spawn processes: {line}"
                );
            }
        }
    }
}

#[test]
fn the_release_gate_matches_version_and_target_lines_exactly_and_checks_sqlite() {
    let release = read(".github/workflows/release.yml");
    let checks: Vec<&str> = release
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("grep ") && line.ends_with("version.txt"))
        .collect();
    let version = "grep -qxF \"dgxtop ${GITHUB_REF_NAME#v}\" version.txt";
    let target = "grep -qxF \"target: ${{ matrix.target }}\" version.txt";
    assert!(checks.contains(&version), "{checks:?}");
    assert!(checks.contains(&target), "{checks:?}");
    assert!(
        checks
            .iter()
            .any(|line| line.contains("runtime gate >= 3\\.51\\.3: ok")),
        "the artifact's SQLite runtime gate is not checked: {checks:?}"
    );
    assert_eq!(checks.len(), 3, "{checks:?}");
}

#[test]
fn workflows_pin_actions_by_commit_and_build_from_the_lock_file() {
    for workflow in [
        ".github/workflows/ci.yml",
        ".github/workflows/release.yml",
        ".github/workflows/install.yml",
    ] {
        for line in read(workflow).lines().map(str::trim) {
            if let Some(action) = line
                .strip_prefix("- uses: ")
                .or_else(|| line.strip_prefix("uses: "))
            {
                let reference = action.split_whitespace().next().unwrap_or_default();
                // A workflow of this repository runs from the caller's commit.
                if let Some(local) = reference.strip_prefix("./") {
                    assert!(
                        repository().join(local).is_file(),
                        "{workflow}: no such workflow: {reference}"
                    );
                    continue;
                }
                let sha = reference.rsplit('@').next().unwrap_or_default();
                assert!(
                    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "{workflow}: action not pinned to a commit: {reference}"
                );
            }
            let cargo_build_step = [
                "cargo build",
                "cargo test",
                "cargo clippy",
                "cargo check",
                "cargo metadata",
            ]
            .iter()
            .any(|command| line.contains(command));
            if cargo_build_step {
                assert!(
                    line.contains("--locked"),
                    "{workflow}: not --locked: {line}"
                );
            }
        }
    }
}

/// The release page shows `docs/releases/<tag>.md` (D-102): the version the
/// workspace is at must have its notes, so the PR that bumps it brings them.
#[test]
fn the_workspace_version_has_its_release_notes() {
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    let notes = read(&format!("docs/releases/{tag}.md"));
    assert!(
        notes.contains(&tag),
        "docs/releases/{tag}.md must name {tag}"
    );
    let workflow = read(".github/workflows/release.yml");
    assert!(workflow.contains("body_path: docs/releases/${{ github.ref_name }}.md"));
}

/// Whether git tracks the lock file can only be checked once it is committed;
/// this guards what the tree itself controls.
#[test]
fn the_lock_file_exists_is_not_ignored_and_the_toolchain_is_pinned_like_ci() {
    assert!(repository().join("Cargo.lock").is_file());
    let ignored = read(".gitignore");
    assert!(
        !ignored.lines().any(|line| line.trim() == "Cargo.lock"),
        "Cargo.lock must not be ignored"
    );
    let toolchain = read("rust-toolchain.toml");
    let pinned = toolchain
        .lines()
        .find_map(|line| line.trim().strip_prefix("channel = "))
        .map(|value| value.trim_matches('"').to_owned())
        .expect("rust-toolchain.toml pins a channel");
    for workflow in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        assert!(
            read(workflow).contains(&format!("RUST_TOOLCHAIN: \"{pinned}\"")),
            "{workflow} must build with the pinned toolchain {pinned}"
        );
    }
}
