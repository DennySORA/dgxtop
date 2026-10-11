//! Fixed worker pools and single-owner actors behind bounded channels.
//!
//! Threads are created once at spawn time and never per job (`RUNTIME.md`
//! §1). Submission is `try_send`: a full queue is an immediate
//! [`DispatchError::Busy`], never a blocked caller. Every job runs inside
//! `catch_unwind`, so one panicking job is reported and the worker keeps
//! serving. [`PoolHandle::stop`] is bounded by its timeout: workers that are
//! still busy are detached and counted, never waited for indefinitely.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{
    Receiver, SendTimeoutError, Sender, TryRecvError, TrySendError, bounded, select,
};
use dgxtop_core::errors::InfraError;
use dgxtop_core::ports::{Dispatch, DispatchError};
use dgxtop_core::text::sanitize_message;

/// Maximum workers per pool (the spec's largest pool has 4).
pub const MAX_POOL_THREADS: usize = 16;
/// Poll step while waiting for workers to finish in [`PoolHandle::stop`].
const STOP_POLL: Duration = Duration::from_millis(2);

/// Callback invoked with (pool name, sanitized panic message).
pub type PanicHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Optional pool behaviour.
#[derive(Clone, Default)]
pub struct PoolOptions {
    pub on_panic: Option<PanicHook>,
}

impl std::fmt::Debug for PoolOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PoolOptions")
            .field("on_panic", &self.on_panic.is_some())
            .finish()
    }
}

/// Counters of one pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub completed: u64,
    pub panicked: u64,
    pub rejected_busy: u64,
}

/// Outcome of [`PoolHandle::stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopReport {
    /// Workers that finished and were joined.
    pub joined: usize,
    /// Workers still running a job at the timeout (left detached).
    pub detached: usize,
    /// Jobs still queued when the pool stopped (dropped, not run).
    pub discarded: usize,
}

#[derive(Default)]
struct Counters {
    completed: AtomicU64,
    panicked: AtomicU64,
    rejected_busy: AtomicU64,
}

struct PoolShared<J> {
    name: String,
    jobs: Sender<J>,
    pending: Receiver<J>,
    stop_signal: Mutex<Option<Sender<()>>>,
    stopped: AtomicBool,
    workers: Mutex<Vec<JoinHandle<()>>>,
    counters: Arc<Counters>,
}

/// Cloneable submission handle of a pool or actor.
pub struct PoolHandle<J> {
    shared: Arc<PoolShared<J>>,
}

impl<J> Clone for PoolHandle<J> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<J> std::fmt::Debug for PoolHandle<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PoolHandle")
            .field("name", &self.shared.name)
            .field("queued", &self.shared.jobs.len())
            .field("stopped", &self.shared.stopped.load(Ordering::Acquire))
            .finish()
    }
}

impl<J: Send> Dispatch<J> for PoolHandle<J> {
    fn try_dispatch(&self, job: J) -> Result<(), DispatchError> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(DispatchError::Closed);
        }
        match self.shared.jobs.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.shared
                    .counters
                    .rejected_busy
                    .fetch_add(1, Ordering::Relaxed);
                Err(DispatchError::Busy)
            }
            Err(TrySendError::Disconnected(_)) => Err(DispatchError::Closed),
        }
    }
}

impl<J: Send> PoolHandle<J> {
    /// Like [`Dispatch::try_dispatch`], but waits up to `timeout` for room:
    /// for a worker handing its result to an actor that never waits on it,
    /// so a momentarily full queue does not lose the result. Never for the
    /// Manager, which must not block.
    pub fn dispatch_within(&self, job: J, timeout: Duration) -> Result<(), DispatchError> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(DispatchError::Closed);
        }
        match self.shared.jobs.send_timeout(job, timeout) {
            Ok(()) => Ok(()),
            Err(SendTimeoutError::Timeout(_)) => {
                self.shared
                    .counters
                    .rejected_busy
                    .fetch_add(1, Ordering::Relaxed);
                Err(DispatchError::Busy)
            }
            Err(SendTimeoutError::Disconnected(_)) => Err(DispatchError::Closed),
        }
    }
}

impl<J> PoolHandle<J> {
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Jobs waiting in the queue.
    pub fn queued(&self) -> usize {
        self.shared.jobs.len()
    }

