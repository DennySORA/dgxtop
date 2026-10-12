//! Test environment of a bench run (CPU model, kernel,
//! Rust/SQLite/driver versions, filesystem, background load, release
//! flags and commit are all recorded).
//!
//! Parsers are pure functions over the text of `/proc` files so they can be
//! tested without the host; [`probe`] reads the real files. A value that
//! cannot be read is reported as such, never guessed.

use std::path::Path;

use serde::Serialize;
use thiserror::Error;

use super::Environment;
use crate::version::BuildInfo;

/// git's null object id: the commit is unknown (never a fabricated SHA).
pub const NULL_COMMIT: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Error)]
pub enum EnvironmentError {
    #[error("architecture {0} is not a release target (x86_64/aarch64 GNU Linux only)")]
    UnsupportedArchitecture(String),
}

/// Build provenance as recorded in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildProvenance {
    /// 40-hex commit of the build, when `build.rs` could read one.
    pub commit: Option<String>,
    /// The tracked files differed from that commit at build time.
    pub dirty: bool,
    pub raw_commit: String,
    pub profile: String,
    pub rustc: String,
    pub target: String,
    pub version: String,
}

impl BuildProvenance {
    pub fn from_build(build: &BuildInfo) -> Self {
        let (sha, dirty) = match build.commit.strip_suffix("-dirty") {
            Some(sha) => (sha, true),
            None => (build.commit, false),
        };
        let valid = sha.len() == 40
            && sha
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        Self {
            commit: valid.then(|| sha.to_owned()),
            dirty,
            raw_commit: build.commit.to_owned(),
            profile: build.profile.to_owned(),
            rustc: build.rustc.to_owned(),
            target: build.target.to_owned(),
            version: build.version.to_owned(),
        }
    }

    /// Commit for the report's `source_commit` (null id when unknown).
    pub fn report_commit(&self) -> String {
        self.commit
            .clone()
            .unwrap_or_else(|| NULL_COMMIT.to_owned())
    }

    /// The measured code is exactly one known commit.
    pub fn identifies_source(&self) -> bool {
        self.commit.is_some() && !self.dirty
    }
}

/// Facts recorded beside the schema's environment block.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnvironmentDetails {
    pub build: BuildProvenance,
    pub cpu_frequency_governor: Option<String>,
    pub load_average_at_start: Option<String>,
    pub clock_ticks_per_second: Option<u64>,
    pub available_parallelism: Option<usize>,
}

/// `model name` (x86) or the ARM implementer/part pair from `/proc/cpuinfo`.
pub fn cpu_model_from_cpuinfo(cpuinfo: &str) -> Option<String> {
    let field = |name: &str| {
        cpuinfo.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name)
                .then(|| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
    };
    field("model name").or_else(|| {
        let implementer = field("CPU implementer")?;
        let part = field("CPU part")?;
        Some(format!("ARM implementer {implementer} part {part}"))
    })
}

/// Number of `processor` entries in `/proc/cpuinfo`.
pub fn logical_cpus_from_cpuinfo(cpuinfo: &str) -> usize {
    cpuinfo
        .lines()
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(key, _)| key.trim() == "processor")
        })
        .count()
}

/// `MemTotal` of `/proc/meminfo` in MiB (2^20 bytes).
pub fn mem_total_mib_from_meminfo(meminfo: &str) -> Option<u64> {
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib / 1024)
}

/// Filesystem type and source of the mount holding `path` (longest mount
/// point prefix in `/proc/self/mountinfo`).
pub fn filesystem_from_mountinfo(mountinfo: &str, path: &Path) -> Option<String> {
    mountinfo
        .lines()
        .filter_map(parse_mount_line)
        .filter(|(mount_point, _)| path.starts_with(mount_point))
        .max_by_key(|(mount_point, _)| mount_point.len())
        .map(|(_, description)| description)
}

