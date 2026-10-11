//! The terminal main-thread loop (`RUNTIME.md` §1, `UI.md` §4–§5).
//!
//! Each step: poll input (until the next draw could happen, at most
//! [`LONGEST_IDLE_POLL`], never a 0 ms busy loop),
//! route events through the Controller (non-blocking submits), drain
//! command results, honour shutdown, write at most 8 KiB / 2 ms of the
//! pending frame, and — only once the previous frame is completely written —
//! take the latest projection and draw a dirty frame into the bounded
//! buffer. Input keeps being handled while the terminal does not read.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::Event;
use dgxtop_core::ports::{SessionPort, TerminalTransport};
use dgxtop_core::view::{BannerLevel, OverloadState, ViewModel};
use ratatui::Terminal;
use ratatui::layout::Size;

use super::backend::AnsiBackend;
use super::clock::LoopClock;
use super::error::TuiError;
use super::input::InputSource;
use super::output::{FlushProgress, FrameWriter, OUTPUT_BUFFER_CAPACITY, OutputPipeline};
use super::scheduler::{Decision, INPUT_REDRAW_INTERVAL, RenderKey, RenderScheduler};
use super::setup;
use super::stats::{FrameStats, FrameStatsSummary};
use crate::controller::{Controller, ControllerContext};
use crate::viewer::{HitMap, RenderInput, Viewer};

/// Input poll while a command result is awaited: results are looked for
/// at the input cadence.
pub const RESULT_POLL: Duration = Duration::from_millis(50);
/// Longest input poll once a new projection could be drawn. Projections
/// and the Manager's shutdown request arrive without waking the loop, so
/// this bounds how late they are seen.
pub const MAX_IDLE_POLL: Duration = Duration::from_millis(100);
/// Longest input poll of all: right after a draw, nothing that arrives can
/// be drawn before the data interval, so the loop sleeps until then, up to
/// this.
pub const LONGEST_IDLE_POLL: Duration = Duration::from_millis(250);
/// Shortest first poll of a step (never a 0 ms busy loop).
pub const MIN_POLL: Duration = Duration::from_millis(1);
/// Poll timeout while a frame is being written and the terminal accepts.
const FLUSH_POLL: Duration = Duration::from_millis(1);
/// Poll timeout while the terminal would block (retry cadence).
const BLOCKED_POLL: Duration = Duration::from_millis(10);
/// Bound of the terminal-restore wait at exit (`RUNTIME.md` §8).
pub const MAX_EXIT_WAIT: Duration = Duration::from_millis(250);
/// A frame whose writing took longer is followed by a full redraw.
pub const SLOW_FRAME: Duration = Duration::from_secs(1);
const MAX_EVENTS_PER_STEP: usize = 64;
const MAX_RESULTS_PER_STEP: usize = 64;
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(2);

/// Runner options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiOptions {
    /// Data-driven frame cap (1..=20, default 4).
    pub fps: u32,
    /// Capture the mouse for click selection.
    pub mouse: bool,
    /// Terminal-restore wait at exit (capped at 250 ms).
    pub exit_wait: Duration,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            fps: 4,
            mouse: true,
            exit_wait: MAX_EXIT_WAIT,
        }
    }
}

/// Why the loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiExitReason {
    /// `q` / `Ctrl-C` (shutdown was requested out of band).
    UserQuit,
    /// The Manager asked every frontend to stop.
    ManagerShutdown,
    /// Terminal output failed (EIO/EPIPE); rendering stopped.
    OutputClosed(String),
    /// The terminal hung up; rendering stopped.
    InputClosed,
    /// Terminal input failed.
    InputFailed(String),
}

/// Result of a finished loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiExit {
    pub reason: TuiExitReason,
    pub stats: FrameStatsSummary,
    /// Whether the terminal-restore bytes were completely written in time.
    pub restore_flushed: bool,
}

/// Builds the controller context from disjoint fields so the borrow checker
/// accepts a simultaneous `&mut self.controller`.
macro_rules! controller_context {
    ($runner:ident, $model:expr, $now:expr) => {
        ControllerContext {
            model: $model,
            hit_map: &$runner.hit_map,
            frame_revision: $runner.rendered_revision,
            now: $now,
            projection_received_at: $runner.active_received_at,
        }
    };
}

