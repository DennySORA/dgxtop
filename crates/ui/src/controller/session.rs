//! View-session intents. The authoritative session lives in the Manager;
//! the Controller only stages the next `UpdateViewSession` (coalescing pure
//! navigation within one input batch) and remembers the last request until
//! a projection reflects it, so bursts of keys build on each other.

use std::time::Duration;

use dgxtop_core::commands::{CommandBody, SortKey, ViewKind};
use dgxtop_core::identity::{EntityId, ProcessKey};
use dgxtop_core::process_history::BarSize;
use dgxtop_core::view::{SessionState, ViewModel};

/// A sent draft no projection has reflected is dropped after this long.
const PENDING_TTL: Duration = Duration::from_secs(2);

/// All fields of an `UpdateViewSession` except the frame revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFields {
    pub view: ViewKind,
    pub selected_entity_id: Option<EntityId>,
    pub selected_process: Option<ProcessKey>,
    pub filter: String,
    pub sort: SortKey,
    pub ascending: bool,
    /// The history's bar sizes (D-78, D-80), kept across views.
    pub overview_bar: BarSize,
    pub process_bar: BarSize,
}

impl SessionFields {
    pub fn from_state(state: &SessionState) -> Self {
        Self {
            view: state.view,
            selected_entity_id: state.selected_entity_id.clone(),
            selected_process: state.selected_process.clone(),
            filter: state.filter.clone(),
            sort: state.sort,
            ascending: state.ascending,
            overview_bar: state.overview_bar,
            process_bar: state.process_bar,
        }
    }

    pub fn into_command(self, frame_revision: u64) -> CommandBody {
        CommandBody::UpdateViewSession {
            view: self.view,
            selected_entity_id: self.selected_entity_id,
            selected_process: self.selected_process,
            filter: self.filter,
            sort: self.sort,
            ascending: self.ascending,
            frame_revision,
            overview_bar: Some(self.overview_bar),
            process_bar: Some(self.process_bar),
        }
    }
}

/// View-specific part of the fields, remembered per view so switching away
/// and back restores selection and filters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ViewMemory {
    selected_entity_id: Option<EntityId>,
    selected_process: Option<ProcessKey>,
    filter: String,
}

