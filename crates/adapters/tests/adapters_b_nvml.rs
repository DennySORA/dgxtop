//! NVML adapter behaviour against a fake GPU backend (no driver needed), plus
//! one `#[ignore]` smoke test on this machine's real GPUs.

// Test-only crate: a broken fixture must panic loudly, also in helpers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use dgxtop_adapters::nvml::{
    BackendConnector, BackendError, BackendResult, C2cLinks, ClockDomain, ConnectError,
    DeviceDetails, EccCounterScope, EccErrorKind, FramebufferMemory, GpuBackend, GpuInventory,
    GpuProcessEntry, GpuProcessQueries, GpuTelemetry, HealthValue, NvmlAdapter,
    PCIE_UNVERIFIED_REASON, PcieDirection, PcieLink, ProcessUtilizationSample, RetiredPageCause,
    StaticFact, ThermalThreshold, ThrottleCause, UtilizationRates,
};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::gpu_state::{GpuLink, PeerAccess};
use dgxtop_core::metric::{CanonicalUnit, ids, native, normalize};
use dgxtop_core::observation::{
    BatchStatus, CatalogDelta, DiscoverRequest, Observation, ObservationBatch, ReadPlan,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, Quality, ReadStatus, Support};
use dgxtop_core::testing::{FakeClock, adapter_id, host_id, read_group};
use uuid::Uuid;

const SECOND_MS: u64 = 1000;

// ---------------------------------------------------------------- fake GPU

#[derive(Debug, Clone)]
struct FakeDevice {
    uuid: String,
    details: BackendResult<DeviceDetails>,
    fans: BackendResult<u32>,
    framebuffer: BackendResult<FramebufferMemory>,
    utilization: BackendResult<UtilizationRates>,
    temperature: BackendResult<u32>,
    power_draw: BackendResult<u32>,
    power_instant: BackendResult<u32>,
    power_limit: BackendResult<u32>,
    clock: BackendResult<u32>,
    max_clock: BackendResult<u32>,
    fan_speed: BackendResult<u32>,
    pcie: BackendResult<u32>,
    encoder: BackendResult<u32>,
    energy: BackendResult<u64>,
    ecc: BackendResult<u64>,
    retired: BackendResult<u64>,
    nvlinks: Vec<BackendResult<bool>>,
    performance_state: BackendResult<u32>,
    throttle: BackendResult<u64>,
    pcie_link: [BackendResult<u32>; 2],
    max_pcie_link: [BackendResult<u32>; 2],
    /// Slowdown, shutdown, max operating, target.
    thresholds: [BackendResult<u32>; 4],
    static_facts: BTreeMap<StaticFact, BackendResult<u64>>,
    health: BTreeMap<HealthValue, BackendResult<u64>>,
    cpu_affinity: BackendResult<Vec<u64>>,
    numa_affinity: BackendResult<Vec<u64>>,
    c2c: BackendResult<C2cLinks>,
    /// The path and peer access to every other device.
    peer_link: BackendResult<GpuLink>,
    peer_access: BackendResult<PeerAccess>,
    compute: BackendResult<Vec<GpuProcessEntry>>,
    graphics: BackendResult<Vec<GpuProcessEntry>>,
    samples: BackendResult<Vec<ProcessUtilizationSample>>,
    lost: bool,
}

impl FakeDevice {
    fn healthy(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_owned(),
            details: Ok(DeviceDetails {
                name: Some(format!("Fake GPU {uuid}")),
                pci_bus_id: Some("00000000:01:00.0".to_owned()),
                serial: Some("SER-1".to_owned()),
                architecture: Some("Blackwell".to_owned()),
                brand: Some("GeForce".to_owned()),
                compute_capability: Some("12.0".to_owned()),
                vbios_version: Some("98.03.3B.00.01".to_owned()),
                inforom_image: Some("G005.0000.98.01".to_owned()),
                gsp_firmware: Some("595.12".to_owned()),
                board_part_number: Some("900-1G144-2545-000".to_owned()),
                minor_number: Some(0),
                pci_device_id: Some("10de:2c02".to_owned()),
                pci_subsystem_id: Some("10de:2095".to_owned()),
            }),
            fans: Ok(1),
            framebuffer: Ok(FramebufferMemory {
                total: 16 << 30,
                reserved: 1 << 28,
                used: 4 << 30,
                free: 11 << 30,
            }),
            utilization: Ok(UtilizationRates {
                compute_percent: 37,
                memory_busy_percent: 12,
            }),
            temperature: Ok(61),
            power_draw: Ok(250_000),
            power_instant: Ok(262_500),
            power_limit: Ok(360_000),
            clock: Ok(1800),
            max_clock: Ok(3000),
            fan_speed: Ok(44),
            pcie: Ok(1234),
            encoder: Ok(3),
            energy: Ok(987_654_321),
            ecc: Ok(2),
            retired: Ok(1),
            nvlinks: vec![Err(BackendError::NotSupported)],
            performance_state: Ok(2),
            throttle: Ok(0x4),
            pcie_link: [Ok(4), Ok(16)],
            max_pcie_link: [Ok(5), Ok(16)],
            thresholds: [Ok(90), Ok(93), Ok(88), Err(BackendError::NotSupported)],
            static_facts: [
                (StaticFact::Cores, Ok(10_752)),
                (StaticFact::MemoryBusWidth, Ok(256)),
                (StaticFact::Bar1Total, Ok(16 << 30)),
                (StaticFact::PowerLimitDefault, Ok(360_000)),
                (StaticFact::PowerLimitMinimum, Ok(300_000)),
                (StaticFact::PowerLimitMaximum, Ok(390_000)),
                (StaticFact::PowerLimitRequested, Ok(380_000)),
            ]
            .into_iter()
            .collect(),
            health: HealthValue::all()
                .into_iter()
                .map(|value| {
                    let reading = match value {
                        HealthValue::Bar1Used => 6 << 20,
                        HealthValue::EncoderSessions => 1,
                        HealthValue::EncoderFps => 60,
                        HealthValue::EncoderLatency => 2_500,
                        HealthValue::ThrottleTime(ThrottleCause::Power) => 219_152_032,
                        _ => 0,
                    };
                    (value, Ok(reading))
                })
                .collect(),
            cpu_affinity: Ok(vec![0xffff_ffff]),
            numa_affinity: Ok(vec![0b1]),
            // A discrete GPU: no chip-to-chip link.
            c2c: Err(BackendError::NotSupported),
            peer_link: Ok(GpuLink::HostBridge),
            peer_access: Ok(PeerAccess {
                read: false,
                write: false,
            }),
            compute: Ok(Vec::new()),
            graphics: Ok(Vec::new()),
            samples: Ok(Vec::new()),
            lost: false,
        }
    }
}

#[derive(Debug)]
struct FakeState {
    driver: String,
    devices: Vec<FakeDevice>,
    uninitialized: bool,
    count_error: Option<BackendError>,
    calls: BTreeMap<String, u64>,
    utilization_since: Vec<(String, u64)>,
}

#[derive(Debug, Clone)]
struct FakeGpu {
    state: Arc<Mutex<FakeState>>,
}

impl FakeGpu {
    fn new(devices: Vec<FakeDevice>) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                driver: "595.12".to_owned(),
                devices,
                uninitialized: false,
                count_error: None,
                calls: BTreeMap::new(),
                utilization_since: Vec::new(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn calls(&self, method: &str) -> u64 {
        self.lock().calls.get(method).copied().unwrap_or(0)
    }

    fn device_calls(&self, method: &str, uuid: &str) -> u64 {
        self.calls(&format!("{method}:{uuid}"))
    }

    fn total_calls_for(&self, uuid: &str) -> u64 {
        let suffix = format!(":{uuid}");
        self.lock()
            .calls
            .iter()
            .filter(|(key, _)| key.ends_with(&suffix))
            .map(|(_, count)| *count)
            .sum()
    }

    fn edit(&self, uuid: &str, change: impl FnOnce(&mut FakeDevice)) {
        let mut state = self.lock();
        let device = state
            .devices
            .iter_mut()
            .find(|device| device.uuid == uuid)
            .expect("fake device exists");
        change(device);
    }

    fn query<T: Clone>(
        &self,
        method: &str,
        uuid: &str,
        pick: impl FnOnce(&FakeDevice) -> BackendResult<T>,
    ) -> BackendResult<T> {
        let mut state = self.lock();
        *state.calls.entry(method.to_owned()).or_insert(0) += 1;
        *state.calls.entry(format!("{method}:{uuid}")).or_insert(0) += 1;
        if state.uninitialized {
            return Err(BackendError::Uninitialized);
        }
        let device = state
            .devices
            .iter()
            .find(|device| device.uuid == uuid)
            .ok_or(BackendError::GpuLost)?;
        if device.lost {
            return Err(BackendError::GpuLost);
        }
        pick(device)
    }
}

impl GpuInventory for FakeGpu {
    fn driver_version(&self) -> BackendResult<String> {
        let state = self.lock();
        if state.uninitialized {
            return Err(BackendError::Uninitialized);
        }
        Ok(state.driver.clone())
    }

