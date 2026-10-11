//! Accepted commands awaiting their `CommandResult`, and the translation of
//! results into user-visible notices. Results arrive independently of
//! projections; nothing here is ever retried automatically.

use std::collections::VecDeque;
use std::time::Duration;

use dgxtop_core::action::ActionState;
use dgxtop_core::commands::{CommandResult, CommandStatus};
use dgxtop_core::errors::{ErrorCode, ResultCode};
use dgxtop_core::identity::{ModuleId, ProcessKey};
use dgxtop_core::view::BannerLevel;

use crate::format::action_state_label;
use crate::format::{clean, format_interval_ms};

/// Bound of tracked requests (oldest are forgotten first).
pub const MAX_PENDING_REQUESTS: usize = 64;
/// A request without any result for this long is forgotten.
const REQUEST_TTL: Duration = Duration::from_secs(60);

/// What an accepted request was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    ViewSession,
    Prepare { target: ProcessKey },
    Confirm { target: ProcessKey },
    Cancel,
    Interval { interval_ms: u32 },
    ModuleToggle { module_id: ModuleId, enabled: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRequest {
    id: u128,
    kind: RequestKind,
    sent_at: Duration,
}

/// Bounded map from request id to kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingRequests {
    entries: VecDeque<PendingRequest>,
}

impl PendingRequests {
    pub fn track(&mut self, id: u128, kind: RequestKind, now: Duration) {
        if self.entries.len() == MAX_PENDING_REQUESTS {
            self.entries.pop_front();
        }
        self.entries.push_back(PendingRequest {
            id,
            kind,
            sent_at: now,
        });
    }

    /// Kind of a tracked request; terminal results forget it.
    pub fn resolve(&mut self, id: u128, terminal: bool) -> Option<RequestKind> {
        let position = self.entries.iter().position(|entry| entry.id == id)?;
        if terminal {
            self.entries.remove(position).map(|entry| entry.kind)
        } else {
            self.entries.get(position).map(|entry| entry.kind.clone())
        }
    }

    /// Whether a SetSamplingInterval is still unresolved (a second one
    /// would only conflict on the stale config revision).
    pub fn has_pending_interval(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.kind, RequestKind::Interval { .. }))
    }

    pub fn expire(&mut self, now: Duration) {
        self.entries
            .retain(|entry| now.saturating_sub(entry.sent_at) < REQUEST_TTL);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Whether no further result is expected for the request.
pub const fn is_terminal(status: CommandStatus) -> bool {
    matches!(
        status,
        CommandStatus::Completed | CommandStatus::Rejected | CommandStatus::Unknown
    )
}

fn code_text(code: ResultCode) -> &'static str {
    code.as_str()
}

fn failure(result: &CommandResult) -> String {
    format!("{}: {}", code_text(result.code), clean(&result.message))
}

/// User-visible notice for a result, if any.
pub fn notice_for(
    kind: Option<&RequestKind>,
    result: &CommandResult,
) -> Option<(BannerLevel, String)> {
    match kind {
        Some(RequestKind::ViewSession) | None => view_session_notice(result),
        Some(RequestKind::Prepare { .. }) => prepare_notice(result),
        Some(RequestKind::Confirm { target }) => confirm_notice(target, result),
        Some(RequestKind::Cancel) => cancel_notice(result),
        Some(RequestKind::Interval { interval_ms }) => interval_notice(*interval_ms, result),
        Some(RequestKind::ModuleToggle { module_id, enabled }) => {
            module_notice(module_id, *enabled, result)
        }
    }
}

fn view_session_notice(result: &CommandResult) -> Option<(BannerLevel, String)> {
    match (result.status, result.code) {
        (CommandStatus::Rejected, ResultCode::Error(ErrorCode::StaleView)) => Some((
            BannerLevel::Warning,
            "the list changed before the selection arrived; select again".to_owned(),
        )),
        (CommandStatus::Rejected | CommandStatus::Unknown, _) => Some((
            BannerLevel::Warning,
            format!("view change not applied: {}", failure(result)),
        )),
        _ => None,
    }
}

fn prepare_notice(result: &CommandResult) -> Option<(BannerLevel, String)> {
    match result.status {
        CommandStatus::Rejected | CommandStatus::Unknown => Some((
            BannerLevel::Error,
            format!("terminate refused: {}", failure(result)),
        )),
        _ => None,
    }
}

