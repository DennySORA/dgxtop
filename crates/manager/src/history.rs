//! HistoryService: bounded, per-session history queries for the history
//! view (STORAGE.md §5, UI.md §6, D-46, D-48).
//!
//! * Charts: the overview's series over a longer window, as the
//!   presentation picks them ([`HistoryChartSpec`]): the CPU total, the RAM
//!   in use, each GPU's utilization and the rates of a disk and an
//!   interface. Window: the session filter token (`5m`, `1h`, `6h`, `12h`,
//!   `24h`), default `1h`. Every chart of a session queries the same window,
//!   so all of them share one time axis.
//! * Statistics (D-48): each GPU's utilization, memory in use, temperature
//!   and power over the last 1 h, 6 h, 12 h and 24 h: per series and window
//!   one mean and one maximum over the whole window, refreshed once a
//!   minute. A window change redraws the charts and keeps the statistics.
//! * Only sessions currently on the history view query. A window, chart or
//!   resolution change bumps the charts' generation (new statistics series
//!   bump theirs), cancels the group's running queries and makes their late
//!   results stale.
//! * At most `queues.history_requests` jobs run at once, each with deadline
//!   `now + runtime.history_deadline_ms`; the charts' queries go first.
//!   Resolution keeps the point count ≤ max(chart columns, 60) and
//!   ≤ `store.max_history_points`.
//! * Per chart the mean per point and, when a point merges several raw
//!   samples, the maximum, so a spike is never hidden by the mean
//!   (STORAGE.md §4). Refreshed every 5 s for 5 minutes and at the
//!   resolution for longer windows (at most once a minute); three times
//!   slower while throttled.
//! * History kept on disk (D-51): memory holds this run; the time before it
//!   started (earlier runs and boots) is read from the history archive, once
//!   per chart and statistics window of a generation (those minutes no
//!   longer change), at most two reads at a time. The view puts both on the
//!   wall clock.

mod view;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use dgxtop_core::clock::{NANOS_PER_MILLI, TimeProvider, ms_to_ns};
use dgxtop_core::commands::ViewKind;
use dgxtop_core::errors::ErrorDetail;
use dgxtop_core::history::{
    ARCHIVE_MINUTE_NS, ArchiveQuery, ArchiveResult, HISTORY_SCHEMA_VERSION, HistoryQuery,
    HistoryResult, HistoryStat, MAX_HISTORY_POINTS, SeriesInEveryBoot,
};
use dgxtop_core::identity::{MetricKey, SessionId};
use dgxtop_core::ports::{ArchiveReadJob, CancelFlag, Dispatch, DispatchError, HistoryJob};
use dgxtop_core::view::{
    DiskHistory, HistoryChartKind, HistoryChartView, HistoryView, HistoryWindowSummary,
    SessionState, history_plot_columns,
};
use uuid::Uuid;

use self::view::{Evidence, SummaryInput, UtcWindow, Wall, summary, unit_of};

/// Refresh bounds: 5 minutes refresh every 5 s, longer windows at their
/// resolution, never less often than once a minute.
const MIN_REFRESH_MS: u64 = 5_000;
const MAX_REFRESH_MS: u64 = 60_000;
/// Throttled sessions refresh this many times slower.
const LIMITED_REFRESH_FACTOR: u64 = 3;
/// Grace after a job deadline before the slot is reclaimed locally.
const RESULT_GRACE_NS: u64 = 100 * NANOS_PER_MILLI;
/// Fewest points a chart query asks for; the history view keeps them all
/// (the Viewer merges them into its columns).
pub(crate) const MIN_DISPLAY_POINTS: u32 = 60;
const MIN_RESOLUTION_MS: u64 = 100;
const MAX_RESOLUTION_MS: u64 = 86_400_000;
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
/// Archive reads running at once: a disk read never crowds out memory.
const MAX_ARCHIVE_INFLIGHT: usize = 2;
/// Points of a statistics window's archive read: the window on the UTC
/// grid of its own width straddles at most two.
const WINDOW_ARCHIVE_POINTS: u32 = 4;

/// Supported history windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HistoryWindow {
    FiveMinutes,
    OneHour,
    SixHours,
    TwelveHours,
    OneDay,
}

/// Window used when the session gives none.
pub(crate) const DEFAULT_WINDOW: HistoryWindow = HistoryWindow::OneHour;

impl HistoryWindow {
    /// Parses a window token; the empty token is not a window.
    pub(crate) fn parse(token: &str) -> Option<Self> {
        match token {
            "5m" => Some(Self::FiveMinutes),
            "1h" => Some(Self::OneHour),
            "6h" => Some(Self::SixHours),
            "12h" => Some(Self::TwelveHours),
            "24h" => Some(Self::OneDay),
            _ => None,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
            Self::SixHours => "6h",
            Self::TwelveHours => "12h",
            Self::OneDay => "24h",
        }
    }

    const fn span_ms(self) -> u64 {
        match self {
            Self::FiveMinutes => 300_000,
            Self::OneHour => 3_600_000,
            Self::SixHours => 21_600_000,
            Self::TwelveHours => 43_200_000,
            Self::OneDay => 86_400_000,
        }
    }

