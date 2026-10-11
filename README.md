<div align="center">

# dgxtop

**A terminal GPU monitor built for NVIDIA DGX Spark**

Every GPU, the processes using it and the whole machine on one screen,<br>
with history that survives restarts. Runs on any Linux machine with NVIDIA GPUs.

[繁體中文](docs/README.zh-TW.md) · [日本語](docs/README.ja.md) · [Project page](https://dennysora.me/en/projects/tools/dgxtop/)

[![Release](https://img.shields.io/github/v/release/DennySORA/dgxtop)](https://github.com/DennySORA/dgxtop/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92.0-orange.svg)](rust-toolchain.toml)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)

<img src="docs/images/screenshots/dgx-spark-overview.png" alt="dgxtop overview on a DGX Spark: CPU and memory, the GB10 GPU at 92 % utilization with its RAM bar, the disk and network tables, and the GPU process table" width="100%">

<sub>Overview on a DGX Spark. The GB10 shares memory with the CPU, so the RAM its processes hold is drawn inside system memory.</sub>

</div>

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

Linux on x86_64 or aarch64 with glibc 2.34 or newer (Ubuntu 22.04, Debian 12, RHEL 9 and later),
DGX Spark included. WSL 2 runs the Linux build. GPU metrics need the NVIDIA driver; without it, CPU,
memory, disks and network still work. WSL 1, musl, macOS and Windows are not supported.

The installer picks the build for your CPU, checks it against the release's `SHA256SUMS` and
installs it to `~/.local/bin`. The previous version is kept, and `./install.sh --rollback` brings
it back. For a specific release, append `-s -- --version v2.0.2` to `bash`.

Or build from source with Rust 1.92.0 (pinned by `rust-toolchain.toml`):

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release --locked
./target/release/dgxtop
```

## Features

htop centers on the CPU, and nvtop and nvidia-smi show GPUs only while you watch. dgxtop puts the
GPUs first, shows the whole machine around them, and keeps the history.

- **Every GPU at a glance.** Utilization, memory, temperature, power, clocks and fans for each GPU,
  with live charts.
- **Full GPU details.** One GPU at a time:
  - performance state and throttle reasons, clocks;
  - memory and the memory bus, PCIe link and traffic, NVLink-C2C;
  - thermal limits, power and energy;
  - CUDA and driver versions;
  - NUMA and CPU affinity, P2P paths, device and firmware.

  A value the GPU does not report gets no row.
- **GPU processes.** Every process using a GPU, with its GPU, type (compute or graphics), GPU
  memory, CPU and RAM. A process is named by what it runs, not only by its executable:
  `python -m vllm.entrypoints.openai.api_server` shows as `vllm.api_server`, and `python main.py`
  run in a `ComfyUI` folder as `ComfyUI/main`. Filter by GPU or name, sort by any column, and stop
  a process after a confirmation. The eight processes holding the most GPU memory each get a
  color: their parts of the memory and VRAM bars and charts use it, and so does the marker beside
  their names. The color comes from the name, so a job gets the same color after it or dgxtop
  restarts, unless another running process already holds it.
- **History.** Charts on one time axis, one bar per column, each bar the average of 1, 5, 10 or
  30 minutes or 1, 2, 4, 6, 8, 12, 18 or 24 hours (`w` / `W`) of the local clock: a 1-hour bar is
  15:00–16:00, a 24-hour bar a day, the latest bar the one being filled now. A bar is the average
  of the time it holds (dgxtop started at 15:15: the 15:00 bar is 15:15–16:00), and a time
  without data stays a gap:
  - CPU, memory, disks and network;
  - each GPU's utilization, memory, temperature and power;
  - now, average and maximum for every chart;
  - each GPU's last 1, 6, 12 and 24 hours, with the energy it used;
  - each colored process's part of the memory charts.

  History is saved on disk and survives restarts and reboots, with no age limit: each bar size
  keeps its latest 300 bars, the older time lives on in the coarser sizes, so the 24-hour bars
  reach back 300 days. The 1-hour to 24-hour statistics are merged from those bars too. The
  per-process parts are kept in memory, for as long as dgxtop runs.
- **Process History.** Every GPU process dgxtop saw, the latest start first: when it started and
  how long it ran, its GPUs, peak GPU memory and RAM, GPU and CPU use in the load colors, its most
  child processes, and how it ended: still running, stopped by dgxtop, killed by a signal
  (`SIGKILL`, `SIGTERM`, …), exited with a code, most likely the OOM killer, or ended without a
  visible cause. Open one to see its full command line (`e` shows all of it, `y` copies it), its
  working folder, program, parent process, niceness and control group, its start and end times,
  and charts laid out like the Overview History's, with average and maximum: CPU and memory of the
  process and of its child processes, swap, GPU utilization and memory, threads, child processes
  and open files, disk reads and writes, and the host's network. `w`/`W` set how much time one bar
  is: 1, 5, 10 or 30 minutes, 1, 2, 4, 6, 8, 12, 18 or 24 hours, or `auto` (the finest that shows
  the whole run). Each bar size keeps its latest 300 bars, older ones live on in the coarser
  sizes, so a run takes at most about a megabyte however long it lasts, and runs never expire by
  age: the oldest ended ones give way only when the 64 MiB database fills.
- **The whole machine.** CPU usage per core, CPU temperature and power where the hardware reports
  them, memory and swap, disk throughput and filesystem capacity, network throughput.
- **Made for DGX Spark.** The GB10 shares memory with the CPU: dgxtop shows how much system RAM GPU
  processes hold and how much the system itself holds, the CPU temperature from the GB10's cluster
  sensors, and the GB10's NVLink-C2C link. Native aarch64 build.
- **Settings in the app.** Change the sampling interval and the display modules. Each setting shows
  its saved and effective value, where it came from, and whether it applies now or after a restart.

## Screens

### Overview

Every GPU with its utilization and memory, CPU and memory, the busiest disks and network
interfaces, and the processes using a GPU. `c` expands the CPU into per-core bars and sensors.

<img src="docs/images/screenshots/workstation-overview.png" alt="dgxtop overview on a workstation with two GPUs, an RTX 5080 and an RTX 3060, with the processes holding GPU memory" width="100%">

### GPU

One GPU at a time: four live bars, then everything the driver reports. `h` / `l` switches between
GPUs.

<img src="docs/images/screenshots/dgx-spark-gpu.png" alt="GPU view of the DGX Spark's GB10: state, clocks, unified memory, PCIe, NVLink-C2C, thermal thresholds, power, CUDA 13.0, driver, device and topology" width="100%">

<img src="docs/images/screenshots/workstation-gpu.png" alt="GPU view of an RTX 5080: state, clocks, VRAM and BAR1, PCIe Gen5, fans and thresholds, power limits, CUDA, driver, device, firmware and the P2P path to the second GPU" width="100%">

<sub>GB10 with unified memory and NVLink-C2C (top); RTX 5080 with VRAM and PCIe Gen5 (bottom). Serial numbers and UUIDs are masked.</sub>

### History

The overview's resources as bars of 1 minute to 24 hours (`w` / `W`), in paired panels on one time
axis, with each GPU's last 1, 6, 12 and 24 hours below.

<img src="docs/images/screenshots/dgx-spark-history.png" alt="History view of the DGX Spark over one hour: CPU, memory, GPU utilization, GPU memory, temperature, power with energy, disk and network" width="100%">

<img src="docs/images/screenshots/workstation-history.png" alt="History view of the two-GPU workstation with each GPU's mean and peak over the last 1, 6, 12 and 24 hours" width="100%">

### Settings

Every setting with its saved and effective value, its origin and whether it applies live or needs a
restart. Display modules can be switched on and off.

<img src="docs/images/screenshots/dgx-spark-settings.png" alt="Settings view listing each setting's saved value, effective value, origin and restart requirement, next to the display module toggles" width="100%">

## Capabilities

- **Several GPUs per machine.** Tested on a DGX Spark (GB10) and on a workstation with an RTX 5080
  and an RTX 3060.
- **Light.** Written in Rust. It redraws only when something changes, at most 4 times a second, and
  keeps its data within a fixed 256 MiB.
- **Any terminal size.** The full layout from 80×24, a compact one below that. Keyboard and mouse.
- **Private.** No network access and no telemetry. Settings and history are files only you can
  read. Process History keeps the command lines, working folders and programs of GPU processes
  (the latest 2000 runs); environments are never stored.
- **Safe modes.** `--read-only` turns off process actions. `--replay` shows simulated data for
  demos and tests. `--diagnostics-json` prints versions and health.
- **Safe upgrades.** The installer verifies every download and keeps the previous version for a
  one-command rollback.

## Architecture

dgxtop is one program built from six components. Readings travel the data path; your keys and
commands travel the control path.

<img src="docs/images/architecture/architecture-overview.png" alt="dgxtop architecture: adapter hosts for procfs, hwmon, thermal, powercap, NVML, network and filesystem feed the collector, the manager writes the store and publishes view models to the viewer, and the controller and the process action adapter sit on the control path" width="100%">

| Component | What it does |
|---|---|
| Adapters | Read the hardware: `/proc`, hwmon, thermal zones, powercap, NVML for the GPUs, network and filesystems. Each adapter runs in its own background process. |
| Collectors | Turn raw readings into metrics: units, duplicates, the best source for each value, and rates from counters. |
| Manager | Schedules sampling, applies settings, checks commands, and is the only part that writes data. |
| Store | Holds the live snapshot, the history, the settings and the log of process actions. |
| Viewer | Draws the screen with Ratatui from ready-made views of the data. |
| Controller | Turns keys, mouse clicks and command-line options into commands. |

<details>
<summary>Code layout</summary>

The code is a Rust workspace of eight crates around one shared contract crate.

| Crate | Contains |
|---|---|
| `dgxtop` | The program: startup, command line and wiring |
| `dgxtop-core` | Shared types: metrics, units, commands and interfaces |
| `dgxtop-runtime` | Background processes, IPC, workers and terminal I/O |
| `dgxtop-adapters` | procfs, sysfs, NVML and process actions |
| `dgxtop-collectors` | Normalization, source selection and rates |
| `dgxtop-store` | Snapshot, history, settings and the action log (SQLite) |
| `dgxtop-manager` | Sampling, settings, commands and views |
| `dgxtop-ui` | Screens, widgets and the key map |

</details>

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

Each panel names its own keys at the right of its title; the footer keeps the global ones.

| Key | Action |
|---|---|
| `q`, `Ctrl-C` | Quit (`q` is text while editing a filter; `Ctrl-C` always quits) |
| `1`–`5`, `Tab`, `Shift-Tab` | Overview, GPU, Overview History, Process History, Settings |
| `w` / `W`, `→` / `←` | Next / previous bar size of the history charts (1m to 24h); in an opened run, its bar size |
| `j`/`k`, `Enter` | In Process History, choose a run and open or close it |
| `e` / `y` | In an opened run, show the whole command line / copy it (OSC 52) |
| `c` | Show / hide the CPU sensors and per-core bars (or click the CPU panel) |
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move the selection |
| `h`/`l`, `←`/`→` | Previous / next GPU in the GPU view |
| `g` | Cycle the process GPU filter (all GPUs → each GPU) |
| `/` | Filter processes (Enter applies, Esc clears) |
| `s` / `r` | Sort column / reverse order |
| `+` / `-` | Change the sampling interval |
| `K` | Stop the selected process (asks for confirmation) |
| `F9` | Force-kill the selected process with SIGKILL (asks for confirmation) |
| `[` / `]` | Scroll the GPU processes of the memory panel |
| `y`/`Enter`, `n`/`Esc` | Confirm / cancel a pending process action |
| `F2`, `?`/`F1` | Settings view, help |

Command-line values override saved settings only for the current run.

## Configuration and files

| Purpose | Location |
|---|---|
| Configuration | `$XDG_CONFIG_HOME/dgxtop/v2/config.json` (`~/.config/dgxtop/v2/config.json`) |
| History, every bar size (at most 64 MiB) | `$XDG_STATE_HOME/dgxtop/v2/history_tiers.sqlite3` (`~/.local/state/...`) |
| History's recent minutes (at most 7 days and 1 GiB) | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3` |
| Process History (at most 64 MiB; `store.process_history_enabled`, unset following the archive) | `$XDG_STATE_HOME/dgxtop/v2/process_history.sqlite3` and `process_recent.json` |
| Log of process actions | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3` |
| Log file and instance lock | `$XDG_STATE_HOME/dgxtop/v2/` |

Directories are created `0700` and files `0600`. Upgrading from v1 never modifies
`~/.config/dgxtop/config.json`: review the mapping with `--import-legacy <path> --dry-run`, then
write the v2 file with `--apply`.

## License

Apache-2.0. See [LICENSE](LICENSE).
