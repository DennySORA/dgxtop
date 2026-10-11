//! Test doubles of the Manager's ports for the suites that assemble a
//! Manager (feature `test-support`): each site names only the ports it
//! wires for real and takes the rest with `..inert_ports(...)`, so adding
//! or removing a port changes this one place.

use std::sync::Arc;

use dgxtop_core::action::{ActionOp, AuditOp};
use dgxtop_core::budget::{ByteLedger, QueueGate};
use dgxtop_core::canonical::CollectionPlan;
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::ports::{AdapterJob, ConfigJob, HistoryJob, SnapshotReader, StateJob};
use dgxtop_core::testing::{CapturingDispatch, MemoryLogger};
use dgxtop_core::trends::TrendBook;

use crate::ManagerPorts;

/// Ports where nothing happens: every job is captured and never reaches a
/// worker, the trends are empty, no process history is attached and the
/// log is kept in memory.
pub fn inert_ports(
    clock: Arc<dyn TimeProvider>,
    snapshots: Arc<dyn SnapshotReader>,
    ledger: &ByteLedger,
    ingress: QueueGate,
) -> ManagerPorts {
    ManagerPorts {
        clock,
        adapters: Box::new(CapturingDispatch::<AdapterJob>::new()),
        collectors: Box::new(CapturingDispatch::<CollectionPlan>::new()),
        collector_registry: Vec::new(),
        state: Box::new(CapturingDispatch::<StateJob>::new()),
        snapshots,
        trends: Arc::new(TrendBook::default()),
        history: Box::new(CapturingDispatch::<HistoryJob>::new()),
        config: Box::new(CapturingDispatch::<ConfigJob>::new()),
        audit: Box::new(CapturingDispatch::<AuditOp>::new()),
        actions: Box::new(CapturingDispatch::<ActionOp>::new()),
        process_history: None,
        logger: Arc::new(MemoryLogger::default()),
        ledger: ledger.clone(),
        ingress,
    }
}

#[cfg(test)]
mod tests {
    use dgxtop_core::budget::Partition;
    use dgxtop_core::testing::FakeClock;

    use super::*;
    use crate::Manager;
    use crate::test_support::{FakeSnapshots, HarnessOptions, harness_setup};

    /// The inert ports alone start a Manager and open a session: nothing
    /// is attached that a suite did not ask for.
    #[test]
    fn a_manager_starts_on_the_inert_ports_alone() {
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let ingress = QueueGate::new("ingress", 32, 8 << 20, ledger.clone(), Partition::Ingress);
        let ports = inert_ports(
            FakeClock::new(1_000_000_000),
            Arc::new(FakeSnapshots::default()),
            &ledger,
            ingress,
        );
        assert!(ports.process_history.is_none());
        assert!(ports.collector_registry.is_empty());
        let (mut manager, _inboxes) =
            Manager::new(harness_setup(&HarnessOptions::default()), ports).unwrap();
        assert!(manager.open_session().is_ok());
    }
}
