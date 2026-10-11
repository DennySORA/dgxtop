//! Processes module projection: the processes that use a GPU (D-36), GPU
//! and text filters, a stable sort whose tie-breaker is the ProcessKey, and
//! a window around the selected ProcessKey. Only the window plus a small
//! overscan is copied (RUNTIME.md §2); selection is never a row index.

use std::cmp::Ordering;

use dgxtop_core::canonical::{ProcessRow, ProcessTable};
use dgxtop_core::commands::SortKey;
use dgxtop_core::identity::EntityId;
use dgxtop_core::process_model::ProcessHistoryModel;
use dgxtop_core::text::{MAX_BASENAME_BYTES, sanitize_display};
use dgxtop_core::view::{
    ProcessGpuView, ProcessRowView, ProcessesView, SessionState, overview_gpus_rows,
};

use super::gpu_directory::{GpuDirectory, is_gpu_entity};
use super::labels::{TextNeedle, filter_text, note_text};

/// Rows copied beyond the estimated visible window.
pub const PROCESS_OVERSCAN_ROWS: usize = 8;
const MAX_LINKS_PER_ROW: usize = 16;
const UNKNOWN_USER: &str = "unknown";
const UNNAMED_PROCESS: &str = "?";
/// Overview rows around the process table in the reference grid, with the
/// charts under their bars (D-38, D-43): header and footer, the CPU/memory
/// row (8 with load and tasks on the sensor line; a narrow panel needs one
/// more, which the overscan absorbs), the disk/network row (9) and the
/// table's border, column header and status line (4).
const OVERVIEW_RESERVED_ROWS: u16 = 2 + 8 + 9 + 4;
/// Border of the overview GPU panel.
const GPU_PANEL_BORDER_ROWS: u16 = 2;

/// Process rows the Overview can show, estimated from the viewport
/// and the overview grid: the table takes the rows the other rows and the
/// GPU panel of `gpu_count` GPUs leave. The overscan absorbs small
/// differences from the viewer's real layout.
pub(crate) fn visible_process_rows(viewport_rows: u16, gpu_count: usize) -> usize {
    let rows = viewport_rows
        .saturating_sub(OVERVIEW_RESERVED_ROWS)
        .saturating_sub(gpu_panel_rows(gpu_count));
    usize::from(rows.max(1))
}

/// Rows the overview GPU panel asks for with `gpu_count` GPUs (none without).
fn gpu_panel_rows(gpu_count: usize) -> u16 {
    if gpu_count == 0 {
        return 0;
    }
    let count = u16::try_from(gpu_count).unwrap_or(u16::MAX);
    GPU_PANEL_BORDER_ROWS.saturating_add(overview_gpus_rows(count))
}

/// Copied slice `[start, end)` of the ordered rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessWindow {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl ProcessWindow {
    /// The builder keeps no scroll state, so the selection is centered and
    /// the window clamped at both ends: the selected row is always inside
    /// `[start, start + visible)` and the last row is reachable.
    pub(crate) fn around(selected: Option<usize>, total: usize, visible: usize) -> Self {
        let visible = visible.max(1);
        let last_start = total.saturating_sub(visible);
        let start = selected.map_or(0, |index| index.saturating_sub(visible / 2).min(last_start));
        let end = start
            .saturating_add(visible)
            .saturating_add(PROCESS_OVERSCAN_ROWS)
            .min(total);
        Self { start, end }
    }
}

/// Whether the bound may have dropped a GPU process or link. The collector
/// keeps GPU processes first, so a kept row without a GPU proves every GPU
/// process was kept.
fn gpu_rows_incomplete(table: &ProcessTable) -> bool {
    table.gpu_links_truncated
        || (table.truncated && table.rows.iter().all(|row| !row.gpu.is_empty()))
}

