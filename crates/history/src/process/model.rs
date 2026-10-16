//! The read model of the process actor (D-79): built when what it shows
//! changed, never per frame. The rows take each run's whole-life figures,
//! which the runs keep as their buckets close; a detail lays out its run's
//! bars once per change of the book, for each run a session asked for.

use std::collections::VecDeque;

use dgxtop_compression::bars_of;
use dgxtop_core::process_history::MAX_LISTED_RUNS;
use dgxtop_core::process_model::{
    ProcessHistoryModel, RunBars, RunDetail, RunRequest, RunRow, TrackedProcess, WriteProgress,
};
use dgxtop_core::view::MAX_RUN_CHART_POINTS;

use super::memory::ProcessMemoryBook;
use super::runs::{ProcessHistoryBook, Run};

/// Runs whose details are kept: one per session showing a run, and the
/// sizes or widths it switched from.
pub(super) const MAX_SHOWN_RUNS: usize = 4;

/// What the model was built from, to tell whether it must be built again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Built {
    book: u64,
    memory: u64,
    shown: u64,
    writes: WriteProgress,
    kept_on_disk: bool,
}

/// Builds the read model (module docs).
#[derive(Debug, Default)]
pub(super) struct ModelBuilder {
    revision: u64,
    built: Option<Built>,
    /// The runs asked for, the latest last.
    requests: VecDeque<RunRequest>,
    /// Moves with every new request.
    shown: u64,
    /// Each request's detail and the book revision it was laid out at.
    details: Vec<(u64, RunDetail)>,
    /// The rows, the runs beyond them, and the book revision they show.
    rows: Option<(u64, Vec<RunRow>, usize)>,
    /// The tracked processes and the memory revision they show.
    tracked: Option<(u64, Vec<TrackedProcess>)>,
}

/// What a model is built from.
pub(super) struct ModelInputs<'a> {
    pub(super) book: &'a ProcessHistoryBook,
    pub(super) memory: &'a ProcessMemoryBook,
    pub(super) kept_on_disk: bool,
    pub(super) writes: WriteProgress,
}

impl ModelBuilder {
    /// A session shows `request`: it becomes the latest asked for, and
    /// the oldest beyond [`MAX_SHOWN_RUNS`] goes.
    pub(super) fn show(&mut self, request: RunRequest) {
        if self.requests.back() == Some(&request) {
            return;
        }
        self.requests.retain(|asked| *asked != request);
        self.requests.push_back(request);
        while self.requests.len() > MAX_SHOWN_RUNS {
            self.requests.pop_front();
        }
        self.shown += 1;
    }

    /// The model, when anything it shows changed since the last one.
    pub(super) fn build(&mut self, inputs: &ModelInputs<'_>) -> Option<ProcessHistoryModel> {
        let built = Built {
            book: inputs.book.revision(),
            memory: inputs.memory.revision(),
            shown: self.shown,
            writes: inputs.writes,
            kept_on_disk: inputs.kept_on_disk,
        };
        if self.built == Some(built) {
            return None;
        }
        self.built = Some(built);
        self.revision += 1;
        let (runs, more_runs) = self.rows(inputs.book);
        self.refresh_details(inputs.book);
        Some(ProcessHistoryModel {
            revision: self.revision,
            runs,
            more_runs,
            kept_on_disk: inputs.kept_on_disk,
            details: self
                .details
                .iter()
                .map(|(_, detail)| detail.clone())
                .collect(),
            tracked: self.tracked(inputs.memory),
            writes: inputs.writes,
        })
    }

    /// The rows, laid out again only when the book moved.
    fn rows(&mut self, book: &ProcessHistoryBook) -> (Vec<RunRow>, usize) {
        let revision = book.revision();
        let (_, rows, more) = match self.rows.take() {
            Some(rows) if rows.0 == revision => self.rows.insert(rows),
            _ => {
                let (rows, more) = rows_of(book);
                self.rows.insert((revision, rows, more))
            }
        };
        (rows.clone(), *more)
    }

    /// Lays out again the details of the runs asked for whose book moved,
    /// and keeps the ones still asked for.
    fn refresh_details(&mut self, book: &ProcessHistoryBook) {
        let revision = book.revision();
        let mut details = Vec::with_capacity(self.requests.len());
        for request in &self.requests {
            let cached = self
                .details
                .iter()
                .position(|(at, detail)| *at == revision && detail.request == *request);
            if let Some(position) = cached {
                details.push(self.details.swap_remove(position));
                continue;
            }
            if let Some(run) = book.run(&request.key) {
                details.push((revision, detail_of(run, request, book.is_loading(run))));
            }
        }
        self.details = details;
    }

    /// The processes holding a color, taken again only when the memory
    /// record moved.
    fn tracked(&mut self, memory: &ProcessMemoryBook) -> Vec<TrackedProcess> {
        let revision = memory.revision();
        let (_, tracked) = match self.tracked.take() {
            Some(tracked) if tracked.0 == revision => self.tracked.insert(tracked),
            _ => self.tracked.insert((revision, tracked_of(memory))),
        };
        tracked.clone()
    }
}

fn tracked_of(memory: &ProcessMemoryBook) -> Vec<TrackedProcess> {
    memory
        .tracked()
        .into_iter()
        .map(|process| TrackedProcess {
            slot: process.slot,
            key: process.key.clone(),
            pid: process.pid,
            name: process.name.to_owned(),
            current: process.current.to_vec(),
        })
        .collect()
}

/// The runs, the running ones first (D-115), each part the latest start
/// first, every one listed (at most [`MAX_LISTED_RUNS`]: each session's
/// projection takes the ones its filter keeps, D-113), and how many more
/// there are.
fn rows_of(book: &ProcessHistoryBook) -> (Vec<RunRow>, usize) {
    let mut runs: Vec<&Run> = book.runs().collect();
    runs.sort_by(|left, right| {
        right
            .is_live()
            .cmp(&left.is_live())
            .then_with(|| right.info().started_utc_ns.cmp(&left.info().started_utc_ns))
            .then_with(|| left.info().key.cmp(&right.info().key))
    });
    let more = runs.len().saturating_sub(MAX_LISTED_RUNS);
    let rows = runs
        .iter()
        .take(MAX_LISTED_RUNS)
        .map(|run| row_of(run))
        .collect();
    (rows, more)
}

fn row_of(run: &Run) -> RunRow {
    let info = run.info();
    RunRow {
        key: info.key.clone(),
        name: info.name.clone(),
        started_utc_ns: info.started_utc_ns,
        runtime_ns: info.runtime_ns(),
        live: run.is_live(),
        end: info.end,
        gpus: info.gpus.clone(),
        whole: *run.whole(),
    }
}

/// `run` as `request` shows it: its bars of the size asked for (the
/// compression's [`bars_of`], D-79) over the plot's columns; `loading`
/// while its buckets are being read.
fn detail_of(run: &Run, request: &RunRequest, loading: bool) -> RunDetail {
    let columns = request.columns.clamp(1, MAX_RUN_CHART_POINTS);
    let series = run.series();
    RunDetail {
        request: request.clone(),
        info: run.info().clone(),
        live: run.is_live(),
        bars: bars_of(series, request.bar, columns).map(|bars| RunBars {
            bar: bars.bar,
            step_ns: bars.step_ns,
            window: bars.window,
            bars: bars.bars,
        }),
        whole: *run.whole(),
        loading,
    }
}

#[cfg(test)]
mod tests;
