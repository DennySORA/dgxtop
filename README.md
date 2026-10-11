<div align="center">

# dgxtop

**An honest terminal monitor for NVIDIA DGX Spark and GPU Linux hosts**

CPU · GPU · memory · disk · network · GPU processes<br>
isolated collection · explainable sources · exact history · protected process control

[繁體中文](docs/README.zh-TW.md) · [日本語](docs/README.ja.md) · [Design notes](https://dennysora.me/en/projects/tools/dgxtop/)

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92.0-orange.svg)](rust-toolchain.toml)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)

<img src="docs/images/screenshots/dgx-spark-overview.png" alt="dgxtop overview on a DGX Spark: CPU and memory, the GB10 GPU at 92 % utilization with its RAM bar, the disk and network tables, and the GPU process table" width="100%">

<sub>Overview on a DGX Spark. The GB10 shares memory with the CPU, so the RAM its processes hold is drawn inside system memory.</sub>

</div>

dgxtop never shows a fake zero. A value it cannot read is shown as **unsupported**, **read failed**,
**stale** or **source conflict**, together with where it came from.

## Views

### Overview

CPU and memory, every GPU with its utilization and memory, the busiest disk and network interface,
and the processes that use a GPU. Busy stretches turn red at a glance; `c` adds every CPU sensor and
per-core bars.

<img src="docs/images/screenshots/workstation-overview.png" alt="dgxtop overview on a workstation with two GPUs, an RTX 5080 and an RTX 3060, with the processes holding GPU memory" width="100%">

### GPU

One device at a time: four live bars, then everything the driver reports, read from NVML and the
CUDA toolkit files without running a command. A value the GPU does not report gets no row.

<img src="docs/images/screenshots/dgx-spark-gpu.png" alt="GPU view of the DGX Spark's GB10: state, clocks, unified memory, PCIe, NVLink-C2C, thermal thresholds, power, CUDA 13.0, driver, device and topology" width="100%">

<img src="docs/images/screenshots/workstation-gpu.png" alt="GPU view of an RTX 5080: state, clocks, VRAM and BAR1, PCIe Gen5, fans and thresholds, power limits, CUDA, driver, device, firmware and the P2P path to the second GPU" width="100%">

<sub>GB10 with unified memory and NVLink-C2C (top); RTX 5080 with VRAM and PCIe Gen5 (bottom). Serial numbers and UUIDs are masked.</sub>

### History

The overview's resources over 5 minutes to 24 hours (`w` / `W`), in paired panels on one time axis.
Each strip shows its scale, column means colored by level, and now / avg / max. Below the axis,
each GPU's last 1, 6, 12 and 24 hours. History is kept in `history.sqlite3`, so earlier runs are
charted too.

<img src="docs/images/screenshots/dgx-spark-history.png" alt="History view of the DGX Spark over one hour: CPU, memory, GPU utilization, GPU memory, temperature, power with energy, disk and network" width="100%">

<img src="docs/images/screenshots/workstation-history.png" alt="History view of the two-GPU workstation with each GPU's mean and peak over the last 1, 6, 12 and 24 hours" width="100%">

### Settings

Every setting with its saved and effective value, its origin and whether it applies live or needs a
restart. Display modules can be switched on and off.

<img src="docs/images/screenshots/dgx-spark-settings.png" alt="Settings view listing each setting's saved value, effective value, origin and restart requirement, next to the display module toggles" width="100%">

## Why dgxtop

- **Never a fake zero.** Read status, sample kind (measured, derived, estimated, simulated),
  quality and freshness are separate axes. Unproven units are not converted by guesswork, and no
  bandwidth is invented from a utilization percentage.
- **A hung driver never freezes the screen.** Every live source runs in its own resident host
  process. A stuck NVML call stalls that host only; CPU, memory and the UI keep updating.
- **Explainable sources.** Proven aliases of one sensor count once. Independent sources that
  disagree show a source conflict instead of an average.
