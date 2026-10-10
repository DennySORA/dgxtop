//! Replay adapter, fixture validation and REF generator integration tests.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests may unwrap

#[path = "fixtures/adapters_c/support.rs"]
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use dgxtop_adapters::replay::{
    self, FaultKind, FixtureFault, FixtureSample, FixtureSource, FixtureValue, RefProfile,
    ReplayAdapter, ReplayError, ReplayFixture, generate_ref_fixture,
};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::{
    EntityId, MetricId, OriginId, OriginIdentity, OriginRelation, ProfileId, Scope, SourceId,
};
use dgxtop_core::metric::{NativeUnit, ids};
use dgxtop_core::observation::Isolation;
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Namespace, Quality, RawKind, ReadStatus, SampleKind};
use dgxtop_core::testing::adapter_id;
use support::*;

const ADAPTER: &str = "replay";
const DEMO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/replay/demo.json");
const PROCESS_ENTITY: &str = "process:pid:[1]:42:7";

fn fixture_source(
    id: &str,
    entity: &str,
    metric: &str,
    scope: &str,
    unit: &str,
    kind: RawKind,
    group: &str,
) -> FixtureSource {
    FixtureSource {
        source_id: SourceId::new(id).unwrap(),
        entity_id: EntityId::new(entity).unwrap(),
        metric_id: MetricId::new(metric).unwrap(),
        scope: Scope::new(scope).unwrap(),
        native_unit: NativeUnit::new(unit).unwrap(),
        raw_kind: kind,
        sample_kind: SampleKind::Replay,
        read_group: group.to_owned(),
        origin: OriginIdentity::unknown(),
        quality: Quality::Verified,
        profile_id: ProfileId::new("test-profile").unwrap(),
        profile_revision: 1,
        profile_rank: 0,
        driver: None,
        device_path: None,
        channel: None,
        label: None,
        counter_modulus: None,
        update_interval_ms: None,
        rows: false,
    }
}

fn sample(id: &str, value: FixtureValue) -> FixtureSample {
    FixtureSample::value(SourceId::new(id).unwrap(), value)
}

fn row(id: &str, value: u64) -> FixtureSample {
    let mut sample = sample(id, FixtureValue::U64(value));
    sample.entity_id = Some(EntityId::new(PROCESS_ENTITY).unwrap());
    sample.channel = Some("python3".to_owned());
    sample
}

/// Package temperature, a counter, a simulated SoC and a process row; 3 cycles.
fn base_fixture() -> ReplayFixture {
    let mut temperature = fixture_source(
        "hwmon:t:temp1",
        "cpu.package.0",
        ids::CPU_TEMPERATURE_PACKAGE,
        "package:0",
        "mdegC",
        RawKind::Gauge,
        "hwmon.fast",
    );
    temperature.origin = OriginIdentity::confirmed(OriginId::new("x86_pkg_therm:0").unwrap());
    let counter = fixture_source(
        "procfs:t:user",
        "cpu",
        ids::CPU_TIME_USER,
        "host",
        "ticks",
        RawKind::Counter,
        "procfs.cpu",
    );
    let mut soc = fixture_source(
        "thermal:t:soc",
        "soc",
        ids::SOC_TEMPERATURE,
        "soc",
        "mdegC",
        RawKind::Gauge,
        "thermal.fast",
    );
    soc.sample_kind = SampleKind::Simulated;
    soc.quality = Quality::Usable;
    let mut rss = fixture_source(
        "procfs:process:rss",
        "host",
        ids::PROCESS_RSS,
        "process",
        "bytes",
        RawKind::Gauge,
        "procfs.process",
    );
    rss.rows = true;
    let mut soc_timeout =
        FixtureSample::status(SourceId::new("thermal:t:soc").unwrap(), ReadStatus::Timeout);
    soc_timeout.reasons.push("SYNTHETIC_TIMEOUT".to_owned());
    ReplayFixture {
        format: replay::FIXTURE_FORMAT.to_owned(),
        seed: 7,
        namespace_label: "unit".to_owned(),
        host_id: None,
        sampling_ms: 1000,
        simulated_latency_us: 0,
        entities: Vec::new(),
        sources: vec![temperature, counter, soc, rss],
        cycles: vec![
            vec![
                sample("hwmon:t:temp1", FixtureValue::F64(68000.0)),
                sample("procfs:t:user", FixtureValue::U64(100)),
                sample("thermal:t:soc", FixtureValue::F64(42000.0)),
                row("procfs:process:rss", 1_048_576),
            ],
            vec![
                sample("hwmon:t:temp1", FixtureValue::F64(69000.0)),
                sample("procfs:t:user", FixtureValue::U64(110)),
                soc_timeout,
                row("procfs:process:rss", 2_097_152),
            ],
            vec![
                sample("hwmon:t:temp1", FixtureValue::F64(0.0)),
                sample("procfs:t:user", FixtureValue::U64(120)),
                row("procfs:process:rss", 3_145_728),
            ],
        ],
        faults: Vec::new(),
    }
}

