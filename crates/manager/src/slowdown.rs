//! The GPUs their driver slows down (D-146): what the banner of every view
//! says, so a job that runs slower for heat is not found out days later.
//!
//! The driver reports why it holds a GPU's clocks down each time it is
//! read, and a reason comes and goes within seconds. A GPU therefore
//! stays listed for [`HOLD_NS`] after its last such reading, with every
//! reason seen meanwhile, so the banner neither comes nor goes with each
//! sample. The clock beside it is the one read with the last slowdown.

use std::collections::BTreeMap;

use dgxtop_core::canonical::{ImmutableSnapshot, SnapshotEntry};
use dgxtop_core::clock::NANOS_PER_SECOND;
use dgxtop_core::gpu_state::slowing_reasons;
use dgxtop_core::identity::EntityId;
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;

/// How long a GPU stays listed after its last slowed reading.
const HOLD_NS: u64 = 10 * NANOS_PER_SECOND;
/// The clock domain whose share of its maximum is told: the shader cores'.
const CLOCK_DOMAIN: &str = "sm";

/// A GPU its driver slows down, or did within the hold.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuSlowdown {
    pub gpu: EntityId,
    /// The reason bits that slow work down
    /// (`gpu_state::THROTTLE_REASONS`), every one seen during the hold.
    pub reasons: u64,
    /// The shader clock read with the last slowdown and its maximum, MHz;
    /// `None` where either is not read.
    pub clock_mhz: Option<(f64, f64)>,
    /// The power limit in force and the highest one the board allows, W,
    /// so a power cap names the limit it runs at; `None` where not read.
    pub power_limit_w: Option<f64>,
    pub power_limit_maximum_w: Option<f64>,
    /// The GPU's temperature read with the last slowdown, °C.
    pub temperature_c: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct Held {
    slowdown: GpuSlowdown,
    last_seen_boot_ns: u64,
}

/// The GPUs slowed down now or within the hold.
#[derive(Debug, Default)]
pub(crate) struct GpuSlowdowns {
    held: BTreeMap<EntityId, Held>,
    /// The snapshot looked at last: one is counted once.
    revision: Option<u64>,
}

impl GpuSlowdowns {
    /// Looks at `snapshot` at `now_boot_ns` and returns the GPUs slowed
    /// down now or within the hold, in id order.
    pub(crate) fn observe(
        &mut self,
        snapshot: &ImmutableSnapshot,
        now_boot_ns: u64,
    ) -> Vec<GpuSlowdown> {
        if self.revision != Some(snapshot.revision) {
            self.revision = Some(snapshot.revision);
            self.note(snapshot, now_boot_ns);
        }
        self.held
            .retain(|_, held| now_boot_ns.saturating_sub(held.last_seen_boot_ns) <= HOLD_NS);
        self.held
            .values()
            .map(|held| held.slowdown.clone())
            .collect()
    }

    fn note(&mut self, snapshot: &ImmutableSnapshot, now_boot_ns: u64) {
        let entries = || {
            snapshot
                .domains
                .values()
                .flat_map(|domain| domain.samples.values())
        };
        for entry in entries() {
            let key = &entry.sample.key;
            if key.metric_id.as_str() != ids::GPU_THROTTLE_REASONS {
                continue;
            }
            let slowing = current(entry)
                .and_then(Scalar::as_u64)
                .map_or(0, slowing_reasons);
            if slowing == 0 {
                continue;
            }
            let gpu = &key.entity_id;
            let read = |metric: &str, scope: &str| reading(snapshot, gpu, metric, scope);
            let clock_mhz = read(ids::GPU_CLOCK_SM, entity::SCOPE_GPU)
                .zip(read(ids::GPU_CLOCK_MAXIMUM, CLOCK_DOMAIN));
            let power_limit_w = read(ids::GPU_POWER_LIMIT, entity::SCOPE_GPU);
            let power_limit_maximum_w = read(ids::GPU_POWER_LIMIT_MAXIMUM, entity::SCOPE_GPU);
            let temperature_c = read(ids::GPU_TEMPERATURE, entity::SCOPE_GPU);
            let expired = self
                .held
                .get(&key.entity_id)
                .is_none_or(|held| now_boot_ns.saturating_sub(held.last_seen_boot_ns) > HOLD_NS);
            let earlier = if expired {
                0
            } else {
                self.held
                    .get(&key.entity_id)
                    .map_or(0, |held| held.slowdown.reasons)
            };
            self.held.insert(
                key.entity_id.clone(),
                Held {
                    slowdown: GpuSlowdown {
                        gpu: key.entity_id.clone(),
                        reasons: earlier | slowing,
                        clock_mhz,
                        power_limit_w,
                        power_limit_maximum_w,
                        temperature_c,
                    },
                    last_seen_boot_ns: now_boot_ns,
                },
            );
        }
    }
}

