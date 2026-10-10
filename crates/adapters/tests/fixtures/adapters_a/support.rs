//! Shared helpers of the adapters-A integration tests: fake procfs/sysfs
//! trees in per-test directories, request builders, a mirror of the parent
//! catalog that applies deltas like the Manager would, a fault-injecting
//! file layer and a fake statvfs probe. Nothing touches real drivers.
// Each test crate uses a subset of the helpers, and a broken fixture must
// abort the test loudly, hence unwrap in these non-#[test] helpers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use dgxtop_adapters::common::{CountingFiles, EntryName, RootedFiles, SystemFiles};
use dgxtop_adapters::filesystem::{CapacityFigures, CapacityProbe};
use dgxtop_core::identity::{AdapterId, ReadGroup};
use dgxtop_core::observation::{
    CatalogDelta, DiscoverRequest, EntityRecord, Observation, ObservationBatch, ReadPlan,
    SourceDescriptor,
};
use dgxtop_core::ports::TelemetryPort;
use uuid::Uuid;

pub const PID_NAMESPACE: &str = "pid:[4026531836]";
pub const NET_NAMESPACE: &str = "net:[4026531840]";
pub const MNT_NAMESPACE: &str = "mnt:[4026531841]";

/// A throw-away directory tree that looks like `/` to the adapters.
pub struct FakeTree {
    root: PathBuf,
}

impl FakeTree {
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "adapters-a-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    pub fn write(&self, path: &str, contents: &str) {
        let full = self.path(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }

    pub fn mkdir(&self, path: &str) {
        fs::create_dir_all(self.path(path)).unwrap();
    }

    pub fn remove(&self, path: &str) {
        let full = self.path(path);
        if full.is_dir() {
            fs::remove_dir_all(full).unwrap();
        } else if full.symlink_metadata().is_ok() {
            fs::remove_file(full).unwrap();
        }
    }

    pub fn symlink(&self, path: &str, target: &str) {
        let full = self.path(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&full);
        std::os::unix::fs::symlink(target, full).unwrap();
    }

    pub fn chmod(&self, path: &str, mode: u32) {
        fs::set_permissions(self.path(path), fs::Permissions::from_mode(mode)).unwrap();
    }

    pub fn files(&self) -> Arc<dyn SystemFiles> {
        Arc::new(RootedFiles::new(&self.root))
    }

    pub fn counting(&self) -> Arc<CountingFiles<RootedFiles>> {
        Arc::new(CountingFiles::new(RootedFiles::new(&self.root)))
    }

    pub fn faulty(&self) -> Arc<FaultyFiles> {
        Arc::new(FaultyFiles::new(RootedFiles::new(&self.root)))
    }
}

impl Drop for FakeTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Whether the tests run as root (permission bits are not enforced then).
pub fn running_as_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// `/proc/self/ns/{pid,net,mnt}` links.
pub fn write_namespaces(tree: &FakeTree) {
    tree.symlink("/proc/self/ns/pid", PID_NAMESPACE);
    tree.symlink("/proc/self/ns/net", NET_NAMESPACE);
    tree.symlink("/proc/self/ns/mnt", MNT_NAMESPACE);
}

pub const STAT: &str = "\
cpu  1000 10 500 90000 40 0 7 3 100 5
cpu0 400 4 200 45000 20 0 3 1 50 2
cpu1 300 3 150 30000 10 0 2 1 50 3
cpu3 300 3 150 15000 10 0 2 1 0 0
intr 15428341 9 0
ctxt 18225208
btime 1791045735
";

pub const MEMINFO: &str = "\
MemTotal:       32654612 kB
MemFree:         1000000 kB
MemAvailable:   20000000 kB
Buffers:          500000 kB
Cached:          6000000 kB
SwapCached:            0 kB
SwapTotal:       8000000 kB
SwapFree:        7000000 kB
HugePages_Total:       0
";

pub const DISKSTATS: &str = "\
 259       0 nvme0n1 347806 38507 11509086 31169 554347 133357 82536402 2694532 0 35004 2729390 0 0 0 0 17013 3688
 259       1 nvme0n1p1 577 1643 12554 267 2 0 2 0 0 12 267 0 0 0 0 0 0
   8       0 sda 59889 0 483728 2907 0 0 0 0 0 2816 2907 0 0 0 0 0 0
   7       0 loop0 100 0 200 10 0 0 0 0 0 10 10 0 0 0 0 0 0
   1       0 ram0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
";

pub fn write_cpufreq(tree: &FakeTree, cpu: u32, current_khz: u64, maximum_khz: u64) {
    let base = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq");
    tree.write(
        &format!("{base}/scaling_cur_freq"),
        &format!("{current_khz}\n"),
    );
    tree.write(
        &format!("{base}/cpuinfo_max_freq"),
        &format!("{maximum_khz}\n"),
    );
}

