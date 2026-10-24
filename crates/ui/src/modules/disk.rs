//! Disk module in the reference layout (D-38): one table of the block
//! devices that hold a filesystem (busiest first, D-124) with the capacity
//! of the filesystem each one holds, then the read and write rates of the
//! busiest whole disk, the one the title names, which counts the rows on
//! it; its own row, where it has one, is highlighted. A device's name
//! follows the name of its filesystem's mount point in brackets (D-130:
//! `[DennySORA_SSD] nvme0n1p3`). Filesystems on no listed device follow as
//! rows of their own. The filesystem type, its free
//! space (reserved blocks included, beside what is available to users), the
//! drive's temperature (D-92; blank when the drive reports none) and the
//! device's await time are the first columns a narrow panel gives up.

use dgxtop_core::identity::EntityId;
use dgxtop_core::view::{DiskDeviceView, DiskView, FilesystemView, MetricCell, ViewModel};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::ModuleBuildError;
use super::common::{
    ColumnSpec, ManifestSpec, Output, RATE_ROWS, RateHistory, RowCursor, TableCell, build_manifest,
    cell_label, draw_colored_header, draw_table_row, fit_columns, message, metric_table_cell,
    no_data, panel, panel_with_keys, rate_rows, render_rate_rows,
};
use super::folds::{fold_keys, render_folded};
use crate::format::{clean, display_width, state_label_for_width, truncate_middle, value_text};
use crate::num::saturate_u16;
use crate::registry::{
    DisplayContext, DisplayModule, DisplayModuleManifest, OverviewCell, RenderOutput, Slot,
};

/// Table rows asked for before the rest scrolls into "+N more".
const PREFERRED_TABLE_ROWS: u16 = 4;
/// Text of a cell that does not apply to the row (no filesystem, no device).
const NOT_APPLICABLE: &str = "–";
/// Columns the brackets and the gap around a mount point take.
const MOUNT_MARKS: usize = 3;

