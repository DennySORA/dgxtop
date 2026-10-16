//! The keymap table. One static table drives key dispatch, the
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
    NextHistoryBarSize,
    PreviousHistoryBarSize,
    ToggleModule,
    ToggleCpuDetail,
    ScrollMemoryUp,
    ScrollMemoryDown,
    HistoryDown,
    HistoryUp,
    /// The top of the list, the running runs first (D-108, D-115).
    HistoryFirst,
    /// The oldest run, back on the list.
    HistoryLast,
    /// Marks or unmarks the run under the cursor (D-108).
    HistoryMark,
    /// Marks every ended run, or unmarks all when they all are.
    HistoryMarkAll,
    /// Asks to delete the marked runs, else the run under the cursor.
    HistoryDelete,
    HistoryOpen,
    /// Shows the opened run's whole command line, or one line of it (D-76).
    HistoryExpandCommand,
    /// Copies the opened run's command line to the clipboard (D-76).
    HistoryCopyCommand,
    /// Folds or unfolds a panel (D-114): one of `view::FOLDABLE_PANELS`.
    Fold(&'static str),
    /// Unfolds every panel of the view (D-114).
    UnfoldAll,
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
            Self::Processes => "Processes (Overview)",
            Self::Gpu => "GPU view",
            Self::History => "History views",
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

const LIST_VIEWS: &[ViewKind] = &[ViewKind::Overview, ViewKind::Settings];
/// Views that pick a GPU with `h`/`l`: the GPU view and the Overview
/// History (D-107).
const GPU_VIEWS: &[ViewKind] = &[ViewKind::GpuDetail];
/// The process table, and so its keys, is on the Overview only (D-91).
const PROCESS_VIEWS: &[ViewKind] = &[ViewKind::Overview];
/// The views with a filtered list: the Overview's processes, the Process
/// History's runs (D-113).
const FILTER_VIEWS: &[ViewKind] = &[ViewKind::Overview, ViewKind::History];

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
    binding("Esc", "dismiss notices and the Process History's marks", None, Section::General, ModeSet::NORMAL, ViewScope::All, &[KeyPattern::key(KeyCode::Esc)], KeyAction::DismissNotices),
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
    binding("PgDn/Ctrl-D", "page down", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::key(KeyCode::PageDown), KeyPattern::ctrl('d')], KeyAction::PageDown),
    binding("PgUp/Ctrl-U", "page up", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::key(KeyCode::PageUp), KeyPattern::ctrl('u')], KeyAction::PageUp),
    binding("Home/<", "first row", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::key(KeyCode::Home), KeyPattern::char('<')], KeyAction::First),
    binding("End/>", "last row (reachable in long lists)", None, Section::Navigation, ModeSet::NORMAL, ViewScope::Only(LIST_VIEWS), &[KeyPattern::key(KeyCode::End), KeyPattern::char('>')], KeyAction::Last),
    binding("h/←", "previous GPU", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(GPU_VIEWS), &[KeyPattern::char('h'), KeyPattern::key(KeyCode::Left)], KeyAction::PreviousGpu),
    binding("l/→", "next GPU", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(GPU_VIEWS), &[KeyPattern::char('l'), KeyPattern::key(KeyCode::Right)], KeyAction::NextGpu),
    binding("j/↓", "next page of the GPU's sections, when they do not all fit", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(GPU_VIEWS), &[KeyPattern::char('j'), KeyPattern::key(KeyCode::Down)], KeyAction::MoveDown),
    binding("k/↑", "previous page of the GPU's sections", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(GPU_VIEWS), &[KeyPattern::char('k'), KeyPattern::key(KeyCode::Up)], KeyAction::MoveUp),
    binding("PgDn/Ctrl-D, PgUp/Ctrl-U", "next / previous page of the GPU's sections", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(GPU_VIEWS), &[KeyPattern::key(KeyCode::PageDown), KeyPattern::ctrl('d')], KeyAction::PageDown),
    binding("PgUp/Ctrl-U", "previous page of the GPU's sections", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(GPU_VIEWS), &[KeyPattern::key(KeyCode::PageUp), KeyPattern::ctrl('u')], KeyAction::PageUp),
    binding("g/Home/<, G/End/>", "first / last page of the GPU's sections", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(GPU_VIEWS), &[KeyPattern::char('g'), KeyPattern::key(KeyCode::Home), KeyPattern::char('<')], KeyAction::First),
    binding("G/End/>", "last page of the GPU's sections", None, Section::Gpu, ModeSet::NORMAL, ViewScope::Only(GPU_VIEWS), &[KeyPattern::char('G'), KeyPattern::key(KeyCode::End), KeyPattern::char('>')], KeyAction::Last),
    binding("c", "show / hide the CPU sensors and per-core bars", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview]), &[KeyPattern::char('c')], KeyAction::ToggleCpuDetail),
    binding("i", "fold / unfold the disk I/O panel, so the processes get its rows (kept after a restart)", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview]), &[KeyPattern::char('i')], KeyAction::Fold("disk")),
    binding("n", "fold / unfold the network panel", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview]), &[KeyPattern::char('n')], KeyAction::Fold("network")),
    binding("o", "unfold every panel", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview, ViewKind::History]), &[KeyPattern::char('o')], KeyAction::UnfoldAll),
    binding("[/]", "scroll the GPU processes of the memory panel up / down", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview]), &[KeyPattern::char('[')], KeyAction::ScrollMemoryUp),
    binding("]", "scroll the GPU processes of the memory panel down", None, Section::Views, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Overview]), &[KeyPattern::char(']')], KeyAction::ScrollMemoryDown),
    binding("g", "cycle GPU filter: all → each GPU → all", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::char('g')], KeyAction::CycleGpuFilter),
    binding("/", "filter processes, or the Process History's runs, by name or PID (≤ 256 bytes; Enter applies, Esc clears; Ctrl-B/F/A/E move like ←/→/Home/End)", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(FILTER_VIEWS), &[KeyPattern::char('/')], KeyAction::StartFilter),
    binding("s", "next sort column", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::char('s')], KeyAction::CycleSort),
    binding("r", "reverse sort order", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::char('r')], KeyAction::ToggleOrder),
    binding("K", "terminate the visible selected process (SIGTERM, asks to confirm)", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::char('K')], KeyAction::Terminate),
    binding("F9", "force kill the visible selected process (SIGKILL, asks to confirm)", None, Section::Processes, ModeSet::NORMAL, ViewScope::Only(PROCESS_VIEWS), &[KeyPattern::key(KeyCode::F(9))], KeyAction::Kill),
    binding("j/↓", "next run in the Process History; the next GPU in the Overview History", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('j'), KeyPattern::key(KeyCode::Down)], KeyAction::HistoryDown),
    binding("k/↑", "previous run; the previous GPU", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('k'), KeyPattern::key(KeyCode::Up)], KeyAction::HistoryUp),
    binding("PgDn/Ctrl-D, PgUp/Ctrl-U", "scroll the Overview History's panels a band down / up, when they do not all fit; turn an opened run's pages", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::key(KeyCode::PageDown), KeyPattern::ctrl('d')], KeyAction::PageDown),
    binding("PgUp/Ctrl-U", "scroll the Overview History's panels a band up; the previous page of an opened run", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::key(KeyCode::PageUp), KeyPattern::ctrl('u')], KeyAction::PageUp),
    binding("g/Home/<", "back to the top: the running runs, then the newest (from an opened run too); the first panels", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('g'), KeyPattern::key(KeyCode::Home), KeyPattern::char('<')], KeyAction::HistoryFirst),
    binding("G/End/>", "to the end: the oldest ended run; the last panels", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('G'), KeyPattern::key(KeyCode::End), KeyPattern::char('>')], KeyAction::HistoryLast),
    binding("Space", "mark / unmark the run under the cursor and go to the next", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char(' ')], KeyAction::HistoryMark),
    binding("a", "mark every ended run / unmark all", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('a')], KeyAction::HistoryMarkAll),
    binding("d/Del", "delete the marked runs, else the run under the cursor (asks to confirm; running runs stay)", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('d'), KeyPattern::key(KeyCode::Delete)], KeyAction::HistoryDelete),
    binding("Enter", "open / close the selected run", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::key(KeyCode::Enter)], KeyAction::HistoryOpen),
    binding("c/m/u/v", "fold / unfold the Overview History's CPU, memory, GPU utilization or GPU memory panel, so the others get its rows (kept after a restart)", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('c')], KeyAction::Fold("history.cpu")),
    binding("m", "fold / unfold the Overview History's memory panel", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('m')], KeyAction::Fold("history.memory")),
    binding("u", "fold / unfold the Overview History's GPU utilization panel", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('u')], KeyAction::Fold("history.gpu_utilization")),
    binding("v", "fold / unfold the Overview History's GPU memory panel", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('v')], KeyAction::Fold("history.gpu_memory")),
    binding("t/p/i/n", "fold / unfold the Overview History's GPU temperature, power, disk or network panel", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('t')], KeyAction::Fold("history.gpu_temperature")),
    binding("p", "fold / unfold the Overview History's power panel", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('p')], KeyAction::Fold("history.power")),
    binding("i", "fold / unfold the Overview History's disk panel", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('i')], KeyAction::Fold("history.disk")),
    binding("n", "fold / unfold the Overview History's network panel", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('n')], KeyAction::Fold("history.network")),
    binding("e", "show the whole command line of the opened run / one line", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('e')], KeyAction::HistoryExpandCommand),
    binding("y", "copy the opened run's command line (OSC 52)", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::char('y')], KeyAction::HistoryCopyCommand),
    binding("→/l/w", "next bar size of the history charts (1m … 24h)", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::key(KeyCode::Right), KeyPattern::char('l'), KeyPattern::char('w')], KeyAction::NextHistoryBarSize),
    binding("←/h/W", "previous bar size of the history charts", None, Section::History, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::History]), &[KeyPattern::key(KeyCode::Left), KeyPattern::char('h'), KeyPattern::char('W')], KeyAction::PreviousHistoryBarSize),
    binding("Space", "enable / disable the selected module", None, Section::Settings, ModeSet::NORMAL, ViewScope::Only(&[ViewKind::Settings]), &[KeyPattern::char(' ')], KeyAction::ToggleModule),
    binding("Enter", "apply the filter", Some("apply"), Section::FilterMode, ModeSet::FILTER, ViewScope::All, &[KeyPattern::key(KeyCode::Enter)], KeyAction::FilterApply),
    binding("Esc", "clear the filter and leave input", Some("clear"), Section::FilterMode, ModeSet::FILTER, ViewScope::All, &[KeyPattern::key(KeyCode::Esc)], KeyAction::FilterClear),
    binding("y/Enter", "do what the confirmation asks: send the signal (pressed before it is verified, it goes out once it is), delete the runs", Some("confirm"), Section::ConfirmMode, ModeSet::CONFIRM, ViewScope::All, &[KeyPattern::char('y'), KeyPattern::key(KeyCode::Enter)], KeyAction::Confirm),
    binding("n/Esc", "cancel the confirmation (before it is verified too)", Some("cancel"), Section::ConfirmMode, ModeSet::CONFIRM, ViewScope::All, &[KeyPattern::char('n'), KeyPattern::key(KeyCode::Esc)], KeyAction::Cancel),
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
        assert_eq!(resolve(&q, ModeKind::Filter, ViewKind::Overview), None);
        assert_eq!(
            resolve(&ctrl_c, ModeKind::Filter, ViewKind::Overview),
            Some(KeyAction::Quit)
        );
        assert_eq!(
            resolve(&q, ModeKind::Confirm, ViewKind::Overview),
            Some(KeyAction::Quit)
        );
        assert_eq!(
            resolve(&q, ModeKind::Help, ViewKind::Overview),
            Some(KeyAction::Quit)
        );
    }

    #[test]
    fn terminate_only_binds_in_process_views_in_normal_mode() {
        let shift_k = key(KeyCode::Char('K'), KeyModifiers::SHIFT);
        assert_eq!(
            resolve(&shift_k, ModeKind::Normal, ViewKind::Overview),
            Some(KeyAction::Terminate)
        );
        assert_eq!(
            resolve(&shift_k, ModeKind::Normal, ViewKind::Overview),
            Some(KeyAction::Terminate)
        );
        assert_eq!(
            resolve(&shift_k, ModeKind::Normal, ViewKind::Settings),
            None
        );
        assert_eq!(
            resolve(&shift_k, ModeKind::Filter, ViewKind::Overview),
            None
        );
        assert_eq!(
            resolve(&shift_k, ModeKind::Confirm, ViewKind::Overview),
            None
        );
    }

    #[test]
    fn confirm_keys_only_exist_in_confirm_mode() {
        let y = key(KeyCode::Char('y'), KeyModifiers::NONE);
        let enter = key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(resolve(&y, ModeKind::Normal, ViewKind::Overview), None);
        assert_eq!(resolve(&enter, ModeKind::Normal, ViewKind::Overview), None);
        assert_eq!(
            resolve(&y, ModeKind::Confirm, ViewKind::Overview),
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

    /// The history views' keys (D-107, D-108) bind there alone: `g` still
    /// cycles the Overview's GPU filter, Space still toggles a module in
    /// Settings; in the GPU view ←/→ pick the GPU and ↑/↓ turn pages
    /// (D-110).
    #[test]
    fn history_keys_bind_in_the_history_view_only() {
        let plain = |code| key(code, KeyModifiers::NONE);
        let history = [
            (plain(KeyCode::Char('g')), KeyAction::HistoryFirst),
            (plain(KeyCode::Home), KeyAction::HistoryFirst),
            (
                key(KeyCode::Char('G'), KeyModifiers::SHIFT),
                KeyAction::HistoryLast,
            ),
            (plain(KeyCode::End), KeyAction::HistoryLast),
            (plain(KeyCode::Char(' ')), KeyAction::HistoryMark),
            (plain(KeyCode::Char('a')), KeyAction::HistoryMarkAll),
            (plain(KeyCode::Char('d')), KeyAction::HistoryDelete),
            (plain(KeyCode::Delete), KeyAction::HistoryDelete),
            (plain(KeyCode::Left), KeyAction::PreviousHistoryBarSize),
            (plain(KeyCode::Right), KeyAction::NextHistoryBarSize),
            (plain(KeyCode::Char('h')), KeyAction::PreviousHistoryBarSize),
            (plain(KeyCode::Char('l')), KeyAction::NextHistoryBarSize),
            (plain(KeyCode::Down), KeyAction::HistoryDown),
            (plain(KeyCode::Up), KeyAction::HistoryUp),
            (plain(KeyCode::PageDown), KeyAction::PageDown),
            (plain(KeyCode::PageUp), KeyAction::PageUp),
        ];
        for (event, action) in history {
            assert_eq!(
                resolve(&event, ModeKind::Normal, ViewKind::History),
                Some(action),
                "{event:?}"
            );
            assert_eq!(resolve(&event, ModeKind::Confirm, ViewKind::History), None);
        }
        assert_eq!(
            resolve(
                &plain(KeyCode::Char('g')),
                ModeKind::Normal,
                ViewKind::Overview
            ),
            Some(KeyAction::CycleGpuFilter)
        );
        assert_eq!(
            resolve(
                &plain(KeyCode::Char(' ')),
                ModeKind::Normal,
                ViewKind::Settings
            ),
            Some(KeyAction::ToggleModule)
        );
        assert_eq!(
            resolve(
                &plain(KeyCode::Char('d')),
                ModeKind::Normal,
                ViewKind::Overview
            ),
            None
        );
        assert_eq!(
            resolve(
                &plain(KeyCode::Right),
                ModeKind::Normal,
                ViewKind::GpuDetail
            ),
            Some(KeyAction::NextGpu)
        );
        for (code, action) in [
            (KeyCode::Down, KeyAction::MoveDown),
            (KeyCode::Char('k'), KeyAction::MoveUp),
            (KeyCode::PageDown, KeyAction::PageDown),
            (KeyCode::Home, KeyAction::First),
            (KeyCode::End, KeyAction::Last),
        ] {
            let event = plain(code);
            assert_eq!(
                resolve(&event, ModeKind::Normal, ViewKind::GpuDetail),
                Some(action),
                "{code:?}"
            );
            let binding = KEYMAP
                .iter()
                .find(|binding| {
                    binding.views.contains(ViewKind::GpuDetail)
                        && binding
                            .patterns
                            .iter()
                            .any(|pattern| pattern.matches(&event))
                })
                .unwrap();
            assert!(
                binding.description.contains("page"),
                "{}",
                binding.description
            );
        }
    }

    /// A keyboard without arrows, PgUp/PgDn or Home/End reaches every
    /// action (D-111): each binding of those keys has a letter, a symbol
    /// or a Ctrl key too.
    #[test]
    fn every_navigation_key_has_a_letter_or_ctrl_stand_in() {
        let special = |pattern: &KeyPattern| {
            matches!(
                pattern.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
            )
        };
        for binding in KEYMAP {
            if binding.patterns.iter().any(special) {
                assert!(
                    binding
                        .patterns
                        .iter()
                        .any(|pattern| matches!(pattern.code, KeyCode::Char(_))),
                    "{} has no stand-in",
                    binding.keys
                );
            }
        }
        let ctrl = |character| key(KeyCode::Char(character), KeyModifiers::CONTROL);
        let plain = |code| key(code, KeyModifiers::NONE);
        for view in [ViewKind::Overview, ViewKind::GpuDetail, ViewKind::History] {
            assert_eq!(
                resolve(&ctrl('d'), ModeKind::Normal, view),
                Some(KeyAction::PageDown),
                "{view:?}"
            );
            assert_eq!(
                resolve(&ctrl('u'), ModeKind::Normal, view),
                Some(KeyAction::PageUp),
                "{view:?}"
            );
        }
        for view in [ViewKind::Overview, ViewKind::GpuDetail] {
            assert_eq!(
                resolve(&plain(KeyCode::Char('<')), ModeKind::Normal, view),
                Some(KeyAction::First)
            );
            assert_eq!(
                resolve(&plain(KeyCode::Char('>')), ModeKind::Normal, view),
                Some(KeyAction::Last)
            );
        }
        assert_eq!(
            resolve(
                &plain(KeyCode::Char('>')),
                ModeKind::Normal,
                ViewKind::History
            ),
            Some(KeyAction::HistoryLast)
        );
        assert_eq!(
            resolve(
                &plain(KeyCode::Char('g')),
                ModeKind::Normal,
                ViewKind::GpuDetail
            ),
            Some(KeyAction::First)
        );
        assert_eq!(
            resolve(
                &plain(KeyCode::Char('g')),
                ModeKind::Normal,
                ViewKind::Overview
            ),
            Some(KeyAction::CycleGpuFilter),
            "g stays the Overview's GPU filter"
        );
        assert_eq!(
            resolve(&ctrl('d'), ModeKind::Filter, ViewKind::Overview),
            None,
            "nothing pages while typing"
        );
    }

    /// The fold keys the panels show (D-114) are the ones the keymap
    /// binds, in the view of each panel.
    #[test]
    fn the_fold_keys_shown_are_the_keys_bound() {
        for fold in crate::modules::folds::FOLDS {
            let view = if fold.panel.starts_with("history.") {
                ViewKind::History
            } else {
                ViewKind::Overview
            };
            let event = key(KeyCode::Char(fold.key), KeyModifiers::NONE);
            assert_eq!(
                resolve(&event, ModeKind::Normal, view),
                Some(KeyAction::Fold(fold.panel)),
                "{fold:?}"
            );
        }
    }

    #[test]
    fn release_events_are_ignored() {
        let mut event = key(KeyCode::Char('q'), KeyModifiers::NONE);
        event.kind = KeyEventKind::Release;
        assert_eq!(resolve(&event, ModeKind::Normal, ViewKind::Overview), None);
    }
}
