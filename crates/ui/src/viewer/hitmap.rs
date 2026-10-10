//! HitMap of one rendered frame. Clicks are resolved against the map of the
//! frame the user actually saw, together with that frame's revision, so a
//! click can never be retargeted to whatever row now occupies the position.

use dgxtop_core::identity::ProcessKey;
use ratatui::layout::{Position, Rect};

use crate::registry::{HitTarget, controls};

/// Upper bound of targets kept per frame.
pub const MAX_HIT_TARGETS: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HitMap {
    frame_revision: u64,
    targets: Vec<HitTarget>,
}

impl HitMap {
    pub fn new(frame_revision: u64) -> Self {
        Self {
            frame_revision,
            targets: Vec::new(),
        }
    }

    /// Revision of the projection this frame was drawn from.
    pub fn frame_revision(&self) -> u64 {
        self.frame_revision
    }

    /// Adds a target clipped to `bounds`; targets outside are dropped.
    pub fn push_clipped(&mut self, mut target: HitTarget, bounds: Rect) {
        if self.targets.len() >= MAX_HIT_TARGETS {
            return;
        }
        target.rect = target.rect.intersection(bounds);
        if target.rect.is_empty() {
            return;
        }
        target.frame_revision = self.frame_revision;
        self.targets.push(target);
    }

    pub fn targets(&self) -> &[HitTarget] {
        &self.targets
    }

    /// Topmost target at a cell (later targets are drawn over earlier ones).
    pub fn hit(&self, column: u16, row: u16) -> Option<&HitTarget> {
        let position = Position::new(column, row);
        self.targets
            .iter()
            .rev()
            .find(|target| target.rect.contains(position))
    }

    pub fn count(&self, control_id: &str) -> usize {
        self.targets
            .iter()
            .filter(|target| target.control_id == control_id)
            .count()
    }

    /// The selected process row as drawn (the only valid terminate target).
    pub fn visible_selected_process(&self) -> Option<&ProcessKey> {
        self.targets
            .iter()
            .find(|target| target.control_id == controls::PROCESS_ROW_SELECTED)
            .and_then(|target| target.process_key.as_ref())
    }

    /// Number of process rows drawn (page size for PageUp/PageDown).
    pub fn visible_process_rows(&self) -> usize {
        self.count(controls::PROCESS_ROW) + self.count(controls::PROCESS_ROW_SELECTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::process_key;
    use dgxtop_core::identity::ModuleId;

    fn target(control_id: &'static str, rect: Rect) -> HitTarget {
        HitTarget {
            frame_revision: 0,
            module_id: ModuleId::new("processes").unwrap(),
            control_id,
            entity_id: None,
            process_key: Some(process_key(7, 1)),
            rect,
        }
    }

    #[test]
    fn hits_resolve_topmost_and_carry_the_frame_revision() {
        let mut map = HitMap::new(9);
        let bounds = Rect::new(0, 0, 80, 24);
        map.push_clipped(
            target(controls::PROCESS_ROW, Rect::new(0, 5, 80, 1)),
            bounds,
        );
        map.push_clipped(
            target(controls::PROCESS_ROW_SELECTED, Rect::new(10, 5, 5, 1)),
            bounds,
        );
        assert_eq!(
            map.hit(12, 5).unwrap().control_id,
            controls::PROCESS_ROW_SELECTED
        );
        assert_eq!(map.hit(1, 5).unwrap().control_id, controls::PROCESS_ROW);
        assert!(map.hit(1, 6).is_none());
        assert!(
            map.targets()
                .iter()
                .all(|target| target.frame_revision == 9)
        );
        assert_eq!(map.visible_process_rows(), 2);
        assert_eq!(map.visible_selected_process(), Some(&process_key(7, 1)));
    }

    #[test]
    fn targets_are_clipped_and_bounded() {
        let mut map = HitMap::new(1);
        let bounds = Rect::new(0, 0, 10, 10);
        map.push_clipped(
            target(controls::PROCESS_ROW, Rect::new(20, 20, 5, 1)),
            bounds,
        );
        assert!(map.targets().is_empty());
        map.push_clipped(
            target(controls::PROCESS_ROW, Rect::new(5, 0, 20, 1)),
            bounds,
        );
        assert_eq!(map.targets()[0].rect, Rect::new(5, 0, 5, 1));
        for _ in 0..(MAX_HIT_TARGETS + 10) {
            map.push_clipped(target(controls::PROCESS_ROW, Rect::new(0, 0, 1, 1)), bounds);
        }
        assert_eq!(map.targets().len(), MAX_HIT_TARGETS);
    }
}
