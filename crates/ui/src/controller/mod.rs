//! Controller (`ARCHITECTURE.md` §8): input → typed intents, never side effects.
//!
//! Five parts: the InputBackend ([`crate::terminal::InputSource`]), the
//! [`keymap`], the context router (this module: mode/view/HitMap routing),
//! the command builder ([`session`] drafts + typed `CommandBody` values) and
//! the confirmation input ([`confirmation`]). The Controller validates only
//! syntax, length and key context; the Manager re-validates everything.
//! Selection is always sent as `ProcessKey`/entity identity with the frame
//! revision of the frame the user saw, never as a row index.

mod confirmation;
mod filter;
pub mod keymap;
mod navigation;
mod notices;
mod requests;
mod session;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use dgxtop_core::action::Signal;
use dgxtop_core::commands::{
    CommandBody, CommandEnvelope, CommandRejection, CommandResult, CommandStatus, ViewKind,
};
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::{EntityId, ModuleId, ProcessKey, SessionId};
use dgxtop_core::ports::SessionPort;
use dgxtop_core::view::{BannerLevel, ViewModel};

use crate::num::saturate_u16;

pub use confirmation::{ConfirmationInput, MAX_CONFIRMATION_TTL};
pub use filter::{FilterEdit, FilterEditError};
pub use keymap::{KeyAction, ModeKind};
pub use navigation::{INTERVAL_STEPS_MS, Movement};
pub use notices::MAX_NOTICES;
pub use session::SessionFields;

use confirmation::PrepareOutcome;
use notices::Notices;
use requests::{PendingRequests, RequestKind};
use session::SessionDrafts;

use crate::modules::{CPU_MODULE_ID, HISTORY_MODULE_ID, MEMORY_MODULE_ID};
use crate::registry::controls;
pub use crate::viewer::UiState;
use crate::viewer::{HitMap, ModeView};
use crate::views;

/// Lines a module's scrollable part may be scrolled past its start.
const MAX_MODULE_SCROLL: u16 = 64;
/// Page size for list views without a measured table.
const DEFAULT_PAGE: u32 = 10;
/// A seek toward an off-window target gives up after this long without
/// progress (each re-centred projection that advances it refreshes it).
const SEEK_TTL: Duration = Duration::from_secs(5);
/// Upper bound of automatic seek steps (one per projection).
const MAX_SEEK_STEPS: u16 = 512;

/// Where commands go: the Manager's session queue (non-blocking).
pub trait CommandSink {
    fn session_id(&self) -> &SessionId;
    /// `Ok` means accepted (never "completed"); a full queue is `Busy`.
    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection>;
}

impl<T: SessionPort + ?Sized> CommandSink for T {
    fn session_id(&self) -> &SessionId {
        SessionPort::session_id(self)
    }

    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection> {
        SessionPort::submit(self, command)
    }
}

/// Facts the router needs about what is on screen.
#[derive(Debug, Clone, Copy)]
pub struct ControllerContext<'a> {
    /// The projection drawn in the last frame (what the user sees).
    pub model: Option<&'a ViewModel>,
    /// HitMap of the last rendered frame.
    pub hit_map: &'a HitMap,
    /// Revision of the last rendered projection (sent with session updates).
    pub frame_revision: u64,
    /// Loop clock.
    pub now: Duration,
    /// When the active projection was received (loop clock).
    pub projection_received_at: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputMode {
    Normal,
    Filter(FilterEdit),
    Help { scroll: u16 },
}

/// A selection move whose target lies outside the projected window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessSeek {
    requested: ProcessKey,
    requested_index: u32,
    goal: Option<u32>,
    steps: u16,
    view: ViewKind,
    started_at: Duration,
}

/// The Controller.
#[derive(Debug)]
pub struct Controller {
    mode: InputMode,
    confirmation: ConfirmationInput,
    drafts: SessionDrafts,
    seek: Option<ProcessSeek>,
    requests: PendingRequests,
    notices: Notices,
    quit: bool,
    version: u64,
    busy_notified: bool,
    /// Collapsible modules the user expanded (presentation only).
    expanded: BTreeSet<ModuleId>,
    /// How far the user scrolled each module's scrollable part (D-70).
    scroll: BTreeMap<ModuleId, u16>,
    /// Each module's inner tab (D-72).
    tabs: BTreeMap<ModuleId, u8>,
    /// A command line to copy with the next frame (D-76).
    clipboard: Option<String>,
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}

fn current_view(context: &ControllerContext<'_>) -> ViewKind {
    context
        .model
        .map_or(ViewKind::Overview, |model| model.session.view)
}

fn rejection_text(what: &str, rejection: &CommandRejection) -> (BannerLevel, String) {
    if rejection.code == ErrorCode::Busy {
        (
            BannerLevel::Warning,
            format!("command queue busy: {what} was not sent; try again"),
        )
    } else {
        (
            BannerLevel::Error,
            format!(
                "{what} rejected: {} {}",
                rejection.code.as_str(),
                crate::format::clean(&rejection.message)
            ),
        )
    }
}

