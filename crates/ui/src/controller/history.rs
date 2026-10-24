//! The history views' keys beyond the run cursor: on the Overview History
//! the GPU its GPU panels show (D-107; its bands scroll with
//! [`super::paging`]); on the Process History the marks and the deletion of
//! runs (D-108), which asks to confirm before anything is sent. `a` marks
//! every ended run the filter keeps, the ones beyond the projected rows too
//! (D-128), until a mark changes one by one.

use std::time::Duration;

use dgxtop_core::commands::{CommandResult, CommandStatus, delete_run_batches};
use dgxtop_core::config::MIN_COMMAND_BYTES;
use dgxtop_core::identity::{EntityId, ModuleId, ProcessKey};
use dgxtop_core::view::{BannerLevel, ProcessHistoryView, RunRowView};

use super::deletion::{RunDeletion, RunSet};
use super::keymap::KeyAction;
use super::navigation::{self, Movement};
use super::requests::RequestKind;
use super::{CommandSink, Controller, ControllerContext, InputMode};
use crate::format::counted;
use crate::modules::{HISTORY_MODULE_ID, history_gpus};

impl Controller {
    /// `↑`/`↓` on the Overview History: the GPU its GPU panels show;
    /// nothing on a host of one GPU. A choice of what the view already
    /// holds, so nothing is sent to the Manager.
    pub(super) fn navigate_history_gpu(
        &mut self,
        movement: Movement,
        context: &ControllerContext<'_>,
    ) {
        if self.history_page() != 0 {
            return;
        }
        let Some(history) = context
            .model
            .and_then(|model| model.modules.history.as_deref())
        else {
            return;
        };
        let ids: Vec<EntityId> = history_gpus(history)
            .into_iter()
            .map(|gpu| gpu.entity_id)
            .collect();
        if ids.len() < 2 {
            return;
        }
        let current = ModuleId::new(HISTORY_MODULE_ID)
            .ok()
            .and_then(|module| self.picks.get(&module))
            .filter(|id| ids.contains(id))
            .or_else(|| ids.first())
            .cloned();
        if let Some(next) = navigation::step_list(&ids, current.as_ref(), movement) {
            self.pick_history_gpu(next);
        }
    }

    /// Shows `gpu` in the Overview History's GPU panels (a key or its
    /// picker's chip).
    pub(super) fn pick_history_gpu(&mut self, gpu: EntityId) {
        let Ok(history) = ModuleId::new(HISTORY_MODULE_ID) else {
            return;
        };
        if self.picks.get(&history) != Some(&gpu) {
            self.picks.insert(history, gpu);
            self.bump();
        }
    }

    /// `Space` on the list: marks or unmarks the run under the cursor, then
    /// the cursor goes to the next run, so a run of rows is marked key by
    /// key.
    pub(super) fn toggle_mark(&mut self, context: &ControllerContext<'_>) {
        if self.history_page() != crate::views::PROCESS_LIST_PAGE {
            return;
        }
        let Some(key) = self.history_cursor_run(context).map(|run| run.key.clone()) else {
            return;
        };
        self.marked_every = None;
        if !self.marked.remove(&key) {
            self.marked.insert(key);
        }
        self.bump();
        self.navigate_history(KeyAction::HistoryDown, context);
    }

    /// `a` on the list: marks every ended run the filter keeps, listed or
    /// not (D-128), or unmarks all when the listed ones all are.
    pub(super) fn toggle_mark_all(&mut self, context: &ControllerContext<'_>) {
        if self.history_page() != crate::views::PROCESS_LIST_PAGE {
            return;
        }
        let Some(runs) = process_history(context) else {
            return;
        };
        let ended: Vec<ProcessKey> = runs
            .runs
            .iter()
            .filter(|run| has_ended(run))
            .map(|run| run.key.clone())
            .collect();
        if ended.is_empty() {
            return;
        }
        if ended.iter().all(|key| self.marked.contains(key)) {
            self.marked.clear();
            self.marked_every = None;
        } else {
            self.marked.extend(ended);
            self.marked_every = Some(
                self.drafts
                    .current(context.model, context.now)
                    .map(|fields| fields.filter)
                    .unwrap_or_default(),
            );
        }
        self.bump();
    }