fn confirm_notice(target: &ProcessKey, result: &CommandResult) -> Option<(BannerLevel, String)> {
    let pid = target.pid;
    match (result.status, result.action_state) {
        (CommandStatus::Rejected, _) => Some((
            BannerLevel::Error,
            format!("signal not sent to pid {pid}: {}", failure(result)),
        )),
        (CommandStatus::Unknown, _) | (_, Some(ActionState::Unknown)) => Some((
            BannerLevel::Error,
            format!("outcome for pid {pid} is unknown; it is not retried automatically"),
        )),
        (_, Some(state)) => {
            let level = if matches!(state, ActionState::StillRunning) {
                BannerLevel::Warning
            } else {
                BannerLevel::Info
            };
            Some((level, format!("pid {pid}: {}", action_state_label(state))))
        }
        (CommandStatus::Accepted | CommandStatus::InProgress, None) => None,
        (CommandStatus::Completed, None) => Some((
            BannerLevel::Info,
            format!("pid {pid}: {}", clean(&result.message)),
        )),
    }
}

fn cancel_notice(result: &CommandResult) -> Option<(BannerLevel, String)> {
    let too_late = result
        .action_state
        .is_some_and(ActionState::is_past_point_of_no_return)
        || result.status == CommandStatus::InProgress;
    if too_late {
        return Some((
            BannerLevel::Warning,
            "too late to cancel: the signal is already being sent".to_owned(),
        ));
    }
    match result.status {
        CommandStatus::Completed => Some((
            BannerLevel::Info,
            "confirmation cancelled; nothing was sent".to_owned(),
        )),
        CommandStatus::Rejected | CommandStatus::Unknown => {
            Some((BannerLevel::Warning, format!("cancel: {}", failure(result))))
        }
        CommandStatus::Accepted | CommandStatus::InProgress => None,
    }
}

fn interval_notice(interval_ms: u32, result: &CommandResult) -> Option<(BannerLevel, String)> {
    match (result.status, result.code) {
        (CommandStatus::Completed, _) => {
            let effective = result
                .effective_interval_ms
                .map_or_else(|| "not reported".to_owned(), format_interval_ms);
            let revision = result
                .config_revision
                .map_or_else(String::new, |revision| format!(" (revision {revision})"));
            Some((
                BannerLevel::Info,
                format!(
                    "sampling interval: configured {}, effective {effective}{revision}",
                    format_interval_ms(interval_ms)
                ),
            ))
        }
        (CommandStatus::Rejected, ResultCode::Error(ErrorCode::Conflict)) => Some((
            BannerLevel::Warning,
            "settings changed elsewhere (revision conflict); press +/- again".to_owned(),
        )),
        (CommandStatus::Rejected | CommandStatus::Unknown, _) if result.commit_uncertain() => {
            Some((
                BannerLevel::Warning,
                uncertain_notice("interval change", result),
            ))
        }
        (CommandStatus::Rejected | CommandStatus::Unknown, _) => Some((
            BannerLevel::Error,
            format!("interval not changed: {}", failure(result)),
        )),
        (CommandStatus::Accepted | CommandStatus::InProgress, _) => Some((
            BannerLevel::Info,
            format!(
                "interval change to {} pending",
                format_interval_ms(interval_ms)
            ),
        )),
    }
}

/// An uncertain persistent write: what is known about the on-disk settings.
fn uncertain_notice(change: &str, result: &CommandResult) -> String {
    match result.config_revision {
        Some(revision) => format!(
            "{change} outcome uncertain; the on-disk settings (revision {revision}) were adopted: {}",
            failure(result)
        ),
        None => format!(
            "{change} outcome uncertain; the on-disk settings are unknown: {}",
            failure(result)
        ),
    }
}

