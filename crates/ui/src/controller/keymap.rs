//! The keymap table (`UI.md` §3). One static table drives key dispatch, the
//! footer hints and the help overlay, so they can never disagree.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dgxtop_core::commands::ViewKind;

use crate::views::Tab;

/// Intent of a key press before context routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    Quit,
    ShowTab(Tab),
    NextView,
    PreviousView,
    MoveDown,
    MoveUp,
    PageDown,
    PageUp,
    First,
    Last,
    NextGpu,
    PreviousGpu,
    CycleGpuFilter,
    StartFilter,
    CycleSort,
    ToggleOrder,
    FasterInterval,
    SlowerInterval,
    Terminate,
    Kill,
    Confirm,
    Cancel,
    ToggleHelp,
    CloseHelp,
    NextHistoryWindow,
    PreviousHistoryWindow,
    ToggleModule,
    ToggleCpuDetail,
    ScrollMemoryUp,
    ScrollMemoryDown,
    HistoryDown,
    HistoryUp,
    HistoryOpen,
    /// Shows the opened run's whole command line, or one line of it (D-76).
    HistoryExpandCommand,
    /// Copies the opened run's command line to the clipboard (D-76).
    HistoryCopyCommand,
    DismissNotices,
    FilterApply,
    FilterClear,
}

/// Input mode a binding applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModeKind {
    Normal,
    Filter,
    Confirm,
    Help,
}

/// Set of modes (bit mask).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSet(u8);

impl ModeSet {
    pub const NORMAL: Self = Self(1);
    pub const FILTER: Self = Self(2);
    pub const CONFIRM: Self = Self(4);
    pub const HELP: Self = Self(8);
    pub const ALL: Self = Self(15);
    /// Every mode except text entry (where printable keys are text).
    pub const NOT_FILTER: Self = Self(13);

    pub const fn contains(self, mode: ModeKind) -> bool {
        let bit = match mode {
            ModeKind::Normal => 1,
            ModeKind::Filter => 2,
            ModeKind::Confirm => 4,
            ModeKind::Help => 8,
        };
        self.0 & bit != 0
    }
}

/// Views where a Normal-mode binding is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewScope {
    All,
    Only(&'static [ViewKind]),
}

impl ViewScope {
    pub fn contains(self, view: ViewKind) -> bool {
        match self {
            Self::All => true,
            Self::Only(views) => views.contains(&view),
        }
    }
}

/// Help overlay grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    General,
    Views,
    Navigation,
    Processes,
    Gpu,
    History,
    Settings,
    FilterMode,
    ConfirmMode,
    HelpMode,
}

impl Section {
    pub const ORDER: [Section; 10] = [
        Section::General,
        Section::Views,
        Section::Navigation,
        Section::Processes,
        Section::Gpu,
        Section::History,
        Section::Settings,
        Section::FilterMode,
        Section::ConfirmMode,
        Section::HelpMode,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Views => "Views",
            Self::Navigation => "Navigation (selection is by identity, never row number)",
            Self::Processes => "Processes (Overview and Processes views)",
            Self::Gpu => "GPU view",
            Self::History => "History view",
            Self::Settings => "Settings view",
            Self::FilterMode => "Filter input",
            Self::ConfirmMode => "Confirmation (only with an active token)",
            Self::HelpMode => "Help",
        }
    }
}

/// One physical key (modifiers other than Ctrl are ignored for characters,
/// which already carry their shifted form).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPattern {
    pub code: KeyCode,
    pub control: bool,
}

impl KeyPattern {
    pub const fn key(code: KeyCode) -> Self {
        Self {
            code,
            control: false,
        }
    }

    pub const fn char(character: char) -> Self {
        Self::key(KeyCode::Char(character))
    }

    pub const fn ctrl(character: char) -> Self {
        Self {
            code: KeyCode::Char(character),
            control: true,
        }
    }

    pub fn matches(&self, event: &KeyEvent) -> bool {
        let control = event.modifiers.contains(KeyModifiers::CONTROL);
        let shift = event.modifiers.contains(KeyModifiers::SHIFT);
        if event.modifiers.contains(KeyModifiers::ALT) || control != self.control {
            return false;
        }
        match (self.code, event.code) {
            (KeyCode::BackTab, KeyCode::Tab) => shift,
            (KeyCode::Tab, KeyCode::Tab) => !shift,
            (KeyCode::Char(expected), KeyCode::Char(actual)) if self.control => {
                expected.eq_ignore_ascii_case(&actual)
            }
            (expected, actual) => expected == actual,
        }
    }
}