impl Controller {
    pub fn new() -> Self {
        Self {
            mode: InputMode::Normal,
            confirmation: ConfirmationInput::default(),
            drafts: SessionDrafts::default(),
            seek: None,
            requests: PendingRequests::default(),
            notices: Notices::default(),
            quit: false,
            version: 0,
            busy_notified: false,
            expanded: BTreeSet::new(),
            scroll: BTreeMap::new(),
            clipboard: None,
            tabs: BTreeMap::new(),
        }
    }

    /// The user asked to quit (`q` / `Ctrl-C`).
    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// Whether a command's result is still expected: the loop then looks
    /// for it at the input cadence instead of its idle one.
    pub fn awaits_results(&self) -> bool {
        !self.requests.is_empty()
    }

    /// Increments whenever something the Viewer shows changed.
    pub fn version(&self) -> u64 {
        self.version
    }

    fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Adds a notice (also used by the runner for output problems).
    pub fn notify(&mut self, level: BannerLevel, text: impl Into<String>, now: Duration) {
        self.notices.push(level, text, now);
        self.bump();
    }

    /// Current input mode.
    pub fn mode(&self) -> ModeKind {
        if self.confirmation.is_active() {
            return ModeKind::Confirm;
        }
        match self.mode {
            InputMode::Normal => ModeKind::Normal,
            InputMode::Filter(_) => ModeKind::Filter,
            InputMode::Help { .. } => ModeKind::Help,
        }
    }

    /// Redacted state for the Viewer (never contains the token).
    pub fn ui_state(&self, now: Duration) -> UiState {
        let mode = match (self.confirmation.summary(now), &self.mode) {
            (Some(summary), _) => ModeView::Confirm {
                target: summary.target,
                signal: summary.signal,
                remaining_ms: Some(
                    u64::try_from(summary.remaining.as_millis()).unwrap_or(u64::MAX),
                ),
            },
            (None, InputMode::Filter(edit)) => ModeView::Filter {
                buffer: edit.text().to_owned(),
                cursor: edit.cursor(),
            },
            (None, InputMode::Help { scroll }) => ModeView::Help { scroll: *scroll },
            (None, InputMode::Normal) => ModeView::Normal,
        };
        UiState {
            mode,
            notices: self.notices.visible(),
            awaiting_confirmation: self.confirmation.is_awaiting(),
            expanded: self.expanded.clone(),
            scroll: self.scroll.clone(),
            tabs: self.tabs.clone(),
        }
    }

    /// Routes one input event.
    pub fn handle_event<S: CommandSink + ?Sized>(
        &mut self,
        event: &Event,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        match event {
            Event::Key(key) => self.handle_key(key, context, sink),
            Event::Mouse(mouse) => self.handle_mouse(mouse, context),
            Event::Paste(text) => self.paste(text, context.now),
            Event::FocusGained | Event::FocusLost | Event::Resize(..) => {}
        }
    }

    /// End of an input batch: submits the coalesced session update.
    pub fn finish_input_batch<S: CommandSink + ?Sized>(
        &mut self,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        self.flush_staged(context, sink);
    }

    /// Housekeeping on the loop clock (notices, token and request expiry).
    pub fn tick(&mut self, now: Duration) {
        if self.notices.expire(now) {
            self.bump();
        }
        let was_active = self.confirmation.is_active();
        let was_awaiting = self.confirmation.is_awaiting();
        if self.confirmation.expire(now) {
            self.notify(
                BannerLevel::Warning,
                "confirmation expired; nothing was sent",
                now,
            );
        } else if was_awaiting && self.confirmation.is_idle() {
            self.notify(
                BannerLevel::Warning,
                "no answer to the terminate request; nothing was sent",
                now,
            );
        } else if was_active && !self.confirmation.is_active() {
            self.bump();
        }
        self.requests.expire(now);
    }

    fn handle_key<S: CommandSink + ?Sized>(
        &mut self,
        key: &KeyEvent,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let mode = self.mode();
        match keymap::resolve(key, mode, current_view(context)) {
            Some(action) => self.apply(action, context, sink),
            None if mode == ModeKind::Filter => self.edit_filter(key, context.now),
            None => {}
        }
    }

