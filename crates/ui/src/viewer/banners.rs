//! Safety banners: pending confirmation, errors, overload, registry warnings
//! and controller notices. Blocks are ordered by level and word-wrapped, so
//! their height is exact and the layout can give them rows before modules.

use dgxtop_core::view::{BannerLevel, OverloadState, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use super::ui_state::{ModeView, UiState};
use crate::format::{
    COMPACT_CONFIRMATION_WIDTH, CONFIRM_DELETE_HINT, CONFIRM_HINT, clean, confirmation_lines,
    counted, display_width, format_countdown_ms, format_interval_ms,
};
use crate::num::saturate_u16;
use crate::theme::ThemePalette;

/// Upper bound of banner blocks per frame.
const MAX_BLOCKS: usize = 12;

/// One banner: a level and its text lines (before wrapping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerBlock {
    pub level: BannerLevel,
    pub lines: Vec<String>,
}

impl BannerBlock {
    fn single(level: BannerLevel, text: String) -> Self {
        Self {
            level,
            lines: vec![text],
        }
    }

    /// Wrapped lines for a given width (one leading space of padding).
    pub fn wrapped(&self, width: u16) -> Vec<String> {
        let room = usize::from(width.saturating_sub(1)).max(1);
        self.lines
            .iter()
            .flat_map(|line| wrap_words(line, room))
            .collect()
    }

    pub fn height(&self, width: u16) -> u16 {
        saturate_u16(self.wrapped(width).len())
    }
}

