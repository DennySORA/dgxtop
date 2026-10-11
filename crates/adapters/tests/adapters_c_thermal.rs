//! Thermal-zone adapter integration tests over fake sysfs trees.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests may unwrap

#[path = "fixtures/adapters_c/support.rs"]
mod support;

use std::sync::Arc;

use dgxtop_adapters::common::{CountingFiles, SystemFiles};
use dgxtop_adapters::hwmon::HwmonAdapter;
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_adapters::thermal::{self, ThermalAdapter};
use dgxtop_core::identity::OriginRelation;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::MAX_ATTRIBUTES;
use dgxtop_core::quality::{Availability, Namespace, Quality, ReadStatus, Support};
use support::*;

const ADAPTER: &str = "thermal";

fn adapter(sysfs: &FakeSysfs, clock: &Arc<dgxtop_core::testing::FakeClock>) -> ThermalAdapter {
    ThermalAdapter::new(
        time(clock),
        host(),
        sysfs.files(),
        AdapterOptions::default(),
    )
}

/// Single-package host with acpitz, iwlwifi and x86_pkg_temp zones (like the dev box).
fn intel_laptop(sysfs: &FakeSysfs) {
    sysfs.cpus(&[0, 0, 0, 0]);
    sysfs.thermal_zone(
        0,
        "acpitz",
        "27800",
        Some("LNXSYSTM:00/LNXSYBUS:01/LNXTHERM:00"),
    );
    sysfs.thermal_zone(1, "iwlwifi_1", "35000", None);
    sysfs.thermal_zone(2, "x86_pkg_temp", "68000", None);
}