    pub fn is_stopped(&self) -> bool {
        self.shared.stopped.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> PoolStats {
        let counters = &self.shared.counters;
        PoolStats {
            completed: counters.completed.load(Ordering::Relaxed),
            panicked: counters.panicked.load(Ordering::Relaxed),
            rejected_busy: counters.rejected_busy.load(Ordering::Relaxed),
        }
    }

    /// Stops accepting jobs, discards queued ones and waits at most
    /// `timeout` for running jobs to finish. Idempotent.
    pub fn stop(&self, timeout: Duration) -> StopReport {
        self.shared.stopped.store(true, Ordering::Release);
        // Dropping the only stop sender disconnects every worker's stop arm.
        drop(
            self.shared
                .stop_signal
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let discarded = drain(&self.shared.pending);
        let deadline = Instant::now() + timeout;
        let mut workers = self
            .shared
            .workers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while workers.iter().any(|worker| !worker.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(STOP_POLL);
        }
        let mut joined = 0;
        let mut detached = 0;
        for worker in workers.drain(..) {
            if worker.is_finished() {
                // A finished worker's join cannot block; its panics were
                // already caught per job.
                let _ = worker.join();
                joined += 1;
            } else {
                detached += 1;
            }
        }
        StopReport {
            joined,
            detached,
            discarded,
        }
    }
}

fn drain<J>(receiver: &Receiver<J>) -> usize {
    let mut discarded = 0;
    loop {
        match receiver.try_recv() {
            Ok(_) => discarded += 1,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return discarded,
        }
    }
}

/// Spawns `threads` fixed workers sharing one bounded queue of `capacity`.
pub fn spawn_pool<J: Send + 'static>(
    name: &str,
    threads: usize,
    capacity: usize,
    handler: Arc<dyn Fn(J) + Send + Sync>,
) -> Result<PoolHandle<J>, InfraError> {
    spawn_pool_with(name, threads, capacity, handler, PoolOptions::default())
}

/// [`spawn_pool`] with options (panic reporting).
pub fn spawn_pool_with<J: Send + 'static>(
    name: &str,
    threads: usize,
    capacity: usize,
    handler: Arc<dyn Fn(J) + Send + Sync>,
    options: PoolOptions,
) -> Result<PoolHandle<J>, InfraError> {
    let threads = threads.clamp(1, MAX_POOL_THREADS);
    let workers = (0..threads)
        .map(|_| {
            let handler = Arc::clone(&handler);
            move |job: J| handler(job)
        })
        .collect();
    spawn_workers(name, capacity, options, workers)
}

/// Spawns one thread that owns `state` and applies `handler` to each job.
pub fn spawn_actor<S, J, H>(
    name: &str,
    capacity: usize,
    state: S,
    handler: H,
) -> Result<PoolHandle<J>, InfraError>
where
    S: Send + 'static,
    J: Send + 'static,
    H: FnMut(&mut S, J) + Send + 'static,
{
    spawn_actor_with(name, capacity, state, handler, PoolOptions::default())
}

/// [`spawn_actor`] with options. After a panicking job the state is kept as
/// the handler left it; handlers must keep it consistent across unwinding.
pub fn spawn_actor_with<S, J, H>(
    name: &str,
    capacity: usize,
    state: S,
    handler: H,
    options: PoolOptions,
) -> Result<PoolHandle<J>, InfraError>
where
    S: Send + 'static,
    J: Send + 'static,
    H: FnMut(&mut S, J) + Send + 'static,
{
    let mut state = state;
    let mut handler = handler;
    let worker = move |job: J| handler(&mut state, job);
    spawn_workers(name, capacity, options, vec![worker])
}

