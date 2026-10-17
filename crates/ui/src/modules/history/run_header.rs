//! The fixed top of an opened run's page (D-116), the same on every page
//! of its charts: the identity line (its color, name, PID, user and
//! niceness, its GPUs and how it ended), then `key value` rows read down
//! two columns, the keys aligned as in the GPU view's blocks: when it
//! started, ended and how long it ran, and its parent, at the left; its
//! folder, program, control group and command line at the right, where
//! the long values have room. A path too long is cut in its middle, so
//! its file stays; the command is cut at its end, or with `expanded`
//! wrapped onto at most [`MAX_COMMAND_LINES`] rows. A panel too narrow for
//! two columns stacks them. What was not read is a dim `n/a`.

use dgxtop_core::process_history::RunInfo;
use dgxtop_core::view::{RunDetailView, RunRowView};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::runs::{gpu_parts, runtime_text, status_style, status_text};
use crate::format::{
    NOT_AVAILABLE, clean, display_width, format_local_datetime, pad_right, truncate_middle,
    truncate_to_width,
};
use crate::modules::common::separator;
use crate::registry::DisplayContext;

/// Columns before the rows under the identity line, as wide as its color
/// swatch, so the keys line up with the run's name.
const INDENT: usize = 2;
/// Columns between a key and its value.
const KEY_GAP: usize = 2;
/// Columns between the two columns, as between the GPU view's.
const COLUMN_GAP: usize = 3;
/// Widest value of the left column; a longer one is cut.
const MAX_LEFT_VALUE: usize = 24;
/// Least room for the right column's values; less stacks the columns.
const MIN_RIGHT_VALUE: usize = 24;
/// Rows an expanded command takes at most.
const MAX_COMMAND_LINES: usize = 8;

/// The run's identity line, then its `key value` rows for a panel `width`
/// columns wide (module docs).
pub(super) fn header_lines(
    run: &RunRowView,
    detail: Option<&RunDetailView>,
    offset_s: i32,
    width: u16,
    context: &DisplayContext,
) -> Vec<Line<'static>> {
    let info = detail.map(|detail| &detail.info);
    let mut lines = vec![identity_line(run, info, context)];
    let left = time_fields(run, info, offset_s, context);
    let right = place_fields(info, context);
    lines.extend(Grid { left, right }.lines(usize::from(width), context));
    lines
}

/// `■ python │ PID 4242 │ uid 1000 │ nice 0 │ GPU 0 · GB10 │ running`; the user
/// and the niceness left out while unread.
fn identity_line(
    run: &RunRowView,
    info: Option<&RunInfo>,
    context: &DisplayContext,
) -> Line<'static> {
    let theme = &context.theme;
    let swatch = run.slot.map_or_else(
        || Span::raw(" ".repeat(INDENT)),
        |slot| Span::styled("■ ", Style::new().fg(theme.process_color(slot))),
    );
    let mut spans = vec![swatch, Span::styled(clean(&run.name), theme.title_style())];
    let uid = info.and_then(|info| info.uid).map(|uid| uid.to_string());
    let nice = info
        .and_then(|info| info.details.nice)
        .map(|nice| nice.to_string());
    let labeled = [
        ("PID", Some(run.key.pid.to_string())),
        ("uid", uid),
        ("nice", nice),
    ];
    for (label, value) in labeled {
        let Some(value) = value else { continue };
        spans.push(separator(context));
        spans.push(Span::styled(format!("{label} "), theme.label_style()));
        spans.push(Span::styled(value, theme.text_style()));
    }
    spans.push(separator(context));
    // Each GPU in its color (D-123).
    spans.extend(
        gpu_parts(&run.gpus, &run.gpu_names)
            .into_iter()
            .map(|(text, gpu)| {
                let style = theme.label_style();
                let style = gpu.map_or(style, |gpu| theme.gpu_style(gpu, style));
                Span::styled(text, style)
            }),
    );
    spans.push(separator(context));
    spans.push(Span::styled(status_text(run), status_style(run, context)));
    Line::from(spans)
}

