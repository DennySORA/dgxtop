//! Capability checks performed before anything reaches the action worker
//! (SECURITY.md §1–3). The caller context is built from the REAL euid of
//! this process and the setup's host/boot/namespace — never from command
//! JSON. Every check fails closed.

use dgxtop_core::action::{CapabilityContext, PersistenceStatus, Signal};
use dgxtop_core::canonical::ProcessRow;
use dgxtop_core::config::Config;
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::{BootId, HostId, PidNamespace, ProcessKey, SessionId};

/// Identity of this dgxtop instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionIdentity {
    pub(crate) host_id: HostId,
    pub(crate) boot_id: BootId,
    pub(crate) pid_namespace: PidNamespace,
    /// Real effective uid of the dgxtop process.
    pub(crate) euid: u32,
    pub(crate) self_pid: i32,
}

/// The action-relevant part of the effective configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionPolicy {
    pub(crate) enabled: bool,
    pub(crate) read_only: bool,
    pub(crate) allow_force: bool,
    pub(crate) same_uid_only: bool,
    pub(crate) confirmation_ttl_ms: u32,
    pub(crate) max_pending_confirmations: usize,
    pub(crate) intent_deadline_ms: u32,
}

impl ActionPolicy {
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            enabled: config.actions.enabled,
            read_only: config.actions.read_only,
            allow_force: config.actions.allow_force,
            // Fixed safety constant (validated true); kept explicit here.
            same_uid_only: config.actions.same_uid_only,
            confirmation_ttl_ms: config.actions.confirmation_ttl_ms,
            max_pending_confirmations: usize::try_from(config.actions.max_pending_confirmations)
                .unwrap_or(1)
                .max(1),
            intent_deadline_ms: config.runtime.audit_intent_deadline_ms,
        }
    }
}

/// A refused action with its wire code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Denial {
    pub(crate) code: ErrorCode,
    pub(crate) reason: String,
}

