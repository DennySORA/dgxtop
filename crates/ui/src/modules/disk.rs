//! Disk module in the reference layout (D-38): one table of block devices
//! (busiest first) with the capacity of the filesystem each one holds, then
//! the read and write rates of the busiest device, the one the title names.
//! Filesystems on no listed device follow as rows of their own. The
//! filesystem type, its free space (reserved blocks included, beside what
//! is available to users) and the device's await time are the first
//! columns a narrow panel gives up.

use dgxtop_core::identity::EntityId;
use dgxtop_core::view::{DiskDeviceView, DiskView, FilesystemView, MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RATE_ROWS, RateHistory, RowCursor, TableCell, build_manifest,
    cell_label, draw_colored_header, draw_table_row, fit_columns, message, metric_table_cell,
    no_data, panel, rate_rows, render_rate_rows,
};
use crate::format::{clean, state_label_for_width, value_text};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot, Topic,
};

/// Table rows asked for before the rest scrolls into "+N more".
const PREFERRED_TABLE_ROWS: u16 = 4;
/// Text of a cell that does not apply to the row (no filesystem, no device).
const NOT_APPLICABLE: &str = "–";

const COLUMNS: [ColumnSpec; 10] = [
    ColumnSpec::left("Device", 8, 100).flexible().prefer(12),
    ColumnSpec::left("Type", 5, 20),
    ColumnSpec::right("Used", 8, 70),
    ColumnSpec::right("Free", 8, 25),
    ColumnSpec::right("Avail", 8, 60),
    ColumnSpec::right("Use%", 6, 80),
    ColumnSpec::right("Read", 10, 90),
    ColumnSpec::right("Write", 10, 90),
    ColumnSpec::right("IOPS", 10, 40),
    ColumnSpec::right("Await", 8, 35),
];
/// Spec indexes of the read and write columns (their titles are the legend).
const READ_COLUMN: usize = 6;
const WRITE_COLUMN: usize = 7;

pub struct DiskModule {
    manifest: DisplayModuleManifest,
}

impl DiskModule {
    pub fn new() -> Result<Self, ModuleBuildError> {
        Ok(Self {
            manifest: build_manifest(ManifestSpec {
                id: "disk",
                title: "Disk I/O and filesystem capacity",
                topics: &[Topic::Disk],
                min_columns: 36,
                min_rows: 4,
                priority: 60,
                slots: &[Slot::Overview],
            })?,
        })
    }
}

impl DisplayModule for DiskModule {
    fn manifest(&self) -> &DisplayModuleManifest {
        &self.manifest
    }

    fn overview_cell(&self) -> Option<OverviewCell> {
        Some(OverviewCell::at(2, 0, 50))
    }

    fn preferred_rows(&self, model: &ViewModel, _width: u16, _slot: Slot) -> Option<u16> {
        let Some(disk) = model.modules.disk.as_deref() else {
            return Some(3);
        };
        let rows = saturate_u16(table_rows(disk).len()).clamp(1, PREFERRED_TABLE_ROWS);
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
        let Some(disk) = model.modules.disk.as_deref() else {
            let inner = panel(frame, area, "Disk I/O", context);
            message(frame, inner, &no_data("disk"), context.theme.muted_style());
            output.label("disk: no data");
            return output.finish();
        };
        let title = disk.devices.first().map_or_else(
            || "Disk I/O".to_owned(),
            |busiest| format!("Disk I/O [{}]", clean(&busiest.name)),
        );
        let inner = panel(frame, area, &title, context);
        let rows = table_rows(disk);
        if rows.is_empty() {
            message(
                frame,
                inner,
                "no block devices or filesystems reported",
                context.theme.muted_style(),
            );
            output.label("disk: none reported");
            return output.finish();
        }
        // Read and write histories of the busiest device (D-41) below the
        // table, which keeps every other row.
        let history_rows = rate_rows(inner.height);
        let mut cursor = RowCursor::new(inner);
        if let Some(table) = cursor.take(inner.height - history_rows) {
            render_table(frame, table, &rows, context);
        }
        if let (Some(history), Some(busiest)) = (cursor.rest(), disk.devices.first()) {
            let theme = &context.theme;
            let rates = [
                RateHistory {
                    label: "R",
                    color: theme.success,
                    current: &busiest.read_bps,
                    points: &disk.read_trend.points,
                },
                RateHistory {
                    label: "W",
                    color: theme.warning,
                    current: &busiest.write_bps,
                    points: &disk.write_trend.points,
                },
            ];
            render_rate_rows(frame, history, &rates, context);
        }
        add_labels(&mut output, disk, context);
        output.finish()
    }
}

