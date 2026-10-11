//! Viewer composition (`ARCHITECTURE.md` §9): LayoutEngine, module registry,
//! chrome, banners, summary strip, help overlay and the HitMap of the frame.
//!
//! `Viewer::render` is pure presentation: it reads one immutable projection
//! and the redacted controller state, never performs I/O, never blocks and
//! never queries anything.

mod banners;
mod chrome;
mod help;
mod hitmap;
mod isolation;
pub mod layout;
mod summary;
mod ui_state;

use dgxtop_core::commands::ViewKind;
use dgxtop_core::identity::ModuleId;
use dgxtop_core::view::ViewModel;
use ratatui::Frame;
use ratatui::widgets::Clear;

pub use banners::{BannerBlock, wrap_words};
pub use hitmap::{HitMap, MAX_HIT_TARGETS};
pub use isolation::in_isolated_render;
pub use layout::{MIN_COLUMNS, MIN_ROWS, SizeTier, size_tier};
pub use ui_state::{ModeView, Notice, UiState};

use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModuleManifest, ModuleRegistry, OverviewCell, Slot, detail_module_id,
};
use crate::theme::ThemePalette;
use layout::{Candidate, LayoutRequest, Placement};

/// Upper bound of accessibility labels per frame.
const MAX_FRAME_LABELS: usize = 512;

/// Inputs of one frame.
#[derive(Debug, Clone, Copy)]
pub struct RenderInput<'a> {
    /// The active projection (`None` before the first one arrives).
    pub model: Option<&'a ViewModel>,
    pub ui: &'a UiState,
    /// Time since the projection was received (advances age text).
    pub projection_age_ms: u64,
    /// Seconds-granularity animation counter.
    pub tick: u64,
}

/// What a frame produced besides cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameReport {
    pub tier: SizeTier,
    pub hit_map: HitMap,
    pub accessibility_labels: Vec<String>,
    pub rendered_modules: Vec<String>,
    pub hidden_modules: Vec<String>,
    pub module_errors: Vec<String>,
    pub dropped_banners: usize,
}

impl FrameReport {
    fn new(tier: SizeTier, frame_revision: u64) -> Self {
        Self {
            tier,
            hit_map: HitMap::new(frame_revision),
            accessibility_labels: Vec::new(),
            rendered_modules: Vec::new(),
            hidden_modules: Vec::new(),
            module_errors: Vec::new(),
            dropped_banners: 0,
        }
    }
}

/// The Viewer: registry + composition.
#[derive(Debug)]
pub struct Viewer {
    registry: ModuleRegistry,
    /// Module id stamped on chrome hit targets (tabs).
    chrome_id: Option<ModuleId>,
}

impl Viewer {
    pub fn new(registry: ModuleRegistry) -> Self {
        Self {
            registry,
            chrome_id: ModuleId::new("viewer").ok(),
        }
    }

    /// Viewer over the ten built-in modules.
    pub fn builtin() -> Self {
        Self::new(ModuleRegistry::builtin())
    }

