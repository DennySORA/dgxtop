//! Session plumbing between frontends (UI threads) and the control thread.
//!
//! * `submit` never blocks: syntax and per-command byte cap are checked on the
//!   caller's thread, a result slot is reserved *before* acceptance and the
//!   command enters a bounded queue with `try_send`; anything full is `Busy`.
//! * Results go to a per-session mailbox that is never coalesced. Its size is
//!   bounded by the reservations (outstanding commands), so delivering a result
//!   can never fail or drop.
//! * Projections use the core latest-only [`ProjectionSlot`].
//! * Shutdown is out of band: `SessionPort::request_shutdown` raises a flag
//!   plus a wake token, never queued behind ordinary commands. The sealed
//!   `Shutdown` command body is refused (D-93): one way to quit.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use crossbeam_channel::{Sender, TrySendError};
use dgxtop_core::budget::{ByteLedger, BytePermit, Partition};
use dgxtop_core::commands::{
    CommandBody, CommandEnvelope, CommandRejection, CommandResult, CommandSyntaxError,
};
use dgxtop_core::errors::ErrorCode;
use dgxtop_core::identity::SessionId;
use dgxtop_core::ports::SessionPort;
use dgxtop_core::text::sanitize_message;
use dgxtop_core::view::{ProjectionSlot, ViewModel};
use uuid::Uuid;

use crate::shutdown::ShutdownSignal;

/// Viewport assumed until the frontend reports its terminal size.
const DEFAULT_VIEWPORT: (u16, u16) = (80, 24);

/// An accepted command waiting for the control thread.
#[derive(Debug)]
pub(crate) struct QueuedCommand {
    pub(crate) envelope: CommandEnvelope,
    /// Reserve-partition bytes of the payload, held until it is processed.
    pub(crate) _permit: Option<BytePermit>,
}

/// Global count of accepted commands whose result has not been drained yet
/// (`runtime.max_pending_requests`).
#[derive(Debug)]
pub(crate) struct Outstanding {
    count: AtomicUsize,
    limit: usize,
}

impl Outstanding {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            count: AtomicUsize::new(0),
            limit: limit.max(1),
        }
    }

    fn try_reserve(&self) -> bool {
        self.count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < self.limit).then_some(current + 1)
            })
            .is_ok()
    }

    fn release(&self) {
        let _ = self
            .count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(1))
            });
    }

    #[cfg(test)]
    pub(crate) fn current(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

/// Results waiting for the frontend; each holds one `Outstanding` slot
/// until it is drained.
#[derive(Debug, Default)]
struct Mailbox {
    closed: bool,
    entries: VecDeque<CommandResult>,
}

/// State shared by every clone of one session's handle and the Manager.
#[derive(Debug)]
pub(crate) struct SessionShared {
    id: SessionId,
    outstanding: Arc<Outstanding>,
    mailbox: Mutex<Mailbox>,
    projection: ProjectionSlot,
    viewport: AtomicU32,
    shutdown_requested: AtomicBool,
}

impl SessionShared {
    pub(crate) fn new(id: SessionId, outstanding: Arc<Outstanding>) -> Self {
        Self {
            id,
            outstanding,
            mailbox: Mutex::new(Mailbox::default()),
            projection: ProjectionSlot::new(),
            viewport: AtomicU32::new(pack_viewport(DEFAULT_VIEWPORT.0, DEFAULT_VIEWPORT.1)),
            shutdown_requested: AtomicBool::new(false),
        }
    }

    pub(crate) fn id(&self) -> &SessionId {
        &self.id
    }

    /// Delivers the single result of a command accepted through the queue.
    pub(crate) fn deliver(&self, result: CommandResult) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
        if mailbox.closed {
            drop(mailbox);
            self.outstanding.release();
            return;
        }
        mailbox.entries.push_back(result);
    }

    pub(crate) fn publish(&self, projection: Arc<ViewModel>) {
        self.projection.publish(projection);
    }

    pub(crate) fn viewport(&self) -> (u16, u16) {
        unpack_viewport(self.viewport.load(Ordering::Acquire))
    }

    pub(crate) fn store_viewport(&self, columns: u16, rows: u16) {
        self.viewport
            .store(pack_viewport(columns, rows), Ordering::Release);
    }

    /// Takes the pending projection (what the frontend renders next).
    pub(crate) fn take_projection(&self) -> Option<Arc<ViewModel>> {
        self.projection.take()
    }

    /// Drops the pending projection so a replacement never needs room for
    /// both at once (its bytes are released before the next reservation).
    pub(crate) fn discard_pending_projection(&self) {
        drop(self.projection.take());
    }

    pub(crate) fn mark_shutdown_requested(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
    }

    /// True once every handle of the session has been dropped.
    pub(crate) fn is_closed(&self) -> bool {
        self.mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed
    }

    fn drain(&self, max: usize) -> Vec<CommandResult> {
        let taken: Vec<CommandResult> = {
            let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
            let count = max.min(mailbox.entries.len());
            mailbox.entries.drain(..count).collect()
        };
        for _ in &taken {
            self.outstanding.release();
        }
        taken
    }

    fn close(&self) {
        let entries = {
            let mut mailbox = self.mailbox.lock().unwrap_or_else(PoisonError::into_inner);
            mailbox.closed = true;
            std::mem::take(&mut mailbox.entries)
        };
        for _ in entries {
            self.outstanding.release();
        }
    }
}

