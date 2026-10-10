//! Profile mapping vocabulary shared by the hwmon, thermal and powercap
//! profiles, and its resolution into typed identities.

use dgxtop_core::identity::{EntityId, MetricKey, OriginId, OriginIdentity};
use dgxtop_core::naming::entity;
use dgxtop_core::quality::Quality;

use super::engine::ScanContext;
use super::sysfs::shared_entity_draft;
use super::table::EntityDraft;

/// Identity of a versioned profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProfileSpec {
    pub id: &'static str,
    pub revision: u32,
}

/// Where a channel's records go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// Profile-proven metric key parts.
    Proven {
        entity: String,
        metric: &'static str,
        scope: String,
    },
    /// Diagnostics only (`sensor.*`, entity `sensor:<adapter>:<device>:<channel>`).
    Diagnostic { metric: &'static str },
}

/// Classification of one channel by a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub target: Target,
    /// Confirmed physical origin id; `None` = unknown independence.
    pub origin: Option<String>,
    pub quality: Quality,
    pub profile: ProfileSpec,
    pub rank: u16,
}

/// A mapping resolved into typed identities.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedTarget {
    pub entity_id: EntityId,
    pub record_key: MetricKey,
    pub proven: bool,
    pub origin: OriginIdentity,
    /// Shared entity record for proven targets (`cpu.package.N`, `soc`).
    pub shared_entity: Option<EntityDraft>,
}

/// Resolves `mapping`; diagnostics use entity `sensor:<adapter>:<device>:<channel>`
/// with scope `sensor`. `None` when an identity would be invalid.
pub(crate) fn resolve(
    context: &ScanContext<'_>,
    mapping: &Mapping,
    adapter: &str,
    device_key: &str,
    sensor_channel: &str,
) -> Option<ResolvedTarget> {
    let origin = match &mapping.origin {
        Some(origin) => OriginIdentity::confirmed(OriginId::new(origin.as_str()).ok()?),
        None => OriginIdentity::unknown(),
    };
    match &mapping.target {
        Target::Proven {
            entity: entity_text,
            metric,
            scope,
        } => {
            let entity_id = EntityId::new(entity_text.as_str()).ok()?;
            let record_key = context.metric_key(&entity_id, metric, scope)?;
            let shared_entity = shared_entity_draft(&entity_id);
            Some(ResolvedTarget {
                entity_id,
                record_key,
                proven: true,
                origin,
                shared_entity,
            })
        }
        Target::Diagnostic { metric } => {
            let entity_id = entity::sensor(adapter, device_key, sensor_channel).ok()?;
            let record_key = context.metric_key(&entity_id, metric, entity::SCOPE_SENSOR)?;
            Some(ResolvedTarget {
                entity_id,
                record_key,
                proven: false,
                origin,
                shared_entity: None,
            })
        }
    }
}
