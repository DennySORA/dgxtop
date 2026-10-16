//! HistoryService: bounded, per-session Overview History reads for the
//! history view (D-46, D-48, D-82).
//!
//! * Charts: the overview's series over a longer window, as the
//!   presentation picks them ([`HistoryChartSpec`]): the CPU total, the RAM
//!   in use, each GPU's series and the rates of a disk and an interface.
//!   Bars (D-78, D-82): a bar of the session's `overview_bar` size (`1m` to
//!   `24h`, D-80) for each column of a plot, aligned to the local clock and
//!   ending with the bar being filled now ([`BarGrid`]); a bar is the mean
//!   of the time it holds, a time without data is a gap. Every chart of a
//!   session draws the same bars, so all of them share one time axis.
//! * The history answers them from its own sizes (`OverviewReader`, D-82),
//!   which are its cache: each round asks a chart's whole window and gets
//!   its bars, the window's mean, peak and time held and, for a memory
//!   chart, the tracked processes' parts (D-64, D-99). The service keeps
//!   only the latest answer of each read and shapes the view; a round's
//!   view is built once, when its last answer arrives.
//! * Only sessions currently on the history view read. A bar size, chart or
//!   column change bumps the charts' generation, cancels the running reads
//!   and makes their late answers stale. The GPUs' 1 h to 24 h statistics of
//!   D-48 are gone (D-107): the charts and their bar sizes show the same. At most `queues.history_requests` reads run at once, each with
//!   deadline `now + runtime.history_deadline_ms`. Refreshed at the bar size (5 s to 60 s), three times slower while
//!   throttled.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use dgxtop_core::clock::{NANOS_PER_MILLI, TimeProvider, ms_to_ns};
use dgxtop_core::commands::ViewKind;
use dgxtop_core::errors::ErrorDetail;
use dgxtop_core::history::{MAX_HISTORY_POINTS, SeriesInEveryBoot};
use dgxtop_core::identity::{EntityId, MetricKey, SessionId};
use dgxtop_core::metric::{CanonicalUnit, descriptor};
use dgxtop_core::overview_history::{
    OverviewAnswer, OverviewBars, OverviewBucket, OverviewRequest,
};
use dgxtop_core::ports::{CancelFlag, Dispatch, DispatchError, HistoryJob};
use dgxtop_core::tiers::{BarSize, Tier};
use dgxtop_core::view::{
    DiskHistory, HistoryChartKind, HistoryChartView, HistoryPart, HistoryPointView, HistoryView,
    SessionState, history_plot_columns,
};
use uuid::Uuid;

/// Refresh bounds: bars refresh at their size, at least every 5 s, at most
/// once a minute.
const MIN_REFRESH_MS: u64 = 5_000;
const MAX_REFRESH_MS: u64 = 60_000;
/// Throttled sessions refresh this many times slower.
const LIMITED_REFRESH_FACTOR: u64 = 3;
/// Grace after a job deadline before the slot is reclaimed locally.
const RESULT_GRACE_NS: u64 = 100 * NANOS_PER_MILLI;
/// Fewest bars a chart draws; the history view keeps them all (the Viewer
/// merges them into its columns).
pub(crate) const MIN_DISPLAY_POINTS: u32 = 60;
/// Charts one session queries; the presentation picks at most this many:
/// the host's nine (D-50, D-106) and four for each of eight GPUs.
pub(crate) const MAX_CHARTS: usize = 41;
/// Bar size of the charts when the session gives none.
const DEFAULT_BAR: BarSize = BarSize::OneMinute;

/// What a session's charts draw (D-78, D-82): `columns` bars of `bar` on
/// the local clock, the last one being filled now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BarGrid {
    bar: BarSize,
    columns: u32,
}

impl BarGrid {
    fn new(bar: BarSize, columns: u32) -> Self {
        Self {
            bar: match bar.for_overview() {
                BarSize::Auto => DEFAULT_BAR,
                fixed => fixed,
            },
            columns: columns.max(1),
        }
    }