    fn apply<S: CommandSink + ?Sized>(
        &mut self,
        action: KeyAction,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        let view = current_view(context);
        match action {
            KeyAction::Quit => {
                self.quit = true;
                self.bump();
            }
            KeyAction::ToggleHelp => self.set_mode(InputMode::Help { scroll: 0 }),
            KeyAction::CloseHelp => self.set_mode(InputMode::Normal),
            KeyAction::DismissNotices => {
                if self.notices.clear() {
                    self.bump();
                }
            }
            KeyAction::MoveDown
            | KeyAction::MoveUp
            | KeyAction::PageDown
            | KeyAction::PageUp
            | KeyAction::First
            | KeyAction::Last => {
                let movement = self.movement_for(action, context);
                if let InputMode::Help { scroll } = &mut self.mode {
                    *scroll = match movement {
                        Movement::Down(step) => {
                            scroll.saturating_add(u16::try_from(step).unwrap_or(u16::MAX))
                        }
                        Movement::Up(step) => {
                            scroll.saturating_sub(u16::try_from(step).unwrap_or(u16::MAX))
                        }
                        Movement::First => 0,
                        Movement::Last => u16::MAX,
                    };
                    self.bump();
                } else {
                    self.navigate(movement, context);
                }
            }
            KeyAction::ShowTab(target) => self.show_tab(target, context),
            KeyAction::NextView | KeyAction::PreviousView => {
                // Cycle from the view already requested so Tab bursts advance.
                let base = self
                    .drafts
                    .current(context.model, context.now)
                    .map_or(view, |fields| fields.view);
                let current = views::Tab::of(base, self.history_page());
                let target = if action == KeyAction::NextView {
                    views::next_tab(current)
                } else {
                    views::previous_tab(current)
                };
                self.show_tab(target, context);
            }
            KeyAction::NextGpu => self.navigate_gpu(Movement::Down(1), context),
            KeyAction::PreviousGpu => self.navigate_gpu(Movement::Up(1), context),
            KeyAction::CycleGpuFilter => self.cycle_gpu_filter(context),
            KeyAction::StartFilter => self.start_filter(context),
            KeyAction::CycleSort => {
                self.reorder(context, |fields| fields.sort = fields.sort.next())
            }
            KeyAction::ToggleOrder => {
                self.reorder(context, |fields| fields.ascending = !fields.ascending)
            }
            KeyAction::FasterInterval => self.change_interval(true, context, sink),
            KeyAction::SlowerInterval => self.change_interval(false, context, sink),
            KeyAction::Terminate => self.prepare_signal(Signal::Sigterm, context, sink),
            KeyAction::Kill => self.prepare_signal(Signal::Sigkill, context, sink),
            KeyAction::Confirm => self.confirm(context, sink),
            KeyAction::Cancel => self.cancel(context, sink),
            KeyAction::NextHistoryBarSize => self.cycle_history_bar_size(true, context),
            KeyAction::PreviousHistoryBarSize => self.cycle_history_bar_size(false, context),
            KeyAction::ToggleModule => self.toggle_module(context, sink),
            KeyAction::ToggleCpuDetail => {
                if let Ok(cpu) = ModuleId::new(CPU_MODULE_ID) {
                    self.toggle_expanded(cpu);
                }
            }
            KeyAction::ScrollMemoryUp | KeyAction::ScrollMemoryDown => {
                if let Ok(memory) = ModuleId::new(MEMORY_MODULE_ID) {
                    self.scroll_module(memory, action == KeyAction::ScrollMemoryDown);
                }
            }
            KeyAction::HistoryDown | KeyAction::HistoryUp | KeyAction::HistoryOpen => {
                self.navigate_history(action, context);
            }
            KeyAction::HistoryExpandCommand => {
                if self.history_page() == 2
                    && let Ok(history) = ModuleId::new(HISTORY_MODULE_ID)
                {
                    self.toggle_expanded(history);
                }
            }
            KeyAction::HistoryCopyCommand => self.copy_command(context),
            KeyAction::FilterApply => self.finish_filter(true, context),
            KeyAction::FilterClear => self.finish_filter(false, context),
        }
    }

    fn set_mode(&mut self, mode: InputMode) {
        if self.mode != mode {
            self.mode = mode;
            self.bump();
        }
    }

    fn movement_for(&self, action: KeyAction, context: &ControllerContext<'_>) -> Movement {
        let page = match context.hit_map.visible_process_rows() {
            0 => DEFAULT_PAGE,
            rows => crate::num::saturate_u32(rows.saturating_sub(1).max(1)),
        };
        match action {
            KeyAction::MoveUp => Movement::Up(1),
            KeyAction::PageDown => Movement::Down(page),
            KeyAction::PageUp => Movement::Up(page),
            KeyAction::First => Movement::First,
            KeyAction::Last => Movement::Last,
            _ => Movement::Down(1),
        }
    }

    /// Applies a change to the current session fields and stages it.
    fn update_fields(
        &mut self,
        context: &ControllerContext<'_>,
        change: impl FnOnce(&mut SessionFields),
    ) -> bool {
        let Some(mut fields) = self.drafts.current(context.model, context.now) else {
            return false;
        };
        let before = fields.clone();
        change(&mut fields);
        if fields == before {
            return false;
        }
        self.drafts.stage(fields);
        true
    }