fn replay_adapter(
    fixture: ReplayFixture,
    clock: &std::sync::Arc<dgxtop_core::testing::FakeClock>,
) -> ReplayAdapter {
    ReplayAdapter::from_fixture(fixture, adapter_id(ADAPTER), time(clock), host(), None).unwrap()
}

fn read_group_records(
    adapter: &mut ReplayAdapter,
    group: &str,
    cycle: u64,
    clock: &std::sync::Arc<dgxtop_core::testing::FakeClock>,
) -> Vec<dgxtop_core::observation::Observation> {
    records(&read_all(
        adapter,
        ADAPTER,
        group,
        cycle,
        deadline(clock),
        Vec::new(),
    ))
}

fn violation_of(fixture: &ReplayFixture) -> &'static str {
    fixture
        .validate()
        .expect_err("fixture must be rejected")
        .code
}

fn small_profile() -> RefProfile {
    RefProfile {
        id: "SMALL".to_owned(),
        series: 48,
        sources: 160,
        processes: 12,
        process_gpu_links: 24,
        cycles: 4,
        ..RefProfile::reference()
    }
}

#[test]
fn same_seed_produces_identical_bytes_and_other_seeds_differ() {
    let first = generate_ref_fixture(20_261_004, &small_profile())
        .to_json_bytes()
        .unwrap();
    let second = generate_ref_fixture(20_261_004, &small_profile())
        .to_json_bytes()
        .unwrap();
    assert_eq!(
        first, second,
        "AT35: byte-identical output for the same seed"
    );
    let other = generate_ref_fixture(20_261_005, &small_profile())
        .to_json_bytes()
        .unwrap();
    assert_ne!(first, other);
}

