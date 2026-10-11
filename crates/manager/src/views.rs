//! ViewSessionService: the single authoritative `SessionState` per session
//! (UI.md §3, AT13).
//!
//! `UpdateViewSession` semantics follow `dgxtop_core::commands::CommandBody`:
//! * `processes`: `selected_process` is the selection, `selected_entity_id`
//!   a GPU filter (`gpu:<uuid>`), `filter` a name filter (≤ 256 bytes);
//! * `gpu_detail`: `selected_entity_id` is the shown GPU;
//! * `history` / `sources`: `selected_entity_id` is a series selector
//!   (`metric_id|scope|entity_id`); `filter` is the history window token or
//!   the sources text filter; in `history`, `selected_process` is the run
//!   the Process History tab opened (D-76), of any boot, never validated
//!   against the live table.
//!
//! Stale-frame rule: every published projection has a `frame_revision`; a
//! session remembers the last [`STALE_FRAME_WINDOW`] revisions it was sent.
//! An update whose frame is older than that window while carrying a selection
//! that no longer exists is `StaleView`; a frame never sent is invalid. A
//! selection is never moved to another row. A destructive command must target
//! a process present in the current table and, when the session has a
//! selection, exactly that selection.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use dgxtop_core::canonical::{CatalogSnapshot, ProcessRow, ProcessTable};
use dgxtop_core::commands::{MAX_FILTER_BYTES, SortKey, ViewKind};
use dgxtop_core::errors::{ErrorCode, ErrorDetail};
use dgxtop_core::identity::{BootId, EntityId, HostId, MetricKey, ProcessKey, SessionId};
use dgxtop_core::naming::entity;
use dgxtop_core::process_history::HistoryFilter;
use dgxtop_core::text::sanitize_display;
use dgxtop_core::view::SessionState;

use crate::error::ManagerError;
use crate::history::HistoryWindow;
use crate::session::SessionShared;

/// Recent frame revisions remembered per session (4 s at 4 fps).
pub(crate) const STALE_FRAME_WINDOW: usize = 16;

/// The payload of an `UpdateViewSession` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewUpdate {
    pub(crate) view: ViewKind,
    pub(crate) selected_entity_id: Option<EntityId>,
    pub(crate) selected_process: Option<ProcessKey>,
    pub(crate) filter: String,
    pub(crate) sort: SortKey,
    pub(crate) ascending: bool,
    pub(crate) frame_revision: u64,
}

/// What view validation needs to know about the world.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewContext<'a> {
    pub(crate) host_id: &'a HostId,
    pub(crate) boot_id: &'a BootId,
    pub(crate) table: &'a ProcessTable,
    pub(crate) catalog: &'a CatalogSnapshot,
}

#[derive(Debug)]
struct SessionRecord {
    shared: Arc<SessionShared>,
    state: SessionState,
    recent_frames: VecDeque<u64>,
    dirty: bool,
    last_frame_ns: Option<u64>,
    last_viewport: (u16, u16),
}

