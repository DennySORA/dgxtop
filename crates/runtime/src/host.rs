//! Adapter host: the child side of the IPC protocol.
//!
//! The host owns exactly one `TelemetryPort` and serves one request at a
//! time. It first writes a hello (`protocol_version`, `adapter_id`,
//! `generation`, the parent's nonce and the manifest), then answers
//! `discover`/`read`/`shutdown` frames. Only frames are ever written to the
//! output; diagnostics go to a separate bounded, sanitized stream (stderr for
//! a child process). Any protocol violation ends the host with a non-success
//! [`HostExit`], because a desynchronized stream cannot be trusted again.

use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use dgxtop_core::clock::TimeProvider;
use dgxtop_core::errors::InfraError;
use dgxtop_core::identity::AdapterId;
use dgxtop_core::observation::{
    BatchStatus, CatalogDelta, CatalogReply, CatalogTag, DiscoverRequest, HelloTag, HostHello,
    IPC_PROTOCOL_VERSION, IpcRequest, IpcRequestKind, ObservationBatch, ReadPlan, ShutdownAck,
    ShutdownTag, split_page,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::text::sanitize_message;
use serde::Serialize;

use crate::ipc::{
    FrameDecoder, IpcError, IpcLimits, REQUEST_FRAME_BYTES, decode_request, encode_frame,
    read_frame_blocking,
};

/// Argument the parent appends to the launch command line.
pub const HOST_GENERATION_ARG: &str = "--host-generation";
/// Environment variable carrying the per-incarnation nonce.
pub const HOST_NONCE_ENV: &str = "DGXTOP_HOST_NONCE";
/// Base read group of discover requests (`catalog`, `catalog:1`, ...).
pub const DISCOVER_READ_GROUP: &str = "catalog";
/// `error_code` of a read/discover whose deadline passed before it started.
pub const DEADLINE_EXPIRED: &str = "DEADLINE_EXPIRED";
/// `error_code` replacing adapter output that violates the contract.
pub const ADAPTER_CONTRACT_VIOLATION: &str = "ADAPTER_CONTRACT_VIOLATION";
/// `error_code` replacing a reply that would exceed the frame limit.
pub const REPLY_TOO_LARGE: &str = "REPLY_TOO_LARGE";
/// `error_code` of the reply sent after the adapter panicked.
pub const ADAPTER_PANIC: &str = "ADAPTER_PANIC";

/// Upper bound on diagnostic lines one host incarnation writes.
const MAX_DIAGNOSTIC_LINES: u32 = 128;
/// Upper bound on one diagnostic line.
const MAX_DIAGNOSTIC_LINE_BYTES: usize = 1024;
/// Maximum nonce length accepted from the environment.
const MAX_NONCE_BYTES: usize = 128;

/// How an adapter host ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostExit {
    /// The parent sent `shutdown`; it was acknowledged.
    ShutdownRequested,
    /// The parent closed the request stream at a frame boundary.
    InputClosed,
    /// Malformed frame, wrong adapter/generation, truncated stream ...
    ProtocolViolation(String),
    /// Reading requests failed.
    InputFailed(String),
    /// Writing a reply failed (parent gone).
    OutputFailed(String),
    /// The adapter panicked; a failed reply was sent when possible.
    AdapterPanicked(String),
}

impl HostExit {
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::ShutdownRequested | Self::InputClosed)
    }

    /// Process exit code (sysexits-style) for a child host.
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::ShutdownRequested | Self::InputClosed => 0,
            Self::ProtocolViolation(_) => 65,
            Self::AdapterPanicked(_) => 70,
            Self::InputFailed(_) | Self::OutputFailed(_) => 74,
        }
    }
}

/// Identity and limits of one host incarnation.
#[derive(Debug, Clone)]
pub struct HostOptions {
    pub generation: u32,
    pub nonce: String,
    pub limits: IpcLimits,
}

/// Runs the host loop with protocol diagnostics on the process stderr.
pub fn run_adapter_host(
    adapter: Box<dyn TelemetryPort>,
    generation: u32,
    nonce: String,
    clock: Arc<dyn TimeProvider>,
    input: impl Read,
    output: impl Write,
) -> HostExit {
    let options = HostOptions {
        generation,
        nonce,
        limits: IpcLimits::default(),
    };
    let mut diagnostics = std::io::stderr();
    run_adapter_host_with(adapter, options, clock, input, output, &mut diagnostics)
}

