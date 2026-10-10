//! Fail-closed policy gate evaluated before any descriptor is opened. The
//! order is fixed: disabled/read-only, root session, force, namespace/host/
//! boot, protected targets. The first failing rule decides the error.

use dgxtop_core::action::{ActionError, CapabilityContext, Signal};
use dgxtop_core::identity::ProcessKey;

/// PID 1 (init) is never a valid target.
const INIT_PID: i32 = 1;

/// Checks the caller context and target key; no system call is made.
pub fn check_policy(
    key: &ProcessKey,
    signal: Signal,
    context: &CapabilityContext,
    own_pid: i32,
) -> Result<(), ActionError> {
    if !context.actions_enabled {
        return Err(ActionError::Disabled(
            "process actions are disabled".to_owned(),
        ));
    }
    if context.read_only {
        return Err(ActionError::Disabled("read-only mode".to_owned()));
    }
    if context.euid == 0 {
        return Err(ActionError::RootSession);
    }
    if signal == Signal::Sigkill && !context.allow_force {
        return Err(ActionError::ForceNotAllowed);
    }
    if key.host_id != context.host_id
        || key.boot_id != context.boot_id
        || key.pid_namespace != context.pid_namespace
    {
        return Err(ActionError::NamespaceMismatch);
    }
    protected_reason(key.pid.get(), context, own_pid).map_or(Ok(()), |reason| {
        Err(ActionError::Protected(reason.to_owned()))
    })
}

fn protected_reason(pid: i32, context: &CapabilityContext, own_pid: i32) -> Option<&'static str> {
    if pid == INIT_PID {
        Some("pid 1")
    } else if pid == own_pid {
        Some("dgxtop itself")
    } else if context.protected_pids.contains(&pid) {
        Some("managed dgxtop process")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::identity::{Pid, PidNamespace, SessionId};
    use dgxtop_core::testing::{boot_id, host_id};

    fn context() -> CapabilityContext {
        CapabilityContext {
            host_id: host_id("h"),
            boot_id: boot_id("b"),
            pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
            session_id: SessionId::new("s").unwrap(),
            euid: 1000,
            policy_revision: 1,
            actions_enabled: true,
            read_only: false,
            allow_force: false,
            protected_pids: vec![77],
        }
    }

    fn key(pid: i64) -> ProcessKey {
        ProcessKey {
            host_id: host_id("h"),
            boot_id: boot_id("b"),
            pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
            pid: Pid::new(pid).unwrap(),
            start_time_ticks: 5,
        }
    }

    #[test]
    fn allowed_target_passes() {
        assert_eq!(
            check_policy(&key(500), Signal::Sigterm, &context(), 9),
            Ok(())
        );
    }

    #[test]
    fn rules_apply_in_fixed_order() {
        let mut everything_wrong = context();
        everything_wrong.actions_enabled = false;
        everything_wrong.read_only = true;
        everything_wrong.euid = 0;
        assert!(matches!(
            check_policy(&key(1), Signal::Sigkill, &everything_wrong, 9),
            Err(ActionError::Disabled(_))
        ));
        everything_wrong.actions_enabled = true;
        assert!(matches!(
            check_policy(&key(1), Signal::Sigkill, &everything_wrong, 9),
            Err(ActionError::Disabled(_))
        ));
        everything_wrong.read_only = false;
        assert_eq!(
            check_policy(&key(1), Signal::Sigkill, &everything_wrong, 9),
            Err(ActionError::RootSession)
        );
        everything_wrong.euid = 1000;
        assert_eq!(
            check_policy(&key(1), Signal::Sigkill, &everything_wrong, 9),
            Err(ActionError::ForceNotAllowed)
        );
        let mut foreign = key(1);
        foreign.boot_id = boot_id("other");
        assert_eq!(
            check_policy(&foreign, Signal::Sigterm, &context(), 9),
            Err(ActionError::NamespaceMismatch)
        );
    }

    #[test]
    fn protected_targets_are_refused() {
        for pid in [1, 9, 77] {
            assert!(matches!(
                check_policy(&key(pid), Signal::Sigterm, &context(), 9),
                Err(ActionError::Protected(_))
            ));
        }
        let mut forced = context();
        forced.allow_force = true;
        assert_eq!(check_policy(&key(500), Signal::Sigkill, &forced, 9), Ok(()));
    }
}
