//! Per-device capability memory: a query that answered "not supported" or
//! "no permission" is not repeated every cycle; it is re-probed at most once
//! per probe interval, so one failure never blocks the hardware forever and a
//! permanently missing capability never costs a driver call per frame.

use std::collections::BTreeMap;

use dgxtop_core::clock::ms_to_ns;
use dgxtop_core::quality::Support;

use super::backend::{BackendError, BackendResult};

/// Default re-probe interval for unsupported/denied capabilities.
pub const DEFAULT_UNSUPPORTED_PROBE_MS: u64 = 60_000;

/// Whether a backend query should run now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Query,
    /// Cached capability failure; report it without calling the driver.
    Skip(BackendError),
}

#[derive(Debug, Clone)]
struct CapabilityState {
    support: Support,
    failure: Option<BackendError>,
    last_probe_boot_ns: u64,
}

/// Capability state keyed by `(device uuid, query key)`.
#[derive(Debug, Clone)]
pub struct CapabilityTracker {
    probe_interval_ns: u64,
    states: BTreeMap<(String, String), CapabilityState>,
}

impl CapabilityTracker {
    pub fn new(unsupported_probe_ms: u64) -> Self {
        Self {
            probe_interval_ns: ms_to_ns(unsupported_probe_ms),
            states: BTreeMap::new(),
        }
    }

    /// Decides whether to call the backend; a due re-probe is stamped here.
    pub fn admit(&mut self, uuid: &str, query_key: &str, now_boot_ns: u64) -> Admission {
        let Some(state) = self.states.get_mut(&key(uuid, query_key)) else {
            return Admission::Query;
        };
        let Some(failure) = &state.failure else {
            return Admission::Query;
        };
        if now_boot_ns.saturating_sub(state.last_probe_boot_ns) < self.probe_interval_ns {
            return Admission::Skip(failure.clone());
        }
        state.last_probe_boot_ns = now_boot_ns;
        Admission::Query
    }

    /// Learns from a query result. Transient faults are not cached.
    pub fn record<T>(
        &mut self,
        uuid: &str,
        query_key: &str,
        result: &BackendResult<T>,
        now_boot_ns: u64,
    ) {
        match result {
            Ok(_) => {
                self.states.insert(
                    key(uuid, query_key),
                    CapabilityState {
                        support: Support::Supported,
                        failure: None,
                        last_probe_boot_ns: now_boot_ns,
                    },
                );
            }
            Err(error) if error.is_capability_failure() => {
                self.states.insert(
                    key(uuid, query_key),
                    CapabilityState {
                        support: Support::Unsupported,
                        failure: Some(error.clone()),
                        last_probe_boot_ns: now_boot_ns,
                    },
                );
            }
            Err(_) => {}
        }
    }

    /// Current support and reason for one query of one device.
    pub fn support_of(&self, uuid: &str, query_key: &str) -> (Support, Option<String>) {
        match self.states.get(&key(uuid, query_key)) {
            Some(state) => (
                state.support,
                state.failure.as_ref().map(BackendError::reason_code),
            ),
            None => (Support::Unknown, None),
        }
    }

    /// Driver reload / generation change: everything is probed again.
    pub fn clear(&mut self) {
        self.states.clear();
    }
}

fn key(uuid: &str, query_key: &str) -> (String, String) {
    (uuid.to_owned(), query_key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_until_probed_and_clear_forgets() {
        let mut tracker = CapabilityTracker::new(60_000);
        assert_eq!(tracker.support_of("GPU-1", "x"), (Support::Unknown, None));
        tracker.record::<u32>("GPU-1", "x", &Err(BackendError::NotSupported), 0);
        assert_eq!(
            tracker.support_of("GPU-1", "x"),
            (Support::Unsupported, Some("NVML_NOT_SUPPORTED".to_owned()))
        );
        tracker.clear();
        assert_eq!(tracker.support_of("GPU-1", "x"), (Support::Unknown, None));
        assert_eq!(tracker.admit("GPU-1", "x", 1), Admission::Query);
    }
}
