use std::collections::BTreeMap;

use dgxtop_core::identity::{EntityId, ProcessKey, SessionId};
use dgxtop_core::process_history::RunDetails;
use dgxtop_core::process_model::TrackedProcess;
use dgxtop_core::testing::test_process_key;

use super::*;

const SECOND: i64 = 1_000_000_000;
const MINUTE: i64 = 60 * SECOND;
const GIB: f64 = 1_073_741_824.0;
/// UTC of the tests' first minute.
const START: i64 = 1_789_999_800 * SECOND;

fn scales() -> RunScales {
    RunScales {
        cpu: Some(2_000.0),
        ram: Some(128.0 * GIB),
        gpu_memory: BTreeMap::from([("GPU 0".to_owned(), Some(120.0 * GIB))]),
    }
}

fn key(pid: i64) -> ProcessKey {
    test_process_key(pid)
}

fn info(pid: i64) -> RunInfo {
    RunInfo {
        key: key(pid),
        name: "acme_llm.train".to_owned(),
        command: "python -m acme_llm.train".to_owned(),
        cwd: String::new(),
        uid: Some(1000),
        gpus: vec!["GPU 0".to_owned()],
        details: RunDetails::default(),
        started_utc_ns: START,
        last_seen_utc_ns: START + 30 * SECOND,
        end: None,
    }
}

/// A bucket at `start` with `samples` samples of `cpu` % and 4 GiB of GPU
/// memory.
fn bucket(start: i64, cpu: f64, samples: u32) -> RunBucket {
    let mut bucket = RunBucket::new(start);
    for _ in 0..samples {
        bucket.add(RunMetric::Cpu, Some(cpu));
        bucket.add(RunMetric::GpuMemory, Some(4.0 * GIB));
        bucket.add(RunMetric::Memory, Some(3.0 * GIB));
    }
    bucket
}

fn row(pid: i64, whole: RunBucket) -> RunRow {
    let info = info(pid);
    RunRow {
        key: info.key.clone(),
        name: info.name.clone(),
        started_utc_ns: info.started_utc_ns,
        runtime_ns: info.runtime_ns(),
        live: true,
        end: None,
        gpus: info.gpus,
        whole,
    }
}

fn request(pid: i64, bar: BarSize) -> RunRequest {
    RunRequest {
        key: key(pid),
        bar,
        columns: 80,
    }
}

/// Run 1 at 1 m bars: two minutes, the second at 100 %.
fn detail(pid: i64, bar: BarSize) -> RunDetail {
    let mut whole = bucket(0, 50.0, 30);
    whole.merge(&bucket(0, 100.0, 30));
    RunDetail {
        request: request(pid, bar),
        info: info(pid),
        live: true,
        bars: Some(RunBars {
            bar: BarSize::OneMinute,
            step_ns: MINUTE,
            window: (START - 78 * MINUTE, START + 2 * MINUTE),
            bars: vec![bucket(START, 50.0, 30), bucket(START + MINUTE, 100.0, 30)],
        }),
        whole,
        loading: false,
    }
}

fn model() -> ProcessHistoryModel {
    ProcessHistoryModel {
        revision: 3,
        runs: vec![row(2, bucket(0, 20.0, 4)), row(1, bucket(0, 50.0, 4))],
        more_runs: 5,
        kept_on_disk: true,
        details: vec![detail(1, BarSize::Auto)],
        tracked: vec![TrackedProcess {
            slot: 2,
            key: key(1),
            pid: 1,
            name: "acme_llm.train".to_owned(),
            current: vec![(EntityId::new("gpu:GPU-aaaa").unwrap(), 4 << 30)],
        }],
        ..ProcessHistoryModel::default()
    }
}

fn view(model: &ProcessHistoryModel, opened: Option<&RunRequest>) -> ProcessHistoryView {
    filtered(model, opened, "")
}

fn filtered(
    model: &ProcessHistoryModel,
    opened: Option<&RunRequest>,
    filter: &str,
) -> ProcessHistoryView {
    build_process_history_view(&RunHistoryInputs {
        model,
        opened,
        utc_offset_s: 3_600,
        bar: BarSize::Auto,
        scales: &scales(),
        filter,
    })
}

