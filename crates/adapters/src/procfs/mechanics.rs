//! Read and discovery mechanics shared by the procfs, network and filesystem
//! adapters (candidates for `common.rs`): plan targeting and guards,
//! consistent per-group paging, sticky-failure re-probe gating, timed reads,
//! namespace links and identities built from compile-time literals.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::identity::AdapterId;
use dgxtop_core::observation::{
    DiscoverRequest, Observation, ObservationBatch, ReadPlan, split_page,
};
use dgxtop_core::quality::ReadStatus;
use dgxtop_core::text::{sanitize_display, truncate_utf8};

use crate::common::{Pager, ProbeThrottle, RecordFactory, SystemFiles, read_status_of, reason_of};

/// Maximum bytes of a batch or catalog `error_code`.
const MAX_ERROR_CODE_BYTES: usize = 128;
/// Maximum bytes of a namespace link target such as `net:[4026531840]`.
const MAX_NAMESPACE_BYTES: usize = 64;

/// Unwraps an identity built from a compile-time literal. Such literals are
/// covered by unit tests, so a failure is a programming error, not input.
pub(crate) fn invariant<T, E: Display>(result: Result<T, E>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("compile-time identity {what:?} is invalid: {error}"),
    }
}

/// Base read group and page index addressed by a plan. The page comes from
/// `plan.page`, or from a `group:page` suffix when `plan.page` is 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanTarget {
    pub group: String,
    pub page: u32,
}

pub(crate) fn plan_target(plan: &ReadPlan) -> PlanTarget {
    let (group, encoded_page) = split_page(plan.read_group.as_str());
    let page = if plan.page > 0 {
        plan.page
    } else {
        encoded_page
    };
    PlanTarget {
        group: group.to_owned(),
        page,
    }
}

/// Failed batch for a plan this adapter must not serve, `None` otherwise.
pub(crate) fn rejected_plan(
    plan: &ReadPlan,
    adapter_id: &AdapterId,
    shut_down: bool,
    now_boot_ns: u64,
) -> Option<ObservationBatch> {
    let code = if shut_down {
        "ADAPTER_SHUT_DOWN"
    } else if &plan.adapter_id != adapter_id {
        "ADAPTER_ID_MISMATCH"
    } else if now_boot_ns > plan.deadline_boot_ns {
        "DEADLINE_EXPIRED"
    } else {
        return None;
    };
    Some(RecordFactory::failed_batch(plan, now_boot_ns, code))
}

/// Error code for a discovery request this adapter must not serve.
pub(crate) fn rejected_discovery(
    request: &DiscoverRequest,
    adapter_id: &AdapterId,
    shut_down: bool,
    now_boot_ns: u64,
) -> Option<&'static str> {
    if shut_down {
        Some("ADAPTER_SHUT_DOWN")
    } else if &request.adapter_id != adapter_id {
        Some("ADAPTER_ID_MISMATCH")
    } else if now_boot_ns > request.deadline_boot_ns {
        Some("DEADLINE_EXPIRED")
    } else {
        None
    }
}

/// Records of one fresh read-group snapshot (page 0).
#[derive(Debug, Default)]
pub(crate) struct ReadSnapshot {
    pub records: Vec<Observation>,
    pub started_boot_ns: u64,
    pub finished_boot_ns: u64,
    /// Batch error code carried by the final page only.
    pub final_error: Option<String>,
}

/// One consistent pager per read group, so interleaved groups never discard
/// each other's snapshot.
#[derive(Debug, Default)]
pub(crate) struct GroupPagers {
    groups: BTreeMap<String, GroupPage>,
}

#[derive(Debug, Default)]
struct GroupPage {
    pager: Pager,
    final_error: Option<String>,
}

impl GroupPagers {
    /// Page 0 builds and stores a fresh snapshot; later pages slice the
    /// snapshot of the same cycle or fail with `PAGE_SNAPSHOT_MISSING`.
    pub(crate) fn serve(
        &mut self,
        plan: &ReadPlan,
        target: &PlanTarget,
        page_size: usize,
        now_boot_ns: u64,
        build: impl FnOnce() -> ReadSnapshot,
    ) -> ObservationBatch {
        let group = self.groups.entry(target.group.clone()).or_default();
        if target.page == 0 {
            let snapshot = build();
            group.final_error = snapshot.final_error;
            group.pager.store(
                plan.cycle_id,
                &target.group,
                snapshot.records,
                snapshot.started_boot_ns,
                snapshot.finished_boot_ns,
            );
        }
        let Some(page) = group
            .pager
            .page(plan.cycle_id, &target.group, target.page, page_size)
        else {
            return RecordFactory::failed_batch(plan, now_boot_ns, "PAGE_SNAPSHOT_MISSING");
        };
        let error = if page.continues {
            None
        } else {
            group.final_error.clone()
        };
        RecordFactory::batch(
            plan,
            page.records,
            page.started_boot_ns,
            page.finished_boot_ns,
            page.continues,
            error,
        )
    }

    pub(crate) fn clear(&mut self) {
        self.groups.clear();
    }
}

/// Status and reason of a failure that should not be retried every cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StickyFailure {
    pub status: ReadStatus,
    pub reason: String,
    last_probe_boot_ns: u64,
}

/// Unsupported and permission-denied outcomes are sticky: the underlying
/// file is touched again at most once per re-probe interval. Transient
/// errors are retried every cycle. The throttle only ever holds keys that
/// are currently failing, so churn cannot grow it.
#[derive(Debug, Clone)]
pub(crate) struct ReprobeGate {
    interval_ms: u64,
    throttle: ProbeThrottle,
    failing: BTreeMap<String, StickyFailure>,
}

