//! Bounded, frame-atomic terminal output (`UI.md` §5).
//!
//! Ratatui encodes a frame into an in-memory buffer capped at 1 MiB; the
//! runner then hands those bytes to the non-blocking [`TerminalTransport`]
//! at most 8 KiB or 2 ms per loop iteration. A new frame is only encoded
//! once the previous one was written completely, so frames are never
//! spliced and an escape sequence is never cut. A frame that does not fit
//! the buffer is discarded before a single byte of it is sent.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dgxtop_core::ports::TerminalTransport;

use super::clock::LoopClock;

/// Upper bound of buffered terminal output (frame + restore tail).
pub const OUTPUT_BUFFER_CAPACITY: usize = 1 << 20;
/// Bytes kept free for the terminal-restore sequence written at exit.
pub const CONTROL_RESERVE: usize = 4 * 1024;
/// Write budget per loop iteration (bytes).
pub const FLUSH_BYTES_PER_ITERATION: usize = 8 * 1024;
/// Write budget per loop iteration (time).
pub const FLUSH_TIME_PER_ITERATION: Duration = Duration::from_millis(2);
/// Capacity kept after a large frame (the buffer shrinks back to this).
const RETAINED_CAPACITY: usize = 64 * 1024;
/// Recent frame records kept for diagnostics.
const FRAME_LOG: usize = 64;
/// Upper bound of write attempts while draining at exit.
const MAX_DRAIN_ATTEMPTS: usize = 100_000;
/// Longest single writability wait while draining at exit.
const DRAIN_WAIT_SLICE: Duration = Duration::from_millis(10);

#[derive(Debug)]
struct BufferState {
    bytes: Vec<u8>,
    flushed: usize,
    capacity: usize,
    overflowed: bool,
    peak: usize,
}

impl BufferState {
    fn frame_limit(&self) -> usize {
        self.capacity.saturating_sub(CONTROL_RESERVE)
    }

    fn append(&mut self, data: &[u8], limit: usize) -> bool {
        let needed = self.bytes.len().saturating_add(data.len());
        if needed > limit {
            return false;
        }
        if needed > self.bytes.capacity() {
            let target = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .min(limit);
            self.bytes.reserve_exact(target - self.bytes.len());
        }
        self.bytes.extend_from_slice(data);
        self.peak = self.peak.max(self.bytes.len());
        true
    }

    fn reset(&mut self) {
        self.bytes.clear();
        self.flushed = 0;
        self.overflowed = false;
        if self.bytes.capacity() > RETAINED_CAPACITY {
            self.bytes.shrink_to(RETAINED_CAPACITY);
        }
    }
}

/// Buffer shared by the Ratatui backend (writer) and the pipeline (drainer).
#[derive(Debug, Clone)]
pub struct SharedFrameBuffer(Arc<Mutex<BufferState>>);

impl SharedFrameBuffer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(BufferState {
            bytes: Vec::new(),
            flushed: 0,
            capacity: capacity.max(CONTROL_RESERVE.saturating_mul(2)),
            overflowed: false,
            peak: 0,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, BufferState> {
        // Single-threaded use; a poisoned lock still holds valid bytes.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn writer(&self) -> FrameWriter {
        FrameWriter {
            shared: self.clone(),
        }
    }
}

/// `io::Write` half used by the backend. Writes past the frame limit fail
/// and poison the current frame so a truncated frame is never sent.
#[derive(Debug, Clone)]
pub struct FrameWriter {
    shared: SharedFrameBuffer,
}

impl Write for FrameWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut state = self.shared.lock();
        let limit = state.frame_limit();
        if state.overflowed || !state.append(data, limit) {
            state.overflowed = true;
            return Err(io::Error::other(
                "frame exceeds the terminal output buffer (1 MiB)",
            ));
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Progress of one flush step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushProgress {
    /// Nothing was pending.
    Idle,
    /// The frame finished; `elapsed` is the time since it was enqueued.
    Completed { elapsed: Duration },
    /// Budget used up; more bytes remain.
    Partial,
    /// The terminal would block.
    Blocked,
}

/// One enqueued frame (diagnostics: proves frames never interleave).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRecord {
    /// Stream offset of the frame's first byte.
    pub start_offset: u64,
    pub len: u64,
    /// Bytes the transport had accepted when the frame was enqueued.
    pub written_before: u64,
}

/// The frame did not fit the output buffer and was discarded unsent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameOverflow;

/// Buffer + transport.
pub struct OutputPipeline {
    buffer: SharedFrameBuffer,
    transport: Box<dyn TerminalTransport>,
    frame_started_at: Option<Duration>,
    bytes_enqueued: u64,
    bytes_written: u64,
    blocked: bool,
    frames: VecDeque<FrameRecord>,
}

impl std::fmt::Debug for OutputPipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputPipeline")
            .field("pending", &self.pending_len())
            .field("bytes_written", &self.bytes_written)
            .field("blocked", &self.blocked)
            .finish()
    }
}

