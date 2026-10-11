//! Complete v2 configuration (`contracts/config.schema.json`) and its semantic
//! validation (`contracts/CONFIG.md`).
//!
//! Every field is required and unknown keys are rejected. The source of the
//! defaults is `spec/dgxtop-v2/examples/config.default.json`, embedded
//! verbatim: the REF workload's configuration ([`Config::reference`]). The
//! built-in defaults are that document with the owner's decisions applied
//! ([`Config::defaults`]: the history archive on, D-51; SIGKILL allowed,
//! D-71); the spec copy stays
//! byte-identical (D-02).

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::budget::{MIB, Partition, PartitionLimits};
use crate::identity::{MetricKey, SourceId};

/// The embedded default configuration document of the spec (the REF
/// workload's configuration).
pub const DEFAULT_CONFIG_JSON: &str =
    include_str!("../../../spec/dgxtop-v2/examples/config.default.json");

/// Current configuration schema version.
pub const CONFIG_SCHEMA_VERSION: u32 = 2;

/// Upper bound of `revision` (schema maximum); reaching it requires migration.
pub const MAX_CONFIG_REVISION: u32 = 2_147_483_647;

/// Command payload limit (also bounds a full `ReplaceConfig` candidate).
pub const MAX_COMMAND_BYTES: u64 = 64 * 1024;

/// Smallest acceptable effective per-command cap (fits a full config candidate).
pub const MIN_COMMAND_BYTES: u64 = 8 * 1024;

