//! Identity re-verification while the pidfd is held. Opening the pidfd first
//! and only then reading `/proc` closes the PID-reuse window: if the PID was
//! recycled before `pidfd_open`, the start time no longer matches; the final
//! liveness check proves the held descriptor still names that live process.

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use dgxtop_core::action::{ActionError, CapabilityContext, VerifiedIdentity};
use dgxtop_core::identity::ProcessKey;
use dgxtop_core::text::{MAX_BASENAME_BYTES, truncate_utf8};

use super::pidfd::{PidfdReadiness, PidfdSyscalls};
use super::proc_files::{
    PidfdTarget, ProcessFiles, StatFacts, parse_pidfd_target, parse_stat, parse_uids,
};

/// Verifies `key` against `/proc` and the held `pidfd`; any doubt rejects.
pub fn verify_identity(
    files: &dyn ProcessFiles,
    syscalls: &dyn PidfdSyscalls,
    pidfd: &OwnedFd,
    key: &ProcessKey,
    context: &CapabilityContext,
) -> Result<VerifiedIdentity, ActionError> {
    let pid = key.pid.get();
    let stat = read_stat(files, pid)?;
    if stat.start_time_ticks != key.start_time_ticks {
        return Err(ActionError::IdentityChanged);
    }
    let status = files
        .status(pid)
        .map_err(|error| unverifiable("status", &error))?;
    let uids = parse_uids(&status)
        .ok_or_else(|| ActionError::Unavailable("unparseable /proc status".to_owned()))?;
    if uids.real != context.euid || uids.effective != context.euid {
        return Err(ActionError::UidMismatch);
    }
    let namespace = files
        .pid_namespace(pid)
        .map_err(|error| unverifiable("pid namespace", &error))?;
    if namespace != key.pid_namespace.as_str() {
        return Err(ActionError::NamespaceMismatch);
    }
    ensure_held_target_alive(files, syscalls, pidfd, pid)?;
    Ok(VerifiedIdentity {
        key: key.clone(),
        uid: uids.real,
        // Raw (bounded): the summary sanitizes it, the audit escapes it.
        basename: truncate_utf8(&stat.comm, MAX_BASENAME_BYTES).to_owned(),
    })
}

/// Reads and parses `stat`; a vanished entry means the process exited.
pub fn read_stat(files: &dyn ProcessFiles, pid: i32) -> Result<StatFacts, ActionError> {
    let text = files
        .stat(pid)
        .map_err(|error| unverifiable("stat", &error))?;
    parse_stat(&text).ok_or_else(|| ActionError::Unavailable("unparseable /proc stat".to_owned()))
}

/// The held descriptor must be unreadable (process alive) and must name `pid`.
fn ensure_held_target_alive(
    files: &dyn ProcessFiles,
    syscalls: &dyn PidfdSyscalls,
    pidfd: &OwnedFd,
    pid: i32,
) -> Result<(), ActionError> {
    match syscalls.readiness(pidfd.as_fd()) {
        Ok(PidfdReadiness::Running) => {}
        Ok(PidfdReadiness::Exited) => return Err(ActionError::Exited),
        Err(errno) => {
            return Err(ActionError::Unavailable(format!(
                "pidfd poll failed: errno {}",
                errno as i32
            )));
        }
    }
    let info = files
        .descriptor_info(pidfd.as_raw_fd())
        .map_err(|error| unverifiable("pidfd fdinfo", &error))?;
    match parse_pidfd_target(&info) {
        Some(PidfdTarget::Pid(target)) if target == i64::from(pid) => Ok(()),
        Some(PidfdTarget::Pid(_)) => Err(ActionError::IdentityChanged),
        Some(PidfdTarget::Exited) => Err(ActionError::Exited),
        Some(PidfdTarget::OutsideNamespace) => Err(ActionError::NamespaceMismatch),
        None => Err(ActionError::Unavailable(
            "pidfd fdinfo does not name a pid".to_owned(),
        )),
    }
}

fn unverifiable(what: &str, error: &io::Error) -> ActionError {
    if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) {
        return ActionError::Exited;
    }
    ActionError::Unavailable(format!("cannot read {what}: {}", error.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vanished_proc_entries_mean_exited() {
        assert_eq!(
            unverifiable("stat", &io::Error::from(io::ErrorKind::NotFound)),
            ActionError::Exited
        );
        assert_eq!(
            unverifiable("stat", &io::Error::from_raw_os_error(libc::ESRCH)),
            ActionError::Exited
        );
        assert!(matches!(
            unverifiable("stat", &io::Error::from(io::ErrorKind::PermissionDenied)),
            ActionError::Unavailable(_)
        ));
    }
}
