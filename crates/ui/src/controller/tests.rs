//! Controller behavior: input modes, identity-based selection, safe
//! confirmation and coalescing. The HitMap comes from a real Viewer render
//! so `K` targets exactly what the process table drew.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use dgxtop_core::action::{ActionState, Signal};
use dgxtop_core::commands::{
    CommandBody, CommandEnvelope, CommandRejection, CommandResult, ViewKind,
};
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::{EntityId, ModuleId, ProcessKey, SessionId};
use dgxtop_core::view::{BannerLevel, ViewModel};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::registry::controls;
use crate::test_fixtures::{self, token};
use crate::viewer::{RenderInput, Viewer};

struct FakeSink {
    session: SessionId,
    submitted: RefCell<Vec<CommandEnvelope>>,
    busy: Cell<bool>,
}

impl FakeSink {
    fn new() -> Self {
        Self {
            session: SessionId::new("tui").unwrap(),
            submitted: RefCell::new(Vec::new()),
            busy: Cell::new(false),
        }
    }
}

impl CommandSink for FakeSink {
    fn session_id(&self) -> &SessionId {
        &self.session
    }

    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection> {
        command.validate_syntax().unwrap();
        if self.busy.get() {
            return Err(CommandRejection {
                request_id: command.request_id,
                code: ErrorCode::Busy,
                message: "queue full".into(),
            });
        }
        self.submitted.borrow_mut().push(command);
        Ok(())
    }
}

fn render_hit_map(model: &ViewModel, ui: &UiState) -> HitMap {
    let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
    let mut map = None;
    terminal
        .draw(|frame| {
            map = Some(
                Viewer::builtin()
                    .render(
                        frame,
                        &RenderInput {
                            model: Some(model),
                            ui,
                            projection_age_ms: 0,
                            tick: 0,
                        },
                    )
                    .hit_map,
            );
        })
        .unwrap();
    map.unwrap()
}

struct Harness {
    controller: Controller,
    sink: FakeSink,
    model: ViewModel,
    hit_map: HitMap,
    now: Duration,
}

impl Harness {
    fn new(view: ViewKind) -> Self {
        Self::with_model(test_fixtures::view_model(view, 5))
    }

    fn with_model(model: ViewModel) -> Self {
        let hit_map = render_hit_map(&model, &UiState::default());
        Self {
            controller: Controller::new(),
            sink: FakeSink::new(),
            model,
            hit_map,
            now: Duration::from_secs(1),
        }
    }

    fn press(&mut self, event: Event) {
        let context = ControllerContext {
            model: Some(&self.model),
            hit_map: &self.hit_map,
            frame_revision: self.hit_map.frame_revision(),
            now: self.now,
            projection_received_at: Some(Duration::ZERO),
        };
        self.controller.handle_event(&event, &context, &self.sink);
    }

    /// Presses keys then ends the input batch (flushes coalesced navigation).
    fn keys(&mut self, events: &[Event]) {
        for event in events {
            self.press(event.clone());
        }
        self.batch();
    }

    fn batch(&mut self) {
        let context = ControllerContext {
            model: Some(&self.model),
            hit_map: &self.hit_map,
            frame_revision: self.hit_map.frame_revision(),
            now: self.now,
            projection_received_at: Some(Duration::ZERO),
        };
        self.controller.finish_input_batch(&context, &self.sink);
    }

    fn deliver(&mut self, result: CommandResult) {
        let context = ControllerContext {
            model: Some(&self.model),
            hit_map: &self.hit_map,
            frame_revision: self.hit_map.frame_revision(),
            now: self.now,
            projection_received_at: Some(Duration::ZERO),
        };
        self.controller.on_result(&result, &context, &self.sink);
    }

    /// Makes a new projection active (re-renders its HitMap).
    fn project(&mut self, model: ViewModel) {
        self.model = model;
        self.hit_map = render_hit_map(&self.model, &self.controller.ui_state(self.now));
        let context = ControllerContext {
            model: Some(&self.model),
            hit_map: &self.hit_map,
            frame_revision: self.hit_map.frame_revision(),
            now: self.now,
            projection_received_at: Some(self.now),
        };
        self.controller.on_projection(&self.model, &context);
    }

    fn submitted(&self) -> Vec<CommandBody> {
        self.sink
            .submitted
            .borrow()
            .iter()
            .map(|envelope| envelope.body.clone())
            .collect()
    }

    fn take(&self) -> Vec<CommandEnvelope> {
        std::mem::take(&mut *self.sink.submitted.borrow_mut())
    }

    fn last_session(&self) -> (ViewKind, Option<EntityId>, Option<ProcessKey>, String, u64) {
        self.submitted()
            .into_iter()
            .rev()
            .find_map(|body| match body {
                CommandBody::UpdateViewSession {
                    view,
                    selected_entity_id,
                    selected_process,
                    filter,
                    frame_revision,
                    ..
                } => Some((
                    view,
                    selected_entity_id,
                    selected_process,
                    filter,
                    frame_revision,
                )),
                _ => None,
            })
            .expect("an UpdateViewSession was submitted")
    }

    /// The bar sizes of the last UpdateViewSession (D-80).
    fn last_bars(&self) -> (dgxtop_core::tiers::BarSize, dgxtop_core::tiers::BarSize) {
        self.submitted()
            .into_iter()
            .rev()
            .find_map(|body| match body {
                CommandBody::UpdateViewSession {
                    overview_bar,
                    process_bar,
                    ..
                } => Some((overview_bar?, process_bar?)),
                _ => None,
            })
            .expect("an UpdateViewSession with bar sizes was submitted")
    }

    /// K, then the Manager answers the PrepareSignal with a token.
    fn activate_confirmation(&mut self) -> ProcessKey {
        self.keys(&[shift('K')]);
        let envelope = self.take().pop().expect("PrepareSignal submitted");
        let CommandBody::PrepareSignal { target, .. } = envelope.body else {
            panic!("expected PrepareSignal, got {:?}", envelope.body);
        };
        let mut result = CommandResult::completed(envelope.request_id, "prepared");
        result.confirmation_token = Some(token());
        result.expires_boot_ns = Some(self.model.generated_boot_ns + 9_000_000_000);
        self.deliver(result);
        assert_eq!(self.controller.mode(), ModeKind::Confirm);
        target
    }
}

fn key(character: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
}

fn shift(character: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::SHIFT))
}

fn code(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(character: char) -> Event {
    Event::Key(KeyEvent::new(
        KeyCode::Char(character),
        KeyModifiers::CONTROL,
    ))
}

fn click(column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn processes_model(
    view: ViewKind,
    rows: usize,
    window_start: u32,
    total: u32,
    selected: usize,
) -> ViewModel {
    let mut model = test_fixtures::view_model(view, 5);
    let mut processes = test_fixtures::processes_view(rows, window_start, total);
    processes.selected = Some(processes.rows[selected].key.clone());
    processes.selected_index = Some(window_start + u32::try_from(selected).unwrap());
    model.session.selected_process = processes.selected.clone();
    model.modules.processes = Some(Arc::new(processes));
    model
}

#[test]
fn in_filter_mode_k_and_q_are_text_while_ctrl_c_quits() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[key('/'), shift('K'), key('q'), key('j')]);
    assert_eq!(harness.controller.mode(), ModeKind::Filter);
    assert!(!harness.controller.quit_requested());
    assert!(
        harness.submitted().is_empty(),
        "nothing is sent while typing"
    );
    let ModeView::Filter { buffer, .. } = harness.controller.ui_state(harness.now).mode else {
        panic!("filter mode");
    };
    assert_eq!(buffer, "Kqj");
    harness.keys(&[ctrl('c')]);
    assert!(harness.controller.quit_requested());
}

#[test]
fn q_and_ctrl_c_quit_in_normal_mode() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[key('q')]);
    assert!(harness.controller.quit_requested());
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[ctrl('c')]);
    assert!(harness.controller.quit_requested());
}

#[test]
fn k_without_a_visible_selection_does_nothing() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.hit_map = HitMap::new(5);
    harness.keys(&[shift('K')]);
    assert!(harness.submitted().is_empty());
    assert!(
        harness.controller.ui_state(harness.now).notices[0]
            .text
            .contains("no visible selected process")
    );
    let mut settings = Harness::new(ViewKind::Settings);
    settings.keys(&[shift('K')]);
    assert!(
        settings.submitted().is_empty(),
        "K is not bound outside process views"
    );
}

