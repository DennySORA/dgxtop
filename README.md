# dgxtop

[繁體中文](docs/README.zh-TW.md) | [简体中文](docs/README.zh-CN.md) | [日本語](docs/README.ja.md)

> A stable, efficient local terminal monitor for NVIDIA DGX and GPU Linux hosts: CPU, GPU,
> memory, disk, network and processes — with explainable multi-source arbitration, exact
> history and protected process control.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92.0-orange.svg)](rust-toolchain.toml)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)

dgxtop v2 is a complete rewrite built from the specification in [`spec/dgxtop-v2/`](spec/dgxtop-v2/README.md).
It never shows a fake zero: a value that could not be read is displayed as *unsupported*,
*read failed*, *stale* or *source conflict*, together with where it came from.

## Highlights

- **Isolated, non-blocking collection.** Every live data source (procfs, hwmon, thermal,
  powercap, NVML, network, filesystem) runs in its own resident adapter-host process. A hung
  driver call (for example a stuck NVML) only affects that host; CPU, memory and the UI keep
  updating. Timed-out hosts are restarted with bounded backoff or quarantined — never respawned
  without limit.
- **Explainable source arbitration.** When two sources report the same metric (e.g. coretemp
  and the thermal zone for the CPU package), dgxtop de-duplicates proven aliases, compares only
  independent sources, and shows a *source conflict* instead of averaging or silently picking
  one. Every candidate, why it was selected or rejected, its age and origin are kept for the
  source diagnostics view (its tab is hidden for now).
- **Honest units and quality.** Read status, sample kind (measured / derived / estimated /
  simulated), quality and freshness are separate axes. Unproven units (such as NVML PCIe "KB/s")
  are not converted by guesswork, and no bandwidth is invented from utilization percentages.
- **Exact history.** Rates come from exact `u64` counter deltas over the real elapsed time;
  totals over a window count only intervals fully inside it (cross-minute intervals are kept as
  exact *bridges*). Gaps, coverage and source changes are visible.
- **Bounded resources.** One global managed-memory budget (256 MiB by default) with byte and item
  limits on every queue; a fixed set of threads; dirty-only rendering at most 4 fps; non-blocking
  terminal output that never stalls input when the terminal is slow.
- **Protected process control.** `K` asks for a SIGTERM to the selected process: same user only,
  never root sessions, never PID 1 / dgxtop itself / its hosts, a one-time 10-second confirmation,
  a durable audit record *before* the signal, and the signal is sent through a held `pidfd`
  (no PID reuse races). `SIGKILL` is disabled unless explicitly enabled. Results distinguish
  *signal sent* from *exit observed*.

## Views and modules

Ten display modules: `cpu`, `gpu`, `memory`, `disk`, `network`, `processes`, `history`,
`sources`, `settings` and `actions`. The tabs are Overview, GPU, History and Settings. A
pending process action shows as a banner in every view, and its outcome (sent, still running,
exited, unknown) as a notice.

- **Overview.** Four rows, top to bottom:
  - **CPU and memory**, side by side, memory the wider. CPU shows its temperature, power and
    maximum frequency (with load and tasks when they fit), its utilization bar with a chart
    under it, and the usr/sys/iowait/steal split. Memory shows the RAM in use, the used bar with
    a chart under it, swap, buffers, cache and available memory. Click the CPU panel or press
    `c` to add every sensor and the per-core bars. On unified-memory GPUs (DGX Spark) memory
    draws the RAM that GPU processes hold in its own color and says how much it is.
  - **GPUs.** Each GPU gets a line with its temperature, power, clock and fan, its utilization
    bar with a chart under it, then its memory bar: `VRAM`, or on unified memory `RAM`, the RAM
    the GPU's processes hold. The GPUs grow first when the terminal is short.
  - **Disk I/O and network**, side by side. Each is a table (disks with the capacity of the
    filesystem they hold), then the read/write or receive/transmit rates of the busiest device,
    which the panel title names.
  - **GPU processes**, which take the remaining rows. The table lists only processes that use
    a GPU, with `TYPE` saying whether each holds a compute (`C`) or graphics (`G`) context on
    it, or both (`C+G`), as `nvidia-smi` prints it. All process actions (select, sort, filter,
    terminate) happen there.

  Charts are built for a quick read: block columns, one per sample with the newest at the
  right, each in the bar's color for its level, so busy stretches show red at a glance.
  Utilization and memory keep a fixed scale. Disk and network rates are scaled to their own
  peak, which is printed next to them.