fn spawn_workers<J, W>(
    name: &str,
    capacity: usize,
    options: PoolOptions,
    workers: Vec<W>,
) -> Result<PoolHandle<J>, InfraError>
where
    J: Send + 'static,
    W: FnMut(J) + Send + 'static,
{
    let threads = workers.len();
    let (jobs, pending) = bounded::<J>(capacity.max(1));
    let (stop_sender, stop_receiver) = bounded::<()>(0);
    let counters = Arc::new(Counters::default());
    let shared = Arc::new(PoolShared {
        name: name.to_owned(),
        jobs,
        pending: pending.clone(),
        stop_signal: Mutex::new(Some(stop_sender)),
        stopped: AtomicBool::new(false),
        workers: Mutex::new(Vec::with_capacity(threads)),
        counters: Arc::clone(&counters),
    });
    let handle = PoolHandle { shared };
    for (index, worker) in workers.into_iter().enumerate() {
        let context = WorkerContext {
            pool: name.to_owned(),
            jobs: pending.clone(),
            stop: stop_receiver.clone(),
            counters: Arc::clone(&counters),
            on_panic: options.on_panic.clone(),
        };
        let spawned = std::thread::Builder::new()
            .name(format!("{name}-{index}"))
            .spawn(move || context.run(worker));
        match spawned {
            Ok(join) => handle
                .shared
                .workers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(join),
            Err(error) => {
                handle.stop(Duration::from_millis(100));
                return Err(InfraError::from_io("queues.spawn_worker", &error));
            }
        }
    }
    Ok(handle)
}

struct WorkerContext<J> {
    pool: String,
    jobs: Receiver<J>,
    stop: Receiver<()>,
    counters: Arc<Counters>,
    on_panic: Option<PanicHook>,
}

impl<J> WorkerContext<J> {
    fn run<W: FnMut(J)>(self, mut worker: W) {
        loop {
            select! {
                recv(self.stop) -> _ => return,
                recv(self.jobs) -> job => match job {
                    Ok(job) => self.execute(&mut worker, job),
                    Err(_) => return,
                },
            }
        }
    }

