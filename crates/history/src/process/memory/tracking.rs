//! Which GPU processes the record tracks and the color of each (D-64,
//! D-65).
//!
//! - The processes listed with GPU memory at one time have different
//!   colors, at most [`PROCESS_COLOR_SLOTS`] of them.
//! - A process keeps its color while it is tracked, and a new process
//!   takes the color its display name hashes to unless a listed process
//!   holds it, so a job keeps its color when it or dgxtop restarts.
//!   Otherwise it takes the next color no listed process holds, a free one
//!   (or an exited process's of the same name) before an exited process's
//!   of another name; processes of one name thus take the next colors, the
//!   one started first first.
//! - The record holds up to [`MAX_TRACKED_PROCESSES`] processes. One that
//!   is no longer listed (it exited) keeps its record and color for the
//!   history view until it has not been listed for a day, or until a new
//!   process needs its place and it is the one listed longest ago. A
//!   process listed again whose color a listed process took meanwhile gets
//!   another one, or gives its place up when every color is taken.
//! - With every color held by a listed process, a newcomer takes the
//!   place of the smallest only when it holds twice as much and 256 MiB
//!   more, so two processes of similar size never take turns.

use std::collections::BTreeMap;

use dgxtop_core::identity::ProcessKey;
use dgxtop_core::view::PROCESS_COLOR_SLOTS;

/// Processes the record holds: twice the colors, so exited processes keep
/// their history while new ones run.
pub(super) const MAX_TRACKED_PROCESSES: usize = 2 * PROCESS_COLOR_SLOTS;
/// A newcomer takes a listed holder's place at this many times its memory.
const DISPLACE_FACTOR: u64 = 2;
/// ... and at least this much more.
const DISPLACE_MIN_BYTES: u64 = 256 << 20;

/// A process one table lists with GPU memory.
#[derive(Debug, Clone, Copy)]
pub(super) struct Listed<'a> {
    pub(super) key: &'a ProcessKey,
    pub(super) name: &'a str,
    pub(super) bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Holder {
    key: ProcessKey,
    name: String,
    color: u8,
    listed: bool,
    last_listed_ns: u64,
}

/// The tracked processes (module docs).
#[derive(Debug, Clone, Default)]
pub(super) struct Tracking {
    holders: Vec<Holder>,
}

impl Tracking {
    /// The color of `key` while it is tracked.
    pub(super) fn color_of(&self, key: &ProcessKey) -> Option<u8> {
        self.holder(key).map(|holder| holder.color)
    }

    /// Every tracked process with its color and whether the newest table
    /// lists it.
    pub(super) fn holders(&self) -> impl Iterator<Item = (u8, &ProcessKey, bool)> {
        self.holders
            .iter()
            .map(|holder| (holder.color, &holder.key, holder.listed))
    }

