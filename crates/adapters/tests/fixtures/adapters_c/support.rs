//! Shared helpers of the adapters-C integration tests: fake sysfs trees in
//! temporary directories, fault-injecting file access and request builders.
//! Nothing here touches real drivers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use dgxtop_adapters::common::{EntryName, RootedFiles, SystemFiles};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::HostId;
use dgxtop_core::observation::{
    CatalogDelta, DiscoverRequest, Observation, ObservationBatch, ReadPlan, SourceDescriptor,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::testing::{FakeClock, adapter_id, host_id, read_group};
use uuid::Uuid;

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// A fake `/` tree in a temporary directory, removed on drop.
pub struct FakeSysfs {
    root: PathBuf,
}

impl FakeSysfs {
    pub fn new(label: &str) -> Self {
        let unique = UNIQUE.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "dgxtop-adapters-c-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    pub fn write(&self, path: &str, contents: &str) {
        let target = self.path(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, contents).unwrap();
    }

    pub fn mkdir(&self, path: &str) {
        std::fs::create_dir_all(self.path(path)).unwrap();
    }

    /// Symlink `link` -> `target` (both root-relative absolute-style paths).
    pub fn symlink(&self, link: &str, target: &str) {
        let link_path = self.path(link);
        std::fs::create_dir_all(link_path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&link_path);
        std::os::unix::fs::symlink(self.path(target), link_path).unwrap();
    }

    /// Removes a file, symlink or directory tree.
    pub fn remove(&self, path: &str) {
        let target = self.path(path);
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&target).unwrap(),
            Ok(_) => std::fs::remove_file(&target).unwrap(),
            Err(_) => {}
        }
    }

    pub fn rooted(&self) -> RootedFiles {
        RootedFiles::new(&self.root)
    }

    pub fn files(&self) -> Arc<dyn SystemFiles> {
        Arc::new(self.rooted())
    }

    /// Adds `/sys/devices/<device>/hwmon/hwmon<index>` with `name`, a `device`
    /// link and the class link; returns the hwmon directory.
    pub fn hwmon(&self, index: u32, device: &str, name: &str) -> String {
        let device_path = format!("/sys/devices/{device}");
        let directory = format!("{device_path}/hwmon/hwmon{index}");
        self.mkdir(&device_path);
        self.write(&format!("{directory}/name"), &format!("{name}\n"));
        self.symlink(&format!("{directory}/device"), &device_path);
        self.symlink(&format!("/sys/class/hwmon/hwmon{index}"), &directory);
        directory
    }

    /// Removes one hwmon device directory and its class link.
    pub fn remove_hwmon(&self, index: u32, device: &str) {
        self.remove(&format!("/sys/class/hwmon/hwmon{index}"));
        self.remove(&format!("/sys/devices/{device}/hwmon/hwmon{index}"));
    }

    /// Adds a thermal zone with `type`, `temp` and an optional device link.
    pub fn thermal_zone(
        &self,
        index: u32,
        zone_type: &str,
        temp: &str,
        device: Option<&str>,
    ) -> String {
        let directory = format!("/sys/devices/virtual/thermal/thermal_zone{index}");
        self.write(&format!("{directory}/type"), &format!("{zone_type}\n"));
        self.write(&format!("{directory}/temp"), &format!("{temp}\n"));
        if let Some(device) = device {
            let device_path = format!("/sys/devices/{device}");
            self.mkdir(&device_path);
            self.symlink(&format!("{directory}/device"), &device_path);
        }
        self.symlink(
            &format!("/sys/class/thermal/thermal_zone{index}"),
            &directory,
        );
        directory
    }

    /// Adds a powercap zone directory (`intel-rapl:0`, `intel-rapl:0:0`, ...).
    pub fn powercap_zone(
        &self,
        zone: &str,
        name: &str,
        energy: Option<&str>,
        max_range: Option<&str>,
    ) -> String {
        let parent = match zone.rsplit_once(':') {
            Some((parent, _)) if parent.contains(':') => {
                format!("/sys/devices/virtual/powercap/intel-rapl/{parent}")
            }
            _ => "/sys/devices/virtual/powercap/intel-rapl".to_owned(),
        };
        let directory = format!("{parent}/{zone}");
        self.write(&format!("{directory}/name"), &format!("{name}\n"));
        if let Some(energy) = energy {
            self.write(&format!("{directory}/energy_uj"), &format!("{energy}\n"));
        }
        if let Some(range) = max_range {
            self.write(
                &format!("{directory}/max_energy_range_uj"),
                &format!("{range}\n"),
            );
        }
        self.symlink(&format!("/sys/class/powercap/{zone}"), &directory);
        directory
    }

    /// CPU topology: one `cpuN/topology/physical_package_id` per entry.
    pub fn cpus(&self, packages: &[u32]) {
        for (cpu, package) in packages.iter().enumerate() {
            self.write(
                &format!("/sys/devices/system/cpu/cpu{cpu}/topology/physical_package_id"),
                &format!("{package}\n"),
            );
        }
        self.mkdir("/sys/devices/system/cpu/cpufreq");
    }
}

impl Drop for FakeSysfs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Injected failure of one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Kind(io::ErrorKind),
    Os(i32),
}

impl Failure {
    fn error(self) -> io::Error {
        match self {
            Self::Kind(kind) => io::Error::from(kind),
            Self::Os(code) => io::Error::from_raw_os_error(code),
        }
    }
}

/// File access failing selected exact paths (reads, listings, canonicalization).
pub struct FaultyFiles<F> {
    inner: F,
    failures: Mutex<BTreeMap<String, Failure>>,
}

