//! The one-shot snapshot (`--snapshot-json`, D-140): what the monitor knows
//! at one moment, as one JSON document for scripts.
//!
//! It holds every committed metric under the name the naming contract gives
//! it (metric id, entity, scope) with its value in the canonical unit and
//! the evidence the panels show beside it, the entities those metrics
//! belong to, and the GPU processes.
//!
//! The document is built from the committed snapshot and the catalog alone,
//! so it never differs from what a panel would draw of the same moment. A
//! metric without a value keeps its row and says why (`reasons`): a missing
//! reading is never 0. Command lines are left out: they may hold secrets,
//! and a snapshot is made to be piped and pasted. What names the host, its
//! hardware and its users stays unless the document is asked for without
//! it ([`SnapshotReport::redacted`], D-145).

mod redact;

pub use redact::REDACTED;

use std::collections::BTreeMap;

use serde::Serialize;

use crate::canonical::{
    CatalogSnapshot, GpuUsageKind, ImmutableSnapshot, ProcessGpuLink, ProcessRow, ProcessTable,
    SnapshotEntry,
};
use crate::metric::{self, CanonicalUnit, Scalar, ScalarView};
use crate::observation::{EntityKind, EntityRecord};
use crate::quality::{Namespace, Quality, SampleKind, SelectionState};
use crate::text::{MAX_BASENAME_BYTES, sanitize_display};
use crate::users::UserNames;
use crate::view::{HostState, HostStatus};

/// Format tag of the document; a change that breaks a reader renames it.
pub const SNAPSHOT_FORMAT: &str = "dgxtop-snapshot/1";

const NANOS_PER_MILLI: u64 = 1_000_000;

/// What the document says of the run that made it, beside its data.
#[derive(Debug, Clone, Copy)]
pub struct ReportContext<'a> {
    pub dgxtop_version: &'a str,
    pub host_name: &'a str,
    pub kernel: Option<&'a str>,
    pub namespace: Namespace,
    /// The sampling interval the values were read at.
    pub interval_ms: u32,
    /// The moment of the document on the boot clock: ages and staleness
    /// are told against it.
    pub now_boot_ns: u64,
    /// The same moment in milliseconds since the Unix epoch.
    pub now_unix_ms: u64,
    /// The login names of the host's users (D-138).
    pub users: &'a UserNames,
    /// The adapter hosts at that moment.
    pub adapters: &'a [HostStatus],
}

/// The document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotReport {
    pub format: &'static str,
    pub dgxtop_version: String,
    pub host: ReportHost,
    /// `replay` for simulated data, which never is a host's reading.
    pub namespace: Namespace,
    pub taken_unix_ms: u64,
    pub uptime_ms: u64,
    pub interval_ms: u32,
    pub snapshot_revision: u64,
    /// The readers of the host's interfaces: one that is not `healthy`
    /// delivered nothing, or not everything, of what it reads.
    pub adapters: Vec<ReportAdapter>,
    pub entities: Vec<ReportEntity>,
    pub metrics: Vec<ReportMetric>,
    /// The processes that use a GPU, as the process table lists them.
    pub processes: Vec<ReportProcess>,
    /// The process table's bound may have dropped a GPU process or one of
    /// its GPUs.
    pub processes_truncated: bool,
    /// What names the host, its hardware and its users was taken out
    /// (`--redact`, D-145).
    pub redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportHost {
    /// The host's identity as dgxtop keys its data: a hash, not a name.
    pub id: Option<String>,
    pub name: String,
    pub kernel: Option<String>,
}

/// One adapter host and how it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportAdapter {
    pub id: String,
    pub state: HostState,
    /// Why it is not healthy, where the supervisor says.
    pub detail: Option<String>,
}

/// One thing metrics are read of: the host, a GPU, a disk, an interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportEntity {
    pub id: String,
    pub kind: EntityKind,
    pub name: String,
    pub parent: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

/// A value as JSON writes a number: whole where the metric counts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ReportValue {
    Integer(u64),
    Number(f64),
}

impl From<Scalar> for ReportValue {
    fn from(scalar: Scalar) -> Self {
        match scalar.view() {
            ScalarView::U64(value) => Self::Integer(value),
            ScalarView::F64(value) => Self::Number(value),
        }
    }
}

/// One committed metric.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportMetric {
    pub metric: String,
    pub entity: String,
    pub scope: String,
    /// The catalog's label of the metric; `None` for an id it does not
    /// list.
    pub label: Option<&'static str>,
    /// `None` while the metric has no value; `reasons` then says why.
    pub value: Option<ReportValue>,
    pub unit: CanonicalUnit,
    pub state: SelectionState,
    pub quality: Quality,
    pub kind: SampleKind,
    /// The value is older than its source's period allows: it is the last
    /// one read, not the current one.
    pub stale: bool,
    /// Time since the value was measured, or read where the source tells no
    /// time of its own.
    pub age_ms: u64,
    pub source: Option<String>,
    pub reasons: Vec<String>,
}

