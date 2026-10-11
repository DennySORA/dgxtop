//! ConfigurationService: CAS-protected configuration changes (CONFIG.md,
//! AT08).
//!
//! Flow: validate → stage candidate → `ConfigJob::Persist` → receipt →
//! infallible swap of saved + effective → caller applies the new epoch /
//! policy revision / token revocation → `ConfigApplied` → result. Nothing
//! changes in memory before the durable receipt:
//! * `PersistError::Io`/`Invalid`/`Conflict` keep the old configuration;
//! * `PersistError::Uncertain` adopts the on-disk configuration reported by the
//!   repository (or keeps the old one when there is none) and answers
//!   `PersistFailed` with `commit_uncertain`;
//!
//! The commands change the sampling interval or one module switch, never a
//! restart-only field; `ReplaceConfig` is refused before it reaches this
//! service (D-91).
//!
//! CLI overrides (`ValueOrigin::CliOverride`) are re-applied to the effective
//! configuration after every swap and are never persisted. A path the command
//! explicitly changed supersedes its CLI override for the rest of the run —
//! except a `--read-only` override, which no UI command can lift.

use std::collections::{BTreeMap, BTreeSet};

use dgxtop_core::action::{PersistenceKind, PersistenceState, PersistenceStatus};
use dgxtop_core::canonical::CatalogSnapshot;
use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::commands::CommandResult;
use dgxtop_core::config::{BUILTIN_MODULES, Config, ValidationIssue, ValueOrigin, is_known_module};
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::identity::ModuleId;
use dgxtop_core::ports::{ConfigJob, Dispatch, DispatchError, PersistError};
use dgxtop_core::text::sanitize_message;
use serde_json::Value;
use uuid::Uuid;

/// The config worker must answer within this time; afterwards the outcome
/// is reported as unknown (the late receipt is still adopted).
const PERSIST_TIMEOUT_NS: u64 = 10 * NANOS_PER_SECOND;
/// Validation issues quoted in one result message.
const MAX_ISSUES_IN_MESSAGE: usize = 8;
/// CLI override that UI commands may never supersede (when it is `true`).
const READ_ONLY_PATH: &str = "actions.read_only";

/// A configuration command after syntax validation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConfigRequest {
    SetInterval {
        interval_ms: u32,
        expected_revision: u32,
    },
    SetModule {
        module_id: ModuleId,
        enabled: bool,
        expected_revision: u32,
    },
}

impl ConfigRequest {
    fn expected_revision(&self) -> u32 {
        match self {
            Self::SetInterval {
                expected_revision, ..
            }
            | Self::SetModule {
                expected_revision, ..
            } => *expected_revision,
        }
    }
}

/// Immediate answer or "wait for the persist receipt".
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConfigDecision {
    Reply(CommandResult),
    Pending,
}

/// A configuration that became durable and is now saved + effective.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppliedChange {
    pub(crate) request_id: Uuid,
    /// Effective configuration before the swap.
    pub(crate) previous: Config,
    pub(crate) revision: u32,
    /// Set when the on-disk configuration was adopted after an uncertain commit.
    pub(crate) uncertain: Option<String>,
    /// False when the command was already answered (persist timeout).
    pub(crate) reply_pending: bool,
}

/// Outcome of a persist completion.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PersistOutcome {
    Applied(Box<AppliedChange>),
    /// Old configuration kept; this is the command's result.
    Failed(Uuid, CommandResult),
    /// Failure of a persist whose command was already answered.
    LateFailure,
    /// No persist with this request id is pending.
    Stale,
}

#[derive(Debug, Clone, PartialEq)]
struct PendingPersist {
    request_id: Uuid,
    candidate: Config,
    explicit_paths: BTreeSet<String>,
    description: String,
    deadline_ns: u64,
    answered: bool,
}

pub(crate) struct ConfigurationService {
    port: Box<dyn Dispatch<ConfigJob>>,
    /// Configuration the process was sized with; restart-only fields of the
    /// effective configuration never move away from it at runtime.
    running: Config,
    saved: Config,
    effective: Config,
    origins: BTreeMap<String, ValueOrigin>,
    cli_values: BTreeMap<String, Value>,
    status: PersistenceStatus,
    pending: Option<PendingPersist>,
    last_result: Option<String>,
    restart_pending: Vec<String>,
}