    fn tier(self) -> Tier {
        self.bar.tier().unwrap_or(Tier::OneMinute)
    }

    fn span_ns(self) -> u64 {
        self.tier()
            .step_ns()
            .saturating_mul(u64::from(self.columns))
    }

    /// `[start, end)` at `utc_ns`: the bars ending with the one holding it.
    fn window_at(self, utc_ns: i64, shift_ns: i64) -> (i64, i64) {
        self.tier()
            .window_ending_at(utc_ns, shift_ns, i64::from(self.columns))
    }

    fn refresh_ms(self) -> u64 {
        (self.tier().step_ns() / 1_000_000).clamp(MIN_REFRESH_MS, MAX_REFRESH_MS)
    }
}

/// A series of the history view, picked by the presentation (D-46).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HistoryChartSpec {
    pub(crate) kind: HistoryChartKind,
    /// The device charted; empty for the host's CPU and memory.
    pub(crate) subject: String,
    pub(crate) key: MetricKey,
    /// Fixed top of the scale; `None` scales the chart to its peak.
    pub(crate) scale_max: Option<f64>,
    /// The GPUs whose tracked processes' parts the chart stacks (a GPU's
    /// memory, or the unified GPUs under the RAM used, D-64); empty for
    /// other charts.
    pub(crate) parts_of: BTreeSet<EntityId>,
}

/// What the presentation picks for a session on the history view: the
/// charts drawn over its window.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct HistoryPicks {
    pub(crate) charts: Vec<HistoryChartSpec>,
}

/// What the presentation picks a session's series from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChartRequest<'a> {
    /// The charts the session shows now, so a pick can keep a device.
    pub(crate) shown: &'a [HistoryChartSpec],
}

/// One read of a session: the bars of one of its charts.
#[derive(Debug)]
struct Inflight {
    session_id: SessionId,
    generation: u64,
    /// The round it was sent in: only the current round's reads end it.
    round: u64,
    /// The chart read.
    chart: usize,
    deadline_ns: u64,
    cancel: CancelFlag,
}

#[derive(Debug)]
struct ChartState {
    spec: HistoryChartSpec,
    /// The history's latest answer for the chart's window (D-99).
    bars: Option<OverviewBars>,
    error: Option<String>,
}

impl ChartState {
    fn new(spec: HistoryChartSpec) -> Self {
        Self {
            spec,
            bars: None,
            error: None,
        }
    }
}

/// The charts' reads with their queue and schedule.
#[derive(Debug, Default)]
struct Round {
    generation: u64,
    /// Counts the rounds begun.
    serial: u64,
    queue: VecDeque<usize>,
    /// Reads of the current round not answered yet.
    pending: usize,
    /// The wall-clock time every read of the round asks at.
    at_utc_ns: i64,
    refresh_due_ns: u64,
}

impl Round {
    /// Drops the queue for a new generation.
    fn restart(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.queue.clear();
        self.pending = 0;
    }

    /// Queues `count` reads at `at_utc_ns`, the next round `refresh_ms`
    /// after `now_ns`.
    fn begin(&mut self, count: usize, (now_ns, at_utc_ns): (u64, i64), refresh_ms: u64) {
        self.serial = self.serial.wrapping_add(1);
        self.at_utc_ns = at_utc_ns;
        for index in 0..count {
            if !self.queue.contains(&index) {
                self.queue.push_back(index);
            }
        }
        self.pending = self.queue.len();
        self.refresh_due_ns = now_ns.saturating_add(ms_to_ns(refresh_ms));
    }

    /// One read of round `serial` is over; whether it was the current
    /// round's last.
    fn finish(&mut self, serial: u64) -> bool {
        serial == self.serial && self.finish_one()
    }

    /// One read of the current round is over; whether it was the last.
    fn finish_one(&mut self) -> bool {
        let was_pending = self.pending > 0;
        self.pending = self.pending.saturating_sub(1);
        was_pending && self.pending == 0
    }
}

/// How a new pick changes a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickChange {
    Unchanged,
    /// Same series with a new scale or name: redraw only.
    Redraw,
    /// Other series: read again.
    Restart,
}

