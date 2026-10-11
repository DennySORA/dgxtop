//! Replay fixture format `dgxtop-replay-fixture/1` and its validation.
//!
//! ```json
//! {
//!   "format": "dgxtop-replay-fixture/1",
//!   "seed": "20261004",                       // u64 as decimal string
//!   "namespace_label": "demo",
//!   "host_id": "demo-host",                    // optional, informative
//!   "sampling_ms": 1000,                       // optional (default 1000)
//!   "simulated_latency_us": 0,                 // optional fixed API delay per read
//!   "entities": [ { "entity_id", "kind", "parent_id"?, "display_name", "attributes"? } ],
//!   "sources": [ {
//!     "source_id", "entity_id", "metric_id", "scope", "native_unit", "raw_kind",
//!     "sample_kind": "replay" | "simulated",   // live kinds are rejected
//!     "read_group": "<live adapter>.<group>",  // e.g. "hwmon.fast", "procfs.process"
//!     "origin"?: { "origin_id", "relation" }, "quality", "profile_id",
//!     "profile_revision"?, "profile_rank"?, "driver"?, "device_path"?, "channel"?,
//!     "label"?, "counter_modulus"? (decimal string), "update_interval_ms"?,
//!     "rows"?: true                             // per-record entities (process groups only)
//!   } ],
//!   "cycles": [ [ { "source_id", "entity_id"?, "channel"?,
//!                   "value": 68000.0 | "123456"   // f64 number or exact u64 string
//!                   | "status": "timeout",        // exactly one of value/status
//!                   "quality"?, "reasons"? } ] ],
//!   "faults": [ { "cycle", "read_group"?, "kind": { "type": "hang", "ms": 1500 }
//!                 | { "type": "fail", "status": "error" } | { "type": "generation_change" }
//!                 | { "type": "remove_source", "source_id": "..." } } ]
//! }
//! ```
//!
//! Everything is bounded (file size, sources, cycles, entries, faults) and
//! every source must use a replay/simulated sample kind, so a fixture can
//! never inject live data. Ordering is `Vec` only, so serialization is stable.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use dgxtop_core::identity::{
    EntityId, HostId, MetricId, OriginIdentity, Pid, ProfileId, Scope, SourceId,
};
use dgxtop_core::metric::{self, NativeUnit, native};
use dgxtop_core::naming;
use dgxtop_core::observation::{Attribute, EntityKind, MAX_ATTRIBUTES};
use dgxtop_core::process_stat::ProcessStat;
use dgxtop_core::quality::{Quality, RawKind, ReadStatus, SampleKind};
use dgxtop_core::wire::parse_decimal_u64;

use crate::common::exact_i64_to_f64;

/// Format tag of version 1.
pub const FIXTURE_FORMAT: &str = "dgxtop-replay-fixture/1";
/// Largest accepted fixture file.
pub const MAX_FIXTURE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest accepted number of sources.
pub const MAX_SOURCES: usize = 8192;
/// Largest accepted number of cycles.
pub const MAX_CYCLES: usize = 10_000;
/// Largest accepted number of entities.
pub const MAX_ENTITIES: usize = 8192;
/// Largest accepted number of faults.
pub const MAX_FAULTS: usize = 10_000;
/// Largest accepted number of samples in one cycle.
pub const MAX_SAMPLES_PER_CYCLE: usize = 65_536;
/// Largest accepted number of samples over all cycles (bounded memory).
pub const MAX_TOTAL_SAMPLES: usize = 2_000_000;
/// Longest injected hang.
pub const MAX_HANG_MS: u32 = 600_000;
/// Longest simulated per-read API latency.
pub const MAX_SIMULATED_LATENCY_US: u32 = 1_000_000;
const MAX_REASONS_PER_SAMPLE: usize = 8;
const MAX_REASON_BYTES: usize = 80;
const MAX_TEXT_BYTES: usize = 256;
const MAX_DEVICE_PATH_BYTES: usize = 1024;
const MAX_LABEL_BYTES: usize = 64;
const MAX_UPDATE_INTERVAL_MS: u32 = 3_600_000;

