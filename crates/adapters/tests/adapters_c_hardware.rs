//! Hardware smoke tests against the real host's sysfs. Ignored by default;
//! run with `cargo test -p dgxtop-adapters -- --ignored hardware_ --nocapture`.
//! Assertions are generic (contract invariants) plus driver-specific checks
//! that only apply when the corresponding driver is present.
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests may unwrap

#[path = "fixtures/adapters_c/support.rs"]
mod support;

use std::sync::Arc;

use dgxtop_adapters::common::{RootedFiles, SystemFiles};
use dgxtop_adapters::hwmon::HwmonAdapter;
use dgxtop_adapters::powercap::PowercapAdapter;
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_adapters::thermal::ThermalAdapter;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::{CatalogDelta, Observation, SourceDescriptor};
use dgxtop_core::quality::ReadStatus;
use support::*;

fn host_files() -> Arc<dyn SystemFiles> {
    Arc::new(RootedFiles::host())
}

fn print_catalog(
    adapter: &str,
    pages: &[CatalogDelta],
    catalog: &[SourceDescriptor],
    readings: &[Observation],
) {
    println!(
        "[{adapter}] {} pages, {} sources, availability {:?}, error {:?}",
        pages.len(),
        catalog.len(),
        pages.last().and_then(|page| page.availability),
        pages.last().and_then(|page| page.error_code.clone())
    );
    for descriptor in catalog {
        let reading = readings
            .iter()
            .find(|reading| reading.provenance.source_id == descriptor.source_id);
        println!(
            "  {} metric={} origin={:?} profile={} rank={} label={:?} -> {:?} {:?} {:?}",
            descriptor.source_id,
            descriptor
                .metric
                .as_ref()
                .map_or("diagnostics".to_owned(), |key| key.series_selector()),
            descriptor
                .origin
                .origin_id
                .as_ref()
                .map(|origin| origin.as_str()),
            descriptor.profile_id,
            descriptor.profile_rank,
            descriptor.label,
            reading.map(|reading| reading.read_status),
            reading.and_then(|reading| reading.value),
            reading.map(|reading| &reading.reason_codes)
        );
    }
}

/// Contract invariants every real reading must satisfy.
fn assert_contract(readings: &[Observation]) {
    for reading in readings {
        reading.validate().unwrap();
        assert_eq!(reading.read_status.is_ok(), reading.value.is_some());
    }
}

#[test]
#[ignore = "reads the real host's /sys/class/hwmon"]
fn hardware_hwmon_smoke() {
    let clock = clock();
    let mut adapter = HwmonAdapter::new(
        time(&clock),
        host(),
        host_files(),
        AdapterOptions::default(),
    );
    let (_, pages) = discover(&mut adapter, "hwmon", 0, deadline(&clock));
    let catalog = sources(&pages);
    let readings = records(&read_all(
        &mut adapter,
        "hwmon",
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    print_catalog("hwmon", &pages, &catalog, &readings);
    assert_contract(&readings);
    let coretemp: Vec<&SourceDescriptor> = catalog
        .iter()
        .filter(|descriptor| descriptor.driver == "coretemp")
        .collect();
    if !coretemp.is_empty() {
        let package = coretemp
            .iter()
            .find(|descriptor| {
                descriptor
                    .metric
                    .as_ref()
                    .is_some_and(|key| key.metric_id.as_str() == ids::CPU_TEMPERATURE_PACKAGE)
            })
            .expect("coretemp exposes a proven package temperature");
        let reading = readings
            .iter()
            .find(|reading| reading.provenance.source_id == package.source_id)
            .unwrap();
        assert_eq!(reading.read_status, ReadStatus::Ok);
        let millidegrees = reading.value.unwrap().to_f64_lossy();
        assert!(
            (0.0..=125_000.0).contains(&millidegrees),
            "{millidegrees} mdegC"
        );
    }
    // Every catalogued channel is proven by a profile (D-92).
    for descriptor in &catalog {
        assert!(
            descriptor.metric.is_some(),
            "{} is unproven",
            descriptor.source_id
        );
    }
}

#[test]
#[ignore = "reads the real host's /sys/class/thermal"]
fn hardware_thermal_smoke() {
    let clock = clock();
    let mut adapter = ThermalAdapter::new(
        time(&clock),
        host(),
        host_files(),
        AdapterOptions::default(),
    );
    let (_, pages) = discover(&mut adapter, "thermal", 0, deadline(&clock));
    let catalog = sources(&pages);
    let readings = records(&read_all(
        &mut adapter,
        "thermal",
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    print_catalog("thermal", &pages, &catalog, &readings);
    assert_contract(&readings);
    for descriptor in &catalog {
        if let Some(key) = &descriptor.metric
            && key.metric_id.as_str() == ids::CPU_TEMPERATURE_PACKAGE
        {
            assert!(
                descriptor.label.as_deref() == Some("x86_pkg_temp") || descriptor.profile_rank == 2
            );
        }
    }
}

#[test]
#[ignore = "reads the real host's /sys/class/powercap"]
fn hardware_powercap_smoke() {
    let clock = clock();
    let mut adapter = PowercapAdapter::new(
        time(&clock),
        host(),
        host_files(),
        AdapterOptions::default(),
    );
    let (_, pages) = discover(&mut adapter, "powercap", 0, deadline(&clock));
    let catalog = sources(&pages);
    let readings = records(&read_all(
        &mut adapter,
        "powercap",
        "fast",
        1,
        deadline(&clock),
        Vec::new(),
    ));
    print_catalog("powercap", &pages, &catalog, &readings);
    assert_contract(&readings);
    let root = nix::unistd::geteuid().is_root();
    for reading in &readings {
        match reading.read_status {
            ReadStatus::Ok => assert!(
                reading.value.unwrap().as_u64().is_some(),
                "exact u64 counter"
            ),
            ReadStatus::PermissionDenied => {
                assert!(!root, "root should be able to read energy_uj");
                assert!(reading.value.is_none());
            }
            other => panic!(
                "unexpected status {other:?} for {}",
                reading.provenance.source_id
            ),
        }
    }
    if let Some(package) = catalog
        .iter()
        .find(|descriptor| descriptor.source_id.as_str() == "powercap:intel-rapl:0:energy_uj")
    {
        assert_eq!(
            package.metric.as_ref().unwrap().metric_id.as_str(),
            ids::CPU_ENERGY_PACKAGE
        );
        assert!(package.counter_modulus.is_some());
    }
}