fn pick_change(current: &[&HistoryChartSpec], picked: &[HistoryChartSpec]) -> PickChange {
    let same = picked.len() == current.len();
    if same && picked.iter().zip(current).all(|(new, old)| new == *old) {
        return PickChange::Unchanged;
    }
    let same_series = same
        && picked
            .iter()
            .zip(current)
            .all(|(new, old)| new.key == old.key && new.kind == old.kind);
    if same_series {
        PickChange::Redraw
    } else {
        PickChange::Restart
    }
}

#[derive(Debug)]
struct SessionHistory {
    active: bool,
    grid: BarGrid,
    /// Snapshot revision the series were last picked from.
    charts_revision: Option<u64>,
    charts: Vec<ChartState>,
    round: Round,
    /// Whether the history keeps the sizes on disk and why not, as it last
    /// said.
    disk: Option<(bool, Option<String>)>,
    view: Arc<HistoryView>,
}

impl SessionHistory {
    /// Drops the charts' answers and queue for a new generation.
    fn restart(&mut self) {
        self.round.restart();
        for chart in &mut self.charts {
            chart.bars = None;
            chart.error = None;
        }
    }

    fn begin_round(&mut self, at: (u64, i64)) {
        let refresh = self.grid.refresh_ms();
        self.round.begin(self.charts.len(), at, refresh);
    }

    fn set_error(&mut self, chart: usize, error: String) {
        if let Some(chart) = self.charts.get_mut(chart) {
            chart.error = Some(error);
        }
    }
}

/// Static settings of the history service.
#[derive(Debug, Clone)]
pub(crate) struct HistorySettings {
    pub(crate) max_inflight: usize,
    pub(crate) deadline_ms: u32,
    pub(crate) max_points: u32,
    /// Local time's offset from UTC, seconds east: the bars' clock.
    pub(crate) utc_offset_s: i32,
    /// The history keeps the sizes on disk (the archive is on), until its
    /// answers say otherwise.
    pub(crate) kept_on_disk: bool,
}

/// What a tick did.
#[derive(Debug, Default)]
pub(crate) struct HistoryTick {
    pub(crate) dirty: Vec<SessionId>,
    pub(crate) dispatched: u32,
    pub(crate) expired: u32,
}

pub(crate) struct HistoryService {
    port: Box<dyn Dispatch<HistoryJob>>,
    clock: Arc<dyn TimeProvider>,
    settings: HistorySettings,
    raw_interval_ms: u32,
    sessions: BTreeMap<SessionId, SessionHistory>,
    inflight: BTreeMap<Uuid, Inflight>,
    cursor: usize,
}

impl HistoryService {
    pub(crate) fn new(
        port: Box<dyn Dispatch<HistoryJob>>,
        clock: Arc<dyn TimeProvider>,
        settings: HistorySettings,
        raw_interval_ms: u32,
    ) -> Self {
        Self {
            port,
            clock,
            settings: HistorySettings {
                max_inflight: settings.max_inflight.max(1),
                ..settings
            },
            raw_interval_ms,
            sessions: BTreeMap::new(),
            inflight: BTreeMap::new(),
            cursor: 0,
        }
    }

    /// Raw sampling cadence (decides whether a bar merges samples).
    pub(crate) fn set_raw_interval(&mut self, raw_interval_ms: u32) {
        self.raw_interval_ms = raw_interval_ms;
    }

    fn shift_ns(&self) -> i64 {
        i64::from(self.settings.utc_offset_s).saturating_mul(1_000_000_000)
    }

    fn now_utc_ns(&self) -> i64 {
        i64::try_from(self.clock.now_utc_ns()).unwrap_or(i64::MAX)
    }

    pub(crate) fn open_session(&mut self, session_id: &SessionId) {
        let mut session = SessionHistory {
            active: false,
            grid: BarGrid::new(DEFAULT_BAR, MIN_DISPLAY_POINTS),
            charts_revision: None,
            charts: Vec::new(),
            round: Round::default(),
            disk: None,
            view: Arc::new(empty_view()),
        };
        session.view = Arc::new(self.build_view(&session));
        self.sessions.insert(session_id.clone(), session);
    }