/// Native unit spellings a fixture may declare.
const KNOWN_NATIVE_UNITS: &[&str] = &[
    native::DEG_C,
    native::MILLI_DEG_C,
    native::WATT,
    native::MILLI_WATT,
    native::MICRO_WATT,
    native::JOULE,
    native::MILLI_JOULE,
    native::MICRO_JOULE,
    native::MHZ,
    native::KHZ,
    native::HZ,
    native::BYTES,
    native::KIB,
    native::SECTORS_512,
    native::BYTES_PER_SECOND,
    native::KIB_PER_SECOND,
    native::KB_SI_PER_SECOND,
    native::KB_UNVERIFIED_PER_SECOND,
    native::PERCENT,
    native::COUNT,
    native::TICKS,
    native::COUNT_PER_SECOND,
    native::MILLISECONDS,
];

/// A domain rule violated by a fixture (stable `code` for tests and logs).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("[{code}] {detail}")]
pub struct FixtureViolation {
    pub code: &'static str,
    pub detail: String,
}

fn violation(code: &'static str, detail: impl Into<String>) -> FixtureViolation {
    FixtureViolation {
        code,
        detail: detail.into(),
    }
}

/// Live adapter family of a replay read group (`hwmon.fast` -> `hwmon`).
pub fn family_of(read_group: &str) -> &str {
    read_group.split('.').next().unwrap_or(read_group)
}

/// Replay errors: infrastructure (`Io`) is kept apart from domain violations.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// The fixture file could not be read.
    #[error("cannot read replay fixture {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    /// The fixture violates the format or a bound.
    #[error("invalid replay fixture {0}")]
    Invalid(#[from] FixtureViolation),
    /// The adapter id is outside the replay namespace (`replay`, `replay-N`).
    #[error("adapter id {0:?} is not a replay adapter id")]
    NotReplayAdapter(String),
    /// The slice is not `(index, count)` with `index < count`.
    #[error("invalid replay slice {index}/{count}")]
    InvalidSlice { index: u32, count: u32 },
    /// One replay host may serve at most the manifest source limit.
    #[error("replay slice holds {sources} sources; one host serves at most {limit}")]
    SliceTooLarge { sources: usize, limit: usize },
    /// The fixture could not be encoded.
    #[error("replay fixture could not be encoded: {0}")]
    Encode(String),
}

impl ReplayError {
    /// Violation code of an invalid fixture.
    pub fn violation_code(&self) -> Option<&'static str> {
        match self {
            Self::Invalid(violation) => Some(violation.code),
            _ => None,
        }
    }
}

/// A sample value: finite `f64` (JSON number) or exact `u64` (decimal string).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FixtureValue {
    F64(f64),
    U64(u64),
}

impl Serialize for FixtureValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::F64(value) => serializer.serialize_f64(*value),
            Self::U64(value) => serializer.serialize_str(&value.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for FixtureValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(FixtureValueVisitor)
    }
}

struct FixtureValueVisitor;

