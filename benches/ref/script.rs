//! The REF command workload: a bounded 10 Hz script of non-destructive view
//! commands (`PERFORMANCE.md` §2), and the router that times each command
//! from submission to the Manager's result.
//!
//! Benches cannot construct terminal key events (crossterm is not one of
//! their dependencies), so the script submits the `UpdateViewSession`
//! commands the Controller would produce directly to the session. Results
//! are drained by whichever side polls first — the script polls every
//! 0.5 ms while a command is outstanding, so a result is timestamped within
//! about half a millisecond of the Manager delivering it — and results the
//! script did not send are handed to the frontend unchanged.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dgxtop::perf::latency::{PendingRequests, duration_us};
use dgxtop::perf::threads::HarnessCpu;
use dgxtop_core::commands::{
    COMMAND_SCHEMA_VERSION, CommandBody, CommandEnvelope, CommandResult, SortKey, ViewKind,
};
use dgxtop_core::identity::{EntityId, SessionId};
use dgxtop_core::ports::SessionPort;
use dgxtop_core::tiers::BarSize;
use dgxtop_core::view::ViewModel;
use dgxtop_manager::SessionHandle;
use uuid::Uuid;

use super::probes::Probes;
use crate::common::BenchResult;

/// 10 Hz command cadence.
const COMMAND_INTERVAL: Duration = Duration::from_millis(100);
/// Result polling while a command is outstanding.
const RESULT_POLL: Duration = Duration::from_micros(500);
/// A command unanswered this long counts as a stall (deadlock suspicion).
pub const STALL_AFTER: Duration = Duration::from_secs(5);
/// Steps of one script cycle (10 s at 10 Hz).
const CYCLE_STEPS: u64 = 100;
/// The Overview History's bar sizes the script cycles through (D-80).
const HISTORY_BAR_SIZES: [BarSize; 5] = [
    BarSize::FiveMinutes,
    BarSize::OneHour,
    BarSize::SixHours,
    BarSize::TwelveHours,
    BarSize::OneDay,
];
const SORTS: [SortKey; 5] = [
    SortKey::GpuMemory,
    SortKey::GpuUtil,
    SortKey::CpuPercent,
    SortKey::HostMemory,
    SortKey::Pid,
];

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the latest projections offer to select (kept by the frontend).
#[derive(Debug, Clone, Default)]
pub struct ScriptHints {
    pub frame_revision: u64,
    pub gpus: Vec<EntityId>,
}

impl ScriptHints {
    pub fn absorb(&mut self, model: &ViewModel) {
        self.frame_revision = self.frame_revision.max(model.frame_revision);
        if let Some(gpu) = model.modules.gpu.as_deref()
            && !gpu.cards.is_empty()
        {
            self.gpus = gpu
                .cards
                .iter()
                .map(|card| card.entity_id.clone())
                .collect();
        }
    }
}

#[derive(Debug)]
struct RouterState {
    pending: PendingRequests<Uuid>,
    latencies_us: Vec<u64>,
    statuses: BTreeMap<String, u64>,
    for_frontend: VecDeque<CommandResult>,
}

/// Times script commands; passes every other result to the frontend.
pub struct ResultRouter {
    session: SessionHandle,
    probes: Arc<Probes>,
    state: Mutex<RouterState>,
}

impl ResultRouter {
    pub fn new(session: SessionHandle, probes: Arc<Probes>) -> Self {
        Self {
            session,
            probes,
            state: Mutex::new(RouterState {
                pending: PendingRequests::with_capacity(4096),
                latencies_us: Vec::new(),
                statuses: BTreeMap::new(),
                for_frontend: VecDeque::new(),
            }),
        }
    }

    pub fn session(&self) -> &SessionHandle {
        &self.session
    }

    /// Submits a script command; a synchronous rejection is its result.
    pub fn submit_script(&self, envelope: CommandEnvelope) {
        let request_id = envelope.request_id;
        locked(&self.state)
            .pending
            .start(request_id, Instant::now());
        if let Err(rejection) = self.session.submit(envelope) {
            let finished = Instant::now();
            let mut state = locked(&self.state);
            if let Some(elapsed) = state.pending.finish(&request_id, finished) {
                let label = format!("rejected_at_submit/{:?}", rejection.code);
                self.record(&mut state, elapsed, label);
            }
        }
    }

    /// Drains the session's results and routes them.
    pub fn poll(&self) {
        let results = self.session.drain_results(64);
        if results.is_empty() {
            return;
        }
        let arrived = Instant::now();
        let mut state = locked(&self.state);
        for result in results {
            match state.pending.finish(&result.request_id, arrived) {
                Some(elapsed) => {
                    let label = format!("{:?}/{:?}", result.status, result.code);
                    self.record(&mut state, elapsed, label);
                }
                None => state.for_frontend.push_back(result),
            }
        }
    }

    fn record(&self, state: &mut RouterState, elapsed: Duration, label: String) {
        if self.probes.is_measuring() {
            state.latencies_us.push(duration_us(elapsed));
            *state.statuses.entry(label).or_insert(0) += 1;
        }
    }

    /// Results for the frontend (polling first so it never starves).
    pub fn drain_for_frontend(&self, max: usize) -> Vec<CommandResult> {
        self.poll();
        let mut state = locked(&self.state);
        let count = state.for_frontend.len().min(max);
        state.for_frontend.drain(..count).collect()
    }

    pub fn outstanding(&self) -> usize {
        locked(&self.state).pending.len()
    }

    fn oldest_wait(&self, now: Instant) -> Option<Duration> {
        locked(&self.state).pending.oldest_wait(now)
    }

