//! Deterministic synthetic workload fixtures mirroring `bench/scenarios.json`
//! (REF / OVERLOAD). Every value comes from integer arithmetic over a
//! stateless SplitMix64 hash of `(seed, purpose, indices)`, and the fixture
//! only uses `Vec`/`BTreeMap` ordering, so the same seed always serializes to
//! identical bytes on every platform (AT35). Everything is labelled
//! `simulated` (`SYNTHETIC_FIXTURE`); it is never hardware evidence.
//!
//! Interpretation of the profile numbers:
//! * `series` = distinct non-process MetricKeys with candidates; `sources` =
//!   their candidate sources (1..=`max_candidates_per_series` per key:
//!   primary, confirmed alias, independent confirmed origin, unknown origin).
//! * Temperature keys exercise arbitration: aliases share an origin,
//!   independent origins occasionally disagree beyond tolerance (conflict),
//!   and samples intermittently carry `SENSOR_FAULT` (quality invalid) or
//!   `ALARM` (a real high reading, quality kept).
//! * Every candidate intermittently misses samples (absent or `timeout`).
//! * Processes use one row source (`procfs.process`: the CPU ticks) and,
//!   like the live adapter, one sample per process carrying the rest of
//!   its `stat` as [`ProcessStat`] tags (D-79, D-86); GPU-process links use
//!   `nvml.process` rows (memory/utilization).
//! * `adapter_hosts` is the number of replay slices the composition root
//!   should create (`ReplayAdapter` `slice = (index, adapter_hosts)`).

use std::collections::BTreeMap;

use dgxtop_core::clock::splitmix64;
use dgxtop_core::identity::{
    EntityId, HostId, MetricId, OriginId, OriginIdentity, ProfileId, Scope, SourceId,
};
use dgxtop_core::metric::{NativeUnit, ids, native};
use dgxtop_core::observation::{Attribute, EntityKind};
use dgxtop_core::process_stat::ProcessStat;
use dgxtop_core::quality::{Quality, RawKind, ReadStatus, SampleKind};

use super::fixture::{
    FIXTURE_FORMAT, FixtureEntity, FixtureSample, FixtureSource, FixtureValue,
    MAX_SAMPLES_PER_CYCLE, MAX_SIMULATED_LATENCY_US, MAX_SOURCES, MAX_TOTAL_SAMPLES, ReplayFixture,
};
use crate::common::exact_i64_to_f64;

/// Synthetic workload profile (numbers of `bench/scenarios.json`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefProfile {
    pub id: String,
    pub series: u32,
    pub sources: u32,
    pub max_candidates_per_series: u32,
    pub processes: u32,
    pub process_gpu_links: u32,
    /// GPUs of the synthetic host (not in scenarios.json; DGX-like default 8).
    pub gpus: u32,
    pub adapter_hosts: u32,
    pub sampling_ms: u32,
    /// Distinct fixture cycles before the replay loops.
    pub cycles: u32,
    /// Fixed per-read API delay of a replay host (PERFORMANCE.md: 1 ms).
    pub simulated_latency_us: u32,
}

impl RefProfile {
    /// REF: 512 series, 2048 sources, 256 processes, 1024 GPU links, 7 hosts, 1 s.
    pub fn reference() -> Self {
        Self {
            id: "REF".to_owned(),
            series: 512,
            sources: 2048,
            max_candidates_per_series: 4,
            processes: 256,
            process_gpu_links: 1024,
            gpus: 8,
            adapter_hosts: 7,
            sampling_ms: 1000,
            cycles: 60,
            simulated_latency_us: 1000,
        }
    }

    /// OVERLOAD: 4096 series, 8192 sources, 4096 processes, 16384 links, 8 hosts, 100 ms.
    pub fn overload() -> Self {
        Self {
            id: "OVERLOAD".to_owned(),
            series: 4096,
            sources: 8192,
            max_candidates_per_series: 4,
            processes: 4096,
            process_gpu_links: 16384,
            gpus: 8,
            adapter_hosts: 8,
            sampling_ms: 100,
            cycles: 6,
            simulated_latency_us: 1000,
        }
    }
}

/// Seeded deterministic fixture for `profile` (inputs are clamped to the
/// fixture bounds, so the result always validates).
pub fn generate_ref_fixture(seed: u64, profile: &RefProfile) -> ReplayFixture {
    let plan = Plan::from_profile(profile);
    let topology = Topology::sized_for(plan.series, plan.gpus);
    let uuids: Vec<String> = (0..topology.gpus).map(|gpu| gpu_uuid(seed, gpu)).collect();
    let keys = select_keys(&topology, plan.series, &uuids);
    Generator {
        seed,
        plan,
        uuids,
        keys,
    }
    .fixture(profile)
}

const PROFILE_ID: &str = "ref-synthetic";
const PID_NAMESPACE: &str = "pid:[4026531836]";
const NET_NAMESPACE: &str = "net:[4026531840]";
const MOUNT_NAMESPACE: &str = "mnt:[4026531841]";
const RAPL_MODULUS: u64 = 262_143_328_850;
const BASENAMES: [&str; 8] = [
    "python3",
    "torchrun",
    "trainer",
    "jupyter",
    "inference",
    "dataload",
    "tritonserver",
    "bench",
];
const UIDS: [u32; 4] = [1000, 1001, 1002, 0];
const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

// Hash purposes (stateless randomness per purpose and indices).
const TAG_BASE: u64 = 1;
const TAG_NOISE: u64 = 2;
const TAG_STEP: u64 = 3;
const TAG_MISSING: u64 = 4;
const TAG_FAULT: u64 = 5;
const TAG_ALARM: u64 = 6;
const TAG_CONFLICT: u64 = 7;
const TAG_PROCESS: u64 = 8;
const TAG_LINK: u64 = 9;
const TAG_GPU: u64 = 10;
const TAG_CANDIDATE: u64 = 11;

fn mix(seed: u64, parts: &[u64]) -> u64 {
    parts
        .iter()
        .fold(splitmix64(seed), |state, part| splitmix64(state ^ *part))
}

fn uniform(seed: u64, parts: &[u64], low: u64, high: u64) -> u64 {
    let width = high.saturating_sub(low).saturating_add(1);
    low.saturating_add(mix(seed, parts) % width)
}

fn signed_noise(seed: u64, parts: &[u64], spread: u64) -> i64 {
    if spread == 0 {
        return 0;
    }
    let offset = mix(seed, parts) % spread.saturating_mul(2).saturating_add(1);
    to_i64(offset) - to_i64(spread)
}

