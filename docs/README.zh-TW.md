# dgxtop

[English](../README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

> 給 NVIDIA DGX 與 GPU Linux 主機使用的穩定、高效率本機終端監控工具：CPU、GPU、記憶體、
> 磁碟、網路與程序——具備可解釋的多來源仲裁、精確歷史與受保護的程序控制。

dgxtop v2 依照 [`spec/dgxtop-v2/`](../spec/dgxtop-v2/README.md) 規格完整重寫。它不會顯示假的 0：
讀不到的值會標示為 *unsupported*（不支援）、*read failed*（讀取失敗）、*stale*（過期）或
*source conflict*（來源衝突），並附上資料來源。

## 特色

- **隔離且不阻塞的採集。** 每個 live 資料來源（procfs、hwmon、thermal、powercap、NVML、network、
  filesystem）都在各自常駐的 adapter-host 程序中執行。驅動呼叫卡住（例如 NVML 卡死）只影響該 host，
  CPU、記憶體與 UI 照常更新；逾時的 host 以有上限的退避重啟或隔離（quarantine），不會無限重生。
- **可解釋的來源仲裁。** 兩個來源回報同一指標時（例如 CPU package 的 coretemp 與 thermal zone），
  dgxtop 會去除已證明的同源別名、只比較獨立來源，差距超過容差時顯示「來源衝突」，不平均也不默默挑一個。
  *Sources* 頁列出每個候選、被選上或排除的原因、age 與來源身分。
- **誠實的單位與品質。** 讀取狀態、資料種類（量測／衍生／估算／模擬）、品質與新鮮度是分開的軸。
  未經證實的單位（例如 NVML PCIe 的「KB/s」）不靠猜測換算，也不從使用率百分比捏造頻寬。
- **精確的歷史。** 速率來自精確的 `u64` counter 差值除以真實經過時間；時窗總量只計入完全落在窗內的區間
  （跨分鐘的區間以精確的 *bridge* 保存）。空窗、覆蓋率與來源切換都看得到。
- **有界的資源。** 單一全域受管記憶體預算（預設 256 MiB），每條佇列都有位元組與筆數上限；固定數量的執行緒；
  只在有變化時重繪且最多 4 fps；終端輸出不阻塞，終端很慢時也不會卡住輸入。
- **受保護的程序控制。** `K` 對選取的程序請求 SIGTERM：僅限同使用者、root 會話一律拒絕、永遠不對 PID 1／
  dgxtop 本身／其 host 動作；需要 10 秒內有效的一次性確認；送訊號**之前**先寫入持久化稽核；訊號透過預先持有的
  `pidfd` 送出（沒有 PID 重用競態）。`SIGKILL` 預設停用。結果會區分「已送出訊號」與「已觀察到結束」。

## 畫面與模組

十個顯示模組：`cpu`、`gpu`、`memory`、`disk`、`network`、`processes`、`history`、`sources`、
`settings`、`actions`。分頁為 Overview、GPU、History、Settings。待確認的程序操作會在每個頁面以橫幅顯示，結果（已送出、仍在執行、已結束、未知）以通知顯示。

- **Overview：** 由上到下四列。
  - **CPU 與記憶體並排，記憶體較寬。** CPU 顯示溫度、功耗與最高頻率（放得下時同一行接著負載與工作數）、使用率長條與其下方的圖，以及 usr/sys/iowait/steal 拆分；記憶體顯示使用量、used 長條與其下方的圖、swap、buffers、cache 與可用記憶體。點擊 CPU 面板或按 `c` 會加入所有感測器與各核心長條。統一記憶體 GPU（DGX Spark）會以不同顏色標出 GPU 程序佔用的 RAM，並寫出用量。
  - **GPU。** 每張 GPU 一行溫度、功耗、時脈與風扇，接著是使用率長條與其下方的圖，再來是記憶體長條（`VRAM`；統一記憶體則為 `RAM`，即 GPU 程序佔用的 RAM）；終端較矮時 GPU 優先取得空間。
  - **磁碟 I/O 與網路並排。** 各自是一張表格（磁碟會帶出所在檔案系統的容量），接著是最忙裝置（標題所示）的讀寫或收送速率。
  - **GPU 程序表**佔用剩下的列，只列有使用 GPU 的程序；`TYPE` 欄照 `nvidia-smi` 的寫法標出程序在該 GPU 上是運算（`C`）、圖形（`G`）還是兩者都有（`C+G`）。選取、排序、篩選、終止都在這裡操作。

  圖表以快速閱讀為目標：方塊欄位，每欄一次取樣、最新在最右，每一欄都套用長條在該數值的顏色，忙碌的時段一眼就是紅色。使用率與記憶體使用固定刻度；磁碟與網路速率依自身峰值縮放，峰值直接標在旁邊。
- **GPU：** 一次看一張 GPU：上方四條即時長條（使用率、記憶體、溫度、功耗），下方是這張 GPU 全部即時與深度資訊。資料全部從 NVIDIA 驅動（NVML）與驅動、CUDA toolkit 的檔案讀取，不連網、不執行任何指令。資訊依分類排成由上往下讀的「名稱　值」區塊，需要比較的值上下對齊，並依終端大小排成最少且高度平衡的欄數：
  - **State：** P-state、throttle 原因、各原因累計降頻時間。
  - **Utilization：** 記憶體控制器、編碼器、解碼器。
  - **Clocks：** graphics、SM、memory、video 時脈，目前值與最高值並排。
  - **Memory：** 總量、已用、free、reserved、程序記憶體、BAR1、匯流排寬度與理論峰值頻寬。
  - **PCIe：** link 與理論頻寬的目前值與最高值並排，transmit 在 receive 正上方，replay。
  - **Thermal：** 溫度與風扇（多風扇的 GPU 會列出每一顆），接著是裝置自身的溫度門檻，由低到高排列。
  - **Power：** 功耗與驅動的瞬時讀值（Ampere 以後的 GPU，功耗是 NVML 的 1 秒平均）、上限、其他限制壓低生效上限時另列設定的上限、預設上限、可設定範圍、自驅動程式載入以來的能耗。
  - **CUDA：** 驅動支援的 CUDA 版本、compute capability（`sm_120`）、已安裝的 toolkit、路徑與 `nvcc`。
  - **Driver：** 版本、open／proprietary kernel module、NVML、persistence、compute mode。
  - **Device、PCI & firmware：** 架構、品牌、CUDA cores、board part、序號、UUID、裝置節點；PCI 身分、VBIOS、InfoROM、GSP 韌體。
  - **Health：** GPU 有提供時的 ECC、retired pages 與 NVLink 計數。
  - **Topology：** 離 GPU 最近的 NUMA node 與 CPU，以及到每張其他 GPU 的 PCIe 路徑（`nvidia-smi topo -m` 的 `PIX` 到 `SYS`）和能否直接讀寫對方顯存（P2P）。不支援 P2P 時，GPU 之間的複製會經過系統記憶體。

  GPU 沒有提供的資料不佔列；終端太矮、放不下全部區塊時，上方會列出未顯示的區塊。各 GPU 的程序列在 overview 的程序表（按 `g` 只看一張 GPU）。
- **History：** overview 的各項資源在更長時間範圍內的變化（5 分鐘到 24 小時，`w`/`W` 切換），排成監控儀表板：面板兩兩並排——CPU 與記憶體、GPU 使用率與記憶體、GPU 溫度與功耗（每張 GPU 一條），以及最忙磁碟的讀寫與網卡的收送（每組共用一個刻度）。每一條都有刻度、時間窗內的方塊欄位（平均值依等級上色，峰值以淡色帶畫在上方，沒有紀錄的時段顯示虛線底線），以及「目前／平均／峰值」圖例；每欄面板下方有時間軸。歷史會存到磁碟（`history.sqlite3`，預設開啟）：之前幾次執行與重開機前的資料會讀回，畫在同一條時間軸上；視窗列會標出時間窗已記錄多久（只在 dgxtop 執行時記錄）。時間軸下方是每張 GPU 最近 1、6、12、24 小時的統計：使用率、記憶體、溫度、功耗的平均與峰值及能耗，只列出確實有新增資料的時間窗。設計分析見 [docs/process/v2-rewrite/history-redesign.md](process/v2-rewrite/history-redesign.md)。

版面依終端大小調整：

- 80×24 以上顯示完整版面；
- 小於 80×24 顯示精簡摘要；
- 小於 40×10 顯示尺寸提示（仍可按 q 離開）。

## 安裝

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

安裝程式會把 x86_64 或 aarch64 的 GNU 版本下載到目標檔案系統上的暫存目錄，驗證 `SHA256SUMS` 與壓縮檔結構，
執行 `dgxtop --version --verbose` 確認可在本機載入，再以原子方式替換執行檔，並把舊版保留為 `dgxtop.previous`。
可用 `./install.sh --rollback` 回復（不會動到設定與狀態；已送出的訊號無法撤回）。不會自動改用 musl 版本。

從原始碼建置（`rust-toolchain.toml` 固定 Rust 1.92.0）：

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release --locked
./target/release/dgxtop
```

## 使用方式

```bash
dgxtop                       # 即時監控
dgxtop -i 0.5                # 本次執行採樣 500 ms（不會改動已儲存的設定）
dgxtop --no-gpu              # 不啟動 NVML host
dgxtop --read-only           # 停用所有程序操作
dgxtop --replay fixture.json # 只用模擬資料、唯讀、獨立的暫存狀態
dgxtop --check-config        # 驗證並列出設定值與實際生效值後離開
dgxtop --diagnostics-json    # 以 JSON 輸出版本、SQLite、能力與健康摘要
dgxtop --import-legacy ~/.config/dgxtop/config.json --dry-run   # 顯示 v1 → v2 對照
dgxtop --version --verbose   # 版本、commit、工具鏈、target、SQLite runtime
```

命令列參數只有在明確指定時才會覆寫已儲存的設定，而且只對本次執行有效。

### 按鍵

| 按鍵 | 動作 |
|---|---|
| `q`、`Ctrl-C` | 離開（編輯篩選字串時 `q` 是文字；`Ctrl-C` 永遠可以離開） |
| `1`–`4`、`Tab`、`Shift-Tab` | Overview、GPU、History、Settings |
| `w` / `W` | 下一個／上一個 History 時間範圍（5m、1h、6h、12h、24h） |
| `c` | 在 Overview 顯示／隱藏 CPU 感測器與各核心長條（或點擊 CPU 面板） |
| `j`/`k`、方向鍵、`PgUp`/`PgDn`、`Home`/`End` | 移動選取（依程序身分，不是列索引） |
| `h`/`l` | GPU 畫面切換上一張／下一張 GPU |
| `g` | 切換程序表的 GPU 篩選（全部 → 每張 GPU） |
| `/` | 篩選程序（Enter 套用、Esc 清除） |
| `s` / `r` | 排序欄位／反向排序 |
| `+` / `-` | 調整採樣間隔（由 store 確認寫入後才生效） |
| `K` | 對選取的程序請求 SIGTERM（需要確認） |
| `y`/`Enter`、`n`/`Esc` | 確認／取消待處理的程序操作 |
| `F2`、`?`/`F1` | Settings 畫面、說明 |

## 設定與檔案

| 用途 | 位置 |
|---|---|
| 設定（schema 2、具版本、原子寫入） | `$XDG_CONFIG_HOME/dgxtop/v2/config.json`（`~/.config/dgxtop/v2/config.json`） |
| 程序操作稽核（SQLite，`synchronous=FULL`） | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3`（`~/.local/state/...`） |
| 歷史封存（預設開啟，保留 7 天、最多 1 GiB） | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3` |
| 有上限的日誌、instance lock | `$XDG_STATE_HOME/dgxtop/v2/` |

目錄以 `0700`、檔案以 `0600` 建立。完整命令列、環境變數與 token 永遠不會落盤。完整的設定契約見
[`spec/dgxtop-v2/contracts/CONFIG.md`](../spec/dgxtop-v2/contracts/CONFIG.md)，預設值見
[`spec/dgxtop-v2/examples/config.default.json`](../spec/dgxtop-v2/examples/config.default.json)。

從 v1 升級：舊的 `~/.config/dgxtop/config.json` 永遠不會被修改。先用 `--import-legacy <path> --dry-run`
檢查對照，再用 `--apply` 寫出 v2 設定檔。

## 架構

```text
資料：驅動／作業系統 → adapter host（Adapter）→ Collector → Manager → Store
      Store → Manager 投影 → Viewer
操作：輸入 → Controller → Manager → 設定／排程／pidfd 操作 adapter → 稽核 → Store → Viewer
```

| Crate | 角色 |
|---|---|
| `dgxtop-core` | 型別化身分、單位、品質軸、命令、事件、設定、位元組預算、port |
| `dgxtop-runtime` | adapter-host 監管與 IPC framing、有界 worker、不阻塞的終端輸出 |
| `dgxtop-adapters` | 唯讀 adapter（procfs、hwmon、thermal、powercap、NVML、network、filesystem、replay）與獨立的 pidfd 操作 adapter |
| `dgxtop-collectors` | 正規化、同源去重、仲裁、counter 速率、程序／GPU 關聯 |
| `dgxtop-store` | 熱快照、有界精確歷史、設定 repository、SQLite 稽核與封存 |
| `dgxtop-manager` | 排程、catalog、政策、資料提交、歷史、設定、程序操作、投影、會話 |
| `dgxtop-ui` | 顯示模組、版面、按鍵對應、controller、終端執行迴圈 |
| `dgxtop` | composition root 與 CLI |

只有 composition root 會依賴 `dgxtop-core` 以外的 workspace crate；此邊界由
[`spec/dgxtop-v2/tools/check_boundaries.py`](../spec/dgxtop-v2/tools/check_boundaries.py) 檢查。

## 開發

```bash
just ci        # fmt、check、clippy -D warnings、測試、依賴邊界（全部 --locked）
just e2e       # 需要時才跑的端對端測試：PTY、故障注入、真實的 host 子程序
just hardware  # 真實 procfs/sysfs/NVML 的硬體測試（pidfd 只對可拋棄的子程序）
```

## 驗收狀態

每個驗收案例（AT01–AT40）的命令、退出碼、環境與證據雜湊見
[`docs/validation/VALIDATION.md`](validation/VALIDATION.md)。規格中的效能數字是驗收目標；只有該報告列出實測證據的平台才稱為已驗證。

## 授權

Apache-2.0，見 [LICENSE](../LICENSE)。