    fn nvml_version(&self) -> BackendResult<String> {
        Ok("13.595.12".to_owned())
    }

    fn cuda_driver_version(&self) -> BackendResult<String> {
        Ok("13.2".to_owned())
    }

    fn device_count(&self) -> BackendResult<u32> {
        let mut state = self.lock();
        *state.calls.entry("device_count".to_owned()).or_insert(0) += 1;
        if state.uninitialized {
            return Err(BackendError::Uninitialized);
        }
        if let Some(error) = state.count_error.clone() {
            return Err(error);
        }
        Ok(u32::try_from(state.devices.len()).unwrap())
    }

    fn device_uuid(&self, index: u32) -> BackendResult<String> {
        let state = self.lock();
        state
            .devices
            .get(usize::try_from(index).unwrap())
            .map(|device| device.uuid.clone())
            .ok_or(BackendError::GpuLost)
    }

    fn device_details(&self, uuid: &str) -> BackendResult<DeviceDetails> {
        self.query("device_details", uuid, |device| device.details.clone())
    }

    fn compute_mode(&self, uuid: &str) -> BackendResult<String> {
        self.query("compute_mode", uuid, |_| Ok("default".to_owned()))
    }

    fn persistence_mode(&self, uuid: &str) -> BackendResult<bool> {
        self.query("persistence_mode", uuid, |_| Ok(true))
    }

    fn fan_count(&self, uuid: &str) -> BackendResult<u32> {
        self.query("fan_count", uuid, |device| device.fans.clone())
    }

    fn power_limit_mw(&self, uuid: &str) -> BackendResult<u32> {
        self.query("power_limit", uuid, |device| device.power_limit.clone())
    }

    fn max_clock_mhz(&self, uuid: &str, _domain: ClockDomain) -> BackendResult<u32> {
        self.query("max_clock", uuid, |device| device.max_clock.clone())
    }

    fn temperature_threshold_c(
        &self,
        uuid: &str,
        threshold: ThermalThreshold,
    ) -> BackendResult<u32> {
        let index = ThermalThreshold::ALL
            .iter()
            .position(|known| *known == threshold)
            .unwrap();
        self.query("temperature_threshold", uuid, |device| {
            device.thresholds[index].clone()
        })
    }

    fn max_pcie_link(&self, uuid: &str, link: PcieLink) -> BackendResult<u32> {
        self.query("max_pcie_link", uuid, |device| {
            device.max_pcie_link[usize::from(link == PcieLink::Width)].clone()
        })
    }

    fn static_fact(&self, uuid: &str, fact: StaticFact) -> BackendResult<u64> {
        self.query("static_fact", uuid, |device| {
            device
                .static_facts
                .get(&fact)
                .cloned()
                .unwrap_or(Err(BackendError::NotSupported))
        })
    }

    fn cpu_affinity(&self, uuid: &str) -> BackendResult<Vec<u64>> {
        self.query("cpu_affinity", uuid, |device| device.cpu_affinity.clone())
    }

    fn numa_affinity(&self, uuid: &str) -> BackendResult<Vec<u64>> {
        self.query("numa_affinity", uuid, |device| device.numa_affinity.clone())
    }

    fn c2c_links(&self, uuid: &str) -> BackendResult<C2cLinks> {
        self.query("c2c_links", uuid, |device| device.c2c.clone())
    }

    fn peer_link(&self, uuid: &str, _peer: &str) -> BackendResult<GpuLink> {
        self.query("peer_link", uuid, |device| device.peer_link.clone())
    }

    fn peer_access(&self, uuid: &str, _peer: &str) -> BackendResult<PeerAccess> {
        self.query("peer_access", uuid, |device| device.peer_access.clone())
    }
}

impl GpuTelemetry for FakeGpu {
    fn utilization(&self, uuid: &str) -> BackendResult<UtilizationRates> {
        self.query("utilization", uuid, |device| device.utilization.clone())
    }

    fn temperature_c(&self, uuid: &str) -> BackendResult<u32> {
        self.query("temperature", uuid, |device| device.temperature.clone())
    }

    fn power_draw_mw(&self, uuid: &str) -> BackendResult<u32> {
        self.query("power_draw", uuid, |device| device.power_draw.clone())
    }

    fn power_instant_mw(&self, uuid: &str) -> BackendResult<u32> {
        self.query("power_instant", uuid, |device| device.power_instant.clone())
    }

    fn clock_mhz(&self, uuid: &str, _domain: ClockDomain) -> BackendResult<u32> {
        self.query("clock", uuid, |device| device.clock.clone())
    }

    fn fan_speed_percent(&self, uuid: &str, _fan: u32) -> BackendResult<u32> {
        self.query("fan_speed", uuid, |device| device.fan_speed.clone())
    }

    fn framebuffer_memory(&self, uuid: &str) -> BackendResult<FramebufferMemory> {
        self.query("framebuffer", uuid, |device| device.framebuffer.clone())
    }

    fn pcie_throughput_kb(&self, uuid: &str, _direction: PcieDirection) -> BackendResult<u32> {
        self.query("pcie", uuid, |device| device.pcie.clone())
    }

    fn encoder_utilization(&self, uuid: &str) -> BackendResult<u32> {
        self.query("encoder", uuid, |device| device.encoder.clone())
    }

    fn decoder_utilization(&self, uuid: &str) -> BackendResult<u32> {
        self.query("decoder", uuid, |device| device.encoder.clone())
    }

    fn total_energy_mj(&self, uuid: &str) -> BackendResult<u64> {
        self.query("energy", uuid, |device| device.energy.clone())
    }

    fn ecc_errors(
        &self,
        uuid: &str,
        _kind: EccErrorKind,
        _scope: EccCounterScope,
    ) -> BackendResult<u64> {
        self.query("ecc", uuid, |device| device.ecc.clone())
    }

    fn retired_page_count(&self, uuid: &str, _cause: RetiredPageCause) -> BackendResult<u64> {
        self.query("retired", uuid, |device| device.retired.clone())
    }

    fn nvlink_active(&self, uuid: &str, link: u32) -> BackendResult<bool> {
        self.query("nvlink", uuid, |device| {
            device
                .nvlinks
                .get(usize::try_from(link).unwrap())
                .cloned()
                .unwrap_or(Err(BackendError::Other("INVALID_ARGUMENT".to_owned())))
        })
    }

    fn performance_state(&self, uuid: &str) -> BackendResult<u32> {
        self.query("performance_state", uuid, |device| {
            device.performance_state.clone()
        })
    }

    fn throttle_reasons(&self, uuid: &str) -> BackendResult<u64> {
        self.query("throttle_reasons", uuid, |device| device.throttle.clone())
    }

    fn pcie_link(&self, uuid: &str, link: PcieLink) -> BackendResult<u32> {
        self.query("pcie_link", uuid, |device| {
            device.pcie_link[usize::from(link == PcieLink::Width)].clone()
        })
    }

    fn health_value(&self, uuid: &str, value: HealthValue) -> BackendResult<u64> {
        self.query("health_value", uuid, |device| {
            device
                .health
                .get(&value)
                .cloned()
                .unwrap_or(Err(BackendError::NotSupported))
        })
    }
}

impl GpuProcessQueries for FakeGpu {
    fn compute_processes(&self, uuid: &str) -> BackendResult<Vec<GpuProcessEntry>> {
        self.query("compute_processes", uuid, |device| device.compute.clone())
    }

