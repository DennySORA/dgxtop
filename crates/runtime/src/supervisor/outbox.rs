//! Bounded buffer between the supervisor thread and the ingestion channel.
//!
//! The supervisor never blocks on the Manager: completions are offered with
//! `try_send`, and while the channel is full they wait here (bounded). When
//! the outbox itself is full, status updates go first (the latest status is
//! always available from `SupervisorHandle::statuses`), then the oldest data
//! completion; every drop is counted.

use std::collections::VecDeque;

use crossbeam_channel::{Sender, TrySendError};
use dgxtop_core::ports::IngestionCompletion;

#[derive(Debug)]
pub(crate) struct Outbox {
    items: VecDeque<IngestionCompletion>,
    capacity: usize,
    sender: Sender<IngestionCompletion>,
    dropped: u64,
    disconnected: bool,
}

impl Outbox {
    pub(crate) fn new(sender: Sender<IngestionCompletion>, capacity: usize) -> Self {
        Self {
            items: VecDeque::new(),
            capacity: capacity.max(1),
            sender,
            dropped: 0,
            disconnected: false,
        }
    }

    /// Queues a completion; returns how many items were dropped to make room.
    pub(crate) fn push(&mut self, item: IngestionCompletion) -> u64 {
        if self.disconnected {
            self.dropped += 1;
            return 1;
        }
        let mut dropped = 0;
        if self.items.len() >= self.capacity {
            let victim = self
                .items
                .iter()
                .position(|queued| matches!(queued, IngestionCompletion::HostStatus(_)))
                .unwrap_or(0);
            self.items.remove(victim);
            dropped += 1;
        }
        self.items.push_back(item);
        self.dropped += dropped;
        dropped
    }

    /// Hands queued completions to the channel without blocking.
    pub(crate) fn flush(&mut self) {
        while let Some(item) = self.items.pop_front() {
            match self.sender.try_send(item) {
                Ok(()) => {}
                Err(TrySendError::Full(item)) => {
                    self.items.push_front(item);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    // The Manager is gone; keep supervising until shutdown.
                    self.dropped += 1 + u64::try_from(self.items.len()).unwrap_or(u64::MAX);
                    self.items.clear();
                    self.disconnected = true;
                    return;
                }
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub(crate) const fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::view::{HostState, HostStatus};

    fn status(generation: u32) -> IngestionCompletion {
        IngestionCompletion::HostStatus(HostStatus {
            adapter_id: "nvml".into(),
            state: HostState::Healthy,
            generation,
            pid: None,
            last_success_boot_ns: None,
            timeouts_total: 0,
            consecutive_failures: 0,
            restarts_total: 0,
            backoff_until_boot_ns: None,
            detail: None,
        })
    }

    fn generation_of(item: &IngestionCompletion) -> u32 {
        match item {
            IngestionCompletion::HostStatus(status) => status.generation,
            _ => u32::MAX,
        }
    }

    #[test]
    fn full_channel_keeps_items_until_there_is_room() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let mut outbox = Outbox::new(sender, 4);
        outbox.push(status(1));
        outbox.push(status(2));
        outbox.flush();
        assert!(!outbox.is_empty());
        assert_eq!(generation_of(&receiver.try_recv().unwrap()), 1);
        outbox.flush();
        assert_eq!(generation_of(&receiver.try_recv().unwrap()), 2);
        assert!(outbox.is_empty());
    }

    #[test]
    fn bounded_and_counts_drops() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let mut outbox = Outbox::new(sender, 2);
        for generation in 1..=5 {
            outbox.push(status(generation));
        }
        assert_eq!(outbox.dropped(), 3);
        outbox.flush();
        assert_eq!(generation_of(&receiver.try_recv().unwrap()), 4);
        drop(receiver);
        outbox.flush();
        assert!(outbox.is_empty());
        assert_eq!(outbox.push(status(9)), 1);
    }
}