    fn span_ns(self) -> u64 {
        ms_to_ns(self.span_ms())
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

/// One query of a session: the mean or the maximum of chart `index` at the
/// view's resolution, or of statistics series `index` over a whole window;
/// or the archive's minutes of either before this run (D-51).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Mean(usize),
    Max(usize),
    WindowMean(usize, HistoryWindow),
    WindowMax(usize, HistoryWindow),
    Archive(usize),
    WindowArchive(usize, HistoryWindow),
}

impl Purpose {
    const fn group(self) -> Group {
        match self {
            Self::Mean(_) | Self::Max(_) | Self::Archive(_) => Group::Charts,
            Self::WindowMean(..) | Self::WindowMax(..) | Self::WindowArchive(..) => Group::Stats,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Mean(index)
            | Self::Max(index)
            | Self::Archive(index)
            | Self::WindowMean(index, _)
            | Self::WindowMax(index, _)
            | Self::WindowArchive(index, _) => index,
        }
    }

    /// Whether this is a mean (its failure shows on the series).
    const fn is_mean(self) -> bool {
        matches!(self, Self::Mean(_) | Self::WindowMean(..))
    }
}

/// The window a session on the history view wants charted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wanted {
    window: HistoryWindow,
    resolution_ms: u32,
    points_target: u32,
}

#[derive(Debug)]
struct Inflight {
    session_id: SessionId,
    generation: u64,
    purpose: Purpose,
    deadline_ns: u64,
    cancel: CancelFlag,
}

/// The archive's answer for one chart or statistics window (D-51): asked
/// once a generation; a failed read is asked again next round.
#[derive(Debug, Default)]
struct Archived {
    result: Option<ArchiveResult>,
    asked: bool,
}

#[derive(Debug)]
struct ChartState {
    spec: HistoryChartSpec,
    mean: Option<HistoryResult>,
    maximum: Option<HistoryResult>,
    archived: Archived,
    /// Whole-window results of a statistics series.
    windows: BTreeMap<HistoryWindow, WindowResults>,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct WindowResults {
    mean: Option<HistoryResult>,
    maximum: Option<HistoryResult>,
    archived: Archived,
}

impl ChartState {
    fn new(spec: HistoryChartSpec) -> Self {
        Self {
            spec,
            mean: None,
            maximum: None,
            archived: Archived::default(),
            windows: BTreeMap::new(),
            error: None,
        }
    }

    fn clear_results(&mut self) {
        self.mean = None;
        self.maximum = None;
        self.archived = Archived::default();
        self.windows.clear();
        self.error = None;
    }

    /// The archive slot a purpose fills.
    fn archived_mut(&mut self, purpose: Purpose) -> Option<&mut Archived> {
        match purpose {
            Purpose::Archive(_) => Some(&mut self.archived),
            Purpose::WindowArchive(_, window) => {
                Some(&mut self.windows.entry(window).or_default().archived)
            }
            _ => None,
        }
    }
}

/// The series of one query group with its queues and schedule.
#[derive(Debug, Default)]
struct SeriesGroup {
    generation: u64,
    series: Vec<ChartState>,
    /// Memory queries.
    queue: VecDeque<Purpose>,
    /// Archive reads (D-51), dispatched on their own so a slow disk never
    /// holds up memory.
    archive_queue: VecDeque<Purpose>,
    /// End of the windows every query of the current round asks for.
    end_ns: u64,
    refresh_due_ns: u64,
}

impl SeriesGroup {
    fn specs(&self) -> Vec<HistoryChartSpec> {
        self.series.iter().map(|chart| chart.spec.clone()).collect()
    }

    /// Drops the group's results and queues for a new generation.
    fn restart(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.queue.clear();
        self.archive_queue.clear();
        for chart in &mut self.series {
            chart.clear_results();
        }
    }

    /// Queues `purposes` for the windows ending at `now_ns`, the next round
    /// `refresh_ms` later.
    fn begin_round(&mut self, purposes: Vec<Purpose>, now_ns: u64, refresh_ms: u64) {
        self.end_ns = now_ns;
        for purpose in purposes {
            if !self.queue.contains(&purpose) {
                self.queue.push_back(purpose);
            }
        }
        self.refresh_due_ns = now_ns.saturating_add(ms_to_ns(refresh_ms));
    }
}

/// How a new pick changes a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickChange {
    Unchanged,
    /// Same series with a new scale or name: redraw only.
    Redraw,
    /// Other series: query again.
    Restart,
}

/// Applies `picked` to `group`; a restart is left to the caller.
fn pick_change(group: &SeriesGroup, picked: &[HistoryChartSpec]) -> PickChange {
    let current = group.specs();
    if picked == current.as_slice() {
        return PickChange::Unchanged;
    }
    let same_series = picked.len() == current.len()
        && picked
            .iter()
            .zip(&current)
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
    window: HistoryWindow,
    resolution_ms: u32,
    points_target: u32,
    /// Snapshot revision the series were last picked from.
    charts_revision: Option<u64>,
    charts: SeriesGroup,
    stats: SeriesGroup,
    /// Why the last archive read failed, until one succeeds (D-51).
    disk_failure: Option<String>,
    view: Arc<HistoryView>,
}