/// When it started and ended, how long it ran, and its parent.
fn time_fields(
    run: &RunRowView,
    info: Option<&RunInfo>,
    offset_s: i32,
    context: &DisplayContext,
) -> Vec<Field> {
    let date = |utc_ns| format_local_datetime(utc_ns, offset_s);
    let mut fields = vec![Field::text(
        "started",
        &date(run.started_utc_ns),
        Fit::End,
        context,
    )];
    if let Some(end) = run.end {
        fields.push(Field::text("ended", &date(end.utc_ns), Fit::End, context));
    }
    fields.push(Field::text(
        "runtime",
        &runtime_text(run.runtime_ns),
        Fit::End,
        context,
    ));
    fields.push(Field::text("parent", &parent_text(info), Fit::End, context));
    fields
}

/// Where it ran and what: its folder, program, control group and command.
fn place_fields(info: Option<&RunInfo>, context: &DisplayContext) -> Vec<Field> {
    let text = |pick: fn(&RunInfo) -> &str| info.map_or("", pick);
    vec![
        Field::text("folder", text(|info| &info.cwd), Fit::Middle, context),
        Field::text(
            "program",
            text(|info| &info.details.executable),
            Fit::Middle,
            context,
        ),
        Field::text(
            "cgroup",
            text(|info| &info.details.cgroup),
            Fit::Middle,
            context,
        ),
        Field::text("command", text(|info| &info.command), Fit::Wrap, context),
    ]
}

/// `7 bash`: the parent's PID and name as first read; empty while unread.
fn parent_text(info: Option<&RunInfo>) -> String {
    let Some(details) = info.map(|info| &info.details) else {
        return String::new();
    };
    match (details.parent_pid, details.parent_name.as_str()) {
        (None, _) => String::new(),
        (Some(pid), "") => pid.to_string(),
        (Some(pid), name) => format!("{pid} {}", clean(name)),
    }
}

/// How a value too long for its room is cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fit {
    /// At its end: `2026-10-04 09:0…`.
    End,
    /// In its middle, so a path keeps its file: `/home/u/…/bin/python3`.
    Middle,
    /// At its end, or wrapped onto more rows when the whole of it is asked
    /// for (`e`).
    Wrap,
}

/// A `key value` row.
#[derive(Debug, Clone)]
struct Field {
    key: &'static str,
    value: String,
    style: Style,
    fit: Fit,
}

impl Field {
    /// `value`, cleaned, in the text color; a dim `n/a` when empty.
    fn text(key: &'static str, value: &str, fit: Fit, context: &DisplayContext) -> Self {
        let (value, style) = if value.is_empty() {
            (NOT_AVAILABLE.to_owned(), context.theme.muted_style())
        } else {
            (clean(value), context.theme.text_style())
        };
        Self {
            key,
            value,
            style,
            fit,
        }
    }

    /// The value's rows in `room` columns.
    fn value_rows(&self, room: usize, expanded: bool) -> Vec<String> {
        match self.fit {
            Fit::Middle => vec![truncate_middle(&self.value, room)],
            Fit::Wrap if expanded => wrapped(&self.value, room.max(1)),
            Fit::End | Fit::Wrap => vec![truncate_to_width(&self.value, room)],
        }
    }

    /// The field's rows in a column whose keys take `keys` columns: the
    /// key, then the value in `room`; a wrapped value's later rows under
    /// it, the key left blank.
    fn rows(
        &self,
        (keys, room): (usize, usize),
        context: &DisplayContext,
    ) -> Vec<Vec<Span<'static>>> {
        self.value_rows(room, context.expanded)
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let key = if index == 0 { self.key } else { "" };
                vec![
                    Span::styled(pad_right(key, keys + KEY_GAP), context.theme.label_style()),
                    Span::styled(value, self.style),
                ]
            })
            .collect()
    }
}

