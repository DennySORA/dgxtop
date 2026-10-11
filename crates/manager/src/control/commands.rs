//! Command routing (CommandPort). Semantic validation lives in the owning
//! services; this module checks idempotency, records the result route and
//! maps the typed command to its service. Every accepted command gets
//! exactly one result.

use dgxtop_core::action::Signal;
use dgxtop_core::commands::{CommandBody, CommandEnvelope, CommandResult, ViewKind};
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::{ProcessKey, SessionId};
use uuid::Uuid;

use super::{ControlCore, Route};
use crate::config::{ConfigDecision, ConfigRequest};
use crate::effects::Effects;
use crate::process::{Now, PrepareRequest, TokenCommand};
use crate::results::CacheLookup;
use crate::session::QueuedCommand;
use crate::views::{ViewContext, ViewUpdate};

impl ControlCore {
    pub(crate) fn on_command(&mut self, queued: QueuedCommand) {
        let QueuedCommand {
            envelope,
            accepted_boot_ns,
            _permit,
        } = queued;
        if !self.is_running() {
            self.reject_for_shutdown(&envelope);
            return;
        }
        if !self.sessions.contains(&envelope.session_id) {
            // The session closed while the command was queued: its authority
            // ended with it, so nothing takes effect.
            let result = CommandResult::rejected(
                envelope.request_id,
                ErrorCode::Unavailable,
                "session closed",
            );
            self.count_rejection(&result);
            self.deliver(&envelope.session_id, result);
            return;
        }
        let now = self.now();
        let request_id = envelope.request_id;
        let digest = envelope.payload_digest();
        match self.results.lookup(request_id, &digest, now.boot_ns) {
            CacheLookup::Miss => {}
            CacheLookup::Completed(result) => {
                self.deliver(&envelope.session_id, result);
                return;
            }
            CacheLookup::InProgress => {
                let result = CommandResult::in_progress(request_id, "request already in progress");
                self.deliver(&envelope.session_id, result);
                return;
            }
            CacheLookup::Conflict => {
                let result = CommandResult::rejected(
                    request_id,
                    ErrorCode::Conflict,
                    "request_id reused with a different payload",
                );
                self.count_rejection(&result);
                self.deliver(&envelope.session_id, result);
                return;
            }
        }
        self.results.begin(request_id, digest.clone());
        self.routes.insert(
            request_id,
            Route {
                session_id: envelope.session_id.clone(),
                accepted_ns: accepted_boot_ns,
                command: envelope.body.name(),
                notify: !envelope.body.is_coalescible_navigation(),
            },
        );
        self.dispatch_command(envelope, digest, now);
    }

    /// Answers a command accepted before shutdown started.
    pub(super) fn reject_for_shutdown(&mut self, envelope: &CommandEnvelope) {
        self.rejected_commands = self.rejected_commands.saturating_add(1);
        let result = CommandResult::rejected(
            envelope.request_id,
            ErrorCode::Unavailable,
            "dgxtop is shutting down",
        );
        self.count_rejection(&result);
        self.deliver(&envelope.session_id, result);
    }