/// A complete fake host for the procfs adapter: CPUs 0, 1 and 3 (no CPU 2,
/// so identity must come from labels), cpufreq on CPUs 0 and 1 only.
pub fn standard_procfs(tree: &FakeTree) {
    write_namespaces(tree);
    tree.write("/proc/stat", STAT);
    tree.write("/proc/loadavg", "0.50 0.75 1.25 3/1591 71703\n");
    tree.write("/proc/meminfo", MEMINFO);
    tree.write("/proc/diskstats", DISKSTATS);
    tree.write("/proc/sys/kernel/hostname", "dgx-test\n");
    tree.write("/proc/sys/kernel/osrelease", "6.12.0-test\n");
    tree.write(
        "/proc/cpuinfo",
        "processor\t: 0\nmodel name\t: Test CPU @ 3.0GHz\n",
    );
    tree.write("/sys/class/block/nvme0n1p1/partition", "1\n");
    tree.mkdir("/sys/class/block/nvme0n1");
    tree.mkdir("/sys/class/block/sda");
    write_cpufreq(tree, 0, 2_400_000, 3_500_000);
    write_cpufreq(tree, 1, 1_200_000, 3_500_000);
}

/// One fake process `/proc/<pid>/stat`.
pub fn write_process(
    tree: &FakeTree,
    pid: u32,
    comm: &str,
    utime: u64,
    stime: u64,
    start: u64,
    rss_pages: &str,
) {
    let stat = format!(
        "{pid} ({comm}) S 1 {pid} {pid} 0 -1 4194560 100 0 0 0 {utime} {stime} 0 0 20 0 1 0 {start} 1234567 {rss_pages} 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0\n"
    );
    tree.write(&format!("/proc/{pid}/stat"), &stat);
}

pub fn discover_request(adapter: &str, previous_generation: u32, page: u32) -> DiscoverRequest {
    DiscoverRequest {
        request_id: Uuid::new_v4(),
        adapter_id: AdapterId::new(adapter).unwrap(),
        host_generation: 1,
        catalog_revision: 1,
        config_revision: 1,
        deadline_boot_ns: u64::MAX,
        previous_generation,
        page,
    }
}

pub fn read_plan(adapter: &str, group: &str, cycle: u64, page: u32, slots: Vec<u32>) -> ReadPlan {
    ReadPlan {
        request_id: Uuid::new_v4(),
        adapter_id: AdapterId::new(adapter).unwrap(),
        host_generation: 1,
        catalog_revision: 1,
        config_revision: 1,
        cycle_id: cycle,
        read_group: ReadGroup::new(group).unwrap(),
        source_slots: slots,
        deadline_boot_ns: u64::MAX,
        epoch: 1,
        page,
    }
}

/// Requests discovery pages until the complete one.
pub fn discover_all(
    port: &mut dyn TelemetryPort,
    adapter: &str,
    previous_generation: u32,
) -> Vec<CatalogDelta> {
    let mut pages = Vec::new();
    let mut page = 0;
    loop {
        let delta = port.discover(&discover_request(adapter, previous_generation, page));
        delta.validate(256).unwrap();
        for entity in &delta.entities {
            entity.validate().unwrap();
        }
        let (complete, next) = (delta.complete, delta.next_page);
        pages.push(delta);
        assert!(pages.len() < 10_000, "discovery never completes");
        if complete {
            return pages;
        }
        page = next.expect("an incomplete page announces the next one");
    }
}

/// Requests read pages of one cycle until the final one; validates every
/// batch against its plan and every record.
pub fn read_all(
    port: &mut dyn TelemetryPort,
    adapter: &str,
    group: &str,
    cycle: u64,
) -> Vec<ObservationBatch> {
    let mut batches = Vec::new();
    let mut page = 0;
    loop {
        let plan = read_plan(adapter, group, cycle, page, Vec::new());
        let batch = port.read(&plan);
        batch.validate_against(&plan, 256).unwrap();
        let continues = batch.continues();
        batches.push(batch);
        assert!(batches.len() < 10_000, "read never completes");
        if !continues {
            return batches;
        }
        page += 1;
    }
}

pub fn records(batches: &[ObservationBatch]) -> Vec<Observation> {
    batches
        .iter()
        .flat_map(|batch| batch.records.clone())
        .collect()
}

pub fn find<'a>(records: &'a [Observation], metric: &str, entity: &str) -> &'a Observation {
    records
        .iter()
        .find(|record| {
            record.key.metric_id.as_str() == metric && record.key.entity_id.as_str() == entity
        })
        .unwrap_or_else(|| panic!("no record {metric} on {entity}"))
}

pub fn u64_value(record: &Observation) -> u64 {
    record.value.unwrap().as_u64().unwrap()
}

