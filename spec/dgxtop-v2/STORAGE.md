# Store、歷史正確性與持久化

## 1. 分區、擁有權與提交

HotStore actor是遙測單一實體writer；Manager的IngestionService是單一**邏輯**寫入入口。CollectionBatch包含逐指標canonical、必要provenance／explanation、缺值、state_delta、來源序號與epochs。commit先驗schema／namespace／單位、catalog引用與memory admission，再修改ring/快照，最後一次發布revision與Ack；驗證失敗不得留下半批更新。Collector收到Ack才使用新的前態。

HotStore不能等待磁碟fsync。optional archive拿同revision的有界journal副本；接受HotStore成功不代表archive durable。回覆明確區分`memory_committed`、`archive_pending`、`archive_committed`、`archive_gap`。Archive故障後即時監控仍運作，不能把尚未保存的紀錄標已持久化。

Config、Audit、Archive各有獨立worker與容量；物理上共用SSD仍可能一起滿盤，不宣稱分檔就消除所有共因失效。Config可靠保存失敗保留舊設定；Audit不能記intent時拒絕動作。每個資料目錄持有排他instance lock；第二個writer程序對相同state dir回AlreadyRunning，避免兩份dgxtop同時消耗相同稽核token。兩個view session不是兩個writer程序。

## 2. 資料模型

| 結構 | Key | 數值與生命週期 |
|---|---|---|
| CurrentSnapshot | MetricSlot＋generation | 最新canonical、last_good、原因、source/version；missing不刷新last_good時間 |
| CandidateRing | SourceSlot＋generation＋read time | 最多60秒、raw分區上限先到先限制；原生型別及轉換id |
| SeriesSegments | MetricKey＋source generation＋policy revision＋method | source/policy/clock/boot改变切段，引用catalog摘要不得懸空 |
| RawHistory | SegmentId＋observed/read time | 通常300秒；精確counter端點與gauge step不混用 |
| MinuteRollup | SegmentId＋[start,end) | covered duration、weight sum、min/max、counter interval total或missing reason |
| GapIndex | series＋[start,end)＋reason | drop、timeout、reset、suspend、evicted；合併相鄰同原因，受96MiB總預算 |
| ProcessSnapshot | ProcessKey | 單一host資源快照，GPU rows只引用；預設不持久化全cmdline |
| ActionAudit | request_id／intent_id／event_id | prepared/rejected/cancelled/intent/outcome與correlation；不歸telemetry eviction |

刪除source後，只在無history／query／action reference時回收descriptor；否則保留compact tombstone。過度churn先淘汰最舊允許歷史、標記truncated；不能重用slot而讓舊series指向新裝置。retired Arc與query pin的實際存活配置仍计入budget。

## 3. 時計與區間

