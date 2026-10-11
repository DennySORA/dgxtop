//! Runner driven deterministically: fake SessionPort, scripted input on a
//! fake clock and a fake TerminalTransport that can accept N bytes per call,
//! block, block for a while, or fail.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use dgxtop_core::commands::{
    CommandBody, CommandEnvelope, CommandRejection, CommandResult, ViewKind,
};
use dgxtop_core::identity::SessionId;
use dgxtop_core::ports::{SessionPort, TerminalTransport};
use dgxtop_core::view::ViewModel;

use super::*;
use crate::controller::ModeKind;
use crate::terminal::AGE_TICK;
use crate::terminal::output::CONTROL_RESERVE;
use crate::test_fixtures;

#[derive(Clone, Default)]
struct FakeTime(Arc<Mutex<Duration>>);

impl FakeTime {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
}

struct FakeClock(FakeTime);

impl LoopClock for FakeClock {
    fn now(&self) -> Duration {
        self.0.now()
    }
}

enum Step {
    Event(Event),
    Wait(Duration),
    HangUp,
}

#[derive(Clone, Default)]
struct Script(Arc<Mutex<VecDeque<Step>>>);

impl Script {
    fn push(&self, step: Step) {
        self.0.lock().unwrap().push_back(step);
    }

    fn key(&self, character: char) {
        let modifiers = if character.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        self.push(Step::Event(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            modifiers,
        ))));
    }
}

/// Polls consume fake time: a wait step or an empty script advances the
/// clock by the poll timeout, exactly like a real blocking poll would.
struct ScriptedInput {
    time: FakeTime,
    script: Script,
    polls: Arc<Mutex<Vec<(Duration, bool)>>>,
}

impl InputSource for ScriptedInput {
    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>, TuiError> {
        let mut script = self.script.0.lock().unwrap();
        let event = match script.front_mut() {
            // A hung-up terminal stays hung up.
            Some(Step::HangUp) => return Err(TuiError::InputClosed),
            Some(Step::Event(_)) => match script.pop_front() {
                Some(Step::Event(event)) => Some(event),
                _ => None,
            },
            Some(Step::Wait(remaining)) => {
                let spent = (*remaining).min(timeout);
                self.time.advance(spent);
                *remaining -= spent;
                if remaining.is_zero() {
                    script.pop_front();
                }
                None
            }
            None => {
                self.time.advance(timeout);
                None
            }
        };
        self.polls.lock().unwrap().push((timeout, event.is_some()));
        Ok(event)
    }
}

#[derive(Clone, Copy)]
enum Mode {
    AcceptAll,
    PerCall(usize),
    Blocked,
    BlockedUntil(Duration),
    Fail(io::ErrorKind),
}

struct TransportState {
    accepted: Vec<u8>,
    mode: Mode,
    restored: usize,
}

struct FakeTransport {
    state: Arc<Mutex<TransportState>>,
    time: FakeTime,
}

impl TerminalTransport for FakeTransport {
    fn write_nonblocking(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap();
        let accepted = match state.mode {
            Mode::AcceptAll => bytes.len(),
            Mode::PerCall(limit) => limit.min(bytes.len()),
            Mode::Blocked => 0,
            Mode::BlockedUntil(until) => {
                if self.time.now() < until {
                    0
                } else {
                    bytes.len()
                }
            }
            Mode::Fail(kind) => return Err(io::Error::from(kind)),
        };
        state.accepted.extend_from_slice(&bytes[..accepted]);
        Ok(accepted)
    }

    fn wait_writable(&mut self, timeout: Duration) -> io::Result<bool> {
        self.time.advance(timeout);
        Ok(!matches!(self.state.lock().unwrap().mode, Mode::Blocked))
    }

    fn restore(&mut self) -> io::Result<()> {
        self.state.lock().unwrap().restored += 1;
        Ok(())
    }
}

#[derive(Default)]
struct SessionState {
    submitted: Vec<CommandEnvelope>,
    slot: Option<Arc<ViewModel>>,
    results: VecDeque<CommandResult>,
    shutdown_reasons: Vec<String>,
    manager_shutdown: bool,
    viewports: Vec<(u16, u16)>,
    takes: usize,
}

struct FakeSession {
    id: SessionId,
    state: Arc<Mutex<SessionState>>,
}

impl SessionPort for FakeSession {
    fn session_id(&self) -> &SessionId {
        &self.id
    }

    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection> {
        self.state.lock().unwrap().submitted.push(command);
        Ok(())
    }

