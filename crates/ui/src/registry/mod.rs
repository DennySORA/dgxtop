//! Display module contract (`contracts/INTERFACES.md` §6) and the trusted,
//! compile-time module registry.
//!
//! A module renders only from the immutable [`ViewModel`] and its allocated
//! area; [`DisplayContext`] carries presentation facts only (no ports, no
//! stores, no clocks). Registration rejects duplicate ids and any
//! `api_version` other than 1, keeping a visible warning for each rejection.

mod manifest;

use dgxtop_core::commands::ViewKind;
use dgxtop_core::identity::{EntityId, ModuleId, ProcessKey};
use dgxtop_core::view::ViewModel;
use ratatui::Frame;
use ratatui::layout::Rect;
use thiserror::Error;

pub use manifest::{DISPLAY_MODULE_API_VERSION, DisplayModuleManifest, ManifestError, Slot};

use crate::theme::ThemePalette;

/// Upper bound of registered modules (config allows 64 enabled ids).
pub const MAX_MODULES: usize = 64;
/// Upper bound of retained registry warnings.
const MAX_WARNINGS: usize = 16;

/// Presentation-only context of one render call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayContext {
    pub theme: ThemePalette,
    /// Whole terminal area of this frame.
    pub viewport: Rect,
    /// Seconds-granularity counter for spinners; never a data clock.
    pub tick_for_animation: u64,
    /// Narrow/short layout: render the summary form.
    pub compact: bool,
    /// Slot this call renders into.
    pub slot: Slot,
    /// Time since the projection arrived; advances age text between
    /// projections without the module reading a clock.
    pub projection_age_ms: u64,
    /// The user expanded this module (a collapsible module shows its detail;
    /// see [`DisplayModule::collapsed_rows`]).
    pub expanded: bool,
    /// How far the user scrolled this module's scrollable part (D-70); the
    /// module clamps it to what it has. A module with a list uses it as the
    /// list's cursor.
    pub scroll: u16,
    /// The module's inner tab (D-72): 0 its first page.
    pub tab: u8,
}

/// Clickable region produced by a module, resolved by the Controller through
/// the HitMap of the frame it was drawn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitTarget {
    pub frame_revision: u64,
    pub module_id: ModuleId,
    pub control_id: &'static str,
    pub entity_id: Option<EntityId>,
    pub process_key: Option<ProcessKey>,
    pub rect: Rect,
}

/// Where a module sits in the overview grid (D-38). Modules sharing a `row`
/// sit side by side, left to right by `column`, with widths in proportion to
/// `weight`; rows run top to bottom. The row of a `fills` module takes the
/// rows the other rows leave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverviewCell {
    pub row: u8,
    pub column: u8,
    pub weight: u16,
    pub fills: bool,
}

impl OverviewCell {
    /// A cell of `row` at `column` with `weight` that does not fill.
    pub const fn at(row: u8, column: u8, weight: u16) -> Self {
        Self {
            row,
            column,
            weight,
            fills: false,
        }
    }

    /// The same cell, taking the rows the other rows leave.
    pub const fn filling(mut self) -> Self {
        self.fills = true;
        self
    }
}

/// Result of one module render.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderOutput {
    pub hit_targets: Vec<HitTarget>,
    pub accessibility_labels: Vec<String>,
}

/// One display module. `render` must not block, perform I/O or keep
/// references to the projection beyond the call.
pub trait DisplayModule: Send {
    fn manifest(&self) -> &DisplayModuleManifest;
    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput;

    /// Height this module would like at `width` columns in `slot`; `None`
    /// means "take spare rows". A layout hint only: the default keeps a
    /// module at its manifest minimum plus its share of spare rows.
    fn preferred_rows(&self, _model: &ViewModel, _width: u16, _slot: Slot) -> Option<u16> {
        None
    }

    /// Height of the collapsed form of a module that starts collapsed and
    /// shows its detail once the user expands it (a click on its
    /// [`controls::MODULE_TOGGLE`] target); `None` = not collapsible.
    fn collapsed_rows(&self, _model: &ViewModel, _width: u16, _slot: Slot) -> Option<u16> {
        None
    }