    /// Sort/filter changes reorder the list: any seek toward an index stops.
    fn reorder(
        &mut self,
        context: &ControllerContext<'_>,
        change: impl FnOnce(&mut SessionFields),
    ) {
        if self.update_fields(context, change) {
            self.seek = None;
        }
    }

    /// Shows `tab`: its view and, for a history tab, its page (D-75); the
    /// process history keeps an open process open.
    fn show_tab(&mut self, tab: views::Tab, context: &ControllerContext<'_>) {
        if let Ok(history) = ModuleId::new(HISTORY_MODULE_ID) {
            let page = self.history_page();
            match tab {
                views::Tab::OverviewHistory => {
                    self.tabs.insert(history, 0);
                }
                views::Tab::ProcessHistory if page == 0 => {
                    self.tabs.insert(history, views::PROCESS_LIST_PAGE);
                }
                _ => {}
            }
        }
        self.bump();
        self.show_view(tab.view(), context);
    }

    /// The history module's page (0 the overview history).
    fn history_page(&self) -> u8 {
        ModuleId::new(HISTORY_MODULE_ID)
            .ok()
            .and_then(|history| self.tabs.get(&history).copied())
            .unwrap_or(0)
    }

    fn show_view(&mut self, view: ViewKind, context: &ControllerContext<'_>) {
        let Some(current) = self.drafts.current(context.model, context.now) else {
            return;
        };
        if current.view == view {
            return;
        }
        let fields = self.drafts.switch_view(&current, view);
        self.drafts.stage(fields);
        self.seek = None;
    }

    fn navigate(&mut self, movement: Movement, context: &ControllerContext<'_>) {
        match current_view(context) {
            ViewKind::Overview => self.navigate_processes(movement, context),
            ViewKind::GpuDetail => self.navigate_gpu(movement, context),
            ViewKind::Settings => self.navigate_modules(movement, context),
            // The history view charts fixed series (D-46); nothing to
            // pick, and the retired views are never shown (D-91).
            ViewKind::History | ViewKind::Processes | ViewKind::Sources | ViewKind::Actions => {}
        }
    }

    fn navigate_processes(&mut self, movement: Movement, context: &ControllerContext<'_>) {
        let Some(processes) = context
            .model
            .and_then(|model| model.modules.processes.as_deref())
        else {
            return;
        };
        let view = current_view(context);
        let base = self
            .seek
            .as_ref()
            .filter(|seek| {
                seek.view == view && context.now.saturating_sub(seek.started_at) <= SEEK_TTL
            })
            .map(|seek| seek.goal.unwrap_or(seek.requested_index));
        let Some(step) = navigation::process_target(processes, base, movement) else {
            return;
        };
        let key = step.key.clone();
        self.update_fields(context, |fields| fields.selected_process = Some(key));
        self.seek = Some(ProcessSeek {
            requested: step.key,
            requested_index: step.index,
            goal: step.goal,
            steps: 0,
            view,
            started_at: context.now,
        });
    }

    fn select_entity(&mut self, context: &ControllerContext<'_>, entity: EntityId) {
        self.update_fields(context, |fields| fields.selected_entity_id = Some(entity));
    }

    fn navigate_gpu(&mut self, movement: Movement, context: &ControllerContext<'_>) {
        let Some(gpu) = context.model.and_then(|model| model.modules.gpu.as_deref()) else {
            return;
        };
        let ids: Vec<EntityId> = gpu
            .cards
            .iter()
            .map(|card| card.entity_id.clone())
            .collect();
        // Without a selection the detail view shows the first card.
        let current = self
            .drafts
            .current(context.model, context.now)
            .and_then(|fields| fields.selected_entity_id)
            .filter(|id| ids.contains(id))
            .or_else(|| gpu.selected.clone())
            .or_else(|| ids.first().cloned());
        if let Some(next) = navigation::step_list(&ids, current.as_ref(), movement) {
            self.select_entity(context, next);
        }
    }

    fn navigate_modules(&mut self, movement: Movement, context: &ControllerContext<'_>) {
        let Some(settings) = context
            .model
            .and_then(|model| model.modules.settings.as_deref())
        else {
            return;
        };
        let ids: Vec<EntityId> = settings
            .modules
            .iter()
            .filter_map(|(module_id, _)| EntityId::new(module_id.as_str()).ok())
            .collect();
        let current = self
            .drafts
            .current(context.model, context.now)
            .and_then(|fields| fields.selected_entity_id)
            .filter(|id| ids.contains(id));
        if let Some(next) = navigation::step_list(&ids, current.as_ref(), movement) {
            self.select_entity(context, next);
        }
    }

    fn cycle_gpu_filter(&mut self, context: &ControllerContext<'_>) {
        let Some(processes) = context
            .model
            .and_then(|model| model.modules.processes.as_deref())
        else {
            return;
        };
        let choices = processes.gpu_choices.clone();
        self.reorder(context, |fields| {
            fields.selected_entity_id =
                navigation::cycle_gpu_filter(&choices, fields.selected_entity_id.as_ref());
        });
    }

