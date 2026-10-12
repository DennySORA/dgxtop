//! Replay adapter (`replay` / `replay-N`): serves recorded or synthetic
//! fixtures in the **replay namespace** only. Every source of a
//! fixture must be `replay` or `simulated`; live sample kinds are rejected
//! at validation, so replay can never inject live data. Read groups mirror
//! the live groups as `<live adapter>.<group>` (`naming::replay_group`) and
//! are routed to the same collectors.
//!
//! * [`ReplayFixture`]: format `dgxtop-replay-fixture/1` (see `fixture`).
//! * [`ReplayAdapter`]: `TelemetryPort` over a fixture (see `adapter`).
//! * [`generate_ref_fixture`] / [`RefProfile`]: deterministic REF/OVERLOAD
//!   workloads mirroring `bench/scenarios.json`.
//! * A small curated fixture ships as `crates/adapters/fixtures/replay/demo.json`
//!   for `dgxtop --replay`.

mod adapter;
mod fixture;
mod generator;

use dgxtop_core::identity::AdapterId;
use dgxtop_core::observation::{AdapterManifest, AdapterOperation, Isolation};
use dgxtop_core::quality::Namespace;

pub use self::adapter::{MAX_SOURCES_PER_HOST, ReplayAdapter};
pub use self::fixture::{
    FIXTURE_FORMAT, FaultKind, FixtureEntity, FixtureFault, FixtureSample, FixtureSource,
    FixtureValue, FixtureViolation, MAX_CYCLES, MAX_ENTITIES, MAX_FAULTS, MAX_FIXTURE_BYTES,
    MAX_HANG_MS, MAX_SAMPLES_PER_CYCLE, MAX_SIMULATED_LATENCY_US, MAX_SOURCES, MAX_TOTAL_SAMPLES,
    ReplayError, ReplayFixture, family_of,
};
pub use self::generator::{RefProfile, generate_ref_fixture};

/// Shortest sampling period a replay host accepts (OVERLOAD samples at 100 ms).
const REPLAY_MINIMUM_SAMPLE_MS: u32 = 100;

/// Manifest of a replay adapter: `in_process_replay` isolation for
/// `--replay`, or a persistent host process (benchmark slices).
pub fn manifest(adapter_id: &AdapterId, in_process: bool) -> AdapterManifest {
    AdapterManifest {
        api_version: 1,
        adapter_id: adapter_id.clone(),
        implementation: "ReplayAdapter".to_owned(),
        isolation: if in_process {
            Isolation::InProcessReplay
        } else {
            Isolation::PersistentProcess
        },
        operations: vec![
            AdapterOperation::Discover,
            AdapterOperation::Read,
            AdapterOperation::Shutdown,
        ],
        max_inflight: 1,
        runtime_library: None,
        minimum_sample_ms: REPLAY_MINIMUM_SAMPLE_MS,
        source_limit: 2048,
        sample_namespace: Namespace::Replay,
    }
}
