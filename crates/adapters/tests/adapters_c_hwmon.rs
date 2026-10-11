//! hwmon adapter integration tests over fake sysfs trees (no real drivers).
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests may unwrap

#[path = "fixtures/adapters_c/support.rs"]
mod support;

use std::io;
use std::sync::Arc;

use dgxtop_adapters::common::{CountingFiles, SystemFiles};
use dgxtop_adapters::hwmon::{self, HwmonAdapter};
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::OriginRelation;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::BatchStatus;
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{
    Availability, Namespace, Quality, RawKind, ReadStatus, SampleKind, Support,
};
use support::*;

const ADAPTER: &str = "hwmon";
const CORETEMP_DEVICE: &str = "platform/coretemp.0";
const CORETEMP_KEY: &str = "coretemp@platform/coretemp.0";

fn adapter(
    sysfs: &FakeSysfs,
    clock: &Arc<dgxtop_core::testing::FakeClock>,
    options: AdapterOptions,
) -> HwmonAdapter {
    HwmonAdapter::new(time(clock), host(), sysfs.files(), options)
}

fn coretemp(sysfs: &FakeSysfs, index: u32) -> String {
    let directory = sysfs.hwmon(index, CORETEMP_DEVICE, "coretemp");
    sysfs.write(&format!("{directory}/temp1_label"), "Package id 0\n");
    sysfs.write(&format!("{directory}/temp1_input"), "68000\n");
    sysfs.write(&format!("{directory}/temp2_label"), "Core 0\n");
    sysfs.write(&format!("{directory}/temp2_input"), "65000\n");
    sysfs.write(&format!("{directory}/temp3_label"), "Core 4\n");
    sysfs.write(&format!("{directory}/temp3_input"), "0\n");
    sysfs.write(&format!("{directory}/temp1_crit_alarm"), "0\n");
    directory
}

fn source_id(device_key: &str, channel: &str) -> String {
    format!("hwmon:{device_key}:{channel}")
}

#[test]
fn coretemp_package_and_cores_map_to_cpu_metrics_with_confirmed_origins() {
    let sysfs = FakeSysfs::new("hwmon-coretemp");
    coretemp(&sysfs, 2);
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);

    let package = source(&catalog, &source_id(CORETEMP_KEY, "temp1"));
    let key = package.metric.as_ref().expect("package is profile-proven");
    assert_eq!(key.entity_id.as_str(), "cpu.package.0");
    assert_eq!(key.metric_id.as_str(), ids::CPU_TEMPERATURE_PACKAGE);
    assert_eq!(key.scope.as_str(), "package:0");
    assert_eq!(package.origin.relation, OriginRelation::Confirmed);
    assert_eq!(
        package.origin.origin_id.as_ref().unwrap().as_str(),
        "x86_pkg_therm:0"
    );
    assert_eq!(package.profile_id.as_str(), "hwmon-coretemp");
    assert_eq!(package.profile_rank, 0);
    assert_eq!(package.label.as_deref(), Some("Package id 0"));
    assert_eq!(package.native_unit.as_str(), "mdegC");
    assert_eq!(package.raw_kind, RawKind::Gauge);
    assert_eq!(package.read_group.as_str(), "fast");
    assert_eq!(package.sample_kind, SampleKind::Measured);
    assert_eq!(package.channel, "temp1_input");

    let core = source(&catalog, &source_id(CORETEMP_KEY, "temp3"));
    let core_key = core.metric.as_ref().unwrap();
    assert_eq!(core_key.entity_id.as_str(), "cpu.package.0");
    assert_eq!(core_key.metric_id.as_str(), ids::CPU_TEMPERATURE_CORE);
    assert_eq!(core_key.scope.as_str(), "core:4");
    assert_eq!(
        core.origin.origin_id.as_ref().unwrap().as_str(),
        "coretemp:0:core:4"
    );

    let entity = pages
        .iter()
        .flat_map(|page| page.entities.iter())
        .find(|entity| entity.entity_id.as_str() == "cpu.package.0")
        .expect("shared package entity");
    assert_eq!(entity.attribute("physical_package"), Some("0"));

    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let package_reading = record(&readings, &source_id(CORETEMP_KEY, "temp1"));
    assert_eq!(package_reading.read_status, ReadStatus::Ok);
    assert_eq!(value_of(package_reading), 68000.0);
    assert_eq!(package_reading.quality, Quality::Verified);
    assert_eq!(package_reading.device_sample_boot_ns, None);
    assert_eq!(package_reading.provenance.channel, "temp1_input");
    assert!(package_reading.reason_codes.is_empty());
}