impl SessionHistory {
    fn group(&self, group: Group) -> &SeriesGroup {
        match group {
            Group::Charts => &self.charts,
            Group::Stats => &self.stats,
        }
    }

    fn group_mut(&mut self, group: Group) -> &mut SeriesGroup {
        match group {
            Group::Charts => &mut self.charts,
            Group::Stats => &mut self.stats,
        }
    }

    /// Queues every chart's queries for the window ending now, and the
    /// archive reads of charts not read yet when the window starts before
    /// `since` (this run's start, with an archive).
    fn begin_chart_round(&mut self, raw_interval_ms: u32, now_ns: u64, since: Option<u64>) {
        let maxima = merges_samples(self.resolution_ms, raw_interval_ms);
        let purposes = (0..self.charts.series.len())
            .flat_map(|index| {
                let maximum = maxima.then_some(Purpose::Max(index));
                std::iter::once(Purpose::Mean(index)).chain(maximum)
            })
            .collect();
        let refresh = refresh_ms(self.resolution_ms);
        self.charts.begin_round(purposes, now_ns, refresh);
        if reaches_archive(since, now_ns, self.window.span_ns()) {
            queue_unasked(&mut self.charts, |index| {
                std::iter::once(Purpose::Archive(index))
            });
        }
    }

    /// Queues every statistics series' queries over the windows ending now,
    /// and the archive reads of the windows that start before `since`.
    fn begin_stats_round(&mut self, now_ns: u64, since: Option<u64>) {
        let purposes = (0..self.stats.series.len())
            .flat_map(|index| {
                STAT_WINDOWS.into_iter().flat_map(move |window| {
                    [
                        Purpose::WindowMean(index, window),
                        Purpose::WindowMax(index, window),
                    ]
                })
            })
            .collect();
        self.stats.begin_round(purposes, now_ns, STATS_REFRESH_MS);
        let archived: Vec<HistoryWindow> = STAT_WINDOWS
            .into_iter()
            .filter(|window| reaches_archive(since, now_ns, window.span_ns()))
            .collect();
        queue_unasked(&mut self.stats, |index| {
            archived
                .iter()
                .map(move |window| Purpose::WindowArchive(index, *window))
        });
    }
}

/// Whether the archive holds part of a window of `span_ns` ending at
/// `end_ns`: there is an archive and the window starts before this run
/// (`since`) (D-51).
fn reaches_archive(since: Option<u64>, end_ns: u64, span_ns: u64) -> bool {
    since.is_some_and(|since| end_ns.saturating_sub(span_ns) < since)
}

/// Static settings of the history service.
#[derive(Debug, Clone)]
pub(crate) struct HistorySettings {
    pub(crate) max_inflight: usize,
    pub(crate) deadline_ms: u32,
    pub(crate) max_points: u32,
}

/// What a tick did.
#[derive(Debug, Default)]
pub(crate) struct HistoryTick {
    pub(crate) dirty: Vec<SessionId>,
    pub(crate) dispatched: u32,
    pub(crate) expired: u32,
}

/// The history archive the view reads back (D-51).
pub(crate) struct ArchiveSide {
    pub(crate) port: Box<dyn Dispatch<ArchiveReadJob>>,
    /// Boot time this run's history begins: the archive answers before it.
    pub(crate) since_boot_ns: u64,
}

pub(crate) struct HistoryService {
    port: Box<dyn Dispatch<HistoryJob>>,
    archive: Option<ArchiveSide>,
    clock: Arc<dyn TimeProvider>,
    settings: HistorySettings,
    raw_interval_ms: u32,
    sessions: BTreeMap<SessionId, SessionHistory>,
    inflight: BTreeMap<Uuid, Inflight>,
    archive_inflight: BTreeMap<Uuid, Inflight>,
    cursor: usize,
}

impl HistoryService {
    pub(crate) fn new(
        port: Box<dyn Dispatch<HistoryJob>>,
        archive: Option<ArchiveSide>,
        clock: Arc<dyn TimeProvider>,
        settings: HistorySettings,
        raw_interval_ms: u32,
    ) -> Self {
        Self {
            port,
            archive,
            clock,
            settings: HistorySettings {
                max_inflight: settings.max_inflight.max(1),
                ..settings
            },
            raw_interval_ms,
            sessions: BTreeMap::new(),
            inflight: BTreeMap::new(),
            archive_inflight: BTreeMap::new(),
            cursor: 0,
        }
    }

    /// Raw sampling cadence (decides whether a point merges samples).
    pub(crate) fn set_raw_interval(&mut self, raw_interval_ms: u32) {
        self.raw_interval_ms = raw_interval_ms;
    }

