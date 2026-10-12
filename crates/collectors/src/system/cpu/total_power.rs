//! The host's total power (D-106): what every CPU package and every GPU
//! draw together, `host.power.total` on the host. The CPU collector derives
//! it whenever it publishes a CPU package's power, adding the latest
//! committed draw of each GPU the catalog lists (`CollectionPlan::committed`),
//! so the total follows the CPU's readings. It holds a value only while
//! every part has a current one: a part without one leaves the total
//! unavailable, never a partial sum. A host without a GPU has no total; it
//! would only repeat the CPU's power.

use std::collections::BTreeMap;

use dgxtop_core::canonical::{CanonicalValue, CatalogSnapshot, SnapshotEntry};
use dgxtop_core::identity::{EntityId, IdentityError, MetricKey, Scope};
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;
use dgxtop_core::observation::EntityKind;
use dgxtop_core::quality::Freshness;

use crate::shared::context::Session;
use crate::shared::reasons;
use crate::shared::values::{self, Evidence};

/// What a job reads of the latest snapshot (`Collector::committed_metrics`):
/// each GPU's draw, and the CPU packages' power a job did not read again.
pub(super) const COMMITTED_METRICS: [&str; 2] = [ids::GPU_POWER_DRAW, ids::CPU_POWER_PACKAGE];

/// One part of the total: its value now, or why it has none.
type Part = Result<(Scalar, Evidence), &'static str>;

/// Publishes the total when this job published a CPU package's power.
pub(super) fn publish(session: &mut Session<'_>) {
    let Some(reference) = session
        .published(ids::CPU_POWER_PACKAGE)
        .next()
        .map(|value| value.key.clone())
    else {
        return;
    };
    let gpus = live_gpus(&session.plan().catalog);
    if gpus.is_empty() {
        return;
    }
    let Ok(key) = total_key(&reference) else {
        return;
    };
    let mut parts = cpu_parts(session);
    parts.extend(gpus.into_iter().map(|gpu| gpu_part(session, gpu)));
    if let Some(value) = total(session, key, &parts) {
        session.publish_value(value);
    }
}

/// The GPUs the catalog lists and has not seen removed.
fn live_gpus(catalog: &CatalogSnapshot) -> Vec<&EntityId> {
    catalog
        .entities
        .values()
        .filter(|record| record.kind == EntityKind::Gpu && record.removed_boot_ns.is_none())
        .map(|record| &record.entity_id)
        .collect()
}

/// `host.power.total` on the host, in the boot of `reference`.
fn total_key(reference: &MetricKey) -> Result<MetricKey, IdentityError> {
    values::with_series(
        reference,
        EntityId::new(entity::HOST)?,
        ids::HOST_POWER_TOTAL,
        Scope::new(entity::SCOPE_HOST)?,
    )
}

/// Every CPU package's power: as this job published it, else as the
/// latest snapshot holds it (a package this job did not read).
fn cpu_parts(session: &Session<'_>) -> Vec<Part> {
    let now = session.now();
    let mut packages: BTreeMap<&MetricKey, Part> = BTreeMap::new();
    for entry in session.plan().committed.of_metric(ids::CPU_POWER_PACKAGE) {
        packages.insert(&entry.sample.key, committed_part(entry, now));
    }
    for value in session.published(ids::CPU_POWER_PACKAGE) {
        packages.insert(&value.key, published_part(value));
    }
    packages.into_values().collect()
}

/// A GPU's draw as the latest snapshot holds it.
fn gpu_part(session: &Session<'_>, gpu: &EntityId) -> Part {
    session
        .plan()
        .committed
        .of_metric(ids::GPU_POWER_DRAW)
        .find(|entry| &entry.sample.key.entity_id == gpu)
        .map_or(Err(reasons::MISSING_INPUT), |entry| {
            committed_part(entry, session.now())
        })
}

/// A value this job published counts while it has one.
fn published_part(value: &CanonicalValue) -> Part {
    let scalar = value
        .value
        .filter(|_| value.selection_state.has_value())
        .ok_or(reasons::MISSING_INPUT)?;
    let evidence = Evidence::of_value(value).ok_or(reasons::MISSING_INPUT)?;
    Ok((scalar, evidence))
}

/// A committed value counts while it has one that is not stale at `now`.
fn committed_part(entry: &SnapshotEntry, now: u64) -> Part {
    let sample = &entry.sample;
    let scalar = sample
        .value
        .filter(|_| sample.selection_state.has_value())
        .ok_or(reasons::MISSING_INPUT)?;
    if entry
        .stale_after_boot_ns
        .is_none_or(|deadline| deadline <= now)
    {
        return Err(reasons::STALE);
    }
    let evidence = Evidence::of_sample(sample).ok_or(reasons::MISSING_INPUT)?;
    Ok((scalar, evidence))
}

/// The sum of `parts` (the CPU's first, so a CPU package's source names
/// the total), or the total unavailable with the reason the first part
/// without a value has.
fn total(session: &Session<'_>, key: MetricKey, parts: &[Part]) -> Option<CanonicalValue> {
    let present: Vec<&(Scalar, Evidence)> =
        parts.iter().filter_map(|part| part.as_ref().ok()).collect();
    let evidence: Vec<Evidence> = present
        .iter()
        .map(|(_, evidence)| evidence.clone())
        .collect();
    let context = session.context();
    let unavailable = |code: &str| {
        values::unavailable(
            context,
            key.clone(),
            &evidence,
            &[reasons::FORMULA_HOST_POWER_TOTAL, code],
            Freshness::Unknown,
        )
    };
    if let Some(code) = parts.iter().find_map(|part| part.as_ref().err()) {
        return unavailable(code);
    }
    let mut watts = present.iter().map(|(value, _)| *value);
    let sum = watts
        .next()
        .and_then(|first| watts.try_fold(first, values::sum));
    match sum {
        Some(sum) => values::derived(
            context,
            key.clone(),
            sum,
            &evidence,
            &[reasons::FORMULA_HOST_POWER_TOTAL],
        ),
        None => unavailable(reasons::VALUE_OVERFLOW),
    }
}