    pub(crate) fn close_session(&mut self, session_id: &SessionId) {
        self.cancel_queries(session_id);
        self.sessions.remove(session_id);
    }

    /// Cancels everything (shutdown).
    pub(crate) fn cancel_all(&mut self) {
        for inflight in self.inflight.values() {
            inflight.cancel.cancel();
        }
        self.inflight.clear();
        for session in self.sessions.values_mut() {
            session.round.queue.clear();
            session.active = false;
        }
    }

    pub(crate) fn view(&self, session_id: &SessionId) -> Arc<HistoryView> {
        self.sessions.get(session_id).map_or_else(
            || Arc::new(empty_view()),
            |session| Arc::clone(&session.view),
        )
    }

    #[cfg(test)]
    pub(crate) fn inflight_count(&self) -> usize {
        self.inflight.len()
    }

    /// Applies a session's view state; returns whether its history view
    /// changed. Opening the view picks its series afresh; a bar size change
    /// redraws the charts.
    pub(crate) fn sync_session(
        &mut self,
        session_id: &SessionId,
        state: &SessionState,
        columns: u16,
        now_ns: u64,
    ) -> bool {
        let wanted = self.wanted(state, columns);
        let Some(session) = self.sessions.get(session_id) else {
            return false;
        };
        let opening = !session.active;
        let changed = match wanted {
            None => session.active,
            Some(grid) => opening || session.grid != grid,
        };
        if !changed {
            return false;
        }
        self.cancel_queries(session_id);
        let at = (now_ns, self.now_utc_ns());
        let Some(session) = self.sessions.get_mut(session_id) else {
            return false;
        };
        session.restart();
        let Some(grid) = wanted else {
            session.active = false;
            self.rebuild(session_id);
            return true;
        };
        session.grid = grid;
        if opening {
            // Pick the series afresh: the busiest disk and interface are
            // those of the moment the view opens.
            session.charts.clear();
            session.charts_revision = None;
        }
        session.active = true;
        session.begin_round(at);
        self.rebuild(session_id);
        true
    }

    /// Whether a session on the history view still needs its series picked
    /// from snapshot `revision`.
    pub(crate) fn wants_charts(&self, revision: u64) -> bool {
        self.sessions
            .values()
            .any(|session| session.active && session.charts_revision != Some(revision))
    }

    /// Gives every active session the series `pick` chooses from snapshot
    /// `revision` (it also receives the session's current charts, so it can
    /// keep a device the session already shows). Other series restart the
    /// reads; a new scale or name only redraws. Returns the sessions
    /// whose view changed.
    pub(crate) fn update_charts(
        &mut self,
        revision: u64,
        pick: impl Fn(&ChartRequest<'_>) -> HistoryPicks,
        now_ns: u64,
    ) -> Vec<SessionId> {
        let due: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.active && session.charts_revision != Some(revision))
            .map(|(session_id, _)| session_id.clone())
            .collect();
        let at = (now_ns, self.now_utc_ns());
        let mut dirty = Vec::new();
        for session_id in due {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            session.charts_revision = Some(revision);
            let shown: Vec<HistoryChartSpec> = session
                .charts
                .iter()
                .map(|chart| chart.spec.clone())
                .collect();
            let mut picked = pick(&ChartRequest { shown: &shown });
            picked.charts.truncate(MAX_CHARTS);
            let change = pick_change(
                &session
                    .charts
                    .iter()
                    .map(|chart| &chart.spec)
                    .collect::<Vec<_>>(),
                &picked.charts,
            );
            if change == PickChange::Restart {
                self.cancel_queries(&session_id);
            }
            let Some(session) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            apply_charts(session, change, picked.charts, at);
            if change != PickChange::Unchanged {
                self.rebuild(&session_id);
                dirty.push(session_id);
            }
        }
        dirty
    }

