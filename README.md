# dgxtop

![dgxtop — Overview on DGX Spark](docs/screenshot-overview.png)

[繁體中文](docs/README.zh-TW.md) | [简体中文](docs/README.zh-CN.md) | [日本語](docs/README.ja.md)

> High-performance, interactive system monitor for NVIDIA DGX systems with real-time GPU, CPU, memory, disk, and network monitoring.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)
[![Release](https://github.com/DennySORA/dgxtop/actions/workflows/release.yml/badge.svg)](https://github.com/DennySORA/dgxtop/releases)

**dgxtop** is a comprehensive system monitoring tool purpose-built for NVIDIA DGX infrastructure. It provides real-time visibility into GPU utilization, VRAM, temperature, power draw, NVLink topology, and system resources — all in an interactive terminal UI. Built in Rust with direct NVML access for maximum performance and reliability.

## Quick Install

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

See [Installation](#installation) for more options.

## Why dgxtop?

- **Direct NVML Access** — Reads GPU metrics through NVIDIA Management Library, not nvidia-smi subprocess calls. Faster, more reliable, and more detailed.
- **DGX-Optimized** — Supports multi-GPU monitoring, NVLink topology, ECC error tracking, and PCIe throughput — features critical for DGX A100/H100/B200 and DGX Spark.
- **Full System View** — CPU per-core utilization, memory (RAM + Swap), disk I/O (IOPS, latency, throughput), and network interfaces in a single dashboard.
- **Interactive Process Management** — Sort, filter, and kill GPU processes directly from the TUI. See per-process GPU utilization, VRAM usage, and host memory.
- **Secure by Design** — No subprocess shell-outs, PID recycling protection, config value sanitization, and UTF-8 safe rendering. Passed deep security audit.

## Features

### GPU Monitoring (via NVML)

| Category | Metrics |
|----------|---------|
| **Utilization** | GPU %, Memory controller %, per-process SM utilization |
| **Memory** | VRAM used/total/free, BAR1 usage, per-process GPU memory |
| **Bandwidth** | Memory bandwidth utilization (actual/theoretical GB/s), unified-memory auto-detection (LPDDR5X), PCIe TX/RX throughput with 1/6/12/24h avg/max stats |
| **Thermal** | Temperature (with slowdown/shutdown thresholds), fan speed, 1/6/12/24h avg/max |
| **Power** | Draw/limit (watts), usage %, total energy consumption (kWh), 1/6/12/24h avg/max/cumulative energy |
| **Clock** | Graphics, SM, Memory, Video frequencies (current/max MHz) |
| **State** | Performance state (P0–P15), throttle reasons, compute mode, persistence mode |
| **Health** | ECC errors (corrected/uncorrected), retired pages (SBE/DBE) |
| **Topology** | NVLink active links with remote GPU mapping, PCIe Gen/Width |
| **Codec** | Encoder/Decoder utilization % |
| **Identity** | UUID, serial number |

### System Monitoring

| Category | Metrics |
|----------|---------|
| **CPU** | Aggregate and per-core usage (htop-style), user/system/iowait breakdown, temperature, frequency, **load average** (1/5/15m), **tasks** (running/total) |
| **Memory** | RAM used/total, buffers, cached, available, swap usage |
| **Disk I/O** | Per-device read/write throughput, IOPS, await latency, sorted by throughput |
| **Network** | Per-interface RX/TX throughput, errors, sorted by activity |

### Time-Series Analytics

- **Line charts** for GPU utilization, memory, temperature, and power with Braille markers
- **1/6/12/24h statistics** — avg/max for CPU, Memory, GPU utilization, temperature, power (with cumulative kWh), memory, PCIe TX/RX; avg R/W for Disk; cumulative ↓/↑ for Network
- **Progressive display** — stats windows only appear after enough data has been collected
- **Minute-resolution aggregation** — memory-efficient 24h storage (~28KB per metric)

### Interactive TUI

- **Three Views** — Overview dashboard, GPU detail (full-page per-GPU), full-screen process table
- **Vim Keybindings** — Navigate with `j/k`, switch tabs with `1/2/3`, GPU selection with `h/l`
- **Device Selection** — Cycle network interfaces (`n/N`), disk devices (`d/D`) with chart/stats following selection
- **Process Management** — Sort by GPU mem/utilization/CPU/PID, filter by name, kill with confirmation
- **Responsive Layout** — Auto-adapts to terminal size, conditionally shows charts/stats based on available space
- **Visual Design** — Rounded panels, gradient gauges, line charts, alternating row colors, color-coded thresholds

#### Multi-GPU Overview with Active Workloads

![dgxtop — Multi-GPU with active workloads](docs/screenshot-multi-gpu.png)

## Installation

### Quick Install (Recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

The installer auto-detects your libc and picks the matching release target.
For NVIDIA GPU metrics, glibc (`-gnu`) builds are recommended.

### Download Binary

Download pre-built binaries from [GitHub Releases](https://github.com/DennySORA/dgxtop/releases):

| Platform | Architecture | Download |
|----------|--------------|----------|
| Linux | x86_64 (glibc, recommended) | `dgxtop-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | x86_64 (musl, compatibility) | `dgxtop-x86_64-unknown-linux-musl.tar.gz` |
| Linux | ARM64 (glibc, recommended) | `dgxtop-aarch64-unknown-linux-gnu.tar.gz` |
| Linux | ARM64 (musl, compatibility) | `dgxtop-aarch64-unknown-linux-musl.tar.gz` |

> Note: On some systems, musl builds may fail to load NVIDIA NVML (`libnvidia-ml.so`), resulting in missing GPU metrics.

### Build from Source

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release
# Binary: target/release/dgxtop
```

### Cargo Install

```bash
cargo install --git https://github.com/DennySORA/dgxtop.git
```

## Usage

### Basic Usage

```bash
# Start with default settings
dgxtop

# Custom refresh interval (0.5 seconds)
dgxtop -i 0.5

# Disable GPU monitoring (system metrics only)
dgxtop --no-gpu

# Use green color theme
dgxtop -t green
```

### Command Line Options

| Option | Description | Default |
|--------|-------------|---------|
| `-i, --interval <SECS>` | Update interval in seconds (0.1–10.0) | `1.0` |
| `-t, --theme <NAME>` | Color theme: `cyan`, `green`, `amber` | `cyan` |
| `--no-gpu` | Disable GPU monitoring | `false` |
| `--net-max <N>` | Max visible network interfaces / disk devices (1–20) | `3` |
| `--log-level <LEVEL>` | Log level: `error`, `warn`, `info`, `debug` | `warn` |

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `q` / `Ctrl+C` | Quit |
| `Tab` / `Shift+Tab` | Switch between views |
| `1` / `2` / `3` | Jump to Overview / GPU Detail / Processes |
| `j/k` or `↑/↓` | Navigate up/down |
| `h/l` or `←/→` | Select GPU (in GPU Detail view) |
| `s` | Enter sort mode |
| `r` | Reverse sort order (in sort mode) |
| `/` | Filter processes by name/PID/user |
| `K` | Kill selected process (with confirmation) |
| `e` | Toggle per-core CPU view (htop-style) |
| `n` / `N` | Cycle network interface |
| `d` / `D` | Cycle disk device |
| `+` / `-` | Increase / decrease refresh speed |
| `?` | Toggle help overlay |

### Views

**Overview** — Responsive dashboard with CPU gauge (load average, tasks, htop per-core toggle), memory bars, GPU cards, disk I/O and network panels with line charts and 1/6/12/24h statistics. Device selection highlights chart/stats for the chosen interface or disk.

**GPU Detail** — Full-page single-GPU view with comprehensive metrics: utilization, VRAM, **memory bandwidth** (actual/theoretical GB/s), BAR1, thermal (with thresholds), power (with energy kWh), **throttle reasons**, P-state, all clock domains (Graphics/SM/Memory/Video), PCIe info, **NVLink topology**, encoder/decoder, ECC, retired pages, compute mode, UUID — plus time-series charts and per-GPU process list.

![dgxtop — GPU Detail view](docs/screenshot-gpu-detail.png)

**Processes** — Full-screen GPU process table with sortable columns, search filter, and process kill capability. Shows PID, user, GPU device, type, GPU%, VRAM, CPU%, host memory, and command.

![dgxtop — Processes view](docs/screenshot-processes.png)

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                           Main Thread                               │
│  ┌───────────┐    ┌────────────┐    ┌────────────────────────────┐ │
│  │  AppState  │◄───│  UI Loop   │◄───│ crossbeam channel (rx)    │ │
│  │            │    │  (ratatui) │    │ bounded(256)               │ │
│  └───────────┘    └────────────┘    └────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────┘
                                                ▲
                     Tick events + Key/Mouse    │
                                                │
┌───────────────────────────────────────────────┴─────────────────────┐
│                        Event Thread                                  │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  crossterm::event::poll + Tick timer → AppEvent → channel tx  │  │
│  └───────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────┘

Collectors (called on each Tick):
  ├── GpuCollector        (NVML: utilization, temp, power, clocks, memory, bandwidth,
  │                         ECC, PCIe, NVLink, BAR1, throttle, P-state, encoder/decoder,
  │                         retired pages, energy, UUID, serial)
  ├── GpuProcessCollector (NVML + /proc: per-process GPU/CPU/memory stats)
  ├── CpuCollector        (/proc/stat + /proc/loadavg: per-core usage, frequency,
  │                         temperature, load average, task count)
  ├── MemoryCollector     (/proc/meminfo: RAM, swap, buffers, cached)
  ├── DiskCollector       (/proc/diskstats: per-device throughput, IOPS, latency)
  └── NetworkCollector    (/sys/class/net: per-interface RX/TX, packets, errors)

History (per metric):
  ├── RingBuffer          (short-term: 300 samples for charts)
  └── TimeWindowAggregator (long-term: minute-resolution buckets for 24h stats)
```

## Requirements

- **OS**: Linux (DGX systems, WSL2, containers)
- **GPU**: NVIDIA drivers with NVML (libnvidia-ml.so)
- **Runtime**: No additional dependencies. For GPU monitoring, prefer glibc (`-gnu`) builds.

## Security

dgxtop has undergone a comprehensive security audit addressing:
- UTF-8 boundary safety in all string rendering
- PID recycling protection with pre-kill verification
- Integer overflow protection with saturating arithmetic
- Path traversal prevention in filesystem readers
- Config value sanitization against NaN/Infinity/DoS
- Bounded event channels to prevent memory exhaustion

See commit history for the full audit report and fixes.

## License

Apache License 2.0 — see [LICENSE](LICENSE) for details.

## Contributing

Contributions welcome! Please feel free to submit issues and pull requests.