    pub(crate) fn open_session(&mut self, session_id: &SessionId) {
        let mut session = SessionHistory {
            active: false,
            window: DEFAULT_WINDOW,
            resolution_ms: 0,
            points_target: MIN_DISPLAY_POINTS,
            charts_revision: None,
            charts: SeriesGroup::default(),
            stats: SeriesGroup::default(),
            disk_failure: None,
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
        for inflight in self.inflight.values().chain(self.archive_inflight.values()) {
            inflight.cancel.cancel();
        }
        self.inflight.clear();
        self.archive_inflight.clear();
        for session in self.sessions.values_mut() {
            for group in [&mut session.charts, &mut session.stats] {
                group.queue.clear();
                group.archive_queue.clear();
            }
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
    /// changed. Opening the view picks its series afresh; a window change
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
            Some(wanted) => {
                opening
                    || session.window != wanted.window
                    || session.resolution_ms != wanted.resolution_ms
            }
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
        let Some(session) = self.sessions.get_mut(session_id) else {
            return false;
        };
        session.charts.restart();
        if leaving_or_opening {
            session.stats.restart();
        }
        let Some(wanted) = wanted else {
            session.active = false;
            self.rebuild(session_id);
            return true;
        };
        session.window = wanted.window;
        session.resolution_ms = wanted.resolution_ms;
        session.points_target = wanted.points_target;
        let since = self.archive.as_ref().map(|archive| archive.since_boot_ns);
        if opening {
            // Pick the series afresh: the busiest disk and interface are
            // those of the moment the view opens.
            session.charts.series.clear();
            session.stats.series.clear();
            session.charts_revision = None;
            session.begin_stats_round(now_ns, since);
        }
        session.active = true;
        session.begin_chart_round(self.raw_interval_ms, now_ns, since);
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
    /// group's queries; a new scale or name only redraws. Returns the
    /// sessions whose view changed.
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
        let mut dirty = Vec::new();
        for session_id in due {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            session.charts_revision = Some(revision);
            let shown = session.charts.specs();
            let mut picked = pick(&ChartRequest { shown: &shown });
            picked.charts.truncate(MAX_CHARTS);
            picked.stats.truncate(MAX_STAT_SERIES);
            let changes = [
                (
                    Group::Charts,
                    pick_change(&session.charts, &picked.charts),
                    picked.charts,
                ),
                (
                    Group::Stats,
                    pick_change(&session.stats, &picked.stats),
                    picked.stats,
                ),
            ];
            let mut changed = false;
            for (group, change, specs) in changes {
                changed |= change != PickChange::Unchanged;
                self.apply_pick(&session_id, group, change, specs, now_ns);
            }
            if changed {
                self.rebuild(&session_id);
                dirty.push(session_id);
            }
        }
        dirty
    }

    /// Applies one group's new series.
    fn apply_pick(
        &mut self,
        session_id: &SessionId,
        group: Group,
        change: PickChange,
        specs: Vec<HistoryChartSpec>,
        now_ns: u64,
    ) {
        if change == PickChange::Restart {
            self.cancel_queries(session_id, &[group]);
        }
        let raw_interval_ms = self.raw_interval_ms;
        let since = self.archive.as_ref().map(|archive| archive.since_boot_ns);
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        match change {
            PickChange::Unchanged => {}
            PickChange::Redraw => {
                for (chart, spec) in session.group_mut(group).series.iter_mut().zip(specs) {
                    chart.spec = spec;
                }
            }
            PickChange::Restart => {
                let target = session.group_mut(group);
                target.restart();
                target.series = specs.into_iter().map(ChartState::new).collect();
                match group {
                    Group::Charts => session.begin_chart_round(raw_interval_ms, now_ns, since),
                    Group::Stats => session.begin_stats_round(now_ns, since),
                }
            }
        }
    }

    pub(crate) fn tick(&mut self, now_ns: u64, limited: bool) -> HistoryTick {
        let mut tick = HistoryTick::default();
        self.schedule_refreshes(now_ns, limited);
        self.expire(now_ns, &mut tick);
        self.dispatch(now_ns, &mut tick);
        self.dispatch_archive_reads(now_ns, &mut tick);
        tick
    }

    /// Applies a result; `None` means it was stale (old generation/unknown).
    pub(crate) fn on_result(
        &mut self,
        request_id: Uuid,
        session_id: &SessionId,
        generation: u64,
        result: Result<HistoryResult, ErrorDetail>,
    ) -> Option<SessionId> {
        let inflight = self.inflight.remove(&request_id)?;
        let session = self.sessions.get_mut(&inflight.session_id)?;
        let group = session.group_mut(inflight.purpose.group());
        if &inflight.session_id != session_id
            || inflight.generation != generation
            || group.generation != generation
        {
            return None;
        }
        let chart = group.series.get_mut(inflight.purpose.index())?;
        match (inflight.purpose, result) {
            (Purpose::Mean(_), Ok(result)) => {
                chart.error = None;
                chart.mean = Some(result);
            }
            (Purpose::Max(_), Ok(result)) => chart.maximum = Some(result),
            (Purpose::WindowMean(_, window), Ok(result)) => {
                chart.windows.entry(window).or_default().mean = Some(result);
            }
            (Purpose::WindowMax(_, window), Ok(result)) => {
                chart.windows.entry(window).or_default().maximum = Some(result);
            }
            // Archive reads answer through `on_archive_result`.
            (Purpose::Archive(_) | Purpose::WindowArchive(..), Ok(_)) => return None,
            (purpose, Err(error)) => {
                if purpose.is_mean() {
                    chart.error = Some(error.message);
                }
            }
        }
        let session_id = inflight.session_id;
        self.rebuild(&session_id);
        Some(session_id)
    }

    /// Applies an archive read (D-51); `None` means it was stale. A failed
    /// read shows in the view and is asked again next round.
    pub(crate) fn on_archive_result(
        &mut self,
        request_id: Uuid,
        session_id: &SessionId,
        generation: u64,
        result: Result<ArchiveResult, ErrorDetail>,
    ) -> Option<SessionId> {
        let inflight = self.archive_inflight.remove(&request_id)?;
        let session = self.sessions.get_mut(&inflight.session_id)?;
        let group = session.group_mut(inflight.purpose.group());
        if &inflight.session_id != session_id
            || inflight.generation != generation
            || group.generation != generation
        {
            return None;
        }
        let slot = group
            .series
            .get_mut(inflight.purpose.index())?
            .archived_mut(inflight.purpose)?;
        match result {
            Ok(result) => {
                slot.result = Some(result);
                session.disk_failure = None;
            }
            Err(error) => {
                slot.asked = false;
                session.disk_failure = Some(error.message);
            }
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
            .flat_map(|session| [session.charts.refresh_due_ns, session.stats.refresh_due_ns]);
        let deadlines = self
            .inflight
            .values()
            .chain(self.archive_inflight.values())
            .map(|inflight| inflight.deadline_ns.saturating_add(RESULT_GRACE_NS));
        refreshes.chain(deadlines).min()
    }

    /// The window a session on the history view wants charted; other views
    /// query no history (the GPU view shows live data, D-48).
    fn wanted(&self, state: &SessionState, columns: u16) -> Option<Wanted> {
        if state.view != ViewKind::History {
            return None;
        }
        let window = HistoryWindow::parse(&state.filter).unwrap_or(DEFAULT_WINDOW);
        let upper = self.settings.max_points.clamp(1, MAX_HISTORY_POINTS);
        let plot_columns = history_plot_columns(columns);
        let points_target = u32::from(plot_columns).clamp(MIN_DISPLAY_POINTS.min(upper), upper);
        Some(Wanted {
            window,
            resolution_ms: resolution_for(window, points_target),
            points_target,
        })
    }

    fn cancel_queries(&mut self, session_id: &SessionId, groups: &[Group]) {
        // Entries stay until their result or deadline so the running-job
        // bound holds; their results are dropped by the generation check.
        let running = self.inflight.values().chain(self.archive_inflight.values());
        for inflight in running.filter(|inflight| {
            &inflight.session_id == session_id && groups.contains(&inflight.purpose.group())
        }) {
            inflight.cancel.cancel();
        }
    }

    /// Sends queued archive reads, at most [`MAX_ARCHIVE_INFLIGHT`] at once.
    fn dispatch_archive_reads(&mut self, now_ns: u64, tick: &mut HistoryTick) {
        let ids: Vec<SessionId> = self.sessions.keys().cloned().collect();
        for session_id in ids {
            while self.archive_inflight.len() < MAX_ARCHIVE_INFLIGHT {
                match self.dispatch_archive_read(&session_id, now_ns) {
                    Dispatched::Sent => tick.dispatched = tick.dispatched.saturating_add(1),
                    Dispatched::Nothing => break,
                    Dispatched::PortBusy => return,
                    Dispatched::Failed => tick.dirty.push(session_id.clone()),
                }
            }
        }
    }

    fn dispatch_archive_read(&mut self, session_id: &SessionId, now_ns: u64) -> Dispatched {
        let Some(archive) = self.archive.as_ref() else {
            return Dispatched::Nothing;
        };
        let wall = Wall::now(self.clock.as_ref());
        let since_utc = wall.utc_of(archive.since_boot_ns);
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Dispatched::Nothing;
        };
        let Some(purpose) = session
            .charts
            .archive_queue
            .pop_front()
            .or_else(|| session.stats.archive_queue.pop_front())
        else {
            return Dispatched::Nothing;
        };
        let Some(query) = archive_query(session, purpose, wall, since_utc) else {
            // Nothing before this run in that window after all: an empty
            // answer, so the slot is not asked again.
            fill_empty(session, purpose);
            return Dispatched::Nothing;
        };
        let generation = session.group(purpose.group()).generation;
        let cancel = CancelFlag::new();
        let deadline_ns = now_ns.saturating_add(ms_to_ns(u64::from(self.settings.deadline_ms)));
        let request_id = query.request_id;
        let job = ArchiveReadJob {
            query,
            session_id: session_id.clone(),
            session_generation: generation,
            deadline_boot_ns: deadline_ns,
            cancel: cancel.clone(),
        };
        match archive.port.try_dispatch(job) {
            Ok(()) => {
                self.archive_inflight.insert(
                    request_id,
                    Inflight {
                        session_id: session_id.clone(),
                        generation,
                        purpose,
                        deadline_ns,
                        cancel,
                    },
                );
                Dispatched::Sent
            }
            Err(DispatchError::Busy) => {
                session
                    .group_mut(purpose.group())
                    .archive_queue
                    .push_front(purpose);
                Dispatched::PortBusy
            }
            Err(error) => {
                session.disk_failure = Some(format!("history archive unavailable: {error}"));
                if let Some(slot) = session
                    .group_mut(purpose.group())
                    .series
                    .get_mut(purpose.index())
                    .and_then(|chart| chart.archived_mut(purpose))
                {
                    slot.asked = false;
                }
                self.rebuild(session_id);
                Dispatched::Failed
            }
        }
    }

    fn schedule_refreshes(&mut self, now_ns: u64, limited: bool) {
        let raw_interval_ms = self.raw_interval_ms;
        let since = self.archive.as_ref().map(|archive| archive.since_boot_ns);
        for session in self.sessions.values_mut().filter(|session| session.active) {
            if now_ns >= session.charts.refresh_due_ns {
                session.begin_chart_round(raw_interval_ms, now_ns, since);
                if limited {
                    let slower = refresh_ms(session.resolution_ms) * (LIMITED_REFRESH_FACTOR - 1);
                    session.charts.refresh_due_ns = session
                        .charts
                        .refresh_due_ns
                        .saturating_add(ms_to_ns(slower));
                }
            }
            if now_ns >= session.stats.refresh_due_ns {
                session.begin_stats_round(now_ns, since);
                if limited {
                    let slower = STATS_REFRESH_MS * (LIMITED_REFRESH_FACTOR - 1);
                    session.stats.refresh_due_ns = session
                        .stats
                        .refresh_due_ns
                        .saturating_add(ms_to_ns(slower));
                }
            }
        }
    }

    /// Archive reads past their deadline are asked again next round.
    fn expire_archive_reads(&mut self, now_ns: u64, tick: &mut HistoryTick) {
        let overdue: Vec<Uuid> = self
            .archive_inflight
            .iter()
            .filter(|(_, inflight)| now_ns >= inflight.deadline_ns.saturating_add(RESULT_GRACE_NS))
            .map(|(id, _)| *id)
            .collect();
        for request_id in overdue {
            let Some(inflight) = self.archive_inflight.remove(&request_id) else {
                continue;
            };
            inflight.cancel.cancel();
            tick.expired = tick.expired.saturating_add(1);
            let Some(session) = self.sessions.get_mut(&inflight.session_id) else {
                continue;
            };
            let group = session.group_mut(inflight.purpose.group());
            if group.generation != inflight.generation {
                continue;
            }
            if let Some(slot) = group
                .series
                .get_mut(inflight.purpose.index())
                .and_then(|chart| chart.archived_mut(inflight.purpose))
            {
                slot.asked = false;
            }
            session.disk_failure = Some("history archive read timed out".to_owned());
            let session_id = inflight.session_id.clone();
            self.rebuild(&session_id);
            tick.dirty.push(session_id);
        }
    }

    fn expire(&mut self, now_ns: u64, tick: &mut HistoryTick) {
        self.expire_archive_reads(now_ns, tick);
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
            if !inflight.purpose.is_mean() {
                continue;
            }
            let chart = self
                .sessions
                .get_mut(&inflight.session_id)
                .map(|session| session.group_mut(inflight.purpose.group()))
                .filter(|group| group.generation == inflight.generation)
                .and_then(|group| group.series.get_mut(inflight.purpose.index()));
            if let Some(chart) = chart {
                chart.error = Some("history query timed out".to_owned());
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
                Dispatched::Failed => {
                    idle_rounds = 0;
                    tick.dirty.push(session_id.clone());
                }
            }
        }
    }

    /// Sends the session's next query: the charts' before the statistics'.
    fn dispatch_next(&mut self, session_id: &SessionId, now_ns: u64) -> Dispatched {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Dispatched::Nothing;
        };
        let Some(purpose) = session
            .charts
            .queue
            .pop_front()
            .or_else(|| session.stats.queue.pop_front())
        else {
            return Dispatched::Nothing;
        };
        let Some(query) = build_query(session, purpose, self.settings.max_points) else {
            return Dispatched::Nothing;
        };
        let generation = session.group(purpose.group()).generation;
        let cancel = CancelFlag::new();
        let deadline_ns = now_ns.saturating_add(ms_to_ns(u64::from(self.settings.deadline_ms)));
        let request_id = query.request_id;
        let job = HistoryJob {
            query,
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
                        purpose,
                        deadline_ns,
                        cancel,
                    },
                );
                Dispatched::Sent
            }
            Err(DispatchError::Busy) => {
                session.group_mut(purpose.group()).queue.push_front(purpose);
                Dispatched::PortBusy
            }
            Err(error) => {
                if purpose.is_mean()
                    && let Some(chart) = session
                        .group_mut(purpose.group())
                        .series
                        .get_mut(purpose.index())
                {
                    chart.error = Some(format!("history unavailable: {error}"));
                }
                self.rebuild(session_id);
                Dispatched::Failed
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
        let downsampled = merges_samples(session.resolution_ms, self.raw_interval_ms);
        let wall = Wall::now(self.clock.as_ref());
        let window = self.utc_window(wall, session.charts.end_ns, session.window.span_ns());
        HistoryView {
            window_label: session.window.label().to_owned(),
            requested_span_ns: session.window.span_ns(),
            start_utc_ns: window.start,
            end_utc_ns: window.end,
            charts: session
                .charts
                .series
                .iter()
                .map(|chart| {
                    chart_view(
                        chart,
                        (session.active, downsampled),
                        (wall, window, self.seam(wall)),
                    )
                })
                .collect(),
            summaries: self.summaries_of(&session.stats, session.active, wall),
            disk: match (&self.archive, &session.disk_failure) {
                (None, _) => DiskHistory::Off,
                (Some(_), None) => DiskHistory::On,
                (Some(_), Some(failure)) => DiskHistory::Failed(failure.clone()),
            },
            error: None,
        }
    }

    /// This run's start on the wall clock when the archive answers before it.
    fn seam(&self, wall: Wall) -> Option<u64> {
        self.archive
            .as_ref()
            .map(|archive| wall.utc_of(archive.since_boot_ns))
    }

    /// A window of `span_ns` ending at boot time `end_ns` on the wall clock;
    /// without the archive nothing before boot can be read, so it starts at
    /// boot at the earliest (D-50).
    fn utc_window(&self, wall: Wall, end_ns: u64, span_ns: u64) -> UtcWindow {
        let end = wall.utc_of(end_ns);
        let start = if self.archive.is_some() {
            end.saturating_sub(span_ns)
        } else {
            wall.utc_of(end_ns.saturating_sub(span_ns))
        };
        UtcWindow { start, end }
    }

    /// The statistics: per series and window its mean, peak, coverage and,
    /// for power, the energy over the covered time, from memory and the
    /// archive.
    fn summaries_of(
        &self,
        stats: &SeriesGroup,
        active: bool,
        wall: Wall,
    ) -> Vec<HistoryWindowSummary> {
        let mut summaries = Vec::new();
        for chart in &stats.series {
            for window in STAT_WINDOWS {
                let results = chart.windows.get(&window);
                let evidence = Evidence {
                    mean: results.and_then(|results| results.mean.as_ref()),
                    maximum: results.and_then(|results| results.maximum.as_ref()),
                    archived: results.and_then(|results| results.archived.result.as_ref()),
                    seam: self.seam(wall),
                };
                summaries.push(summary(&SummaryInput {
                    kind: chart.spec.kind,
                    subject: &chart.spec.subject,
                    metric_id: chart.spec.key.metric_id.as_str(),
                    window_label: window.label(),
                    window: self.utc_window(wall, stats.end_ns, window.span_ns()),
                    pending: active && evidence.mean.is_none() && chart.error.is_none(),
                    evidence,
                }));
            }
        }
        summaries
    }
}