impl Visitor<'_> for FixtureValueVisitor {
    type Value = FixtureValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a finite number or a decimal u64 string")
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<FixtureValue, E> {
        Ok(FixtureValue::F64(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<FixtureValue, E> {
        exact_i64_to_f64(value)
            .map(FixtureValue::F64)
            .ok_or_else(|| E::custom("integers beyond 2^53 must be decimal strings"))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<FixtureValue, E> {
        i64::try_from(value)
            .ok()
            .and_then(exact_i64_to_f64)
            .map(FixtureValue::F64)
            .ok_or_else(|| E::custom("integers beyond 2^53 must be decimal strings"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<FixtureValue, E> {
        parse_decimal_u64(value)
            .map(FixtureValue::U64)
            .map_err(E::custom)
    }
}

/// One entity of the fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureEntity {
    pub entity_id: EntityId,
    pub kind: EntityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<EntityId>,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<Attribute>,
}

fn default_revision() -> u32 {
    1
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn default_sampling_ms() -> u32 {
    1000
}

/// One source of the fixture (SourceDescriptor-like).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureSource {
    pub source_id: SourceId,
    pub entity_id: EntityId,
    pub metric_id: MetricId,
    pub scope: Scope,
    pub native_unit: NativeUnit,
    pub raw_kind: RawKind,
    pub sample_kind: SampleKind,
    /// `<live adapter>.<group>` (see `naming::replay_group`).
    pub read_group: String,
    #[serde(default = "OriginIdentity::unknown")]
    pub origin: OriginIdentity,
    pub quality: Quality,
    pub profile_id: ProfileId,
    #[serde(default = "default_revision")]
    pub profile_revision: u32,
    #[serde(default)]
    pub profile_rank: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "dgxtop_core::wire::opt_u64_decimal"
    )]
    pub counter_modulus: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_interval_ms: Option<u32>,
    /// Records carry their own entity (process rows); process groups only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub rows: bool,
}

/// One sample of one cycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureSample {
    pub source_id: SourceId,
    /// Record entity of a row source (process / GPU-process).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<EntityId>,
    /// Record channel of a procfs row (process basename).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<FixtureValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ReadStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<Quality>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

impl FixtureSample {
    /// Sample with a value.
    pub fn value(source_id: SourceId, value: FixtureValue) -> Self {
        Self {
            source_id,
            entity_id: None,
            channel: None,
            value: Some(value),
            status: None,
            quality: None,
            reasons: Vec::new(),
        }
    }

    /// Failed sample.
    pub fn status(source_id: SourceId, status: ReadStatus) -> Self {
        Self {
            source_id,
            entity_id: None,
            channel: None,
            value: None,
            status: Some(status),
            quality: None,
            reasons: Vec::new(),
        }
    }
}

/// Injected fault kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FaultKind {
    /// Sleep inside `read` so the adapter host misses its deadline.
    Hang { ms: u32 },
    /// Every record of the read fails with `status`.
    Fail { status: ReadStatus },
    /// Every affected source gets a new generation (device re-created).
    GenerationChange,
    /// The source disappears (records `removed`, then a catalog removal).
    RemoveSource { source_id: SourceId },
}

/// One fault, fired whenever fixture cycle `cycle` is replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureFault {
    pub cycle: u32,
    /// Restricts the fault to one read group; `None` = every group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_group: Option<String>,
    pub kind: FaultKind,
}

/// A complete replay fixture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayFixture {
    pub format: String,
    #[serde(with = "dgxtop_core::wire::u64_decimal")]
    pub seed: u64,
    pub namespace_label: String,
    /// Host the fixture describes; the composition root decides whether to use it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<HostId>,
    #[serde(default = "default_sampling_ms")]
    pub sampling_ms: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub simulated_latency_us: u32,
    #[serde(default)]
    pub entities: Vec<FixtureEntity>,
    pub sources: Vec<FixtureSource>,
    pub cycles: Vec<Vec<FixtureSample>>,
    #[serde(default)]
    pub faults: Vec<FixtureFault>,
}

impl ReplayFixture {
    /// Reads (at most [`MAX_FIXTURE_BYTES`]), decodes and validates a fixture file.
    pub fn load(path: &Path) -> Result<Self, ReplayError> {
        let io_error = |source| ReplayError::Io {
            path: path.display().to_string(),
            source,
        };
        let file = std::fs::File::open(path).map_err(io_error)?;
        let length = file.metadata().map_err(io_error)?.len();
        if length > MAX_FIXTURE_BYTES {
            return Err(too_large(length).into());
        }
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        file.take(MAX_FIXTURE_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        Self::from_json_slice(&bytes)
    }

    /// Decodes and validates fixture JSON.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, ReplayError> {
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if length > MAX_FIXTURE_BYTES {
            return Err(too_large(length).into());
        }
        let fixture: Self = serde_json::from_slice(bytes)
            .map_err(|error| violation("DECODE", error.to_string()))?;
        fixture.validate()?;
        Ok(fixture)
    }