#[test]
fn reference_fixture_is_byte_identical_and_matches_the_ref_workload() {
    let profile = RefProfile::reference();
    let fixture = generate_ref_fixture(20_261_004, &profile);
    let bytes = fixture.to_json_bytes().unwrap();
    assert_eq!(
        bytes,
        generate_ref_fixture(20_261_004, &profile)
            .to_json_bytes()
            .unwrap()
    );
    assert!(bytes.len() as u64 <= replay::MAX_FIXTURE_BYTES);
    fixture.validate().unwrap();
    assert_eq!(ReplayFixture::from_json_slice(&bytes).unwrap(), fixture);

    // PERFORMANCE.md §1: 2048 sources in all (the catalog cap counts the
    // process row sources as well).
    assert_eq!(fixture.sources.len(), 2048);
    let key_sources: Vec<&FixtureSource> = fixture
        .sources
        .iter()
        .filter(|source| !source.rows)
        .collect();
    assert!(key_sources.len() < 2048);
    let mut candidates: BTreeMap<(String, String, String), Vec<&FixtureSource>> = BTreeMap::new();
    for source in &key_sources {
        candidates
            .entry((
                source.entity_id.to_string(),
                source.metric_id.to_string(),
                source.scope.to_string(),
            ))
            .or_default()
            .push(source);
    }
    assert_eq!(candidates.len(), 512, "512 series");
    assert!(candidates.values().all(|list| list.len() <= 4));
    let aliases = candidates.values().any(|list| {
        let confirmed: Vec<_> = list
            .iter()
            .filter_map(|source| source.origin.confirmed_id())
            .collect();
        confirmed.len() >= 2 && confirmed.iter().collect::<BTreeSet<_>>().len() < confirmed.len()
    });
    assert!(aliases, "confirmed aliases share an origin");
    let independent = candidates.values().any(|list| {
        list.iter()
            .filter_map(|source| source.origin.confirmed_id())
            .collect::<BTreeSet<_>>()
            .len()
            >= 2
    });
    assert!(independent, "independent confirmed origins exist");
    assert!(
        fixture
            .sources
            .iter()
            .all(|source| source.sample_kind == SampleKind::Simulated)
    );

    let metrics: BTreeSet<&str> = fixture
        .sources
        .iter()
        .map(|source| source.metric_id.as_str())
        .collect();
    for family in [
        ids::CPU_TIME_USER,
        ids::CPU_TIME_IDLE,
        ids::CPU_LOAD_1M,
        ids::CPU_TASKS_TOTAL,
        ids::CPU_TEMPERATURE_PACKAGE,
        ids::CPU_TEMPERATURE_CORE,
        ids::SOC_TEMPERATURE,
        ids::CPU_ENERGY_PACKAGE,
        ids::CPU_POWER_CLUSTER,
        ids::MEMORY_TOTAL,
        ids::MEMORY_AVAILABLE,
        ids::DISK_SECTORS_READ,
        ids::NET_RX_BYTES,
        ids::NET_TX_PACKETS,
        ids::FS_AVAILABLE,
        ids::GPU_UTILIZATION_COMPUTE,
        ids::GPU_TEMPERATURE,
        ids::GPU_ENERGY_TOTAL,
        ids::PROCESS_CPU_TICKS,
        ids::PROCESS_RSS,
        ids::PROCESS_UID,
        ids::PROCESS_GPU_MEMORY,
        ids::PROCESS_GPU_UTILIZATION,
    ] {
        assert!(metrics.contains(family), "{family} missing from REF");
    }

    let samples: Vec<&FixtureSample> = fixture.cycles.iter().flatten().collect();
    let has_reason = |code: &str| {
        samples
            .iter()
            .any(|sample| sample.reasons.iter().any(|reason| reason == code))
    };
    assert!(has_reason("SENSOR_FAULT"), "intermittent faults");
    assert!(has_reason("ALARM"), "intermittent alarms");
    assert!(
        samples
            .iter()
            .any(|sample| sample.status == Some(ReadStatus::Timeout)),
        "missing samples"
    );
    assert!(
        fixture.cycles[0].len() < fixture.sources.len() - 19 + 256 * 3 + 1024 * 2,
        "some samples are absent"
    );
    let processes: BTreeSet<&str> = samples
        .iter()
        .filter(|sample| sample.source_id.as_str() == "procfs:process:rss")
        .filter_map(|sample| sample.entity_id.as_ref().map(EntityId::as_str))
        .collect();
    assert_eq!(processes.len(), 256);
    let links: BTreeSet<&str> = samples
        .iter()
        .filter(|sample| sample.source_id.as_str().ends_with(":memory_allocated"))
        .filter_map(|sample| sample.entity_id.as_ref().map(EntityId::as_str))
        .collect();
    assert_eq!(links.len(), 1024);
    assert_eq!(profile.adapter_hosts, 7);
    assert_eq!(fixture.sampling_ms, 1000);
}

#[test]
fn overload_fixture_stays_within_bounds() {
    let profile = RefProfile::overload();
    let fixture = generate_ref_fixture(1, &profile);
    fixture.validate().unwrap();
    assert!(fixture.sources.len() <= replay::MAX_SOURCES);
    assert!(
        fixture
            .cycles
            .iter()
            .all(|cycle| cycle.len() <= replay::MAX_SAMPLES_PER_CYCLE)
    );
    let one_cycle = ReplayFixture {
        cycles: vec![fixture.cycles[0].clone()],
        ..fixture.clone()
    };
    let bytes_per_cycle = one_cycle.to_json_bytes().unwrap().len();
    assert!(
        bytes_per_cycle * fixture.cycles.len()
            <= usize::try_from(replay::MAX_FIXTURE_BYTES).unwrap()
    );
    assert_eq!(fixture.sampling_ms, 100);
}

#[test]
fn validation_rejects_live_sample_kinds() {
    for live in [
        SampleKind::Measured,
        SampleKind::Derived,
        SampleKind::Estimated,
    ] {
        let mut fixture = base_fixture();
        fixture.sources[0].sample_kind = live;
        assert_eq!(violation_of(&fixture), "LIVE_SAMPLE_KIND");
        let clock = clock();
        let error =
            ReplayAdapter::from_fixture(fixture, adapter_id(ADAPTER), time(&clock), host(), None)
                .err()
                .unwrap();
        assert_eq!(error.violation_code(), Some("LIVE_SAMPLE_KIND"));
    }
}