#[test]
fn k_builds_prepare_signal_with_the_full_process_key() {
    let mut harness = Harness::new(ViewKind::Overview);
    let drawn = harness.hit_map.visible_selected_process().cloned().unwrap();
    harness.keys(&[shift('K')]);
    let submitted = harness.submitted();
    assert_eq!(submitted.len(), 1);
    let CommandBody::PrepareSignal { target, signal } = &submitted[0] else {
        panic!("{submitted:?}");
    };
    assert_eq!(*signal, Signal::Sigterm);
    assert_eq!(target, &drawn);
    assert_eq!(
        target,
        &test_fixtures::process_key(4242, 90_100),
        "complete identity incl. start time and namespace"
    );
    assert!(
        harness
            .controller
            .ui_state(harness.now)
            .awaiting_confirmation
    );
}

/// F9 asks to force kill the selected process (D-70): the same PrepareSignal
/// and confirmation as K, with SIGKILL; the Manager's policy decides.
#[test]
fn f9_builds_prepare_signal_with_sigkill() {
    let mut harness = Harness::new(ViewKind::Overview);
    let drawn = harness.hit_map.visible_selected_process().cloned().unwrap();
    harness.keys(&[code(KeyCode::F(9))]);
    let submitted = harness.submitted();
    assert_eq!(submitted.len(), 1);
    let CommandBody::PrepareSignal { target, signal } = &submitted[0] else {
        panic!("{submitted:?}");
    };
    assert_eq!(*signal, Signal::Sigkill);
    assert_eq!(target, &drawn);
    assert!(
        harness
            .controller
            .ui_state(harness.now)
            .awaiting_confirmation
    );
}

fn wheel(down: bool, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        },
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// `[`/`]` and the wheel over the memory panel's GPU legend scroll it,
/// apart from the process selection, and send nothing (D-70).
#[test]
fn the_memory_legend_scrolls_apart_from_the_process_selection() {
    let mut harness = Harness::new(ViewKind::Overview);
    let memory = ModuleId::new(crate::modules::MEMORY_MODULE_ID).unwrap();
    let scroll = |harness: &Harness| {
        harness
            .controller
            .ui_state(harness.now)
            .scroll
            .get(&memory)
            .copied()
            .unwrap_or(0)
    };
    harness.keys(&[key(']'), key(']'), key('[')]);
    assert_eq!(scroll(&harness), 1);
    harness.keys(&[key('['), key('[')]);
    assert_eq!(scroll(&harness), 0, "never above the start");
    let area = ratatui::layout::Rect::new(150, 2, 10, 2);
    let revision = harness.hit_map.frame_revision();
    harness.hit_map.push_clipped(
        crate::registry::HitTarget {
            frame_revision: revision,
            module_id: memory.clone(),
            control_id: controls::MODULE_SCROLL,
            entity_id: None,
            process_key: None,
            rect: area,
        },
        area,
    );
    harness.keys(&[
        wheel(true, 151, 2),
        wheel(true, 151, 3),
        wheel(false, 152, 2),
    ]);
    assert_eq!(scroll(&harness), 1);
    assert!(
        harness.submitted().is_empty(),
        "scrolling is presentation only: {:?}",
        harness.submitted()
    );
}

/// In the Overview History ←/→ step the bar size (time) and ↑/↓ pick the
/// GPU (D-107); in the GPU view ←/→ pick the GPU and ↑/↓ turn the pages of
/// its sections (D-110).
#[test]
fn arrows_act_on_the_page_they_are_pressed_on() {
    use crate::controller::keymap::{KeyAction, ModeKind, resolve};
    let arrow = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
    let history = |code| resolve(&arrow(code), ModeKind::Normal, ViewKind::History);
    let gpu = |code| resolve(&arrow(code), ModeKind::Normal, ViewKind::GpuDetail);
    assert_eq!(history(KeyCode::Right), Some(KeyAction::NextHistoryBarSize));
    assert_eq!(
        history(KeyCode::Left),
        Some(KeyAction::PreviousHistoryBarSize)
    );
    assert_eq!(
        history(KeyCode::Char('w')),
        Some(KeyAction::NextHistoryBarSize)
    );
    assert_eq!(
        history(KeyCode::Char('W')),
        Some(KeyAction::PreviousHistoryBarSize)
    );
    assert_eq!(history(KeyCode::Down), Some(KeyAction::HistoryDown));
    assert_eq!(history(KeyCode::Up), Some(KeyAction::HistoryUp));
    assert_eq!(history(KeyCode::PageDown), Some(KeyAction::PageDown));
    assert_eq!(gpu(KeyCode::Right), Some(KeyAction::NextGpu));
    assert_eq!(gpu(KeyCode::Left), Some(KeyAction::PreviousGpu));
    assert_eq!(gpu(KeyCode::Down), Some(KeyAction::MoveDown));
    assert_eq!(gpu(KeyCode::PageUp), Some(KeyAction::PageUp));
}

/// `4` opens the Process History tab and `3` the Overview History one; in
/// the process pages `j`/`k` move the cursor over the runs, clamped to the
/// list, and Enter opens and closes a run (D-72, D-75, D-76). The run under
/// the cursor becomes the history view's selection.
#[test]
fn history_process_pages_navigate_and_select_the_run() {
    let mut harness = Harness::new(ViewKind::History);
    let history = ModuleId::new(crate::modules::HISTORY_MODULE_ID).unwrap();
    let state = |harness: &Harness| {
        let ui = harness.controller.ui_state(harness.now);
        (
            ui.tabs.get(&history).copied().unwrap_or(0),
            ui.scroll.get(&history).copied().unwrap_or(0),
        )
    };
    harness.keys(&[key('j'), code(KeyCode::Enter)]);
    assert_eq!(state(&harness), (0, 0), "the system page has no cursor");
    harness.keys(&[key('4'), key('j'), key('j'), key('k'), key('j')]);
    assert_eq!(
        state(&harness),
        (1, 1),
        "two runs: the cursor stops at the last"
    );
    let training = test_fixtures::process_history_view().runs[1].key.clone();
    let (view, _, selected, _, _) = harness.last_session();
    assert_eq!(view, ViewKind::History);
    assert_eq!(
        selected,
        Some(training),
        "the run under the cursor is selected"
    );
    harness.keys(&[code(KeyCode::Enter)]);
    assert_eq!(state(&harness), (2, 1));
    harness.keys(&[code(KeyCode::Enter)]);
    assert_eq!(state(&harness), (1, 1));
    harness.keys(&[key('3')]);
    assert_eq!(state(&harness).0, 0);
    assert!(
        harness
            .submitted()
            .iter()
            .all(|command| format!("{command:?}").contains("History")),
        "{:?}",
        harness.submitted()
    );
}

/// A run that starts later moves the selected one down the list: the
/// cursor stays on the selected run, so Enter opens it (D-77) and `k` goes
/// up from it.
#[test]
fn the_cursor_follows_the_selected_run_when_the_list_moves() {
    let mut harness = Harness::new(ViewKind::History);
    harness.keys(&[key('4'), key('j')]);
    let training = test_fixtures::process_history_view().runs[1].key.clone();
    assert_eq!(harness.last_session().2, Some(training.clone()));
    // A newer run arrives at the top: the selected run is third now.
    let runs = std::sync::Arc::make_mut(harness.model.modules.process_history.as_mut().unwrap());
    let mut newer = runs.runs[1].clone();
    newer.key.pid = dgxtop_core::identity::Pid::new(77).unwrap();
    runs.runs.insert(0, newer);
    harness.keys(&[code(KeyCode::Enter)]);
    assert_eq!(
        harness.last_session().2,
        Some(training),
        "Enter opens the selected run"
    );
    harness.keys(&[code(KeyCode::Enter), key('k')]);
    assert_eq!(
        harness.last_session().2.unwrap().pid.get(),
        42,
        "up from the selected run, not from the row the cursor had"
    );
}

/// `w`/`W` step the Overview History's bar size (D-78) and an opened
/// run's (D-77); the session's filter keeps both.
#[test]
fn w_steps_the_window_or_an_opened_runs_bar_size() {
    use dgxtop_core::tiers::BarSize;
    let mut harness = Harness::new(ViewKind::History);
    let bars = |harness: &Harness| harness.last_bars();
    harness.keys(&[key('w')]);
    assert_eq!(bars(&harness), (BarSize::FiveMinutes, BarSize::Auto));
    harness.keys(&[key('4'), code(KeyCode::Enter), key('w')]);
    assert_eq!(bars(&harness), (BarSize::FiveMinutes, BarSize::OneMinute));
    harness.keys(&[key('W'), key('W')]);
    assert_eq!(
        bars(&harness),
        (BarSize::FiveMinutes, BarSize::OneDay),
        "back past auto to a day"
    );
    harness.keys(&[code(KeyCode::Right)]);
    assert_eq!(
        bars(&harness),
        (BarSize::FiveMinutes, BarSize::Auto),
        "→ steps the bar size like w"
    );
    harness.keys(&[code(KeyCode::Left), key('3'), key('h'), key('W')]);
    assert_eq!(
        bars(&harness),
        (BarSize::OneDay, BarSize::OneDay),
        "the overview wraps past 1m to a day, the run's stays"
    );
    assert_eq!(
        harness.last_session().3,
        "",
        "the filter carries no bar (D-80)"
    );
}