    /// `w`/`W` on the history view: the Overview History's bar size, or on
    /// an opened run the Process History's (D-77, D-78), each its own
    /// session field (D-80).
    fn cycle_history_bar_size(&mut self, forward: bool, context: &ControllerContext<'_>) {
        let on_run = self.history_page() == 2;
        self.update_fields(context, |fields| {
            if on_run {
                fields.process_bar = fields.process_bar.step(forward);
            } else {
                fields.overview_bar = fields.overview_bar.step_overview(forward);
            }
        });
    }

    fn start_filter(&mut self, context: &ControllerContext<'_>) {
        if current_view(context) != ViewKind::Overview {
            return;
        }
        let initial = self
            .drafts
            .current(context.model, context.now)
            .map(|fields| fields.filter)
            .unwrap_or_default();
        self.set_mode(InputMode::Filter(FilterEdit::new(&initial)));
    }

    fn finish_filter(&mut self, apply: bool, context: &ControllerContext<'_>) {
        let InputMode::Filter(edit) = std::mem::replace(&mut self.mode, InputMode::Normal) else {
            return;
        };
        self.bump();
        let text = if apply {
            edit.text().to_owned()
        } else {
            String::new()
        };
        self.reorder(context, |fields| fields.filter = text);
    }

    fn edit_filter(&mut self, key: &KeyEvent, now: Duration) {
        let InputMode::Filter(edit) = &mut self.mode else {
            return;
        };
        let control = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let outcome = match key.code {
            KeyCode::Char(character) if !control => edit.insert(character).map(|()| true),
            KeyCode::Backspace => {
                edit.backspace();
                Ok(true)
            }
            KeyCode::Delete => {
                edit.delete();
                Ok(true)
            }
            KeyCode::Left => {
                edit.left();
                Ok(true)
            }
            KeyCode::Right => {
                edit.right();
                Ok(true)
            }
            KeyCode::Home => {
                edit.home();
                Ok(true)
            }
            KeyCode::End => {
                edit.end();
                Ok(true)
            }
            _ => Ok(false),
        };
        match outcome {
            Ok(true) => self.bump(),
            Ok(false) => {}
            Err(error) => self.notify(BannerLevel::Warning, error.to_string(), now),
        }
    }

    fn paste(&mut self, text: &str, now: Duration) {
        let InputMode::Filter(edit) = &mut self.mode else {
            return;
        };
        let mut failure = None;
        for character in text.chars().filter(|character| !character.is_control()) {
            if let Err(error) = edit.insert(character) {
                failure = Some(error);
                break;
            }
        }
        self.bump();
        if let Some(error) = failure {
            self.notify(BannerLevel::Warning, error.to_string(), now);
        }
    }