    pub(crate) fn tick(&mut self, now_ns: u64, limited: bool) -> HistoryTick {
        let mut tick = HistoryTick::default();
        self.schedule_refreshes(now_ns, limited);
        self.expire(now_ns, &mut tick);
        self.dispatch(now_ns, &mut tick);
        tick
    }

    /// Applies an answer; `None` means it was stale (old generation or
    /// unknown) or its round is not over yet (the view is built once a
    /// round is answered).
    pub(crate) fn on_result(
        &mut self,
        request_id: Uuid,
        session_id: &SessionId,
        generation: u64,
        result: Result<OverviewAnswer, ErrorDetail>,
    ) -> Option<SessionId> {
        let inflight = self.inflight.remove(&request_id)?;
        let session = self.sessions.get_mut(&inflight.session_id)?;
        if &inflight.session_id != session_id
            || inflight.generation != generation
            || session.round.generation != generation
        {
            return None;
        }
        match result {
            Ok(OverviewAnswer::Bars(bars)) => {
                session.disk = Some((bars.kept_on_disk, bars.store_issue.clone()));
                let chart = session.charts.get_mut(inflight.chart)?;
                chart.error = None;
                chart.bars = Some(bars);
            }
            Ok(_) => {
                session.set_error(inflight.chart, "history answered another read".to_owned());
            }
            Err(error) => session.set_error(inflight.chart, error.message),
        }
        if !session.round.finish(inflight.round) {
            return None;
        }
        let session_id = inflight.session_id;
        self.rebuild(&session_id);
        Some(session_id)
    }

    pub(crate) fn next_deadline(&self) -> Option<u64> {
        let refreshes = self
            .sessions
            .values()
            .filter(|session| session.active)
            .map(|session| session.round.refresh_due_ns);
        let deadlines = self
            .inflight
            .values()
            .map(|inflight| inflight.deadline_ns.saturating_add(RESULT_GRACE_NS));
        refreshes.chain(deadlines).min()
    }

    /// The bars a session on the history view wants; other views read no
    /// history (the GPU view shows live data, D-48).
    fn wanted(&self, state: &SessionState, columns: u16) -> Option<BarGrid> {
        if state.view != ViewKind::History {
            return None;
        }
        let upper = self.settings.max_points.clamp(1, MAX_HISTORY_POINTS);
        // A bar for each column of the plot.
        let columns = u32::from(history_plot_columns(columns)).clamp(1, upper);
        Some(BarGrid::new(state.overview_bar, columns))
    }

    fn cancel_queries(&mut self, session_id: &SessionId) {
        // Entries stay until their answer or deadline so the running-job
        // bound holds; their answers are dropped by the generation check.
        for inflight in self
            .inflight
            .values()
            .filter(|inflight| &inflight.session_id == session_id)
        {
            inflight.cancel.cancel();
        }
    }

    fn schedule_refreshes(&mut self, now_ns: u64, limited: bool) {
        let at = (now_ns, self.now_utc_ns());
        for session in self.sessions.values_mut().filter(|session| session.active) {
            if now_ns < session.round.refresh_due_ns {
                continue;
            }
            session.begin_round(at);
            if limited {
                let period = session.grid.refresh_ms();
                let slower = ms_to_ns(period * (LIMITED_REFRESH_FACTOR - 1));
                session.round.refresh_due_ns = session.round.refresh_due_ns.saturating_add(slower);
            }
        }
    }

    /// A read past its deadline counts as answered with an error.
    fn expire(&mut self, now_ns: u64, tick: &mut HistoryTick) {
        let overdue: Vec<Uuid> = self
            .inflight
            .iter()
            .filter(|(_, inflight)| now_ns >= inflight.deadline_ns.saturating_add(RESULT_GRACE_NS))
            .map(|(id, _)| *id)
            .collect();
        for request_id in overdue {
            let Some(inflight) = self.inflight.remove(&request_id) else {
                continue;
            };
            inflight.cancel.cancel();
            tick.expired = tick.expired.saturating_add(1);
            let Some(session) = self.sessions.get_mut(&inflight.session_id) else {
                continue;
            };
            if session.round.generation != inflight.generation {
                continue;
            }
            session.set_error(inflight.chart, "history read timed out".to_owned());
            if session.round.finish(inflight.round) {
                let session_id = inflight.session_id.clone();
                self.rebuild(&session_id);
                tick.dirty.push(session_id);
            }
        }
    }