impl SessionRecord {
    fn frame_age(&self, frame_revision: u64) -> FrameAge {
        let newest = self.recent_frames.back().copied().unwrap_or(0);
        if frame_revision > newest {
            return FrameAge::Unknown;
        }
        let oldest = self.recent_frames.front().copied().unwrap_or(0);
        if frame_revision < oldest {
            FrameAge::Stale
        } else {
            FrameAge::Recent
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameAge {
    Recent,
    Stale,
    Unknown,
}

#[derive(Debug)]
pub(crate) struct ViewSessionService {
    max_sessions: usize,
    sessions: BTreeMap<SessionId, SessionRecord>,
}

impl ViewSessionService {
    pub(crate) fn new(max_sessions: u32) -> Self {
        Self {
            max_sessions: usize::try_from(max_sessions).unwrap_or(1).max(1),
            sessions: BTreeMap::new(),
        }
    }

    pub(crate) fn open(&mut self, shared: Arc<SessionShared>) -> Result<(), ManagerError> {
        if self.sessions.len() >= self.max_sessions {
            return Err(ManagerError::SessionLimit {
                max: u32::try_from(self.max_sessions).unwrap_or(u32::MAX),
            });
        }
        let session_id = shared.id().clone();
        let viewport = shared.viewport();
        self.sessions.insert(
            session_id.clone(),
            SessionRecord {
                state: default_state(session_id.clone()),
                shared,
                recent_frames: VecDeque::new(),
                dirty: true,
                last_frame_ns: None,
                last_viewport: viewport,
            },
        );
        Ok(())
    }

    pub(crate) fn contains(&self, session_id: &SessionId) -> bool {
        self.sessions.contains_key(session_id)
    }

    pub(crate) fn shared(&self, session_id: &SessionId) -> Option<&Arc<SessionShared>> {
        self.sessions.get(session_id).map(|record| &record.shared)
    }

    pub(crate) fn state(&self, session_id: &SessionId) -> Option<&SessionState> {
        self.sessions.get(session_id).map(|record| &record.state)
    }

    pub(crate) fn open_ids(&self) -> BTreeSet<SessionId> {
        self.sessions.keys().cloned().collect()
    }

    /// Validates and applies an update; returns whether the state changed.
    pub(crate) fn update(
        &mut self,
        session_id: &SessionId,
        update: ViewUpdate,
        context: &ViewContext<'_>,
    ) -> Result<bool, ErrorDetail> {
        let record = self
            .sessions
            .get(session_id)
            .ok_or_else(|| ErrorDetail::new(ErrorCode::ValidationFailed, "unknown session"))?;
        let age = record.frame_age(update.frame_revision);
        if age == FrameAge::Unknown {
            return Err(ErrorDetail::new(
                ErrorCode::ValidationFailed,
                "frame_revision was never sent to this session",
            ));
        }
        validate_view(&update, age == FrameAge::Stale, context)?;
        let state = SessionState {
            session_id: session_id.clone(),
            view: update.view,
            selected_entity_id: update.selected_entity_id,
            selected_process: update.selected_process,
            filter: sanitize_display(&update.filter, MAX_FILTER_BYTES),
            sort: update.sort,
            ascending: update.ascending,
        };
        let Some(record) = self.sessions.get_mut(session_id) else {
            return Ok(false);
        };
        let changed = record.state != state;
        if changed {
            record.state = state;
            record.dirty = true;
        }
        Ok(changed)
    }

    /// A destructive command's target must be in the current table and match
    /// the session's selection when there is one. Never retargets.
    pub(crate) fn check_signal_target<'t>(
        &self,
        session_id: &SessionId,
        target: &ProcessKey,
        table: &'t ProcessTable,
    ) -> Result<&'t ProcessRow, ErrorDetail> {
        let row = table
            .rows
            .iter()
            .find(|row| &row.key == target)
            .ok_or_else(|| {
                ErrorDetail::new(
                    ErrorCode::ProcessExited,
                    "target is not in the current process table (exited or stale view); reselect",
                )
            })?;
        let selection = self
            .sessions
            .get(session_id)
            .and_then(|record| record.state.selected_process.as_ref());
        if selection.is_some_and(|selected| selected != target) {
            return Err(ErrorDetail::new(
                ErrorCode::StaleView,
                "target differs from the session's current selection; reselect",
            ));
        }
        Ok(row)
    }

    pub(crate) fn mark_dirty(&mut self, session_id: &SessionId) {
        if let Some(record) = self.sessions.get_mut(session_id) {
            record.dirty = true;
        }
    }

    pub(crate) fn mark_all_dirty(&mut self) {
        for record in self.sessions.values_mut() {
            record.dirty = true;
        }
    }

    /// Sessions whose terminal size changed since the last check (marked dirty).
    pub(crate) fn viewport_changes(&mut self) -> Vec<SessionId> {
        let mut changed = Vec::new();
        for (session_id, record) in &mut self.sessions {
            let viewport = record.shared.viewport();
            if viewport != record.last_viewport {
                record.last_viewport = viewport;
                record.dirty = true;
                changed.push(session_id.clone());
            }
        }
        changed
    }

    /// Dirty sessions whose frame interval has elapsed.
    pub(crate) fn due_for_frame(&self, now_ns: u64, interval_ns: u64) -> Vec<SessionId> {
        self.sessions
            .iter()
            .filter(|(_, record)| {
                record.dirty
                    && record
                        .last_frame_ns
                        .is_none_or(|last| now_ns >= last.saturating_add(interval_ns))
            })
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    /// When the next frame may be built for a dirty session.
    pub(crate) fn next_frame_ns(&self, interval_ns: u64) -> Option<u64> {
        self.sessions
            .values()
            .filter(|record| record.dirty)
            .map(|record| {
                record
                    .last_frame_ns
                    .map_or(0, |last| last.saturating_add(interval_ns))
            })
            .min()
    }

    pub(crate) fn record_frame(
        &mut self,
        session_id: &SessionId,
        frame_revision: u64,
        now_ns: u64,
    ) {
        if let Some(record) = self.sessions.get_mut(session_id) {
            if record.recent_frames.len() == STALE_FRAME_WINDOW {
                record.recent_frames.pop_front();
            }
            record.recent_frames.push_back(frame_revision);
            record.last_frame_ns = Some(now_ns);
            record.dirty = false;
        }
    }

    /// A skipped frame still counts against the rate limit (no retry storm).
    pub(crate) fn frame_skipped(&mut self, session_id: &SessionId, now_ns: u64) {
        if let Some(record) = self.sessions.get_mut(session_id) {
            record.last_frame_ns = Some(now_ns);
        }
    }

    /// Removes sessions whose handles were all dropped.
    pub(crate) fn reap_closed(&mut self) -> Vec<SessionId> {
        let closed: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, record)| record.shared.is_closed())
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in &closed {
            self.sessions.remove(session_id);
        }
        closed
    }

