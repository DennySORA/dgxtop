//! Global managed-memory ledger and dual item/byte admission.
//!
//! `store.memory_limit_mib` is ONE budget for all managed dynamic data, split
//! into fixed partitions (`spec/dgxtop-v2/RUNTIME.md` §6). Every buffer, queue
//! entry, retained snapshot and IPC payload holds a [`BytePermit`] taken
//! *before* allocation; dropping the permit returns the bytes. Moving a permit
//! transfers ownership, so a payload is never counted twice or forgotten.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod accounted;

/// Bytes per MiB.
pub const MIB: u64 = 1024 * 1024;

/// Budget partitions with their share (MiB per 256 MiB of `memory_limit_mib`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Partition {
    /// Interned strings, descriptors, tombstones, profile index, committed bases.
    Catalog,
    /// Canonical hot state and the live process table.
    HotState,
    /// 300 s full-resolution canonical history.
    RawHistory,
    /// Each series' bookkeeping, its segments and the gap index (D-90).
    Aggregates,
    /// All live wire/decoded/collection buffers.
    Ingress,
    /// View projections, formatted caches and retired frames.
    Presentation,
    /// Commands/results, archive journal, log buffer and safety reserve.
    Reserve,
}

impl Partition {
    pub const ALL: [Partition; 7] = [
        Partition::Catalog,
        Partition::HotState,
        Partition::RawHistory,
        Partition::Aggregates,
        Partition::Ingress,
        Partition::Presentation,
        Partition::Reserve,
    ];

    /// MiB share at the 256 MiB reference budget.
    pub const fn reference_mib(self) -> u64 {
        match self {
            Self::Catalog => 24,
            Self::HotState => 16,
            // RUNTIME.md §6's 32 MiB plus what the aggregates and the
            // removed history query partition no longer hold (D-98): raw
            // steps are the one share that grows with the sampling rate and
            // the series count.
            Self::RawHistory => 144,
            // No minute buckets any more (D-90), only bookkeeping, segments
            // and gaps (D-98).
            Self::Aggregates => 32,
            Self::Ingress => 8,
            Self::Presentation => 12,
            Self::Reserve => 20,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Catalog => 0,
            Self::HotState => 1,
            Self::RawHistory => 2,
            Self::Aggregates => 3,
            Self::Ingress => 4,
            Self::Presentation => 5,
            Self::Reserve => 6,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::HotState => "hot_state",
            Self::RawHistory => "raw_history",
            Self::Aggregates => "aggregates",
            Self::Ingress => "ingress",
            Self::Presentation => "presentation",
            Self::Reserve => "reserve",
        }
    }
}

/// Byte limits per partition. Scaling is exact: `share_mib * limit_mib * 4096`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionLimits {
    limits: [u64; Partition::ALL.len()],
}

impl PartitionLimits {
    /// Limits for a total budget of `memory_limit_mib` MiB.
    pub fn for_memory_limit_mib(memory_limit_mib: u64) -> Self {
        let mut limits = [0u64; Partition::ALL.len()];
        for partition in Partition::ALL {
            // share/256 of the budget; MIB/256 == 4096 keeps this exact.
            limits[partition.index()] = partition
                .reference_mib()
                .saturating_mul(memory_limit_mib)
                .saturating_mul(MIB / 256);
        }
        Self { limits }
    }

    pub const fn limit(&self, partition: Partition) -> u64 {
        self.limits[partition.index()]
    }

    pub fn total(&self) -> u64 {
        self.limits
            .iter()
            .fold(0u64, |sum, limit| sum.saturating_add(*limit))
    }
}

/// Admission failure; callers surface a visible capacity state instead of OOM.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CapacityError {
    #[error("{partition} partition full: requested {requested} bytes, used {used} of {limit}")]
    BytesExhausted {
        partition: &'static str,
        requested: u64,
        used: u64,
        limit: u64,
    },
    #[error("queue {queue} full: {limit} items")]
    ItemsExhausted { queue: &'static str, limit: usize },
}

struct PartitionCounters {
    used: AtomicU64,
    peak: AtomicU64,
    denied: AtomicU64,
}

