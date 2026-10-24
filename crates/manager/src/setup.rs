//! Construction inputs handed over by the composition root.
//!
//! Every port is a `dgxtop_core` abstraction: the Manager never names a
//! runtime, store, adapter or UI type. Each service receives only the ports
//! it needs (the scheduler the adapter port, ingestion the collector/state
//! ports, the ProcessService the audit and action ports, ...).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crossbeam_channel::Sender;
use dgxtop_core::action::{ActionOp, AuditOp, PersistenceStatus};
use dgxtop_core::budget::{ByteLedger, QueueGate};
use dgxtop_core::canonical::CollectionPlan;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::config::{Config, ValueOrigin};
use dgxtop_core::identity::{BootId, HostId, PidNamespace};
use dgxtop_core::observation::AdapterManifest;
use dgxtop_core::ports::{
    AdapterJob, Collector, ConfigJob, ControlCompletion, Dispatch, HistoryJob, IngestionCompletion,
    Logger, SnapshotReader, StateJob, TrendReader,
};
use dgxtop_core::quality::Namespace;

use crate::error::ManagerError;
use crate::presentation::HostFacts;
pub use crate::process_link::ProcessHistoryHandles;

/// Capacity of the data-path completion inbox (adapter replies, collector
/// results, state acks, host status). Payload bytes are bounded separately by
/// the ingress `QueueGate` permits the batches carry.
pub const INGESTION_INBOX_CAPACITY: usize = 256;
/// Capacity of the control completion inbox (config, audit, action, history).
/// The Manager bounds its outstanding control jobs far below this, so workers
/// may use a blocking send without ever waiting long: control completions are
/// never droppable.
pub const CONTROL_INBOX_CAPACITY: usize = 256;

/// Every port the Manager uses. Dispatch ports must never block.
pub struct ManagerPorts {
    pub clock: Arc<dyn TimeProvider>,
    pub adapters: Box<dyn Dispatch<AdapterJob>>,
    pub collectors: Box<dyn Dispatch<CollectionPlan>>,
    pub collector_registry: Vec<Arc<dyn Collector>>,
    /// The live state's actor: commits, gaps, maintenance (D-79).
    pub state: Box<dyn Dispatch<StateJob>>,
    pub snapshots: Arc<dyn SnapshotReader>,
    /// The state's short trends (D-99).
    pub trends: Arc<dyn TrendReader>,
    /// The Overview History's reads (D-82).
    pub history: Box<dyn Dispatch<HistoryJob>>,
    pub config: Box<dyn Dispatch<ConfigJob>>,
    pub audit: Box<dyn Dispatch<AuditOp>>,
    pub actions: Box<dyn Dispatch<ActionOp>>,
    /// The history's process actor (D-76, D-79): the GPU process runs and
    /// colors; `None` without one (tests and tools).
    pub process_history: Option<ProcessHistoryHandles>,
    pub logger: Arc<dyn Logger>,
    pub ledger: ByteLedger,
    /// Ingress admission gate shared with the runtime (observed for overload).
    pub ingress: QueueGate,
}

/// Static facts and configuration of this run.
#[derive(Debug, Clone)]
pub struct ManagerSetup {
    /// Configuration as persisted (CAS base).
    pub saved_config: Config,
    /// Saved configuration plus explicit CLI overrides.
    pub effective_config: Config,
    /// Field provenance; `CliOverride` paths are re-applied after every swap.
    pub value_origins: BTreeMap<String, ValueOrigin>,
    /// The configuration file as its repository reported it after loading
    /// (`ConfigRepository::status`: state, size and limit).
    pub config_file: PersistenceStatus,
    pub host_id: HostId,
    pub boot_id: BootId,
    pub pid_namespace: PidNamespace,
    /// Real effective uid of this process (never taken from commands).
    pub euid: u32,
    /// The uid of the user who started this process with sudo (its
    /// `SUDO_UID`), whom a root session's process actions act for (D-127).
    pub sudo_uid: Option<u32>,
    pub self_pid: i32,
    pub adapters: Vec<AdapterManifest>,
    pub facts: HostFacts,
    /// Namespace accepted into canonical state (`live`, or `replay` in `--replay`).
    pub namespace: Namespace,
}

/// Senders the composition root hands to the runtime workers.
#[derive(Debug, Clone)]
pub struct ManagerInboxes {
    pub ingestion: Sender<IngestionCompletion>,
    pub control: Sender<ControlCompletion>,
}

/// Rejects inconsistent setups before any thread starts.
pub(crate) fn validate(setup: &ManagerSetup, ports: &ManagerPorts) -> Result<(), ManagerError> {
    for (name, config) in [
        ("saved_config", &setup.saved_config),
        ("effective_config", &setup.effective_config),
    ] {
        if let Err(issues) = config.validate() {
            let first = issues
                .first()
                .map(|issue| format!("{}: {}", issue.path, issue.message))
                .unwrap_or_default();
            return Err(invalid(format!("{name} is invalid ({first})")));
        }
    }
    let restart_only = setup
        .effective_config
        .restart_required_changes(&setup.saved_config);
    if !restart_only.is_empty() {
        return Err(invalid(format!(
            "effective configuration differs from the saved one in restart-only fields: {}",
            restart_only.join(", ")
        )));
    }
    if ports.clock.boot_id() != setup.boot_id {
        return Err(invalid("setup boot_id differs from the clock's boot id"));
    }
    if setup.self_pid <= 0 {
        return Err(invalid("self_pid must be positive"));
    }
    validate_adapters(setup)?;
    validate_collectors(&ports.collector_registry)
}

fn validate_adapters(setup: &ManagerSetup) -> Result<(), ManagerError> {
    let limit = usize::try_from(setup.effective_config.sampling.max_adapters).unwrap_or(0);
    if setup.adapters.len() > limit {
        return Err(invalid(format!(
            "{} adapters exceed sampling.max_adapters ({limit})",
            setup.adapters.len()
        )));
    }
    let mut seen = BTreeSet::new();
    for manifest in &setup.adapters {
        manifest
            .validate()
            .map_err(|error| invalid(format!("adapter {}: {error}", manifest.adapter_id)))?;
        if !seen.insert(manifest.adapter_id.clone()) {
            return Err(invalid(format!(
                "duplicate adapter {}",
                manifest.adapter_id
            )));
        }
    }
    Ok(())
}

fn validate_collectors(registry: &[Arc<dyn Collector>]) -> Result<(), ManagerError> {
    let mut seen = BTreeSet::new();
    for collector in registry {
        if !seen.insert(collector.id().clone()) {
            return Err(invalid(format!("duplicate collector {}", collector.id())));
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ManagerError {
    ManagerError::InvalidSetup(message.into())
}
