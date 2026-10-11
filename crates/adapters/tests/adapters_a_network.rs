//! Network adapter against fake `/sys/class/net` trees.

#[path = "fixtures/adapters_a/support.rs"]
mod support;

use std::sync::Arc;

use dgxtop_adapters::common::SystemFiles;
use dgxtop_adapters::network::{NetworkAdapter, manifest};
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::BatchStatus;
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, ReadStatus};
use dgxtop_core::testing::{FakeClock, host_id};
use support::*;

const ADAPTER: &str = "network";

const COUNTERS: [&str; 8] = [
    "rx_bytes",
    "tx_bytes",
    "rx_packets",
    "tx_packets",
    "rx_errors",
    "tx_errors",
    "rx_dropped",
    "tx_dropped",
];

fn write_interface(tree: &FakeTree, name: &str, ifindex: u32, kind: u32, base_value: u64) {
    let base = format!("/sys/class/net/{name}");
    tree.write(&format!("{base}/ifindex"), &format!("{ifindex}\n"));
    tree.write(&format!("{base}/operstate"), "up\n");
    tree.write(&format!("{base}/type"), &format!("{kind}\n"));
    for (offset, counter) in (0u64..).zip(COUNTERS) {
        tree.write(
            &format!("{base}/statistics/{counter}"),
            &format!("{}\n", base_value + offset),
        );
    }
}

fn network_tree(label: &str) -> FakeTree {
    let tree = FakeTree::new(label);
    write_namespaces(&tree);
    write_interface(&tree, "lo", 1, 772, 100);
    write_interface(&tree, "eth0", 2, 1, 1000);
    tree.write("/sys/class/net/bonding_masters", "\n");
    tree
}

fn adapter_with(files: Arc<dyn SystemFiles>, page_size: usize) -> NetworkAdapter {
    NetworkAdapter::new(
        FakeClock::new(1_000_000_000),
        host_id("test-host"),
        files,
        AdapterOptions {
            page_size,
            ..AdapterOptions::default()
        },
    )
}

fn entity_of(ifindex: u32) -> String {
    format!("netif:{NET_NAMESPACE}:{ifindex}")
}

#[test]
fn counters_are_exact_u64_values() {
    let tree = network_tree("net-read");
    tree.write(
        "/sys/class/net/eth0/statistics/rx_bytes",
        "18446744073709551615\n",
    );
    let mut adapter = adapter_with(tree.files(), 256);
    ParentCatalog::default().sync(&mut adapter, ADAPTER);
    let batches = read_all(&mut adapter, ADAPTER, "fast", 1);
    assert_eq!(batches[0].status, BatchStatus::Ok);
    let records = records(&batches);
    assert_eq!(records.len(), 16);
    assert_eq!(
        u64_value(find(&records, ids::NET_RX_BYTES, &entity_of(2))),
        u64::MAX
    );
    assert_eq!(
        u64_value(find(&records, ids::NET_TX_BYTES, &entity_of(2))),
        1001
    );
    assert_eq!(
        u64_value(find(&records, ids::NET_TX_DROPS, &entity_of(1))),
        107
    );
    for record in &records {
        record.validate().unwrap();
        assert_eq!(record.device_sample_boot_ns, None);
    }
}

#[test]
fn unreadable_counter_has_no_value() {
    let tree = network_tree("net-garbage");
    tree.write("/sys/class/net/eth0/statistics/rx_errors", "lots\n");
    tree.remove("/sys/class/net/eth0/statistics/tx_errors");
    let faulty = tree.faulty();
    faulty.inject("/sys/class/net/eth0/statistics/rx_dropped", 13);
    let mut adapter = adapter_with(faulty.clone(), 256);
    ParentCatalog::default().sync(&mut adapter, ADAPTER);
    let batches = read_all(&mut adapter, ADAPTER, "fast", 1);
    assert_eq!(batches[0].status, BatchStatus::Partial);
    let records = records(&batches);
    let garbage = find(&records, ids::NET_RX_ERRORS, &entity_of(2));
    assert_eq!(
        (garbage.read_status, garbage.value),
        (ReadStatus::Error, None)
    );
    let missing = find(&records, ids::NET_TX_ERRORS, &entity_of(2));
    assert_eq!(
        (missing.read_status, missing.value),
        (ReadStatus::Removed, None)
    );
    let denied = find(&records, ids::NET_RX_DROPS, &entity_of(2));
    assert_eq!(
        (denied.read_status, denied.value),
        (ReadStatus::PermissionDenied, None)
    );
    assert_eq!(
        u64_value(find(&records, ids::NET_RX_BYTES, &entity_of(2))),
        1000
    );
}

