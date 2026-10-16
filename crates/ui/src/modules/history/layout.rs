//! Rows and columns of a history page (D-50): the bar-size row, the panels
//! in bands (two side by side from [`HISTORY_TWO_COLUMN_WIDTH`]) and the
//! time axis under them. Every strip has the same height, so the bands read
//! alike. The panels come first at one row a strip, then the strips grow.
//! The rows left over go to the bands in order, the CPU and the memory
//! first, each strip up to twice the others' height, so no row stays blank.
//! Bands that do not fit wait below; the Overview History scrolls to them a
//! band at a time (D-107), from the band `first`. A run's charts go on
//! pages instead ([`paginate`], D-112), so every strip keeps its legend.

use std::ops::Range;

use dgxtop_core::view::{HISTORY_PANEL_GAP, HISTORY_TWO_COLUMN_WIDTH};

/// Rows of the bar-size row and of the time axis.
pub(super) const FRAME_ROWS: u16 = 2;
/// Rows a strip needs for the three lines of its legend: the last value,
/// the mean and the peak.
const LEGIBLE_STRIP_ROWS: u16 = 3;
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
    /// The bands scrolled past and the bands without room under the ones
    /// shown, and their panels.
    pub(super) bands_above: usize,
    pub(super) bands_below: usize,
    pub(super) panels_above: usize,
    pub(super) panels_below: usize,
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

/// Panels a band holds side by side in `width`: one or two.
fn columns_of(width: u16) -> u16 {
    if width >= HISTORY_TWO_COLUMN_WIDTH {
        2
    } else {
        1
    }
}

/// The panels of `strips` strips each in pages (D-112), as their
/// positions: all on one page when every strip has its legend's rows
/// there; else as few pages as give every strip those rows, the first
/// holding `first_rows` rows of bands and the others `rows`, the bands
/// shared out so the pages fill alike. A band too tall for any page is
/// alone on its own.
pub(super) fn paginate(
    width: u16,
    (first_rows, rows): (u16, u16),
    strips: &[u16],
) -> Vec<Range<usize>> {
    let columns = usize::from(columns_of(width));
    let heights: Vec<u32> = strips
        .chunks(columns)
        .map(|band| {
            let tallest = band.iter().map(|count| (*count).max(1)).max().unwrap_or(1);
            1 + u32::from(tallest) * u32::from(LEGIBLE_STRIP_ROWS)
        })
        .collect();
    let capacity = |page: usize| u32::from(if page == 0 { first_rows } else { rows }).max(1);
    let pages = balanced_pages(&heights, capacity);
    pages
        .into_iter()
        .map(|bands| bands.start * columns..(bands.end * columns).min(strips.len()))
        .collect()
}

/// `heights` in as few consecutive pages as `capacity` allows (greedy),
/// then cut again so the fullest page is as little full as can be.
fn balanced_pages(heights: &[u32], capacity: impl Fn(usize) -> u32) -> Vec<Range<usize>> {
    let total = heights.len();
    if total == 0 {
        return std::iter::once(0..0).collect();
    }
    let mut count = 1;
    let mut used = 0;
    for &height in heights {
        if used > 0 && used + height > capacity(count - 1) {
            count += 1;
            used = 0;
        }
        used += height;
    }
    let count = count.min(total);
    // fill[p][i]: the least "fullest page" (per mille of its capacity) of
    // the first `i` bands on `p` pages; cut[p][i]: where page `p` starts.
    let mut fill = vec![vec![u64::MAX; total + 1]; count + 1];
    let mut cut = vec![vec![0usize; total + 1]; count + 1];
    fill[0][0] = 0;
    for page in 1..=count {
        for end in page..=total {
            for start in (page - 1)..end {
                if fill[page - 1][start] == u64::MAX {
                    continue;
                }
                let load: u32 = heights[start..end].iter().sum();
                let share = u64::from(load) * 1000 / u64::from(capacity(page - 1));
                let fullest = fill[page - 1][start].max(share);
                if fullest < fill[page][end] {
                    fill[page][end] = fullest;
                    cut[page][end] = start;
                }
            }
        }
    }
    let mut pages = Vec::with_capacity(count);
    let mut end = total;
    for page in (1..=count).rev() {
        let start = cut[page][end];
        pages.push(start..end);
        end = start;
    }
    pages.reverse();
    pages
}

/// Rows `bands` take, each its title and strips of `strip_rows` rows.
fn rows_of(bands: &[u16], strip_rows: u16) -> u16 {
    bands.iter().fold(0u16, |rows, strips| {
        rows.saturating_add(1 + strips.saturating_mul(strip_rows))
    })
}

/// Lays out panels of `strips` strips each in `width` × `height`, from
/// the band `first` on (module docs). `first` stops at the band from which
/// every band after it shows.
pub(super) fn plan(width: u16, height: u16, strips: &[u16], first: usize) -> HistoryLayout {
    plan_folded(width, height, (strips, 0), first)
}