    fn take_projection(&self) -> Option<Arc<ViewModel>> {
        let mut state = self.state.lock().unwrap();
        let projection = state.slot.take();
        if projection.is_some() {
            state.takes += 1;
        }
        projection
    }

    fn drain_results(&self, max: usize) -> Vec<CommandResult> {
        let mut state = self.state.lock().unwrap();
        let count = max.min(state.results.len());
        state.results.drain(..count).collect()
    }

    fn request_shutdown(&self, reason: &str) {
        self.state
            .lock()
            .unwrap()
            .shutdown_reasons
            .push(reason.to_owned());
    }

    fn shutdown_requested(&self) -> bool {
        self.state.lock().unwrap().manager_shutdown
    }

    fn set_viewport(&self, columns: u16, rows: u16) {
        self.state.lock().unwrap().viewports.push((columns, rows));
    }
}

struct Rig {
    time: FakeTime,
    script: Script,
    polls: Arc<Mutex<Vec<(Duration, bool)>>>,
    session: Arc<Mutex<SessionState>>,
    transport: Arc<Mutex<TransportState>>,
    tui: TuiLoop,
}

impl Rig {
    fn new(mode: Mode, viewport: (u16, u16)) -> Self {
        Self::with_capacity(mode, viewport, OUTPUT_BUFFER_CAPACITY)
    }

    fn with_capacity(mode: Mode, viewport: (u16, u16), capacity: usize) -> Self {
        let time = FakeTime::default();
        let script = Script::default();
        let polls = Arc::new(Mutex::new(Vec::new()));
        let session = Arc::new(Mutex::new(SessionState::default()));
        let transport = Arc::new(Mutex::new(TransportState {
            accepted: Vec::new(),
            mode,
            restored: 0,
        }));
        let tui = TuiLoop::with_output_capacity(
            Box::new(FakeSession {
                id: SessionId::new("tui").unwrap(),
                state: Arc::clone(&session),
            }),
            Box::new(FakeTransport {
                state: Arc::clone(&transport),
                time: time.clone(),
            }),
            Box::new(ScriptedInput {
                time: time.clone(),
                script: script.clone(),
                polls: Arc::clone(&polls),
            }),
            Box::new(FakeClock(time.clone())),
            TuiOptions::default(),
            viewport,
            capacity,
        )
        .unwrap();
        Self {
            time,
            script,
            polls,
            session,
            transport,
            tui,
        }
    }

    fn publish(&self, model: ViewModel) {
        self.session.lock().unwrap().slot = Some(Arc::new(model));
    }

    fn publish_revision(&self, view: ViewKind, revision: u64) {
        self.publish(test_fixtures::view_model(view, revision));
    }

    /// Steps until `done` holds (or the bound is hit); returns an exit reason if one occurred.
    fn steps_until(
        &mut self,
        bound: usize,
        mut done: impl FnMut(&Self) -> bool,
    ) -> Option<TuiExitReason> {
        for _ in 0..bound {
            if done(self) {
                return None;
            }
            if let Some(reason) = self.tui.step().unwrap() {
                return Some(reason);
            }
        }
        None
    }

    fn frames(&self) -> u64 {
        self.tui.stats().frames_drawn
    }

    fn accepted(&self) -> Vec<u8> {
        self.transport.lock().unwrap().accepted.clone()
    }