/// `z` shows the GPU processes alone on the Overview for a moment (D-121):
/// `z` again, `Esc` or another view brings everything back. Elsewhere `z`
/// does nothing, and it sends nothing.
#[test]
fn z_shows_the_processes_alone_until_z_esc_or_another_view() {
    let mut harness = Harness::new(ViewKind::Overview);
    let processes = ModuleId::new(crate::modules::PROCESSES_MODULE_ID).unwrap();
    let zoomed = |harness: &Harness| {
        harness
            .controller
            .ui_state(harness.now)
            .expanded
            .contains(&processes)
    };
    harness.keys(&[key('z')]);
    assert!(zoomed(&harness));
    assert!(harness.submitted().is_empty(), "{:?}", harness.submitted());
    harness.keys(&[key('z')]);
    assert!(!zoomed(&harness), "z again");
    harness.keys(&[key('z'), code(KeyCode::Esc)]);
    assert!(!zoomed(&harness), "Esc");
    // `Esc` dismisses a notice first, so the table stays alone.
    harness.keys(&[key('z')]);
    harness
        .controller
        .notify(BannerLevel::Info, "SIGTERM sent", harness.now);
    harness.keys(&[code(KeyCode::Esc)]);
    assert!(zoomed(&harness), "the notice went first");
    harness.keys(&[code(KeyCode::Esc)]);
    assert!(!zoomed(&harness), "then the panels came back");
    // A key of another panel shows the panels first, then acts.
    let cpu = ModuleId::new(crate::modules::CPU_MODULE_ID).unwrap();
    harness.keys(&[key('z'), key('c')]);
    let state = harness.controller.ui_state(harness.now);
    assert!(!zoomed(&harness) && state.expanded.contains(&cpu));
    harness.keys(&[key('c')]);
    harness.keys(&[key('z'), key('2')]);
    assert!(!zoomed(&harness), "another view");
    // `z` before the GPU view's projection came: gone once it comes.
    harness.keys(&[key('z')]);
    assert!(zoomed(&harness));
    harness.project(crate::test_fixtures::view_model(ViewKind::GpuDetail, 2));
    assert!(!zoomed(&harness), "the GPU view came");
    harness.keys(&[key('z')]);
    assert!(!zoomed(&harness), "the GPU view has no z");
}

/// In an opened run `e` shows the whole command line and `y` queues it for
/// the clipboard once; on the list neither does anything (D-76).
#[test]
fn an_opened_run_expands_and_copies_its_command() {
    let mut harness = Harness::new(ViewKind::History);
    let history = ModuleId::new(crate::modules::HISTORY_MODULE_ID).unwrap();
    harness.keys(&[key('4'), key('e'), key('y')]);
    assert!(
        harness.controller.take_clipboard().is_none(),
        "the list copies nothing"
    );
    assert!(
        !harness
            .controller
            .ui_state(harness.now)
            .expanded
            .contains(&history)
    );
    harness.keys(&[code(KeyCode::Enter), key('e'), key('y')]);
    assert!(
        harness
            .controller
            .ui_state(harness.now)
            .expanded
            .contains(&history)
    );
    let copied = harness.controller.take_clipboard().unwrap();
    assert!(copied.starts_with("/venv/bin/python main.py"), "{copied}");
    assert!(harness.controller.take_clipboard().is_none(), "once");
}

#[test]
fn y_only_confirms_in_confirm_mode_with_a_token() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[key('y'), code(KeyCode::Enter)]);
    assert!(
        harness.submitted().is_empty(),
        "y/Enter do nothing without a token"
    );
    let target = harness.activate_confirmation();
    harness.keys(&[key('y')]);
    let submitted = harness.submitted();
    assert_eq!(submitted.len(), 1);
    let CommandBody::ConfirmSignal { confirmation_token } = &submitted[0] else {
        panic!("{submitted:?}");
    };
    assert_eq!(confirmation_token, &token());
    assert_eq!(
        harness.controller.mode(),
        ModeKind::Normal,
        "the token is consumed"
    );
    harness.keys(&[key('y')]);
    assert_eq!(harness.submitted().len(), 1, "never resent");
    assert_eq!(target.pid.get(), 4242);
}

#[test]
fn enter_confirms_and_esc_or_n_cancel() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.activate_confirmation();
    harness.keys(&[code(KeyCode::Enter)]);
    assert!(matches!(
        harness.submitted()[0],
        CommandBody::ConfirmSignal { .. }
    ));
    for cancel_key in [code(KeyCode::Esc), key('n')] {
        let mut harness = Harness::new(ViewKind::Overview);
        harness.activate_confirmation();
        harness.keys(&[cancel_key]);
        let submitted = harness.submitted();
        assert!(
            matches!(&submitted[0], CommandBody::CancelSignal { confirmation_token } if *confirmation_token == token())
        );
        assert_eq!(harness.controller.mode(), ModeKind::Normal);
    }
}

#[test]
fn other_keys_are_ignored_while_confirming() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.activate_confirmation();
    harness.keys(&[key('j'), code(KeyCode::Tab), shift('K'), key('s'), key('/')]);
    assert!(harness.submitted().is_empty());
    assert_eq!(harness.controller.mode(), ModeKind::Confirm);
    harness.keys(&[key('q')]);
    assert!(harness.controller.quit_requested(), "quit still works");
}

/// While the token is on its way no typing can start (D-109): `/` waits
/// like every key but the answers, so a `y` meant as text never confirms;
/// the token arrives into the confirmation, unanswered.
#[test]
fn no_typing_starts_while_a_token_is_on_its_way() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[shift('K')]);
    let prepare = harness.take().pop().unwrap();
    harness.keys(&[key('/')]);
    assert_eq!(harness.controller.mode(), ModeKind::Confirm);
    assert_eq!(harness.controller.ui_state(harness.now).early_answer, None);
    let mut result = CommandResult::completed(prepare.request_id, "prepared");
    result.confirmation_token = Some(token());
    harness.deliver(result);
    assert!(harness.submitted().is_empty(), "{:?}", harness.submitted());
    assert!(matches!(
        harness.controller.ui_state(harness.now).mode,
        ModeView::Confirm { .. }
    ));
}

#[test]
fn the_token_never_reaches_ui_state_or_debug_output() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.activate_confirmation();
    let secret = token().as_str().to_owned();
    assert!(!format!("{:?}", harness.controller.ui_state(harness.now)).contains(&secret));
    assert!(!format!("{:?}", harness.controller).contains(&secret));
    let ModeView::Confirm {
        target,
        remaining_ms,
        ..
    } = harness.controller.ui_state(harness.now).mode
    else {
        panic!("confirm mode");
    };
    assert_eq!(target.pid.get(), 4242);
    assert!(remaining_ms.unwrap() <= 9_000);
}

#[test]
fn j_and_k_select_by_process_key_across_a_reordered_projection() {
    let mut harness = Harness::with_model(processes_model(ViewKind::Overview, 5, 0, 5, 0));
    let first_rows: Vec<ProcessKey> = harness
        .model
        .modules
        .processes
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .map(|row| row.key.clone())
        .collect();
    harness.keys(&[key('j')]);
    assert_eq!(harness.last_session().2, Some(first_rows[1].clone()));
    // The Manager applies the selection and the list is re-sorted.
    let mut reordered = processes_model(ViewKind::Overview, 5, 0, 5, 0);
    let processes = Arc::make_mut(reordered.modules.processes.as_mut().unwrap());
    processes.rows.reverse();
    processes.selected = Some(first_rows[1].clone());
    processes.selected_index = Some(3);
    reordered.session.selected_process = Some(first_rows[1].clone());
    harness.project(reordered);
    harness.take();
    harness.keys(&[key('j')]);
    assert_eq!(
        harness.last_session().2,
        Some(first_rows[0].clone()),
        "the next row in the new order, by identity"
    );
    harness.take();
    harness.keys(&[key('k')]);
    assert_eq!(harness.last_session().2, Some(first_rows[1].clone()));
}