    /// The part one of `count` replay hosts serves (REF: one host per live
    /// adapter, PERFORMANCE.md §1): the live adapter families (read-group
    /// prefixes, ordered) whose ordinal is `index` modulo `count`, with their
    /// sources, their samples of every cycle, the faults of their read
    /// groups (and the faults of every group), and the entities their
    /// sources reference plus those entities' ancestors. Entities no source
    /// references go to slice 0.
    pub fn slice(&self, index: u32, count: u32) -> Result<Self, ReplayError> {
        let invalid = ReplayError::InvalidSlice { index, count };
        let (Ok(position), Ok(hosts)) = (usize::try_from(index), usize::try_from(count)) else {
            return Err(invalid);
        };
        if position >= hosts {
            return Err(invalid);
        }
        let families: BTreeSet<&str> = self
            .sources
            .iter()
            .map(|source| family_of(&source.read_group))
            .collect();
        let chosen: BTreeSet<&str> = families
            .into_iter()
            .enumerate()
            .filter(|(ordinal, _)| ordinal % hosts == position)
            .map(|(_, family)| family)
            .collect();
        let sources: Vec<FixtureSource> = self
            .sources
            .iter()
            .filter(|source| chosen.contains(family_of(&source.read_group)))
            .cloned()
            .collect();
        let served: BTreeSet<&SourceId> = sources.iter().map(|source| &source.source_id).collect();
        let cycles = self
            .cycles
            .iter()
            .map(|cycle| {
                cycle
                    .iter()
                    .filter(|sample| served.contains(&sample.source_id))
                    .cloned()
                    .collect()
            })
            .collect();
        let faults = self
            .faults
            .iter()
            .filter(|fault| {
                fault
                    .read_group
                    .as_deref()
                    .is_none_or(|group| chosen.contains(family_of(group)))
            })
            .cloned()
            .collect();
        let entities = self.entities_for(&sources, position == 0);
        Ok(Self {
            format: self.format.clone(),
            seed: self.seed,
            namespace_label: self.namespace_label.clone(),
            host_id: self.host_id.clone(),
            sampling_ms: self.sampling_ms,
            simulated_latency_us: self.simulated_latency_us,
            entities,
            sources,
            cycles,
            faults,
        })
    }

    /// Entities `sources` reference with their ancestors (in fixture order);
    /// `with_unreferenced` adds the entities no source of the fixture uses.
    fn entities_for(
        &self,
        sources: &[FixtureSource],
        with_unreferenced: bool,
    ) -> Vec<FixtureEntity> {
        let parents: BTreeMap<&EntityId, &EntityId> = self
            .entities
            .iter()
            .filter_map(|entity| {
                entity
                    .parent_id
                    .as_ref()
                    .map(|parent| (&entity.entity_id, parent))
            })
            .collect();
        let mut wanted: BTreeSet<&EntityId> = BTreeSet::new();
        for source in sources {
            let mut next = Some(&source.entity_id);
            while let Some(entity) = next {
                if !wanted.insert(entity) {
                    break;
                }
                next = parents.get(entity).copied();
            }
        }
        if with_unreferenced {
            let referenced: BTreeSet<&EntityId> = self
                .sources
                .iter()
                .map(|source| &source.entity_id)
                .collect();
            wanted.extend(
                self.entities
                    .iter()
                    .map(|entity| &entity.entity_id)
                    .filter(|entity| !referenced.contains(entity)),
            );
        }
        self.entities
            .iter()
            .filter(|entity| wanted.contains(&entity.entity_id))
            .cloned()
            .collect()
    }