/// One process that uses a GPU.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportProcess {
    pub pid: i32,
    /// With the PID, the process itself: a PID alone may be reused.
    pub start_time_ticks: u64,
    pub name: String,
    pub uid: Option<u32>,
    /// The login name of `uid` where the host's passwd file lists it.
    pub user: Option<String>,
    pub state: Option<char>,
    /// Of one core, so several busy threads read above 100.
    pub cpu_percent: Option<f64>,
    pub rss_bytes: Option<u64>,
    pub gpus: Vec<ReportProcessGpu>,
}

/// What a process uses of one GPU.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportProcessGpu {
    pub gpu: String,
    pub usage: GpuUsageKind,
    pub utilization_percent: Option<f64>,
    pub encoder_percent: Option<f64>,
    pub decoder_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

impl SnapshotReport {
    /// The document of `snapshot`, its entities named by `catalog`.
    pub fn of(
        snapshot: &ImmutableSnapshot,
        catalog: &CatalogSnapshot,
        context: &ReportContext<'_>,
    ) -> Self {
        let table = &snapshot.process_table;
        Self {
            format: SNAPSHOT_FORMAT,
            dgxtop_version: context.dgxtop_version.to_owned(),
            host: ReportHost {
                id: catalog.host_id.as_ref().map(ToString::to_string),
                name: context.host_name.to_owned(),
                kernel: context.kernel.map(str::to_owned),
            },
            namespace: context.namespace,
            taken_unix_ms: context.now_unix_ms,
            uptime_ms: context.now_boot_ns / NANOS_PER_MILLI,
            interval_ms: context.interval_ms,
            snapshot_revision: snapshot.revision,
            adapters: context.adapters.iter().map(report_adapter).collect(),
            entities: entities_of(catalog),
            metrics: metrics_of(snapshot, context.now_boot_ns),
            processes: gpu_processes(table, context.users),
            processes_truncated: table.gpu_rows_incomplete(),
            redacted: false,
        }
    }
}

fn report_adapter(host: &HostStatus) -> ReportAdapter {
    ReportAdapter {
        id: host.adapter_id.clone(),
        state: host.state,
        detail: host.detail.clone(),
    }
}

/// The catalog's entities that still exist, by id.
fn entities_of(catalog: &CatalogSnapshot) -> Vec<ReportEntity> {
    catalog
        .entities
        .values()
        .filter(|entity| entity.removed_boot_ns.is_none())
        .map(|entity| report_entity(entity))
        .collect()
}

fn report_entity(entity: &EntityRecord) -> ReportEntity {
    ReportEntity {
        id: entity.entity_id.as_str().to_owned(),
        kind: entity.kind,
        name: entity.display_name.clone(),
        parent: entity
            .parent_id
            .as_ref()
            .map(|parent| parent.as_str().to_owned()),
        attributes: entity
            .attributes
            .iter()
            .map(|attribute| (attribute.key.clone(), attribute.value.clone()))
            .collect(),
    }
}

/// Every committed metric, by entity, metric and scope.
fn metrics_of(snapshot: &ImmutableSnapshot, now_boot_ns: u64) -> Vec<ReportMetric> {
    let mut metrics: Vec<ReportMetric> = snapshot
        .domains
        .values()
        .flat_map(|domain| domain.samples.values())
        .map(|entry| report_metric(entry, now_boot_ns))
        .collect();
    metrics.sort_by(|left, right| {
        (&left.entity, &left.metric, &left.scope).cmp(&(&right.entity, &right.metric, &right.scope))
    });
    metrics
}

fn report_metric(entry: &SnapshotEntry, now_boot_ns: u64) -> ReportMetric {
    let sample = &entry.sample;
    let measured_boot_ns = sample
        .observed_boot_ns
        .unwrap_or(sample.read_finished_boot_ns);
    ReportMetric {
        metric: sample.key.metric_id.as_str().to_owned(),
        entity: sample.key.entity_id.as_str().to_owned(),
        scope: sample.key.scope.as_str().to_owned(),
        label: metric::descriptor(sample.key.metric_id.as_str()).map(|metric| metric.label),
        value: sample
            .value
            .filter(|_| sample.selection_state.has_value())
            .map(ReportValue::from),
        unit: sample.unit,
        state: sample.selection_state,
        quality: sample.quality,
        kind: sample.sample_kind,
        stale: entry
            .stale_after_boot_ns
            .is_some_and(|deadline| now_boot_ns >= deadline),
        age_ms: now_boot_ns.saturating_sub(measured_boot_ns) / NANOS_PER_MILLI,
        source: sample
            .source_id
            .as_ref()
            .map(|source| source.as_str().to_owned()),
        reasons: sample.reason_codes.clone(),
    }
}

