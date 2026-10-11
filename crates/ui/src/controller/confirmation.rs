//! ConfirmationInput: the one-time confirmation token lives only here, in
//! controller memory. It is never rendered, logged or exposed through the
//! UI state; `y`/`Enter` only act while a token is active, and a token that
//! arrives while the user is typing elsewhere is cancelled, never activated.

use std::time::Duration;

use dgxtop_core::action::{ConfirmationToken, Signal};
use dgxtop_core::commands::{CommandResult, CommandStatus};
use dgxtop_core::identity::ProcessKey;

/// Upper bound of a confirmation (config maximum TTL is 30 s); the Manager
/// enforces the real TTL, this only clears stale local state.
pub const MAX_CONFIRMATION_TTL: Duration = Duration::from_secs(30);
/// A prepare request without any token-bearing result is dropped after this.
const AWAIT_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, Eq)]
enum State {
    Idle,
    Awaiting {
        request_id: u128,
        target: ProcessKey,
        signal: Signal,
        since: Duration,
    },
    Active {
        token: ConfirmationToken,
        request_id: u128,
        target: ProcessKey,
        signal: Signal,
        expires_at: Duration,
    },
}

/// What a prepare result did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareOutcome {
    /// Not our prepare request.
    Unrelated,
    /// Still waiting (accepted / in progress without a token).
    Waiting,
    /// Confirm mode entered.
    Activated,
    /// A token arrived while another input mode was active: it must be
    /// cancelled at once (the caller submits the returned CancelSignal).
    MustCancel(ConfirmationToken),
    /// The prepare failed; no confirmation exists.
    Failed,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConfirmationInput {
    state: State,
}

impl Default for ConfirmationInput {
    fn default() -> Self {
        Self { state: State::Idle }
    }
}

/// Tokens are secrets: the Debug output never contains one.
impl std::fmt::Debug for ConfirmationInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            State::Idle => "idle",
            State::Awaiting { .. } => "awaiting token",
            State::Active { .. } => "active (token redacted)",
        };
        formatter
            .debug_struct("ConfirmationInput")
            .field("state", &state)
            .finish()
    }
}

/// Redacted view of an active confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSummary {
    pub target: ProcessKey,
    pub signal: Signal,
    pub remaining: Duration,
}

