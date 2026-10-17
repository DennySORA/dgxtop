use std::collections::BTreeMap;

use dgxtop_core::canonical::ProcessRow;
use dgxtop_core::identity::{EntityId, ProcessKey};
use dgxtop_core::process_history::{EndCause, ProcessHistoryLoad, RunEnd, RunMetric};
use dgxtop_core::testing::{boot_id, gpu_process, process_table, test_process_key};
use dgxtop_core::tiers::BarSize;

use super::*;
use crate::process::Now;
use crate::process::runs::SampleInputs;

const SECOND: u64 = 1_000_000_000;
const GPU: &str = "gpu:GPU-aaaa";

fn row(pid: i64, start_ticks: u64) -> ProcessRow {
    let mut row = gpu_process(pid, "acme_llm.train", &[(GPU, Some(1 << 30))]);
    row.key.start_time_ticks = start_ticks;
    row.cpu_percent = Some(50.0);
    row
}

fn observe(book: &mut ProcessHistoryBook, second: u64, rows: Vec<ProcessRow>) {
    let labels = BTreeMap::from([(EntityId::new(GPU).unwrap(), "GPU 0".to_owned())]);
    book.observe(
        &process_table(second, second * SECOND, rows),
        &SampleInputs {
            now: Now {
                boot_ns: second * SECOND,
                utc_ns: 1_789_999_800_000_000_000 + i128::from(second * SECOND),
            },
            clk_tck: Some(100),
            gpu_labels: &labels,
            gpu_utilization: &BTreeMap::new(),
            host_network: [Some(10.0), None],
        },
    );
}

fn build(builder: &mut ModelBuilder, book: &ProcessHistoryBook) -> Option<ProcessHistoryModel> {
    builder.build(&ModelInputs {
        book,
        memory: &ProcessMemoryBook::new(1_000, 0),
        kept_on_disk: false,
        writes: WriteProgress::default(),
    })
}

fn key(pid: i64, start_ticks: u64) -> ProcessKey {
    let mut key = test_process_key(pid);
    key.start_time_ticks = start_ticks;
    key
}

fn request(pid: i64, bar: BarSize) -> RunRequest {
    RunRequest {
        key: key(pid, 100),
        bar,
        columns: 80,
    }
}

/// The model is built when the book moved or a run was asked for, not
/// for an input that changed nothing it shows.
#[test]
fn a_model_is_built_only_when_what_it_shows_changed() {
    let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
    let mut builder = ModelBuilder::default();
    observe(&mut book, 2, vec![row(1, 100)]);
    let first = build(&mut builder, &book).unwrap();
    assert_eq!(first.revision, 1);
    assert!(build(&mut builder, &book).is_none(), "nothing changed");
    builder.show(request(1, BarSize::Auto));
    let shown = build(&mut builder, &book).unwrap();
    assert_eq!(shown.revision, 2);
    assert_eq!(shown.details.len(), 1);
    builder.show(request(1, BarSize::Auto));
    assert!(build(&mut builder, &book).is_none(), "asked again");
    observe(&mut book, 3, vec![row(1, 100)]);
    let sampled = build(&mut builder, &book).unwrap();
    assert_eq!(
        sampled.runs[0].whole.value(RunMetric::Cpu).count,
        2,
        "the new sample"
    );
    assert_eq!(
        sampled.details[0].whole.value(RunMetric::Cpu).count,
        2,
        "the detail follows the book"
    );
}

/// The latest start first, every run listed: a projection takes the ones
/// its session's filter keeps (D-113).
#[test]
fn rows_list_the_latest_start_first_within_the_bound() {
    let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
    let mut writer = ProcessHistoryBook::new(boot_id("boot-test"));
    observe(&mut writer, 1, vec![row(1, 100)]);
    let template = writer.run(&key(1, 100)).unwrap().info().clone();
    let extra = 3;
    let runs = (0..MAX_LISTED_RUNS + extra)
        .map(|index| {
            let mut info = template.clone();
            let index = i64::try_from(index).unwrap();
            info.key = key(index + 1, 100);
            info.started_utc_ns += index;
            info.end = Some(RunEnd {
                utc_ns: info.started_utc_ns + 1,
                cause: EndCause::Exit(0),
            });
            info
        })
        .collect();
    book.restore(
        ProcessHistoryLoad {
            runs,
            ..ProcessHistoryLoad::default()
        },
        Now {
            boot_ns: 10 * SECOND,
            utc_ns: 1_789_999_810_000_000_000,
        },
    );
    let model = build(&mut ModelBuilder::default(), &book).unwrap();
    assert_eq!(model.runs.len(), MAX_LISTED_RUNS);
    assert_eq!(model.more_runs, 0, "the book lists no more");
    let pids: Vec<i32> = model
        .runs
        .iter()
        .take(2)
        .map(|run| run.key.pid.get())
        .collect();
    let last = i32::try_from(MAX_LISTED_RUNS + extra).unwrap();
    assert_eq!(pids, vec![last, last - 1], "the latest start first");
}