impl ConfigurationService {
    pub(crate) fn new(
        port: Box<dyn Dispatch<ConfigJob>>,
        saved: Config,
        effective: Config,
        origins: BTreeMap<String, ValueOrigin>,
    ) -> Self {
        let effective_json = serde_json::to_value(&effective).unwrap_or(Value::Null);
        let cli_values = origins
            .iter()
            .filter(|(_, origin)| **origin == ValueOrigin::CliOverride)
            .filter_map(|(path, _)| {
                value_at(&effective_json, path).map(|value| (path.clone(), value.clone()))
            })
            .collect();
        let restart_pending = restart_paths(&effective, &saved);
        Self {
            port,
            running: effective.clone(),
            saved,
            effective,
            origins,
            cli_values,
            status: PersistenceStatus::healthy(PersistenceKind::Config),
            pending: None,
            last_result: None,
            restart_pending,
        }
    }

    pub(crate) fn saved(&self) -> &Config {
        &self.saved
    }

    pub(crate) fn effective(&self) -> &Config {
        &self.effective
    }

    pub(crate) fn origins(&self) -> &BTreeMap<String, ValueOrigin> {
        &self.origins
    }

    pub(crate) fn status(&self) -> &PersistenceStatus {
        &self.status
    }

    pub(crate) fn restart_pending(&self) -> &[String] {
        &self.restart_pending
    }

    pub(crate) fn last_result(&self) -> Option<String> {
        self.last_result.clone()
    }

    /// Settings-view text while a change waits for its durable receipt.
    pub(crate) fn pending_label(&self) -> Option<String> {
        self.pending
            .as_ref()
            .map(|pending| format!("saving: {}", pending.description))
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn submit(
        &mut self,
        request_id: Uuid,
        request: ConfigRequest,
        catalog: &CatalogSnapshot,
        now_ns: u64,
    ) -> ConfigDecision {
        match self.stage(request_id, request, catalog) {
            Ok(Some((candidate, description))) => {
                self.persist(request_id, candidate, description, now_ns)
            }
            Ok(None) => {
                let mut result = CommandResult::completed(request_id, "configuration unchanged");
                result.config_revision = Some(self.saved.revision);
                ConfigDecision::Reply(result)
            }
            Err(result) => ConfigDecision::Reply(*result),
        }
    }

    /// Validates a request into `(candidate, description)`; `None` = no-op.
    fn stage(
        &mut self,
        request_id: Uuid,
        request: ConfigRequest,
        catalog: &CatalogSnapshot,
    ) -> Result<Option<(Config, String)>, Box<CommandResult>> {
        let reject = |code: ErrorCode, message: &str| {
            Box::new(CommandResult::rejected(request_id, code, message))
        };
        if self.pending.is_some() {
            return Err(reject(
                ErrorCode::Busy,
                "a configuration change is still being persisted",
            ));
        }
        let expected = request.expected_revision();
        if expected != self.saved.revision {
            let mut result = reject(
                ErrorCode::Conflict,
                &format!(
                    "configuration revision is {}, command expected {expected}",
                    self.saved.revision
                ),
            );
            result.config_revision = Some(self.saved.revision);
            return Err(result);
        }
        let (candidate, description) = match request {
            ConfigRequest::SetInterval { interval_ms, .. } => {
                let mut candidate = self.saved.clone();
                candidate.sampling.interval_ms = interval_ms;
                (candidate, format!("sampling interval {interval_ms} ms"))
            }
            ConfigRequest::SetModule {
                module_id, enabled, ..
            } => {
                if !BUILTIN_MODULES.contains(&module_id.as_str()) {
                    return Err(reject(
                        ErrorCode::ValidationFailed,
                        &format!("module {module_id} is not in the built-in registry"),
                    ));
                }
                let candidate = with_module(&self.saved, module_id.as_str(), enabled);
                let verb = if enabled { "enable" } else { "disable" };
                (candidate, format!("{verb} module {module_id}"))
            }
        };
        if let Err(issues) = candidate.validate() {
            return Err(reject(ErrorCode::ValidationFailed, &issue_summary(&issues)));
        }
        if let Some(problem) = registry_or_pin_problem(&candidate, catalog) {
            return Err(reject(ErrorCode::ValidationFailed, &problem));
        }
        if candidate == self.saved {
            return Ok(None);
        }
        Ok(Some((candidate, description)))
    }

    fn persist(
        &mut self,
        request_id: Uuid,
        candidate: Config,
        description: String,
        now_ns: u64,
    ) -> ConfigDecision {
        let job = ConfigJob::Persist {
            request_id,
            expected_revision: self.saved.revision,
            candidate: Box::new(candidate.clone()),
        };
        if let Err(error) = self.port.try_dispatch(job) {
            let code = match error {
                DispatchError::Closed => ErrorCode::Unavailable,
                DispatchError::Busy | DispatchError::Capacity(_) => ErrorCode::Busy,
            };
            return ConfigDecision::Reply(CommandResult::rejected(
                request_id,
                code,
                &format!("config worker refused the change: {error}"),
            ));
        }
        let explicit_paths = changed_paths(&self.saved, &candidate);
        self.pending = Some(PendingPersist {
            request_id,
            candidate,
            explicit_paths,
            description,
            deadline_ns: now_ns.saturating_add(PERSIST_TIMEOUT_NS),
            answered: false,
        });
        ConfigDecision::Pending
    }

    pub(crate) fn on_persist_result(
        &mut self,
        request_id: Uuid,
        result: Result<dgxtop_core::ports::PersistReceipt, PersistError>,
    ) -> PersistOutcome {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.request_id != request_id)
        {
            return PersistOutcome::Stale;
        }
        let Some(pending) = self.pending.take() else {
            return PersistOutcome::Stale;
        };
        match result {
            Ok(receipt) => {
                let mut saved = pending.candidate;
                saved.revision = receipt.revision;
                let previous = self.adopt(saved, &pending.explicit_paths);
                self.status = PersistenceStatus::healthy(PersistenceKind::Config);
                self.last_result = Some(format!(
                    "applied revision {} ({})",
                    receipt.revision, pending.description
                ));
                PersistOutcome::Applied(Box::new(AppliedChange {
                    request_id,
                    previous,
                    revision: receipt.revision,
                    uncertain: None,
                    reply_pending: !pending.answered,
                }))
            }
            Err(PersistError::Uncertain { message, on_disk }) => {
                self.uncertain(request_id, pending, &message, on_disk)
            }
            Err(error) => self.failed(request_id, &pending, &error),
        }
    }