/// An entry's value while it is a current reading.
fn current(entry: &SnapshotEntry) -> Option<Scalar> {
    entry
        .sample
        .value
        .filter(|_| entry.sample.selection_state.has_value())
}

/// The current, positive reading of `metric` in `scope` of `gpu`.
fn reading(snapshot: &ImmutableSnapshot, gpu: &EntityId, metric: &str, scope: &str) -> Option<f64> {
    snapshot
        .domains
        .values()
        .flat_map(|domain| domain.samples.values())
        .find(|entry| {
            let key = &entry.sample.key;
            key.entity_id == *gpu && key.metric_id.as_str() == metric && key.scope.as_str() == scope
        })
        .and_then(|entry| current(entry))
        .map(Scalar::to_f64_lossy)
        .filter(|value| value.is_finite() && *value > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::{metric_key, snapshot_entry, snapshot_of};

    const GPU: &str = "gpu:GPU-aaaa1111";
    const SECOND: u64 = NANOS_PER_SECOND;

    /// A snapshot of one GPU: its reason bits, its clock and its maximum.
    fn snapshot(revision: u64, reasons: u64, clock: Option<f64>) -> ImmutableSnapshot {
        let mut bits = snapshot_entry(
            &metric_key(GPU, ids::GPU_THROTTLE_REASONS, "gpu"),
            None,
            SECOND,
        );
        bits.sample.value = Some(Scalar::from_u64(reasons));
        bits.sample.selection_state = dgxtop_core::quality::SelectionState::Selected;
        let mut entries = vec![("gpu", bits)];
        if let Some(clock) = clock {
            entries.push((
                "gpu",
                snapshot_entry(
                    &metric_key(GPU, ids::GPU_CLOCK_SM, "gpu"),
                    Some(clock),
                    SECOND,
                ),
            ));
            entries.push((
                "gpu",
                snapshot_entry(
                    &metric_key(GPU, ids::GPU_CLOCK_MAXIMUM, "sm"),
                    Some(3000.0),
                    SECOND,
                ),
            ));
        }
        snapshot_of(revision, entries)
    }

    /// A GPU is listed from its first slowed reading, with its clock; idle
    /// and set clocks list nothing.
    #[test]
    fn a_slowed_gpu_is_listed_with_its_clock() {
        let mut slowdowns = GpuSlowdowns::default();
        assert!(
            slowdowns
                .observe(&snapshot(1, 0, Some(2900.0)), SECOND)
                .is_empty()
        );
        assert!(
            slowdowns
                .observe(&snapshot(2, 0x1 | 0x2, None), 2 * SECOND)
                .is_empty()
        );
        let listed = slowdowns.observe(&snapshot(3, 0x20 | 0x1, Some(1500.0)), 3 * SECOND);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].gpu.as_str(), GPU);
        assert_eq!(listed[0].reasons, 0x20, "what slows work down only");
        assert_eq!(listed[0].clock_mhz, Some((1500.0, 3000.0)));
        // Without a clock the reason is listed alone.
        let mut bare = GpuSlowdowns::default();
        let listed = bare.observe(&snapshot(1, 0x4, None), SECOND);
        assert_eq!((listed[0].reasons, listed[0].clock_mhz), (0x4, None));
    }

    /// A reason that comes and goes keeps the GPU listed for the hold, with
    /// every reason seen meanwhile and the clock of the last slowdown; then
    /// the GPU is gone, and a later slowdown starts anew.
    #[test]
    fn a_slowdown_is_held_so_the_banner_does_not_flicker() {
        let mut slowdowns = GpuSlowdowns::default();
        slowdowns.observe(&snapshot(1, 0x4, Some(2000.0)), SECOND);
        let quiet = slowdowns.observe(&snapshot(2, 0, Some(2900.0)), 2 * SECOND);
        assert_eq!(quiet[0].reasons, 0x4, "held");
        assert_eq!(quiet[0].clock_mhz, Some((2000.0, 3000.0)));
        let both = slowdowns.observe(&snapshot(3, 0x20, Some(1500.0)), 6 * SECOND);
        assert_eq!(both[0].reasons, 0x4 | 0x20);
        assert_eq!(both[0].clock_mhz, Some((1500.0, 3000.0)));
        // The same snapshot again counts once and keeps the hold running.
        assert_eq!(
            slowdowns
                .observe(&snapshot(3, 0x20, Some(1500.0)), 16 * SECOND)
                .len(),
            1
        );
        assert!(
            slowdowns
                .observe(&snapshot(4, 0, None), 16 * SECOND + 1)
                .is_empty()
        );
        let anew = slowdowns.observe(&snapshot(5, 0x20, None), 40 * SECOND);
        assert_eq!(anew[0].reasons, 0x20, "the old reasons are not carried");
    }
}
