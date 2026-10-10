//! Network module in the reference layout (D-38): interfaces (busiest first,
//! `●` up, `○` otherwise) with receive and transmit rates and errors, then
//! the rates of the busiest interface, the one the title names.

use dgxtop_core::view::{InterfaceView, MetricCell, NetworkView, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RATE_ROWS, RateHistory, RowCursor, TableCell, build_manifest,
    cell_label, draw_colored_header, draw_table_row, fit_columns, message, metric_table_cell,
    no_data, panel, rate_rows, render_rate_rows,
};
use crate::format::{clean, state_label_for_width};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot, Topic,
};

/// Table rows asked for before the rest scrolls into "+N more".
const PREFERRED_TABLE_ROWS: u16 = 4;
/// Operational state of an interface that carries traffic.
const UP: &str = "up";

const COLUMNS: [ColumnSpec; 4] = [
    ColumnSpec::left("Interface", 10, 100).flexible().prefer(14),
    ColumnSpec::right("RX/s", 11, 95),
    ColumnSpec::right("TX/s", 11, 95),
    ColumnSpec::right("Errors", 7, 60),
];
/// Spec indexes of the receive and transmit columns (the chart legend).
const RX_COLUMN: usize = 1;
const TX_COLUMN: usize = 2;

pub struct NetworkModule {
    manifest: DisplayModuleManifest,
}

impl NetworkModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "network",
                title: "Network interfaces",
                topics: &[Topic::Network],
                min_columns: 36,
                min_rows: 4,
                priority: 55,
                slots: &[Slot::Overview],
            })?,
        })
    }
}

impl DisplayModule for NetworkModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn overview_cell(&self) -> Option<OverviewCell> {
        Some(OverviewCell::at(2, 1, 50))
    }

    fn preferred_rows(&self, model: &ViewModel, _width: u16, _slot: Slot) -> Option<u16> {
        let Some(network) = model.modules.network.as_deref() else {
            return Some(3);
        };
        let rows = saturate_u16(visible_count(network)).clamp(1, PREFERRED_TABLE_ROWS);
        Some(2 + 1 + rows + RATE_ROWS)
    }

    fn render(
        &self,
        context: &DisplayContext,
        model: &ViewModel,
        area: Rect,
        frame: &mut Frame<'_>,
    ) -> RenderOutput {
        let mut output = Output::default();
        let Some(network) = model.modules.network.as_deref() else {
            let inner = panel(frame, area, "Network", context);
            message(
                frame,
                inner,
                &no_data("network"),
                context.theme.muted_style(),
            );
            output.label("network: no data");
            return output.finish();
        };
        let title = network.interfaces.first().map_or_else(
            || "Network".to_owned(),
            |busiest| format!("Network [{}]", clean(&busiest.name)),
        );
        let inner = panel(frame, area, &title, context);
        if network.interfaces.is_empty() {
            message(
                frame,
                inner,
                "no network interfaces reported",
                context.theme.muted_style(),
            );
            output.label("network: none reported");
            return output.finish();
        }
        // Receive and transmit histories of the busiest interface (D-41)
        // below the table, which keeps every other row.
        let history_rows = rate_rows(inner.height);
        let mut cursor = RowCursor::new(inner);
        if let Some(table) = cursor.take(inner.height - history_rows) {
            render_table(frame, table, network, context);
        }
        if let (Some(history), Some(busiest)) = (cursor.rest(), network.interfaces.first()) {
            let theme = &context.theme;
            let rates = [
                RateHistory {
                    label: "↓",
                    color: theme.success,
                    current: &busiest.rx_bps,
                    points: &network.rx_trend.points,
                },
                RateHistory {
                    label: "↑",
                    color: theme.accent,
                    current: &busiest.tx_bps,
                    points: &network.tx_trend.points,
                },
            ];
            render_rate_rows(frame, history, &rates, context);
        }
        for interface in network.interfaces.iter().take(8) {
            let name = clean(&interface.name);
            output.label(cell_label(
                &format!("network {name} rx"),
                &interface.rx_bps,
                context,
            ));
            output.label(cell_label(
                &format!("network {name} tx"),
                &interface.tx_bps,
                context,
            ));
        }
        output.finish()
    }
}

