//! Catalog behavior shared by the adapters-A family: paged discovery
//! (AT15), hotplug boundedness and slot stability (AT15), source limits
//! (AT15), metadata caching and unsupported re-probe throttling (AT34).

#[path = "fixtures/adapters_a/support.rs"]
mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use dgxtop_adapters::common::SystemFiles;
use dgxtop_adapters::network::NetworkAdapter;
use dgxtop_adapters::procfs::{HostConstants, ProcfsAdapter};
use dgxtop_adapters::registry::AdapterOptions;
use dgxtop_core::metric::ids;
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{ReadStatus, Support};
use dgxtop_core::testing::{FakeClock, host_id};
use support::*;

const PROCFS: &str = "procfs";
const NETWORK: &str = "network";
const CPU0_MAX: &str = "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq";
const CPU0_CURRENT: &str = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq";

fn procfs(
    clock: &Arc<FakeClock>,
    files: Arc<dyn SystemFiles>,
    options: AdapterOptions,
) -> ProcfsAdapter {
    ProcfsAdapter::with_host_constants(
        clock.clone(),
        host_id("test-host"),
        files,
        options,
        HostConstants {
            clk_tck: Some(100),
            page_size: Some(4096),
        },
    )
}

fn small_pages(page_size: usize) -> AdapterOptions {
    AdapterOptions {
        page_size,
        ..AdapterOptions::default()
    }
}

#[test]
fn half_page_discovery_never_deletes() {
    let tree = FakeTree::new("catalog-halfpage");
    standard_procfs(&tree);
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = procfs(&clock, tree.files(), small_pages(8));
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(&mut adapter, PROCFS);
    assert_eq!(pages.len(), 10, "75 upserts in pages of 8");
    assert!(
        pages
            .iter()
            .all(|page| page.entities.len() + page.sources.len() <= 8)
    );
    assert!(
        pages[..9]
            .iter()
            .all(|page| !page.complete && page.next_page.is_some())
    );

    tree.write("/proc/diskstats", &DISKSTATS.replace(" sda ", " gone "));
    tree.remove("/sys/class/block/sda");
    // A resync request yields many pages; the first ones must not delete.
    let first = adapter.discover(&discover_request(PROCFS, 0, 0));
    assert!(!first.complete);
    assert!(first.removed_sources.is_empty() && first.removed_entities.is_empty());
    parent.apply(&first);
    assert!(
        parent
            .sources
            .contains_key("procfs:blockdev.8.0:reads_completed")
    );
    // The parent abandons that discovery and starts over.
    let pages = discover_all(&mut adapter, PROCFS, 0);
    for page in &pages[..pages.len() - 1] {
        assert!(page.removed_sources.is_empty() && page.removed_entities.is_empty());
    }
    let last = pages.last().unwrap();
    assert!(last.complete);
    assert_eq!(last.next_page, None);
    assert!(last.generation > 1);
}

#[test]
fn discovery_pages_without_a_snapshot_are_rejected() {
    let tree = FakeTree::new("catalog-nopage");
    standard_procfs(&tree);
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = procfs(&clock, tree.files(), small_pages(256));
    let orphan = adapter.discover(&discover_request(PROCFS, 0, 3));
    assert_eq!(
        orphan.error_code.as_deref(),
        Some("DISCOVERY_PAGE_UNAVAILABLE")
    );
    assert!(!orphan.complete);
    assert!(orphan.sources.is_empty() && orphan.removed_sources.is_empty());
    let pages = discover_all(&mut adapter, PROCFS, 0);
    assert_eq!(pages.len(), 1);
    let beyond = adapter.discover(&discover_request(PROCFS, 0, 1));
    assert_eq!(
        beyond.error_code.as_deref(),
        Some("DISCOVERY_PAGE_UNAVAILABLE")
    );
    let foreign = adapter.discover(&discover_request(PROCFS, 7, 0));
    assert!(foreign.complete, "page 0 always starts a fresh snapshot");
}

