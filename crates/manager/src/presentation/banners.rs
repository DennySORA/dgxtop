//! Global banners ordered by priority: a pending confirmation first, then
//! errors (persistence failures, quarantined hosts, unknown action
//! outcomes), warnings (overload, conflicts, degraded hosts) and info.

use dgxtop_core::action::{PersistenceKind, PersistenceState, PersistenceStatus};
use dgxtop_core::canonical::ImmutableSnapshot;
use dgxtop_core::naming::adapters;
use dgxtop_core::quality::SelectionState;
use dgxtop_core::view::{
    ActionsView, Banner, BannerLevel, ConfirmationView, HostState, HostStatus, IntervalView,
    OverloadState,
};

use super::PresentationContext;
use super::labels::{bounded_list, host_state_text, label_text, note_text};

/// Banners kept per projection; the confirmation always sorts first.
pub(crate) const MAX_BANNERS: usize = 8;
const MAX_LISTED_PATHS: usize = 4;

/// Facts computed while building the module views.
pub(crate) struct BannerFacts<'a> {
    pub(crate) disabled_in_view: &'a [&'static str],
    pub(crate) conflict_count: usize,
}

pub(crate) fn build_banners(
    context: &PresentationContext<'_>,
    facts: &BannerFacts<'_>,
) -> Vec<Banner> {
    let mut banners = Vec::new();
    if let Some(confirmation) = &context.actions.confirmation {
        push(
            &mut banners,
            BannerLevel::Confirmation,
            confirmation_text(confirmation),
        );
    }
    persistence_banner(context.settings.status, &mut banners);
    persistence_banner(&context.actions.audit, &mut banners);
    if let Some(archive) = context.archive {
        persistence_banner(archive, &mut banners);
    }
    host_banners(
        context.hosts,
        context.settings.effective.sampling.gpu_enabled,
        &mut banners,
    );
    action_outcome_banner(&context.actions, &mut banners);
    overload_banner(
        context.overload,
        context.intervals,
        context.settings.effective.sampling.interval_ms,
        &mut banners,
    );
    if facts.conflict_count > 0 {
        push(
            &mut banners,
            BannerLevel::Warning,
            format!(
                "{} metric(s) in source conflict: values withheld (see Sources)",
                facts.conflict_count
            ),
        );
    }
    if !facts.disabled_in_view.is_empty() {
        push(
            &mut banners,
            BannerLevel::Info,
            format!(
                "module disabled: {} (enable in Settings)",
                facts.disabled_in_view.join(", ")
            ),
        );
    }
    if !context.settings.restart_pending.is_empty() {
        push(
            &mut banners,
            BannerLevel::Info,
            note_text(&format!(
                "restart required to apply: {}",
                bounded_list(context.settings.restart_pending, MAX_LISTED_PATHS)
            )),
        );
    }
    order_and_bound(banners)
}

/// Highest level first (stable within a level), at most [`MAX_BANNERS`].
pub(crate) fn order_and_bound(mut banners: Vec<Banner>) -> Vec<Banner> {
    banners.sort_by(|left, right| right.level.cmp(&left.level));
    banners.truncate(MAX_BANNERS);
    banners
}

pub(crate) fn push(banners: &mut Vec<Banner>, level: BannerLevel, text: String) {
    banners.push(Banner { level, text });
}

/// Safety text is never truncated; only the untrusted basename is sanitized.
fn confirmation_text(confirmation: &ConfirmationView) -> String {
    format!(
        "Confirm {} to {} (pid {})? y = confirm · n/Esc = cancel · expires in {}s",
        confirmation.signal.name(),
        label_text(&confirmation.basename),
        confirmation.target.pid,
        confirmation.expires_in_ms.div_ceil(1000)
    )
}