impl PartitionCounters {
    const fn new() -> Self {
        Self {
            used: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            denied: AtomicU64::new(0),
        }
    }
}

struct LedgerInner {
    limits: PartitionLimits,
    counters: [PartitionCounters; Partition::ALL.len()],
}

impl LedgerInner {
    fn try_take(&self, partition: Partition, bytes: u64) -> Result<(), CapacityError> {
        let counters = &self.counters[partition.index()];
        let limit = self.limits.limit(partition);
        let mut current = counters.used.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(bytes).filter(|next| *next <= limit);
            let Some(next) = next else {
                counters.denied.fetch_add(1, Ordering::Relaxed);
                return Err(CapacityError::BytesExhausted {
                    partition: partition.name(),
                    requested: bytes,
                    used: current,
                    limit,
                });
            };
            match counters.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    counters.peak.fetch_max(next, Ordering::Relaxed);
                    return Ok(());
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn give_back(&self, partition: Partition, bytes: u64) {
        let counters = &self.counters[partition.index()];
        // Saturating: a release can never exceed what was taken by construction,
        // the saturation only guards against arithmetic surprises.
        let mut current = counters.used.load(Ordering::Acquire);
        loop {
            let next = current.saturating_sub(bytes);
            match counters.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

/// Shared global ledger handle (cheap to clone).
#[derive(Clone)]
pub struct ByteLedger {
    inner: Arc<LedgerInner>,
}

impl fmt::Debug for ByteLedger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ByteLedger")
            .field("usage", &self.snapshot())
            .finish()
    }
}

impl ByteLedger {
    pub fn new(limits: PartitionLimits) -> Self {
        Self {
            inner: Arc::new(LedgerInner {
                limits,
                counters: std::array::from_fn(|_| PartitionCounters::new()),
            }),
        }
    }

    /// Ledger sized for `memory_limit_mib`.
    pub fn for_memory_limit_mib(memory_limit_mib: u64) -> Self {
        Self::new(PartitionLimits::for_memory_limit_mib(memory_limit_mib))
    }

    /// Reserves `bytes` before the caller allocates. Never over-commits.
    pub fn try_reserve(
        &self,
        partition: Partition,
        bytes: u64,
    ) -> Result<BytePermit, CapacityError> {
        self.inner.try_take(partition, bytes)?;
        Ok(BytePermit {
            ledger: Arc::clone(&self.inner),
            partition,
            bytes,
        })
    }

    pub fn limits(&self) -> PartitionLimits {
        self.inner.limits
    }

    /// Current usage of one partition.
    pub fn used(&self, partition: Partition) -> u64 {
        self.inner.counters[partition.index()]
            .used
            .load(Ordering::Acquire)
    }

    /// Fraction in `[0, 1]` of one partition currently used.
    #[allow(clippy::cast_precision_loss)]
    pub fn utilization(&self, partition: Partition) -> f64 {
        let limit = self.inner.limits.limit(partition);
        if limit == 0 {
            return 1.0;
        }
        self.used(partition) as f64 / limit as f64
    }

    /// Fraction of the whole budget currently used.
    #[allow(clippy::cast_precision_loss)]
    pub fn total_utilization(&self) -> f64 {
        let total = self.inner.limits.total();
        if total == 0 {
            return 1.0;
        }
        let used: u64 = Partition::ALL
            .iter()
            .map(|partition| self.used(*partition))
            .sum();
        used as f64 / total as f64
    }

    pub fn snapshot(&self) -> LedgerSnapshot {
        let partitions = Partition::ALL
            .iter()
            .map(|partition| {
                let counters = &self.inner.counters[partition.index()];
                PartitionUsage {
                    partition: *partition,
                    used: counters.used.load(Ordering::Acquire),
                    peak: counters.peak.load(Ordering::Relaxed),
                    limit: self.inner.limits.limit(*partition),
                    denied_total: counters.denied.load(Ordering::Relaxed),
                }
            })
            .collect::<Vec<_>>();
        let used = partitions.iter().map(|usage| usage.used).sum();
        LedgerSnapshot {
            partitions,
            used,
            limit: self.inner.limits.total(),
        }
    }
}

/// RAII byte reservation. Dropping releases; moving transfers ownership.
pub struct BytePermit {
    ledger: Arc<LedgerInner>,
    partition: Partition,
    bytes: u64,
}

impl fmt::Debug for BytePermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BytePermit")
            .field("partition", &self.partition)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl BytePermit {
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub const fn partition(&self) -> Partition {
        self.partition
    }

    /// Grows the reservation in place, failing without side effects.
    pub fn try_grow(&mut self, additional: u64) -> Result<(), CapacityError> {
        self.ledger.try_take(self.partition, additional)?;
        self.bytes = self.bytes.saturating_add(additional);
        Ok(())
    }

    /// Releases everything above `bytes`, e.g. after decoding proved a smaller size.
    pub fn shrink_to(&mut self, bytes: u64) {
        if bytes < self.bytes {
            self.ledger.give_back(self.partition, self.bytes - bytes);
            self.bytes = bytes;
        }
    }

    /// Splits `bytes` off into a new permit owned by another holder.
    pub fn split_off(&mut self, bytes: u64) -> Option<BytePermit> {
        if bytes > self.bytes {
            return None;
        }
        self.bytes -= bytes;
        Some(BytePermit {
            ledger: Arc::clone(&self.ledger),
            partition: self.partition,
            bytes,
        })
    }

    /// Absorbs another permit of the same partition and ledger.
    pub fn merge(&mut self, mut other: BytePermit) -> Result<(), BytePermit> {
        if other.partition != self.partition || !Arc::ptr_eq(&other.ledger, &self.ledger) {
            return Err(other);
        }
        self.bytes = self.bytes.saturating_add(other.bytes);
        other.bytes = 0;
        Ok(())
    }
}

impl Drop for BytePermit {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.ledger.give_back(self.partition, self.bytes);
        }
    }
}