/// Each offered view's memory; the retired views (D-91) remember nothing.
const fn memory_slot(view: ViewKind) -> Option<usize> {
    match view {
        ViewKind::Overview => Some(0),
        ViewKind::GpuDetail => Some(1),
        ViewKind::History => Some(2),
        ViewKind::Settings => Some(3),
        ViewKind::Processes | ViewKind::Sources | ViewKind::Actions => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingDraft {
    fields: SessionFields,
    request_id: u128,
    sent_at: Duration,
    acknowledged: bool,
}

/// Staged, pending and remembered session intents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDrafts {
    staged: Option<SessionFields>,
    pending: Option<PendingDraft>,
    memory: [Option<ViewMemory>; 4],
}

impl SessionDrafts {
    /// Fields new intents build on: staged, then pending (until reflected,
    /// acknowledged or expired), then the projection.
    pub fn current(&mut self, model: Option<&ViewModel>, now: Duration) -> Option<SessionFields> {
        if let Some(staged) = &self.staged {
            return Some(staged.clone());
        }
        let projected = model.map(|model| SessionFields::from_state(&model.session));
        if let Some(pending) = &self.pending {
            let reflected = projected.as_ref() == Some(&pending.fields);
            let expired = now.saturating_sub(pending.sent_at) > PENDING_TTL;
            if reflected || expired {
                self.pending = None;
            } else {
                return Some(pending.fields.clone());
            }
        }
        projected
    }

    /// Stages fields for the next flush (replacing earlier staged fields:
    /// pure navigation coalesces).
    pub fn stage(&mut self, fields: SessionFields) {
        self.staged = Some(fields);
    }

    /// Takes the staged fields for submission.
    pub fn take_staged(&mut self) -> Option<SessionFields> {
        self.staged.take()
    }

    /// Restores fields whose submission was refused as busy (retried next batch).
    pub fn restage(&mut self, fields: SessionFields) {
        if self.staged.is_none() {
            self.staged = Some(fields);
        }
    }

    pub fn mark_sent(&mut self, fields: SessionFields, request_id: u128, now: Duration) {
        self.pending = Some(PendingDraft {
            fields,
            request_id,
            sent_at: now,
            acknowledged: false,
        });
    }

    /// A result for a session request: rejections drop the draft at once;
    /// completions let the next projection replace it.
    pub fn on_result(&mut self, request_id: u128, accepted: bool) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        if pending.request_id != request_id {
            return;
        }
        if accepted {
            pending.acknowledged = true;
        } else {
            self.pending = None;
        }
    }

    /// A new projection arrived: an acknowledged draft is now reflected.
    pub fn on_projection(&mut self, model: &ViewModel) {
        if let Some(pending) = &self.pending {
            let reflected = SessionFields::from_state(&model.session) == pending.fields;
            if reflected || pending.acknowledged {
                self.pending = None;
            }
        }
    }

    /// Fields for switching from `current` to `view`, remembering the view
    /// being left and restoring what `view` showed last time.
    pub fn switch_view(&mut self, current: &SessionFields, view: ViewKind) -> SessionFields {
        if let Some(slot) = memory_slot(current.view) {
            self.memory[slot] = Some(ViewMemory {
                selected_entity_id: current.selected_entity_id.clone(),
                selected_process: current.selected_process.clone(),
                filter: current.filter.clone(),
            });
        }
        let memory = memory_slot(view)
            .and_then(|slot| self.memory[slot].clone())
            .unwrap_or_default();
        SessionFields {
            view,
            selected_entity_id: memory.selected_entity_id,
            selected_process: memory.selected_process,
            filter: memory.filter,
            sort: current.sort,
            ascending: current.ascending,
            overview_bar: current.overview_bar,
            process_bar: current.process_bar,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{self, process_key};

    fn fields(view: ViewKind) -> SessionFields {
        SessionFields {
            view,
            selected_entity_id: None,
            selected_process: Some(process_key(1, 1)),
            filter: "py".into(),
            sort: SortKey::Pid,
            ascending: true,
            overview_bar: BarSize::OneMinute,
            process_bar: BarSize::Auto,
        }
    }

    #[test]
    fn staged_then_pending_then_projection() {
        let model = test_fixtures::view_model(ViewKind::Overview, 1);
        let mut drafts = SessionDrafts::default();
        let projected = drafts.current(Some(&model), Duration::ZERO).unwrap();
        assert_eq!(projected.view, ViewKind::Overview);
        drafts.stage(fields(ViewKind::Overview));
        assert_eq!(
            drafts.current(Some(&model), Duration::ZERO).unwrap().view,
            ViewKind::Overview
        );
        let staged = drafts.take_staged().unwrap();
        drafts.mark_sent(staged, 7, Duration::ZERO);
        assert_eq!(
            drafts
                .current(Some(&model), Duration::from_millis(500))
                .unwrap()
                .view,
            ViewKind::Overview
        );
        assert_eq!(
            drafts
                .current(Some(&model), Duration::from_secs(3))
                .unwrap()
                .view,
            ViewKind::Overview,
            "an unreflected draft expires"
        );
    }

    #[test]
    fn acknowledged_drafts_yield_to_the_next_projection_and_rejections_drop_them() {
        let model = test_fixtures::view_model(ViewKind::Overview, 1);
        let mut drafts = SessionDrafts::default();
        drafts.mark_sent(fields(ViewKind::Overview), 7, Duration::ZERO);
        drafts.on_result(8, true);
        drafts.on_projection(&model);
        assert_eq!(
            drafts.current(Some(&model), Duration::ZERO).unwrap().view,
            ViewKind::Overview
        );
        drafts.on_result(7, true);
        drafts.on_projection(&model);
        assert_eq!(
            drafts.current(Some(&model), Duration::ZERO).unwrap().view,
            ViewKind::Overview
        );
        drafts.mark_sent(fields(ViewKind::Overview), 9, Duration::ZERO);
        drafts.on_result(9, false);
        assert_eq!(
            drafts.current(Some(&model), Duration::ZERO).unwrap().view,
            ViewKind::Overview
        );
    }

    #[test]
    fn fields_become_an_update_view_session_command() {
        let body = fields(ViewKind::Overview).into_command(42);
        assert!(matches!(
            body,
            CommandBody::UpdateViewSession {
                view: ViewKind::Overview,
                frame_revision: 42,
                ascending: true,
                ..
            }
        ));
    }
}
