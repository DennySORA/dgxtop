//! Shared adapter mechanics: root-relative filesystem access (injectable for
//! fake trees and call counting), stable slot interning, consistent paging,
//! unsupported-capability probe throttling and observation construction.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dgxtop_core::clock::{TimeProvider, ms_to_ns};
use dgxtop_core::identity::{AdapterId, ReadGroup, SourceId};
use dgxtop_core::metric::Scalar;
use dgxtop_core::observation::{
    BatchStatus, IPC_PROTOCOL_VERSION, MAX_RECORDS_PER_BATCH, OBSERVATION_SCHEMA_VERSION,
    Observation, ObservationBatch, PAGE_CONTINUES, Provenance, ReadPlan, SourceDescriptor,
};
use dgxtop_core::quality::{Quality, ReadStatus};

/// Maximum bytes read from one pseudo file (procfs/sysfs files are small;
/// the bound protects against unexpected huge files).
pub const MAX_PSEUDO_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// One directory entry name plus whether it is a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryName {
    pub name: String,
    pub is_dir: bool,
}

/// Root-relative read-only filesystem access used by every sysfs/procfs
/// adapter. Paths passed in are absolute-style (`/proc/stat`) and resolved
/// under the configured root, so tests can use fake trees.
pub trait SystemFiles: Send + Sync {
    fn read_to_string(&self, path: &str) -> io::Result<String>;
    fn read_dir(&self, path: &str) -> io::Result<Vec<EntryName>>;
    fn read_link(&self, path: &str) -> io::Result<PathBuf>;
    fn canonicalize(&self, path: &str) -> io::Result<PathBuf>;
    fn exists(&self, path: &str) -> bool;
    /// Owner uid of a path (used for `/proc/<pid>`).
    fn owner_uid(&self, path: &str) -> io::Result<u32>;
}

/// Real filesystem rooted at `root` (`/` in production).
#[derive(Debug, Clone)]
pub struct RootedFiles {
    root: PathBuf,
}

impl RootedFiles {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn host() -> Self {
        Self::new("/")
    }

