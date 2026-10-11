//! The tracked GPU processes in the projections (D-64): what each holds of
//! a memory bar now and, on the history view, its part of the memory
//! charts and what it held over the window. The history comes from the
//! process memory record, which the HistoryService does not query, so its
//! parts are added here to the view the service built.

use std::collections::BTreeSet;
use std::sync::Arc;

use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::CanonicalUnit;
use dgxtop_core::view::{
    HistoryChartKind, HistoryPart, HistoryProcessView, HistoryView, ProcessShare,
};

use super::ProcessMemoryBook;
use super::gpu_directory::GpuDirectory;
use super::labels::note_text;
use super::processes::display_name;
use crate::process_memory::TrackedProcess;

/// What each tracked process holds now on the GPUs `include` accepts, in
/// slot order; a process holding nothing there is left out.
pub(crate) fn process_shares(
    book: &ProcessMemoryBook,
    include: impl Fn(&EntityId) -> bool,
) -> Vec<ProcessShare> {
    book.tracked()
        .into_iter()
        .filter_map(|process| {
            let bytes = process.bytes_on(&include);
            (bytes > 0).then(|| ProcessShare {
                slot: process.slot,
                pid: process.pid,
                name: display_name(process.name),
                bytes,
            })
        })
        .collect()
}

/// The GPUs of `directory` that allocate from system RAM.
pub(crate) fn unified_gpus(directory: &GpuDirectory<'_>) -> BTreeSet<EntityId> {
    directory
        .gpus()
        .iter()
        .filter(|gpu| gpu.has_unified_memory())
        .map(|gpu| gpu.entity_id.clone())
        .collect()
}

/// What the history view's process parts are made from.
pub(crate) struct ProcessHistoryInputs<'a> {
    pub(crate) book: &'a ProcessMemoryBook,
    pub(crate) directory: &'a GpuDirectory<'a>,
    /// Wall clock minus boot clock: the record is on the boot clock, the
    /// view on the wall clock (D-51).
    pub(crate) boot_to_utc_ns: i128,
    /// Cells of a part's grid: the points of a history chart.
    pub(crate) cells: usize,
}

/// `history` with the tracked processes' parts of its memory charts (each
/// GPU's memory and, when a GPU shares the RAM, the memory used) and what
/// each held on each GPU over the window. Shared unchanged when no process
/// is tracked.
pub(crate) fn with_process_parts(
    history: Arc<HistoryView>,
    inputs: &ProcessHistoryInputs<'_>,
) -> Arc<HistoryView> {
    let tracked = inputs.book.tracked();
    if tracked.is_empty() || history.error.is_some() {
        return history;
    }
    let window = boot_window(&history, inputs.boot_to_utc_ns);
    let unified = unified_gpus(inputs.directory);
    let processes = window_summaries(&tracked, inputs, window);
    if processes.is_empty() {
        return history;
    }
    let mut view = (*history).clone();
    for chart in &mut view.charts {
        let gpu = chart.series.entity_id.clone();
        chart.parts = match chart.kind {
            HistoryChartKind::GpuMemory => parts(&tracked, inputs, |id| *id == gpu, window),
            HistoryChartKind::MemoryUsed if !unified.is_empty() => {
                parts(&tracked, inputs, |id| unified.contains(id), window)
            }
            _ => continue,
        };
    }
    view.processes = processes;
    Arc::new(view)
}

/// The view's window on the boot clock.
fn boot_window(history: &HistoryView, boot_to_utc_ns: i128) -> (u64, u64) {
    let boot = |utc: u64| {
        u64::try_from(i128::from(utc).saturating_sub(boot_to_utc_ns).max(0)).unwrap_or(u64::MAX)
    };
    (boot(history.start_utc_ns), boot(history.end_utc_ns))
}