fn parse_mount_line(line: &str) -> Option<(String, String)> {
    let (left, right) = line.split_once(" - ")?;
    let mount_point = unescape_mount(left.split_whitespace().nth(4)?);
    let mut fields = right.split_whitespace();
    let fs_type = fields.next()?;
    let source = fields.next().unwrap_or("none");
    Some((mount_point, format!("{fs_type} ({source})")))
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo`.
fn unescape_mount(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Kernel module version from `/proc/driver/nvidia/version`. The wording
/// around the number differs between the proprietary and the open module
/// ("... Kernel Module  580.82.07" vs "... Kernel Module for x86_64
/// 595.104.02"), so the first dotted number after "Kernel Module" is taken.
pub fn nvidia_driver_from_version(text: &str) -> Option<String> {
    let line = text
        .lines()
        .find(|line| line.starts_with("NVRM version:"))?;
    let after = line.split("Kernel Module").nth(1)?;
    let version = after
        .split_whitespace()
        .find(|token| is_dotted_number(token))?;
    Some(format!("NVIDIA kernel module {version}"))
}

fn is_dotted_number(token: &str) -> bool {
    token.contains('.')
        && token
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Truncates to at most `max_chars` characters (schema string limits).
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn read_text(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn first_line(path: &str) -> Option<String> {
    read_text(path).and_then(|text| text.lines().next().map(|line| line.trim().to_owned()))
}

fn sqlite_runtime() -> String {
    match dgxtop_store::sqlite::ensure_sqlite_version() {
        Ok(version) => format!(
            "{} (bundled, built {})",
            version.runtime_text, version.built_number
        ),
        Err(error) => format!("SQLite gate failed: {error}"),
    }
}

/// Probes the host. `terminal` and `background_load` describe how the bench
/// drove the workload; the probe appends the measured load average.
pub fn probe(
    target_dir: &Path,
    build: &BuildInfo,
    terminal: &str,
    background_load: &str,
) -> Result<(Environment, EnvironmentDetails), EnvironmentError> {
    let architecture = std::env::consts::ARCH;
    if !matches!(architecture, "x86_64" | "aarch64") {
        return Err(EnvironmentError::UnsupportedArchitecture(
            architecture.to_owned(),
        ));
    }
    let cpuinfo = read_text("/proc/cpuinfo").unwrap_or_default();
    let load = first_line("/proc/loadavg");
    let parallelism = std::thread::available_parallelism().ok().map(usize::from);
    let logical = match logical_cpus_from_cpuinfo(&cpuinfo) {
        0 => parallelism.unwrap_or(1),
        counted => counted,
    };
    let canonical_target = target_dir
        .canonicalize()
        .unwrap_or_else(|_| target_dir.to_path_buf());
    let environment = Environment {
        cpu: truncate_chars(
            &cpu_model_from_cpuinfo(&cpuinfo)
                .unwrap_or_else(|| "unknown CPU (no model in /proc/cpuinfo)".to_owned()),
            256,
        ),
        architecture: architecture.to_owned(),
        os_kernel: truncate_chars(
            &format!(
                "Linux {}",
                first_line("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".to_owned())
            ),
            256,
        ),
        logical_cpus: u32::try_from(logical).unwrap_or(u32::MAX).clamp(1, 1024),
        ram_mib: read_text("/proc/meminfo")
            .and_then(|text| mem_total_mib_from_meminfo(&text))
            .unwrap_or(1)
            .max(1),
        rustc: truncate_chars(build.rustc, 128),
        sqlite: truncate_chars(&sqlite_runtime(), 128),
        gpu_driver: truncate_chars(
            &read_text("/proc/driver/nvidia/version")
                .and_then(|text| nvidia_driver_from_version(&text))
                .unwrap_or_else(|| "none (no NVIDIA kernel module loaded)".to_owned()),
            128,
        ),
        filesystem: truncate_chars(
            &read_text("/proc/self/mountinfo")
                .and_then(|text| filesystem_from_mountinfo(&text, &canonical_target))
                .unwrap_or_else(|| "unknown".to_owned()),
            128,
        ),
        terminal: truncate_chars(terminal, 128),
        background_load: truncate_chars(
            &format!(
                "{background_load}; loadavg at start: {}",
                load.clone().unwrap_or_else(|| "unreadable".to_owned())
            ),
            256,
        ),
    };
    let details = EnvironmentDetails {
        build: BuildProvenance::from_build(build),
        cpu_frequency_governor: first_line("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        load_average_at_start: load,
        clock_ticks_per_second: dgxtop_adapters::procfs::HostConstants::from_sysconf().clk_tck,
        available_parallelism: parallelism,
    };
    Ok((environment, details))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(commit: &'static str) -> BuildInfo {
        BuildInfo {
            commit,
            ..BuildInfo::current()
        }
    }

    #[test]
    fn commits_are_reported_only_when_known() {
        let sha = "b4b5a8d0123456789abcdef0123456789abcdef0";
        let clean = BuildProvenance::from_build(&build("b4b5a8d0123456789abcdef0123456789abcdef0"));
        assert_eq!(clean.report_commit(), sha);
        assert!(clean.identifies_source());
        let dirty =
            BuildProvenance::from_build(&build("b4b5a8d0123456789abcdef0123456789abcdef0-dirty"));
        assert_eq!(dirty.report_commit(), sha);
        assert!(dirty.dirty && !dirty.identifies_source());
        let unknown = BuildProvenance::from_build(&build("unknown"));
        assert_eq!(unknown.report_commit(), NULL_COMMIT);
        assert!(!unknown.identifies_source());
        let short = BuildProvenance::from_build(&build("b4b5a8d"));
        assert!(
            short.commit.is_none(),
            "abbreviated ids are not report commits"
        );
    }

    #[test]
    fn cpuinfo_and_meminfo_are_parsed() {
        let x86 = "processor\t: 0\nmodel name\t: 13th Gen Intel(R) Core(TM) i9-13900K\n\nprocessor\t: 1\nmodel name\t: 13th Gen Intel(R) Core(TM) i9-13900K\n";
        assert_eq!(
            cpu_model_from_cpuinfo(x86).as_deref(),
            Some("13th Gen Intel(R) Core(TM) i9-13900K")
        );
        assert_eq!(logical_cpus_from_cpuinfo(x86), 2);
        let arm =
            "processor\t: 0\nBogoMIPS\t: 2000.00\nCPU implementer\t: 0x41\nCPU part\t: 0xd85\n";
        assert_eq!(
            cpu_model_from_cpuinfo(arm).as_deref(),
            Some("ARM implementer 0x41 part 0xd85")
        );
        assert_eq!(cpu_model_from_cpuinfo("flags : fpu"), None);
        assert_eq!(
            mem_total_mib_from_meminfo("MemTotal:       65653516 kB\n"),
            Some(64114)
        );
        assert_eq!(mem_total_mib_from_meminfo("MemFree: 1 kB"), None);
    }

    #[test]
    fn probing_this_host_fills_every_schema_field() {
        let target = std::env::temp_dir();
        let (environment, details) =
            probe(&target, &BuildInfo::current(), "headless test", "unit test").unwrap();
        assert!(!environment.cpu.is_empty());
        assert!(environment.logical_cpus >= 1);
        assert!(environment.ram_mib >= 1);
        assert!(environment.os_kernel.starts_with("Linux "));
        assert!(environment.background_load.contains("loadavg at start"));
        assert_eq!(details.build.version, env!("CARGO_PKG_VERSION"));
    }
}
