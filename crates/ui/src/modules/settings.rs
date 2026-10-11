//! Settings module: saved / effective / origin per field, restart-required
//! and overload notes, pending change and last result, persistence status
//! and the module list (toggled with SetModuleEnabled through the controller).

use dgxtop_core::action::{PersistenceState, PersistenceStatus};
use dgxtop_core::config::ValueOrigin;
use dgxtop_core::identity::EntityId;
use dgxtop_core::view::{SettingField, SettingsView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RowCursor, TableCell, build_manifest, draw_line,
    draw_table_header, draw_table_row, fit_columns, message, no_data, panel, panel_with_keys,
    split_columns,
};
use crate::format::{clean, format_bytes, truncate_to_width};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, RenderOutput, Slot, Topic, controls,
};

const MODULE_LIST_WIDTH: u16 = 26;
const SIDE_BY_SIDE: u16 = 140;

const FIELD_COLUMNS: [ColumnSpec; 6] = [
    ColumnSpec::left("SETTING", 16, 100).flexible().prefer(26),
    ColumnSpec::left("SAVED", 9, 80).prefer(12),
    ColumnSpec::left("EFFECTIVE", 9, 95).prefer(12),
    ColumnSpec::left("ORIGIN", 12, 70),
    ColumnSpec::left("RESTART", 16, 60),
    ColumnSpec::left("NOTE", 10, 50).flexible().prefer(24),
];

/// The keys acting on the view, at the right of its title (D-70).
const SETTINGS_KEYS: &str = "j/k/↑/↓ select · Space enable / disable";

pub struct SettingsModule {
    manifest: DisplayModuleManifest,
}

impl SettingsModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "settings",
                title: "Settings",
                topics: &[Topic::Settings],
                min_columns: 40,
                min_rows: 6,
                priority: 35,
                slots: &[Slot::Detail],
            })?,
        })
    }
}

impl DisplayModule for SettingsModule {
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
        let Some(settings) = model.modules.settings.as_deref() else {
            let inner = panel(frame, area, "Settings", context);
            message(
                frame,
                inner,
                &no_data("settings"),
                context.theme.muted_style(),
            );
            output.label("settings: no data");
            return output.finish();
        };
        let inner = panel_with_keys(
            frame,
            area,
            &format!("Settings · revision {}", settings.revision),
            SETTINGS_KEYS,
            context,
        );
        let mut rows = RowCursor::new(inner);
        for line in status_lines(settings) {
            output.label(format!("settings {line}"));
            let Some(row) = rows.next_row() else { break };
            draw_line(
                frame.buffer_mut(),
                row,
                &Line::from(Span::styled(line, context.theme.text_style())),
            );
        }
        let Some(body) = rows.rest() else {
            return output.finish();
        };
        if body.width >= SIDE_BY_SIDE {
            let (fields, modules) =
                split_columns(body, body.width.saturating_sub(MODULE_LIST_WIDTH + 1));
            render_fields(frame, fields, &settings.fields, context, &mut output);
            self.render_modules(frame, modules, model, settings, context, &mut output);
        } else {
            let module_rows = saturate_u16(settings.modules.len() + 1).min(body.height / 2);
            let mut stacked = RowCursor::new(body);
            let fields = stacked
                .take(body.height.saturating_sub(module_rows))
                .unwrap_or_default();
            render_fields(frame, fields, &settings.fields, context, &mut output);
            if let Some(modules) = stacked.rest() {
                self.render_modules(frame, modules, model, settings, context, &mut output);
            }
        }
        output.finish()
    }
}

fn persistence_word(state: PersistenceState) -> &'static str {
    match state {
        PersistenceState::Disabled => "disabled",
        PersistenceState::Healthy => "healthy",
        PersistenceState::Degraded => "degraded",
        PersistenceState::Failed => "failed (writes refused)",
        PersistenceState::ReadOnlyDiagnostic => "read-only diagnostic (newer schema)",
    }
}

/// `config healthy · 12 KiB of 1.0 MiB` plus the optional detail.
pub(crate) fn persistence_text(label: &str, status: &PersistenceStatus) -> String {
    let mut text = format!("{label} {}", persistence_word(status.state));
    if status.bytes_limit > 0 {
        text.push_str(&format!(
            " · {} of {}",
            format_bytes(status.bytes_used),
            format_bytes(status.bytes_limit)
        ));
    }
    if status.wal_bytes > 0 {
        text.push_str(&format!(" · WAL {}", format_bytes(status.wal_bytes)));
    }
    if let Some(detail) = status.detail.as_deref() {
        text.push_str(&format!(" · {}", clean(detail)));
    }
    text
}

