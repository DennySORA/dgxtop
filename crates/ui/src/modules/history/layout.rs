//! Rows and columns of the history view (D-50): the window bar, the panels
//! in bands (two side by side from [`HISTORY_TWO_COLUMN_WIDTH`]), the time
//! axis under them, then the GPUs' long-term table and the GPU processes'
//! table (D-64). Every strip has the same height, so the bands read alike.
//! The panels come first at one row a strip (the bands that do not fit are
//! named in the window bar), then grow to three rows; then the long-term
//! table takes whole GPUs and the process table as many processes as fit;
//! then the strips grow further. The rows left over go to the bands in
//! order, the CPU and the memory first, each strip up to twice the others'
//! height, so no row stays blank.

use dgxtop_core::view::{HISTORY_PANEL_GAP, HISTORY_TWO_COLUMN_WIDTH};

/// Rows of the window bar and of the time axis.
const FRAME_ROWS: u16 = 2;
/// Strip height the panels reach before the table.
const COMFORTABLE_STRIP_ROWS: u16 = 3;
/// Tallest strip.
const MAX_STRIP_ROWS: u16 = 8;

/// Where everything goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HistoryLayout {
    /// Panels a band holds side by side: one or two.
    pub(super) columns: u16,
    pub(super) panel_width: u16,
    /// The bands shown, each its panels' positions.
    pub(super) bands: Vec<Vec<usize>>,
    /// Strips each band holds (its panels' most).
    band_strips: Vec<u16>,
    /// Rows of a strip, and the rows each band's strips add to it.
    pub(super) strip_rows: u16,
    band_extra: Vec<u16>,
    /// GPUs of the long-term table shown, and the rows it takes with its
    /// header.
    pub(super) table_blocks: usize,
    pub(super) table_rows: u16,
    /// Rows of the process table with its header (none, or the header and
    /// at least one process).
    pub(super) process_rows: u16,
    /// Panels without room.
    pub(super) hidden_panels: usize,
}

impl HistoryLayout {
    /// Rows of a strip of `band`.
    pub(super) fn rows_of_strip(&self, band: usize) -> u16 {
        self.strip_rows + self.band_extra.get(band).copied().unwrap_or(0)
    }

    /// Rows `band` takes: its titles and its strips.
    pub(super) fn band_height(&self, band: usize) -> u16 {
        let strips = self.band_strips.get(band).copied().unwrap_or(0);
        1 + strips * self.rows_of_strip(band)
    }

    /// Left edge of the panel in `column`, from the view's left edge.
    pub(super) fn panel_offset(&self, column: u16) -> u16 {
        column * (self.panel_width + HISTORY_PANEL_GAP)
    }
}

/// Lays out panels of `strips` strips each, a table of GPU blocks of
/// `table_blocks` rows each and a process table of `process_rows` rows with
/// its header in `width` × `height` (module docs).
pub(super) fn plan(
    width: u16,
    height: u16,
    strips: &[u16],
    (table_blocks, process_rows): (&[u16], u16),
) -> HistoryLayout {
    let columns: u16 = if width >= HISTORY_TWO_COLUMN_WIDTH {
        2
    } else {
        1
    };
    let panel_width = if columns == 2 {
        (width - HISTORY_PANEL_GAP) / 2
    } else {
        width
    };
    let positions: Vec<usize> = (0..strips.len()).collect();
    let all_bands: Vec<Vec<usize>> = positions
        .chunks(usize::from(columns))
        .map(<[usize]>::to_vec)
        .collect();
    let all_strips: Vec<u16> = all_bands
        .iter()
        .map(|band| {
            band.iter()
                .map(|&panel| strips[panel].max(1))
                .max()
                .unwrap_or(1)
        })
        .collect();
    let body = height.saturating_sub(FRAME_ROWS);
    let rows_at = |count: usize, strip_rows: u16| -> u16 {
        all_strips[..count].iter().fold(0u16, |rows, strips| {
            rows.saturating_add(1 + strips.saturating_mul(strip_rows))
        })
    };
    let mut shown = all_bands.len();
    while shown > 0 && rows_at(shown, 1) > body {
        shown -= 1;
    }
    let grows = |strip_rows: u16, limit: u16, table_rows: u16| {
        shown > 0
            && strip_rows < limit
            && rows_at(shown, strip_rows + 1).saturating_add(table_rows) <= body
    };
    let mut strip_rows = 1;
    while grows(strip_rows, COMFORTABLE_STRIP_ROWS, 0) {
        strip_rows += 1;
    }
    let every_band = shown == all_bands.len() && shown > 0;
    let (table_blocks, table_rows) = if every_band {
        fit_table(
            body.saturating_sub(rows_at(shown, strip_rows)),
            table_blocks,
        )
    } else {
        (0, 0)
    };
    let process_rows = if every_band {
        fit_processes(
            body.saturating_sub(rows_at(shown, strip_rows))
                .saturating_sub(table_rows),
            process_rows,
        )
    } else {
        0
    };
    let tables = table_rows.saturating_add(process_rows);
    while grows(strip_rows, MAX_STRIP_ROWS, tables) {
        strip_rows += 1;
    }
    let mut left = body.saturating_sub(rows_at(shown, strip_rows).saturating_add(tables));
    let band_extra: Vec<u16> = all_strips[..shown]
        .iter()
        .map(|strips| {
            let extra = (left / strips).min(strip_rows);
            left -= extra * strips;
            extra
        })
        .collect();
    HistoryLayout {
        columns,
        panel_width,
        bands: all_bands[..shown].to_vec(),
        band_strips: all_strips[..shown].to_vec(),
        strip_rows,
        band_extra,
        table_blocks,
        table_rows,
        process_rows,
        hidden_panels: all_bands[shown..].iter().map(Vec::len).sum(),
    }
}