/// Running runs come first, however long ago they started; then the
/// ended ones; each the latest start first (D-115).
#[test]
fn running_runs_come_first() {
    let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
    let mut writer = ProcessHistoryBook::new(boot_id("boot-test"));
    observe(&mut writer, 1, vec![row(1, 100)]);
    let template = writer.run(&key(1, 100)).unwrap().info().clone();
    // Ended runs that started after the running one.
    let ended = (2..=3)
        .map(|pid| {
            let mut info = template.clone();
            info.key = key(pid, 100);
            info.started_utc_ns += pid * 1_000;
            info.end = Some(RunEnd {
                utc_ns: info.started_utc_ns + 1,
                cause: EndCause::Exit(0),
            });
            info
        })
        .collect();
    book.restore(
        ProcessHistoryLoad {
            runs: ended,
            ..ProcessHistoryLoad::default()
        },
        Now {
            boot_ns: 10 * SECOND,
            utc_ns: 1_789_999_810_000_000_000,
        },
    );
    observe(&mut book, 11, vec![row(1, 100), row(4, 900)]);
    let model = build(&mut ModelBuilder::default(), &book).unwrap();
    let order: Vec<(i32, bool)> = model
        .runs
        .iter()
        .map(|run| (run.key.pid.get(), run.live))
        .collect();
    assert_eq!(
        order,
        vec![(4, true), (1, true), (3, false), (2, false)],
        "running first, the latest start first within each"
    );
}

/// The details of the latest runs asked for are kept, the latest last.
#[test]
fn the_latest_runs_asked_for_keep_their_details() {
    let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
    observe(&mut book, 2, vec![row(1, 100), row(2, 100)]);
    let mut builder = ModelBuilder::default();
    let sizes = [
        BarSize::Auto,
        BarSize::OneMinute,
        BarSize::FiveMinutes,
        BarSize::OneHour,
        BarSize::OneDay,
    ];
    for bar in sizes {
        builder.show(request(1, bar));
    }
    builder.show(request(2, BarSize::Auto));
    let model = build(&mut builder, &book).unwrap();
    let asked: Vec<(i32, BarSize)> = model
        .details
        .iter()
        .map(|detail| (detail.request.key.pid.get(), detail.request.bar))
        .collect();
    assert_eq!(
        asked,
        vec![
            (1, BarSize::FiveMinutes),
            (1, BarSize::OneHour),
            (1, BarSize::OneDay),
            (2, BarSize::Auto),
        ]
    );
    assert_eq!(asked.len(), MAX_SHOWN_RUNS);
    let unknown = RunRequest {
        key: key(7, 100),
        ..request(1, BarSize::Auto)
    };
    builder.show(unknown.clone());
    let model = build(&mut builder, &book).unwrap();
    assert!(!model.answers(&unknown), "no such run");
}

const GIB: f64 = 1_073_741_824.0;

/// Process `pid` started at `start_ticks`, at 50 % CPU with 3 GiB of RAM
/// and `gpu_bytes` on the GPU.
fn train(pid: i64, start_ticks: u64, gpu_bytes: u64) -> ProcessRow {
    let mut row = gpu_process(pid, "acme_llm.train", &[(GPU, Some(gpu_bytes))]);
    row.key.start_time_ticks = start_ticks;
    row.cpu_percent = Some(50.0);
    row.rss_bytes = Some(3 << 30);
    row
}

/// `request` answered by a model of `book`.
fn detail(book: &ProcessHistoryBook, request: RunRequest) -> RunDetail {
    let mut builder = ModelBuilder::default();
    builder.show(request.clone());
    let model = build(&mut builder, book).unwrap();
    model.detail_for(&request).unwrap().clone()
}