fn persistence_banner(status: &PersistenceStatus, banners: &mut Vec<Banner>) {
    let detail = status
        .detail
        .as_deref()
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default();
    let (level, text) = match (status.state, status.kind) {
        (PersistenceState::Failed, PersistenceKind::Config) => (
            BannerLevel::Error,
            format!("settings could not be saved{detail}; previous values stay active"),
        ),
        (PersistenceState::Failed, PersistenceKind::Audit) => (
            BannerLevel::Error,
            format!("action audit unavailable{detail}; process signals are refused"),
        ),
        (PersistenceState::Failed, PersistenceKind::Archive) => (
            BannerLevel::Error,
            format!("history archive failed{detail}; only in-memory history is kept"),
        ),
        (PersistenceState::ReadOnlyDiagnostic, kind) => (
            BannerLevel::Error,
            format!(
                "{} store has an unknown schema; read-only{detail}",
                kind_name(kind)
            ),
        ),
        (PersistenceState::Degraded, kind) => (
            BannerLevel::Warning,
            format!("{} persistence degraded{detail}", kind_name(kind)),
        ),
        (PersistenceState::Healthy | PersistenceState::Disabled, _) => return,
    };
    push(banners, level, note_text(&text));
}

const fn kind_name(kind: PersistenceKind) -> &'static str {
    match kind {
        PersistenceKind::Config => "settings",
        PersistenceKind::Audit => "action audit",
        PersistenceKind::Archive => "history archive",
    }
}

/// Quarantined hosts need intervention (error); degraded, circuit-open and
/// unavailable hosts are warnings. NVML is skipped when GPU sampling is off.
fn host_banners(hosts: &[HostStatus], gpu_enabled: bool, banners: &mut Vec<Banner>) {
    for host in hosts {
        let level = match host.state {
            HostState::Quarantined => BannerLevel::Error,
            HostState::Degraded | HostState::CircuitOpen | HostState::Unavailable => {
                BannerLevel::Warning
            }
            HostState::Starting | HostState::Healthy | HostState::Disabled => continue,
        };
        if host.adapter_id == adapters::NVML && !gpu_enabled {
            continue;
        }
        let subject = format!("adapter {}", label_text(&host.adapter_id));
        push(banners, level, host_state_text(&subject, host));
    }
}

fn action_outcome_banner(actions: &ActionsView, banners: &mut Vec<Banner>) {
    if actions.unknown_outcomes > 0 {
        push(
            banners,
            BannerLevel::Error,
            format!(
                "{} process action outcome(s) unknown; never retried automatically (see Actions)",
                actions.unknown_outcomes
            ),
        );
    }
}

/// The slowest effective period is what the user must know about.
fn overload_banner(
    overload: OverloadState,
    intervals: &[IntervalView],
    configured_ms: u32,
    banners: &mut Vec<Banner>,
) {
    if overload == OverloadState::Normal {
        return;
    }
    let effective_ms = intervals
        .iter()
        .map(|interval| interval.effective_ms)
        .max()
        .unwrap_or(configured_ms);
    push(
        banners,
        BannerLevel::Warning,
        format!(
            "overload {}: sampling every {effective_ms} ms (configured {configured_ms} ms)",
            overload.label()
        ),
    );
}