/// Runs the host loop with an explicit diagnostics sink (in-process hosts
/// route it to a pipe drained by the supervisor; tests capture it).
pub fn run_adapter_host_with(
    adapter: Box<dyn TelemetryPort>,
    options: HostOptions,
    clock: Arc<dyn TimeProvider>,
    mut input: impl Read,
    mut output: impl Write,
    diagnostics: &mut dyn Write,
) -> HostExit {
    let adapter_id = adapter.manifest().adapter_id.clone();
    let mut session = HostSession {
        adapter,
        adapter_id,
        options,
        clock,
        diagnostics: DiagnosticLines::new(diagnostics),
    };
    let exit = session.serve(&mut input, &mut output);
    session.shut_down_adapter();
    exit
}

struct HostSession<'a> {
    adapter: Box<dyn TelemetryPort>,
    adapter_id: AdapterId,
    options: HostOptions,
    clock: Arc<dyn TimeProvider>,
    diagnostics: DiagnosticLines<'a>,
}

/// What one request produced: a reply to send and whether to keep serving.
enum Outcome {
    Continue,
    Stop(HostExit),
}

impl HostSession<'_> {
    fn serve(&mut self, input: &mut dyn Read, output: &mut dyn Write) -> HostExit {
        let hello = self.hello();
        if let Err(exit) = self.send(output, &hello) {
            return exit;
        }
        let request_cap = self
            .options
            .limits
            .max_frame_bytes()
            .min(REQUEST_FRAME_BYTES);
        let mut decoder = match FrameDecoder::new(request_cap) {
            Ok(decoder) => decoder,
            Err(error) => return self.violation(&error),
        };
        let mut scratch = [0u8; 4096];
        loop {
            let payload = match read_frame_blocking(input, &mut decoder, &mut scratch) {
                Ok(Some(payload)) => payload,
                Ok(None) => return HostExit::InputClosed,
                Err(IpcError::Io { message }) => return HostExit::InputFailed(message),
                Err(error) => return self.violation(&error),
            };
            match self.handle(&payload, output) {
                Outcome::Continue => {}
                Outcome::Stop(exit) => return exit,
            }
        }
    }

    fn hello(&self) -> HostHello {
        let manifest = self.adapter.manifest().clone();
        HostHello {
            kind: HelloTag::Hello,
            protocol_version: IPC_PROTOCOL_VERSION,
            adapter_id: manifest.adapter_id.clone(),
            generation: self.options.generation,
            nonce: self.options.nonce.clone(),
            manifest,
        }
    }

    fn handle(&mut self, payload: &[u8], output: &mut dyn Write) -> Outcome {
        let request_limits = self.options.limits.with_frame_cap(REQUEST_FRAME_BYTES);
        let request = match decode_request(payload, &request_limits) {
            Ok(request) => request,
            Err(error) => return Outcome::Stop(self.violation(&error)),
        };
        if let Err(reason) = self.check_identity(&request) {
            let error = IpcError::Contract { message: reason };
            return Outcome::Stop(self.violation(&error));
        }
        match request.kind {
            IpcRequestKind::Discover => self.serve_discover(&request, output),
            IpcRequestKind::Read => self.serve_read(&request, output),
            IpcRequestKind::Shutdown => {
                let ack = ShutdownAck {
                    kind: ShutdownTag::ShutdownAck,
                    request_id: request.request_id,
                };
                match self.send(output, &ack) {
                    Ok(()) => Outcome::Stop(HostExit::ShutdownRequested),
                    Err(exit) => Outcome::Stop(exit),
                }
            }
        }
    }

    fn check_identity(&self, request: &IpcRequest) -> Result<(), String> {
        if request.adapter_id != self.adapter_id {
            return Err(format!(
                "request for adapter {} sent to host {}",
                request.adapter_id, self.adapter_id
            ));
        }
        if request.host_generation != self.options.generation {
            return Err(format!(
                "stale host_generation {} (host is {})",
                request.host_generation, self.options.generation
            ));
        }
        Ok(())
    }

    fn serve_discover(&mut self, request: &IpcRequest, output: &mut dyn Write) -> Outcome {
        let discover = match discover_request(request) {
            Ok(discover) => discover,
            Err(reason) => {
                let error = IpcError::Contract { message: reason };
                return Outcome::Stop(self.violation(&error));
            }
        };
        let reply = if self.clock.now_boot_ns() >= discover.deadline_boot_ns {
            failed_catalog(&discover, self.options.generation, DEADLINE_EXPIRED)
        } else {
            match self.call_discover(&discover) {
                Ok(reply) => reply,
                Err(exit) => {
                    let failed = failed_catalog(&discover, self.options.generation, ADAPTER_PANIC);
                    let _ = self.send(output, &failed);
                    return Outcome::Stop(exit);
                }
            }
        };
        self.send_catalog(output, reply, &discover)
    }

    fn call_discover(&mut self, discover: &DiscoverRequest) -> Result<CatalogReply, HostExit> {
        let adapter = &mut self.adapter;
        let delta = catch_unwind(AssertUnwindSafe(|| adapter.discover(discover)))
            .map_err(|panic| HostExit::AdapterPanicked(panic_text(panic.as_ref())))?;
        if let Err(error) = delta.validate(dgxtop_core::observation::MAX_CATALOG_PAGE) {
            self.diagnostics
                .note(ADAPTER_CONTRACT_VIOLATION, &error.to_string());
            return Ok(failed_catalog(
                discover,
                self.options.generation,
                ADAPTER_CONTRACT_VIOLATION,
            ));
        }
        Ok(CatalogReply {
            kind: CatalogTag::Catalog,
            request_id: discover.request_id,
            generation: self.options.generation,
            delta,
        })
    }

    fn send_catalog(
        &mut self,
        output: &mut dyn Write,
        reply: CatalogReply,
        discover: &DiscoverRequest,
    ) -> Outcome {
        match self.send(output, &reply) {
            Ok(()) => Outcome::Continue,
            Err(HostExit::ProtocolViolation(reason)) => {
                self.diagnostics.note(REPLY_TOO_LARGE, &reason);
                let failed = failed_catalog(discover, self.options.generation, REPLY_TOO_LARGE);
                self.continue_or_stop(output, &failed)
            }
            Err(exit) => Outcome::Stop(exit),
        }
    }

    fn serve_read(&mut self, request: &IpcRequest, output: &mut dyn Write) -> Outcome {
        let plan = read_plan(request);
        let now = self.clock.now_boot_ns();
        if now >= plan.deadline_boot_ns {
            let failed = failed_batch(&plan, now, DEADLINE_EXPIRED);
            return self.continue_or_stop(output, &failed);
        }
        let adapter = &mut self.adapter;
        let batch = match catch_unwind(AssertUnwindSafe(|| adapter.read(&plan))) {
            Ok(batch) => batch,
            Err(panic) => {
                let failed = failed_batch(&plan, self.clock.now_boot_ns(), ADAPTER_PANIC);
                let _ = self.send(output, &failed);
                return Outcome::Stop(HostExit::AdapterPanicked(panic_text(panic.as_ref())));
            }
        };
        let batch = self.contract_checked(batch, &plan);
        match self.send(output, &batch) {
            Ok(()) => Outcome::Continue,
            Err(HostExit::ProtocolViolation(reason)) => {
                self.diagnostics.note(REPLY_TOO_LARGE, &reason);
                let failed = failed_batch(&plan, self.clock.now_boot_ns(), REPLY_TOO_LARGE);
                self.continue_or_stop(output, &failed)
            }
            Err(exit) => Outcome::Stop(exit),
        }
    }

    /// Replaces adapter output that the parent would reject anyway, so an
    /// adapter bug degrades one batch instead of restarting the host.
    fn contract_checked(&mut self, batch: ObservationBatch, plan: &ReadPlan) -> ObservationBatch {
        match batch.validate_against(plan, self.options.limits.max_records()) {
            Ok(()) => batch,
            Err(error) => {
                self.diagnostics
                    .note(ADAPTER_CONTRACT_VIOLATION, &error.to_string());
                failed_batch(plan, self.clock.now_boot_ns(), ADAPTER_CONTRACT_VIOLATION)
            }
        }
    }

    fn continue_or_stop<T: Serialize>(&mut self, output: &mut dyn Write, reply: &T) -> Outcome {
        match self.send(output, reply) {
            Ok(()) => Outcome::Continue,
            Err(exit) => Outcome::Stop(exit),
        }
    }

    /// Encodes and writes one frame. An oversized reply is reported as a
    /// protocol violation so the caller can substitute a small failure reply.
    fn send<T: Serialize>(&mut self, output: &mut dyn Write, message: &T) -> Result<(), HostExit> {
        let frame = encode_frame(message, self.options.limits.max_frame_bytes())
            .map_err(|error| HostExit::ProtocolViolation(error.to_string()))?;
        output
            .write_all(&frame)
            .and_then(|()| output.flush())
            .map_err(|error| HostExit::OutputFailed(sanitize_message(&error.to_string())))
    }

    fn violation(&mut self, error: &IpcError) -> HostExit {
        let reason = error.to_string();
        self.diagnostics.note(error.code(), &reason);
        HostExit::ProtocolViolation(sanitize_message(&reason))
    }

    fn shut_down_adapter(&mut self) {
        let adapter = &mut self.adapter;
        if catch_unwind(AssertUnwindSafe(|| adapter.shutdown())).is_err() {
            self.diagnostics
                .note(ADAPTER_PANIC, "adapter panicked during shutdown");
        }
    }
}