/// The parts of the processes that held memory on the GPUs `include`
/// accepts in `window`.
fn parts(
    tracked: &[TrackedProcess<'_>],
    inputs: &ProcessHistoryInputs<'_>,
    include: impl Fn(&EntityId) -> bool,
    window: (u64, u64),
) -> Vec<HistoryPart> {
    tracked
        .iter()
        .filter_map(|process| {
            let means = inputs
                .book
                .part_means(process.key, &include, window, inputs.cells);
            means
                .iter()
                .flatten()
                .any(|mean| *mean > 0.0)
                .then_some(HistoryPart {
                    slot: process.slot,
                    means,
                })
        })
        .collect()
}

/// What each tracked process held on each GPU over `window`, the largest
/// mean first; a GPU it held nothing on in the window is left out.
fn window_summaries(
    tracked: &[TrackedProcess<'_>],
    inputs: &ProcessHistoryInputs<'_>,
    window: (u64, u64),
) -> Vec<HistoryProcessView> {
    let mut summaries: Vec<HistoryProcessView> = tracked
        .iter()
        .flat_map(|process| {
            inputs
                .book
                .gpus_of(process.key)
                .into_iter()
                .filter_map(move |gpu| {
                    let held = inputs.book.held_on(process.key, gpu, window);
                    (held.covered_ns > 0).then(|| HistoryProcessView {
                        slot: process.slot,
                        pid: process.pid,
                        name: display_name(process.name),
                        command: note_text(process.command),
                        gpu_label: inputs.directory.label_of(gpu),
                        unit: CanonicalUnit::Bytes,
                        mean: held.mean(),
                        peak: held.peak.map(bytes_value),
                        listed_ns: held.covered_ns,
                        running: process.is_listed(),
                    })
                })
        })
        .collect();
    summaries.sort_by(|left, right| {
        right
            .mean
            .unwrap_or(0.0)
            .total_cmp(&left.mean.unwrap_or(0.0))
            .then(left.slot.cmp(&right.slot))
            .then_with(|| left.gpu_label.cmp(&right.gpu_label))
    });
    summaries
}

/// A byte count as a value of the view (exact below 2^53 bytes).
fn bytes_value(bytes: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64;
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::index::SampleIndex;
    use crate::presentation::testkit::{Fixture, empty_history, entity_record, history_chart};
    use crate::process_memory::preferred_color;
    use crate::test_support::{gpu_process, process_table};
    use dgxtop_core::naming::attributes;
    use dgxtop_core::observation::EntityKind;
    use dgxtop_core::testing::metric_key;
    use dgxtop_core::view::HistoryChartView;

    const UNIFIED: &str = "gpu:GPU-aaaa1111";
    const DISCRETE: &str = "gpu:GPU-cccc3333";
    const SECOND: u64 = 1_000_000_000;
    const GIB: u64 = 1 << 30;

    /// A GB10 beside a discrete GPU; tables every 10 s from 10 s to 30 s:
    /// `train` holds 6 GiB of the unified GPU, `serve` 2 GiB of it and
    /// 1 GiB of the discrete one.
    fn fixture() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.add_entity(entity_record(
            UNIFIED,
            EntityKind::Gpu,
            "NVIDIA GB10",
            &[
                (attributes::NVML_INDEX, "0"),
                (attributes::MEMORY_MODEL, "unified"),
            ],
        ));
        fixture.add_entity(entity_record(
            DISCRETE,
            EntityKind::Gpu,
            "NVIDIA L4",
            &[(attributes::NVML_INDEX, "1")],
        ));
        fixture.process_memory = ProcessMemoryBook::new(10_000);
        for at in [10, 20, 30] {
            fixture.process_memory.record(&process_table(
                at,
                at * SECOND,
                vec![
                    gpu_process(1, "train", &[(UNIFIED, Some(6 * GIB))]),
                    gpu_process(
                        2,
                        "serve",
                        &[(UNIFIED, Some(2 * GIB)), (DISCRETE, Some(GIB))],
                    ),
                ],
            ));
        }
        fixture
    }

    fn memory_chart(
        kind: HistoryChartKind,
        entity: &str,
        metric: &str,
        scope: &str,
    ) -> HistoryChartView {
        let mut chart = history_chart(Vec::new());
        chart.kind = kind;
        chart.series = metric_key(entity, metric, scope);
        chart
    }

    /// The window `[10 s, 30 s)` on a wall clock `offset` ahead of the boot
    /// clock, with both GPUs' memory, the memory used and the CPU charted.
    fn history(offset_s: u64) -> Arc<HistoryView> {
        let mut view = empty_history();
        view.start_utc_ns = (offset_s + 10) * SECOND;
        view.end_utc_ns = (offset_s + 30) * SECOND;
        view.charts = vec![
            memory_chart(
                HistoryChartKind::GpuMemory,
                UNIFIED,
                "gpu.memory.process_allocated",
                "gpu",
            ),
            memory_chart(
                HistoryChartKind::GpuMemory,
                DISCRETE,
                "gpu.memory.framebuffer.used",
                "gpu",
            ),
            memory_chart(
                HistoryChartKind::MemoryUsed,
                "memory",
                "memory.used",
                "system",
            ),
            history_chart(Vec::new()),
        ];
        Arc::new(view)
    }

    fn attach(fixture: &Fixture, history: Arc<HistoryView>, offset_s: u64) -> Arc<HistoryView> {
        let index = SampleIndex::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        with_process_parts(
            history,
            &ProcessHistoryInputs {
                book: &fixture.process_memory,
                directory: &directory,
                boot_to_utc_ns: i128::from(offset_s * SECOND),
                cells: 2,
            },
        )
    }

    fn gib(count: u64) -> Option<f64> {
        Some(bytes_value(count * GIB))
    }

    /// Each process's share of a bar is what it holds on the bar's GPUs,
    /// in the order of its color.
    #[test]
    fn shares_are_what_each_process_holds_on_the_gpus_of_a_bar() {
        let fixture = fixture();
        let book = &fixture.process_memory;
        let unified = process_shares(book, |gpu| gpu.as_str() == UNIFIED);
        let shown: Vec<(u8, &str, u64)> = unified
            .iter()
            .map(|share| (share.slot, share.name.as_str(), share.bytes))
            .collect();
        let (train, serve) = (preferred_color("train"), preferred_color("serve"));
        let mut expected = vec![(train, "train", 6 * GIB), (serve, "serve", 2 * GIB)];
        expected.sort_unstable();
        assert_eq!(shown, expected, "in color order");
        let discrete = process_shares(book, |gpu| gpu.as_str() == DISCRETE);
        assert_eq!(discrete.len(), 1);
        assert_eq!((discrete[0].slot, discrete[0].bytes), (serve, GIB));
    }

    /// The memory charts get each process's part in its color; the memory
    /// used takes the unified GPU's parts only, other charts none. The
    /// table lists each process on each GPU, the largest mean first.
    #[test]
    fn memory_charts_get_each_process_part_and_the_window_its_means() {
        let fixture = fixture();
        // A wall clock 1000 s ahead of the boot clock gives the same parts.
        for offset in [0, 1_000] {
            let view = attach(&fixture, history(offset), offset);
            let parts = |index: usize| -> Vec<(u8, Vec<Option<f64>>)> {
                view.charts[index]
                    .parts
                    .iter()
                    .map(|part| (part.slot, part.means.clone()))
                    .collect()
            };
            let (train, serve) = (preferred_color("train"), preferred_color("serve"));
            let mut unified = vec![(train, vec![gib(6); 2]), (serve, vec![gib(2); 2])];
            unified.sort_by_key(|(slot, _)| *slot);
            assert_eq!(parts(0), unified);
            assert_eq!(parts(1), vec![(serve, vec![gib(1); 2])]);
            assert_eq!(parts(2), parts(0), "RAM the unified GPU holds");
            assert!(view.charts[3].parts.is_empty());
            let listed: Vec<(&str, &str, Option<f64>, u64)> = view
                .processes
                .iter()
                .map(|process| {
                    (
                        process.name.as_str(),
                        process.gpu_label.as_str(),
                        process.mean,
                        process.listed_ns,
                    )
                })
                .collect();
            assert_eq!(
                listed,
                vec![
                    ("train", "GPU 0", gib(6), 20 * SECOND),
                    ("serve", "GPU 0", gib(2), 20 * SECOND),
                    ("serve", "GPU 1", gib(1), 20 * SECOND),
                ]
            );
            assert!(view.processes.iter().all(|process| process.running));
            assert_eq!(view.processes[0].peak, gib(6));
        }
    }

    /// Without a tracked process, or before the window, the service's view
    /// is shared as it is.
    #[test]
    fn a_view_without_processes_in_its_window_is_shared_unchanged() {
        let fixture = Fixture::new();
        let view = history(0);
        assert!(Arc::ptr_eq(&attach(&fixture, Arc::clone(&view), 0), &view));
        let tracked = self::fixture();
        let mut early = empty_history();
        early.start_utc_ns = 0;
        early.end_utc_ns = 5 * SECOND;
        let early = Arc::new(early);
        assert!(Arc::ptr_eq(
            &attach(&tracked, Arc::clone(&early), 0),
            &early
        ));
    }
}
