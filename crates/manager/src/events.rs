//! Bounded in-memory event log.
//!
//! Control/audit events (`EventKind::is_control`) are never evicted while a
//! telemetry event is still held: telemetry bursts (gaps, commits, source
//! changes) must not push configuration or action history out of the log.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use dgxtop_core::event::{Event, EventKind};
use dgxtop_core::identity::BootId;
use uuid::Uuid;

/// Total events kept (control + telemetry).
pub(crate) const EVENT_LOG_CAPACITY: usize = 512;

#[derive(Debug)]
struct EventLog {
    capacity: usize,
    next_sequence: u64,
    control: VecDeque<(u64, Event)>,
    telemetry: VecDeque<(u64, Event)>,
    evicted_total: u64,
}

impl EventLog {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            next_sequence: 0,
            control: VecDeque::new(),
            telemetry: VecDeque::new(),
            evicted_total: 0,
        }
    }

    fn len(&self) -> usize {
        self.control.len() + self.telemetry.len()
    }

    fn record(&mut self, event: Event) {
        let control = event.kind.is_control();
        if self.len() >= self.capacity && !self.make_room(control) {
            // A ring holding only control events drops incoming telemetry:
            // control is never dropped to admit telemetry (AT30).
            self.evicted_total = self.evicted_total.saturating_add(1);
            return;
        }
        let entry = (self.next_sequence, event);
        self.next_sequence = self.next_sequence.saturating_add(1);
        if control {
            self.control.push_back(entry);
        } else {
            self.telemetry.push_back(entry);
        }
    }

    /// Evicts the oldest telemetry event, or the oldest control event when
    /// the incoming event is a control event; false when nothing may go.
    fn make_room(&mut self, incoming_control: bool) -> bool {
        let evicted = self.telemetry.pop_front().is_some()
            || (incoming_control && self.control.pop_front().is_some());
        if evicted {
            self.evicted_total = self.evicted_total.saturating_add(1);
        }
        evicted
    }

    /// Newest `max` events in recording order (oldest first).
    fn recent(&self, max: usize) -> Vec<Event> {
        let mut merged: Vec<&(u64, Event)> =
            self.control.iter().chain(self.telemetry.iter()).collect();
        merged.sort_by_key(|(sequence, _)| *sequence);
        let skip = merged.len().saturating_sub(max);
        merged
            .into_iter()
            .skip(skip)
            .map(|(_, event)| event.clone())
            .collect()
    }
}

/// Read handle on the Manager's bounded event log (diagnostics export,
/// composition root). Cheap to clone; never blocks the control thread for
/// longer than a copy of at most [`EVENT_LOG_CAPACITY`] events.
#[derive(Debug, Clone)]
pub struct EventFeed {
    log: Arc<Mutex<EventLog>>,
}

impl EventFeed {
    /// Newest `max` events, oldest first.
    pub fn recent(&self, max: usize) -> Vec<Event> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recent(max)
    }

    pub fn len(&self) -> usize {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Events dropped because the log was full.
    pub fn evicted_total(&self) -> u64 {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .evicted_total
    }
}

/// Writer side used by the control loop.
#[derive(Debug)]
pub(crate) struct EventRecorder {
    boot_id: BootId,
    log: Arc<Mutex<EventLog>>,
}

impl EventRecorder {
    pub(crate) fn new(boot_id: BootId, capacity: usize) -> Self {
        Self {
            boot_id,
            log: Arc::new(Mutex::new(EventLog::new(capacity))),
        }
    }

    pub(crate) fn feed(&self) -> EventFeed {
        EventFeed {
            log: Arc::clone(&self.log),
        }
    }

    pub(crate) fn emit(&self, occurred_boot_ns: u64, correlation_id: Uuid, kind: EventKind) {
        let event = Event::new(self.boot_id.clone(), occurred_boot_ns, correlation_id, kind);
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use dgxtop_core::testing::boot_id;

    fn gap() -> EventKind {
        EventKind::GapRecorded {
            start_boot_ns: 0,
            end_boot_ns: 1,
            reason: "timeout".into(),
            affected_count: 1,
        }
    }

    fn applied(revision: u32) -> EventKind {
        EventKind::ConfigApplied { revision, epoch: 1 }
    }

    #[test]
    fn telemetry_is_evicted_before_control() {
        let recorder = EventRecorder::new(boot_id("b"), 4);
        let feed = recorder.feed();
        recorder.emit(1, Uuid::nil(), applied(1));
        recorder.emit(2, Uuid::nil(), gap());
        recorder.emit(3, Uuid::nil(), applied(2));
        recorder.emit(4, Uuid::nil(), gap());
        // Full: the next events evict the two gaps first.
        recorder.emit(5, Uuid::nil(), applied(3));
        recorder.emit(6, Uuid::nil(), applied(4));
        let kinds: Vec<_> = feed
            .recent(10)
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![applied(1), applied(2), applied(3), applied(4)],
            "control events survive telemetry bursts"
        );
        assert_eq!(feed.evicted_total(), 2);
        // A ring holding only control events drops the incoming telemetry
        // instead (AT30: control is never dropped for telemetry).
        recorder.emit(7, Uuid::nil(), gap());
        let kinds: Vec<_> = feed
            .recent(10)
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![applied(1), applied(2), applied(3), applied(4)],
            "telemetry never displaces a control event"
        );
        assert_eq!(feed.evicted_total(), 3);
        // Control events still replace the oldest control event.
        recorder.emit(8, Uuid::nil(), applied(5));
        let kinds: Vec<_> = feed
            .recent(10)
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(kinds, vec![applied(2), applied(3), applied(4), applied(5)]);
        assert_eq!(feed.evicted_total(), 4);
    }
}