    fn uncertain(
        &mut self,
        request_id: Uuid,
        pending: PendingPersist,
        message: &str,
        on_disk: Option<Box<Config>>,
    ) -> PersistOutcome {
        self.status = status_with(PersistenceState::Degraded, message);
        let adoptable = on_disk.filter(|config| config.validate().is_ok());
        match adoptable {
            Some(config) => {
                let explicit = changed_paths(&self.saved, &config);
                let revision = config.revision;
                let previous = self.adopt(*config, &explicit);
                self.last_result = Some(format!(
                    "commit uncertain; adopted on-disk revision {revision}"
                ));
                PersistOutcome::Applied(Box::new(AppliedChange {
                    request_id,
                    previous,
                    revision,
                    uncertain: Some(sanitize_message(message)),
                    reply_pending: !pending.answered,
                }))
            }
            None => {
                // The rename may have replaced the file, so neither the old
                // nor the new document can be claimed to be on disk
                // (CONFIG.md): only the running configuration is known.
                self.last_result = Some(format!(
                    "commit uncertain; on-disk state unknown (running revision {} kept)",
                    self.saved.revision
                ));
                if pending.answered {
                    return PersistOutcome::LateFailure;
                }
                let detail = ErrorDetail::new(
                    ErrorCode::PersistFailed,
                    format!(
                        "commit outcome uncertain (commit_uncertain): {message}; the on-disk configuration could not be reloaded and is unknown; the running configuration is kept"
                    ),
                )
                .with_request(request_id)
                .uncertain();
                PersistOutcome::Failed(request_id, CommandResult::from_error(request_id, &detail))
            }
        }
    }