    /// Applies one table's listed processes; returns the processes whose
    /// record goes.
    pub(super) fn update(&mut self, listed: &[Listed<'_>], now_ns: u64) -> Vec<ProcessKey> {
        let bytes: BTreeMap<&ProcessKey, u64> = listed
            .iter()
            .map(|process| (process.key, process.bytes))
            .collect();
        let relisted = self.mark_listed(listed, now_ns);
        let mut lost = self.recolor(&relisted);
        let accepted = self.admit(listed, &bytes, &mut lost);
        for process in accepted {
            // Admission left a color no listed process holds.
            let Some(color) = self.color_for(process.name) else {
                continue;
            };
            self.holders.push(Holder {
                key: process.key.clone(),
                name: process.name.to_owned(),
                color,
                listed: true,
                last_listed_ns: now_ns,
            });
        }
        lost
    }

    /// Frees the places of processes last listed before `cutoff_ns`.
    pub(super) fn expire(&mut self, cutoff_ns: u64) -> Vec<ProcessKey> {
        let (expired, kept): (Vec<Holder>, Vec<Holder>) = std::mem::take(&mut self.holders)
            .into_iter()
            .partition(|holder| !holder.listed && holder.last_listed_ns < cutoff_ns);
        self.holders = kept;
        expired.into_iter().map(|holder| holder.key).collect()
    }

    fn holder(&self, key: &ProcessKey) -> Option<&Holder> {
        self.holders.iter().find(|holder| &holder.key == key)
    }

    /// Marks which holders `listed` names (keeping their newest names);
    /// returns those listed again after a time unlisted.
    fn mark_listed(&mut self, listed: &[Listed<'_>], now_ns: u64) -> Vec<ProcessKey> {
        let mut relisted = Vec::new();
        for holder in &mut self.holders {
            let found = listed.iter().find(|process| *process.key == holder.key);
            if let Some(process) = found {
                if !holder.listed {
                    relisted.push(holder.key.clone());
                }
                holder.listed = true;
                holder.last_listed_ns = now_ns;
                process.name.clone_into(&mut holder.name);
            } else {
                holder.listed = false;
            }
        }
        relisted.sort_by_key(start_order);
        relisted
    }

    /// A process listed again whose color a listed process took meanwhile
    /// gets another one, or gives its place up (returned) when every color
    /// is taken.
    fn recolor(&mut self, relisted: &[ProcessKey]) -> Vec<ProcessKey> {
        let mut lost = Vec::new();
        for key in relisted {
            let Some(index) = self.holders.iter().position(|holder| &holder.key == key) else {
                continue;
            };
            let color = self.holders[index].color;
            let taken =
                self.holders.iter().enumerate().any(|(other, holder)| {
                    other != index && holder.listed && holder.color == color
                });
            if !taken {
                continue;
            }
            let name = self.holders[index].name.clone();
            match self.color_for(&name) {
                Some(color) => self.holders[index].color = color,
                None => {
                    self.holders.remove(index);
                    lost.push(key.clone());
                }
            }
        }
        lost
    }

    /// The newcomers that get a place, the one started first first: while
    /// colors are free, the largest; then only one much larger than the
    /// smallest listed process, which gives its place up (`lost`). A place
    /// taken from the record's limit is the exited process's listed
    /// longest ago.
    fn admit<'a>(
        &mut self,
        listed: &[Listed<'a>],
        bytes: &BTreeMap<&ProcessKey, u64>,
        lost: &mut Vec<ProcessKey>,
    ) -> Vec<Listed<'a>> {
        let mut newcomers: Vec<Listed<'a>> = listed
            .iter()
            .filter(|process| self.holder(process.key).is_none())
            .copied()
            .collect();
        newcomers.sort_by(|left, right| {
            right
                .bytes
                .cmp(&left.bytes)
                .then_with(|| start_order(left.key).cmp(&start_order(right.key)))
        });
        let mut accepted: Vec<Listed<'a>> = Vec::new();
        for process in newcomers {
            let listed_now = self.holders.iter().filter(|holder| holder.listed).count();
            if listed_now + accepted.len() >= PROCESS_COLOR_SLOTS {
                let Some(smallest) = self.smallest_listed(bytes) else {
                    break;
                };
                if !displaces(process.bytes, bytes.get(&smallest).copied().unwrap_or(0)) {
                    break;
                }
                self.holders.retain(|holder| holder.key != smallest);
                lost.push(smallest);
            }
            if self.holders.len() + accepted.len() >= MAX_TRACKED_PROCESSES {
                let Some(oldest) = self.oldest_unlisted() else {
                    break;
                };
                self.holders.retain(|holder| holder.key != oldest);
                lost.push(oldest);
            }
            accepted.push(process);
        }
        accepted.sort_by(|left, right| start_order(left.key).cmp(&start_order(right.key)));
        accepted
    }

    /// The listed holder holding the least now.
    fn smallest_listed(&self, bytes: &BTreeMap<&ProcessKey, u64>) -> Option<ProcessKey> {
        self.holders
            .iter()
            .filter(|holder| holder.listed)
            .min_by_key(|holder| (bytes.get(&holder.key).copied().unwrap_or(0), holder.color))
            .map(|holder| holder.key.clone())
    }

    /// The holder no longer listed that was listed longest ago.
    fn oldest_unlisted(&self) -> Option<ProcessKey> {
        self.holders
            .iter()
            .filter(|holder| !holder.listed)
            .min_by_key(|holder| (holder.last_listed_ns, start_order(&holder.key)))
            .map(|holder| holder.key.clone())
    }

    /// The color for a process named `name` (module docs): its
    /// [`preferred_color`] unless a listed process holds it, else the next
    /// color no listed process holds, a free one (or an exited process's of
    /// the same name) first; `None` when listed processes hold them all.
    fn color_for(&self, name: &str) -> Option<u8> {
        let first = preferred_color(name);
        let holders_of = |color: u8| {
            self.holders
                .iter()
                .filter(move |holder| holder.color == color)
        };
        let listed = |color: u8| holders_of(color).any(|holder| holder.listed);
        if !listed(first) {
            return Some(first);
        }
        let mut fallback = None;
        for offset in 1..PROCESS_COLOR_SLOTS {
            let Ok(color) = u8::try_from((usize::from(first) + offset) % PROCESS_COLOR_SLOTS)
            else {
                continue;
            };
            if listed(color) {
                continue;
            }
            if holders_of(color).all(|holder| holder.name == name) {
                return Some(color);
            }
            fallback.get_or_insert(color);
        }
        fallback
    }
}

/// The color a display name hashes to (FNV-1a: stable across runs and
/// platforms).
pub(crate) fn preferred_color(name: &str) -> u8 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let slots = u64::try_from(PROCESS_COLOR_SLOTS).unwrap_or(1).max(1);
    u8::try_from(hash % slots).unwrap_or(0)
}