/// Series whose independent sources currently disagree.
pub(crate) fn count_conflicts(snapshot: &ImmutableSnapshot) -> usize {
    snapshot
        .domains
        .values()
        .flat_map(|domain| domain.samples.values())
        .filter(|entry| entry.sample.selection_state == SelectionState::Conflict)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::testkit::{Fixture, host_status, process_key};
    use dgxtop_core::action::Signal;
    use std::sync::Arc;

    fn confirmation() -> ConfirmationView {
        ConfirmationView {
            request_id: uuid::Uuid::nil(),
            target: process_key(4242, 90_100),
            uid: 1000,
            basename: "python3\u{1b}[31m".to_owned(),
            signal: Signal::Sigterm,
            expires_in_ms: 8_200,
            warning: "SIGTERM cannot be undone".to_owned(),
        }
    }

    fn build(fixture: &Fixture, facts: &BannerFacts<'_>) -> Vec<Banner> {
        build_banners(&fixture.context(), facts)
    }

    const NO_FACTS: BannerFacts<'static> = BannerFacts {
        disabled_in_view: &[],
        conflict_count: 0,
    };

    #[test]
    fn confirmation_sorts_first_then_errors_warnings_info() {
        let mut fixture = Fixture::new();
        let mut actions = (*fixture.actions).clone();
        actions.confirmation = Some(confirmation());
        actions.unknown_outcomes = 2;
        fixture.actions = Arc::new(actions);
        fixture.overload = OverloadState::Throttled;
        fixture.intervals = vec![IntervalView {
            adapter_id: "procfs".into(),
            read_group: "cpu".into(),
            configured_ms: 1000,
            effective_ms: 2000,
            reason: "overload".into(),
        }];
        fixture.hosts = vec![
            host_status("hwmon", HostState::CircuitOpen, Some("3 timeouts")),
            host_status("nvml", HostState::Quarantined, Some("child not reaped")),
        ];
        fixture.restart_pending = vec!["queues".into()];
        let facts = BannerFacts {
            disabled_in_view: &["disk"],
            conflict_count: 1,
        };
        let banners = build(&fixture, &facts);
        let levels: Vec<BannerLevel> = banners.iter().map(|banner| banner.level).collect();
        assert_eq!(
            levels,
            vec![
                BannerLevel::Confirmation,
                BannerLevel::Error,
                BannerLevel::Error,
                BannerLevel::Warning,
                BannerLevel::Warning,
                BannerLevel::Warning,
                BannerLevel::Info,
                BannerLevel::Info,
            ]
        );
        assert_eq!(
            banners[0].text,
            "Confirm SIGTERM to python3 (pid 4242)? y = confirm · n/Esc = cancel · expires in 9s"
        );
        assert_eq!(
            banners[1].text,
            "adapter nvml quarantined (requires intervention): child not reaped"
        );
        assert!(
            banners[2]
                .text
                .starts_with("2 process action outcome(s) unknown")
        );
        assert_eq!(banners[3].text, "adapter hwmon circuit open: 3 timeouts");
        assert_eq!(
            banners[4].text,
            "overload throttled: sampling every 2000 ms (configured 1000 ms)"
        );
        assert!(
            banners[5]
                .text
                .starts_with("1 metric(s) in source conflict")
        );
        assert_eq!(
            banners[6].text,
            "module disabled: disk (enable in Settings)"
        );
        assert_eq!(banners[7].text, "restart required to apply: queues");
    }

    #[test]
    fn persistence_failures_are_errors_and_degradation_warnings() {
        let mut fixture = Fixture::new();
        fixture.config_status.state = PersistenceState::Failed;
        fixture.config_status.detail = Some("disk full".into());
        let mut actions = (*fixture.actions).clone();
        actions.audit.state = PersistenceState::Degraded;
        fixture.actions = Arc::new(actions);
        let banners = build(&fixture, &NO_FACTS);
        assert_eq!(banners.len(), 2);
        assert_eq!(banners[0].level, BannerLevel::Error);
        assert_eq!(
            banners[0].text,
            "settings could not be saved: disk full; previous values stay active"
        );
        assert_eq!(banners[1].level, BannerLevel::Warning);
        assert_eq!(banners[1].text, "action audit persistence degraded");
    }

    #[test]
    fn a_failed_history_archive_is_visible_and_live_monitoring_continues() {
        let mut fixture = Fixture::new();
        assert!(build(&fixture, &NO_FACTS).is_empty());
        fixture.archive_status = Some(PersistenceStatus {
            state: PersistenceState::Failed,
            detail: Some("free disk below 64 MiB".into()),
            ..PersistenceStatus::healthy(PersistenceKind::Archive)
        });
        let banners = build(&fixture, &NO_FACTS);
        assert_eq!(banners.len(), 1);
        assert_eq!(banners[0].level, BannerLevel::Error);
        assert_eq!(
            banners[0].text,
            "history archive failed: free disk below 64 MiB; only in-memory history is kept"
        );
    }

    #[test]
    fn nvml_warning_respects_gpu_setting_and_banners_are_bounded() {
        let mut fixture = Fixture::new();
        fixture.hosts = vec![host_status(
            "nvml",
            HostState::Unavailable,
            Some("no driver"),
        )];
        assert_eq!(
            build(&fixture, &NO_FACTS)[0].text,
            "adapter nvml unavailable: no driver"
        );
        fixture.effective.sampling.gpu_enabled = false;
        assert!(build(&fixture, &NO_FACTS).is_empty());
        fixture.hosts = (0..20)
            .map(|index| host_status(&format!("host{index}"), HostState::Degraded, None))
            .collect();
        assert_eq!(build(&fixture, &NO_FACTS).len(), MAX_BANNERS);
    }
}