fn discover_request(request: &IpcRequest) -> Result<DiscoverRequest, String> {
    let (group, page) = split_page(request.read_group.as_str());
    if group != DISCOVER_READ_GROUP {
        return Err(format!(
            "discover read_group must be {DISCOVER_READ_GROUP}[:page]"
        ));
    }
    // The IPC request has no previous_generation field; discover requests
    // carry it in `cycle_id`, which has no meaning for discovery.
    let previous_generation = u32::try_from(request.cycle_id)
        .map_err(|_| "discover cycle_id (previous generation) exceeds u32".to_owned())?;
    Ok(DiscoverRequest {
        request_id: request.request_id,
        adapter_id: request.adapter_id.clone(),
        host_generation: request.host_generation,
        catalog_revision: request.catalog_revision,
        config_revision: request.config_revision,
        deadline_boot_ns: request.deadline_boot_ns,
        previous_generation,
        page,
    })
}

fn read_plan(request: &IpcRequest) -> ReadPlan {
    ReadPlan {
        request_id: request.request_id,
        adapter_id: request.adapter_id.clone(),
        host_generation: request.host_generation,
        catalog_revision: request.catalog_revision,
        config_revision: request.config_revision,
        cycle_id: request.cycle_id,
        read_group: request.read_group.clone(),
        source_slots: request.source_slots.clone(),
        deadline_boot_ns: request.deadline_boot_ns,
        // The config epoch is a parent-side concept; late replies of older
        // epochs are filtered there, never inside the host.
        epoch: 0,
        page: request.page(),
    }
}

