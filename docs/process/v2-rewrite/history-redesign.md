# History view redesign (D-50)

The owner called the history view "completely wrong and ugly" and asked for a deep analysis
and a redesign modelled on good history charts. This note records the analysis, the
references and the resulting design. The decision itself is D-50 in
[DECISIONS.md](DECISIONS.md).

## What was wrong with the D-46/D-48 view

Measured on the owner's terminal (213×41) with the D-48 build:

1. **Charts too flat to read.** Eight charts shared 26 rows: each was 211 columns wide and
   2–4 rows high, one of them a caption. At that aspect ratio a slope is invisible and a busy
   hour looks like a stripe.
2. **No scale.** A chart had no axis labels: only the caption's average and peak gave a
   number. Disk and network charts scale to their own peak, so 1 KiB/s and 1 GiB/s looked the
   same.
3. **A weak time axis.** One row under the last chart said `-1h … -30m … now`; nothing tied a
   column to a time.
4. **Related series apart.** Read and write, receive and transmit were separate charts; the
   GPUs had a utilization chart each, while their memory, temperature and power over time
   appeared only as numbers in the table.
5. **Numbers that looked wrong.** History started with dgxtop (it lived in memory; the
   SQLite archive was off by default, until D-51). After a restart the 6 h, 12 h and 24 h rows of the GPU table
   repeated the 1 h numbers exactly, and "coverage 0.3%" did not explain why.
6. **Missing data looked like no data.** Before recording started a chart was simply empty,
   the same as an idle device.
7. **Noise.** Every caption repeated `avg … peak … coverage …` in the title colour.

## References and what was taken from them

| Source | Practice | Taken |
|---|---|---|
| Grafana time series panels and dashboard guidance | One question per panel; a legend table with last, mean and max; general before specific; consistent units and colours; one time range for the whole dashboard | Panels per resource; a legend with now, mean and max beside every strip; one window for all panels |
| Netdata charts | Related dimensions in one chart (received and sent together); the chart title names the device; the time range drives every chart | Each disk and interface pair shares one panel and one scale |
| btop | Download and upload graphs share one scale (`net_sync`); block or braille symbols; the scale's top labelled | Rate pairs share the larger peak; the gutter labels the scale; block glyphs (the owner rejected thin braille, D-41) |
| bottom (btm) | CPU, memory and network graphs over time on one layout; zoomable time | Panels in the overview's order; the windows `5m` to `24h` |
| s-tui | Graphs of one machine stacked on one time base, with the current and peak values beside them | Strips on a shared time axis; the legend beside the plot |
| Tufte, sparklines and small multiples; Few, information dashboards | Small multiples share scales and axes; label data directly; a sparkline needs nearby numbers; maximise data ink | One strip per GPU in each GPU panel; the scale in the gutter, the numbers in the legend; no frames inside a panel |
| Studies of multiple time series (Javed, McDonnel, Elmqvist) | Separate aligned panels beat overlaid lines over long spans | Strips rather than overlays (block glyphs cannot overlay) |

## Design

The view answers, for the chosen window, "how busy were the CPU, memory, GPUs, disk and
network, and when", in the overview's order, so the overview and the history read alike.

- **Window bar.** The windows `5m 1h 6h 12h 24h` (`w`/`W`); while the window is only partly
  recorded, `recorded 1h 12m of 6h · history starts with dgxtop`; what a short terminal leaves
  out.
- **Panels in pairs** (one column below 152 columns):
  - CPU utilization | Memory used (of the RAM total)
  - GPU utilization | GPU memory
  - GPU temperature | GPU power
  - Disk read and write | Network receive and transmit (the busiest device each, kept while it
    reports)
- **A strip per series.** Each GPU panel holds a strip per GPU; each rate panel holds the pair.
  Left to right a strip has:
  - a gutter: the scale's top and `0`, or the strip's label and scale;
  - the plot: each column the mean over its time in block glyphs, coloured by its share of the
    scale (rates in the overview's colours), its peak as a neutral band above it; a dotted base
    where nothing was recorded. D-57 dropped the peak band: at minute resolution the band
    turned short bursts into large grey blocks, and the legend keeps the window's peak;
  - a legend: now, mean and max over the window (and for GPU power the energy).
- **Scales.** Utilization 100 %; memory the RAM total or each GPU's framebuffer (on unified
  memory the RAM); temperature 100 °C; power each GPU's limit; a rate pair the larger peak of
  the two (since D-57 its tallest column).
- **Time axis.** Under each column of panels, ticks about six to a window (every minute of
  5 m, every 10 minutes of 1 h, every hour of 6 h, every 2 h of 12 h, every 4 h of 24 h),
  `-1h` to `now`. History lives on the boot clock, so a window reaching back before boot is
  answered from boot on and its axis spans the time since boot.
- **Long-term table.** Each GPU's last 1, 6, 12 and 24 h (mean / peak and energy) under the
  axis. A window that holds no more than the one before it is left out; a partly recorded
  window says how much it holds: `last 6h (1h 12m)`. The time held is the store's covered time,
  never the coverage ratio times the window: on a machine up five hours the 24 h window's ratio
  is of those five hours, and ratio × 24 h read `1h 10m` for 15 minutes of history (found on
  the live check).
- **Rows.** Every strip has the same height: one row each first, then up to three, then the
  table's whole GPUs, then up to eight; leftover rows go to the bands in order (the CPU and the
  memory first), each strip at most twice the others' height.

At 213×41 with two GPUs: four bands of panels, strips 3–4 rows high, the CPU and memory up to
6, the axis and the table.

## History kept on disk (D-51)

The view reads the time before this run from the history archive: earlier runs of this boot
and earlier boots, placed on the wall clock by the archive's UTC stamps (boots share no boot
clock). Memory answers this run. The archive's points end where this run starts; the window
bar says `recorded … · only while dgxtop runs`, because the gaps are the times dgxtop was not
running. Details in [DECISIONS.md](DECISIONS.md) D-51.

## Not done

- **Clock times on the axis.** The axis is relative (`-40m`). Local clock times need the
  local timezone, which no dependency in the lock file provides.
- **Overlaid lines.** Several series in one chart need braille or colour-per-cell lines; the
  owner rejected thin braille (D-41), and block glyphs cannot overlay.
- **The minute before a restart.** Since D-51 the history is kept on disk and read back
  across restarts and boots, but the last one or two minutes before dgxtop stops are not
  written (minutes are finalized a stale window after they end); a flush at shutdown would
  close that gap.
- **Zoom or a cursor.** The window keys are the only time control.