/// Usage of one partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionUsage {
    pub partition: Partition,
    pub used: u64,
    pub peak: u64,
    pub limit: u64,
    pub denied_total: u64,
}

/// Point-in-time ledger usage for diagnostics and overload control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    pub partitions: Vec<PartitionUsage>,
    pub used: u64,
    pub limit: u64,
}

/// Dual admission for one queue: bounded item count AND bytes from a partition.
/// Element-count bounds alone never limit payload size.
#[derive(Clone)]
pub struct QueueGate {
    name: &'static str,
    item_limit: usize,
    byte_limit: u64,
    items: Arc<AtomicUsize>,
    bytes_in_queue: Arc<AtomicU64>,
    ledger: ByteLedger,
    partition: Partition,
}

impl fmt::Debug for QueueGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueueGate")
            .field("name", &self.name)
            .field("items", &self.items.load(Ordering::Relaxed))
            .field("item_limit", &self.item_limit)
            .field("bytes", &self.bytes_in_queue.load(Ordering::Relaxed))
            .field("byte_limit", &self.byte_limit)
            .finish()
    }
}

impl QueueGate {
    /// `byte_limit` is the queue's own cap (e.g. 8 MiB ingress); the partition
    /// ledger is checked as well so the global budget always holds.
    pub fn new(
        name: &'static str,
        item_limit: usize,
        byte_limit: u64,
        ledger: ByteLedger,
        partition: Partition,
    ) -> Self {
        Self {
            name,
            item_limit,
            byte_limit,
            items: Arc::new(AtomicUsize::new(0)),
            bytes_in_queue: Arc::new(AtomicU64::new(0)),
            ledger,
            partition,
        }
    }

