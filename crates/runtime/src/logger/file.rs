//! File logger with a dedicated writer thread.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::InfraError;
use dgxtop_core::ports::{LogLevel, Logger};
use dgxtop_core::text::sanitize_message;

use super::coalesce::{Coalescer, Summary, Verdict};
use super::format::{format_utc, level_name};
use super::{COALESCE_WINDOW_NS, MAX_QUEUE_BYTES};

/// Smallest accepted queue.
const MIN_QUEUE_BYTES: usize = 4096;
/// Bookkeeping charged per record on top of its text.
const RECORD_OVERHEAD_BYTES: u64 = 64;
/// How often the writer emits due coalescing summaries when idle.
const SUMMARY_TICK: Duration = Duration::from_secs(1);
/// Distinct coalescing windows tracked at once.
const COALESCE_CAPACITY: usize = 256;
/// Upper bound on rotated files kept.
const MAX_KEEP_FILES: u32 = 16;

/// Counters exposed for diagnostics (`log_dropped_total`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoggerStats {
    pub accepted: u64,
    pub dropped_error: u64,
    pub dropped_warn: u64,
    pub dropped_info: u64,
    pub dropped_debug: u64,
    pub written_lines: u64,
    pub coalesced: u64,
    pub rotations: u64,
    pub write_errors: u64,
    pub queued_bytes: u64,
}

#[cfg(test)]
impl LoggerStats {
    /// Dropped debug/info records.
    pub const fn dropped_low_priority(&self) -> u64 {
        self.dropped_info + self.dropped_debug
    }

    /// Dropped warn/error records (counted separately on purpose).
    pub const fn dropped_high_priority(&self) -> u64 {
        self.dropped_error + self.dropped_warn
    }
}

#[derive(Default)]
struct Counters {
    queued_bytes: AtomicU64,
    accepted: AtomicU64,
    dropped: [AtomicU64; 4],
    written_lines: AtomicU64,
    coalesced: AtomicU64,
    rotations: AtomicU64,
    write_errors: AtomicU64,
}

const fn level_slot(level: LogLevel) -> usize {
    match level {
        LogLevel::Error => 0,
        LogLevel::Warn => 1,
        LogLevel::Info => 2,
        LogLevel::Debug => 3,
    }
}

impl Counters {
    /// Byte admission: debug/info may use 3/4 of the queue, warn/error all
    /// of it, so pressure drops low-priority records first.
    fn try_admit(&self, level: LogLevel, cost: u64, limit: u64) -> bool {
        let allowance = match level {
            LogLevel::Error | LogLevel::Warn => limit,
            LogLevel::Info | LogLevel::Debug => limit / 4 * 3,
        };
        let admitted = self
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued.checked_add(cost).filter(|next| *next <= allowance)
            })
            .is_ok();
        if !admitted {
            self.dropped[level_slot(level)].fetch_add(1, Ordering::Relaxed);
        }
        admitted
    }

    fn release(&self, cost: u64) {
        let _ = self
            .queued_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                Some(queued.saturating_sub(cost))
            });
    }

    fn snapshot(&self) -> LoggerStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        LoggerStats {
            accepted: load(&self.accepted),
            dropped_error: load(&self.dropped[0]),
            dropped_warn: load(&self.dropped[1]),
            dropped_info: load(&self.dropped[2]),
            dropped_debug: load(&self.dropped[3]),
            written_lines: load(&self.written_lines),
            coalesced: load(&self.coalesced),
            rotations: load(&self.rotations),
            write_errors: load(&self.write_errors),
            queued_bytes: self.queued_bytes.load(Ordering::Acquire),
        }
    }
}

struct Record {
    level: LogLevel,
    target: &'static str,
    message: String,
    boot_ns: u64,
    utc_ns: i128,
    cost: u64,
}

enum Command {
    Record(Record),
    Flush(Sender<()>),
    Stop,
}

/// Non-blocking file logger (see the module documentation).
pub struct FileLogger {
    commands: Sender<Command>,
    counters: Arc<Counters>,
    queue_limit: u64,
    clock: Arc<dyn TimeProvider>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for FileLogger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileLogger")
            .field("stats", &self.stats())
            .finish()
    }
}

impl FileLogger {
    /// Creates the `0700` directory and `0600` file (no symlinks), then
    /// starts the writer thread. `queue_bytes` is clamped to 4 KiB..=1 MiB;
    /// `keep_files` counts the active file plus rotated ones.
    pub fn start(
        path: &Path,
        queue_bytes: usize,
        rotate_bytes: u64,
        keep_files: u32,
        clock: Arc<dyn TimeProvider>,
    ) -> Result<Self, InfraError> {
        Self::start_with_gate(path, queue_bytes, rotate_bytes, keep_files, clock, None)
    }