    fn dispatch_command(&mut self, envelope: CommandEnvelope, digest: String, now: Now) {
        let request_id = envelope.request_id;
        let session_id = envelope.session_id;
        match envelope.body {
            CommandBody::SetSamplingInterval {
                interval_ms,
                expected_config_revision,
            } => self.submit_config(
                request_id,
                ConfigRequest::SetInterval {
                    interval_ms,
                    expected_revision: expected_config_revision,
                },
                now,
            ),
            CommandBody::ReplaceConfig {
                expected_config_revision,
                candidate,
            } => self.submit_config(
                request_id,
                ConfigRequest::Replace {
                    expected_revision: expected_config_revision,
                    candidate,
                },
                now,
            ),
            CommandBody::SetModuleEnabled {
                module_id,
                enabled,
                expected_config_revision,
            } => self.submit_config(
                request_id,
                ConfigRequest::SetModule {
                    module_id,
                    enabled,
                    expected_revision: expected_config_revision,
                },
                now,
            ),
            CommandBody::UpdateViewSession {
                view,
                selected_entity_id,
                selected_process,
                filter,
                sort,
                ascending,
                frame_revision,
            } => self.update_view(
                request_id,
                &session_id,
                ViewUpdate {
                    view,
                    selected_entity_id,
                    selected_process,
                    filter,
                    sort,
                    ascending,
                    frame_revision,
                },
                now,
            ),
            CommandBody::PrepareSignal { target, signal } => {
                self.prepare_signal(request_id, &session_id, target, signal, digest, now);
            }
            CommandBody::ConfirmSignal { confirmation_token } => {
                let mut fx = Effects::default();
                self.process.confirm(
                    TokenCommand {
                        request_id,
                        session_id: &session_id,
                        token: &confirmation_token,
                        payload_sha256: digest,
                    },
                    now,
                    &mut fx,
                );
                self.apply(fx);
            }
            CommandBody::CancelSignal { confirmation_token } => {
                let mut fx = Effects::default();
                self.process.cancel(
                    TokenCommand {
                        request_id,
                        session_id: &session_id,
                        token: &confirmation_token,
                        payload_sha256: digest,
                    },
                    now,
                    &mut fx,
                );
                self.apply(fx);
            }
            // Frontends send Shutdown out of band; a queued one still works.
            CommandBody::Shutdown { reason } => {
                self.shutdown.request(&reason);
                self.finish(CommandResult::completed(request_id, "shutdown started"));
            }
        }
    }

    fn submit_config(&mut self, request_id: Uuid, request: ConfigRequest, now: Now) {
        let catalog = self.catalog.snapshot();
        match self
            .config
            .submit(request_id, request, &catalog, now.boot_ns)
        {
            ConfigDecision::Reply(result) => self.finish(result),
            // The settings view shows the pending change until the receipt.
            ConfigDecision::Pending => self.sessions.mark_all_dirty(),
        }
    }

    fn update_view(
        &mut self,
        request_id: Uuid,
        session_id: &SessionId,
        update: ViewUpdate,
        now: Now,
    ) {
        // The run the Process History tab opened (D-76).
        let opened = (update.view == ViewKind::History)
            .then(|| update.selected_process.clone())
            .flatten();
        let snapshot = self.ingestion.snapshots().latest();
        let catalog = self.catalog.snapshot();
        let context = ViewContext {
            host_id: &self.host_id,
            boot_id: &self.boot_id,
            table: &snapshot.process_table,
            catalog: &catalog,
        };
        match self.sessions.update(session_id, update, &context) {
            Ok(changed) => {
                if changed {
                    self.sync_history(session_id, now.boot_ns);
                }
                if let Some(key) = &opened {
                    self.process_history.open(key);
                }
                self.finish(CommandResult::completed(request_id, "view updated"));
            }
            Err(detail) => self.finish(CommandResult::from_error(request_id, &detail)),
        }
    }

    /// Re-derives the history selection of a session (view, selector,
    /// window, viewport columns).
    pub(super) fn sync_history(&mut self, session_id: &SessionId, now_ns: u64) {
        let (Some(state), Some(shared)) = (
            self.sessions.state(session_id),
            self.sessions.shared(session_id),
        ) else {
            return;
        };
        let columns = shared.viewport().0;
        if self
            .history
            .sync_session(session_id, state, columns, now_ns)
        {
            self.sessions.mark_dirty(session_id);
        }
    }

    fn prepare_signal(
        &mut self,
        request_id: Uuid,
        session_id: &SessionId,
        target: ProcessKey,
        signal: Signal,
        digest: String,
        now: Now,
    ) {
        let snapshot = self.ingestion.snapshots().latest();
        let row =
            match self
                .sessions
                .check_signal_target(session_id, &target, &snapshot.process_table)
            {
                Ok(row) => row,
                Err(detail) => {
                    self.finish(CommandResult::from_error(request_id, &detail));
                    return;
                }
            };
        let protected = self.hosts.protected_pids();
        let mut fx = Effects::default();
        self.process.prepare(
            PrepareRequest {
                request_id,
                session_id,
                target,
                signal,
                payload_sha256: digest,
                row: Some(row),
                protected_pids: &protected,
            },
            now,
            &mut fx,
        );
        self.apply(fx);
    }
}