/// The table's processes that use a GPU, in the table's order.
fn gpu_processes(table: &ProcessTable, users: &UserNames) -> Vec<ReportProcess> {
    table
        .rows
        .iter()
        .filter(|row| !row.gpu.is_empty())
        .map(|row| report_process(row, users))
        .collect()
}

fn report_process(row: &ProcessRow, users: &UserNames) -> ReportProcess {
    ReportProcess {
        pid: row.key.pid.get(),
        start_time_ticks: row.key.start_time_ticks,
        name: sanitize_display(&row.name, MAX_BASENAME_BYTES),
        uid: row.uid,
        user: row
            .uid
            .and_then(|uid| users.name_of(uid))
            .map(str::to_owned),
        state: row.state.filter(char::is_ascii_graphic),
        cpu_percent: finite(row.cpu_percent),
        rss_bytes: row.rss_bytes,
        gpus: row.gpu.iter().map(report_link).collect(),
    }
}

fn report_link(link: &ProcessGpuLink) -> ReportProcessGpu {
    ReportProcessGpu {
        gpu: link.gpu_entity_id.as_str().to_owned(),
        usage: link.usage,
        utilization_percent: finite(link.utilization_percent),
        encoder_percent: finite(link.encoder_percent),
        decoder_percent: finite(link.decoder_percent),
        memory_bytes: link.memory_bytes,
    }
}

