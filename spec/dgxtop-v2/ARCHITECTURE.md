# 已核准架構：邊界與功能

文件 ID：`dgxtop-spec`；版本 `2.0.0`；基準 `b4b5a8d87d3c148cedacf91daf6f926719694a18`；核對日期 2026-10-04。

## 1. 設計結論與範圍

採用 **六個主要元件、兩條路徑、單一邏輯寫入權**。

資料路徑：`驅動／作業系統 → Telemetry Adapter → Collector → Manager → Store → Manager 的查詢投影 → Viewer`。

操作路徑：`鍵盤／滑鼠／CLI → Controller → Manager → 設定／排程／受授權的 Action Adapter → 操作結果與稽核 → Store → Viewer`。

Manager 統籌系統，但不是包辦所有演算法的巨型物件。Collector 判斷「這一筆觀測可以怎麼解讀」；Manager 判斷「何時採樣、採用哪份政策、誰能執行什麼操作」；Store 管理儲存機制；Viewer 決定呈現；Controller 轉換使用者意圖。

**本規格完整範圍**是六元件完整重寫；本次交付為可實作的規格包，而非軟體。所有約定功能均必須實作，不能縮成唯讀或 mock-only 版本。不是本次直接實作 Rust 成品。維持本機 Linux、Rust、Ratatui 終端程式；不新增 Web 伺服器、遠端控制、使用者帳號系統或分散式資料庫。擴充點必須存在，但不宣稱所有未測驅動已可用。

## 2. 現況與要拆開的責任