    fn submitted_bodies(&self) -> Vec<CommandBody> {
        self.session
            .lock()
            .unwrap()
            .submitted
            .iter()
            .map(|envelope| envelope.body.clone())
            .collect()
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn rendering_is_dirty_only() {
    let mut rig = Rig::new(Mode::AcceptAll, (100, 30));
    rig.publish_revision(ViewKind::Overview, 1);
    rig.steps_until(20, |rig| rig.frames() == 1);
    let drawn_at = rig.time.now();
    // Same revision again, same viewport, nothing typed: no frame until the
    // one-second age tick.
    rig.publish_revision(ViewKind::Overview, 1);
    rig.steps_until(1000, |rig| {
        rig.time.now() >= drawn_at + Duration::from_millis(900)
    });
    assert_eq!(rig.frames(), 1, "same revision and viewport: no draw");
    rig.steps_until(1000, |rig| rig.frames() == 2);
    assert!(
        rig.time.now() >= drawn_at + AGE_TICK,
        "age text redraws once per second"
    );
    rig.publish_revision(ViewKind::Overview, 2);
    let before = rig.time.now();
    rig.steps_until(1000, |rig| rig.frames() == 3);
    assert!(
        rig.time.now() - before <= Duration::from_millis(300),
        "new data drawn promptly"
    );
}

#[test]
fn data_frames_are_capped_by_fps() {
    let mut rig = Rig::new(Mode::AcceptAll, (80, 24));
    let mut revision = 1;
    let start = rig.time.now();
    while rig.time.now() < start + Duration::from_secs(2) {
        revision += 1;
        rig.publish_revision(ViewKind::Overview, revision);
        assert!(rig.tui.step().unwrap().is_none());
    }
    assert!(
        rig.frames() <= 9,
        "4 fps for 2 s plus the first frame, got {}",
        rig.frames()
    );
    assert!(rig.frames() >= 6);
}

#[test]
fn q_requests_shutdown_and_restores_the_terminal() {
    let mut rig = Rig::new(Mode::AcceptAll, (80, 24));
    rig.tui.enter_screen().unwrap();
    rig.publish_revision(ViewKind::Overview, 1);
    rig.script.push(Step::Wait(Duration::from_millis(100)));
    rig.script.key('q');
    let exit = rig.tui.run().unwrap();
    assert_eq!(exit.reason, TuiExitReason::UserQuit);
    assert!(exit.restore_flushed);
    assert_eq!(
        rig.session.lock().unwrap().shutdown_reasons,
        vec!["user".to_owned()]
    );
    assert!(rig.transport.lock().unwrap().restored >= 1);
    assert!(rig.accepted().ends_with(&setup::restore_sequence()));
}

#[test]
fn manager_shutdown_ends_the_loop() {
    let mut rig = Rig::new(Mode::AcceptAll, (80, 24));
    rig.session.lock().unwrap().manager_shutdown = true;
    let exit = rig.tui.run().unwrap();
    assert_eq!(exit.reason, TuiExitReason::ManagerShutdown);
    assert!(rig.session.lock().unwrap().shutdown_reasons.is_empty());
}

#[test]
fn terminal_hang_up_stops_rendering_and_requests_shutdown() {
    let mut rig = Rig::new(Mode::AcceptAll, (80, 24));
    rig.tui.enter_screen().unwrap();
    rig.publish_revision(ViewKind::Overview, 1);
    rig.script.push(Step::Wait(Duration::from_millis(100)));
    rig.script.push(Step::HangUp);
    let exit = rig.tui.run().unwrap();
    assert_eq!(exit.reason, TuiExitReason::InputClosed);
    assert!(!exit.restore_flushed);
    assert_eq!(
        rig.session.lock().unwrap().shutdown_reasons,
        vec!["terminal hung up".to_owned()]
    );
    // Descriptor flags are restored; no restore bytes go to a dead terminal.
    assert!(rig.transport.lock().unwrap().restored >= 1);
    assert!(!rig.accepted().ends_with(&setup::restore_sequence()));
}

#[test]
fn output_errors_stop_rendering_and_request_shutdown() {
    let mut rig = Rig::new(Mode::Fail(io::ErrorKind::BrokenPipe), (80, 24));
    rig.publish_revision(ViewKind::Overview, 1);
    let exit = rig.tui.run().unwrap();
    assert!(matches!(exit.reason, TuiExitReason::OutputClosed(_)));
    assert!(!exit.restore_flushed);
    assert_eq!(
        rig.session.lock().unwrap().shutdown_reasons,
        vec!["terminal output closed".to_owned()]
    );
    assert_eq!(rig.transport.lock().unwrap().restored, 1);
}

#[test]
fn frames_never_interleave_under_partial_writes() {
    let mut rig = Rig::new(Mode::PerCall(7), (100, 30));
    rig.tui.enter_screen().unwrap();
    for revision in 1..=4u64 {
        rig.publish_revision(ViewKind::Overview, revision);
        rig.steps_until(20_000, |rig| {
            rig.frames() == revision && !rig.tui.output.has_pending()
        });
        assert_eq!(rig.frames(), revision);
    }
    let records = rig.tui.frame_records();
    assert_eq!(records.len(), 5, "enter sequence + 4 frames");
    for record in &records {
        assert_eq!(
            record.written_before, record.start_offset,
            "a frame only starts after the previous one was completely written"
        );
    }
    let total: u64 = records.iter().map(|record| record.len).sum();
    assert_eq!(
        u64::try_from(rig.accepted().len()).unwrap(),
        total,
        "nothing spliced or lost"
    );
    assert!(rig.tui.stats().peak_output_bytes <= OUTPUT_BUFFER_CAPACITY);
}

#[test]
fn oversized_frames_are_never_sent_and_the_buffer_stays_bounded() {
    let capacity = 3 * CONTROL_RESERVE;
    let mut rig = Rig::with_capacity(Mode::AcceptAll, (160, 50), capacity);
    rig.tui.enter_screen().unwrap();
    rig.publish_revision(ViewKind::Overview, 1);
    rig.steps_until(200, |rig| rig.tui.stats().overflowed_frames >= 2);
    let stats = rig.tui.stats().clone();
    assert!(stats.overflowed_frames >= 2, "retried with backoff");
    assert_eq!(stats.frames_drawn, 0);
    assert!(stats.peak_output_bytes <= capacity);
    assert_eq!(
        rig.accepted(),
        setup::enter_sequence(true),
        "no partial frame reached the terminal"
    );
    assert!(
        rig.tui
            .controller()
            .ui_state(rig.time.now())
            .notices
            .iter()
            .any(|notice| notice.text.contains("did not fit"))
    );
    // A small terminal fits: the next frame is a full redraw.
    rig.script.push(Step::Event(Event::Resize(40, 10)));
    rig.steps_until(500, |rig| rig.frames() == 1);
    assert_eq!(rig.frames(), 1);
    assert!(rig.tui.stats().full_redraws >= 1);
    assert!(contains(&rig.accepted(), b"\x1b[2J"));
}

#[test]
fn input_and_q_still_work_while_output_is_blocked_for_30_seconds() {
    let mut rig = Rig::new(Mode::Blocked, (100, 30));
    rig.publish_revision(ViewKind::Processes, 1);
    rig.steps_until(10, |rig| rig.frames() == 1);
    assert!(rig.tui.output.has_pending(), "the first frame is stuck");
    let stuck_peak = rig.tui.stats().peak_output_bytes;
    rig.script.push(Step::Wait(Duration::from_secs(30)));
    rig.script.key('j');
    rig.script.key('q');
    let mut published_newer = false;
    let mut exit = None;
    for _ in 0..10_000 {
        if !published_newer && rig.time.now() > Duration::from_secs(15) {
            rig.publish_revision(ViewKind::Processes, 2);
            published_newer = true;
        }
        if let Some(reason) = rig.tui.step().unwrap() {
            exit = Some(reason);
            break;
        }
    }
    assert_eq!(
        exit,
        Some(TuiExitReason::UserQuit),
        "q works while output is blocked"
    );
    assert!(rig.time.now() >= Duration::from_secs(30));
    assert_eq!(
        rig.frames(),
        1,
        "no new diff while the previous frame is unflushed"
    );
    assert!(rig.tui.stats().deferred_while_flushing > 0);
    assert_eq!(
        rig.tui.stats().peak_output_bytes,
        stuck_peak,
        "the buffer did not grow"
    );
    assert!(stuck_peak <= OUTPUT_BUFFER_CAPACITY);
    let session = rig.session.lock().unwrap();
    assert_eq!(
        session.takes, 1,
        "the newer projection stays in the latest-only slot"
    );
    assert!(session.slot.is_some());
    assert!(
        session
            .submitted
            .iter()
            .any(|envelope| matches!(envelope.body, CommandBody::UpdateViewSession { .. })),
        "keys were turned into commands while blocked"
    );
    assert_eq!(session.shutdown_reasons, vec!["user".to_owned()]);
    drop(session);
    let quit_at = rig.time.now();
    let exit = rig.tui.finish(TuiExitReason::UserQuit);
    assert!(
        !exit.restore_flushed,
        "a terminal that reads nothing cannot show the restore"
    );
    assert!(
        rig.time.now() - quit_at <= MAX_EXIT_WAIT + Duration::from_millis(20),
        "bounded exit wait"
    );
    assert!(rig.transport.lock().unwrap().restored >= 1);
}

#[test]
fn a_slow_frame_is_followed_by_a_full_redraw() {
    let mut rig = Rig::new(Mode::BlockedUntil(Duration::from_secs(2)), (100, 30));
    rig.publish_revision(ViewKind::Overview, 1);
    rig.steps_until(5_000, |rig| rig.tui.stats().slow_frames == 1);
    assert_eq!(rig.tui.stats().slow_frames, 1);
    rig.publish_revision(ViewKind::Overview, 2);
    rig.steps_until(5_000, |rig| {
        rig.frames() == 3 && !rig.tui.output.has_pending()
    });
    assert_eq!(
        rig.tui.stats().full_redraws,
        1,
        "exactly one resynchronizing frame"
    );
    let records = rig.tui.frame_records();
    let accepted = rig.accepted();
    let slice = |index: usize| {
        let record = records[index];
        let start = usize::try_from(record.start_offset).unwrap();
        &accepted[start..start + usize::try_from(record.len).unwrap()]
    };
    assert!(
        contains(slice(1), b"\x1b[2J"),
        "the frame after a slow flush repaints everything"
    );
    assert!(
        !contains(slice(2), b"\x1b[2J"),
        "later frames are diffs again"
    );
}

#[test]
fn confirmation_tokens_come_from_drained_results() {
    let mut rig = Rig::new(Mode::AcceptAll, (160, 50));
    rig.publish_revision(ViewKind::Processes, 1);
    rig.steps_until(20, |rig| rig.frames() == 1);
    rig.script.key('K');
    rig.steps_until(20, |rig| !rig.submitted_bodies().is_empty());
    let prepare = rig.session.lock().unwrap().submitted.pop().unwrap();
    assert!(matches!(prepare.body, CommandBody::PrepareSignal { .. }));
    let mut result = CommandResult::completed(prepare.request_id, "prepared");
    result.confirmation_token = Some(test_fixtures::token());
    rig.session.lock().unwrap().results.push_back(result);
    rig.steps_until(20, |rig| rig.tui.controller().mode() == ModeKind::Confirm);
    assert_eq!(rig.tui.controller().mode(), ModeKind::Confirm);
    rig.script.key('y');
    rig.steps_until(20, |rig| !rig.submitted_bodies().is_empty());
    let bodies = rig.submitted_bodies();
    assert!(
        matches!(&bodies[0], CommandBody::ConfirmSignal { confirmation_token } if *confirmation_token == test_fixtures::token())
    );
    assert!(
        !contains(&rig.accepted(), test_fixtures::token().as_str().as_bytes()),
        "the token is never drawn"
    );
}

#[test]
fn idle_polls_are_bounded_and_never_zero() {
    let mut rig = Rig::new(Mode::AcceptAll, (80, 24));
    rig.publish_revision(ViewKind::Overview, 1);
    rig.steps_until(300, |_| false);
    let polls = rig.polls.lock().unwrap();
    assert!(polls.len() >= 300);
    for (timeout, _) in polls.iter() {
        assert!(
            *timeout >= MIN_POLL,
            "never a 0 ms poll without a pending event"
        );
        assert!(*timeout <= LONGEST_IDLE_POLL);
    }
}

/// An idle loop wakes a few times a second, not every 50 ms: right after a
/// draw it sleeps until a new projection could be drawn, then looks for one
/// every [`MAX_IDLE_POLL`].
#[test]
fn an_idle_loop_sleeps_until_a_new_projection_could_be_drawn() {
    let mut rig = Rig::new(Mode::AcceptAll, (80, 24));
    rig.publish_revision(ViewKind::Overview, 1);
    rig.steps_until(20, |rig| rig.frames() == 1);
    rig.polls.lock().unwrap().clear();
    let start = rig.time.now();
    rig.steps_until(1000, |rig| {
        rig.time.now() >= start + Duration::from_secs(10)
    });
    let wakes = rig.polls.lock().unwrap().len();
    assert!(
        wakes <= 100,
        "{wakes} wakes in 10 s, 200 at the old 50 ms poll"
    );
    // A projection published while the loop sleeps is still drawn promptly.
    rig.publish_revision(ViewKind::Overview, 2);
    let published = rig.time.now();
    let frames = rig.frames();
    rig.steps_until(100, |rig| rig.frames() > frames);
    assert!(rig.time.now() - published <= LONGEST_IDLE_POLL);
}

/// While a command's result is awaited the loop looks for it at the input
/// cadence, so a reply is never seen later than before.
#[test]
fn an_awaited_result_is_polled_at_the_input_cadence() {
    let mut rig = Rig::new(Mode::AcceptAll, (80, 24));
    rig.publish_revision(ViewKind::Overview, 1);
    rig.steps_until(20, |rig| rig.frames() == 1);
    rig.script.key('2');
    rig.steps_until(20, |rig| !rig.submitted_bodies().is_empty());
    assert!(rig.tui.controller().awaits_results());
    rig.polls.lock().unwrap().clear();
    rig.steps_until(20, |_| false);
    let polls = rig.polls.lock().unwrap();
    assert!(
        polls.iter().all(|(timeout, _)| *timeout <= RESULT_POLL),
        "{polls:?}"
    );
}