    fn graphics_processes(&self, uuid: &str) -> BackendResult<Vec<GpuProcessEntry>> {
        self.query("graphics_processes", uuid, |device| device.graphics.clone())
    }

    fn process_utilization(
        &self,
        uuid: &str,
        since_timestamp_us: u64,
    ) -> BackendResult<Vec<ProcessUtilizationSample>> {
        self.lock()
            .utilization_since
            .push((uuid.to_owned(), since_timestamp_us));
        self.query("process_utilization", uuid, |device| {
            device.samples.clone().map(|samples| {
                samples
                    .into_iter()
                    .filter(|sample| sample.timestamp_us > since_timestamp_us)
                    .collect()
            })
        })
    }
}

struct FakeConnector {
    gpu: FakeGpu,
    failure: Arc<Mutex<Option<ConnectError>>>,
    connects: Arc<AtomicU64>,
}

impl BackendConnector for FakeConnector {
    fn connect(&mut self) -> Result<Box<dyn GpuBackend>, ConnectError> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        if let Some(failure) = self
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Err(failure);
        }
        self.gpu.lock().uninitialized = false;
        Ok(Box::new(self.gpu.clone()))
    }
}

// ---------------------------------------------------------------- harness

struct Harness {
    adapter: NvmlAdapter,
    clock: Arc<FakeClock>,
    gpu: FakeGpu,
    failure: Arc<Mutex<Option<ConnectError>>>,
    connects: Arc<AtomicU64>,
    generation: u32,
    cycle: u64,
}

impl Harness {
    fn new(devices: Vec<FakeDevice>) -> Self {
        Self::with_failure(devices, None)
    }

    fn with_failure(devices: Vec<FakeDevice>, failure: Option<ConnectError>) -> Self {
        let clock = FakeClock::new(1_000_000_000);
        let gpu = FakeGpu::new(devices);
        let failure = Arc::new(Mutex::new(failure));
        let connects = Arc::new(AtomicU64::new(0));
        let connector = FakeConnector {
            gpu: gpu.clone(),
            failure: Arc::clone(&failure),
            connects: Arc::clone(&connects),
        };
        let shared_clock: Arc<dyn TimeProvider> = clock.clone();
        Self {
            adapter: NvmlAdapter::with_connector(
                shared_clock,
                host_id("test-host"),
                Box::new(connector),
            ),
            clock,
            gpu,
            failure,
            connects,
            generation: 0,
            cycle: 0,
        }
    }

    fn discover_page(&mut self, page: u32) -> CatalogDelta {
        let request = DiscoverRequest {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id("nvml"),
            host_generation: 1,
            catalog_revision: 1,
            config_revision: 1,
            deadline_boot_ns: self.clock.now_boot_ns() + 5_000_000_000,
            previous_generation: self.generation,
            page,
        };
        self.adapter.discover(&request)
    }

    /// Full discovery (all pages); the parent then holds the new generation.
    fn discover(&mut self) -> CatalogDelta {
        let mut merged = self.discover_page(0);
        merged.validate(256).unwrap();
        let mut next = merged.next_page;
        while let Some(page) = next {
            let delta = self.discover_page(page);
            delta.validate(256).unwrap();
            merged.entities.extend(delta.entities);
            merged.sources.extend(delta.sources);
            merged.removed_sources = delta.removed_sources;
            merged.removed_entities = delta.removed_entities;
            merged.capabilities = delta.capabilities;
            merged.complete = delta.complete;
            next = delta.next_page;
        }
        self.generation = merged.generation;
        merged
    }

    fn plan(&mut self, group: &str, page: u32, slots: Vec<u32>) -> ReadPlan {
        ReadPlan {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id("nvml"),
            host_generation: 1,
            catalog_revision: 1,
            config_revision: 1,
            cycle_id: self.cycle,
            read_group: read_group(group),
            source_slots: slots,
            deadline_boot_ns: self.clock.now_boot_ns() + 1_000_000_000,
            epoch: 1,
            page,
        }
    }

    fn read(&mut self, group: &str) -> ObservationBatch {
        self.cycle += 1;
        let plan = self.plan(group, 0, Vec::new());
        let batch = self.adapter.read(&plan);
        batch.validate_against(&plan, 256).unwrap();
        batch
    }
}

fn find<'a>(
    records: &'a [Observation],
    entity: &str,
    metric: &str,
    scope: &str,
) -> &'a Observation {
    records
        .iter()
        .find(|record| {
            record.key.entity_id.as_str() == entity
                && record.key.metric_id.as_str() == metric
                && record.key.scope.as_str() == scope
        })
        .unwrap_or_else(|| panic!("no record {entity} {metric} {scope}"))
}

fn capability<'a>(
    delta: &'a CatalogDelta,
    entity: &str,
    metric: &str,
) -> &'a dgxtop_core::observation::Capability {
    delta
        .capabilities
        .iter()
        .find(|capability| {
            capability.capability_id == metric
                && capability.entity_id.as_ref().map(|id| id.as_str()) == Some(entity)
        })
        .unwrap_or_else(|| panic!("no capability {entity} {metric}"))
}

fn assert_never_zero_substitute(records: &[Observation]) {
    for record in records {
        assert_eq!(
            record.read_status.is_ok(),
            record.value.is_some(),
            "value must exist exactly for ok reads: {record:?}"
        );
        record.validate().unwrap();
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn healthy_gpu_reports_native_values_with_uuid_identity() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    let delta = harness.discover();
    assert_eq!(delta.availability, Some(Availability::Available));
    assert!(delta.complete);
    let entity = &delta.entities[0];
    assert_eq!(entity.entity_id.as_str(), "gpu:GPU-a");
    assert_eq!(entity.display_name, "Fake GPU GPU-a");
    assert_eq!(entity.attribute("uuid"), Some("GPU-a"));
    assert_eq!(entity.attribute("nvml_index"), Some("0"));
    assert_eq!(entity.attribute("pci_bus_id"), Some("00000000:01:00.0"));
    assert_eq!(entity.attribute("driver_version"), Some("595.12"));
    assert_eq!(entity.attribute("serial"), Some("SER-1"));
    assert_eq!(entity.attribute("memory_model"), Some("framebuffer"));
    assert_eq!(entity.attribute("compute_mode"), Some("default"));
    assert_eq!(entity.attribute("persistence_mode"), Some("enabled"));
    assert!(
        delta
            .sources
            .iter()
            .all(|source| source.source_id.as_str().starts_with("nvml:GPU-a:"))
    );

    let fast = harness.read("fast");
    assert_eq!(fast.status, BatchStatus::Ok);
    let power = find(&fast.records, "gpu:GPU-a", ids::GPU_POWER_DRAW, "gpu");
    assert_eq!(power.native_unit.as_str(), native::MILLI_WATT);
    assert_eq!(power.value.unwrap().as_u64(), Some(250_000));
    assert_eq!(power.quality, Quality::Usable);
    let instant = find(&fast.records, "gpu:GPU-a", ids::GPU_POWER_INSTANT, "gpu");
    assert_eq!(
        (
            instant.native_unit.as_str(),
            instant.value.unwrap().as_u64()
        ),
        (native::MILLI_WATT, Some(262_500))
    );
    let temperature = find(&fast.records, "gpu:GPU-a", ids::GPU_TEMPERATURE, "gpu");
    assert_eq!(temperature.native_unit.as_str(), native::DEG_C);
    assert_eq!(temperature.value.unwrap().as_u64(), Some(61));
    let busy = find(
        &fast.records,
        "gpu:GPU-a",
        ids::GPU_UTILIZATION_MEMORY_BUSY,
        "gpu",
    );
    assert_eq!(busy.native_unit.as_str(), native::PERCENT);
    let fan = find(&fast.records, "gpu:GPU-a", ids::GPU_FAN_SPEED, "fan:0");
    assert_eq!(fan.value.unwrap().as_u64(), Some(44));
    // One utilization call serves both utilization channels.
    assert_eq!(harness.gpu.device_calls("utilization", "GPU-a"), 1);

    let slow = harness.read("slow_health");
    let energy = find(&slow.records, "gpu:GPU-a", ids::GPU_ENERGY_TOTAL, "gpu");
    assert_eq!(energy.native_unit.as_str(), native::MILLI_JOULE);
    assert_eq!(energy.value.unwrap().as_u64(), Some(987_654_321));
    for scope in ["volatile", "aggregate"] {
        let ecc = find(&slow.records, "gpu:GPU-a", ids::GPU_ECC_CORRECTED, scope);
        assert_eq!(ecc.value.unwrap().as_u64(), Some(2));
        find(&slow.records, "gpu:GPU-a", ids::GPU_ECC_UNCORRECTED, scope);
    }
    let limit = find(&slow.records, "gpu:GPU-a", ids::GPU_POWER_LIMIT, "gpu");
    assert_eq!(limit.value.unwrap().as_u64(), Some(360_000));
    for scope in ["graphics", "memory", "sm", "video"] {
        find(&slow.records, "gpu:GPU-a", ids::GPU_CLOCK_MAXIMUM, scope);
    }
    find(&slow.records, "gpu:GPU-a", ids::GPU_MEMORY_FB_TOTAL, "gpu");
    find(
        &slow.records,
        "gpu:GPU-a",
        ids::GPU_MEMORY_FB_RESERVED,
        "gpu",
    );
    assert_never_zero_substitute(&fast.records);
    assert_never_zero_substitute(&slow.records);
}

#[test]
fn performance_state_throttle_reasons_link_and_thresholds_are_reported() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    harness.discover();
    let fast = harness.read("fast");
    let state = find(
        &fast.records,
        "gpu:GPU-a",
        ids::GPU_PERFORMANCE_STATE,
        "gpu",
    );
    assert_eq!(state.value.unwrap().as_u64(), Some(2));
    assert_eq!(state.native_unit.as_str(), native::COUNT);
    let throttle = find(&fast.records, "gpu:GPU-a", ids::GPU_THROTTLE_REASONS, "gpu");
    assert_eq!(throttle.value.unwrap().as_u64(), Some(0x4));
    let slow = harness.read("slow_health");
    let link = |metric: &str, scope: &str| {
        find(&slow.records, "gpu:GPU-a", metric, scope)
            .value
            .unwrap()
            .as_u64()
    };
    assert_eq!(link(ids::GPU_PCIE_LINK_GENERATION, "gpu"), Some(4));
    assert_eq!(link(ids::GPU_PCIE_LINK_WIDTH, "gpu"), Some(16));
    assert_eq!(link(ids::GPU_PCIE_LINK_MAXIMUM, "generation"), Some(5));
    assert_eq!(link(ids::GPU_PCIE_LINK_MAXIMUM, "width"), Some(16));
    let slowdown = find(
        &slow.records,
        "gpu:GPU-a",
        ids::GPU_TEMPERATURE_THRESHOLD,
        "slowdown",
    );
    assert_eq!(slowdown.native_unit.as_str(), native::DEG_C);
    assert_eq!(slowdown.value.unwrap().as_u64(), Some(90));
    assert_eq!(
        link(ids::GPU_TEMPERATURE_THRESHOLD, "shutdown"),
        Some(93),
        "the device's own threshold, never a fixed number"
    );
    // Thresholds and link maxima are read once per generation, not per read.
    harness.read("slow_health");
    assert_eq!(
        harness.gpu.device_calls("temperature_threshold", "GPU-a"),
        4
    );
    assert_eq!(harness.gpu.device_calls("max_pcie_link", "GPU-a"), 2);
    assert_eq!(harness.gpu.device_calls("pcie_link", "GPU-a"), 4);
}