const COLUMNS: [ColumnSpec; 11] = [
    // Room for `[DennySORA_SSD] nvme0n1p3` (D-130): the optional columns
    // give way to it first.
    ColumnSpec::left("Device", 8, 100).flexible().prefer(26),
    ColumnSpec::left("Type", 5, 20),
    ColumnSpec::right("Used", 8, 70),
    ColumnSpec::right("Free", 8, 25),
    ColumnSpec::right("Avail", 8, 60),
    ColumnSpec::right("Use%", 6, 80),
    ColumnSpec::right("Read", 10, 90),
    ColumnSpec::right("Write", 10, 90),
    ColumnSpec::right("IOPS", 10, 40),
    ColumnSpec::right("Await", 8, 35),
    ColumnSpec::right("Temp", 5, 30),
];
const DEVICE_COLUMN: usize = 0;
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
        let title = disk.charted.as_ref().map_or_else(
            || "Disk I/O".to_owned(),
            |charted| format!("Disk I/O [{}]", clean(&charted.name)),
        );
        if context.folded.contains("disk") {
            render_folded(
                frame,
                area,
                (&title, "disk"),
                (&self.manifest, model),
                context,
                &mut output,
            );
            return output.finish();
        }
        // Its fold key, where it acts (D-114).
        let inner = panel_with_keys(frame, area, &title, &fold_keys("disk"), context);
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
        // Read and write histories of the charted disk (D-41) below the
        // table, which keeps every other row.
        let history_rows = rate_rows(inner.height);
        let charted = disk.charted.as_ref();
        let mut cursor = RowCursor::new(inner);
        if let Some(table) = cursor.take(inner.height - history_rows) {
            let charted_id = charted.map(|device| &device.entity_id);
            render_table(frame, table, &rows, charted_id, context);
        }
        if let (Some(history), Some(charted)) = (cursor.rest(), charted) {
            let theme = &context.theme;
            let rates = [
                RateHistory {
                    label: "R",
                    color: theme.success,
                    current: &charted.read_bps,
                    points: &disk.read_trend.points,
                },
                RateHistory {
                    label: "W",
                    color: theme.warning,
                    current: &charted.write_bps,
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
    /// The row's name in `width` columns (D-130): a device's after the name
    /// of its filesystem's mount point, a filesystem on no listed device by
    /// that name alone.
    fn name(&self, width: usize) -> String {
        let mount = self
            .filesystem
            .map(|filesystem| clean(mount_name(&filesystem.mount_point)));
        match (self.device, mount) {
            (Some(device), Some(mount)) => mounted_name(&mount, &clean(&device.name), width),
            (Some(device), None) => clean(&device.name),
            (None, Some(mount)) => format!("[{mount}]"),
            (None, None) => String::new(),
        }
    }
}

/// What tells a mount point apart (D-130): its last component, the name
/// the path ends in (`/mnt/DennySORA_SSD` → `DennySORA_SSD`); `/` itself.
fn mount_name(mount_point: &str) -> &str {
    mount_point
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(mount_point)
}

/// `[DennySORA_SSD] nvme0n1p3` (D-130); a mount's name too long for
/// `width` loses its middle, the device's name staying whole
/// (`[Den…_SSD] nvme0n1p3`).
fn mounted_name(mount: &str, device: &str, width: usize) -> String {
    let room = width.saturating_sub(display_width(device) + MOUNT_MARKS);
    if room == 0 {
        // The column cuts what it cannot hold.
        return format!("[{mount}] {device}");
    }
    format!("[{}] {device}", truncate_middle(mount, room))
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

fn render_table(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[DiskRow<'_>],
    charted: Option<&EntityId>,
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
                READ_COLUMN => palette.header_style().fg(palette.success),
                WRITE_COLUMN => palette.header_style().fg(palette.warning),
                _ => palette.header_style(),
            },
        );
    }
    let name_width = columns
        .iter()
        .find(|column| column.spec == DEVICE_COLUMN)
        .map_or(0, |column| usize::from(column.width));
    let capacity = usize::from(cursor.remaining());
    let overflow = rows.len() > capacity && capacity > 1;
    let shown = if overflow {
        capacity - 1
    } else {
        rows.len().min(capacity)
    };
    for disk_row in rows.iter().take(shown) {
        let Some(row) = cursor.next_row() else { break };
        let highlight = disk_row
            .device
            .is_some_and(|device| Some(&device.entity_id) == charted)
            .then(|| Style::new().bg(palette.highlight_background));
        draw_table_row(
            frame.buffer_mut(),
            row,
            &columns,
            &row_cells(disk_row, name_width, context),
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

fn row_cells(row: &DiskRow<'_>, name_width: usize, context: &DisplayContext) -> [TableCell; 11] {
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
        TableCell::new(row.name(name_width), name_style),
        fs_type,
        capacity(|filesystem| &filesystem.used, 8),
        capacity(|filesystem| &filesystem.free, 8),
        capacity(|filesystem| &filesystem.available, 8),
        row.filesystem
            .map_or_else(absent, |filesystem| use_percent_cell(filesystem, context)),
        rate(|device| &device.read_bps, 10),
        rate(|device| &device.write_bps, 10),
        row.device.map_or_else(absent, |device| {
            TableCell::new(iops_text(device), palette.text_style())
        }),
        rate(|device| &device.await_ms, 8),
        row.device
            .map_or_else(absent, |device| temperature_cell(device, context)),
    ]
}

/// The filesystem's used share in the level color of a gauge that full
/// (D-104, D-125), or the tone that says it was not read as it should be.
fn use_percent_cell(filesystem: &FilesystemView, context: &DisplayContext) -> TableCell {
    let cell = &filesystem.use_percent;
    let mut table_cell = metric_table_cell(cell, &context.theme, 6);
    table_cell.style = context.theme.percent_style(cell);
    table_cell
}

/// The drive's temperature in its level color (D-104), or blank when it
/// reports none: a drive without a sensor is not an error and never
/// reads 0.
fn temperature_cell(device: &DiskDeviceView, context: &DisplayContext) -> TableCell {
    let cell = &device.temperature;
    if cell.state.has_value() && cell.value.is_some() {
        let mut table_cell = metric_table_cell(cell, &context.theme, 5);
        table_cell.style = context.theme.temperature_style(cell);
        table_cell
    } else {
        TableCell::new("", context.theme.muted_style())
    }
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
    // The charted disk's rates are drawn even where it is no row.
    let unlisted_chart = disk.charted.iter().filter(|charted| {
        disk.devices
            .iter()
            .all(|device| device.entity_id != charted.entity_id)
    });
    for device in unlisted_chart.chain(disk.devices.iter().take(8)) {
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
        output.label(cell_label(
            &format!("disk {name} temperature"),
            &device.temperature,
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

    /// A name column wide enough for any fixture's name.
    const WIDE: usize = 40;

    fn header_and_root_row(width: u16) -> (String, String) {
        let model = test_fixtures::view_model(ViewKind::Overview, 1);
        let text = test_fixtures::render_text(&DiskModule::new().unwrap(), &model, (width, 12));
        let lines: Vec<&str> = text.lines().collect();
        (lines[1].to_owned(), lines[2].to_owned())
    }

    /// A wide panel shows the filesystem type, its free and available
    /// space, the device's await time and the drive's temperature beside
    /// the I/O rates.
    #[test]
    fn a_wide_panel_shows_type_free_space_await_and_temperature() {
        let (header, row) = header_and_root_row(130);
        for title in ["Type", "Free", "Avail", "Await", "Temp"] {
            assert!(header.contains(title), "{header}");
        }
        for value in ["ext4", "988 GiB", "900 GiB", "0.4 ms", "59°C"] {
            assert!(row.contains(value), "{value} missing in {row}");
        }
    }

    /// A drive's temperature is written in its level color (D-104).
    #[test]
    fn a_drive_temperature_takes_its_level_color() {
        let context = test_fixtures::display_context();
        let disk = disk_view();
        let rows = table_rows(&disk);
        let cell = &row_cells(&rows[0], WIDE, &context)[10];
        assert_eq!(cell.text, "59°C");
        assert_eq!(cell.style.fg, Some(context.theme.temperature_color(59.0)));
    }

    /// The used share is written in the level color of a gauge that full
    /// (D-104, D-125); one not read as it should be keeps its tone.
    #[test]
    fn the_use_percentage_takes_its_level_color() {
        let context = test_fixtures::display_context();
        let theme = &context.theme;
        let mut disk = disk_view();
        let cell = &row_cells(&table_rows(&disk)[0], WIDE, &context)[5];
        assert_eq!(cell.text, value_text(&disk.filesystems[0].use_percent));
        assert_eq!(cell.style.fg, Some(theme.level_color(0.451)));
        disk.filesystems[0].use_percent =
            test_fixtures::stale(dgxtop_core::metric::CanonicalUnit::Percent, 45.1, 9_000);
        let cell = &row_cells(&table_rows(&disk)[0], WIDE, &context)[5];
        assert_eq!(cell.style, theme.tone_style(crate::format::CellTone::Stale));
    }

    /// A drive without a temperature leaves the cell blank, never 0 or a
    /// state word; a row without a device marks it not applicable.
    #[test]
    fn an_unknown_drive_temperature_is_blank() {
        let context = test_fixtures::display_context();
        let mut disk = disk_view();
        disk.devices[0].temperature = MetricCell::missing(
            dgxtop_core::metric::CanonicalUnit::DegC,
            dgxtop_core::view::CellState::Unavailable,
        );
        let mut network = disk.filesystems[0].clone();
        network.mount_point = "/net".into();
        network.block_device = None;
        disk.filesystems.push(network);
        let rows = table_rows(&disk);
        assert_eq!(row_cells(&rows[0], WIDE, &context)[10].text, "");
        assert_eq!(row_cells(&rows[1], WIDE, &context)[10].text, NOT_APPLICABLE);
    }

    /// A narrow panel gives up the type, the free space and the await time
    /// first and keeps the capacity and the rates.
    #[test]
    fn a_narrow_panel_drops_the_detail_columns_first() {
        let (header, row) = header_and_root_row(70);
        for title in ["Used", "Avail", "Use%", "Read", "Write"] {
            assert!(header.contains(title), "{header}");
        }
        for title in ["Type", "Free", "Await", "Temp"] {
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
        assert_eq!(row_cells(&rows[0], WIDE, &context)[1].text, NOT_APPLICABLE);
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
        let names: Vec<String> = rows.iter().map(|row| row.name(WIDE)).collect();
        // nvme0n1 holds "/"; "/boot" names an unlisted device and "/net" none.
        assert_eq!(names, vec!["[/] nvme0n1", "[boot]", "[net]"]);
        assert_eq!(
            rows[0].filesystem.map(|fs| fs.mount_point.as_str()),
            Some("/")
        );
        assert!(rows[1].device.is_none() && rows[2].device.is_none());
    }

    fn draw(disk: DiskView) -> (ratatui::buffer::Buffer, RenderOutput) {
        let mut model = test_fixtures::view_model(ViewKind::Overview, 1);
        model.modules.disk = Some(std::sync::Arc::new(disk));
        let context = test_fixtures::display_context();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(130, 12)).unwrap();
        let mut output = None;
        terminal
            .draw(|frame| {
                let module = DiskModule::new().unwrap();
                output = Some(module.render(&context, &model, frame.area(), frame));
            })
            .unwrap();
        (terminal.backend().buffer().clone(), output.unwrap())
    }

    fn line_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn line_with(buffer: &ratatui::buffer::Buffer, text: &str) -> u16 {
        (0..buffer.area.height)
            .find(|&y| line_text(buffer, y).contains(text))
            .unwrap_or_else(|| panic!("no line with {text}"))
    }

    /// The title and the rate rows follow the charted whole disk (D-124),
    /// which need not be a row: the table lists the partition holding the
    /// filesystem, and only the charted device's own row is highlighted.
    #[test]
    fn the_charted_disk_names_the_panel_and_carries_the_rates() {
        let highlight = test_fixtures::display_context().theme.highlight_background;
        let listed = disk_view();
        let (buffer, _) = draw(listed.clone());
        let row = line_with(&buffer, "ext4");
        assert_eq!(buffer[(1, row)].bg, highlight, "the charted row");

        let mut partition_only = listed;
        let mut partition = partition_only.devices[0].clone();
        partition.entity_id = entity("disk:nvme0n1p2");
        partition.name = "nvme0n1p2".into();
        partition.read_bps =
            test_fixtures::ok(1024.0, dgxtop_core::metric::CanonicalUnit::BytesPerSecond);
        let row_read = value_text(&partition.read_bps);
        partition_only.devices = vec![partition];
        partition_only.filesystems[0].block_device = Some(entity("disk:nvme0n1p2"));
        let charted_read = value_text(&partition_only.charted.as_ref().unwrap().read_bps);
        let (buffer, output) = draw(partition_only);
        line_with(&buffer, "Disk I/O [nvme0n1]");
        let row = line_with(&buffer, "nvme0n1p2");
        assert_ne!(buffer[(1, row)].bg, highlight, "no row is the charted disk");
        let reads = line_text(&buffer, line_with(&buffer, "│R "));
        assert!(
            reads.contains(&charted_read) && !reads.contains(&row_read),
            "{reads}"
        );
        assert!(
            output
                .accessibility_labels
                .iter()
                .any(|label| label.starts_with("disk nvme0n1 read")),
            "{:?}",
            output.accessibility_labels
        );
    }

    /// A device's name follows the name of the mount point it holds, in
    /// brackets (D-130): the path's last component, `/` itself; a name too
    /// long loses its middle, never the device's; a column too narrow for
    /// even that cuts the end.
    #[test]
    fn a_device_is_named_after_the_mount_point_it_holds() {
        for (mount_point, name) in [
            ("/", "/"),
            ("/mnt/DennySORA_SSD", "DennySORA_SSD"),
            ("/boot/efi", "efi"),
            ("/mnt/data/", "data"),
        ] {
            assert_eq!(mount_name(mount_point), name, "{mount_point}");
        }
        assert_eq!(
            mounted_name("DennySORA_SSD", "nvme0n1p3", 40),
            "[DennySORA_SSD] nvme0n1p3"
        );
        let short = mounted_name("DennySORA_SSD", "nvme0n1p3", 20);
        assert_eq!(short, "[Den…_SSD] nvme0n1p3");
        assert_eq!(display_width(&short), 20);
        assert_eq!(mounted_name("data", "nvme0n1p3", 5), "[data] nvme0n1p3");
    }

    #[test]
    fn iops_show_both_directions_or_their_state() {
        let device = disk_view().devices[0].clone();
        // The fixture's write IOPS is still warming up.
        assert_eq!(iops_text(&device), "120/warmup");
    }
}