#[test]
fn permission_bits_are_honored() {
    if running_as_root() {
        eprintln!("skipped: permission bits are not enforced for euid 0");
        return;
    }
    let tree = network_tree("net-chmod");
    let mut adapter = adapter_with(tree.files(), 256);
    ParentCatalog::default().sync(&mut adapter, ADAPTER);
    tree.chmod("/sys/class/net/eth0/statistics/tx_packets", 0o000);
    let records = records(&read_all(&mut adapter, ADAPTER, "fast", 1));
    let denied = find(&records, ids::NET_TX_PACKETS, &entity_of(2));
    assert_eq!(
        (denied.read_status, denied.value),
        (ReadStatus::PermissionDenied, None)
    );
    tree.chmod("/sys/class/net/eth0/statistics/tx_packets", 0o644);
}

#[test]
fn recreated_interface_never_continues_the_old_counter() {
    let tree = network_tree("net-recreate");
    let mut adapter = adapter_with(tree.files(), 256);
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    let old_generation = parent.entity(&entity_of(2)).generation;
    tree.remove("/sys/class/net/eth0");
    write_interface(&tree, "eth0", 9, 1, 5);
    let records = records(&read_all(&mut adapter, ADAPTER, "fast", 1));
    for record in records
        .iter()
        .filter(|record| record.key.entity_id.as_str() == entity_of(2))
    {
        assert_eq!(record.read_status, ReadStatus::Removed);
        assert_eq!(record.value, None);
        assert_eq!(record.reason_codes, vec!["INTERFACE_RECREATED".to_owned()]);
    }
    let pages = parent.sync(&mut adapter, ADAPTER);
    let last = pages.last().unwrap();
    let removed: Vec<&str> = last.removed_entities.iter().map(|id| id.as_str()).collect();
    assert_eq!(removed, vec![entity_of(2).as_str()]);
    assert_eq!(last.removed_sources.len(), 8);
    let fresh = parent.entity(&entity_of(9));
    assert!(fresh.generation > old_generation);
    assert_eq!(fresh.display_name, "eth0");
    let records = support::records(&read_all(&mut adapter, ADAPTER, "fast", 2));
    assert_eq!(
        u64_value(find(&records, ids::NET_RX_BYTES, &entity_of(9))),
        5
    );
}

#[test]
fn removed_interface_only_removes_itself() {
    let tree = network_tree("net-remove");
    let mut adapter = adapter_with(tree.files(), 256);
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    tree.remove("/sys/class/net/eth0");
    let records = records(&read_all(&mut adapter, ADAPTER, "fast", 1));
    assert!(
        records
            .iter()
            .filter(|record| record.key.entity_id.as_str() == entity_of(1))
            .all(|record| record.read_status == ReadStatus::Ok)
    );
    let pages = parent.sync(&mut adapter, ADAPTER);
    assert_eq!(pages.last().unwrap().removed_entities.len(), 1);
    assert_eq!(parent.entities.len(), 1);
    let records = support::records(&read_all(&mut adapter, ADAPTER, "fast", 2));
    assert_eq!(records.len(), 8);
}

