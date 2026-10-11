//! Which GPU processes have a color (D-64). A process keeps its slot while
//! it is tracked, so its color never follows its rank:
//!
//! - a newly listed process takes the free slot freed longest ago, so a
//!   color just given up is not reused while another one is free;
//! - with no slot free it takes the slot of the process listed longest ago
//!   among those no longer listed (an exited process);
//! - with every holder still listed it takes the slot of the smallest one
//!   only when it holds at least twice as much and 256 MiB more, so two
//!   processes of similar size never take turns;
//! - a holder not listed for a day gives its slot up.

use std::collections::{BTreeMap, VecDeque};

use dgxtop_core::identity::ProcessKey;
use dgxtop_core::view::PROCESS_COLOR_SLOTS;

/// A newcomer takes a listed holder's slot at this many times its memory.
const DISPLACE_FACTOR: u64 = 2;
/// ... and at least this much more.
const DISPLACE_MIN_BYTES: u64 = 256 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Holder {
    key: ProcessKey,
    /// The newest process list names it with GPU memory.
    listed: bool,
    last_listed_ns: u64,
}

/// The slots and who holds them.
#[derive(Debug, Clone)]
pub(super) struct ColorSlots {
    holders: Vec<Option<Holder>>,
    /// Free slots, the one freed longest ago first.
    free: VecDeque<u8>,
}

impl Default for ColorSlots {
    fn default() -> Self {
        Self {
            holders: vec![None; PROCESS_COLOR_SLOTS],
            free: (0..PROCESS_COLOR_SLOTS)
                .filter_map(|slot| u8::try_from(slot).ok())
                .collect(),
        }
    }
}

impl ColorSlots {
    pub(super) fn slot_of(&self, key: &ProcessKey) -> Option<u8> {
        self.holders
            .iter()
            .position(|holder| holder.as_ref().is_some_and(|holder| &holder.key == key))
            .and_then(|slot| u8::try_from(slot).ok())
    }

    /// Applies one process list, `listed` being each process's GPU memory
    /// (known and above zero); returns the processes that lost their slot.
    pub(super) fn update(&mut self, listed: &[(ProcessKey, u64)], now_ns: u64) -> Vec<ProcessKey> {
        let bytes: BTreeMap<&ProcessKey, u64> =
            listed.iter().map(|(key, bytes)| (key, *bytes)).collect();
        for holder in self.holders.iter_mut().flatten() {
            holder.listed = bytes.contains_key(&holder.key);
            if holder.listed {
                holder.last_listed_ns = now_ns;
            }
        }
        let mut newcomers: Vec<(&ProcessKey, u64)> = listed
            .iter()
            .filter(|(key, _)| self.slot_of(key).is_none())
            .map(|(key, bytes)| (key, *bytes))
            .collect();
        newcomers.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
        let mut lost = Vec::new();
        for (key, size) in newcomers {
            let Some(slot) = self.slot_for(size, &bytes, &mut lost) else {
                // The rest hold less still.
                break;
            };
            if let Some(holder) = self.holders.get_mut(usize::from(slot)) {
                *holder = Some(Holder {
                    key: key.clone(),
                    listed: true,
                    last_listed_ns: now_ns,
                });
            }
        }
        lost
    }

    /// A slot for a newcomer holding `size` bytes (module docs), pushing
    /// the process that gives it up onto `lost`.
    fn slot_for(
        &mut self,
        size: u64,
        bytes: &BTreeMap<&ProcessKey, u64>,
        lost: &mut Vec<ProcessKey>,
    ) -> Option<u8> {
        if let Some(slot) = self.free.pop_front() {
            return Some(slot);
        }
        let exited = self
            .holders
            .iter()
            .enumerate()
            .filter_map(|(slot, holder)| Some((slot, holder.as_ref()?)))
            .filter(|(_, holder)| !holder.listed)
            .min_by_key(|(_, holder)| (holder.last_listed_ns, holder.key.clone()))
            .map(|(slot, _)| slot);
        let slot = exited.or_else(|| {
            let (slot, smallest) = self
                .holders
                .iter()
                .enumerate()
                .filter_map(|(slot, holder)| {
                    let holder = holder.as_ref()?;
                    Some((slot, bytes.get(&holder.key).copied().unwrap_or(0)))
                })
                .min_by_key(|(slot, held)| (*held, *slot))?;
            displaces(size, smallest).then_some(slot)
        })?;
        lost.extend(self.holders.get_mut(slot)?.take().map(|holder| holder.key));
        u8::try_from(slot).ok()
    }

    /// Frees the slots of processes last listed before `cutoff_ns`.
    pub(super) fn expire(&mut self, cutoff_ns: u64) -> Vec<ProcessKey> {
        let mut lost = Vec::new();
        for (slot, holder) in self.holders.iter_mut().enumerate() {
            if holder
                .as_ref()
                .is_some_and(|holder| !holder.listed && holder.last_listed_ns < cutoff_ns)
            {
                lost.extend(holder.take().map(|holder| holder.key));
                self.free.extend(u8::try_from(slot).ok());
            }
        }
        lost
    }