    fn dispatch(&mut self, now_ns: u64, tick: &mut HistoryTick) {
        let ids: Vec<SessionId> = self.sessions.keys().cloned().collect();
        if ids.is_empty() {
            return;
        }
        let mut idle_rounds = 0;
        while self.inflight.len() < self.settings.max_inflight && idle_rounds < ids.len() {
            let index = self.cursor % ids.len();
            self.cursor = self.cursor.wrapping_add(1);
            let Some(session_id) = ids.get(index) else {
                break;
            };
            match self.dispatch_next(session_id, now_ns) {
                Dispatched::Sent => {
                    idle_rounds = 0;
                    tick.dispatched = tick.dispatched.saturating_add(1);
                }
                Dispatched::Nothing => idle_rounds += 1,
                Dispatched::PortBusy => break,
                Dispatched::Failed { round_over } => {
                    idle_rounds = 0;
                    if round_over {
                        tick.dirty.push(session_id.clone());
                    }
                }
            }
        }
    }

    /// Sends the session's next chart read.
    fn dispatch_next(&mut self, session_id: &SessionId, now_ns: u64) -> Dispatched {
        let shift = self.shift_ns();
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Dispatched::Nothing;
        };
        let Some(chart) = session.round.queue.pop_front() else {
            return Dispatched::Nothing;
        };
        if chart >= session.charts.len() {
            let round_over = session.round.finish_one();
            return Dispatched::Failed { round_over };
        }
        let request = request_of(session, chart, shift);
        let (generation, round) = (session.round.generation, session.round.serial);
        let cancel = CancelFlag::new();
        let deadline_ns = now_ns.saturating_add(ms_to_ns(u64::from(self.settings.deadline_ms)));
        let request_id = Uuid::new_v4();
        let job = HistoryJob {
            request_id,
            request,
            session_id: session_id.clone(),
            session_generation: generation,
            deadline_boot_ns: deadline_ns,
            cancel: cancel.clone(),
        };
        match self.port.try_dispatch(job) {
            Ok(()) => {
                self.inflight.insert(
                    request_id,
                    Inflight {
                        session_id: session_id.clone(),
                        generation,
                        round,
                        chart,
                        deadline_ns,
                        cancel,
                    },
                );
                Dispatched::Sent
            }
            Err(DispatchError::Busy) => {
                session.round.queue.push_front(chart);
                Dispatched::PortBusy
            }
            Err(error) => {
                session.set_error(chart, format!("history unavailable: {error}"));
                let round_over = session.round.finish_one();
                if round_over {
                    self.rebuild(session_id);
                }
                Dispatched::Failed { round_over }
            }
        }
    }

    fn rebuild(&mut self, session_id: &SessionId) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        let view = Arc::new(self.build_view(session));
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.view = view;
        }
    }

    fn build_view(&self, session: &SessionHistory) -> HistoryView {
        let grid = session.grid;
        let at = if session.round.at_utc_ns == 0 {
            self.now_utc_ns()
        } else {
            session.round.at_utc_ns
        };
        let window = grid.window_at(at, self.shift_ns());
        let merges = grid.tier().step_ns() > ms_to_ns(u64::from(self.raw_interval_ms.max(1)));
        HistoryView {
            window_label: grid.bar.token().to_owned(),
            requested_span_ns: grid.span_ns(),
            start_utc_ns: utc_u64(window.0),
            end_utc_ns: utc_u64(window.1),
            utc_offset_s: self.settings.utc_offset_s,
            charts: session
                .charts
                .iter()
                .map(|chart| chart_view(chart, (grid, window), (session.active, merges)))
                .collect(),
            disk: match &session.disk {
                None if self.settings.kept_on_disk => DiskHistory::On,
                None | Some((false, _)) => DiskHistory::Off,
                Some((true, None)) => DiskHistory::On,
                Some((true, Some(issue))) => DiskHistory::Failed(issue.clone()),
            },
            error: None,
        }
    }
}