/// The testable terminal loop.
pub struct TuiLoop {
    session: Box<dyn SessionPort>,
    input: Box<dyn InputSource>,
    clock: Box<dyn LoopClock>,
    output: OutputPipeline,
    terminal: Terminal<AnsiBackend<FrameWriter>>,
    viewer: Viewer,
    controller: Controller,
    scheduler: RenderScheduler,
    stats: FrameStats,
    options: TuiOptions,
    active: Option<Arc<ViewModel>>,
    active_received_at: Option<Duration>,
    hit_map: HitMap,
    rendered_revision: u64,
    viewport: (u16, u16),
    pending_resize: Option<(u16, u16)>,
    started_at: Duration,
    next_poll: Duration,
    consecutive_failures: u32,
    finished: bool,
}

impl std::fmt::Debug for TuiLoop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TuiLoop")
            .field("viewport", &self.viewport)
            .field("rendered_revision", &self.rendered_revision)
            .field("output", &self.output)
            .finish()
    }
}

impl TuiLoop {
    pub fn new(
        session: Box<dyn SessionPort>,
        transport: Box<dyn TerminalTransport>,
        input: Box<dyn InputSource>,
        clock: Box<dyn LoopClock>,
        options: TuiOptions,
        viewport: (u16, u16),
    ) -> Result<Self, TuiError> {
        Self::with_output_capacity(
            session,
            transport,
            input,
            clock,
            options,
            viewport,
            OUTPUT_BUFFER_CAPACITY,
        )
    }

    /// Like [`Self::new`] with a custom (smaller) output cap, for tests and
    /// constrained harnesses. The cap never exceeds 1 MiB.
    #[allow(clippy::too_many_arguments)]
    pub fn with_output_capacity(
        session: Box<dyn SessionPort>,
        transport: Box<dyn TerminalTransport>,
        input: Box<dyn InputSource>,
        clock: Box<dyn LoopClock>,
        options: TuiOptions,
        viewport: (u16, u16),
        capacity: usize,
    ) -> Result<Self, TuiError> {
        let output = OutputPipeline::new(transport, capacity.min(OUTPUT_BUFFER_CAPACITY));
        let backend = AnsiBackend::new(output.writer(), Size::new(viewport.0, viewport.1));
        let terminal = Terminal::new(backend).map_err(TuiError::Output)?;
        session.set_viewport(viewport.0, viewport.1);
        let started_at = clock.now();
        Ok(Self {
            session,
            input,
            clock,
            output,
            terminal,
            viewer: Viewer::builtin(),
            controller: Controller::new(),
            scheduler: RenderScheduler::new(options.fps),
            stats: FrameStats::default(),
            options,
            active: None,
            active_received_at: None,
            hit_map: HitMap::default(),
            rendered_revision: 0,
            viewport,
            pending_resize: None,
            started_at,
            next_poll: MIN_POLL,
            consecutive_failures: 0,
            finished: false,
        })
    }

    pub fn stats(&self) -> &FrameStats {
        &self.stats
    }

    pub fn controller(&self) -> &Controller {
        &self.controller
    }

    /// Recent frames written (frames never interleave).
    #[cfg(test)]
    pub fn frame_records(&self) -> Vec<super::FrameRecord> {
        self.output.frames().copied().collect()
    }

    /// Queues alternate screen, hidden cursor, clear and mouse capture as
    /// the first output; it is written before the first frame is drawn.
    pub fn enter_screen(&mut self) -> Result<(), TuiError> {
        let mut writer = self.output.writer();
        writer
            .write_all(&setup::enter_sequence(self.options.mouse))
            .map_err(TuiError::Output)?;
        let _ = self.output.begin_frame(self.clock.now());
        Ok(())
    }

    /// Runs until quit, Manager shutdown or a terminal failure; always
    /// attempts the bounded terminal restore before returning.
    pub fn run(&mut self) -> Result<TuiExit, TuiError> {
        loop {
            match self.step() {
                Ok(None) => {}
                Ok(Some(reason)) => return Ok(self.finish(reason)),
                Err(error) => {
                    self.session.request_shutdown("terminal input failed");
                    let _ = self.finish(TuiExitReason::InputFailed(error.to_string()));
                    return Err(error);
                }
            }
        }
    }