- **GPU.** One device at a time: four live bars (utilization, memory, temperature, power) over
  all of its live and deep information, read from the NVIDIA driver (NVML) and from driver and
  CUDA toolkit files, without network access and without running any command. The information
  reads top to bottom in blocks of `key value` rows, so values you compare sit one above the
  other, in as few balanced columns as fit the terminal:
  - **State:** performance state, throttle reasons, and how long each cause held the clocks back.
  - **Utilization:** memory controller, encoder and decoder.
  - **Clocks:** graphics, SM, memory and video, each now beside its maximum.
  - **Memory:** total, used, free, reserved and process memory, BAR1, bus width and theoretical
    peak bandwidth.
  - **PCIe:** the link and its theoretical bandwidth now beside their maximum, transmit over
    receive, replays.
  - **Thermal:** temperature and fan (every fan on a GPU with several), then the device's own
    thresholds in rising order.
  - **Power:** draw and the driver's instantaneous reading (on Ampere and newer GPUs, draw is
    NVML's one-second average), limit, the requested limit when another limiter enforces a
    lower one, default limit, the range it can be set within, and the energy since the driver
    loaded.
  - **CUDA:** the CUDA version the driver supports, the compute capability (`sm_120`), the
    installed toolkit, its path and its `nvcc`.
  - **Driver:** version, open or proprietary kernel module, NVML, persistence, compute mode.
  - **Device, PCI & firmware:** architecture, brand, CUDA cores, board part, serial, UUID, device
    node; PCI identity, VBIOS, InfoROM and GSP firmware.
  - **Health:** ECC, retired-page and NVLink counters, when the GPU has them.
  - **Topology:** the NUMA node and CPUs nearest the GPU, and for every other GPU the PCIe path
    (`nvidia-smi topo -m`'s `PIX` to `SYS`) and whether it can read and write that GPU's memory
    directly (P2P). Without P2P, copies between the GPUs go through system memory.

  What the GPU does not report gets no row. A terminal too short for every block names the
  blocks it leaves out. The processes using each GPU are in the overview's
  process table (`g` limits it to one GPU).
- **History.** The overview's resources over a longer window (5 minutes to 24 hours, `w`/`W`),
  laid out like a monitoring dashboard: panels in pairs, CPU and memory, the GPUs' utilization
  and memory, their temperature and power (a strip per GPU), and the read/write and
  receive/transmit rates of the busiest disk and interface (each pair on one scale, its tallest
  column). Every strip shows its scale, block columns of the window (the mean coloured by level,
  a dotted base where nothing was recorded) and a legend with the value now, the mean and the
  peak of the window. A time axis runs under each column of panels. The history is kept on disk
  (`history.sqlite3`, on by default): earlier runs and boots are read back and charted on the
  same axis, and the window bar says how much of a window is recorded (only while dgxtop runs).
  Under the axis, each GPU's last 1, 6, 12
  and 24 hours: mean and peak utilization, memory, temperature and power, and the energy drawn,
  listing only the windows that add data. The analysis behind the design is in
  [docs/process/v2-rewrite/history-redesign.md](docs/process/v2-rewrite/history-redesign.md).

Layout adapts to the terminal:

- full layout from 80×24;
- a compact summary below 80×24;
- a size notice below 40×10 (quit still works).

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

The installer downloads the GNU build for x86_64 or aarch64 into a temporary directory on the
target filesystem, verifies `SHA256SUMS` and the archive layout, runs `dgxtop --version
--verbose`, then atomically replaces the binary and keeps the previous one as
`dgxtop.previous`. Roll back with `./install.sh --rollback` (configuration and state are not
touched; signals already sent cannot be undone). There is no automatic musl fallback.

Build from source (Rust 1.92.0 is pinned by `rust-toolchain.toml`):

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release --locked
./target/release/dgxtop
```

## Usage

```bash
dgxtop                       # live monitor
dgxtop -i 0.5                # 500 ms sampling for this run (saved settings are not changed)
dgxtop --no-gpu              # do not start the NVML host
dgxtop --read-only           # disable all process actions
dgxtop --replay fixture.json # simulated data only, read-only, separate temporary state
dgxtop --check-config        # validate and print configured/effective settings, then exit
dgxtop --diagnostics-json    # versions, SQLite, capabilities and health as JSON
dgxtop --import-legacy ~/.config/dgxtop/config.json --dry-run   # show the v1 → v2 mapping
dgxtop --version --verbose   # version, commit, toolchain, target, SQLite runtime
```

Command-line values override the saved configuration only when they are given explicitly and
only for the current run.

### Keys

| Key | Action |
|---|---|
| `q`, `Ctrl-C` | Quit (`q` is text while editing a filter; `Ctrl-C` always quits) |
| `1`–`4`, `Tab`, `Shift-Tab` | Overview, GPU, History, Settings |
| `w` / `W` | Next / previous history window (5m, 1h, 6h, 12h, 24h) |
| `c` | Show / hide the CPU sensors and per-core bars in the overview (or click the CPU panel) |
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move the selection (by process identity, not row index) |
| `h`/`l` | Previous/next GPU in the GPU view |
| `g` | Cycle the process GPU filter (all GPUs → each GPU) |
| `/` | Filter processes (Enter applies, Esc clears) |
| `s` / `r` | Sort column / reverse order |
| `+` / `-` | Change the sampling interval (saved after confirmation by the store) |
| `K` | Request SIGTERM for the selected process (confirmation required) |
| `y`/`Enter`, `n`/`Esc` | Confirm / cancel a pending process action |
| `F2`, `?`/`F1` | Settings view, help |

## Configuration and files

| Purpose | Location |
|---|---|
| Configuration (schema 2, versioned, atomic writes) | `$XDG_CONFIG_HOME/dgxtop/v2/config.json` (`~/.config/dgxtop/v2/config.json`) |
| Action audit (SQLite, `synchronous=FULL`) | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3` (`~/.local/state/...`) |
| History archive (on by default, 7 days, at most 1 GiB) | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3` |
| Bounded log, instance lock | `$XDG_STATE_HOME/dgxtop/v2/` |

Directories are created `0700` and files `0600`. Full command lines, environments and tokens are
never persisted. The complete configuration contract is in
[`spec/dgxtop-v2/contracts/CONFIG.md`](spec/dgxtop-v2/contracts/CONFIG.md) and the defaults in
[`spec/dgxtop-v2/examples/config.default.json`](spec/dgxtop-v2/examples/config.default.json).

Upgrading from v1: the old `~/.config/dgxtop/config.json` is never modified. Review the mapping
with `--import-legacy <path> --dry-run`, then write the v2 file with `--apply`.

## Architecture

```text
data:    driver/OS → adapter host (Adapter) → Collector → Manager → Store
         Store → Manager projection → Viewer
control: input → Controller → Manager → config / scheduler / pidfd action adapter
         → audit → Store → Viewer
```

| Crate | Role |
|---|---|
| `dgxtop-core` | Typed identities, units, quality axes, commands, events, configuration, byte budget, ports |
| `dgxtop-runtime` | Adapter-host supervision and IPC framing, bounded workers, non-blocking terminal output |
| `dgxtop-adapters` | Read-only adapters (procfs, hwmon, thermal, powercap, NVML, network, filesystem, replay) and the separate pidfd action adapter |
| `dgxtop-collectors` | Normalization, origin de-duplication, arbitration, counter rates, process/GPU join |
| `dgxtop-store` | Hot snapshots, bounded exact history, config repository, SQLite audit and archive |
| `dgxtop-manager` | Scheduling, catalog, policy, ingestion, history, configuration, process actions, presentation, sessions |
| `dgxtop-ui` | Display modules, layout, keymap, controller, terminal runner |
| `dgxtop` | Composition root and CLI |

Only the composition root depends on more than `dgxtop-core`; the boundary is checked by
[`spec/dgxtop-v2/tools/check_boundaries.py`](spec/dgxtop-v2/tools/check_boundaries.py).

## Development

```bash
just ci        # fmt, check, clippy -D warnings, tests, dependency boundaries (all --locked)
just e2e       # end-to-end suites on request: PTY sessions, fault injection, real host processes
just hardware  # hardware suites on real procfs/sysfs/NVML (and pidfd on disposable children)
```

## Validation status

See [`docs/validation/VALIDATION.md`](docs/validation/VALIDATION.md) for every acceptance case
(AT01–AT40) with its command, exit code, environment and evidence hash. Performance numbers in
the specification are acceptance targets; a platform is only called validated when that report
lists measured evidence for it.

## License

Apache-2.0 — see [LICENSE](LICENSE).