    /// Esc: forgets the marks; whether there were any.
    pub(super) fn clear_marks(&mut self) -> bool {
        let had = !self.marked.is_empty();
        self.marked.clear();
        self.marked_every = None;
        had
    }

    /// `d`: asks to delete the marked runs listed, else the run under the
    /// cursor; on an opened run that run. Running runs stay (the next table
    /// would follow them again); with no ended run among them nothing is
    /// asked, nor while a signal waits for its own confirmation.
    pub(super) fn ask_delete(&mut self, context: &ControllerContext<'_>) {
        let page = self.history_page();
        if page == 0 || !self.confirmation.is_idle() {
            return;
        }
        if let Some(deletion) = &self.deletion {
            let text = format!("still deleting: {} left", counted(deletion.left(), "run"));
            self.notify(BannerLevel::Info, text, context.now);
            return;
        }
        let Some(runs) = process_history(context) else {
            return;
        };
        if page == crate::views::PROCESS_LIST_PAGE
            && let Some(filter) = self.marked_every.clone()
        {
            self.ask_delete_every(runs, filter, context);
            return;
        }
        let chosen: Vec<&RunRowView> =
            if self.marked.is_empty() || page != crate::views::PROCESS_LIST_PAGE {
                self.history_cursor_run(context).into_iter().collect()
            } else {
                runs.runs
                    .iter()
                    .filter(|run| self.marked.contains(&run.key))
                    .collect()
            };
        let (ended, running): (Vec<&RunRowView>, Vec<&RunRowView>) =
            chosen.into_iter().partition(|run| has_ended(run));
        if ended.is_empty() {
            self.notify(
                BannerLevel::Info,
                "nothing to delete: a running run stays until it ends",
                context.now,
            );
            return;
        }
        self.set_mode(InputMode::ConfirmDelete {
            runs: RunSet::Listed(ended.into_iter().map(|run| run.key.clone()).collect()),
            running: running.len(),
        });
    }

    /// Asks to delete every ended run `filter` keeps (D-128): the listed
    /// ones and the runs beyond the projected rows, which are all ended
    /// since running runs are listed first.
    fn ask_delete_every(
        &mut self,
        runs: &ProcessHistoryView,
        filter: String,
        context: &ControllerContext<'_>,
    ) {
        let (ended, running): (Vec<&RunRowView>, Vec<&RunRowView>) =
            runs.runs.iter().partition(|run| has_ended(run));
        let count = ended.len() + runs.more_runs;
        if count == 0 {
            self.notify(
                BannerLevel::Info,
                "nothing to delete: a running run stays until it ends",
                context.now,
            );
            return;
        }
        self.set_mode(InputMode::ConfirmDelete {
            runs: RunSet::Every { filter, count },
            running: running.len(),
        });
    }

    /// y/Enter on the deletion prompt: sends the deletion's first command
    /// (see [`RunDeletion`]) and goes back to the list.
    pub(super) fn confirm_delete<S: CommandSink + ?Sized>(
        &mut self,
        runs: &RunSet,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        let batches = match runs {
            RunSet::Listed(runs) => delete_run_batches(sink.session_id(), runs, MIN_COMMAND_BYTES)
                .into_iter()
                .map(RunSet::Listed)
                .collect(),
            RunSet::Every { .. } => vec![runs.clone()],
        };
        self.deletion = Some(RunDeletion::new(batches));
        self.send_next_deletion(context, sink);
        if self.history_page() == crate::views::RUN_PAGE {
            self.set_history_page(crate::views::PROCESS_LIST_PAGE);
        }
        self.bump();
    }