#[test]
fn capabilities_and_slow_health_values_are_reported_in_native_units() {
    let mut device = FakeDevice::healthy("GPU-a");
    device
        .health
        .insert(HealthValue::PcieReplays, Err(BackendError::NotSupported));
    let mut harness = Harness::new(vec![device]);
    harness.discover();
    let slow = harness.read("slow_health");
    let value = |metric: &str, scope: &str| {
        let record = find(&slow.records, "gpu:GPU-a", metric, scope);
        (
            record.value.and_then(|value| value.as_u64()),
            record.native_unit.as_str().to_owned(),
        )
    };
    assert_eq!(
        value(ids::GPU_CORES, "gpu"),
        (Some(10_752), "count".to_owned())
    );
    assert_eq!(value(ids::GPU_MEMORY_BUS_WIDTH, "gpu").0, Some(256));
    assert_eq!(value(ids::GPU_MEMORY_BAR1_TOTAL, "gpu").0, Some(16 << 30));
    assert_eq!(value(ids::GPU_MEMORY_BAR1_USED, "gpu").0, Some(6 << 20));
    assert_eq!(
        value(ids::GPU_POWER_LIMIT_MAXIMUM, "gpu"),
        (Some(390_000), native::MILLI_WATT.to_owned())
    );
    assert_eq!(value(ids::GPU_POWER_LIMIT_DEFAULT, "gpu").0, Some(360_000));
    assert_eq!(
        value(ids::GPU_POWER_LIMIT_REQUESTED, "gpu"),
        (Some(380_000), native::MILLI_WATT.to_owned())
    );
    assert_eq!(value(ids::GPU_ENCODER_SESSIONS, "gpu").0, Some(1));
    assert_eq!(
        value(ids::GPU_ENCODER_LATENCY, "gpu"),
        (Some(2_500), native::MICROSECONDS.to_owned())
    );
    assert_eq!(
        value(ids::GPU_THROTTLE_TIME, "power"),
        (Some(219_152_032), native::NANOSECONDS.to_owned())
    );
    assert_eq!(value(ids::GPU_THROTTLE_TIME, "thermal").0, Some(0));
    assert_eq!(
        value(ids::GPU_TEMPERATURE_THRESHOLD, "max_operating").0,
        Some(88)
    );
    let target = find(
        &slow.records,
        "gpu:GPU-a",
        ids::GPU_TEMPERATURE_THRESHOLD,
        "target",
    );
    assert_eq!(target.read_status, ReadStatus::Unsupported);
    let replays = find(&slow.records, "gpu:GPU-a", ids::GPU_PCIE_REPLAYS, "gpu");
    assert_eq!(
        (replays.read_status, replays.value),
        (ReadStatus::Unsupported, None)
    );
    // Capabilities are read once per generation; health values every read,
    // except the unsupported replay counter, re-probed once a minute.
    harness.read("slow_health");
    assert_eq!(harness.gpu.device_calls("static_fact", "GPU-a"), 7);
    assert_eq!(harness.gpu.device_calls("health_value", "GPU-a"), 10 + 9);
}

#[test]
fn topology_names_the_nearest_cpus_and_the_path_to_each_other_gpu() {
    let mut second = FakeDevice::healthy("GPU-b");
    second.peer_link = Err(BackendError::NotSupported);
    second.cpu_affinity = Err(BackendError::NotSupported);
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a"), second]);
    let delta = harness.discover();
    let topology = |entity: &str| {
        let record = delta
            .entities
            .iter()
            .find(|record| record.entity_id.as_str() == entity)
            .unwrap();
        (
            record.attribute("cpu_affinity"),
            record.attribute("numa_nodes"),
            record.attribute("gpu_peers"),
        )
    };
    assert_eq!(
        topology("gpu:GPU-a"),
        (Some("0-31"), Some("0"), Some("1:PHB:none"))
    );
    // What the driver cannot answer stays unknown, never guessed.
    assert_eq!(topology("gpu:GPU-b"), (None, Some("0"), Some("0:?:none")));
}

/// GB10 reaches the CPU over NVLink-C2C: its links and the bandwidth of a
/// link are GPU attributes; a GPU without such a link has none.
#[test]
fn chip_to_chip_links_are_attributes_of_the_gpu() {
    let mut unified = FakeDevice::healthy("GPU-a");
    unified.c2c = Ok(C2cLinks {
        count: 8,
        link_max_mb_per_s: Some(43_125),
    });
    let mut idle = FakeDevice::healthy("GPU-b");
    idle.c2c = Ok(C2cLinks {
        count: 0,
        link_max_mb_per_s: None,
    });
    let mut harness = Harness::new(vec![unified, idle, FakeDevice::healthy("GPU-c")]);
    let delta = harness.discover();
    let c2c = |entity: &str| {
        let record = delta
            .entities
            .iter()
            .find(|record| record.entity_id.as_str() == entity)
            .unwrap();
        (
            record.attribute("c2c_links").map(str::to_owned),
            record.attribute("c2c_link_bandwidth").map(str::to_owned),
        )
    };
    assert_eq!(
        c2c("gpu:GPU-a"),
        (Some("8".to_owned()), Some("43125000000".to_owned()))
    );
    // No active link, or a driver that cannot tell: no attribute.
    assert_eq!(c2c("gpu:GPU-b"), (None, None));
    assert_eq!(c2c("gpu:GPU-c"), (None, None));
}

