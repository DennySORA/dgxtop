//! `bench/scenarios.json`: the seeded synthetic workload definitions (REF,
//! SOAK, OVERLOAD) and their mapping onto the replay fixture generator.
//!
//! The file is the only source of the profile numbers and the seed; the
//! harness never hard-codes them, so a changed scenario cannot silently
//! diverge from what was measured.

use std::path::Path;

use dgxtop_adapters::replay::RefProfile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::Workload;

/// Profile id of the reference workload.
pub const REF_PROFILE: &str = "REF";

/// Why the scenario file could not be used.
#[derive(Debug, Error)]
pub enum ScenarioError {
    #[error("cannot read scenarios {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("scenarios {path} are not valid JSON for schema 1: {source}")]
    Decode {
        path: String,
        source: serde_json::Error,
    },
    #[error("scenarios define no profile {0:?}")]
    UnknownProfile(String),
}

/// One workload profile exactly as written in `bench/scenarios.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioProfile {
    pub id: String,
    pub series: u32,
    pub sources: u32,
    pub processes: u32,
    pub process_gpu_links: u32,
    pub adapter_hosts: u32,
    pub sampling_ms: u32,
    pub terminal_columns: u16,
    pub terminal_rows: u16,
    pub sessions: u32,
    pub archive_enabled: bool,
    pub duration_seconds: u64,
}

/// The whole scenario file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenarios {
    pub schema_version: u32,
    pub kind: String,
    pub seed: u64,
    pub profiles: Vec<ScenarioProfile>,
    pub faults: Vec<String>,
}

impl Scenarios {
    pub fn load(path: &Path) -> Result<Self, ScenarioError> {
        let text = std::fs::read_to_string(path).map_err(|source| ScenarioError::Io {
            path: path.display().to_string(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| ScenarioError::Decode {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn profile(&self, id: &str) -> Result<&ScenarioProfile, ScenarioError> {
        self.profiles
            .iter()
            .find(|profile| profile.id == id)
            .ok_or_else(|| ScenarioError::UnknownProfile(id.to_owned()))
    }
}

impl ScenarioProfile {
    /// The workload block of a performance report for this profile.
    pub fn workload(&self) -> Workload {
        Workload {
            series: self.series,
            sources: self.sources,
            processes: self.processes,
            process_gpu_links: self.process_gpu_links,
            adapter_hosts: self.adapter_hosts,
            sampling_ms: self.sampling_ms,
            terminal_columns: u32::from(self.terminal_columns),
            terminal_rows: u32::from(self.terminal_rows),
            sessions: self.sessions,
            archive_enabled: self.archive_enabled,
        }
    }

    /// Whether this profile is the fixed REF workload the report schema demands.
    #[cfg(test)]
    pub fn is_reference_workload(&self) -> bool {
        self.workload() == Workload::reference()
    }

    /// Generator profile with this scenario's counts. GPU count and the
    /// number of distinct cycles are generator choices the scenario file
    /// does not fix, so they keep the generator's REF defaults.
    pub fn replay_profile(&self, simulated_latency_us: u32) -> RefProfile {
        RefProfile {
            id: self.id.clone(),
            series: self.series,
            sources: self.sources,
            processes: self.processes,
            process_gpu_links: self.process_gpu_links,
            adapter_hosts: self.adapter_hosts,
            sampling_ms: self.sampling_ms,
            simulated_latency_us,
            ..RefProfile::reference()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec_scenarios() -> Scenarios {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../spec/dgxtop-v2/bench/scenarios.json");
        Scenarios::load(&path).unwrap()
    }

    #[test]
    fn reference_profile_is_the_schema_workload() {
        let scenarios = spec_scenarios();
        assert_eq!(scenarios.seed, 20_261_004);
        let reference = scenarios.profile(REF_PROFILE).unwrap();
        assert!(reference.is_reference_workload());
        assert_eq!(reference.duration_seconds, 1800);
        assert_eq!(scenarios.profile("SOAK").unwrap().duration_seconds, 259_200);
    }

    #[test]
    fn unknown_profiles_and_bad_files_are_errors() {
        let scenarios = spec_scenarios();
        assert!(matches!(
            scenarios.profile("NOPE"),
            Err(ScenarioError::UnknownProfile(_))
        ));
        let missing = Scenarios::load(Path::new("/nonexistent/scenarios.json"));
        assert!(matches!(missing, Err(ScenarioError::Io { .. })));
        let directory =
            std::env::temp_dir().join(format!("dgxtop-scenarios-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let bad = directory.join("bad.json");
        std::fs::write(&bad, br#"{"schema_version":1}"#).unwrap();
        assert!(matches!(
            Scenarios::load(&bad),
            Err(ScenarioError::Decode { .. })
        ));
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