/// A failed batch for `plan` with no records (never an empty success).
pub fn failed_batch(plan: &ReadPlan, now_boot_ns: u64, error_code: &str) -> ObservationBatch {
    ObservationBatch {
        protocol_version: IPC_PROTOCOL_VERSION,
        adapter_id: plan.adapter_id.clone(),
        host_generation: plan.host_generation,
        catalog_revision: plan.catalog_revision,
        config_revision: plan.config_revision,
        request_id: plan.request_id,
        cycle_id: plan.cycle_id,
        read_group: plan.read_group.clone(),
        started_boot_ns: now_boot_ns,
        finished_boot_ns: now_boot_ns,
        records: Vec::new(),
        status: BatchStatus::Failed,
        error_code: Some(error_code.to_owned()),
    }
}

/// A failed, incomplete catalog page: it changes nothing relative to the
/// generation the parent already holds.
fn failed_catalog(
    discover: &DiscoverRequest,
    host_generation: u32,
    error_code: &str,
) -> CatalogReply {
    CatalogReply {
        kind: CatalogTag::Catalog,
        request_id: discover.request_id,
        generation: host_generation,
        delta: CatalogDelta {
            request_id: Some(discover.request_id),
            adapter_id: Some(discover.adapter_id.clone()),
            generation: discover.previous_generation,
            complete: false,
            error_code: Some(error_code.to_owned()),
            ..CatalogDelta::default()
        },
    }
}

fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    let text = panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());
    sanitize_message(&text)
}

/// Bounded, sanitized diagnostic lines (never frames).
struct DiagnosticLines<'a> {
    sink: &'a mut dyn Write,
    written: u32,
    suppressed: u64,
}

impl<'a> DiagnosticLines<'a> {
    fn new(sink: &'a mut dyn Write) -> Self {
        Self {
            sink,
            written: 0,
            suppressed: 0,
        }
    }

    fn note(&mut self, code: &str, detail: &str) {
        if self.written >= MAX_DIAGNOSTIC_LINES {
            self.suppressed = self.suppressed.saturating_add(1);
            return;
        }
        self.written += 1;
        let mut line = sanitize_message(&format!("{code}: {detail}"));
        line.truncate(dgxtop_core::text::truncate_utf8(&line, MAX_DIAGNOSTIC_LINE_BYTES).len());
        line.push('\n');
        // Diagnostics are best effort: a closed stderr must not stop the host.
        let _ = self.sink.write_all(line.as_bytes());
        let _ = self.sink.flush();
    }
}

/// Parses `--host-generation <n>` (or `--host-generation=<n>`) from argv.
pub fn host_generation_from_args<I, S>(args: I) -> Result<u32, InfraError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        let argument = argument.as_ref();
        if argument == HOST_GENERATION_ARG {
            let value = arguments.next().ok_or_else(|| {
                InfraError::Protocol(format!("{HOST_GENERATION_ARG} needs a value"))
            })?;
            return parse_generation(value.as_ref());
        }
        if let Some(value) = argument
            .strip_prefix(HOST_GENERATION_ARG)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return parse_generation(value);
        }
    }
    Err(InfraError::Protocol(format!(
        "missing {HOST_GENERATION_ARG} argument"
    )))
}

