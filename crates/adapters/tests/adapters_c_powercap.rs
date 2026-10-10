//! Powercap (RAPL) adapter integration tests over fake sysfs trees.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests may unwrap

#[path = "fixtures/adapters_c/support.rs"]
mod support;

use std::io;
use std::sync::Arc;

use dgxtop_adapters::common::{CountingFiles, SystemFiles};
use dgxtop_adapters::powercap::{self, PowercapAdapter};
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_core::identity::OriginRelation;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::EntityKind;
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, Namespace, Quality, RawKind, ReadStatus, Support};
use support::*;

const ADAPTER: &str = "powercap";
const MODULUS: &str = "262143328850";

fn rapl(sysfs: &FakeSysfs) {
    sysfs.mkdir("/sys/devices/virtual/powercap/intel-rapl");
    sysfs.symlink(
        "/sys/class/powercap/intel-rapl",
        "/sys/devices/virtual/powercap/intel-rapl",
    );
    sysfs.powercap_zone(
        "intel-rapl:0",
        "package-0",
        Some("123456789"),
        Some(MODULUS),
    );
    sysfs.powercap_zone("intel-rapl:0:0", "core", Some("23456789"), Some(MODULUS));
    sysfs.powercap_zone("intel-rapl:1", "psys", Some("987654321"), Some(MODULUS));
}

fn adapter_with(
    files: Arc<dyn SystemFiles>,
    clock: &Arc<dgxtop_core::testing::FakeClock>,
) -> PowercapAdapter {
    PowercapAdapter::new(time(clock), host(), files, AdapterOptions::default())
}