    pub fn registry(&self) -> &ModuleRegistry {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut ModuleRegistry {
        &mut self.registry
    }

    /// Renders one complete frame and returns its HitMap and labels.
    pub fn render(&self, frame: &mut Frame<'_>, input: &RenderInput<'_>) -> FrameReport {
        let area = frame.area();
        // Start every frame from a clean buffer so nothing stale survives a
        // failed or partial previous draw.
        frame.render_widget(Clear, area);
        let palette = input
            .model
            .map_or_else(ThemePalette::default, ThemePalette::for_model);
        let tier = size_tier(area.width, area.height);
        let mut report =
            FrameReport::new(tier, input.model.map_or(0, |model| model.frame_revision));
        if tier == SizeTier::TooSmall {
            chrome::render_too_small(frame, area, input.ui, &palette);
            return report;
        }
        let Some(model) = input.model else {
            chrome::render_waiting(frame, area, input.ui, &palette);
            return report;
        };
        self.render_model(frame, model, input, &palette, &mut report);
        report
    }

    fn render_model(
        &self,
        frame: &mut Frame<'_>,
        model: &ViewModel,
        input: &RenderInput<'_>,
        palette: &ThemePalette,
        report: &mut FrameReport,
    ) {
        let area = frame.area();
        let view = model.session.view;
        let banners = banners::collect(
            model,
            input.ui,
            self.registry.warnings(),
            input.projection_age_ms,
            area.width,
        );
        let heights: Vec<u16> = banners
            .iter()
            .map(|block| block.height(area.width))
            .collect();
        let overview = self.overview_candidates(model, input.ui);
        let detail = self.detail_candidate(view);
        let preferred = |index: usize, width: u16| {
            let module = self.registry.module_at(index)?;
            let collapsed = (!is_expanded(input.ui, module.manifest()))
                .then(|| module.collapsed_rows(model, width, Slot::Overview))
                .flatten();
            collapsed.or_else(|| module.preferred_rows(model, width, Slot::Overview))
        };
        let plan = layout::plan(&LayoutRequest {
            area,
            view,
            banner_heights: &heights,
            overview: &overview,
            detail,
            summary_lines: saturate_u16(summary::summary_lines(model, palette, area.width).len()),
            preferred_rows: &preferred,
        });
        let history_page = ModuleId::new(crate::modules::HISTORY_MODULE_ID)
            .ok()
            .and_then(|history| input.ui.tabs.get(&history).copied())
            .unwrap_or(0);
        let tab = crate::views::Tab::of(view, history_page);
        chrome::render_header(
            frame,
            plan.header,
            &chrome::HeaderInput {
                model,
                tab,
                show_tabs: plan.tier != SizeTier::Minimal,
                projection_age_ms: input.projection_age_ms,
                chrome_id: self.chrome_id.as_ref(),
            },
            palette,
            &mut report.hit_map,
        );
        if let Some(selector) = plan.tabs {
            chrome::render_selector(frame, selector, tab, palette);
        }
        for (rect, block) in plan.banners.iter().zip(&banners) {
            banners::render_block(frame, *rect, block, palette);
        }
        if let Some(summary_area) = plan.summary {
            summary::render(frame, summary_area, model, palette);
        }
        for placement in &plan.placements {
            self.render_placement(frame, placement, model, input, palette, report);
        }
        if let Some(note) = plan.body_note {
            chrome::render_body_note(frame, note, &self.body_note_text(view), palette);
        }
        let interval = chrome::interval_text(model);
        chrome::render_footer(frame, plan.footer, input.ui, view, Some(&interval), palette);
        if let ModeView::Help { scroll } = input.ui.mode {
            help::render_help(frame, plan.body, scroll, palette);
        }
        report.hidden_modules = plan
            .hidden
            .iter()
            .filter_map(|index| self.registry.module_at(*index))
            .map(|module| module.manifest().module_id.to_string())
            .collect();
        report.dropped_banners = plan.dropped_banners;
    }

    fn render_placement(
        &self,
        frame: &mut Frame<'_>,
        placement: &Placement,
        model: &ViewModel,
        input: &RenderInput<'_>,
        palette: &ThemePalette,
        report: &mut FrameReport,
    ) {
        let Some(module) = self.registry.module_at(placement.index) else {
            return;
        };
        let module_id = module.manifest().module_id.to_string();
        let context = DisplayContext {
            theme: *palette,
            viewport: frame.area(),
            tick_for_animation: input.tick,
            compact: placement.compact,
            slot: placement.slot,
            projection_age_ms: input.projection_age_ms,
            expanded: is_expanded(input.ui, module.manifest()),
            scroll: input
                .ui
                .scroll
                .get(&module.manifest().module_id)
                .copied()
                .unwrap_or(0),
            tab: input
                .ui
                .tabs
                .get(&module.manifest().module_id)
                .copied()
                .unwrap_or(0),
        };
        let outcome =
            isolation::render_isolated(|| module.render(&context, model, placement.area, frame));
        match outcome {
            Ok(output) => {
                for target in output.hit_targets {
                    report.hit_map.push_clipped(target, placement.area);
                }
                let room = MAX_FRAME_LABELS.saturating_sub(report.accessibility_labels.len());
                report
                    .accessibility_labels
                    .extend(output.accessibility_labels.into_iter().take(room));
                report.rendered_modules.push(module_id);
            }
            Err(message) => {
                frame.render_widget(Clear, placement.area);
                let title = &module.manifest().title;
                chrome::render_module_error(frame, placement.area, title, &message, palette);
                report.module_errors.push(format!("{module_id}: {message}"));
            }
        }
    }

    fn overview_candidates(&self, model: &ViewModel, ui: &UiState) -> Vec<Candidate> {
        self.registry
            .iter()
            .enumerate()
            .filter(|(_, module)| module.manifest().has_slot(Slot::Overview))
            .filter(|(_, module)| module_enabled(model, module.manifest()))
            .map(|(index, module)| Candidate {
                expanded: is_expanded(ui, module.manifest()),
                ..candidate(index, module.manifest(), module.overview_cell())
            })
            .collect()
    }

    fn detail_candidate(&self, view: ViewKind) -> Option<Candidate> {
        let module_id = detail_module_id(view)?;
        self.registry
            .iter()
            .enumerate()
            .find(|(_, module)| {
                module.manifest().module_id.as_str() == module_id
                    && module.manifest().has_slot(Slot::Detail)
            })
            .map(|(index, module)| candidate(index, module.manifest(), None))
    }

    fn body_note_text(&self, view: ViewKind) -> String {
        match self.registry.detail_module_for(view) {
            Some(module) => {
                let manifest = module.manifest();
                format!(
                    "enlarge the terminal to show {} (the panel needs at least {}x{})",
                    view.title(),
                    manifest.min_columns,
                    manifest.min_rows
                )
            }
            None => format!(
                "no display module is registered for the {} view",
                view.title()
            ),
        }
    }
}

fn candidate(
    index: usize,
    manifest: &DisplayModuleManifest,
    cell: Option<OverviewCell>,
) -> Candidate {
    Candidate {
        index,
        min_columns: manifest.min_columns,
        min_rows: manifest.min_rows,
        priority: manifest.priority,
        cell,
        expanded: false,
    }
}

fn is_expanded(ui: &UiState, manifest: &DisplayModuleManifest) -> bool {
    ui.expanded.contains(&manifest.module_id)
}

/// Enabled modules come from the effective configuration in the projection;
/// an empty list (not yet known) falls back to the manifest default.
fn module_enabled(model: &ViewModel, manifest: &DisplayModuleManifest) -> bool {
    if model.enabled_modules.is_empty() {
        manifest.enabled_by_default
    } else {
        model.enabled_modules.contains(&manifest.module_id)
    }
}

#[cfg(test)]
mod tests;