/// The ten built-in display modules.
pub const BUILTIN_MODULES: [&str; 10] = [
    "cpu",
    "gpu",
    "memory",
    "disk",
    "network",
    "processes",
    "history",
    "sources",
    "settings",
    "actions",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub revision: u32,
    pub sampling: SamplingConfig,
    pub quality: QualityConfig,
    pub store: StoreConfig,
    pub actions: ActionsConfig,
    pub viewer: ViewerConfig,
    pub queues: QueuesConfig,
    pub privacy: PrivacyConfig,
    pub runtime: RuntimeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingConfig {
    pub interval_ms: u32,
    pub discovery_interval_ms: u32,
    pub read_deadline_ms: u32,
    pub stale_factor: u32,
    pub max_adapters: u32,
    pub failure_threshold: u32,
    pub max_backoff_ms: u32,
    pub gpu_enabled: bool,
    pub capacity_interval_ms: u32,
    pub slow_health_interval_ms: u32,
    pub unsupported_probe_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityConfig {
    pub switch_successes: u32,
    pub switch_min_hold_ms: u32,
    pub temperature_disagreement_deg_c: f64,
    pub alignment_tolerance_ms: u32,
    pub allow_suspect_as_canonical: bool,
    pub excluded_sources: Vec<SourceId>,
    pub pinned_sources: Vec<PinnedSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedSource {
    pub key: MetricKey,
    pub source_id: SourceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    pub raw_candidate_seconds: u32,
    pub raw_history_seconds: u32,
    pub aggregate_seconds: u32,
    pub aggregate_retention_hours: u32,
    pub memory_limit_mib: u32,
    pub max_series: u32,
    pub max_sources: u32,
    pub max_candidates_per_series: u32,
    pub max_history_points: u32,
    pub archive_enabled: bool,
    /// Whether the Process History is kept on disk (D-80, a contract
    /// extension); unset follows `archive_enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_history_enabled: Option<bool>,
    pub archive_retention_days: u32,
    pub archive_limit_mib: u32,
    pub audit_retention_days: u32,
    pub audit_limit_mib: u32,
    pub max_processes: u32,
    pub max_process_gpu_links: u32,
    pub max_query_scan_records: u32,
}

impl StoreConfig {
    /// Whether the Process History is kept on disk: as set, else as the
    /// history archive (D-80).
    pub fn process_history_on_disk(&self) -> bool {
        self.process_history_enabled.unwrap_or(self.archive_enabled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionsConfig {
    pub enabled: bool,
    pub read_only: bool,
    pub same_uid_only: bool,
    pub allow_force: bool,
    pub confirmation_ttl_ms: u32,
    pub max_pending_confirmations: u32,
    pub deny_root_session: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Dark,
    Cyan,
    Green,
    Amber,
}

impl Theme {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Cyan => "cyan",
            Self::Green => "green",
            Self::Amber => "amber",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(Self::Dark),
            "cyan" => Some(Self::Cyan),
            "green" => Some(Self::Green),
            "amber" => Some(Self::Amber),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerConfig {
    pub fps: u32,
    pub max_sessions: u32,
    pub theme: Theme,
    pub enabled_modules: Vec<String>,
    pub network_max_visible: u32,
    pub redline_threshold_percent: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuesConfig {
    pub commands: u32,
    pub collection_batches: u32,
    pub max_batch_bytes: u32,
    pub ingress_bytes: u32,
    pub max_decoded_batch_bytes: u32,
    pub max_records_per_batch: u32,
    pub collector_jobs: u32,
    pub history_requests: u32,
    pub archive_bytes: u32,
    pub log_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacyConfig {
    pub persist_full_command_line: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub compute_workers: u32,
    pub history_workers: u32,
    pub control_slice_ms: u32,
    pub history_deadline_ms: u32,
    pub audit_intent_deadline_ms: u32,
    pub child_grace_ms: u32,
    pub child_term_ms: u32,
    pub child_reap_ms: u32,
    pub max_pending_requests: u32,
    pub result_cache_entries: u32,
    pub result_cache_ttl_ms: u32,
    pub max_projection_bytes: u32,
    pub max_history_result_bytes: u32,
    pub adaptive_enabled: bool,
    pub pressure_samples: u32,
    pub recovery_ms: u32,
}

/// One validation finding with its JSON path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

/// Configuration parse/validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("config JSON is invalid at line {line}, column {column}: {message}")]
    Parse {
        line: usize,
        column: usize,
        message: String,
    },
    #[error("config validation failed: {}", format_issues(.0))]
    Invalid(Vec<ValidationIssue>),
}

fn format_issues(issues: &[ValidationIssue]) -> String {
    issues
        .iter()
        .map(|issue| format!("{}: {}", issue.path, issue.message))
        .collect::<Vec<_>>()
        .join("; ")
}

impl Config {
    /// Parses and fully validates a configuration document.
    pub fn from_json(text: &str) -> Result<Self, ConfigError> {
        let config: Config = serde_json::from_str(text).map_err(|error| ConfigError::Parse {
            line: error.line(),
            column: error.column(),
            message: crate::text::sanitize_message(&error.to_string()),
        })?;
        config.validate().map_err(ConfigError::Invalid)?;
        Ok(config)
    }

    /// The spec's default document, the REF workload's configuration. The
    /// embedded document is validated by tests, so a failure here is a build
    /// defect, reported as a parse error.
    pub fn reference() -> Result<Self, ConfigError> {
        Self::from_json(DEFAULT_CONFIG_JSON)
    }

    /// The built-in defaults: the spec's document with the owner's
    /// decisions: the history archive on, so the history view keeps earlier
    /// runs and boots (D-51), and SIGKILL allowed, so F9 force-kills after
    /// its confirmation (D-71). REF runs use [`Config::reference`].
    pub fn defaults() -> Result<Self, ConfigError> {
        let mut config = Self::reference()?;
        config.store.archive_enabled = true;
        config.actions.allow_force = true;
        config.validate().map_err(ConfigError::Invalid)?;
        Ok(config)
    }

    /// Pretty JSON with a trailing newline (persisted form).
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text)
    }

    /// Schema ranges, safety constants and cross-field capacity rules.
    pub fn validate(&self) -> Result<(), Vec<ValidationIssue>> {
        let mut issues = Vec::new();
        let mut check = |ok: bool, path: &str, message: &str| {
            if !ok {
                issues.push(ValidationIssue {
                    path: path.to_owned(),
                    message: message.to_owned(),
                });
            }
        };
        let range = |value: u32, min: u32, max: u32| (min..=max).contains(&value);

        check(
            self.schema_version == CONFIG_SCHEMA_VERSION,
            "schema_version",
            "must be 2",
        );
        check(
            range(self.revision, 1, MAX_CONFIG_REVISION),
            "revision",
            "must be 1..=2147483647",
        );

        let sampling = &self.sampling;
        check(
            range(sampling.interval_ms, 100, 60_000),
            "sampling.interval_ms",
            "must be 100..=60000",
        );
        check(
            range(sampling.discovery_interval_ms, 1000, 300_000),
            "sampling.discovery_interval_ms",
            "must be 1000..=300000",
        );
        check(
            range(sampling.read_deadline_ms, 50, 10_000),
            "sampling.read_deadline_ms",
            "must be 50..=10000",
        );
        check(
            range(sampling.stale_factor, 2, 10),
            "sampling.stale_factor",
            "must be 2..=10",
        );
        check(
            range(sampling.max_adapters, 1, 8),
            "sampling.max_adapters",
            "must be 1..=8",
        );
        check(
            range(sampling.failure_threshold, 1, 10),
            "sampling.failure_threshold",
            "must be 1..=10",
        );
        check(
            range(sampling.max_backoff_ms, 1000, 300_000),
            "sampling.max_backoff_ms",
            "must be 1000..=300000",
        );
        check(
            range(sampling.capacity_interval_ms, 1000, 60_000),
            "sampling.capacity_interval_ms",
            "must be 1000..=60000",
        );
        check(
            range(sampling.slow_health_interval_ms, 1000, 60_000),
            "sampling.slow_health_interval_ms",
            "must be 1000..=60000",
        );
        check(
            range(sampling.unsupported_probe_ms, 10_000, 300_000),
            "sampling.unsupported_probe_ms",
            "must be 10000..=300000",
        );

        let quality = &self.quality;
        check(
            range(quality.switch_successes, 1, 10),
            "quality.switch_successes",
            "must be 1..=10",
        );
        check(
            quality.switch_min_hold_ms <= 60_000,
            "quality.switch_min_hold_ms",
            "must be 0..=60000",
        );
        check(
            quality.temperature_disagreement_deg_c.is_finite()
                && (0.1..=50.0).contains(&quality.temperature_disagreement_deg_c),
            "quality.temperature_disagreement_deg_c",
            "must be a finite number in 0.1..=50",
        );
        check(
            quality.alignment_tolerance_ms <= 5000,
            "quality.alignment_tolerance_ms",
            "must be 0..=5000",
        );
        check(
            !quality.allow_suspect_as_canonical,
            "quality.allow_suspect_as_canonical",
            "is a fixed safety constant (false)",
        );
        check(
            quality.excluded_sources.len() <= 2048,
            "quality.excluded_sources",
            "at most 2048 entries",
        );
        check(
            quality.pinned_sources.len() <= 512,
            "quality.pinned_sources",
            "at most 512 entries",
        );
        let mut pinned_keys = std::collections::BTreeSet::new();
        for (index, pin) in quality.pinned_sources.iter().enumerate() {
            check(
                pinned_keys.insert(&pin.key),
                &format!("quality.pinned_sources[{index}]"),
                "duplicate pin for the same MetricKey",
            );
            check(
                !quality.excluded_sources.contains(&pin.source_id),
                &format!("quality.pinned_sources[{index}].source_id"),
                "a source cannot be both pinned and excluded",
            );
        }
        let mut excluded = std::collections::BTreeSet::new();
        for (index, source) in quality.excluded_sources.iter().enumerate() {
            check(
                excluded.insert(source),
                &format!("quality.excluded_sources[{index}]"),
                "duplicate entry",
            );
        }

        let store = &self.store;
        check(
            range(store.raw_candidate_seconds, 1, 600),
            "store.raw_candidate_seconds",
            "must be 1..=600",
        );
        check(
            range(store.raw_history_seconds, 30, 3600),
            "store.raw_history_seconds",
            "must be 30..=3600",
        );
        check(
            store.aggregate_seconds == 60,
            "store.aggregate_seconds",
            "must be 60",
        );
        check(
            range(store.aggregate_retention_hours, 1, 24),
            "store.aggregate_retention_hours",
            "must be 1..=24",
        );
        check(
            range(store.memory_limit_mib, 64, 1024),
            "store.memory_limit_mib",
            "must be 64..=1024",
        );
        check(
            range(store.max_series, 1, 4096),
            "store.max_series",
            "must be 1..=4096",
        );
        check(
            range(store.max_sources, 1, 8192),
            "store.max_sources",
            "must be 1..=8192",
        );
        check(
            range(store.max_candidates_per_series, 1, 4),
            "store.max_candidates_per_series",
            "must be 1..=4",
        );
        check(
            range(store.max_history_points, 100, 2000),
            "store.max_history_points",
            "must be 100..=2000",
        );
        check(
            range(store.archive_retention_days, 1, 30),
            "store.archive_retention_days",
            "must be 1..=30",
        );
        check(
            range(store.archive_limit_mib, 128, 8192),
            "store.archive_limit_mib",
            "must be 128..=8192",
        );
        check(
            range(store.audit_retention_days, 1, 365),
            "store.audit_retention_days",
            "must be 1..=365",
        );
        check(
            range(store.audit_limit_mib, 16, 1024),
            "store.audit_limit_mib",
            "must be 16..=1024",
        );
        check(
            range(store.max_processes, 1, 4096),
            "store.max_processes",
            "must be 1..=4096",
        );
        check(
            range(store.max_process_gpu_links, 1, 16_384),
            "store.max_process_gpu_links",
            "must be 1..=16384",
        );
        check(
            range(store.max_query_scan_records, 1000, 200_000),
            "store.max_query_scan_records",
            "must be 1000..=200000",
        );

        let actions = &self.actions;
        check(
            actions.same_uid_only,
            "actions.same_uid_only",
            "is a fixed safety constant (true)",
        );
        check(
            actions.deny_root_session,
            "actions.deny_root_session",
            "is a fixed safety constant (true)",
        );
        check(
            range(actions.confirmation_ttl_ms, 1000, 30_000),
            "actions.confirmation_ttl_ms",
            "must be 1000..=30000",
        );
        check(
            range(actions.max_pending_confirmations, 1, 8),
            "actions.max_pending_confirmations",
            "must be 1..=8",
        );

        let viewer = &self.viewer;
        check(range(viewer.fps, 1, 20), "viewer.fps", "must be 1..=20");
        check(
            range(viewer.max_sessions, 1, 2),
            "viewer.max_sessions",
            "must be 1..=2",
        );
        check(
            viewer.enabled_modules.len() <= 64,
            "viewer.enabled_modules",
            "at most 64 entries",
        );
        let mut modules = std::collections::BTreeSet::new();
        for (index, module) in viewer.enabled_modules.iter().enumerate() {
            let path = format!("viewer.enabled_modules[{index}]");
            check(
                !module.is_empty() && module.len() <= 64,
                &path,
                "must be 1..=64 bytes",
            );
            check(modules.insert(module.as_str()), &path, "duplicate module");
        }
        check(
            range(viewer.network_max_visible, 1, 20),
            "viewer.network_max_visible",
            "must be 1..=20",
        );
        check(
            viewer.redline_threshold_percent.is_finite()
                && (0.0..=100.0).contains(&viewer.redline_threshold_percent),
            "viewer.redline_threshold_percent",
            "must be a finite number in 0..=100",
        );

        let queues = &self.queues;
        check(
            range(queues.commands, 8, 128),
            "queues.commands",
            "must be 8..=128",
        );
        check(
            range(queues.collection_batches, 1, 32),
            "queues.collection_batches",
            "must be 1..=32",
        );
        check(
            range(queues.max_batch_bytes, 4096, 1_048_576),
            "queues.max_batch_bytes",
            "must be 4096..=1048576",
        );
        check(
            range(queues.ingress_bytes, 1_048_576, 8_388_608),
            "queues.ingress_bytes",
            "must be 1048576..=8388608",
        );
        check(
            range(queues.max_decoded_batch_bytes, 65_536, 4_194_304),
            "queues.max_decoded_batch_bytes",
            "must be 65536..=4194304",
        );
        check(
            range(queues.max_records_per_batch, 1, 256),
            "queues.max_records_per_batch",
            "must be 1..=256",
        );
        check(
            range(queues.collector_jobs, 1, 8),
            "queues.collector_jobs",
            "must be 1..=8",
        );
        check(
            range(queues.history_requests, 1, 4),
            "queues.history_requests",
            "must be 1..=4",
        );
        check(
            range(queues.archive_bytes, 1_048_576, 8_388_608),
            "queues.archive_bytes",
            "must be 1048576..=8388608",
        );
        check(
            range(queues.log_bytes, 65_536, 1_048_576),
            "queues.log_bytes",
            "must be 65536..=1048576",
        );

        check(
            !self.privacy.persist_full_command_line,
            "privacy.persist_full_command_line",
            "is a fixed privacy constant (false)",
        );

        let runtime = &self.runtime;
        check(
            range(runtime.compute_workers, 1, 4),
            "runtime.compute_workers",
            "must be 1..=4",
        );
        check(
            range(runtime.history_workers, 1, 2),
            "runtime.history_workers",
            "must be 1..=2",
        );
        check(
            range(runtime.control_slice_ms, 1, 2),
            "runtime.control_slice_ms",
            "must be 1..=2",
        );
        check(
            range(runtime.history_deadline_ms, 50, 200),
            "runtime.history_deadline_ms",
            "must be 50..=200",
        );
        check(
            range(runtime.audit_intent_deadline_ms, 100, 1000),
            "runtime.audit_intent_deadline_ms",
            "must be 100..=1000",
        );
        check(
            range(runtime.child_grace_ms, 50, 200),
            "runtime.child_grace_ms",
            "must be 50..=200",
        );
        check(
            range(runtime.child_term_ms, 50, 200),
            "runtime.child_term_ms",
            "must be 50..=200",
        );
        check(
            range(runtime.child_reap_ms, 100, 1000),
            "runtime.child_reap_ms",
            "must be 100..=1000",
        );
        check(
            range(runtime.max_pending_requests, 8, 128),
            "runtime.max_pending_requests",
            "must be 8..=128",
        );
        check(
            range(runtime.result_cache_entries, 128, 4096),
            "runtime.result_cache_entries",
            "must be 128..=4096",
        );
        check(
            range(runtime.result_cache_ttl_ms, 1000, 600_000),
            "runtime.result_cache_ttl_ms",
            "must be 1000..=600000",
        );
        check(
            range(runtime.max_projection_bytes, 65_536, 1_048_576),
            "runtime.max_projection_bytes",
            "must be 65536..=1048576",
        );
        check(
            range(runtime.max_history_result_bytes, 65_536, 4_194_304),
            "runtime.max_history_result_bytes",
            "must be 65536..=4194304",
        );
        check(
            runtime.adaptive_enabled,
            "runtime.adaptive_enabled",
            "is a fixed constant (true)",
        );
        check(
            range(runtime.pressure_samples, 3, 10),
            "runtime.pressure_samples",
            "must be 3..=10",
        );
        check(
            range(runtime.recovery_ms, 30_000, 300_000),
            "runtime.recovery_ms",
            "must be 30000..=300000",
        );

        // Cross-field capacity rules (CONFIG.md): per-field validity is not enough.
        check(
            queues.commands <= runtime.max_pending_requests,
            "queues.commands",
            "must not exceed runtime.max_pending_requests",
        );
        check(
            queues.max_batch_bytes <= queues.max_decoded_batch_bytes,
            "queues.max_batch_bytes",
            "wire bytes must not exceed max_decoded_batch_bytes",
        );
        check(
            u64::from(queues.max_batch_bytes) + u64::from(queues.max_decoded_batch_bytes)
                <= u64::from(queues.ingress_bytes),
            "queues.ingress_bytes",
            "must hold one maximum wire + decoded batch",
        );
        check(
            queues.history_requests >= runtime.history_workers,
            "queues.history_requests",
            "must be at least runtime.history_workers",
        );
        let limits = PartitionLimits::for_memory_limit_mib(u64::from(store.memory_limit_mib));
        check(
            self.effective_command_bytes() >= MIN_COMMAND_BYTES,
            "queues",
            "commands x per-command cap + archive + log + control bookkeeping must fit the reserve partition with at least 8 KiB per command",
        );
        check(
            u64::from(queues.ingress_bytes) <= limits.limit(Partition::Ingress),
            "queues.ingress_bytes",
            "must fit the ingress partition (memory_limit_mib x 8/256)",
        );
        let presentation_need =
            u64::from(viewer.max_sessions) * 2 * u64::from(runtime.max_projection_bytes);
        check(
            presentation_need <= limits.limit(Partition::Presentation),
            "runtime.max_projection_bytes",
            "max_sessions x (pending + active) projections must fit the presentation partition",
        );
        let history_need =
            u64::from(runtime.history_workers) * u64::from(runtime.max_history_result_bytes);
        check(
            history_need <= limits.limit(Partition::HistoryQuery),
            "runtime.max_history_result_bytes",
            "running history results must fit the history_query partition",
        );

        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    /// Dotted paths of fields that differ and can only take effect after a restart.
    pub fn restart_required_changes(&self, candidate: &Config) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.queues != candidate.queues {
            changed.push("queues");
        }
        if self.runtime != candidate.runtime {
            changed.push("runtime");
        }
        if self.sampling.max_adapters != candidate.sampling.max_adapters {
            changed.push("sampling.max_adapters");
        }
        let store = (&self.store, &candidate.store);
        if store.0.memory_limit_mib != store.1.memory_limit_mib {
            changed.push("store.memory_limit_mib");
        }
        if store.0.max_series != store.1.max_series {
            changed.push("store.max_series");
        }
        if store.0.max_sources != store.1.max_sources {
            changed.push("store.max_sources");
        }
        if store.0.max_processes != store.1.max_processes {
            changed.push("store.max_processes");
        }
        if store.0.max_process_gpu_links != store.1.max_process_gpu_links {
            changed.push("store.max_process_gpu_links");
        }
        if store.0.archive_enabled != store.1.archive_enabled {
            changed.push("store.archive_enabled");
        }
        if store.0.process_history_on_disk() != store.1.process_history_on_disk() {
            changed.push("store.process_history_enabled");
        }
        changed
    }

    /// Whether scheduling-relevant fields differ (requires a new scheduler epoch).
    pub fn scheduling_changed(&self, candidate: &Config) -> bool {
        self.sampling != candidate.sampling
    }

    /// Whether arbitration policy fields differ (requires a new policy revision).
    pub fn policy_changed(&self, candidate: &Config) -> bool {
        self.quality != candidate.quality
    }

    /// Whether action policy changed in a way that must revoke pending tokens.
    pub fn action_policy_changed(&self, candidate: &Config) -> bool {
        self.actions != candidate.actions
    }

    /// Effective per-command payload cap: 64 KiB, reduced (never exceeded) when
    /// the reserve partition cannot hold `commands x 64 KiB` next to the archive
    /// journal, the log buffer and control bookkeeping.
    pub fn effective_command_bytes(&self) -> u64 {
        let limits = self.partition_limits();
        let reserve = limits.limit(Partition::Reserve);
        let bookkeeping = 3 * MIB * u64::from(self.store.memory_limit_mib) / 256;
        let fixed =
            u64::from(self.queues.archive_bytes) + u64::from(self.queues.log_bytes) + bookkeeping;
        let commands = u64::from(self.queues.commands.max(1));
        let available = reserve.saturating_sub(fixed) / commands;
        available.min(MAX_COMMAND_BYTES)
    }

    /// Byte limits of the global ledger for this configuration.
    pub fn partition_limits(&self) -> PartitionLimits {
        PartitionLimits::for_memory_limit_mib(u64::from(self.store.memory_limit_mib))
    }
}

/// Field-level provenance shown by the settings view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueOrigin {
    /// Built-in default (`examples/config.default.json`).
    Default,
    /// Persisted configuration file.
    Saved,
    /// Explicit command-line override for this run only (never persisted).
    CliOverride,
}

/// Explicit command-line overrides. `None` means "not given": it must never
/// overwrite a persisted value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CliOverrides {
    pub interval_ms: Option<u32>,
    pub gpu_enabled: Option<bool>,
    pub theme: Option<Theme>,
    pub network_max_visible: Option<u32>,
    pub read_only: Option<bool>,
}

impl CliOverrides {
    /// Applies only explicitly provided fields; returns the overridden paths.
    pub fn apply(&self, config: &mut Config) -> Vec<&'static str> {
        let mut applied = Vec::new();
        if let Some(interval) = self.interval_ms {
            config.sampling.interval_ms = interval;
            applied.push("sampling.interval_ms");
        }
        if let Some(gpu_enabled) = self.gpu_enabled {
            config.sampling.gpu_enabled = gpu_enabled;
            applied.push("sampling.gpu_enabled");
        }
        if let Some(theme) = self.theme {
            config.viewer.theme = theme;
            applied.push("viewer.theme");
        }
        if let Some(visible) = self.network_max_visible {
            config.viewer.network_max_visible = visible;
            applied.push("viewer.network_max_visible");
        }
        if let Some(read_only) = self.read_only {
            config.actions.read_only = read_only;
            applied.push("actions.read_only");
        }
        applied
    }

    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Legacy v1 configuration (`~/.config/dgxtop/config.json` before the rewrite).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyConfig {
    pub update_interval_secs: Option<f64>,
    pub color_theme: Option<String>,
    pub gpu_enabled: Option<bool>,
    pub redline_threshold: Option<f64>,
    pub history_length: Option<u64>,
    pub network_max_visible: Option<u64>,
    #[serde(flatten)]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

/// One line of the legacy migration report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyMapping {
    pub legacy_field: String,
    pub legacy_value: String,
    pub target_field: Option<String>,
    pub target_value: Option<String>,
    pub status: LegacyMappingStatus,
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyMappingStatus {
    Mapped,
    MappedWithWarning,
    Rejected,
    Unsupported,
    Absent,
}

/// Complete dry-run report; `candidate` is only written on explicit `--apply`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportReport {
    pub mappings: Vec<LegacyMapping>,
    pub candidate: Config,
    pub warnings: Vec<String>,
}

/// Converts an already rounded, finite, non-negative `f64` to `u32` exactly.
fn whole_f64_to_u32(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 || value > f64::from(u32::MAX) || value.fract() != 0.0 {
        return None;
    }
    // Range and integrality were checked above, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(value as u32)
}

/// Maps a legacy document onto a copy of `base` (normally the defaults).
pub fn map_legacy_config(legacy: &LegacyConfig, base: &Config) -> LegacyImportReport {
    let mut candidate = base.clone();
    let mut mappings = Vec::new();
    let mut warnings = Vec::new();
    let mut push = |legacy_field: &str,
                    legacy_value: String,
                    target_field: Option<&str>,
                    target_value: Option<String>,
                    status: LegacyMappingStatus,
                    note: &str| {
        mappings.push(LegacyMapping {
            legacy_field: legacy_field.to_owned(),
            legacy_value,
            target_field: target_field.map(str::to_owned),
            target_value,
            status,
            note: note.to_owned(),
        });
    };

    let interval_secs = legacy.update_interval_secs;
    match interval_secs {
        Some(seconds) if seconds.is_finite() => {
            let rounded = (seconds * 1000.0).round();
            if (100.0..=60_000.0).contains(&rounded) {
                let milliseconds = whole_f64_to_u32(rounded).unwrap_or(1000);
                candidate.sampling.interval_ms = milliseconds;
                push(
                    "update_interval_secs",
                    seconds.to_string(),
                    Some("sampling.interval_ms"),
                    Some(milliseconds.to_string()),
                    LegacyMappingStatus::Mapped,
                    "rounded to whole milliseconds",
                );
            } else {
                push(
                    "update_interval_secs",
                    seconds.to_string(),
                    Some("sampling.interval_ms"),
                    None,
                    LegacyMappingStatus::Rejected,
                    "outside 100..=60000 ms after rounding; default kept",
                );
            }
        }
        Some(seconds) => push(
            "update_interval_secs",
            seconds.to_string(),
            Some("sampling.interval_ms"),
            None,
            LegacyMappingStatus::Rejected,
            "not a finite number",
        ),
        None => push(
            "update_interval_secs",
            "absent".into(),
            Some("sampling.interval_ms"),
            None,
            LegacyMappingStatus::Absent,
            "default kept",
        ),
    }

    match legacy.color_theme.as_deref() {
        Some(name) => match Theme::parse(name) {
            Some(theme) => {
                candidate.viewer.theme = theme;
                push(
                    "color_theme",
                    name.to_owned(),
                    Some("viewer.theme"),
                    Some(theme.name().to_owned()),
                    LegacyMappingStatus::Mapped,
                    "same theme name",
                );
            }
            None => push(
                "color_theme",
                crate::text::sanitize_display(name, 64),
                Some("viewer.theme"),
                None,
                LegacyMappingStatus::Rejected,
                "unknown theme; default kept",
            ),
        },
        None => push(
            "color_theme",
            "absent".into(),
            Some("viewer.theme"),
            None,
            LegacyMappingStatus::Absent,
            "default kept",
        ),
    }

    match legacy.gpu_enabled {
        Some(enabled) => {
            candidate.sampling.gpu_enabled = enabled;
            push(
                "gpu_enabled",
                enabled.to_string(),
                Some("sampling.gpu_enabled"),
                Some(enabled.to_string()),
                LegacyMappingStatus::Mapped,
                "selects whether the NVML adapter host starts",
            );
        }
        None => push(
            "gpu_enabled",
            "absent".into(),
            Some("sampling.gpu_enabled"),
            None,
            LegacyMappingStatus::Absent,
            "default kept",
        ),
    }

    match legacy.history_length {
        Some(points) => {
            let interval = interval_secs
                .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                .unwrap_or(1.0);
            let seconds = (points as f64 * interval).round();
            let clamped = seconds.clamp(30.0, 3600.0);
            let value = whole_f64_to_u32(clamped).unwrap_or(300);
            candidate.store.raw_history_seconds = value;
            warnings.push("history_length counted samples; v2 retains raw history by time, so the result is not equivalent".to_owned());
            push(
                "history_length",
                points.to_string(),
                Some("store.raw_history_seconds"),
                Some(value.to_string()),
                LegacyMappingStatus::MappedWithWarning,
                "converted with the legacy interval; not equivalent (time based, memory capped)",
            );
        }
        None => push(
            "history_length",
            "absent".into(),
            Some("store.raw_history_seconds"),
            None,
            LegacyMappingStatus::Absent,
            "default kept",
        ),
    }

    match legacy.network_max_visible {
        Some(visible) if (1..=20).contains(&visible) => {
            let value = u32::try_from(visible).unwrap_or(3);
            candidate.viewer.network_max_visible = value;
            push(
                "network_max_visible",
                visible.to_string(),
                Some("viewer.network_max_visible"),
                Some(value.to_string()),
                LegacyMappingStatus::Mapped,
                "layout only; not a collection cap",
            );
        }
        Some(visible) => push(
            "network_max_visible",
            visible.to_string(),
            Some("viewer.network_max_visible"),
            None,
            LegacyMappingStatus::Rejected,
            "outside 1..=20; default kept",
        ),
        None => push(
            "network_max_visible",
            "absent".into(),
            Some("viewer.network_max_visible"),
            None,
            LegacyMappingStatus::Absent,
            "default kept",
        ),
    }

    match legacy.redline_threshold {
        Some(threshold) if threshold.is_finite() && (0.0..=100.0).contains(&threshold) => {
            candidate.viewer.redline_threshold_percent = threshold;
            push(
                "redline_threshold",
                threshold.to_string(),
                Some("viewer.redline_threshold_percent"),
                Some(threshold.to_string()),
                LegacyMappingStatus::Mapped,
                "display warning only; not a data validity range",
            );
        }
        Some(threshold) => push(
            "redline_threshold",
            threshold.to_string(),
            Some("viewer.redline_threshold_percent"),
            None,
            LegacyMappingStatus::Rejected,
            "outside 0..=100; default kept",
        ),
        None => push(
            "redline_threshold",
            "absent".into(),
            Some("viewer.redline_threshold_percent"),
            None,
            LegacyMappingStatus::Absent,
            "default kept",
        ),
    }

    for (key, value) in &legacy.unknown {
        push(
            &crate::text::sanitize_display(key, 64),
            crate::text::sanitize_display(&value.to_string(), 128),
            None,
            None,
            LegacyMappingStatus::Unsupported,
            "no v2 equivalent; not imported",
        );
    }

    LegacyImportReport {
        mappings,
        candidate,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Config {
        Config::defaults().unwrap()
    }

    #[test]
    fn query_and_worker_defaults_match_the_runtime_limits() {
        // AT38 / RUNTIME.md §5: <= 2000 points and 4 MiB per result, 2
        // history workers; at most 2 compute workers by default.
        let defaults = Config::defaults().unwrap();
        assert_eq!(defaults.runtime.max_history_result_bytes, 4 * 1024 * 1024);
        assert_eq!(defaults.runtime.history_workers, 2);
        assert_eq!(defaults.runtime.compute_workers, 2);
    }

    #[test]
    fn defaults_round_trip_json() {
        let config = defaults();
        let text = config.to_pretty_json().unwrap();
        assert_eq!(Config::from_json(&text).unwrap(), config);
        let reference = Config::reference().unwrap();
        let original: serde_json::Value = serde_json::from_str(DEFAULT_CONFIG_JSON).unwrap();
        let reencoded: serde_json::Value =
            serde_json::from_str(&reference.to_pretty_json().unwrap()).unwrap();
        assert_eq!(original, reencoded);
    }

    #[test]
    fn the_built_in_defaults_keep_history_on_disk_and_the_reference_stays_the_spec() {
        // D-51: the owner wants the history kept on disk; the spec document
        // (and so the REF workload) keeps its archive off.
        let reference = Config::reference().unwrap();
        assert!(!reference.store.archive_enabled);
        let defaults = defaults();
        assert!(defaults.store.archive_enabled);
        // D-71: F9 force-kills by default; the spec document keeps it off.
        assert!(!reference.actions.allow_force);
        assert!(defaults.actions.allow_force);
        // D-80: the Process History follows the archive unless set.
        assert_eq!(defaults.store.process_history_enabled, None);
        assert!(defaults.store.process_history_on_disk());
        let mut only_the_owners = defaults.clone();
        only_the_owners.store.archive_enabled = false;
        only_the_owners.actions.allow_force = false;
        assert_eq!(only_the_owners, reference, "nothing else differs");
        assert!(defaults.validate().is_ok());
    }

    #[test]
    fn unknown_keys_and_missing_fields_are_rejected() {
        let mut value: serde_json::Value = serde_json::from_str(DEFAULT_CONFIG_JSON).unwrap();
        value["sampling"]["unexpected"] = serde_json::json!(1);
        assert!(matches!(
            Config::from_json(&value.to_string()),
            Err(ConfigError::Parse { .. })
        ));
        let mut value: serde_json::Value = serde_json::from_str(DEFAULT_CONFIG_JSON).unwrap();
        value["sampling"]
            .as_object_mut()
            .unwrap()
            .remove("interval_ms");
        assert!(matches!(
            Config::from_json(&value.to_string()),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn safety_constants_cannot_be_disabled() {
        let mut config = defaults();
        config.actions.same_uid_only = false;
        config.actions.deny_root_session = false;
        config.privacy.persist_full_command_line = true;
        config.quality.allow_suspect_as_canonical = true;
        let issues = config.validate().unwrap_err();
        let paths: Vec<_> = issues.iter().map(|issue| issue.path.as_str()).collect();
        assert!(paths.contains(&"actions.same_uid_only"));
        assert!(paths.contains(&"actions.deny_root_session"));
        assert!(paths.contains(&"privacy.persist_full_command_line"));
        assert!(paths.contains(&"quality.allow_suspect_as_canonical"));
    }

    #[test]
    fn non_finite_and_out_of_range_numbers_are_rejected() {
        let mut config = defaults();
        config.quality.temperature_disagreement_deg_c = f64::NAN;
        config.sampling.interval_ms = 99;
        config.viewer.fps = 21;
        let issues = config.validate().unwrap_err();
        assert!(issues.len() >= 3);
    }

    #[test]
    fn cross_field_capacity_rules() {
        let mut config = defaults();
        config.queues.max_batch_bytes = 1_048_576;
        config.queues.max_decoded_batch_bytes = 4_194_304;
        config.queues.ingress_bytes = 4_194_304;
        let issues = config.validate().unwrap_err();
        assert!(
            issues
                .iter()
                .any(|issue| issue.path == "queues.ingress_bytes")
        );

        let mut config = defaults();
        config.queues.commands = 128;
        config.runtime.max_pending_requests = 64;
        assert!(
            config
                .validate()
                .unwrap_err()
                .iter()
                .any(|issue| issue.path == "queues.commands")
        );

        let mut config = defaults();
        config.store.memory_limit_mib = 64;
        // 8 MiB archive journal alone exceeds a 5 MiB reserve; the 8 MiB
        // ingress cap exceeds the 2 MiB ingress partition.
        let issues = config.validate().unwrap_err();
        assert!(issues.iter().any(|issue| issue.path == "queues"));
        assert!(
            issues
                .iter()
                .any(|issue| issue.path == "queues.ingress_bytes")
        );
        assert_eq!(defaults().effective_command_bytes(), MAX_COMMAND_BYTES);
    }

    #[test]
    fn pins_must_not_duplicate_or_overlap_exclusions() {
        let mut config = defaults();
        let key: MetricKey = serde_json::from_value(serde_json::json!({
            "host_id":"h","boot_id":"b","entity_id":"cpu.package.0",
            "metric_id":"cpu.temperature.package","scope":"package:0"
        }))
        .unwrap();
        let source = SourceId::new("hwmon:coretemp:temp1").unwrap();
        config.quality.pinned_sources.push(PinnedSource {
            key: key.clone(),
            source_id: source.clone(),
        });
        config.quality.pinned_sources.push(PinnedSource {
            key,
            source_id: source.clone(),
        });
        config.quality.excluded_sources.push(source);
        let issues = config.validate().unwrap_err();
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("duplicate pin"))
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("pinned and excluded"))
        );
    }

    #[test]
    fn restart_only_fields_are_detected() {
        let base = defaults();
        let mut candidate = base.clone();
        candidate.sampling.interval_ms = 2000;
        assert!(base.restart_required_changes(&candidate).is_empty());
        assert!(base.scheduling_changed(&candidate));
        candidate.queues.commands = 64;
        candidate.store.max_series = 100;
        let changed = base.restart_required_changes(&candidate);
        assert!(changed.contains(&"queues"));
        assert!(changed.contains(&"store.max_series"));
    }

    /// The Process History's own switch (D-80): unset follows the archive,
    /// set wins, and a change that flips it needs a restart.
    #[test]
    fn the_process_history_follows_the_archive_unless_set() {
        let mut store = defaults().store;
        store.archive_enabled = false;
        assert!(!store.process_history_on_disk());
        store.process_history_enabled = Some(true);
        assert!(store.process_history_on_disk());
        store.archive_enabled = true;
        store.process_history_enabled = Some(false);
        assert!(!store.process_history_on_disk());
        let base = defaults();
        let mut candidate = base.clone();
        candidate.store.process_history_enabled = Some(true);
        assert!(
            base.restart_required_changes(&candidate).is_empty(),
            "already on with the archive"
        );
        candidate.store.process_history_enabled = Some(false);
        assert!(
            base.restart_required_changes(&candidate)
                .contains(&"store.process_history_enabled")
        );
        // Unset, it is not written: the spec's document stays valid.
        let json = serde_json::to_value(&base).unwrap();
        assert!(json["store"].get("process_history_enabled").is_none());
        let mut set = json.clone();
        set["store"]["process_history_enabled"] = serde_json::json!(false);
        let parsed: Config = serde_json::from_value(set).unwrap();
        assert_eq!(parsed.store.process_history_enabled, Some(false));
    }

    #[test]
    fn cli_overrides_only_touch_explicit_fields() {
        let mut config = defaults();
        config.sampling.interval_ms = 5000;
        let overrides = CliOverrides {
            gpu_enabled: Some(false),
            ..CliOverrides::default()
        };
        let applied = overrides.apply(&mut config);
        assert_eq!(applied, vec!["sampling.gpu_enabled"]);
        assert_eq!(
            config.sampling.interval_ms, 5000,
            "absent --interval must not overwrite"
        );
        assert!(!config.sampling.gpu_enabled);
        assert!(CliOverrides::default().is_empty());
    }

    #[test]
    fn legacy_mapping_reports_every_field() {
        let legacy: LegacyConfig = serde_json::from_value(serde_json::json!({
            "update_interval_secs": 0.25,
            "color_theme": "amber",
            "gpu_enabled": false,
            "redline_threshold": 90.0,
            "history_length": 600,
            "network_max_visible": 5,
            "mystery": true
        }))
        .unwrap();
        let report = map_legacy_config(&legacy, &defaults());
        assert_eq!(report.candidate.sampling.interval_ms, 250);
        assert_eq!(report.candidate.viewer.theme, Theme::Amber);
        assert!(!report.candidate.sampling.gpu_enabled);
        assert_eq!(report.candidate.store.raw_history_seconds, 150);
        assert_eq!(report.candidate.viewer.network_max_visible, 5);
        assert_eq!(report.mappings.len(), 7);
        assert!(
            report
                .mappings
                .iter()
                .any(|mapping| mapping.status == LegacyMappingStatus::Unsupported)
        );
        assert!(report.candidate.validate().is_ok());
    }

    #[test]
    fn legacy_out_of_range_values_keep_defaults() {
        let legacy: LegacyConfig = serde_json::from_value(serde_json::json!({
            "update_interval_secs": 120.0,
            "color_theme": "neon",
            "network_max_visible": 99
        }))
        .unwrap();
        let report = map_legacy_config(&legacy, &defaults());
        assert_eq!(report.candidate.sampling.interval_ms, 1000);
        assert_eq!(report.candidate.viewer.theme, Theme::Dark);
        assert_eq!(report.candidate.viewer.network_max_visible, 3);
        assert_eq!(
            report
                .mappings
                .iter()
                .filter(|mapping| mapping.status == LegacyMappingStatus::Rejected)
                .count(),
            3
        );
    }
}