/// The session's filter keeps the runs whose name or PID hold it, any
/// case, before the rows are bounded (D-113): an older run that matches
/// is listed though newer ones are not.
#[test]
fn the_filter_keeps_the_runs_whose_name_or_pid_hold_it() {
    let mut model = model();
    model.more_runs = 0;
    let mut runs: Vec<RunRow> = (1..=250)
        .map(|pid| {
            let mut run = row(pid, bucket(0, 10.0, 4));
            run.name = if pid == 7 {
                "ComfyUI/main".to_owned()
            } else {
                "acme_llm.train".to_owned()
            };
            run
        })
        .collect();
    runs.reverse();
    model.runs = runs;
    let pids = |view: &ProcessHistoryView| -> Vec<i32> {
        view.runs.iter().map(|run| run.key.pid.get()).collect()
    };
    let all = filtered(&model, None, "");
    assert_eq!(all.runs.len(), MAX_RUN_ROWS);
    assert_eq!(all.more_runs, 50);
    let comfy = filtered(&model, None, "  comfyui ");
    assert_eq!(pids(&comfy), vec![7], "the oldest rows are searched too");
    assert_eq!(comfy.more_runs, 0);
    assert_eq!(pids(&filtered(&model, None, "249")), vec![249]);
    let none = filtered(&model, None, "nothing");
    assert!(none.runs.is_empty() && none.more_runs == 0);
}

/// The rows come in the model's order with the figures of their whole
/// life and their colors.
#[test]
fn rows_take_the_published_order_figures_and_colors() {
    let view = view(&model(), None);
    let pids: Vec<i32> = view.runs.iter().map(|run| run.key.pid.get()).collect();
    assert_eq!(pids, vec![2, 1]);
    let first = &view.runs[1];
    assert!(first.live && first.end.is_none());
    assert_eq!(first.gpus, vec!["GPU 0".to_owned()]);
    assert_eq!(first.peak_gpu_memory, Some(4.0 * GIB));
    assert_eq!(first.peak_memory, Some(3.0 * GIB));
    assert_eq!(first.peak_children, None, "no probe, no descendants");
    assert_eq!(first.mean_cpu, Some(50.0));
    assert_eq!(first.runtime_ns, 30_000_000_000);
    assert_eq!((first.slot, view.runs[0].slot), (Some(2), None));
    assert_eq!(view.more_runs, 5);
    assert!(view.opened.is_none());
    assert!(view.kept_on_disk);
    assert_eq!(view.utc_offset_s, 3_600);
    assert_eq!((view.bar, view.cpu_scale), (BarSize::Auto, Some(2_000.0)));
}

/// An opened run charts every metric as bars against the host's scales.
#[test]
fn an_opened_run_charts_each_metric_as_bars() {
    let opened = view(&model(), Some(&request(1, BarSize::Auto)))
        .opened
        .unwrap();
    assert_eq!(opened.bar, BarSize::OneMinute, "the size Auto chose");
    assert_eq!(
        opened.window_end_utc_ns - opened.window_start_utc_ns,
        80 * 60 * 1_000_000_000,
        "a bar for each of the 80 columns"
    );
    assert_eq!(opened.slot, Some(2));
    assert_eq!(opened.charts.len(), RunMetric::ALL.len());
    let cpu = &opened.charts[RunMetric::Cpu.index()];
    assert_eq!(cpu.unit, CanonicalUnit::Percent);
    assert_eq!(cpu.scale_max, Some(2_000.0), "every core");
    assert_eq!(cpu.points.len(), 2);
    assert_eq!(cpu.points[0].mean, Some(50.0));
    assert_eq!(
        cpu.points[0].end_utc_ns - cpu.points[0].start_utc_ns,
        60 * 1_000_000_000
    );
    assert_eq!(
        (cpu.latest, cpu.mean, cpu.peak),
        (Some(100.0), Some(75.0), Some(100.0))
    );
    let scale = |metric: RunMetric| opened.charts[metric.index()].scale_max;
    assert_eq!(scale(RunMetric::GpuUtilization), Some(100.0));
    assert_eq!(scale(RunMetric::GpuMemory), Some(120.0 * GIB));
    assert_eq!(scale(RunMetric::Memory), Some(128.0 * GIB));
    assert_eq!(scale(RunMetric::DiskRead), None, "a rate scales to itself");
    // Each GPU's own (D-120): 100 % of it and its own memory; a place the
    // run has no GPU in has no memory scale.
    assert_eq!(scale(RunMetric::Gpu0Utilization), Some(100.0));
    assert_eq!(scale(RunMetric::Gpu0Device), Some(100.0));
    assert_eq!(scale(RunMetric::Gpu0Memory), Some(120.0 * GIB));
    assert_eq!(scale(RunMetric::Gpu1Memory), None);
    let unit = |metric: RunMetric| opened.charts[metric.index()].unit;
    assert_eq!(unit(RunMetric::Gpu0Device), CanonicalUnit::Percent);
    assert_eq!(unit(RunMetric::Gpu3Memory), CanonicalUnit::Bytes);
    let threads = &opened.charts[RunMetric::Threads.index()];
    assert_eq!(threads.unit, CanonicalUnit::Count);
    assert!(threads.points.is_empty(), "no probe, no threads");
    assert!(!opened.loading);
}