impl ReprobeGate {
    pub(crate) fn new(unsupported_probe_ms: u64) -> Self {
        Self {
            interval_ms: unsupported_probe_ms,
            throttle: ProbeThrottle::new(unsupported_probe_ms),
            failing: BTreeMap::new(),
        }
    }

    /// The cached failure when `key` must not be probed now; `None` means
    /// the caller performs the I/O (and reports the outcome).
    pub(crate) fn deferred(&mut self, key: &str, now_boot_ns: u64) -> Option<StickyFailure> {
        let failure = self.failing.get_mut(key)?;
        if self.throttle.should_probe(key, now_boot_ns) {
            failure.last_probe_boot_ns = now_boot_ns;
            return None;
        }
        Some(failure.clone())
    }

    pub(crate) fn record_success(&mut self, key: &str) {
        if self.failing.remove(key).is_some() && self.failing.is_empty() {
            self.throttle.reset();
        }
    }

    pub(crate) fn record_failure(
        &mut self,
        key: &str,
        now_boot_ns: u64,
        status: ReadStatus,
        reason: &str,
    ) {
        if !is_sticky(status) {
            self.record_success(key);
            return;
        }
        if let Some(existing) = self.failing.get_mut(key) {
            existing.status = status;
            existing.reason = reason.to_owned();
            return;
        }
        // Stamps the first probe of this failure episode.
        self.throttle.should_probe(key, now_boot_ns);
        self.failing.insert(
            key.to_owned(),
            StickyFailure {
                status,
                reason: reason.to_owned(),
                last_probe_boot_ns: now_boot_ns,
            },
        );
    }

    /// Re-probes `key` at the next opportunity (device re-created, driver
    /// restarted).
    pub(crate) fn forget(&mut self, key: &str) {
        if self.failing.remove(key).is_some() {
            self.rebuild_throttle();
        }
    }

    /// Drops state of keys that no longer exist.
    pub(crate) fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        let before = self.failing.len();
        self.failing.retain(|key, _| keep(key));
        if self.failing.len() != before {
            self.rebuild_throttle();
        }
    }

    #[cfg(test)]
    pub(crate) fn failing_len(&self) -> usize {
        self.failing.len()
    }

    fn rebuild_throttle(&mut self) {
        self.throttle = ProbeThrottle::new(self.interval_ms);
        for (key, failure) in &self.failing {
            self.throttle.should_probe(key, failure.last_probe_boot_ns);
        }
    }
}

fn is_sticky(status: ReadStatus) -> bool {
    matches!(
        status,
        ReadStatus::Unsupported | ReadStatus::PermissionDenied
    )
}

/// Outcome of one file access plus the boot-clock interval around it.
#[derive(Debug)]
pub(crate) struct TimedRead<T> {
    pub result: io::Result<T>,
    pub started_boot_ns: u64,
    pub finished_boot_ns: u64,
}

pub(crate) fn timed<T>(
    clock: &dyn TimeProvider,
    read: impl FnOnce() -> io::Result<T>,
) -> TimedRead<T> {
    let started_boot_ns = clock.now_boot_ns();
    let result = read();
    let finished_boot_ns = clock.now_boot_ns().max(started_boot_ns);
    TimedRead {
        result,
        started_boot_ns,
        finished_boot_ns,
    }
}

/// Read status and bounded reason code of an I/O failure.
pub(crate) fn io_failure(error: &io::Error) -> (ReadStatus, String) {
    (read_status_of(error), reason_of(error))
}

/// Namespace identity such as `pid:[4026531836]` from a `/proc/self/ns/*`
/// link; odd or oversized targets are rejected rather than trusted.
pub(crate) fn namespace_link(files: &dyn SystemFiles, path: &str) -> Option<String> {
    let target = files.read_link(path).ok()?;
    let text = target.to_str()?;
    let clean = sanitize_display(text, MAX_NAMESPACE_BYTES);
    let well_formed = !clean.is_empty() && clean == text && !clean.contains('/');
    well_formed.then_some(clean)
}

/// Joins distinct reason codes into one bounded `error_code`.
pub(crate) fn joined_error_code(codes: &[String]) -> Option<String> {
    let mut unique: Vec<&str> = Vec::new();
    for code in codes {
        if !unique.contains(&code.as_str()) {
            unique.push(code);
        }
    }
    if unique.is_empty() {
        return None;
    }
    Some(truncate_utf8(&unique.join(","), MAX_ERROR_CODE_BYTES).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::clock::ms_to_ns;

    #[test]
    fn retain_and_forget_keep_the_gate_bounded() {
        let mut gate = ReprobeGate::new(60_000);
        for index in 0..100 {
            gate.record_failure(
                &format!("k{index}"),
                0,
                ReadStatus::PermissionDenied,
                "PERMISSION_DENIED",
            );
        }
        gate.retain(|key| key == "k1" || key == "k2");
        assert_eq!(gate.failing_len(), 2);
        assert!(gate.deferred("k1", ms_to_ns(1)).is_some());
        gate.forget("k1");
        assert!(gate.deferred("k1", ms_to_ns(2)).is_none());
        assert!(gate.deferred("k2", ms_to_ns(2)).is_some());
    }

    #[test]
    fn error_codes_are_deduplicated_and_bounded() {
        assert_eq!(joined_error_code(&[]), None);
        let codes = vec!["A".to_owned(), "B".to_owned(), "A".to_owned()];
        assert_eq!(joined_error_code(&codes).as_deref(), Some("A,B"));
        let long = vec!["X".repeat(300)];
        assert_eq!(joined_error_code(&long).unwrap().len(), 128);
    }
}
