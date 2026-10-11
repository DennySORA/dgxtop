//! PresentationService: frame intervals and byte accounting of projections
//! (RUNTIME.md §5 presentation row, AT13).
//!
//! The view models themselves are built by `crate::presentation`;
//! `crate::pacing` decides *when* a session gets one (data at most
//! `viewer.fps` per second, ≤ 2 fps outside the normal overload state) and
//! this service makes sure every published projection holds a
//! presentation-partition `BytePermit`. A whole
//! `runtime.max_projection_bytes` is reserved *before* a model is built
//! (AT21: the peak is admitted before it is allocated) and shrunk to the
//! measured size afterwards. A frame the ledger cannot admit is skipped
//! before building, and a projection larger than the cap is dropped; both
//! are counted instead of over-committing. The previous pending projection
//! is released before the reservation so a session never needs room for
//! more than one pending plus the renderer's active frame.

use std::sync::Arc;

use dgxtop_core::budget::{ByteLedger, BytePermit, Partition};
use dgxtop_core::clock::{NANOS_PER_MILLI, NANOS_PER_SECOND};
use dgxtop_core::diagnostics::{Diagnostics, names};
use dgxtop_core::view::ViewModel;

use crate::session::SessionShared;

/// Outcome of publishing one projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    Published { bytes: u64 },
    TooLarge { bytes: u64 },
    LedgerFull { bytes: u64 },
}

#[derive(Debug)]
pub(crate) struct PresentationService {
    ledger: ByteLedger,
    max_projection_bytes: u64,
    diagnostics: Diagnostics,
    frame_counter: u64,
}

impl PresentationService {
    pub(crate) fn new(
        ledger: ByteLedger,
        max_projection_bytes: u32,
        diagnostics: Diagnostics,
    ) -> Self {
        Self {
            ledger,
            max_projection_bytes: u64::from(max_projection_bytes),
            diagnostics,
            frame_counter: 0,
        }
    }

    /// Next global frame revision (monotonic, never reused).
    pub(crate) fn next_frame_revision(&mut self) -> u64 {
        self.frame_counter = self.frame_counter.saturating_add(1);
        self.frame_counter
    }

    /// Reserves a whole projection's worth of presentation bytes before the
    /// model is built; the session's pending projection is released first.
    pub(crate) fn reserve_frame(
        &self,
        session: &SessionShared,
    ) -> Result<BytePermit, PublishOutcome> {
        session.discard_pending_projection();
        self.ledger
            .try_reserve(Partition::Presentation, self.max_projection_bytes)
            .map_err(|_| {
                self.diagnostics
                    .increment(names::QUEUE_DROPPED_TOTAL, "projection_ledger_full", 1);
                PublishOutcome::LedgerFull {
                    bytes: self.max_projection_bytes,
                }
            })
    }

    /// Publishes a model built under `permit`, keeping only its measured size.
    pub(crate) fn publish_reserved(
        &self,
        mut model: ViewModel,
        measured_bytes: usize,
        mut permit: BytePermit,
        session: &SessionShared,
    ) -> PublishOutcome {
        let bytes = u64::try_from(measured_bytes).unwrap_or(u64::MAX);
        if bytes > self.max_projection_bytes {
            self.diagnostics
                .increment(names::QUEUE_DROPPED_TOTAL, "projection_too_large", 1);
            return PublishOutcome::TooLarge { bytes };
        }
        permit.shrink_to(bytes);
        model.permit = Some(permit);
        session.publish(Arc::new(model));
        PublishOutcome::Published { bytes }
    }

    /// Reserve, then publish an already built model (tests).
    #[cfg(test)]
    pub(crate) fn publish(&self, model: ViewModel, session: &SessionShared) -> PublishOutcome {
        match self.reserve_frame(session) {
            Ok(permit) => {
                let measured = model.estimated_bytes();
                self.publish_reserved(model, measured, permit, session)
            }
            Err(skipped) => skipped,
        }
    }
}

/// Minimum spacing of frames at `fps` frames per second.
pub(crate) fn frame_interval_ns(fps: u32) -> u64 {
    NANOS_PER_SECOND.div_ceil(u64::from(fps.max(1)))
}

/// Spacing of input-driven projections in the normal state.
const PROJECTION_INTERVAL_NS: u64 = 50 * NANOS_PER_MILLI;

