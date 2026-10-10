//! Adapter host table fed by runtime `HostStatus` completions.
//!
//! The table is bounded to the adapters of the setup. It drives scheduling
//! (generation and dispatchability), the header/sources views and the
//! protected-pid list of the ProcessService (managed hosts may never be
//! signalled by a user action).

use std::collections::BTreeMap;

use dgxtop_core::event::ModuleHealthState;
use dgxtop_core::identity::AdapterId;
use dgxtop_core::observation::AdapterManifest;
use dgxtop_core::text::sanitize_display;
use dgxtop_core::view::{HostState, HostStatus};

/// Longest kept host detail text.
const MAX_DETAIL_BYTES: usize = 256;

#[derive(Debug)]
struct HostEntry {
    status: HostStatus,
    reported: bool,
}

/// Effect of one accepted status update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostUpdate {
    pub(crate) adapter_id: AdapterId,
    pub(crate) generation: u32,
    pub(crate) state: HostState,
    pub(crate) dispatchable: bool,
    pub(crate) state_changed: bool,
    pub(crate) detail: Option<String>,
}

/// Why a status update was ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostStatusRejection {
    /// Not one of the setup's adapters (the table stays bounded).
    UnknownAdapter,
    /// Older generation than the one already reported.
    OutOfOrder,
}

#[derive(Debug, Default)]
pub(crate) struct HostTable {
    hosts: BTreeMap<AdapterId, HostEntry>,
}

impl HostTable {
    pub(crate) fn new(manifests: &[AdapterManifest]) -> Self {
        let hosts = manifests
            .iter()
            .map(|manifest| {
                let status = HostStatus {
                    adapter_id: manifest.adapter_id.to_string(),
                    state: HostState::Starting,
                    generation: 0,
                    pid: None,
                    last_success_boot_ns: None,
                    timeouts_total: 0,
                    consecutive_failures: 0,
                    restarts_total: 0,
                    backoff_until_boot_ns: None,
                    detail: None,
                };
                (
                    manifest.adapter_id.clone(),
                    HostEntry {
                        status,
                        reported: false,
                    },
                )
            })
            .collect();
        Self { hosts }
    }

    pub(crate) fn apply(
        &mut self,
        mut status: HostStatus,
    ) -> Result<HostUpdate, HostStatusRejection> {
        let adapter_id = AdapterId::new(status.adapter_id.as_str())
            .map_err(|_| HostStatusRejection::UnknownAdapter)?;
        let entry = self
            .hosts
            .get_mut(&adapter_id)
            .ok_or(HostStatusRejection::UnknownAdapter)?;
        if entry.reported && status.generation < entry.status.generation {
            return Err(HostStatusRejection::OutOfOrder);
        }
        status.detail = status
            .detail
            .as_deref()
            .map(|detail| sanitize_display(detail, MAX_DETAIL_BYTES));
        let state_changed = !entry.reported
            || entry.status.state != status.state
            || entry.status.generation != status.generation;
        let update = HostUpdate {
            adapter_id,
            generation: status.generation,
            state: status.state,
            dispatchable: is_dispatchable(status.state),
            state_changed,
            detail: status.detail.clone(),
        };
        entry.status = status;
        entry.reported = true;
        Ok(update)
    }

    /// Current statuses in adapter id order.
    pub(crate) fn statuses(&self) -> Vec<HostStatus> {
        self.hosts
            .values()
            .map(|entry| entry.status.clone())
            .collect()
    }

    /// Pids of every managed adapter host (never valid action targets).
    pub(crate) fn protected_pids(&self) -> Vec<i32> {
        self.hosts
            .values()
            .filter_map(|entry| entry.status.pid)
            .collect()
    }
}

/// Whether the scheduler may send work to a host in this state.
pub(crate) const fn is_dispatchable(state: HostState) -> bool {
    matches!(
        state,
        HostState::Starting | HostState::Healthy | HostState::Degraded
    )
}

/// Event-level health of a host state.
pub(crate) const fn module_health(state: HostState) -> ModuleHealthState {
    match state {
        HostState::Starting | HostState::Healthy => ModuleHealthState::Healthy,
        HostState::Degraded | HostState::CircuitOpen => ModuleHealthState::Degraded,
        HostState::Quarantined => ModuleHealthState::Quarantined,
        HostState::Disabled | HostState::Unavailable => ModuleHealthState::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::manifest;

    fn status(adapter: &str, state: HostState, generation: u32, pid: Option<i32>) -> HostStatus {
        HostStatus {
            adapter_id: adapter.into(),
            state,
            generation,
            pid,
            last_success_boot_ns: None,
            timeouts_total: 0,
            consecutive_failures: 0,
            restarts_total: 0,
            backoff_until_boot_ns: None,
            detail: Some("ok\u{1b}[31m".into()),
        }
    }

    #[test]
    fn placeholders_until_reported_then_updates() {
        let mut table = HostTable::new(&[manifest("procfs", 100), manifest("nvml", 1000)]);
        assert_eq!(table.statuses().len(), 2);
        assert!(table.protected_pids().is_empty());
        let update = table
            .apply(status("nvml", HostState::Healthy, 1, Some(4242)))
            .unwrap();
        assert!(update.dispatchable);
        assert!(update.state_changed);
        assert_eq!(update.detail.as_deref(), Some("ok"));
        assert_eq!(table.protected_pids(), vec![4242]);
        let same = table
            .apply(status("nvml", HostState::Healthy, 1, Some(4242)))
            .unwrap();
        assert!(!same.state_changed);
    }

    #[test]
    fn unknown_adapters_and_old_generations_are_ignored() {
        let mut table = HostTable::new(&[manifest("procfs", 100)]);
        assert_eq!(
            table.apply(status("ghost", HostState::Healthy, 1, None)),
            Err(HostStatusRejection::UnknownAdapter)
        );
        table
            .apply(status("procfs", HostState::Healthy, 3, None))
            .unwrap();
        assert_eq!(
            table.apply(status("procfs", HostState::Healthy, 2, None)),
            Err(HostStatusRejection::OutOfOrder)
        );
        assert_eq!(table.statuses().len(), 1);
    }

    #[test]
    fn dispatchability_and_health_mapping() {
        assert!(is_dispatchable(HostState::Degraded));
        assert!(!is_dispatchable(HostState::CircuitOpen));
        assert!(!is_dispatchable(HostState::Quarantined));
        assert_eq!(
            module_health(HostState::Quarantined),
            ModuleHealthState::Quarantined
        );
        assert_eq!(
            module_health(HostState::Disabled),
            ModuleHealthState::Unavailable
        );
    }
}