    fn failed(
        &mut self,
        request_id: Uuid,
        pending: &PendingPersist,
        error: &PersistError,
    ) -> PersistOutcome {
        let code = match error {
            PersistError::Conflict { .. } => ErrorCode::Conflict,
            PersistError::Invalid(_) => ErrorCode::ValidationFailed,
            PersistError::Io(_) | PersistError::Uncertain { .. } => ErrorCode::PersistFailed,
        };
        let message = match error {
            PersistError::Invalid(issues) => issue_summary(issues),
            other => other.to_string(),
        };
        if matches!(error, PersistError::Io(_)) {
            self.status = status_with(PersistenceState::Failed, &message);
        }
        self.last_result = Some(format!("not saved: {message}"));
        if pending.answered {
            return PersistOutcome::LateFailure;
        }
        let mut result = CommandResult::rejected(request_id, code, &message);
        result.config_revision = Some(self.saved.revision);
        PersistOutcome::Failed(request_id, result)
    }

    /// Answers a persist that exceeded its timeout (outcome unknown).
    pub(crate) fn expire(&mut self, now_ns: u64) -> Option<(Uuid, CommandResult)> {
        let pending = self.pending.as_mut()?;
        if pending.answered || now_ns < pending.deadline_ns {
            return None;
        }
        pending.answered = true;
        let request_id = pending.request_id;
        self.status = status_with(PersistenceState::Failed, "config worker did not answer");
        self.last_result = Some("save outcome unknown: config worker did not answer".to_owned());
        let detail = ErrorDetail::new(
            ErrorCode::PersistFailed,
            "persist outcome unknown (commit_uncertain): config worker did not answer in time; configuration unchanged in memory",
        )
        .with_request(request_id)
        .uncertain();
        let mut result = CommandResult::from_error(request_id, &detail);
        result.config_revision = Some(self.saved.revision);
        Some((request_id, result))
    }

    pub(crate) fn next_deadline(&self) -> Option<u64> {
        self.pending
            .as_ref()
            .filter(|pending| !pending.answered)
            .map(|pending| pending.deadline_ns)
    }

    /// Request id of a persist still waiting for its receipt (shutdown summary).
    pub(crate) fn pending_request(&self) -> Option<Uuid> {
        self.pending.as_ref().map(|pending| pending.request_id)
    }

    /// Swaps in a durable configuration; returns the previous effective one.
    fn adopt(&mut self, saved: Config, explicit_paths: &BTreeSet<String>) -> Config {
        for path in explicit_paths {
            let sticky =
                path == READ_ONLY_PATH && self.cli_values.get(path) == Some(&Value::Bool(true));
            if sticky {
                continue;
            }
            self.cli_values.remove(path);
            self.origins.insert(path.clone(), ValueOrigin::Saved);
        }
        self.saved = saved;
        let mut effective = self.with_cli_overrides(&self.saved);
        pin_restart_only(&mut effective, &self.running);
        self.restart_pending = restart_paths(&self.running, &self.saved);
        std::mem::replace(&mut self.effective, effective)
    }

    fn with_cli_overrides(&self, saved: &Config) -> Config {
        if self.cli_values.is_empty() {
            return saved.clone();
        }
        let Ok(mut json) = serde_json::to_value(saved) else {
            return saved.clone();
        };
        for (path, value) in &self.cli_values {
            set_at(&mut json, path, value.clone());
        }
        serde_json::from_value::<Config>(json)
            .ok()
            .filter(|config| config.validate().is_ok())
            .unwrap_or_else(|| saved.clone())
    }
}

/// Result of a successfully applied change.
pub(crate) fn applied_result(change: &AppliedChange, effective_interval_ms: u32) -> CommandResult {
    let mut result = match &change.uncertain {
        None => CommandResult::completed(change.request_id, "configuration applied"),
        Some(message) => {
            let detail = ErrorDetail::new(
                ErrorCode::PersistFailed,
                format!(
                    "commit outcome uncertain (commit_uncertain): {message}; adopted the on-disk configuration"
                ),
            )
            .with_request(change.request_id)
            .uncertain();
            CommandResult::from_error(change.request_id, &detail)
        }
    };
    result.config_revision = Some(change.revision);
    result.effective_interval_ms = Some(effective_interval_ms);
    result
}

fn status_with(state: PersistenceState, detail: &str) -> PersistenceStatus {
    PersistenceStatus {
        state,
        detail: Some(sanitize_message(detail)),
        ..PersistenceStatus::healthy(PersistenceKind::Config)
    }
}