#[test]
fn reconnected_device_is_a_new_incarnation_with_a_fresh_slot() {
    let tree = FakeTree::new("catalog-reconnect");
    standard_procfs(&tree);
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = procfs(&clock, tree.files(), small_pages(256));
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, PROCFS);
    let before = parent.source("procfs:blockdev.8.0:reads_completed").clone();
    let sda = "   8       0 sda 59889 0 483728 2907 0 0 0 0 0 2816 2907 0 0 0 0 0 0\n";
    tree.write("/proc/diskstats", &DISKSTATS.replace(sda, ""));
    parent.sync(&mut adapter, PROCFS);
    assert!(
        !parent
            .sources
            .contains_key("procfs:blockdev.8.0:reads_completed")
    );
    tree.write("/proc/diskstats", DISKSTATS);
    parent.sync(&mut adapter, PROCFS);
    let after = parent.source("procfs:blockdev.8.0:reads_completed");
    assert!(after.generation > before.generation);
    assert!(after.slot > before.slot, "slots are never handed out twice");
}

#[test]
fn thousand_hotplug_cycles_keep_the_catalog_bounded_and_never_reuse_slots() {
    let tree = FakeTree::new("catalog-hotplug");
    write_namespaces(&tree);
    write_interface(&tree, "eth0", 2);
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = NetworkAdapter::new(
        clock.clone(),
        host_id("test-host"),
        tree.files(),
        AdapterOptions::default(),
    );
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, NETWORK);
    let eth0_slot = parent.source("network:netif.2:rx_bytes").slot;
    let mut generations = BTreeSet::new();
    for cycle in 0..1000u32 {
        let (name, ifindex) = if cycle % 2 == 0 {
            (format!("veth{cycle}"), 100 + cycle)
        } else {
            // Same name re-created with a new ifindex.
            ("vethsame".to_owned(), 100 + cycle)
        };
        write_interface(&tree, &name, ifindex);
        parent.sync(&mut adapter, NETWORK);
        let entity = format!("netif:{NET_NAMESPACE}:{ifindex}");
        assert!(
            generations.insert(parent.entity(&entity).generation),
            "generation reused"
        );
        tree.remove(&format!("/sys/class/net/{name}"));
        let pages = parent.sync(&mut adapter, NETWORK);
        let removed: Vec<&str> = pages
            .last()
            .unwrap()
            .removed_entities
            .iter()
            .map(|id| id.as_str())
            .collect();
        assert_eq!(removed, vec![entity.as_str()]);
    }
    let stats = adapter.catalog_stats();
    assert_eq!(stats.entities, 1);
    assert_eq!(stats.sources, 8);
    assert!(stats.slot_table <= 8, "{stats:?}");
    assert!(stats.incarnations <= 1, "{stats:?}");
    assert_eq!(stats.next_slot, 8 + 1000 * 8);
    assert_eq!(parent.sources.len(), 8);
    assert_eq!(parent.source("network:netif.2:rx_bytes").slot, eth0_slot);
    assert_eq!(parent.slot_owner.len(), 8 + 1000 * 8);
}

fn write_interface(tree: &FakeTree, name: &str, ifindex: u32) {
    let base = format!("/sys/class/net/{name}");
    tree.write(&format!("{base}/ifindex"), &format!("{ifindex}\n"));
    tree.write(&format!("{base}/operstate"), "up\n");
    tree.write(&format!("{base}/type"), "1\n");
    for counter in [
        "rx_bytes",
        "tx_bytes",
        "rx_packets",
        "tx_packets",
        "rx_errors",
        "tx_errors",
        "rx_dropped",
        "tx_dropped",
    ] {
        tree.write(&format!("{base}/statistics/{counter}"), "1\n");
    }
}