    /// Every holder with its slot and whether the newest list names it.
    pub(super) fn holders(&self) -> impl Iterator<Item = (u8, &ProcessKey, bool)> {
        self.holders
            .iter()
            .enumerate()
            .filter_map(|(slot, holder)| {
                let holder = holder.as_ref()?;
                Some((u8::try_from(slot).ok()?, &holder.key, holder.listed))
            })
    }
}

/// Whether a newcomer holding `size` bytes takes the slot of a listed
/// holder holding `smallest`.
fn displaces(size: u64, smallest: u64) -> bool {
    size >= smallest.saturating_mul(DISPLACE_FACTOR)
        && size.saturating_sub(smallest) >= DISPLACE_MIN_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::identity::{BootId, HostId, Pid, PidNamespace};

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    fn key(pid: i64) -> ProcessKey {
        ProcessKey {
            host_id: HostId::new("host-test").unwrap(),
            boot_id: BootId::new("boot-test").unwrap(),
            pid_namespace: PidNamespace::new("pid:[4026531836]").unwrap(),
            pid: Pid::new(pid).unwrap(),
            start_time_ticks: u64::try_from(pid).unwrap() * 10,
        }
    }

    fn slots_of(slots: &ColorSlots, pids: &[i64]) -> Vec<Option<u8>> {
        pids.iter().map(|pid| slots.slot_of(&key(*pid))).collect()
    }

    /// The largest newcomer gets the first color; colors stay with their
    /// process when the sizes change order.
    #[test]
    fn newcomers_take_free_slots_largest_first_and_keep_them() {
        let mut slots = ColorSlots::default();
        let lost = slots.update(&[(key(1), 2 * GIB), (key(2), 40 * GIB)], 1);
        assert!(lost.is_empty());
        assert_eq!(slots_of(&slots, &[2, 1]), vec![Some(0), Some(1)]);
        slots.update(&[(key(1), 60 * GIB), (key(2), GIB)], 2);
        assert_eq!(slots_of(&slots, &[2, 1]), vec![Some(0), Some(1)]);
    }

    /// A slot given up goes to the back of the free list: the next newcomer
    /// gets a color nobody had just held.
    #[test]
    fn a_freed_color_is_reused_last() {
        let mut slots = ColorSlots::default();
        slots.update(&[(key(1), GIB)], 1);
        assert_eq!(slots.slot_of(&key(1)), Some(0));
        slots.update(&[], 2);
        assert_eq!(slots.expire(3), vec![key(1)]);
        slots.update(&[(key(2), GIB)], 4);
        assert_eq!(slots.slot_of(&key(2)), Some(1));
        let held: Vec<(u8, bool)> = slots
            .holders()
            .map(|(slot, _, listed)| (slot, listed))
            .collect();
        assert_eq!(held, vec![(1, true)]);
    }

    /// Four colors held: a newcomer first takes the slot of the process
    /// listed longest ago that is gone.
    #[test]
    fn with_every_slot_held_an_exited_process_gives_its_slot_up_first() {
        let mut slots = ColorSlots::default();
        let four: Vec<(ProcessKey, u64)> = (1..=4).map(|pid| (key(pid), GIB)).collect();
        slots.update(&four, 1);
        // 2 exits at t = 2, 3 at t = 3.
        slots.update(&[(key(1), GIB), (key(3), GIB), (key(4), GIB)], 2);
        slots.update(&[(key(1), GIB), (key(4), GIB)], 3);
        let lost = slots.update(&[(key(1), GIB), (key(4), GIB), (key(9), MIB)], 4);
        assert_eq!(lost, vec![key(2)]);
        assert_eq!(slots.slot_of(&key(9)), Some(1));
        assert_eq!(slots.slot_of(&key(3)), Some(2), "3 exited later");
    }

    /// With every holder listed a newcomer needs twice the smallest
    /// holder's memory and 256 MiB more; smaller ones stay without color.
    #[test]
    fn a_much_larger_newcomer_displaces_the_smallest_listed_holder() {
        let mut slots = ColorSlots::default();
        let held = [(key(1), 8 * GIB), (key(2), 6 * GIB), (key(3), 4 * GIB)];
        let mut listed = held.to_vec();
        listed.push((key(4), 100 * MIB));
        slots.update(&listed, 1);
        // Twice 100 MiB but not 256 MiB more: no.
        listed.push((key(5), 300 * MIB));
        assert!(slots.update(&listed, 2).is_empty());
        assert_eq!(slots.slot_of(&key(5)), None);
        // 20 GiB displaces the 100 MiB holder; the second newcomer holds
        // less than twice the new smallest and stays without a color.
        listed.push((key(6), 20 * GIB));
        let lost = slots.update(&listed, 3);
        assert_eq!(lost, vec![key(4)]);
        assert_eq!(slots.slot_of(&key(6)), Some(3));
        assert_eq!(slots.slot_of(&key(5)), None);
        assert!(displaces(2 * GIB, GIB));
        assert!(!displaces(GIB + 300 * MIB, GIB));
        assert!(!displaces(400 * MIB, 200 * MIB));
    }

    #[test]
    fn a_listed_holder_never_expires() {
        let mut slots = ColorSlots::default();
        slots.update(&[(key(1), GIB)], 1);
        assert!(slots.expire(u64::MAX).is_empty());
        assert_eq!(slots.slot_of(&key(1)), Some(0));
    }
}