    pub(crate) fn mark_shutdown_requested_all(&self) {
        for record in self.sessions.values() {
            record.shared.mark_shutdown_requested();
        }
    }

    pub(crate) fn answer_shutdown_requests(&self, message: &str) -> usize {
        self.sessions
            .values()
            .map(|record| record.shared.answer_shutdown_requests(message))
            .sum()
    }
}

fn default_state(session_id: SessionId) -> SessionState {
    SessionState {
        session_id,
        view: ViewKind::Overview,
        selected_entity_id: None,
        selected_process: None,
        filter: String::new(),
        sort: SortKey::GpuMemory,
        ascending: false,
    }
}

fn validate_view(
    update: &ViewUpdate,
    stale: bool,
    context: &ViewContext<'_>,
) -> Result<(), ErrorDetail> {
    match update.view {
        ViewKind::Processes => {
            validate_selected_process(update, stale, context)?;
            if let Some(gpu) = &update.selected_entity_id {
                validate_gpu(gpu, stale, context)?;
            }
            Ok(())
        }
        ViewKind::GpuDetail => update
            .selected_entity_id
            .as_ref()
            .map_or(Ok(()), |gpu| validate_gpu(gpu, stale, context)),
        ViewKind::History => {
            validate_series(update, context)?;
            let Some(filter) = HistoryFilter::parse(&update.filter) else {
                return Err(ErrorDetail::new(
                    ErrorCode::ValidationFailed,
                    "history filter must be a window, then optionally a bar size: \
                     auto, 1m, 5m, 10m, 30m, 1h, 2h, 4h, 6h, 8h, 12h, 18h or 24h",
                ));
            };
            if !filter.window.is_empty() && HistoryWindow::parse(filter.window).is_none() {
                return Err(ErrorDetail::new(
                    ErrorCode::ValidationFailed,
                    "history window must be one of 5m, 1h, 6h, 12h, 24h",
                ));
            }
            Ok(())
        }
        ViewKind::Sources => validate_series(update, context),
        ViewKind::Overview | ViewKind::Settings | ViewKind::Actions => Ok(()),
    }
}