/// One keymap row.
#[derive(Debug, Clone, Copy)]
pub struct KeyBinding {
    pub keys: &'static str,
    pub description: &'static str,
    /// Short footer label; `None` keeps the binding out of the footer.
    pub hint: Option<&'static str>,
    pub section: Section,
    pub modes: ModeSet,
    pub views: ViewScope,
    pub patterns: &'static [KeyPattern],
    pub action: KeyAction,
}

const LIST_VIEWS: &[ViewKind] = &[
    ViewKind::Overview,
    ViewKind::GpuDetail,
    ViewKind::Processes,
    ViewKind::Sources,
    ViewKind::Settings,
];
const PROCESS_VIEWS: &[ViewKind] = &[ViewKind::Overview, ViewKind::Processes];
const FILTER_VIEWS: &[ViewKind] = &[ViewKind::Overview, ViewKind::Processes, ViewKind::Sources];

/// Table-row constructor: positional arguments keep the table one line per binding.
#[allow(clippy::too_many_arguments)]
const fn binding(
    keys: &'static str,
    description: &'static str,
    hint: Option<&'static str>,
    section: Section,
    modes: ModeSet,
    views: ViewScope,
    patterns: &'static [KeyPattern],
    action: KeyAction,
) -> KeyBinding {
    KeyBinding {
        keys,
        description,
        hint,
        section,
        modes,
        views,
        patterns,
        action,
    }
}