    pub fn begin_window(&self) {
        let mut state = locked(&self.state);
        state.latencies_us.clear();
        state.statuses.clear();
    }

    /// Latencies and result labels recorded during the window.
    pub fn end_window(&self) -> (Vec<u64>, BTreeMap<String, u64>, u64) {
        let mut state = locked(&self.state);
        let evicted = state.pending.evicted();
        (
            std::mem::take(&mut state.latencies_us),
            std::mem::take(&mut state.statuses),
            evicted,
        )
    }
}

/// One view state the script asks for.
struct ViewChoice {
    view: ViewKind,
    entity: Option<EntityId>,
    sort: SortKey,
    ascending: bool,
    overview_bar: Option<BarSize>,
}

impl ViewChoice {
    fn plain(view: ViewKind) -> Self {
        Self {
            view,
            entity: None,
            sort: SortKey::GpuMemory,
            ascending: false,
            overview_bar: None,
        }
    }

    fn into_command(self, frame_revision: u64) -> CommandBody {
        CommandBody::UpdateViewSession {
            view: self.view,
            selected_entity_id: self.entity,
            selected_process: None,
            filter: String::new(),
            sort: self.sort,
            ascending: self.ascending,
            frame_revision,
            overview_bar: self.overview_bar,
            process_bar: None,
        }
    }
}

/// `items[index mod len]`, or `None` for an empty list.
fn cyclic<T: Clone>(items: &[T], index: u64) -> Option<T> {
    let length = u64::try_from(items.len())
        .ok()
        .filter(|length| *length > 0)?;
    items.get(usize::try_from(index % length).ok()?).cloned()
}

/// The view command of script step `step` (deterministic 10 s cycle:
/// overview, GPU detail, history, settings). Only views the Manager serves
/// are visited: it refuses the Processes, Sources and Actions views (D-91).
pub fn view_command(step: u64, hints: &ScriptHints) -> CommandBody {
    let (cycle, phase) = (step / CYCLE_STEPS, step % CYCLE_STEPS);
    let choice = match phase {
        0..=19 => overview(phase % 10),
        20..=39 => gpu_detail(phase - 20, hints),
        40..=79 => history(cycle),
        _ => ViewChoice::plain(ViewKind::Settings),
    };
    choice.into_command(hints.frame_revision)
}

fn overview(step: u64) -> ViewChoice {
    ViewChoice {
        sort: cyclic(&SORTS, step).unwrap_or(SortKey::GpuMemory),
        ascending: step >= 5,
        ..ViewChoice::plain(ViewKind::Overview)
    }
}

fn gpu_detail(step: u64, hints: &ScriptHints) -> ViewChoice {
    ViewChoice {
        entity: cyclic(&hints.gpus, step),
        ..ViewChoice::plain(ViewKind::GpuDetail)
    }
}

/// One bar size per cycle over every chart of the view (D-46, D-80); the
/// rest of the 3 s dwell repeats the same state (idempotent, no re-query),
/// as a user reading the charts would.
fn history(cycle: u64) -> ViewChoice {
    ViewChoice {
        overview_bar: cyclic(&HISTORY_BAR_SIZES, cycle),
        ..ViewChoice::plain(ViewKind::History)
    }
}

/// Summary of the driver thread.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScriptReport {
    pub submitted: u64,
    pub stalls: u64,
}

/// Runs the script on its own (harness) thread.
pub struct ScriptDriver {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<ScriptReport>>,
}

/// What the driver needs.
pub struct ScriptInputs {
    pub router: Arc<ResultRouter>,
    pub hints: Arc<Mutex<ScriptHints>>,
    pub session_id: SessionId,
    pub harness: HarnessCpu,
    pub probes: Arc<Probes>,
}

impl ScriptDriver {
    pub fn start(inputs: ScriptInputs) -> BenchResult<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("bench-command-script".to_owned())
            .spawn(move || drive(&inputs, &flag))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) -> ScriptReport {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_default()
    }
}

fn drive(inputs: &ScriptInputs, stop: &AtomicBool) -> ScriptReport {
    let slot = inputs.harness.register();
    let started = Instant::now();
    let mut report = ScriptReport::default();
    let mut stalled = false;
    while !stop.load(Ordering::Acquire) {
        let tick = started
            + COMMAND_INTERVAL.saturating_mul(u32::try_from(report.submitted).unwrap_or(u32::MAX));
        wait_for_tick(&inputs.router, tick, stop);
        let now = Instant::now();
        let stall = inputs
            .router
            .oldest_wait(now)
            .is_some_and(|wait| wait >= STALL_AFTER);
        if stall && !stalled && inputs.probes.is_measuring() {
            report.stalls += 1;
        }
        stalled = stall;
        let hints = locked(&inputs.hints).clone();
        inputs.router.submit_script(CommandEnvelope {
            schema_version: COMMAND_SCHEMA_VERSION,
            request_id: Uuid::new_v4(),
            session_id: inputs.session_id.clone(),
            body: view_command(report.submitted, &hints),
        });
        report.submitted += 1;
        slot.refresh();
    }
    report
}

/// Sleeps until `tick`, polling results every 0.5 ms while any is pending.
fn wait_for_tick(router: &ResultRouter, tick: Instant, stop: &AtomicBool) {
    loop {
        let now = Instant::now();
        if now >= tick || stop.load(Ordering::Acquire) {
            return;
        }
        if router.outstanding() > 0 {
            router.poll();
            std::thread::sleep(RESULT_POLL.min(tick - now));
        } else {
            std::thread::sleep((tick - now).min(Duration::from_millis(5)));
        }
    }
}