/// The bounded command queue shared by every session.
pub(crate) struct CommandInbox {
    /// `true` once the Manager stopped; checked under the lock so no command
    /// can slip into the queue after the final drain.
    closed: RwLock<bool>,
    sender: Sender<QueuedCommand>,
    outstanding: Arc<Outstanding>,
    command_byte_cap: u64,
    ledger: ByteLedger,
    shutdown: Arc<ShutdownSignal>,
}

/// Construction inputs of [`CommandInbox`].
pub(crate) struct InboxParts {
    pub(crate) sender: Sender<QueuedCommand>,
    pub(crate) outstanding: Arc<Outstanding>,
    pub(crate) command_byte_cap: u64,
    pub(crate) ledger: ByteLedger,
    pub(crate) shutdown: Arc<ShutdownSignal>,
}

impl CommandInbox {
    pub(crate) fn new(parts: InboxParts) -> Self {
        Self {
            closed: RwLock::new(false),
            sender: parts.sender,
            outstanding: parts.outstanding,
            command_byte_cap: parts.command_byte_cap,
            ledger: parts.ledger,
            shutdown: parts.shutdown,
        }
    }

    pub(crate) fn outstanding(&self) -> Arc<Outstanding> {
        Arc::clone(&self.outstanding)
    }

    /// Releases the slot of a result whose session no longer exists.
    pub(crate) fn release_orphan_slot(&self) {
        self.outstanding.release();
    }

    /// Wakes the control thread for a change it reads outside the command
    /// queue (a viewport), so it is not left until the next deadline.
    pub(crate) fn wake_control_loop(&self) {
        self.shutdown.wake();
    }

    /// After this returns no further command can enter the queue.
    pub(crate) fn close(&self) {
        *self.closed.write().unwrap_or_else(PoisonError::into_inner) = true;
    }

    fn enqueue(&self, envelope: CommandEnvelope, encoded_len: u64) -> Result<(), CommandRejection> {
        let request_id = envelope.request_id;
        if !self.outstanding.try_reserve() {
            return Err(self.reject(request_id, ErrorCode::Busy, "command queue full"));
        }
        let permit = match self.ledger.try_reserve(Partition::Reserve, encoded_len) {
            Ok(permit) => permit,
            Err(error) => {
                self.outstanding.release();
                let message = format!("command reserve exhausted: {error}");
                return Err(self.reject(request_id, ErrorCode::Busy, &message));
            }
        };
        let queued = QueuedCommand {
            envelope,
            _permit: Some(permit),
        };
        let closed = self.closed.read().unwrap_or_else(PoisonError::into_inner);
        if *closed {
            drop(closed);
            self.outstanding.release();
            return Err(self.reject(request_id, ErrorCode::Unavailable, "manager stopped"));
        }
        let sent = self.sender.try_send(queued);
        drop(closed);
        match sent {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.outstanding.release();
                Err(self.reject(request_id, ErrorCode::Busy, "command queue full"))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.outstanding.release();
                Err(self.reject(request_id, ErrorCode::Unavailable, "manager stopped"))
            }
        }
    }

    fn reject(&self, request_id: Uuid, code: ErrorCode, message: &str) -> CommandRejection {
        CommandRejection {
            request_id,
            code,
            message: sanitize_message(message),
        }
    }
}