enum Dispatched {
    Sent,
    Nothing,
    PortBusy,
    Failed,
}

/// Whether a point at `resolution_ms` merges several raw samples (and the
/// chart needs the maxima to keep spikes).
fn merges_samples(resolution_ms: u32, raw_interval_ms: u32) -> bool {
    u64::from(resolution_ms) > u64::from(raw_interval_ms.max(1))
}

/// Refresh period of a window shown at `resolution_ms`.
fn refresh_ms(resolution_ms: u32) -> u64 {
    u64::from(resolution_ms).clamp(MIN_REFRESH_MS, MAX_REFRESH_MS)
}

/// Resolution so that the window fits `points_target` points.
fn resolution_for(window: HistoryWindow, points_target: u32) -> u32 {
    let raw = window.span_ms().div_ceil(u64::from(points_target.max(1)));
    u32::try_from(raw.clamp(MIN_RESOLUTION_MS, MAX_RESOLUTION_MS)).unwrap_or(u32::MAX)
}

fn build_query(
    session: &SessionHistory,
    purpose: Purpose,
    configured_max_points: u32,
) -> Option<HistoryQuery> {
    let cap = configured_max_points.min(MAX_HISTORY_POINTS);
    let whole = |window: HistoryWindow| u32::try_from(window.span_ms()).unwrap_or(u32::MAX);
    let charted = (session.window, session.resolution_ms);
    let ((window, resolution_ms), stat, max_points) = match purpose {
        Purpose::Mean(_) => (charted, HistoryStat::Mean, session.points_target + 1),
        Purpose::Max(_) => (charted, HistoryStat::Max, session.points_target + 1),
        Purpose::WindowMean(_, window) => ((window, whole(window)), HistoryStat::Mean, 2),
        Purpose::WindowMax(_, window) => ((window, whole(window)), HistoryStat::Max, 2),
        Purpose::Archive(_) | Purpose::WindowArchive(..) => return None,
    };
    let group = session.group(purpose.group());
    let chart = group.series.get(purpose.index())?;
    let query = HistoryQuery {
        schema_version: HISTORY_SCHEMA_VERSION,
        request_id: Uuid::new_v4(),
        key: chart.spec.key.clone(),
        start_boot_ns: group.end_ns.saturating_sub(window.span_ns()),
        end_boot_ns: group.end_ns,
        resolution_ms,
        stat,
        max_points: max_points.min(cap),
        source_filter: None,
    };
    query.validate(configured_max_points).ok().map(|()| query)
}