    /// Stable compact JSON (identical bytes for identical fixtures).
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, ReplayError> {
        serde_json::to_vec(self).map_err(|error| ReplayError::Encode(error.to_string()))
    }

    /// Checks every format rule and bound.
    pub fn validate(&self) -> Result<(), FixtureViolation> {
        self.check_header()?;
        let sources = check_sources(&self.sources)?;
        check_entities(&self.entities)?;
        check_cycles(&self.cycles, &sources)?;
        check_faults(&self.faults, self.cycles.len(), &sources)
    }

    fn check_header(&self) -> Result<(), FixtureViolation> {
        if self.format != FIXTURE_FORMAT {
            return Err(violation(
                "FORMAT",
                format!("format must be {FIXTURE_FORMAT:?}"),
            ));
        }
        check_text("namespace_label", &self.namespace_label, MAX_LABEL_BYTES)?;
        if !(100..=60_000).contains(&self.sampling_ms) {
            return Err(violation(
                "SAMPLING_RANGE",
                "sampling_ms must be 100..=60000",
            ));
        }
        if self.simulated_latency_us > MAX_SIMULATED_LATENCY_US {
            return Err(violation(
                "LATENCY_RANGE",
                "simulated_latency_us exceeds 1 s",
            ));
        }
        check_count(
            "TOO_MANY_SOURCES",
            "sources",
            self.sources.len(),
            1,
            MAX_SOURCES,
        )?;
        check_count(
            "TOO_MANY_CYCLES",
            "cycles",
            self.cycles.len(),
            1,
            MAX_CYCLES,
        )?;
        check_count(
            "TOO_MANY_ENTITIES",
            "entities",
            self.entities.len(),
            0,
            MAX_ENTITIES,
        )?;
        check_count(
            "TOO_MANY_FAULTS",
            "faults",
            self.faults.len(),
            0,
            MAX_FAULTS,
        )
    }
}

fn too_large(bytes: u64) -> FixtureViolation {
    violation(
        "FILE_TOO_LARGE",
        format!("{bytes} bytes exceed the {MAX_FIXTURE_BYTES} byte limit"),
    )
}

fn check_count(
    code: &'static str,
    field: &str,
    count: usize,
    min: usize,
    max: usize,
) -> Result<(), FixtureViolation> {
    if (min..=max).contains(&count) {
        Ok(())
    } else {
        Err(violation(
            code,
            format!("{field}: {count} outside {min}..={max}"),
        ))
    }
}

fn check_text(field: &str, value: &str, max: usize) -> Result<(), FixtureViolation> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(violation(
            "STRING_FIELD",
            format!("{field} must be 1..={max} bytes without control characters"),
        ));
    }
    Ok(())
}

fn check_optional_text(
    field: &str,
    value: Option<&str>,
    max: usize,
) -> Result<(), FixtureViolation> {
    value.map_or(Ok(()), |text| check_text(field, text, max))
}

/// Live adapter and group named by a replay read group (`hwmon.fast`).
pub(crate) fn live_group_of(read_group: &str) -> Option<(&'static str, &'static str)> {
    let (adapter, group) = read_group.split_once('.')?;
    naming::READ_GROUPS
        .iter()
        .find(|spec| spec.adapter == adapter && spec.group == group)
        .map(|spec| (spec.adapter, spec.group))
}

/// Kind of per-record entities of a row source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    /// `procfs.process`: `process:<ns>:<pid>:<start>` entities.
    Process,
    /// `nvml.process`: `gpupid:<uuid>:<pid>` entities.
    GpuProcess,
}

/// Row kind implied by a replay read group, if it is a process group.
pub(crate) fn row_kind_of(read_group: &str) -> Option<RowKind> {
    match live_group_of(read_group)? {
        (naming::adapters::PROCFS, naming::groups::PROCESS) => Some(RowKind::Process),
        (naming::adapters::NVML, naming::groups::PROCESS) => Some(RowKind::GpuProcess),
        _ => None,
    }
}