    /// Position in the overview grid; `None` gives the module a full-width
    /// row of its own below the placed rows, ordered by priority.
    fn overview_cell(&self) -> Option<OverviewCell> {
        None
    }
}

/// Well-known control ids shared by modules (producers) and the Controller
/// (consumer of the HitMap).
pub mod controls {
    /// A visible process row (carries the full `ProcessKey`).
    pub const PROCESS_ROW: &str = "processes.row";
    /// The highlighted selected process row; the only valid `K` target.
    pub const PROCESS_ROW_SELECTED: &str = "processes.row.selected";
    /// A GPU card or GPU selector chip (carries the GPU entity id).
    pub const GPU_CARD: &str = "gpu.card";
    /// A module toggle row in settings (entity id = module id).
    pub const SETTINGS_MODULE: &str = "settings.module";
    /// Expands or collapses the module that drew the target.
    pub const MODULE_TOGGLE: &str = "module.toggle";
    /// A part of the module that drew the target the mouse wheel scrolls
    /// (D-70).
    pub const MODULE_SCROLL: &str = "module.scroll";
}

/// Why a module was not registered.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegistryError {
    #[error("display module {0:?} is already registered")]
    DuplicateId(String),
    #[error("display module {module_id:?} declares api_version {found}; only 1 is supported")]
    UnsupportedApiVersion { module_id: String, found: u32 },
    #[error("display module {module_id:?} has an invalid manifest: {reason}")]
    InvalidManifest { module_id: String, reason: String },
    #[error("display module registry is full ({MAX_MODULES} modules)")]
    Full,
}

/// Trusted compile-time registry. Order of registration is irrelevant to
/// layout: placement is derived from manifests (slots, priority, sizes).
pub struct ModuleRegistry {
    modules: Vec<Box<dyn DisplayModule>>,
    warnings: Vec<String>,
}

impl std::fmt::Debug for ModuleRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModuleRegistry")
            .field("modules", &self.module_ids())
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