impl ConfirmationInput {
    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    pub fn is_awaiting(&self) -> bool {
        matches!(self.state, State::Awaiting { .. })
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, State::Active { .. })
    }

    /// A PrepareSignal was accepted by the session queue.
    pub fn begin(&mut self, request_id: u128, target: ProcessKey, signal: Signal, now: Duration) {
        self.state = State::Awaiting {
            request_id,
            target,
            signal,
            since: now,
        };
    }

    /// Handles a result that may carry the token. `may_activate` is false
    /// while the user is in filter or help input.
    pub fn on_prepare_result(
        &mut self,
        result: &CommandResult,
        may_activate: bool,
        expires_at: Duration,
    ) -> PrepareOutcome {
        let State::Awaiting {
            request_id,
            target,
            signal,
            ..
        } = &self.state
        else {
            return PrepareOutcome::Unrelated;
        };
        if result.request_id.as_u128() != *request_id {
            return PrepareOutcome::Unrelated;
        }
        if let Some(token) = result.confirmation_token.clone() {
            if !may_activate {
                self.state = State::Idle;
                return PrepareOutcome::MustCancel(token);
            }
            self.state = State::Active {
                token,
                request_id: *request_id,
                target: target.clone(),
                signal: *signal,
                expires_at,
            };
            return PrepareOutcome::Activated;
        }
        match result.status {
            CommandStatus::Accepted | CommandStatus::InProgress => PrepareOutcome::Waiting,
            CommandStatus::Completed | CommandStatus::Rejected | CommandStatus::Unknown => {
                self.state = State::Idle;
                PrepareOutcome::Failed
            }
        }
    }

    /// The token for a confirm/cancel submission (the state stays active
    /// until [`Self::finish`] reports the submission was accepted).
    pub fn token(&self) -> Option<(ConfirmationToken, ProcessKey)> {
        match &self.state {
            State::Active { token, target, .. } => Some((token.clone(), target.clone())),
            _ => None,
        }
    }

    /// The confirm/cancel command was accepted: the token is consumed.
    pub fn finish(&mut self) {
        self.state = State::Idle;
    }

    /// Clears expired state; returns true when an active token expired.
    pub fn expire(&mut self, now: Duration) -> bool {
        match &self.state {
            State::Active { expires_at, .. } if now >= *expires_at => {
                self.state = State::Idle;
                true
            }
            State::Awaiting { since, .. } if now.saturating_sub(*since) > AWAIT_TTL => {
                self.state = State::Idle;
                false
            }
            _ => false,
        }
    }

    pub fn summary(&self, now: Duration) -> Option<ActiveSummary> {
        match &self.state {
            State::Active {
                target,
                signal,
                expires_at,
                ..
            } => Some(ActiveSummary {
                target: target.clone(),
                signal: *signal,
                remaining: expires_at.saturating_sub(now),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{TEST_REQUEST_ID, process_key, token};

    fn prepare_result(with_token: bool, status: CommandStatus) -> CommandResult {
        let mut result = CommandResult::completed(TEST_REQUEST_ID.try_into().unwrap(), "prepared");
        result.status = status;
        if with_token {
            result.confirmation_token = Some(token());
        }
        result
    }

    fn request_id() -> u128 {
        let id: dgxtop_core::commands::CommandResult =
            prepare_result(false, CommandStatus::Accepted);
        id.request_id.as_u128()
    }

    #[test]
    fn a_token_activates_confirm_mode_and_is_consumed_once() {
        let mut input = ConfirmationInput::default();
        input.begin(
            request_id(),
            process_key(4242, 1),
            Signal::Sigterm,
            Duration::ZERO,
        );
        assert!(input.is_awaiting());
        assert_eq!(
            input.on_prepare_result(
                &prepare_result(false, CommandStatus::Accepted),
                true,
                Duration::from_secs(10)
            ),
            PrepareOutcome::Waiting
        );
        assert_eq!(
            input.on_prepare_result(
                &prepare_result(true, CommandStatus::Completed),
                true,
                Duration::from_secs(10)
            ),
            PrepareOutcome::Activated
        );
        assert!(input.is_active());
        let (token, target) = input.token().unwrap();
        assert_eq!(token, crate::test_fixtures::token());
        assert_eq!(target, process_key(4242, 1));
        input.finish();
        assert!(input.token().is_none());
    }

    #[test]
    fn unrelated_and_failed_results() {
        let mut input = ConfirmationInput::default();
        assert_eq!(
            input.on_prepare_result(
                &prepare_result(true, CommandStatus::Completed),
                true,
                Duration::ZERO
            ),
            PrepareOutcome::Unrelated
        );
        input.begin(
            request_id().wrapping_add(1),
            process_key(1, 1),
            Signal::Sigterm,
            Duration::ZERO,
        );
        assert_eq!(
            input.on_prepare_result(
                &prepare_result(true, CommandStatus::Completed),
                true,
                Duration::ZERO
            ),
            PrepareOutcome::Unrelated
        );
        input.begin(
            request_id(),
            process_key(1, 1),
            Signal::Sigterm,
            Duration::ZERO,
        );
        assert_eq!(
            input.on_prepare_result(
                &prepare_result(false, CommandStatus::Rejected),
                true,
                Duration::ZERO
            ),
            PrepareOutcome::Failed
        );
        assert!(input.is_idle());
    }

    #[test]
    fn active_tokens_expire_and_waiting_requests_time_out() {
        let mut input = ConfirmationInput::default();
        input.begin(
            request_id(),
            process_key(1, 1),
            Signal::Sigterm,
            Duration::ZERO,
        );
        input.on_prepare_result(
            &prepare_result(true, CommandStatus::Completed),
            true,
            Duration::from_secs(9),
        );
        assert_eq!(
            input.summary(Duration::from_secs(4)).unwrap().remaining,
            Duration::from_secs(5)
        );
        assert!(!input.expire(Duration::from_secs(8)));
        assert!(input.expire(Duration::from_secs(9)));
        assert!(input.is_idle());
        input.begin(
            request_id(),
            process_key(1, 1),
            Signal::Sigterm,
            Duration::ZERO,
        );
        assert!(!input.expire(Duration::from_secs(31)));
        assert!(input.is_idle());
    }
}