/// PID of `process:<namespace>:<pid>:<start_ticks>` (namespace may contain `:`).
pub(crate) fn process_pid(entity_id: &str) -> Option<i32> {
    let rest = entity_id.strip_prefix("process:")?;
    let (head, start) = rest.rsplit_once(':')?;
    let (namespace, pid) = head.rsplit_once(':')?;
    if namespace.is_empty() {
        return None;
    }
    parse_decimal_u64(start).ok()?;
    Some(Pid::new(pid.parse::<i64>().ok()?).ok()?.get())
}

fn entity_matches_rows(kind: RowKind, entity_id: &str) -> bool {
    match kind {
        RowKind::Process => process_pid(entity_id).is_some(),
        RowKind::GpuProcess => {
            naming::entity::parse_gpu_pid(entity_id).is_some_and(|(_, pid)| pid > 0)
        }
    }
}

fn check_sources(
    sources: &[FixtureSource],
) -> Result<BTreeMap<&SourceId, &FixtureSource>, FixtureViolation> {
    let mut by_id = BTreeMap::new();
    for source in sources {
        check_source(source)?;
        if by_id.insert(&source.source_id, source).is_some() {
            return Err(violation(
                "DUPLICATE_SOURCE",
                format!("source {} appears twice", source.source_id),
            ));
        }
    }
    Ok(by_id)
}

fn check_source(source: &FixtureSource) -> Result<(), FixtureViolation> {
    let id = &source.source_id;
    if source.sample_kind.namespace() != dgxtop_core::quality::Namespace::Replay {
        return Err(violation(
            "LIVE_SAMPLE_KIND",
            format!("{id}: sample_kind must be replay or simulated"),
        ));
    }
    let descriptor = metric::descriptor(source.metric_id.as_str()).ok_or_else(|| {
        violation(
            "UNKNOWN_METRIC",
            format!("{id}: unknown metric {}", source.metric_id),
        )
    })?;
    if descriptor.kind != source.raw_kind {
        return Err(violation(
            "RAW_KIND_MISMATCH",
            format!("{id}: raw_kind differs from the metric catalog"),
        ));
    }
    if !KNOWN_NATIVE_UNITS.contains(&source.native_unit.as_str()) {
        return Err(violation(
            "UNKNOWN_UNIT",
            format!("{id}: unknown native unit {}", source.native_unit),
        ));
    }
    if live_group_of(&source.read_group).is_none() {
        return Err(violation(
            "READ_GROUP",
            format!("{id}: read_group must be <live adapter>.<group>"),
        ));
    }
    source
        .origin
        .validate()
        .map_err(|error| violation("ORIGIN", format!("{id}: {error}")))?;
    if source.profile_revision == 0 || i32::try_from(source.profile_revision).is_err() {
        return Err(violation(
            "PROFILE_REVISION",
            format!("{id}: profile_revision 1..=i32::MAX"),
        ));
    }
    check_optional_text("driver", source.driver.as_deref(), MAX_TEXT_BYTES)?;
    check_optional_text(
        "device_path",
        source.device_path.as_deref(),
        MAX_DEVICE_PATH_BYTES,
    )?;
    check_optional_text("channel", source.channel.as_deref(), MAX_TEXT_BYTES)?;
    check_optional_text("label", source.label.as_deref(), MAX_TEXT_BYTES)?;
    if source.counter_modulus == Some(0) {
        return Err(violation(
            "MODULUS_ZERO",
            format!("{id}: counter_modulus must be > 0"),
        ));
    }
    if source
        .update_interval_ms
        .is_some_and(|interval| !(1..=MAX_UPDATE_INTERVAL_MS).contains(&interval))
    {
        return Err(violation(
            "UPDATE_INTERVAL",
            format!("{id}: update_interval_ms 1..=3600000"),
        ));
    }
    check_source_entity(source)
}