#[test]
fn unsupported_metrics_are_unsupported_records_never_zero() {
    let mut device = FakeDevice::healthy("GPU-a");
    device.temperature = Err(BackendError::NotSupported);
    device.fans = Ok(0);
    device.energy = Err(BackendError::NotSupported);
    device.ecc = Err(BackendError::NotSupported);
    device.retired = Err(BackendError::NoPermission);
    let mut harness = Harness::new(vec![device]);
    harness.discover();
    let fast = harness.read("fast");
    let temperature = find(&fast.records, "gpu:GPU-a", ids::GPU_TEMPERATURE, "gpu");
    assert_eq!(temperature.read_status, ReadStatus::Unsupported);
    assert_eq!(temperature.value, None);
    assert_eq!(
        temperature.reason_codes,
        vec!["NVML_NOT_SUPPORTED".to_owned()]
    );
    assert!(
        fast.records
            .iter()
            .all(|record| record.key.metric_id.as_str() != ids::GPU_FAN_SPEED),
        "a GPU without fans must not report a fan value"
    );
    let slow = harness.read("slow_health");
    let energy = find(&slow.records, "gpu:GPU-a", ids::GPU_ENERGY_TOTAL, "gpu");
    assert_eq!(energy.read_status, ReadStatus::Unsupported);
    for scope in ["volatile", "aggregate"] {
        let ecc = find(&slow.records, "gpu:GPU-a", ids::GPU_ECC_UNCORRECTED, scope);
        assert_eq!(
            ecc.read_status,
            ReadStatus::Unsupported,
            "never 'no errors'"
        );
        assert_eq!(ecc.value, None);
    }
    let retired = find(
        &slow.records,
        "gpu:GPU-a",
        ids::GPU_RETIRED_PAGES_DOUBLE,
        "gpu",
    );
    assert_eq!(retired.read_status, ReadStatus::PermissionDenied);
    assert_eq!(fast.status, BatchStatus::Partial);
    assert_never_zero_substitute(&fast.records);
    assert_never_zero_substitute(&slow.records);

    let delta = harness.discover();
    let fan = capability(&delta, "gpu:GPU-a", ids::GPU_FAN_SPEED);
    assert_eq!(fan.support, Support::Unsupported);
    assert_eq!(fan.reason.as_deref(), Some("NVML_NO_FAN"));
    assert_eq!(
        capability(&delta, "gpu:GPU-a", ids::GPU_TEMPERATURE).support,
        Support::Unsupported
    );
    assert_eq!(
        capability(&delta, "gpu:GPU-a", ids::GPU_POWER_DRAW).support,
        Support::Supported
    );
}

#[test]
fn unified_memory_gpu_never_substitutes_system_ram() {
    let mut device = FakeDevice::healthy("GPU-gb10");
    device.framebuffer = Err(BackendError::NotSupported);
    let mut harness = Harness::new(vec![device]);
    let delta = harness.discover();
    assert_eq!(delta.entities[0].attribute("memory_model"), Some("unified"));
    assert_eq!(
        capability(&delta, "gpu:GPU-gb10", ids::GPU_MEMORY_FB_USED).support,
        Support::Unsupported
    );
    let fast = harness.read("fast");
    for metric in [ids::GPU_MEMORY_FB_USED, ids::GPU_MEMORY_FB_FREE] {
        let record = find(&fast.records, "gpu:GPU-gb10", metric, "gpu");
        assert_eq!(record.read_status, ReadStatus::Unsupported);
        assert_eq!(record.value, None);
        assert_eq!(
            record.reason_codes,
            vec!["NVML_UNIFIED_MEMORY_NO_FRAMEBUFFER".to_owned()]
        );
    }
    let slow = harness.read("slow_health");
    for metric in [ids::GPU_MEMORY_FB_TOTAL, ids::GPU_MEMORY_FB_RESERVED] {
        let record = find(&slow.records, "gpu:GPU-gb10", metric, "gpu");
        assert_eq!(record.read_status, ReadStatus::Unsupported);
        assert_eq!(record.value, None);
    }
    // Other metrics keep working on the unified device.
    let power = find(&fast.records, "gpu:GPU-gb10", ids::GPU_POWER_DRAW, "gpu");
    assert!(power.read_status.is_ok());
}

#[test]
fn two_gpus_with_the_same_pid_are_two_associations() {
    let mut first = FakeDevice::healthy("GPU-a");
    first.compute = Ok(vec![GpuProcessEntry {
        pid: 4242,
        used_memory_bytes: Some(1 << 20),
    }]);
    let mut second = FakeDevice::healthy("GPU-b");
    second.compute = Ok(vec![GpuProcessEntry {
        pid: 4242,
        used_memory_bytes: Some(2 << 20),
    }]);
    let mut harness = Harness::new(vec![first, second]);
    harness.discover();
    let batch = harness.read("process");
    let on_a = find(
        &batch.records,
        "gpupid:GPU-a:4242",
        ids::PROCESS_GPU_MEMORY,
        "gpu:GPU-a",
    );
    let on_b = find(
        &batch.records,
        "gpupid:GPU-b:4242",
        ids::PROCESS_GPU_MEMORY,
        "gpu:GPU-b",
    );
    assert_eq!(on_a.value.unwrap().as_u64(), Some(1 << 20));
    assert_eq!(on_b.value.unwrap().as_u64(), Some(2 << 20));
    assert_eq!(on_a.native_unit.as_str(), native::BYTES);
    assert_eq!(
        on_a.provenance.source_id.as_str(),
        "nvml:GPU-a:process.memory_allocated"
    );
}

/// A GPU whose process lists are both read and name no process says its
/// processes hold nothing (D-119): one 0-byte reading of the GPU's process
/// memory; a graphics list the device does not support counts as empty. A
/// GPU listing a process gets none (the collector sums what the lists
/// name), and one whose list failed a failed reading, never 0.
#[test]
fn an_empty_process_list_reads_as_no_process_memory() {
    let idle = FakeDevice::healthy("GPU-idle");
    let mut compute_only = FakeDevice::healthy("GPU-compute");
    compute_only.graphics = Err(BackendError::NotSupported);
    let mut busy = FakeDevice::healthy("GPU-busy");
    busy.compute = Ok(vec![GpuProcessEntry {
        pid: 7,
        used_memory_bytes: Some(1 << 20),
    }]);
    let mut failed = FakeDevice::healthy("GPU-failed");
    failed.graphics = Err(BackendError::Other("NVML_ERROR_UNKNOWN".to_owned()));
    // Only a PID no process key can hold: something holds memory unseen.
    let mut unkeyable = FakeDevice::healthy("GPU-unkeyable");
    unkeyable.compute = Ok(vec![GpuProcessEntry {
        pid: 0,
        used_memory_bytes: Some(1 << 20),
    }]);
    let mut harness = Harness::new(vec![idle, compute_only, busy, failed, unkeyable]);
    harness.discover();
    let batch = harness.read("process");
    let held = |gpu: &str| -> Vec<&Observation> {
        batch
            .records
            .iter()
            .filter(|record| {
                record.key.entity_id.as_str() == gpu
                    && record.key.metric_id.as_str() == ids::GPU_MEMORY_PROCESS_ALLOCATED
            })
            .collect()
    };
    let idle = held("gpu:GPU-idle");
    assert_eq!(idle.len(), 1, "{:?}", batch.records);
    assert_eq!(idle[0].value.unwrap().as_u64(), Some(0));
    assert!(idle[0].read_status.is_ok());
    assert_eq!(idle[0].key.scope.as_str(), "gpu");
    assert_eq!(idle[0].native_unit.as_str(), native::BYTES);
    assert_eq!(
        idle[0].provenance.source_id.as_str(),
        "nvml:GPU-idle:memory.process_allocated"
    );
    let compute_only = held("gpu:GPU-compute");
    assert_eq!(compute_only.len(), 1);
    assert_eq!(compute_only[0].value.unwrap().as_u64(), Some(0));
    assert!(held("gpu:GPU-busy").is_empty());
    for gpu in ["gpu:GPU-failed", "gpu:GPU-unkeyable"] {
        let failed = held(gpu);
        assert_eq!(failed.len(), 1, "{gpu}: {:?}", batch.records);
        assert!(!failed[0].read_status.is_ok(), "{gpu}");
        assert_eq!(failed[0].value, None, "{gpu}");
    }
    assert_never_zero_substitute(&batch.records);
    // The lists' support is the reading's, as the catalog says it.
    let delta = harness.discover();
    assert_eq!(
        capability(&delta, "gpu:GPU-idle", ids::GPU_MEMORY_PROCESS_ALLOCATED).support,
        Support::Supported
    );
    // A graphics list the device does not support leaves the read whole.
    let mut alone = FakeDevice::healthy("GPU-compute");
    alone.graphics = Err(BackendError::NotSupported);
    let mut harness = Harness::new(vec![alone]);
    harness.discover();
    assert_eq!(harness.read("process").status, BatchStatus::Ok);
}