pub(crate) fn build_processes_view(
    table: &ProcessTable,
    session: &SessionState,
    directory: &GpuDirectory<'_>,
    colors: &ProcessHistoryModel,
    visible_rows: usize,
) -> ProcessesView {
    let gpu_filter = gpu_filter(session, directory);
    let text_filter = TextNeedle::new(&session.filter);
    let mut ordered: Vec<&ProcessRow> = table
        .rows
        .iter()
        .filter(|row| !row.gpu.is_empty())
        .filter(|row| {
            gpu_filter
                .as_ref()
                .is_none_or(|(gpu, _)| uses_gpu(row, gpu))
        })
        .filter(|row| matches_text(&text_filter, row))
        .collect();
    ordered.sort_by(|left, right| compare_rows(left, right, session.sort, session.ascending));
    let selected_index = session
        .selected_process
        .as_ref()
        .and_then(|key| ordered.iter().position(|row| &row.key == key));
    let window = ProcessWindow::around(selected_index, ordered.len(), visible_rows);
    let rows = ordered
        .get(window.start..window.end)
        .unwrap_or_default()
        .iter()
        .map(|row| row_view(row, directory, colors))
        .collect();
    ProcessesView {
        rows,
        window_start: u32::try_from(window.start).unwrap_or(u32::MAX),
        total_rows: u32::try_from(ordered.len()).unwrap_or(u32::MAX),
        truncated: gpu_rows_incomplete(table),
        sort: session.sort,
        ascending: session.ascending,
        filter: filter_text(&session.filter),
        gpu_filter,
        gpu_choices: directory.choices(),
        selected: selected_index.and(session.selected_process.clone()),
        selected_index: selected_index.and_then(|index| u32::try_from(index).ok()),
        selected_missing: session
            .selected_process
            .as_ref()
            .filter(|key| !table.rows.iter().any(|row| &row.key == *key))
            .cloned(),
    }
}

/// The session's selected entity filters by GPU only when it names a GPU.
fn gpu_filter(session: &SessionState, directory: &GpuDirectory<'_>) -> Option<(EntityId, String)> {
    let entity_id = session.selected_entity_id.as_ref()?;
    is_gpu_entity(entity_id.as_str()).then(|| (entity_id.clone(), directory.label_of(entity_id)))
}

fn uses_gpu(row: &ProcessRow, gpu: &EntityId) -> bool {
    row.gpu.iter().any(|link| &link.gpu_entity_id == gpu)
}

/// The filter matches the process name or its PID.
fn matches_text(needle: &TextNeedle, row: &ProcessRow) -> bool {
    needle.matches(&row.name) || needle.matches(&row.key.pid.to_string())
}

fn compare_rows(left: &ProcessRow, right: &ProcessRow, sort: SortKey, ascending: bool) -> Ordering {
    let primary = match sort {
        SortKey::Pid => directed(left.key.pid.cmp(&right.key.pid), ascending),
        SortKey::GpuMemory => {
            present_first(gpu_memory(left), gpu_memory(right), ascending, u64::cmp)
        }
        SortKey::GpuUtil => present_first(
            gpu_utilization(left),
            gpu_utilization(right),
            ascending,
            f64::total_cmp,
        ),
        SortKey::CpuPercent => present_first(
            finite(left.cpu_percent),
            finite(right.cpu_percent),
            ascending,
            f64::total_cmp,
        ),
        SortKey::HostMemory => present_first(left.rss_bytes, right.rss_bytes, ascending, u64::cmp),
    };
    primary.then_with(|| left.key.cmp(&right.key))
}

/// Missing values sort last in both directions: an unreadable process is
/// neither the heaviest nor the lightest.
fn present_first<T>(
    left: Option<T>,
    right: Option<T>,
    ascending: bool,
    compare: impl Fn(&T, &T) -> Ordering,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => directed(compare(&left, &right), ascending),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn directed(ordering: Ordering, ascending: bool) -> Ordering {
    if ascending {
        ordering
    } else {
        ordering.reverse()
    }
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|number| number.is_finite())
}

/// Total GPU memory over all links; `None` when no link reports memory.
fn gpu_memory(row: &ProcessRow) -> Option<u64> {
    row.gpu
        .iter()
        .filter_map(|link| link.memory_bytes)
        .fold(None, |total: Option<u64>, bytes| {
            Some(total.unwrap_or(0).saturating_add(bytes))
        })
}

/// Busiest GPU of the process; `None` when no link reports utilization.
fn gpu_utilization(row: &ProcessRow) -> Option<f64> {
    row.gpu
        .iter()
        .filter_map(|link| finite(link.utilization_percent))
        .max_by(f64::total_cmp)
}

/// A process name as the views show it.
pub(crate) fn display_name(name: &str) -> String {
    let name = sanitize_display(name, MAX_BASENAME_BYTES);
    if name.is_empty() {
        UNNAMED_PROCESS.to_owned()
    } else {
        name
    }
}

