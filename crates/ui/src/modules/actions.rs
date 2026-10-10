//! Actions module: pending confirmation details (never the token), recent
//! actions with honest state words ("signal sent" is not "terminated"),
//! audit persistence status and the unknown-outcome warning.

use dgxtop_core::action::ActionState;
use dgxtop_core::view::{ActionRecordView, ActionsView, ConfirmationView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_line,
    draw_table_header, draw_table_row, fit_columns, message, no_data, panel,
};
use super::settings::persistence_text;
use crate::format::{clean, format_age, format_countdown_ms};
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot, Topic,
};

const RECENT_COLUMNS: [ColumnSpec; 5] = [
    ColumnSpec::right("AGE", 11, 70),
    ColumnSpec::left("TARGET", 14, 100).flexible().prefer(28),
    ColumnSpec::left("SIGNAL", 7, 90),
    ColumnSpec::left("STATE", 14, 99).prefer(31),
    ColumnSpec::left("RESULT", 10, 60).flexible(),
];

/// Keys of an active confirmation, shown right under the question.
pub(crate) const CONFIRM_HINT: &str = "y / Enter send · n / Esc cancel";

/// Human state words. `SignalSent` deliberately says nothing about exit.
pub const fn action_state_label(state: ActionState) -> &'static str {
    match state {
        ActionState::Requested => "requested",
        ActionState::Prepared => "awaiting confirmation",
        ActionState::Confirming => "confirming",
        ActionState::IntentPending => "recording intent",
        ActionState::IntentCommitted => "intent recorded",
        ActionState::Sending => "sending",
        ActionState::SignalSent => "signal sent (exit not observed)",
        ActionState::ExitObserved => "exit observed",
        ActionState::StillRunning => "still running",
        ActionState::Cancelled => "cancelled",
        ActionState::Expired => "expired",
        ActionState::Rejected => "rejected",
        ActionState::NotSent => "not sent",
        ActionState::Unknown => "unknown outcome",
    }
}

/// Width below which the confirmation uses its compact wording.
pub(crate) const COMPACT_CONFIRMATION_WIDTH: u16 = 60;

/// Confirmation details shared by this module and the never-clipped banner,
/// most important first: question, countdown/irreversibility, identity,
/// warning. The confirmation token is not part of the view and never shown.
pub(crate) fn confirmation_lines(
    confirmation: &ConfirmationView,
    age_offset_ms: u64,
    compact: bool,
) -> Vec<String> {
    let remaining = confirmation.expires_in_ms.saturating_sub(age_offset_ms);
    let countdown = if remaining == 0 {
        "expired".to_owned()
    } else {
        format!("expires in {}", format_countdown_ms(remaining))
    };
    let target = &confirmation.target;
    let mut lines = if compact {
        vec![
            format!(
                "{} pid {} ({})?",
                confirmation.signal.name(),
                target.pid,
                clean(&confirmation.basename)
            ),
            format!("{countdown} · cannot be undone"),
            format!(
                "uid {} · start {} · ns {}",
                confirmation.uid, target.start_time_ticks, target.pid_namespace
            ),
        ]
    } else {
        vec![
            format!(
                "Send {} to pid {} ({}) owned by uid {}?",
                confirmation.signal.name(),
                target.pid,
                clean(&confirmation.basename),
                confirmation.uid
            ),
            format!("{countdown} · this signal cannot be undone"),
            format!("start identity: {}", target.summary()),
        ]
    };
    if !confirmation.warning.is_empty() {
        lines.push(clean(&confirmation.warning));
    }
    lines
}

pub struct ActionsModule {
    manifest: DisplayModuleManifest,
}

impl ActionsModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "actions",
                title: "Process actions and audit",
                topics: &[Topic::Actions, Topic::Processes],
                min_columns: 40,
                min_rows: 4,
                priority: 50,
                slots: &[Slot::Detail, Slot::Overlay],
            })?,
        })
    }
}

impl DisplayModule for ActionsModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let inner = panel(frame, area, "Actions", context);
        let Some(actions) = model.modules.actions.as_deref() else {
            message(
                frame,
                inner,
                &no_data("actions"),
                context.theme.muted_style(),
            );
            output.label("actions: no data");
            return output.finish();
        };
        let mut rows = RowCursor::new(inner);
        let compact = inner.width < COMPACT_CONFIRMATION_WIDTH;
        for (line, tone) in header_lines(actions, context.projection_age_ms, compact) {
            output.label(format!("actions: {line}"));
            let Some(row) = rows.next_row() else { break };
            let style = match tone {
                LineTone::Normal => context.theme.text_style(),
                LineTone::Warning => context.theme.text_style().fg(context.theme.warning),
                LineTone::Confirmation => context
                    .theme
                    .banner_style(dgxtop_core::view::BannerLevel::Confirmation),
            };
            draw_line(
                frame.buffer_mut(),
                row,
                &Line::from(Span::styled(line, style)),
            );
        }
        if let Some(table) = rows.rest() {
            render_recent(frame, table, &actions.recent, context, &mut output);
        }
        output.finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineTone {
    Normal,
    Warning,
    Confirmation,
}