    /// One loop iteration; `Some(reason)` means the loop must end.
    pub fn step(&mut self) -> Result<Option<TuiExitReason>, TuiError> {
        match self.poll_input() {
            Ok(()) => {}
            Err(TuiError::InputClosed) => return Ok(Some(self.input_closed())),
            Err(error) => return Err(error),
        }
        let now = self.clock.now();
        let context = controller_context!(self, self.active.as_deref(), now);
        self.controller.finish_input_batch(&context, &*self.session);
        if self.controller.quit_requested() {
            self.session.request_shutdown("user");
            return Ok(Some(TuiExitReason::UserQuit));
        }
        let results = self.session.drain_results(MAX_RESULTS_PER_STEP);
        for result in &results {
            let context = controller_context!(self, self.active.as_deref(), now);
            self.controller.on_result(result, &context, &*self.session);
        }
        if self.session.shutdown_requested() {
            return Ok(Some(TuiExitReason::ManagerShutdown));
        }
        self.controller.tick(now);
        if let Err(error) = self.flush_output() {
            return Ok(Some(self.output_closed(&error)));
        }
        if self.output.has_pending() {
            if self.draw_due(now) {
                self.stats.deferred_while_flushing += 1;
            }
        } else {
            self.accept_projection(now);
            self.render_if_due(now);
            if let Err(error) = self.flush_output() {
                return Ok(Some(self.output_closed(&error)));
            }
        }
        self.stats.peak_output_bytes = self.output.peak_bytes();
        self.stats.bytes_written = self.output.bytes_written();
        self.next_poll = self.poll_timeout(self.clock.now());
        Ok(None)
    }

    fn poll_input(&mut self) -> Result<(), TuiError> {
        let mut event = self.input.poll_event(self.next_poll)?;
        let mut handled = 0usize;
        while let Some(current) = event.take() {
            self.route_event(current);
            handled += 1;
            if handled >= MAX_EVENTS_PER_STEP || self.controller.quit_requested() {
                break;
            }
            // Drain what is already queued without waiting.
            event = self.input.poll_event(Duration::ZERO)?;
        }
        Ok(())
    }

    fn route_event(&mut self, event: Event) {
        if let Event::Resize(columns, rows) = event {
            self.session.set_viewport(columns, rows);
            self.pending_resize = Some((columns, rows));
            return;
        }
        let now = self.clock.now();
        let context = controller_context!(self, self.active.as_deref(), now);
        self.controller
            .handle_event(&event, &context, &*self.session);
    }

    fn output_closed(&mut self, error: &std::io::Error) -> TuiExitReason {
        self.session.request_shutdown("terminal output closed");
        TuiExitReason::OutputClosed(error.to_string())
    }

    fn input_closed(&mut self) -> TuiExitReason {
        self.session.request_shutdown("terminal hung up");
        TuiExitReason::InputClosed
    }

    /// Keeps only the latest projection; called only when the previous frame
    /// is fully written (until then the latest one waits in the session slot).
    fn accept_projection(&mut self, now: Duration) {
        let Some(projection) = self.session.take_projection() else {
            return;
        };
        self.active = Some(projection);
        self.active_received_at = Some(now);
        if let Some(model) = self.active.as_deref() {
            let context = controller_context!(self, Some(model), now);
            self.controller.on_projection(model, &context);
        }
    }

    fn render_key(&self) -> RenderKey {
        RenderKey {
            frame_revision: self.active.as_ref().map(|model| model.frame_revision),
            viewport: self.pending_resize.unwrap_or(self.viewport),
            ui_version: self.controller.version(),
        }
    }

    fn pressured(&self) -> bool {
        self.active.as_ref().is_some_and(|model| {
            matches!(
                model.header.overload,
                OverloadState::Pressured | OverloadState::Throttled
            )
        })
    }

    fn draw_due(&self, now: Duration) -> bool {
        self.scheduler
            .decide(self.render_key(), now, self.pressured())
            == Decision::Draw
    }

    fn render_if_due(&mut self, now: Duration) {
        let key = self.render_key();
        if self.scheduler.decide(key, now, self.pressured()) == Decision::Draw {
            self.draw(key, now);
        }
    }