    fn execute<W: FnMut(J)>(&self, worker: &mut W, job: J) {
        match catch_unwind(AssertUnwindSafe(|| worker(job))) {
            Ok(()) => {
                self.counters.completed.fetch_add(1, Ordering::Relaxed);
            }
            Err(panic) => {
                self.counters.panicked.fetch_add(1, Ordering::Relaxed);
                if let Some(hook) = &self.on_panic {
                    hook(&self.pool, &panic_message(panic.as_ref()));
                }
            }
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    let text = panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());
    sanitize_message(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Condvar;

    /// A latch the test opens to release blocked jobs.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.changed.wait(open).unwrap();
            }
        }

        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not reached in time");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn busy_when_the_queue_is_full_and_never_blocks() {
        let gate = Arc::new(Gate::default());
        let started = Arc::new(AtomicU64::new(0));
        let pool = {
            let gate = Arc::clone(&gate);
            let started = Arc::clone(&started);
            spawn_pool(
                "busy",
                1,
                2,
                Arc::new(move |_: u32| {
                    started.fetch_add(1, Ordering::SeqCst);
                    gate.wait();
                }),
            )
            .unwrap()
        };
        pool.try_dispatch(1).unwrap();
        wait_until(|| started.load(Ordering::SeqCst) == 1);
        pool.try_dispatch(2).unwrap();
        pool.try_dispatch(3).unwrap();
        let begin = Instant::now();
        assert_eq!(pool.try_dispatch(4), Err(DispatchError::Busy));
        assert!(begin.elapsed() < Duration::from_millis(50));
        assert_eq!(pool.stats().rejected_busy, 1);
        gate.release();
        wait_until(|| pool.stats().completed == 3);
        let report = pool.stop(Duration::from_secs(1));
        assert_eq!(report.joined, 1);
        assert_eq!(pool.try_dispatch(5), Err(DispatchError::Closed));
    }

    /// A worker handing its result to an actor waits a bounded time for
    /// room, then gives up as busy; a stopped actor refuses at once.
    #[test]
    fn a_bounded_wait_gets_room_or_gives_up() {
        let gate = Arc::new(Gate::default());
        let started = Arc::new(AtomicU64::new(0));
        let actor = {
            let gate = Arc::clone(&gate);
            let started = Arc::clone(&started);
            spawn_actor("wait", 1, (), move |(): &mut (), _: u32| {
                started.fetch_add(1, Ordering::SeqCst);
                gate.wait();
            })
            .unwrap()
        };
        actor.try_dispatch(1).unwrap();
        wait_until(|| started.load(Ordering::SeqCst) == 1);
        actor.dispatch_within(2, Duration::ZERO).unwrap();
        let begin = Instant::now();
        assert_eq!(
            actor.dispatch_within(3, Duration::from_millis(30)),
            Err(DispatchError::Busy)
        );
        assert!(begin.elapsed() >= Duration::from_millis(30));
        assert_eq!(actor.stats().rejected_busy, 1);
        let releaser = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                gate.release();
            })
        };
        actor
            .dispatch_within(4, Duration::from_secs(5))
            .expect("room once the job finished");
        releaser.join().unwrap();
        wait_until(|| actor.stats().completed == 3);
        actor.stop(Duration::from_secs(1));
        assert_eq!(
            actor.dispatch_within(5, Duration::from_secs(1)),
            Err(DispatchError::Closed)
        );
    }

    #[test]
    fn threads_are_fixed_and_reused() {
        let seen = Arc::new(Mutex::new(HashSet::new()));
        let pool = {
            let seen = Arc::clone(&seen);
            spawn_pool(
                "fixed",
                3,
                64,
                Arc::new(move |_: u32| {
                    seen.lock().unwrap().insert(std::thread::current().id());
                }),
            )
            .unwrap()
        };
        let mut submitted = 0;
        while submitted < 1_000 {
            if pool.try_dispatch(submitted).is_ok() {
                submitted += 1;
            } else {
                std::thread::yield_now();
            }
        }
        wait_until(|| pool.stats().completed == 1_000);
        assert!(seen.lock().unwrap().len() <= 3);
        assert_eq!(pool.stop(Duration::from_secs(1)).joined, 3);
    }

    #[test]
    fn panics_are_isolated_and_reported() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let hook: PanicHook = {
            let reports = Arc::clone(&reports);
            Arc::new(move |pool: &str, message: &str| {
                reports.lock().unwrap().push(format!("{pool}: {message}"));
            })
        };
        let pool = spawn_pool_with(
            "panicky",
            1,
            8,
            Arc::new(|job: u32| {
                assert!(job != 2, "job two\u{1b}[31m exploded");
            }),
            PoolOptions {
                on_panic: Some(hook),
            },
        )
        .unwrap();
        for job in 1..=3 {
            pool.try_dispatch(job).unwrap();
        }
        wait_until(|| pool.stats().completed + pool.stats().panicked == 3);
        assert_eq!(pool.stats().panicked, 1);
        assert_eq!(pool.stats().completed, 2);
        let reports = reports.lock().unwrap().clone();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].starts_with("panicky: job two exploded"));
        pool.stop(Duration::from_secs(1));
    }

    #[test]
    fn stop_never_hangs_on_a_stuck_job() {
        let gate = Arc::new(Gate::default());
        let started = Arc::new(AtomicU64::new(0));
        let pool = {
            let gate = Arc::clone(&gate);
            let started = Arc::clone(&started);
            spawn_pool(
                "stuck",
                1,
                4,
                Arc::new(move |_: u32| {
                    started.fetch_add(1, Ordering::SeqCst);
                    gate.wait();
                }),
            )
            .unwrap()
        };
        pool.try_dispatch(1).unwrap();
        wait_until(|| started.load(Ordering::SeqCst) == 1);
        pool.try_dispatch(2).unwrap();
        let begin = Instant::now();
        let report = pool.stop(Duration::from_millis(50));
        assert!(begin.elapsed() < Duration::from_millis(500));
        assert_eq!(
            report,
            StopReport {
                joined: 0,
                detached: 1,
                discarded: 1
            }
        );
        gate.release();
        // A second stop is harmless.
        assert_eq!(pool.stop(Duration::from_millis(10)).joined, 0);
    }

    #[test]
    fn actor_owns_its_state_and_survives_panics() {
        let (results, collected) = crossbeam_channel::bounded::<u64>(16);
        let actor = spawn_actor("actor", 8, 0u64, move |total: &mut u64, job: u64| {
            assert!(job != 99, "poison job");
            *total += job;
            results.send(*total).unwrap();
        })
        .unwrap();
        actor.try_dispatch(1).unwrap();
        actor.try_dispatch(99).unwrap();
        actor.try_dispatch(2).unwrap();
        let first = collected.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = collected.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((first, second), (1, 3));
        wait_until(|| actor.stats().panicked == 1);
        assert_eq!(actor.stop(Duration::from_secs(1)).joined, 1);
    }
}