- **Exact history.** Rates come from exact `u64` counter deltas over the real elapsed time, and gaps
  stay visible.
- **Bounded resources.** One 256 MiB budget for all managed data, byte and item limits on every
  queue, a fixed set of threads, and redraws only on change at no more than 4 fps.
- **Protected process control.** `K` asks for a SIGTERM to the selected GPU process: same user
  only, a one-time 10-second confirmation, an audit record written *before* the signal, and the
  signal sent through a held `pidfd`.

## Architecture

### Six components, two paths, one writer

<img src="docs/images/architecture/architecture-overview.png" alt="Architecture: Linux kernel and NVIDIA driver feed seven adapter hosts, then the Collector, the Manager, the Store and the Viewer on the data path; the Controller and the process action adapter sit on the control path" width="100%">

Data flows from the kernel and the driver through isolated adapter hosts to the Collector, which
normalizes and arbitrates, and to the Manager, the only writer to the Store. The Viewer draws
immutable view models; the Controller only turns keys into commands.

### Isolated, non-blocking collection

<img src="docs/images/architecture/runtime-isolation.png" alt="Runtime: one parent process with a fixed set of threads talks over anonymous pipes to seven resident adapter hosts; a hung NVML host goes through degraded, circuit open, backoff, probe and warmup" width="100%">

A timed-out host backs off (1 s, 2 s, 4 s … 60 s) and is restarted only after it is reaped. A host
that cannot be reaped is quarantined, never respawned without limit.

### Explainable source arbitration

<img src="docs/images/architecture/source-arbitration.png" alt="Arbitration: hwmon reads 68 °C and a thermal zone reads 42 °C; nine steps lead to selected, source conflict, unsupported, read failed or stale, never the 55 °C average" width="100%">

A switch to another source needs three consecutive wins and at least five seconds, so the value
does not flap between sensors.

### Protected process termination

<img src="docs/images/architecture/protected-kill.png" alt="Process termination: K, prepare with same-user and safety checks, pidfd_open, a 10-second confirmation, a durable audit intent, then pidfd_send_signal SIGTERM, then signal sent and either exit observed or still running" width="100%">

The result says *signal sent* separately from *exit observed*. `SIGKILL` stays off unless
`actions.allow_force` is enabled, and `--read-only` disables every action.

### Exact history

<img src="docs/images/architecture/exact-history.png" alt="History: counter deltas over real elapsed time, minute buckets with an exact bridge across a minute boundary, a visible gap, the 5m to 24h windows and the history.sqlite3 archive" width="100%">

Totals count only intervals that lie fully inside a window; an interval that crosses a minute
boundary is kept as an exact bridge instead of being split by guesswork.

### Workspace boundaries

<img src="docs/images/architecture/crate-boundaries.png" alt="Crate graph: dgxtop-runtime, adapters, collectors, store, manager and ui each depend only on dgxtop-core; the dgxtop composition root depends on all of them" width="100%">

<details>
<summary>What each crate owns</summary>

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

[`spec/dgxtop-v2/tools/check_boundaries.py`](spec/dgxtop-v2/tools/check_boundaries.py) checks the
boundary in CI.

</details>

## Install

Linux on x86_64 or aarch64 with glibc 2.34 or newer (Ubuntu 22.04, Debian 12, RHEL 9 and later),
DGX Spark included. WSL 2 runs the Linux build: GPUs need the NVIDIA driver on Windows, and CPU
temperature and power are usually not available there. WSL 1, musl, macOS and Windows are not
supported. GPU metrics need the NVIDIA driver (NVML); without it, everything else still works.

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

`install.sh` picks the build for your CPU, verifies `SHA256SUMS` and the archive layout, runs
`dgxtop --version --verbose`, then atomically replaces `~/.local/bin/dgxtop` and keeps the previous
binary as `dgxtop.previous` (`./install.sh --rollback` restores it). For a specific release, append
`-s -- --version v2.0.0` to `bash`. Releases before v2.0.0 are the earlier v1 program.