#[test]
fn zero_celsius_is_kept_as_a_valid_reading() {
    let sysfs = FakeSysfs::new("hwmon-zero");
    coretemp(&sysfs, 0);
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let zero = record(&readings, &source_id(CORETEMP_KEY, "temp3"));
    assert_eq!(zero.read_status, ReadStatus::Ok);
    assert_eq!(zero.value.unwrap().to_f64_lossy(), 0.0);
    assert_eq!(zero.quality, Quality::Verified);
}

#[test]
fn unclassified_sensors_are_diagnostics_with_unknown_origin() {
    let sysfs = FakeSysfs::new("hwmon-diagnostics");
    let nvme = sysfs.hwmon(1, "pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0", "nvme");
    sysfs.write(&format!("{nvme}/temp1_input"), "41850\n");
    sysfs.write(&format!("{nvme}/temp1_label"), "Composite\n");
    let superio = sysfs.hwmon(2, "platform/nct6775.656", "nct6798");
    // A label that looks like a CPU must not be trusted without a profile.
    sysfs.write(&format!("{superio}/temp1_input"), "45000\n");
    sysfs.write(&format!("{superio}/temp1_label"), "CPUTIN\n");
    sysfs.write(&format!("{superio}/fan1_input"), "1180\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    assert_eq!(catalog.len(), 3);
    for descriptor in &catalog {
        assert!(
            descriptor.metric.is_none(),
            "{} must be diagnostics",
            descriptor.source_id
        );
        assert_eq!(descriptor.origin.relation, OriginRelation::Unknown);
        assert!(descriptor.origin.origin_id.is_none());
        assert_eq!(descriptor.profile_id.as_str(), "hwmon-generic");
        assert!(descriptor.entity_id.as_str().starts_with("sensor:hwmon:"));
    }
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let cputin = record(
        &readings,
        &source_id("nct6798@platform/nct6775.656", "temp1"),
    );
    assert_eq!(cputin.key.metric_id.as_str(), ids::SENSOR_TEMPERATURE);
    assert_eq!(cputin.key.scope.as_str(), "sensor");
    assert_eq!(
        cputin.key.entity_id.as_str(),
        "sensor:hwmon:nct6798@platform/nct6775.656:temp1"
    );
    assert_eq!(cputin.quality, Quality::Unknown);
    let fan = record(
        &readings,
        &source_id("nct6798@platform/nct6775.656", "fan1"),
    );
    assert_eq!(fan.key.metric_id.as_str(), ids::SENSOR_FAN);
    assert_eq!(fan.native_unit.as_str(), "count");
    assert_eq!(value_of(fan), 1180.0);
    let nvme_record = record(
        &readings,
        &source_id(
            "nvme@pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0",
            "temp1",
        ),
    );
    assert_eq!(nvme_record.provenance.driver, "nvme");
}

/// The kernel mirrors thermal zones as an hwmon device whose `device` is a
/// zone (GB10's `acpitz`: one `tempN_input` per zone). The thermal adapter
/// already reads every zone and maps it by ACPI path (D-68), so hwmon leaves
/// the mirror out instead of reading each sensor twice (D-81).
#[test]
fn thermal_zone_mirrors_are_left_to_the_thermal_adapter() {
    let sysfs = FakeSysfs::new("hwmon-thermal-mirror");
    let acpitz = sysfs.hwmon(0, "virtual/thermal/thermal_zone0", "acpitz");
    for index in 1..=7 {
        sysfs.write(
            &format!("{acpitz}/temp{index}_input"),
            &format!("{}\n", 70_000 + index),
        );
    }
    let nvme = sysfs.hwmon(1, "pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0", "nvme");
    sysfs.write(&format!("{nvme}/temp1_input"), "41850\n");
    let counting = Arc::new(CountingFiles::new(sysfs.rooted()));
    let files: Arc<dyn SystemFiles> = counting.clone();
    let clock = clock();
    let mut hwmon = HwmonAdapter::new(time(&clock), host(), files, AdapterOptions::default());

    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let source_ids: Vec<&str> = catalog
        .iter()
        .map(|descriptor| descriptor.source_id.as_str())
        .collect();
    assert_eq!(
        source_ids,
        vec![source_id(
            "nvme@pci0000:00/0000:00:06.0/0000:02:00.0/nvme/nvme0",
            "temp1"
        )]
    );
    for cycle in 1..=3 {
        clock.advance_ms(1_000);
        let readings = records(&read_all(
            &mut hwmon,
            ADAPTER,
            "fast",
            cycle,
            deadline(&clock),
            Vec::new(),
        ));
        assert_eq!(readings.len(), 1, "only the NVMe sensor is read");
        discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    }
    assert_eq!(
        counting.reads_matching("hwmon0/temp"),
        0,
        "no mirrored zone value is ever read"
    );
    assert_eq!(
        counting.reads_matching("hwmon0/name"),
        1,
        "the mirror's metadata is read once and cached"
    );
}

#[test]
fn fault_flag_keeps_value_but_marks_quality_invalid() {
    let sysfs = FakeSysfs::new("hwmon-fault");
    let directory = coretemp(&sysfs, 0);
    sysfs.write(&format!("{directory}/temp1_fault"), "1\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let faulted = record(&readings, &source_id(CORETEMP_KEY, "temp1"));
    assert_eq!(faulted.read_status, ReadStatus::Ok);
    assert_eq!(value_of(faulted), 68000.0);
    assert_eq!(faulted.quality, Quality::Invalid);
    assert!(faulted.reason_codes.contains(&"SENSOR_FAULT".to_owned()));
    assert!(!faulted.reason_codes.contains(&"ALARM".to_owned()));
}

#[test]
fn alarm_flag_keeps_high_reading_and_quality() {
    let sysfs = FakeSysfs::new("hwmon-alarm");
    let directory = coretemp(&sysfs, 0);
    sysfs.write(&format!("{directory}/temp1_input"), "105000\n");
    sysfs.write(&format!("{directory}/temp1_crit_alarm"), "1\n");
    sysfs.write(&format!("{directory}/temp1_alarm"), "1\n");
    sysfs.write(&format!("{directory}/temp1_fault"), "0\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let hot = record(&readings, &source_id(CORETEMP_KEY, "temp1"));
    assert_eq!(hot.read_status, ReadStatus::Ok);
    assert_eq!(value_of(hot), 105000.0);
    assert_eq!(hot.quality, Quality::Verified, "an alarm is not a fault");
    assert!(hot.reason_codes.contains(&"ALARM".to_owned()));
    assert!(hot.reason_codes.contains(&"ALARM_CRIT".to_owned()));
    assert!(!hot.reason_codes.contains(&"SENSOR_FAULT".to_owned()));
}

#[test]
fn renumbered_hwmon_keeps_source_ids_and_slots_and_bumps_generation() {
    let sysfs = FakeSysfs::new("hwmon-renumber");
    coretemp(&sysfs, 3);
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (generation, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let before = sources(&pages);

    sysfs.remove_hwmon(3, CORETEMP_DEVICE);
    coretemp(&sysfs, 7);
    clock.advance_ms(10_000);
    let (next_generation, pages) = discover(&mut hwmon, ADAPTER, generation, deadline(&clock));
    let after = sources(&pages);
    assert!(next_generation > generation);
    assert!(pages.iter().all(|page| page.removed_sources.is_empty()));
    assert_eq!(before.len(), after.len());
    for old in &before {
        let new = source(&after, old.source_id.as_str());
        assert_eq!(
            new.slot, old.slot,
            "slot of {} must be stable",
            old.source_id
        );
        assert!(
            new.generation > old.generation,
            "re-created device needs a new generation"
        );
        assert!(new.device_path.ends_with("hwmon7"));
    }
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        2,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(
        value_of(record(&readings, &source_id(CORETEMP_KEY, "temp1"))),
        68000.0
    );
}

#[test]
fn gb10_labels_map_clusters_and_platform_power() {
    let sysfs = FakeSysfs::new("hwmon-gb10");
    let directory = sysfs.hwmon(4, "platform/spbm.0", "spbm");
    for (index, label, input) in [
        (1, "cpu_p", "21000000"),
        (2, "cpu_e", "4000000"),
        (3, "soc_pkg", "35000000"),
        (4, "cpu_gpu", "60000000"),
    ] {
        sysfs.write(
            &format!("{directory}/power{index}_label"),
            &format!("{label}\n"),
        );
        sysfs.write(
            &format!("{directory}/power{index}_input"),
            &format!("{input}\n"),
        );
    }
    sysfs.write(&format!("{directory}/power1_average"), "20500000\n");
    sysfs.write(&format!("{directory}/temp1_input"), "52000\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let key = "spbm@platform/spbm.0";

    let cpu_p = source(&catalog, &source_id(key, "power1"));
    let cpu_p_key = cpu_p.metric.as_ref().unwrap();
    assert_eq!(cpu_p_key.metric_id.as_str(), ids::CPU_POWER_CLUSTER);
    assert_eq!(cpu_p_key.entity_id.as_str(), "cpu.package.0");
    assert_eq!(cpu_p_key.scope.as_str(), "cluster:p");
    assert_eq!(cpu_p.profile_id.as_str(), "nvidia-gb10-hwmon");
    assert_eq!(cpu_p.native_unit.as_str(), "uW");
    assert_eq!(cpu_p.profile_rank, 0);

    let cpu_p_average = source(&catalog, &source_id(key, "power1_average"));
    assert_eq!(
        cpu_p_average.metric, cpu_p.metric,
        "same key, different semantics"
    );
    assert_eq!(cpu_p_average.profile_rank, 1);
    assert_eq!(cpu_p_average.origin, cpu_p.origin);
    assert_eq!(
        cpu_p.origin.origin_id.as_ref().unwrap().as_str(),
        "hwmon:spbm@platform/spbm.0:power1"
    );
    assert_ne!(cpu_p_average.source_id, cpu_p.source_id);

    let cpu_e = source(&catalog, &source_id(key, "power2"));
    assert_eq!(cpu_e.metric.as_ref().unwrap().scope.as_str(), "cluster:e");
    let platform = source(&catalog, &source_id(key, "power3"));
    let platform_key = platform.metric.as_ref().unwrap();
    assert_eq!(platform_key.metric_id.as_str(), ids::CPU_POWER_PLATFORM);
    assert_eq!(platform_key.entity_id.as_str(), "host");
    assert_eq!(platform_key.scope.as_str(), "platform");
    assert!(source(&catalog, &source_id(key, "power4")).metric.is_none());
    assert!(source(&catalog, &source_id(key, "temp1")).metric.is_none());

    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let reading = record(&readings, &source_id(key, "power1"));
    assert_eq!(
        reading.quality,
        Quality::Usable,
        "unverified hardware is never verified"
    );
    assert_eq!(value_of(reading), 21_000_000.0);
    assert_eq!(
        value_of(record(&readings, &source_id(key, "power1_average"))),
        20_500_000.0
    );
}

#[test]
fn gb10_profile_needs_both_cluster_labels() {
    let sysfs = FakeSysfs::new("hwmon-gb10-partial");
    let directory = sysfs.hwmon(0, "platform/spbm.0", "spbm");
    sysfs.write(&format!("{directory}/power1_label"), "cpu_p\n");
    sysfs.write(&format!("{directory}/power1_input"), "21000000\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let only = source(&catalog, &source_id("spbm@platform/spbm.0", "power1"));
    assert!(only.metric.is_none());
    // Diagnostics power still proves input/average share one sensor.
    assert_eq!(only.origin.relation, OriginRelation::Confirmed);
}

#[test]
fn amd_tdie_maps_on_single_socket_and_tctl_is_only_a_fallback() {
    let sysfs = FakeSysfs::new("hwmon-amd");
    let directory = sysfs.hwmon(0, "pci0000:00/0000:00:18.3", "k10temp");
    sysfs.write(&format!("{directory}/temp1_label"), "Tctl\n");
    sysfs.write(&format!("{directory}/temp1_input"), "71000\n");
    sysfs.write(&format!("{directory}/temp2_label"), "Tdie\n");
    sysfs.write(&format!("{directory}/temp2_input"), "61000\n");
    sysfs.write(&format!("{directory}/temp3_label"), "Tccd1\n");
    sysfs.write(&format!("{directory}/temp3_input"), "59000\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let key = "k10temp@pci0000:00/0000:00:18.3";
    let tdie = source(&catalog, &source_id(key, "temp2"));
    assert_eq!(
        tdie.metric.as_ref().unwrap().metric_id.as_str(),
        ids::CPU_TEMPERATURE_PACKAGE
    );
    assert!(
        source(&catalog, &source_id(key, "temp1")).metric.is_none(),
        "Tctl ignored when Tdie exists"
    );
    assert!(source(&catalog, &source_id(key, "temp3")).metric.is_none());

    // Two AMD devices: the socket mapping is unproven.
    let second = sysfs.hwmon(1, "pci0000:40/0000:40:18.3", "k10temp");
    sysfs.write(&format!("{second}/temp1_label"), "Tdie\n");
    sysfs.write(&format!("{second}/temp1_input"), "60000\n");
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    assert!(
        sources(&pages)
            .iter()
            .all(|descriptor| descriptor.metric.is_none())
    );
}

#[test]
fn amd_tctl_is_usable_fallback_without_tdie() {
    let sysfs = FakeSysfs::new("hwmon-amd-tctl");
    let directory = sysfs.hwmon(0, "pci0000:00/0000:00:18.3", "zenpower");
    sysfs.write(&format!("{directory}/temp1_label"), "Tctl\n");
    sysfs.write(&format!("{directory}/temp1_input"), "71000\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let tctl = source(
        &catalog,
        &source_id("zenpower@pci0000:00/0000:00:18.3", "temp1"),
    );
    assert_eq!(tctl.profile_rank, 1);
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(readings[0].quality, Quality::Usable);
}

#[test]
fn unsupported_channel_is_reprobed_at_most_once_per_interval() {
    let sysfs = FakeSysfs::new("hwmon-unsupported");
    let directory = coretemp(&sysfs, 0);
    let value_path = format!("{directory}/temp2_input");
    let faulty = FaultyFiles::new(sysfs.rooted());
    faulty.fail(&value_path, Failure::Os(61));
    let counting = Arc::new(CountingFiles::new(faulty));
    let files: Arc<dyn SystemFiles> = counting.clone();
    let clock = clock();
    let options = AdapterOptions {
        unsupported_probe_ms: 60_000,
        ..AdapterOptions::default()
    };
    let mut hwmon = HwmonAdapter::new(time(&clock), host(), files, options);
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let capability = pages
        .iter()
        .flat_map(|page| page.capabilities.iter())
        .find(|capability| capability.capability_id == "cpu.temperature.core|core:0")
        .expect("core 0 capability");
    assert_eq!(capability.support, Support::Unsupported);
    assert_eq!(capability.reason.as_deref(), Some("UNSUPPORTED_BY_DRIVER"));
    let probes_after_discovery = counting.reads_of(&value_path);
    assert_eq!(probes_after_discovery, 1);

    for cycle in 1..=5 {
        clock.advance_ms(1_000);
        let readings = records(&read_all(
            &mut hwmon,
            ADAPTER,
            "fast",
            cycle,
            deadline(&clock),
            Vec::new(),
        ));
        let core = record(&readings, &source_id(CORETEMP_KEY, "temp2"));
        assert_eq!(core.read_status, ReadStatus::Unsupported);
        assert!(core.value.is_none(), "unsupported never becomes 0");
        assert_eq!(core.reason_codes, vec!["UNSUPPORTED_BY_DRIVER".to_owned()]);
    }
    assert_eq!(
        counting.reads_of(&value_path),
        1,
        "no re-probe inside the interval"
    );

    clock.advance_ms(60_000);
    read_all(&mut hwmon, ADAPTER, "fast", 6, deadline(&clock), Vec::new());
    assert_eq!(
        counting.reads_of(&value_path),
        2,
        "one re-probe after the interval"
    );
}

#[test]
fn reprobe_recovers_a_channel_after_the_interval() {
    let sysfs = FakeSysfs::new("hwmon-recover");
    let directory = coretemp(&sysfs, 0);
    let value_path = format!("{directory}/temp2_input");
    let faulty = Arc::new(FaultyFiles::new(sysfs.rooted()));
    faulty.fail(&value_path, Failure::Kind(io::ErrorKind::PermissionDenied));
    let files: Arc<dyn SystemFiles> = faulty.clone();
    let clock = clock();
    let mut hwmon = HwmonAdapter::new(time(&clock), host(), files, AdapterOptions::default());
    discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let first = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(
        record(&first, &source_id(CORETEMP_KEY, "temp2")).read_status,
        ReadStatus::PermissionDenied
    );
    faulty.heal(&value_path);
    clock.advance_ms(60_001);
    let healed = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        2,
        deadline(&clock),
        Vec::new(),
    ));
    let core = record(&healed, &source_id(CORETEMP_KEY, "temp2"));
    assert_eq!(core.read_status, ReadStatus::Ok);
    assert_eq!(value_of(core), 65000.0);
}

#[test]
fn paging_bounds_catalog_pages_and_read_batches() {
    let sysfs = FakeSysfs::new("hwmon-paging");
    let directory = sysfs.hwmon(0, "platform/nct6775.656", "nct6798");
    for index in 1..=8 {
        sysfs.write(
            &format!("{directory}/temp{index}_input"),
            &format!("{}\n", 30000 + index),
        );
    }
    let clock = clock();
    let options = AdapterOptions {
        page_size: 3,
        ..AdapterOptions::default()
    };
    let mut hwmon = adapter(&sysfs, &clock, options);
    let pages = discover_all(&mut hwmon, ADAPTER, 0, deadline(&clock));
    assert!(pages.len() >= 6, "8 sources + 8 entities in pages of 3");
    for (index, page) in pages.iter().enumerate() {
        assert!(page.entities.len() + page.sources.len() <= 3);
        let last = index + 1 == pages.len();
        assert_eq!(page.complete, last);
        assert_eq!(page.next_page.is_some(), !last);
    }
    assert_eq!(sources(&pages).len(), 8);

    let batches = read_all(&mut hwmon, ADAPTER, "fast", 1, deadline(&clock), Vec::new());
    assert_eq!(batches.len(), 3);
    assert_eq!(batches[0].records.len(), 3);
    assert_eq!(batches[0].error_code.as_deref(), Some("PAGE_CONTINUES"));
    assert_eq!(batches[0].status, BatchStatus::Partial);
    assert_eq!(batches[2].records.len(), 2);
    assert_eq!(batches[2].status, BatchStatus::Ok);
    let missing = hwmon.read(&read_plan(
        ADAPTER,
        "fast",
        99,
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(missing.status, BatchStatus::Failed);
    assert_eq!(missing.error_code.as_deref(), Some("PAGE_SNAPSHOT_MISSING"));
}

#[test]
fn interrupted_discovery_never_removes_sources() {
    let sysfs = FakeSysfs::new("hwmon-interrupted");
    let directory = sysfs.hwmon(0, "platform/nct6775.656", "nct6798");
    for index in 1..=4 {
        sysfs.write(&format!("{directory}/temp{index}_input"), "30000\n");
    }
    let clock = clock();
    let options = AdapterOptions {
        page_size: 2,
        ..AdapterOptions::default()
    };
    let mut hwmon = adapter(&sysfs, &clock, options);
    let (generation, _) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    sysfs.remove(&format!("{directory}/temp4_input"));
    let first = hwmon.discover(&discover_request(ADAPTER, generation, 0, deadline(&clock)));
    assert!(!first.complete);
    assert!(
        first.removed_sources.is_empty(),
        "removals only on the final page"
    );
    // A later page that does not belong to the pending discovery is refused.
    let stray = hwmon.discover(&discover_request(ADAPTER, generation, 5, deadline(&clock)));
    assert!(!stray.complete);
    assert_eq!(
        stray.error_code.as_deref(),
        Some("DISCOVERY_RESTART_REQUIRED")
    );
    assert!(stray.removed_sources.is_empty());
}

#[test]
fn vanished_device_reads_removed_then_is_removed_from_the_catalog() {
    let sysfs = FakeSysfs::new("hwmon-vanish");
    coretemp(&sysfs, 0);
    let nvme = sysfs.hwmon(1, "pci0000:00/0000:02:00.0/nvme/nvme0", "nvme");
    sysfs.write(&format!("{nvme}/temp1_input"), "40000\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (generation, _) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let nvme_id = source_id("nvme@pci0000:00/0000:02:00.0/nvme/nvme0", "temp1");

    sysfs.remove_hwmon(1, "pci0000:00/0000:02:00.0/nvme/nvme0");
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let gone = record(&readings, &nvme_id);
    assert_eq!(gone.read_status, ReadStatus::Removed);
    assert!(gone.value.is_none());

    let (next, pages) = discover(&mut hwmon, ADAPTER, generation, deadline(&clock));
    assert!(next > generation);
    let last = pages.last().unwrap();
    assert!(last.removed_sources.iter().any(|id| id.as_str() == nvme_id));
    assert!(
        last.removed_entities
            .iter()
            .any(|id| id.as_str() == "sensor:hwmon:nvme@pci0000:00/0000:02:00.0/nvme/nvme0:temp1")
    );
    assert!(
        !last
            .removed_entities
            .iter()
            .any(|id| id.as_str() == "cpu.package.0"),
        "shared entities are never removed"
    );
    let after = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        2,
        deadline(&clock),
        Vec::new(),
    ));
    assert!(
        after
            .iter()
            .all(|reading| reading.provenance.source_id.as_str() != nvme_id)
    );
}

#[test]
fn missing_hwmon_class_is_unavailable_and_empty() {
    let sysfs = FakeSysfs::new("hwmon-missing");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    assert_eq!(hwmon.health().availability, Availability::Unavailable);
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    assert!(sources(&pages).is_empty());
    assert_eq!(pages[0].availability, Some(Availability::Unavailable));
    let batch = hwmon.read(&read_plan(
        ADAPTER,
        "fast",
        1,
        0,
        deadline(&clock),
        Vec::new(),
    ));
    assert!(batch.records.is_empty());
}

#[test]
fn unreadable_class_directory_keeps_the_previous_catalog() {
    let sysfs = FakeSysfs::new("hwmon-unreadable");
    coretemp(&sysfs, 0);
    let faulty = Arc::new(FaultyFiles::new(sysfs.rooted()));
    let files: Arc<dyn SystemFiles> = faulty.clone();
    let clock = clock();
    let mut hwmon = HwmonAdapter::new(time(&clock), host(), files, AdapterOptions::default());
    let (generation, _) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    faulty.fail("/sys/class/hwmon", Failure::Os(5));
    let failed = hwmon.discover(&discover_request(ADAPTER, generation, 0, deadline(&clock)));
    assert!(!failed.complete);
    assert!(failed.removed_sources.is_empty());
    assert_eq!(failed.error_code.as_deref(), Some("HWMON_CLASS_IO_ERROR_5"));
    assert_eq!(failed.generation, generation);
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(readings.len(), 3, "sources stay readable");
}

#[test]
fn labels_are_sanitized() {
    let sysfs = FakeSysfs::new("hwmon-labels");
    let directory = sysfs.hwmon(0, "platform/nct6775.656", "nct6798");
    sysfs.write(
        &format!("{directory}/temp1_label"),
        "SYS\u{1b}[31mTIN\u{1b}]0;x\u{7}\n",
    );
    sysfs.write(&format!("{directory}/temp1_input"), "30000\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let descriptor = source(
        &catalog,
        &source_id("nct6798@platform/nct6775.656", "temp1"),
    );
    assert_eq!(descriptor.label.as_deref(), Some("SYSTIN"));
}

#[test]
fn source_limit_keeps_profile_proven_sources_first() {
    let sysfs = FakeSysfs::new("hwmon-limit");
    coretemp(&sysfs, 0);
    let superio = sysfs.hwmon(1, "platform/nct6775.656", "nct6798");
    for index in 1..=3 {
        sysfs.write(&format!("{superio}/temp{index}_input"), "30000\n");
    }
    let clock = clock();
    let options = AdapterOptions {
        max_sources: 2,
        ..AdapterOptions::default()
    };
    let mut hwmon = adapter(&sysfs, &clock, options);
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    assert_eq!(catalog.len(), 2);
    assert!(catalog.iter().all(|descriptor| descriptor.metric.is_some()));
    assert_eq!(
        pages[0].error_code.as_deref(),
        Some("SOURCE_LIMIT_EXCEEDED")
    );
    assert_eq!(hwmon.source_count(), 2);
}

#[test]
fn plans_with_slots_read_only_those_sources() {
    let sysfs = FakeSysfs::new("hwmon-slots");
    coretemp(&sysfs, 0);
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let package = source(&sources(&pages), &source_id(CORETEMP_KEY, "temp1")).slot;
    let batches = read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        vec![package, 999],
    );
    let readings = records(&batches);
    assert_eq!(readings.len(), 1);
    assert_eq!(
        batches[0].error_code.as_deref(),
        Some("UNKNOWN_SOURCE_SLOT")
    );
}

#[test]
fn power_and_energy_channels_keep_native_units() {
    let sysfs = FakeSysfs::new("hwmon-units");
    let directory = sysfs.hwmon(0, "pci0000:00/0000:03:00.0", "amdgpu");
    sysfs.write(&format!("{directory}/power1_input"), "45000000\n");
    sysfs.write(&format!("{directory}/power1_average"), "43000000\n");
    sysfs.write(
        &format!("{directory}/energy1_input"),
        "18446744073709551615\n",
    );
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (_, pages) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let key = "amdgpu@pci0000:00/0000:03:00.0";
    let input = source(&catalog, &source_id(key, "power1"));
    let average = source(&catalog, &source_id(key, "power1_average"));
    assert!(input.metric.is_none() && average.metric.is_none());
    assert_eq!((input.profile_rank, average.profile_rank), (0, 1));
    assert_eq!(input.origin, average.origin);
    let energy = source(&catalog, &source_id(key, "energy1"));
    assert_eq!(energy.raw_kind, RawKind::Counter);
    assert_eq!(energy.native_unit.as_str(), "uJ");
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(
        record(&readings, &source_id(key, "energy1"))
            .value
            .unwrap()
            .as_u64(),
        Some(u64::MAX)
    );
    assert_eq!(
        record(&readings, &source_id(key, "power1"))
            .key
            .metric_id
            .as_str(),
        ids::SENSOR_POWER
    );
}

#[test]
fn garbage_values_fail_without_numbers() {
    let sysfs = FakeSysfs::new("hwmon-garbage");
    let directory = sysfs.hwmon(0, "platform/nct6775.656", "nct6798");
    sysfs.write(&format!("{directory}/temp1_input"), "not-a-number\n");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let readings = records(&read_all(
        &mut hwmon,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(readings[0].read_status, ReadStatus::Error);
    assert!(readings[0].value.is_none());
    assert_eq!(readings[0].reason_codes, vec!["PARSE_ERROR".to_owned()]);
}

#[test]
fn invalid_requests_fail_without_records() {
    let sysfs = FakeSysfs::new("hwmon-invalid");
    coretemp(&sysfs, 0);
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let expired = hwmon.read(&read_plan(
        ADAPTER,
        "fast",
        1,
        0,
        clock.now_boot_ns() - 1,
        Vec::new(),
    ));
    assert_eq!(expired.status, BatchStatus::Failed);
    assert_eq!(expired.error_code.as_deref(), Some("DEADLINE_EXPIRED"));
    let unknown = hwmon.read(&read_plan(
        ADAPTER,
        "capacity",
        1,
        0,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(unknown.error_code.as_deref(), Some("UNKNOWN_READ_GROUP"));
    let wrong_adapter = hwmon.read(&read_plan(
        "thermal",
        "fast",
        1,
        0,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(
        wrong_adapter.error_code.as_deref(),
        Some("ADAPTER_ID_MISMATCH")
    );
    hwmon.shutdown();
    let after = hwmon.read(&read_plan(
        ADAPTER,
        "fast",
        2,
        0,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(after.error_code.as_deref(), Some("ADAPTER_SHUT_DOWN"));
    assert_eq!(hwmon.health().availability, Availability::Unavailable);
}

#[test]
fn manifest_is_a_valid_live_host_process() {
    let manifest = hwmon::manifest();
    manifest.validate().unwrap();
    assert_eq!(manifest.adapter_id.as_str(), "hwmon");
    assert_eq!(manifest.sample_namespace, Namespace::Live);
    assert_eq!(manifest.minimum_sample_ms, 1000);
    assert_eq!(manifest.source_limit, 2048);
    assert_eq!(manifest.runtime_library, None);
    assert_eq!(
        manifest.isolation,
        dgxtop_core::observation::Isolation::PersistentProcess
    );
}

#[test]
fn hotplug_churn_keeps_identity_and_generations_monotonic() {
    let sysfs = FakeSysfs::new("hwmon-churn");
    coretemp(&sysfs, 0);
    let device = "pci0000:00/0000:02:00.0/nvme/nvme0";
    let nvme_id = source_id("nvme@pci0000:00/0000:02:00.0/nvme/nvme0", "temp1");
    let clock = clock();
    let mut hwmon = adapter(&sysfs, &clock, AdapterOptions::default());
    let (mut generation, _) = discover(&mut hwmon, ADAPTER, 0, deadline(&clock));
    let mut slot = None;
    let mut source_generation = 0;
    for round in 0..1000u32 {
        let present = round % 2 == 0;
        if present {
            // Alternate enumeration indices like a real re-plug.
            let directory = sysfs.hwmon(1 + round % 3, device, "nvme");
            sysfs.write(&format!("{directory}/temp1_input"), "40000\n");
        } else {
            sysfs.remove_hwmon(1 + (round - 1) % 3, device);
        }
        let (next, pages) = discover(&mut hwmon, ADAPTER, generation, deadline(&clock));
        assert!(
            next > generation,
            "every hot-plug changes the catalog generation"
        );
        generation = next;
        let last = pages.last().unwrap();
        if present {
            let catalog = sources(&pages);
            assert_eq!(catalog.len(), 1, "only the re-plugged device is upserted");
            let descriptor = source(&catalog, &nvme_id);
            assert_eq!(*slot.get_or_insert(descriptor.slot), descriptor.slot);
            assert!(descriptor.generation > source_generation, "new incarnation");
            source_generation = descriptor.generation;
        } else {
            assert_eq!(last.removed_sources.len(), 1);
            assert_eq!(last.removed_sources[0].as_str(), nvme_id);
        }
    }
    assert_eq!(
        hwmon.source_count(),
        3,
        "coretemp sources survive the churn"
    );
}
