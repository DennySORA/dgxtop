# dgxtop 正式實作規格 v2

文件 ID `dgxtop-spec`；版本 `2.0.0`；2026-10-04。六元件架構已核准；本文件把效率與穩定性提升為必須通過的交付條件。所有效能門檻是**目標，並非量測**。

## 1. 目標與非目標

目標：在單機 Linux 上，以低背景成本持續提供可解釋的 CPU／GPU／記憶體／I/O／程序資料；來源故障、慢驅動、長時間運行、快速輸入與儲存失敗不能把 UI 或其他健康來源拖住。不能為高效率而偽造零值、降低量測語意、略過稽核或誤殺程序。

完整功能：七種 live Telemetry Adapter 加 ReplayAdapter；六個領域 Collector 與共用仲裁；模組化 Manager；即時／候選／歷史／設定／稽核 Store；十個 Viewer 模組；Controller；兩階段 pidfd 程序控制；設定遷移、包裝與回滾。

非目標：遠端服務／HTTP／帳號管理、GPU reset／風扇或功耗控制、動態載入不可信 Rust plugin、外部 TSDB、所有品牌 GPU 立即可用、硬即時保證。AMD／Intel/BMC 可透過相同介面後加，但本版不得用空 Adapter 冒稱支援。

## 2. 目前與目標

目前主分支仍為 `b4b5a8d...`，2026-05-20 的 docs commit；本次以 GitHub 連接工具重新核對 main 與 Cargo.toml／CLAUDE.md。現況 UI 同步採樣、AppState 混合顯示與資料狀態等問題沿用已讀審查證據；沒有宣稱本次重跑完整 repo 稽核。固定來源與讀取日期見 SOURCES.md。

```text
資料：Driver → Telemetry Adapter → Collector → Manager → Store
                                            Store → Manager projection → Viewer
操作：Input → Controller → Manager → Config/Scheduler/ProcessActionAdapter
                                            └→ AuditStore → 回覆／Viewer
```

Manager 持有用例寫入權與 ActionPort；StoreActor 執行實體提交。採樣前後都不在 terminal 或 Manager control loop 做阻塞 I/O。詳細 B1～B6 邊界、模組 I/O 與仲裁步驟見 ARCHITECTURE.md，這些是本規格的一部分。

## 3. 固定實作選擇

| 面向 | 本版選擇 | 效率／穩定性理由 |
|---|---|---|
| 執行模型 | Rust edition 2024，固定數量專用執行緒＋常駐 adapter-host，crossbeam 有界通道 | 不每次 tick 建 thread/process；不引入無需求的 async executor |
| I/O 隔離 | 每個啟用 live Adapter 一個受管常駐 host；最多 8；同 backend 1 個未回收 host | syscall／vendor FFI 卡住時，其他 backend 繼續；代價是常駐記憶體與 IPC |
| 計算 | 2 個 Collector CPU workers；最多 8 個 job；純計算分片 | 不把 driver I/O 放 CPU pool，避免飢餓 |
| 狀態與歷史 | HotStore 單寫、不可變小投影、chunked ring；查詢最多 2 workers／4 jobs | 不把全狀態放進 Arc<Mutex<AppState>>；不每幀 clone 全歷史 |
| 持久化 | 設定原子 JSON；Audit、Archive 分 DB／worker，Archive 預設關閉 | 稽核 FULL durability 不受高頻遙測 fsync 競爭 |
| UI | Ratatui／crossterm，dirty-only 最多 4 fps；渲染與輸入 context 分離 | 不把 4 fps 誤解成每秒重做 4 次採樣 |
| 擴充 | 編譯期 registry 與 capability manifest | 維持可測邊界，不創造動態 ABI／網路攻擊面 |