/// Greedy word wrap with hard breaks for words longer than the width.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = display_width(word);
        let separator = usize::from(!current.is_empty());
        if current_width + separator + word_width <= width {
            if separator == 1 {
                current.push(' ');
            }
            current.push_str(word);
            current_width += separator + word_width;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        for character in word.chars() {
            let character_width = character.width().unwrap_or(0);
            if current_width + character_width > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(character);
            current_width += character_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// All banners of a frame, highest priority first.
pub fn collect(
    model: &ViewModel,
    ui: &UiState,
    registry_warnings: &[String],
    projection_age_ms: u64,
    width: u16,
) -> Vec<BannerBlock> {
    let mut blocks = Vec::new();
    if let Some(confirmation) = confirmation_block(model, ui, projection_age_ms, width) {
        blocks.push(confirmation);
    }
    for banner in &model.banners {
        if banner.level != BannerLevel::Confirmation {
            blocks.push(BannerBlock::single(banner.level, clean(&banner.text)));
        }
    }
    if let Some(overload) = overload_block(model) {
        blocks.push(overload);
    }
    for warning in registry_warnings {
        blocks.push(BannerBlock::single(
            BannerLevel::Warning,
            format!("display module rejected: {}", clean(warning)),
        ));
    }
    for notice in &ui.notices {
        blocks.push(BannerBlock::single(notice.level, clean(&notice.text)));
    }
    // Stable sort keeps arrival order within a level.
    blocks.sort_by(|a, b| b.level.cmp(&a.level));
    blocks.truncate(MAX_BLOCKS);
    blocks
}

fn confirmation_block(
    model: &ViewModel,
    ui: &UiState,
    projection_age_ms: u64,
    width: u16,
) -> Option<BannerBlock> {
    // The controller asks to delete only while no signal waits for its own
    // confirmation, so y/Enter answers this question (D-108).
    if let ModeView::ConfirmDelete {
        runs,
        running,
        every,
    } = ui.mode
    {
        return Some(deletion_block(runs, running, every, width));
    }
    let projected = model
        .modules
        .actions
        .as_deref()
        .and_then(|actions| actions.confirmation.as_ref());
    // The structured ConfirmationView carries everything the Manager's
    // confirmation banner text says; that text is only shown without it.
    let manager_texts: Vec<String> = if projected.is_some() {
        Vec::new()
    } else {
        model
            .banners
            .iter()
            .filter(|banner| banner.level == BannerLevel::Confirmation)
            .map(|banner| clean(&banner.text))
            .collect()
    };
    let compact = width < COMPACT_CONFIRMATION_WIDTH;
    let mut details = match (projected, &ui.mode) {
        (Some(confirmation), _) => confirmation_lines(confirmation, projection_age_ms, compact),
        (
            None,
            ModeView::Confirm {
                target,
                signal,
                remaining_ms,
            },
        ) => {
            let countdown = remaining_ms.map_or_else(
                || "expiry unknown".to_owned(),
                |remaining| format!("expires in {}", format_countdown_ms(remaining)),
            );
            vec![
                format!("Send {} to {}?", signal.name(), target.summary()),
                format!("{countdown} · this signal cannot be undone"),
            ]
        }
        (None, _) if !manager_texts.is_empty() || ui.awaiting_confirmation => Vec::new(),
        (None, _) => return None,
    };
    details.extend(manager_texts);
    let hint = if ui.is_confirming() {
        CONFIRM_HINT
    } else {
        awaiting_hint(ui.early_answer)
    };
    // Question, then the keys, then the remaining details: when rows run out
    // the least important lines are the ones left out.
    let mut details = details.into_iter();
    let separator = if compact { " " } else { " · " };
    let question = details.next().map_or_else(
        || "CONFIRM".to_owned(),
        |first| format!("CONFIRM{separator}{first}"),
    );
    let mut lines = vec![question, hint.to_owned()];
    lines.extend(details);
    Some(BannerBlock {
        level: BannerLevel::Confirmation,
        lines,
    })
}

/// The keys while the token is on its way: they already answer it, and
/// once answered, what happens when it arrives (D-109).
const fn awaiting_hint(early_answer: Option<bool>) -> &'static str {
    match early_answer {
        None => {
            "waiting for the confirmation token · y / Enter sends once it arrives · n / Esc cancel"
        }
        Some(true) => "sending as soon as the confirmation token arrives · n / Esc cancel",
        Some(false) => "cancelling: nothing will be sent",
    }
}

/// The Process History's deletion prompt: the question, the keys, then
/// what stays (D-108).
fn deletion_block(runs: usize, running: usize, every: bool, width: u16) -> BannerBlock {
    let separator = if width < COMPACT_CONFIRMATION_WIDTH {
        " "
    } else {
        " · "
    };
    // Every one the filter keeps, listed or not (D-128).
    let all = if every { "all " } else { "" };
    let mut lines = vec![
        format!(
            "CONFIRM{separator}Delete {all}{} from the Process History?",
            counted(runs, "ended run")
        ),
        CONFIRM_DELETE_HINT.to_owned(),
        "their samples go with them · this cannot be undone".to_owned(),
    ];
    if running > 0 {
        lines.push(format!(
            "{}{} stay{}: a running run is kept until it ends",
            counted(running, "running run"),
            if every { "" } else { " marked" },
            if running == 1 { "s" } else { "" }
        ));
    }
    BannerBlock {
        level: BannerLevel::Confirmation,
        lines,
    }
}

fn overload_block(model: &ViewModel) -> Option<BannerBlock> {
    let header = &model.header;
    if header.overload == OverloadState::Normal {
        return None;
    }
    let mentioned = model
        .banners
        .iter()
        .any(|banner| banner.text.to_ascii_lowercase().contains("overload"));
    if mentioned {
        return None;
    }
    Some(BannerBlock::single(
        BannerLevel::Warning,
        format!(
            "overload {}: effective interval {} (configured {})",
            header.overload.label(),
            format_interval_ms(header.effective_interval_ms),
            format_interval_ms(header.configured_interval_ms)
        ),
    ))
}

/// Draws a block into its allocated rect (never truncating whole lines away
/// unless the layout itself had no room).
pub fn render_block(
    frame: &mut Frame<'_>,
    area: Rect,
    block: &BannerBlock,
    palette: &ThemePalette,
) {
    if area.is_empty() {
        return;
    }
    let style = palette.banner_style(block.level);
    frame.buffer_mut().set_style(area, style);
    for (offset, line) in block.wrapped(area.width).iter().enumerate() {
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        if offset >= area.height {
            break;
        }
        frame.buffer_mut().set_stringn(
            area.x + 1.min(area.width),
            area.y + offset,
            line,
            usize::from(area.width.saturating_sub(1)),
            style,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;
    use crate::viewer::ui_state::Notice;
    use dgxtop_core::commands::ViewKind;
    use dgxtop_core::view::Banner;
    use std::sync::Arc;

    #[test]
    fn confirmation_comes_first_and_errors_before_warnings() {
        let mut model = test_fixtures::view_model(ViewKind::Overview, 1);
        model.modules.actions = Some(Arc::new(test_fixtures::actions_view(true)));
        model.banners = vec![
            Banner {
                level: BannerLevel::Info,
                text: "info".into(),
            },
            Banner {
                level: BannerLevel::Error,
                text: "audit failed".into(),
            },
        ];
        model.header.overload = OverloadState::Throttled;
        let ui = UiState {
            notices: vec![Notice {
                level: BannerLevel::Warning,
                text: "command queue busy".into(),
            }],
            ..UiState::default()
        };
        let blocks = collect(&model, &ui, &["bad module".into()], 0, 120);
        let levels: Vec<BannerLevel> = blocks.iter().map(|block| block.level).collect();
        assert_eq!(
            levels,
            vec![
                BannerLevel::Confirmation,
                BannerLevel::Error,
                BannerLevel::Warning,
                BannerLevel::Warning,
                BannerLevel::Warning,
                BannerLevel::Info
            ]
        );
        assert!(blocks[0].lines[0].starts_with("CONFIRM · Send SIGTERM to pid 4242"));
        assert!(blocks[0].lines[1].contains("waiting for the confirmation token"));
        assert!(
            blocks
                .iter()
                .any(|block| block.lines[0].contains("overload throttled"))
        );
    }

    #[test]
    fn confirm_mode_without_projection_still_shows_a_banner() {
        let mut model = test_fixtures::view_model(ViewKind::Overview, 1);
        model.modules.actions = None;
        let ui = UiState {
            mode: ModeView::Confirm {
                target: test_fixtures::process_key(4242, 90_100),
                signal: dgxtop_core::action::Signal::Sigterm,
                remaining_ms: Some(5_000),
            },
            ..UiState::default()
        };
        let blocks = collect(&model, &ui, &[], 0, 120);
        assert_eq!(blocks[0].level, BannerLevel::Confirmation);
        let text = blocks[0].lines.join(" ");
        assert!(text.contains("pid 4242"));
        assert!(text.contains("y / Enter send"));
        assert!(!text.contains(test_fixtures::token().as_str()));
    }

    /// While the token is on its way the keys already answer it, and the
    /// banner says what happens on its arrival (D-109).
    #[test]
    fn a_signal_on_its_way_says_its_keys_and_its_answer() {
        let model = test_fixtures::view_model(ViewKind::Overview, 1);
        let hint = |early_answer| {
            let ui = UiState {
                awaiting_confirmation: true,
                early_answer,
                ..UiState::default()
            };
            collect(&model, &ui, &[], 0, 160)[0].lines[1].clone()
        };
        assert!(hint(None).contains("y / Enter sends once it arrives"));
        assert!(hint(Some(true)).starts_with("sending as soon as"));
        assert_eq!(hint(Some(false)), "cancelling: nothing will be sent");
    }

    /// The Process History's deletion prompt (D-108): the question, its
    /// own keys, that it cannot be undone and the marked running runs that
    /// stay; a projected signal confirmation does not take its place.
    #[test]
    fn the_deletion_prompt_says_what_goes_and_what_stays() {
        let mut model = test_fixtures::view_model(ViewKind::History, 1);
        model.modules.actions = Some(Arc::new(test_fixtures::actions_view(true)));
        let ask_any = |runs, running, every, width| {
            let ui = UiState {
                mode: ModeView::ConfirmDelete {
                    runs,
                    running,
                    every,
                },
                ..UiState::default()
            };
            let blocks = collect(&model, &ui, &[], 0, width);
            assert_eq!(blocks[0].level, BannerLevel::Confirmation);
            blocks[0].lines.clone()
        };
        let ask = |runs, running, width| ask_any(runs, running, false, width);
        assert_eq!(
            ask(3, 1, 120),
            [
                "CONFIRM · Delete 3 ended runs from the Process History?",
                "y / Enter delete · n / Esc cancel",
                "their samples go with them · this cannot be undone",
                "1 running run marked stays: a running run is kept until it ends",
            ]
        );
        let one = ask(1, 0, 40);
        assert_eq!(
            one[0],
            "CONFIRM Delete 1 ended run from the Process History?"
        );
        assert_eq!(one.len(), 3, "no running run to mention: {one:?}");
        assert!(ask(2, 2, 120)[3].starts_with("2 running runs marked stay:"));
        // Every run the filter keeps, listed or not (D-128).
        let every = ask_any(523, 2, true, 120);
        assert_eq!(
            every[0],
            "CONFIRM · Delete all 523 ended runs from the Process History?"
        );
        assert!(every[3].starts_with("2 running runs stay:"), "{every:?}");
    }

    #[test]
    fn manager_confirmation_text_is_a_fallback_only() {
        let mut model = test_fixtures::view_model(ViewKind::Overview, 1);
        model.banners = vec![Banner {
            level: BannerLevel::Confirmation,
            text: "Confirm SIGTERM for pid 4242".into(),
        }];
        model.modules.actions = Some(Arc::new(test_fixtures::actions_view(true)));
        let with_view = collect(&model, &UiState::default(), &[], 0, 120);
        assert_eq!(with_view.len(), 1);
        assert!(
            !with_view[0]
                .lines
                .iter()
                .any(|line| line.contains("Confirm SIGTERM for"))
        );
        model.modules.actions = None;
        let without_view = collect(&model, &UiState::default(), &[], 0, 120);
        assert!(
            without_view[0]
                .lines
                .iter()
                .any(|line| line.contains("Confirm SIGTERM for pid 4242"))
        );
    }
}