/// Queues `purposes` of every series of `group` whose archive slot was not
/// asked yet this generation, and marks them asked.
fn queue_unasked<I>(group: &mut SeriesGroup, purposes: impl Fn(usize) -> I)
where
    I: Iterator<Item = Purpose>,
{
    for index in 0..group.series.len() {
        for purpose in purposes(index) {
            let Some(slot) = group
                .series
                .get_mut(index)
                .and_then(|chart| chart.archived_mut(purpose))
            else {
                continue;
            };
            if !slot.asked {
                slot.asked = true;
                group.archive_queue.push_back(purpose);
            }
        }
    }
}

/// Stores an empty archive answer for a slot whose window starts in this
/// run after all.
fn fill_empty(session: &mut SessionHistory, purpose: Purpose) {
    if let Some(slot) = session
        .group_mut(purpose.group())
        .series
        .get_mut(purpose.index())
        .and_then(|chart| chart.archived_mut(purpose))
    {
        slot.result = Some(ArchiveResult::empty(Uuid::nil()));
    }
}

/// Points of a chart's archive read: the view's resolution in whole
/// minutes, the archive's finest.
fn archive_point_ns(resolution_ms: u32) -> u64 {
    ms_to_ns(u64::from(resolution_ms))
        .div_ceil(ARCHIVE_MINUTE_NS)
        .max(1)
        .saturating_mul(ARCHIVE_MINUTE_NS)
}