#[test]
fn validation_rejects_unknown_metric_ids() {
    let mut fixture = base_fixture();
    fixture.sources[0].metric_id = MetricId::new("cpu.bogus.metric").unwrap();
    assert_eq!(violation_of(&fixture), "UNKNOWN_METRIC");
    let text = String::from_utf8(base_fixture().to_json_bytes().unwrap())
        .unwrap()
        .replace("cpu.temperature.package", "cpu.temperature.bogus");
    let error = ReplayFixture::from_json_slice(text.as_bytes()).unwrap_err();
    assert_eq!(error.violation_code(), Some("UNKNOWN_METRIC"));
    let malformed = String::from_utf8(base_fixture().to_json_bytes().unwrap())
        .unwrap()
        .replace("cpu.temperature.package", "Not-A-Metric");
    assert_eq!(
        ReplayFixture::from_json_slice(malformed.as_bytes())
            .unwrap_err()
            .violation_code(),
        Some("DECODE")
    );
}

#[test]
fn validation_rejects_oversize_inputs() {
    let sysfs = FakeSysfs::new("replay-oversize");
    let path = sysfs.path("/huge.json");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(replay::MAX_FIXTURE_BYTES + 1).unwrap();
    let error = ReplayFixture::load(&path).unwrap_err();
    assert_eq!(error.violation_code(), Some("FILE_TOO_LARGE"));
    let bytes = vec![b' '; usize::try_from(replay::MAX_FIXTURE_BYTES).unwrap() + 1];
    assert_eq!(
        ReplayFixture::from_json_slice(&bytes)
            .unwrap_err()
            .violation_code(),
        Some("FILE_TOO_LARGE")
    );
    let missing = ReplayFixture::load(Path::new("/nonexistent/dgxtop/fixture.json")).unwrap_err();
    assert!(
        matches!(missing, ReplayError::Io { .. }),
        "I/O is an infrastructure error"
    );

    let mut too_many_sources = base_fixture();
    let template = too_many_sources.sources[1].clone();
    too_many_sources.sources = (0..=replay::MAX_SOURCES)
        .map(|index| FixtureSource {
            source_id: SourceId::new(format!("procfs:t:{index}")).unwrap(),
            ..template.clone()
        })
        .collect();
    too_many_sources.cycles = vec![Vec::new()];
    assert_eq!(violation_of(&too_many_sources), "TOO_MANY_SOURCES");

    let mut too_many_cycles = base_fixture();
    too_many_cycles.cycles = vec![Vec::new(); replay::MAX_CYCLES + 1];
    assert_eq!(violation_of(&too_many_cycles), "TOO_MANY_CYCLES");
}

#[test]
fn adapter_ids_and_manifests_stay_in_the_replay_namespace() {
    let clock = clock();
    let wrong = ReplayAdapter::from_fixture(
        base_fixture(),
        adapter_id("hwmon"),
        time(&clock),
        host(),
        None,
    );
    assert!(matches!(wrong, Err(ReplayError::NotReplayAdapter(_))));
    let slice = ReplayAdapter::from_fixture(
        base_fixture(),
        adapter_id(ADAPTER),
        time(&clock),
        host(),
        Some((2, 2)),
    );
    assert!(matches!(slice, Err(ReplayError::InvalidSlice { .. })));
    let adapter = ReplayAdapter::from_fixture(
        base_fixture(),
        adapter_id("replay-3"),
        time(&clock),
        host(),
        None,
    )
    .unwrap();
    assert_eq!(adapter.manifest().sample_namespace, Namespace::Replay);
    assert_eq!(adapter.manifest().isolation, Isolation::PersistentProcess);
    let in_process = adapter.with_in_process(true);
    assert_eq!(in_process.manifest().isolation, Isolation::InProcessReplay);
    in_process.manifest().validate().unwrap();
    let manifest = replay::manifest(&adapter_id("replay-1"), false);
    manifest.validate().unwrap();
    assert_eq!(manifest.sample_namespace, Namespace::Replay);
    assert_eq!(manifest.isolation, Isolation::PersistentProcess);
    assert_eq!(
        replay::manifest(&adapter_id("replay"), true).isolation,
        Isolation::InProcessReplay
    );
}

