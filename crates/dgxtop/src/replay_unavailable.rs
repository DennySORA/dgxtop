//! Stand-in used when an in-process replay host cannot be rebuilt after a
//! restart: it reports itself unavailable instead of inventing data.

use dgxtop_adapters::common::RecordFactory;
use dgxtop_core::identity::AdapterId;
use dgxtop_core::observation::{
    AdapterManifest, CatalogDelta, DiscoverRequest, ObservationBatch, ReadPlan,
};
use dgxtop_core::ports::TelemetryPort;
use dgxtop_core::quality::Availability;

/// Telemetry port that is always unavailable.
#[derive(Debug, Clone)]
pub struct UnavailableReplay {
    manifest: AdapterManifest,
}

impl UnavailableReplay {
    pub fn new(adapter_id: AdapterId) -> Self {
        Self {
            manifest: dgxtop_adapters::replay::manifest(&adapter_id, true),
        }
    }
}

impl TelemetryPort for UnavailableReplay {
    fn manifest(&self) -> &AdapterManifest {
        &self.manifest
    }

    fn discover(&mut self, request: &DiscoverRequest) -> CatalogDelta {
        CatalogDelta {
            request_id: Some(request.request_id),
            adapter_id: Some(request.adapter_id.clone()),
            generation: request.previous_generation,
            availability: Some(Availability::Unavailable),
            complete: true,
            error_code: Some("REPLAY_FIXTURE_UNAVAILABLE".to_owned()),
            ..CatalogDelta::default()
        }
    }

    fn read(&mut self, plan: &ReadPlan) -> ObservationBatch {
        RecordFactory::failed_batch(
            plan,
            plan.deadline_boot_ns.saturating_sub(1),
            "REPLAY_FIXTURE_UNAVAILABLE",
        )
    }

    fn shutdown(&mut self) {}
}