fn module_notice(
    module_id: &ModuleId,
    enabled: bool,
    result: &CommandResult,
) -> Option<(BannerLevel, String)> {
    let action = if enabled { "enabled" } else { "disabled" };
    if matches!(
        result.status,
        CommandStatus::Rejected | CommandStatus::Unknown
    ) && result.commit_uncertain()
    {
        let change = format!("module {} change", module_id.as_str());
        return Some((BannerLevel::Warning, uncertain_notice(&change, result)));
    }
    match result.status {
        CommandStatus::Completed => Some((
            BannerLevel::Info,
            format!("module {} {action}", module_id.as_str()),
        )),
        CommandStatus::Rejected | CommandStatus::Unknown => Some((
            BannerLevel::Error,
            format!(
                "module {} not {action}: {}",
                module_id.as_str(),
                failure(result)
            ),
        )),
        CommandStatus::Accepted | CommandStatus::InProgress => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{TEST_REQUEST_ID, process_key};

    fn result(status: CommandStatus, code: ResultCode) -> CommandResult {
        let mut result = CommandResult::completed(TEST_REQUEST_ID.try_into().unwrap(), "done");
        result.status = status;
        result.code = code;
        result
    }

    #[test]
    fn pending_requests_are_bounded_and_expire() {
        let mut pending = PendingRequests::default();
        for id in 0..(MAX_PENDING_REQUESTS as u128 + 5) {
            pending.track(id, RequestKind::ViewSession, Duration::ZERO);
        }
        assert_eq!(pending.len(), MAX_PENDING_REQUESTS);
        assert!(pending.resolve(0, true).is_none(), "oldest were evicted");
        assert_eq!(pending.resolve(10, false), Some(RequestKind::ViewSession));
        assert_eq!(pending.resolve(10, true), Some(RequestKind::ViewSession));
        assert!(pending.resolve(10, true).is_none());
        pending.expire(Duration::from_secs(61));
        assert!(pending.is_empty());
    }

    #[test]
    fn signal_sent_is_reported_as_sent_not_terminated() {
        let mut sent = result(CommandStatus::Completed, ResultCode::OK);
        sent.action_state = Some(ActionState::SignalSent);
        let (level, text) = notice_for(
            Some(&RequestKind::Confirm {
                target: process_key(4242, 1),
            }),
            &sent,
        )
        .unwrap();
        assert_eq!(level, BannerLevel::Info);
        assert!(text.contains("signal sent"), "{text}");
        assert!(!text.contains("terminated"));
    }

    #[test]
    fn unknown_outcomes_and_rejections_are_errors() {
        let target = RequestKind::Confirm {
            target: process_key(1, 1),
        };
        let unknown = result(
            CommandStatus::Unknown,
            ResultCode::from(ErrorCode::UnknownOutcome),
        );
        assert_eq!(
            notice_for(Some(&target), &unknown).unwrap().0,
            BannerLevel::Error
        );
        let expired = result(
            CommandStatus::Rejected,
            ResultCode::from(ErrorCode::ConfirmationExpired),
        );
        let (_, text) = notice_for(Some(&target), &expired).unwrap();
        assert!(text.contains("ConfirmationExpired"), "{text}");
    }

    #[test]
    fn cancel_reports_too_late_when_past_the_point_of_no_return() {
        let mut late = result(CommandStatus::InProgress, ResultCode::OK);
        late.action_state = Some(ActionState::Sending);
        assert!(
            notice_for(Some(&RequestKind::Cancel), &late)
                .unwrap()
                .1
                .contains("too late")
        );
        let done = result(CommandStatus::Completed, ResultCode::OK);
        assert!(
            notice_for(Some(&RequestKind::Cancel), &done)
                .unwrap()
                .1
                .contains("nothing was sent")
        );
    }

    #[test]
    fn an_uncertain_commit_is_never_reported_as_unchanged() {
        let detail =
            dgxtop_core::errors::ErrorDetail::new(ErrorCode::PersistFailed, "dir fsync failed")
                .uncertain();
        let mut adopted = CommandResult::from_error(TEST_REQUEST_ID.try_into().unwrap(), &detail);
        adopted.config_revision = Some(2);
        let (level, text) =
            notice_for(Some(&RequestKind::Interval { interval_ms: 500 }), &adopted).unwrap();
        assert_eq!(level, BannerLevel::Warning);
        assert!(!text.contains("not changed"), "{text}");
        assert!(
            text.contains("uncertain") && text.contains("revision 2"),
            "{text}"
        );
        adopted.config_revision = None;
        let (_, unknown) =
            notice_for(Some(&RequestKind::Interval { interval_ms: 500 }), &adopted).unwrap();
        assert!(unknown.contains("unknown"), "{unknown}");
        assert!(!unknown.contains("not changed"), "{unknown}");
    }

    #[test]
    fn stale_view_rejections_ask_to_reselect() {
        let stale = result(
            CommandStatus::Rejected,
            ResultCode::from(ErrorCode::StaleView),
        );
        let (_, text) = notice_for(Some(&RequestKind::ViewSession), &stale).unwrap();
        assert!(text.contains("select again"));
        assert!(
            notice_for(
                Some(&RequestKind::ViewSession),
                &result(CommandStatus::Completed, ResultCode::OK)
            )
            .is_none()
        );
    }
}