/// [`plan`] when the first `folded` panels are folded (D-114): they hold
/// no strips, only their title row, and sit in bands of their own, so the
/// panels after them pair up as if they were not there.
pub(super) fn plan_folded(
    width: u16,
    height: u16,
    (strips, folded): (&[u16], usize),
    first: usize,
) -> HistoryLayout {
    let columns = columns_of(width);
    let panel_width = if columns == 2 {
        (width - HISTORY_PANEL_GAP) / 2
    } else {
        width
    };
    let positions: Vec<usize> = (0..strips.len()).collect();
    let folded = folded.min(positions.len());
    let all_bands: Vec<Vec<usize>> = positions[..folded]
        .chunks(usize::from(columns))
        .chain(positions[folded..].chunks(usize::from(columns)))
        .map(<[usize]>::to_vec)
        .collect();
    let all_strips: Vec<u16> = all_bands
        .iter()
        .map(|band| {
            band.iter()
                .map(|&panel| {
                    if panel < folded {
                        0
                    } else {
                        strips[panel].max(1)
                    }
                })
                .max()
                .unwrap_or(0)
        })
        .collect();
    let body = height.saturating_sub(FRAME_ROWS);
    // Bands from `from` on that fit at one row a strip.
    let fitting = |from: usize| {
        let mut count = 0;
        while from + count < all_strips.len()
            && rows_of(&all_strips[from..=from + count], 1) <= body
        {
            count += 1;
        }
        count
    };
    let last_first = (0..all_bands.len())
        .find(|&from| from + fitting(from) == all_bands.len())
        .unwrap_or(0);
    let first = first.min(last_first);
    let shown = fitting(first);
    let visible = &all_strips[first..first + shown];
    let mut strip_rows = 1;
    while shown > 0 && strip_rows < MAX_STRIP_ROWS && rows_of(visible, strip_rows + 1) <= body {
        strip_rows += 1;
    }
    let mut left = body.saturating_sub(rows_of(visible, strip_rows));
    let band_extra: Vec<u16> = visible
        .iter()
        .map(|strips| {
            if *strips == 0 {
                return 0;
            }
            let extra = (left / strips).min(strip_rows);
            left -= extra * strips;
            extra
        })
        .collect();
    let panels_of = |bands: &[Vec<usize>]| bands.iter().map(Vec::len).sum();
    HistoryLayout {
        columns,
        panel_width,
        bands: all_bands[first..first + shown].to_vec(),
        band_strips: visible.to_vec(),
        strip_rows,
        band_extra,
        bands_above: first,
        bands_below: all_bands.len() - first - shown,
        panels_above: panels_of(&all_bands[..first]),
        panels_below: panels_of(&all_bands[first + shown..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture's panels: CPU, memory, four GPU panels of two GPUs, the
    /// disk and the network pairs.
    const PANELS: [u16; 8] = [1, 1, 2, 2, 2, 2, 2, 2];

    /// A run's panels go on pages giving every strip its legend's three
    /// rows (D-112): one page when they fit; else the fewest pages, the
    /// bands shared out so the pages fill alike, the first page shorter
    /// for the run's details above it.
    #[test]
    fn a_runs_panels_go_on_pages_that_fill_alike() {
        // A run's panels: CPU, memory, GPU utilization and memory, tasks,
        // disk and network, two a band on a wide terminal.
        const RUN: [u16; 7] = [2, 3, 1, 1, 3, 2, 2];
        // Bands: 3, 1, 3 and 2 strips: 10 + 4 + 10 + 7 rows legible.
        let all = paginate(211, (31, 36), &RUN);
        assert_eq!((all.len(), all[0].clone()), (1, 0..7), "31 rows: all fit");
        let pages = paginate(211, (28, 33), &RUN);
        assert_eq!(pages, vec![0..4, 4..7], "two pages of two bands each");
        // One a band: seven bands, 7 + 10 + 4 + 4 + 10 + 7 + 7 rows.
        let narrow = paginate(100, (28, 33), &RUN);
        assert!(narrow.len() >= 2);
        let flat: Vec<usize> = narrow.iter().flat_map(Clone::clone).collect();
        assert_eq!(
            flat,
            (0..7).collect::<Vec<_>>(),
            "every panel once, in order"
        );
        // A band taller than any page is alone on its own.
        assert_eq!(paginate(100, (5, 5), &[1, 9, 1]), vec![0..1, 1..2, 2..3]);
        let none = paginate(100, (10, 10), &[]);
        assert_eq!((none.len(), none[0].clone()), (1, 0..0));
    }

    #[test]
    fn a_short_terminal_scrolls_a_band_at_a_time() {
        // 100×30: one panel a band, 24 rows for the bands: all at one row
        // a strip; the CPU and the memory take the two rows left.
        let layout = plan(98, 26, &PANELS, 0);
        assert_eq!((layout.columns, layout.bands.len()), (1, 8));
        assert_eq!((layout.rows_of_strip(0), layout.rows_of_strip(2)), (2, 1));
        // 80×24: 18 rows; the disk and the network wait below.
        let layout = plan(78, 20, &PANELS, 0);
        assert_eq!(layout.bands.len(), 6);
        assert_eq!((layout.bands_above, layout.bands_below), (0, 2));
        assert_eq!((layout.panels_above, layout.panels_below), (0, 2));
        let rows: u16 = (0..6).map(|band| layout.band_height(band)).sum();
        assert_eq!(rows + 2, 20, "every row is used");
        // A band down: the CPU scrolls out, the disk in.
        let layout = plan(78, 20, &PANELS, 1);
        assert_eq!(layout.bands.first(), Some(&vec![1]));
        assert_eq!((layout.panels_above, layout.panels_below), (1, 1));
        // Scrolling stops once the network shows.
        let last = plan(78, 20, &PANELS, 2);
        assert_eq!((last.bands_above, last.bands_below), (2, 0));
        assert_eq!((last.panels_above, last.panels_below), (2, 0));
        // Two panels a band: bands and panels count apart.
        let wide = plan(200, 9, &PANELS, 1);
        assert_eq!(wide.columns, 2);
        assert_eq!((wide.bands_above, wide.bands_below), (1, 1));
        assert_eq!((wide.panels_above, wide.panels_below), (2, 2));
        assert_eq!(plan(78, 20, &PANELS, 9), last);
        // Too short for any band; no panels at all.
        let layout = plan(78, 3, &PANELS, 0);
        assert!(layout.bands.is_empty());
        assert_eq!(layout.panels_below, 8);
        assert!(plan(211, 37, &[], 0).bands.is_empty());
    }
}
