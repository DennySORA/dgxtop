//! Profile mapping vocabulary shared by the hwmon, thermal and powercap
//! profiles, and its resolution into typed identities. A channel no profile
//! proves has no mapping and is not catalogued (D-92): nothing reads an
//! unclassified sensor.

use dgxtop_core::identity::{EntityId, MetricKey, OriginId, OriginIdentity};
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

/// The metric key parts a profile proves for a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub entity: String,
    pub metric: &'static str,
    pub scope: String,
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
    pub origin: OriginIdentity,
    /// Shared entity record (`cpu.package.N`, `soc`).
    pub shared_entity: Option<EntityDraft>,
}

/// Resolves `mapping`; `None` when an identity would be invalid.
pub(crate) fn resolve(context: &ScanContext<'_>, mapping: &Mapping) -> Option<ResolvedTarget> {
    let origin = match &mapping.origin {
        Some(origin) => OriginIdentity::confirmed(OriginId::new(origin.as_str()).ok()?),
        None => OriginIdentity::unknown(),
    };
    let target = &mapping.target;
    let entity_id = EntityId::new(target.entity.as_str()).ok()?;
    let record_key = context.metric_key(&entity_id, target.metric, &target.scope)?;
    let shared_entity = shared_entity_draft(&entity_id);
    Some(ResolvedTarget {
        entity_id,
        record_key,
        origin,
        shared_entity,
    })
}
