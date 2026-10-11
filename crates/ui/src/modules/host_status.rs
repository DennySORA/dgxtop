//! The data sources block of the Settings tab (D-91): each adapter host's
//! health, how long ago it last read successfully and how long a backed-off
//! host still waits. Narrow terminals keep the adapter and its state and
//! give up the timing columns before the detail.

use dgxtop_core::view::{HostState, HostStatus};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::common::{
    ColumnSpec, Output, RowCursor, TableCell, draw_table_header, draw_table_row, fit_columns,
};
use crate::format::{clean, format_age, format_countdown_ms};
use crate::num::saturate_u16;
use crate::registry::DisplayContext;

const HOST_COLUMNS: [ColumnSpec; 6] = [
    ColumnSpec::left("ADAPTER", 9, 100),
    ColumnSpec::left("STATE", 12, 99).prefer(35),
    ColumnSpec::right("LAST OK", 10, 90),
    ColumnSpec::right("BACKOFF", 7, 80),
    ColumnSpec::right("FAILS", 5, 40),
    ColumnSpec::left("DETAIL", 10, 30).flexible(),
];
/// Title of the block.
const TITLE: &str = "data sources";
/// Text of a host that is not backing off.
const NO_BACKOFF: &str = "–";
const NANOS_PER_MILLI: u64 = 1_000_000;

/// Rows the block takes for `hosts`: its title, the table header and a row
/// per host (one row saying so when there is none).
pub(super) fn host_status_rows(hosts: &[HostStatus]) -> u16 {
    saturate_u16(hosts.len().max(1) + 2)
}

/// Draws the block into `area`, on the projection's clock `now_boot_ns`.
pub(super) fn render_host_status(
    frame: &mut Frame<'_>,
    area: Rect,
    (hosts, now_boot_ns): (&[HostStatus], u64),
    context: &DisplayContext,
    output: &mut Output,
) {
    if area.is_empty() {
        return;
    }
    let palette = &context.theme;
    let mut rows = RowCursor::new(area);
    if let Some(title) = rows.next_row() {
        frame.buffer_mut().set_stringn(
            title.x,
            title.y,
            TITLE,
            usize::from(title.width),
            palette.header_style(),
        );
    }
    let columns = fit_columns(&HOST_COLUMNS, area.width);
    if let Some(header) = rows.next_row() {
        draw_table_header(
            frame.buffer_mut(),
            header,
            &HOST_COLUMNS,
            &columns,
            palette.label_style(),
        );
    }
    if hosts.is_empty()
        && let Some(row) = rows.next_row()
    {
        frame.buffer_mut().set_stringn(
            row.x,
            row.y,
            "no adapter hosts reported",
            usize::from(row.width),
            palette.muted_style(),
        );
        output.label("data sources: no adapter hosts reported");
    }
    for host in hosts {
        let Some(row) = rows.next_row() else { break };
        let state_style = match host.state {
            HostState::Healthy => palette.text_style().fg(palette.success),
            HostState::Starting | HostState::Disabled => palette.label_style(),
            _ => palette.text_style().fg(palette.danger),
        };
        let last_success = last_success_text(host, now_boot_ns);
        let backoff = backoff_text(host, now_boot_ns);
        let cells = [
            TableCell::new(clean(&host.adapter_id), palette.text_style()),
            TableCell::new(host.state.label(), state_style),
            TableCell::new(last_success.clone(), palette.label_style()),
            TableCell::new(backoff.clone(), palette.label_style()),
            TableCell::new(host.consecutive_failures.to_string(), palette.label_style()),
            TableCell::new(
                host.detail.as_deref().map_or_else(String::new, clean),
                palette.muted_style(),
            ),
        ];
        draw_table_row(frame.buffer_mut(), row, &columns, &cells, None);
        output.label(format!(
            "data source {}: {} · last ok {last_success} · backoff {backoff}",
            clean(&host.adapter_id),
            host.state.label()
        ));
    }
}

/// How long ago the host last read successfully, `never` before its first.
pub(super) fn last_success_text(host: &HostStatus, now_boot_ns: u64) -> String {
    host.last_success_boot_ns.map_or_else(
        || "never".to_owned(),
        |success| format_age(now_boot_ns.saturating_sub(success) / NANOS_PER_MILLI),
    )
}

/// Time left before a backed-off host is tried again.
pub(super) fn backoff_text(host: &HostStatus, now_boot_ns: u64) -> String {
    host.backoff_until_boot_ns
        .filter(|until| *until > now_boot_ns)
        .map_or_else(
            || NO_BACKOFF.to_owned(),
            |until| format_countdown_ms((until - now_boot_ns).div_ceil(NANOS_PER_MILLI)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures;

    fn host(last_success: Option<u64>, backoff_until: Option<u64>) -> HostStatus {
        HostStatus {
            last_success_boot_ns: last_success,
            backoff_until_boot_ns: backoff_until,
            ..test_fixtures::host_statuses()[0].clone()
        }
    }

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn last_success_is_an_age_or_never() {
        let now = 100 * SECOND;
        assert_eq!(last_success_text(&host(None, None), now), "never");
        assert_eq!(
            last_success_text(&host(Some(97 * SECOND), None), now),
            "3s ago"
        );
        assert_eq!(last_success_text(&host(Some(now), None), now), "<1s ago");
        // A success stamped after the frame's clock is not a negative age.
        assert_eq!(
            last_success_text(&host(Some(now + SECOND), None), now),
            "<1s ago"
        );
    }

    #[test]
    fn backoff_counts_down_and_ends() {
        let now = 100 * SECOND;
        assert_eq!(backoff_text(&host(None, None), now), NO_BACKOFF);
        assert_eq!(backoff_text(&host(None, Some(107 * SECOND)), now), "7s");
        // Part of a second left still shows a second, never 0s.
        assert_eq!(backoff_text(&host(None, Some(now + 1)), now), "1s");
        assert_eq!(backoff_text(&host(None, Some(now)), now), NO_BACKOFF);
    }

    #[test]
    fn rows_cover_title_header_and_hosts() {
        assert_eq!(host_status_rows(&[]), 3);
        assert_eq!(host_status_rows(&test_fixtures::host_statuses()), 4);
    }
}