fn check_source_entity(source: &FixtureSource) -> Result<(), FixtureViolation> {
    let id = &source.source_id;
    match (row_kind_of(&source.read_group), source.rows) {
        (None, true) => Err(violation(
            "ROWS_NOT_ALLOWED",
            format!("{id}: rows are only allowed in process read groups"),
        )),
        (Some(kind), false) if !entity_matches_rows(kind, source.entity_id.as_str()) => {
            Err(violation(
                "PROCESS_ENTITY",
                format!("{id}: process group sources need a process entity or rows"),
            ))
        }
        _ => Ok(()),
    }
}

fn check_entities(entities: &[FixtureEntity]) -> Result<(), FixtureViolation> {
    let mut seen = BTreeSet::new();
    for entity in entities {
        if !seen.insert(&entity.entity_id) {
            return Err(violation(
                "DUPLICATE_ENTITY",
                format!("entity {} appears twice", entity.entity_id),
            ));
        }
        check_text("display_name", &entity.display_name, MAX_TEXT_BYTES)?;
        let oversized = entity.attributes.len() > MAX_ATTRIBUTES
            || entity
                .attributes
                .iter()
                .any(|attribute| attribute.key.len() > 64 || attribute.value.len() > 256);
        if oversized {
            return Err(violation(
                "ENTITY_ATTRIBUTES",
                format!(
                    "{}: at most 16 attributes (key<=64, value<=256)",
                    entity.entity_id
                ),
            ));
        }
    }
    Ok(())
}

fn check_cycles(
    cycles: &[Vec<FixtureSample>],
    sources: &BTreeMap<&SourceId, &FixtureSource>,
) -> Result<(), FixtureViolation> {
    let mut total = 0usize;
    for (index, cycle) in cycles.iter().enumerate() {
        if cycle.len() > MAX_SAMPLES_PER_CYCLE {
            return Err(violation(
                "TOO_MANY_SAMPLES",
                format!("cycle {index} has {} samples", cycle.len()),
            ));
        }
        total = total.saturating_add(cycle.len());
        if total > MAX_TOTAL_SAMPLES {
            return Err(violation(
                "TOO_MANY_SAMPLES",
                "more than 2000000 samples in total",
            ));
        }
        let mut seen = BTreeSet::new();
        for sample in cycle {
            let source = sources.get(&sample.source_id).ok_or_else(|| {
                violation(
                    "SAMPLE_UNKNOWN_SOURCE",
                    format!("cycle {index}: unknown source {}", sample.source_id),
                )
            })?;
            check_sample(index, sample, source)?;
            if !seen.insert((&sample.source_id, sample.entity_id.as_ref())) {
                return Err(violation(
                    "SAMPLE_DUPLICATE",
                    format!("cycle {index}: {} sampled twice", sample.source_id),
                ));
            }
        }
    }
    Ok(())
}

fn check_sample(
    cycle: usize,
    sample: &FixtureSample,
    source: &FixtureSource,
) -> Result<(), FixtureViolation> {
    let id = &sample.source_id;
    match (&sample.value, sample.status) {
        (Some(value), None) => check_value(cycle, id, *value, source)?,
        (None, Some(status)) if status != ReadStatus::Ok => {}
        (None, Some(_)) => {
            return Err(violation(
                "SAMPLE_STATUS_OK",
                format!("cycle {cycle} {id}: status ok needs a value instead"),
            ));
        }
        _ => {
            return Err(violation(
                "SAMPLE_VALUE_STATUS",
                format!("cycle {cycle} {id}: exactly one of value/status"),
            ));
        }
    }
    check_reasons(cycle, sample)?;
    check_row_identity(cycle, sample, source)
}