#[test]
fn destructive_commands_are_never_coalesced_and_keep_order() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[key('j'), shift('K')]);
    let submitted = harness.submitted();
    assert_eq!(submitted.len(), 2);
    assert!(matches!(
        submitted[0],
        CommandBody::UpdateViewSession { .. }
    ));
    assert!(matches!(submitted[1], CommandBody::PrepareSignal { .. }));
}

#[test]
fn a_click_on_a_stale_hitmap_keeps_the_clicked_identity_and_old_revision() {
    let mut harness = Harness::with_model(processes_model(ViewKind::Overview, 5, 0, 5, 0));
    let stale_map = harness.hit_map.clone();
    let clicked = stale_map
        .targets()
        .iter()
        .find(|target| target.control_id == controls::PROCESS_ROW)
        .unwrap()
        .clone();
    // A newer projection (revision 6) reorders the rows, but the user is
    // still looking at the frame of revision 5.
    let mut newer = processes_model(ViewKind::Overview, 5, 0, 5, 0);
    newer.frame_revision = 6;
    Arc::make_mut(newer.modules.processes.as_mut().unwrap())
        .rows
        .reverse();
    harness.model = newer;
    harness.hit_map = stale_map;
    harness.keys(&[click(clicked.rect.x + 1, clicked.rect.y)]);
    let (_, _, selected, _, frame_revision) = harness.last_session();
    assert_eq!(
        selected, clicked.process_key,
        "never retargeted to the row now at that position"
    );
    assert_eq!(
        frame_revision, 5,
        "sent with the old revision so the Manager can reject it"
    );
}

#[test]
fn tab_clicks_and_keys_switch_views() {
    let mut harness = Harness::new(ViewKind::Overview);
    let tab = harness
        .hit_map
        .targets()
        .iter()
        .find(|target| target.control_id == crate::views::Tab::OverviewHistory.control_id())
        .unwrap()
        .clone();
    harness.keys(&[click(tab.rect.x, tab.rect.y)]);
    assert_eq!(harness.last_session().0, ViewKind::History);
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[code(KeyCode::Tab), code(KeyCode::Tab)]);
    assert_eq!(
        harness.last_session().0,
        ViewKind::History,
        "bursts build on the staged view and skip hidden views"
    );
    // The action record is not a view (D-44): 6 switches nothing, and Tab
    // wraps from the last offered tab, Settings, to the overview (D-75).
    let sent = harness.submitted().len();
    harness.keys(&[key('6')]);
    assert_eq!(harness.submitted().len(), sent, "6 is not a view key");
    harness.keys(&[key('5'), code(KeyCode::Tab)]);
    assert_eq!(harness.last_session().0, ViewKind::Overview, "Tab wraps");
    harness.keys(&[code(KeyCode::F(2))]);
    assert_eq!(harness.last_session().0, ViewKind::Settings);
}

#[test]
fn the_cpu_panel_expands_and_collapses_by_click_or_c_without_a_command() {
    let mut harness = Harness::new(ViewKind::Overview);
    let cpu = ModuleId::new(crate::modules::CPU_MODULE_ID).unwrap();
    let toggle = harness
        .hit_map
        .targets()
        .iter()
        .find(|target| target.control_id == controls::MODULE_TOGGLE && target.module_id == cpu)
        .expect("the collapsed CPU panel is clickable")
        .clone();
    assert!(harness.controller.ui_state(harness.now).expanded.is_empty());
    harness.keys(&[click(toggle.rect.x + 1, toggle.rect.y + 1)]);
    assert!(
        harness
            .controller
            .ui_state(harness.now)
            .expanded
            .contains(&cpu)
    );
    harness.keys(&[key('c')]);
    assert!(harness.controller.ui_state(harness.now).expanded.is_empty());
    harness.keys(&[key('c')]);
    assert!(
        harness
            .controller
            .ui_state(harness.now)
            .expanded
            .contains(&cpu)
    );
    assert!(
        harness.submitted().is_empty(),
        "expanding is presentation only: {:?}",
        harness.submitted()
    );
}

#[test]
fn filter_enter_applies_and_esc_clears() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[
        key('/'),
        key('w'),
        key('o'),
        code(KeyCode::Backspace),
        key('k'),
        code(KeyCode::Enter),
    ]);
    assert_eq!(harness.last_session().3, "wk");
    assert_eq!(harness.controller.mode(), ModeKind::Normal);
    harness.keys(&[key('/'), code(KeyCode::Esc)]);
    assert_eq!(harness.last_session().3, "");
    assert_eq!(harness.controller.mode(), ModeKind::Normal);
}

#[test]
fn filter_input_is_bounded_to_256_bytes() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.press(key('/'));
    for _ in 0..300 {
        harness.press(key('x'));
    }
    let ModeView::Filter { buffer, .. } = harness.controller.ui_state(harness.now).mode else {
        panic!("filter mode");
    };
    assert_eq!(buffer.len(), 256);
    assert!(
        harness
            .controller
            .ui_state(harness.now)
            .notices
            .iter()
            .any(|notice| notice.text.contains("256"))
    );
    harness.keys(&[code(KeyCode::Enter)]);
    assert_eq!(harness.last_session().3.len(), 256);
}

#[test]
fn interval_keys_require_the_settings_revision() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.model.modules.settings = None;
    harness.keys(&[key('+')]);
    assert!(harness.submitted().is_empty());
    assert!(
        harness.controller.ui_state(harness.now).notices[0]
            .text
            .contains("revision unknown")
    );
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[key('+')]);
    assert_eq!(
        harness.submitted(),
        vec![CommandBody::SetSamplingInterval {
            interval_ms: 500,
            expected_config_revision: 7
        }]
    );
    harness.keys(&[key('-')]);
    assert_eq!(
        harness.submitted().len(),
        1,
        "one interval change at a time"
    );
}

#[test]
fn results_become_honest_notices() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.activate_confirmation();
    harness.keys(&[key('y')]);
    let confirm = harness.take().pop().unwrap();
    let mut result = CommandResult::completed(confirm.request_id, "sent");
    result.action_state = Some(ActionState::SignalSent);
    harness.deliver(result);
    let notices = harness.controller.ui_state(harness.now).notices;
    assert!(
        notices
            .iter()
            .any(|notice| notice.text.contains("signal sent") && notice.level == BannerLevel::Info)
    );
    assert!(
        !notices
            .iter()
            .any(|notice| notice.text.contains("terminated"))
    );
}

/// The History module's id.
fn history_module() -> ModuleId {
    ModuleId::new(crate::modules::HISTORY_MODULE_ID).unwrap()
}

/// A frame of the Overview History scrolled past `above` bands with
/// `below` bands waiting, as its time axis tells (D-107).
fn history_bands(harness: &mut Harness, above: usize, below: usize) {
    page_frame(harness, history_module(), (above, below));
}

/// A frame where `module` drew its page with `above` units scrolled past
/// and `below` waiting (D-107, D-110).
fn page_frame(harness: &mut Harness, module: ModuleId, (above, below): (usize, usize)) {
    let mut map = HitMap::new(harness.hit_map.frame_revision());
    let rows = ratatui::layout::Rect::new(1, 40, 120, 1);
    for (control_id, count) in [(controls::PAGE_ABOVE, above), (controls::PAGE_BELOW, below)] {
        for _ in 0..count {
            map.push_clipped(
                crate::registry::HitTarget {
                    frame_revision: 0,
                    module_id: module.clone(),
                    control_id,
                    entity_id: None,
                    process_key: None,
                    rect: rows,
                },
                rows,
            );
        }
    }
    harness.hit_map = map;
}

/// The Overview History's band offset the controller keeps.
fn history_offset(harness: &Harness) -> u16 {
    harness
        .controller
        .ui_state(harness.now)
        .offsets
        .get(&history_module())
        .copied()
        .unwrap_or(0)
}

/// The runs the controller has marked.
fn marked_pids(harness: &Harness) -> Vec<i32> {
    harness
        .controller
        .ui_state(harness.now)
        .marked
        .iter()
        .map(|key| key.pid.get())
        .collect()
}

/// Answers each DeleteRuns sent with `status` until none is left
/// unanswered (the next command leaves once the last is answered); the
/// commands' runs.
fn answer_deletions(harness: &mut Harness, completed: bool) -> Vec<Vec<ProcessKey>> {
    let mut answered = Vec::new();
    loop {
        let sent: Vec<CommandEnvelope> = harness
            .take()
            .into_iter()
            .filter(|envelope| matches!(envelope.body, CommandBody::DeleteRuns { .. }))
            .collect();
        if sent.is_empty() {
            return answered;
        }
        for envelope in sent {
            assert!(
                envelope.encoded_len() <= dgxtop_core::config::MIN_COMMAND_BYTES,
                "every command fits any Manager"
            );
            let CommandBody::DeleteRuns { runs } = envelope.body else {
                unreachable!()
            };
            answered.push(runs);
            let result = if completed {
                CommandResult::completed(envelope.request_id, "deleting")
            } else {
                CommandResult::rejected(envelope.request_id, ErrorCode::Busy, "busy")
            };
            harness.deliver(result);
        }
    }
}

