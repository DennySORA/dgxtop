//! Page accumulation for paged read groups.
//!
//! A host has one request in flight, so at most one cycle per adapter is
//! open. Pages must arrive in order (0, 1, 2, ...); the cycle completes with
//! the first page that does not announce `PAGE_CONTINUES`. An abandoned cycle
//! (failure, deadline, restart) is discarded as a whole: collectors never see
//! a partial page set as if it were complete.

use std::collections::BTreeMap;
use std::sync::Arc;

use dgxtop_core::identity::AdapterId;
use dgxtop_core::ports::IngressBatch;

use crate::scheduler::ReadReply;

#[derive(Debug)]
struct OpenCycle {
    group: String,
    cycle_id: u64,
    batches: Vec<Arc<IngressBatch>>,
}

/// Result of adding one page.
#[derive(Debug)]
pub(crate) enum PagePush {
    /// More pages follow.
    Continue,
    /// The final page arrived; every page of the cycle in order.
    Complete(Vec<Arc<IngressBatch>>),
    /// Out-of-order page (earlier pages were lost); the cycle was dropped.
    Discarded { records: u32 },
}

/// A cycle dropped before its final page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AbandonedCycle {
    pub(crate) group: String,
    pub(crate) cycle_id: u64,
    pub(crate) pages: usize,
    pub(crate) records: u32,
}

#[derive(Debug, Default)]
pub(crate) struct PageAccumulator {
    open: BTreeMap<AdapterId, OpenCycle>,
}

impl PageAccumulator {
    pub(crate) fn push(
        &mut self,
        adapter_id: &AdapterId,
        reply: &ReadReply,
        batch: Arc<IngressBatch>,
        final_page: bool,
    ) -> PagePush {
        if reply.page == 0 {
            self.open.insert(
                adapter_id.clone(),
                OpenCycle {
                    group: reply.group.clone(),
                    cycle_id: reply.cycle_id,
                    batches: Vec::new(),
                },
            );
        }
        let in_order = self.open.get(adapter_id).is_some_and(|cycle| {
            cycle.group == reply.group
                && cycle.cycle_id == reply.cycle_id
                && usize::try_from(reply.page).is_ok_and(|page| page == cycle.batches.len())
        });
        if !in_order {
            let mut dropped = self
                .open
                .remove(adapter_id)
                .map(|cycle| cycle.batches)
                .unwrap_or_default();
            dropped.push(batch);
            return PagePush::Discarded {
                records: record_count(&dropped),
            };
        }
        if final_page {
            let mut batches = self
                .open
                .remove(adapter_id)
                .map(|cycle| cycle.batches)
                .unwrap_or_default();
            batches.push(batch);
            return PagePush::Complete(batches);
        }
        if let Some(cycle) = self.open.get_mut(adapter_id) {
            cycle.batches.push(batch);
        }
        PagePush::Continue
    }

    /// Drops the open cycle of an adapter (failure, deadline, restart).
    pub(crate) fn abandon(&mut self, adapter_id: &AdapterId) -> Option<AbandonedCycle> {
        self.open.remove(adapter_id).map(|cycle| AbandonedCycle {
            records: record_count(&cycle.batches),
            group: cycle.group,
            cycle_id: cycle.cycle_id,
            pages: cycle.batches.len(),
        })
    }

    /// Drops every open cycle (shutdown).
    pub(crate) fn clear(&mut self) {
        self.open.clear();
    }

    #[cfg(test)]
    pub(crate) fn open_cycles(&self) -> usize {
        self.open.len()
    }
}

/// Records carried by a set of pages.
pub(crate) fn record_count(batches: &[Arc<IngressBatch>]) -> u32 {
    let total: usize = batches.iter().map(|batch| batch.records.len()).sum();
    u32::try_from(total).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::{ObservationBuilder, adapter_id, batch_of, metric_key};

    fn reply(page: u32, cycle_id: u64) -> ReadReply {
        ReadReply {
            group: "process".into(),
            cycle_id,
            page,
            epoch: 1,
        }
    }

    fn page(records: usize) -> Arc<IngressBatch> {
        let key = metric_key("host", "cpu.load.1m", "host");
        let observations = (0..records)
            .map(|_| ObservationBuilder::new(key.clone(), "procfs", "procfs:load:1m").build())
            .collect();
        Arc::new(IngressBatch::new(
            batch_of("procfs", "process", 1, observations),
            None,
        ))
    }

    #[test]
    fn pages_accumulate_until_the_final_page() {
        let mut pages = PageAccumulator::default();
        let procfs = adapter_id("procfs");
        assert!(matches!(
            pages.push(&procfs, &reply(0, 7), page(2), false),
            PagePush::Continue
        ));
        assert!(matches!(
            pages.push(&procfs, &reply(1, 7), page(3), false),
            PagePush::Continue
        ));
        let PagePush::Complete(batches) = pages.push(&procfs, &reply(2, 7), page(1), true) else {
            panic!("cycle should complete");
        };
        assert_eq!(batches.len(), 3);
        assert_eq!(record_count(&batches), 6);
        assert_eq!(pages.open_cycles(), 0);
    }

    #[test]
    fn out_of_order_pages_drop_the_cycle() {
        let mut pages = PageAccumulator::default();
        let procfs = adapter_id("procfs");
        pages.push(&procfs, &reply(0, 1), page(1), false);
        assert!(matches!(
            pages.push(&procfs, &reply(2, 1), page(1), true),
            PagePush::Discarded { records: 2 }
        ));
        assert!(matches!(
            pages.push(&procfs, &reply(1, 9), page(3), true),
            PagePush::Discarded { records: 3 }
        ));
    }

    #[test]
    fn abandon_reports_what_was_dropped() {
        let mut pages = PageAccumulator::default();
        let procfs = adapter_id("procfs");
        pages.push(&procfs, &reply(0, 3), page(5), false);
        let abandoned = pages.abandon(&procfs).unwrap();
        assert_eq!(
            abandoned,
            AbandonedCycle {
                group: "process".into(),
                cycle_id: 3,
                pages: 1,
                records: 5
            }
        );
        assert!(pages.abandon(&procfs).is_none());
        pages.push(&procfs, &reply(0, 4), page(1), false);
        pages.clear();
        assert_eq!(pages.open_cycles(), 0);
    }
}