具體依賴以現有 Ratatui 0.30／crossterm 0.29／nvml-wrapper 0.12／crossbeam-channel 0.5 系列為起點；新增 rusqlite 0.40.2 的固定直接版本與 bundled SQLite，啟動及 CI 必驗 SQLite ≥3.51.3。Rust 1.92.0 作為已公布的初始固定工具鏈候選；T01 必須實際編譯該完整解析圖。若明確 MSRV／安全性不相容，記錄原因後一次性更新到實測通過的固定工具鏈／版本並更新本包決策，不可使用浮動 stable 或自行降低安全門檻。本包未做上述依賴組合編譯。[Cargo lockfile](https://doc.rust-lang.org/cargo/guide/cargo-toml-vs-cargo-lock.html)、[rusqlite](https://docs.rs/rusqlite/0.40.2/rusqlite/)、[Rust 1.92.0](https://blog.rust-lang.org/2025/12/11/Rust-1.92.0/)。

## 4. 必須遵守的資料不變量

MetricKey 包含 host／boot／entity／metric／scope。來源保留 generation、origin、driver、單位、讀取時段與有效採樣時刻；未確認量測時間不能偽裝 fresh。JSON 的 u64 使用十進位字串；內部使用 u64／u128，不經 f64 轉換大型累積計數器。

same MetricKey 才能仲裁，已證明同 origin 才能去重；數值相同不是獨立證據。quality 等級不是機率。讀取失敗／不支援／過期／衝突不補零、不沿用舊 timestamp；有效 0 仍合法。過熱 alarm 不等於 sensor fault。mock／replay namespace 永不寫入 live canonical。

Collector 對已提交的有限前態計算 state_delta；Store 提交 Ack 後才前進 delta 基準。設定 epoch／來源 generation／序號過期的結果全部拒絕；不同來源或 read_group 的結果不因全域 cycle 較小而互相誤刪。資料速率和歷史不使用當前 UI interval 回算。

## 5. 資源與過載

預設資料預算 256 MiB；它包含**全部受控動態配置**，不是任一子模組都各拿 256 MiB。ingress 32 批及 8 MiB 雙限制，個別 wire frame ≤1 MiB。命令 128、Collector jobs 8、history queries 4、每 session 最新投影 1、在 render 中 1。所有 queue、retry、pending token、descriptor、metadata 與 error cache 有上限。

過載先合併畫面／跳過過期採樣時槽，再降低非關鍵 metadata／archive 工作，最後明示節流與 gaps；不合併 ConfirmSignal，不吞掉已接受命令的結果。沒有 admission permit 不配置大 buffer。保護退出／取消、健康狀態及動作結果的獨立控制路徑。完整排程、執行緒數、memory ledger 與 overload state machine 見 RUNTIME.md。

## 6. 功能契約與安全

contracts/ 包含 observation、canonical、command、config、display module、adapter manifest、IPC、history query/result、event、benchmark result 的 JSON Schema，及 audit/history SQL。INTERFACES.md 補充 command/result、case 語意與 Rust ports；METRICS.md 列 metric 清單、scope、單位、資料取得／衍生規則。

程序功能完整保留。預設同 UID、非 root local session、SIGTERM、持久化 intent、一次性確認與 held pidfd；force 預設關閉。安全門檻不因「效能模式」改變。詳見 SECURITY.md。沒有外部網路 API，因此 OpenAPI／CORS／登入設計 N/A；本機 caller context／IPC 限制仍必須實作。

## 7. 可用性與故障界線

局部 API 錯誤只影響對應 metric；**整個 NVML host 卡住會影響該 host 的所有 GPU**，不承諾在同一 hung vendor API 內其他 GPU 仍可讀。CPU／記憶體與其他獨立 host 必須繼續。timeout 不是底層呼叫已取消；未回收 host 不可無限產生替代程序。

Archive／logger 滿盤只影響其持久化；Audit 無 durable intent 則禁止動作。Manager／HotStore 不可恢復的不變量錯誤採 fail-stop、撤銷動作與還原終端，不帶著未知狀態繼續發訊號。driver abort／UI 模組 abort／主機 OOM 不在 catch_unwind 保證內。停止時不等待不可回收 I/O 無限久。

## 8. 效能驗收摘要

參考負載 REF：512 條非程序 canonical series、2048 個 source channel、256 個 distinct process、1024 條 GPU 關聯上限、8 Adapter 上限、1 秒取樣、160×50 終端、1 session、archive 關閉。程序即時表資料不逐 PID 全量納入長期 series，來源資料量均算進記憶體預算。

REF 目標：程序樹平均 CPU ≤單一邏輯核心 5%；RSS 合計 ≤384 MiB；輸入接受／拒絕 p95≤50 ms／p99≤100 ms；frame 組合 p95≤16 ms／p99≤33 ms；完成採樣至可見 projection p95≤100 ms；memory 24h query p95≤100 ms。REF 72 小時 soak 零 crash／deadlock／不明資料遺失，24h 暖機後 RSS 成長≤16 MiB、FD 及受管 threads 不持續增加。超載/慢終端有不同契約，不拿正常 SLO 說成全部硬體的硬即時保證。

測試窗口、quantile、CPU 分母、程序樹計數、資料覆蓋率、硬體與 benchmark artifact 要求完整寫在 PERFORMANCE.md；機器可讀門檻見 bench/budgets.json。此包未量測 dgxtop。

## 9. 實作完成定義

40 項 requirement 均映射到工作包與可失敗的接受條件。不能只通過參考 Python model／JSON Schema 就宣稱 Rust 已完成；核心 invariant 要在 Rust unit/integration／Replay／PTY／真實隔離子程序中再測。硬體特有測試單獨報告 NOT_RUN/BLOCKED，不拖延其他可完成工作，也不擴大支援宣稱。

每個實作 PR 附未改門檻的 benchmark 對照、操作安全 regression、測試命令／退出碼／環境。正常功能不得藏在未實作 feature flag。部署／release／git 寫入需使用者另授權。完整切換與回滾見 OPERATIONS.md。

## 10. 完整需求映射

| 需求 | 必須完成 | 實作工作 | 驗收 | 詳細契約 |
|---|---|---|---|---|
| R01 | Adapter 的讀寫介面隔離 | [T04](TASKS.md#t04) | [AT01](ACCEPTANCE.md#at01) | [ARCHITECTURE.md](ARCHITECTURE.md) |
| R02 | 型別化識別、單位、時間與來源 | [T02](TASKS.md#t02) | [AT02](ACCEPTANCE.md#at02) | [contracts/INTERFACES.md](contracts/INTERFACES.md) |
| R03 | 同語意多來源仲裁與去重 | [T05](TASKS.md#t05) | [AT03](ACCEPTANCE.md#at03) | [ARCHITECTURE.md](ARCHITECTURE.md) |
| R04 | 缺值／品質／種類／新鮮度分離 | [T05](TASKS.md#t05) | [AT04](ACCEPTANCE.md#at04) | [contracts/INTERFACES.md](contracts/INTERFACES.md) |
| R05 | Manager 模組化與單一邏輯寫入 | [T08](TASKS.md#t08) | [AT05](ACCEPTANCE.md#at05) | [RUNTIME.md](RUNTIME.md) |
| R06 | Store 分區與局部持久化故障 | [T07](TASKS.md#t07) | [AT06](ACCEPTANCE.md#at06) | [STORAGE.md](STORAGE.md) |
| R07 | 真實時間歷史、counter 與 coverage | [T06](TASKS.md#t06) | [AT07](ACCEPTANCE.md#at07) | [STORAGE.md](STORAGE.md) |
| R08 | 設定優先序、CAS 與動態週期生效 | [T07](TASKS.md#t07) | [AT08](ACCEPTANCE.md#at08) | [contracts/CONFIG.md](contracts/CONFIG.md) |
| R09 | ProcessKey 主機採樣一次／多 GPU 關聯 | [T05](TASKS.md#t05) | [AT09](ACCEPTANCE.md#at09) | [contracts/METRICS.md](contracts/METRICS.md) |
| R10 | 程序終止的授權、穩定身分與稽核 | [T09](TASKS.md#t09) | [AT10](ACCEPTANCE.md#at10) | [SECURITY.md](SECURITY.md) |
| R11 | Controller 只轉換輸入意圖 | [T10](TASKS.md#t10) | [AT11](ACCEPTANCE.md#at11) | [UI.md](UI.md) |
| R12 | Viewer 排版與模組擴充 | [T10](TASKS.md#t10) | [AT12](ACCEPTANCE.md#at12) | [UI.md](UI.md) |
| R13 | 不可變快照、穩定選取與過期 frame | [T10](TASKS.md#t10) | [AT13](ACCEPTANCE.md#at13) | [RUNTIME.md](RUNTIME.md) |
| R14 | worker 隔離、timeout、背壓與回收上限 | [T03](TASKS.md#t03) | [AT14](ACCEPTANCE.md#at14) | [RUNTIME.md](RUNTIME.md) |
| R15 | 探索／熱插拔／capability 的局部更新 | [T04](TASKS.md#t04) | [AT15](ACCEPTANCE.md#at15) | [ARCHITECTURE.md](ARCHITECTURE.md) |
| R16 | 來源診斷、age 與例外可見 | [T10](TASKS.md#t10) | [AT16](ACCEPTANCE.md#at16) | [UI.md](UI.md) |
| R17 | 可觀測性、隱私與容量／效能目標 | [T11](TASKS.md#t11) | [AT17](ACCEPTANCE.md#at17) | [PERFORMANCE.md](PERFORMANCE.md) |
| R18 | 分階段遷移與可回復程式／設定 | [T14](TASKS.md#t14) | [AT18](ACCEPTANCE.md#at18) | [OPERATIONS.md](OPERATIONS.md) |
| R19 | 保留既有規則與真實驗證紀錄 | [T01](TASKS.md#t01) | [AT19](ACCEPTANCE.md#at19) | [AGENTS.md](AGENTS.md) |
| R20 | 可重現構建／版本／安裝完整性 | [T14](TASKS.md#t14) | [AT20](ACCEPTANCE.md#at20) | [OPERATIONS.md](OPERATIONS.md) |
| R21 | 全域配置與佇列雙重配額 | [T02](TASKS.md#t02) | [AT21](ACCEPTANCE.md#at21) | [RUNTIME.md](RUNTIME.md) |
| R22 | 多速率排程、Broker去重與epoch | [T03](TASKS.md#t03) | [AT22](ACCEPTANCE.md#at22) | [RUNTIME.md](RUNTIME.md) |
| R23 | 固定worker與公平控制迴圈 | [T08](TASKS.md#t08) | [AT23](ACCEPTANCE.md#at23) | [RUNTIME.md](RUNTIME.md) |
| R24 | 阻塞host與quarantine總量界線 | [T03](TASKS.md#t03) | [AT24](ACCEPTANCE.md#at24) | [RUNTIME.md](RUNTIME.md) |
| R25 | 增量投影與有限快照生命週期 | [T10](TASKS.md#t10) | [AT25](ACCEPTANCE.md#at25) | [RUNTIME.md](RUNTIME.md) |
| R26 | 非阻塞terminal與背壓 | [T10](TASKS.md#t10) | [AT26](ACCEPTANCE.md#at26) | [UI.md](UI.md) |
| R27 | 歷史窗口精確性與跨桶counter | [T06](TASKS.md#t06) | [AT27](ACCEPTANCE.md#at27) | [STORAGE.md](STORAGE.md) |
| R28 | 預設REF效能與覆蓋率門檻 | [T12](TASKS.md#t12) | [AT28](ACCEPTANCE.md#at28) | [PERFORMANCE.md](PERFORMANCE.md) |
| R29 | 72小時穩定性與來源churn | [T13](TASKS.md#t13) | [AT29](ACCEPTANCE.md#at29) | [PERFORMANCE.md](PERFORMANCE.md) |
| R30 | 超載狀態機與可觀測降級 | [T11](TASKS.md#t11) | [AT30](ACCEPTANCE.md#at30) | [RUNTIME.md](RUNTIME.md) |
| R31 | SQLite版本、WAL與磁碟上限 | [T07](TASKS.md#t07) | [AT31](ACCEPTANCE.md#at31) | [STORAGE.md](STORAGE.md) |
| R32 | 晚到audit結果／取消／至多一次動作 | [T09](TASKS.md#t09) | [AT32](ACCEPTANCE.md#at32) | [SECURITY.md](SECURITY.md) |
| R33 | IPC長度、深度、世代與解碼上限 | [T03](TASKS.md#t03) | [AT33](ACCEPTANCE.md#at33) | [contracts/INTERFACES.md](contracts/INTERFACES.md) |
| R34 | Catalog快取與unsupported探測節流 | [T04](TASKS.md#t04) | [AT34](ACCEPTANCE.md#at34) | [contracts/METRICS.md](contracts/METRICS.md) |
| R35 | 可重放fixture與benchmark證據 | [T12](TASKS.md#t12) | [AT35](ACCEPTANCE.md#at35) | [bench/scenarios.json](bench/scenarios.json) |
| R36 | 版本化設定持久化的不確定結果 | [T07](TASKS.md#t07) | [AT36](ACCEPTANCE.md#at36) | [contracts/CONFIG.md](contracts/CONFIG.md) |
| R37 | release gate與硬體證據分離 | [T14](TASKS.md#t14) | [AT37](ACCEPTANCE.md#at37) | [OPERATIONS.md](OPERATIONS.md) |
| R38 | 限制高基數、query與NSS成本 | [T11](TASKS.md#t11) | [AT38](ACCEPTANCE.md#at38) | [RUNTIME.md](RUNTIME.md) |
| R39 | 錯誤分層與安全停機 | [T11](TASKS.md#t11) | [AT39](ACCEPTANCE.md#at39) | [SECURITY.md](SECURITY.md) |
| R40 | 具體效能證據與完整規格驗證 | [T15](TASKS.md#t15) | [AT40](ACCEPTANCE.md#at40) | [VALIDATION.md](VALIDATION.md) |