fn validate_selected_process(
    update: &ViewUpdate,
    stale: bool,
    context: &ViewContext<'_>,
) -> Result<(), ErrorDetail> {
    let Some(selected) = &update.selected_process else {
        return Ok(());
    };
    if &selected.host_id != context.host_id || &selected.boot_id != context.boot_id {
        return Err(ErrorDetail::new(
            ErrorCode::ValidationFailed,
            "selected process belongs to another host or boot",
        ));
    }
    let present = context.table.rows.iter().any(|row| &row.key == selected);
    if stale && !present {
        return Err(ErrorDetail::new(
            ErrorCode::StaleView,
            "the selected process is gone and the frame is stale; reselect",
        ));
    }
    Ok(())
}

fn validate_gpu(gpu: &EntityId, stale: bool, context: &ViewContext<'_>) -> Result<(), ErrorDetail> {
    if entity::parse_gpu(gpu.as_str()).is_none() {
        return Err(ErrorDetail::new(
            ErrorCode::ValidationFailed,
            "GPU selection must be a gpu:<uuid> entity",
        ));
    }
    let known = context.catalog.entities.contains_key(gpu)
        || context
            .catalog
            .sources
            .values()
            .any(|source| &source.entity_id == gpu);
    if stale && !known {
        return Err(ErrorDetail::new(
            ErrorCode::StaleView,
            "the selected GPU is gone and the frame is stale; reselect",
        ));
    }
    Ok(())
}