/// The list puts the latest start first and sums up each run.
#[test]
fn runs_are_listed_newest_start_first_with_their_figures() {
    let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
    for second in 100..170 {
        observe(
            &mut book,
            second,
            vec![train(1, 100, 4 << 30), train(2, 900, 1 << 30)],
        );
    }
    let model = build(&mut ModelBuilder::default(), &book).unwrap();
    let pids: Vec<i32> = model.runs.iter().map(|run| run.key.pid.get()).collect();
    assert_eq!(pids, vec![2, 1], "started at 9 s, then at 1 s");
    let first = &model.runs[1];
    assert!(first.live && first.end.is_none());
    assert_eq!(first.gpus, vec!["GPU 0".to_owned()]);
    assert_eq!(
        first.whole.value(RunMetric::GpuMemory).peak(),
        Some(4.0 * GIB)
    );
    assert_eq!(first.whole.value(RunMetric::Memory).peak(), Some(3.0 * GIB));
    assert_eq!(
        first.whole.value(RunMetric::Children).peak(),
        None,
        "no probe, no descendants"
    );
    assert_eq!(first.whole.value(RunMetric::Cpu).mean(), Some(50.0));
    assert_eq!(model.more_runs, 0);
    assert!(model.details.is_empty());
}

/// An opened run gets bars of every metric; `Auto` takes the finest bar
/// that shows the run whole.
#[test]
fn an_opened_run_gets_bars_of_the_size_that_shows_it_whole() {
    let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
    // Sampled from 2 s to 31 s.
    for second in 2..32 {
        observe(&mut book, second, vec![train(1, 100, 4 << 30)]);
    }
    let opened = detail(&book, request(1, BarSize::Auto));
    let bars = opened.bars.as_ref().unwrap();
    assert_eq!(bars.bar, BarSize::OneMinute);
    assert_eq!(
        bars.window.1 - bars.window.0,
        80 * 60 * 1_000_000_000,
        "a bar for each of the 80 columns"
    );
    assert_eq!(bars.bars.len(), 1, "the minute it ran");
    assert_eq!(bars.step_ns, 60 * 1_000_000_000);
    let cpu = bars.bars[0].value(RunMetric::Cpu);
    assert_eq!((cpu.mean(), cpu.peak()), (Some(50.0), Some(50.0)));
    assert_eq!(opened.whole.value(RunMetric::Cpu).mean(), Some(50.0));
    assert_eq!(bars.bars[0].value(RunMetric::Threads).count, 0, "no probe");
    assert_eq!(
        opened.whole.value(RunMetric::HostNetworkReceive).mean(),
        Some(10.0)
    );
    assert!(opened.live && !opened.loading);
}

/// The session's bar size: a sample every second over 30 s is one 1 h
/// bar, whose mean is the run's.
#[test]
fn the_size_asked_for_is_laid_out() {
    let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
    for second in 2..32 {
        observe(&mut book, second, vec![train(1, 100, 4 << 30)]);
    }
    let opened = detail(
        &book,
        RunRequest {
            columns: 24,
            ..request(1, BarSize::OneHour)
        },
    );
    let bars = opened.bars.unwrap();
    assert_eq!(bars.bar, BarSize::OneHour);
    assert_eq!(bars.window.1 - bars.window.0, 24 * 3_600 * 1_000_000_000);
    assert_eq!(bars.bars.len(), 1);
    assert_eq!(
        bars.bars[0].value(RunMetric::GpuMemory).mean(),
        Some(4.0 * GIB)
    );
}

/// A run that started long before dgxtop saw it is laid out at the finest
/// size of what dgxtop saw, not one coarse bucket.
#[test]
fn a_run_older_than_dgxtop_is_laid_out_finely() {
    let mut book = ProcessHistoryBook::new(boot_id("boot-test"));
    // Started 1 s after boot; seen from an hour later.
    for second in 3_600..3_700 {
        observe(&mut book, second, vec![train(1, 100, 4 << 30)]);
    }
    let bars = detail(&book, request(1, BarSize::Auto)).bars.unwrap();
    assert_eq!(bars.bar, BarSize::OneMinute);
    assert!(bars.bars.len() >= 2);
}
