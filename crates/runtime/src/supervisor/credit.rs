//! Ingress byte credit (`RUNTIME.md` §5, §9).
//!
//! A read reserves its provable worst case — wire + decoded — *before* it is
//! dispatched, from the shared ingress gate (8 MiB, 32 batches by default).
//! The plan bounds the record count (its slots, or `max_records` when it asks
//! for the whole group); each record is bounded by the schema's string
//! limits. Once a frame's exact length is known its credit is checked again
//! and topped up if a reply carries more records than slots (e.g. process
//! lists); if the top-up fails the supervisor stops reading that pipe until
//! the deadline and then rejects the whole batch.

use dgxtop_core::observation::ReadPlan;

use crate::ipc::IpcLimits;

/// Batch envelope (ids, revisions, timestamps, status) on the wire.
pub const BATCH_ENVELOPE_WIRE_BYTES: u64 = 2048;
/// One observation on the wire with schema-bounded identity strings.
pub const RECORD_WIRE_BYTES: u64 = 8192;
/// Decoded bytes per wire byte (`ipc::size` test proves ≤ 3 for batches).
pub const DECODED_PER_WIRE_BYTE: u64 = 3;
/// Initial wire credit of a discover request (pages are topped up exactly).
pub const DISCOVER_INITIAL_WIRE_BYTES: u64 = 64 * 1024;

fn bytes_of(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Decoded credit for `wire` bytes, capped by the decoded budget.
pub fn decoded_credit(wire: u64, limits: &IpcLimits) -> u64 {
    wire.saturating_mul(DECODED_PER_WIRE_BYTE)
        .min(bytes_of(limits.max_decoded_bytes()))
}

/// Wire + decoded credit for a frame of exactly `declared` bytes.
pub fn frame_credit(declared: usize, limits: &IpcLimits) -> u64 {
    let wire = bytes_of(declared);
    wire.saturating_add(decoded_credit(wire, limits))
}

/// Credit reserved before dispatching `plan`.
pub fn read_credit(plan: &ReadPlan, limits: &IpcLimits) -> u64 {
    let records = if plan.source_slots.is_empty() {
        limits.max_records()
    } else {
        plan.source_slots.len().min(limits.max_records())
    };
    let wire = BATCH_ENVELOPE_WIRE_BYTES
        .saturating_add(bytes_of(records).saturating_mul(RECORD_WIRE_BYTES))
        .min(bytes_of(limits.max_frame_bytes()));
    wire.saturating_add(decoded_credit(wire, limits))
}

/// Credit reserved before dispatching a discover request.
pub fn discover_credit(limits: &IpcLimits) -> u64 {
    let wire = DISCOVER_INITIAL_WIRE_BYTES.min(bytes_of(limits.max_frame_bytes()));
    wire.saturating_add(decoded_credit(wire, limits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::{adapter_id, read_group};
    use uuid::Uuid;

    fn plan(slots: Vec<u32>) -> ReadPlan {
        ReadPlan {
            request_id: Uuid::new_v4(),
            adapter_id: adapter_id("nvml"),
            host_generation: 1,
            catalog_revision: 1,
            config_revision: 1,
            cycle_id: 1,
            read_group: read_group("fast"),
            source_slots: slots,
            deadline_boot_ns: 1,
            epoch: 1,
            page: 0,
        }
    }

    #[test]
    fn whole_group_plans_reserve_the_bounded_worst_case() {
        let limits = IpcLimits::default();
        let whole = read_credit(&plan(Vec::new()), &limits);
        // 1 MiB wire + 3 MiB decoded: fits the 5 MiB per-request ceiling.
        assert_eq!(whole, (1 << 20) + 3 * (1 << 20));
        assert!(whole <= (1 << 20) + (4 << 20));
        let tight = IpcLimits::new(65_536, 65_536, 16).unwrap();
        assert_eq!(read_credit(&plan(Vec::new()), &tight), 65_536 * 2);
    }

    #[test]
    fn frame_and_discover_credit() {
        let limits = IpcLimits::default();
        assert_eq!(frame_credit(1000, &limits), 4000);
        assert_eq!(frame_credit(1 << 20, &limits), (1 << 20) + (3 << 20));
        assert_eq!(discover_credit(&limits), 4 * 64 * 1024);
    }
}