TimeProvider提供boot_id、CLOCK_BOOTTIME ns與UTC pair。BOOTTIME包含suspend，用於age、deadline、sample dt；只在同boot相減。UTC只能索引／展示，wall clock回撥或jump須切clock segment，不能把兩段混成一条均勻時間線。[Linux clock語意](https://man7.org/linux/man-pages/man2/clock_gettime.2.html)。

hardware timestamp有明確conversion/mapping才寫observed_boot_ns；否則null並顯示measurement_freshness unknown。用read_finished的age可以證明最近讀取，但不证明sensor重新量測。stale判斷比較`now >= deadline`，邊界到期即stale；故障不延長deadline。

所有查詢為半開區間`[start,end)`，end>start。跨boot查詢拆成多個segment並保留discontinuity；內部API單次HistoryQuery限定一個boot。查詢聚合的coverage是有效資料時間聯集，不能重疊加總超過1。

## 4. 計算規則

**counter**：先以u64精確相減，再除實際dt得到f64 rate。正常同generation且`curr>=prev`才可衍生；重置、source切換、boot改變、dt≤0／dt大於stale deadline、被標記counter不可靠時返回warmup／gap。energy counter只有driver明確宣告modulus且可证明最多wrap一次時才用模數差；不能對所有負差猜一次wrap。

CPU總使用率與breakdown由同一次/proc/stat差分，guest已包括在user時不能再重複相加；core identity不能用讀到的順序代替cpuN。Process CPU%=Δ(utime+stime)/(CLK_TCK×實際秒)×100，可超過100%表示多核心使用；不是`/全機核心數`。PID跨GPU只算一次。I/O rates由bytes/count delta；Filesystem used=blocks-bfree、available=bavail分開，available不是raw device free。

**gauge**：採time_weighted_step。樣本值v在`[t_i,min(t_next, stale_deadline_i, query_end))`有效，`sum(v×covered_ns)/sum(covered_ns)`；不把缺值區間填0，不將stale值延到下一個成功樣本。alarm值仍可進歷史，invalid/fault不行。圖表min/max保留尖峰，mean與max分開，不用平均線掩蓋熱尖峰。

**counter總量與bucket邊界**：一筆delta對應完整實測區間`[t_prev,t_now)`，不知道區間內每個byte何時發生。查詢只把**完全落入查詢範圍**的實測delta算exact；部分交集不能按比例拆分後仍標exact。建立60秒rollup時跨分鐘的counter interval另存bridge區間，常規bucket只包含完整落入的interval。query將完整bucket與完全包含的bridge相加；邊界剩餘無raw可拆時省略、回boundary_gap與較低coverage。bridge保留與rollup同TTL／budget，不可無界堆積。

只用瞬時速率算總量則明確`estimated_integral`，來源model與有效時間標示；不能把NVML memory utilization的活動時間百分比乘理論峰值稱為實測GB/s。沒有byte counter就不提供measured頻寬。來源估計可以另列series，禁止進入measured namespace。

## 5. 查詢與索引

Memory query最多2000點、4MiB結果，最多2 running/4 submitted，每個job deadline200ms；REF p95≤100ms。依requested resolution選raw／minute rollup，單次scan最多200000個已記帳records，超過回`QueryTooLarge`及建議較粗resolution，不同步掃描全部sources。還有raw時才能精確切gauge邊界；已淘汰raw的部分bucket不比例重建成exact。

range total/min/max/mean/raw明確區別；raw超過max_points回truncated與next_start_boot_ns，不能默默sample。圖表壓縮按pixel bucket保留min/max/mean，標`display_downsampled`，不改Store的權威資料。每個query帶request_id，result帶snapshot_revision與next_start_boot_ns；過期UI查詢結果丟棄，不覆蓋新tab狀態。

SQLite BOOTTIME與u64 counter不可轉REAL排序。DDL採固定20位零填充decimal TEXT作ns排序key，對JSON輸出移除前導0；numeric domain≤u64::MAX，以string length／regex／上界共同驗證；累積counter跨多interval使用u128 checked add。不得直接SQLite SUM(TEXT)讓其變double。UTC獨立TEXT索引不作dt。

## 6. SQLite 策略與WAL容量

啟用SQLite前檢查`sqlite3_libversion_number() >= 3051003`與built/runtime一致；此門檻是已確認修正WAL-reset的保守下限，不是「高於此版本永遠無bug」。2026-03-13的3.51.3已修正相關競態，官方WAL頁2026-08-25仍提供說明。[SQLite WAL](https://www.sqlite.org/wal.html)。本版不接受需另辨識backport的較舊版本，以減少測試組合。

| 設定 | Audit | Archive |
|---|---|---|
| journal_mode | WAL | WAL |
| synchronous | FULL，intent commit確定後才send | NORMAL，明示斷電可能失去最近transaction |
| cache_size | -2048（KiB建議值） | -4096（KiB建議值） |
| busy_timeout | 100 ms，總intent期限1000 ms | 50 ms，batch總期限500 ms |
| writer/checkpoint | 同一connection序列化；不另開並行checkpointer | 同一connection序列化；reader只讀 |
| transaction | 每個action intent/outcome小交易，預留outcome容量 | 最多512 rows或1MiB或1000ms先到者提交 |
| retention／cap | 30天、128MiB，未到期不自動刪除來繼續kill | 7天soft、1024MiB硬預算；先淘汰舊資料 |

FULL/NORMAL的差異是durability與同步成本，不是可以任意省略稽核；仍依賴OS/VFS/儲存硬體誠實完成sync。[SQLite synchronous](https://www.sqlite.org/pragma.html#pragma_synchronous)。

capacity要算db＋wal＋shm＋寫入／checkpoint scratch。Audit DB配置`max_page_count`限制主檔，WAL達8MiB開始PASSIVE checkpoint、16MiB停止新intent並先縮減讀取；archive WAL達16MiB開始checkpoint、32MiB停archive寫入。若即將突破總cap，交易前拒絕並用reserve保留未完成outcome；未checkpoint時不能把journal_size_limit當硬WAL cap。所有SQL使用prepared statements與bound params，不開load_extension。

read transaction最多200ms，進度handler／cancel讓長查詢離開，避免reader pin住WAL。不得在正常熱路徑FULL/TRUNCATE checkpoint或VACUUM；必要maintenance進入可見維護模式，先停止writer，超時即報忙碌。外部storage完全卡死時worker可卡住，但Manager不join等待，也不為同DB再開writer。磁碟總空間少於64MiB拒絕新增archive與新action intent，Config更新失敗顯示而不假成功；free-space檢查也在I/O worker，且不是ENOSPC防護的替代。

## 7. 設定原子性與revision

schema_version=2，defaults < persisted < explicitly provided CLI。只有明確CLI欄位覆寫；`--interval`沒指定不能覆蓋設定檔。candidate.revision=current，CAS成功由Manager+1；未知欄位、source不匹配pin、超容量組合一起拒絕。runtime／queues資源改變回RequiresRestart；interval/profile/view等hot設定先stage、保存temp、fsync、rename、fsync dir，再不可失敗epoch swap及SchedulerAck，之後才ConfigApplied。

保存後runtime尚未activate崩潰，重啟讀新完整版本；runtime stage前失敗不能覆寫舊檔。boot後不重放任何操作token。config CLI暫時override不自動persist；使用者透過設定提交的完整candidate才保存。config receipt完成前UI顯示pending，不能先改速度數字當成功。

## 8. schema migration／損毀處理

archive/audit `user_version=2`，以一個SQL transaction建立；重復建表應冪等。這份DDL是新v2資料目錄，不假設舊App有SQLite可遷移。已存在未知future schema只允許唯讀診斷、禁止降版改寫；migration前備份／integrity check，未通過保留檔案、停止該持久分區、不默默create empty替代。

Audit啟動將有intent但無可證outcome者標unknown，**不自動再send**。archive清理cascade刪segments/buckets；未過期稽核滿額拒絕新動作。兩分區獨立status與UI banner。原始完整cmdline／env預設不落盤；用uid、basename、stable identity與reason完成必要稽核。

## 9. 結果分頁與精度

raw分頁以`next_start_boot_ns`提示下一個半開區間起點，最後頁為null。每頁獨立snapshot_revision，不承諾多頁原子快照；來源/epoch變更以segment明示，失去原始資料則gap。下一頁仍遵守4MiB/2000點/200ms限制，不保留無期限server cursor。`covered_ns`全部用u64 decimal；SQLite聚合的covered_ns也以20位TEXT保存，≤60000000000ns，不先取整成毫秒而損失coverage精度。
