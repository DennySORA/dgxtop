# dgxtop

<!-- ![dgxtop](screenshot.png) -->

[English](../README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

> 專為 NVIDIA DGX 系統打造的高效能互動式系統監控工具，支援即時 GPU、CPU、記憶體、磁碟及網路監控。

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)
[![Release](https://github.com/DennySORA/dgxtop/actions/workflows/release.yml/badge.svg)](https://github.com/DennySORA/dgxtop/releases)

**dgxtop** 是一款專為 NVIDIA DGX 基礎設施打造的全方位系統監控工具。透過互動式終端介面，即時呈現 GPU 使用率、VRAM、溫度、功耗、NVLink 拓撲及系統資源。以 Rust 開發，直接存取 NVML 以達到最佳效能與可靠性。

## 快速安裝

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

詳見 [安裝方式](#安裝方式) 了解更多選項。

## 為什麼選擇 dgxtop？

- **直接存取 NVML** — 透過 NVIDIA Management Library 讀取 GPU 指標，非 nvidia-smi 子程序呼叫。更快、更可靠、更詳細。
- **DGX 專屬最佳化** — 支援多 GPU 監控、NVLink 拓撲、ECC 錯誤追蹤、PCIe 頻寬 — 這些是 DGX A100/H100/B200 和 DGX Spark 的關鍵功能。
- **完整系統視野** — 單一儀表板涵蓋 CPU 每核心使用率、記憶體（RAM + Swap）、磁碟 I/O（IOPS、延遲、吞吐量）及網路介面。
- **互動式程序管理** — 直接在 TUI 中排序、篩選及終止 GPU 程序。可查看每個程序的 GPU 使用率、VRAM 用量及主機記憶體。
- **安全設計** — 無子程序 shell 呼叫、PID 回收保護、設定值消毒、UTF-8 安全渲染。已通過深度安全審查。

## 功能特色

### GPU 監控（透過 NVML）

| 類別 | 指標 |
|------|------|
| **使用率** | GPU %、記憶體 %、每程序 SM 使用率 |
| **記憶體** | VRAM 已用/總量/可用、每程序 GPU 記憶體 |
| **溫度** | 溫度、風扇轉速 |
| **功耗** | 實際/上限（瓦特）、使用百分比 |
| **時脈** | 繪圖頻率、記憶體頻率、最大頻率 |
| **健康** | ECC 錯誤（已修正/未修正）、PCIe 吞吐量 |
| **拓撲** | NVLink 活躍連結、遠端 GPU 對映 |

### 系統監控

| 類別 | 指標 |
|------|------|
| **CPU** | 總體及每核心使用率、使用者/系統/iowait 分解、溫度、頻率 |
| **記憶體** | RAM 已用/總量、緩衝區、快取、可用、Swap 用量 |
| **磁碟 I/O** | 每裝置讀寫吞吐量、IOPS、await 延遲、佇列深度 |
| **網路** | 每介面 RX/TX 吞吐量、封包速率、錯誤、丟棄封包 |

### 互動式 TUI

- **三種視圖** — 總覽儀表板、GPU 詳細資訊含歷史圖表、全螢幕程序表格
- **Vim 快捷鍵** — 以 `j/k` 導航、`1/2/3` 切換頁籤、`h/l` 選擇 GPU
- **程序管理** — 依 GPU 記憶體/使用率/CPU/PID 排序、依名稱篩選、確認後終止程序
- **視覺設計** — 圓角面板、半格精度漸層量表、走勢圖、交替列顏色、色彩編碼閾值

## 安裝方式

### 快速安裝（推薦）

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

安裝腳本會自動偵測 libc 並選擇對應目標。
若要完整 NVIDIA GPU 指標，建議使用 glibc（`-gnu`）版本。

### 下載二進位檔

從 [GitHub Releases](https://github.com/DennySORA/dgxtop/releases) 下載預建二進位檔：

| 平台 | 架構 | 下載 |
|------|------|------|
| Linux | x86_64（glibc，推薦） | `dgxtop-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | x86_64（musl，相容性） | `dgxtop-x86_64-unknown-linux-musl.tar.gz` |
| Linux | ARM64（glibc，推薦） | `dgxtop-aarch64-unknown-linux-gnu.tar.gz` |
| Linux | ARM64（musl，相容性） | `dgxtop-aarch64-unknown-linux-musl.tar.gz` |

> 注意：在部分系統上，musl 版本可能無法載入 NVIDIA NVML（`libnvidia-ml.so`），導致 GPU 指標缺失。

### 從原始碼建置

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release
# 二進位檔：target/release/dgxtop
```

### Cargo Install

```bash
cargo install --git https://github.com/DennySORA/dgxtop.git
```

## 使用方式

### 基本用法

```bash
# 以預設設定啟動
dgxtop

# 自訂更新間隔（0.5 秒）
dgxtop -i 0.5

# 停用 GPU 監控（僅系統指標）
dgxtop --no-gpu

# 使用綠色主題
dgxtop -t green
```

### 命令列選項

| 選項 | 說明 | 預設值 |
|------|------|--------|
| `-i, --interval <秒>` | 更新間隔（0.1–10.0 秒） | `1.0` |
| `-t, --theme <名稱>` | 色彩主題：`cyan`、`green`、`amber` | `cyan` |
| `--no-gpu` | 停用 GPU 監控 | `false` |
| `--log-level <等級>` | 記錄等級：`error`、`warn`、`info`、`debug` | `warn` |

### 鍵盤快捷鍵

| 按鍵 | 動作 |
|------|------|
| `q` / `Ctrl+C` | 離開 |
| `Tab` / `Shift+Tab` | 切換視圖 |
| `1` / `2` / `3` | 跳至 總覽 / GPU 詳情 / 程序 |
| `j/k` 或 `↑/↓` | 上下導航 |
| `h/l` 或 `←/→` | 選擇 GPU（GPU 詳情視圖） |
| `s` | 進入排序模式 |
| `r` | 反轉排序順序（排序模式中） |
| `/` | 依名稱/PID/使用者篩選程序 |
| `K` | 終止選取的程序（需確認） |
| `e` | 切換每核心 CPU 顯示 |
| `+` / `-` | 加快 / 減慢更新速度 |
| `?` | 顯示/隱藏說明 |

## 系統需求

- **作業系統**：Linux（DGX 系統、WSL2、容器）
- **GPU**：安裝 NVML 的 NVIDIA 驅動程式（libnvidia-ml.so）
- **執行環境**：無額外相依性。若需 GPU 監控，建議使用 glibc（`-gnu`）版本。

## 授權條款

Apache License 2.0 — 詳見 [LICENSE](../LICENSE)。

## 貢獻

歡迎貢獻！請隨時提交 Issue 和 Pull Request。