#[test]
fn package_and_psys_are_distinct_metrics_and_subdomains_are_diagnostics() {
    let sysfs = FakeSysfs::new("powercap-map");
    rapl(&sysfs);
    let clock = clock();
    let mut adapter = adapter_with(sysfs.files(), &clock);
    let (_, pages) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    assert_eq!(catalog.len(), 3, "the control-type directory is not a zone");

    let package = source(&catalog, "powercap:intel-rapl:0:energy_uj");
    let package_key = package.metric.as_ref().unwrap();
    assert_eq!(package_key.metric_id.as_str(), ids::CPU_ENERGY_PACKAGE);
    assert_eq!(package_key.entity_id.as_str(), "cpu.package.0");
    assert_eq!(package_key.scope.as_str(), "package:0");
    assert_eq!(package.origin.relation, OriginRelation::Confirmed);
    assert_eq!(package.raw_kind, RawKind::Counter);
    assert_eq!(package.native_unit.as_str(), "uJ");
    assert_eq!(package.label.as_deref(), Some("package-0"));
    assert_eq!(package.driver, "intel-rapl");

    let psys = source(&catalog, "powercap:intel-rapl:1:energy_uj");
    let psys_key = psys.metric.as_ref().unwrap();
    assert_eq!(psys_key.metric_id.as_str(), ids::CPU_ENERGY_PLATFORM);
    assert_eq!(psys_key.entity_id.as_str(), "host");
    assert_eq!(psys_key.scope.as_str(), "platform");
    assert_ne!(
        psys.metric, package.metric,
        "platform energy is not package energy"
    );

    let core = source(&catalog, "powercap:intel-rapl:0:0:energy_uj");
    assert!(core.metric.is_none());
    assert_eq!(
        core.entity_id.as_str(),
        "sensor:powercap:intel-rapl:0:0:energy_uj"
    );

    let domains: Vec<_> = pages
        .iter()
        .flat_map(|page| page.entities.iter())
        .filter(|entity| entity.kind == EntityKind::PowerDomain)
        .collect();
    assert_eq!(domains.len(), 3);
    let subdomain = domains
        .iter()
        .find(|entity| entity.entity_id.as_str() == "power:intel-rapl:0:0")
        .unwrap();
    assert_eq!(
        subdomain.parent_id.as_ref().unwrap().as_str(),
        "power:intel-rapl:0"
    );

    let readings = records(&read_all(
        &mut adapter,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    let package_reading = record(&readings, "powercap:intel-rapl:0:energy_uj");
    assert_eq!(package_reading.value.unwrap().as_u64(), Some(123_456_789));
    assert_eq!(package_reading.quality, Quality::Verified);
    assert_eq!(
        record(&readings, "powercap:intel-rapl:1:energy_uj").quality,
        Quality::Usable
    );
    assert_eq!(
        record(&readings, "powercap:intel-rapl:0:0:energy_uj").quality,
        Quality::Unknown
    );
}

#[test]
fn counter_modulus_comes_from_max_energy_range() {
    let sysfs = FakeSysfs::new("powercap-modulus");
    rapl(&sysfs);
    sysfs.powercap_zone("intel-rapl:2", "package-1", Some("1"), None);
    let clock = clock();
    let mut adapter = adapter_with(sysfs.files(), &clock);
    let (_, pages) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    assert_eq!(
        source(&catalog, "powercap:intel-rapl:0:energy_uj").counter_modulus,
        Some(262_143_328_850)
    );
    assert_eq!(
        source(&catalog, "powercap:intel-rapl:2:energy_uj").counter_modulus,
        None,
        "no declared range, no guessed modulus"
    );
}

#[test]
fn counters_stay_exact_u64() {
    let sysfs = FakeSysfs::new("powercap-exact");
    sysfs.powercap_zone(
        "intel-rapl:0",
        "package-0",
        Some("18446744073709551615"),
        Some(MODULUS),
    );
    let clock = clock();
    let mut adapter = adapter_with(sysfs.files(), &clock);
    discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    let readings = records(&read_all(
        &mut adapter,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    assert_eq!(readings[0].value.unwrap().as_u64(), Some(u64::MAX));
}

#[test]
fn permission_denied_energy_is_reported_never_zero() {
    let sysfs = FakeSysfs::new("powercap-denied");
    rapl(&sysfs);
    let faulty = FaultyFiles::new(sysfs.rooted());
    for zone in [
        "intel-rapl:0",
        "intel-rapl:0/intel-rapl:0:0",
        "intel-rapl:1",
    ] {
        faulty.fail(
            &format!("/sys/devices/virtual/powercap/intel-rapl/{zone}/energy_uj"),
            Failure::Kind(io::ErrorKind::PermissionDenied),
        );
    }
    let clock = clock();
    let mut adapter = adapter_with(Arc::new(faulty), &clock);
    let (_, pages) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    assert_eq!(pages[0].availability, Some(Availability::PermissionDenied));
    let capability = pages
        .iter()
        .flat_map(|page| page.capabilities.iter())
        .find(|capability| capability.capability_id == "cpu.energy.package|package:0")
        .unwrap();
    assert_eq!(capability.support, Support::Unsupported);
    assert_eq!(capability.reason.as_deref(), Some("PERMISSION_DENIED"));

    let readings = records(&read_all(
        &mut adapter,
        ADAPTER,
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    for reading in &readings {
        assert_eq!(reading.read_status, ReadStatus::PermissionDenied);
        assert!(reading.value.is_none(), "permission denied never becomes 0");
        assert_eq!(reading.reason_codes, vec!["PERMISSION_DENIED".to_owned()]);
    }
    assert_eq!(
        adapter.health().availability,
        Availability::PermissionDenied
    );
}

#[test]
fn denied_energy_is_reprobed_at_most_once_per_interval() {
    let sysfs = FakeSysfs::new("powercap-throttle");
    let zone = sysfs.powercap_zone("intel-rapl:0", "package-0", Some("42"), Some(MODULUS));
    let energy = format!("{zone}/energy_uj");
    let faulty = FaultyFiles::new(sysfs.rooted());
    faulty.fail(&energy, Failure::Kind(io::ErrorKind::PermissionDenied));
    let counting = Arc::new(CountingFiles::new(faulty));
    let files: Arc<dyn SystemFiles> = counting.clone();
    let clock = clock();
    let mut adapter = adapter_with(files, &clock);
    let mut generation = discover(&mut adapter, ADAPTER, 0, deadline(&clock)).0;
    for cycle in 1..=10 {
        clock.advance_ms(1_000);
        read_all(
            &mut adapter,
            ADAPTER,
            "fast",
            cycle,
            deadline(&clock),
            Vec::new(),
        );
        if cycle % 5 == 0 {
            generation = discover(&mut adapter, ADAPTER, generation, deadline(&clock)).0;
        }
    }
    assert_eq!(
        counting.reads_of(&energy),
        1,
        "denied capability is not re-read every cycle"
    );
    clock.advance_ms(60_000);
    read_all(
        &mut adapter,
        ADAPTER,
        "fast",
        11,
        deadline(&clock),
        Vec::new(),
    );
    assert_eq!(counting.reads_of(&energy), 2);
}

#[test]
fn mmio_duplicates_and_die_domains_stay_diagnostics() {
    let sysfs = FakeSysfs::new("powercap-mmio");
    sysfs.powercap_zone("intel-rapl:0", "package-0", Some("1"), Some(MODULUS));
    sysfs.powercap_zone("intel-rapl-mmio:0", "package-0", Some("2"), Some(MODULUS));
    sysfs.powercap_zone("intel-rapl:1", "package-0-die-1", Some("3"), Some(MODULUS));
    let clock = clock();
    let mut adapter = adapter_with(sysfs.files(), &clock);
    let (_, pages) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    let catalog = sources(&pages);
    assert!(
        source(&catalog, "powercap:intel-rapl:0:energy_uj")
            .metric
            .is_some()
    );
    assert!(
        source(&catalog, "powercap:intel-rapl-mmio:0:energy_uj")
            .metric
            .is_none()
    );
    assert!(
        source(&catalog, "powercap:intel-rapl:1:energy_uj")
            .metric
            .is_none()
    );
}

#[test]
fn zones_without_energy_counters_are_skipped() {
    let sysfs = FakeSysfs::new("powercap-no-energy");
    sysfs.powercap_zone("intel-rapl:0", "package-0", None, Some(MODULUS));
    let clock = clock();
    let mut adapter = adapter_with(sysfs.files(), &clock);
    let (_, pages) = discover(&mut adapter, ADAPTER, 0, deadline(&clock));
    assert!(sources(&pages).is_empty());
    assert_eq!(pages[0].availability, Some(Availability::Unavailable));
}

#[test]
fn manifest_is_a_valid_live_host_process() {
    let manifest = powercap::manifest();
    manifest.validate().unwrap();
    assert_eq!(manifest.adapter_id.as_str(), "powercap");
    assert_eq!(manifest.sample_namespace, Namespace::Live);
    assert_eq!(manifest.max_inflight, 1);
}
