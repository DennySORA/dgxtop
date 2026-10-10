//! Typed Manager events (`event.schema.json`). Control and audit events are
//! never treated as droppable view events; each event is at most 64 KiB and
//! never carries a full command line.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::identity::BootId;

/// Event schema version.
pub const EVENT_SCHEMA_VERSION: u32 = 1;
/// Maximum encoded event size.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleHealthState {
    Healthy,
    Degraded,
    Unavailable,
    Quarantined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    Completed,
    Rejected,
    Unknown,
}

/// Payload for the action lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionEventPayload {
    pub request_id: Uuid,
    pub intent_id: Option<Uuid>,
    pub reason: String,
}

/// Event kind plus typed payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum EventKind {
    CatalogChanged {
        catalog_revision: u32,
        changed_count: u32,
        removed_count: u32,
    },
    CollectionCommitted {
        batch_id: Uuid,
        #[serde(with = "crate::wire::u64_decimal")]
        snapshot_revision: u64,
        sample_count: u32,
    },
    SourceChanged {
        entity_id: String,
        metric_id: String,
        previous_source: Option<String>,
        source_id: String,
        policy_revision: u32,
    },
    SourceConflict {
        entity_id: String,
        metric_id: String,
        source_ids: Vec<String>,
    },
    SourceUnavailable {
        source_id: String,
        reason: String,
    },
    ConfigApplied {
        revision: u32,
        #[serde(with = "crate::wire::u64_decimal")]
        epoch: u64,
    },
    ModuleHealthChanged {
        module_id: String,
        state: ModuleHealthState,
        reason: String,
    },
    GapRecorded {
        #[serde(with = "crate::wire::u64_decimal")]
        start_boot_ns: u64,
        #[serde(with = "crate::wire::u64_decimal")]
        end_boot_ns: u64,
        reason: String,
        affected_count: u32,
    },
    CommandCompleted {
        request_id: Uuid,
        status: CompletionStatus,
        code: String,
    },
    ActionPrepared(ActionEventPayload),
    ActionIntentCommitted(ActionEventPayload),
    ActionSignalSent(ActionEventPayload),
    ActionExitObserved(ActionEventPayload),
    ActionOutcomeUnknown(ActionEventPayload),
}

impl EventKind {
    /// Control/audit events must never be dropped to make room for telemetry.
    pub const fn is_control(&self) -> bool {
        matches!(
            self,
            Self::ConfigApplied { .. }
                | Self::CommandCompleted { .. }
                | Self::ActionPrepared(_)
                | Self::ActionIntentCommitted(_)
                | Self::ActionSignalSent(_)
                | Self::ActionExitObserved(_)
                | Self::ActionOutcomeUnknown(_)
        )
    }
}

/// One event envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub schema_version: u32,
    pub event_id: Uuid,
    pub boot_id: BootId,
    #[serde(with = "crate::wire::u64_decimal")]
    pub occurred_boot_ns: u64,
    pub correlation_id: Uuid,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl Event {
    pub fn new(
        boot_id: BootId,
        occurred_boot_ns: u64,
        correlation_id: Uuid,
        kind: EventKind,
    ) -> Self {
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            event_id: Uuid::new_v4(),
            boot_id,
            occurred_boot_ns,
            correlation_id,
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_gap_example_round_trips() {
        let text = include_str!("../../../spec/dgxtop-v2/examples/event.gap.json");
        let event: Event = serde_json::from_str(text).unwrap();
        assert!(matches!(
            event.kind,
            EventKind::GapRecorded {
                affected_count: 2,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }
}
