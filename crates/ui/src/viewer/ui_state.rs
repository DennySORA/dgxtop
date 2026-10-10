//! Redacted controller state handed to the Viewer for one frame. It carries
//! what must be drawn (mode, filter buffer, notices, confirmation target)
//! and, by construction, never the confirmation token.

use std::collections::BTreeSet;

use dgxtop_core::action::Signal;
use dgxtop_core::identity::{ModuleId, ProcessKey};
use dgxtop_core::view::BannerLevel;

/// Transient controller notice (busy queue, rejected command, results).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: BannerLevel,
    pub text: String,
}

/// Which filter the text input edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterTarget {
    Processes,
    Sources,
}

impl FilterTarget {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Processes => "process name",
            Self::Sources => "source text",
        }
    }
}

/// Input mode as the Viewer needs to show it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModeView {
    #[default]
    Normal,
    Filter {
        buffer: String,
        /// Byte offset of the cursor inside `buffer` (always a char boundary).
        cursor: usize,
        target: FilterTarget,
    },
    /// An active token exists; y/Enter would confirm, n/Esc cancels.
    Confirm {
        target: ProcessKey,
        signal: Signal,
        remaining_ms: Option<u64>,
    },
    Help {
        scroll: u16,
    },
}

/// Everything controller-side that a frame shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UiState {
    pub mode: ModeView,
    pub notices: Vec<Notice>,
    /// A PrepareSignal was accepted and its token has not arrived yet.
    pub awaiting_confirmation: bool,
    /// Collapsible modules the user expanded (presentation only; nothing is
    /// sent to the Manager).
    pub expanded: BTreeSet<ModuleId>,
}

impl UiState {
    pub fn is_confirming(&self) -> bool {
        matches!(self.mode, ModeView::Confirm { .. })
    }
}
