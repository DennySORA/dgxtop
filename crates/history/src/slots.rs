//! Generational slot table for series.
//!
//! A handle is `(index, generation)`. Freeing a slot bumps its generation, so
//! a handle to a removed series never resolves to the series that later reuses
//! the index; an index whose generation would wrap is retired for good.

/// Stable handle of one series incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SlotHandle {
    pub index: u32,
    pub generation: u32,
}

struct SlotEntry<T> {
    generation: u32,
    value: Option<T>,
}

/// Slots with generation checks; freed indices are reused with a new generation.
pub(crate) struct SlotTable<T> {
    entries: Vec<SlotEntry<T>>,
    free: Vec<u32>,
    live: usize,
}

impl<T> SlotTable<T> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            free: Vec::new(),
            live: 0,
        }
    }

    pub(crate) fn live(&self) -> usize {
        self.live
    }

    pub(crate) fn insert(&mut self, value: T) -> Option<SlotHandle> {
        if let Some(index) = self.free.pop()
            && let Some(entry) = self.entries.get_mut(usize::try_from(index).ok()?)
        {
            entry.value = Some(value);
            self.live += 1;
            return Some(SlotHandle {
                index,
                generation: entry.generation,
            });
        }
        let index = u32::try_from(self.entries.len()).ok()?;
        self.entries.push(SlotEntry {
            generation: 0,
            value: Some(value),
        });
        self.live += 1;
        Some(SlotHandle {
            index,
            generation: 0,
        })
    }

    fn entry(&self, handle: SlotHandle) -> Option<&SlotEntry<T>> {
        self.entries
            .get(usize::try_from(handle.index).ok()?)
            .filter(|entry| entry.generation == handle.generation)
    }

    pub(crate) fn get(&self, handle: SlotHandle) -> Option<&T> {
        self.entry(handle)?.value.as_ref()
    }

    pub(crate) fn get_mut(&mut self, handle: SlotHandle) -> Option<&mut T> {
        self.entries
            .get_mut(usize::try_from(handle.index).ok()?)
            .filter(|entry| entry.generation == handle.generation)?
            .value
            .as_mut()
    }

    pub(crate) fn remove(&mut self, handle: SlotHandle) -> Option<T> {
        let entry = self
            .entries
            .get_mut(usize::try_from(handle.index).ok()?)
            .filter(|entry| entry.generation == handle.generation)?;
        let value = entry.value.take()?;
        self.live -= 1;
        if let Some(next) = entry.generation.checked_add(1) {
            entry.generation = next;
            self.free.push(handle.index);
        }
        Some(value)
    }

    pub(crate) fn handles(&self) -> Vec<SlotHandle> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.value.is_some())
            .filter_map(|(index, entry)| {
                Some(SlotHandle {
                    index: u32::try_from(index).ok()?,
                    generation: entry.generation,
                })
            })
            .collect()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().filter_map(|entry| entry.value.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freed_slot_gets_a_new_generation() {
        let mut table = SlotTable::new();
        let first = table.insert("a").unwrap();
        let second = table.insert("b").unwrap();
        assert_eq!(table.remove(first), Some("a"));
        assert_eq!(table.get(first), None);
        let reused = table.insert("c").unwrap();
        assert_eq!(reused.index, first.index);
        assert_ne!(reused.generation, first.generation);
        assert_eq!(table.get(first), None);
        assert_eq!(table.get(reused), Some(&"c"));
        assert_eq!(table.get(second), Some(&"b"));
        assert_eq!(table.live(), 2);
        assert_eq!(table.handles().len(), 2);
        assert_eq!(table.remove(first), None);
    }

    #[test]
    fn exhausted_generation_retires_the_index() {
        let mut table = SlotTable::new();
        let handle = table.insert(1).unwrap();
        table.entries[0].generation = u32::MAX;
        let exhausted = SlotHandle {
            index: handle.index,
            generation: u32::MAX,
        };
        assert_eq!(table.remove(exhausted), Some(1));
        let next = table.insert(2).unwrap();
        assert_eq!(next.index, 1);
        assert!(table.get(exhausted).is_none());
    }
}