fn restart_paths(running: &Config, saved: &Config) -> Vec<String> {
    running
        .restart_required_changes(saved)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Keeps the running values of every restart-only field (the set checked by
/// `Config::restart_required_changes`): they cannot change before a restart.
fn pin_restart_only(effective: &mut Config, running: &Config) {
    effective.queues = running.queues.clone();
    effective.runtime = running.runtime.clone();
    effective.sampling.max_adapters = running.sampling.max_adapters;
    let store = &mut effective.store;
    store.memory_limit_mib = running.store.memory_limit_mib;
    store.max_series = running.store.max_series;
    store.max_sources = running.store.max_sources;
    store.max_processes = running.store.max_processes;
    store.max_process_gpu_links = running.store.max_process_gpu_links;
    store.archive_enabled = running.store.archive_enabled;
    store.process_history_enabled = running.store.process_history_enabled;
}

fn with_module(saved: &Config, module: &str, enabled: bool) -> Config {
    let mut candidate = saved.clone();
    let modules = &mut candidate.viewer.enabled_modules;
    let present = modules.iter().any(|entry| entry == module);
    if enabled && !present {
        modules.push(module.to_owned());
    } else if !enabled && present {
        modules.retain(|entry| entry != module);
    }
    candidate
}

/// Registry and pin rules that need runtime knowledge (CONFIG.md).
fn registry_or_pin_problem(candidate: &Config, catalog: &CatalogSnapshot) -> Option<String> {
    if let Some(unknown) = candidate
        .viewer
        .enabled_modules
        .iter()
        .find(|module| !is_known_module(module))
    {
        return Some(format!(
            "viewer.enabled_modules: {unknown} is not in the built-in registry"
        ));
    }
    candidate
        .quality
        .pinned_sources
        .iter()
        .enumerate()
        .find(|(_, pin)| {
            catalog
                .source(&pin.source_id)
                .is_none_or(|source| source.metric.as_ref() != Some(&pin.key))
        })
        .map(|(index, pin)| {
            format!(
                "quality.pinned_sources[{index}]: source {} is not a catalog candidate for that MetricKey",
                pin.source_id
            )
        })
}

fn issue_summary(issues: &[ValidationIssue]) -> String {
    let mut parts: Vec<String> = issues
        .iter()
        .take(MAX_ISSUES_IN_MESSAGE)
        .map(|issue| format!("{}: {}", issue.path, issue.message))
        .collect();
    if issues.len() > MAX_ISSUES_IN_MESSAGE {
        parts.push(format!("+{} more", issues.len() - MAX_ISSUES_IN_MESSAGE));
    }
    format!("invalid configuration: {}", parts.join("; "))
}

/// Dotted leaf paths whose values differ (arrays compare as one leaf).
fn changed_paths(before: &Config, after: &Config) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    if let (Ok(left), Ok(right)) = (serde_json::to_value(before), serde_json::to_value(after)) {
        diff_values(&left, &right, "", &mut paths);
    }
    paths.remove("revision");
    paths
}

fn diff_values(left: &Value, right: &Value, prefix: &str, paths: &mut BTreeSet<String>) {
    match (left, right) {
        (Value::Object(left_map), Value::Object(right_map)) => {
            let keys: BTreeSet<&String> = left_map.keys().chain(right_map.keys()).collect();
            for key in keys {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match (left_map.get(key), right_map.get(key)) {
                    (Some(a), Some(b)) => diff_values(a, b, &path, paths),
                    _ => {
                        paths.insert(path);
                    }
                }
            }
        }
        (a, b) if a != b => {
            paths.insert(prefix.to_owned());
        }
        _ => {}
    }
}

fn value_at<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(root, |node, segment| node.as_object()?.get(segment))
}