enum Dispatched {
    Sent,
    Nothing,
    PortBusy,
    Failed { round_over: bool },
}

/// Applies the charts a pick chose.
fn apply_charts(
    session: &mut SessionHistory,
    change: PickChange,
    specs: Vec<HistoryChartSpec>,
    at: (u64, i64),
) {
    match change {
        PickChange::Unchanged => {}
        PickChange::Redraw => {
            for (chart, spec) in session.charts.iter_mut().zip(specs) {
                chart.spec = spec;
            }
        }
        PickChange::Restart => {
            session.restart();
            session.charts = specs.into_iter().map(ChartState::new).collect();
            session.begin_round(at);
        }
    }
}

/// The read of chart `index`: its whole window (the history's sizes are
/// its cache, D-99) at its round's time.
fn request_of(session: &SessionHistory, index: usize, shift: i64) -> OverviewRequest {
    let chart = &session.charts[index];
    let at = session.round.at_utc_ns;
    let window = session.grid.window_at(at, shift);
    OverviewRequest::Bars {
        series: SeriesInEveryBoot::from(&chart.spec.key),
        tier: session.grid.tier(),
        from_utc_ns: window.0,
        end_utc_ns: window.1,
        at_utc_ns: at,
        parts_of: chart.spec.parts_of.clone(),
    }
}

fn utc_u64(utc_ns: i64) -> u64 {
    u64::try_from(utc_ns).unwrap_or(0)
}

/// Unit of a series' metric (counts when unknown).
fn unit_of(metric_id: &str) -> CanonicalUnit {
    descriptor(metric_id).map_or(CanonicalUnit::Count, |known| known.unit)
}

/// A chart: a point per bar of the window holding a value, and the
/// window's mean, peak, time held and process parts as the history merged
/// them (D-99).
fn chart_view(
    chart: &ChartState,
    (grid, (start, end)): (BarGrid, (i64, i64)),
    (active, merges): (bool, bool),
) -> HistoryChartView {
    let spec = &chart.spec;
    let step = grid.tier().step_i64();
    let answer = chart.bars.as_ref();
    let points: Vec<HistoryPointView> = answer
        .into_iter()
        .flat_map(|answer| &answer.bars)
        .filter(|bar| bar.start_utc_ns >= start && bar.start_utc_ns < end)
        .map(|bar| HistoryPointView {
            start_utc_ns: utc_u64(bar.start_utc_ns),
            end_utc_ns: utc_u64(bar.start_utc_ns.saturating_add(step)),
            mean: bar.mean(),
            min: bar.low(),
            max: bar.peak(),
        })
        .collect();
    let whole = answer.map(|answer| &answer.whole);
    HistoryChartView {
        kind: spec.kind,
        subject: spec.subject.clone(),
        series: spec.key.clone(),
        unit: unit_of(spec.key.metric_id.as_str()),
        scale_max: spec.scale_max,
        points,
        mean: whole.and_then(OverviewBucket::mean),
        peak: whole.and_then(OverviewBucket::peak),
        covered_ns: whole.map(|whole| whole.covered_ns),
        display_downsampled: merges,
        pending: active && answer.is_none() && chart.error.is_none(),
        error: chart.error.clone(),
        parts: answer
            .map(|answer| {
                answer
                    .parts
                    .iter()
                    .map(|part| HistoryPart {
                        slot: part.slot,
                        means: part.means.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn empty_view() -> HistoryView {
    let grid = BarGrid::new(DEFAULT_BAR, MIN_DISPLAY_POINTS);
    HistoryView {
        window_label: grid.bar.token().to_owned(),
        requested_span_ns: grid.span_ns(),
        start_utc_ns: 0,
        end_utc_ns: 0,
        utc_offset_s: 0,
        charts: Vec::new(),
        disk: DiskHistory::Off,
        error: None,
    }
}

#[cfg(test)]
mod tests;