    fn handle_mouse(&mut self, mouse: &MouseEvent, context: &ControllerContext<'_>) {
        match self.mode() {
            ModeKind::Normal => {}
            ModeKind::Help => {
                if let InputMode::Help { scroll } = &mut self.mode {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => *scroll = scroll.saturating_add(1),
                        MouseEventKind::ScrollUp => *scroll = scroll.saturating_sub(1),
                        _ => return,
                    }
                    self.bump();
                }
                return;
            }
            // Confirmation and text entry ignore the mouse: a click must
            // never confirm or retarget anything.
            ModeKind::Confirm | ModeKind::Filter => return,
        }
        // The wheel over a module's scrollable part scrolls it; anywhere
        // else it moves the process selection.
        let scrollable = context
            .hit_map
            .hit(mouse.column, mouse.row)
            .filter(|target| target.control_id == controls::MODULE_SCROLL)
            .map(|target| target.module_id.clone());
        match (mouse.kind, scrollable) {
            (MouseEventKind::Down(MouseButton::Left), _) => {
                self.click(mouse.column, mouse.row, context);
            }
            (MouseEventKind::ScrollDown, Some(module)) => self.scroll_module(module, true),
            (MouseEventKind::ScrollUp, Some(module)) => self.scroll_module(module, false),
            (MouseEventKind::ScrollDown, None) => self.navigate(Movement::Down(1), context),
            (MouseEventKind::ScrollUp, None) => self.navigate(Movement::Up(1), context),
            _ => {}
        }
    }

    /// The process history (D-72, D-75, D-76): `j`/`k` move the cursor over
    /// the runs and Enter opens or closes the run under it. The run under
    /// the cursor is the session's selection, so the Manager details it.
    fn navigate_history(&mut self, action: KeyAction, context: &ControllerContext<'_>) {
        let Ok(history) = ModuleId::new(HISTORY_MODULE_ID) else {
            return;
        };
        let tab = self.history_page();
        if tab == 0 {
            return;
        }
        let Some(runs) = context
            .model
            .and_then(|model| model.modules.process_history.as_deref())
        else {
            return;
        };
        let last = saturate_u16(runs.runs.len().saturating_sub(1));
        // The selection the session has or is about to have: a run that
        // starts later moves it down the list.
        let selected = self
            .drafts
            .current(context.model, context.now)
            .and_then(|fields| fields.selected_process);
        let scroll = self.scroll.get(&history).copied().unwrap_or(0);
        let cursor = selected
            .and_then(|key| runs.runs.iter().position(|run| run.key == key))
            .map_or(scroll.min(last), saturate_u16);
        let moved = match action {
            KeyAction::HistoryOpen => {
                self.tabs
                    .insert(history.clone(), if tab == 1 { 2 } else { 1 });
                cursor
            }
            KeyAction::HistoryDown => cursor.saturating_add(1).min(last),
            KeyAction::HistoryUp => cursor.saturating_sub(1),
            _ => return,
        };
        self.scroll.insert(history, moved);
        self.bump();
        if let Some(run) = runs.runs.get(usize::from(moved)) {
            let key = run.key.clone();
            self.update_fields(context, |fields| fields.selected_process = Some(key));
        }
    }

    /// Queues the opened run's command line for the clipboard (D-76).
    fn copy_command(&mut self, context: &ControllerContext<'_>) {
        if self.history_page() != 2 {
            return;
        }
        let command = context
            .model
            .and_then(|model| model.modules.process_history.as_deref())
            .and_then(|runs| runs.opened.as_ref())
            .map(|opened| opened.info.command.clone())
            .filter(|command| !command.is_empty());
        let (level, text) = match &command {
            Some(_) => (BannerLevel::Info, "command line copied to the clipboard"),
            None => (BannerLevel::Warning, "no command line to copy"),
        };
        self.notices.push(level, text, context.now);
        self.clipboard = command;
        self.bump();
    }

    /// The text to copy with the next frame, once.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// Scrolls a module's scrollable part one line (presentation only; the
    /// module clamps the offset to what it has).
    fn scroll_module(&mut self, module_id: ModuleId, down: bool) {
        let offset = self.scroll.entry(module_id).or_insert(0);
        *offset = if down {
            offset.saturating_add(1).min(MAX_MODULE_SCROLL)
        } else {
            offset.saturating_sub(1)
        };
        self.bump();
    }

    /// Resolves a click through the HitMap of the frame the user saw. The
    /// target carries the full identity drawn there, and the update is sent
    /// with that frame's revision, so a stale map can never be retargeted to
    /// whatever row occupies the position now; the Manager rejects it as
    /// `StaleView` if the frame is too old.
    fn click(&mut self, column: u16, row: u16, context: &ControllerContext<'_>) {
        let Some(target) = context.hit_map.hit(column, row).cloned() else {
            return;
        };
        if let Some(tab) = views::Tab::for_control(target.control_id) {
            self.show_tab(tab, context);
            return;
        }
        match target.control_id {
            controls::MODULE_TOGGLE => self.toggle_expanded(target.module_id),
            controls::PROCESS_ROW | controls::PROCESS_ROW_SELECTED => {
                if let Some(key) = target.process_key {
                    self.update_fields(context, |fields| fields.selected_process = Some(key));
                    self.seek = None;
                }
            }
            controls::GPU_CARD => {
                let Some(entity) = target.entity_id else {
                    return;
                };
                if current_view(context) == ViewKind::GpuDetail {
                    self.select_entity(context, entity);
                } else if let Some(current) = self.drafts.current(context.model, context.now) {
                    let mut fields = self.drafts.switch_view(&current, ViewKind::GpuDetail);
                    fields.selected_entity_id = Some(entity);
                    self.drafts.stage(fields);
                    self.seek = None;
                }
            }
            controls::SETTINGS_MODULE => {
                if let Some(entity) = target.entity_id {
                    self.select_entity(context, entity);
                }
            }
            _ => {}
        }
    }

    /// Expands a collapsed module or collapses an expanded one. A pure
    /// presentation toggle: nothing is sent to the Manager.
    fn toggle_expanded(&mut self, module_id: ModuleId) {
        if !self.expanded.remove(&module_id) {
            self.expanded.insert(module_id);
        }
        self.bump();
    }

    /// Submits a non-navigation command right away (never coalesced), after
    /// any staged navigation so causal order is kept.
    fn submit<S: CommandSink + ?Sized>(
        &mut self,
        body: CommandBody,
        kind: RequestKind,
        context: &ControllerContext<'_>,
        sink: &S,
    ) -> Option<u128> {
        self.flush_staged(context, sink);
        let name = body.name();
        let envelope = CommandEnvelope::new(sink.session_id().clone(), body);
        if let Err(error) = envelope.validate_syntax() {
            self.notify(
                BannerLevel::Error,
                format!("{name} not sent: {error}"),
                context.now,
            );
            return None;
        }
        let request_id = envelope.request_id.as_u128();
        match sink.submit(envelope) {
            Ok(()) => {
                self.requests.track(request_id, kind, context.now);
                Some(request_id)
            }
            Err(rejection) => {
                let (level, text) = rejection_text(name, &rejection);
                self.notify(level, text, context.now);
                None
            }
        }
    }

    fn flush_staged<S: CommandSink + ?Sized>(&mut self, context: &ControllerContext<'_>, sink: &S) {
        let Some(fields) = self.drafts.take_staged() else {
            return;
        };
        let envelope = CommandEnvelope::new(
            sink.session_id().clone(),
            fields.clone().into_command(context.frame_revision),
        );
        if let Err(error) = envelope.validate_syntax() {
            self.notify(
                BannerLevel::Error,
                format!("view change not sent: {error}"),
                context.now,
            );
            return;
        }
        let request_id = envelope.request_id.as_u128();
        match sink.submit(envelope) {
            Ok(()) => {
                self.drafts.mark_sent(fields, request_id, context.now);
                self.requests
                    .track(request_id, RequestKind::ViewSession, context.now);
                self.busy_notified = false;
            }
            Err(rejection) if rejection.code == ErrorCode::Busy => {
                // Pure navigation is latest-wins: keep it staged and retry
                // on the next batch instead of dropping it.
                self.drafts.restage(fields);
                if !self.busy_notified {
                    self.busy_notified = true;
                    self.notify(
                        BannerLevel::Warning,
                        "command queue busy; the view change will be retried",
                        context.now,
                    );
                }
            }
            Err(rejection) => {
                let (level, text) = rejection_text("view change", &rejection);
                self.notify(level, text, context.now);
            }
        }
    }

    fn change_interval<S: CommandSink + ?Sized>(
        &mut self,
        faster: bool,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        let Some(model) = context.model else { return };
        let Some(revision) = model
            .modules
            .settings
            .as_deref()
            .map(|settings| settings.revision)
            .filter(|revision| *revision >= 1)
        else {
            self.notify(
                BannerLevel::Warning,
                "settings revision unknown: open Settings (F2) first; the interval was not changed",
                context.now,
            );
            return;
        };
        if self.requests.has_pending_interval() {
            self.notify(
                BannerLevel::Info,
                "an interval change is still pending",
                context.now,
            );
            return;
        }
        let configured = model.header.configured_interval_ms;
        let Some(interval_ms) = navigation::next_interval(configured, faster) else {
            let edge = if faster { "fastest" } else { "slowest" };
            self.notify(
                BannerLevel::Info,
                format!("sampling interval is already the {edge} step"),
                context.now,
            );
            return;
        };
        self.submit(
            CommandBody::SetSamplingInterval {
                interval_ms,
                expected_config_revision: revision,
            },
            RequestKind::Interval { interval_ms },
            context,
            sink,
        );
    }

    fn toggle_module<S: CommandSink + ?Sized>(
        &mut self,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        let Some(settings) = context
            .model
            .and_then(|model| model.modules.settings.as_deref())
        else {
            return;
        };
        let selected = self
            .drafts
            .current(context.model, context.now)
            .and_then(|fields| fields.selected_entity_id);
        let Some((module_id, enabled)) = selected.and_then(|selected| {
            settings
                .modules
                .iter()
                .find(|(module_id, _)| module_id.as_str() == selected.as_str())
                .cloned()
        }) else {
            self.notify(
                BannerLevel::Info,
                "select a module with j/k first",
                context.now,
            );
            return;
        };
        if settings.revision < 1 {
            self.notify(
                BannerLevel::Warning,
                "settings revision unknown; the module was not changed",
                context.now,
            );
            return;
        }
        self.submit(
            CommandBody::SetModuleEnabled {
                module_id: module_id.clone(),
                enabled: !enabled,
                expected_config_revision: settings.revision,
            },
            RequestKind::ModuleToggle {
                module_id,
                enabled: !enabled,
            },
            context,
            sink,
        );
    }

    /// `K` (SIGTERM) and `F9` (SIGKILL, D-70): PrepareSignal for the
    /// selected process exactly as drawn in the visible process table, with
    /// its full `ProcessKey`; the same confirmation follows either, and the
    /// Manager's policy decides whether SIGKILL may be sent.
    fn prepare_signal<S: CommandSink + ?Sized>(
        &mut self,
        signal: Signal,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        if !self.confirmation.is_idle() {
            self.notify(
                BannerLevel::Info,
                "a terminate request is already pending",
                context.now,
            );
            return;
        }
        let Some(target) = context.hit_map.visible_selected_process().cloned() else {
            self.notify(
                BannerLevel::Info,
                "no visible selected process: select one in the process table first",
                context.now,
            );
            return;
        };
        let body = CommandBody::PrepareSignal {
            target: target.clone(),
            signal,
        };
        if let Some(request_id) = self.submit(
            body,
            RequestKind::Prepare {
                target: target.clone(),
            },
            context,
            sink,
        ) {
            self.confirmation
                .begin(request_id, target, signal, context.now);
            self.bump();
        }
    }

    fn confirm<S: CommandSink + ?Sized>(&mut self, context: &ControllerContext<'_>, sink: &S) {
        let Some((token, target)) = self.confirmation.token() else {
            return;
        };
        let body = CommandBody::ConfirmSignal {
            confirmation_token: token,
        };
        if self
            .submit(body, RequestKind::Confirm { target }, context, sink)
            .is_some()
        {
            self.confirmation.finish();
            self.bump();
        }
    }

    fn cancel<S: CommandSink + ?Sized>(&mut self, context: &ControllerContext<'_>, sink: &S) {
        let Some((token, _)) = self.confirmation.token() else {
            return;
        };
        let body = CommandBody::CancelSignal {
            confirmation_token: token,
        };
        if self
            .submit(body, RequestKind::Cancel, context, sink)
            .is_some()
        {
            self.confirmation.finish();
            self.bump();
        }
    }

    /// Local deadline of a token from the result's boot-clock expiry, mapped
    /// through the projection's generation time; bounded by the max TTL.
    fn confirmation_deadline(result: &CommandResult, context: &ControllerContext<'_>) -> Duration {
        let fallback = context.now.saturating_add(MAX_CONFIRMATION_TTL);
        match (
            result.expires_boot_ns,
            context.model,
            context.projection_received_at,
        ) {
            (Some(expires), Some(model), Some(received)) => {
                let remaining =
                    Duration::from_nanos(expires.saturating_sub(model.generated_boot_ns));
                received.saturating_add(remaining).min(fallback)
            }
            _ => fallback,
        }
    }

    /// Routes one command result (results are never coalesced away).
    pub fn on_result<S: CommandSink + ?Sized>(
        &mut self,
        result: &CommandResult,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        let request_id = result.request_id.as_u128();
        let terminal = requests::is_terminal(result.status);
        let kind = self.requests.resolve(request_id, terminal);
        match &kind {
            Some(RequestKind::ViewSession) if terminal => {
                let accepted = result.status == CommandStatus::Completed;
                self.drafts.on_result(request_id, accepted);
            }
            Some(RequestKind::Prepare { .. }) => self.on_prepare_result(result, context, sink),
            _ => {}
        }
        if let Some((level, text)) = requests::notice_for(kind.as_ref(), result) {
            self.notify(level, text, context.now);
        }
    }

    fn on_prepare_result<S: CommandSink + ?Sized>(
        &mut self,
        result: &CommandResult,
        context: &ControllerContext<'_>,
        sink: &S,
    ) {
        let may_activate = self.mode == InputMode::Normal;
        let deadline = Self::confirmation_deadline(result, context);
        match self
            .confirmation
            .on_prepare_result(result, may_activate, deadline)
        {
            PrepareOutcome::MustCancel(token) => {
                let body = CommandBody::CancelSignal {
                    confirmation_token: token,
                };
                self.submit(body, RequestKind::Cancel, context, sink);
                self.notify(
                    BannerLevel::Warning,
                    "confirmation cancelled because another input was open; press K again",
                    context.now,
                );
            }
            PrepareOutcome::Activated | PrepareOutcome::Failed => self.bump(),
            PrepareOutcome::Waiting | PrepareOutcome::Unrelated => {}
        }
    }

    /// A new projection became active: reconcile drafts and continue a seek
    /// toward an off-window target with the best known row.
    pub fn on_projection(&mut self, model: &ViewModel, context: &ControllerContext<'_>) {
        self.drafts.on_projection(model);
        let Some(seek) = self.seek.clone() else {
            return;
        };
        let expired = context.now.saturating_sub(seek.started_at) > SEEK_TTL;
        let Some(processes) = model.modules.processes.as_deref() else {
            self.seek = None;
            return;
        };
        if expired || model.session.view != seek.view {
            self.seek = None;
            return;
        }
        let applied = processes.selected.as_ref() == Some(&seek.requested);
        let Some(goal) = seek.goal else {
            if applied {
                self.seek = None;
            }
            return;
        };
        if processes.selected_index == Some(goal) {
            self.seek = None;
            return;
        }
        if !applied {
            return;
        }
        let Some(step) = navigation::process_toward(processes, goal) else {
            self.seek = None;
            return;
        };
        if step.key == seek.requested || seek.steps >= MAX_SEEK_STEPS {
            self.seek = None;
            return;
        }
        let key = step.key.clone();
        self.update_fields(context, |fields| fields.selected_process = Some(key));
        self.seek = Some(ProcessSeek {
            requested: step.key,
            requested_index: step.index,
            goal: step.goal,
            steps: seek.steps + 1,
            view: seek.view,
            started_at: context.now,
        });
    }
}

#[cfg(test)]
mod tests;