/// The complete keymap, in precedence and footer order.
#[rustfmt::skip]
pub static KEYMAP: &[KeyBinding] = &[
    binding("q", "quit (in filter input q is text)", Some("quit"), Section::General, ModeSet::NOT_FILTER, ViewScope::All, &[KeyPattern::char('q')], KeyAction::Quit),
    binding("Ctrl-C", "quit from any mode, including filter input", None, Section::General, ModeSet::ALL, ViewScope::All, &[KeyPattern::ctrl('c')], KeyAction::Quit),
    binding("?/F1", "toggle this help", Some("help"), Section::General, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::char('?'), KeyPattern::key(KeyCode::F(1))], KeyAction::ToggleHelp),
    binding("Esc", "dismiss notices", None, Section::General, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::key(KeyCode::Esc)], KeyAction::DismissNotices),
    binding("+/-", "faster / slower sampling (SetSamplingInterval with the settings revision)", Some("interval"), Section::General, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::char('+'), KeyPattern::char('=')], KeyAction::FasterInterval),
    binding("-", "slower sampling", None, Section::General, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::char('-')], KeyAction::SlowerInterval),
    binding("Tab", "next view", Some("view"), Section::Views, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::key(KeyCode::Tab)], KeyAction::NextView),
    binding("Shift-Tab", "previous view", None, Section::Views, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::key(KeyCode::BackTab)], KeyAction::PreviousView),
    binding("1", "Overview (GPUs, system, GPU processes)", None, Section::Views, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::char('1')], KeyAction::ShowTab(Tab::Overview)),
    binding("2", "GPU detail", None, Section::Views, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::char('2')], KeyAction::ShowTab(Tab::Gpu)),
    binding("3", "Overview History", None, Section::Views, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::char('3')], KeyAction::ShowTab(Tab::OverviewHistory)),
    binding("4", "Process History", None, Section::Views, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::char('4')], KeyAction::ShowTab(Tab::ProcessHistory)),
    binding("5/F2", "Settings", None, Section::Views, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::char('5'), KeyPattern::key(KeyCode::F(2))], KeyAction::ShowTab(Tab::Settings)),
    binding("j/↓", "select next", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::char('j'), KeyPattern::key(KeyCode::Down)], KeyAction::MoveDown),
    binding("k/↑", "select previous", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::char('k'), KeyPattern::key(KeyCode::Up)], KeyAction::MoveUp),
    binding("PgDn", "page down", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::key(KeyCode::PageDown)], KeyAction::PageDown),
    binding("PgUp", "page up", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::key(KeyCode::PageUp)], KeyAction::PageUp),
    binding("Home", "first row", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::key(KeyCode::Home)], KeyAction::First),
    binding("End", "last row (reachable in long lists)", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::key(KeyCode::End)], KeyAction::Last),
    binding("h/←", "previous GPU", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::GpuDetail]), &[KeyPattern::char('h'), KeyPattern::key(KeyCode::Left)], KeyAction::PreviousGpu),
    binding("l/→", "next GPU", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::GpuDetail]), &[KeyPattern::char('l'), KeyPattern::key(KeyCode::Right)], KeyAction::NextGpu),
    binding("c", "show / hide the CPU sensors and per-core bars", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview]), &[KeyPattern::char('c')], KeyAction::ToggleCpuDetail),
    binding("[/]", "scroll the GPU processes of the memory panel up / down", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview]), &[KeyPattern::char('[')], KeyAction::ScrollMemoryUp),
    binding("]", "scroll the GPU processes of the memory panel down", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview]), &[KeyPattern::char(']')], KeyAction::ScrollMemoryDown),
    binding("g", "cycle GPU filter: all → each GPU → all", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::char('g')], KeyAction::CycleGpuFilter),
    binding("/", "filter by name (≤ 256 bytes; Enter applies, Esc clears)", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(FILTER_VIEWS), &[KeyPattern::char('/')], KeyAction::StartFilter),
    binding("s", "next sort column", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::char('s')], KeyAction::CycleSort),
    binding("r", "reverse sort order", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::char('r')], KeyAction::ToggleOrder),
    binding("K", "terminate the visible selected process (SIGTERM, asks to confirm)", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::char('K')], KeyAction::Terminate),
    binding("F9", "force kill the visible selected process (SIGKILL, asks to confirm)", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::key(KeyCode::F(9))], KeyAction::Kill),
    binding("j/↓", "next run in the process history", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('j'), KeyPattern::key(KeyCode::Down)], KeyAction::HistoryDown),
    binding("k/↑", "previous run in the process history", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('k'), KeyPattern::key(KeyCode::Up)], KeyAction::HistoryUp),
    binding("Enter", "open / close the selected run", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::key(KeyCode::Enter)], KeyAction::HistoryOpen),
    binding("e", "show the whole command line of the opened run / one line", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('e')], KeyAction::HistoryExpandCommand),
    binding("y", "copy the opened run's command line (OSC 52)", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('y')], KeyAction::HistoryCopyCommand),
    binding("w/W/→/←", "next / previous bar size of the history charts (1m … 24h)", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('w'), KeyPattern::key(KeyCode::Right)], KeyAction::NextHistoryWindow),
    binding("W", "previous history window", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('W'), KeyPattern::key(KeyCode::Left)], KeyAction::PreviousHistoryWindow),
    binding("Space", "enable / disable the selected module", None, Section::Settings, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Settings]), &[KeyPattern::char(' ')], KeyAction::ToggleModule),
    binding("Enter", "apply the filter", Some("apply"), Section::FilterMode, ModeSet::FILTER, ViewScope::All, &[KeyPattern::key(KeyCode::Enter)], KeyAction::FilterApply),
    binding("Esc", "clear the filter and leave input", Some("clear"), Section::FilterMode, ModeSet::FILTER, ViewScope::All, &[KeyPattern::key(KeyCode::Esc)], KeyAction::FilterClear),
    binding("y/Enter", "send the signal shown in the confirmation", Some("send"), Section::ConfirmMode, ModeSet::CONFIRM, ViewScope::All, &[KeyPattern::char('y'), KeyPattern::key(KeyCode::Enter)], KeyAction::Confirm),
    binding("n/Esc", "cancel the confirmation", Some("cancel"), Section::ConfirmMode, ModeSet::CONFIRM, ViewScope::All, &[KeyPattern::char('n'), KeyPattern::key(KeyCode::Esc)], KeyAction::Cancel),
    binding("Esc/?/F1", "close help", Some("close"), Section::HelpMode, ModeSet::HELP, ViewScope::All, &[KeyPattern::key(KeyCode::Esc), KeyPattern::char('?'), KeyPattern::key(KeyCode::F(1))], KeyAction::CloseHelp),
    binding("j/k", "scroll help", None, Section::HelpMode, ModeSet::HELP, ViewScope::All, &[KeyPattern::char('j'), KeyPattern::key(KeyCode::Down)], KeyAction::MoveDown),
    binding("k", "scroll help up", None, Section::HelpMode, ModeSet::HELP, ViewScope::All, &[KeyPattern::char('k'), KeyPattern::key(KeyCode::Up)], KeyAction::MoveUp),
];

