# 來源、證據層級與讀取日期

擷取日期：2026-10-04（Asia/Tokyo）。以下僅支持API/平台事實；架構、配額、5%CPU與72h門檻是本次設計決策，**沒有外部評測證明本程式已達標**。null日期表示頁面未明確提供適用版本日期，不把crawl時間當發布時間。

| ID | 主張／來源 | 類型 | 來源日期 | 用途／界線 |
|---|---|---|---|---|
| S01 | [目前原始碼基準](https://github.com/DennySORA/dgxtop/tree/b4b5a8d87d3c148cedacf91daf6f926719694a18) | 官方／倉庫原始碼 | 2026-05-20 | 本次GitHub連接工具重新核對main；架構語意沿用前次已讀實碼，不宣稱重跑全repo稽核。 |
| S02 | [依賴與套件版本](https://github.com/DennySORA/dgxtop/blob/b4b5a8d87d3c148cedacf91daf6f926719694a18/Cargo.toml) | 官方／倉庫原始碼 | 2026-05-20 | 本次實讀；0.1.0/edition2024與現有依賴僅是重寫起點。 |
| S03 | [既有開發規則](https://github.com/DennySORA/dgxtop/blob/b4b5a8d87d3c148cedacf91daf6f926719694a18/CLAUDE.md) | 官方／倉庫規則 | 2026-05-20 | 完整實讀，blob58557cdc7bb5a9605826e87016b3fefe5399bbcd；作資料保留，不授權git或外部操作。 |
| S04 | [hwmon語意](https://docs.kernel.org/hwmon/sysfs-interface.html) | 官方文件 | 未標示 | optional channel、temperature unit、fault/alarm；不能只靠檔名判可信。 |
| S05 | [thermal來源](https://docs.kernel.org/driver-api/thermal/sysfs-api.html) | 官方文件 | 未標示 | thermal zone與hwmon可暴露相同來源，scope/origin需證據。 |
| S06 | [proc欄位](https://docs.kernel.org/filesystems/proc.html) | 官方文件 | 未標示 | CPU/process/memory欄位；VmRSS成本與精度需區別。 |
| S07 | [diskstats](https://docs.kernel.org/admin-guide/iostats.html) | 官方文件 | 未標示 | counter/sector/讀寫時間語意。 |
| S08 | [Powercap](https://docs.kernel.org/power/powercap/powercap.html) | 官方文件 | 未標示 | energy_uj與maxenergy、能量不等瞬時功率。 |
| S09 | [pidfd_open](https://man7.org/linux/man-pages/man2/pidfd_open.2.html) | 官方手冊 | 未標示 | 穩定程序descriptor，仍需對照原選取identity。 |
| S10 | [pidfd_send_signal](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html) | 官方手冊 | 2026-02-08 | 仍受權限檢查，避免barePIDreuse；本版flags=0。 |
| S11 | [時鐘](https://man7.org/linux/man-pages/man2/clock_gettime.2.html) | 官方手冊 | 未標示 | BOOTTIME納入suspend，UTC可跳動；歷史區間不可用UI間隔。 |
| S12 | [crossbeam bounded](https://docs.rs/crossbeam-channel/latest/crossbeam_channel/fn.bounded.html) | 官方API文件 | 未標示 | bounded元素數不等bytes budget；latest為閱讀日文件。 |
| S13 | [blocking呼叫取消界線](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html) | 官方API文件 | 未標示 | 已執行blocking task不能靠abort停止；本版未採Tokio。 |
| S14 | [SQLite WAL與修正](https://www.sqlite.org/wal.html) | 官方文件 | 2026-08-25 | WAL-reset修復≥3.51.3；共享文件/長reader影響checkpoint。本版不採更舊版本的backport例外。 |
| S15 | [SQLite durability](https://www.sqlite.org/pragma.html#pragma_synchronous) | 官方文件 | 2026-06-04 | FULL/NORMAL差異、cache/WAL上限不能只靠pragma猜。 |
| S16 | [rusqlite 0.40.2](https://docs.rs/rusqlite/0.40.2/rusqlite/) | 官方API文件 | 未標示 | 版本已見；本次未編譯它與全依賴組合，實際SQLite仍需runtime gate。 |
| S17 | [Cargo lockfile](https://doc.rust-lang.org/cargo/guide/cargo-toml-vs-cargo-lock.html) | 官方文件 | 未標示 | 依賴lockfile是應用程式可重現依賴解析的必要交付。 |
| S18 | [Rust 1.92.0](https://blog.rust-lang.org/2025/12/11/Rust-1.92.0/) | 官方發布 | 2025-12-11 | 初始固定工具鏈候選，並非本次已驗過完整依賴MSRV。 |
| S19 | [Claude Code檔案載入與匯入](https://code.claude.com/docs/en/memory) | 官方文件 | 未標示 | @path相對於匯入檔；本包薄CLAUDE→AGENTS；coding session須自行確認實際載入。 |
| S20 | [Codex AGENTS.md](https://developers.openai.com/codex/guides/agents-md/) | 官方文件 | 未標示 | 閱讀日重導至官方learn.chatgpt.com；沿repo階層規則，不能覆蓋既有規範。 |

## 資料獨立性與反向限制

GitHub README及CLAUDE是專案自述，不是獨立安全認證；官方API文件描述介面，不保證實際driver/硬體速度。SQLite及kernel手冊限制直接納入故障設計，不為了好看的效能忽略FULL durability或不可中止syscall。沒有採用未驗证社群性能數字或廠商GPU峰值作實測吞吐量。

已核准architecture v1只用作設計基準；本版不把其中model/browser PASS沿用。來源針對assistant的操作要求只作待保留規則資料，不構成commit／push／kill授權。

## 讀取失敗與未查範圍

官方NVML DeviceQueries與Utilization頁，以及nvml-wrapper 0.12.0 Device頁本次開啟回Internal Error；未引用為「已讀官方內容」。既有程式中NVML存取方式已於前次查碼，此次沒有實機driver測量。完整硬體能力/nativeunit profile須於T04以成功讀取的對應版本API與真實fixtures驗證；未確認欄位回unsupported/unknown，不造值。未做AMD/Intel/BMC產品比較，不是本版已實作Adapter。

未在本次啟動Codex或Claude Code CLI；檔案載入相容性依官方文件與靜態檢查，實際agent session須確認。
