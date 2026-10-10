# dgxtop 實作規則

## 範圍與閱讀

本資料夾是完整重寫的實作規格。先讀目標repo既有AGENTS.md、CLAUDE.md及作用中的子目錄規則；本包不能覆寫或削弱它們。讀 README.md → SPEC.md → ARCHITECTURE.md → RUNTIME.md → STORAGE.md → PERFORMANCE.md → contracts/ → SECURITY.md/UI.md → TASKS.md/ACCEPTANCE.md。其後直接實作與測試，不再只交提案或縮小成MVP。

六元件＋安全kill＋全部10顯示模組是已批准範圍。保留原本Linux TUI定位。viewer/controller沒有driver、StoreWriter、ActionPort；Manager各模組只有最小所需權限；core不能依賴NVML、libc、SQLite或UI。依賴圖由tools/check_boundaries.py驗證。

## 必守

遵守SOLID、簡單單一職責、可注入I/O/clock、handler不放domain規則；但不為每小函式建crate或為簡单同步需求加複雜executor。所有熱路徑、快取、queue與Arc生命週期必有byte/item上限；不每tick新建thread/process。不接受unchecked型別轉換、偽造0或假信任百分比。

程序操作只用明確權限、held pidfd、token/政策重核與durableintent。不得為效能省audit fsync、偷用root或退回kill(PID)。测试只用自建可丟棄子程序，未授權不觸碰使用者工作、硬體設定、遠端帳號或部署。

每bug先寫可失敗case；每新行為至少成功/失敗/邊界，unit測試不得依賴真驅動或網路。Rust property、integration、PTY、故障注入、REF與72hsoak分層。保留版本鎖，CI不浮動stable。

## 驗證與交付

cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked

效能依bench/budgets.json，不能未經使用者同意改門檻。附raw報告、環境、命令、退出碼與hash。未跑項標NOT_RUN／缺硬體標BLOCKED；不可將Python/HTML/編譯成功當GPU或pidfd實機PASS。直到40個AT都有實際處理狀態才封版。

## Git與安全

不預設取得commit/push/tag/merge/deploy權。確有授權才依原repo英文Conventional Commits、scope、imperative、What/Why/Tests與breaking註記；不推測email、不擅改git identity。秘密與完整cmdline不得提交/落盤。保留使用者未提交變更，不做破壞性reset或刪檔。

## 版本與文件

Markdown與contracts為權威；發現不一致先以最小修訂對齊並記DECISIONS，不能隨便選較易實作版本。CLAUDE.md僅匯入本檔；不得形成循環。既有規則摘要見baseline/EXISTING_RULES.md。