impl OutputPipeline {
    pub fn new(transport: Box<dyn TerminalTransport>, capacity: usize) -> Self {
        Self {
            buffer: SharedFrameBuffer::with_capacity(capacity),
            transport,
            frame_started_at: None,
            bytes_enqueued: 0,
            bytes_written: 0,
            blocked: false,
            frames: VecDeque::new(),
        }
    }

    pub fn writer(&self) -> FrameWriter {
        self.buffer.writer()
    }

    pub fn pending_len(&self) -> usize {
        let state = self.buffer.lock();
        state.bytes.len() - state.flushed
    }

    pub fn has_pending(&self) -> bool {
        self.pending_len() > 0
    }

    /// The last write attempt would have blocked.
    pub fn is_blocked(&self) -> bool {
        self.blocked
    }

    /// The frame being encoded exceeded the buffer.
    pub fn overflowed(&self) -> bool {
        self.buffer.lock().overflowed
    }

    pub fn peak_bytes(&self) -> usize {
        self.buffer.lock().peak
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub fn frames(&self) -> impl Iterator<Item = &FrameRecord> {
        self.frames.iter()
    }

    /// Seals the bytes encoded since the last frame as one frame. Returns
    /// the frame size, or discards everything if the encoding overflowed.
    pub fn begin_frame(&mut self, now: Duration) -> Result<usize, FrameOverflow> {
        let mut state = self.buffer.lock();
        if state.overflowed {
            state.reset();
            return Err(FrameOverflow);
        }
        let len = state.bytes.len() - state.flushed;
        if len == 0 {
            return Ok(0);
        }
        let record = FrameRecord {
            start_offset: self.bytes_enqueued,
            len: u64::try_from(len).unwrap_or(u64::MAX),
            written_before: self.bytes_written,
        };
        drop(state);
        self.bytes_enqueued = self.bytes_enqueued.saturating_add(record.len);
        if self.frames.len() == FRAME_LOG {
            self.frames.pop_front();
        }
        self.frames.push_back(record);
        self.frame_started_at = Some(now);
        Ok(len)
    }

    /// Drops an encoded but unsent frame (a failed draw).
    pub fn discard_unsent(&mut self) {
        let mut state = self.buffer.lock();
        if state.flushed == 0 {
            state.reset();
        }
    }

    /// Appends control bytes (terminal restore) using the reserved space.
    pub fn append_control(&mut self, bytes: &[u8]) -> bool {
        let mut state = self.buffer.lock();
        let capacity = state.capacity;
        let appended = !state.overflowed && state.append(bytes, capacity);
        drop(state);
        if appended {
            self.bytes_enqueued = self
                .bytes_enqueued
                .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            if self.frame_started_at.is_none() {
                self.frame_started_at = Some(Duration::ZERO);
            }
        }
        appended
    }

    /// Writes at most 8 KiB / 2 ms of the pending frame, in order.
    pub fn flush_step(&mut self, clock: &dyn LoopClock) -> io::Result<FlushProgress> {
        let started = clock.now();
        let mut written_now = 0usize;
        let mut state = self.buffer.lock();
        if state.bytes.len() == state.flushed {
            return Ok(FlushProgress::Idle);
        }
        loop {
            let pending = state.bytes.len() - state.flushed;
            if pending == 0 {
                break;
            }
            let over_time = clock.now().saturating_sub(started) >= FLUSH_TIME_PER_ITERATION;
            if written_now >= FLUSH_BYTES_PER_ITERATION || over_time {
                return Ok(FlushProgress::Partial);
            }
            let take = pending.min(FLUSH_BYTES_PER_ITERATION - written_now);
            let start = state.flushed;
            let chunk = &state.bytes[start..start + take];
            match self.transport.write_nonblocking(chunk) {
                Ok(0) => {
                    self.blocked = true;
                    return Ok(FlushProgress::Blocked);
                }
                Ok(accepted) => {
                    let accepted = accepted.min(take);
                    state.flushed += accepted;
                    written_now += accepted;
                    self.bytes_written = self
                        .bytes_written
                        .saturating_add(u64::try_from(accepted).unwrap_or(u64::MAX));
                    self.blocked = false;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.blocked = true;
                    return Ok(FlushProgress::Blocked);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        state.reset();
        drop(state);
        let elapsed = self
            .frame_started_at
            .take()
            .map_or(Duration::ZERO, |enqueued| {
                clock.now().saturating_sub(enqueued)
            });
        Ok(FlushProgress::Completed { elapsed })
    }

    /// Exit path: keeps writing (waiting for writability in short slices)
    /// until everything is out or `deadline` passes. Returns whether all
    /// pending bytes were written.
    pub fn drain_until(&mut self, deadline: Duration, clock: &dyn LoopClock) -> io::Result<bool> {
        for _ in 0..MAX_DRAIN_ATTEMPTS {
            if !self.has_pending() {
                return Ok(true);
            }
            let now = clock.now();
            if now >= deadline {
                return Ok(false);
            }
            if self.flush_step(clock)? == FlushProgress::Blocked {
                let wait = deadline.saturating_sub(now).min(DRAIN_WAIT_SLICE);
                self.transport.wait_writable(wait)?;
            }
        }
        Ok(!self.has_pending())
    }

    /// Restores the transport's original descriptor flags (idempotent).
    pub fn restore_transport(&mut self) -> io::Result<()> {
        self.transport.restore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct StillClock(Cell<Duration>);

    impl LoopClock for StillClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
    }

    #[derive(Default)]
    struct Recorder {
        accepted: Vec<u8>,
        per_call: Option<usize>,
        script: VecDeque<io::Result<usize>>,
    }

    struct RecordingTransport(Arc<Mutex<Recorder>>);

    impl TerminalTransport for RecordingTransport {
        fn write_nonblocking(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut recorder = self.0.lock().unwrap();
            if let Some(next) = recorder.script.pop_front() {
                let accepted = next?;
                let accepted = accepted.min(bytes.len());
                recorder.accepted.extend_from_slice(&bytes[..accepted]);
                return Ok(accepted);
            }
            let accepted = recorder
                .per_call
                .map_or(bytes.len(), |limit| limit.min(bytes.len()));
            recorder.accepted.extend_from_slice(&bytes[..accepted]);
            Ok(accepted)
        }

        fn wait_writable(&mut self, _timeout: Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn restore(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn pipeline(recorder: &Arc<Mutex<Recorder>>, capacity: usize) -> OutputPipeline {
        OutputPipeline::new(Box::new(RecordingTransport(Arc::clone(recorder))), capacity)
    }

    #[test]
    fn a_frame_is_written_in_order_within_the_per_step_budget() {
        let recorder = Arc::new(Mutex::new(Recorder::default()));
        let mut output = pipeline(&recorder, OUTPUT_BUFFER_CAPACITY);
        let frame: Vec<u8> = (0..20_000u32)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        output.writer().write_all(&frame).unwrap();
        assert_eq!(output.begin_frame(Duration::ZERO), Ok(20_000));
        let clock = StillClock(Cell::new(Duration::ZERO));
        assert_eq!(output.flush_step(&clock).unwrap(), FlushProgress::Partial);
        assert_eq!(
            recorder.lock().unwrap().accepted.len(),
            FLUSH_BYTES_PER_ITERATION
        );
        assert_eq!(output.flush_step(&clock).unwrap(), FlushProgress::Partial);
        assert!(matches!(
            output.flush_step(&clock).unwrap(),
            FlushProgress::Completed { .. }
        ));
        assert_eq!(recorder.lock().unwrap().accepted, frame);
        assert_eq!(output.flush_step(&clock).unwrap(), FlushProgress::Idle);
    }

    #[test]
    fn would_block_and_interrupts_are_handled_and_errors_surface() {
        let recorder = Arc::new(Mutex::new(Recorder::default()));
        recorder.lock().unwrap().script = VecDeque::from(vec![
            Err(io::Error::from(io::ErrorKind::Interrupted)),
            Ok(3),
            Err(io::Error::from(io::ErrorKind::WouldBlock)),
        ]);
        let mut output = pipeline(&recorder, OUTPUT_BUFFER_CAPACITY);
        output.writer().write_all(b"abcdef").unwrap();
        output.begin_frame(Duration::ZERO).unwrap();
        let clock = StillClock(Cell::new(Duration::ZERO));
        assert_eq!(output.flush_step(&clock).unwrap(), FlushProgress::Blocked);
        assert!(output.is_blocked());
        assert_eq!(output.pending_len(), 3);
        recorder.lock().unwrap().script =
            VecDeque::from(vec![Err(io::Error::from(io::ErrorKind::BrokenPipe))]);
        assert_eq!(
            output.flush_step(&clock).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(matches!(
            output.flush_step(&clock).unwrap(),
            FlushProgress::Completed { .. }
        ));
        assert_eq!(recorder.lock().unwrap().accepted, b"abcdef");
    }

    #[test]
    fn an_oversized_frame_is_discarded_unsent_and_the_buffer_stays_bounded() {
        let recorder = Arc::new(Mutex::new(Recorder::default()));
        let capacity = 16 * 1024;
        let mut output = pipeline(&recorder, capacity);
        let mut writer = output.writer();
        assert!(writer.write_all(&vec![b'x'; 20_000]).is_err());
        assert!(
            writer.write_all(b"y").is_err(),
            "a poisoned frame rejects later writes"
        );
        assert_eq!(output.begin_frame(Duration::ZERO), Err(FrameOverflow));
        assert!(!output.has_pending());
        assert!(output.peak_bytes() <= capacity);
        assert!(recorder.lock().unwrap().accepted.is_empty());
        writer.write_all(b"next frame").unwrap();
        assert_eq!(output.begin_frame(Duration::ZERO), Ok(10));
    }

    #[test]
    fn the_restore_tail_always_fits_after_a_full_frame() {
        let recorder = Arc::new(Mutex::new(Recorder::default()));
        let capacity = 16 * 1024;
        let mut output = pipeline(&recorder, capacity);
        output
            .writer()
            .write_all(&vec![b'f'; capacity - CONTROL_RESERVE])
            .unwrap();
        output.begin_frame(Duration::ZERO).unwrap();
        assert!(output.append_control(b"\x1b[?1049l"));
        let clock = StillClock(Cell::new(Duration::ZERO));
        assert!(
            output
                .drain_until(Duration::from_millis(250), &clock)
                .unwrap()
        );
        assert!(recorder.lock().unwrap().accepted.ends_with(b"\x1b[?1049l"));
    }

    #[test]
    fn frame_records_show_each_frame_started_after_the_previous_one_finished() {
        let recorder = Arc::new(Mutex::new(Recorder::default()));
        recorder.lock().unwrap().per_call = Some(7);
        let mut output = pipeline(&recorder, OUTPUT_BUFFER_CAPACITY);
        let clock = StillClock(Cell::new(Duration::ZERO));
        for frame in 0..3u8 {
            output.writer().write_all(&[frame; 100]).unwrap();
            output.begin_frame(Duration::ZERO).unwrap();
            while output.has_pending() {
                output.flush_step(&clock).unwrap();
            }
        }
        for record in output.frames() {
            assert_eq!(record.written_before, record.start_offset);
        }
        assert_eq!(output.bytes_written(), 300);
    }
}