/// One table row: a device with the capacity of the filesystem it holds, or
/// a filesystem on no listed device.
#[derive(Debug, Clone, Copy)]
struct DiskRow<'a> {
    device: Option<&'a DiskDeviceView>,
    filesystem: Option<&'a FilesystemView>,
}

impl DiskRow<'_> {
    fn name(&self) -> String {
        match (self.device, self.filesystem) {
            (Some(device), _) => clean(&device.name),
            (None, Some(filesystem)) => clean(&filesystem.mount_point),
            (None, None) => String::new(),
        }
    }
}

/// Devices in projection order, each with the first filesystem (by mount
/// point) it holds, then the filesystems no listed device holds.
fn table_rows(disk: &DiskView) -> Vec<DiskRow<'_>> {
    let held_by = |device: &EntityId| {
        disk.filesystems
            .iter()
            .find(|filesystem| filesystem.block_device.as_ref() == Some(device))
    };
    let listed = |filesystem: &FilesystemView| {
        filesystem.block_device.as_ref().is_some_and(|device| {
            disk.devices
                .iter()
                .any(|listed| &listed.entity_id == device)
        })
    };
    disk.devices
        .iter()
        .map(|device| DiskRow {
            device: Some(device),
            filesystem: held_by(&device.entity_id),
        })
        .chain(
            disk.filesystems
                .iter()
                .filter(|filesystem| !listed(filesystem))
                .map(|filesystem| DiskRow {
                    device: None,
                    filesystem: Some(filesystem),
                }),
        )
        .collect()
}

fn render_table(frame: &mut Frame<'_>, area: Rect, rows: &[DiskRow<'_>], context: &DisplayContext) {
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
                READ_COLUMN => palette.header_style().fg(palette.success),
                WRITE_COLUMN => palette.header_style().fg(palette.warning),
                _ => palette.header_style(),
            },
        );
    }
    let capacity = usize::from(cursor.remaining());
    let overflow = rows.len() > capacity && capacity > 1;
    let shown = if overflow {
        capacity - 1
    } else {
        rows.len().min(capacity)
    };
    for (position, disk_row) in rows.iter().take(shown).enumerate() {
        let Some(row) = cursor.next_row() else { break };
        // The first device is the one the chart draws.
        let highlight = (position == 0 && disk_row.device.is_some())
            .then(|| Style::new().bg(palette.highlight_background));
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &row_cells(disk_row, context),
            highlight,
        );
    }
    if overflow && let Some(row) = cursor.next_row() {
        frame.buffer_mut().set_stringn(
            row.x,
            row.y,
            format!("+{} more", rows.len() - shown),
            usize::from(row.width),
            palette.muted_style(),
        );
    }
}

fn row_cells(row: &DiskRow<'_>, context: &DisplayContext) -> [TableCell; 10] {
    let palette = &context.theme;
    let absent = || TableCell::new(NOT_APPLICABLE, palette.muted_style());
    let capacity = |cell: fn(&FilesystemView) -> &MetricCell, width: usize| {
        row.filesystem.map_or_else(absent, |filesystem| {
            metric_table_cell(cell(filesystem), palette, width)
        })
    };
    let rate = |cell: fn(&DiskDeviceView) -> &MetricCell, width: usize| {
        row.device.map_or_else(absent, |device| {
            metric_table_cell(cell(device), palette, width)
        })
    };
    let fs_type = row
        .filesystem
        .map(|filesystem| clean(&filesystem.fs_type))
        .filter(|fs_type| !fs_type.is_empty())
        .map_or_else(absent, |fs_type| {
            TableCell::new(fs_type, palette.label_style())
        });
    let name_style = if row.device.is_some() {
        palette.text_style()
    } else {
        palette.label_style()
    };
    [
        TableCell::new(row.name(), name_style),
        fs_type,
        capacity(|filesystem| &filesystem.used, 8),
        capacity(|filesystem| &filesystem.free, 8),
        capacity(|filesystem| &filesystem.available, 8),
        capacity(|filesystem| &filesystem.use_percent, 6),
        rate(|device| &device.read_bps, 10),
        rate(|device| &device.write_bps, 10),
        row.device.map_or_else(absent, |device| {
            TableCell::new(iops_text(device), palette.text_style())
        }),
        rate(|device| &device.await_ms, 8),
    ]
}

