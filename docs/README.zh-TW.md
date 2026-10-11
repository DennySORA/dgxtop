<div align="center">

# dgxtop

**為 NVIDIA DGX Spark 打造的終端 GPU 監控工具**

所有 GPU、使用它們的程序與整台機器，一個畫面看完；<br>
歷史紀錄重開也還在。任何裝有 NVIDIA GPU 的 Linux 主機都能使用。

[English](../README.md) · [日本語](README.ja.md) · [專案頁面](https://dennysora.me/zh-hant/projects/tools/dgxtop/)

[![Release](https://img.shields.io/github/v/release/DennySORA/dgxtop)](https://github.com/DennySORA/dgxtop/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92.0-orange.svg)](../rust-toolchain.toml)
[![CI](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml/badge.svg)](https://github.com/DennySORA/dgxtop/actions/workflows/ci.yml)

<img src="images/screenshots/dgx-spark-overview.png" alt="DGX Spark 上的 dgxtop 總覽：CPU 與記憶體、使用率 92% 的 GB10 GPU 與其 RAM 長條、磁碟與網路表格，以及 GPU 程序表" width="100%">

<sub>DGX Spark 上的總覽。GB10 與 CPU 共用記憶體，所以 GPU 程序佔用的 RAM 直接畫在系統記憶體裡。</sub>

</div>

## 安裝

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

支援 x86_64 或 aarch64 的 Linux，glibc 2.34 以上（Ubuntu 22.04、Debian 12、RHEL 9 及更新版本），包括 DGX Spark。WSL 2 可以執行 Linux 版。GPU 指標需要 NVIDIA 驅動；沒有驅動時，CPU、記憶體、磁碟與網路照常顯示。不支援 WSL 1、musl、macOS 與 Windows。

安裝程式會依 CPU 選擇對應的版本，以 release 的 `SHA256SUMS` 驗證後安裝到 `~/.local/bin`。舊版本會保留，執行 `./install.sh --rollback` 就能還原。要安裝特定版本，在 `bash` 後面加上 `-s -- --version v2.0.2`。

也可以用 Rust 1.92.0 從原始碼建置（由 `rust-toolchain.toml` 固定）：

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release --locked
./target/release/dgxtop
```

## 功能

htop 以 CPU 為主，nvtop 和 nvidia-smi 只在你盯著的時候顯示 GPU。dgxtop 以 GPU 為主，把整台機器放在旁邊，而且保留歷史。

- **一眼看完所有 GPU。** 每張 GPU 的使用率、記憶體、溫度、功耗、時脈與風扇，附即時圖表。
- **完整的 GPU 細節。** 一次看一張 GPU：
  - 效能狀態與降頻原因、時脈；
  - 記憶體與記憶體匯流排、PCIe 連線與流量、NVLink-C2C；
  - 溫度上限、功耗與耗電量；
  - CUDA 與驅動版本；
  - NUMA 與 CPU 親和性、P2P 路徑、裝置與韌體。

  GPU 不回報的值不會佔一行。
- **GPU 程序。** 所有使用 GPU 的程序，以及所在的 GPU、類型（運算或圖形）、GPU 記憶體、CPU 與 RAM。程序以它執行的內容命名，而不只是執行檔：`python -m vllm.entrypoints.openai.api_server` 顯示為 `vllm.api_server`，在 `ComfyUI` 資料夾執行的 `python main.py` 顯示為 `ComfyUI/main`。可依 GPU 或名稱篩選、依任一欄排序，確認後即可結束程序。佔用 GPU 記憶體最多的八個程序各有一種顏色：記憶體與 VRAM 長條、圖表中它們的部分，以及名稱旁的標記，都用同一種顏色。顏色由名稱決定，所以同一個工作在它或 dgxtop 重新啟動後仍是同一種顏色，除非另一個執行中的程序已經用了它。
- **歷史紀錄。** 圖表共用一條時間軸，每欄一根 bar，一根是本地時鐘上 1、5、10、30 分鐘或 1、2、4、6、8、12、18、24 小時的平均（`w` / `W`）：1 小時的 bar 是 15:00–16:00，24 小時的 bar 是一天，最新一根是正在累積的那根。一根 bar 是它實際涵蓋時間的平均（dgxtop 在 15:15 啟動時，15:00 那根是 15:15–16:00），沒有資料的時間保持空白：
  - CPU、記憶體、磁碟與網路；
  - 每張 GPU 的使用率、記憶體、溫度與功耗；
  - 每張圖都標出目前值、平均與最大值；
  - 每張 GPU 最近 1、6、12、24 小時的統計與耗電量；
  - 每個上色程序在記憶體圖表中的部分。

  歷史寫入磁碟，重開程式或重開機都還在，也沒有保存天數限制：每種 bar 大小保留最新 300 根，更舊的時間保存在更粗的大小裡，所以 24 小時的 bar 可回溯 300 天。1～24 小時的統計也由這些 bar 合併而成。各程序的部分保存在記憶體中，只在 dgxtop 執行期間記錄。
- **程序歷史（Process History）。** dgxtop 看過的每個 GPU 程序，最新啟動的排最前：啟動時間與執行多久、使用的 GPU、GPU 記憶體與 RAM 峰值、以負載顏色標示的 GPU 與 CPU 使用率、最多同時有幾個子程序，以及它如何結束：仍在執行、被 dgxtop 停止、被訊號結束（`SIGKILL`、`SIGTERM`…）、以結束碼退出、很可能是 OOM killer，或看不到原因而結束。打開一個程序，可看到完整命令列（`e` 展開全部、`y` 複製）、工作目錄、執行檔、父程序、nice 值與 cgroup、啟動與結束時間，以及與 Overview History 相同排版、附平均與最大值的圖表：程序本身與子程序的 CPU 和記憶體、swap、GPU 使用率與記憶體、執行緒、子程序與開啟的檔案數、磁碟讀寫，以及主機網路。`w`/`W` 設定一根 bar 代表多少時間：1、5、10、30 分鐘，1、2、4、6、8、12、18、24 小時，或 `auto`（能顯示整個執行期間的最細刻度）。每種大小保留最新 300 根，更舊的已合併在更粗的大小裡，所以不論執行多久，一個程序最多約佔 1 MB，也不會因為時間久而被刪除；只有 64 MiB 的資料庫快滿時，才會先刪除結束最久的程序。
- **整台機器。** 每顆核心的 CPU 使用率、硬體有回報時的 CPU 溫度與功耗、記憶體與 swap、磁碟流量與檔案系統容量、網路流量。
- **為 DGX Spark 打造。** GB10 與 CPU 共用記憶體：dgxtop 會顯示 GPU 程序與系統本身（OS）各佔用多少系統 RAM、取自 GB10 各叢集感測器的 CPU 溫度，以及 GB10 的 NVLink-C2C 連線。提供原生 aarch64 版本。
- **程式內設定。** 調整採樣間隔與顯示模組。每項設定都顯示儲存值與生效值、來源，以及是立即生效還是重開後生效。

## 畫面

### 總覽

每張 GPU 的使用率與記憶體、CPU 與記憶體、最忙的磁碟與網路介面，以及使用 GPU 的程序。按 `c` 會把 CPU 展開成每顆核心的長條與感測器。

<img src="images/screenshots/workstation-overview.png" alt="雙 GPU 工作站（RTX 5080 與 RTX 3060）上的 dgxtop 總覽，以及佔用 GPU 記憶體的程序" width="100%">

### GPU

一次看一張 GPU：四條即時長條，接著是驅動回報的所有資訊。用 `h` / `l` 切換 GPU。

<img src="images/screenshots/dgx-spark-gpu.png" alt="DGX Spark 的 GB10 GPU 頁面：狀態、時脈、統一記憶體、PCIe、NVLink-C2C、溫度門檻、功耗、CUDA 13.0、驅動、裝置與拓樸" width="100%">

<img src="images/screenshots/workstation-gpu.png" alt="RTX 5080 的 GPU 頁面：狀態、時脈、VRAM 與 BAR1、PCIe Gen5、風扇與溫度門檻、功耗上限、CUDA、驅動、裝置、韌體，以及到第二張 GPU 的 P2P 路徑" width="100%">

<sub>上：GB10，統一記憶體與 NVLink-C2C；下：RTX 5080，VRAM 與 PCIe Gen5。序號與 UUID 已遮蔽。</sub>

### 歷史

總覽的各項資源以 1 分鐘到 24 小時一根的 bar 呈現（`w` / `W`），以成對的面板共用一條時間軸；下方是每張 GPU 最近 1、6、12、24 小時的統計。

<img src="images/screenshots/dgx-spark-history.png" alt="DGX Spark 一小時的歷史：CPU、記憶體、GPU 使用率、GPU 記憶體、溫度、功耗與耗電量、磁碟與網路" width="100%">

<img src="images/screenshots/workstation-history.png" alt="雙 GPU 工作站的歷史，附每張 GPU 最近 1、6、12、24 小時的平均與峰值" width="100%">

### 設定

每項設定的儲存值與生效值、來源，以及是即時套用還是需要重開。可以開關各個顯示模組。

<img src="images/screenshots/dgx-spark-settings.png" alt="設定頁列出每個設定的儲存值、生效值、來源與是否需要重開，旁邊是顯示模組的開關" width="100%">

## 能力

- **一台機器多張 GPU。** 已在 DGX Spark（GB10）與裝有 RTX 5080 和 RTX 3060 的工作站上測試。
- **輕量。** 以 Rust 撰寫。只在內容變化時重繪，每秒最多 4 次，資料使用量固定在 256 MiB 以內。
- **任何終端尺寸。** 80×24 以上顯示完整版面，更小時改用精簡版面。支援鍵盤與滑鼠。
- **隱私。** 不連網，也不回傳任何資料。設定與歷史是只有你能讀的檔案。程序歷史會保留 GPU 程序的命令列、工作目錄與執行檔（最近 2000 個程序）；環境變數一律不儲存。
- **安全模式。** `--read-only` 關閉程序操作；`--replay` 用模擬資料做展示或測試；`--diagnostics-json` 輸出版本與健康狀態。
- **安全升級。** 安裝程式會驗證每個下載的檔案，並保留上一版，一個指令就能還原。

## 架構

dgxtop 是由六個元件組成的單一程式。讀值走資料路徑；你的按鍵與指令走控制路徑。

<img src="images/architecture/architecture-overview.png" alt="dgxtop 架構：procfs、hwmon、thermal、powercap、NVML、網路與檔案系統的 adapter host 送資料給 Collector，Manager 寫入 Store 並把畫面資料交給 Viewer；Controller 與程序操作 adapter 位於控制路徑" width="100%">

| 元件 | 功能 |
|---|---|
| Adapter | 讀取硬體：`/proc`、hwmon、thermal zone、powercap、GPU 的 NVML、網路與檔案系統。每個 adapter 在自己的背景程序裡執行。 |
| Collector | 把原始讀值整理成指標：單位、重複的來源、每個值的最佳來源，以及由計數器算出的速率。 |
| Manager | 排定採樣、套用設定、檢查指令，也是唯一寫入資料的元件。 |
| Store | 保存即時快照、歷史、設定與程序操作紀錄。 |
| Viewer | 用整理好的畫面資料，以 Ratatui 繪製畫面。 |
| Controller | 把按鍵、滑鼠點擊與命令列選項轉成指令。 |

<details>
<summary>程式碼結構</summary>

程式碼是 8 個 crate 組成的 Rust workspace，圍繞一個共用的契約 crate。

| Crate | 內容 |
|---|---|
| `dgxtop` | 主程式：啟動、命令列與組裝 |
| `dgxtop-core` | 共用型別：指標、單位、指令與介面 |
| `dgxtop-runtime` | 背景程序、IPC、worker 與終端 I/O |
| `dgxtop-adapters` | procfs、sysfs、NVML 與程序操作 |
| `dgxtop-collectors` | 正規化、來源選擇與速率 |
| `dgxtop-store` | 快照、歷史、設定與操作紀錄（SQLite） |
| `dgxtop-manager` | 採樣、設定、指令與畫面資料 |
| `dgxtop-ui` | 畫面、元件與按鍵對應 |

</details>

## 使用方式

```bash
dgxtop                       # 即時監控
dgxtop -i 0.5                # 本次以 500 ms 採樣（不更改已保存的設定）
dgxtop --no-gpu              # 不啟動 NVML host
dgxtop --read-only           # 停用所有程序操作
dgxtop --replay fixture.json # 只用模擬資料、唯讀、使用獨立的暫存狀態
dgxtop --check-config        # 驗證並列出設定值與生效值後結束
dgxtop --diagnostics-json    # 以 JSON 輸出版本、SQLite、能力與健康狀態
dgxtop --import-legacy ~/.config/dgxtop/config.json --dry-run   # 顯示 v1 → v2 的對應
dgxtop --version --verbose   # 版本、commit、工具鏈、target、SQLite runtime
```

| 按鍵 | 動作 |
|---|---|
| `q`、`Ctrl-C` | 離開（編輯篩選時 `q` 是文字；`Ctrl-C` 一律離開） |
| `1`–`5`、`Tab`、`Shift-Tab` | 總覽、GPU、Overview History、Process History、設定 |
| `w` / `W`、`→` / `←` | 下一個／上一個歷史圖表的 bar 大小（1m～24h）；在已開啟的程序中切換它的 bar 大小 |
| `j`/`k`、`Enter` | 在 Process History 選擇程序並開啟或關閉 |
| `e` / `y` | 在開啟的程序中展開完整命令列／複製（OSC 52） |
| `c` | 顯示／隱藏 CPU 感測器與各核心長條（也可點擊 CPU 面板） |
| `j`/`k`、方向鍵、`PgUp`/`PgDn`、`Home`/`End` | 移動選取 |
| `h`/`l`、`←`/`→` | GPU 頁面切換上一張／下一張 GPU |
| `g` | 循環切換程序表的 GPU 篩選（全部 → 各張 GPU） |
| `/` | 篩選程序（Enter 套用、Esc 清除） |
| `s` / `r` | 排序欄位／反轉順序 |
| `+` / `-` | 調整採樣間隔 |
| `K` | 結束選取的程序（需要確認） |
| `F9` | 以 SIGKILL 強制結束選取的程序（需要確認） |
| `[` / `]` | 捲動記憶體面板中的 GPU 程序 |
| `y`/`Enter`、`n`/`Esc` | 確認／取消待執行的程序操作 |
| `F2`、`?`/`F1` | 設定頁、說明 |

命令列參數只在本次執行時覆寫已保存的設定。

## 設定與檔案

| 用途 | 位置 |
|---|---|
| 設定 | `$XDG_CONFIG_HOME/dgxtop/v2/config.json`（`~/.config/dgxtop/v2/config.json`） |
| 歷史，各種 bar 大小（最多 64 MiB） | `$XDG_STATE_HOME/dgxtop/v2/history_tiers.sqlite3`（`~/.local/state/...`） |
| 歷史的近期分鐘資料（最多 7 天、1 GiB） | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3` |
| 程序歷史（最多 64 MiB；`store.process_history_enabled`，未設定時跟著 archive） | `$XDG_STATE_HOME/dgxtop/v2/process_history.sqlite3` 與 `process_recent.json` |
| 程序操作紀錄 | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3` |
| 日誌與執行個體鎖 | `$XDG_STATE_HOME/dgxtop/v2/` |

目錄以 `0700`、檔案以 `0600` 建立。從 v1 升級不會修改 `~/.config/dgxtop/config.json`：先用 `--import-legacy <path> --dry-run` 檢查對應，再用 `--apply` 寫入 v2 設定檔。

## 授權

Apache-2.0，見 [LICENSE](../LICENSE)。