/// Processes in the order they started.
fn start_order(key: &ProcessKey) -> (u64, i32) {
    (key.start_time_ticks, key.pid.get())
}

/// Whether a newcomer holding `size` bytes takes the place of a listed
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

    struct Table {
        keys: Vec<ProcessKey>,
        names: Vec<String>,
        bytes: Vec<u64>,
    }

    impl Table {
        fn of(processes: &[(i64, &str, u64)]) -> Self {
            Self {
                keys: processes.iter().map(|(pid, _, _)| key(*pid)).collect(),
                names: processes
                    .iter()
                    .map(|(_, name, _)| (*name).to_owned())
                    .collect(),
                bytes: processes.iter().map(|(_, _, bytes)| *bytes).collect(),
            }
        }

        fn listed(&self) -> Vec<Listed<'_>> {
            (0..self.keys.len())
                .map(|index| Listed {
                    key: &self.keys[index],
                    name: &self.names[index],
                    bytes: self.bytes[index],
                })
                .collect()
        }
    }

    fn update(
        tracking: &mut Tracking,
        processes: &[(i64, &str, u64)],
        now: u64,
    ) -> Vec<ProcessKey> {
        let table = Table::of(processes);
        tracking.update(&table.listed(), now)
    }

    /// A job's color comes from its name: the same job gets the same color
    /// after it restarts (a new PID) or after dgxtop restarts (a new
    /// record).
    #[test]
    fn a_name_keeps_its_color_across_restarts() {
        let mut tracking = Tracking::default();
        update(&mut tracking, &[(10, "acme_llm.train", 40 * GIB)], 1);
        let color = tracking.color_of(&key(10)).unwrap();
        assert_eq!(color, preferred_color("acme_llm.train"));
        // The job restarts: the exited record keeps its color, the new
        // process of the same name takes it again.
        update(&mut tracking, &[], 2);
        update(&mut tracking, &[(11, "acme_llm.train", 40 * GIB)], 3);
        assert_eq!(tracking.color_of(&key(11)), Some(color));
        assert_eq!(
            tracking.color_of(&key(10)),
            Some(color),
            "the exited record stays"
        );
        // A fresh record (dgxtop restarted) gives it the same color.
        let mut fresh = Tracking::default();
        update(&mut fresh, &[(11, "acme_llm.train", 40 * GIB)], 1);
        assert_eq!(fresh.color_of(&key(11)), Some(color));
    }

    /// Processes listed together never share a color; processes of one
    /// name take the colors after their name's, the one started first
    /// first, whatever order the table lists them in.
    #[test]
    fn listed_processes_have_different_colors_in_start_order() {
        let mut tracking = Tracking::default();
        update(
            &mut tracking,
            &[
                (30, "train", GIB),
                (10, "train", 3 * GIB),
                (20, "train", 2 * GIB),
            ],
            1,
        );
        let first = preferred_color("train");
        let next = |offset: u8| (first + offset) % u8::try_from(PROCESS_COLOR_SLOTS).unwrap();
        assert_eq!(
            [10, 20, 30].map(|pid| tracking.color_of(&key(pid))),
            [Some(first), Some(next(1)), Some(next(2))]
        );
    }

    /// Eight colors held by listed processes: a ninth waits without a
    /// color unless it is much larger than the smallest, which then gives
    /// its place up.
    #[test]
    fn a_ninth_process_needs_to_be_much_larger_than_the_smallest() {
        let names: Vec<String> = (1..=8).map(|pid| format!("job{pid}")).collect();
        let mut processes: Vec<(i64, &str, u64)> = (1..=8)
            .map(|pid| (pid, names[usize::try_from(pid).unwrap() - 1].as_str(), GIB))
            .collect();
        processes[7].2 = 100 * MIB;
        let mut tracking = Tracking::default();
        update(&mut tracking, &processes, 1);
        let colors: std::collections::BTreeSet<u8> = (1..=8)
            .filter_map(|pid| tracking.color_of(&key(pid)))
            .collect();
        assert_eq!(colors.len(), 8, "all different");
        processes.push((9, "small", 300 * MIB));
        assert!(update(&mut tracking, &processes, 2).is_empty());
        assert_eq!(tracking.color_of(&key(9)), None);
        processes.push((10, "big", 20 * GIB));
        assert_eq!(update(&mut tracking, &processes, 3), vec![key(8)]);
        assert!(tracking.color_of(&key(10)).is_some());
        assert_eq!(tracking.color_of(&key(9)), None);
        assert!(displaces(2 * GIB, GIB));
        assert!(!displaces(GIB + 300 * MIB, GIB));
        assert!(!displaces(400 * MIB, 200 * MIB));
    }

    /// An exited process keeps its record while new processes come, until
    /// the record is full: then the one listed longest ago gives way. A
    /// new process prefers a free color to an exited process's.
    #[test]
    fn exited_processes_keep_their_record_until_the_record_is_full() {
        let mut tracking = Tracking::default();
        let mut now = 1;
        // Sixteen short jobs, one after the other.
        let names: Vec<String> = (1..=17).map(|pid| format!("job{pid}")).collect();
        for pid in 1..=16 {
            let name = names[usize::try_from(pid).unwrap() - 1].as_str();
            assert!(update(&mut tracking, &[(pid, name, GIB)], now).is_empty());
            now += 1;
            update(&mut tracking, &[], now);
            now += 1;
        }
        assert_eq!(tracking.holders().count(), 16);
        assert!(tracking.holders().all(|(_, _, listed)| !listed));
        // The seventeenth takes the place of the first.
        let lost = update(&mut tracking, &[(17, names[16].as_str(), GIB)], now);
        assert_eq!(lost, vec![key(1)]);
        assert_eq!(tracking.holders().count(), 16);
        // A new process whose color a listed process holds goes on to a
        // free color before an exited process's of another name.
        let name_with = |color: u8, prefix: &str| -> String {
            (0..1000)
                .map(|candidate| format!("{prefix}{candidate}"))
                .find(|name| preferred_color(name) == color)
                .unwrap()
        };
        let mut fresh = Tracking::default();
        let alpha = preferred_color("alpha");
        let slots = u8::try_from(PROCESS_COLOR_SLOTS).unwrap();
        let after = (alpha + 1) % slots;
        let exited = name_with(after, "exited");
        update(&mut fresh, &[(1, "alpha", GIB), (2, &exited, GIB)], 1);
        update(&mut fresh, &[(1, "alpha", GIB)], 2);
        let rival = name_with(alpha, "rival");
        update(&mut fresh, &[(1, "alpha", GIB), (3, &rival, GIB)], 3);
        assert_eq!(fresh.color_of(&key(2)), Some(after));
        assert_eq!(fresh.color_of(&key(3)), Some((alpha + 2) % slots));
    }

    /// A process listed again after another took its color gets a new
    /// one; a listed holder never expires.
    #[test]
    fn a_process_listed_again_never_shares_a_listed_color() {
        let mut tracking = Tracking::default();
        update(&mut tracking, &[(1, "same", GIB)], 1);
        update(&mut tracking, &[], 2);
        // A new process of the same name takes the color of the exited one.
        update(&mut tracking, &[(2, "same", GIB)], 3);
        assert_eq!(tracking.color_of(&key(1)), tracking.color_of(&key(2)));
        // The first is listed again: it gets another color.
        update(&mut tracking, &[(1, "same", GIB), (2, "same", GIB)], 4);
        assert_ne!(tracking.color_of(&key(1)), tracking.color_of(&key(2)));
        assert!(tracking.expire(u64::MAX).is_empty(), "both are listed");
        update(&mut tracking, &[(2, "same", GIB)], 5);
        assert_eq!(tracking.expire(u64::MAX), vec![key(1)]);
    }
}