Or build from source (Rust 1.92.0 is pinned by `rust-toolchain.toml`):

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

| Key | Action |
|---|---|
| `q`, `Ctrl-C` | Quit (`q` is text while editing a filter; `Ctrl-C` always quits) |
| `1`–`4`, `Tab`, `Shift-Tab` | Overview, GPU, History, Settings |
| `w` / `W` | Next / previous history window (5m, 1h, 6h, 12h, 24h) |
| `c` | Show / hide the CPU sensors and per-core bars (or click the CPU panel) |
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move the selection (by process identity, not row index) |
| `h`/`l` | Previous / next GPU in the GPU view |
| `g` | Cycle the process GPU filter (all GPUs → each GPU) |
| `/` | Filter processes (Enter applies, Esc clears) |
| `s` / `r` | Sort column / reverse order |
| `+` / `-` | Change the sampling interval (saved once the store confirms it) |
| `K` | Request SIGTERM for the selected process (confirmation required) |
| `y`/`Enter`, `n`/`Esc` | Confirm / cancel a pending process action |
| `F2`, `?`/`F1` | Settings view, help |

The layout adapts to the terminal: the full layout from 80×24, a compact summary below that, and a
size notice below 40×10 (quit still works). Command-line values override saved settings only when
given, and only for the current run.

## Configuration and files

| Purpose | Location |
|---|---|
| Configuration (schema 2, versioned, atomic writes) | `$XDG_CONFIG_HOME/dgxtop/v2/config.json` (`~/.config/dgxtop/v2/config.json`) |
| Action audit (SQLite, `synchronous=FULL`) | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3` (`~/.local/state/...`) |
| History archive (on by default, 7 days, at most 1 GiB) | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3` |
| Bounded log, instance lock | `$XDG_STATE_HOME/dgxtop/v2/` |

Directories are created `0700` and files `0600`. Full command lines, environments and tokens are
never persisted. The configuration contract is in
[`spec/dgxtop-v2/contracts/CONFIG.md`](spec/dgxtop-v2/contracts/CONFIG.md) and the defaults in
[`spec/dgxtop-v2/examples/config.default.json`](spec/dgxtop-v2/examples/config.default.json).
Upgrading from v1 never modifies `~/.config/dgxtop/config.json`: review the mapping with
`--import-legacy <path> --dry-run`, then write the v2 file with `--apply`.

## Development and validation

```bash
just ci        # fmt, check, clippy -D warnings, tests, dependency boundaries (all --locked)
just e2e       # end-to-end suites on request: PTY sessions, fault injection, real host processes
just hardware  # hardware suites on real procfs/sysfs/NVML (and pidfd on disposable children)
```

Format, lint, the test suite (1,373 tests on x86_64, 1,372 on aarch64), the dependency boundaries
and the hardware suites pass on a Debian 13 workstation (RTX 5080 + RTX 3060) and on a DGX Spark
(GB10). Their logs and SHA-256 hashes are in [`docs/validation/evidence/`](docs/validation/evidence/).
[`docs/validation/VALIDATION.md`](docs/validation/VALIDATION.md) tracks the 40 acceptance cases
(currently 14 PASS, 24 NOT_RUN, 2 BLOCKED). The 3 × 30-minute benchmark runs and the 72-hour soak are
still missing, so no platform is called validated, and the performance numbers in the specification
remain targets.

## Documentation

- [Design notes](https://dennysora.me/en/projects/tools/dgxtop/): the architecture and the ideas
  behind it ([繁體中文](https://dennysora.me/zh-hant/projects/tools/dgxtop/)).
- [`spec/dgxtop-v2/`](spec/dgxtop-v2/README.md): the specification (Traditional Chinese).
- [`docs/design/DECISIONS.md`](docs/design/DECISIONS.md): implementation decisions D-01 to D-63.
- [`docs/design/history-redesign.md`](docs/design/history-redesign.md): how the history view was
  designed.

## License

Apache-2.0. See [LICENSE](LICENSE).