#[test]
fn records_are_labelled_replay_and_stamped_with_the_current_clock() {
    let clock = clock();
    let mut adapter = replay_adapter(base_fixture(), &clock);
    let (_, pages) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    assert_eq!(catalog.len(), 4);
    let temperature = source(&catalog, "hwmon:t:temp1");
    assert_eq!(temperature.read_group.as_str(), "hwmon.fast");
    assert_eq!(temperature.sample_kind, SampleKind::Replay);
    assert!(temperature.metric.is_some());
    assert!(
        source(&catalog, "procfs:process:rss").metric.is_none(),
        "rows have no single key"
    );

    clock.advance_ms(1_234);
    let now = clock.now_boot_ns();
    let readings = read_group_records(&mut adapter, "hwmon.fast", 0, &clock);
    assert_eq!(readings.len(), 1, "only the requested read group");
    let reading = &readings[0];
    assert_eq!(value_of(reading), 68000.0);
    assert_eq!(reading.sample_kind, SampleKind::Replay);
    assert_eq!(reading.reason_codes[0], "REPLAY_FIXTURE");
    assert_eq!(reading.read_started_boot_ns, now);
    assert_eq!(reading.read_finished_boot_ns, now);
    assert_eq!(
        reading.provenance.origin_relation,
        OriginRelation::Confirmed
    );
    assert_eq!(reading.key.host_id, host());

    let soc = read_group_records(&mut adapter, "thermal.fast", 0, &clock);
    assert_eq!(soc[0].sample_kind, SampleKind::Simulated);
    assert_eq!(soc[0].reason_codes[0], "SYNTHETIC_FIXTURE");
    assert_eq!(soc[0].quality, Quality::Usable);
}

#[test]
fn failed_samples_have_no_value_and_absent_samples_have_no_record() {
    let clock = clock();
    let mut adapter = replay_adapter(base_fixture(), &clock);
    discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    let timeout = read_group_records(&mut adapter, "thermal.fast", 1, &clock);
    assert_eq!(timeout[0].read_status, ReadStatus::Timeout);
    assert!(timeout[0].value.is_none());
    assert!(
        timeout[0]
            .reason_codes
            .contains(&"SYNTHETIC_TIMEOUT".to_owned())
    );
    assert!(read_group_records(&mut adapter, "thermal.fast", 2, &clock).is_empty());
    let zero = read_group_records(&mut adapter, "hwmon.fast", 2, &clock);
    assert_eq!(zero[0].read_status, ReadStatus::Ok);
    assert_eq!(
        value_of(&zero[0]),
        0.0,
        "0 is a value, not a missing sample"
    );
}

#[test]
fn cycles_wrap_and_counters_keep_increasing_across_loops() {
    let clock = clock();
    let mut adapter = replay_adapter(base_fixture(), &clock);
    discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    assert_eq!(adapter.cycle_count(), 3);
    let values: Vec<u64> = (0..8)
        .map(|cycle| {
            read_group_records(&mut adapter, "procfs.cpu", cycle, &clock)[0]
                .value
                .unwrap()
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(values, vec![100, 110, 120, 130, 140, 150, 160, 170]);
    let gauges: Vec<f64> = [0, 3, 6]
        .iter()
        .map(|cycle| value_of(&read_group_records(&mut adapter, "hwmon.fast", *cycle, &clock)[0]))
        .collect();
    assert_eq!(gauges, vec![68000.0; 3], "gauges repeat per loop");
}

#[test]
fn removed_source_reads_removed_then_leaves_the_catalog() {
    let mut fixture = base_fixture();
    fixture.faults.push(FixtureFault {
        cycle: 1,
        read_group: None,
        kind: FaultKind::RemoveSource {
            source_id: SourceId::new("hwmon:t:temp1").unwrap(),
        },
    });
    let clock = clock();
    let mut adapter = replay_adapter(fixture, &clock);
    let (generation, _) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    assert_eq!(
        read_group_records(&mut adapter, "hwmon.fast", 0, &clock)[0].read_status,
        ReadStatus::Ok
    );
    let removed = read_group_records(&mut adapter, "hwmon.fast", 1, &clock);
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].read_status, ReadStatus::Removed);
    assert!(removed[0].value.is_none());
    let (next, pages) = discover(&mut adapter, ADAPTER, generation, deadline(&clock));
    assert!(next > generation);
    assert_eq!(
        pages.last().unwrap().removed_sources,
        vec![SourceId::new("hwmon:t:temp1").unwrap()]
    );
    assert!(read_group_records(&mut adapter, "hwmon.fast", 2, &clock).is_empty());
    assert!(read_group_records(&mut adapter, "hwmon.fast", 3, &clock).is_empty());
    assert_eq!(adapter.source_count(), 3);
}