/// Rows of a process table of `wanted` rows with its header that fit
/// `rows`: none unless the header and one process fit.
fn fit_processes(rows: u16, wanted: u16) -> u16 {
    if wanted < 2 || rows < 2 {
        0
    } else {
        wanted.min(rows)
    }
}

/// Whole GPU blocks of the table that fit `rows`, with the header.
fn fit_table(rows: u16, blocks: &[u16]) -> (usize, u16) {
    let mut taken = 0;
    let mut used = 0u16;
    for block in blocks {
        let needed = block + u16::from(taken == 0);
        if used + needed > rows {
            break;
        }
        used += needed;
        taken += 1;
    }
    (taken, used)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture's panels: CPU, memory, four GPU panels of two GPUs, the
    /// disk and the network pairs.
    const PANELS: [u16; 8] = [1, 1, 2, 2, 2, 2, 2, 2];

    #[test]
    fn a_short_terminal_keeps_whole_bands_in_order_and_names_the_rest() {
        // 100×30: one panel a band, 26 rows inside: every band at one row a
        // strip, and the table's first GPU in the two rows left.
        let layout = plan(98, 26, &PANELS, (&[1, 1], 0));
        assert_eq!(layout.columns, 1);
        assert_eq!(layout.bands.len(), 8);
        assert_eq!((layout.strip_rows, layout.table_blocks), (1, 1));
        // 80×24: 20 rows inside; the disk and the network do not fit, so
        // the table waits too; the CPU and the memory take a second row
        // each, so no row stays blank.
        let layout = plan(78, 20, &PANELS, (&[1, 1], 0));
        assert_eq!(layout.bands.len(), 6);
        assert_eq!((layout.hidden_panels, layout.table_blocks), (2, 0));
        assert_eq!((layout.rows_of_strip(0), layout.rows_of_strip(1)), (2, 2));
        let charts: u16 = (0..6).map(|band| layout.band_height(band)).sum();
        assert_eq!(charts + 2, 20);
        // Too short for any band.
        let layout = plan(78, 3, &PANELS, (&[1], 0));
        assert!(layout.bands.is_empty());
        assert_eq!(layout.hidden_panels, 8);
        assert_eq!(layout.rows_of_strip(0), 1);
        // No panels at all.
        assert!(plan(211, 37, &[], (&[1], 0)).bands.is_empty());
    }

    /// The process table comes after the long-term table, before the
    /// strips grow beyond three rows, with as many processes as fit; it
    /// needs room for its header and one process.
    #[test]
    fn the_process_table_follows_the_long_term_table() {
        // 213×41 on a DGX: one GPU, so one strip in every GPU panel.
        let panels = [1, 1, 1, 1, 1, 1, 2, 2];
        let layout = plan(211, 37, &panels, (&[4], 4));
        assert_eq!((layout.table_rows, layout.process_rows), (5, 4));
        assert!(layout.strip_rows >= 3);
        let charts: u16 = (0..layout.bands.len())
            .map(|band| layout.band_height(band))
            .sum();
        assert_eq!(charts + 2 + 5 + 4, 37, "every row is used");
        // Only the header and one process fit after the long-term table.
        let tight = plan(98, 26, &PANELS, (&[1, 1], 5));
        assert_eq!(tight.process_rows, 0, "no room once the strips have a row");
        assert_eq!(fit_processes(1, 5), 0);
        assert_eq!(fit_processes(3, 5), 3);
        assert_eq!(fit_processes(9, 5), 5);
        assert_eq!(fit_processes(9, 0), 0);
    }
}