/// `text` in pieces of `room` columns, whole characters, on at most
/// [`MAX_COMMAND_LINES`] rows, the last cut with `…` when more is left.
fn wrapped(text: &str, room: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut rest = text;
    while !rest.is_empty() && rows.len() < MAX_COMMAND_LINES {
        if rows.len() + 1 == MAX_COMMAND_LINES {
            rows.push(truncate_to_width(rest, room));
            break;
        }
        let taken = take_width(rest, room).max(rest.chars().next().map_or(0, char::len_utf8));
        rows.push(rest[..taken].to_owned());
        rest = &rest[taken..];
    }
    rows
}

/// Bytes of `text` that fit `width` columns, whole characters.
fn take_width(text: &str, width: usize) -> usize {
    let mut used = 0;
    for (index, character) in text.char_indices() {
        let columns = display_width(&character.to_string());
        if used + columns > width {
            return index;
        }
        used += columns;
    }
    text.len()
}

/// The rows under the identity line: two columns of fields side by side.
struct Grid {
    left: Vec<Field>,
    right: Vec<Field>,
}

impl Grid {
    /// The rows in `width` columns (module docs).
    fn lines(&self, width: usize, context: &DisplayContext) -> Vec<Line<'static>> {
        let key_width = |fields: &[Field]| {
            fields
                .iter()
                .map(|field| field.key.len())
                .max()
                .unwrap_or(0)
        };
        let (left_keys, right_keys) = (key_width(&self.left), key_width(&self.right));
        let left_values = self
            .left
            .iter()
            .map(|field| display_width(&field.value))
            .max()
            .unwrap_or(0)
            .min(MAX_LEFT_VALUE);
        let right_x = INDENT + left_keys + KEY_GAP + left_values + COLUMN_GAP;
        let right_room = width.saturating_sub(right_x + right_keys + KEY_GAP);
        if right_room < MIN_RIGHT_VALUE {
            let keys = left_keys.max(right_keys);
            let room = width.saturating_sub(INDENT + keys + KEY_GAP);
            return self
                .left
                .iter()
                .chain(&self.right)
                .flat_map(|field| field.rows((keys, room), context))
                .map(indented)
                .collect();
        }
        let column = |fields: &[Field], keys: usize, room: usize| -> Vec<Vec<Span<'static>>> {
            fields
                .iter()
                .flat_map(|field| field.rows((keys, room), context))
                .collect()
        };
        let left = column(&self.left, left_keys, left_values);
        let right = column(&self.right, right_keys, right_room);
        (0..left.len().max(right.len()))
            .map(|index| {
                let mut spans = left.get(index).cloned().unwrap_or_default();
                if let Some(right) = right.get(index) {
                    let used: usize = spans.iter().map(Span::width).sum();
                    spans.push(Span::raw(" ".repeat(right_x - INDENT - used)));
                    spans.extend(right.iter().cloned());
                }
                indented(spans)
            })
            .collect()
    }
}