impl ModuleRegistry {
    pub fn empty() -> Self {
        Self {
            modules: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// The built-in modules (`config::BUILTIN_MODULES`).
    pub fn builtin() -> Self {
        let mut registry = Self::empty();
        for candidate in crate::modules::builtin_modules() {
            match candidate {
                Ok(module) => {
                    // Rejections are already recorded as warnings by `register`.
                    let _ = registry.register(module);
                }
                Err(error) => registry.warn(format!("built-in module failed to build: {error}")),
            }
        }
        registry
    }

    /// Registers a module after validating its manifest.
    pub fn register(&mut self, module: Box<dyn DisplayModule>) -> Result<(), RegistryError> {
        let result = self.check(module.manifest());
        match result {
            Ok(()) => {
                self.modules.push(module);
                Ok(())
            }
            Err(error) => {
                self.warn(error.to_string());
                Err(error)
            }
        }
    }

    fn check(&self, manifest: &DisplayModuleManifest) -> Result<(), RegistryError> {
        let module_id = manifest.module_id.as_str().to_owned();
        if manifest.api_version != DISPLAY_MODULE_API_VERSION {
            return Err(RegistryError::UnsupportedApiVersion {
                module_id,
                found: manifest.api_version,
            });
        }
        if let Err(error) = manifest.validate() {
            return Err(RegistryError::InvalidManifest {
                module_id,
                reason: error.to_string(),
            });
        }
        if self.get(manifest.module_id.as_str()).is_some() {
            return Err(RegistryError::DuplicateId(module_id));
        }
        if self.modules.len() >= MAX_MODULES {
            return Err(RegistryError::Full);
        }
        Ok(())
    }

    fn warn(&mut self, warning: String) {
        if self.warnings.len() == MAX_WARNINGS {
            self.warnings.remove(0);
        }
        self.warnings.push(warning);
    }

    /// Visible registration warnings (rejected modules).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn get(&self, module_id: &str) -> Option<&dyn DisplayModule> {
        self.modules
            .iter()
            .find(|module| module.manifest().module_id.as_str() == module_id)
            .map(|module| &**module)
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    pub fn module_ids(&self) -> Vec<ModuleId> {
        self.modules
            .iter()
            .map(|module| module.manifest().module_id.clone())
            .collect()
    }

    /// Module at a registration index (the layout engine's handle).
    pub fn module_at(&self, index: usize) -> Option<&dyn DisplayModule> {
        self.modules.get(index).map(|module| &**module)
    }

    /// Modules in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &dyn DisplayModule> {
        self.modules.iter().map(|module| &**module)
    }

    /// Module that renders the main area of a detail view.
    pub fn detail_module_for(&self, view: ViewKind) -> Option<&dyn DisplayModule> {
        detail_module_id(view)
            .and_then(|module_id| self.get(module_id))
            .filter(|module| module.manifest().has_slot(Slot::Detail))
    }
}

/// View → module mapping of the detail views. `Overview` is composed from
/// every overview-slot module instead, and the retired Processes, Sources
/// and Actions views have none (D-91).
pub const fn detail_module_id(view: ViewKind) -> Option<&'static str> {
    match view {
        ViewKind::GpuDetail => Some("gpu"),
        ViewKind::History => Some("history"),
        ViewKind::Settings => Some("settings"),
        ViewKind::Overview | ViewKind::Processes | ViewKind::Sources | ViewKind::Actions => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::config::BUILTIN_MODULES;

    struct StubModule {
        manifest: DisplayModuleManifest,
    }

    impl DisplayModule for StubModule {
        fn manifest(&self) -> &DisplayModuleManifest {
            &self.manifest
        }

        fn render(
            &self,
            _context: &DisplayContext,
            _model: &ViewModel,
            _area: Rect,
            _frame: &mut Frame<'_>,
        ) -> RenderOutput {
            RenderOutput::default()
        }
    }

    fn stub(id: &str, api_version: u32) -> Box<dyn DisplayModule> {
        Box::new(StubModule {
            manifest: DisplayModuleManifest {
                module_id: ModuleId::new(id).unwrap(),
                api_version,
                title: "Stub".into(),
                min_columns: 20,
                min_rows: 3,
                priority: 10,
                slots: vec![Slot::Overview],
                enabled_by_default: true,
            },
        })
    }

    #[test]
    fn builtin_registers_every_builtin_module_without_warnings() {
        let registry = ModuleRegistry::builtin();
        let ids: Vec<String> = registry
            .module_ids()
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        assert_eq!(ids, BUILTIN_MODULES.map(str::to_owned));
        assert!(registry.warnings().is_empty(), "{:?}", registry.warnings());
        for module in registry.iter() {
            module.manifest().validate().unwrap();
        }
    }

    #[test]
    fn duplicate_ids_are_rejected_with_a_warning() {
        let mut registry = ModuleRegistry::builtin();
        let error = registry.register(stub("cpu", 1)).unwrap_err();
        assert_eq!(error, RegistryError::DuplicateId("cpu".into()));
        assert_eq!(registry.len(), BUILTIN_MODULES.len());
        assert!(registry.warnings()[0].contains("already registered"));
    }

    #[test]
    fn api_version_mismatch_is_rejected_with_a_warning() {
        let mut registry = ModuleRegistry::builtin();
        let error = registry.register(stub("future", 2)).unwrap_err();
        assert!(matches!(
            error,
            RegistryError::UnsupportedApiVersion { found: 2, .. }
        ));
        assert!(registry.get("future").is_none());
        assert!(registry.warnings()[0].contains("api_version 2"));
    }

    #[test]
    fn a_further_module_can_be_registered() {
        let mut registry = ModuleRegistry::builtin();
        registry.register(stub("fixture", 1)).unwrap();
        assert_eq!(registry.len(), BUILTIN_MODULES.len() + 1);
        assert!(registry.get("fixture").is_some());
    }

    #[test]
    fn registry_is_bounded() {
        let mut registry = ModuleRegistry::empty();
        for index in 0..MAX_MODULES {
            registry.register(stub(&format!("m{index}"), 1)).unwrap();
        }
        assert_eq!(
            registry.register(stub("overflow", 1)),
            Err(RegistryError::Full)
        );
        for index in 0..40 {
            let _ = registry.register(stub(&format!("m{index}"), 1));
        }
        assert!(registry.warnings().len() <= MAX_WARNINGS);
    }
}