/// JSON has no NaN or infinity: such a figure is no figure.
fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::metric::ids;
    use crate::testing::{
        catalog_of, entity_id, entity_record, gpu_process, host_id, metric_key, process_table,
        snapshot_entry, snapshot_of,
    };

    const GPU: &str = "gpu:GPU-aaaa1111";
    const SECOND: u64 = 1_000_000_000;

    fn context(users: &UserNames) -> ReportContext<'_> {
        ReportContext {
            dgxtop_version: "9.9.9",
            host_name: "acme-node",
            kernel: Some("6.8.0-acme"),
            namespace: Namespace::Live,
            interval_ms: 1000,
            now_boot_ns: 10 * SECOND,
            now_unix_ms: 1_800_000_000_000,
            users,
            adapters: &[],
        }
    }

    fn catalog() -> CatalogSnapshot {
        let mut gpu = entity_record(
            GPU,
            EntityKind::Gpu,
            "Acme GPU 9000",
            &[("nvml_index", "0"), ("driver_version", "1.2.3")],
        );
        gpu.parent_id = Some(entity_id("host"));
        let mut unplugged = entity_record("gpu:GPU-gone", EntityKind::Gpu, "Gone", &[]);
        unplugged.removed_boot_ns = Some(SECOND);
        let mut catalog = catalog_of(vec![
            entity_record("host", EntityKind::Host, "acme-node", &[]),
            gpu,
            unplugged,
        ]);
        catalog.host_id = Some(host_id("host-hash-1"));
        catalog
    }

    /// Every committed metric is listed once under its name, by entity,
    /// metric and scope, with its value in the canonical unit; one without
    /// a value keeps its row, its reasons and no number.
    #[test]
    fn every_committed_metric_is_listed_with_its_evidence() {
        let utilization = metric_key(GPU, ids::GPU_UTILIZATION_COMPUTE, "gpu");
        let memory = metric_key("memory", ids::MEMORY_OOM_KILLS, "memory");
        let rate = metric_key("memory", ids::MEMORY_SWAP_IN_BPS, "memory");
        let mut busy = snapshot_entry(&utilization, Some(94.5), 9 * SECOND);
        busy.sample.observed_boot_ns = Some(9 * SECOND + SECOND / 2);
        busy.stale_after_boot_ns = Some(12 * SECOND);
        let mut kills = snapshot_entry(&memory, None, 8 * SECOND);
        kills.sample.value = Some(Scalar::from_u64(4));
        kills.sample.selection_state = SelectionState::Selected;
        kills.sample.unit = CanonicalUnit::Count;
        kills.stale_after_boot_ns = Some(9 * SECOND);
        let mut warming = snapshot_entry(&rate, None, 9 * SECOND);
        warming.sample.unit = CanonicalUnit::BytesPerSecond;
        warming.sample.reason_codes = vec!["WARMUP".to_owned()];
        let snapshot = snapshot_of(
            7,
            vec![("gpu", busy), ("memory", warming), ("memory", kills)],
        );
        let users = UserNames::default();
        let report = SnapshotReport::of(&snapshot, &catalog(), &context(&users));

        assert_eq!(report.format, SNAPSHOT_FORMAT);
        assert_eq!(report.snapshot_revision, 7);
        assert_eq!(report.uptime_ms, 10_000);
        assert_eq!(report.host.id.as_deref(), Some("host-hash-1"));
        let names: Vec<(&str, &str)> = report
            .metrics
            .iter()
            .map(|metric| (metric.entity.as_str(), metric.metric.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                (GPU, ids::GPU_UTILIZATION_COMPUTE),
                ("memory", ids::MEMORY_OOM_KILLS),
                ("memory", ids::MEMORY_SWAP_IN_BPS),
            ]
        );
        let [busy, kills, warming] = report.metrics.as_slice() else {
            panic!("three metrics: {:?}", report.metrics);
        };
        assert_eq!(busy.value, Some(ReportValue::Number(94.5)));
        assert_eq!(busy.unit, CanonicalUnit::Percent);
        assert_eq!(busy.age_ms, 500, "since it was measured");
        assert!(!busy.stale);
        assert_eq!(busy.source.as_deref(), Some("test:source:1"));
        assert!(busy.label.is_some());
        assert_eq!(kills.value, Some(ReportValue::Integer(4)));
        assert_eq!(kills.age_ms, 2000, "since it was read");
        assert!(kills.stale, "past its deadline: the last value, said so");
        assert_eq!(warming.value, None, "no reading is never 0");
        assert_eq!(warming.state, SelectionState::Unavailable);
        assert_eq!(warming.reasons, vec!["WARMUP".to_owned()]);
    }

    /// The entities are the catalog's that still exist, with their names,
    /// parents and attributes.
    #[test]
    fn entities_name_what_the_metrics_belong_to() {
        let users = UserNames::default();
        let report =
            SnapshotReport::of(&ImmutableSnapshot::default(), &catalog(), &context(&users));
        let ids: Vec<&str> = report
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect();
        assert_eq!(ids, vec![GPU, "host"], "the unplugged GPU is left out");
        let gpu = &report.entities[0];
        assert_eq!(gpu.kind, EntityKind::Gpu);
        assert_eq!(gpu.name, "Acme GPU 9000");
        assert_eq!(gpu.parent.as_deref(), Some("host"));
        assert_eq!(
            gpu.attributes.get("driver_version").map(String::as_str),
            Some("1.2.3")
        );
        assert!(report.metrics.is_empty() && report.processes.is_empty());
    }

    /// The processes are those that use a GPU, with their owner's name and
    /// what they use of each GPU; a command line never leaves the monitor.
    #[test]
    fn processes_are_the_gpu_processes_without_their_command_lines() {
        let mut train = gpu_process(4242, "python3", &[(GPU, Some(8 << 30))]);
        train.command = "python3 train.py --token acme-secret".to_owned();
        train.uid = Some(1000);
        train.cpu_percent = Some(250.0);
        train.rss_bytes = Some(2 << 30);
        train.gpu[0].utilization_percent = Some(94.0);
        train.gpu[0].encoder_percent = Some(35.0);
        train.gpu[0].decoder_percent = Some(f64::NAN);
        let mut shell = gpu_process(7, "bash", &[]);
        shell.cpu_percent = Some(1.0);
        let mut table = process_table(3, 9 * SECOND, vec![train, shell]);
        table.gpu_links_truncated = true;
        let snapshot = ImmutableSnapshot {
            revision: 3,
            process_table: Arc::new(table),
            ..ImmutableSnapshot::default()
        };
        let users = UserNames::from_passwd("alice:x:1000:1000::/home/alice:/bin/sh\n");
        let report = SnapshotReport::of(&snapshot, &catalog(), &context(&users));

        assert!(report.processes_truncated);
        let [process] = report.processes.as_slice() else {
            panic!("only the GPU process: {:?}", report.processes);
        };
        assert_eq!(process.pid, 4242);
        assert_eq!(process.name, "python3");
        assert_eq!(process.user.as_deref(), Some("alice"));
        assert_eq!(process.uid, Some(1000));
        assert_eq!(process.cpu_percent, Some(250.0));
        assert_eq!(process.rss_bytes, Some(2 << 30));
        let [link] = process.gpus.as_slice() else {
            panic!("one GPU: {:?}", process.gpus);
        };
        assert_eq!(link.gpu, GPU);
        assert_eq!(link.memory_bytes, Some(8 << 30));
        assert_eq!(link.utilization_percent, Some(94.0));
        assert_eq!(link.encoder_percent, Some(35.0));
        assert_eq!(link.decoder_percent, None, "not a number: no figure");
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("acme-secret"), "{json}");
        assert!(!json.contains("train.py"), "{json}");

        // A bound that cut the table dropped no GPU process while a row
        // without a GPU was kept (they go first); with GPU rows alone left,
        // one may be missing.
        let truncated = |keep_shell: bool| {
            let mut table = (*snapshot.process_table).clone();
            table.gpu_links_truncated = false;
            table.truncated = true;
            table.rows.retain(|row| keep_shell || !row.gpu.is_empty());
            let bounded = ImmutableSnapshot {
                process_table: Arc::new(table),
                ..ImmutableSnapshot::default()
            };
            SnapshotReport::of(&bounded, &catalog(), &context(&users)).processes_truncated
        };
        assert!(!truncated(true), "every GPU process was kept");
        assert!(truncated(false));
    }

    /// The document's JSON: plain numbers (whole ones for counts), `null`
    /// for what is unknown and the names the contract spells.
    #[test]
    fn the_document_is_plain_json() {
        let utilization = metric_key(GPU, ids::GPU_UTILIZATION_COMPUTE, "gpu");
        let kills_key = metric_key("memory", ids::MEMORY_OOM_KILLS, "memory");
        let mut kills = snapshot_entry(&kills_key, None, 9 * SECOND);
        kills.sample.value = Some(Scalar::from_u64(4));
        kills.sample.selection_state = SelectionState::Selected;
        kills.sample.unit = CanonicalUnit::Count;
        let snapshot = snapshot_of(
            2,
            vec![
                ("gpu", snapshot_entry(&utilization, None, 9 * SECOND)),
                ("memory", kills),
            ],
        );
        let users = UserNames::default();
        let host = |adapter: &str, state: HostState, detail: Option<&str>| HostStatus {
            adapter_id: adapter.to_owned(),
            state,
            generation: 1,
            pid: Some(4242),
            last_success_boot_ns: None,
            timeouts_total: 0,
            consecutive_failures: 0,
            restarts_total: 0,
            backoff_until_boot_ns: None,
            detail: detail.map(str::to_owned),
        };
        let hosts = [
            host("procfs", HostState::Healthy, None),
            host("nvml", HostState::Unavailable, Some("driver not loaded")),
        ];
        let mut replayed = context(&users);
        replayed.namespace = Namespace::Replay;
        replayed.kernel = None;
        replayed.adapters = &hosts;
        let report = SnapshotReport::of(&snapshot, &catalog(), &replayed);
        let json = serde_json::to_value(&report).unwrap();
        // Which reader delivered: an adapter's state and why, no more.
        assert_eq!(json["adapters"][0]["id"], "procfs");
        assert_eq!(json["adapters"][0]["state"], "healthy");
        assert!(json["adapters"][0]["detail"].is_null());
        assert_eq!(json["adapters"][1]["state"], "unavailable");
        assert_eq!(json["adapters"][1]["detail"], "driver not loaded");
        assert!(json["adapters"][0].get("pid").is_none());
        assert_eq!(json["format"], "dgxtop-snapshot/1");
        assert_eq!(json["dgxtop_version"], "9.9.9");
        assert_eq!(json["namespace"], "replay");
        assert_eq!(json["host"]["name"], "acme-node");
        assert!(json["host"]["kernel"].is_null());
        assert_eq!(json["taken_unix_ms"], 1_800_000_000_000_u64);
        assert_eq!(json["interval_ms"], 1000);
        let unknown = &json["metrics"][0];
        assert_eq!(unknown["metric"], "gpu.utilization.compute");
        assert_eq!(unknown["entity"], GPU);
        assert_eq!(unknown["scope"], "gpu");
        assert!(unknown["value"].is_null());
        assert_eq!(unknown["unit"], "percent");
        assert_eq!(unknown["state"], "unavailable");
        let kills = &json["metrics"][1];
        assert_eq!(kills["value"], 4);
        assert!(kills["value"].is_u64(), "a count is a whole number");
        assert_eq!(kills["unit"], "count");
        assert_eq!(kills["state"], "selected");
        assert_eq!(kills["kind"], "measured");
        assert_eq!(json["entities"][0]["kind"], "gpu");
        assert_eq!(json["entities"][0]["attributes"]["nvml_index"], "0");
        assert_eq!(json["processes"].as_array().unwrap().len(), 0);
        assert_eq!(json["processes_truncated"], false);
    }
}