/// Until the model answers the size the session switched to, the run
/// stays open with the bars it has; a run the model has no detail of is
/// not opened.
#[test]
fn a_run_stays_open_while_its_new_size_is_on_its_way() {
    let model = model();
    let switched = view(&model, Some(&request(1, BarSize::OneDay)));
    assert_eq!(switched.opened.unwrap().bar, BarSize::OneMinute);
    assert!(
        view(&model, Some(&request(7, BarSize::Auto)))
            .opened
            .is_none()
    );
}

/// A run whose buckets are being read has no bars yet.
#[test]
fn a_run_being_read_is_loading_without_bars() {
    let mut model = model();
    model.details[0].bars = None;
    model.details[0].loading = true;
    let opened = view(&model, Some(&request(1, BarSize::Auto)))
        .opened
        .unwrap();
    assert!(opened.loading);
    assert_eq!(opened.bar, BarSize::Auto, "the session's size");
    assert_eq!(
        (opened.window_start_utc_ns, opened.window_end_utc_ns),
        (0, 0)
    );
    assert!(opened.charts.iter().all(|chart| chart.points.is_empty()));
}

/// The History view asks for the run it opened at its bar size over the
/// plot's columns; another view or no selection asks for none.
#[test]
fn a_session_asks_for_the_run_its_history_opened() {
    let mut session = SessionState {
        session_id: SessionId::new("session-1").unwrap(),
        view: ViewKind::History,
        selected_entity_id: None,
        selected_process: Some(key(1)),
        filter: String::new(),
        sort: dgxtop_core::commands::SortKey::GpuMemory,
        ascending: false,
        overview_bar: BarSize::OneMinute,
        process_bar: BarSize::OneHour,
    };
    let asked = run_request(&session, (160, 50)).unwrap();
    assert_eq!((asked.key, asked.bar), (key(1), BarSize::OneHour));
    assert_eq!(
        asked.columns,
        ViewportLayout::new((160, 50)).run_chart_bars()
    );
    session.view = ViewKind::Overview;
    assert!(run_request(&session, (160, 50)).is_none());
    session.view = ViewKind::History;
    session.selected_process = None;
    assert!(run_request(&session, (160, 50)).is_none());
}

/// A run on GPUs the host does not report has no GPU memory scale.
#[test]
fn a_gpu_without_a_scale_leaves_the_chart_to_its_peak() {
    let mut info = info(1);
    info.gpus = vec!["GPU 0".to_owned(), "GPU 7".to_owned()];
    assert_eq!(scales().of(RunMetric::GpuMemory, &info), None);
    assert_eq!(scales().of(RunMetric::GpuUtilization, &info), Some(200.0));
    info.gpus.clear();
    assert_eq!(scales().of(RunMetric::GpuMemory, &info), None);
    assert_eq!(scales().of(RunMetric::GpuUtilization, &info), Some(100.0));
}