目前 `src/ui/terminal.rs` 在 UI 迴圈內執行 `collect_all`；`src/app/worker.rs` 主要產生輸入與 Tick；`AppState` 同時放資料、歷史與 UI 狀態。CPU 溫度來源以 thermal zone 的名稱與掃描順序挑選，最後甚至退到第一個 zone。[原始終端迴圈](https://github.com/DennySORA/dgxtop/blob/b4b5a8d87d3c148cedacf91daf6f926719694a18/src/ui/terminal.rs)、[事件執行緒](https://github.com/DennySORA/dgxtop/blob/b4b5a8d87d3c148cedacf91daf6f926719694a18/src/app/worker.rs)、[狀態](https://github.com/DennySORA/dgxtop/blob/b4b5a8d87d3c148cedacf91daf6f926719694a18/src/app/state.rs)、[CPU 溫度](https://github.com/DennySORA/dgxtop/blob/b4b5a8d87d3c148cedacf91daf6f926719694a18/src/collectors/cpu.rs#L145-L176)。

新架構不是只把現有 Collector 改名：必須把驅動 I/O、物理語意、多來源仲裁、排程、歷史統計與呈現分成可獨立測試的邊界。下列模組、預設值與效能數字均為**設計要求／待實測目標**，不是現有軟體功能或量測結果。

## 3. 六個主要元件

| 元件 | 負責 | 輸入 → 輸出 | 明確禁止 |
|---|---|---|---|
| Adapter | 對接一種 API／子系統；探索資源、能力與原生數值；保留錯誤與來源 | ReadPlan → ObservationBatch | 自選權威數值、跨來源平均、直接寫 Store、渲染 |
| Collector | 依 CPU／GPU 等領域整理資料；正規化、驗證、去重、仲裁、衍生值 | 多個 ObservationBatch＋政策＋有限前態 → CollectionBatch | 驅動私有呼叫、直接 SQL、設定永久修改、kill |
| Manager | 排程、政策、模組生命週期、命令授權、資料提交、查詢投影 | Commands／CollectionBatch → Results／ViewModel／Store operations | UI 排版、在事件迴圈等待阻塞驅動、任意 SQL 暴露 |
| Store | 當前狀態、來源、歷史、設定、稽核的儲存與索引 | 型別化讀寫 → 快照／時窗／提交結果 | 自行選感測器、執行命令、主動控制驅動 |
| Viewer | 排版、可擴充顯示模組、繪製與圖表互動區域 | 不可變 ViewModel＋Viewport → Frame＋HitMap | 自行採樣、直接讀寫 Store、計算權威速率、授權操作 |
| Controller | 輸入、焦點與按鍵映射；轉為穩定 ID 的意圖命令 | RawInput＋InputContext → Command | 判定採信來源、直接發訊號、變更資料紀錄、繪製 |

名稱統一採 `Controller`。六者共用 `core` 的資料型別與埠介面；`core` 是跨層契約，不是第七個業務總管。

## 4. Adapter：以驅動介面分，不以畫面分

### 4.1 計畫納入的唯讀 Adapter

| Adapter | 介面／來源 | 提供能力 | 不應做的推論 |
|---|---|---|---|
| ProcfsAdapter | `/proc/stat`、`meminfo`、`diskstats`、PID metadata | CPU 計數器、主機 RAM、磁碟累積 I/O、程序主機資源 | 讀不到不補 0；不將不同 GPU 的同 PID 重算 CPU |
| HwmonAdapter | Linux hwmon；可替換為 libsensors frontend | 溫度、風扇、功率與 fault／label／更新週期 | `temp1` 不必然是 CPU；不能對所有 temp 通道盲目除 1000 |
| ThermalZoneAdapter | `/sys/class/thermal` | zone 類型、溫度、有效 trip metadata | SoC／ACPI zone 不自動等於 CPU package |
| PowercapAdapter | Linux powercap 能量計數器 | package／其他 power domain 的能量與範圍 | platform power 不冒充 CPU package power |
| NvmlAdapter | NVIDIA NVML 驅動 API | GPU 裝置、支援的指標、程序關係、拓樸 | 不支援不等於 0；不以產品名稱猜已量到的吞吐量 |
| NetworkAdapter | netlink／sysfs 可替換 frontend | 網卡識別、bytes／packets 累積值、狀態 | 介面重建不接續舊基準 |
| FilesystemAdapter | mountinfo＋statvfs | 檔案系統容量、掛載與 block-device 關係 | 一個裝置不只一個 mount；不把 mount 空間當物理裝置總量 |
| ReplayAdapter | 本包合成 fixtures／明確錄製資料 | 可重現故障與時間序列 | 必須標示 `simulated`／`replay`，不混入 live namespace |

以上是實作目標與界面分工，不是已寫好的驅動成品。AMD／Intel GPU、BMC 或其他資料來源可另註冊 Adapter；新增功能只擴充 manifest、profile 與測試，不需要改 Viewer 核心。

Linux 文件指出感測器通道的接線與用途沒有統一保證，某些值還需使用者空間轉換；且 hwmon 和 thermal 有可能暴露同一個實體來源。設計因此保留 driver、device path、channel 與物理來源識別，不只保存溫度值。[hwmon 官方介面](https://docs.kernel.org/hwmon/sysfs-interface.html)、[thermal 官方介面](https://docs.kernel.org/driver-api/thermal/sysfs-api.html)。

### 4.2 每個 Adapter 的固定職責

提供 `discover`、`capabilities`、`read`、`health`、`shutdown`。API handle 由 Adapter 自己擁有；同一 NVML 連線的讀取由 runner 序列化或依實際 thread-safety 契約執行。Collector 只能經 `TelemetryPort` 取得資料，不接觸 NVML handle 或 sysfs 私有實作。

回傳原生型別、原始單位、讀取起訖時間、驅動回報的採樣時間（若有）、來源 generation、錯誤與有效性旗標。Adapter 可以解碼文字／SDK 結構，但通用單位轉換與跨來源判斷歸 Collector。不能丟棄 `Unsupported`／`PermissionDenied` 後塞預設值。

探索結果有 `availability=available/unavailable/permission_denied`，及逐項能力 `supported/unsupported/unknown`；庫可載入不代表每個 metric 都支援。受支援的新裝置可自動加入唯讀採樣；動作能力不隨裝置發現自動放寬。

### 4.3 讀取與動作介面必須分開

`ProcessActionAdapter` 實作 `ProcessActionPort`，只接受 Manager 驗證過的 process handle 與限定 signal。它不實作一般 TelemetryPort，Collector 永遠拿不到它。這仍屬 Adapter 家族，但位於**控制路徑**，不是資料收集鏈的一部分。

未來風扇、功率限制或 GPU reset 必須各自增加獨立 ActionPort、能力與驗收；不得利用一般 `execute(string)` 或 shell 指令繞過設計。本版只定義程序控制，不控制風扇／溫度／GPU 電源。

## 5. Collector：收集與多來源仲裁

### 5.1 領域分工

CPU、GPU、Memory、Disk、Network、Process Collector 各負責自己的 `MetricKey` 與投影。SourceArbitrator 與 Normalizer 是 Collector 內可共用的純邏輯元件，不另造全域服務。Process Collector 先以 ProcessKey 收集一次主機資料，再關聯 `(ProcessKey, GPU UUID／MIG entity)`，避免重複消耗 delta 基準。

每次 `collect(plan)` 經 TelemetryPort 讀取所需來源；Manager 所屬 AdapterBroker 依 `(adapter, generation, cycle_id, read_group)` 合併相同工作，使同一次 `/proc/stat` 或 NVML process list 不被不同 Collector 重複讀取。完成後回傳候選、仲裁解釋、標準觀測、局部失敗與下一個 CollectorState。CollectorState 有界，只保留 delta、來源健康與遲滯所需前態；採用結果由 Manager 提交後才提升為 committed state。

### 5.2 「同一指標」必須先定義

`MetricKey = host_id + boot_id + entity_id + metric_id + scope`。GPU 用 UUID／分割實體識別，不以排序 index 當身分；CPU 分 package／die／core，記憶體分 system RAM、GPU framebuffer、GPU process allocation，磁碟分 block device 與 filesystem。

`cpu.temperature.package`、`cpu.temperature.core`、`cpu.temperature.core_max`、`soc.temperature` 是不同指標。`core_max` 只能作具公式與來源集合的 derived metric；不得與 package temperature 互換而仍顯示同一名稱。

### 5.3 四種狀態不能壓成一個信任分數

| 軸 | 例子 | 意義 |
|---|---|---|
| 讀取結果 read_status | ok／unsupported／permission_denied／timeout／error／removed | 來源能否提供這筆資料 |
| 資料種類 sample_kind | measured／derived／estimated／simulated／replay | 這個值是量測、運算、估算還是測試 |
| 品質 quality | verified／usable／suspect／invalid／unknown | 依版本化 profile 與驗證規則的可信等級；不是機率 |
| 新鮮度 freshness／measurement_freshness | fresh／stale／unknown | 分別表示最近讀取與硬體量測的新鮮度；不可混用 |

CanonicalSample 使用 `read_finished_boot_ns` 表示最近讀取、`observed_boot_ns` 表示可確認的硬體量測時間；後者未知則為 null。`measurement_freshness=unknown` 不自動等於讀取失敗，但必須顯示證據限制；已確認 stale 的量測不得當權威值。

CanonicalSample 另有 `selection_state=selected/degraded/conflict/unavailable`。缺值使用 `value=null` 加 reason；0°C、0% 或 0 B 本身都是可能合法的數值，不當缺值 sentinel。沒有可用候選時，保留 `last_good` 供畫面註記，但不製造一筆新量測。

### 5.4 仲裁順序

1. **建立語意對應**：依已驗證 profile、driver label、拓樸與明確設定定位 entity／metric／scope；無法確認用途的溫度只放來源診斷，不冒稱 CPU。
2. **正規化**：使用來源宣告且可驗證的單位與轉換方式；未知單位不猜。保留原始值與 conversion_id。
3. **硬性驗證**：解析錯誤、非有限數、driver fault、不支援、已移除、profile 明定無效碼等不得進入權威值；fault 不是過熱 alarm。
4. **新鮮度與時間對齊**：以來源 generation、讀取範圍、device timestamp／更新週期判定。成功反覆讀取只能證明讀取成功，不能證明硬體量測更新。
5. **物理來源去重**：只有已證明同一 sensor／counter 的兩個介面共享 origin_id；重複路徑不構成兩票。僅數值相同不能證明同源；同樣，來源獨立性未知時不能當多數佐證。
6. **產生候選順序**：先比可接受品質，再比語意確定度與 profile 的逐指標來源優先序，再比新鮮度；最後用 source_id 穩定決勝。沒有「NVML 永遠最好」或「hwmon 永遠高於 thermal」的全域排序。
7. **檢查不一致**：只對相同 MetricKey、可比且對齊的候選比較。超過該指標容差時標示 conflict，保留候選與差距；同級無法判斷者不提供單一權威值，不平均掩蓋。
8. **防止頻繁切換**：健康主來源在 challenger 連續達標 3 次、且至少 5 秒後才切換；硬失敗／stale 可立即退到有效備援。這是可調的工程預設，不是感測器規格。
9. **提交可解釋結果**：選中來源、淘汰原因、quality、policy_revision、候選差異、SourceChanged 事件一起返回。Manager 將其當作同一邏輯 commit 存入 Store。

CPU 範例容差暫定 8°C、時間對齊容差 250 ms，僅作**診斷預設**；沒有平台校驗前不得稱為通用真偽界線。裝置採樣時間未知時，receipt time 對齊只能產生較弱的診斷疑慮，不能確證某顆感測器造假。來源固定值、與負載相關性低等只能加 suspect reason；除非有 fault／明確 profile 證據，不得自動判為假資料。

### 5.5 端到端例子：兩個 CPU 溫度來源

**以下全部是合成案例，不是使用者實機數據。** A=Hwmon package 0，原值 68000 m°C，B=Thermal zone，原值 42000 m°C。

| 情境 | Collector 的處理 | Viewer 顯示 |
|---|---|---|
| B 的 scope 是 SoC | A 與 B 屬不同 MetricKey，不競選 | CPU package 68°C；SoC 42°C |
| B 是 A 的已確認別名 | origin 去重，不增加信任票數；差異交來源時間／轉換診斷 | 一個實體感測器、兩個入口；保留不一致提示 |
| A、B 均為獨立且同級的同 package 量測，已對齊 | 26°C 差距大於例示容差，回傳 conflict、value=null | 「來源衝突」＋兩個讀值，不顯示 55°C |
| B 回報 fault=1，A 有效 | B 排除；A 選中，附降級／備援不可用資訊 | 68°C，來源 A，B 故障 |

A 故障時也只能退到**同語意**的有效備援。若只剩 SoC 值，CPU package 應顯示不可用，不能偷偷改用 SoC。

## 6. Manager：總控制器內部的功能模組

| Manager 模組 | 責任 | Store 權限／輸出 |
|---|---|---|
| RuntimeSupervisor | 啟停 AdapterRunner、Collector worker、Store worker；健康與重啟上限 | 寫模組健康與生命週期事件 |
| SamplingScheduler | 有效採樣週期、截止時間、背壓、單一在途請求 | 寫 schedule revision；輸出 ReadPlan |
| DeviceCatalog | entity／source／capability 身分及 generation | 讀寫資源與來源 registry；提供不可變目錄 |
| QualityPolicyService | 來源 profile、pin／exclude、仲裁規則版本 | 讀寫政策；不自己算感測器數值 |
| IngestionService | 驗證 CollectionBatch，提交 canonical／候選解釋／衍生狀態 | 唯一遙測邏輯寫入入口 |
| HistoryService | 時窗、重採樣、覆蓋率與統計查詢 | 透過 HistoryReader 查詢，不重新選來源 |
| ConfigurationService | defaults → 儲存設定 → 明確 CLI override；驗證、CAS、原子保存 | 讀寫設定版本；觸發 scheduler 更新 |
| ProcessService | 程序身分、確認流程、能力授權、signal 結果 | 讀程序快照；寫操作 intent／outcome；唯一 ActionPort 呼叫者 |
| PresentationService | ViewModel 投影、排序、過濾與來源解釋；按 entity 保持選取 | 讀快照／歷史／會話；發布不可變 projection |
| ViewSessionService | 每個介面的 active view、focus、選取、篩選與 overlay 邏輯狀態 | 讀寫 session state；無 GPU／硬碟 I/O |

這些是同一 application 層內的模組，不等於十個微服務。它們透過小型埠協作。Manager 的公開入口只有 **CommandPort、QueryPort、SubscriptionPort**；不暴露完整 Store 或任意 mutable state。

## 7. Store：保存資料，不制定領域政策

### 7.1 分區

| 分區 | 內容 | 預設與限制 |
|---|---|---|
| ResourceStore | host／boot／GPU／CPU／程序／filesystem／來源與能力 | 有界當前集合；移除保留 tombstone 與 generation |
| ObservationStore | 原始候選、錯誤與最近仲裁解釋 | 最近 60 秒；最多 2048 來源；每來源最多 600 筆 |
| SnapshotStore | committed canonical samples、quality、last_good 與 revision | 一致的不可變快照；非同步來源各自帶時間 |
| HistoryStore | 最近原解析資料與時間戳記聚合桶 | 原解析 300 秒；60 秒桶 24 小時；受總記憶體上限約束 |
| ConfigStore | 版本化完整設定與 session preferences | 本機原子 JSON；revision CAS；不跟監控降採樣一起丟棄 |
| AuditStore | 動作預備、授權拒絕、intent、送出結果、未知結果 | SQLite；保留 30 天、上限 128 MiB；滿額拒絕新破壞性操作 |
| OptionalArchive | 歷史跨重啟保存 | 預設關閉；啟用時 SQLite，7 天為軟目標、1 GiB 為硬上限 |

Manager 擁有「發起寫入」權；StoreActor 擁有「實際修改儲存結構／資料庫」權。StoreWorker 的檔案 I/O 不在 UI 或 Manager 控制事件迴圈上執行。Viewer、Controller、Adapter、Collector 都沒有 StoreWriter。

ConfigStore 與 AuditStore 不與 telemetry 共用失敗路徑。OptionalArchive 故障時可以繼續記憶體監控並顯示 `persistence_degraded`；AuditStore 無法先可靠記錄 intent 時，不允許送出 signal。啟動時即使無 GPU，也能運行 CPU／記憶體／介面。

### 7.2 當前值、歷史與資料血緣

每筆資料保留 `boot_id`、`source_generation`、`cycle_id`、`policy_revision`、`collected_at`、`valid_at`（若可得）、`sample_kind`。canonical 與 raw 分開；切換來源寫 SourceChanged，圖表斷線或分段，counter delta 重建基準。不因後來改設定而重算已存歷史。

歷史量測只包含合格 measured／明示 derived 資料；estimated 另列 series，simulated／replay 不進 live store。conflict、stale、invalid 不補零。保留 age 與覆蓋率，讓空窗可見。

### 7.3 精確性與保留上限

COUNTER 跨分鐘切片、時間戳排序與持久化邊界，以 STORAGE.md 的完整區間規則實作；不得按比例拆分 counter delta 卻標示 exact。

速率是 `rate=(counter_now-counter_prev)/實際經過秒數`。來源／generation／boot 改變、counter reset 或經過時間不正，回傳 warmup／gap，不把負值 saturate 成 0 後宣稱正常。

累積量優先用可靠 counter delta；只能取得瞬時速率時採時間積分並標示 estimated。gauge 平均使用有效時間權重；步階保留模型最多延伸至 `min(next_sample_time, source_stale_deadline, query_end)`，空白區段不延伸。來源切換與 stale deadline 邊界切段。requested window、covered duration、coverage ratio、aggregation_method 一起回傳。

單調時間採可注入 TimeProvider；Linux 以 `CLOCK_BOOTTIME` 做本次 boot 的 age／duration，避免 suspend 後把舊值當新鮮。wall time 僅作展示與跨開機查詢索引，時鐘回撥不得產生負速率。不同 boot 不共用 delta 基準；UTC 修正前後的查詢必須標記 clock discontinuity。[Linux clock_gettime 的 CLOCK_BOOTTIME 語意](https://man7.org/linux/man-pages/man2/clock_gettime.2.html)。

有界儲存先清除過期 raw 候選，再原解析資料，最後縮短聚合歷史；不可為維持宣稱的 24 小時而突破硬上限。查詢回傳實際最早時間與 coverage，不偽裝完整時窗。SQLite 的 checkpoint 與 busy 處理放背景 worker；WAL 不消除單一 writer 與 SQLITE_BUSY 的處理責任。[SQLite WAL](https://www.sqlite.org/wal.html)。

## 8. Controller：從按鍵到意圖，不從按鍵到副作用

由 InputBackend、Keymap、ContextRouter、CommandBuilder、ConfirmationInput 五部分構成。TUI 使用 crossterm；未來別的 frontend 可替換 InputBackend，而不改 Manager。

raw key／mouse → 依當前 focus、InputMode、Viewer 的 HitMap 解碼 → 形成 typed Command。純視窗 resize 可更新 Renderer viewport；切 tab、選取、搜尋／排序等可見會話狀態則經 ViewSessionService，避免 Viewer 與 Controller 各藏一份不同選取。

Controller 只做語法／長度／按鍵情境驗證。語意驗證、存取授權、設定 CAS、來源品質與程序身分驗證都在 Manager 重做。確認按鍵只有使用者意圖，不等於授權。

選取動作帶 `entity_id` 或完整 ProcessKey，不帶可能被重排的 row index。滑鼠點擊含 frame_revision；若 HitMap 與目前清單不一致，Manager 拒絕或要求重選，不能把同一索引套到另一筆程序。

## 9. Viewer：可組合的顯示系統

Viewer 包含 RendererBackend、LayoutEngine、DisplayModuleRegistry、RenderScheduler、Theme／Formatters、Viewport／HitMap。顯示模組至少有 CPU、GPU、Memory、Disk、Network、Processes、History、SourceDiagnostics、Settings、ActionAudit。

每個模組以 DisplayModuleManifest 宣告 `module_id`、`api_version`、所需 topics、最小寬高、priority、支援的版面位置與是否預設啟用。以編譯期 registry 註冊 render handler。模組可要求「溫度」或「來源解釋」資料，不自行要求某品牌驅動。

render 的唯一輸入為已準備好的不可變 ViewModel 和分配區域；輸出 Frame 子區域與 HitTargets。模組不得在 render() 讀 `/proc`、呼叫 QueryPort 同步等資料、改設定或取 Store lock。需要新歷史範圍時透過 Controller 提交意圖，Manager 異步查詢後推送結果。

採樣頻率與畫面頻率分開。相同 revision／viewport 不必重繪；過載可合併中間畫面，但不默默丟命令。小視窗按 min_size 與 priority 降為摘要／隱藏次要模組；不得裁掉正在等待的 kill 確認、錯誤訊息或來源衝突提示。

來源品質、資料種類、age／coverage 都有文字或符號，不只靠顏色。溫度顯示為「68°C · package 0 · hwmon · 可用」，衝突為「來源衝突 · A 68°C／B 42°C」，缺值為「不支援」或「讀取失敗」，而非空白或 0。

## 10. 執行模型、排程與背壓

主終端執行緒只輪詢輸入、傳送命令、讀取最新 ViewModel 並畫圖。Manager control loop 處理命令／完成事件，不做阻塞硬體或磁碟 I/O。Collector workers 做有界計算；每個 AdapterRunner 有獨立 deadline、單一在途與健康狀態。

對不可取消的 vendor FFI，預設採同一 binary 的 `--adapter-host nvml` 子程序隔離，以父子匿名管線傳輸白名單 DTO，不開網路 listener、不經 shell、不提升 UID。其他可能長時間卡在 statvfs 的來源也可使用相同 isolation policy。

**timeout 只代表本次結果逾期，不代表底層呼叫已被取消。** FFI thread 不能靠取消 future 保證中止；子程序被要求終止也可能無法立即回收。每個 backend 最多 1 個在途 host；未成功回收前不得建立無限替代程序。逾時切 degraded，連續 3 次失敗後開啟 circuit breaker，退避 1、2、4…最多 60 秒；成功 probe 後重新 warmup。

| 通道 | 上限／策略 | 失敗行為 |
|---|---|---|
| 控制命令 | 128 個；非阻塞投遞 | 滿時回 Busy；破壞性命令不合併 |
| 採樣結果 | 32 批且全域 ingress 8 MiB；wire 每 frame 最多 1 MiB | 筆數與位元組雙重 admission，滿時整批拒絕並報 Gap；不得部分提交 |
| 每來源採樣 | 同時 1 個；保留最新待採週期 | 不累積過期 Tick；同源 counter 仍按真正取樣時間 |
| 畫面訂閱 | 每 session 只保留最新快照 | subscriber 看 revision 差，必要時重新讀快照 |
| 操作回覆 | request_id 對應、有界保留 | 無回覆不自行重送 signal；可查操作狀態 |
| 稽核與設定 | 專用小型序列化佇列 | 逾時失敗明示，不能無限阻塞 Manager |

同一 metric 的 cycle 與 generation 必須遞增；重複回覆丟棄，舊版本回覆不得覆寫新值。快照是「一次 Manager commit 的一致視圖」，不是聲稱所有感測器同時取樣。設定改採樣週期時使舊 epoch 失效，等待 scheduler Ack 才回 ConfigApplied；UI 不得只改一個數字。

## 11. 程序終止：Manager 保有權限，不代表任意呼叫都能執行

ProcessKey 至少包含 host_id、boot_id、pid_namespace、PID、start_time_ticks；PID 必須是正整數且不超過 i32::MAX。索引、程序名稱、PID 單獨都不是足夠身分。

`PrepareSignal` → 檢查本機會話、same UID、允許的 signal、不可為 PID 1／自己／dgxtop 受管 worker → ProcessActionAdapter 開啟 pidfd → 讀 `/proc` 身分與 fd 關聯、重核 ProcessKey、確認程序尚未退出 → 回傳描述與 10 秒一次性 confirmation_token。

`ConfirmSignal` → 確認相同 session、token 未使用／未逾時、政策版本未改、權限仍成立 → 原子消耗token進intent_pending → durable audit intent 成功 → 重核TTL/取消/政策 → 以準備時保留的 pidfd 送訊號 → 保存結果 → 非阻塞觀察退出。`signal_sent` 與 `exit_observed` 是不同狀態。

pidfd 的目的在於綁住核心中的程序實體；**準備時才開 pidfd 仍須與原先 ProcessKey 比對**，避免準備之前 PID 已重用。持有 descriptor 後不再靠 PID 重新開啟目標。對 `/proc` 讀取與 fd 對應無法確證、namespace 不一致、核心不支援或程序已退出者一律拒絕。[pidfd_open](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)、[pidfd_send_signal](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html)。

預設功能存在且允許本機一般使用者對同 UID 的程序明確送 `SIGTERM`；`SIGKILL` 功能有設計，但 `allow_force=false`，要明確啟用並另外確認，不自動升級。`read_only=true` 可停用所有動作。euid=0 的會話預設停用程序動作，不能藉 root 繞過產品政策。不新增 sudo／setuid helper。

程序可能在相同 pidfd／start_time 下 exec 成不同程式；command 名稱是確認資訊，不是安全身分。對 exec／credential 變化採重核與保守拒絕，但不宣稱能把任意執行中程序的所有狀態原子凍結。核心仍作最後權限檢查。

只有專用 action worker 能執行由Manager已核准且仍有效的intent。崩潰發生在「已送訊號、尚未落盤 outcome」時，重啟後狀態是 unknown，不自動重試。SIGTERM 不是可回滾交易；已終止的工作無法靠軟體 rollback 復原。

## 12. 設定與公開契約

設定唯一持久來源為 ConfigStore 管理的版本化 JSON；已存在欄位不被未提供的 CLI 預設覆蓋。Priority：內建 defaults < 已保存設定 < 明確 CLI arguments。CLI 欄位使用 Option／value_source 判斷是否由使用者提供。

新增設定按 schema 驗證；拒絕未知 key、非法數字、非有限數、超大 cardinality。先產生 validated candidate，再 stage 需要的資源／排程，持久寫入暫存檔、fsync、原子 rename、fsync 目錄，最後切換記憶體版本。尚未完成的操作不能回報成功。若在持久完成後、runtime 切換前崩潰，重啟讀取新版本，而不是把半套參數拼起來。

具體預設與逐欄驗證見 `contracts/CONFIG.md`、`contracts/config.schema.json`、`examples/config.default.json`。資料／命令／事件與錯誤見 `contracts/INTERFACES.md`；JSON 範例只含合成資料。沒有 HTTP API，所以 OpenAPI、登入 token、跨網路 ACL 在此範圍為 N/A；不因此省略本機動作授權。

## 13. 失敗模式與可見行為

| 故障 | 系統行為 | 使用者能看見什麼 |
|---|---|---|
| 一個溫度來源 fault | 排除該筆，選同語意有效備援；無則 unavailable | 原因、備援與切換時間 |
| 兩個可信來源衝突 | 不自動平均，不默認較低者正確 | 兩個值、scope、age、origin 與仲裁政策 |
| NVML 局部 API 不支援 | 該 metric unsupported，其他 GPU／指標繼續 | 不支援而非 0 |
| GPU／網卡移除重建 | generation 更新、舊序列結束、counter warmup | 移除／恢復與新身分 |
| Adapter 卡住 | 期限到後過期化，健康降級；限制替代 worker | 最後成功時間與 timeout |
| Store archive 寫入失敗 | 保留即時／記憶體歷史，拒絕假裝已存檔 | 保存失敗與實際保留範圍 |
| AuditStore 失敗 | 拒絕新程序訊號 | 未執行、原因為無法保存稽核 |
| 頁面模組 panic | 模組隔離／停用並顯示錯誤；不能影響採樣 | 模組錯誤與可重啟入口 |
| 控制命令滿載 | 回 Busy；保留 UI 可取消／退出 | 明確忙碌，不宣稱已生效 |
| suspend／時間跳動 | 保留 boot 與時間 discontinuity，產生 gap | 歷史空窗而非平滑假資料 |
| 雙重執行 same command_id | 回既有結果或 in_progress | 不重發同一次操作 |

同程序 display module 的 panic 隔離只保證 unwind 模式且沒有 abort／UB 的範圍；不是不可信第三方程式碼沙箱。對外部模組的更強隔離需另做 IPC plugin 協定，非本輪默認設計。

## 14. 效能與資源目標

**以下全部是驗收目標／設計容量，尚無實機測量。完整門檻、取樣方法與環境分級見 PERFORMANCE.md；詳細資源預算見 RUNTIME.md。** 參考負載為 512 條 canonical series、2048 個來源、256 個程序、7 個啟用 Adapter host（硬上限8），1 秒採樣；硬體報告須寫 CPU／GPU／驅動／OS 版本。

| 項目 | 目標 |
|---|---|
| 使用者輸入到 Manager 接受／拒絕 | p95 ≤ 50 ms；不包含硬體動作完成時間 |
| TUI render 單幀 | p95 ≤ 16 ms，終端 160×50；預設最多 4 fps、dirty-only，最高 20 fps |
| 預設採樣 | 1000 ms；可配置 100～60000 ms，effective period ≥ Adapter 宣告最短週期 |
| Metadata discovery | 預設 10000 ms；熱插拔通知可提前 |
| 單次讀取 deadline | 500 ms；可配置 50～10000 ms，非保證中止 |
| 資料記憶體預算 | 256 MiB 硬預算；同負載程序樹 RSS 合計目標 ≤ 384 MiB，需實測證明 |
| 不可變畫面快照 | 每 session 最新一份；預設最多 2 個 session |
| 歷史查詢 | 最多 2000 點／結果；p95 ≤ 100 ms（記憶體 24h 視窗） |
| 受管 worker 數量 | 每 Adapter 一個 runner／host；不可累積逾時 orphan |

粗估 `512 × 1440 × 96 bytes ≈ 67.5 MiB` 的 24h 聚合 payload；這不是 Rust 容器或 SQLite 的實際用量，另有索引、allocator、raw 候選與字串成本。Store 依實際配置與使用量限制保留，不能拿這個 payload 估計當 RSS 保證。

## 15. 可觀測性與隱私

記錄 `collection_duration_ms`、`adapter_timeout_total`、`source_conflict_total`、`source_changed_total`、`queue_dropped_total`、`store_commit_duration_ms`、`viewmodel_age_ms`、`history_coverage_ratio`、`command_rejected_total`。單位寫在名稱或 descriptor；labels 使用有界 adapter_id／metric kind／reason，不把完整 PID command line 當高基數標籤。

每個動作有 request_id、session_id、policy_revision、ProcessKey 摘要、結果。預設不持久保存完整 cmdline／環境變數，避免記錄 token；必要描述使用 executable basename 與 UID。日誌清除控制字元，避免終端 escape 注入。debug raw 資料由使用者明確啟用且受容量限制。

## 16. 擴充、依賴方向與程式目錄

核准邊界下的實作 workspace（具體以 IMPLEMENTATION_LAYOUT.md 為準；runtime 是機制支援，不增加業務層）原分組：`core`（DTO／ports／domain policy 型別）、`adapters`、`collectors`、`manager`、`store`、`ui`（viewer／controller）、`dgxtop`（composition root／CLI）。不要先切成每個小模組一個 crate。

依賴方向：adapter／collector／store／manager／ui 都依賴 core；Manager 僅經 core ports 呼叫其他實作。Composition root 組裝 concrete implementations。`core` 不依賴 Ratatui、NVML、SQLite、crossterm 或 libc；Viewer／Controller 不依賴 adapters／store。啟動時檢查 duplicate module_id、api_version、缺少能力、無效 layout 與循環依賴。

新增顯示模組的步驟是實作 DisplayModule trait、提交 manifest、註冊 factory、提供 snapshot fixture 與窄視窗測試。新增 Adapter 的步驟是 manifest／TelemetryPort 實作、來源 profile、units／failure fixtures 與 capability tests。預設編譯期 registry；不使用未穩定的第三方 Rust 動態 ABI。

## 17. 遷移與回滾

先鎖定原有行為與 F1～F10 的 regression fixtures，再建立 core 契約與 adapters；逐領域搬出 I/O、引入 Collector 仲裁，最後接 Manager／Store 與新 UI。不能直接把舊的 0 預設值包入新 `verified` 型別。

本機路徑分開：設定 `XDG_CONFIG_HOME/dgxtop/v2/config.json`；稽核 `XDG_STATE_HOME/dgxtop/v2/audit.sqlite3` 與可選歷史 `XDG_STATE_HOME/dgxtop/v2/history.sqlite3`；不存在 XDG 時使用標準 home-based fallback。legacy 設定只提供 dry-run 匯入報告，使用者確認後另寫 v2，不覆寫舊檔。原始純記憶體歷史沒有可遷移內容；不要捏造舊資料。

影子比對模式只允許唯讀採樣，用來比較新舊語意；不是取消最終程序控制功能。驗收後切換 binary／symlink；保留原 binary 與設定。回滾只還原程式／設定，不回滾已送出的 signal。v2 資料庫可保留供診斷；不得交由舊版自動改寫。

| 前次審查 | 新設計承接點 | 本次狀態 |
|---|---|---|
| F1 程序身分 | ProcessKey＋pidfd＋Manager capability＋兩階段確認 | 已設計，未實作 |
| F2 缺值變零 | Observation 狀態軸＋CanonicalSample null | 已設計，未實作 |
| F3 共享記憶體語意 | system／framebuffer／process allocation 分離 | 已設計，未實作 |
| F4 頻寬估算 | measured／derived／estimated 分離，沒有計數器不冒稱吞吐量 | 已設計，未實作 |
| F5 更新間隔與累積量 | 有效 scheduler Ack＋實際 dt／counter delta | 已設計，未實作 |
| F6 時窗錯置 | 帶時間桶、boot、coverage 與 gap | 已設計，未實作 |
| F7 多 GPU 同 PID | ProcessKey 只採主機資源一次 | 已設計，未實作 |
| F8 安裝完整性 | release checksum 驗證與失敗保留舊 binary 的交付規則 | 實作任務列入，未執行 |
| F9 採樣卡 UI | 控制／採樣／渲染分離＋worker 隔離 | 已設計，未實作 |
| F10 版本與可重現 | commit Cargo.lock、版本一致、鎖定 toolchain／release gates | 實作任務列入，未執行 |

前次 D1「是否保留終止」依本輪需求採**保留且集中授權**，不再默認縮成唯讀版本。硬體矩陣仍以能力探索與實測為準，不把完整設計等同全面 DGX／MIG 認證。

## 18. 必須維持的不變量

任何進入 Store 的權威 metric 都可追到來源與政策；未知不等於零。每次動作只能經 Manager 的授權與專用 ActionPort。Viewer 不影響採樣結果，改 FPS 不影響採樣時鐘。歷史值不因當下更新間隔而重算。任何失敗都不能靜默冒充成功、正常或完整。所有示範／估算與實測資料都必須可區分。

## 19. 完整實作映射

40 項需求、15 個有序工作包及驗收條件見 SPEC.md、TASKS.md、ACCEPTANCE.md 與 requirements.json。前版 R01～R20 保留 ID，本版增加 R21～R40。