/// The texts of the notices shown.
fn notice_texts(harness: &Harness) -> Vec<String> {
    harness
        .controller
        .ui_state(harness.now)
        .notices
        .into_iter()
        .map(|notice| notice.text)
        .collect()
}

/// The runs of each DeleteRuns sent.
fn deletions(harness: &Harness) -> Vec<Vec<i32>> {
    harness
        .submitted()
        .into_iter()
        .filter_map(|body| match body {
            CommandBody::DeleteRuns { runs } => {
                Some(runs.iter().map(|key| key.pid.get()).collect())
            }
            _ => None,
        })
        .collect()
}

/// ↑/↓ on the Overview History pick the GPU its GPU panels show, and so
/// does a click on the picker's chip (D-107). The view holds every GPU's
/// charts, so nothing is sent; on a host of one GPU the keys do nothing,
/// and on the Process History they move the run cursor.
#[test]
fn up_and_down_pick_the_gpu_the_overview_history_shows() {
    let mut harness = Harness::new(ViewKind::History);
    let gpus: Vec<EntityId> =
        crate::modules::history_gpus(harness.model.modules.history.as_deref().unwrap())
            .into_iter()
            .map(|(gpu, _)| gpu)
            .collect();
    assert_eq!(gpus.len(), 2);
    let picked = |harness: &Harness| {
        harness
            .controller
            .ui_state(harness.now)
            .picks
            .get(&history_module())
            .cloned()
    };
    harness.keys(&[code(KeyCode::Down)]);
    assert_eq!(picked(&harness), Some(gpus[1].clone()));
    harness.keys(&[key('j')]);
    assert_eq!(
        picked(&harness),
        Some(gpus[1].clone()),
        "the last GPU stays"
    );
    harness.keys(&[code(KeyCode::Up)]);
    assert_eq!(picked(&harness), Some(gpus[0].clone()));
    let chip = harness
        .hit_map
        .targets()
        .iter()
        .find(|target| {
            target.control_id == controls::HISTORY_GPU
                && target.entity_id.as_ref() == Some(&gpus[1])
        })
        .expect("the frame drew GPU 1's chip")
        .rect;
    harness.keys(&[click(chip.x, chip.y)]);
    assert_eq!(picked(&harness), Some(gpus[1].clone()));
    harness.keys(&[key('4'), code(KeyCode::Up)]);
    assert_eq!(
        picked(&harness),
        Some(gpus[1].clone()),
        "the Process History has no GPU panels"
    );
    assert!(
        harness.submitted().iter().all(|body| !matches!(
            body,
            CommandBody::UpdateViewSession {
                selected_entity_id: Some(_),
                ..
            }
        )),
        "the pick is presentation only: {:?}",
        harness.submitted()
    );
    let mut model = test_fixtures::view_model(ViewKind::History, 5);
    Arc::make_mut(model.modules.history.as_mut().unwrap())
        .charts
        .retain(|chart| chart.series.entity_id != gpus[1]);
    let mut single = Harness::with_model(model);
    single.keys(&[code(KeyCode::Down)]);
    assert_eq!(picked(&single), None, "one GPU: nothing to pick");
}

/// PgDn/PgUp scroll the Overview History a band at a time and g/G to the
/// first and the last bands, counted from the bands the frame showed:
/// PgDn stops at the last band, and after a taller frame PgUp moves at
/// once (D-107).
#[test]
fn the_overview_history_scrolls_from_the_bands_the_frame_showed() {
    let mut harness = Harness::new(ViewKind::History);
    history_bands(&mut harness, 0, 0);
    harness.keys(&[code(KeyCode::PageDown)]);
    assert_eq!(history_offset(&harness), 0, "everything shows");
    history_bands(&mut harness, 0, 2);
    harness.keys(&[code(KeyCode::PageDown)]);
    assert_eq!(history_offset(&harness), 1);
    history_bands(&mut harness, 1, 1);
    harness.keys(&[code(KeyCode::PageDown)]);
    assert_eq!(history_offset(&harness), 2);
    history_bands(&mut harness, 2, 0);
    harness.keys(&[code(KeyCode::PageDown)]);
    assert_eq!(history_offset(&harness), 2, "the last band shows");
    harness.keys(&[code(KeyCode::PageUp)]);
    assert_eq!(history_offset(&harness), 1);
    history_bands(&mut harness, 0, 3);
    harness.keys(&[shift('G')]);
    assert_eq!(history_offset(&harness), 3);
    // A taller terminal shows from the first band on.
    history_bands(&mut harness, 0, 0);
    harness.keys(&[code(KeyCode::PageUp)]);
    assert_eq!(history_offset(&harness), 0);
    history_bands(&mut harness, 2, 1);
    harness.keys(&[code(KeyCode::End)]);
    assert_eq!(history_offset(&harness), 3);
    harness.keys(&[key('g')]);
    assert_eq!(history_offset(&harness), 0);
    harness.keys(&[shift('G'), code(KeyCode::Home)]);
    assert_eq!(history_offset(&harness), 0);
    assert!(
        harness.submitted().is_empty(),
        "scrolling is presentation only: {:?}",
        harness.submitted()
    );
}

/// g/Home and G/End on the Process History's list: the newest and the
/// oldest run (D-108); on an opened run they turn to its first and last
/// page and leave the run open (D-116).
#[test]
fn the_process_history_goes_to_its_first_and_last_run() {
    let mut harness = Harness::new(ViewKind::History);
    let state = |harness: &Harness| {
        let ui = harness.controller.ui_state(harness.now);
        (
            ui.tabs.get(&history_module()).copied().unwrap_or(0),
            ui.scroll.get(&history_module()).copied().unwrap_or(0),
        )
    };
    harness.keys(&[key('4'), shift('G')]);
    assert_eq!(state(&harness), (1, 1));
    assert_eq!(harness.last_session().2.unwrap().pid.get(), 41);
    harness.keys(&[key('g')]);
    assert_eq!(state(&harness), (1, 0));
    assert_eq!(harness.last_session().2.unwrap().pid.get(), 42);
    harness.keys(&[code(KeyCode::End), code(KeyCode::Enter)]);
    assert_eq!(state(&harness), (2, 1));
    page_frame(&mut harness, history_module(), (1, 1));
    harness.keys(&[code(KeyCode::Home)]);
    assert_eq!(state(&harness), (2, 1), "the run stays open");
    assert_eq!(history_offset(&harness), 0, "at its first page");
    assert_eq!(harness.last_session().2.unwrap().pid.get(), 41);
}

/// Space marks the run under the cursor and goes to the next; `a` marks
/// every ended run, or unmarks all once they all are; Esc forgets the
/// marks. Marks are made on the list only (D-108).
#[test]
fn runs_are_marked_one_by_one_or_all_at_once() {
    let mut harness = Harness::new(ViewKind::History);
    harness.keys(&[key(' '), key('a')]);
    assert!(
        marked_pids(&harness).is_empty(),
        "the Overview History lists no runs"
    );
    harness.keys(&[key('4'), key(' ')]);
    assert_eq!(marked_pids(&harness), vec![42]);
    assert_eq!(
        harness.last_session().2.unwrap().pid.get(),
        41,
        "the cursor went to the next run"
    );
    harness.keys(&[key(' ')]);
    assert_eq!(
        marked_pids(&harness),
        vec![41, 42],
        "a running run is marked too"
    );
    harness.keys(&[key(' ')]);
    assert_eq!(marked_pids(&harness), vec![42], "Space again unmarks it");
    harness.keys(&[key('a')]);
    assert!(
        marked_pids(&harness).is_empty(),
        "every ended run was marked: all are unmarked"
    );
    harness.keys(&[key('a')]);
    assert_eq!(marked_pids(&harness), vec![42], "the ended runs only");
    harness.keys(&[code(KeyCode::Enter), key(' '), key('a')]);
    assert_eq!(
        marked_pids(&harness),
        vec![42],
        "an opened run takes no marks"
    );
    harness.keys(&[code(KeyCode::Esc)]);
    assert!(marked_pids(&harness).is_empty());
    assert!(deletions(&harness).is_empty());
}