#[test]
fn generation_change_bumps_generations_once_per_cycle() {
    let mut fixture = base_fixture();
    fixture.faults.push(FixtureFault {
        cycle: 1,
        read_group: None,
        kind: FaultKind::GenerationChange,
    });
    let clock = clock();
    let mut adapter = replay_adapter(fixture, &clock);
    let (generation, _) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    assert_eq!(
        read_group_records(&mut adapter, "hwmon.fast", 0, &clock)[0]
            .provenance
            .generation,
        1
    );
    let bumped = read_group_records(&mut adapter, "hwmon.fast", 1, &clock);
    assert_eq!(bumped[0].provenance.generation, 2);
    assert!(
        bumped[0]
            .reason_codes
            .contains(&"REPLAY_GENERATION_CHANGE".to_owned())
    );
    assert_eq!(
        read_group_records(&mut adapter, "procfs.cpu", 1, &clock)[0]
            .provenance
            .generation,
        2
    );
    let (next, pages) = discover(&mut adapter, ADAPTER, generation, deadline(&clock));
    assert!(next > generation);
    assert!(
        sources(&pages)
            .iter()
            .all(|descriptor| descriptor.generation == 2)
    );
    assert_eq!(
        read_group_records(&mut adapter, "hwmon.fast", 4, &clock)[0]
            .provenance
            .generation,
        3
    );
}

#[test]
fn large_groups_are_paged_consistently() {
    let mut fixture = base_fixture();
    let template = fixture.sources[0].clone();
    let mut cycle = Vec::new();
    for index in 0..300 {
        let id = format!("hwmon:t:extra{index}");
        fixture.sources.push(FixtureSource {
            source_id: SourceId::new(id.as_str()).unwrap(),
            origin: OriginIdentity::unknown(),
            scope: Scope::new(format!("package:{index}")).unwrap(),
            entity_id: EntityId::new(format!("cpu.package.{index}")).unwrap(),
            ..template.clone()
        });
        cycle.push(sample(&id, FixtureValue::F64(40000.0)));
    }
    fixture.cycles[0].extend(cycle);
    let clock = clock();
    let mut adapter = replay_adapter(fixture, &clock);
    let pages = discover_all(&mut adapter, ADAPTER, 0, deadline(&clock));
    assert!(pages.len() >= 2);
    assert!(
        pages
            .iter()
            .all(|page| page.entities.len() + page.sources.len() <= 256)
    );
    let batches = read_all(
        &mut adapter,
        ADAPTER,
        "hwmon.fast",
        0,
        deadline(&clock),
        Vec::new(),
    );
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].records.len(), 256);
    assert_eq!(batches[0].error_code.as_deref(), Some("PAGE_CONTINUES"));
    assert_eq!(batches[1].records.len(), 45);
    assert!(!batches[1].continues());
}

