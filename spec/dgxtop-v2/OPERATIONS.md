# 建置、操作、遷移與回滾

## 1. 目錄與首次啟動

設定：`$XDG_CONFIG_HOME/dgxtop/v2/config.json`，fallback `$HOME/.config/dgxtop/v2/config.json`。狀態：`$XDG_STATE_HOME/dgxtop/v2/`，fallback `$HOME/.local/state/dgxtop/v2/`；audit.sqlite3、可選history.sqlite3、bounded log與instance.lock。live／replay使用不同namespace與不同根目錄。XDG值須為absolute，否則診斷後使用明確fallback；不擅自寫相對路徑。

啟動順序：解析CLI→安全路徑／lock→validate config/admission→檢查SQLite版本/schema→啟動必要Store workers→Manager／broker→adapters capability discovery→先發布無GPU亦可用畫面→逐來源warmup。啟動GPU失敗不阻止CPU／RAM功能；必要config格式錯誤提供錯誤位置並退出，不能静默忽略整份設定。

## 2. 必須提供的CLI

| 命令／旗標 | 語意 |
|---|---|
| `dgxtop` | 本機TUI，schema2設定，normal profile |
| `--interval <seconds>`／`--no-gpu`／`--theme <name>` | 保留既有相容入口，只有明確指定才override |
| `--read-only` | 禁止全部外部程序動作；功能仍實作存在 |
| `--config <absolute-path>`／`--state-dir <absolute-path>` | 規範owner、symlink、namespace安全驗證 |
| `--check-config` | 讀取／驗證／輸出configured/effective預測，不啟動採樣、不寫設定 |
| `--diagnostics-json` | 版本、dependency、sqlite、capabilities與有界health摘要；去秘密 |
| `--import-legacy <path> --dry-run` | 顯示完整mapping與差異，預設不落盤 |
| `--import-legacy <path> --apply` | 明確接受後另寫v2，existing v2存在時拒絕覆寫 |
| `--replay <fixture.json>` | simulated-only，各種注入受白名單限制；强制read-only、獨立temp state |
| `--version --verbose` | package version、commit、toolchain、SQLite runtime、target、build profile |

內部`--adapter-host`只接受composition root建立的pipe handshake；直接執行它不能成為公開任意檔案讀取／動作入口。benchmark harness為單獨的測試binary，規格見PERFORMANCE.md，不在正式UI偷偷打開networkserver。

legacy mapping：update_interval_secs→sampling.interval_ms（round到整ms後範圍檢查）；color_theme→viewer.theme（舊cyan/green/amber映射可用主題）；gpu_enabled→CLI/session adapter enable或新config選擇nvml；history_length→raw_history_seconds按舊interval換算，顯示不等價警告；network_max_visible→只影響UI layout，不當硬cap；redline_threshold→只影響顯示警戒、不當資料有效範圍。未知欄位報unsupported，使用者apply前必看migration report。實作不得捨棄舊欄位而宣稱完全相容。

## 3. 觀測與故障處置

關鍵metrics：adapter_read_duration_ms、source_age_ms、collector_duration_ms、manager_command_ack_ms、commit_to_projection_ms、queue_items/queue_bytes、queue_dropped_total{bounded_reason}、history_coverage_ratio、archive_gap_total、action_unknown_total、process_tree_rss_bytes、owned_allocation_bytes、worker_threads、open_fds、frame_compute_ms、tty_flush_ms。

counter labels僅adapter/group/reason等有界集合；不把PID／cmdline／source完整路徑當metric labels。日誌default info但高頻成功採樣不逐筆記錄；相同source錯誤每30秒summary，log queue滿跳過debug但保留bounded error counters。stats在記憶體維持，不需要為監控自身每秒執行外部command。

source失敗：查看來源診斷的read/measurement freshness和scope，不調低安全validator来讓值出現。NVML quarantined：先看timeout與子程序狀態；禁止無限restart；需要OS/driver介入時明示。Archive滿：停止archive並保留即時，依retention清理／maintenance checkpoint。Audit滿或損毁：停用動作，備份後由使用者檢查；不得自動刪掉未到期audit讓kill繼續。

## 4. 遷移與回滾流程

保留舊binary與設定，先在新的state-dir跑readonly影子比較。比較相同scope來源，不以舊版「0=失敗」作新版本必須一致的golden truth。新架構通過contracts、Replay、PTY、action-disposable與硬體能力測試後，才能在該硬體完成release驗收。

安裝時下載到同一目標filesystem的temporary path，驗證manifest/checksum／target、解壓允許單一dgxtop檔名（拒絕絕對路徑、..、symlink），執行`--version --verbose`驗證架構與loader，再原子rename；失敗保留原binary。checksum驗證不能替代簽章／供應鏈可信任性，但不應忽略現有SHA256SUMS。不能從GNU下載失敗自動換musl而聲稱GPU支援等同。

回滾：停新程序、恢復舊binary指向與舊設定路径；v2 DB原樣保留只供診斷，不讓舊版改寫。已送signal不可回滾。新binary不同schema启动时不自动覆蓋舊檔。任何switch／commit／push／release實際操作都須使用者另行授權；本規格不執行。

## 5. CI／發行閘門

T01固定toolchain和Cargo.lock，後續`cargo check/test/build --locked`。保留原fmt、Clippy `-D warnings`，加dependency-boundary腳本、schema、unit、Replay、PTY、fault-injection、可拋棄child程序控制、SQLite crash/reopen與benchmark evaluator。兩個主要target為x86_64-unknown-linux-gnu與aarch64-unknown-linux-gnu；musl只能標system-only實驗性、未測NVML不得自動fallback。

在同一個release artifact實測SQLite runtime≥3.51.3；記錄transitive `libsqlite3-sys`與實際amalgamation checksum，不只看rusqlite版本。Release／package／CLI版本一致，tag須指向已通過gate的commit，不能CI build green就略過target-hardware tests。SBOM/license/advisory由鎖定依賴生成；Actions／cross builder與容器固定SHA/digest，權限最小化。SDK/driver不打包到release內。

perf／soak需專用runner或人工hardware report，普通共享CI只作功能与相對回歸訊號。release artifact附基準報告、capability矩陣、測試未通過／未涵蓋清單及rollback說明。不得將本包Python單元測試當作Rust或GPU驗收。
