//! Supervisor limits and timings.

use dgxtop_core::config::Config;
use dgxtop_core::errors::InfraError;

use crate::ipc::IpcLimits;

/// Hard cap on live + quarantined hosts (`RUNTIME.md` §1).
pub const MAX_HOSTS: usize = 8;

/// Timing and capacity knobs of the adapter-host supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// Live + quarantined hosts (≤ 8).
    pub max_hosts: usize,
    /// Wait after the protocol shutdown request before SIGTERM.
    pub child_grace_ms: u64,
    /// Wait after SIGTERM before SIGKILL.
    pub child_term_ms: u64,
    /// Wait after SIGKILL for the reap before quarantining.
    pub child_reap_ms: u64,
    /// Consecutive failures that open the circuit.
    pub failure_threshold: u32,
    /// First circuit backoff (1 s in production).
    pub backoff_base_ms: u64,
    pub max_backoff_ms: u64,
    /// ±jitter in per mille (100 = ±10 %).
    pub backoff_jitter_permille: u64,
    /// Time a new incarnation has to send its hello.
    pub hello_timeout_ms: u64,
    /// Deadline of the half-open discover probe.
    pub probe_deadline_ms: u64,
    pub max_frame_bytes: usize,
    pub max_decoded_bytes: usize,
    pub max_records: usize,
    /// De-duplication table entries (in flight + recycle area).
    pub broker_capacity: usize,
    /// Requesters sharing one in-flight read.
    pub broker_max_requesters: usize,
    /// Completions buffered while the ingestion channel is full.
    pub outbox_capacity: usize,
    /// Commands queued from handles to the supervisor thread.
    pub command_capacity: usize,
    /// Child stderr lines forwarded per window and host.
    pub stderr_lines_per_window: u32,
    pub stderr_window_ms: u64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_hosts: MAX_HOSTS,
            child_grace_ms: 200,
            child_term_ms: 200,
            child_reap_ms: 1000,
            failure_threshold: 3,
            backoff_base_ms: 1000,
            max_backoff_ms: 60_000,
            backoff_jitter_permille: 100,
            hello_timeout_ms: 2000,
            probe_deadline_ms: 2000,
            max_frame_bytes: crate::ipc::MAX_WIRE_FRAME_BYTES,
            max_decoded_bytes: crate::ipc::MAX_DECODED_BYTES,
            max_records: dgxtop_core::observation::MAX_RECORDS_PER_BATCH,
            broker_capacity: 64,
            broker_max_requesters: 8,
            outbox_capacity: 64,
            command_capacity: 64,
            stderr_lines_per_window: 20,
            stderr_window_ms: 10_000,
        }
    }
}

fn widen(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

impl SupervisorConfig {
    /// Values from the validated application configuration.
    pub fn from_config(config: &Config) -> Self {
        let defaults = Self::default();
        Self {
            max_hosts: widen(config.sampling.max_adapters).min(MAX_HOSTS),
            child_grace_ms: u64::from(config.runtime.child_grace_ms),
            child_term_ms: u64::from(config.runtime.child_term_ms),
            child_reap_ms: u64::from(config.runtime.child_reap_ms),
            failure_threshold: config.sampling.failure_threshold,
            max_backoff_ms: u64::from(config.sampling.max_backoff_ms),
            probe_deadline_ms: u64::from(config.sampling.read_deadline_ms)
                .max(defaults.probe_deadline_ms),
            max_frame_bytes: widen(config.queues.max_batch_bytes),
            max_decoded_bytes: widen(config.queues.max_decoded_batch_bytes),
            max_records: widen(config.queues.max_records_per_batch),
            ..defaults
        }
    }

    pub fn validate(&self) -> Result<(), InfraError> {
        let fail = |message: &str| {
            Err(InfraError::Unavailable(format!(
                "supervisor config: {message}"
            )))
        };
        if !(1..=MAX_HOSTS).contains(&self.max_hosts) {
            return fail("max_hosts must be 1..=8");
        }
        if self.failure_threshold == 0 {
            return fail("failure_threshold must be at least 1");
        }
        if self.backoff_base_ms == 0 || self.max_backoff_ms < self.backoff_base_ms {
            return fail("backoff must satisfy 1 <= base <= max");
        }
        if self.backoff_jitter_permille > 1000 {
            return fail("backoff_jitter_permille must be <= 1000");
        }
        let timings = [
            self.child_grace_ms,
            self.child_term_ms,
            self.child_reap_ms,
            self.hello_timeout_ms,
            self.probe_deadline_ms,
            self.stderr_window_ms,
        ];
        if timings.contains(&0) {
            return fail("timings must be positive");
        }
        let capacities = [
            self.broker_capacity,
            self.broker_max_requesters,
            self.outbox_capacity,
            self.command_capacity,
        ];
        if capacities.contains(&0) || self.stderr_lines_per_window == 0 {
            return fail("capacities must be positive");
        }
        self.ipc_limits().map(|_| ())
    }

    pub fn ipc_limits(&self) -> Result<IpcLimits, InfraError> {
        IpcLimits::new(
            self.max_frame_bytes,
            self.max_decoded_bytes,
            self.max_records,
        )
        .map_err(InfraError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_spec_and_validate() {
        let config = SupervisorConfig::default();
        assert_eq!(config.max_hosts, 8);
        assert_eq!(
            (
                config.child_grace_ms,
                config.child_term_ms,
                config.child_reap_ms
            ),
            (200, 200, 1000)
        );
        assert_eq!(config.failure_threshold, 3);
        assert_eq!(config.max_backoff_ms, 60_000);
        assert_eq!(config.max_frame_bytes, 1 << 20);
        assert_eq!(config.max_decoded_bytes, 4 << 20);
        assert_eq!(config.max_records, 256);
        config.validate().unwrap();
    }

    #[test]
    fn invalid_values_are_rejected() {
        let invalid = [
            SupervisorConfig {
                max_hosts: 9,
                ..SupervisorConfig::default()
            },
            SupervisorConfig {
                max_hosts: 0,
                ..SupervisorConfig::default()
            },
            SupervisorConfig {
                failure_threshold: 0,
                ..SupervisorConfig::default()
            },
            SupervisorConfig {
                backoff_base_ms: 2000,
                max_backoff_ms: 1000,
                ..SupervisorConfig::default()
            },
            SupervisorConfig {
                max_frame_bytes: 2 << 20,
                ..SupervisorConfig::default()
            },
            SupervisorConfig {
                hello_timeout_ms: 0,
                ..SupervisorConfig::default()
            },
            SupervisorConfig {
                outbox_capacity: 0,
                ..SupervisorConfig::default()
            },
        ];
        for config in invalid {
            assert!(config.validate().is_err(), "{config:?}");
        }
    }
}
