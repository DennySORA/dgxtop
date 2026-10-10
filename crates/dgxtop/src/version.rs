//! `--version` / `--version --verbose` output (also used by the installer and
//! release gate, which grep for `dgxtop <version>` and `target: <triple>`).

use std::fmt::Write as _;

/// Build provenance embedded by `build.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    pub version: &'static str,
    pub commit: &'static str,
    pub rustc: &'static str,
    pub target: &'static str,
    pub profile: &'static str,
}

impl BuildInfo {
    pub const fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            commit: env!("DGXTOP_BUILD_COMMIT"),
            rustc: env!("DGXTOP_BUILD_RUSTC"),
            target: env!("DGXTOP_BUILD_TARGET"),
            profile: env!("DGXTOP_BUILD_PROFILE"),
        }
    }
}

/// Runtime facts that need the storage layer (SQLite gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFacts {
    pub sqlite_version: String,
    pub sqlite_gate_ok: bool,
}

/// One-line version (`dgxtop 0.2.0`).
pub fn short(build: &BuildInfo) -> String {
    format!("dgxtop {}", build.version)
}

/// Multi-line verbose report.
pub fn verbose(build: &BuildInfo, runtime: &RuntimeFacts) -> String {
    let mut text = short(build);
    text.push('\n');
    let gate = if runtime.sqlite_gate_ok {
        "ok"
    } else {
        "FAILED"
    };
    let _ = writeln!(text, "commit: {}", build.commit);
    let _ = writeln!(text, "toolchain: {}", build.rustc);
    let _ = writeln!(text, "target: {}", build.target);
    let _ = writeln!(text, "profile: {}", build.profile);
    let _ = writeln!(
        text,
        "sqlite: {} (runtime gate >= 3.51.3: {gate})",
        runtime.sqlite_version
    );
    let _ = writeln!(
        text,
        "config schema: {}",
        dgxtop_core::config::CONFIG_SCHEMA_VERSION
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_report_contains_installer_markers() {
        let build = BuildInfo::current();
        let report = verbose(
            &build,
            &RuntimeFacts {
                sqlite_version: "3.53.2".into(),
                sqlite_gate_ok: true,
            },
        );
        assert!(report.starts_with(&format!("dgxtop {}\n", env!("CARGO_PKG_VERSION"))));
        assert!(report.contains(&format!("target: {}", build.target)));
        assert!(report.contains("sqlite: 3.53.2 (runtime gate >= 3.51.3: ok)"));
    }
}