fn per_mille(seed: u64, parts: &[u64]) -> u64 {
    mix(seed, parts) % 1000
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn index(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Catalog sources of the process rows: the CPU ticks only (D-86).
const PROCESS_ROW_SOURCES: usize = 1;

/// Clamped profile.
#[derive(Debug, Clone)]
struct Plan {
    series: usize,
    sources: usize,
    max_candidates: usize,
    processes: usize,
    links: usize,
    gpus: usize,
    cycles: usize,
    sampling_ms: u32,
    latency_us: u32,
}

fn bounded(value: u32, low: usize, high: usize) -> usize {
    usize::try_from(value)
        .unwrap_or(high)
        .clamp(low, high.max(low))
}

impl Plan {
    fn from_profile(profile: &RefProfile) -> Self {
        let gpus = bounded(profile.gpus, 1, 64);
        let max_candidates = bounded(profile.max_candidates_per_series, 1, 4);
        let row_sources = PROCESS_ROW_SOURCES + 2 * gpus;
        let source_room = MAX_SOURCES - row_sources;
        let series = bounded(profile.series, 1, source_room);
        // `profile.sources` counts every catalog source, row sources included
        // (PERFORMANCE.md §1 against the catalog cap of RUNTIME.md §5).
        let key_sources = usize::try_from(profile.sources)
            .unwrap_or(usize::MAX)
            .saturating_sub(row_sources);
        let sources = key_sources.clamp(series, (series * max_candidates).min(source_room));
        let mut processes = bounded(profile.processes, 0, 16_384);
        let mut links = bounded(profile.process_gpu_links, 0, processes * gpus);
        let room = MAX_SAMPLES_PER_CYCLE - sources;
        while processes + links * 2 > room {
            if links > 0 {
                links = links.saturating_sub(links / 8 + 1);
            } else {
                processes = processes.saturating_sub(processes / 8 + 1);
            }
        }
        let per_cycle = (sources + processes + links * 2).max(1);
        let cycles = bounded(profile.cycles, 1, super::fixture::MAX_CYCLES)
            .min(MAX_TOTAL_SAMPLES / per_cycle)
            .max(1);
        Self {
            series,
            sources,
            max_candidates,
            processes,
            links,
            gpus,
            cycles,
            sampling_ms: profile.sampling_ms.clamp(100, 60_000),
            latency_us: profile.simulated_latency_us.min(MAX_SIMULATED_LATENCY_US),
        }
    }

    /// Candidates of key `key`: `sources` spread as evenly as possible.
    fn candidates(&self, key: usize, keys: usize) -> usize {
        let keys = keys.max(1);
        let base = self.sources / keys;
        let extra = self.sources % keys;
        (base + usize::from(key < extra)).clamp(1, self.max_candidates)
    }
}

/// Size of the synthetic host.
#[derive(Debug, Clone, Copy)]
struct Topology {
    packages: usize,
    cores: usize,
    disks: usize,
    interfaces: usize,
    filesystems: usize,
    gpus: usize,
}

impl Topology {
    fn scaled(multiplier: usize, gpus: usize) -> Self {
        Self {
            packages: 2,
            cores: 32 * multiplier,
            disks: 4 * multiplier,
            interfaces: 4 * multiplier,
            filesystems: 4 * multiplier,
            gpus,
        }
    }

    fn sized_for(series: usize, gpus: usize) -> Self {
        let mut multiplier = 1;
        loop {
            let topology = Self::scaled(multiplier, gpus);
            if multiplier >= 256 || key_capacity(&topology) >= series {
                return topology;
            }
            multiplier *= 2;
        }
    }

    fn package_of_core(&self, core: usize) -> usize {
        let per_package = (self.cores / self.packages.max(1)).max(1);
        (core / per_package).min(self.packages.saturating_sub(1))
    }
}

/// Value model of one key.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ValueModel {
    /// `(base + noise) / divisor` as f64; base in `[low, high]`.
    Gauge {
        low: u64,
        high: u64,
        spread: u64,
        divisor: u32,
    },
    /// Exact integer gauge (bytes, KiB).
    ExactGauge { low: u64, high: u64, spread: u64 },
    /// Monotonic counter: `start + sum(step)`.
    Counter {
        start_low: u64,
        start_high: u64,
        step_low: u64,
        step_high: u64,
    },
    /// Constant metadata value.
    Static { low: u64, high: u64 },
}

/// One synthetic MetricKey.
#[derive(Debug, Clone)]
struct KeySpec {
    entity: String,
    metric: &'static str,
    scope: String,
    primary_group: &'static str,
    alias_group: &'static str,
    unit: &'static str,
    kind: RawKind,
    model: ValueModel,
    /// Physical origin shared by primary and alias (`None` -> synthetic).
    origin: Option<String>,
    modulus: Option<u64>,
    /// Units per degree for temperature keys (arbitration scenarios).
    degree: Option<i64>,
}

impl KeySpec {
    fn new(
        entity: String,
        metric: &'static str,
        scope: String,
        group: &'static str,
        unit: &'static str,
        kind: RawKind,
        model: ValueModel,
    ) -> Self {
        Self {
            entity,
            metric,
            scope,
            primary_group: group,
            alias_group: group,
            unit,
            kind,
            model,
            origin: None,
            modulus: None,
            degree: None,
        }
    }

    fn temperature(
        mut self,
        alias_group: &'static str,
        origin: Option<String>,
        degree: i64,
    ) -> Self {
        self.alias_group = alias_group;
        self.origin = origin;
        self.degree = Some(degree);
        self
    }

    fn modulus(mut self, modulus: u64) -> Self {
        self.modulus = Some(modulus);
        self
    }

    fn origin(mut self, origin: String) -> Self {
        self.origin = Some(origin);
        self
    }
}

const fn gauge(low: u64, high: u64, spread: u64, divisor: u32) -> ValueModel {
    ValueModel::Gauge {
        low,
        high,
        spread,
        divisor,
    }
}

const fn exact(low: u64, high: u64, spread: u64) -> ValueModel {
    ValueModel::ExactGauge { low, high, spread }
}

const fn counter(start_high: u64, step_low: u64, step_high: u64) -> ValueModel {
    ValueModel::Counter {
        start_low: 0,
        start_high,
        step_low,
        step_high,
    }
}

const PERCENT_TENTHS: ValueModel = gauge(0, 1000, 80, 10);

fn gpu_uuid(seed: u64, gpu: usize) -> String {
    let hash = mix(seed, &[TAG_GPU, index(gpu)]);
    format!(
        "GPU-{:08x}-{:04x}-4000-8000-{:012x}",
        hash >> 32,
        (hash >> 16) & 0xffff,
        index(gpu)
    )
}

fn cpu_time_keys(topology: &Topology) -> Vec<KeySpec> {
    let metrics = [
        ids::CPU_TIME_USER,
        ids::CPU_TIME_NICE,
        ids::CPU_TIME_SYSTEM,
        ids::CPU_TIME_IDLE,
        ids::CPU_TIME_IOWAIT,
        ids::CPU_TIME_IRQ,
        ids::CPU_TIME_SOFTIRQ,
        ids::CPU_TIME_STEAL,
    ];
    let entities = std::iter::once(("cpu".to_owned(), "host".to_owned())).chain(
        (0..topology.cores).map(|core| (format!("cpu.core.{core}"), format!("core:{core}"))),
    );
    entities
        .flat_map(|(entity, scope)| {
            metrics.iter().map(move |metric| {
                KeySpec::new(
                    entity.clone(),
                    metric,
                    scope.clone(),
                    "procfs.cpu",
                    native::TICKS,
                    RawKind::Counter,
                    counter(50_000_000, 0, 100),
                )
            })
        })
        .collect()
}

fn cpu_load_keys() -> Vec<KeySpec> {
    let host = |metric, model| {
        KeySpec::new(
            "host".to_owned(),
            metric,
            "host".to_owned(),
            "procfs.cpu",
            native::COUNT,
            RawKind::Gauge,
            model,
        )
    };
    vec![
        host(ids::CPU_LOAD_1M, gauge(0, 4000, 60, 100)),
        host(ids::CPU_LOAD_5M, gauge(0, 4000, 20, 100)),
        host(ids::CPU_LOAD_15M, gauge(0, 4000, 10, 100)),
        host(ids::CPU_TASKS_RUNNING, gauge(1, 64, 3, 1)),
        host(ids::CPU_TASKS_TOTAL, gauge(300, 2000, 10, 1)),
    ]
}

fn cpu_frequency_keys(topology: &Topology) -> Vec<KeySpec> {
    (0..topology.cores)
        .map(|core| {
            KeySpec::new(
                format!("cpu.core.{core}"),
                ids::CPU_FREQUENCY_CURRENT,
                format!("core:{core}"),
                "procfs.cpu",
                native::KHZ,
                RawKind::Gauge,
                gauge(800_000, 5_000_000, 150_000, 1),
            )
        })
        .collect()
}

fn temperature_keys(topology: &Topology) -> Vec<KeySpec> {
    let model = gauge(35_000, 60_000, 2_000, 1);
    let temperature = |entity: String, metric, scope: String, group| {
        KeySpec::new(
            entity,
            metric,
            scope,
            group,
            native::MILLI_DEG_C,
            RawKind::Gauge,
            model,
        )
    };
    let mut keys = Vec::new();
    for package in 0..topology.packages {
        keys.push(
            temperature(
                format!("cpu.package.{package}"),
                ids::CPU_TEMPERATURE_PACKAGE,
                format!("package:{package}"),
                "hwmon.fast",
            )
            .temperature(
                "thermal.fast",
                Some(format!("x86_pkg_therm:{package}")),
                1000,
            ),
        );
    }
    keys.push(
        temperature(
            "soc".to_owned(),
            ids::SOC_TEMPERATURE,
            "soc".to_owned(),
            "thermal.fast",
        )
        .temperature("thermal.fast", None, 1000),
    );
    for core in 0..topology.cores {
        let package = topology.package_of_core(core);
        keys.push(
            temperature(
                format!("cpu.package.{package}"),
                ids::CPU_TEMPERATURE_CORE,
                format!("core:{core}"),
                "hwmon.fast",
            )
            .temperature(
                "hwmon.fast",
                Some(format!("coretemp:{package}:core:{core}")),
                1000,
            ),
        );
    }
    keys
}

fn power_keys(topology: &Topology) -> Vec<KeySpec> {
    let model = gauge(5_000_000, 120_000_000, 2_000_000, 1);
    let power = |entity: String, metric, scope: &str| {
        KeySpec::new(
            entity,
            metric,
            scope.to_owned(),
            "hwmon.fast",
            native::MICRO_WATT,
            RawKind::Gauge,
            model,
        )
    };
    let mut keys = vec![
        power(
            "cpu.package.0".to_owned(),
            ids::CPU_POWER_CLUSTER,
            "cluster:p",
        )
        .origin("hwmon:ref-gb10:power1".to_owned()),
        power(
            "cpu.package.0".to_owned(),
            ids::CPU_POWER_CLUSTER,
            "cluster:e",
        )
        .origin("hwmon:ref-gb10:power2".to_owned()),
        power("host".to_owned(), ids::CPU_POWER_PLATFORM, "platform")
            .origin("hwmon:ref-gb10:power3".to_owned()),
    ];
    for package in 0..topology.packages {
        keys.push(power(
            format!("cpu.package.{package}"),
            ids::CPU_POWER_PACKAGE,
            &format!("package:{package}"),
        ));
    }
    keys
}

fn energy_keys(topology: &Topology) -> Vec<KeySpec> {
    let energy = |entity: String, metric, scope: &str| {
        KeySpec::new(
            entity,
            metric,
            scope.to_owned(),
            "powercap.fast",
            native::MICRO_JOULE,
            RawKind::Counter,
            counter(RAPL_MODULUS - 1, 10_000_000, 60_000_000),
        )
        .modulus(RAPL_MODULUS)
    };
    let mut keys = Vec::new();
    for package in 0..topology.packages {
        let mut key = energy(
            format!("cpu.package.{package}"),
            ids::CPU_ENERGY_PACKAGE,
            &format!("package:{package}"),
        )
        .origin(format!("rapl:package:{package}"));
        if package == 0 {
            // Starts just below the modulus so the counter wraps early.
            key.model = ValueModel::Counter {
                start_low: RAPL_MODULUS - 150_000_000,
                start_high: RAPL_MODULUS - 120_000_000,
                step_low: 10_000_000,
                step_high: 60_000_000,
            };
        }
        keys.push(key);
    }
    keys.push(
        energy("host".to_owned(), ids::CPU_ENERGY_PLATFORM, "platform")
            .origin("rapl:psys".to_owned()),
    );
    keys
}

fn memory_keys() -> Vec<KeySpec> {
    let memory = |metric, model| {
        KeySpec::new(
            "memory".to_owned(),
            metric,
            "system".to_owned(),
            "procfs.memory",
            native::KIB,
            RawKind::Gauge,
            model,
        )
    };
    vec![
        memory(ids::MEMORY_TOTAL, exact(131_072_000, 131_072_000, 0)),
        memory(
            ids::MEMORY_AVAILABLE,
            exact(60_000_000, 110_000_000, 400_000),
        ),
        memory(ids::MEMORY_BUFFERS, exact(100_000, 900_000, 5_000)),
        memory(ids::MEMORY_CACHED, exact(10_000_000, 40_000_000, 100_000)),
        memory(ids::MEMORY_SWAP_TOTAL, exact(8_388_604, 8_388_604, 0)),
        memory(
            ids::MEMORY_SWAP_FREE_RAW,
            exact(8_000_000, 8_388_604, 1_000),
        ),
    ]
}

fn disk_keys(topology: &Topology) -> Vec<KeySpec> {
    let metrics = [
        (
            ids::DISK_READS_COMPLETED,
            native::COUNT,
            counter(10_000_000, 0, 400),
        ),
        (
            ids::DISK_SECTORS_READ,
            native::SECTORS_512,
            counter(900_000_000, 0, 80_000),
        ),
        (
            ids::DISK_READ_TIME,
            native::MILLISECONDS,
            counter(50_000_000, 0, 300),
        ),
        (
            ids::DISK_WRITES_COMPLETED,
            native::COUNT,
            counter(10_000_000, 0, 300),
        ),
        (
            ids::DISK_SECTORS_WRITTEN,
            native::SECTORS_512,
            counter(900_000_000, 0, 60_000),
        ),
        (
            ids::DISK_WRITE_TIME,
            native::MILLISECONDS,
            counter(50_000_000, 0, 300),
        ),
    ];
    (0..topology.disks)
        .flat_map(|disk| {
            metrics.iter().map(move |(metric, unit, model)| {
                KeySpec::new(
                    format!("blockdev:259:{}", disk * 16),
                    metric,
                    "device".to_owned(),
                    "procfs.disk",
                    unit,
                    RawKind::Counter,
                    *model,
                )
            })
        })
        .collect()
}

fn network_keys(topology: &Topology) -> Vec<KeySpec> {
    let metrics = [
        (
            ids::NET_RX_BYTES,
            native::BYTES,
            counter(900_000_000_000, 0, 120_000_000),
        ),
        (
            ids::NET_TX_BYTES,
            native::BYTES,
            counter(900_000_000_000, 0, 120_000_000),
        ),
        (
            ids::NET_RX_PACKETS,
            native::COUNT,
            counter(900_000_000, 0, 90_000),
        ),
        (
            ids::NET_TX_PACKETS,
            native::COUNT,
            counter(900_000_000, 0, 90_000),
        ),
        (ids::NET_RX_ERRORS, native::COUNT, counter(1_000, 0, 1)),
        (ids::NET_TX_ERRORS, native::COUNT, counter(1_000, 0, 1)),
        (ids::NET_RX_DROPS, native::COUNT, counter(10_000, 0, 2)),
        (ids::NET_TX_DROPS, native::COUNT, counter(10_000, 0, 2)),
    ];
    (0..topology.interfaces)
        .flat_map(|interface| {
            metrics.iter().map(move |(metric, unit, model)| {
                KeySpec::new(
                    format!("netif:{NET_NAMESPACE}:{}", interface + 2),
                    metric,
                    "interface".to_owned(),
                    "network.fast",
                    unit,
                    RawKind::Counter,
                    *model,
                )
            })
        })
        .collect()
}

fn filesystem_keys(topology: &Topology) -> Vec<KeySpec> {
    let metrics = [
        (ids::FS_TOTAL, exact(2 * 1024 * GIB, 2 * 1024 * GIB, 0)),
        (ids::FS_FREE, exact(200 * GIB, 1500 * GIB, 64 * MIB)),
        (ids::FS_AVAILABLE, exact(100 * GIB, 1400 * GIB, 64 * MIB)),
    ];
    (0..topology.filesystems)
        .flat_map(|filesystem| {
            metrics.iter().map(move |(metric, model)| {
                KeySpec::new(
                    format!("fs:{MOUNT_NAMESPACE}:{}", filesystem + 20),
                    metric,
                    "filesystem".to_owned(),
                    "filesystem.capacity",
                    native::BYTES,
                    RawKind::Gauge,
                    *model,
                )
            })
        })
        .collect()
}

fn gpu_fast_keys(topology: &Topology, uuids: &[String]) -> Vec<KeySpec> {
    let metrics: [(&'static str, &'static str, ValueModel, &'static str); 16] = [
        (
            ids::GPU_UTILIZATION_COMPUTE,
            native::PERCENT,
            PERCENT_TENTHS,
            "gpu",
        ),
        (
            ids::GPU_UTILIZATION_MEMORY_BUSY,
            native::PERCENT,
            PERCENT_TENTHS,
            "gpu",
        ),
        (
            ids::GPU_TEMPERATURE,
            native::DEG_C,
            gauge(30, 85, 3, 1),
            "gpu",
        ),
        (
            ids::GPU_POWER_DRAW,
            native::MILLI_WATT,
            gauge(50_000, 700_000, 20_000, 1),
            "gpu",
        ),
        (
            ids::GPU_CLOCK_GRAPHICS,
            native::MHZ,
            gauge(300, 2100, 60, 1),
            "gpu",
        ),
        (
            ids::GPU_CLOCK_MEMORY,
            native::MHZ,
            gauge(800, 2600, 20, 1),
            "gpu",
        ),
        (
            ids::GPU_CLOCK_SM,
            native::MHZ,
            gauge(300, 2100, 60, 1),
            "gpu",
        ),
        (
            ids::GPU_CLOCK_VIDEO,
            native::MHZ,
            gauge(300, 1900, 40, 1),
            "gpu",
        ),
        (ids::GPU_FAN_SPEED, native::PERCENT, PERCENT_TENTHS, "fan:0"),
        (
            ids::GPU_MEMORY_FB_USED,
            native::BYTES,
            exact(GIB, 70 * GIB, 256 * MIB),
            "gpu",
        ),
        (
            ids::GPU_MEMORY_FB_FREE,
            native::BYTES,
            exact(GIB, 70 * GIB, 256 * MIB),
            "gpu",
        ),
        (
            ids::GPU_PCIE_TX,
            native::KIB_PER_SECOND,
            gauge(0, 16_000_000, 400_000, 1),
            "gpu",
        ),
        (
            ids::GPU_PCIE_RX,
            native::KIB_PER_SECOND,
            gauge(0, 16_000_000, 400_000, 1),
            "gpu",
        ),
        (
            ids::GPU_ENCODER_UTILIZATION,
            native::PERCENT,
            PERCENT_TENTHS,
            "gpu",
        ),
        (
            ids::GPU_DECODER_UTILIZATION,
            native::PERCENT,
            PERCENT_TENTHS,
            "gpu",
        ),
        (
            ids::GPU_MEMORY_PROCESS_ALLOCATED,
            native::BYTES,
            exact(0, 70 * GIB, 128 * MIB),
            "gpu",
        ),
    ];
    (0..topology.gpus)
        .flat_map(|gpu| {
            let uuid = uuids.get(gpu).cloned().unwrap_or_default();
            metrics.iter().map(move |(metric, unit, model, scope)| {
                let mut key = KeySpec::new(
                    format!("gpu:{uuid}"),
                    metric,
                    (*scope).to_owned(),
                    "nvml.fast",
                    unit,
                    RawKind::Gauge,
                    *model,
                );
                if *metric == ids::GPU_TEMPERATURE {
                    key = key.temperature("nvml.fast", None, 1);
                }
                key
            })
        })
        .collect()
}

fn gpu_slow_keys(topology: &Topology, uuids: &[String]) -> Vec<KeySpec> {
    let metrics: [(
        &'static str,
        &'static str,
        RawKind,
        ValueModel,
        &'static str,
    ); 10] = [
        (
            ids::GPU_ENERGY_TOTAL,
            native::MILLI_JOULE,
            RawKind::Counter,
            counter(900_000_000_000, 250_000, 3_500_000),
            "gpu",
        ),
        (
            ids::GPU_ECC_CORRECTED,
            native::COUNT,
            RawKind::Counter,
            counter(100, 0, 1),
            "gpu",
        ),
        (
            ids::GPU_ECC_UNCORRECTED,
            native::COUNT,
            RawKind::Counter,
            counter(3, 0, 0),
            "gpu",
        ),
        (
            ids::GPU_RETIRED_PAGES_SINGLE,
            native::COUNT,
            RawKind::Gauge,
            gauge(0, 5, 0, 1),
            "gpu",
        ),
        (
            ids::GPU_RETIRED_PAGES_DOUBLE,
            native::COUNT,
            RawKind::Gauge,
            gauge(0, 2, 0, 1),
            "gpu",
        ),
        (
            ids::GPU_NVLINK_ACTIVE,
            native::COUNT,
            RawKind::Gauge,
            gauge(0, 18, 0, 1),
            "gpu",
        ),
        (
            ids::GPU_POWER_LIMIT,
            native::MILLI_WATT,
            RawKind::Static,
            ValueModel::Static {
                low: 300_000,
                high: 700_000,
            },
            "gpu",
        ),
        (
            ids::GPU_CLOCK_MAXIMUM,
            native::MHZ,
            RawKind::Static,
            ValueModel::Static {
                low: 1980,
                high: 2100,
            },
            "graphics",
        ),
        (
            ids::GPU_MEMORY_FB_TOTAL,
            native::BYTES,
            RawKind::Static,
            ValueModel::Static {
                low: 80 * GIB,
                high: 80 * GIB,
            },
            "gpu",
        ),
        (
            ids::GPU_MEMORY_FB_RESERVED,
            native::BYTES,
            RawKind::Static,
            ValueModel::Static {
                low: 512 * MIB,
                high: 512 * MIB,
            },
            "gpu",
        ),
    ];
    (0..topology.gpus)
        .flat_map(|gpu| {
            let uuid = uuids.get(gpu).cloned().unwrap_or_default();
            metrics
                .iter()
                .map(move |(metric, unit, kind, model, scope)| {
                    KeySpec::new(
                        format!("gpu:{uuid}"),
                        metric,
                        (*scope).to_owned(),
                        "nvml.slow_health",
                        unit,
                        *kind,
                        *model,
                    )
                })
        })
        .collect()
}

/// Key lists per metric family.
fn families(topology: &Topology, uuids: &[String]) -> Vec<Vec<KeySpec>> {
    vec![
        cpu_time_keys(topology),
        cpu_load_keys(),
        cpu_frequency_keys(topology),
        temperature_keys(topology),
        power_keys(topology),
        energy_keys(topology),
        memory_keys(),
        disk_keys(topology),
        network_keys(topology),
        filesystem_keys(topology),
        gpu_fast_keys(topology, uuids),
        gpu_slow_keys(topology, uuids),
    ]
}

fn key_capacity(topology: &Topology) -> usize {
    let uuids: Vec<String> = (0..topology.gpus).map(|gpu| format!("GPU-{gpu}")).collect();
    families(topology, &uuids).iter().map(Vec::len).sum()
}

/// Round-robin over families so every family is represented.
fn select_keys(topology: &Topology, series: usize, uuids: &[String]) -> Vec<KeySpec> {
    let families = families(topology, uuids);
    let longest = families.iter().map(Vec::len).max().unwrap_or(0);
    let mut selected = Vec::with_capacity(series);
    for round in 0..longest {
        for family in &families {
            if selected.len() == series {
                return selected;
            }
            if let Some(key) = family.get(round) {
                selected.push(key.clone());
            }
        }
    }
    selected
}

/// One synthetic process.
#[derive(Debug, Clone)]
struct ProcessSpec {
    entity: EntityId,
    pid: u64,
    basename: &'static str,
    uid: u32,
}

/// One synthetic GPU-process link.
#[derive(Debug, Clone)]
struct LinkSpec {
    entity: EntityId,
    gpu: usize,
}

/// Source ids of the row sources.
#[derive(Debug, Clone)]
struct RowSources {
    ticks: SourceId,
    gpu_memory: Vec<SourceId>,
    gpu_utilization: Vec<SourceId>,
}

struct Generator {
    seed: u64,
    plan: Plan,
    uuids: Vec<String>,
    keys: Vec<KeySpec>,
}

impl Generator {
    fn fixture(&self, profile: &RefProfile) -> ReplayFixture {
        let mut sources = Vec::new();
        let mut key_sources: Vec<Vec<SourceId>> = Vec::with_capacity(self.keys.len());
        for (key_index, key) in self.keys.iter().enumerate() {
            let count = self.plan.candidates(key_index, self.keys.len());
            let mut ids = Vec::with_capacity(count);
            for candidate in 0..count {
                if let Some(source) = self.key_source(key_index, key, candidate) {
                    ids.push(source.source_id.clone());
                    sources.push(source);
                }
            }
            key_sources.push(ids);
        }
        let rows = self.row_sources();
        let (processes, links) = match &rows {
            Some((row_sources, _)) => {
                sources.extend(row_sources.iter().cloned());
                let processes = self.processes();
                let links = self.links(&processes);
                (processes, links)
            }
            None => (Vec::new(), Vec::new()),
        };
        let row_ids = rows.map(|(_, row_ids)| row_ids);
        let cycles = self.cycles(&key_sources, row_ids.as_ref(), &processes, &links);
        ReplayFixture {
            format: FIXTURE_FORMAT.to_owned(),
            seed: self.seed,
            namespace_label: namespace_label(&profile.id),
            host_id: HostId::new("replay-ref-host").ok(),
            sampling_ms: self.plan.sampling_ms,
            simulated_latency_us: self.plan.latency_us,
            entities: self.entities(),
            sources,
            cycles,
            faults: Vec::new(),
        }
    }

    fn key_source(
        &self,
        key_index: usize,
        key: &KeySpec,
        candidate: usize,
    ) -> Option<FixtureSource> {
        let group = if candidate % 2 == 1 {
            key.alias_group
        } else {
            key.primary_group
        };
        let adapter = group.split('.').next().unwrap_or("replay");
        let shared_origin = || {
            key.origin
                .clone()
                .unwrap_or_else(|| format!("ref:{key_index}:a"))
        };
        let (origin, quality) = match candidate {
            // Primary and its confirmed alias read the same physical sensor.
            0 | 1 => (confirmed(&shared_origin())?, Quality::Verified),
            2 => (confirmed(&format!("ref:{key_index}:b"))?, Quality::Usable),
            _ => (OriginIdentity::unknown(), Quality::Usable),
        };
        Some(FixtureSource {
            source_id: SourceId::new(format!(
                "{adapter}:ref{key_index}:{}#{candidate}",
                key.metric
            ))
            .ok()?,
            entity_id: EntityId::new(key.entity.as_str()).ok()?,
            metric_id: MetricId::new(key.metric).ok()?,
            scope: Scope::new(key.scope.as_str()).ok()?,
            native_unit: NativeUnit::new(key.unit).ok()?,
            raw_kind: key.kind,
            sample_kind: SampleKind::Simulated,
            read_group: group.to_owned(),
            origin,
            quality,
            profile_id: ProfileId::new(PROFILE_ID).ok()?,
            profile_revision: 1,
            profile_rank: u16::try_from(candidate).unwrap_or(u16::MAX),
            driver: Some(format!("ref-{adapter}")),
            device_path: None,
            channel: None,
            label: None,
            counter_modulus: key.modulus,
            update_interval_ms: None,
            rows: false,
        })
    }

    /// Row sources (procfs process metrics and per-GPU nvml process metrics).
    fn row_sources(&self) -> Option<(Vec<FixtureSource>, RowSources)> {
        let host = EntityId::new("host").ok()?;
        let procfs = |id: &str, metric, unit, kind| {
            row_source(
                id,
                host.clone(),
                metric,
                "process",
                unit,
                kind,
                "procfs.process",
            )
        };
        let ticks = procfs(
            "procfs:process:cpu.ticks",
            ids::PROCESS_CPU_TICKS,
            native::TICKS,
            RawKind::Counter,
        )?;
        let mut row_ids = RowSources {
            ticks: ticks.source_id.clone(),
            gpu_memory: Vec::new(),
            gpu_utilization: Vec::new(),
        };
        let mut sources = vec![ticks];
        for uuid in &self.uuids {
            let entity = EntityId::new(format!("gpu:{uuid}")).ok()?;
            let scope = format!("gpu:{uuid}");
            let memory = row_source(
                &format!("nvml:process:{uuid}:memory_allocated"),
                entity.clone(),
                ids::PROCESS_GPU_MEMORY,
                &scope,
                native::BYTES,
                RawKind::Gauge,
                "nvml.process",
            )?;
            let utilization = row_source(
                &format!("nvml:process:{uuid}:utilization"),
                entity,
                ids::PROCESS_GPU_UTILIZATION,
                &scope,
                native::PERCENT,
                RawKind::Gauge,
                "nvml.process",
            )?;
            row_ids.gpu_memory.push(memory.source_id.clone());
            row_ids.gpu_utilization.push(utilization.source_id.clone());
            sources.push(memory);
            sources.push(utilization);
        }
        Some((sources, row_ids))
    }

    fn processes(&self) -> Vec<ProcessSpec> {
        (0..self.plan.processes)
            .filter_map(|position| {
                let position = index(position);
                let pid = 1000 + position * 3 + 1;
                let start = uniform(self.seed, &[TAG_PROCESS, position], 10_000, 9_000_000);
                let basename_index =
                    usize::try_from(mix(self.seed, &[TAG_PROCESS, position, 1]) % 8).unwrap_or(0);
                let uid_index = usize::try_from(position % 4).unwrap_or(0);
                Some(ProcessSpec {
                    entity: EntityId::new(format!("process:{PID_NAMESPACE}:{pid}:{start}")).ok()?,
                    pid,
                    basename: BASENAMES[basename_index],
                    uid: UIDS[uid_index],
                })
            })
            .collect()
    }

    fn links(&self, processes: &[ProcessSpec]) -> Vec<LinkSpec> {
        if processes.is_empty() || self.uuids.is_empty() {
            return Vec::new();
        }
        (0..self.plan.links)
            .filter_map(|link| {
                let process = &processes[link % processes.len()];
                let round = link / processes.len();
                let gpu = (link % processes.len() + round) % self.uuids.len();
                let uuid = &self.uuids[gpu];
                Some(LinkSpec {
                    entity: EntityId::new(format!("gpupid:{uuid}:{}", process.pid)).ok()?,
                    gpu,
                })
            })
            .collect()
    }

    fn entities(&self) -> Vec<FixtureEntity> {
        let mut entities: BTreeMap<String, FixtureEntity> = BTreeMap::new();
        let mut add = |id: &str| {
            if let Some(entity) = entity_record(id, &self.uuids) {
                entities.entry(id.to_owned()).or_insert(entity);
            }
        };
        add("host");
        for key in &self.keys {
            add(&key.entity);
        }
        for uuid in &self.uuids {
            add(&format!("gpu:{uuid}"));
        }
        entities.into_values().collect()
    }

    fn cycles(
        &self,
        key_sources: &[Vec<SourceId>],
        rows: Option<&RowSources>,
        processes: &[ProcessSpec],
        links: &[LinkSpec],
    ) -> Vec<Vec<FixtureSample>> {
        let mut counters: Vec<u64> = self
            .keys
            .iter()
            .enumerate()
            .map(|(key_index, key)| self.counter_start(key_index, key))
            .collect();
        let mut ticks: Vec<u64> = (0..processes.len())
            .map(|process| uniform(self.seed, &[TAG_PROCESS, index(process), 2], 0, 5_000_000))
            .collect();
        let mut cycles = Vec::with_capacity(self.plan.cycles);
        for cycle in 0..self.plan.cycles {
            let mut samples = Vec::new();
            for (key_index, key) in self.keys.iter().enumerate() {
                if cycle > 0 {
                    counters[key_index] = self.advance(key_index, key, cycle, counters[key_index]);
                }
                let ids = key_sources.get(key_index).map_or(&[][..], Vec::as_slice);
                self.key_samples(
                    &mut samples,
                    key_index,
                    key,
                    cycle,
                    counters[key_index],
                    ids,
                );
            }
            if let Some(rows) = rows {
                self.process_samples(&mut samples, cycle, rows, processes, &mut ticks);
                self.link_samples(&mut samples, cycle, rows, links);
            }
            cycles.push(samples);
        }
        cycles
    }

    fn counter_start(&self, key_index: usize, key: &KeySpec) -> u64 {
        match key.model {
            ValueModel::Counter {
                start_low,
                start_high,
                ..
            } => uniform(
                self.seed,
                &[TAG_BASE, index(key_index)],
                start_low,
                start_high,
            ),
            _ => 0,
        }
    }

    fn advance(&self, key_index: usize, key: &KeySpec, cycle: usize, value: u64) -> u64 {
        let ValueModel::Counter {
            step_low,
            step_high,
            ..
        } = key.model
        else {
            return value;
        };
        let step = uniform(
            self.seed,
            &[TAG_STEP, index(key_index), index(cycle)],
            step_low,
            step_high,
        );
        match key.modulus {
            Some(modulus) => {
                let sum = (u128::from(value) + u128::from(step)) % u128::from(modulus);
                u64::try_from(sum).unwrap_or(0)
            }
            None => value.saturating_add(step),
        }
    }

    /// Primary integer value of a gauge key at `cycle`.
    fn gauge_level(&self, key_index: usize, cycle: usize, low: u64, high: u64, spread: u64) -> i64 {
        let base = to_i64(uniform(self.seed, &[TAG_BASE, index(key_index)], low, high));
        let noise = signed_noise(
            self.seed,
            &[TAG_NOISE, index(key_index), index(cycle)],
            spread,
        );
        (base + noise).clamp(to_i64(low), to_i64(high))
    }

    fn key_samples(
        &self,
        samples: &mut Vec<FixtureSample>,
        key_index: usize,
        key: &KeySpec,
        cycle: usize,
        counter_value: u64,
        ids: &[SourceId],
    ) {
        let (key_tag, cycle_tag) = (index(key_index), index(cycle));
        let alarm =
            key.degree.is_some() && per_mille(self.seed, &[TAG_ALARM, key_tag, cycle_tag]) < 10;
        let conflict =
            key.degree.is_some() && per_mille(self.seed, &[TAG_CONFLICT, key_tag, cycle_tag]) < 15;
        for (candidate, source_id) in ids.iter().enumerate() {
            let candidate_tag = index(candidate);
            let missing = per_mille(self.seed, &[TAG_MISSING, key_tag, candidate_tag, cycle_tag]);
            if missing < 8 {
                if missing % 2 == 1 {
                    let mut sample = FixtureSample::status(source_id.clone(), ReadStatus::Timeout);
                    sample.reasons.push("SYNTHETIC_TIMEOUT".to_owned());
                    samples.push(sample);
                }
                continue;
            }
            let Some(value) = self.candidate_value(
                key_index,
                key,
                cycle,
                counter_value,
                candidate,
                alarm,
                conflict,
            ) else {
                continue;
            };
            let mut sample = FixtureSample::value(source_id.clone(), value);
            if alarm {
                sample.reasons.push("ALARM".to_owned());
            }
            let faulted = key.degree.is_some()
                && per_mille(self.seed, &[TAG_FAULT, key_tag, candidate_tag, cycle_tag]) < 5;
            if faulted {
                sample.quality = Some(Quality::Invalid);
                sample.reasons.push("SENSOR_FAULT".to_owned());
            }
            samples.push(sample);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate_value(
        &self,
        key_index: usize,
        key: &KeySpec,
        cycle: usize,
        counter_value: u64,
        candidate: usize,
        alarm: bool,
        conflict: bool,
    ) -> Option<FixtureValue> {
        match key.model {
            ValueModel::Gauge {
                low,
                high,
                spread,
                divisor,
            } => {
                let mut level = self.gauge_level(key_index, cycle, low, high, spread);
                if let Some(degree) = key.degree {
                    level = self.temperature_level(
                        key_index, cycle, candidate, level, degree, alarm, conflict,
                    );
                }
                let exact_value = exact_i64_to_f64(level)?;
                Some(FixtureValue::F64(exact_value / f64::from(divisor.max(1))))
            }
            ValueModel::ExactGauge { low, high, spread } => {
                let level = self.gauge_level(key_index, cycle, low, high, spread);
                Some(FixtureValue::U64(u64::try_from(level).ok()?))
            }
            ValueModel::Counter { .. } => Some(FixtureValue::U64(counter_value)),
            ValueModel::Static { low, high } => Some(FixtureValue::U64(uniform(
                self.seed,
                &[TAG_BASE, index(key_index)],
                low,
                high,
            ))),
        }
    }

    /// Temperature candidates: aliases equal, independent/unknown within
    /// tolerance except on conflict cycles; alarms are real high readings.
    #[allow(clippy::too_many_arguments)]
    fn temperature_level(
        &self,
        key_index: usize,
        cycle: usize,
        candidate: usize,
        level: i64,
        degree: i64,
        alarm: bool,
        conflict: bool,
    ) -> i64 {
        let level = if alarm { 96 * degree } else { level };
        let jitter = |spread: i64| {
            signed_noise(
                self.seed,
                &[
                    TAG_CANDIDATE,
                    index(key_index),
                    index(candidate),
                    index(cycle),
                ],
                u64::try_from(spread).unwrap_or(0),
            )
        };
        match candidate {
            2 if conflict => level + 26 * degree,
            2 => level + jitter(degree / 2),
            3 => level + jitter(degree * 2 / 5),
            _ => level,
        }
    }

    fn process_samples(
        &self,
        samples: &mut Vec<FixtureSample>,
        cycle: usize,
        rows: &RowSources,
        processes: &[ProcessSpec],
        ticks: &mut [u64],
    ) {
        for (position, process) in processes.iter().enumerate() {
            let tag = index(position);
            let cycle_tag = index(cycle);
            if cycle > 0 {
                ticks[position] = ticks[position].saturating_add(uniform(
                    self.seed,
                    &[TAG_STEP, TAG_PROCESS, tag, cycle_tag],
                    0,
                    400,
                ));
            }
            if per_mille(self.seed, &[TAG_MISSING, TAG_PROCESS, tag, cycle_tag]) < 2 {
                continue;
            }
            let rss_base = uniform(self.seed, &[TAG_PROCESS, tag, 3], 64 * MIB, 8 * GIB);
            let rss = rss_base.saturating_add(uniform(
                self.seed,
                &[TAG_NOISE, TAG_PROCESS, tag, cycle_tag],
                0,
                32 * MIB,
            ));
            let stat = ProcessStat {
                rss_bytes: Some(rss),
                uid: Some(process.uid),
                ppid: Some(1),
                state: Some('S'),
                reaped_ticks: Some(0),
            };
            let mut sample =
                FixtureSample::value(rows.ticks.clone(), FixtureValue::U64(ticks[position]));
            sample.entity_id = Some(process.entity.clone());
            sample.channel = Some(process.basename.to_owned());
            sample.reasons = stat.reason_codes();
            samples.push(sample);
        }
    }

    fn link_samples(
        &self,
        samples: &mut Vec<FixtureSample>,
        cycle: usize,
        rows: &RowSources,
        links: &[LinkSpec],
    ) {
        for (position, link) in links.iter().enumerate() {
            let (tag, cycle_tag) = (index(position), index(cycle));
            let (Some(memory_id), Some(utilization_id)) = (
                rows.gpu_memory.get(link.gpu),
                rows.gpu_utilization.get(link.gpu),
            ) else {
                continue;
            };
            let memory =
                uniform(self.seed, &[TAG_LINK, tag], 256 * MIB, 16 * GIB).saturating_add(uniform(
                    self.seed,
                    &[TAG_NOISE, TAG_LINK, tag, cycle_tag],
                    0,
                    64 * MIB,
                ));
            let tenths = uniform(
                self.seed,
                &[TAG_NOISE, TAG_LINK, tag, cycle_tag, 1],
                0,
                1000,
            );
            let Some(utilization) = i64::try_from(tenths).ok().and_then(exact_i64_to_f64) else {
                continue;
            };
            let mut memory_sample =
                FixtureSample::value(memory_id.clone(), FixtureValue::U64(memory));
            memory_sample.entity_id = Some(link.entity.clone());
            samples.push(memory_sample);
            let mut utilization_sample = FixtureSample::value(
                utilization_id.clone(),
                FixtureValue::F64(utilization / 10.0),
            );
            utilization_sample.entity_id = Some(link.entity.clone());
            samples.push(utilization_sample);
        }
    }
}

fn confirmed(origin: &str) -> Option<OriginIdentity> {
    Some(OriginIdentity::confirmed(OriginId::new(origin).ok()?))
}

#[allow(clippy::too_many_arguments)]
fn row_source(
    source_id: &str,
    entity_id: EntityId,
    metric: &str,
    scope: &str,
    unit: &str,
    kind: RawKind,
    group: &str,
) -> Option<FixtureSource> {
    let adapter = group.split('.').next().unwrap_or("replay");
    Some(FixtureSource {
        source_id: SourceId::new(source_id).ok()?,
        entity_id,
        metric_id: MetricId::new(metric).ok()?,
        scope: Scope::new(scope).ok()?,
        native_unit: NativeUnit::new(unit).ok()?,
        raw_kind: kind,
        sample_kind: SampleKind::Simulated,
        read_group: group.to_owned(),
        origin: OriginIdentity::unknown(),
        quality: Quality::Verified,
        profile_id: ProfileId::new(PROFILE_ID).ok()?,
        profile_revision: 1,
        profile_rank: 0,
        driver: Some(format!("ref-{adapter}")),
        device_path: None,
        channel: None,
        label: None,
        counter_modulus: None,
        update_interval_ms: None,
        rows: true,
    })
}

fn namespace_label(profile_id: &str) -> String {
    let cleaned: String = profile_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(40)
        .collect::<String>()
        .to_ascii_lowercase();
    if cleaned.is_empty() {
        "ref".to_owned()
    } else {
        format!("ref-{cleaned}")
    }
}

/// Entity record of a synthetic entity id.
fn entity_record(id: &str, uuids: &[String]) -> Option<FixtureEntity> {
    let attribute = |key: &str, value: &str| Attribute {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    let (kind, parent, name, attributes) = if id == "host" {
        (
            EntityKind::Host,
            None,
            "synthetic host".to_owned(),
            Vec::new(),
        )
    } else if id == "memory" {
        (
            EntityKind::SystemMemory,
            Some("host"),
            "system memory".to_owned(),
            Vec::new(),
        )
    } else if id == "soc" {
        (EntityKind::Soc, Some("host"), "SoC".to_owned(), Vec::new())
    } else if let Some(package) = id.strip_prefix("cpu.package.") {
        (
            EntityKind::CpuPackage,
            Some("host"),
            format!("CPU package {package}"),
            vec![attribute("physical_package", package)],
        )
    } else if let Some(core) = id.strip_prefix("cpu.core.") {
        (
            EntityKind::CpuCore,
            Some("host"),
            format!("cpu{core}"),
            Vec::new(),
        )
    } else if let Some(uuid) = id.strip_prefix("gpu:") {
        let position = uuids.iter().position(|candidate| candidate == uuid)?;
        (
            EntityKind::Gpu,
            Some("host"),
            format!("Synthetic GPU {position}"),
            vec![
                attribute("uuid", uuid),
                attribute("nvml_index", &position.to_string()),
            ],
        )
    } else if id.starts_with("blockdev:") {
        (
            EntityKind::BlockDevice,
            Some("host"),
            id.to_owned(),
            Vec::new(),
        )
    } else if id.starts_with("netif:") {
        (
            EntityKind::NetworkInterface,
            Some("host"),
            id.to_owned(),
            Vec::new(),
        )
    } else if id.starts_with("fs:") {
        (
            EntityKind::Filesystem,
            Some("host"),
            id.to_owned(),
            Vec::new(),
        )
    } else {
        return None;
    };
    Some(FixtureEntity {
        entity_id: EntityId::new(id).ok()?,
        kind,
        parent_id: parent.and_then(|parent| EntityId::new(parent).ok()),
        display_name: name,
        attributes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_clamps_to_fixture_bounds() {
        let mut profile = RefProfile::reference();
        profile.sources = 1_000_000;
        profile.cycles = 1_000_000;
        let plan = Plan::from_profile(&profile);
        assert!(plan.sources <= plan.series * plan.max_candidates);
        assert!(plan.sources + PROCESS_ROW_SOURCES + 2 * plan.gpus <= MAX_SOURCES);
        let per_cycle = plan.sources + plan.processes + plan.links * 2;
        assert!(per_cycle <= MAX_SAMPLES_PER_CYCLE);
        assert!(per_cycle * plan.cycles <= MAX_TOTAL_SAMPLES);
    }

    #[test]
    fn reference_plan_matches_scenarios_numbers() {
        let plan = Plan::from_profile(&RefProfile::reference());
        assert_eq!(plan.series, 512);
        // 2048 catalog sources: key sources plus 1 procfs and 2 x 8 GPU rows.
        assert_eq!(plan.sources + PROCESS_ROW_SOURCES + 2 * plan.gpus, 2048);
        assert_eq!(plan.processes, 256);
        assert_eq!(plan.links, 1024);
        assert_eq!(
            (0..512).map(|key| plan.candidates(key, 512)).sum::<usize>(),
            plan.sources
        );
    }

    #[test]
    fn overload_plan_fits_sample_budget() {
        let plan = Plan::from_profile(&RefProfile::overload());
        assert_eq!(plan.series, 4096);
        assert_eq!(plan.sources, 8192 - PROCESS_ROW_SOURCES - 16);
        assert_eq!(plan.processes, 4096);
        assert_eq!(plan.links, 16_384);
    }
}
