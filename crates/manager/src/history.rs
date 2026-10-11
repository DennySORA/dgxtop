//! HistoryService: bounded, per-session Overview History reads for the
//! history view (STORAGE.md §5, UI.md §6, D-46, D-48, D-82).
//!
//! * Charts: the overview's series over a longer window, as the
//!   presentation picks them ([`HistoryChartSpec`]): the CPU total, the RAM
//!   in use, each GPU's series and the rates of a disk and an interface.
//!   Bars (D-78, D-82): a bar of the session's `overview_bar` size (`1m` to
//!   `24h`, D-80) for each column of a plot, aligned to the local clock and
//!   ending with the bar being filled now ([`BarGrid`]); a bar is the mean
//!   of the time it holds, a time without data is a gap. Every chart of a
//!   session draws the same bars, so all of them share one time axis.
//! * Statistics (D-48): each GPU's utilization, memory in use, temperature
//!   and power over the last 1 h, 6 h, 12 h and 24 h, each a merge of the
//!   aligned buckets that cover it (never a sliding average), refreshed once
//!   a minute. A bar size change redraws the charts and keeps the
//!   statistics.
//! * The history answers both from its own sizes (`OverviewReader`, D-82);
//!   the service only asks and shapes the view. A chart keeps the bars that
//!   no longer change and asks only for the rest; a round's view is built
//!   once, when its last answer arrives.
//! * Only sessions currently on the history view read. A bar size, chart or
//!   column change bumps the charts' generation (new statistics series bump
//!   theirs), cancels the group's running reads and makes their late answers
//!   stale. At most `queues.history_requests` reads run at once, each with
//!   deadline `now + runtime.history_deadline_ms`; the charts' go first.
//!   Refreshed at the bar size (5 s to 60 s), three times slower while
//!   throttled.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use dgxtop_core::clock::{NANOS_PER_MILLI, TimeProvider, ms_to_ns};
use dgxtop_core::commands::ViewKind;
use dgxtop_core::errors::ErrorDetail;
use dgxtop_core::history::{HistoryResult, MAX_HISTORY_POINTS, SeriesInEveryBoot};
use dgxtop_core::identity::{MetricKey, SessionId};
use dgxtop_core::metric::{CanonicalUnit, descriptor};
use dgxtop_core::overview_history::{
    OverviewAnswer, OverviewBars, OverviewBucket, OverviewRequest,
};
use dgxtop_core::ports::{CancelFlag, Dispatch, DispatchError, HistoryJob};
use dgxtop_core::process_history::{BarSize, Tier};
use dgxtop_core::view::{
    DiskHistory, HistoryChartKind, HistoryChartView, HistoryPointView, HistoryView,
    HistoryWindowSummary, SessionState, history_plot_columns,
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
/// the host's six and four for each of eight GPUs (D-50).
pub(crate) const MAX_CHARTS: usize = 38;
/// Series one session summarizes: four for each of eight GPUs.
pub(crate) const MAX_STAT_SERIES: usize = 32;
/// Windows of the statistics (D-48).
const STAT_WINDOWS: [HistoryWindow; 4] = [
    HistoryWindow::OneHour,
    HistoryWindow::SixHours,
    HistoryWindow::TwelveHours,
    HistoryWindow::OneDay,
];
/// Statistics over an hour or more change slowly.
const STATS_REFRESH_MS: u64 = 60_000;
/// Bar size of the charts when the session gives none.
const DEFAULT_BAR: BarSize = BarSize::OneMinute;

/// The statistics' windows (D-48).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HistoryWindow {
    OneHour,
    SixHours,
    TwelveHours,
    OneDay,
}