fn parse_generation(text: &str) -> Result<u32, InfraError> {
    let generation: u32 = text.parse().map_err(|_| {
        InfraError::Protocol(format!(
            "{HOST_GENERATION_ARG} must be an integer, got {}",
            dgxtop_core::text::sanitize_display(text, 32)
        ))
    })?;
    if generation == 0 || i32::try_from(generation).is_err() {
        return Err(InfraError::Protocol(format!(
            "{HOST_GENERATION_ARG} must be 1..=2147483647"
        )));
    }
    Ok(generation)
}

/// Reads the per-incarnation nonce from [`HOST_NONCE_ENV`].
pub fn host_nonce_from_env() -> Result<String, InfraError> {
    let nonce = std::env::var(HOST_NONCE_ENV)
        .map_err(|_| InfraError::Protocol(format!("{HOST_NONCE_ENV} is not set")))?;
    validate_nonce(&nonce)?;
    Ok(nonce)
}

pub(crate) fn validate_nonce(nonce: &str) -> Result<(), InfraError> {
    let valid = !nonce.is_empty()
        && nonce.len() <= MAX_NONCE_BYTES
        && nonce.bytes().all(|byte| byte.is_ascii_graphic());
    if valid {
        Ok(())
    } else {
        Err(InfraError::Protocol(format!(
            "{HOST_NONCE_ENV} must be 1..={MAX_NONCE_BYTES} printable ASCII bytes"
        )))
    }
}

#[cfg(test)]
pub(crate) mod test_adapter {
    //! Scriptable in-memory adapter shared by host and supervisor tests.

    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use dgxtop_core::observation::{
        AdapterHealth, AdapterManifest, AdapterOperation, CatalogDelta, DiscoverRequest, Isolation,
        ObservationBatch, ReadPlan,
    };
    use dgxtop_core::ports::TelemetryPort;
    use dgxtop_core::quality::{Availability, Namespace};
    use dgxtop_core::testing::{ObservationBuilder, adapter_id, metric_key};

    use super::{BatchStatus, IPC_PROTOCOL_VERSION};