#[test]
fn source_limit_keeps_host_wide_sources_and_reports_the_rest() {
    let tree = FakeTree::new("catalog-limit");
    standard_procfs(&tree);
    let clock = FakeClock::new(1_000_000_000);
    let options = AdapterOptions {
        max_sources: 19,
        ..AdapterOptions::default()
    };
    let mut adapter = procfs(&clock, tree.files(), options);
    assert_eq!(adapter.manifest().source_limit, 19);
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(&mut adapter, PROCFS);
    assert_eq!(parent.sources.len(), 19);
    let limitation = pages[0]
        .capabilities
        .iter()
        .find(|capability| capability.capability_id == "catalog.source_limit")
        .unwrap();
    assert_eq!(limitation.support, Support::Unsupported);
    assert!(
        limitation
            .reason
            .as_deref()
            .unwrap()
            .starts_with("SOURCE_LIMIT_EXCEEDED:sources=46")
    );
    assert!(parent.sources.contains_key("procfs:loadavg:1m"));
    assert!(parent.sources.contains_key("procfs:cpu:user"));
    assert!(parent.sources.contains_key("procfs:meminfo:MemAvailable"));
    assert!(
        !parent.entities.contains_key("cpu.core.0"),
        "entities without admitted sources are dropped"
    );
    for _ in 0..3 {
        let again = parent.sync(&mut adapter, PROCFS);
        assert!(again[0].entities.is_empty() && again[0].sources.is_empty());
    }
    // Entities over the limit keep their incarnation instead of churning
    // through generations on every discovery.
    assert_eq!(adapter.catalog_stats().incarnations, 9);
    let cpu = records(&read_all(&mut adapter, PROCFS, "cpu", 1));
    assert_eq!(cpu.len(), 13);
    assert!(records(&read_all(&mut adapter, PROCFS, "disk", 1)).is_empty());
}

#[test]
fn static_metadata_is_read_once_per_generation() {
    let tree = FakeTree::new("catalog-metadata");
    standard_procfs(&tree);
    let counting = tree.counting();
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = procfs(&clock, counting.clone(), small_pages(256));
    let mut parent = ParentCatalog::default();
    for cycle in 0..30u64 {
        if cycle % 10 == 0 {
            parent.sync(&mut adapter, PROCFS);
        }
        let records = records(&read_all(&mut adapter, PROCFS, "cpu", cycle + 1));
        assert_eq!(
            u64_value(find(&records, ids::CPU_FREQUENCY_MAXIMUM, "cpu.core.0")),
            3_500_000
        );
        clock.advance_ms(1_000);
    }
    assert_eq!(counting.reads_of(CPU0_MAX), 1);
    assert_eq!(counting.reads_of("/proc/cpuinfo"), 1);
    assert_eq!(counting.reads_of(CPU0_CURRENT), 30);

    let generation = parent.entity("cpu.core.0").generation;
    tree.write(
        "/proc/stat",
        &STAT.replace("cpu0 400 4 200 45000 20 0 3 1 50 2\n", ""),
    );
    parent.sync(&mut adapter, PROCFS);
    assert!(!parent.entities.contains_key("cpu.core.0"));
    tree.write("/proc/stat", STAT);
    parent.sync(&mut adapter, PROCFS);
    assert!(parent.entity("cpu.core.0").generation > generation);
    assert_eq!(
        counting.reads_of(CPU0_MAX),
        2,
        "a new generation re-reads static metadata"
    );
}