/// Resolves a key event in the given mode and view (`None` = unbound).
pub fn resolve(event: &KeyEvent, mode: ModeKind, view: ViewKind) -> Option<KeyAction> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    KEYMAP
        .iter()
        .filter(|binding| binding.modes.contains(mode))
        .filter(|binding| mode != ModeKind::Normal || binding.views.contains(view))
        .find(|binding| {
            binding
                .patterns
                .iter()
                .any(|pattern| pattern.matches(event))
        })
        .map(|binding| binding.action)
}

/// Footer hints for the current mode and view, in table order.
pub fn hints(mode: ModeKind, view: ViewKind) -> Vec<(&'static str, &'static str)> {
    let mut hints: Vec<(&'static str, &'static str)> = KEYMAP
        .iter()
        .filter(|binding| binding.modes.contains(mode))
        .filter(|binding| mode != ModeKind::Normal || binding.views.contains(view))
        .filter_map(|binding| binding.hint.map(|hint| (binding.keys, hint)))
        .collect();
    if mode == ModeKind::Filter {
        hints.push(("Ctrl-C", "quit"));
    }
    hints
}

/// Bindings of one help section, in table order.
pub fn section_bindings(section: Section) -> impl Iterator<Item = &'static KeyBinding> {
    KEYMAP
        .iter()
        .filter(move |binding| binding.section == section)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn q_quits_except_in_filter_mode_where_ctrl_c_still_quits() {
        let q = key(KeyCode::Char('q'), KeyModifiers::NONE);
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            resolve(&q, ModeKind::Normal, ViewKind::Overview),
            Some(KeyAction::Quit)
        );
        assert_eq!(resolve(&q, ModeKind::Filter, ViewKind::Processes), None);
        assert_eq!(
            resolve(&ctrl_c, ModeKind::Filter, ViewKind::Processes),
            Some(KeyAction::Quit)
        );
        assert_eq!(
            resolve(&q, ModeKind::Confirm, ViewKind::Processes),
            Some(KeyAction::Quit)
        );
        assert_eq!(
            resolve(&q, ModeKind::Help, ViewKind::Processes),
            Some(KeyAction::Quit)
        );
    }

    #[test]
    fn terminate_only_binds_in_process_views_in_normal_mode() {
        let shift_k = key(KeyCode::Char('K'), KeyModifiers::SHIFT);
        assert_eq!(
            resolve(&shift_k, ModeKind::Normal, ViewKind::Processes),
            Some(KeyAction::Terminate)
        );
        assert_eq!(
            resolve(&shift_k, ModeKind::Normal, ViewKind::Overview),
            Some(KeyAction::Terminate)
        );
        assert_eq!(resolve(&shift_k, ModeKind::Normal, ViewKind::Sources), None);
        assert_eq!(
            resolve(&shift_k, ModeKind::Filter, ViewKind::Processes),
            None
        );
        assert_eq!(
            resolve(&shift_k, ModeKind::Confirm, ViewKind::Processes),
            None
        );
    }

    #[test]
    fn confirm_keys_only_exist_in_confirm_mode() {
        let y = key(KeyCode::Char('y'), KeyModifiers::NONE);
        let enter = key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(resolve(&y, ModeKind::Normal, ViewKind::Processes), None);
        assert_eq!(resolve(&enter, ModeKind::Normal, ViewKind::Processes), None);
        assert_eq!(
            resolve(&y, ModeKind::Confirm, ViewKind::Processes),
            Some(KeyAction::Confirm)
        );
        assert_eq!(
            resolve(&enter, ModeKind::Confirm, ViewKind::Overview),
            Some(KeyAction::Confirm)
        );
        let esc = key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            resolve(&esc, ModeKind::Confirm, ViewKind::Overview),
            Some(KeyAction::Cancel)
        );
        assert_eq!(
            resolve(&esc, ModeKind::Filter, ViewKind::Overview),
            Some(KeyAction::FilterClear)
        );
        assert_eq!(
            resolve(&enter, ModeKind::Filter, ViewKind::Overview),
            Some(KeyAction::FilterApply)
        );
    }

    #[test]
    fn release_events_are_ignored() {
        let mut event = key(KeyCode::Char('q'), KeyModifiers::NONE);
        event.kind = KeyEventKind::Release;
        assert_eq!(resolve(&event, ModeKind::Normal, ViewKind::Overview), None);
    }
}
