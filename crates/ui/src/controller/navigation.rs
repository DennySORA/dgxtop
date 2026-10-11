//! Identity-based selection movement. Targets are computed from the rows the
//! projection carries and expressed as `ProcessKey`/entity identity — never
//! as a row index. The process table is a window (visible rows + overscan)
//! into the full list: a target outside it selects the best known row in its
//! direction and keeps the absolute goal so the next (re-centred) projection
//! can continue toward it.

use dgxtop_core::identity::{EntityId, ProcessKey};
use dgxtop_core::view::ProcessesView;

use crate::num::saturate_u32;

/// Sampling interval ladder for `+` (faster) and `-` (slower), in ms.
pub const INTERVAL_STEPS_MS: [u32; 11] = [
    100, 200, 250, 500, 1000, 2000, 3000, 5000, 10_000, 30_000, 60_000,
];

/// Movement requested by a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Movement {
    Down(u32),
    Up(u32),
    First,
    Last,
}

/// Next process to select; `goal` is the absolute index still to reach
/// when the target lies outside the projected window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessStep {
    pub key: ProcessKey,
    pub index: u32,
    pub goal: Option<u32>,
}

/// Absolute index of a key inside the projected window.
pub fn absolute_index(view: &ProcessesView, key: &ProcessKey) -> Option<u32> {
    view.rows
        .iter()
        .position(|row| &row.key == key)
        .map(|position| view.window_start.saturating_add(saturate_u32(position)))
}

/// Resolves a movement from `base_index` (or the projection's selection).
pub fn process_target(
    view: &ProcessesView,
    base_index: Option<u32>,
    movement: Movement,
) -> Option<ProcessStep> {
    if view.rows.is_empty() {
        return None;
    }
    let window_end = view
        .window_start
        .saturating_add(saturate_u32(view.rows.len()));
    let last = view.total_rows.max(window_end).saturating_sub(1);
    let current = base_index.or(view.selected_index).or_else(|| {
        view.selected
            .as_ref()
            .and_then(|key| absolute_index(view, key))
    });
    let target = match movement {
        Movement::Down(step) => {
            current.map_or(view.window_start, |index| index.saturating_add(step))
        }
        Movement::Up(step) => current.map_or(view.window_start, |index| index.saturating_sub(step)),
        Movement::First => 0,
        Movement::Last => last,
    };
    process_toward(view, target.min(last))
}

/// Best known row toward an absolute goal.
pub fn process_toward(view: &ProcessesView, goal: u32) -> Option<ProcessStep> {
    let first = view.rows.first()?;
    let last = view.rows.last()?;
    let window_start = view.window_start;
    let window_end = window_start.saturating_add(saturate_u32(view.rows.len()));
    if goal < window_start {
        return Some(ProcessStep {
            key: first.key.clone(),
            index: window_start,
            goal: Some(goal),
        });
    }
    if goal >= window_end {
        return Some(ProcessStep {
            key: last.key.clone(),
            index: window_end.saturating_sub(1),
            goal: Some(goal),
        });
    }
    let offset = usize::try_from(goal - window_start).ok()?;
    view.rows.get(offset).map(|row| ProcessStep {
        key: row.key.clone(),
        index: goal,
        goal: None,
    })
}

/// Moves within a complete list (GPU cards, series, metrics, modules).
pub fn step_list<T: PartialEq + Clone>(
    items: &[T],
    current: Option<&T>,
    movement: Movement,
) -> Option<T> {
    let last = items.len().checked_sub(1)?;
    let position = current.and_then(|current| items.iter().position(|item| item == current));
    let distance = |step: u32| usize::try_from(step).unwrap_or(usize::MAX);
    let index = match (movement, position) {
        (Movement::First, _) => 0,
        (Movement::Last, _) => last,
        (Movement::Down(step), Some(position)) => position.saturating_add(distance(step)).min(last),
        (Movement::Up(step), Some(position)) => position.saturating_sub(distance(step)),
        (_, None) => 0,
    };
    items.get(index).cloned()
}

/// GPU filter cycle: all → each GPU in order → all.
pub fn cycle_gpu_filter(
    choices: &[(EntityId, String)],
    current: Option<&EntityId>,
) -> Option<EntityId> {
    match current {
        None => choices.first().map(|(id, _)| id.clone()),
        Some(current) => {
            let position = choices.iter().position(|(id, _)| id == current)?;
            choices.get(position + 1).map(|(id, _)| id.clone())
        }
    }
}

/// Next interval on the ladder (`None` at either end).
pub fn next_interval(current_ms: u32, faster: bool) -> Option<u32> {
    if faster {
        INTERVAL_STEPS_MS
            .iter()
            .rev()
            .copied()
            .find(|step| *step < current_ms)
    } else {
        INTERVAL_STEPS_MS
            .iter()
            .copied()
            .find(|step| *step > current_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_steps_are_clamped() {
        let items = ["a", "b", "c"];
        assert_eq!(step_list(&items, Some(&"b"), Movement::Down(1)), Some("c"));
        assert_eq!(step_list(&items, Some(&"c"), Movement::Down(1)), Some("c"));
        assert_eq!(step_list(&items, Some(&"a"), Movement::Up(1)), Some("a"));
        assert_eq!(step_list(&items, None, Movement::Down(1)), Some("a"));
        assert_eq!(step_list(&items, Some(&"a"), Movement::Last), Some("c"));
        assert_eq!(step_list::<&str>(&[], None, Movement::First), None);
    }
}