impl HistoryWindow {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::OneHour => "1h",
            Self::SixHours => "6h",
            Self::TwelveHours => "12h",
            Self::OneDay => "24h",
        }
    }

    const fn span_ns(self) -> u64 {
        let hours = match self {
            Self::OneHour => 1,
            Self::SixHours => 6,
            Self::TwelveHours => 12,
            Self::OneDay => 24,
        };
        hours * 3_600 * 1_000_000_000
    }
}

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

    fn step_ns(self) -> i64 {
        i64::try_from(self.tier().step_ns()).unwrap_or(i64::MAX)
    }

    fn span_ns(self) -> u64 {
        self.tier()
            .step_ns()
            .saturating_mul(u64::from(self.columns))
    }

    /// `[start, end)` at `utc_ns`: the bars ending with the one holding it.
    fn window_at(self, utc_ns: i64, shift_ns: i64) -> (i64, i64) {
        let end = self
            .tier()
            .align(utc_ns, shift_ns)
            .saturating_add(self.step_ns());
        let span = i64::try_from(self.span_ns()).unwrap_or(i64::MAX);
        (end.saturating_sub(span), end)
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
}

/// What the presentation picks for a session on the history view: the
/// charts drawn over its window and the series summarized over 1 h to 24 h.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct HistoryPicks {
    pub(crate) charts: Vec<HistoryChartSpec>,
    pub(crate) stats: Vec<HistoryChartSpec>,
}

/// What the presentation picks a session's series from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChartRequest<'a> {
    /// The charts the session shows now, so a pick can keep a device.
    pub(crate) shown: &'a [HistoryChartSpec],
}

/// The query groups of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Charts,
    Stats,
}

/// One read of a session: the bars of chart `index` or the windows of
/// statistics series `index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Bars(usize),
    Windows(usize),
}

impl Purpose {
    const fn group(self) -> Group {
        match self {
            Self::Bars(_) => Group::Charts,
            Self::Windows(_) => Group::Stats,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Bars(index) | Self::Windows(index) => index,
        }
    }
}

#[derive(Debug)]
struct Inflight {
    session_id: SessionId,
    generation: u64,
    /// The round it was sent in: only the current round's reads end it.
    round: u64,
    purpose: Purpose,
    deadline_ns: u64,
    cancel: CancelFlag,
}

/// A chart's bars: the ones that no longer change, kept between rounds,
/// and the latest ones, asked again each round.
#[derive(Debug, Default)]
struct BarCache {
    /// The history's epoch the settled bars belong to.
    epoch: u64,
    settled: Vec<OverviewBucket>,
    /// Bars starting from here are asked again.
    next_from: Option<i64>,
    recent: Vec<OverviewBucket>,
    answered: bool,
}

impl BarCache {
    /// Where the next read starts in `window`.
    fn read_from(&self, (start, _): (i64, i64)) -> i64 {
        self.next_from.map_or(start, |from| from.max(start))
    }

    /// Takes an answer for the bars of `window` from `from` on; `false`
    /// when the history's past changed under the bars kept, so the whole
    /// window must be read again.
    fn take(
        &mut self,
        answer: OverviewBars,
        (from, window_start): (i64, i64),
        (tier, shift): (Tier, i64),
    ) -> bool {
        if self.answered && answer.epoch != self.epoch && from > window_start {
            *self = Self::default();
            return false;
        }
        self.epoch = answer.epoch;
        self.answered = true;
        let settled_from = if answer.settled_through_utc_ns == i64::MIN {
            from
        } else {
            tier.align(answer.settled_through_utc_ns, shift).max(from)
        };
        self.settled.retain(|bar| bar.start_utc_ns < from);
        let (settled, recent): (Vec<OverviewBucket>, Vec<OverviewBucket>) = answer
            .bars
            .into_iter()
            .partition(|bar| bar.start_utc_ns < settled_from);
        self.settled.extend(settled);
        self.recent = recent;
        self.next_from = Some(settled_from);
        true
    }

    /// The bars in `window`, oldest first.
    fn bars_in(&self, (start, end): (i64, i64)) -> impl Iterator<Item = &OverviewBucket> {
        self.settled
            .iter()
            .chain(&self.recent)
            .filter(move |bar| bar.start_utc_ns >= start && bar.start_utc_ns < end)
    }