    fn resolve(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl SystemFiles for RootedFiles {
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        use std::io::Read;
        let file = std::fs::File::open(self.resolve(path))?;
        let mut text = String::new();
        file.take(MAX_PSEUDO_FILE_BYTES).read_to_string(&mut text)?;
        Ok(text)
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<EntryName>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(self.resolve(path))? {
            let entry = entry?;
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            if let Ok(name) = entry.file_name().into_string() {
                entries.push(EntryName { name, is_dir });
            }
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn read_link(&self, path: &str) -> io::Result<PathBuf> {
        std::fs::read_link(self.resolve(path))
    }

    fn canonicalize(&self, path: &str) -> io::Result<PathBuf> {
        let resolved = std::fs::canonicalize(self.resolve(path))?;
        // Report paths relative to the root so fake trees look like the host.
        match resolved.strip_prefix(&self.root) {
            Ok(relative) => Ok(Path::new("/").join(relative)),
            Err(_) => Ok(resolved),
        }
    }

    fn exists(&self, path: &str) -> bool {
        self.resolve(path).exists()
    }

    fn owner_uid(&self, path: &str) -> io::Result<u32> {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::metadata(self.resolve(path))?.uid())
    }
}

/// Wrapper counting reads per path (metadata cache / probe throttle tests).
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub struct CountingFiles<F> {
    inner: F,
    reads: std::sync::Mutex<BTreeMap<String, u64>>,
}

#[cfg(any(test, feature = "test-support"))]
impl<F: SystemFiles> CountingFiles<F> {
    pub fn new(inner: F) -> Self {
        Self {
            inner,
            reads: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Reads of one exact path so far.
    pub fn reads_of(&self, path: &str) -> u64 {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    /// Reads of every path containing `fragment`.
    pub fn reads_matching(&self, fragment: &str) -> u64 {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(path, _)| path.contains(fragment))
            .map(|(_, count)| *count)
            .sum()
    }

    fn count(&self, path: &str) {
        *self
            .reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(path.to_owned())
            .or_insert(0) += 1;
    }
}

#[cfg(any(test, feature = "test-support"))]
impl<F: SystemFiles> SystemFiles for CountingFiles<F> {
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        self.count(path);
        self.inner.read_to_string(path)
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<EntryName>> {
        self.inner.read_dir(path)
    }

    fn read_link(&self, path: &str) -> io::Result<PathBuf> {
        self.inner.read_link(path)
    }

    fn canonicalize(&self, path: &str) -> io::Result<PathBuf> {
        self.inner.canonicalize(path)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn owner_uid(&self, path: &str) -> io::Result<u32> {
        self.inner.owner_uid(path)
    }
}

impl<T: SystemFiles + ?Sized> SystemFiles for Arc<T> {
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        (**self).read_to_string(path)
    }

    fn read_dir(&self, path: &str) -> io::Result<Vec<EntryName>> {
        (**self).read_dir(path)
    }

    fn read_link(&self, path: &str) -> io::Result<PathBuf> {
        (**self).read_link(path)
    }

    fn canonicalize(&self, path: &str) -> io::Result<PathBuf> {
        (**self).canonicalize(path)
    }

    fn exists(&self, path: &str) -> bool {
        (**self).exists(path)
    }

    fn owner_uid(&self, path: &str) -> io::Result<u32> {
        (**self).owner_uid(path)
    }
}

/// Maps an I/O error to a read status. Missing files mean the source was
/// removed; permission errors stay distinct; nothing becomes a value.
pub fn read_status_of(error: &io::Error) -> ReadStatus {
    match error.kind() {
        io::ErrorKind::NotFound => ReadStatus::Removed,
        io::ErrorKind::PermissionDenied => ReadStatus::PermissionDenied,
        _ => match error.raw_os_error() {
            // ENODATA / EOPNOTSUPP / ENXIO from sysfs attributes the driver cannot serve.
            Some(61 | 95 | 6) => ReadStatus::Unsupported,
            _ => ReadStatus::Error,
        },
    }
}

/// Reason code for an I/O error (bounded, no paths).
pub fn reason_of(error: &io::Error) -> String {
    match read_status_of(error) {
        ReadStatus::Removed => "SOURCE_REMOVED".to_owned(),
        ReadStatus::PermissionDenied => "PERMISSION_DENIED".to_owned(),
        ReadStatus::Unsupported => "UNSUPPORTED_BY_DRIVER".to_owned(),
        _ => format!("IO_ERROR_{}", error.raw_os_error().unwrap_or(0)),
    }
}

/// Parses a trimmed unsigned decimal.
pub fn parse_u64(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

/// Parses a trimmed signed decimal (hwmon temperatures may be negative).
pub fn parse_i64(text: &str) -> Option<i64> {
    text.trim().parse::<i64>().ok()
}

/// Exact `i64` -> finite `f64` for native readings (|value| < 2^53 in practice;
/// larger magnitudes are rejected rather than silently rounded).
pub fn exact_i64_to_f64(value: i64) -> Option<f64> {
    const LIMIT: i64 = 1 << 53;
    if (-LIMIT..=LIMIT).contains(&value) {
        #[allow(clippy::cast_precision_loss)]
        Some(value as f64)
    } else {
        None
    }
}

/// Stable slot interning: a source keeps its slot for the lifetime of the
/// catalog; slots of removed sources are never handed to another source.
#[derive(Debug, Default, Clone)]
pub struct SlotInterner {
    slots: BTreeMap<SourceId, u32>,
    next: u32,
}

impl SlotInterner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Slot for `source`, allocating a fresh one on first sight.
    pub fn slot_for(&mut self, source: &SourceId) -> Option<u32> {
        if let Some(slot) = self.slots.get(source) {
            return Some(*slot);
        }
        let slot = self.next;
        self.next = self.next.checked_add(1)?;
        self.slots.insert(source.clone(), slot);
        Some(slot)
    }

    pub fn existing(&self, source: &SourceId) -> Option<u32> {
        self.slots.get(source).copied()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// Consistent paging of one read group: page 0 takes a fresh snapshot, later
/// pages serve slices of that same snapshot for the same cycle.
#[derive(Debug, Default)]
pub struct Pager {
    snapshot: Option<PagedSnapshot>,
}

#[derive(Debug)]
struct PagedSnapshot {
    cycle_id: u64,
    group: String,
    records: Vec<Observation>,
    started_boot_ns: u64,
    finished_boot_ns: u64,
}

/// One page served from a snapshot.
#[derive(Debug)]
pub struct Page {
    pub records: Vec<Observation>,
    pub started_boot_ns: u64,
    pub finished_boot_ns: u64,
    pub continues: bool,
}

impl Pager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a fresh snapshot (page 0) for `cycle_id`/`group`.
    pub fn store(
        &mut self,
        cycle_id: u64,
        group: &str,
        records: Vec<Observation>,
        started_boot_ns: u64,
        finished_boot_ns: u64,
    ) {
        self.snapshot = Some(PagedSnapshot {
            cycle_id,
            group: group.to_owned(),
            records,
            started_boot_ns,
            finished_boot_ns,
        });
    }

    /// Serves page `index` of size `page_size` (≤ 256). `None` when no
    /// snapshot exists for this cycle/group (the caller must start at page 0).
    pub fn page(&self, cycle_id: u64, group: &str, index: u32, page_size: usize) -> Option<Page> {
        let snapshot = self.snapshot.as_ref()?;
        if snapshot.cycle_id != cycle_id || snapshot.group != group {
            return None;
        }
        let size = page_size.clamp(1, MAX_RECORDS_PER_BATCH);
        let start = usize::try_from(index).ok()?.checked_mul(size)?;
        if start > snapshot.records.len() || (start == snapshot.records.len() && index > 0) {
            return None;
        }
        let end = start.saturating_add(size).min(snapshot.records.len());
        Some(Page {
            records: snapshot.records[start..end].to_vec(),
            started_boot_ns: snapshot.started_boot_ns,
            finished_boot_ns: snapshot.finished_boot_ns,
            continues: end < snapshot.records.len(),
        })
    }
}

/// Re-probes unsupported/denied capabilities at most once per interval.
#[derive(Debug, Clone)]
pub struct ProbeThrottle {
    interval_ns: u64,
    last_probe: BTreeMap<String, u64>,
}

impl ProbeThrottle {
    pub fn new(unsupported_probe_ms: u64) -> Self {
        Self {
            interval_ns: ms_to_ns(unsupported_probe_ms),
            last_probe: BTreeMap::new(),
        }
    }

    /// Whether `capability` may be probed now; records the probe if so.
    pub fn should_probe(&mut self, capability: &str, now_boot_ns: u64) -> bool {
        match self.last_probe.get(capability) {
            Some(last) if now_boot_ns.saturating_sub(*last) < self.interval_ns => false,
            _ => {
                self.last_probe.insert(capability.to_owned(), now_boot_ns);
                true
            }
        }
    }

    /// Forgets all throttling (driver restart / permission change).
    pub fn reset(&mut self) {
        self.last_probe.clear();
    }
}

/// Builds observations and batches with consistent provenance and timing.
#[derive(Clone)]
pub struct RecordFactory {
    clock: Arc<dyn TimeProvider>,
}

impl fmt::Debug for RecordFactory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecordFactory")
    }
}

impl RecordFactory {
    pub fn new(clock: Arc<dyn TimeProvider>) -> Self {
        Self { clock }
    }

    pub fn now(&self) -> u64 {
        self.clock.now_boot_ns()
    }

    pub fn clock(&self) -> &Arc<dyn TimeProvider> {
        &self.clock
    }

    /// Provenance derived from a catalog descriptor.
    pub fn provenance(descriptor: &SourceDescriptor) -> Provenance {
        Provenance {
            adapter_id: descriptor.adapter_id.clone(),
            source_id: descriptor.source_id.clone(),
            origin_id: descriptor.origin.origin_id.clone(),
            origin_relation: descriptor.origin.relation,
            driver: non_empty(&descriptor.driver, "unknown").into(),
            device_path: non_empty(&descriptor.device_path, "unknown").into(),
            channel: non_empty(&descriptor.channel, "value").into(),
            generation: descriptor.generation,
            profile_id: descriptor.profile_id.clone(),
            profile_revision: descriptor.profile_revision,
        }
    }

    /// Successful reading of a catalogued source. Sources without a proven
    /// MetricKey must use [`RecordFactory::diagnostic_key`] beforehand.
    #[allow(clippy::too_many_arguments)]
    pub fn ok(
        plan: &ReadPlan,
        descriptor: &SourceDescriptor,
        key: dgxtop_core::identity::MetricKey,
        value: Scalar,
        quality: Quality,
        started_boot_ns: u64,
        finished_boot_ns: u64,
        reasons: Vec<String>,
    ) -> Observation {
        Observation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            cycle_id: plan.cycle_id,
            config_revision: plan.config_revision,
            key,
            provenance: Self::provenance(descriptor),
            sample_kind: descriptor.sample_kind,
            read_status: ReadStatus::Ok,
            quality,
            native_unit: descriptor.native_unit.clone(),
            value: Some(value),
            read_started_boot_ns: started_boot_ns,
            read_finished_boot_ns: finished_boot_ns.max(started_boot_ns),
            device_sample_boot_ns: None,
            source_update_interval_ms: descriptor.update_interval_ms,
            reason_codes: bounded_reasons(reasons),
        }
    }

    /// Failed reading: value is `None`, never a substitute number.
    pub fn failed(
        plan: &ReadPlan,
        descriptor: &SourceDescriptor,
        key: dgxtop_core::identity::MetricKey,
        status: ReadStatus,
        started_boot_ns: u64,
        finished_boot_ns: u64,
        reason: String,
    ) -> Observation {
        let status = if status == ReadStatus::Ok {
            ReadStatus::Error
        } else {
            status
        };
        Observation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            cycle_id: plan.cycle_id,
            config_revision: plan.config_revision,
            key,
            provenance: Self::provenance(descriptor),
            sample_kind: descriptor.sample_kind,
            read_status: status,
            quality: Quality::Unknown,
            native_unit: descriptor.native_unit.clone(),
            value: None,
            read_started_boot_ns: started_boot_ns,
            read_finished_boot_ns: finished_boot_ns.max(started_boot_ns),
            device_sample_boot_ns: None,
            source_update_interval_ms: descriptor.update_interval_ms,
            reason_codes: bounded_reasons(vec![reason]),
        }
    }

    /// Assembles a batch answering `plan`.
    pub fn batch(
        plan: &ReadPlan,
        records: Vec<Observation>,
        started_boot_ns: u64,
        finished_boot_ns: u64,
        continues: bool,
        error_code: Option<String>,
    ) -> ObservationBatch {
        let any_failed = records.iter().any(|record| !record.read_status.is_ok());
        let status = if continues || any_failed || error_code.is_some() {
            BatchStatus::Partial
        } else {
            BatchStatus::Ok
        };
        let error_code = if continues {
            Some(PAGE_CONTINUES.to_owned())
        } else {
            error_code
        };
        ObservationBatch {
            protocol_version: IPC_PROTOCOL_VERSION,
            adapter_id: plan.adapter_id.clone(),
            host_generation: plan.host_generation,
            catalog_revision: plan.catalog_revision,
            config_revision: plan.config_revision,
            request_id: plan.request_id,
            cycle_id: plan.cycle_id,
            read_group: plan.read_group.clone(),
            started_boot_ns,
            finished_boot_ns: finished_boot_ns.max(started_boot_ns),
            records,
            status,
            error_code,
        }
    }

    /// Batch reporting that the whole request failed (e.g. deadline expired).
    pub fn failed_batch(plan: &ReadPlan, now_boot_ns: u64, error_code: &str) -> ObservationBatch {
        ObservationBatch {
            protocol_version: IPC_PROTOCOL_VERSION,
            adapter_id: plan.adapter_id.clone(),
            host_generation: plan.host_generation,
            catalog_revision: plan.catalog_revision,
            config_revision: plan.config_revision,
            request_id: plan.request_id,
            cycle_id: plan.cycle_id,
            read_group: plan.read_group.clone(),
            started_boot_ns: now_boot_ns,
            finished_boot_ns: now_boot_ns,
            records: Vec::new(),
            status: BatchStatus::Failed,
            error_code: Some(error_code.to_owned()),
        }
    }
}

fn non_empty(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn bounded_reasons(mut reasons: Vec<String>) -> Vec<String> {
    reasons.retain(|reason| !reason.is_empty());
    for reason in &mut reasons {
        if reason.len() > 80 {
            *reason = dgxtop_core::text::truncate_utf8(reason, 80).to_owned();
        }
    }
    reasons.truncate(dgxtop_core::observation::MAX_REASON_CODES);
    reasons
}

/// Typed read group from a static name.
pub fn group(name: &str) -> Option<ReadGroup> {
    ReadGroup::new(name).ok()
}

/// Typed adapter id from a static name.
pub fn adapter(name: &str) -> Option<AdapterId> {
    AdapterId::new(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::source_id;

    #[test]
    fn interner_never_reuses_slots() {
        let mut interner = SlotInterner::new();
        let first = interner.slot_for(&source_id("a")).unwrap();
        let second = interner.slot_for(&source_id("b")).unwrap();
        assert_ne!(first, second);
        assert_eq!(interner.slot_for(&source_id("a")), Some(first));
        assert_eq!(interner.existing(&source_id("c")), None);
    }

    #[test]
    fn probe_throttle_limits_reprobes() {
        let mut throttle = ProbeThrottle::new(60_000);
        assert!(throttle.should_probe("nvml.fan", 0));
        assert!(!throttle.should_probe("nvml.fan", ms_to_ns(59_999)));
        assert!(throttle.should_probe("nvml.fan", ms_to_ns(60_000)));
        throttle.reset();
        assert!(throttle.should_probe("nvml.fan", ms_to_ns(60_001)));
    }

    #[test]
    fn io_errors_map_to_statuses() {
        assert_eq!(
            read_status_of(&io::Error::from(io::ErrorKind::NotFound)),
            ReadStatus::Removed
        );
        assert_eq!(
            read_status_of(&io::Error::from(io::ErrorKind::PermissionDenied)),
            ReadStatus::PermissionDenied
        );
        assert_eq!(
            read_status_of(&io::Error::from_raw_os_error(61)),
            ReadStatus::Unsupported
        );
        assert_eq!(
            read_status_of(&io::Error::from_raw_os_error(5)),
            ReadStatus::Error
        );
    }

    #[test]
    fn exact_conversion_rejects_huge_values() {
        assert_eq!(exact_i64_to_f64(-5000), Some(-5000.0));
        assert_eq!(exact_i64_to_f64(i64::MAX), None);
    }

    #[test]
    fn rooted_files_read_fake_tree() {
        let directory = std::env::temp_dir().join(format!("dgxtop-common-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("proc")).unwrap();
        std::fs::write(directory.join("proc/loadavg"), "0.5 0.6 0.7 2/300 42\n").unwrap();
        let files = CountingFiles::new(RootedFiles::new(&directory));
        assert_eq!(
            files.read_to_string("/proc/loadavg").unwrap(),
            "0.5 0.6 0.7 2/300 42\n"
        );
        assert_eq!(files.reads_of("/proc/loadavg"), 1);
        assert!(files.read_to_string("/proc/missing").is_err());
        assert_eq!(files.read_dir("/proc").unwrap()[0].name, "loadavg");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