#[test]
fn x86_pkg_temp_maps_to_package_with_the_coretemp_origin() {
    let sysfs = FakeSysfs::new("thermal-alias");
    intel_laptop(&sysfs);
    let coretemp = sysfs.hwmon(6, "platform/coretemp.0", "coretemp");
    sysfs.write(&format!("{coretemp}/temp1_label"), "Package id 0\n");
    sysfs.write(&format!("{coretemp}/temp1_input"), "68000\n");
    let clock = clock();

    let mut zones = adapter(&sysfs, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let package = source(&catalog, "thermal:x86_pkg_temp:temp");
    let key = package
        .metric
        .as_ref()
        .expect("single package proves x86_pkg_temp");
    assert_eq!(key.entity_id.as_str(), "cpu.package.0");
    assert_eq!(key.metric_id.as_str(), ids::CPU_TEMPERATURE_PACKAGE);
    assert_eq!(key.scope.as_str(), "package:0");
    assert_eq!(package.profile_id.as_str(), "thermal-x86-pkg-temp");
    assert_eq!(package.profile_rank, 1);

    let mut hwmon = HwmonAdapter::new(
        time(&clock),
        host(),
        sysfs.files(),
        AdapterOptions::default(),
    );
    let (_, hwmon_pages) = discover(&mut hwmon, "hwmon", 0, deadline(&clock));
    let hwmon_catalog = sources(&hwmon_pages);
    let hwmon_package = source(&hwmon_catalog, "hwmon:coretemp@platform/coretemp.0:temp1");
    assert_eq!(hwmon_package.metric, package.metric, "same MetricKey");
    assert_eq!(hwmon_package.origin, package.origin, "same physical sensor");
    assert_eq!(package.origin.relation, OriginRelation::Confirmed);
    assert_eq!(
        package.origin.origin_id.as_ref().unwrap().as_str(),
        "x86_pkg_therm:0"
    );
    assert!(hwmon_package.profile_rank < package.profile_rank);

    let readings = records(&read_all(
        &mut zones,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let reading = record(&readings, "thermal:x86_pkg_temp:temp");
    assert_eq!(value_of(reading), 68000.0);
    assert_eq!(reading.quality, Quality::Verified);
    assert_eq!(reading.native_unit.as_str(), "mdegC");
}

#[test]
fn other_zones_are_diagnostics_with_unknown_origin() {
    let sysfs = FakeSysfs::new("thermal-diagnostics");
    intel_laptop(&sysfs);
    sysfs.thermal_zone(3, "INT3400 Thermal", "20000", None);
    sysfs.thermal_zone(4, "TCPU", "55000", None);
    let clock = clock();
    let mut zones = adapter(&sysfs, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    for id in [
        "thermal:acpitz@LNXSYSTM:00/LNXSYBUS:01/LNXTHERM:00:temp",
        "thermal:iwlwifi_1:temp",
        "thermal:INT3400_Thermal:temp",
        "thermal:TCPU:temp",
    ] {
        let descriptor = source(&catalog, id);
        assert!(descriptor.metric.is_none(), "{id}");
        assert_eq!(descriptor.origin.relation, OriginRelation::Unknown);
        assert_eq!(descriptor.profile_id.as_str(), "thermal-generic");
    }
    let readings = records(&read_all(
        &mut zones,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let tcpu = record(&readings, "thermal:TCPU:temp");
    assert_eq!(tcpu.key.metric_id.as_str(), ids::SENSOR_TEMPERATURE);
    assert_eq!(tcpu.key.entity_id.as_str(), "sensor:thermal:TCPU:temp");
    assert_eq!(tcpu.key.scope.as_str(), "sensor");
    assert_eq!(tcpu.quality, Quality::Unknown);
}

#[test]
fn unknown_topology_does_not_prove_a_package() {
    let sysfs = FakeSysfs::new("thermal-no-topology");
    sysfs.thermal_zone(0, "x86_pkg_temp", "60000", None);
    let clock = clock();
    let mut zones = adapter(&sysfs, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    assert!(sources(&pages)[0].metric.is_none());
}

#[test]
fn soc_zone_is_soc_temperature_not_cpu_package() {
    let sysfs = FakeSysfs::new("thermal-soc");
    sysfs.cpus(&[0, 0]);
    sysfs.thermal_zone(0, "soc_thermal", "42000", None);
    let coretemp = sysfs.hwmon(0, "platform/coretemp.0", "coretemp");
    sysfs.write(&format!("{coretemp}/temp1_label"), "Package id 0\n");
    sysfs.write(&format!("{coretemp}/temp1_input"), "68000\n");
    let clock = clock();
    let mut zones = adapter(&sysfs, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let soc = source(&catalog, "thermal:soc_thermal:temp");
    let soc_key = soc.metric.as_ref().unwrap();
    assert_eq!(soc_key.metric_id.as_str(), ids::SOC_TEMPERATURE);
    assert_eq!(soc_key.entity_id.as_str(), "soc");
    assert_eq!(soc_key.scope.as_str(), "soc");
    assert_eq!(soc.origin.relation, OriginRelation::Unknown);

    let mut hwmon = HwmonAdapter::new(
        time(&clock),
        host(),
        sysfs.files(),
        AdapterOptions::default(),
    );
    let (_, hwmon_pages) = discover(&mut hwmon, "hwmon", 0, deadline(&clock));
    let hwmon_catalog = sources(&hwmon_pages);
    let package = source(&hwmon_catalog, "hwmon:coretemp@platform/coretemp.0:temp1");
    assert_ne!(
        package.metric, soc.metric,
        "68 °C package and 42 °C SoC never compete"
    );

    let readings = records(&read_all(
        &mut zones,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let soc_reading = record(&readings, "thermal:soc_thermal:temp");
    assert_eq!(value_of(soc_reading), 42000.0);
    assert_eq!(soc_reading.quality, Quality::Usable);
    let hwmon_readings = records(&read_all(
        &mut hwmon,
        "hwmon",
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(value_of(&hwmon_readings[0]), 68000.0);
}

/// A GB10 host (DGX Spark): its seven zones are all `acpitz`, told apart by
/// their ACPI paths, on one package of 20 CPUs.
fn gb10(sysfs: &FakeSysfs, zones: &[&str]) {
    sysfs.cpus(&[36; 20]);
    for (index, name) in zones.iter().enumerate() {
        let device = format!("LNXSYSTM:00/LNXTHERM:{index:02}");
        let index = u32::try_from(index).unwrap();
        sysfs.thermal_zone(
            index,
            "acpitz",
            &format!("{}", 70_000 + index),
            Some(&device),
        );
        sysfs.write(
            &format!("/sys/devices/{device}/path"),
            &format!("\\_TZ_.{name}\n"),
        );
    }
}

fn zone_id(index: usize) -> String {
    format!("thermal:acpitz@LNXSYSTM:00/LNXTHERM:{index:02}:temp")
}

/// GB10 (D-68): the zones of its CPU clusters' E and P cores map to
/// `cpu.temperature.cluster` and its SoC zone to `soc.temperature`, both
/// usable (named by the firmware, not a datasheet); the GPU and uncore
/// zones stay diagnostics. Without all four cluster zones, or on more than
/// one package, no zone is proven.
#[test]
fn gb10_zones_map_by_their_acpi_paths() {
    let names = ["TSOC", "TS0E", "TS0P", "TS1E", "TS1P", "TGPU", "TUNC"];
    let sysfs = FakeSysfs::new("thermal-gb10");
    gb10(&sysfs, &names);
    let clock = clock();
    let mut zones = adapter(&sysfs, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let mapped = |index: usize| {
        let descriptor = source(&catalog, &zone_id(index));
        descriptor.metric.as_ref().map(|key| {
            (
                key.entity_id.as_str().to_owned(),
                key.metric_id.as_str().to_owned(),
                key.scope.as_str().to_owned(),
                descriptor.profile_id.as_str().to_owned(),
            )
        })
    };
    let cluster = |scope: &str| {
        Some((
            "cpu.package.0".to_owned(),
            ids::CPU_TEMPERATURE_CLUSTER.to_owned(),
            scope.to_owned(),
            "thermal-gb10".to_owned(),
        ))
    };
    assert_eq!(
        mapped(0),
        Some((
            "soc".to_owned(),
            ids::SOC_TEMPERATURE.to_owned(),
            "soc".to_owned(),
            "thermal-gb10".to_owned(),
        ))
    );
    assert_eq!(mapped(1), cluster("cluster:0e"));
    assert_eq!(mapped(2), cluster("cluster:0p"));
    assert_eq!(mapped(3), cluster("cluster:1e"));
    assert_eq!(mapped(4), cluster("cluster:1p"));
    assert_eq!(mapped(5), None, "TGPU: NVML reports the GPU");
    assert_eq!(mapped(6), None, "TUNC");
    let readings = records(&read_all(
        &mut zones,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let hottest_p = record(&readings, &zone_id(4));
    assert_eq!(
        hottest_p.key.metric_id.as_str(),
        ids::CPU_TEMPERATURE_CLUSTER
    );
    assert_eq!(value_of(hottest_p), 70_004.0);
    assert_eq!(hottest_p.quality, Quality::Usable);

    let partial = FakeSysfs::new("thermal-gb10-partial");
    gb10(&partial, &["TSOC", "TS0E", "TS0P"]);
    let mut zones = adapter(&partial, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    assert!(sources(&pages).iter().all(|source| source.metric.is_none()));

    let two = FakeSysfs::new("thermal-gb10-two-packages");
    gb10(&two, &names);
    two.cpus(&[0, 1]);
    let mut zones = adapter(&two, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    assert!(sources(&pages).iter().all(|source| source.metric.is_none()));
}

#[test]
fn arm_cpu_thermal_is_usable_rank_two_on_one_package() {
    let sysfs = FakeSysfs::new("thermal-arm");
    sysfs.cpus(&[0, 0, 0, 0]);
    sysfs.thermal_zone(
        0,
        "cpu-thermal",
        "51000",
        Some("platform/thermal-sensor@1c25000"),
    );
    let clock = clock();
    let mut zones = adapter(&sysfs, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    let cpu = source(
        &catalog,
        "thermal:cpu-thermal@platform/thermal-sensor@1c25000:temp",
    );
    assert_eq!(
        cpu.metric.as_ref().unwrap().metric_id.as_str(),
        ids::CPU_TEMPERATURE_PACKAGE
    );
    assert_eq!(cpu.profile_rank, 2);
    assert_eq!(cpu.origin.relation, OriginRelation::Unknown);
}

#[test]
fn disabled_zone_is_unsupported_and_reprobed_with_throttling() {
    let sysfs = FakeSysfs::new("thermal-disabled");
    sysfs.cpus(&[0]);
    let zone = sysfs.thermal_zone(0, "acpitz", "30000", None);
    sysfs.write(&format!("{zone}/mode"), "disabled\n");
    let counting = Arc::new(CountingFiles::new(sysfs.rooted()));
    let files: Arc<dyn SystemFiles> = counting.clone();
    let clock = clock();
    let mut zones = ThermalAdapter::new(time(&clock), host(), files, AdapterOptions::default());
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    let capability = &pages[0].capabilities[0];
    assert_eq!(capability.support, Support::Unsupported);
    assert_eq!(capability.reason.as_deref(), Some("ZONE_DISABLED"));
    assert_eq!(pages[0].availability, Some(Availability::Unavailable));

    let readings = records(&read_all(
        &mut zones,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(readings[0].read_status, ReadStatus::Unsupported);
    assert!(readings[0].value.is_none());
    assert_eq!(readings[0].reason_codes, vec!["ZONE_DISABLED".to_owned()]);
    let mode_reads = counting.reads_of(&format!("{zone}/mode"));
    for cycle in 2..6 {
        clock.advance_ms(1_000);
        read_all(
            &mut zones,
            ADAPTER,
            "fast",
            cycle,
            deadline(&clock),
            Vec::new(),
        );
    }
    assert_eq!(counting.reads_of(&format!("{zone}/mode")), mode_reads);
    assert_eq!(
        counting.reads_of(&format!("{zone}/temp")),
        0,
        "disabled zone temp is not read"
    );

    sysfs.write(&format!("{zone}/mode"), "enabled\n");
    clock.advance_ms(60_000);
    let readings = records(&read_all(
        &mut zones,
        ADAPTER,
        "fast",
        7,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(readings[0].read_status, ReadStatus::Ok);
    assert_eq!(value_of(&readings[0]), 30000.0);
}

#[test]
fn zone_identity_survives_renumbering_when_a_device_link_exists() {
    let sysfs = FakeSysfs::new("thermal-renumber");
    sysfs.cpus(&[0]);
    let device = "LNXSYSTM:00/LNXSYBUS:01/LNXTHERM:00";
    sysfs.thermal_zone(0, "acpitz", "27800", Some(device));
    let clock = clock();
    let mut zones = adapter(&sysfs, &clock);
    let (generation, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    let before = sources(&pages);
    sysfs.remove("/sys/class/thermal/thermal_zone0");
    sysfs.remove("/sys/devices/virtual/thermal/thermal_zone0");
    sysfs.thermal_zone(5, "acpitz", "28800", Some(device));
    let (next, pages) = discover(&mut zones, ADAPTER, generation, deadline(&clock));
    assert!(next > generation);
    let after = sources(&pages);
    assert_eq!(before[0].source_id, after[0].source_id);
    assert_eq!(before[0].slot, after[0].slot);
    assert!(after[0].generation > before[0].generation);
    assert!(pages.iter().all(|page| page.removed_sources.is_empty()));
}

#[test]
fn trip_points_are_bounded_entity_attributes_not_series() {
    let sysfs = FakeSysfs::new("thermal-trips");
    sysfs.cpus(&[0]);
    let zone = sysfs.thermal_zone(0, "acpitz", "27800", None);
    for trip in 0..15 {
        sysfs.write(&format!("{zone}/trip_point_{trip}_type"), "passive\n");
        sysfs.write(
            &format!("{zone}/trip_point_{trip}_temp"),
            &format!("{}\n", 90000 + trip),
        );
    }
    sysfs.write(&format!("{zone}/policy"), "step_wise\n");
    let clock = clock();
    let mut zones = adapter(&sysfs, &clock);
    let (_, pages) = discover(&mut zones, ADAPTER, 0, deadline(&clock));
    assert_eq!(sources(&pages).len(), 1, "trip points never become sources");
    let entity = pages
        .iter()
        .flat_map(|page| page.entities.iter())
        .find(|entity| entity.entity_id.as_str() == "sensor:thermal:acpitz:temp")
        .unwrap();
    entity.validate().unwrap();
    assert!(entity.attributes.len() <= MAX_ATTRIBUTES);
    assert_eq!(entity.attribute("trip_0"), Some("passive 90000 mdegC"));
    assert_eq!(entity.attribute("policy"), Some("step_wise"));
    assert_eq!(entity.attribute("type"), Some("acpitz"));
}

#[test]
fn zero_and_negative_zone_temperatures_are_kept() {
    let sysfs = FakeSysfs::new("thermal-cold");
    sysfs.cpus(&[0]);
    sysfs.thermal_zone(0, "outdoor", "0", None);
    sysfs.thermal_zone(1, "battery", "-5000", None);
    let clock = clock();
    let mut zones = adapter(&sysfs, &clock);
    discover(&mut zones, ADAPTER, 0, deadline(&clock));
    let readings = records(&read_all(
        &mut zones,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(value_of(record(&readings, "thermal:outdoor:temp")), 0.0);
    assert_eq!(value_of(record(&readings, "thermal:battery:temp")), -5000.0);
}

#[test]
fn manifest_is_a_valid_live_host_process() {
    let manifest = thermal::manifest();
    manifest.validate().unwrap();
    assert_eq!(manifest.adapter_id.as_str(), "thermal");
    assert_eq!(manifest.sample_namespace, Namespace::Live);
}
