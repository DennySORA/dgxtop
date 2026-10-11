//! Words of process actions: each action state's honest label ("signal
//! sent" is not "terminated") and the details of a pending confirmation,
//! which the never-clipped banner shows in every view (D-91).

use dgxtop_core::action::ActionState;
use dgxtop_core::view::ConfirmationView;

use super::age::format_countdown_ms;
use super::text::clean;

/// Keys of an active confirmation, shown right under the question.
pub(crate) const CONFIRM_HINT: &str = "y / Enter send · n / Esc cancel";

/// Width below which the confirmation uses its compact wording.
pub(crate) const COMPACT_CONFIRMATION_WIDTH: u16 = 60;

/// Human state words. `SignalSent` deliberately says nothing about exit.
pub const fn action_state_label(state: ActionState) -> &'static str {
    match state {
        ActionState::Requested => "requested",
        ActionState::Prepared => "awaiting confirmation",
        ActionState::Confirming => "confirming",
        ActionState::IntentPending => "recording intent",
        ActionState::IntentCommitted => "intent recorded",
        ActionState::Sending => "sending",
        ActionState::SignalSent => "signal sent (exit not observed)",
        ActionState::ExitObserved => "exit observed",
        ActionState::StillRunning => "still running",
        ActionState::Cancelled => "cancelled",
        ActionState::Expired => "expired",
        ActionState::Rejected => "rejected",
        ActionState::NotSent => "not sent",
        ActionState::Unknown => "unknown outcome",
    }
}

/// Confirmation details, most important first: question,
/// countdown/irreversibility, identity, warning. The confirmation token is
/// not part of the view and never shown.
pub(crate) fn confirmation_lines(
    confirmation: &ConfirmationView,
    age_offset_ms: u64,
    compact: bool,
) -> Vec<String> {
    let remaining = confirmation.expires_in_ms.saturating_sub(age_offset_ms);
    let countdown = if remaining == 0 {
        "expired".to_owned()
    } else {
        format!("expires in {}", format_countdown_ms(remaining))
    };
    let target = &confirmation.target;
    let mut lines = if compact {
        vec![
            format!(
                "{} pid {} ({})?",
                confirmation.signal.name(),
                target.pid,
                clean(&confirmation.basename)
            ),
            format!("{countdown} · cannot be undone"),
            format!(
                "uid {} · start {} · ns {}",
                confirmation.uid, target.start_time_ticks, target.pid_namespace
            ),
        ]
    } else {
        vec![
            format!(
                "Send {} to pid {} ({}) owned by uid {}?",
                confirmation.signal.name(),
                target.pid,
                clean(&confirmation.basename),
                confirmation.uid
            ),
            format!("{countdown} · this signal cannot be undone"),
            format!("start identity: {}", target.summary()),
        ]
    };
    if !confirmation.warning.is_empty() {
        lines.push(clean(&confirmation.warning));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;

    #[test]
    fn signal_sent_is_never_called_terminated() {
        let label = action_state_label(ActionState::SignalSent);
        assert!(label.contains("signal sent"));
        assert!(!label.contains("terminated"));
        assert!(!label.contains("killed"));
        for state in [
            ActionState::Requested,
            ActionState::Sending,
            ActionState::StillRunning,
            ActionState::Unknown,
        ] {
            assert!(!action_state_label(state).contains("terminated"));
        }
    }

    #[test]
    fn confirmation_lines_have_identity_and_countdown_but_no_token() {
        let actions = test_fixtures::actions_view(true);
        let confirmation = actions.confirmation.as_ref().unwrap();
        let lines = confirmation_lines(confirmation, 1_000, false);
        let text = lines.join("\n");
        assert!(text.contains("SIGTERM"));
        assert!(text.contains("pid 4242"));
        assert!(text.contains("uid 1000"));
        assert!(text.contains("python3"));
        assert!(text.contains("start 90100"));
        assert!(text.contains("expires in 8s"));
        assert!(text.contains("cannot be undone"));
        assert!(!text.contains(&test_fixtures::token().as_str().to_owned()));
        let expired = confirmation_lines(confirmation, 60_000, false).join(" ");
        assert!(expired.contains("expired"));
        let compact = confirmation_lines(confirmation, 0, true).join("\n");
        for needle in [
            "SIGTERM pid 4242 (python3)?",
            "uid 1000",
            "start 90100",
            "cannot be undone",
            "expires in 9s",
        ] {
            assert!(compact.contains(needle), "{needle}: {compact}");
        }
    }
}