    fn draw(&mut self, key: RenderKey, now: Duration) {
        if let Some((columns, rows)) = self.pending_resize.take() {
            self.viewport = (columns, rows);
            self.terminal
                .backend_mut()
                .set_size(Size::new(columns, rows));
        }
        let full_redraw = self.scheduler.full_redraw_pending();
        // A copied command line goes out with this frame (D-76); a frame
        // that fails takes it with it, as a terminal would lose it anyway.
        if let Some(text) = self.controller.take_clipboard() {
            let _ = std::io::Write::write_all(
                &mut self.output.writer(),
                &crate::clipboard::osc52(&text),
            );
        }
        let started = self.clock.now();
        let cleared = !full_redraw || self.terminal.clear().is_ok();
        let ui = self.controller.ui_state(now);
        let projection_age_ms = self.active_received_at.map_or(0, |received| {
            u64::try_from(now.saturating_sub(received).as_millis()).unwrap_or(u64::MAX)
        });
        let tick = now.saturating_sub(self.started_at).as_secs();
        let viewer = &self.viewer;
        let model = self.active.as_deref();
        let mut report = None;
        let drawn = cleared
            && self
                .terminal
                .draw(|frame| {
                    report = Some(viewer.render(
                        frame,
                        &RenderInput {
                            model,
                            ui: &ui,
                            projection_age_ms,
                            tick,
                        },
                    ));
                })
                .is_ok();
        let compute = self.clock.now().saturating_sub(started);
        let overflowed = self.output.overflowed();
        let sealed = drawn && self.output.begin_frame(now).is_ok();
        match (sealed, report) {
            (true, Some(report)) => {
                self.stats.compute.push(compute);
                self.stats.frames_drawn += 1;
                if full_redraw {
                    self.stats.full_redraws += 1;
                }
                self.rendered_revision = report.hit_map.frame_revision();
                self.hit_map = report.hit_map;
                self.consecutive_failures = 0;
                self.scheduler.drawn(key, now);
            }
            _ => self.frame_failed(key, now, overflowed),
        }
    }

    /// A frame that failed to encode (e.g. over 1 MiB) was discarded unsent:
    /// the next frame is a full redraw, retried with backoff.
    fn frame_failed(&mut self, key: RenderKey, now: Duration, overflowed: bool) {
        self.output.discard_unsent();
        if overflowed {
            self.stats.overflowed_frames += 1;
        } else {
            self.stats.draw_errors += 1;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.scheduler.drawn(key, now);
        self.scheduler.request_full_redraw();
        let exponent = self.consecutive_failures.min(6);
        let backoff = INPUT_REDRAW_INTERVAL
            .saturating_mul(1 << exponent)
            .min(MAX_FAILURE_BACKOFF);
        self.scheduler.defer_until(now.saturating_add(backoff));
        if self.consecutive_failures == 1 {
            self.controller.notify(
                BannerLevel::Warning,
                "a frame did not fit the terminal output buffer; retrying with a full redraw",
                now,
            );
        }
    }

    fn flush_output(&mut self) -> std::io::Result<()> {
        if let FlushProgress::Completed { elapsed } = self.output.flush_step(&*self.clock)? {
            self.stats.flush.push(elapsed);
            if elapsed > SLOW_FRAME {
                // The terminal may have been resized or disturbed while it
                // was not reading: resynchronize with a complete frame.
                self.stats.slow_frames += 1;
                self.scheduler.request_full_redraw();
            }
        }
        Ok(())
    }

    fn poll_timeout(&self, now: Duration) -> Duration {
        if self.output.has_pending() {
            return if self.output.is_blocked() {
                BLOCKED_POLL
            } else {
                FLUSH_POLL
            };
        }
        let pressured = self.pressured();
        let until_due = match self.scheduler.decide(self.render_key(), now, pressured) {
            Decision::Draw => MIN_POLL,
            Decision::WaitUntil(due) => due.saturating_sub(now),
        };
        let idle = if self.controller.awaits_results() {
            RESULT_POLL
        } else {
            let until_data = self.scheduler.next_data_draw(pressured).saturating_sub(now);
            until_data.clamp(MAX_IDLE_POLL, LONGEST_IDLE_POLL)
        };
        until_due.min(idle).max(MIN_POLL)
    }

    /// Bounded terminal restore (≤ 250 ms), then the transport's flags.
    fn finish(&mut self, reason: TuiExitReason) -> TuiExit {
        let restore_flushed = match reason {
            // Nobody can see restore bytes on a closed or hung-up terminal.
            TuiExitReason::OutputClosed(_) | TuiExitReason::InputClosed => false,
            _ => {
                let wait = self.options.exit_wait.min(MAX_EXIT_WAIT);
                let deadline = self.clock.now().saturating_add(wait);
                self.output.append_control(&setup::restore_sequence())
                    && self
                        .output
                        .drain_until(deadline, &*self.clock)
                        .unwrap_or(false)
            }
        };
        let _ = self.output.restore_transport();
        self.finished = true;
        self.stats.peak_output_bytes = self.output.peak_bytes();
        self.stats.bytes_written = self.output.bytes_written();
        TuiExit {
            reason,
            stats: self.stats.summary(),
            restore_flushed,
        }
    }
}

impl Drop for TuiLoop {
    fn drop(&mut self) {
        if !self.finished {
            // Restoring descriptor flags is idempotent by contract.
            let _ = self.output.restore_transport();
        }
    }
}

#[cfg(test)]
mod tests;