fn validate_series(update: &ViewUpdate, context: &ViewContext<'_>) -> Result<(), ErrorDetail> {
    let Some(selector) = &update.selected_entity_id else {
        return Ok(());
    };
    MetricKey::from_series_selector(context.host_id, context.boot_id, selector.as_str())
        .map(|_| ())
        .map_err(|_| {
            ErrorDetail::new(
                ErrorCode::ValidationFailed,
                "series selector must be metric_id|scope|entity_id",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Outstanding;
    use crate::test_support::{entity, entity_id};
    use dgxtop_core::identity::{Pid, PidNamespace};
    use dgxtop_core::quality::SampleKind;
    use dgxtop_core::testing::{boot_id, host_id};

    struct Bench {
        service: ViewSessionService,
        session: SessionId,
        shared: Arc<SessionShared>,
    }

    fn bench() -> Bench {
        let mut service = ViewSessionService::new(2);
        let session = SessionId::new("s1").unwrap();
        let shared = Arc::new(SessionShared::new(
            session.clone(),
            Arc::new(Outstanding::new(8)),
        ));
        service.open(Arc::clone(&shared)).unwrap();
        Bench {
            service,
            session,
            shared,
        }
    }

    fn key(pid: i64) -> ProcessKey {
        ProcessKey {
            host_id: host_id("test-host"),
            boot_id: boot_id("fake-boot"),
            pid_namespace: PidNamespace::new("pid:[1]").unwrap(),
            pid: Pid::new(pid).unwrap(),
            start_time_ticks: 5,
        }
    }

    fn table(pids: &[i64]) -> ProcessTable {
        ProcessTable {
            rows: pids
                .iter()
                .map(|pid| ProcessRow {
                    key: key(*pid),
                    name: format!("p{pid}"),
                    command: String::new(),
                    uid: Some(1000),
                    state: None,
                    cpu_percent: None,
                    rss_bytes: None,
                    missing_reason: None,
                    gpu: Vec::new(),
                    sample_kind: SampleKind::Measured,
                    read_finished_boot_ns: 0,
                })
                .collect(),
            ..ProcessTable::default()
        }
    }

    fn update(
        view: ViewKind,
        process: Option<ProcessKey>,
        entity: Option<&str>,
        frame: u64,
    ) -> ViewUpdate {
        ViewUpdate {
            view,
            selected_entity_id: entity.map(entity_id),
            selected_process: process,
            filter: String::new(),
            sort: SortKey::Pid,
            ascending: true,
            frame_revision: frame,
        }
    }

    fn apply(
        bench: &mut Bench,
        update: ViewUpdate,
        table: &ProcessTable,
        catalog: &CatalogSnapshot,
    ) -> Result<bool, ErrorDetail> {
        let host = host_id("test-host");
        let boot = boot_id("fake-boot");
        let context = ViewContext {
            host_id: &host,
            boot_id: &boot,
            table,
            catalog,
        };
        bench.service.update(&bench.session, update, &context)
    }

    #[test]
    fn selection_survives_recent_frames_and_stale_missing_selection_is_rejected() {
        let mut bench = bench();
        let catalog = CatalogSnapshot::default();
        let rows = table(&[10, 11]);
        for frame in 1..=20u64 {
            bench.service.record_frame(&bench.session, frame, frame);
        }
        assert!(
            apply(
                &mut bench,
                update(ViewKind::Processes, Some(key(10)), None, 20),
                &rows,
                &catalog
            )
            .unwrap()
        );
        assert_eq!(
            bench
                .service
                .state(&bench.session)
                .unwrap()
                .selected_process,
            Some(key(10))
        );
        // Process 10 exits. A recent frame keeps the selection (no retarget)...
        let after_exit = table(&[11]);
        apply(
            &mut bench,
            update(ViewKind::Processes, Some(key(10)), None, 19),
            &after_exit,
            &catalog,
        )
        .unwrap();
        assert_eq!(
            bench
                .service
                .state(&bench.session)
                .unwrap()
                .selected_process,
            Some(key(10)),
            "never retargeted to another row"
        );
        // ... but a stale frame carrying the vanished selection is StaleView.
        let error = apply(
            &mut bench,
            update(ViewKind::Processes, Some(key(10)), None, 2),
            &after_exit,
            &catalog,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::StaleView);
        // A stale frame with a selection that still exists is fine.
        assert!(
            apply(
                &mut bench,
                update(ViewKind::Processes, Some(key(11)), None, 2),
                &after_exit,
                &catalog
            )
            .is_ok()
        );
        // Frames never sent are invalid.
        let error = apply(
            &mut bench,
            update(ViewKind::Processes, None, None, 99),
            &after_exit,
            &catalog,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ValidationFailed);
    }

    #[test]
    fn destructive_targets_must_exist_and_match_the_selection() {
        let mut bench = bench();
        let rows = table(&[10, 11]);
        let catalog = CatalogSnapshot::default();
        assert!(
            bench
                .service
                .check_signal_target(&bench.session, &key(10), &rows)
                .is_ok()
        );
        let gone = bench
            .service
            .check_signal_target(&bench.session, &key(12), &rows)
            .unwrap_err();
        assert_eq!(gone.code, ErrorCode::ProcessExited);
        // Same PID, different start time: a different process.
        let mut reused = key(10);
        reused.start_time_ticks = 99;
        assert!(
            bench
                .service
                .check_signal_target(&bench.session, &reused, &rows)
                .is_err()
        );
        apply(
            &mut bench,
            update(ViewKind::Processes, Some(key(10)), None, 0),
            &rows,
            &catalog,
        )
        .unwrap();
        let other = bench
            .service
            .check_signal_target(&bench.session, &key(11), &rows)
            .unwrap_err();
        assert_eq!(other.code, ErrorCode::StaleView);
    }

    #[test]
    fn view_specific_semantics_are_validated() {
        let mut bench = bench();
        let rows = table(&[]);
        let mut catalog = CatalogSnapshot::default();
        catalog
            .entities
            .insert(entity_id("gpu:GPU-1"), Arc::new(entity("gpu:GPU-1")));
        assert!(
            apply(
                &mut bench,
                update(ViewKind::GpuDetail, None, Some("gpu:GPU-1"), 0),
                &rows,
                &catalog
            )
            .is_ok()
        );
        assert_eq!(
            apply(
                &mut bench,
                update(ViewKind::GpuDetail, None, Some("cpu.core.0"), 0),
                &rows,
                &catalog
            )
            .unwrap_err()
            .code,
            ErrorCode::ValidationFailed
        );
        assert!(
            apply(
                &mut bench,
                update(ViewKind::Processes, None, Some("gpu:GPU-1"), 0),
                &rows,
                &catalog
            )
            .is_ok()
        );
        assert!(
            apply(
                &mut bench,
                update(
                    ViewKind::History,
                    None,
                    Some("gpu.temperature|gpu|gpu:GPU-1"),
                    0
                ),
                &rows,
                &catalog
            )
            .is_ok()
        );
        assert!(
            apply(
                &mut bench,
                update(ViewKind::History, None, Some("not a selector"), 0),
                &rows,
                &catalog
            )
            .is_err()
        );
        let mut window = update(ViewKind::History, None, None, 0);
        window.filter = "2h".into();
        assert!(apply(&mut bench, window.clone(), &rows, &catalog).is_err());
        window.filter = "24h".into();
        assert!(apply(&mut bench, window.clone(), &rows, &catalog).is_ok());
        // The window, then the Process History's bar size (D-77).
        window.filter = "24h 18h".into();
        assert!(apply(&mut bench, window.clone(), &rows, &catalog).is_ok());
        window.filter = "24h 3h".into();
        assert!(apply(&mut bench, window.clone(), &rows, &catalog).is_err());
        window.filter = "2h 1h".into();
        assert!(apply(&mut bench, window.clone(), &rows, &catalog).is_err());
        window.filter = "24h 1h 1h".into();
        assert!(apply(&mut bench, window, &rows, &catalog).is_err());
        let mut sources = update(ViewKind::Sources, None, None, 0);
        sources.filter = "temp\u{1b}[31m".into();
        apply(&mut bench, sources, &rows, &catalog).unwrap();
        assert_eq!(bench.service.state(&bench.session).unwrap().filter, "temp");
        let mut foreign = key(5);
        foreign.host_id = host_id("other");
        assert!(
            apply(
                &mut bench,
                update(ViewKind::Processes, Some(foreign), None, 0),
                &rows,
                &catalog
            )
            .is_err()
        );
    }

    #[test]
    fn frames_are_rate_limited_and_viewport_changes_mark_dirty() {
        let mut bench = bench();
        assert_eq!(
            bench.service.due_for_frame(0, 250),
            vec![bench.session.clone()]
        );
        bench.service.record_frame(&bench.session, 1, 1_000);
        assert!(
            bench.service.due_for_frame(1_100, 250).is_empty(),
            "not dirty"
        );
        bench.service.mark_all_dirty();
        assert!(
            bench.service.due_for_frame(1_100, 250).is_empty(),
            "too early"
        );
        assert_eq!(bench.service.due_for_frame(1_250, 250).len(), 1);
        bench.service.frame_skipped(&bench.session, 1_250);
        assert!(bench.service.due_for_frame(1_300, 250).is_empty());
        bench.shared.store_viewport(120, 40);
        assert_eq!(
            bench.service.viewport_changes(),
            vec![bench.session.clone()]
        );
        assert!(bench.service.viewport_changes().is_empty());
    }

    #[test]
    fn session_limit_and_reaping() {
        let mut bench = bench();
        let second = Arc::new(SessionShared::new(
            SessionId::new("s2").unwrap(),
            Arc::new(Outstanding::new(8)),
        ));
        bench.service.open(second).unwrap();
        let third = Arc::new(SessionShared::new(
            SessionId::new("s3").unwrap(),
            Arc::new(Outstanding::new(8)),
        ));
        assert_eq!(
            bench.service.open(third),
            Err(ManagerError::SessionLimit { max: 2 })
        );
        assert_eq!(bench.service.open_ids().len(), 2);
        assert!(bench.service.reap_closed().is_empty());
        bench.service.mark_shutdown_requested_all();
        assert_eq!(bench.service.answer_shutdown_requests("bye"), 0);
        assert!(bench.service.contains(&bench.session));
        assert!(bench.service.shared(&bench.session).is_some());
    }
}