/// `d` asks before anything is deleted, then sends the ended runs among
/// the marked ones, else the run under the cursor; a running run stays.
/// n/Esc sends nothing (D-108).
#[test]
fn d_deletes_the_ended_runs_it_asked_about() {
    let mut harness = Harness::new(ViewKind::History);
    harness.keys(&[key('d')]);
    assert_eq!(
        harness.controller.mode(),
        ModeKind::Normal,
        "no run on the Overview History"
    );
    harness.keys(&[key('4'), key('d')]);
    assert_eq!(
        harness.controller.ui_state(harness.now).mode,
        ModeView::ConfirmDelete {
            runs: 1,
            running: 0
        }
    );
    harness.keys(&[key('n')]);
    assert_eq!(harness.controller.mode(), ModeKind::Normal);
    harness.keys(&[code(KeyCode::Delete), code(KeyCode::Esc)]);
    assert_eq!(harness.controller.mode(), ModeKind::Normal);
    assert!(deletions(&harness).is_empty(), "nothing is sent unasked");
    harness.keys(&[key('d'), key('y')]);
    assert_eq!(deletions(&harness), vec![vec![42]]);
    assert_eq!(harness.controller.mode(), ModeKind::Normal);
    harness.keys(&[key('d')]);
    assert!(
        notice_texts(&harness).contains(&"still deleting: 1 run left".to_owned()),
        "one deletion at a time: {:?}",
        notice_texts(&harness)
    );
    assert_eq!(harness.controller.mode(), ModeKind::Normal);
    answer_deletions(&mut harness, true);
    // The running run under the cursor: nothing to ask.
    harness.keys(&[key('j'), key('d')]);
    assert_eq!(harness.controller.mode(), ModeKind::Normal);
    let notices = harness.controller.ui_state(harness.now).notices;
    assert!(
        notices
            .iter()
            .any(|notice| notice.text.contains("a running run stays")),
        "{notices:?}"
    );
    // Both marked: the ended one goes, the running one stays marked.
    harness.keys(&[key('g'), key(' '), key(' '), key('d')]);
    assert_eq!(
        harness.controller.ui_state(harness.now).mode,
        ModeView::ConfirmDelete {
            runs: 1,
            running: 1
        }
    );
    harness.keys(&[code(KeyCode::Enter)]);
    assert_eq!(deletions(&harness), vec![vec![42]]);
    assert_eq!(
        marked_pids(&harness),
        vec![41, 42],
        "marks go once the deletion is answered"
    );
    answer_deletions(&mut harness, true);
    assert_eq!(marked_pids(&harness), vec![41]);
}

/// On an opened run `d` deletes that run, whatever is marked, then the
/// page goes back to the list; the result is said in a notice (D-108).
#[test]
fn an_opened_run_is_deleted_alone_and_the_list_comes_back() {
    let mut harness = Harness::new(ViewKind::History);
    harness.keys(&[key('4'), key('j'), key(' '), key('g'), code(KeyCode::Enter)]);
    assert_eq!(marked_pids(&harness), vec![41]);
    harness.keys(&[key('d')]);
    assert_eq!(
        harness.controller.ui_state(harness.now).mode,
        ModeView::ConfirmDelete {
            runs: 1,
            running: 0
        }
    );
    harness.keys(&[key('y')]);
    let sent = harness.take();
    let deletion = sent
        .iter()
        .find(|envelope| matches!(envelope.body, CommandBody::DeleteRuns { .. }))
        .expect("DeleteRuns sent");
    let CommandBody::DeleteRuns { runs } = &deletion.body else {
        unreachable!()
    };
    assert_eq!(
        runs.iter().map(|key| key.pid.get()).collect::<Vec<_>>(),
        [42]
    );
    assert_eq!(
        harness
            .controller
            .ui_state(harness.now)
            .tabs
            .get(&history_module())
            .copied(),
        Some(crate::views::PROCESS_LIST_PAGE)
    );
    harness.deliver(CommandResult::completed(
        deletion.request_id,
        "deleting 1 run; running ones stay",
    ));
    let notices = harness.controller.ui_state(harness.now).notices;
    assert!(
        notices
            .iter()
            .any(|notice| notice.text == "deleted 1 run from the Process History"),
        "{notices:?}"
    );
}

/// A deletion of many runs goes out a command at a time, each command
/// within the smallest byte cap a Manager has, the next once the last is
/// answered; its outcome is told once, at the end (D-108).
#[test]
fn a_large_deletion_is_sent_a_bounded_command_at_a_time() {
    let mut model = test_fixtures::view_model(ViewKind::History, 5);
    let runs = Arc::make_mut(model.modules.process_history.as_mut().unwrap());
    let ended = runs.runs[0].clone();
    runs.runs = (0..300)
        .map(|index| {
            let mut run = ended.clone();
            run.key.pid = dgxtop_core::identity::Pid::new(1_000 + index).unwrap();
            run
        })
        .collect();
    runs.opened = None;
    let mut harness = Harness::with_model(model);
    harness.keys(&[key('4'), key('a'), key('d')]);
    assert_eq!(
        harness.controller.ui_state(harness.now).mode,
        ModeView::ConfirmDelete {
            runs: 300,
            running: 0
        }
    );
    harness.keys(&[key('y')]);
    assert_eq!(deletions(&harness).len(), 1, "one command at a time");
    let answered = answer_deletions(&mut harness, true);
    assert!(answered.len() > 1, "{} commands", answered.len());
    let pids: Vec<i32> = answered.concat().iter().map(|key| key.pid.get()).collect();
    assert_eq!(pids, (1_000..1_300).collect::<Vec<_>>(), "every run once");
    assert!(marked_pids(&harness).is_empty());
    let notices = notice_texts(&harness);
    assert_eq!(
        notices
            .iter()
            .filter(|text| text.starts_with("deleted"))
            .collect::<Vec<_>>(),
        ["deleted 300 runs from the Process History"]
    );
}

/// A command of the deletion refused stops it: the runs not deleted keep
/// their marks, and the notices say why and how many (D-108).
#[test]
fn a_refused_deletion_stops_and_keeps_the_marks() {
    let mut model = test_fixtures::view_model(ViewKind::History, 5);
    let runs = Arc::make_mut(model.modules.process_history.as_mut().unwrap());
    let ended = runs.runs[0].clone();
    runs.runs = (0..100)
        .map(|index| {
            let mut run = ended.clone();
            run.key.pid = dgxtop_core::identity::Pid::new(2_000 + index).unwrap();
            run
        })
        .collect();
    runs.opened = None;
    let mut harness = Harness::with_model(model);
    harness.keys(&[key('4'), key('a'), key('d'), key('y')]);
    let refused = answer_deletions(&mut harness, false);
    assert_eq!(refused.len(), 1, "nothing more is sent");
    assert_eq!(marked_pids(&harness).len(), 100);
    let notices = notice_texts(&harness);
    assert!(
        notices
            .iter()
            .any(|text| text.starts_with("runs not deleted: ")),
        "{notices:?}"
    );
    assert!(
        notices
            .iter()
            .any(|text| text == "the deletion stopped: 0 runs deleted, 100 not"),
        "{notices:?}"
    );
    harness.keys(&[key('d')]);
    assert_eq!(
        harness.controller.ui_state(harness.now).mode,
        ModeView::ConfirmDelete {
            runs: 100,
            running: 0
        },
        "asked again"
    );
}

/// A deletion's command without an answer in time stops the deletion:
/// whether it deleted its runs is unknown, so nothing more is sent.
#[test]
fn an_unanswered_deletion_stops_with_a_warning() {
    let mut harness = Harness::new(ViewKind::History);
    harness.keys(&[key('4'), key('d'), key('y')]);
    assert_eq!(deletions(&harness).len(), 1);
    harness.now += Duration::from_secs(61);
    harness.controller.tick(harness.now);
    let notices = notice_texts(&harness);
    assert!(
        notices.iter().any(|text| {
            text == "no answer to the deletion; it stopped with 1 run not known deleted"
        }),
        "{notices:?}"
    );
    harness.keys(&[key('d')]);
    assert_eq!(
        harness.controller.mode(),
        ModeKind::Confirm,
        "a new deletion may be asked"
    );
}

