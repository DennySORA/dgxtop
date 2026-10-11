//! The tracked GPU processes in the projections (D-64): what each holds of
//! a memory bar now and, on the history view, its part of the memory
//! charts over the window. The history's process actor keeps the process
//! memory record (D-79), which the HistoryService does not query, so its
//! parts are asked for the window of the view the service built and added
//! to it here.

use std::collections::BTreeSet;
use std::sync::Arc;

use dgxtop_core::identity::EntityId;
use dgxtop_core::ports::ProcessMemoryReader;
use dgxtop_core::process_model::TrackedProcess;
use dgxtop_core::view::{HistoryChartKind, HistoryPart, HistoryView, ProcessShare};

use super::gpu_directory::GpuDirectory;
use super::processes::display_name;

/// What each tracked process holds now on the GPUs `include` accepts, in
/// slot order; a process holding nothing there is left out.
pub(crate) fn process_shares(
    tracked: &[TrackedProcess],
    include: impl Fn(&EntityId) -> bool,
) -> Vec<ProcessShare> {
    tracked
        .iter()
        .filter_map(|process| {
            let bytes = process.bytes_on(&include);
            (bytes > 0).then(|| ProcessShare {
                slot: process.slot,
                pid: process.pid,
                name: display_name(&process.name),
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
    /// The processes holding a color.
    pub(crate) tracked: &'a [TrackedProcess],
    /// The process memory record of the history's process actor.
    pub(crate) memory: &'a dyn ProcessMemoryReader,
    pub(crate) directory: &'a GpuDirectory<'a>,
    /// Wall clock minus boot clock: the record is on the boot clock, the
    /// view on the wall clock (D-51).
    pub(crate) boot_to_utc_ns: i128,
    /// Cells of a part's grid: the points of a history chart.
    pub(crate) cells: usize,
}

/// `history` with the tracked processes' parts of its memory charts (each
/// GPU's memory and, when a GPU shares the RAM, the memory used). Shared
/// unchanged when no process is tracked; the runs themselves are the
/// Process History's (D-76).
pub(crate) fn with_process_parts(
    history: Arc<HistoryView>,
    inputs: &ProcessHistoryInputs<'_>,
) -> Arc<HistoryView> {
    if inputs.tracked.is_empty() || history.error.is_some() {
        return history;
    }
    let window = boot_window(&history, inputs.boot_to_utc_ns);
    let unified = unified_gpus(inputs.directory);
    let mut view = (*history).clone();
    for chart in &mut view.charts {
        let gpus = match chart.kind {
            HistoryChartKind::GpuMemory => BTreeSet::from([chart.series.entity_id.clone()]),
            HistoryChartKind::MemoryUsed if !unified.is_empty() => unified.clone(),
            _ => continue,
        };
        chart.parts = inputs
            .memory
            .parts(&gpus, window, inputs.cells)
            .into_iter()
            .map(|part| HistoryPart {
                slot: part.slot,
                means: part.means,
            })
            .collect();
    }
    if view.charts.iter().all(|chart| chart.parts.is_empty()) {
        return history;
    }
    Arc::new(view)
}

/// The view's window on the boot clock.
fn boot_window(history: &HistoryView, boot_to_utc_ns: i128) -> (u64, u64) {
    let boot = |utc: u64| {
        u64::try_from(i128::from(utc).saturating_sub(boot_to_utc_ns).max(0)).unwrap_or(u64::MAX)
    };
    (boot(history.start_utc_ns), boot(history.end_utc_ns))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::presentation::index::SampleIndex;
    use crate::presentation::testkit::{Fixture, empty_history, entity_record, history_chart};
    use dgxtop_core::naming::attributes;
    use dgxtop_core::observation::EntityKind;
    use dgxtop_core::process_model::MemoryPart;
    use dgxtop_core::testing::{StaticProcessHistory, metric_key, test_process_key};
    use dgxtop_core::view::HistoryChartView;

    const UNIFIED: &str = "gpu:GPU-aaaa1111";
    const DISCRETE: &str = "gpu:GPU-cccc3333";
    const SECOND: u64 = 1_000_000_000;
    const GIB: u64 = 1 << 30;
    const TRAIN: u8 = 1;
    const SERVE: u8 = 3;

    fn gpu(id: &str) -> EntityId {
        EntityId::new(id).unwrap()
    }

    /// `train` holds 6 GiB of the unified GPU, `serve` 2 GiB of it and
    /// 1 GiB of the discrete one, in the order of their colors.
    fn tracked() -> Vec<TrackedProcess> {
        vec![
            TrackedProcess {
                slot: TRAIN,
                key: test_process_key(1),
                pid: 1,
                name: "train".to_owned(),
                current: vec![(gpu(UNIFIED), 6 * GIB)],
            },
            TrackedProcess {
                slot: SERVE,
                key: test_process_key(2),
                pid: 2,
                name: "serve".to_owned(),
                current: vec![(gpu(UNIFIED), 2 * GIB), (gpu(DISCRETE), GIB)],
            },
        ]
    }

    /// A GB10 beside a discrete GPU.
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

    fn attach(
        fixture: &Fixture,
        (tracked, memory): (&[TrackedProcess], &StaticProcessHistory),
        history: Arc<HistoryView>,
        offset_s: u64,
    ) -> Arc<HistoryView> {
        let index = SampleIndex::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        with_process_parts(
            history,
            &ProcessHistoryInputs {
                tracked,
                memory,
                directory: &directory,
                boot_to_utc_ns: i128::from(offset_s * SECOND),
                cells: 2,
            },
        )
    }

    #[allow(clippy::cast_precision_loss)]
    fn gib(count: u64) -> Option<f64> {
        Some((count * GIB) as f64)
    }

    /// Each process's share of a bar is what it holds on the bar's GPUs,
    /// in the order of its color.
    #[test]
    fn shares_are_what_each_process_holds_on_the_gpus_of_a_bar() {
        let tracked = tracked();
        let unified = process_shares(&tracked, |gpu| gpu.as_str() == UNIFIED);
        let shown: Vec<(u8, &str, u64)> = unified
            .iter()
            .map(|share| (share.slot, share.name.as_str(), share.bytes))
            .collect();
        assert_eq!(
            shown,
            vec![(TRAIN, "train", 6 * GIB), (SERVE, "serve", 2 * GIB)],
            "in color order"
        );
        let discrete = process_shares(&tracked, |gpu| gpu.as_str() == DISCRETE);
        assert_eq!(discrete.len(), 1);
        assert_eq!((discrete[0].slot, discrete[0].bytes), (SERVE, GIB));
    }

    /// Each GPU's memory chart asks for the parts on its GPU, the memory
    /// used for those on the unified GPU, other charts for none, all over
    /// the view's window on the boot clock.
    #[test]
    fn memory_charts_get_each_process_part() {
        let fixture = fixture();
        let tracked = tracked();
        // A wall clock 1000 s ahead of the boot clock asks the same window.
        for offset in [0, 1_000] {
            let memory = StaticProcessHistory::new(Default::default());
            memory.set_parts(vec![MemoryPart {
                slot: TRAIN,
                means: vec![gib(6); 2],
            }]);
            let view = attach(&fixture, (&tracked, &memory), history(offset), offset);
            let asked: Vec<(BTreeSet<EntityId>, (u64, u64), usize)> = memory.asked();
            assert_eq!(
                asked,
                vec![
                    (
                        BTreeSet::from([gpu(UNIFIED)]),
                        (10 * SECOND, 30 * SECOND),
                        2
                    ),
                    (
                        BTreeSet::from([gpu(DISCRETE)]),
                        (10 * SECOND, 30 * SECOND),
                        2
                    ),
                    (
                        BTreeSet::from([gpu(UNIFIED)]),
                        (10 * SECOND, 30 * SECOND),
                        2
                    ),
                ],
                "the GPUs of each memory chart"
            );
            let parts = |index: usize| -> Vec<(u8, Vec<Option<f64>>)> {
                view.charts[index]
                    .parts
                    .iter()
                    .map(|part| (part.slot, part.means.clone()))
                    .collect()
            };
            assert_eq!(parts(0), vec![(TRAIN, vec![gib(6); 2])]);
            assert_eq!(parts(2), parts(0), "RAM the unified GPU holds");
            assert!(view.charts[3].parts.is_empty());
        }
    }

    /// Without a tracked process, or without a part in its window, the
    /// service's view is shared as it is.
    #[test]
    fn a_view_without_processes_in_its_window_is_shared_unchanged() {
        let fixture = fixture();
        let memory = StaticProcessHistory::new(Default::default());
        let view = history(0);
        assert!(Arc::ptr_eq(
            &attach(&fixture, (&[], &memory), Arc::clone(&view), 0),
            &view
        ));
        assert!(memory.asked().is_empty(), "nothing asked without a process");
        let tracked = tracked();
        assert!(Arc::ptr_eq(
            &attach(&fixture, (&tracked, &memory), Arc::clone(&view), 0),
            &view
        ));
    }
}