#[test]
fn compute_and_graphics_of_one_pid_are_one_association() {
    let mut device = FakeDevice::healthy("GPU-a");
    let entry = GpuProcessEntry {
        pid: 77,
        used_memory_bytes: Some(512 << 20),
    };
    device.compute = Ok(vec![entry]);
    device.graphics = Ok(vec![entry]);
    let mut harness = Harness::new(vec![device]);
    harness.discover();
    let batch = harness.read("process");
    let memory: Vec<&Observation> = batch
        .records
        .iter()
        .filter(|record| record.key.metric_id.as_str() == ids::PROCESS_GPU_MEMORY)
        .collect();
    assert_eq!(
        memory.len(),
        1,
        "one association, memory not double counted"
    );
    assert_eq!(memory[0].value.unwrap().as_u64(), Some(512 << 20));
    assert!(
        memory[0]
            .reason_codes
            .contains(&"gpu_usage:compute_and_graphics".to_owned())
    );
}

#[test]
fn unavailable_process_memory_is_unsupported_not_zero() {
    let mut device = FakeDevice::healthy("GPU-a");
    device.compute = Ok(vec![GpuProcessEntry {
        pid: 9,
        used_memory_bytes: None,
    }]);
    let mut harness = Harness::new(vec![device]);
    harness.discover();
    let batch = harness.read("process");
    let memory = find(
        &batch.records,
        "gpupid:GPU-a:9",
        ids::PROCESS_GPU_MEMORY,
        "gpu:GPU-a",
    );
    assert_eq!(memory.read_status, ReadStatus::Unsupported);
    assert_eq!(memory.value, None);
    assert!(
        memory
            .reason_codes
            .contains(&"gpu_usage:compute".to_owned())
    );
    assert_never_zero_substitute(&batch.records);
}

#[test]
fn process_utilization_uses_samples_since_the_last_timestamp() {
    let mut device = FakeDevice::healthy("GPU-a");
    device.compute = Ok(vec![
        GpuProcessEntry {
            pid: 1000,
            used_memory_bytes: Some(1),
        },
        GpuProcessEntry {
            pid: 1001,
            used_memory_bytes: Some(1),
        },
    ]);
    device.samples = Ok(vec![
        ProcessUtilizationSample {
            pid: 1000,
            timestamp_us: 100,
            sm_percent: 10,
        },
        ProcessUtilizationSample {
            pid: 1000,
            timestamp_us: 200,
            sm_percent: 65,
        },
    ]);
    let mut harness = Harness::new(vec![device]);
    harness.discover();
    let first = harness.read("process");
    let utilization = find(
        &first.records,
        "gpupid:GPU-a:1000",
        ids::PROCESS_GPU_UTILIZATION,
        "gpu:GPU-a",
    );
    assert_eq!(utilization.value.unwrap().as_u64(), Some(65));
    assert_eq!(utilization.native_unit.as_str(), native::PERCENT);
    // PID 1001 had no sample: no utilization record (not a fake 0 %).
    assert!(!first.records.iter().any(|record| {
        record.key.entity_id.as_str() == "gpupid:GPU-a:1001"
            && record.key.metric_id.as_str() == ids::PROCESS_GPU_UTILIZATION
    }));
    harness.read("process");
    let since: Vec<u64> = harness
        .gpu
        .lock()
        .utilization_since
        .iter()
        .map(|(_, since)| *since)
        .collect();
    assert_eq!(since, vec![0, 200]);
}

#[test]
fn unsupported_process_utilization_is_reported_per_association() {
    let mut device = FakeDevice::healthy("GPU-a");
    device.compute = Ok(vec![GpuProcessEntry {
        pid: 5,
        used_memory_bytes: Some(3),
    }]);
    device.samples = Err(BackendError::NotSupported);
    let mut harness = Harness::new(vec![device]);
    harness.discover();
    let batch = harness.read("process");
    let utilization = find(
        &batch.records,
        "gpupid:GPU-a:5",
        ids::PROCESS_GPU_UTILIZATION,
        "gpu:GPU-a",
    );
    assert_eq!(utilization.read_status, ReadStatus::Unsupported);
    assert_eq!(utilization.value, None);
}

#[test]
fn lost_gpu_only_affects_its_own_records() {
    let mut harness = Harness::new(vec![
        FakeDevice::healthy("GPU-a"),
        FakeDevice::healthy("GPU-b"),
    ]);
    harness.discover();
    harness.gpu.edit("GPU-a", |device| device.lost = true);
    let fast = harness.read("fast");
    let lost: Vec<&Observation> = fast
        .records
        .iter()
        .filter(|record| record.key.entity_id.as_str() == "gpu:GPU-a")
        .collect();
    assert!(!lost.is_empty());
    assert!(
        lost.iter()
            .all(|record| record.read_status == ReadStatus::Removed)
    );
    assert!(lost.iter().all(|record| record.value.is_none()));
    let healthy = find(&fast.records, "gpu:GPU-b", ids::GPU_POWER_DRAW, "gpu");
    assert_eq!(healthy.value.unwrap().as_u64(), Some(250_000));
    // The lost GPU is not queried again until the next discovery.
    let calls_after_loss = harness.gpu.total_calls_for("GPU-a");
    let again = harness.read("fast");
    assert_eq!(harness.gpu.total_calls_for("GPU-a"), calls_after_loss);
    assert!(
        find(&again.records, "gpu:GPU-b", ids::GPU_TEMPERATURE, "gpu")
            .read_status
            .is_ok()
    );
    let removed = find(&again.records, "gpu:GPU-a", ids::GPU_TEMPERATURE, "gpu");
    assert_eq!(removed.read_status, ReadStatus::Removed);
}

#[test]
fn init_failure_reports_unavailable_with_every_capability_unsupported() {
    let mut harness = Harness::with_failure(
        Vec::new(),
        Some(ConnectError::DriverMissing("LIBRARY_NOT_FOUND".to_owned())),
    );
    let delta = harness.discover();
    assert_eq!(delta.availability, Some(Availability::Unavailable));
    assert!(delta.complete);
    assert!(delta.entities.is_empty() && delta.sources.is_empty());
    assert!(!delta.capabilities.is_empty());
    assert!(delta.capabilities.iter().all(|capability| {
        capability.support == Support::Unsupported
            && capability
                .reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("NVML_DRIVER_MISSING"))
    }));
    let batch = harness.read("fast");
    assert_eq!(batch.status, BatchStatus::Failed);
    assert!(batch.records.is_empty());
}

#[test]
fn permission_denied_init_is_reported_as_such() {
    let mut harness = Harness::with_failure(Vec::new(), Some(ConnectError::NoPermission));
    let delta = harness.discover();
    assert_eq!(delta.availability, Some(Availability::PermissionDenied));
}

#[test]
fn no_gpu_reports_unavailable() {
    let mut harness = Harness::new(Vec::new());
    let delta = harness.discover();
    assert_eq!(delta.availability, Some(Availability::Unavailable));
    assert_eq!(delta.error_code.as_deref(), Some("NVML_NO_GPU"));
    assert!(
        delta
            .capabilities
            .iter()
            .all(|capability| capability.support == Support::Unsupported)
    );
}