#[test]
fn transient_ifindex_failure_keeps_the_interface() {
    let tree = network_tree("net-transient");
    let faulty = tree.faulty();
    let mut adapter = adapter_with(faulty.clone(), 256);
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    faulty.inject("/sys/class/net/eth0/ifindex", 5);
    let pages = parent.sync(&mut adapter, ADAPTER);
    let last = pages.last().unwrap();
    assert!(last.removed_entities.is_empty());
    assert!(
        last.error_code
            .as_deref()
            .unwrap()
            .contains("IFINDEX_UNREADABLE")
    );
    assert!(parent.entities.contains_key(&entity_of(2)));
}

#[test]
fn many_interfaces_are_paged() {
    let tree = FakeTree::new("net-paged");
    write_namespaces(&tree);
    for index in 0..10u32 {
        write_interface(&tree, &format!("ens{index}"), 10 + index, 1, 1);
    }
    let mut adapter = adapter_with(tree.files(), 16);
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(&mut adapter, ADAPTER);
    assert_eq!(pages.len(), 6, "90 upserts in pages of 16");
    let batches = read_all(&mut adapter, ADAPTER, "fast", 1);
    assert_eq!(batches.len(), 5);
    assert_eq!(records(&batches).len(), 80);
}

#[test]
fn missing_namespace_or_class_directory_is_unavailable() {
    let tree = FakeTree::new("net-unavailable");
    write_interface(&tree, "eth0", 2, 1, 1);
    let mut adapter = adapter_with(tree.files(), 256);
    let pages = discover_all(&mut adapter, ADAPTER, 0);
    assert_eq!(pages[0].availability, Some(Availability::Unavailable));
    assert_eq!(
        pages[0].error_code.as_deref(),
        Some("NET_NAMESPACE_UNKNOWN")
    );
    assert!(pages[0].entities.is_empty());

    let tree = FakeTree::new("net-noclass");
    write_namespaces(&tree);
    let mut adapter = adapter_with(tree.files(), 256);
    let pages = discover_all(&mut adapter, ADAPTER, 0);
    assert_eq!(pages[0].availability, Some(Availability::Unavailable));
    assert!(
        pages[0]
            .error_code
            .as_deref()
            .unwrap()
            .starts_with("SYSFS_NET_UNREADABLE")
    );
}

#[test]
fn manifest_and_guards() {
    let manifest = manifest();
    manifest.validate().unwrap();
    assert_eq!(manifest.adapter_id.as_str(), "network");
    assert_eq!(manifest.minimum_sample_ms, 100);
    assert_eq!(manifest.source_limit, 2048);
    let tree = network_tree("net-guards");
    let mut adapter = adapter_with(tree.files(), 256);
    let before = adapter.read(&read_plan(ADAPTER, "fast", 1, 0, Vec::new()));
    assert_eq!(before.error_code.as_deref(), Some("CATALOG_NOT_DISCOVERED"));
    ParentCatalog::default().sync(&mut adapter, ADAPTER);
    let wrong = adapter.read(&read_plan(ADAPTER, "capacity", 1, 0, Vec::new()));
    assert_eq!(wrong.error_code.as_deref(), Some("UNKNOWN_READ_GROUP"));
    adapter.shutdown();
    let after = adapter.read(&read_plan(ADAPTER, "fast", 2, 0, Vec::new()));
    assert_eq!(after.status, BatchStatus::Failed);
}

#[test]
fn operstate_change_is_an_attribute_update_not_a_new_incarnation() {
    let tree = network_tree("net-operstate");
    let mut adapter = adapter_with(tree.files(), 256);
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, ADAPTER);
    let generation = parent.entity(&entity_of(2)).generation;
    tree.write("/sys/class/net/eth0/operstate", "down\n");
    let pages = parent.sync(&mut adapter, ADAPTER);
    assert_eq!(pages[0].entities.len(), 1);
    assert!(pages[0].sources.is_empty());
    let eth0 = parent.entity(&entity_of(2));
    assert_eq!(eth0.attribute("operstate"), Some("down"));
    assert_eq!(eth0.generation, generation);
}