/// Interfaces the configuration lets the table show.
fn visible_count(network: &NetworkView) -> usize {
    let configured = usize::try_from(network.max_visible.max(1)).unwrap_or(usize::MAX);
    network.interfaces.len().min(configured)
}

fn render_table(
    frame: &mut Frame<'_>,
    area: Rect,
    network: &NetworkView,
    context: &DisplayContext,
) {
    let palette = &context.theme;
    let columns = fit_columns(&COLUMNS, area.width);
    let mut cursor = RowCursor::new(area);
    if let Some(header) = cursor.next_row() {
        draw_colored_header(
            frame.buffer_mut(),
            header,
            &COLUMNS,
            &columns,
            |spec| match spec {
                RX_COLUMN => palette.header_style().fg(palette.success),
                TX_COLUMN => palette.header_style().fg(palette.accent),
                _ => palette.header_style(),
            },
        );
    }
    let capacity = usize::from(cursor.remaining()).min(visible_count(network));
    let total = network.interfaces.len();
    // With a single free row an interface is more useful than "+N more".
    let overflow = total > capacity && capacity > 1;
    let shown = if overflow {
        capacity - 1
    } else {
        total.min(capacity)
    };
    for (position, interface) in network.interfaces.iter().take(shown).enumerate() {
        let Some(row) = cursor.next_row() else { break };
        // The first interface is the one the chart draws.
        let highlight = (position == 0).then(|| Style::new().bg(palette.highlight_background));
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &interface_cells(interface, context),
            highlight,
        );
    }
    if overflow && let Some(row) = cursor.next_row() {
        frame.buffer_mut().set_stringn(
            row.x,
            row.y,
            format!("+{} more", total - shown),
            usize::from(row.width),
            palette.muted_style(),
        );
    }
}

fn interface_cells(interface: &InterfaceView, context: &DisplayContext) -> [TableCell; 4] {
    let palette = &context.theme;
    let up = interface.state.as_deref() == Some(UP);
    let (dot, dot_style) = if up {
        ("●", Style::new().fg(palette.success))
    } else {
        ("○", palette.muted_style())
    };
    let name = TableCell::new(format!("{dot} {}", clean(&interface.name)), dot_style);
    [
        name,
        metric_table_cell(&interface.rx_bps, palette, 11),
        metric_table_cell(&interface.tx_bps, palette, 11),
        errors_cell(&interface.rx_errors, &interface.tx_errors, context),
    ]
}

/// Receive plus transmit errors, red when any; a side without a value shows
/// its state instead of being counted as zero.
fn errors_cell(rx: &MetricCell, tx: &MetricCell, context: &DisplayContext) -> TableCell {
    let palette = &context.theme;
    let value = |cell: &MetricCell| cell.value_f64().filter(|_| cell.state.has_value());
    match (value(rx), value(tx)) {
        (Some(rx), Some(tx)) => {
            let total = rx + tx;
            let style = if total > 0.0 {
                Style::new().fg(palette.danger)
            } else {
                palette.label_style()
            };
            TableCell::new(format!("{total:.0}"), style)
        }
        (None, _) => TableCell::new(state_label_for_width(rx.state, 7), palette.muted_style()),
        (_, None) => TableCell::new(state_label_for_width(tx.state, 7), palette.muted_style()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{display_context, exact, missing};
    use dgxtop_core::metric::CanonicalUnit;
    use dgxtop_core::view::CellState;

    #[test]
    fn errors_add_both_directions_and_never_hide_a_missing_side() {
        let context = display_context();
        let count = |value: u64| exact(value, CanonicalUnit::Count);
        assert_eq!(errors_cell(&count(0), &count(0), &context).text, "0");
        assert_eq!(errors_cell(&count(2), &count(3), &context).text, "5");
        let unknown = missing(CanonicalUnit::Count, CellState::Unavailable);
        assert_eq!(errors_cell(&count(2), &unknown, &context).text, "unavail.");
        assert_eq!(errors_cell(&unknown, &count(2), &context).text, "unavail.");
    }
}