#[test]
fn reinit_attempts_are_throttled_to_once_per_minute() {
    let mut harness = Harness::with_failure(Vec::new(), Some(ConnectError::NoGpu));
    harness.discover();
    assert_eq!(harness.connects.load(Ordering::SeqCst), 1);
    harness.clock.advance_ms(30 * SECOND_MS);
    harness.discover();
    harness.clock.advance_ms(29 * SECOND_MS);
    harness.discover();
    assert_eq!(harness.connects.load(Ordering::SeqCst), 1);
    harness.clock.advance_ms(SECOND_MS);
    *harness.failure.lock().unwrap() = None;
    harness
        .gpu
        .lock()
        .devices
        .push(FakeDevice::healthy("GPU-late"));
    let delta = harness.discover();
    assert_eq!(harness.connects.load(Ordering::SeqCst), 2);
    assert_eq!(delta.availability, Some(Availability::Available));
    assert_eq!(delta.entities[0].entity_id.as_str(), "gpu:GPU-late");
}

#[test]
fn metadata_is_cached_until_the_generation_changes() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    let first = harness.discover();
    for _ in 0..5 {
        harness.clock.advance_ms(SECOND_MS);
        harness.read("fast");
        harness.read("slow_health");
        harness.read("process");
    }
    let unchanged = harness.discover();
    assert_eq!(unchanged.generation, first.generation);
    assert!(
        unchanged.entities.is_empty(),
        "parent already holds this generation"
    );
    assert_eq!(harness.gpu.device_calls("device_details", "GPU-a"), 1);
    assert_eq!(harness.gpu.device_calls("power_limit", "GPU-a"), 1);
    assert_eq!(harness.gpu.device_calls("max_clock", "GPU-a"), 4);
    assert_eq!(harness.gpu.device_calls("fan_count", "GPU-a"), 1);

    harness
        .gpu
        .lock()
        .devices
        .push(FakeDevice::healthy("GPU-b"));
    let changed = harness.discover();
    assert_eq!(changed.generation, first.generation + 1);
    assert_eq!(changed.entities.len(), 2);
    // A new generation re-reads metadata of every device.
    assert_eq!(harness.gpu.device_calls("device_details", "GPU-a"), 2);
    assert_eq!(harness.gpu.device_calls("device_details", "GPU-b"), 1);
}

#[test]
fn driver_reload_starts_a_new_generation() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    let first = harness.discover();
    harness.gpu.lock().driver = "596.01".to_owned();
    let reloaded = harness.discover();
    assert_eq!(reloaded.generation, first.generation + 1);
    assert_eq!(
        reloaded.entities[0].attribute("driver_version"),
        Some("596.01")
    );
    assert_eq!(harness.gpu.device_calls("device_details", "GPU-a"), 2);
}

#[test]
fn unsupported_capability_is_reprobed_at_most_once_per_minute() {
    let mut device = FakeDevice::healthy("GPU-a");
    device.fan_speed = Err(BackendError::NotSupported);
    let mut harness = Harness::new(vec![device]);
    harness.discover();
    harness.read("fast");
    assert_eq!(harness.gpu.device_calls("fan_speed", "GPU-a"), 1);
    for _ in 0..59 {
        harness.clock.advance_ms(SECOND_MS);
        let batch = harness.read("fast");
        let fan = find(&batch.records, "gpu:GPU-a", ids::GPU_FAN_SPEED, "fan:0");
        assert_eq!(fan.read_status, ReadStatus::Unsupported);
        assert_eq!(fan.value, None);
    }
    assert_eq!(harness.gpu.device_calls("fan_speed", "GPU-a"), 1);
    harness.clock.advance_ms(SECOND_MS);
    harness
        .gpu
        .edit("GPU-a", |device| device.fan_speed = Ok(30));
    let recovered = harness.read("fast");
    assert_eq!(harness.gpu.device_calls("fan_speed", "GPU-a"), 2);
    let fan = find(&recovered.records, "gpu:GPU-a", ids::GPU_FAN_SPEED, "fan:0");
    assert_eq!(fan.value.unwrap().as_u64(), Some(30));
}

#[test]
fn transient_errors_are_retried_every_cycle() {
    let mut device = FakeDevice::healthy("GPU-a");
    device.temperature = Err(BackendError::Other("TIMEOUT".to_owned()));
    let mut harness = Harness::new(vec![device]);
    harness.discover();
    harness.read("fast");
    harness.clock.advance_ms(SECOND_MS);
    let batch = harness.read("fast");
    assert_eq!(harness.gpu.device_calls("temperature", "GPU-a"), 2);
    let record = find(&batch.records, "gpu:GPU-a", ids::GPU_TEMPERATURE, "gpu");
    assert_eq!(record.read_status, ReadStatus::Error);
    assert_eq!(record.reason_codes, vec!["NVML_ERROR_TIMEOUT".to_owned()]);
}

#[test]
fn paging_serves_one_consistent_snapshot() {
    let devices: Vec<FakeDevice> = (0..16)
        .map(|index| {
            let mut device = FakeDevice::healthy(&format!("GPU-{index:02}"));
            device.fans = Ok(4);
            device
        })
        .collect();
    let mut harness = Harness::new(devices);
    let delta = harness.discover();
    assert_eq!(delta.entities.len(), 16);
    harness.cycle += 1;
    let first_plan = harness.plan("fast", 0, Vec::new());
    let first = harness.adapter.read(&first_plan);
    first.validate_against(&first_plan, 256).unwrap();
    assert_eq!(first.records.len(), 256);
    assert!(first.continues());
    let calls = harness.gpu.calls("temperature");
    let second_plan = harness.plan("fast", 1, Vec::new());
    let second = harness.adapter.read(&second_plan);
    second.validate_against(&second_plan, 256).unwrap();
    assert!(!second.continues());
    // Per GPU: 4 load, instant power, P-state, throttle reasons, 4 clocks,
    // 4 fans, 2 framebuffer, 2 PCIe and the encoder and decoder.
    assert_eq!(first.records.len() + second.records.len(), 16 * 21);
    assert_eq!(
        harness.gpu.calls("temperature"),
        calls,
        "later pages reuse the snapshot"
    );
    // A page of another cycle without its page 0 is refused, not invented.
    harness.cycle += 1;
    let orphan_plan = harness.plan("fast", 1, Vec::new());
    assert_eq!(
        harness.adapter.read(&orphan_plan).status,
        BatchStatus::Failed
    );
}

#[test]
fn enumeration_failure_keeps_the_catalog_and_is_not_available() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    let first = harness.discover();
    harness.gpu.lock().count_error = Some(BackendError::NoPermission);
    let failed = harness.discover_page(0);
    assert!(
        !failed.complete,
        "an incomplete page never commits removals"
    );
    assert!(failed.removed_sources.is_empty() && failed.removed_entities.is_empty());
    assert_eq!(failed.availability, Some(Availability::PermissionDenied));
    assert_eq!(failed.generation, first.generation);
    harness.gpu.lock().count_error = Some(BackendError::Other("TIMEOUT".to_owned()));
    assert_eq!(
        harness.discover_page(0).availability,
        Some(Availability::Unavailable)
    );
    harness.gpu.lock().count_error = None;
    let recovered = harness.discover();
    assert_eq!(
        recovered.generation, first.generation,
        "same devices, same generation"
    );
    assert_eq!(recovered.availability, Some(Availability::Available));
}

#[test]
fn catalog_discovery_is_paged_and_complete_only_at_the_end() {
    let devices: Vec<FakeDevice> = (0..10)
        .map(|index| FakeDevice::healthy(&format!("GPU-{index:02}")))
        .collect();
    let mut harness = Harness::new(devices);
    let mut pages = vec![harness.discover_page(0)];
    while let Some(next) = pages.last().and_then(|page| page.next_page) {
        pages.push(harness.discover_page(next));
    }
    assert!(pages.len() >= 2, "ten GPUs need more than one page");
    let (last, full) = pages.split_last().unwrap();
    for (number, page) in full.iter().enumerate() {
        assert!(!page.complete);
        assert_eq!(page.entities.len() + page.sources.len(), 256);
        assert_eq!(page.next_page, Some(u32::try_from(number + 1).unwrap()));
        assert!(page.capabilities.is_empty() && page.removed_sources.is_empty());
    }
    assert!(last.complete);
    assert!(!last.capabilities.is_empty());
    let mut ids: Vec<&str> = pages
        .iter()
        .flat_map(|page| &page.sources)
        .map(|source| source.source_id.as_str())
        .collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total, "no source appears on two pages");
}