    fn start_with_gate(
        path: &Path,
        queue_bytes: usize,
        rotate_bytes: u64,
        keep_files: u32,
        clock: Arc<dyn TimeProvider>,
        start_gate: Option<Receiver<()>>,
    ) -> Result<Self, InfraError> {
        let output = RotatingFile::open(
            path,
            rotate_bytes.max(1),
            keep_files.clamp(1, MAX_KEEP_FILES),
        )?;
        let queue_limit =
            u64::try_from(queue_bytes.clamp(MIN_QUEUE_BYTES, MAX_QUEUE_BYTES)).unwrap_or(u64::MAX);
        let item_capacity = usize::try_from(queue_limit / RECORD_OVERHEAD_BYTES).unwrap_or(1024);
        let (commands, receiver) = bounded::<Command>(item_capacity.max(16));
        let counters = Arc::new(Counters::default());
        let writer = Writer {
            output,
            commands: receiver,
            counters: Arc::clone(&counters),
            coalescer: Coalescer::new(COALESCE_WINDOW_NS, COALESCE_CAPACITY),
            clock: Arc::clone(&clock),
        };
        let worker = std::thread::Builder::new()
            .name("dgxtop-log".into())
            .spawn(move || {
                if let Some(gate) = start_gate {
                    let _ = gate.recv();
                }
                writer.run();
            })
            .map_err(|error| InfraError::from_io("logger.spawn", &error))?;
        Ok(Self {
            commands,
            counters,
            queue_limit,
            clock,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn stats(&self) -> LoggerStats {
        self.counters.snapshot()
    }

    /// Waits up to `timeout` until everything queued so far is written.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (acknowledge, acknowledged) = bounded::<()>(1);
        if self.commands.try_send(Command::Flush(acknowledge)).is_err() {
            return false;
        }
        acknowledged.recv_timeout(timeout).is_ok()
    }

    /// Writes pending summaries and stops the writer, waiting at most
    /// `timeout`. Idempotent; later records are dropped and counted.
    pub fn stop(&self, timeout: Duration) -> bool {
        let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return true;
        };
        let deadline = std::time::Instant::now() + timeout;
        // A full queue is drained by the writer; keep offering the stop
        // command until it fits or the deadline passes.
        while let Err(TrySendError::Full(_)) = self.commands.try_send(Command::Stop) {
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        if worker.is_finished() {
            let _ = worker.join();
            true
        } else {
            false
        }
    }
}

impl Logger for FileLogger {
    fn log(&self, level: LogLevel, target: &'static str, message: &str) {
        let message = sanitize_message(message);
        let text_bytes = u64::try_from(message.len() + target.len()).unwrap_or(u64::MAX);
        let cost = text_bytes.saturating_add(RECORD_OVERHEAD_BYTES);
        if !self.counters.try_admit(level, cost, self.queue_limit) {
            return;
        }
        let record = Record {
            level,
            target,
            message,
            boot_ns: self.clock.now_boot_ns(),
            utc_ns: self.clock.now_utc_ns(),
            cost,
        };
        match self.commands.try_send(Command::Record(record)) {
            Ok(()) => {
                self.counters.accepted.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.counters.release(cost);
                self.counters.dropped[level_slot(level)].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for FileLogger {
    fn drop(&mut self) {
        self.stop(Duration::from_millis(500));
    }
}

struct Writer {
    output: RotatingFile,
    commands: Receiver<Command>,
    counters: Arc<Counters>,
    coalescer: Coalescer,
    clock: Arc<dyn TimeProvider>,
}

impl Writer {
    fn run(mut self) {
        loop {
            match self.commands.recv_timeout(SUMMARY_TICK) {
                Ok(Command::Record(record)) => {
                    self.counters.release(record.cost);
                    self.write_record(&record);
                }
                Ok(Command::Flush(acknowledge)) => {
                    self.write_due_summaries();
                    self.output.flush(&self.counters);
                    let _ = acknowledge.send(());
                }
                Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    let pending = self.coalescer.drain();
                    self.write_summaries(&pending);
                    self.output.flush(&self.counters);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => self.write_due_summaries(),
            }
        }
    }

    fn write_record(&mut self, record: &Record) {
        let (summaries, verdict) =
            self.coalescer
                .offer(record.level, record.target, &record.message, record.boot_ns);
        self.write_summaries(&summaries);
        match verdict {
            Verdict::Write => {
                let line = format!(
                    "{} boot_ns={} {} {}: {}\n",
                    format_utc(record.utc_ns),
                    record.boot_ns,
                    level_name(record.level),
                    record.target,
                    record.message
                );
                self.output.write_line(line.as_bytes(), &self.counters);
            }
            Verdict::Suppress => {
                self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn write_due_summaries(&mut self) {
        let due = self.coalescer.due(self.clock.now_boot_ns());
        self.write_summaries(&due);
    }

    fn write_summaries(&mut self, summaries: &[Summary]) {
        if summaries.is_empty() {
            return;
        }
        let now_boot_ns = self.clock.now_boot_ns();
        let stamp = format_utc(self.clock.now_utc_ns());
        for summary in summaries {
            let line = format!(
                "{stamp} boot_ns={now_boot_ns} {} {}: {} [repeated {} more times within 30 s]\n",
                level_name(summary.level),
                summary.target,
                summary.message,
                summary.suppressed
            );
            self.output.write_line(line.as_bytes(), &self.counters);
        }
    }
}

/// Size-rotated append-only file (`path`, `path.1` … `path.{keep-1}`).
struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    rotate_bytes: u64,
    keep_files: u32,
}

impl RotatingFile {
    fn open(path: &Path, rotate_bytes: u64, keep_files: u32) -> Result<Self, InfraError> {
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| InfraError::Unavailable("log path has no directory".into()))?;
        prepare_directory(directory)?;
        let file = open_private_file(path)?;
        let size = file
            .metadata()
            .map_err(|error| InfraError::from_io("logger.open", &error))?
            .len();
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            size,
            rotate_bytes,
            keep_files,
        })
    }

    fn write_line(&mut self, line: &[u8], counters: &Counters) {
        let length = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if self.size > 0
            && self.size.saturating_add(length) > self.rotate_bytes
            && self.rotate(counters).is_err()
        {
            counters.write_errors.fetch_add(1, Ordering::Relaxed);
        }
        let Some(file) = self.file.as_mut() else {
            counters.write_errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match file.write_all(line) {
            Ok(()) => {
                self.size = self.size.saturating_add(length);
                counters.written_lines.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                counters.write_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn rotate(&mut self, counters: &Counters) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }
        if self.keep_files <= 1 {
            remove_if_present(&self.path)?;
        } else {
            for index in (1..self.keep_files - 1).rev() {
                rename_if_present(&self.rotated(index), &self.rotated(index + 1))?;
            }
            rename_if_present(&self.path, &self.rotated(1))?;
        }
        let file = open_private_file(&self.path).map_err(io::Error::other)?;
        self.file = Some(file);
        self.size = 0;
        counters.rotations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn rotated(&self, index: u32) -> PathBuf {
        let mut name: OsString = self.path.as_os_str().to_owned();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    fn flush(&mut self, counters: &Counters) {
        if let Some(file) = self.file.as_mut()
            && file.flush().is_err()
        {
            counters.write_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn rename_if_present(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `0700`, owned by us, not a symlink.
fn prepare_directory(directory: &Path) -> Result<(), InfraError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|error| InfraError::from_io("logger.create_dir", &error))?;
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| InfraError::from_io("logger.stat_dir", &error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(InfraError::Unavailable(
            "log directory is a symlink or not a directory".into(),
        ));
    }
    if metadata.uid() != crate::clock::current_euid() {
        return Err(InfraError::Unavailable(
            "log directory is owned by another user".into(),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|error| InfraError::from_io("logger.chmod_dir", &error))?;
    }
    Ok(())
}

/// Append-only `0600` file opened without following symlinks.
fn open_private_file(path: &Path) -> Result<File, InfraError> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| InfraError::from_io("logger.open", &error))?;
    let metadata = file
        .metadata()
        .map_err(|error| InfraError::from_io("logger.stat", &error))?;
    if !metadata.is_file() || metadata.uid() != crate::clock::current_euid() {
        return Err(InfraError::Unavailable(
            "log file is not a regular file owned by this user".into(),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| InfraError::from_io("logger.chmod", &error))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::FakeClock;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dgxtop-runtime-log-{label}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn writes_sanitized_lines_to_a_private_file() {
        let temp = TempDir::new("basic");
        let path = temp.0.join("state").join("dgxtop.log");
        let clock = FakeClock::new(5_000);
        let logger = FileLogger::start(&path, 64 * 1024, 1 << 20, 3, clock).unwrap();
        logger.log(LogLevel::Info, "test", "hello\u{1b}[31m world\u{7}");
        logger.log(LogLevel::Error, "test", "broken\nline");
        assert!(logger.flush(Duration::from_secs(5)));
        let text = read(&path);
        assert!(
            text.contains("boot_ns=5000 INFO test: hello world\n"),
            "{text}"
        );
        assert!(text.contains("ERROR test: broken line\n"));
        assert!(!text.contains('\u{1b}'));
        let file_mode = fs::metadata(&path).unwrap().mode() & 0o777;
        let dir_mode = fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
        assert!(logger.stop(Duration::from_secs(5)));
        assert!(logger.stop(Duration::from_secs(1)), "stop is idempotent");
        logger.log(LogLevel::Error, "test", "after stop");
        assert_eq!(logger.stats().dropped_error, 1);
    }

    #[test]
    fn pressure_drops_low_priority_first_and_counts_errors_separately() {
        let temp = TempDir::new("drops");
        let path = temp.0.join("dgxtop.log");
        let (release, gate) = bounded::<()>(1);
        let logger = FileLogger::start_with_gate(
            &path,
            MIN_QUEUE_BYTES,
            1 << 20,
            3,
            FakeClock::new(0),
            Some(gate),
        )
        .unwrap();
        let message = "m".repeat(100);
        // The writer is paused: info stops at 3/4 of the queue.
        for index in 0..100 {
            logger.log(LogLevel::Info, "flood", &format!("{index}{message}"));
        }
        let after_info = logger.stats();
        assert!(after_info.dropped_info > 0);
        assert!(after_info.queued_bytes <= u64::try_from(MIN_QUEUE_BYTES).unwrap() / 4 * 3);
        // Errors still use the reserved last quarter ...
        logger.log(LogLevel::Error, "flood", "first error");
        assert_eq!(logger.stats().dropped_error, 0);
        // ... until the whole queue is full, then they are counted apart.
        for index in 0..100 {
            logger.log(LogLevel::Error, "flood", &format!("e{index}{message}"));
        }
        let stats = logger.stats();
        assert!(stats.dropped_error > 0);
        assert!(stats.queued_bytes <= u64::try_from(MIN_QUEUE_BYTES).unwrap());
        assert_eq!(stats.dropped_low_priority(), stats.dropped_info);
        assert_eq!(stats.dropped_high_priority(), stats.dropped_error);
        release.send(()).unwrap();
        assert!(logger.flush(Duration::from_secs(5)));
        let final_stats = logger.stats();
        assert_eq!(final_stats.queued_bytes, 0);
        assert_eq!(final_stats.written_lines, final_stats.accepted);
    }

    #[test]
    fn files_rotate_by_size_and_keep_n_files() {
        let temp = TempDir::new("rotate");
        let path = temp.0.join("dgxtop.log");
        let clock = FakeClock::new(0);
        let logger = FileLogger::start(&path, 64 * 1024, 400, 3, clock.clone()).unwrap();
        for index in 0..60 {
            clock.advance_ms(1);
            logger.log(LogLevel::Info, "rotate", &format!("line number {index}"));
        }
        assert!(logger.flush(Duration::from_secs(5)));
        assert!(logger.stats().rotations >= 2);
        let rotated = |index: u32| PathBuf::from(format!("{}.{index}", path.display()));
        assert!(path.exists());
        assert!(rotated(1).exists());
        assert!(rotated(2).exists());
        assert!(!rotated(3).exists());
        for file in [path.clone(), rotated(1), rotated(2)] {
            assert!(fs::metadata(&file).unwrap().len() <= 400);
            assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        }
        assert!(read(&path).contains("line number 59"));
    }

    #[test]
    fn symlinked_paths_are_refused() {
        let temp = TempDir::new("symlink");
        fs::create_dir_all(&temp.0).unwrap();
        let target = temp.0.join("target.log");
        fs::write(&target, "").unwrap();
        let link = temp.0.join("dgxtop.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(FileLogger::start(&link, 64 * 1024, 1 << 20, 3, FakeClock::new(0)).is_err());
        let real_dir = temp.0.join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let dir_link = temp.0.join("linked");
        std::os::unix::fs::symlink(&real_dir, &dir_link).unwrap();
        assert!(
            FileLogger::start(
                &dir_link.join("x.log"),
                64 * 1024,
                1 << 20,
                3,
                FakeClock::new(0)
            )
            .is_err()
        );
    }

    #[test]
    fn broad_directory_permissions_are_tightened() {
        let temp = TempDir::new("perms");
        fs::create_dir_all(&temp.0).unwrap();
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o755)).unwrap();
        let path = temp.0.join("dgxtop.log");
        let logger = FileLogger::start(&path, 64 * 1024, 1 << 20, 3, FakeClock::new(0)).unwrap();
        assert_eq!(fs::metadata(&temp.0).unwrap().mode() & 0o777, 0o700);
        drop(logger);
    }
}