fn status_lines(settings: &SettingsView) -> Vec<String> {
    let mut lines = vec![persistence_text("config", &settings.config_status)];
    lines.push(match settings.pending.as_deref() {
        Some(pending) => format!("pending: {}", clean(pending)),
        None => "pending: none".to_owned(),
    });
    if let Some(result) = settings.last_result.as_deref() {
        lines.push(format!("last result: {}", clean(result)));
    }
    lines
}

const fn origin_word(origin: ValueOrigin) -> &'static str {
    match origin {
        ValueOrigin::Default => "default",
        ValueOrigin::Saved => "saved",
        ValueOrigin::CliOverride => "CLI override",
    }
}

fn render_fields(
    frame: &mut Frame<'_>,
    area: Rect,
    fields: &[SettingField],
    context: &DisplayContext,
    output: &mut Output,
) {
    if area.is_empty() {
        return;
    }
    let palette = &context.theme;
    let columns = fit_columns(&FIELD_COLUMNS, area.width);
    let mut rows = RowCursor::new(area);
    if let Some(header) = rows.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &FIELD_COLUMNS,
            &columns,
            palette.header_style(),
        );
    }
    for field in fields {
        let Some(row) = rows.next_row() else { break };
        let differs = field.saved != field.effective;
        let effective_style = if differs {
            palette.tone_style(crate::format::CellTone::Fallback)
        } else {
            palette.text_style()
        };
        let restart = if field.restart_required {
            "restart required"
        } else {
            "live"
        };
        let cells = [
            TableCell::new(clean(&field.path), palette.text_style()),
            TableCell::new(clean(&field.saved), palette.label_style()),
            TableCell::new(clean(&field.effective), effective_style),
            TableCell::new(origin_word(field.origin), palette.label_style()),
            TableCell::new(restart, palette.label_style()),
            TableCell::new(
                field
                    .overload_note
                    .as_deref()
                    .map_or_else(String::new, clean),
                palette.label_style(),
            ),
        ];
        draw_table_row(frame.buffer_mut(), row, &columns, &cells, None);
        output.label(format!(
            "setting {}: saved {} effective {} origin {} {}{}",
            clean(&field.path),
            clean(&field.saved),
            clean(&field.effective),
            origin_word(field.origin),
            restart,
            field
                .overload_note
                .as_deref()
                .map_or_else(String::new, |note| format!(" · {}", clean(note)))
        ));
    }
}

impl SettingsModule {
    fn render_modules(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        model: &ViewModel,
        settings: &SettingsView,
        context: &DisplayContext,
        output: &mut Output,
    ) {
        if area.is_empty() {
            return;
        }
        let selected = model
            .session
            .selected_entity_id
            .as_ref()
            .map(EntityId::as_str);
        let mut rows = RowCursor::new(area);
        if let Some(header) = rows.next_row() {
            frame.buffer_mut().set_stringn(
                header.x,
                header.y,
                "modules",
                usize::from(header.width),
                context.theme.header_style(),
            );
        }
        for (module_id, enabled) in &settings.modules {
            let Some(row) = rows.next_row() else { break };
            let is_selected = selected == Some(module_id.as_str());
            let marker = if is_selected { "▶" } else { " " };
            let check = if *enabled { "[x]" } else { "[ ]" };
            let state = if *enabled { "enabled" } else { "disabled" };
            let text = format!("{marker} {check} {} · {state}", module_id.as_str());
            let style = if is_selected {
                context.theme.selected_row_style()
            } else {
                context.theme.text_style()
            };
            frame.buffer_mut().set_stringn(
                row.x,
                row.y,
                truncate_to_width(&text, usize::from(row.width)),
                usize::from(row.width),
                style,
            );
            if let Ok(entity) = EntityId::new(module_id.as_str()) {
                output.entity_target(
                    model,
                    &self.manifest,
                    controls::SETTINGS_MODULE,
                    row,
                    entity,
                );
            }
            output.label(format!("module {}: {state}", module_id.as_str()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines_show_pending_and_result() {
        let settings = crate::test_fixtures::settings_view();
        let lines = status_lines(&settings);
        assert!(lines[1].starts_with("pending: "));
        assert!(lines.iter().any(|line| line.starts_with("last result:")));
    }
}