/// Frontend handle of one view session (`SessionPort`). Clones share the
/// session; when the last clone is dropped the session closes and the Manager
/// revokes its pending confirmations.
#[derive(Clone)]
pub struct SessionHandle {
    shared: Arc<SessionShared>,
    inbox: Arc<CommandInbox>,
    _liveness: Arc<Liveness>,
}

/// Closes the session when the last handle clone goes away.
struct Liveness {
    shared: Arc<SessionShared>,
}

impl Drop for Liveness {
    fn drop(&mut self) {
        self.shared.close();
    }
}

impl SessionHandle {
    pub(crate) fn new(shared: Arc<SessionShared>, inbox: Arc<CommandInbox>) -> Self {
        let liveness = Arc::new(Liveness {
            shared: Arc::clone(&shared),
        });
        Self {
            shared,
            inbox,
            _liveness: liveness,
        }
    }

    fn syntax_rejection(&self, request_id: Uuid, error: &CommandSyntaxError) -> CommandRejection {
        let code = match error {
            CommandSyntaxError::TooLarge(_) => ErrorCode::CapacityExceeded,
            _ => ErrorCode::ValidationFailed,
        };
        self.inbox.reject(request_id, code, &error.to_string())
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("session_id", &self.shared.id)
            .finish()
    }
}

impl SessionPort for SessionHandle {
    fn session_id(&self) -> &SessionId {
        &self.shared.id
    }

    fn submit(&self, command: CommandEnvelope) -> Result<(), CommandRejection> {
        let request_id = command.request_id;
        if command.session_id != self.shared.id {
            return Err(self.inbox.reject(
                request_id,
                ErrorCode::CapabilityDenied,
                "session_id does not match this session handle",
            ));
        }
        if let Err(error) = command.validate_syntax() {
            return Err(self.syntax_rejection(request_id, &error));
        }
        let encoded_len = command.encoded_len();
        if encoded_len > self.inbox.command_byte_cap {
            let error = CommandSyntaxError::TooLarge(self.inbox.command_byte_cap);
            return Err(self.syntax_rejection(request_id, &error));
        }
        if matches!(command.body, CommandBody::Shutdown { .. }) {
            return Err(self.inbox.reject(
                request_id,
                ErrorCode::Unsupported,
                "Shutdown is not a queued command; quit through request_shutdown",
            ));
        }
        self.inbox.enqueue(command, encoded_len)
    }

    fn take_projection(&self) -> Option<Arc<ViewModel>> {
        self.shared.take_projection()
    }

    fn drain_results(&self, max: usize) -> Vec<CommandResult> {
        self.shared.drain(max)
    }

    fn request_shutdown(&self, reason: &str) {
        self.inbox.shutdown.request(reason);
    }

    fn shutdown_requested(&self) -> bool {
        self.shared.shutdown_requested.load(Ordering::Acquire)
    }

    fn set_viewport(&self, columns: u16, rows: u16) {
        self.shared.store_viewport(columns, rows);
        self.inbox.wake_control_loop();
    }
}

fn pack_viewport(columns: u16, rows: u16) -> u32 {
    (u32::from(columns) << 16) | u32::from(rows)
}