#[test]
fn slot_selection_and_invalid_plans() {
    let clock = clock();
    let mut adapter = replay_adapter(base_fixture(), &clock);
    let (_, pages) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    let slot = source(&sources(&pages), "procfs:t:user").slot;
    let selected = records(&read_all(
        &mut adapter,
        ADAPTER,
        "procfs.cpu",
        0,
        deadline(&clock),
        vec![slot],
    ));
    assert_eq!(selected.len(), 1);
    let none = records(&read_all(
        &mut adapter,
        ADAPTER,
        "procfs.cpu",
        0,
        deadline(&clock),
        vec![slot + 100],
    ));
    assert!(none.is_empty());
    let unknown = adapter.read(&read_plan(
        ADAPTER,
        "fast",
        0,
        0,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(unknown.error_code.as_deref(), Some("UNKNOWN_READ_GROUP"));
    let expired = adapter.read(&read_plan(
        ADAPTER,
        "hwmon.fast",
        0,
        0,
        clock.now_boot_ns() - 1,
        Vec::new(),
    ));
    assert_eq!(expired.error_code.as_deref(), Some("DEADLINE_EXPIRED"));
    let other_group = records(&read_all(
        &mut adapter,
        ADAPTER,
        "nvml.fast",
        0,
        deadline(&clock),
        Vec::new(),
    ));
    assert!(
        other_group.is_empty(),
        "known group without sources in this slice"
    );
    adapter.shutdown();
    let after = adapter.read(&read_plan(
        ADAPTER,
        "hwmon.fast",
        1,
        0,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(after.error_code.as_deref(), Some("ADAPTER_SHUT_DOWN"));
}

#[test]
fn shipped_demo_fixture_loads_and_shows_package_soc_and_conflict() {
    let clock = clock();
    let fixture = ReplayFixture::load(Path::new(DEMO)).unwrap();
    assert_eq!(fixture.cycles.len(), 60);
    assert_eq!(
        fixture.host_id.as_ref().map(|id| id.as_str()),
        Some("demo-host")
    );
    assert_eq!(
        ReplayFixture::from_json_slice(&fixture.to_json_bytes().unwrap()).unwrap(),
        fixture
    );
    let mut adapter = ReplayAdapter::from_path(
        Path::new(DEMO),
        adapter_id(ADAPTER),
        time(&clock),
        host(),
        None,
    )
    .unwrap()
    .with_in_process(true);
    let (_, pages) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    assert!(
        catalog
            .iter()
            .all(|descriptor| descriptor.sample_kind == SampleKind::Simulated)
    );

    let hwmon = read_group_records(&mut adapter, "hwmon.fast", 0, &clock);
    let thermal = read_group_records(&mut adapter, "thermal.fast", 0, &clock);
    let package = record(&hwmon, "hwmon:coretemp@platform/coretemp.0:temp1");
    let alias = record(&thermal, "thermal:x86_pkg_temp:temp");
    let soc = record(&thermal, "thermal:soc_thermal:temp");
    assert_eq!(value_of(package), 68000.0);
    assert_eq!(value_of(soc), 42000.0);
    assert_eq!(package.key, alias.key);
    assert_eq!(package.provenance.origin_id, alias.provenance.origin_id);
    assert_ne!(
        package.key, soc.key,
        "package and SoC are different MetricKeys"
    );
    let package_one = record(&hwmon, "hwmon:coretemp@platform/coretemp.1:temp1");
    let bmc = record(&hwmon, "hwmon:demo-bmc@platform/ipmi_bmc.0:temp3");
    assert_eq!(package_one.key, bmc.key);
    assert_ne!(package_one.provenance.origin_id, bmc.provenance.origin_id);
    assert_eq!(bmc.provenance.origin_relation, OriginRelation::Confirmed);
    assert!(
        (value_of(package_one) - value_of(bmc)).abs() > 8000.0,
        "conflict example"
    );
    assert!(
        hwmon
            .iter()
            .all(|reading| reading.reason_codes[0] == "SYNTHETIC_FIXTURE")
    );

    let gpu_rows = read_group_records(&mut adapter, "nvml.process", 0, &clock);
    assert_eq!(gpu_rows.len(), 2);
    assert!(
        gpu_rows
            .iter()
            .all(|reading| reading.key.entity_id.as_str().starts_with("gpupid:"))
    );
    for group in [
        "procfs.cpu",
        "procfs.memory",
        "procfs.disk",
        "network.fast",
        "filesystem.capacity",
        "nvml.fast",
        "nvml.slow_health",
        "powercap.fast",
        "procfs.process",
    ] {
        assert!(
            !read_group_records(&mut adapter, group, 0, &clock).is_empty(),
            "{group}"
        );
    }
}

#[test]
fn fixture_values_round_trip_numbers_and_decimal_strings() {
    let json = br#"[68000.0, 42, "18446744073709551615"]"#;
    let values: Vec<FixtureValue> = serde_json::from_slice(json).unwrap();
    assert_eq!(
        values,
        vec![
            FixtureValue::F64(68000.0),
            FixtureValue::F64(42.0),
            FixtureValue::U64(u64::MAX)
        ]
    );
    assert_eq!(
        serde_json::to_string(&values).unwrap(),
        r#"[68000.0,42.0,"18446744073709551615"]"#
    );
    assert!(serde_json::from_slice::<FixtureValue>(b"9007199254740993").is_err());
    assert!(serde_json::from_slice::<FixtureValue>(br#""01""#).is_err());
}
