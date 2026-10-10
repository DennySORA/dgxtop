//! Deterministic REF replay fixtures for the benchmark harness (AT35).
//!
//! The fixture itself comes from the adapters' seeded generator
//! ([`generate_ref_fixture`]); this module turns it into a file in a
//! run-time directory, fingerprints it, and takes a census proving the
//! workload contains every metric kind (counter, gauge, static), the
//! intermittent missing/failed/faulted samples and only simulated
//! (replay-namespace) sources. Large generated files are never committed:
//! they are re-created from the seed whenever a bench runs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use dgxtop_adapters::replay::{RefProfile, ReplayError, ReplayFixture, generate_ref_fixture};
use dgxtop_core::quality::{Quality, RawKind, ReadStatus};
use serde::Serialize;
use thiserror::Error;

/// Reason code the generator puts on a faulted sensor sample.
const SENSOR_FAULT_REASON: &str = "SENSOR_FAULT";
/// Reason code the generator puts on a real (non-faulted) alarm reading.
const ALARM_REASON: &str = "ALARM";

#[derive(Debug, Error)]
pub enum FixtureError {
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error("cannot write fixture {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// What a workload fixture contains (evidence that nothing was left out).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixtureCensus {
    pub namespace_label: String,
    pub sources: usize,
    pub sources_by_kind: BTreeMap<String, usize>,
    pub row_sources: usize,
    pub sample_kinds: BTreeSet<String>,
    pub read_groups: BTreeSet<String>,
    pub cycles: usize,
    pub samples: usize,
    pub value_samples: usize,
    pub failed_samples: usize,
    pub missing_samples: usize,
    pub sensor_fault_samples: usize,
    pub invalid_quality_samples: usize,
    pub alarm_samples: usize,
    pub simulated_latency_us: u32,
}

impl FixtureCensus {
    pub fn of(fixture: &ReplayFixture) -> Self {
        let mut census = Self {
            namespace_label: fixture.namespace_label.clone(),
            sources: fixture.sources.len(),
            sources_by_kind: BTreeMap::new(),
            row_sources: fixture.sources.iter().filter(|source| source.rows).count(),
            sample_kinds: BTreeSet::new(),
            read_groups: BTreeSet::new(),
            cycles: fixture.cycles.len(),
            samples: 0,
            value_samples: 0,
            failed_samples: 0,
            missing_samples: 0,
            sensor_fault_samples: 0,
            invalid_quality_samples: 0,
            alarm_samples: 0,
            simulated_latency_us: fixture.simulated_latency_us,
        };
        for source in &fixture.sources {
            *census
                .sources_by_kind
                .entry(raw_kind_label(source.raw_kind).to_owned())
                .or_default() += 1;
            census
                .sample_kinds
                .insert(format!("{:?}", source.sample_kind).to_lowercase());
            census.read_groups.insert(source.read_group.clone());
        }
        census.count_samples(fixture);
        census
    }

    fn count_samples(&mut self, fixture: &ReplayFixture) {
        let single_value_sources: BTreeSet<&str> = fixture
            .sources
            .iter()
            .filter(|source| !source.rows)
            .map(|source| source.source_id.as_str())
            .collect();
        for cycle in &fixture.cycles {
            let mut present = BTreeSet::new();
            for sample in cycle {
                self.count_sample(sample);
                present.insert(sample.source_id.as_str());
            }
            self.missing_samples += single_value_sources.difference(&present).count();
        }
    }

    fn count_sample(&mut self, sample: &dgxtop_adapters::replay::FixtureSample) {
        self.samples += 1;
        if sample.value.is_some() {
            self.value_samples += 1;
        }
        if sample.status.is_some_and(|status| status != ReadStatus::Ok) {
            self.failed_samples += 1;
        }
        if sample.quality == Some(Quality::Invalid) {
            self.invalid_quality_samples += 1;
        }
        let has_reason = |code: &str| sample.reasons.iter().any(|reason| reason == code);
        if has_reason(SENSOR_FAULT_REASON) {
            self.sensor_fault_samples += 1;
        }
        if has_reason(ALARM_REASON) {
            self.alarm_samples += 1;
        }
    }

    /// Every metric kind and every intermittent sample class is present, and
    /// no source can enter the live namespace.
    pub fn covers_reference_classes(&self) -> bool {
        let has_kind = |kind: &str| {
            self.sources_by_kind
                .get(kind)
                .is_some_and(|count| *count > 0)
        };
        has_kind("counter")
            && has_kind("gauge")
            && has_kind("static")
            && self.row_sources > 0
            && self.failed_samples > 0
            && self.missing_samples > 0
            && self.sensor_fault_samples > 0
            && self.alarm_samples > 0
            && self
                .sample_kinds
                .iter()
                .all(|kind| kind == "simulated" || kind == "replay")
    }
}

fn raw_kind_label(kind: RawKind) -> &'static str {
    match kind {
        RawKind::Gauge => "gauge",
        RawKind::Counter => "counter",
        RawKind::Static => "static",
    }
}

/// A fixture written for one bench run.
#[derive(Debug, Clone)]
pub struct GeneratedFixture {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub seed: u64,
    pub fixture: ReplayFixture,
    pub census: FixtureCensus,
}