fn unpack_viewport(packed: u32) -> (u16, u16) {
    let columns = u16::try_from(packed >> 16).unwrap_or(u16::MAX);
    let rows = u16::try_from(packed & 0xFFFF).unwrap_or(u16::MAX);
    (columns, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::budget::ByteLedger;
    use dgxtop_core::commands::{SortKey, ViewKind};

    struct Fixture {
        handle: SessionHandle,
        receiver: crossbeam_channel::Receiver<QueuedCommand>,
        shared: Arc<SessionShared>,
        inbox: Arc<CommandInbox>,
        wake: crossbeam_channel::Receiver<()>,
        signal: Arc<ShutdownSignal>,
        ledger: ByteLedger,
    }

    fn fixture(queue: usize, max_outstanding: usize) -> Fixture {
        let (sender, receiver) = crossbeam_channel::bounded(queue);
        let (wake_sender, wake) = crossbeam_channel::bounded(1);
        let signal = Arc::new(ShutdownSignal::new(wake_sender));
        let outstanding = Arc::new(Outstanding::new(max_outstanding));
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let inbox = Arc::new(CommandInbox::new(InboxParts {
            sender,
            outstanding: Arc::clone(&outstanding),
            command_byte_cap: 64 * 1024,
            ledger: ledger.clone(),
            shutdown: Arc::clone(&signal),
        }));
        let shared = Arc::new(SessionShared::new(
            SessionId::new("s1").unwrap(),
            outstanding,
        ));
        let handle = SessionHandle::new(Arc::clone(&shared), Arc::clone(&inbox));
        Fixture {
            handle,
            receiver,
            shared,
            inbox,
            wake,
            signal,
            ledger,
        }
    }

    fn navigation(session: &str) -> CommandEnvelope {
        CommandEnvelope::new(
            SessionId::new(session).unwrap(),
            CommandBody::UpdateViewSession {
                view: ViewKind::Overview,
                selected_entity_id: None,
                selected_process: None,
                filter: String::new(),
                sort: SortKey::Pid,
                ascending: true,
                frame_revision: 0,
                overview_bar: None,
                process_bar: None,
            },
        )
    }

    #[test]
    fn accepted_commands_reserve_slots_and_full_queue_is_busy() {
        let fixture = fixture(2, 8);
        fixture.handle.submit(navigation("s1")).unwrap();
        fixture.handle.submit(navigation("s1")).unwrap();
        let rejection = fixture.handle.submit(navigation("s1")).unwrap_err();
        assert_eq!(rejection.code, ErrorCode::Busy);
        assert_eq!(fixture.inbox.outstanding.current(), 2);
        assert!(fixture.ledger.used(Partition::Reserve) > 0);
        // Processing releases the queue entry and its payload bytes...
        let first = fixture.receiver.try_recv().unwrap();
        let second = fixture.receiver.try_recv().unwrap();
        let ids = [first.envelope.request_id, second.envelope.request_id];
        drop(first);
        drop(second);
        assert_eq!(fixture.ledger.used(Partition::Reserve), 0);
        // ... but the result slots stay reserved until drained.
        assert_eq!(fixture.inbox.outstanding.current(), 2);
        for id in ids {
            fixture.shared.deliver(CommandResult::completed(id, "ok"));
        }
        let results = fixture.handle.drain_results(1);
        assert_eq!(results.len(), 1);
        assert_eq!(fixture.inbox.outstanding.current(), 1);
        assert_eq!(fixture.handle.drain_results(10).len(), 1);
        assert_eq!(fixture.inbox.outstanding.current(), 0);
    }

    #[test]
    fn outstanding_limit_rejects_even_with_queue_space() {
        let fixture = fixture(8, 1);
        fixture.handle.submit(navigation("s1")).unwrap();
        let busy = fixture.handle.submit(navigation("s1")).unwrap_err();
        assert_eq!(busy.code, ErrorCode::Busy);
    }

    #[test]
    fn syntax_size_and_session_are_validated_before_acceptance() {
        let fixture = fixture(8, 8);
        let mut foreign = navigation("other");
        foreign.session_id = SessionId::new("other").unwrap();
        assert_eq!(
            fixture.handle.submit(foreign).unwrap_err().code,
            ErrorCode::CapabilityDenied
        );
        let mut bad_version = navigation("s1");
        bad_version.schema_version = 1;
        assert_eq!(
            fixture.handle.submit(bad_version).unwrap_err().code,
            ErrorCode::ValidationFailed
        );
        let huge = CommandEnvelope::new(
            SessionId::new("s1").unwrap(),
            CommandBody::UpdateViewSession {
                view: ViewKind::Overview,
                selected_entity_id: None,
                selected_process: None,
                filter: "x".repeat(300),
                sort: SortKey::Pid,
                ascending: true,
                frame_revision: 0,
                overview_bar: None,
                process_bar: None,
            },
        );
        assert_eq!(
            fixture.handle.submit(huge).unwrap_err().code,
            ErrorCode::ValidationFailed
        );
        assert!(fixture.receiver.is_empty());
        assert_eq!(fixture.inbox.outstanding.current(), 0);
    }

    /// The control thread sleeps until its next deadline, so a resize
    /// wakes it instead of waiting for one; it requests no shutdown.
    #[test]
    fn a_viewport_change_wakes_the_control_thread() {
        let fixture = fixture(1, 1);
        assert!(fixture.wake.try_recv().is_err());
        fixture.handle.set_viewport(132, 43);
        assert_eq!(fixture.shared.viewport(), (132, 43));
        assert!(fixture.wake.try_recv().is_ok());
        assert!(!fixture.signal.is_requested());
    }

    #[test]
    fn shutdown_is_out_of_band_and_never_busy_on_full_queue() {
        let fixture = fixture(1, 1);
        fixture.handle.submit(navigation("s1")).unwrap();
        assert_eq!(
            fixture.handle.submit(navigation("s1")).unwrap_err().code,
            ErrorCode::Busy
        );
        fixture.handle.request_shutdown("user");
        assert!(fixture.signal.is_requested());
        assert!(fixture.wake.try_recv().is_ok());
        fixture.handle.request_shutdown("again");
        assert_eq!(fixture.signal.reason(), "user");
    }

    /// D-93: quitting has one path; the sealed command body is refused
    /// without taking a slot or requesting a shutdown.
    #[test]
    fn a_shutdown_command_is_refused_as_unsupported() {
        let fixture = fixture(4, 4);
        let shutdown = CommandEnvelope::new(
            SessionId::new("s1").unwrap(),
            CommandBody::Shutdown {
                reason: "user".into(),
            },
        );
        let rejection = fixture.handle.submit(shutdown).unwrap_err();
        assert_eq!(rejection.code, ErrorCode::Unsupported);
        assert!(rejection.message.contains("request_shutdown"));
        assert!(!fixture.signal.is_requested());
        assert_eq!(fixture.inbox.outstanding.current(), 0);
        assert!(fixture.receiver.try_recv().is_err());
    }

    #[test]
    fn closed_inbox_rejects_with_unavailable() {
        let fixture = fixture(4, 4);
        fixture.inbox.close();
        assert_eq!(
            fixture.handle.submit(navigation("s1")).unwrap_err().code,
            ErrorCode::Unavailable
        );
        assert_eq!(fixture.inbox.outstanding.current(), 0);
    }

    #[test]
    fn dropping_last_handle_closes_session_and_releases_slots() {
        let fixture = fixture(4, 4);
        let Fixture {
            handle,
            shared,
            inbox,
            receiver,
            ..
        } = fixture;
        let clone = handle.clone();
        handle.submit(navigation("s1")).unwrap();
        let queued = receiver.try_recv().unwrap();
        shared.deliver(CommandResult::completed(queued.envelope.request_id, "ok"));
        drop(handle);
        assert!(!shared.is_closed(), "a clone is still alive");
        drop(clone);
        assert!(shared.is_closed());
        assert_eq!(inbox.outstanding.current(), 0);
        // Late results for a closed session release their slot immediately.
        assert!(inbox.outstanding.try_reserve());
        shared.deliver(CommandResult::completed(Uuid::new_v4(), "late"));
        assert_eq!(inbox.outstanding.current(), 0);
    }
}
