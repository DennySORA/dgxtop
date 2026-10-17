//! Scrolling a module's page (D-107, D-110, D-116). The module draws its
//! page from the unit scrolled to (`DisplayContext.offset`: a band of the
//! Overview History, a page of the GPU view's sections or of an opened
//! run's charts) and publishes a
//! [`controls::PAGE_ABOVE`] target for each unit above what it showed and a
//! [`controls::PAGE_BELOW`] target for each unit below. The controller
//! counts them, so it scrolls from what the frame showed: never past the
//! last unit, and a taller frame never leaves keys that do nothing.

use dgxtop_core::identity::ModuleId;

use super::keymap::KeyAction;
use super::navigation::Movement;
use super::{Controller, ControllerContext};
use crate::num::saturate_u16;
use crate::registry::controls;

/// Where a key moves a module's page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageStep {
    Up,
    Down,
    First,
    Last,
}

impl PageStep {
    /// The page step of a list movement (a page or a row is a unit).
    pub(super) const fn of(movement: Movement) -> Self {
        match movement {
            Movement::Up(_) => Self::Up,
            Movement::Down(_) => Self::Down,
            Movement::First => Self::First,
            Movement::Last => Self::Last,
        }
    }

    /// The page step of a history view's `↑`/`↓` or `g`/`G` key.
    pub(super) const fn of_history_key(action: KeyAction) -> Option<Self> {
        match action {
            KeyAction::HistoryUp => Some(Self::Up),
            KeyAction::HistoryDown => Some(Self::Down),
            KeyAction::HistoryFirst => Some(Self::First),
            KeyAction::HistoryLast => Some(Self::Last),
            _ => None,
        }
    }
}

impl Controller {
    /// Moves `module`'s page by `step`, counted from the frame's units.
    pub(super) fn scroll_page(
        &mut self,
        module: &str,
        step: PageStep,
        context: &ControllerContext<'_>,
    ) {
        let Ok(module) = ModuleId::new(module) else {
            return;
        };
        let above = context.hit_map.count_of(&module, controls::PAGE_ABOVE);
        let below = context.hit_map.count_of(&module, controls::PAGE_BELOW);
        let first = match step {
            PageStep::Up => above.saturating_sub(1),
            PageStep::Down => above + usize::from(below > 0),
            PageStep::First => 0,
            PageStep::Last => above + below,
        };
        let next = saturate_u16(first);
        let offset = self.offsets.entry(module).or_insert(0);
        if next != *offset {
            *offset = next;
            self.bump();
        }
    }
}