    /// The deletion's next command, or once all were answered its outcome.
    fn send_next_deletion<S: CommandSink + ?Sized>(
        &mut self,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        let Some(deletion) = self.deletion.as_mut() else {
            return;
        };
        let Some(batch) = deletion.next_batch() else {
            let deleted = deletion.deleted();
            self.deletion = None;
            let text = format!(
                "deleted {} from the Process History",
                counted(deleted, "run")
            );
            self.notify(BannerLevel::Info, text, context.now);
            return;
        };
        match self.submit(batch.body(), RequestKind::DeleteRuns, context, sink) {
            Some(request_id) => {
                if let Some(deletion) = self.deletion.as_mut() {
                    deletion.sent(request_id, batch);
                }
            }
            // The submission said why; the runs left keep their marks.
            None => self.stop_deletion(batch.len(), context.now),
        }
    }

    /// A result of the deletion's command: the next leaves when it
    /// completed; otherwise the deletion stops (its notice says why).
    pub(super) fn on_deletion_result<S: CommandSink + ?Sized>(
        &mut self,
        request_id: u128,
        result: &CommandResult,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        let Some(batch) = self
            .deletion
            .as_mut()
            .and_then(|deletion| deletion.answered(request_id))
        else {
            return;
        };
        if result.status != CommandStatus::Completed {
            self.stop_deletion(batch.len(), context.now);
            return;
        }
        match &batch {
            RunSet::Listed(runs) => self.marked.retain(|key| !runs.contains(key)),
            RunSet::Every { .. } => {
                self.marked.clear();
                self.marked_every = None;
            }
        }
        if let Some(deletion) = self.deletion.as_mut() {
            deletion.count_deleted(batch.len());
        }
        self.bump();
        self.send_next_deletion(context, sink);
    }

    /// A deletion whose command got no answer in time stops: whether that
    /// command deleted its runs is unknown, so nothing more is sent.
    pub(super) fn expire_deletion(&mut self, now: Duration) {
        let Some(awaited) = self.deletion.as_ref().and_then(RunDeletion::awaited) else {
            return;
        };
        if !self.requests.tracks(awaited) {
            let left = self.deletion.as_ref().map_or(0, RunDeletion::left);
            self.deletion = None;
            let text = format!(
                "no answer to the deletion; it stopped with {} not known deleted",
                counted(left, "run")
            );
            self.notify(BannerLevel::Warning, text, now);
        }
    }

    /// Ends the deletion before its end: `unsent` runs of the command just
    /// refused and the runs still waiting stay, marks and all.
    fn stop_deletion(&mut self, unsent: usize, now: Duration) {
        let Some(deletion) = self.deletion.take() else {
            return;
        };
        let left = deletion.left() + unsent;
        let text = format!(
            "the deletion stopped: {} deleted, {} not",
            counted(deletion.deleted(), "run"),
            left
        );
        self.notify(BannerLevel::Warning, text, now);
    }

    /// The run under the Process History's cursor (see `navigate_history`).
    fn history_cursor_run<'a>(
        &mut self,
        context: &ControllerContext<'a>,
    ) -> Option<&'a RunRowView> {
        let runs = process_history(context)?;
        let history = ModuleId::new(HISTORY_MODULE_ID).ok()?;
        let last = runs.runs.len().checked_sub(1)?;
        let selected = self
            .drafts
            .current(context.model, context.now)
            .and_then(|fields| fields.selected_process);
        let scroll = usize::from(self.scroll.get(&history).copied().unwrap_or(0));
        let cursor = selected
            .and_then(|key| runs.runs.iter().position(|run| run.key == key))
            .unwrap_or_else(|| scroll.min(last));
        runs.runs.get(cursor)
    }
}

fn process_history<'a>(context: &ControllerContext<'a>) -> Option<&'a ProcessHistoryView> {
    context
        .model
        .and_then(|model| model.modules.process_history.as_deref())
}

/// A run that ended, which may be deleted (the history's own rule).
fn has_ended(run: &RunRowView) -> bool {
    run.end.is_some()
}