/// Canonical bytes of the seeded fixture (identical for identical inputs).
pub fn fixture_bytes(seed: u64, profile: &RefProfile) -> Result<Vec<u8>, ReplayError> {
    generate_ref_fixture(seed, profile).to_json_bytes()
}

/// Generates the fixture and writes it below `directory` (created if absent).
pub fn write_fixture(
    directory: &Path,
    seed: u64,
    profile: &RefProfile,
) -> Result<GeneratedFixture, FixtureError> {
    let fixture = generate_ref_fixture(seed, profile);
    let bytes = fixture.to_json_bytes()?;
    let path = directory.join(fixture_file_name(seed, profile));
    let io_error = |source| FixtureError::Io {
        path: path.display().to_string(),
        source,
    };
    std::fs::create_dir_all(directory).map_err(io_error)?;
    std::fs::write(&path, &bytes).map_err(io_error)?;
    Ok(GeneratedFixture {
        sha256: dgxtop_core::digest::sha256_hex(&bytes),
        bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        seed,
        census: FixtureCensus::of(&fixture),
        fixture,
        path,
    })
}

fn fixture_file_name(seed: u64, profile: &RefProfile) -> String {
    format!(
        "{}-seed{seed}-latency{}us.replay.json",
        profile.id.to_ascii_lowercase(),
        profile.simulated_latency_us
    )
}

/// Read groups served by each of `hosts` replay hosts, by the same rule the
/// replay adapter uses (`ReplayFixture::slice`: one live adapter family per
/// host for REF).
pub fn slice_read_groups(fixture: &ReplayFixture, hosts: u32) -> Vec<BTreeSet<String>> {
    let count = hosts.max(1);
    (0..count)
        .map(|index| {
            fixture
                .slice(index, count)
                .map(|slice| {
                    slice
                        .sources
                        .into_iter()
                        .map(|source| source.read_group)
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// Writes one fixture file per replay host next to `generated`, so every
/// host parses only its own slice (a live host never holds another
/// adapter's data either). Returns the paths in host order.
pub fn write_host_slices(
    generated: &GeneratedFixture,
    hosts: u32,
) -> Result<Vec<PathBuf>, FixtureError> {
    let count = hosts.max(1);
    let stem = generated.path.file_name().map_or_else(
        || "fixture".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let directory = generated.path.parent().unwrap_or_else(|| Path::new("."));
    (0..count)
        .map(|index| {
            let bytes = generated.fixture.slice(index, count)?.to_json_bytes()?;
            let path = directory.join(format!("{stem}.host{index}-of-{count}.json"));
            std::fs::write(&path, &bytes).map_err(|source| FixtureError::Io {
                path: path.display().to_string(),
                source,
            })?;
            Ok(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_profile(seed_cycles: u32) -> RefProfile {
        RefProfile {
            cycles: seed_cycles,
            ..RefProfile::reference()
        }
    }

    #[test]
    fn same_seed_gives_byte_identical_fixtures() {
        let profile = small_profile(4);
        let first = fixture_bytes(20_261_004, &profile).unwrap();
        let second = fixture_bytes(20_261_004, &profile).unwrap();
        assert_eq!(first, second);
        let other_seed = fixture_bytes(20_261_005, &profile).unwrap();
        assert_ne!(first, other_seed, "the seed must change the workload");
        let decoded = ReplayFixture::from_json_slice(&first).unwrap();
        assert_eq!(decoded.to_json_bytes().unwrap(), first, "stable round trip");
    }

    #[test]
    fn reference_fixture_contains_every_metric_class_in_the_replay_namespace() {
        let fixture = generate_ref_fixture(20_261_004, &RefProfile::reference());
        let census = FixtureCensus::of(&fixture);
        assert!(census.covers_reference_classes(), "{census:#?}");
        assert_eq!(
            census.sources, 2048,
            "2048 catalog sources, row sources included"
        );
        assert_eq!(census.cycles, 60);
        assert_eq!(census.simulated_latency_us, 1000);
        assert_eq!(
            census.sample_kinds,
            BTreeSet::from(["simulated".to_owned()])
        );
    }

    #[test]
    fn host_slices_are_written_per_family_and_reload() {
        let directory = std::env::temp_dir().join(format!(
            "dgxtop-fixture-slices-{}-{}",
            std::process::id(),
            line!()
        ));
        let written = write_fixture(&directory, 13, &small_profile(2)).unwrap();
        let paths = write_host_slices(&written, 7).unwrap();
        assert_eq!(paths.len(), 7);
        let mut total = 0;
        for path in &paths {
            let slice = ReplayFixture::load(path).unwrap();
            let families: BTreeSet<&str> = slice
                .sources
                .iter()
                .map(|source| dgxtop_adapters::replay::family_of(&source.read_group))
                .collect();
            assert_eq!(families.len(), 1, "{families:?}");
            total += slice.sources.len();
        }
        assert_eq!(total, written.fixture.sources.len());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn written_fixture_matches_its_fingerprint() {
        let directory = std::env::temp_dir().join(format!(
            "dgxtop-fixture-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let profile = small_profile(2);
        let written = write_fixture(&directory, 11, &profile).unwrap();
        let on_disk = std::fs::read(&written.path).unwrap();
        assert_eq!(dgxtop_core::digest::sha256_hex(&on_disk), written.sha256);
        assert_eq!(on_disk, fixture_bytes(11, &profile).unwrap());
        assert!(written.path.starts_with(&directory));
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