#[test]
fn sticky_read_failures_are_reprobed_at_most_once_per_interval() {
    let tree = FakeTree::new("catalog-sticky");
    standard_procfs(&tree);
    let faulty = tree.faulty();
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = procfs(&clock, faulty.clone(), small_pages(256));
    ParentCatalog::default().sync(&mut adapter, PROCFS);
    faulty.inject(CPU0_CURRENT, 61);
    for cycle in 0..=130u64 {
        let records = records(&read_all(&mut adapter, PROCFS, "cpu", cycle + 1));
        let current = find(&records, ids::CPU_FREQUENCY_CURRENT, "cpu.core.0");
        assert_eq!(current.read_status, ReadStatus::Unsupported);
        assert_eq!(current.value, None);
        assert_eq!(current.reason_codes[0], "UNSUPPORTED_BY_DRIVER");
        if cycle % 60 != 0 {
            assert_eq!(
                current.reason_codes.get(1).map(String::as_str),
                Some("REPROBE_DEFERRED")
            );
        }
        clock.advance_ms(1_000);
    }
    assert_eq!(
        faulty.reads_of(CPU0_CURRENT),
        3,
        "probes at 0 s, 60 s and 120 s"
    );
    faulty.clear(CPU0_CURRENT);
    let mut recovered = false;
    for cycle in 0..61u64 {
        let records = records(&read_all(&mut adapter, PROCFS, "cpu", 1_000 + cycle));
        recovered |=
            find(&records, ids::CPU_FREQUENCY_CURRENT, "cpu.core.0").read_status == ReadStatus::Ok;
        clock.advance_ms(1_000);
    }
    assert!(recovered);
}

#[test]
fn transient_read_errors_are_retried_every_cycle() {
    let tree = FakeTree::new("catalog-transient");
    standard_procfs(&tree);
    let faulty = tree.faulty();
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = procfs(&clock, faulty.clone(), small_pages(256));
    ParentCatalog::default().sync(&mut adapter, PROCFS);
    faulty.inject(CPU0_CURRENT, 5);
    for cycle in 0..5u64 {
        let records = records(&read_all(&mut adapter, PROCFS, "cpu", cycle + 1));
        let current = find(&records, ids::CPU_FREQUENCY_CURRENT, "cpu.core.0");
        assert_eq!(current.read_status, ReadStatus::Error);
        assert_eq!(current.reason_codes, vec!["IO_ERROR_5".to_owned()]);
        clock.advance_ms(1_000);
    }
    assert_eq!(faulty.reads_of(CPU0_CURRENT), 5);
}

#[test]
fn slots_whose_removal_is_pending_read_as_removed() {
    let tree = FakeTree::new("catalog-pending");
    standard_procfs(&tree);
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = procfs(&clock, tree.files(), small_pages(4));
    let mut parent = ParentCatalog::default();
    parent.sync(&mut adapter, PROCFS);
    let slot = parent.source("procfs:blockdev.8.0:reads_completed").slot;
    let sda = "   8       0 sda 59889 0 483728 2907 0 0 0 0 0 2816 2907 0 0 0 0 0 0\n";
    tree.write("/proc/diskstats", &DISKSTATS.replace(sda, ""));
    // A resync spans many pages; after page 0 the removal is still pending.
    let first = adapter.discover(&discover_request(PROCFS, 0, 0));
    assert!(!first.complete);
    let plan = read_plan(PROCFS, "disk", 1, 0, vec![slot]);
    let batch = adapter.read(&plan);
    batch.validate_against(&plan, 256).unwrap();
    assert_eq!(batch.records.len(), 1);
    assert_eq!(batch.records[0].read_status, ReadStatus::Removed);
    assert_eq!(batch.records[0].value, None);
    assert_eq!(
        batch.records[0].reason_codes,
        vec!["DEVICE_NOT_PRESENT".to_owned()]
    );
}

#[test]
fn discovery_guards_reject_wrong_adapter_and_expired_deadline() {
    let tree = FakeTree::new("catalog-guards");
    standard_procfs(&tree);
    let clock = FakeClock::new(1_000_000_000);
    let mut adapter = procfs(&clock, tree.files(), small_pages(256));
    let wrong = adapter.discover(&discover_request(NETWORK, 0, 0));
    assert_eq!(wrong.error_code.as_deref(), Some("ADAPTER_ID_MISMATCH"));
    let mut late = discover_request(PROCFS, 0, 0);
    late.deadline_boot_ns = 1;
    let expired = adapter.discover(&late);
    assert_eq!(expired.error_code.as_deref(), Some("DEADLINE_EXPIRED"));
    assert!(!expired.complete && expired.entities.is_empty());
    assert_eq!(adapter.catalog_stats().generation, 0);
}
