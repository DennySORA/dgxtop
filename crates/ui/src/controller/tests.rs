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

/// ←/→ switch the history bar size in History and the GPU in the GPU view:
/// one key, the action of the page it is pressed on (D-70).
#[test]
fn arrows_act_on_the_page_they_are_pressed_on() {
    use crate::controller::keymap::{KeyAction, ModeKind, resolve};
    let arrow = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
    assert_eq!(
        resolve(&arrow(KeyCode::Right), ModeKind::Normal, ViewKind::History),
        Some(KeyAction::NextHistoryBarSize)
    );
    assert_eq!(
        resolve(&arrow(KeyCode::Left), ModeKind::Normal, ViewKind::History),
        Some(KeyAction::PreviousHistoryBarSize)
    );
    assert_eq!(
        resolve(
            &arrow(KeyCode::Right),
            ModeKind::Normal,
            ViewKind::GpuDetail
        ),
        Some(KeyAction::NextGpu)
    );
    assert_eq!(
        resolve(&arrow(KeyCode::Left), ModeKind::Normal, ViewKind::GpuDetail),
        Some(KeyAction::PreviousGpu)
    );
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
/// cursor stays on the selected run, so Enter opens it (D-77).
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
    assert_eq!(bars(&harness), (BarSize::FiveMinutes, BarSize::Auto));
    harness.keys(&[code(KeyCode::Left), key('3'), key('W'), key('W')]);
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

#[test]
fn a_token_arriving_while_typing_is_cancelled_not_activated() {
    let mut harness = Harness::new(ViewKind::Overview);
    harness.keys(&[shift('K')]);
    let prepare = harness.take().pop().unwrap();
    harness.keys(&[key('/')]);
    let mut result = CommandResult::completed(prepare.request_id, "prepared");
    result.confirmation_token = Some(token());
    harness.deliver(result);
    assert_eq!(harness.controller.mode(), ModeKind::Filter);
    let submitted = harness.submitted();
    assert!(matches!(&submitted[0], CommandBody::CancelSignal { .. }));
    harness.keys(&[key('y')]);
    let ModeView::Filter { buffer, .. } = harness.controller.ui_state(harness.now).mode else {
        panic!("still typing");
    };
    assert_eq!(buffer, "y", "y stays text; nothing is confirmed");
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
