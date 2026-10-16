//! The panels a key folds (D-114): the key each names on its title, and
//! the one row a folded panel keeps, `▸ Disk I/O ───── (i unfold)`, which a
//! click unfolds too. The keymap binds the same keys (its tests check
//! them).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::common::{Output, draw_line, key_spans};
use crate::format::clean;
use crate::registry::{DisplayContext, DisplayModuleManifest, controls};
use dgxtop_core::identity::EntityId;
use dgxtop_core::view::ViewModel;

/// One foldable panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fold {
    /// Its id in `view::FOLDABLE_PANELS`.
    pub(crate) panel: &'static str,
    pub(crate) key: char,
}

/// Every foldable panel, in the order of `view::FOLDABLE_PANELS`.
pub(crate) const FOLDS: [Fold; 10] = [
    Fold {
        panel: "disk",
        key: 'i',
    },
    Fold {
        panel: "network",
        key: 'n',
    },
    Fold {
        panel: "history.cpu",
        key: 'c',
    },
    Fold {
        panel: "history.memory",
        key: 'm',
    },
    Fold {
        panel: "history.gpu_utilization",
        key: 'u',
    },
    Fold {
        panel: "history.gpu_memory",
        key: 'v',
    },
    Fold {
        panel: "history.gpu_temperature",
        key: 't',
    },
    Fold {
        panel: "history.power",
        key: 'p',
    },
    Fold {
        panel: "history.disk",
        key: 'i',
    },
    Fold {
        panel: "history.network",
        key: 'n',
    },
];

/// The fold of `panel`.
pub(crate) fn fold_of(panel: &str) -> Option<Fold> {
    FOLDS.iter().copied().find(|fold| fold.panel == panel)
}

/// `i fold`: the keys a foldable panel names at the right of its title.
pub(crate) fn fold_keys(panel: &str) -> String {
    fold_of(panel).map_or_else(String::new, |fold| format!("{} fold", fold.key))
}

/// `▸ Disk I/O ────── (i unfold)`: the row of a folded panel, `width`
/// wide; the keys left out when the row has no room for them.
pub(crate) fn folded_line(
    title: &str,
    panel: &str,
    width: u16,
    context: &DisplayContext,
) -> Line<'static> {
    let theme = &context.theme;
    let mut spans = vec![
        Span::styled("▸ ", theme.muted_style()),
        Span::styled(clean(title), theme.section_title_style(title)),
        Span::raw(" "),
    ];
    let keys = fold_of(panel).map(|fold| format!("{} unfold", fold.key));
    let mut tail = keys.map_or_else(Vec::new, |keys| key_spans(&keys, context));
    let used: usize = spans.iter().chain(&tail).map(Span::width).sum();
    if used > usize::from(width) {
        tail.clear();
    }
    let used: usize = spans.iter().chain(&tail).map(Span::width).sum();
    spans.push(Span::styled(
        "─".repeat(usize::from(width).saturating_sub(used)),
        theme.border_style(false),
    ));
    spans.extend(tail);
    Line::from(spans)
}

/// Draws a folded panel's row at the top of `area` (the rest stays blank)
/// and publishes it as the target a click unfolds.
pub(crate) fn render_folded(
    frame: &mut Frame<'_>,
    area: Rect,
    (title, panel): (&str, &str),
    (manifest, model): (&DisplayModuleManifest, &ViewModel),
    context: &DisplayContext,
    output: &mut Output,
) {
    if area.is_empty() {
        return;
    }
    let row = Rect::new(area.x, area.y, area.width, 1);
    draw_line(
        frame.buffer_mut(),
        row,
        &folded_line(title, panel, row.width, context),
    );
    if let Ok(entity) = EntityId::new(panel) {
        output.entity_target(model, manifest, controls::PANEL_FOLD, row, entity);
    }
    output.label(format!("{} folded", clean(title)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::view::FOLDABLE_PANELS;

    #[test]
    fn every_foldable_panel_has_its_key_and_name_in_order() {
        let panels: Vec<&str> = FOLDS.iter().map(|fold| fold.panel).collect();
        assert_eq!(panels, FOLDABLE_PANELS);
        assert_eq!(fold_keys("history.power"), "p fold");
        assert_eq!(fold_keys("processes"), "");
        let context = crate::test_fixtures::display_context();
        let text = |width: u16| -> String {
            folded_line("Disk I/O", "disk", width, &context)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert_eq!(text(30), "▸ Disk I/O ─────── (i unfold) ");
        assert_eq!(text(14), "▸ Disk I/O ───", "the keys give way first");
    }
}