/// While a signal waits for its confirmation the Process History does not
/// ask to delete, so y/Enter always answer the question shown (D-108): the
/// keys that would ask wait with the rest (D-109).
#[test]
fn a_pending_signal_keeps_the_deletion_question_away() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[shift('K')]);
    let prepare = harness.take().pop().expect("PrepareSignal submitted");
    assert!(matches!(prepare.body, CommandBody::PrepareSignal { .. }));
    let mut model = test_fixtures::view_model(ViewKind::History, 6);
    model.modules = harness.model.modules.clone();
    harness.project(model);
    harness.keys(&[key('4'), key('d')]);
    assert!(
        !matches!(
            harness.controller.ui_state(harness.now).mode,
            ModeView::ConfirmDelete { .. }
        ),
        "the signal's answer is awaited"
    );
    let mut result = CommandResult::completed(prepare.request_id, "prepared");
    result.confirmation_token = Some(token());
    result.expires_boot_ns = Some(harness.model.generated_boot_ns + 9_000_000_000);
    harness.deliver(result);
    assert_eq!(harness.controller.mode(), ModeKind::Confirm);
    harness.keys(&[key('y')]);
    let bodies = harness.submitted();
    assert!(
        bodies
            .iter()
            .any(|body| matches!(body, CommandBody::ConfirmSignal { .. })),
        "{bodies:?}"
    );
    assert!(deletions(&harness).is_empty());
}

/// The prepare of a signal that `F9` sent, and its token-bearing result.
fn prepared_f9(harness: &mut Harness) -> (CommandEnvelope, CommandResult) {
    harness.keys(&[code(KeyCode::F(9))]);
    let prepare = harness
        .take()
        .into_iter()
        .rev()
        .find(|envelope| matches!(envelope.body, CommandBody::PrepareSignal { .. }))
        .expect("PrepareSignal submitted");
    let mut result = CommandResult::completed(prepare.request_id, "prepared");
    result.confirmation_token = Some(token());
    result.expires_boot_ns = Some(harness.model.generated_boot_ns + 9_000_000_000);
    (prepare, result)
}

/// y/Enter pressed before the token arrives are kept, and the kill goes
/// out the moment the token does: a fast F9 then y is never lost (D-109).
/// The banner says what will happen.
#[test]
fn a_kill_confirmed_before_its_token_is_sent_when_the_token_arrives() {
    for confirm_key in [key('y'), code(KeyCode::Enter)] {
        let mut harness = Harness::new(ViewKind::Overview);
        let (_, result) = prepared_f9(&mut harness);
        assert_eq!(
            harness.controller.mode(),
            ModeKind::Confirm,
            "it takes its answer"
        );
        harness.keys(&[confirm_key]);
        let ui = harness.controller.ui_state(harness.now);
        assert_eq!(ui.early_answer, Some(true));
        assert!(harness.submitted().is_empty(), "nothing without the token");
        harness.deliver(result);
        let submitted = harness.submitted();
        assert!(
            matches!(&submitted[..], [CommandBody::ConfirmSignal { confirmation_token }] if *confirmation_token == token()),
            "{submitted:?}"
        );
        assert_eq!(
            harness.controller.mode(),
            ModeKind::Normal,
            "the token is used"
        );
    }
}

/// n/Esc before the token arrives cancel it on arrival, so nothing is
/// sent; the last answer counts (D-109).
#[test]
fn a_kill_cancelled_before_its_token_is_never_sent() {
    let mut harness = Harness::new(ViewKind::Overview);
    let (_, result) = prepared_f9(&mut harness);
    harness.keys(&[key('y'), code(KeyCode::Esc)]);
    assert_eq!(
        harness.controller.ui_state(harness.now).early_answer,
        Some(false)
    );
    harness.deliver(result);
    let submitted = harness.submitted();
    assert!(
        matches!(&submitted[..], [CommandBody::CancelSignal { confirmation_token }] if *confirmation_token == token()),
        "{submitted:?}"
    );
    assert_eq!(harness.controller.mode(), ModeKind::Normal);
    // Other keys wait while the token is on its way.
    let mut waiting = Harness::new(ViewKind::Overview);
    prepared_f9(&mut waiting);
    waiting.keys(&[key('j'), key('/'), code(KeyCode::Tab)]);
    assert!(waiting.submitted().is_empty(), "{:?}", waiting.submitted());
    assert_eq!(waiting.controller.mode(), ModeKind::Confirm);
}

/// K and F9 act on the row the user selected last, though the frame still
/// highlights the one before (the highlight follows a step later); a
/// selection the frame did not draw is never a target (D-109).
#[test]
fn a_kill_targets_the_row_selected_last_when_it_was_drawn() {
    let mut harness = Harness::new(ViewKind::Overview);
    let drawn: Vec<ProcessKey> = harness
        .hit_map
        .targets()
        .iter()
        .filter(|target| target.control_id == controls::PROCESS_ROW)
        .filter_map(|target| target.process_key.clone())
        .collect();
    let highlighted = harness.hit_map.visible_selected_process().cloned().unwrap();
    let next = drawn.first().cloned().expect("another row is drawn");
    // A click selects another drawn row; F9 at once, before a new frame.
    let row = harness
        .hit_map
        .targets()
        .iter()
        .find(|target| target.process_key.as_ref() == Some(&next))
        .unwrap()
        .rect;
    harness.press(click(row.x, row.y));
    harness.keys(&[code(KeyCode::F(9))]);
    let targets: Vec<ProcessKey> = harness
        .submitted()
        .into_iter()
        .filter_map(|body| match body {
            CommandBody::PrepareSignal { target, .. } => Some(target),
            _ => None,
        })
        .collect();
    assert_eq!(
        targets,
        vec![next.clone()],
        "not the stale highlight {highlighted:?}"
    );
    // The selection goes out before the prepare, so the Manager checks the
    // prepare against it.
    let bodies = harness.submitted();
    let selection = bodies
        .iter()
        .position(|body| matches!(body, CommandBody::UpdateViewSession { selected_process: Some(key), .. } if *key == next))
        .expect("selection sent");
    let prepare = bodies
        .iter()
        .position(|body| matches!(body, CommandBody::PrepareSignal { .. }))
        .unwrap();
    assert!(selection < prepare);
    // A selection the frame did not draw falls back to the drawn highlight.
    let mut offscreen = Harness::new(ViewKind::Overview);
    let mut map = HitMap::new(offscreen.hit_map.frame_revision());
    for target in offscreen.hit_map.targets() {
        if target.control_id != controls::PROCESS_ROW {
            map.push_clipped(target.clone(), target.rect);
        }
    }
    offscreen.hit_map = map;
    offscreen.press(click(row.x, row.y));
    offscreen.keys(&[code(KeyCode::F(9))]);
    let fallback: Vec<ProcessKey> = offscreen
        .submitted()
        .into_iter()
        .filter_map(|body| match body {
            CommandBody::PrepareSignal { target, .. } => Some(target),
            _ => None,
        })
        .collect();
    assert_eq!(fallback, vec![highlighted]);
}

/// In the GPU view ↑/↓ (j/k, PgUp/PgDn) turn the pages of the GPU's
/// sections and Home/End go to the first and the last, counted from the
/// pages the frame showed, never past the last; ←/→ still pick the GPU,
/// and turning pages sends nothing (D-110).
#[test]
fn the_gpu_view_turns_the_pages_of_its_sections() {
    let mut harness = Harness::new(ViewKind::GpuDetail);
    let gpu = ModuleId::new(crate::modules::GPU_MODULE_ID).unwrap();
    let page = |harness: &Harness| {
        harness
            .controller
            .ui_state(harness.now)
            .offsets
            .get(&gpu)
            .copied()
            .unwrap_or(0)
    };
    page_frame(&mut harness, gpu.clone(), (0, 0));
    harness.keys(&[code(KeyCode::Down)]);
    assert_eq!(page(&harness), 0, "one page");
    page_frame(&mut harness, gpu.clone(), (0, 2));
    harness.keys(&[key('j')]);
    assert_eq!(page(&harness), 1);
    page_frame(&mut harness, gpu.clone(), (1, 1));
    harness.keys(&[code(KeyCode::PageDown)]);
    assert_eq!(page(&harness), 2);
    page_frame(&mut harness, gpu.clone(), (2, 0));
    harness.keys(&[code(KeyCode::Down)]);
    assert_eq!(page(&harness), 2, "the last page shows");
    harness.keys(&[key('k')]);
    assert_eq!(page(&harness), 1);
    page_frame(&mut harness, gpu.clone(), (1, 1));
    harness.keys(&[code(KeyCode::End)]);
    assert_eq!(page(&harness), 2);
    harness.keys(&[code(KeyCode::Home)]);
    assert_eq!(page(&harness), 0);
    assert!(harness.submitted().is_empty(), "{:?}", harness.submitted());
    harness.keys(&[code(KeyCode::Right)]);
    assert!(harness.last_session().1.is_some(), "→ picks the next GPU");
}