/// `12/340` read/write operations per second, or each side's state label.
fn iops_text(device: &DiskDeviceView) -> String {
    let side = |cell: &MetricCell| match cell.value_f64() {
        Some(value) if cell.state.has_value() => format!("{value:.0}"),
        _ => state_label_for_width(cell.state, 4).to_owned(),
    };
    format!("{}/{}", side(&device.read_iops), side(&device.write_iops))
}

fn add_labels(output: &mut Output, disk: &DiskView, context: &DisplayContext) {
    for device in disk.devices.iter().take(8) {
        let name = clean(&device.name);
        output.label(cell_label(
            &format!("disk {name} read"),
            &device.read_bps,
            context,
        ));
        output.label(cell_label(
            &format!("disk {name} write"),
            &device.write_bps,
            context,
        ));
        output.label(cell_label(
            &format!("disk {name} await"),
            &device.await_ms,
            context,
        ));
    }
    for filesystem in disk.filesystems.iter().take(8) {
        output.label(format!(
            "filesystem {} use: {}",
            clean(&filesystem.mount_point),
            value_text(&filesystem.use_percent)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{self, disk_view, entity};
    use dgxtop_core::commands::ViewKind;

    fn header_and_root_row(width: u16) -> (String, String) {
        let model = test_fixtures::view_model(ViewKind::Overview, 1);
        let text = test_fixtures::render_text(&DiskModule::new().unwrap(), &model, (width, 12));
        let lines: Vec<&str> = text.lines().collect();
        (lines[1].to_owned(), lines[2].to_owned())
    }

    /// A wide panel shows the filesystem type, its free and available
    /// space and the device's await time beside the I/O rates.
    #[test]
    fn a_wide_panel_shows_type_free_space_and_await() {
        let (header, row) = header_and_root_row(120);
        for title in ["Type", "Free", "Avail", "Await"] {
            assert!(header.contains(title), "{header}");
        }
        for value in ["ext4", "988 GiB", "900 GiB", "0.4 ms"] {
            assert!(row.contains(value), "{value} missing in {row}");
        }
    }

    /// A narrow panel gives up the type, the free space and the await time
    /// first and keeps the capacity and the rates.
    #[test]
    fn a_narrow_panel_drops_the_detail_columns_first() {
        let (header, row) = header_and_root_row(70);
        for title in ["Used", "Avail", "Use%", "Read", "Write"] {
            assert!(header.contains(title), "{header}");
        }
        for title in ["Type", "Free", "Await"] {
            assert!(!header.contains(title), "{title} kept in {header}");
        }
        assert!(!row.contains("ext4"), "{row}");
        let (header, _) = header_and_root_row(36);
        assert!(header.contains("Device"), "{header}");
    }

    /// A row without a filesystem, or a filesystem without a type, shows
    /// the not-applicable mark instead of an empty type.
    #[test]
    fn a_missing_filesystem_type_is_not_left_blank() {
        let context = test_fixtures::display_context();
        let mut disk = disk_view();
        disk.filesystems[0].fs_type = String::new();
        let rows = table_rows(&disk);
        assert_eq!(row_cells(&rows[0], &context)[1].text, NOT_APPLICABLE);
    }

    #[test]
    fn devices_carry_the_capacity_of_the_filesystem_they_hold() {
        let mut disk = disk_view();
        let mut boot = disk.filesystems[0].clone();
        boot.mount_point = "/boot".into();
        boot.block_device = Some(entity("disk:nvme0n1p1"));
        let mut network = disk.filesystems[0].clone();
        network.mount_point = "/net".into();
        network.block_device = None;
        disk.filesystems.extend([boot, network]);
        let rows = table_rows(&disk);
        let names: Vec<String> = rows.iter().map(DiskRow::name).collect();
        // nvme0n1 holds "/"; "/boot" names an unlisted device and "/net" none.
        assert_eq!(names, vec!["nvme0n1", "/boot", "/net"]);
        assert_eq!(
            rows[0].filesystem.map(|fs| fs.mount_point.as_str()),
            Some("/")
        );
        assert!(rows[1].device.is_none() && rows[2].device.is_none());
    }

    #[test]
    fn iops_show_both_directions_or_their_state() {
        let device = disk_view().devices[0].clone();
        // The fixture's write IOPS is still warming up.
        assert_eq!(iops_text(&device), "120/warmup");
    }
}