    /// Lets the settled bars before `start` go (the window moved on).
    fn trim(&mut self, start: i64) {
        self.settled.retain(|bar| bar.start_utc_ns >= start);
    }
}

#[derive(Debug)]
struct ChartState {
    spec: HistoryChartSpec,
    bars: BarCache,
    error: Option<String>,
}

#[derive(Debug)]
struct StatState {
    spec: HistoryChartSpec,
    /// One bucket per [`STAT_WINDOWS`] window.
    windows: Option<Vec<OverviewBucket>>,
    error: Option<String>,
}

impl ChartState {
    fn new(spec: HistoryChartSpec) -> Self {
        Self {
            spec,
            bars: BarCache::default(),
            error: None,
        }
    }
}

impl StatState {
    fn new(spec: HistoryChartSpec) -> Self {
        Self {
            spec,
            windows: None,
            error: None,
        }
    }
}

/// Reads of one group with their queue and schedule.
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
    stats: Vec<StatState>,
    chart_round: Round,
    stat_round: Round,
    /// Whether the history keeps the sizes on disk and why not, as it last
    /// said.
    disk: Option<(bool, Option<String>)>,
    view: Arc<HistoryView>,
}

impl SessionHistory {
    fn round(&self, group: Group) -> &Round {
        match group {
            Group::Charts => &self.chart_round,
            Group::Stats => &self.stat_round,
        }
    }

    fn round_mut(&mut self, group: Group) -> &mut Round {
        match group {
            Group::Charts => &mut self.chart_round,
            Group::Stats => &mut self.stat_round,
        }
    }

    fn restart(&mut self, group: Group) {
        self.round_mut(group).restart();
        match group {
            Group::Charts => {
                for chart in &mut self.charts {
                    chart.bars = BarCache::default();
                    chart.error = None;
                }
            }
            Group::Stats => {
                for stat in &mut self.stats {
                    stat.windows = None;
                    stat.error = None;
                }
            }
        }
    }

    fn begin_round(&mut self, group: Group, at: (u64, i64)) {
        match group {
            Group::Charts => {
                let refresh = self.grid.refresh_ms();
                self.chart_round.begin(self.charts.len(), at, refresh);
            }
            Group::Stats => {
                self.stat_round
                    .begin(self.stats.len(), at, STATS_REFRESH_MS);
            }
        }
    }

    fn series_count(&self, group: Group) -> usize {
        match group {
            Group::Charts => self.charts.len(),
            Group::Stats => self.stats.len(),
        }
    }

    fn set_error(&mut self, purpose: Purpose, error: String) {
        match purpose {
            Purpose::Bars(index) => {
                if let Some(chart) = self.charts.get_mut(index) {
                    chart.error = Some(error);
                }
            }
            Purpose::Windows(index) => {
                if let Some(stat) = self.stats.get_mut(index) {
                    stat.error = Some(error);
                }
            }
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
            stats: Vec::new(),
            chart_round: Round::default(),
            stat_round: Round::default(),
            disk: None,
            view: Arc::new(empty_view()),
        };
        session.view = Arc::new(self.build_view(&session));
        self.sessions.insert(session_id.clone(), session);
    }

    pub(crate) fn close_session(&mut self, session_id: &SessionId) {
        self.cancel_queries(session_id, &[Group::Charts, Group::Stats]);
        self.sessions.remove(session_id);
    }