fn check_value(
    cycle: usize,
    id: &SourceId,
    value: FixtureValue,
    source: &FixtureSource,
) -> Result<(), FixtureViolation> {
    match value {
        FixtureValue::F64(number) if !number.is_finite() => Err(violation(
            "SAMPLE_VALUE_KIND",
            format!("cycle {cycle} {id}: value must be finite"),
        )),
        FixtureValue::F64(_) if source.raw_kind == RawKind::Counter => Err(violation(
            "SAMPLE_VALUE_KIND",
            format!("cycle {cycle} {id}: counters must be exact u64 strings"),
        )),
        _ => Ok(()),
    }
}

fn check_reasons(cycle: usize, sample: &FixtureSample) -> Result<(), FixtureViolation> {
    let valid = sample.reasons.len() <= MAX_REASONS_PER_SAMPLE
        && sample.reasons.iter().all(|reason| {
            !reason.is_empty()
                && reason.len() <= MAX_REASON_BYTES
                && (is_reason_code(reason) || ProcessStat::is_tag(reason))
        });
    if valid {
        Ok(())
    } else {
        Err(violation(
            "SAMPLE_REASON",
            format!(
                "cycle {cycle} {}: at most 8 reasons of [A-Z0-9_]{{1,80}} or process stat tags",
                sample.source_id
            ),
        ))
    }
}

fn is_reason_code(reason: &str) -> bool {
    reason
        .bytes()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn check_row_identity(
    cycle: usize,
    sample: &FixtureSample,
    source: &FixtureSource,
) -> Result<(), FixtureViolation> {
    let id = &sample.source_id;
    let row_kind = row_kind_of(&source.read_group).filter(|_| source.rows);
    match (row_kind, &sample.entity_id) {
        (None, Some(_)) => {
            return Err(violation(
                "SAMPLE_ENTITY_NOT_ALLOWED",
                format!("cycle {cycle} {id}: entity_id only for row sources"),
            ));
        }
        (Some(_), None) => {
            return Err(violation(
                "SAMPLE_ENTITY_REQUIRED",
                format!("cycle {cycle} {id}: row samples need entity_id"),
            ));
        }
        (Some(kind), Some(entity_id)) if !entity_matches_rows(kind, entity_id.as_str()) => {
            return Err(violation(
                "PROCESS_ENTITY",
                format!("cycle {cycle} {id}: invalid row entity {entity_id}"),
            ));
        }
        _ => {}
    }
    match (&sample.channel, row_kind) {
        (Some(_), None) => Err(violation(
            "SAMPLE_CHANNEL",
            format!("cycle {cycle} {id}: channel only for row sources"),
        )),
        (Some(channel), Some(_)) => check_text("channel", channel, MAX_LABEL_BYTES),
        _ => Ok(()),
    }
}

fn check_faults(
    faults: &[FixtureFault],
    cycles: usize,
    sources: &BTreeMap<&SourceId, &FixtureSource>,
) -> Result<(), FixtureViolation> {
    for fault in faults {
        let cycle = usize::try_from(fault.cycle).unwrap_or(usize::MAX);
        if cycle >= cycles {
            return Err(violation(
                "FAULT_CYCLE",
                format!("fault cycle {} beyond {cycles} cycles", fault.cycle),
            ));
        }
        if let Some(group) = &fault.read_group
            && live_group_of(group).is_none()
        {
            return Err(violation(
                "FAULT_READ_GROUP",
                format!("unknown read group {group}"),
            ));
        }
        match &fault.kind {
            FaultKind::Hang { ms } if !(1..=MAX_HANG_MS).contains(ms) => {
                return Err(violation("FAULT_HANG_RANGE", "hang ms must be 1..=600000"));
            }
            FaultKind::Fail {
                status: ReadStatus::Ok,
            } => {
                return Err(violation("FAULT_STATUS_OK", "fail status must not be ok"));
            }
            FaultKind::RemoveSource { source_id } if !sources.contains_key(source_id) => {
                return Err(violation(
                    "FAULT_UNKNOWN_SOURCE",
                    format!("remove_source {source_id} is unknown"),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}