/// What the parent holds after applying deltas the way the Manager does:
/// upserts immediately, removals and generation only with the complete page.
#[derive(Debug, Default)]
pub struct ParentCatalog {
    pub generation: u32,
    pub entities: BTreeMap<String, EntityRecord>,
    pub sources: BTreeMap<String, SourceDescriptor>,
    /// Every slot ever announced and the source id it belongs to.
    pub slot_owner: BTreeMap<u32, String>,
}

impl ParentCatalog {
    pub fn apply(&mut self, delta: &CatalogDelta) {
        for entity in &delta.entities {
            self.entities
                .insert(entity.entity_id.to_string(), entity.clone());
        }
        for source in &delta.sources {
            let id = source.source_id.to_string();
            let owner = self
                .slot_owner
                .entry(source.slot)
                .or_insert_with(|| id.clone());
            assert_eq!(owner, &id, "slot {} reused for another source", source.slot);
            self.sources.insert(id, source.clone());
        }
        if delta.complete {
            for id in &delta.removed_sources {
                self.sources.remove(id.as_str());
            }
            for id in &delta.removed_entities {
                self.entities.remove(id.as_str());
            }
            self.generation = delta.generation;
        }
    }

    pub fn sync(&mut self, port: &mut dyn TelemetryPort, adapter: &str) -> Vec<CatalogDelta> {
        let pages = discover_all(port, adapter, self.generation);
        for page in &pages {
            self.apply(page);
        }
        pages
    }

    pub fn source(&self, source_id: &str) -> &SourceDescriptor {
        self.sources
            .get(source_id)
            .unwrap_or_else(|| panic!("no source {source_id}"))
    }

    pub fn entity(&self, entity_id: &str) -> &EntityRecord {
        self.entities
            .get(entity_id)
            .unwrap_or_else(|| panic!("no entity {entity_id}"))
    }
}

/// Real files with injectable errno per path and per-path read counts
/// (including `exists`).
pub struct FaultyFiles {
    inner: RootedFiles,
    faults: Mutex<BTreeMap<String, i32>>,
    reads: Mutex<BTreeMap<String, u64>>,
}

impl FaultyFiles {
    pub fn new(inner: RootedFiles) -> Self {
        Self {
            inner,
            faults: Mutex::new(BTreeMap::new()),
            reads: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn inject(&self, path: &str, errno: i32) {
        self.faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), errno);
    }

    pub fn clear(&self, path: &str) {
        self.faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path);
    }

    pub fn reads_of(&self, path: &str) -> u64 {
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    fn touch(&self, path: &str) -> io::Result<()> {
        *self
            .reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(path.to_owned())
            .or_insert(0) += 1;
        match self
            .faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
        {
            Some(errno) => Err(io::Error::from_raw_os_error(*errno)),
            None => Ok(()),
        }
    }
}

impl SystemFiles for FaultyFiles {
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        self.touch(path)?;
        self.inner.read_to_string(path)
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<EntryName>> {
        self.touch(path)?;
        self.inner.read_dir(path)
    }

    fn read_link(&self, path: &str) -> io::Result<PathBuf> {
        self.touch(path)?;
        self.inner.read_link(path)
    }

    fn canonicalize(&self, path: &str) -> io::Result<PathBuf> {
        self.inner.canonicalize(path)
    }

    fn exists(&self, path: &str) -> bool {
        self.touch(path).is_ok() && self.inner.exists(path)
    }

    fn owner_uid(&self, path: &str) -> io::Result<u32> {
        self.touch(path)?;
        self.inner.owner_uid(path)
    }
}

/// Fake statvfs: configured figures or errno per mount point, call log.
#[derive(Clone, Default)]
pub struct FakeCapacityProbe {
    state: Arc<Mutex<ProbeState>>,
}

#[derive(Default)]
struct ProbeState {
    outcomes: BTreeMap<String, Result<CapacityFigures, i32>>,
    calls: Vec<String>,
}

impl FakeCapacityProbe {
    pub fn set(&self, mount_point: &str, figures: CapacityFigures) {
        self.lock()
            .outcomes
            .insert(mount_point.to_owned(), Ok(figures));
    }

    pub fn fail(&self, mount_point: &str, errno: i32) {
        self.lock()
            .outcomes
            .insert(mount_point.to_owned(), Err(errno));
    }

    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ProbeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CapacityProbe for FakeCapacityProbe {
    fn capacity(&self, mount_point: &str) -> io::Result<CapacityFigures> {
        let mut state = self.lock();
        state.calls.push(mount_point.to_owned());
        match state.outcomes.get(mount_point) {
            Some(Ok(figures)) => Ok(*figures),
            Some(Err(errno)) => Err(io::Error::from_raw_os_error(*errno)),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }
}