fn row_view(
    row: &ProcessRow,
    directory: &GpuDirectory<'_>,
    colors: &ProcessHistoryModel,
) -> ProcessRowView {
    ProcessRowView {
        key: row.key.clone(),
        name: display_name(&row.name),
        user: row
            .uid
            .map_or_else(|| UNKNOWN_USER.to_owned(), |uid| uid.to_string()),
        state: row.state.filter(char::is_ascii_graphic),
        cpu_percent: finite(row.cpu_percent),
        rss_bytes: row.rss_bytes,
        missing_reason: row.missing_reason.as_deref().map(note_text),
        gpus: row
            .gpu
            .iter()
            .take(MAX_LINKS_PER_ROW)
            .map(|link| ProcessGpuView {
                gpu_entity_id: link.gpu_entity_id.clone(),
                gpu_label: directory.label_of(&link.gpu_entity_id),
                usage: link.usage,
                utilization_percent: finite(link.utilization_percent),
                memory_bytes: link.memory_bytes,
            })
            .collect(),
        color_slot: colors.slot_of(&row.key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::index::SampleIndex;
    use crate::presentation::testkit::{
        Fixture, entity_record, gpu_link, process_key, process_row,
    };
    use dgxtop_core::naming::attributes;
    use dgxtop_core::observation::EntityKind;

    const GPU_A: &str = "gpu:GPU-aaaa1111";
    const GPU_B: &str = "gpu:GPU-bbbb2222";

    fn build(fixture: &Fixture, visible: usize) -> ProcessesView {
        let index = SampleIndex::new(&fixture.snapshot);
        let directory = GpuDirectory::new(&fixture.catalog, &index);
        build_processes_view(
            &fixture.snapshot.process_table,
            &fixture.session,
            &directory,
            &fixture.processes,
            visible,
        )
    }

    fn with_gpus() -> Fixture {
        let mut fixture = Fixture::new();
        for (id, index) in [(GPU_A, "0"), (GPU_B, "1")] {
            fixture.add_entity(entity_record(
                id,
                EntityKind::Gpu,
                "NVIDIA H100",
                &[(attributes::NVML_INDEX, index)],
            ));
        }
        fixture
    }

    /// A tracked process's row names its color slot (D-64), so the table
    /// says which process each color of the memory bars is; a process
    /// without known GPU memory has none.
    #[test]
    fn tracked_rows_carry_their_color_slot() {
        let mut fixture = with_gpus();
        let mut train = process_row(1, "train");
        train.gpu = vec![gpu_link(GPU_A, None, Some(8 << 30))];
        let mut idle = process_row(2, "idle");
        idle.gpu = vec![gpu_link(GPU_B, None, None)];
        let key = train.key.clone();
        fixture.set_processes(vec![train, idle]);
        // The color the history's process actor gave `train`.
        fixture.processes.tracked = vec![dgxtop_core::process_model::TrackedProcess {
            slot: 3,
            key,
            pid: 1,
            name: "train".to_owned(),
            current: vec![(EntityId::new(GPU_A).unwrap(), 8 << 30)],
        }];
        let view = build(&fixture, 10);
        let slots: Vec<(&str, Option<u8>)> = view
            .rows
            .iter()
            .map(|row| (row.name.as_str(), row.color_slot))
            .collect();
        assert_eq!(slots, vec![("train", Some(3)), ("idle", None)]);
    }

    /// GPU processes (each has one association with GPU A).
    fn numbered_rows(count: i64) -> Vec<ProcessRow> {
        (1..=count)
            .map(|pid| {
                let mut row = process_row(pid, &format!("worker{pid}"));
                row.cpu_percent = Some(f64::from(u32::try_from(pid).unwrap()));
                row.gpu = vec![gpu_link(GPU_A, None, None)];
                row
            })
            .collect()
    }

    #[test]
    fn only_processes_using_a_gpu_are_listed() {
        let mut fixture = with_gpus();
        let mut rows = numbered_rows(3);
        let mut shell = process_row(10, "bash");
        shell.cpu_percent = Some(80.0);
        rows.push(shell);
        fixture.set_processes(rows);
        fixture.session.sort = SortKey::CpuPercent;
        let view = build(&fixture, 10);
        assert_eq!(view.total_rows, 3);
        assert!(view.rows.iter().all(|row| !row.gpus.is_empty()));
        assert!(!view.rows.iter().any(|row| row.name == "bash"));
        // A selection that stopped using the GPU is hidden like a filtered
        // row: not shown, and not reported as exited.
        fixture.session.selected_process = Some(process_key(10, 1000));
        let hidden = build(&fixture, 10);
        assert_eq!(hidden.selected, None);
        assert_eq!(hidden.selected_missing, None);
    }

    #[test]
    fn window_always_contains_the_selection_and_reaches_both_ends() {
        for total in 0..40_usize {
            for visible in 1..12_usize {
                let none = ProcessWindow::around(None, total, visible);
                assert_eq!(none.start, 0);
                assert!(none.end <= total);
                for selected in 0..total {
                    let window = ProcessWindow::around(Some(selected), total, visible);
                    assert!(window.start <= selected, "{total}/{visible}/{selected}");
                    assert!(
                        selected < window.start + visible,
                        "{total}/{visible}/{selected}"
                    );
                    assert!(window.end <= total);
                    assert!(window.end - window.start <= visible + PROCESS_OVERSCAN_ROWS);
                    assert!(selected < window.end);
                }
            }
        }
        let last = ProcessWindow::around(Some(99), 100, 10);
        assert_eq!(
            last,
            ProcessWindow {
                start: 90,
                end: 100
            }
        );
    }

    #[test]
    fn gpu_filter_and_text_filter_compose() {
        let mut fixture = with_gpus();
        let mut python_a = process_row(10, "python3");
        python_a.gpu = vec![gpu_link(GPU_A, Some(50.0), Some(1 << 30))];
        let mut python_b = process_row(11, "Python-train");
        python_b.gpu = vec![gpu_link(GPU_B, Some(60.0), Some(2 << 30))];
        let mut torch_a = process_row(12, "torchrun");
        torch_a.gpu = vec![gpu_link(GPU_A, Some(70.0), Some(3 << 30))];
        let cpu_only = process_row(13, "python-cli");
        fixture.set_processes(vec![python_a, python_b, torch_a, cpu_only]);
        fixture.session.selected_entity_id = Some(EntityId::new(GPU_A).unwrap());
        fixture.session.filter = "PY".to_owned();
        let view = build(&fixture, 20);
        let pids: Vec<i32> = view.rows.iter().map(|row| row.key.pid.get()).collect();
        assert_eq!(pids, vec![10]);
        assert_eq!(
            view.gpu_filter,
            Some((EntityId::new(GPU_A).unwrap(), "GPU 0".to_owned()))
        );
        assert_eq!(view.gpu_choices.len(), 2);

        fixture.session.filter = "1".to_owned();
        let by_pid = build(&fixture, 20);
        assert_eq!(by_pid.total_rows, 2);
        fixture.session.selected_entity_id = Some(EntityId::new("cpu.core.0").unwrap());
        fixture.session.filter = String::new();
        let unfiltered = build(&fixture, 20);
        // The CPU-only process is not a GPU process and is never listed.
        assert_eq!(unfiltered.total_rows, 3);
        assert_eq!(unfiltered.gpu_filter, None);
    }

    #[test]
    fn selection_follows_the_process_key_when_rows_reorder() {
        let mut fixture = Fixture::new();
        fixture.set_processes(numbered_rows(5));
        fixture.session.sort = SortKey::CpuPercent;
        fixture.session.selected_process = Some(process_key(2, 1000));
        let before = build(&fixture, 10);
        assert_eq!(before.selected_index, Some(3));
        let mut rows = numbered_rows(5);
        rows[1].cpu_percent = Some(99.0);
        fixture.set_processes(rows);
        let after = build(&fixture, 10);
        assert_eq!(after.selected_index, Some(0));
        assert_eq!(after.rows[0].key, process_key(2, 1000));
        let mut reused = numbered_rows(5);
        reused[1].key = process_key(2, 2000);
        fixture.set_processes(reused);
        let gone = build(&fixture, 10);
        assert_eq!(gone.selected, None);
        assert_eq!(gone.selected_index, None);
        // AT09: the exited selection is reported as unavailable, not dropped.
        assert_eq!(gone.selected_missing, Some(process_key(2, 1000)));
    }

    #[test]
    fn a_selection_hidden_by_a_filter_is_not_reported_as_exited() {
        let mut fixture = Fixture::new();
        fixture.set_processes(numbered_rows(5));
        fixture.session.selected_process = Some(process_key(2, 1000));
        fixture.session.filter = "no such process".to_owned();
        let view = build(&fixture, 10);
        assert_eq!(view.selected, None);
        assert_eq!(view.selected_missing, None);
    }

    #[test]
    fn sort_keys_use_link_sums_maxima_and_put_missing_last() {
        let mut fixture = with_gpus();
        let mut multi = process_row(1, "multi");
        multi.gpu = vec![
            gpu_link(GPU_A, Some(10.0), Some(4)),
            gpu_link(GPU_B, Some(90.0), Some(4)),
        ];
        let mut single = process_row(2, "single");
        single.gpu = vec![gpu_link(GPU_A, Some(50.0), Some(6))];
        let mut unknown = process_row(3, "unknown");
        unknown.gpu = vec![gpu_link(GPU_A, None, None)];
        fixture.set_processes(vec![unknown, single, multi]);
        let order = |fixture: &Fixture| -> Vec<i32> {
            build(fixture, 10)
                .rows
                .iter()
                .map(|row| row.key.pid.get())
                .collect()
        };
        fixture.session.sort = SortKey::GpuMemory;
        assert_eq!(order(&fixture), vec![1, 2, 3]);
        fixture.session.ascending = true;
        assert_eq!(order(&fixture), vec![2, 1, 3]);
        fixture.session.sort = SortKey::GpuUtil;
        fixture.session.ascending = false;
        assert_eq!(order(&fixture), vec![1, 2, 3]);
        fixture.session.sort = SortKey::HostMemory;
        assert_eq!(order(&fixture), vec![1, 2, 3]);
        fixture.session.sort = SortKey::Pid;
        assert_eq!(order(&fixture), vec![3, 2, 1]);
    }

    /// The collector keeps GPU processes first, so a cut that also kept a
    /// process without a GPU dropped none of the GPU processes this table
    /// lists: the listed rows are complete.
    #[test]
    fn a_cut_of_processes_without_a_gpu_is_not_reported_as_truncated() {
        let mut fixture = with_gpus();
        let mut gpu = process_row(1, "python");
        gpu.gpu = vec![gpu_link(GPU_A, Some(90.0), Some(64 << 30))];
        let cpu_only = process_row(2, "bash");
        fixture.set_processes(vec![gpu, cpu_only]);
        let truncate = |fixture: &mut Fixture, links: bool| {
            fixture.snapshot.process_table = std::sync::Arc::new(ProcessTable {
                truncated: true,
                gpu_links_truncated: links,
                ..(*fixture.snapshot.process_table).clone()
            });
        };
        truncate(&mut fixture, false);
        let view = build(&fixture, 10);
        assert_eq!(view.total_rows, 1);
        assert!(!view.truncated);
        // Dropped GPU links still leave the listed rows incomplete.
        truncate(&mut fixture, true);
        assert!(build(&fixture, 10).truncated);
    }

    #[test]
    fn rows_carry_gpu_labels_numeric_users_and_sanitized_names() {
        let mut fixture = with_gpus();
        let mut row = process_row(7, "evil\u{1b}[2Jname");
        row.uid = None;
        row.state = Some('\u{7}');
        row.cpu_percent = Some(f64::NAN);
        row.gpu = vec![
            gpu_link(GPU_B, None, Some(1024)),
            gpu_link("gpu:GPU-ffff0000", Some(1.0), None),
        ];
        let mut plain = process_row(8, "");
        plain.state = Some('R');
        plain.gpu = vec![gpu_link(GPU_A, None, None)];
        fixture.set_processes(vec![row, plain]);
        fixture.snapshot.process_table = std::sync::Arc::new(ProcessTable {
            truncated: true,
            ..(*fixture.snapshot.process_table).clone()
        });
        fixture.session.sort = SortKey::Pid;
        fixture.session.ascending = true;
        let view = build(&fixture, 10);
        assert!(view.truncated);
        let first = &view.rows[0];
        assert_eq!(first.name, "evilname");
        assert_eq!(first.user, "unknown");
        assert_eq!(first.state, None);
        assert_eq!(first.cpu_percent, None);
        let labels: Vec<&str> = first
            .gpus
            .iter()
            .map(|gpu| gpu.gpu_label.as_str())
            .collect();
        assert_eq!(labels, vec!["GPU 1", "GPU ffff0000"]);
        assert_eq!(view.rows[1].name, "?");
        assert_eq!(view.rows[1].user, "1000");
        assert_eq!(view.rows[1].state, Some('R'));
    }
}