    /// What the fake does on `read`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ReadBehavior {
        Healthy,
        /// Sleeps before answering (every read).
        Slow(Duration),
        /// Sleeps before answering the first read only.
        SlowOnce(Duration),
        /// Returns a batch for another request id (contract violation).
        WrongRequest,
        Panic,
    }

    /// Calls observed by the fake, shared with the test.
    #[derive(Debug, Default)]
    pub struct Calls {
        pub discovers: AtomicU32,
        pub reads: AtomicU32,
        pub shutdowns: AtomicU32,
        pub last_page: AtomicU32,
        pub last_previous_generation: AtomicU32,
        pub last_epoch: AtomicU64,
        pub last_slots: Mutex<Vec<u32>>,
    }

    pub struct FakeAdapter {
        manifest: AdapterManifest,
        behavior: ReadBehavior,
        pub calls: Arc<Calls>,
    }

    pub fn manifest(adapter: &str, isolation: Isolation) -> AdapterManifest {
        AdapterManifest {
            api_version: 1,
            adapter_id: adapter_id(adapter),
            implementation: "FakeAdapter".into(),
            isolation,
            operations: vec![
                AdapterOperation::Discover,
                AdapterOperation::Read,
                AdapterOperation::Shutdown,
            ],
            max_inflight: 1,
            runtime_library: None,
            minimum_sample_ms: 100,
            source_limit: 16,
            sample_namespace: if isolation == Isolation::InProcessReplay {
                Namespace::Replay
            } else {
                Namespace::Live
            },
        }
    }

    impl FakeAdapter {
        pub fn new(adapter: &str, isolation: Isolation, behavior: ReadBehavior) -> Self {
            Self {
                manifest: manifest(adapter, isolation),
                behavior,
                calls: Arc::new(Calls::default()),
            }
        }

        pub fn with_calls(mut self, calls: Arc<Calls>) -> Self {
            self.calls = calls;
            self
        }
    }

    pub fn batch_for(plan: &ReadPlan, value: f64) -> ObservationBatch {
        let slots = if plan.source_slots.is_empty() {
            vec![0, 1]
        } else {
            plan.source_slots.clone()
        };
        let records = slots
            .iter()
            .map(|slot| {
                ObservationBuilder::new(
                    metric_key("gpu0", "gpu.temperature", "device"),
                    plan.adapter_id.as_str(),
                    &format!("{}:slot{slot}", plan.adapter_id),
                )
                .f64(value)
                .cycle(plan.cycle_id)
                .build()
            })
            .collect();
        ObservationBatch {
            protocol_version: IPC_PROTOCOL_VERSION,
            adapter_id: plan.adapter_id.clone(),
            host_generation: plan.host_generation,
            catalog_revision: plan.catalog_revision,
            config_revision: plan.config_revision,
            request_id: plan.request_id,
            cycle_id: plan.cycle_id,
            read_group: plan.read_group.clone(),
            started_boot_ns: 0,
            finished_boot_ns: 0,
            records,
            status: BatchStatus::Ok,
            error_code: None,
        }
    }

    impl TelemetryPort for FakeAdapter {
        fn manifest(&self) -> &AdapterManifest {
            &self.manifest
        }

        fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta {
            self.calls.discovers.fetch_add(1, Ordering::SeqCst);
            self.calls.last_page.store(request.page, Ordering::SeqCst);
            self.calls
                .last_previous_generation
                .store(request.previous_generation, Ordering::SeqCst);
            CatalogDelta {
                request_id: Some(request.request_id),
                adapter_id: Some(request.adapter_id.clone()),
                generation: request.previous_generation + 1,
                complete: true,
                ..CatalogDelta::default()
            }
        }

        fn read(&mut self, plan: &ReadPlan) -> ObservationBatch {
            let count = self.calls.reads.fetch_add(1, Ordering::SeqCst) + 1;
            self.calls.last_epoch.store(plan.epoch, Ordering::SeqCst);
            *self.calls.last_slots.lock().unwrap() = plan.source_slots.clone();
            match self.behavior {
                ReadBehavior::Healthy => {}
                ReadBehavior::Slow(delay) => std::thread::sleep(delay),
                ReadBehavior::SlowOnce(delay) if count == 1 => std::thread::sleep(delay),
                ReadBehavior::SlowOnce(_) => {}
                ReadBehavior::WrongRequest => {
                    let mut batch = batch_for(plan, f64::from(count));
                    batch.request_id = uuid::Uuid::new_v4();
                    return batch;
                }
                ReadBehavior::Panic => panic!("fake adapter read panic"),
            }
            batch_for(plan, f64::from(count))
        }

        fn health(&self) -> AdapterHealth {
            AdapterHealth {
                availability: Availability::Available,
                detail: None,
            }
        }

        fn shutdown(&mut self) {
            self.calls.shutdowns.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_adapter::{Calls, FakeAdapter, ReadBehavior};
    use super::*;
    use crate::ipc::{decode_batch, decode_catalog, decode_shutdown_ack};
    use dgxtop_core::observation::{Isolation, MAX_RECORDS_PER_BATCH, paged_group};
    use dgxtop_core::testing::{FakeClock, adapter_id, read_group};
    use std::sync::atomic::Ordering;
    use uuid::Uuid;

    fn request(kind: IpcRequestKind, generation: u32, deadline: u64) -> IpcRequest {
        IpcRequest {
            protocol_version: 1,
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id("fake"),
            host_generation: generation,
            kind,
            deadline_boot_ns: deadline,
            catalog_revision: 1,
            config_revision: 1,
            cycle_id: 5,
            read_group: read_group(if kind == IpcRequestKind::Discover {
                DISCOVER_READ_GROUP
            } else {
                "fast"
            }),
            source_slots: if kind == IpcRequestKind::Read {
                vec![3, 4]
            } else {
                Vec::new()
            },
        }
    }

    fn frames_of(requests: &[IpcRequest]) -> Vec<u8> {
        requests
            .iter()
            .flat_map(|request| encode_frame(request, REQUEST_FRAME_BYTES).unwrap())
            .collect()
    }

    struct Run {
        exit: HostExit,
        frames: Vec<Vec<u8>>,
        diagnostics: String,
        calls: Arc<Calls>,
    }

    fn run(input: Vec<u8>, behavior: ReadBehavior, clock_ns: u64) -> Run {
        let calls = Arc::new(Calls::default());
        let adapter = FakeAdapter::new("fake", Isolation::PersistentProcess, behavior)
            .with_calls(Arc::clone(&calls));
        let options = HostOptions {
            generation: 2,
            nonce: "nonce-1".into(),
            limits: IpcLimits::default(),
        };
        let mut output = Vec::new();
        let mut diagnostics = Vec::new();
        let exit = run_adapter_host_with(
            Box::new(adapter),
            options,
            FakeClock::new(clock_ns),
            input.as_slice(),
            &mut output,
            &mut diagnostics,
        );
        let mut decoder = FrameDecoder::new(1 << 20).unwrap();
        let frames = decoder.feed(&output, 64).unwrap();
        decoder.finish().unwrap();
        Run {
            exit,
            frames,
            diagnostics: String::from_utf8(diagnostics).unwrap(),
            calls,
        }
    }

    fn hello_of(frame: &[u8]) -> HostHello {
        serde_json::from_slice(frame).unwrap()
    }

    #[test]
    fn hello_then_discover_read_and_shutdown() {
        let discover = request(IpcRequestKind::Discover, 2, 1_000);
        let read = request(IpcRequestKind::Read, 2, 1_000);
        let shutdown = request(IpcRequestKind::Shutdown, 2, 1_000);
        let result = run(
            frames_of(&[discover.clone(), read.clone(), shutdown.clone()]),
            ReadBehavior::Healthy,
            100,
        );
        assert_eq!(result.exit, HostExit::ShutdownRequested);
        assert_eq!(result.exit.exit_code(), 0);
        assert_eq!(result.frames.len(), 4);
        let hello = hello_of(&result.frames[0]);
        assert_eq!(hello.generation, 2);
        assert_eq!(hello.nonce, "nonce-1");
        assert_eq!(hello.adapter_id.as_str(), "fake");
        let limits = IpcLimits::default();
        let catalog = decode_catalog(&result.frames[1], &limits).unwrap();
        assert_eq!(catalog.request_id, discover.request_id);
        assert_eq!(catalog.generation, 2);
        assert!(catalog.delta.complete);
        let batch = decode_batch(&result.frames[2], &limits).unwrap();
        assert_eq!(batch.request_id, read.request_id);
        assert_eq!(batch.records.len(), 2);
        assert_eq!(*result.calls.last_slots.lock().unwrap(), vec![3, 4]);
        let ack = decode_shutdown_ack(&result.frames[3], &limits).unwrap();
        assert_eq!(ack.request_id, shutdown.request_id);
        assert_eq!(result.calls.shutdowns.load(Ordering::SeqCst), 1);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn expired_deadline_fails_without_calling_the_driver() {
        let read = request(IpcRequestKind::Read, 2, 50);
        let discover = request(IpcRequestKind::Discover, 2, 50);
        let result = run(frames_of(&[read, discover]), ReadBehavior::Healthy, 100);
        assert_eq!(result.exit, HostExit::InputClosed);
        let limits = IpcLimits::default();
        let batch = decode_batch(&result.frames[1], &limits).unwrap();
        assert_eq!(batch.status, BatchStatus::Failed);
        assert_eq!(batch.error_code.as_deref(), Some(DEADLINE_EXPIRED));
        assert!(batch.records.is_empty());
        let catalog = decode_catalog(&result.frames[2], &limits).unwrap();
        assert!(!catalog.delta.complete);
        assert_eq!(catalog.delta.error_code.as_deref(), Some(DEADLINE_EXPIRED));
        assert_eq!(result.calls.reads.load(Ordering::SeqCst), 0);
        assert_eq!(result.calls.discovers.load(Ordering::SeqCst), 0);
        // EOF still shuts the adapter down.
        assert_eq!(result.calls.shutdowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn discover_pages_and_previous_generation_are_forwarded() {
        let mut discover = request(IpcRequestKind::Discover, 2, 1_000);
        discover.read_group = read_group(&paged_group(DISCOVER_READ_GROUP, 3));
        discover.cycle_id = 9;
        let result = run(frames_of(&[discover]), ReadBehavior::Healthy, 0);
        assert_eq!(result.exit, HostExit::InputClosed);
        assert_eq!(result.calls.last_page.load(Ordering::SeqCst), 3);
        assert_eq!(
            result.calls.last_previous_generation.load(Ordering::SeqCst),
            9
        );
        let mut wrong_group = request(IpcRequestKind::Discover, 2, 1_000);
        wrong_group.read_group = read_group("fast");
        let result = run(frames_of(&[wrong_group]), ReadBehavior::Healthy, 0);
        assert!(matches!(result.exit, HostExit::ProtocolViolation(_)));
    }

    #[test]
    fn wrong_generation_or_adapter_is_a_protocol_violation() {
        let stale = request(IpcRequestKind::Read, 1, 1_000);
        let result = run(frames_of(&[stale]), ReadBehavior::Healthy, 0);
        assert!(matches!(result.exit, HostExit::ProtocolViolation(_)));
        assert_eq!(result.exit.exit_code(), 65);
        assert_eq!(result.frames.len(), 1, "only the hello was written");
        assert!(result.diagnostics.contains("stale host_generation"));
        assert_eq!(result.calls.reads.load(Ordering::SeqCst), 0);
        let mut foreign = request(IpcRequestKind::Read, 2, 1_000);
        foreign.adapter_id = adapter_id("nvml");
        let result = run(frames_of(&[foreign]), ReadBehavior::Healthy, 0);
        assert!(matches!(result.exit, HostExit::ProtocolViolation(_)));
    }

    #[test]
    fn malformed_truncated_and_oversized_input_end_the_host() {
        let mut garbage = 7u32.to_be_bytes().to_vec();
        garbage.extend_from_slice(b"{\"a\":}x");
        let result = run(garbage, ReadBehavior::Healthy, 0);
        assert!(matches!(result.exit, HostExit::ProtocolViolation(_)));
        assert!(result.diagnostics.starts_with("MALFORMED"));

        let full = frames_of(&[request(IpcRequestKind::Read, 2, 1_000)]);
        let result = run(full[..full.len() - 3].to_vec(), ReadBehavior::Healthy, 0);
        assert!(matches!(result.exit, HostExit::ProtocolViolation(_)));
        assert!(result.diagnostics.starts_with("TRUNCATED_FRAME"));

        let oversized = u32::MAX.to_be_bytes().to_vec();
        let result = run(oversized, ReadBehavior::Healthy, 0);
        assert!(result.diagnostics.starts_with("FRAME_TOO_LARGE"));
        assert_eq!(result.calls.shutdowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn contract_violating_adapter_output_becomes_a_failed_batch() {
        let read = request(IpcRequestKind::Read, 2, 1_000);
        let result = run(
            frames_of(std::slice::from_ref(&read)),
            ReadBehavior::WrongRequest,
            0,
        );
        assert_eq!(result.exit, HostExit::InputClosed);
        let batch = decode_batch(&result.frames[1], &IpcLimits::default()).unwrap();
        assert_eq!(batch.request_id, read.request_id);
        assert_eq!(batch.status, BatchStatus::Failed);
        assert_eq!(
            batch.error_code.as_deref(),
            Some(ADAPTER_CONTRACT_VIOLATION)
        );
        assert!(result.diagnostics.contains(ADAPTER_CONTRACT_VIOLATION));
    }

    #[test]
    fn too_many_slots_in_a_request_are_rejected() {
        let mut read = request(IpcRequestKind::Read, 2, 1_000);
        read.source_slots = (0..=u32::try_from(MAX_RECORDS_PER_BATCH).unwrap()).collect();
        let result = run(frames_of(&[read]), ReadBehavior::Healthy, 0);
        assert!(matches!(result.exit, HostExit::ProtocolViolation(_)));
    }

    #[test]
    fn adapter_panic_sends_a_failed_reply_and_exits() {
        let read = request(IpcRequestKind::Read, 2, 1_000);
        let result = run(
            frames_of(std::slice::from_ref(&read)),
            ReadBehavior::Panic,
            0,
        );
        assert!(matches!(result.exit, HostExit::AdapterPanicked(_)));
        assert_eq!(result.exit.exit_code(), 70);
        let batch = decode_batch(&result.frames[1], &IpcLimits::default()).unwrap();
        assert_eq!(batch.request_id, read.request_id);
        assert_eq!(batch.error_code.as_deref(), Some(ADAPTER_PANIC));
    }

    #[test]
    fn diagnostics_are_sanitized_and_bounded() {
        let mut sink = Vec::new();
        let mut lines = DiagnosticLines::new(&mut sink);
        lines.note("CODE", "evil\u{1b}]0;title\u{7}text");
        for _ in 0..(MAX_DIAGNOSTIC_LINES * 2) {
            lines.note("CODE", &"x".repeat(5000));
        }
        let suppressed = lines.suppressed;
        let text = String::from_utf8(sink).unwrap();
        assert!(text.starts_with("CODE: eviltext\n"));
        assert_eq!(
            text.lines().count(),
            usize::try_from(MAX_DIAGNOSTIC_LINES).unwrap()
        );
        assert!(
            text.lines()
                .all(|line| line.len() <= MAX_DIAGNOSTIC_LINE_BYTES)
        );
        assert_eq!(suppressed, u64::from(MAX_DIAGNOSTIC_LINES) + 1);
    }

    #[test]
    fn nonce_validation() {
        assert!(validate_nonce("abcDEF0123-_").is_ok());
        assert!(validate_nonce("").is_err());
        assert!(validate_nonce("has space").is_err());
        assert!(validate_nonce(&"n".repeat(MAX_NONCE_BYTES + 1)).is_err());
    }
}