/// The archive read of `purpose`: the part of its window before this run
/// (`since_utc`) on the wall clock; `None` when the window starts in this
/// run.
fn archive_query(
    session: &SessionHistory,
    purpose: Purpose,
    wall: Wall,
    since_utc: u64,
) -> Option<ArchiveQuery> {
    let (window, point_ns, max_points) = match purpose {
        Purpose::Archive(_) => {
            let point_ns = archive_point_ns(session.resolution_ms);
            let points = session
                .window
                .span_ns()
                .div_ceil(point_ns)
                .saturating_add(2);
            (session.window, point_ns, points)
        }
        Purpose::WindowArchive(_, window) => (
            window,
            window.span_ns().div_ceil(ARCHIVE_MINUTE_NS) * ARCHIVE_MINUTE_NS,
            u64::from(WINDOW_ARCHIVE_POINTS),
        ),
        _ => return None,
    };
    let group = session.group(purpose.group());
    let chart = group.series.get(purpose.index())?;
    let end = wall.utc_of(group.end_ns);
    let start = end.saturating_sub(window.span_ns());
    let before_run = since_utc.min(end);
    if start >= before_run {
        return None;
    }
    let query = ArchiveQuery {
        request_id: Uuid::new_v4(),
        series: SeriesInEveryBoot::from(&chart.spec.key),
        start_utc_ns: start,
        end_utc_ns: before_run,
        point_ns,
        max_points: u32::try_from(max_points)
            .unwrap_or(MAX_HISTORY_POINTS)
            .min(MAX_HISTORY_POINTS),
    };
    query.validate().ok().map(|()| query)
}