/// `spans` after the indent.
fn indented(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut line = vec![Span::raw(" ".repeat(INDENT))];
    line.extend(spans);
    Line::from(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header's rows as text.
    fn rows(width: u16, expanded: bool, live: bool) -> Vec<String> {
        let mut context = crate::test_fixtures::display_context();
        context.expanded = expanded;
        let mut view = crate::test_fixtures::process_history_view();
        let mut run = view.runs[0].clone();
        let detail = view.opened.as_mut().unwrap();
        detail.info.details.executable =
            "/home/u/.local/share/uv/python/cpython-3.13.15-linux-aarch64-gnu/bin/python3.13"
                .to_owned();
        if live {
            run.live = true;
            run.end = None;
        }
        header_lines(&run, Some(detail), 8 * 3600, width, &context)
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    /// The identity line, then the times and the parent at the left and
    /// the places at the right, the keys of each column aligned.
    #[test]
    fn the_header_reads_down_two_columns() {
        let header = rows(135, false, false);
        let text = header.join("\n");
        assert_eq!(
            header[0],
            "  ComfyUI/main │ PID 42 │ uid 1000 │ nice 0 │ GPU 0 · GB10 │ SIGKILL by dgxtop"
        );
        assert_eq!(header.len(), 5, "identity and four rows: {text}");
        let started = &header[1];
        assert!(
            started.starts_with("  started  2026-10-04 09:00:00"),
            "{text}"
        );
        assert!(
            started.contains("   folder   /home/u/code/ComfyUI"),
            "{text}"
        );
        assert!(
            header[2].starts_with("  ended    2026-10-04 09:30:00"),
            "{text}"
        );
        assert!(header[3].starts_with("  runtime  30m"), "{text}");
        assert!(header[4].starts_with("  parent   7 bash"), "{text}");
        let column = |row: &str, key: &str| row.find(key).unwrap();
        assert_eq!(column(&header[1], "folder"), column(&header[2], "program"));
        assert_eq!(column(&header[2], "program"), column(&header[4], "command"));
        assert!(
            header[2].contains("program  /home/u/.local/share/uv/python/cpython"),
            "{text}"
        );
        assert!(header[3].contains("cgroup   /user.slice/user-1000.slice/session-3.scope"));
        assert!(header[4].contains("command  /venv/bin/python main.py --listen"));
    }

    /// A running run has no end: three rows at the left under four at the
    /// right.
    #[test]
    fn a_running_run_has_no_end_row() {
        let header = rows(135, false, true);
        assert_eq!(header.len(), 5);
        assert!(header[0].ends_with("│ running"), "{}", header[0]);
        assert!(header[3].starts_with("  parent   7 bash"), "{}", header[3]);
        assert!(
            header[4].trim_start().starts_with("command"),
            "{}",
            header[4]
        );
    }

    /// A long path keeps its start and its file; the command keeps its
    /// start, or with `e` all of it on rows of its own under its key.
    #[test]
    fn long_values_are_cut_to_keep_what_matters() {
        let header = rows(100, false, false);
        let program = header.iter().find(|row| row.contains("program")).unwrap();
        assert!(program.ends_with("bin/python3.13"), "{program}");
        assert!(program.contains('…'), "{program}");
        assert!(header.iter().all(|row| row.chars().count() <= 100));
        let command = header.iter().find(|row| row.contains("command")).unwrap();
        assert!(command.ends_with('…'), "{command}");
        let expanded = rows(100, true, false);
        assert!(expanded.len() > header.len(), "{expanded:?}");
        let whole: String = expanded
            .iter()
            .skip_while(|row| !row.contains("command"))
            .map(|row| row.trim_start().trim_start_matches("command").trim())
            .collect();
        assert!(whole.ends_with("--preview-method auto"), "{whole}");
        assert!(expanded.iter().all(|row| row.chars().count() <= 100));
    }

    /// A panel too narrow for two columns stacks them, one key column.
    #[test]
    fn a_narrow_panel_stacks_the_columns() {
        let header = rows(56, false, false);
        let keys: Vec<&str> = header[1..]
            .iter()
            .map(|row| row.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "started", "ended", "runtime", "parent", "folder", "program", "cgroup", "command"
            ]
        );
        let value_x: Vec<usize> = header[1..]
            .iter()
            .map(|row| row[2..].find("  ").unwrap())
            .collect();
        assert!(
            header[1..].iter().all(|row| row.chars().count() <= 56),
            "{header:?}"
        );
        assert!(value_x.iter().all(|x| *x <= 9), "{value_x:?}");
    }

    /// What was not read shows as a dim `n/a`, and the identity line
    /// leaves out the user and niceness it does not know.
    #[test]
    fn unread_values_are_a_dim_not_available() {
        let context = crate::test_fixtures::display_context();
        let view = crate::test_fixtures::process_history_view();
        let lines = header_lines(&view.runs[1], None, 0, 135, &context);
        let identity = lines[0].to_string();
        assert!(!identity.contains("uid"), "{identity}");
        let folder = lines[1]
            .spans
            .iter()
            .skip_while(|span| span.content.trim() != "folder")
            .nth(1)
            .unwrap();
        assert_eq!(folder.content, NOT_AVAILABLE);
        assert_eq!(folder.style, context.theme.muted_style());
    }
}