impl<F: SystemFiles> FaultyFiles<F> {
    pub fn new(inner: F) -> Self {
        Self {
            inner,
            failures: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn fail(&self, path: &str, failure: Failure) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), failure);
    }

    pub fn heal(&self, path: &str) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path);
    }

    fn check(&self, path: &str) -> io::Result<()> {
        match self
            .failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
        {
            Some(failure) => Err(failure.error()),
            None => Ok(()),
        }
    }
}

impl<F: SystemFiles> SystemFiles for FaultyFiles<F> {
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        self.check(path)?;
        self.inner.read_to_string(path)
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<EntryName>> {
        self.check(path)?;
        self.inner.read_dir(path)
    }

    fn read_link(&self, path: &str) -> io::Result<PathBuf> {
        self.inner.read_link(path)
    }

    fn canonicalize(&self, path: &str) -> io::Result<PathBuf> {
        self.check(path)?;
        self.inner.canonicalize(path)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn owner_uid(&self, path: &str) -> io::Result<u32> {
        self.inner.owner_uid(path)
    }
}

/// Test host id.
pub fn host() -> HostId {
    host_id("test-host")
}

pub fn clock() -> Arc<FakeClock> {
    FakeClock::new(1_000_000_000)
}

pub fn time(clock: &Arc<FakeClock>) -> Arc<dyn TimeProvider> {
    clock.clone()
}

/// Deadline five seconds after the clock's current time.
pub fn deadline(clock: &Arc<FakeClock>) -> u64 {
    clock.now_boot_ns() + 5_000_000_000
}

pub fn discover_request(
    adapter: &str,
    previous_generation: u32,
    page: u32,
    deadline: u64,
) -> DiscoverRequest {
    DiscoverRequest {
        request_id: Uuid::new_v4(),
        adapter_id: adapter_id(adapter),
        host_generation: 1,
        catalog_revision: 1,
        config_revision: 1,
        deadline_boot_ns: deadline,
        previous_generation,
        page,
    }
}

pub fn read_plan(
    adapter: &str,
    group: &str,
    cycle: u64,
    page: u32,
    deadline: u64,
    slots: Vec<u32>,
) -> ReadPlan {
    ReadPlan {
        request_id: Uuid::new_v4(),
        adapter_id: adapter_id(adapter),
        host_generation: 1,
        catalog_revision: 1,
        config_revision: 1,
        cycle_id: cycle,
        read_group: read_group(group),
        source_slots: slots,
        deadline_boot_ns: deadline,
        epoch: 1,
        page,
    }
}

/// Follows discovery pages until the catalog is complete (or a page fails).
pub fn discover_all(
    port: &mut dyn TelemetryPort,
    adapter: &str,
    previous: u32,
    deadline: u64,
) -> Vec<CatalogDelta> {
    let mut pages = Vec::new();
    let mut page = 0;
    loop {
        let delta = port.discover(&discover_request(adapter, previous, page, deadline));
        delta.validate(256).unwrap();
        let next = delta.next_page;
        let done = delta.complete || next.is_none();
        pages.push(delta);
        if done {
            return pages;
        }
        page = next.unwrap();
        assert!(pages.len() < 1000, "discovery never completes");
    }
}

/// Discovers and returns the final generation plus all pages.
pub fn discover(
    port: &mut dyn TelemetryPort,
    adapter: &str,
    previous: u32,
    deadline: u64,
) -> (u32, Vec<CatalogDelta>) {
    let pages = discover_all(port, adapter, previous, deadline);
    let last = pages.last().unwrap();
    assert!(last.complete, "discovery failed: {:?}", last.error_code);
    (last.generation, pages)
}

/// Reads every page of `group` for `cycle`; every batch must match its plan.
pub fn read_all(
    port: &mut dyn TelemetryPort,
    adapter: &str,
    group: &str,
    cycle: u64,
    deadline: u64,
    slots: Vec<u32>,
) -> Vec<ObservationBatch> {
    let mut batches = Vec::new();
    let mut page = 0;
    loop {
        let plan = read_plan(adapter, group, cycle, page, deadline, slots.clone());
        let batch = port.read(&plan);
        batch.validate_against(&plan, 256).unwrap();
        let continues = batch.continues();
        batches.push(batch);
        if !continues {
            return batches;
        }
        page += 1;
        assert!(batches.len() < 1000, "paging never ends");
    }
}

pub fn records(batches: &[ObservationBatch]) -> Vec<Observation> {
    batches
        .iter()
        .flat_map(|batch| batch.records.iter().cloned())
        .collect()
}

pub fn sources(pages: &[CatalogDelta]) -> Vec<SourceDescriptor> {
    pages
        .iter()
        .flat_map(|page| page.sources.iter().cloned())
        .collect()
}

pub fn source<'a>(sources: &'a [SourceDescriptor], source_id: &str) -> &'a SourceDescriptor {
    sources
        .iter()
        .find(|descriptor| descriptor.source_id.as_str() == source_id)
        .unwrap_or_else(|| {
            let known: Vec<&str> = sources.iter().map(|d| d.source_id.as_str()).collect();
            panic!("source {source_id} not found in {known:?}")
        })
}

pub fn record<'a>(records: &'a [Observation], source_id: &str) -> &'a Observation {
    records
        .iter()
        .find(|record| record.provenance.source_id.as_str() == source_id)
        .unwrap_or_else(|| {
            let known: Vec<&str> = records
                .iter()
                .map(|r| r.provenance.source_id.as_str())
                .collect();
            panic!("record {source_id} not found in {known:?}")
        })
}

pub fn value_of(record: &Observation) -> f64 {
    record.value.expect("record has a value").to_f64_lossy()
}