    /// Admits one item of `bytes`, or rejects without side effects.
    pub fn try_admit(&self, bytes: u64) -> Result<QueueTicket, CapacityError> {
        let mut current = self.items.load(Ordering::Acquire);
        loop {
            if current >= self.item_limit {
                return Err(CapacityError::ItemsExhausted {
                    queue: self.name,
                    limit: self.item_limit,
                });
            }
            match self.items.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        let mut queued = self.bytes_in_queue.load(Ordering::Acquire);
        loop {
            let next = queued
                .checked_add(bytes)
                .filter(|next| *next <= self.byte_limit);
            let Some(next) = next else {
                self.items.fetch_sub(1, Ordering::AcqRel);
                return Err(CapacityError::BytesExhausted {
                    partition: self.name,
                    requested: bytes,
                    used: queued,
                    limit: self.byte_limit,
                });
            };
            match self.bytes_in_queue.compare_exchange_weak(
                queued,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => queued = observed,
            }
        }
        match self.ledger.try_reserve(self.partition, bytes) {
            Ok(permit) => Ok(QueueTicket {
                items: Arc::clone(&self.items),
                bytes_in_queue: Arc::clone(&self.bytes_in_queue),
                queued_bytes: bytes,
                left_queue: false,
                permit,
            }),
            Err(error) => {
                self.items.fetch_sub(1, Ordering::AcqRel);
                self.bytes_in_queue.fetch_sub(bytes, Ordering::AcqRel);
                Err(error)
            }
        }
    }

    pub fn len(&self) -> usize {
        self.items.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn queued_bytes(&self) -> u64 {
        self.bytes_in_queue.load(Ordering::Acquire)
    }

    /// Max of item and byte utilization in `[0, 1]`.
    #[allow(clippy::cast_precision_loss)]
    pub fn utilization(&self) -> f64 {
        let items = if self.item_limit == 0 {
            1.0
        } else {
            self.len() as f64 / self.item_limit as f64
        };
        let bytes = if self.byte_limit == 0 {
            1.0
        } else {
            self.queued_bytes() as f64 / self.byte_limit as f64
        };
        items.max(bytes)
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }
}

/// One admitted queue slot; holds its bytes until dropped (after hand-off).
pub struct QueueTicket {
    items: Arc<AtomicUsize>,
    bytes_in_queue: Arc<AtomicU64>,
    queued_bytes: u64,
    left_queue: bool,
    permit: BytePermit,
}

impl fmt::Debug for QueueTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueueTicket")
            .field("bytes", &self.queued_bytes)
            .finish()
    }
}

impl QueueTicket {
    pub const fn bytes(&self) -> u64 {
        self.queued_bytes
    }

    /// Leaves the queue but keeps the bytes accounted (ownership moved on).
    pub fn into_permit(mut self) -> BytePermit {
        self.release_queue_slot();
        let permit = BytePermit {
            ledger: Arc::clone(&self.permit.ledger),
            partition: self.permit.partition,
            bytes: self.permit.bytes,
        };
        self.permit.bytes = 0;
        permit
    }

    fn release_queue_slot(&mut self) {
        if !self.left_queue {
            self.items.fetch_sub(1, Ordering::AcqRel);
            self.bytes_in_queue
                .fetch_sub(self.queued_bytes, Ordering::AcqRel);
            self.left_queue = true;
        }
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.release_queue_slot();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn partitions_scale_exactly_and_sum_to_total() {
        let reference = PartitionLimits::for_memory_limit_mib(256);
        assert_eq!(reference.total(), 256 * MIB);
        assert_eq!(reference.limit(Partition::RawHistory), 144 * MIB);
        assert_eq!(reference.limit(Partition::Aggregates), 32 * MIB);
        assert_eq!(reference.limit(Partition::Ingress), 8 * MIB);
        let small = PartitionLimits::for_memory_limit_mib(64);
        assert_eq!(small.total(), 64 * MIB);
        assert_eq!(small.limit(Partition::Ingress), 2 * MIB);
        let odd = PartitionLimits::for_memory_limit_mib(100);
        assert_eq!(odd.total(), 100 * MIB);
    }

    #[test]
    fn reserve_release_and_never_overcommit() {
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let first = ledger.try_reserve(Partition::Ingress, 5 * MIB).unwrap();
        // 5 MiB batch fits; another 5 MiB does not (8 MiB partition).
        assert!(ledger.try_reserve(Partition::Ingress, 5 * MIB).is_err());
        let second = ledger.try_reserve(Partition::Ingress, 3 * MIB).unwrap();
        assert_eq!(ledger.used(Partition::Ingress), 8 * MIB);
        assert!(ledger.try_reserve(Partition::Ingress, 1).is_err());
        drop(first);
        assert_eq!(ledger.used(Partition::Ingress), 3 * MIB);
        drop(second);
        assert_eq!(ledger.used(Partition::Ingress), 0);
        let snapshot = ledger.snapshot();
        let ingress = snapshot
            .partitions
            .iter()
            .find(|usage| usage.partition == Partition::Ingress)
            .unwrap();
        assert_eq!(ingress.peak, 8 * MIB);
        assert_eq!(ingress.denied_total, 2);
    }

    #[test]
    fn transfer_split_merge_keep_totals() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let mut permit = ledger.try_reserve(Partition::Presentation, 1000).unwrap();
        let part = permit.split_off(400).unwrap();
        assert_eq!(permit.bytes(), 600);
        assert_eq!(part.bytes(), 400);
        assert_eq!(ledger.used(Partition::Presentation), 1000);
        // Moving to another owner keeps the bytes accounted.
        let moved = std::thread::spawn(move || part).join().unwrap();
        assert_eq!(ledger.used(Partition::Presentation), 1000);
        permit.merge(moved).unwrap();
        assert_eq!(permit.bytes(), 1000);
        permit.shrink_to(100);
        assert_eq!(ledger.used(Partition::Presentation), 100);
        assert!(permit.split_off(101).is_none());
        let other = ledger.try_reserve(Partition::Catalog, 1).unwrap();
        assert!(permit.merge(other).is_err());
    }

