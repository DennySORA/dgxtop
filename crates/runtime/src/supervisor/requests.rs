//! Conversions between adapter jobs and IPC requests/completions.

use dgxtop_core::identity::{AdapterId, ReadGroup};
use dgxtop_core::observation::{
    DiscoverRequest, IPC_PROTOCOL_VERSION, IpcRequest, IpcRequestKind, ReadPlan, paged_group,
    split_page,
};
use dgxtop_core::ports::{AdapterFailure, AdapterFailureKind, AdapterJob, IngestionCompletion};
use dgxtop_core::text::sanitize_message;
use uuid::Uuid;

use crate::host::DISCOVER_READ_GROUP;

/// Request id of either job kind.
pub(crate) fn request_id_of(job: &AdapterJob) -> Uuid {
    match job {
        AdapterJob::Read(plan) => plan.request_id,
        AdapterJob::Discover(request) => request.request_id,
    }
}

/// The wire request of a job, or why the job is invalid.
pub(crate) fn request_for(job: &AdapterJob) -> Result<IpcRequest, String> {
    let request = match job {
        AdapterJob::Read(plan) => read_request(plan)?,
        AdapterJob::Discover(discover) => discover_request(discover)?,
    };
    request.validate().map_err(|error| error.to_string())?;
    Ok(request)
}

fn read_request(plan: &ReadPlan) -> Result<IpcRequest, String> {
    // The wire carries the page inside the read group; the batch must echo
    // the plan's read group verbatim, so both must agree.
    let (_, encoded_page) = split_page(plan.read_group.as_str());
    if encoded_page != plan.page {
        return Err(format!(
            "plan page {} disagrees with read group {}",
            plan.page, plan.read_group
        ));
    }
    Ok(IpcRequest {
        protocol_version: IPC_PROTOCOL_VERSION,
        request_id: plan.request_id,
        adapter_id: plan.adapter_id.clone(),
        host_generation: plan.host_generation,
        kind: IpcRequestKind::Read,
        deadline_boot_ns: plan.deadline_boot_ns,
        catalog_revision: plan.catalog_revision,
        config_revision: plan.config_revision,
        cycle_id: plan.cycle_id,
        read_group: plan.read_group.clone(),
        source_slots: plan.source_slots.clone(),
    })
}

fn discover_request(discover: &DiscoverRequest) -> Result<IpcRequest, String> {
    let read_group = ReadGroup::new(paged_group(DISCOVER_READ_GROUP, discover.page))
        .map_err(|error| error.to_string())?;
    Ok(IpcRequest {
        protocol_version: IPC_PROTOCOL_VERSION,
        request_id: discover.request_id,
        adapter_id: discover.adapter_id.clone(),
        host_generation: discover.host_generation,
        kind: IpcRequestKind::Discover,
        deadline_boot_ns: discover.deadline_boot_ns,
        catalog_revision: discover.catalog_revision,
        config_revision: discover.config_revision,
        // IpcRequest has no previous_generation field; discover requests
        // carry it in cycle_id (see `host::discover_request`).
        cycle_id: u64::from(discover.previous_generation),
        read_group,
        source_slots: Vec::new(),
    })
}

/// Revisions echoed by supervisor-originated requests (probe, shutdown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Revisions {
    pub catalog: u32,
    pub config: u32,
}

impl Default for Revisions {
    fn default() -> Self {
        Self {
            catalog: 1,
            config: 1,
        }
    }
}

/// Protocol shutdown request (no slots, no new driver call).
pub(crate) fn shutdown_request(
    adapter_id: &AdapterId,
    generation: u32,
    deadline_boot_ns: u64,
    revisions: Revisions,
) -> Result<IpcRequest, String> {
    supervisor_request(
        adapter_id,
        generation,
        deadline_boot_ns,
        revisions,
        IpcRequestKind::Shutdown,
    )
}

/// Half-open probe: a full discover of the new generation.
pub(crate) fn probe_request(
    adapter_id: &AdapterId,
    generation: u32,
    deadline_boot_ns: u64,
    revisions: Revisions,
) -> Result<IpcRequest, String> {
    supervisor_request(
        adapter_id,
        generation,
        deadline_boot_ns,
        revisions,
        IpcRequestKind::Discover,
    )
}