    /// Cancels everything (shutdown).
    pub(crate) fn cancel_all(&mut self) {
        for inflight in self.inflight.values() {
            inflight.cancel.cancel();
        }
        self.inflight.clear();
        for session in self.sessions.values_mut() {
            session.chart_round.queue.clear();
            session.stat_round.queue.clear();
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
    /// redraws the charts and keeps the statistics.
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
        let leaving_or_opening = wanted.is_none() || opening;
        let groups: &[Group] = if leaving_or_opening {
            &[Group::Charts, Group::Stats]
        } else {
            &[Group::Charts]
        };
        self.cancel_queries(session_id, groups);
        let at = (now_ns, self.now_utc_ns());
        let Some(session) = self.sessions.get_mut(session_id) else {
            return false;
        };
        for group in groups {
            session.restart(*group);
        }
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
            session.stats.clear();
            session.charts_revision = None;
            session.begin_round(Group::Stats, at);
        }
        session.active = true;
        session.begin_round(Group::Charts, at);
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
    /// keep a device the session already shows). Other series restart their
    /// group's reads; a new scale or name only redraws. Returns the sessions
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
            picked.stats.truncate(MAX_STAT_SERIES);
            let chart_change = pick_change(
                &session
                    .charts
                    .iter()
                    .map(|chart| &chart.spec)
                    .collect::<Vec<_>>(),
                &picked.charts,
            );
            let stat_change = pick_change(
                &session
                    .stats
                    .iter()
                    .map(|stat| &stat.spec)
                    .collect::<Vec<_>>(),
                &picked.stats,
            );
            for (group, change) in [(Group::Charts, chart_change), (Group::Stats, stat_change)] {
                if change == PickChange::Restart {
                    self.cancel_queries(&session_id, &[group]);
                }
            }
            let Some(session) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            apply_charts(session, chart_change, picked.charts, at);
            apply_stats(session, stat_change, picked.stats, at);
            if chart_change != PickChange::Unchanged || stat_change != PickChange::Unchanged {
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
        let shift = self.shift_ns();
        let session = self.sessions.get_mut(&inflight.session_id)?;
        let tier = session.grid.tier();
        let group = inflight.purpose.group();
        if &inflight.session_id != session_id
            || inflight.generation != generation
            || session.round(group).generation != generation
        {
            return None;
        }
        let window = session.grid.window_at(session.chart_round.at_utc_ns, shift);
        let again = match (inflight.purpose, result) {
            (Purpose::Bars(index), Ok(OverviewAnswer::Bars(bars))) => {
                session.disk = Some((bars.kept_on_disk, bars.store_issue.clone()));
                let chart = session.charts.get_mut(index)?;
                chart.error = None;
                let from = chart.bars.read_from(window);
                !chart.bars.take(bars, (from, window.0), (tier, shift))
            }
            (Purpose::Windows(index), Ok(OverviewAnswer::Windows(windows))) => {
                let stat = session.stats.get_mut(index)?;
                stat.error = None;
                stat.windows = Some(windows);
                false
            }
            (purpose, Ok(_)) => {
                session.set_error(purpose, "history answered another read".to_owned());
                false
            }
            (purpose, Err(error)) => {
                session.set_error(purpose, error.message);
                false
            }
        };
        if again {
            // The history's past changed: read the whole window again in
            // this round (a later round reads it anyway).
            let round = session.round_mut(group);
            if round.serial == inflight.round {
                round.queue.push_back(inflight.purpose.index());
            }
            return None;
        }
        if !session.round_mut(group).finish(inflight.round) {
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
            .flat_map(|session| {
                [
                    session.chart_round.refresh_due_ns,
                    session.stat_round.refresh_due_ns,
                ]
            });
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

    fn cancel_queries(&mut self, session_id: &SessionId, groups: &[Group]) {
        // Entries stay until their answer or deadline so the running-job
        // bound holds; their answers are dropped by the generation check.
        for inflight in self.inflight.values().filter(|inflight| {
            &inflight.session_id == session_id && groups.contains(&inflight.purpose.group())
        }) {
            inflight.cancel.cancel();
        }
    }

    fn schedule_refreshes(&mut self, now_ns: u64, limited: bool) {
        let at = (now_ns, self.now_utc_ns());
        let shift = self.shift_ns();
        for session in self.sessions.values_mut().filter(|session| session.active) {
            for group in [Group::Charts, Group::Stats] {
                if now_ns < session.round(group).refresh_due_ns {
                    continue;
                }
                session.begin_round(group, at);
                if group == Group::Charts {
                    let (start, _) = session.grid.window_at(at.1, shift);
                    for chart in &mut session.charts {
                        chart.bars.trim(start);
                    }
                }
                if limited {
                    let period = match group {
                        Group::Charts => session.grid.refresh_ms(),
                        Group::Stats => STATS_REFRESH_MS,
                    };
                    let slower = ms_to_ns(period * (LIMITED_REFRESH_FACTOR - 1));
                    let round = session.round_mut(group);
                    round.refresh_due_ns = round.refresh_due_ns.saturating_add(slower);
                }
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
            let group = inflight.purpose.group();
            if session.round(group).generation != inflight.generation {
                continue;
            }
            session.set_error(inflight.purpose, "history read timed out".to_owned());
            if session.round_mut(group).finish(inflight.round) {
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

    /// Sends the session's next read: the charts' before the statistics'.
    fn dispatch_next(&mut self, session_id: &SessionId, now_ns: u64) -> Dispatched {
        let shift = self.shift_ns();
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Dispatched::Nothing;
        };
        let purpose = match session.chart_round.queue.pop_front() {
            Some(index) => Purpose::Bars(index),
            None => match session.stat_round.queue.pop_front() {
                Some(index) => Purpose::Windows(index),
                None => return Dispatched::Nothing,
            },
        };
        if purpose.index() >= session.series_count(purpose.group()) {
            let round_over = session.round_mut(purpose.group()).finish_one();
            return Dispatched::Failed { round_over };
        }
        let request = request_of(session, purpose, shift);
        let (generation, round) = {
            let round = session.round(purpose.group());
            (round.generation, round.serial)
        };
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
                        purpose,
                        deadline_ns,
                        cancel,
                    },
                );
                Dispatched::Sent
            }
            Err(DispatchError::Busy) => {
                let round = session.round_mut(purpose.group());
                round.queue.push_front(purpose.index());
                Dispatched::PortBusy
            }
            Err(error) => {
                session.set_error(purpose, format!("history unavailable: {error}"));
                let round_over = session.round_mut(purpose.group()).finish_one();
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
        let at = if session.chart_round.at_utc_ns == 0 {
            self.now_utc_ns()
        } else {
            session.chart_round.at_utc_ns
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
            summaries: summaries_of(&session.stats, session.active),
            // The presentation adds the processes (D-64).
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
            session.restart(Group::Charts);
            session.charts = specs.into_iter().map(ChartState::new).collect();
            session.begin_round(Group::Charts, at);
        }
    }
}

/// Applies the statistics series a pick chose.
fn apply_stats(
    session: &mut SessionHistory,
    change: PickChange,
    specs: Vec<HistoryChartSpec>,
    at: (u64, i64),
) {
    match change {
        PickChange::Unchanged => {}
        PickChange::Redraw => {
            for (stat, spec) in session.stats.iter_mut().zip(specs) {
                stat.spec = spec;
            }
        }
        PickChange::Restart => {
            session.restart(Group::Stats);
            session.stats = specs.into_iter().map(StatState::new).collect();
            session.begin_round(Group::Stats, at);
        }
    }
}

/// The read of `purpose`: the bars a chart does not hold yet, or a
/// statistics series' windows, at its round's time.
fn request_of(session: &SessionHistory, purpose: Purpose, shift: i64) -> OverviewRequest {
    match purpose {
        Purpose::Bars(index) => {
            let chart = &session.charts[index];
            let at = session.chart_round.at_utc_ns;
            let window = session.grid.window_at(at, shift);
            OverviewRequest::Bars {
                series: SeriesInEveryBoot::from(&chart.spec.key),
                tier: session.grid.tier(),
                from_utc_ns: chart.bars.read_from(window),
                end_utc_ns: window.1,
                at_utc_ns: at,
            }
        }
        Purpose::Windows(index) => OverviewRequest::Windows {
            series: SeriesInEveryBoot::from(&session.stats[index].spec.key),
            spans_ns: STAT_WINDOWS.iter().map(|window| window.span_ns()).collect(),
            at_utc_ns: session.stat_round.at_utc_ns,
        },
    }
}

fn utc_u64(utc_ns: i64) -> u64 {
    u64::try_from(utc_ns).unwrap_or(0)
}

/// Unit of a series' metric (counts when unknown).
fn unit_of(metric_id: &str) -> CanonicalUnit {
    descriptor(metric_id).map_or(CanonicalUnit::Count, |known| known.unit)
}

/// A chart: a point per bar holding a value, with the mean, peak and time
/// held over the window, all merged from the bars.
fn chart_view(
    chart: &ChartState,
    (grid, window): (BarGrid, (i64, i64)),
    (active, merges): (bool, bool),
) -> HistoryChartView {
    let spec = &chart.spec;
    let step = grid.step_ns();
    let mut whole = OverviewBucket::new(window.0);
    let points: Vec<HistoryPointView> = chart
        .bars
        .bars_in(window)
        .map(|bar| {
            whole.merge(bar);
            HistoryPointView {
                start_utc_ns: utc_u64(bar.start_utc_ns),
                end_utc_ns: utc_u64(bar.start_utc_ns.saturating_add(step)),
                mean: bar.mean(),
                min: bar.low(),
                max: bar.peak(),
            }
        })
        .collect();
    let answered = chart.bars.answered;
    let covered_ns = answered.then_some(whole.covered_ns);
    HistoryChartView {
        kind: spec.kind,
        subject: spec.subject.clone(),
        series: spec.key.clone(),
        unit: unit_of(spec.key.metric_id.as_str()),
        scale_max: spec.scale_max,
        points,
        mean: whole.mean(),
        peak: whole.peak(),
        coverage_ratio: covered_ns.map(|covered| HistoryResult::ratio(covered, grid.span_ns())),
        covered_ns,
        display_downsampled: merges,
        pending: active && !answered && chart.error.is_none(),
        error: chart.error.clone(),
        // The presentation adds the processes' parts (D-64).
        parts: Vec::new(),
    }
}

/// The statistics: per series and window its mean, peak, time held and,
/// for power, the energy over that time.
fn summaries_of(stats: &[StatState], active: bool) -> Vec<HistoryWindowSummary> {
    let mut summaries = Vec::new();
    for stat in stats {
        for (position, window) in STAT_WINDOWS.into_iter().enumerate() {
            let bucket = stat
                .windows
                .as_ref()
                .and_then(|windows| windows.get(position));
            let covered_ns = bucket.map(|bucket| bucket.covered_ns);
            summaries.push(HistoryWindowSummary {
                kind: stat.spec.kind,
                subject: stat.spec.subject.clone(),
                window_label: window.label().to_owned(),
                unit: unit_of(stat.spec.key.metric_id.as_str()),
                mean: bucket.and_then(OverviewBucket::mean),
                peak: bucket.and_then(OverviewBucket::peak),
                coverage_ratio: covered_ns
                    .map(|covered| HistoryResult::ratio(covered, window.span_ns())),
                covered_ns,
                integral: (stat.spec.kind == HistoryChartKind::GpuPower)
                    .then_some(bucket)
                    .flatten()
                    .map(|bucket| bucket.integral().unwrap_or(0.0)),
                pending: active && bucket.is_none() && stat.error.is_none(),
            });
        }
    }
    summaries
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
        summaries: Vec::new(),
        disk: DiskHistory::Off,
        error: None,
    }
}

#[cfg(test)]
mod tests;