fn header_lines(
    actions: &ActionsView,
    age_offset_ms: u64,
    compact: bool,
) -> Vec<(String, LineTone)> {
    let mut lines = Vec::new();
    if !actions.actions_available {
        let reason = actions
            .unavailable_reason
            .as_deref()
            .map_or_else(|| "not available".to_owned(), clean);
        lines.push((
            format!("process actions unavailable: {reason}"),
            LineTone::Warning,
        ));
    }
    if let Some(confirmation) = &actions.confirmation {
        let mut details = confirmation_lines(confirmation, age_offset_ms, compact).into_iter();
        if let Some(question) = details.next() {
            lines.push((question, LineTone::Confirmation));
        }
        lines.push((CONFIRM_HINT.to_owned(), LineTone::Confirmation));
        lines.extend(details.map(|line| (line, LineTone::Confirmation)));
    }
    if actions.unknown_outcomes > 0 {
        lines.push((
            format!(
                "! {} action(s) have an unknown outcome; they are never retried automatically",
                actions.unknown_outcomes
            ),
            LineTone::Warning,
        ));
    }
    let audit_tone = if actions.audit.accepts_writes() {
        LineTone::Normal
    } else {
        LineTone::Warning
    };
    lines.push((persistence_text("audit", &actions.audit), audit_tone));
    lines
}

fn render_recent(
    frame: &mut Frame<'_>,
    area: Rect,
    recent: &[ActionRecordView],
    context: &DisplayContext,
    output: &mut Output,
) {
    if area.is_empty() {
        return;
    }
    let palette = &context.theme;
    let columns = fit_columns(&RECENT_COLUMNS, area.width);
    let mut rows = RowCursor::new(area);
    if let Some(header) = rows.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &RECENT_COLUMNS,
            &columns,
            palette.header_style(),
        );
    }
    if recent.is_empty()
        && let Some(row) = rows.next_row()
    {
        frame.buffer_mut().set_stringn(
            row.x,
            row.y,
            "no recent actions",
            usize::from(row.width),
            palette.muted_style(),
        );
    }
    for record in recent {
        let Some(row) = rows.next_row() else { break };
        let state_style = match record.state {
            ActionState::Unknown | ActionState::Rejected | ActionState::NotSent => {
                palette.text_style().fg(palette.danger)
            }
            ActionState::SignalSent | ActionState::StillRunning => {
                palette.text_style().fg(palette.warning)
            }
            _ => palette.text_style(),
        };
        let age = format_age(record.age_ms.saturating_add(context.projection_age_ms));
        let cells = [
            TableCell::new(age.clone(), palette.label_style()),
            TableCell::new(clean(&record.target), palette.text_style()),
            TableCell::new(record.signal.name(), palette.label_style()),
            TableCell::new(action_state_label(record.state), state_style),
            TableCell::new(clean(&record.result), palette.label_style()),
        ];
        draw_table_row(frame.buffer_mut(), row, &columns, &cells, None);
        output.label(format!(
            "action {} {}: {} · {} · {age}",
            record.signal.name(),
            clean(&record.target),
            action_state_label(record.state),
            clean(&record.result)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;

    #[test]
    fn signal_sent_is_never_called_terminated() {
        let label = action_state_label(ActionState::SignalSent);
        assert!(label.contains("signal sent"));
        assert!(!label.contains("terminated"));
        assert!(!label.contains("killed"));
        for state in [
            ActionState::Requested,
            ActionState::Sending,
            ActionState::StillRunning,
            ActionState::Unknown,
        ] {
            assert!(!action_state_label(state).contains("terminated"));
        }
    }

    #[test]
    fn confirmation_lines_have_identity_and_countdown_but_no_token() {
        let actions = test_fixtures::actions_view(true);
        let confirmation = actions.confirmation.as_ref().unwrap();
        let lines = confirmation_lines(confirmation, 1_000, false);
        let text = lines.join("\n");
        assert!(text.contains("SIGTERM"));
        assert!(text.contains("pid 4242"));
        assert!(text.contains("uid 1000"));
        assert!(text.contains("python3"));
        assert!(text.contains("start 90100"));
        assert!(text.contains("expires in 8s"));
        assert!(text.contains("cannot be undone"));
        assert!(!text.contains(&test_fixtures::token().as_str().to_owned()));
        let expired = confirmation_lines(confirmation, 60_000, false).join(" ");
        assert!(expired.contains("expired"));
        let compact = confirmation_lines(confirmation, 0, true).join("\n");
        for needle in [
            "SIGTERM pid 4242 (python3)?",
            "uid 1000",
            "start 90100",
            "cannot be undone",
            "expires in 9s",
        ] {
            assert!(compact.contains(needle), "{needle}: {compact}");
        }
    }

    #[test]
    fn header_lines_warn_about_unknown_outcomes_and_unavailable_actions() {
        let mut actions = test_fixtures::actions_view(false);
        actions.unknown_outcomes = 2;
        actions.actions_available = false;
        actions.unavailable_reason = Some("read-only mode".into());
        let lines = header_lines(&actions, 0, false);
        assert!(
            lines
                .iter()
                .any(|(line, _)| line.contains("unavailable: read-only mode"))
        );
        assert!(
            lines
                .iter()
                .any(|(line, _)| line.contains("2 action(s) have an unknown outcome"))
        );
        assert!(
            lines
                .iter()
                .any(|(line, _)| line.starts_with("audit healthy"))
        );
    }
}