/// A chart on the wall clock: the archive's points up to `seam` (this
/// run's start), then this run's.
fn chart_view(
    chart: &ChartState,
    (active, downsampled): (bool, bool),
    (wall, window, seam): (Wall, UtcWindow, Option<u64>),
) -> HistoryChartView {
    let spec = &chart.spec;
    let evidence = Evidence {
        mean: chart.mean.as_ref(),
        maximum: chart.maximum.as_ref(),
        archived: chart.archived.result.as_ref(),
        seam,
    };
    let covered_ns = evidence.covered_ns(window);
    HistoryChartView {
        kind: spec.kind,
        subject: spec.subject.clone(),
        series: spec.key.clone(),
        unit: unit_of(spec.key.metric_id.as_str()),
        scale_max: spec.scale_max,
        points: evidence.points(wall, window),
        mean: evidence.mean(window),
        peak: evidence.peak(window),
        coverage_ratio: covered_ns.map(|covered| HistoryResult::ratio(covered, window.span())),
        covered_ns,
        truncated: chart.mean.as_ref().is_some_and(|result| result.truncated)
            || chart
                .archived
                .result
                .as_ref()
                .is_some_and(|result| result.truncated),
        display_downsampled: downsampled,
        pending: active && chart.mean.is_none() && chart.error.is_none(),
        error: chart.error.clone(),
    }
}

fn empty_view() -> HistoryView {
    HistoryView {
        window_label: DEFAULT_WINDOW.label().to_owned(),
        requested_span_ns: DEFAULT_WINDOW.span_ns(),
        start_utc_ns: 0,
        end_utc_ns: 0,
        charts: Vec::new(),
        summaries: Vec::new(),
        disk: DiskHistory::Off,
        error: None,
    }
}

#[cfg(test)]
mod tests;
