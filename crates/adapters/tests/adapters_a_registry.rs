//! Registry: manifests and construction of the seven live adapters.

use dgxtop_adapters::registry::{
    AdapterOptions, LIVE_ADAPTERS, RegistryError, build_live_adapter, live_manifest,
};
use dgxtop_core::naming::adapters;
use dgxtop_core::observation::{AdapterOperation, Isolation};
use dgxtop_core::quality::Namespace;
use dgxtop_core::testing::{FakeClock, host_id};

#[test]
fn live_adapter_list_is_the_naming_contract() {
    assert_eq!(LIVE_ADAPTERS, adapters::LIVE);
    assert_eq!(LIVE_ADAPTERS.len(), 7);
}

#[test]
fn every_live_manifest_is_valid_and_kernel_adapters_follow_the_contract() {
    for adapter in LIVE_ADAPTERS {
        let manifest = live_manifest(adapter).unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.adapter_id.as_str(), adapter);
        assert_eq!(manifest.isolation, Isolation::PersistentProcess);
        assert_eq!(manifest.max_inflight, 1);
        assert_eq!(manifest.sample_namespace, Namespace::Live);
    }
    for (adapter, minimum) in [("procfs", 100), ("network", 100), ("filesystem", 1000)] {
        let manifest = live_manifest(adapter).unwrap();
        assert_eq!(manifest.minimum_sample_ms, minimum, "{adapter}");
        assert_eq!(manifest.runtime_library, None);
        assert_eq!(manifest.source_limit, 2048);
        assert_eq!(
            manifest.operations,
            vec![
                AdapterOperation::Discover,
                AdapterOperation::Read,
                AdapterOperation::Shutdown
            ]
        );
    }
}

#[test]
fn every_live_adapter_can_be_built_with_its_options() {
    let options = AdapterOptions {
        unsupported_probe_ms: 30_000,
        max_sources: 300,
        page_size: 64,
    };
    for adapter in LIVE_ADAPTERS {
        let port =
            build_live_adapter(adapter, FakeClock::new(0), host_id("h"), options.clone()).unwrap();
        assert_eq!(port.manifest().adapter_id.as_str(), adapter);
        if ["procfs", "network", "filesystem"].contains(&adapter) {
            assert_eq!(port.manifest().source_limit, 300, "{adapter}");
        }
    }
}

#[test]
fn unknown_and_replay_ids_are_rejected() {
    let clock = FakeClock::new(0);
    for (adapter, expected_live) in [
        ("bogus", false),
        ("libnvidia-ml.so", false),
        ("replay", true),
        ("replay-2", true),
    ] {
        let error = build_live_adapter(
            adapter,
            clock.clone(),
            host_id("h"),
            AdapterOptions::default(),
        )
        .err()
        .unwrap();
        match error {
            RegistryError::NotLive { adapter_id } => {
                assert!(expected_live, "{adapter}");
                assert_eq!(adapter_id, adapter);
            }
            RegistryError::UnknownAdapter { adapter_id } => {
                assert!(!expected_live, "{adapter}");
                assert_eq!(adapter_id, adapter);
            }
        }
    }
}