    #[test]
    fn queue_gate_enforces_items_and_bytes_together() {
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let gate = QueueGate::new("ingress", 32, 8 * MIB, ledger.clone(), Partition::Ingress);
        // Many small batches hit the byte cap before the item cap.
        let mut tickets = Vec::new();
        for _ in 0..8 {
            tickets.push(gate.try_admit(MIB).unwrap());
        }
        assert!(matches!(
            gate.try_admit(1),
            Err(CapacityError::BytesExhausted { .. })
        ));
        tickets.clear();
        assert_eq!(gate.queued_bytes(), 0);
        assert_eq!(ledger.used(Partition::Ingress), 0);
        for _ in 0..32 {
            tickets.push(gate.try_admit(1).unwrap());
        }
        assert!(matches!(
            gate.try_admit(1),
            Err(CapacityError::ItemsExhausted { .. })
        ));
        let permit = tickets.pop().unwrap().into_permit();
        assert_eq!(gate.len(), 31);
        assert_eq!(ledger.used(Partition::Ingress), 32);
        drop(permit);
        assert_eq!(ledger.used(Partition::Ingress), 31);
    }

    #[test]
    fn concurrent_reservations_never_exceed_limit() {
        let ledger = ByteLedger::for_memory_limit_mib(64);
        let limit = ledger.limits().limit(Partition::Ingress);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let ledger = ledger.clone();
                std::thread::spawn(move || {
                    let mut held = Vec::new();
                    for _ in 0..2000 {
                        if let Ok(permit) = ledger.try_reserve(Partition::Ingress, 4096) {
                            held.push(permit);
                        }
                        if held.len() > 16 {
                            held.remove(0);
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let snapshot = ledger.snapshot();
        let ingress = snapshot
            .partitions
            .iter()
            .find(|usage| usage.partition == Partition::Ingress)
            .unwrap();
        assert!(ingress.peak <= limit);
        assert_eq!(ingress.used, 0);
    }

    proptest! {
        #[test]
        fn random_reserve_release_balances(sizes in proptest::collection::vec(0u64..2_000_000, 1..64)) {
            let ledger = ByteLedger::for_memory_limit_mib(64);
            let limit = ledger.limits().limit(Partition::RawHistory);
            let mut held = Vec::new();
            let mut expected = 0u64;
            for size in sizes {
                match ledger.try_reserve(Partition::RawHistory, size) {
                    Ok(permit) => { expected += size; held.push(permit); }
                    Err(_) => prop_assert!(expected + size > limit),
                }
                prop_assert_eq!(ledger.used(Partition::RawHistory), expected);
            }
            drop(held);
            prop_assert_eq!(ledger.used(Partition::RawHistory), 0);
        }
    }
}
