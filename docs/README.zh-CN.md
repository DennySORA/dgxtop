# dgxtop

<!-- ![dgxtop](screenshot.png) -->

[English](../README.md) | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md)

> 专为 NVIDIA DGX 系统打造的高性能交互式系统监控工具，支持实时 GPU、CPU、内存、磁盘及网络监控。

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)
[![Release](https://github.com/DennySORA/dgxtop/actions/workflows/release.yml/badge.svg)](https://github.com/DennySORA/dgxtop/releases)

**dgxtop** 是一款专为 NVIDIA DGX 基础设施打造的全方位系统监控工具。通过交互式终端界面，实时呈现 GPU 利用率、VRAM、温度、功耗、NVLink 拓扑及系统资源。以 Rust 开发，直接访问 NVML 以实现最佳性能与可靠性。

## 快速安装

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

详见 [安装方式](#安装方式) 了解更多选项。

## 为什么选择 dgxtop？

- **直接访问 NVML** — 通过 NVIDIA Management Library 读取 GPU 指标，非 nvidia-smi 子进程调用。更快、更可靠、更详细。
- **DGX 专属优化** — 支持多 GPU 监控、NVLink 拓扑、ECC 错误追踪、PCIe 带宽 — 这些是 DGX A100/H100/B200 和 DGX Spark 的关键功能。
- **完整系统视野** — 单一仪表盘涵盖 CPU 每核心利用率、内存（RAM + Swap）、磁盘 I/O（IOPS、延迟、吞吐量）及网络接口。
- **交互式进程管理** — 直接在 TUI 中排序、筛选及终止 GPU 进程。可查看每个进程的 GPU 利用率、VRAM 用量及主机内存。
- **安全设计** — 无子进程 shell 调用、PID 回收保护、配置值消毒、UTF-8 安全渲染。已通过深度安全审查。

## 功能特性

### GPU 监控（通过 NVML）

| 类别 | 指标 |
|------|------|
| **利用率** | GPU %、显存 %、每进程 SM 利用率 |
| **显存** | VRAM 已用/总量/可用、每进程 GPU 显存 |
| **温度** | 温度、风扇转速 |
| **功耗** | 实际/上限（瓦特）、使用百分比 |
| **频率** | 图形频率、显存频率、最大频率 |
| **健康** | ECC 错误（已纠正/未纠正）、PCIe 吞吐量 |
| **拓扑** | NVLink 活跃链路、远端 GPU 映射 |

### 系统监控

| 类别 | 指标 |
|------|------|
| **CPU** | 总体及每核心利用率、用户/系统/iowait 分解、温度、频率 |
| **内存** | RAM 已用/总量、缓冲区、缓存、可用、Swap 用量 |
| **磁盘 I/O** | 每设备读写吞吐量、IOPS、await 延迟、队列深度 |
| **网络** | 每接口 RX/TX 吞吐量、包速率、错误、丢包 |

### 交互式 TUI

- **三种视图** — 总览仪表盘、GPU 详细信息含历史图表、全屏进程表格
- **Vim 快捷键** — 以 `j/k` 导航、`1/2/3` 切换标签页、`h/l` 选择 GPU
- **进程管理** — 按 GPU 显存/利用率/CPU/PID 排序、按名称筛选、确认后终止进程
- **视觉设计** — 圆角面板、半格精度渐变仪表、趋势图、交替行颜色、色彩编码阈值

## 安装方式

### 快速安装（推荐）

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

### 下载二进制文件

从 [GitHub Releases](https://github.com/DennySORA/dgxtop/releases) 下载预构建二进制文件：

| 平台 | 架构 | 下载 |
|------|------|------|
| Linux | x86_64 | `dgxtop-x86_64-unknown-linux-musl.tar.gz` |
| Linux | x86_64 (glibc) | `dgxtop-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | ARM64 | `dgxtop-aarch64-unknown-linux-musl.tar.gz` |
| Linux | ARM64 (glibc) | `dgxtop-aarch64-unknown-linux-gnu.tar.gz` |

### 从源码构建

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release
# 二进制文件：target/release/dgxtop
```

### Cargo Install

```bash
cargo install --git https://github.com/DennySORA/dgxtop.git
```

## 使用方式

### 基本用法

```bash
# 以默认设置启动
dgxtop

# 自定义更新间隔（0.5 秒）
dgxtop -i 0.5

# 禁用 GPU 监控（仅系统指标）
dgxtop --no-gpu

# 使用绿色主题
dgxtop -t green
```

### 命令行选项

| 选项 | 说明 | 默认值 |
|------|------|--------|
| `-i, --interval <秒>` | 更新间隔（0.1–10.0 秒） | `1.0` |
| `-t, --theme <名称>` | 颜色主题：`cyan`、`green`、`amber` | `cyan` |
| `--no-gpu` | 禁用 GPU 监控 | `false` |
| `--log-level <级别>` | 日志级别：`error`、`warn`、`info`、`debug` | `warn` |

### 键盘快捷键

| 按键 | 动作 |
|------|------|
| `q` / `Ctrl+C` | 退出 |
| `Tab` / `Shift+Tab` | 切换视图 |
| `1` / `2` / `3` | 跳转到 总览 / GPU 详情 / 进程 |
| `j/k` 或 `↑/↓` | 上下导航 |
| `h/l` 或 `←/→` | 选择 GPU（GPU 详情视图） |
| `s` | 进入排序模式 |
| `r` | 反转排序顺序（排序模式中） |
| `/` | 按名称/PID/用户筛选进程 |
| `K` | 终止选中的进程（需确认） |
| `e` | 切换每核心 CPU 显示 |
| `+` / `-` | 加快 / 减慢刷新速度 |
| `?` | 显示/隐藏帮助 |

## 系统要求

- **操作系统**：Linux（DGX 系统、WSL2、容器）
- **GPU**：安装 NVML 的 NVIDIA 驱动程序（libnvidia-ml.so）
- **运行环境**：无额外依赖 — 提供静态 musl 构建

## 许可证

Apache License 2.0 — 详见 [LICENSE](../LICENSE)。

## 贡献

欢迎贡献！请随时提交 Issue 和 Pull Request。
