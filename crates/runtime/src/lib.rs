//! # dgxtop-runtime
//!
//! Execution mechanics of the dgxtop v2 rewrite (`spec/dgxtop-v2/RUNTIME.md`,
//! `ARCHITECTURE.md` §10, `contracts/INTERFACES.md` §7/§10, `SECURITY.md` §4).
//! This crate is mechanism, not policy: the Manager decides *what* to sample
//! and *when*; the runtime decides *how* work crosses threads and processes
//! without unbounded queues, unbounded allocation or blocking the caller.
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`clock`] | `CLOCK_BOOTTIME` time provider, boot/host identity helpers |
//! | [`ipc`] | bounded 4-byte big-endian length + UTF-8 JSON framing and decoding |
//! | [`host`] | adapter-host (child side) request/response loop |
//! | [`supervisor`] | parent side: one thread multiplexing every host pipe, deadlines, restart, circuit breaker, quarantine |
//! | [`broker`] | de-duplication of identical concurrent read requests |
//! | [`queues`] | fixed worker pools and single-owner actors behind bounded channels |
//! | [`terminal`] | non-blocking terminal output transport |
//! | [`logger`] | bounded, rate-limited file logger and adapter-host stderr logger |
//! | [`signals`] | async-signal-safe termination flag |
//!
//! Every channel is bounded by item count *and* bytes (`dgxtop_core::budget`);
//! timeouts never pretend to cancel blocking calls; failures surface as typed
//! results instead of fake zeros. `unsafe` is denied crate-wide and allowed
//! only in narrowly scoped OS functions (signal handler installation and
//! borrowing a caller-provided terminal descriptor), each with a `SAFETY`
//! justification.

pub mod broker;
pub mod clock;
pub mod host;
pub mod ipc;
pub mod logger;
mod pipe;
pub mod queues;
pub mod signals;
pub mod supervisor;
pub mod terminal;
pub mod timezone;

pub use broker::{Admission, BrokerKey, ReadBroker};
pub use clock::{
    BootClock, current_euid, host_identity, host_name, kernel_release, pid_namespace_of,
};
pub use host::{
    HOST_GENERATION_ARG, HOST_NONCE_ENV, HostExit, host_nonce_from_env, run_adapter_host,
};
pub use ipc::{FrameDecoder, IpcError, IpcLimits, decode_bounded, encode_frame};
pub use logger::FileLogger;
pub use queues::{PoolHandle, spawn_actor, spawn_pool};
pub use signals::install_termination_flag;
pub use supervisor::{
    HostLaunch, HostSpec, ShutdownReport, Supervisor, SupervisorConfig, SupervisorHandle,
};
pub use terminal::{NonBlockingTerminal, TtyInputGate};