impl Denial {
    fn new(code: ErrorCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

/// Audit readiness as seen by the ProcessService.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AuditReadiness<'a> {
    pub(crate) status: Option<&'a PersistenceStatus>,
    pub(crate) recovered: bool,
    pub(crate) backlog: bool,
}

/// Target-independent reasons why no action may start now.
pub(crate) fn availability_problem(
    identity: &ActionIdentity,
    policy: &ActionPolicy,
    audit: AuditReadiness<'_>,
) -> Option<Denial> {
    if !policy.enabled {
        return Some(Denial::new(
            ErrorCode::CapabilityDenied,
            "process actions are disabled (actions.enabled=false)",
        ));
    }
    if policy.read_only {
        return Some(Denial::new(
            ErrorCode::CapabilityDenied,
            "read-only mode: process actions are not allowed",
        ));
    }
    // deny_root_session is a fixed safety constant: root never signals.
    if identity.euid == 0 {
        return Some(Denial::new(
            ErrorCode::CapabilityDenied,
            "root sessions may not send process signals",
        ));
    }
    audit_problem(audit)
}

fn audit_problem(audit: AuditReadiness<'_>) -> Option<Denial> {
    let Some(status) = audit.status else {
        return Some(Denial::new(
            ErrorCode::Unavailable,
            "audit store status not yet known; actions fail closed",
        ));
    };
    if !status.accepts_writes() {
        let detail = status.detail.as_deref().unwrap_or("not accepting writes");
        return Some(Denial::new(
            ErrorCode::Unavailable,
            format!("audit store unavailable ({detail}); actions fail closed"),
        ));
    }
    if !audit.recovered {
        return Some(Denial::new(
            ErrorCode::Unavailable,
            "audit recovery of earlier intents has not completed",
        ));
    }
    if audit.backlog {
        return Some(Denial::new(
            ErrorCode::Unavailable,
            "audit records are still waiting to be written",
        ));
    }
    None
}

/// Target-specific checks for a `PrepareSignal`.
pub(crate) fn check_target(
    identity: &ActionIdentity,
    policy: &ActionPolicy,
    target: &ProcessKey,
    signal: Signal,
    row: Option<&ProcessRow>,
    protected_pids: &[i32],
) -> Result<(), Denial> {
    if signal == Signal::Sigkill && !policy.allow_force {
        return Err(Denial::new(
            ErrorCode::CapabilityDenied,
            "SIGKILL requires actions.allow_force",
        ));
    }
    if target.host_id != identity.host_id || target.boot_id != identity.boot_id {
        return Err(Denial::new(
            ErrorCode::NamespaceMismatch,
            "target belongs to another host or boot",
        ));
    }
    if target.pid_namespace != identity.pid_namespace {
        return Err(Denial::new(
            ErrorCode::NamespaceMismatch,
            "target is in another pid namespace",
        ));
    }
    let pid = target.pid.get();
    if pid == 1 {
        return Err(Denial::new(
            ErrorCode::CapabilityDenied,
            "PID 1 is protected",
        ));
    }
    if pid == identity.self_pid || protected_pids.contains(&pid) {
        return Err(Denial::new(
            ErrorCode::CapabilityDenied,
            "protected target (dgxtop itself or a managed adapter host)",
        ));
    }
    let foreign_uid = row
        .and_then(|row| row.uid)
        .is_some_and(|uid| uid != identity.euid);
    if policy.same_uid_only && foreign_uid {
        return Err(Denial::new(
            ErrorCode::CapabilityDenied,
            "target belongs to another user",
        ));
    }
    Ok(())
}

/// The caller context handed to the action worker.
pub(crate) fn capability_context(
    identity: &ActionIdentity,
    policy: &ActionPolicy,
    session_id: &SessionId,
    policy_revision: u32,
    protected_pids: &[i32],
) -> CapabilityContext {
    let mut protected: Vec<i32> = protected_pids.to_vec();
    protected.push(identity.self_pid);
    protected.push(1);
    protected.sort_unstable();
    protected.dedup();
    CapabilityContext {
        host_id: identity.host_id.clone(),
        boot_id: identity.boot_id.clone(),
        pid_namespace: identity.pid_namespace.clone(),
        session_id: session_id.clone(),
        euid: identity.euid,
        policy_revision,
        actions_enabled: policy.enabled,
        read_only: policy.read_only,
        allow_force: policy.allow_force,
        protected_pids: protected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::action::{PersistenceKind, PersistenceState};
    use dgxtop_core::identity::Pid;
    use dgxtop_core::testing::{boot_id, host_id};

    fn identity(euid: u32) -> ActionIdentity {
        ActionIdentity {
            host_id: host_id("test-host"),
            boot_id: boot_id("fake-boot"),
            pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
            euid,
            self_pid: 4000,
        }
    }

    fn policy() -> ActionPolicy {
        ActionPolicy::from_config(&Config::defaults().unwrap())
    }

    fn healthy() -> PersistenceStatus {
        PersistenceStatus::healthy(PersistenceKind::Audit)
    }

    fn ready(status: &PersistenceStatus) -> AuditReadiness<'_> {
        AuditReadiness {
            status: Some(status),
            recovered: true,
            backlog: false,
        }
    }

    fn key(pid: i64) -> ProcessKey {
        ProcessKey {
            host_id: host_id("test-host"),
            boot_id: boot_id("fake-boot"),
            pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
            pid: Pid::new(pid).unwrap(),
            start_time_ticks: 10,
        }
    }

    #[test]
    fn availability_fails_closed() {
        let status = healthy();
        assert!(availability_problem(&identity(1000), &policy(), ready(&status)).is_none());
        let root = availability_problem(&identity(0), &policy(), ready(&status)).unwrap();
        assert_eq!(root.code, ErrorCode::CapabilityDenied);
        let mut read_only = policy();
        read_only.read_only = true;
        assert!(availability_problem(&identity(1000), &read_only, ready(&status)).is_some());
        let mut disabled = policy();
        disabled.enabled = false;
        assert!(availability_problem(&identity(1000), &disabled, ready(&status)).is_some());
        let unknown = AuditReadiness {
            status: None,
            recovered: true,
            backlog: false,
        };
        assert_eq!(
            availability_problem(&identity(1000), &policy(), unknown)
                .unwrap()
                .code,
            ErrorCode::Unavailable
        );
        let failed = PersistenceStatus {
            state: PersistenceState::Failed,
            ..healthy()
        };
        assert!(availability_problem(&identity(1000), &policy(), ready(&failed)).is_some());
        let unrecovered = AuditReadiness {
            recovered: false,
            ..ready(&status)
        };
        assert!(availability_problem(&identity(1000), &policy(), unrecovered).is_some());
    }

    #[test]
    fn target_checks_cover_force_namespace_protection_and_uid() {
        let me = identity(1000);
        let rules = policy();
        assert!(check_target(&me, &rules, &key(42), Signal::Sigterm, None, &[]).is_ok());
        assert_eq!(
            check_target(&me, &rules, &key(42), Signal::Sigkill, None, &[])
                .unwrap_err()
                .code,
            ErrorCode::CapabilityDenied
        );
        let mut other_ns = key(42);
        other_ns.pid_namespace = PidNamespace::new("pid:[2]").unwrap();
        assert_eq!(
            check_target(&me, &rules, &other_ns, Signal::Sigterm, None, &[])
                .unwrap_err()
                .code,
            ErrorCode::NamespaceMismatch
        );
        let mut other_boot = key(42);
        other_boot.boot_id = boot_id("other");
        assert_eq!(
            check_target(&me, &rules, &other_boot, Signal::Sigterm, None, &[])
                .unwrap_err()
                .code,
            ErrorCode::NamespaceMismatch
        );
        assert!(check_target(&me, &rules, &key(1), Signal::Sigterm, None, &[]).is_err());
        assert!(check_target(&me, &rules, &key(4000), Signal::Sigterm, None, &[]).is_err());
        assert!(check_target(&me, &rules, &key(77), Signal::Sigterm, None, &[77]).is_err());
        let foreign = ProcessRow {
            key: key(42),
            name: "x".into(),
            uid: Some(0),
            state: None,
            cpu_percent: None,
            rss_bytes: None,
            missing_reason: None,
            gpu: Vec::new(),
            sample_kind: dgxtop_core::quality::SampleKind::Measured,
            read_finished_boot_ns: 0,
        };
        assert!(check_target(&me, &rules, &key(42), Signal::Sigterm, Some(&foreign), &[]).is_err());
    }

    #[test]
    fn context_carries_real_identity_and_protected_pids() {
        let context = capability_context(
            &identity(1000),
            &policy(),
            &SessionId::new("s1").unwrap(),
            3,
            &[77, 4000],
        );
        assert_eq!(context.euid, 1000);
        assert_eq!(context.policy_revision, 3);
        assert_eq!(context.protected_pids, vec![1, 77, 4000]);
        assert!(context.actions_enabled);
        assert!(!context.allow_force);
    }
}