/// Minimum spacing of any two projections, what input waits for. In the
/// normal state a key is answered within 50 ms while data frames follow
/// `fps` (UI.md §4, `crate::pacing`); outside it everything follows the
/// reduced frame rate to shed work (RUNTIME.md §7).
pub(crate) fn projection_interval_ns(normal: bool, fps: u32) -> u64 {
    let frame = frame_interval_ns(fps);
    if normal {
        frame.min(PROJECTION_INTERVAL_NS)
    } else {
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Outstanding;
    use dgxtop_core::commands::{SortKey, ViewKind};
    use dgxtop_core::config::Theme;
    use dgxtop_core::identity::SessionId;
    use dgxtop_core::view::{HeaderView, ModuleViews, OverloadState, SessionState};

    fn model(frame: u64, padding: usize) -> ViewModel {
        ViewModel {
            frame_revision: frame,
            snapshot_revision: 1,
            generated_boot_ns: 0,
            header: HeaderView {
                host_name: "x".repeat(padding),
                kernel: None,
                uptime_s: None,
                version: "0".into(),
                namespace_label: None,
                overload: OverloadState::Normal,
                configured_interval_ms: 1000,
                effective_interval_ms: 1000,
                hosts_healthy: 0,
                hosts_total: 0,
                read_only: false,
                theme: Theme::Dark,
                redline_threshold_percent: 80.0,
            },
            session: SessionState {
                session_id: SessionId::new("s1").unwrap(),
                view: ViewKind::Overview,
                selected_entity_id: None,
                selected_process: None,
                filter: String::new(),
                sort: SortKey::GpuMemory,
                ascending: false,
                overview_bar: dgxtop_core::process_history::BarSize::OneMinute,
                process_bar: dgxtop_core::process_history::BarSize::Auto,
            },
            modules: ModuleViews::default(),
            banners: Vec::new(),
            enabled_modules: Vec::new(),
            permit: None,
        }
    }

    fn session() -> SessionShared {
        SessionShared::new(SessionId::new("s1").unwrap(), Arc::new(Outstanding::new(4)))
    }

    #[test]
    fn published_projection_holds_its_bytes_until_dropped() {
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let mut service = PresentationService::new(ledger.clone(), 1_048_576, Diagnostics::new());
        let session = session();
        let frame = service.next_frame_revision();
        let PublishOutcome::Published { bytes } = service.publish(model(frame, 10), &session)
        else {
            panic!("published expected");
        };
        assert_eq!(ledger.used(Partition::Presentation), bytes);
        // A newer projection replaces the pending one (latest only).
        let newer = service.next_frame_revision();
        service.publish(model(newer, 10), &session);
        assert!(ledger.used(Partition::Presentation) <= bytes + 2);
        let taken = session_take(&session).unwrap();
        assert_eq!(taken.frame_revision, newer);
        assert!(
            ledger.used(Partition::Presentation) > 0,
            "active frame still accounted"
        );
        drop(taken);
        assert_eq!(ledger.used(Partition::Presentation), 0);
        assert!(newer > frame);
    }

    #[test]
    fn oversized_or_unadmitted_projections_are_skipped() {
        let diagnostics = Diagnostics::new();
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let service = PresentationService::new(ledger.clone(), 65_536, diagnostics.clone());
        let session = session();
        assert!(matches!(
            service.publish(model(1, 100_000), &session),
            PublishOutcome::TooLarge { .. }
        ));
        assert_eq!(
            diagnostics.counter(names::QUEUE_DROPPED_TOTAL, "projection_too_large"),
            1
        );
        // Fill the presentation partition so the next reservation fails.
        let limit = ledger.limits().limit(Partition::Presentation);
        let _hog = ledger.try_reserve(Partition::Presentation, limit).unwrap();
        assert!(matches!(
            service.publish(model(2, 10), &session),
            PublishOutcome::LedgerFull { .. }
        ));
        assert!(session_take(&session).is_none());
    }

    #[test]
    fn a_full_projection_budget_is_reserved_before_building_and_shrunk_after() {
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let service = PresentationService::new(ledger.clone(), 65_536, Diagnostics::new());
        let session = session();
        let permit = service.reserve_frame(&session).unwrap();
        // AT21: the peak is admitted before the model is allocated.
        assert_eq!(ledger.used(Partition::Presentation), 65_536);
        let built = model(1, 10);
        let measured = built.estimated_bytes();
        assert!(matches!(
            service.publish_reserved(built, measured, permit, &session),
            PublishOutcome::Published { .. }
        ));
        assert_eq!(
            ledger.used(Partition::Presentation),
            u64::try_from(measured).unwrap()
        );
        drop(session_take(&session));
        assert_eq!(ledger.used(Partition::Presentation), 0);
    }

    #[test]
    fn a_ledger_without_room_for_a_full_projection_skips_the_frame_before_building() {
        let diagnostics = Diagnostics::new();
        let ledger = ByteLedger::for_memory_limit_mib(256);
        let service = PresentationService::new(ledger.clone(), 65_536, diagnostics.clone());
        let session = session();
        let limit = ledger.limits().limit(Partition::Presentation);
        // Leaves less than one full projection of room.
        let _hog = ledger
            .try_reserve(Partition::Presentation, limit - 65_535)
            .unwrap();
        assert!(matches!(
            service.reserve_frame(&session),
            Err(PublishOutcome::LedgerFull { .. })
        ));
        assert_eq!(
            diagnostics.counter(names::QUEUE_DROPPED_TOTAL, "projection_ledger_full"),
            1
        );
        assert_eq!(ledger.used(Partition::Presentation), limit - 65_535);
    }

    fn session_take(session: &SessionShared) -> Option<Arc<ViewModel>> {
        session.take_projection()
    }
}
