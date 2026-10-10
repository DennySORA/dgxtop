//! Hardware smoke tests against the real host (`--ignored`). They only read
//! `/proc`, `/sys` and statvfs and assert basic sanity.

#[path = "fixtures/adapters_a/support.rs"]
mod support;

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dgxtop_adapters::registry::{AdapterOptions, build_live_adapter};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::BootId;
use dgxtop_core::metric::ids;
use dgxtop_core::observation::Observation;
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::{Availability, ReadStatus};
use dgxtop_core::testing::{boot_id, host_id};
use support::*;

/// Monotonic test clock with the real boot id.
struct HostClock {
    start: Instant,
    boot_id: BootId,
}

impl HostClock {
    fn new() -> Arc<Self> {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap_or_else(|error| panic!("boot id unreadable: {error}"));
        Arc::new(Self {
            start: Instant::now(),
            boot_id: boot_id(boot.trim()),
        })
    }
}

impl TimeProvider for HostClock {
    fn boot_id(&self) -> BootId {
        self.boot_id.clone()
    }

    fn now_boot_ns(&self) -> u64 {
        let elapsed = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        elapsed.saturating_add(1_000_000_000)
    }

    fn now_utc_ns(&self) -> i128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                i128::try_from(since.as_nanos()).unwrap_or(i128::MAX)
            })
    }
}

fn live(adapter: &str) -> Box<dyn TelemetryPort> {
    build_live_adapter(
        adapter,
        HostClock::new(),
        host_id("hardware-smoke"),
        AdapterOptions::default(),
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

fn ok_count(records: &[Observation]) -> usize {
    records
        .iter()
        .filter(|record| record.read_status == ReadStatus::Ok)
        .count()
}

#[test]
#[ignore = "reads the real host"]
fn hardware_procfs_smoke() {
    let mut adapter = live("procfs");
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(adapter.as_mut(), "procfs");
    assert_eq!(pages[0].availability, Some(Availability::Available));
    assert!(parent.entities.contains_key("host") && parent.entities.contains_key("memory"));
    let cores = parent
        .entities
        .keys()
        .filter(|id| id.starts_with("cpu.core."))
        .count();
    assert!(cores >= 1);
    let cpu = records(&read_all(adapter.as_mut(), "procfs", "cpu", 1));
    assert!(u64_value(find(&cpu, ids::CPU_TIME_IDLE, "cpu")) > 0);
    let memory = records(&read_all(adapter.as_mut(), "procfs", "memory", 1));
    let total = u64_value(find(&memory, ids::MEMORY_TOTAL, "memory"));
    assert!(total > 0);
    let disk = records(&read_all(adapter.as_mut(), "procfs", "disk", 1));
    let processes = records(&read_all(adapter.as_mut(), "procfs", "process", 1));
    let own = format!("/proc/{}", std::process::id());
    assert!(
        processes
            .iter()
            .any(|record| record.provenance.device_path == own)
    );
    eprintln!(
        "procfs: {} entities, {} sources, {cores} cores; cpu {}/{} ok, memory total {total} KiB, disk {}/{} ok, process records {} ({} ok)",
        parent.entities.len(),
        parent.sources.len(),
        ok_count(&cpu),
        cpu.len(),
        ok_count(&disk),
        disk.len(),
        processes.len(),
        ok_count(&processes)
    );
}

#[test]
#[ignore = "reads the real host"]
fn hardware_network_smoke() {
    let mut adapter = live("network");
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(adapter.as_mut(), "network");
    assert_eq!(pages[0].availability, Some(Availability::Available));
    let loopback = parent
        .entities
        .values()
        .find(|entity| entity.attribute("loopback") == Some("true"))
        .expect("loopback interface");
    let fast = records(&read_all(adapter.as_mut(), "network", "fast", 1));
    let rx = find(&fast, ids::NET_RX_BYTES, loopback.entity_id.as_str());
    assert_eq!(rx.read_status, ReadStatus::Ok);
    eprintln!(
        "network: {} interfaces, {} sources; {}/{} records ok",
        parent.entities.len(),
        parent.sources.len(),
        ok_count(&fast),
        fast.len()
    );
}

#[test]
#[ignore = "reads the real host"]
fn hardware_filesystem_smoke() {
    let mut adapter = live("filesystem");
    let mut parent = ParentCatalog::default();
    let pages = parent.sync(adapter.as_mut(), "filesystem");
    assert_eq!(pages[0].availability, Some(Availability::Available));
    let root = parent
        .entities
        .values()
        .find(|entity| entity.attribute("mount_point") == Some("/"))
        .expect("root filesystem");
    let capacity = records(&read_all(adapter.as_mut(), "filesystem", "capacity", 1));
    let total = u64_value(find(&capacity, ids::FS_TOTAL, root.entity_id.as_str()));
    let free = u64_value(find(&capacity, ids::FS_FREE, root.entity_id.as_str()));
    let available = u64_value(find(&capacity, ids::FS_AVAILABLE, root.entity_id.as_str()));
    assert!(total > 0 && free <= total && available <= free);
    eprintln!(
        "filesystem: {} mounts, {} sources; {}/{} records ok; / total {total} B",
        parent.entities.len(),
        parent.sources.len(),
        ok_count(&capacity),
        capacity.len()
    );
}