#[test]
fn pcie_throughput_is_marked_unverified() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    harness.discover();
    let batch = harness.read("fast");
    for metric in [ids::GPU_PCIE_TX, ids::GPU_PCIE_RX] {
        let record = find(&batch.records, "gpu:GPU-a", metric, "gpu");
        assert_eq!(
            record.native_unit.as_str(),
            native::KB_UNVERIFIED_PER_SECOND
        );
        assert_eq!(
            record.value.unwrap().as_u64(),
            Some(1234),
            "raw driver number"
        );
        assert_eq!(record.quality, Quality::Suspect);
        assert_eq!(record.reason_codes, vec![PCIE_UNVERIFIED_REASON.to_owned()]);
        assert!(
            normalize(
                record.value.unwrap(),
                record.native_unit.as_str(),
                CanonicalUnit::BytesPerSecond
            )
            .is_err(),
            "the collector cannot convert an unverified KB"
        );
    }
}

#[test]
fn nvlink_counts_active_links_without_throughput() {
    let mut device = FakeDevice::healthy("GPU-a");
    device.nvlinks = vec![
        Ok(true),
        Ok(false),
        Ok(true),
        Err(BackendError::NotSupported),
    ];
    let mut harness = Harness::new(vec![device, FakeDevice::healthy("GPU-b")]);
    harness.discover();
    let batch = harness.read("slow_health");
    let active = find(&batch.records, "gpu:GPU-a", ids::GPU_NVLINK_ACTIVE, "gpu");
    assert_eq!(active.value.unwrap().as_u64(), Some(2));
    let consumer = find(&batch.records, "gpu:GPU-b", ids::GPU_NVLINK_ACTIVE, "gpu");
    assert_eq!(consumer.read_status, ReadStatus::Unsupported);
    assert_eq!(consumer.value, None);
}

#[test]
fn hot_unplug_removes_only_that_gpu_and_keeps_slots_stable() {
    let mut harness = Harness::new(vec![
        FakeDevice::healthy("GPU-a"),
        FakeDevice::healthy("GPU-b"),
    ]);
    let first = harness.discover();
    let slot_of = |delta: &CatalogDelta, id: &str| {
        delta
            .sources
            .iter()
            .find(|source| source.source_id.as_str() == id)
            .map(|source| source.slot)
    };
    let b_temperature = slot_of(&first, "nvml:GPU-b:temperature");
    harness
        .gpu
        .lock()
        .devices
        .retain(|device| device.uuid != "GPU-a");
    let second = harness.discover();
    assert_eq!(second.generation, first.generation + 1);
    assert!(
        second
            .removed_entities
            .iter()
            .any(|entity| entity.as_str() == "gpu:GPU-a")
    );
    assert!(
        second
            .removed_sources
            .iter()
            .any(|source| source.as_str() == "nvml:GPU-a:temperature")
    );
    assert!(
        second
            .sources
            .iter()
            .all(|source| !source.source_id.as_str().contains("GPU-a"))
    );
    assert_eq!(slot_of(&second, "nvml:GPU-b:temperature"), b_temperature);
    // Re-attaching GPU-a revives it with its old slots and no tombstone.
    harness
        .gpu
        .lock()
        .devices
        .push(FakeDevice::healthy("GPU-a"));
    let third = harness.discover();
    assert!(
        !third
            .removed_sources
            .iter()
            .any(|source| source.as_str() == "nvml:GPU-a:temperature")
    );
    assert_eq!(
        slot_of(&third, "nvml:GPU-a:temperature"),
        slot_of(&first, "nvml:GPU-a:temperature")
    );
}

#[test]
fn session_loss_drops_the_backend_and_reconnects_at_once() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    harness.discover();
    harness.gpu.lock().uninitialized = true;
    let batch = harness.read("fast");
    assert_eq!(batch.error_code.as_deref(), Some("NVML_SESSION_LOST"));
    assert!(
        batch
            .records
            .iter()
            .all(|record| !record.read_status.is_ok())
    );
    let delta = harness.discover();
    assert_eq!(harness.connects.load(Ordering::SeqCst), 2);
    assert_eq!(delta.availability, Some(Availability::Available));
}

#[test]
fn expired_deadline_fails_without_driver_calls() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    harness.discover();
    let calls = harness.gpu.calls("temperature");
    harness.cycle += 1;
    let mut plan = harness.plan("fast", 0, Vec::new());
    plan.deadline_boot_ns = harness.clock.now_boot_ns();
    let batch = harness.adapter.read(&plan);
    assert_eq!(batch.status, BatchStatus::Failed);
    assert_eq!(batch.error_code.as_deref(), Some("NVML_DEADLINE_EXPIRED"));
    assert_eq!(harness.gpu.calls("temperature"), calls);
}

#[test]
fn source_slot_filter_reads_only_requested_sources() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    let delta = harness.discover();
    let slot = delta
        .sources
        .iter()
        .find(|source| source.source_id.as_str() == "nvml:GPU-a:temperature")
        .unwrap()
        .slot;
    harness.cycle += 1;
    let plan = harness.plan("fast", 0, vec![slot]);
    let batch = harness.adapter.read(&plan);
    assert_eq!(batch.records.len(), 1);
    assert_eq!(
        batch.records[0].key.metric_id.as_str(),
        ids::GPU_TEMPERATURE
    );
    assert_eq!(harness.gpu.calls("power_draw"), 0);
}

#[test]
fn unknown_read_group_is_rejected() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    harness.discover();
    let batch = harness.read("capacity");
    assert_eq!(batch.status, BatchStatus::Failed);
}

#[test]
fn shutdown_releases_the_backend() {
    let mut harness = Harness::new(vec![FakeDevice::healthy("GPU-a")]);
    harness.discover();
    harness.adapter.shutdown();
    assert_eq!(harness.read("fast").status, BatchStatus::Failed);
}

#[test]
#[ignore = "needs the real NVIDIA driver and GPUs (run with --ignored)"]
fn hardware_nvml_smoke() {
    let clock = FakeClock::new(1_000_000_000);
    let shared_clock: Arc<dyn TimeProvider> = clock.clone();
    let mut adapter = NvmlAdapter::new(shared_clock, host_id("hardware-host"));
    let request = DiscoverRequest {
        request_id: Uuid::new_v4(),
        adapter_id: adapter_id("nvml"),
        host_generation: 1,
        catalog_revision: 1,
        config_revision: 1,
        deadline_boot_ns: clock.now_boot_ns() + 10_000_000_000,
        previous_generation: 0,
        page: 0,
    };
    let delta = adapter.discover(&request);
    assert_eq!(
        delta.availability,
        Some(Availability::Available),
        "{delta:?}"
    );
    assert!(!delta.entities.is_empty(), "at least one gpu entity");
    for entity in &delta.entities {
        eprintln!(
            "entity {} name={:?} attributes={:?}",
            entity.entity_id, entity.display_name, entity.attributes
        );
    }
    for group in ["fast", "slow_health", "process"] {
        let plan = ReadPlan {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id("nvml"),
            host_generation: 1,
            catalog_revision: 1,
            config_revision: 1,
            cycle_id: 1,
            read_group: read_group(group),
            source_slots: Vec::new(),
            deadline_boot_ns: clock.now_boot_ns() + 10_000_000_000,
            epoch: 1,
            page: 0,
        };
        let batch = adapter.read(&plan);
        batch.validate_against(&plan, 256).unwrap();
        for record in &batch.records {
            eprintln!(
                "{group} {} {} {} {:?} {:?} {:?}",
                record.key.entity_id,
                record.key.metric_id,
                record.key.scope,
                record.read_status,
                record.value,
                record.reason_codes
            );
        }
        if group == "fast" {
            for entity in &delta.entities {
                let id = entity.entity_id.as_str();
                for metric in [ids::GPU_TEMPERATURE, ids::GPU_POWER_DRAW] {
                    let record = find(&batch.records, id, metric, "gpu");
                    assert!(record.read_status.is_ok(), "{id} {metric}: {record:?}");
                    assert!(record.value.is_some());
                }
            }
        }
    }
    adapter.shutdown();
}