fn supervisor_request(
    adapter_id: &AdapterId,
    generation: u32,
    deadline_boot_ns: u64,
    revisions: Revisions,
    kind: IpcRequestKind,
) -> Result<IpcRequest, String> {
    let read_group = ReadGroup::new(DISCOVER_READ_GROUP).map_err(|error| error.to_string())?;
    Ok(IpcRequest {
        protocol_version: IPC_PROTOCOL_VERSION,
        request_id: Uuid::new_v4(),
        adapter_id: adapter_id.clone(),
        host_generation: generation,
        kind,
        deadline_boot_ns,
        catalog_revision: revisions.catalog.max(1),
        config_revision: revisions.config.max(1),
        cycle_id: 0,
        read_group,
        source_slots: Vec::new(),
    })
}

pub(crate) fn failure(kind: AdapterFailureKind, message: &str) -> AdapterFailure {
    AdapterFailure {
        kind,
        message: sanitize_message(message),
    }
}

/// Failure completion for `job` delivered under `request_id`.
pub(crate) fn failed_completion(
    job: &AdapterJob,
    request_id: Uuid,
    failure: AdapterFailure,
) -> IngestionCompletion {
    match job {
        AdapterJob::Read(plan) => IngestionCompletion::Batch {
            adapter_id: plan.adapter_id.clone(),
            request_id,
            host_generation: plan.host_generation,
            result: Err(failure),
        },
        AdapterJob::Discover(discover) => IngestionCompletion::Catalog {
            adapter_id: discover.adapter_id.clone(),
            request_id,
            host_generation: discover.host_generation,
            result: Err(failure),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::{adapter_id, read_group};

    fn plan(group: &str, page: u32) -> ReadPlan {
        ReadPlan {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id("procfs"),
            host_generation: 2,
            catalog_revision: 3,
            config_revision: 4,
            cycle_id: 9,
            read_group: read_group(group),
            source_slots: vec![1, 2],
            deadline_boot_ns: 77,
            epoch: 5,
            page,
        }
    }

    #[test]
    fn read_plans_map_to_requests_and_pages_must_agree() {
        let request = request_for(&AdapterJob::Read(plan("process:2", 2))).unwrap();
        assert_eq!(request.kind, IpcRequestKind::Read);
        assert_eq!(request.read_group.as_str(), "process:2");
        assert_eq!(request.page(), 2);
        assert_eq!((request.catalog_revision, request.config_revision), (3, 4));
        assert!(request_for(&AdapterJob::Read(plan("process", 2))).is_err());
        let mut duplicate = plan("fast", 0);
        duplicate.source_slots = vec![1, 1];
        assert!(request_for(&AdapterJob::Read(duplicate)).is_err());
    }

    #[test]
    fn discover_requests_carry_page_and_previous_generation() {
        let discover = DiscoverRequest {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id("nvml"),
            host_generation: 1,
            catalog_revision: 1,
            config_revision: 1,
            deadline_boot_ns: 10,
            previous_generation: 6,
            page: 3,
        };
        let request = request_for(&AdapterJob::Discover(discover)).unwrap();
        assert_eq!(request.read_group.as_str(), "catalog:3");
        assert_eq!(request.cycle_id, 6);
        assert!(request.source_slots.is_empty());
    }

    #[test]
    fn supervisor_requests_validate() {
        let adapter = adapter_id("nvml");
        let shutdown = shutdown_request(&adapter, 4, 100, Revisions::default()).unwrap();
        shutdown.validate().unwrap();
        assert_eq!(shutdown.kind, IpcRequestKind::Shutdown);
        let probe = probe_request(
            &adapter,
            4,
            100,
            Revisions {
                catalog: 0,
                config: 7,
            },
        )
        .unwrap();
        probe.validate().unwrap();
        assert_eq!((probe.catalog_revision, probe.config_revision), (1, 7));
    }
}
