//! The REF frontend: the real terminal runner (`TuiLoop`: Controller,
//! dirty-only render scheduler capped at `viewer.fps`, Viewer, bounded
//! frame-atomic output) on a 160×50 virtual terminal whose transport is a
//! null sink and whose input never fires.
//!
//! Frame time = the runner step that drew a frame minus the time the step
//! spent waiting for input: projection accepted → layout, render, Ratatui
//! diff and ANSI encoding done (`PERFORMANCE.md` §2, transport excluded).
//! Every projection the runner takes is reported to the probes (projection
//! latency) and to the script hints (valid selections).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use dgxtop::perf::latency::duration_us;
use dgxtop_core::commands::{CommandEnvelope, CommandRejection, CommandResult};
use dgxtop_core::identity::SessionId;
use dgxtop_core::ports::SessionPort;
use dgxtop_core::view::ViewModel;
use dgxtop_ui::terminal::GatedInput;
use dgxtop_ui::{SystemClock, TuiLoop, TuiOptions};
use serde::Serialize;

use super::probes::Probes;
use super::script::{ResultRouter, STALL_AFTER, ScriptHints};
use crate::common::BenchResult;
use crate::common::headless::{IdleGate, NullTransport};

/// What the frontend saw.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FrontendReport {
    #[serde(skip)]
    pub frame_us: Vec<u64>,
    pub projections_taken: u64,
    pub frames_drawn: u64,
    pub full_redraws: u64,
    pub overflowed_frames: u64,
    pub bytes_written: u64,
    pub peak_output_bytes: usize,
    pub exit: Option<String>,
    pub stalls: u64,
    pub longest_projection_gap_ms: u64,
}

/// Session the runner talks to: the Manager's handle plus measurement taps.
struct MeasuredSession {
    router: Arc<ResultRouter>,
    probes: Arc<Probes>,
    hints: Arc<Mutex<ScriptHints>>,
    last_projection: Arc<Mutex<Option<Instant>>>,
    projections: Arc<AtomicU64>,
}

impl SessionPort for MeasuredSession {
    fn session_id(&self) -> &SessionId {
        self.router.session().session_id()
    }

    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection> {
        self.router.session().submit(command)
    }

    fn take_projection(&self) -> Option<Arc<ViewModel>> {
        let projection = self.router.session().take_projection()?;
        self.probes
            .on_projection(projection.snapshot_revision, projection.generated_boot_ns);
        self.hints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .absorb(&projection);
        *self
            .last_projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        self.projections.fetch_add(1, Ordering::Relaxed);
        Some(projection)
    }

    fn drain_results(&self, max: usize) -> Vec<CommandResult> {
        self.router.drain_for_frontend(max)
    }

    fn request_shutdown(&self, reason: &str) {
        self.router.session().request_shutdown(reason);
    }

    fn shutdown_requested(&self) -> bool {
        self.router.session().shutdown_requested()
    }

    fn set_viewport(&self, columns: u16, rows: u16) {
        self.router.session().set_viewport(columns, rows);
    }
}

/// What the frontend thread needs.
pub struct FrontendInputs {
    pub router: Arc<ResultRouter>,
    pub probes: Arc<Probes>,
    pub hints: Arc<Mutex<ScriptHints>>,
    pub viewport: (u16, u16),
    pub fps: u32,
}

pub struct Frontend {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<FrontendReport>>,
}

impl Frontend {
    pub fn start(inputs: FrontendInputs) -> BenchResult<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("bench-frontend".to_owned())
            .spawn(move || run_frontend(inputs, &flag))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) -> FrontendReport {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_default()
    }
}

fn run_frontend(inputs: FrontendInputs, stop: &AtomicBool) -> FrontendReport {
    let mut report = FrontendReport::default();
    let waited_ns = Arc::new(AtomicU64::new(0));
    let last_projection = Arc::new(Mutex::new(None));
    let projections = Arc::new(AtomicU64::new(0));
    let session = MeasuredSession {
        router: Arc::clone(&inputs.router),
        probes: Arc::clone(&inputs.probes),
        hints: Arc::clone(&inputs.hints),
        last_projection: Arc::clone(&last_projection),
        projections: Arc::clone(&projections),
    };
    let created = TuiLoop::new(
        Box::new(session),
        Box::new(NullTransport::new(Arc::new(AtomicU64::new(0)))),
        Box::new(GatedInput::crossterm(Box::new(IdleGate::new(
            true,
            Arc::clone(&waited_ns),
        )))),
        Box::new(SystemClock::new()),
        TuiOptions {
            fps: inputs.fps,
            mouse: false,
            ..TuiOptions::default()
        },
        inputs.viewport,
    );
    let mut tui = match created {
        Ok(tui) => tui,
        Err(error) => {
            report.exit = Some(format!("terminal runner could not start: {error}"));
            return report;
        }
    };
    let mut stalled = false;
    while !stop.load(Ordering::Acquire) {
        let waited_before = waited_ns.load(Ordering::Relaxed);
        let drawn_before = tui.stats().frames_drawn;
        let started = Instant::now();
        let outcome = tui.step();
        let elapsed_us = duration_us(started.elapsed());
        let waited_us = waited_ns
            .load(Ordering::Relaxed)
            .saturating_sub(waited_before)
            / 1_000;
        match outcome {
            Ok(None) => {}
            Ok(Some(reason)) => {
                report.exit = Some(format!("{reason:?}"));
                break;
            }
            Err(error) => {
                report.exit = Some(error.to_string());
                break;
            }
        }
        let measuring = inputs.probes.is_measuring();
        if measuring && tui.stats().frames_drawn > drawn_before {
            report.frame_us.push(elapsed_us.saturating_sub(waited_us));
        }
        let gap = last_projection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map(|last| last.elapsed());
        if let Some(gap) = gap.filter(|_| measuring) {
            report.longest_projection_gap_ms = report
                .longest_projection_gap_ms
                .max(u64::try_from(gap.as_millis()).unwrap_or(u64::MAX));
            let stall = gap >= STALL_AFTER;
            if stall && !stalled {
                report.stalls += 1;
            }
            stalled = stall;
        }
    }
    let stats = tui.stats();
    report.projections_taken = projections.load(Ordering::Relaxed);
    report.frames_drawn = stats.frames_drawn;
    report.full_redraws = stats.full_redraws;
    report.overflowed_frames = stats.overflowed_frames;
    report.bytes_written = stats.bytes_written;
    report.peak_output_bytes = stats.peak_output_bytes;
    report
}