/// In the filter input Ctrl-B/F/A/E move the cursor like ←/→/Home/End, for
/// a keyboard without them (D-111); they are never typed as text.
#[test]
fn readline_keys_move_the_filter_cursor() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[key('/'), key('a'), key('b'), key('c')]);
    harness.keys(&[
        ctrl('a'),
        key('x'),
        ctrl('e'),
        key('y'),
        ctrl('b'),
        key('z'),
        ctrl('f'),
        key('w'),
    ]);
    let ModeView::Filter { buffer, .. } = harness.controller.ui_state(harness.now).mode else {
        panic!("typing");
    };
    assert_eq!(buffer, "xabczyw");
}

/// On an opened run ↑/↓ (j/k), PgDn/PgUp (Ctrl-D/Ctrl-U) and g/G turn its
/// pages, counted from the pages the frame showed, and no key switches to
/// another run (D-112, D-116). Closing the run or the Overview History
/// starts from the first page again, and so does the next run opened.
#[test]
fn an_opened_run_turns_its_pages_and_never_switches_the_run() {
    let mut harness = Harness::new(ViewKind::History);
    let tab = |harness: &Harness| {
        harness
            .controller
            .ui_state(harness.now)
            .tabs
            .get(&history_module())
            .copied()
    };
    harness.keys(&[key('4'), code(KeyCode::Enter)]);
    let opened = harness.last_session().2;
    page_frame(&mut harness, history_module(), (0, 2));
    harness.keys(&[code(KeyCode::Down)]);
    assert_eq!(history_offset(&harness), 1);
    page_frame(&mut harness, history_module(), (1, 1));
    harness.keys(&[key('j')]);
    assert_eq!(history_offset(&harness), 2);
    page_frame(&mut harness, history_module(), (2, 0));
    harness.keys(&[code(KeyCode::Down)]);
    assert_eq!(history_offset(&harness), 2, "the last page shows");
    harness.keys(&[code(KeyCode::Up)]);
    assert_eq!(history_offset(&harness), 1);
    page_frame(&mut harness, history_module(), (1, 1));
    harness.keys(&[key('k')]);
    assert_eq!(history_offset(&harness), 0);
    page_frame(&mut harness, history_module(), (0, 2));
    harness.keys(&[shift('G')]);
    assert_eq!(history_offset(&harness), 2, "G: the last page");
    page_frame(&mut harness, history_module(), (2, 0));
    harness.keys(&[key('g')]);
    assert_eq!(history_offset(&harness), 0, "g: the first page");
    page_frame(&mut harness, history_module(), (0, 2));
    harness.keys(&[ctrl('d')]);
    assert_eq!(history_offset(&harness), 1);
    page_frame(&mut harness, history_module(), (1, 1));
    harness.keys(&[code(KeyCode::PageUp)]);
    assert_eq!(history_offset(&harness), 0);
    assert_eq!(harness.last_session().2, opened, "the run stays selected");
    assert_eq!(tab(&harness), Some(crate::views::RUN_PAGE), "and open");
    // Back on the list and into the next run: its first page.
    page_frame(&mut harness, history_module(), (0, 1));
    harness.keys(&[code(KeyCode::PageDown)]);
    assert_eq!(history_offset(&harness), 1);
    harness.keys(&[code(KeyCode::Enter), key('j'), code(KeyCode::Enter)]);
    assert_eq!(
        history_offset(&harness),
        0,
        "the next run starts at its first page"
    );
    assert_ne!(harness.last_session().2, opened);
    page_frame(&mut harness, history_module(), (0, 1));
    harness.keys(&[code(KeyCode::PageDown), key('3')]);
    assert_eq!(
        history_offset(&harness),
        0,
        "the Overview History from its top"
    );
    assert!(
        harness
            .submitted()
            .iter()
            .all(|body| matches!(body, CommandBody::UpdateViewSession { .. })),
        "turning pages sends nothing: {:?}",
        harness.submitted()
    );
}

/// `/` on the Process History filters its runs by name or PID (D-113): the
/// History session carries the filter, the list starts at its first
/// match, an opened run leaves for the list, and the Overview History has
/// no list to filter.
#[test]
fn slash_filters_the_process_historys_runs() {
    let mut harness = Harness::new(ViewKind::History);
    harness.keys(&[key('/')]);
    assert_eq!(
        harness.controller.mode(),
        ModeKind::Normal,
        "no list on the Overview History"
    );
    harness.keys(&[key('4'), key('j'), code(KeyCode::Enter), key('/')]);
    assert_eq!(harness.controller.mode(), ModeKind::Filter);
    assert_eq!(
        harness
            .controller
            .ui_state(harness.now)
            .tabs
            .get(&history_module())
            .copied(),
        Some(crate::views::PROCESS_LIST_PAGE),
        "the run leaves for the list"
    );
    harness.keys(&[key('c'), key('o'), key('m'), code(KeyCode::Enter)]);
    let (view, _, selected, filter, _) = harness.last_session();
    assert_eq!((view, filter.as_str()), (ViewKind::History, "com"));
    assert_eq!(selected, None, "the list starts at its first match");
    assert_eq!(
        harness
            .controller
            .ui_state(harness.now)
            .scroll
            .get(&history_module())
            .copied(),
        Some(0)
    );
    harness.keys(&[key('/'), code(KeyCode::Esc)]);
    assert_eq!(harness.last_session().3, "", "Esc clears it");
}

/// The panels folded now, in order.
fn folded_now(harness: &Harness) -> Vec<String> {
    harness
        .controller
        .ui_state(harness.now)
        .folded
        .iter()
        .cloned()
        .collect()
}

/// The saves of folded panels sent and not yet taken.
fn fold_saves(harness: &Harness) -> Vec<(u128, Vec<String>)> {
    harness
        .sink
        .submitted
        .borrow()
        .iter()
        .filter_map(|envelope| match &envelope.body {
            CommandBody::SetFolded { panels } => {
                Some((envelope.request_id.as_u128(), panels.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The last fold save sent.
fn last_fold_save(harness: &Harness) -> CommandEnvelope {
    harness
        .sink
        .submitted
        .borrow()
        .iter()
        .rev()
        .find(|envelope| matches!(envelope.body, CommandBody::SetFolded { .. }))
        .cloned()
        .expect("a fold save")
}

/// `i`/`n` fold the Overview's disk I/O and network panels on the next
/// frame, and the set is saved through the Manager one command at a time,
/// a busy save tried again; `o` unfolds them (D-114).
#[test]
fn overview_panels_fold_at_once_and_are_saved_one_save_at_a_time() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[key('i')]);
    assert_eq!(folded_now(&harness), ["disk"]);
    let saves = fold_saves(&harness);
    assert_eq!(saves.len(), 1);
    assert_eq!(saves[0].1, ["disk"]);
    harness.keys(&[key('n')]);
    assert_eq!(folded_now(&harness), ["disk", "network"], "shown at once");
    assert_eq!(fold_saves(&harness).len(), 1, "one save at a time");
    let first = last_fold_save(&harness);
    harness.deliver(CommandResult::completed(first.request_id, "saved"));
    let saves = fold_saves(&harness);
    assert_eq!(saves.len(), 2);
    assert_eq!(saves[1].1, ["disk", "network"], "the rest follows");
    let busy = last_fold_save(&harness);
    harness.deliver(CommandResult::rejected(
        busy.request_id,
        ErrorCode::Busy,
        "busy",
    ));
    harness.keys(&[]);
    assert_eq!(fold_saves(&harness).len(), 2, "a moment later");
    harness.now += Duration::from_millis(300);
    harness.keys(&[]);
    assert_eq!(fold_saves(&harness).len(), 3, "tried again");
    let again = last_fold_save(&harness);
    harness.deliver(CommandResult::completed(again.request_id, "saved"));
    harness.keys(&[key('o')]);
    assert!(folded_now(&harness).is_empty());
    let saves = fold_saves(&harness);
    assert_eq!(saves.last().unwrap().1, Vec::<String>::new());
}

/// The saved folds show from the first projection, nothing sent; the
/// Overview History's panels fold on its first page only (D-114).
#[test]
fn saved_folds_show_at_start_and_history_panels_fold_on_their_page() {
    let mut harness = Harness::new(ViewKind::History);
    let mut model = test_fixtures::view_model(ViewKind::History, 6);
    model.folded = vec!["history.disk".to_owned()];
    harness.project(model);
    assert_eq!(folded_now(&harness), ["history.disk"]);
    assert!(fold_saves(&harness).is_empty(), "nothing to save");
    harness.keys(&[key('c'), key('i')]);
    assert_eq!(folded_now(&harness), ["history.cpu"]);
    harness.keys(&[key('4'), key('m'), key('o')]);
    assert_eq!(
        folded_now(&harness),
        ["history.cpu"],
        "not on the Process History"
    );
    harness.keys(&[key('3'), key('o')]);
    assert!(folded_now(&harness).is_empty());
}