fn set_at(root: &mut Value, path: &str, value: Value) {
    let mut node = root;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let Some(map) = node.as_object_mut() else {
            return;
        };
        if segments.peek().is_none() {
            if map.contains_key(segment) {
                map.insert(segment.to_owned(), value);
            }
            return;
        }
        let Some(child) = map.get_mut(segment) else {
            return;
        };
        node = child;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::source_descriptor;
    use dgxtop_core::config::{PinnedSource, Theme};
    use dgxtop_core::errors::ResultCode;
    use dgxtop_core::ports::PersistReceipt;
    use dgxtop_core::testing::{CapturingDispatch, metric_key, source_id};
    use std::sync::Arc;

    const NOW: u64 = 50 * NANOS_PER_SECOND;

    struct Bench {
        service: ConfigurationService,
        port: CapturingDispatch<ConfigJob>,
    }

    fn bench_with(
        saved: Config,
        effective: Config,
        origins: BTreeMap<String, ValueOrigin>,
    ) -> Bench {
        let port = CapturingDispatch::new();
        let service = ConfigurationService::new(Box::new(port.clone()), saved, effective, origins);
        Bench { service, port }
    }

    fn bench() -> Bench {
        let config = Config::defaults().unwrap();
        bench_with(config.clone(), config, BTreeMap::new())
    }

    fn receipt(revision: u32) -> Result<PersistReceipt, PersistError> {
        Ok(PersistReceipt {
            revision,
            sha256: "0".repeat(64),
            path: "/tmp/config.json".into(),
        })
    }

    fn catalog() -> CatalogSnapshot {
        CatalogSnapshot::default()
    }

    fn rejected_code(decision: &ConfigDecision) -> ResultCode {
        match decision {
            ConfigDecision::Reply(result) => result.code,
            ConfigDecision::Pending => panic!("expected an immediate reply"),
        }
    }

    /// A retired module id (D-91) may stay enabled in a saved file, as in
    /// the spec's default, but cannot be toggled: it is no longer listed.
    #[test]
    fn retired_modules_are_accepted_but_not_toggled() {
        let mut saved = Config::defaults().unwrap();
        assert!(
            saved
                .viewer
                .enabled_modules
                .iter()
                .any(|module| module == "sources")
        );
        assert_eq!(registry_or_pin_problem(&saved, &catalog()), None);
        saved.viewer.enabled_modules.push("plugin".into());
        assert!(registry_or_pin_problem(&saved, &catalog()).is_some());
        let mut bench = bench();
        let toggled = bench.service.submit(
            Uuid::new_v4(),
            ConfigRequest::SetModule {
                module_id: ModuleId::new("sources").unwrap(),
                enabled: false,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        assert_eq!(
            rejected_code(&toggled),
            ResultCode::from(ErrorCode::ValidationFailed)
        );
        assert!(bench.port.is_empty());
    }

    #[test]
    fn interval_change_applies_only_after_the_durable_receipt() {
        let mut bench = bench();
        let id = Uuid::new_v4();
        let decision = bench.service.submit(
            id,
            ConfigRequest::SetInterval {
                interval_ms: 2000,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        assert_eq!(decision, ConfigDecision::Pending);
        let jobs = bench.port.take();
        let ConfigJob::Persist {
            request_id,
            expected_revision,
            candidate,
        } = &jobs[0];
        assert_eq!(*request_id, id);
        assert_eq!(*expected_revision, 1);
        assert_eq!(
            candidate.revision, 1,
            "candidate.revision equals the CAS base"
        );
        assert_eq!(candidate.sampling.interval_ms, 2000);
        // Nothing changed before the receipt; the settings view shows pending.
        assert_eq!(bench.service.effective().sampling.interval_ms, 1000);
        assert!(bench.service.pending_label().unwrap().contains("2000"));
        let PersistOutcome::Applied(change) = bench.service.on_persist_result(id, receipt(2))
        else {
            panic!("applied expected");
        };
        assert_eq!(change.revision, 2);
        assert!(change.reply_pending);
        assert_eq!(change.previous.sampling.interval_ms, 1000);
        assert_eq!(bench.service.saved().revision, 2);
        assert_eq!(bench.service.effective().sampling.interval_ms, 2000);
        assert!(!bench.service.has_pending());
        let result = applied_result(&change, 2000);
        assert!(result.code.is_ok());
        assert_eq!(result.config_revision, Some(2));
        assert_eq!(result.effective_interval_ms, Some(2000));
        // A duplicate receipt is stale.
        assert_eq!(
            bench.service.on_persist_result(id, receipt(2)),
            PersistOutcome::Stale
        );
    }

    #[test]
    fn old_revision_conflicts_and_pending_change_is_busy() {
        let mut bench = bench();
        let conflict = bench.service.submit(
            Uuid::new_v4(),
            ConfigRequest::SetInterval {
                interval_ms: 2000,
                expected_revision: 7,
            },
            &catalog(),
            NOW,
        );
        assert_eq!(
            rejected_code(&conflict),
            ResultCode::from(ErrorCode::Conflict)
        );
        let first = Uuid::new_v4();
        bench.service.submit(
            first,
            ConfigRequest::SetInterval {
                interval_ms: 2000,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        let busy = bench.service.submit(
            Uuid::new_v4(),
            ConfigRequest::SetInterval {
                interval_ms: 3000,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        assert_eq!(rejected_code(&busy), ResultCode::from(ErrorCode::Busy));
        assert_eq!(bench.port.len(), 1);
    }

    /// A change is validated as a whole configuration: its schema, the
    /// module registry and the pins against the catalog.
    #[test]
    fn changes_are_validated_against_schema_registry_and_pins() {
        let mut bench = bench();
        let out_of_range = bench.service.submit(
            Uuid::new_v4(),
            ConfigRequest::SetInterval {
                interval_ms: 1,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        let ConfigDecision::Reply(result) = out_of_range else {
            panic!("immediate reply expected");
        };
        assert_eq!(result.code, ResultCode::from(ErrorCode::ValidationFailed));
        assert!(
            result.message.contains("sampling.interval_ms"),
            "{result:?}"
        );
        assert!(bench.port.is_empty());
        let key = metric_key("cpu.package.0", "cpu.temperature.package", "package:0");
        let mut pinned = Config::defaults().unwrap();
        pinned.quality.pinned_sources.push(PinnedSource {
            key: key.clone(),
            source_id: source_id("hwmon:coretemp:temp1"),
        });
        assert!(
            registry_or_pin_problem(&pinned, &catalog()).is_some(),
            "pin must exist in the catalog"
        );
        let mut known = CatalogSnapshot::default();
        known.sources.insert(
            source_id("hwmon:coretemp:temp1"),
            Arc::new(source_descriptor(
                "hwmon",
                "hwmon:coretemp:temp1",
                1,
                "fast",
                Some(key),
            )),
        );
        assert_eq!(registry_or_pin_problem(&pinned, &known), None);
    }

    #[test]
    fn uncertain_commit_adopts_the_on_disk_configuration() {
        let mut bench = bench();
        let id = Uuid::new_v4();
        bench.service.submit(
            id,
            ConfigRequest::SetInterval {
                interval_ms: 500,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        let mut on_disk = Config::defaults().unwrap();
        on_disk.revision = 2;
        on_disk.sampling.interval_ms = 500;
        let outcome = bench.service.on_persist_result(
            id,
            Err(PersistError::Uncertain {
                message: "directory fsync failed".into(),
                on_disk: Some(Box::new(on_disk)),
            }),
        );
        let PersistOutcome::Applied(change) = outcome else {
            panic!("adoption expected");
        };
        assert_eq!(change.revision, 2);
        assert_eq!(bench.service.saved().sampling.interval_ms, 500);
        let result = applied_result(&change, 500);
        assert_eq!(result.code, ResultCode::from(ErrorCode::PersistFailed));
        assert!(result.message.contains("commit_uncertain"));
        assert_eq!(bench.service.status().state, PersistenceState::Degraded);
    }

    #[test]
    fn adopted_restart_only_values_wait_for_a_restart() {
        let mut bench = bench();
        let id = Uuid::new_v4();
        bench.service.submit(
            id,
            ConfigRequest::SetInterval {
                interval_ms: 500,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        let mut on_disk = Config::defaults().unwrap();
        on_disk.revision = 2;
        on_disk.queues.commands = 64;
        on_disk.runtime.max_pending_requests = 64;
        let outcome = bench.service.on_persist_result(
            id,
            Err(PersistError::Uncertain {
                message: "directory fsync failed".into(),
                on_disk: Some(Box::new(on_disk)),
            }),
        );
        assert!(matches!(outcome, PersistOutcome::Applied(_)));
        assert_eq!(bench.service.saved().queues.commands, 64);
        assert_eq!(
            bench.service.effective().queues.commands,
            128,
            "restart-only values keep running until a restart"
        );
        assert!(
            bench
                .service
                .restart_pending()
                .contains(&"queues".to_owned())
        );
        assert!(
            bench
                .service
                .restart_pending()
                .contains(&"runtime".to_owned())
        );
    }

    #[test]
    fn uncertain_commit_without_on_disk_copy_keeps_the_old_configuration() {
        let mut bench = bench();
        let id = Uuid::new_v4();
        bench.service.submit(
            id,
            ConfigRequest::SetInterval {
                interval_ms: 500,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        let PersistOutcome::Failed(_, result) = bench.service.on_persist_result(
            id,
            Err(PersistError::Uncertain {
                message: "reload failed".into(),
                on_disk: None,
            }),
        ) else {
            panic!();
        };
        assert!(result.message.contains("commit_uncertain"));
        assert_eq!(bench.service.saved().revision, 1);
    }

    #[test]
    fn cli_overrides_stay_effective_only_and_explicit_changes_supersede_them() {
        let saved = Config::defaults().unwrap();
        let mut effective = saved.clone();
        effective.viewer.theme = Theme::Amber;
        effective.sampling.interval_ms = 2000;
        effective.actions.read_only = true;
        let origins = BTreeMap::from([
            ("viewer.theme".to_owned(), ValueOrigin::CliOverride),
            ("sampling.interval_ms".to_owned(), ValueOrigin::CliOverride),
            ("actions.read_only".to_owned(), ValueOrigin::CliOverride),
        ]);
        let mut bench = bench_with(saved, effective, origins);
        let id = Uuid::new_v4();
        bench.service.submit(
            id,
            ConfigRequest::SetInterval {
                interval_ms: 500,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        let jobs = bench.port.take();
        let ConfigJob::Persist { candidate, .. } = &jobs[0];
        assert_eq!(
            candidate.viewer.theme,
            Theme::Dark,
            "CLI values never persist"
        );
        assert!(!candidate.actions.read_only);
        bench.service.on_persist_result(id, receipt(2));
        let effective = bench.service.effective();
        assert_eq!(effective.sampling.interval_ms, 500, "explicit change wins");
        assert_eq!(
            effective.viewer.theme,
            Theme::Amber,
            "CLI override re-applied"
        );
        assert!(effective.actions.read_only);
        assert_eq!(
            bench.service.origins().get("sampling.interval_ms"),
            Some(&ValueOrigin::Saved)
        );
        assert_eq!(
            bench.service.origins().get("viewer.theme"),
            Some(&ValueOrigin::CliOverride)
        );
    }

    #[test]
    fn persist_timeout_answers_and_late_receipt_is_still_adopted() {
        let mut bench = bench();
        let id = Uuid::new_v4();
        bench.service.submit(
            id,
            ConfigRequest::SetInterval {
                interval_ms: 3000,
                expected_revision: 1,
            },
            &catalog(),
            NOW,
        );
        assert!(bench.service.expire(NOW + NANOS_PER_SECOND).is_none());
        assert_eq!(
            bench.service.next_deadline(),
            Some(NOW + PERSIST_TIMEOUT_NS)
        );
        let (expired_id, result) = bench.service.expire(NOW + PERSIST_TIMEOUT_NS).unwrap();
        assert_eq!(expired_id, id);
        assert!(result.message.contains("commit_uncertain"));
        assert!(bench.service.next_deadline().is_none());
        assert_eq!(bench.service.pending_request(), Some(id));
        let PersistOutcome::Applied(change) = bench.service.on_persist_result(id, receipt(2))
        else {
            panic!();
        };
        assert!(!change.reply_pending, "already answered");
        assert_eq!(bench.service.effective().sampling.interval_ms, 3000);
    }

    #[test]
    fn pinning_covers_every_restart_only_field() {
        let running = Config::defaults().unwrap();
        let mut moved = running.clone();
        moved.queues.commands = 64;
        moved.runtime.history_workers = 1;
        moved.sampling.max_adapters = 4;
        moved.store.memory_limit_mib = 512;
        moved.store.max_series = 100;
        moved.store.max_sources = 100;
        moved.store.max_processes = 100;
        moved.store.max_process_gpu_links = 100;
        moved.store.archive_enabled = !running.store.archive_enabled;
        moved.store.process_history_enabled = Some(!running.store.process_history_on_disk());
        assert_eq!(running.restart_required_changes(&moved).len(), 10);
        pin_restart_only(&mut moved, &running);
        assert!(running.restart_required_changes(&moved).is_empty());
    }
}
