# 執行、排程、背壓與記憶體

## 1. 固定執行上下文，而非每層都生一條 thread

| 執行上下文 | 預設／上限 | 唯一負責的工作 |
|---|---:|---|
| Terminal 主執行緒 | 1 | crossterm polling、Controller、讀 latest slot、Ratatui frame；不做 I/O 採樣／DB |
| Manager control | 1 | 命令驗證、排程 deadline、有限服務狀態、ready notifications |
| Adapter transport／supervisor | 1 | 非阻塞 pipe multiplex、child health／reaping、framing；不執行 SDK |
| Collector compute | 2／4 | 正規化／仲裁／delta 的純計算 job；預設 2 |
| HotStore actor | 1 | single writer、原子 batch、minute rollover、chunk descriptors |
| History query | 2／2 | 有界 immutable chunk snapshot 上計算，最長 200 ms job |
| Config／Audit／Archive／log worker | 各最多 1 | 各自阻塞檔案 I/O；Archive 關閉時不啟動其 worker |
| Process action worker | 1 | 取得／重核／持有 pidfd，確認動作的單次執行與結果 |
| live Adapter host | 每啟用 Adapter 1，總計≤8 | 以相同 binary 啟動常駐 child，procfs／hwmon／thermal／powercap／NVML／network／filesystem |

預設 parent 在2個compute worker時最多13個執行緒（含archive）；REF archive關閉時最多12；compute調至4時全部上限15。每個 adapter-host 預設1個應用 thread，vendor 自有 threads 另外量測、計入程序樹 CPU/RSS/threads，不能假裝不存在。至多8個存活或 quarantined host；也要在程序樹統計中計入沒有回收的 host。採用固定 Threads＋crossbeam，不默認加入 Tokio／Rayon 的額外 global pool。

只有逾時／崩潰恢復時才重建 host；每個 tick 不 spawn。維持 host 的 NVML handle 與 cache；擁有權只在该 host。多來源為 logical Adapter，不共用可能阻塞的同一 executor 來省 thread。跨程序隔離仍不是對惡意程式碼的完整 sandbox。

## 2. 熱路徑與演算法上限

discover 建立 interned EntitySlot／SourceSlot／MetricSlot；資料點用 u32 slot＋generation，Display names 使用 catalog 共用字串，不每次復制完整 device path。ring 以 column／chunk 儲存固定寬度欄位，原始/native DTO 可借用解析 buffer，跨工作上下文以 move 或 Arc 小批次傳遞。不得以 unsafe 或自製 lock-free queue 取代已驗證庫來追求未量測效益。

來源分組於 catalog/policy 變更時預先建立。每 series 最多4候選，仲裁 O(C log C)、C≤4；獨立來源 pair 比對最多6對，不做全域 O(source²)。每個 ProcessKey 每 cycle 只讀 host CPU/RSS 一次，多GPU行只join；排序 O(P log P)，filter 時字串正規化最多在 command/content 改變時做一次。ViewModel 只帶可視列與小幅 overscan，不每幀復制256程序的全部 cmdline。

Broker key = (adapter_id, host_generation, catalog_revision, config_epoch, cycle_id, read_group)。同key讀取只派一次，多Collector共用不可變 reply；不得把非重疊時槽或不同 generation 的舊結果當 cache hit。第一份完成後移入有界回收區；逾時共用者各見同一失敗，不再各自發送 retry。

## 3. 多速率採樣與 deadline

`effective_period = max(user_requested_period, adapter_verified_minimum, metric_group_minimum) × overload_multiplier`。overload_multiplier僅套用可節流group，UI显示 configured/effective/reason，不假裝原頻率仍生效。未知硬體 cadence 最短按1000 ms，已知低更新頻率不輪詢得更快再宣稱新量測。

| read_group | 正常預設 | 規則 |
|---|---:|---|
| cpu/memory/network/disk 累積計數器、GPU utilization/temperature/power、process resource | 1000 ms | 跟隨明確 interval；受 capability 下限限制 |
| filesystem capacity | 5000 ms | 不每100 ms statvfs；device I/O仍可1秒 |
| GPU ECC／energy累積／健康與較慢sensor | 5000 ms | critical缺值仍即時狀態更新；能量dt按真實取樣 |
| catalog／metadata、UUID、serial、PCI拓樸、max clock | 10000 ms／generation快取 | 靜態欄位只在generation/通知/手動refresh改變時讀 |
| unsupported／permission capability重試 | 60000 ms | 權限／driver epoch改變可以提前；禁止每幀反覆失敗 |

以 BOOTTIME absolute deadline＋可取消wake timer排程。idle poll最長50 ms，沒有工作不busy-spin。不補跑歷史tick：若next_due≤now，直接算首個大於now的due；本次已在途則記一次 skipped_due，絕不新增第二請求。初次各adapter相位按穩定hash分散在一個週期內，metadata低優先。自訂高頻群組的 dt 仍記錄實際時間。

單一host同時一個請求，但 host 有多read_group；採 round-robin＋earliest deadline，不讓capacity讀取永久餓死fast group。單request最多256個source；更大工作拆成不同read_group／page並有明確complete/truncated標示。Snapshot commit允許不同group不同timestamp，不等待所有host的全域barrier。

## 4. Manager 公平性與控制保護

control loop每輪最多2 ms預算：先檢查out-of-band shutdown/cancel旗標、處理最多8個控制completion，再處理最多8個一般命令，再最多4個ingestion completion，最後到期timer。任何類別即使一直ready也必須讓其他類別前進；計算需要>2 ms的工作送worker。不能無限 drain 一個ready queue。

命令submit使用try_send或try_lock的短臨界區，滿則立即Busy。每個accepted command在接受前保留reply slot與pending記錄；completion由固定slot＋通知bit交付，因此通知coalesce也不會遺失結果。pending最多128，inflight action最多8，超量拒絕。重複request_id payload一致回原狀態，不一致Conflict；cache最多4096／TTL10分鐘，已durable action在DB再查而不是單靠記憶體。

退出／取消是獨立atomic flag＋有限token命令優先路徑，不受一般128命令queue阻塞。Config/Audit/Archive completion各有固定小mailbox，不混入可丟telemetry。只有Manager可觸發store writer／action port，不為了免queue讓Viewer直接寫。

## 5. 所有通道的上限

| 通道 | 筆數／容量 | 壓力策略 |
|---|---|---|
| 一般commands | 128，每命令≤64 KiB（完整設定≤64 KiB） | 超限Busy；accepted先保留結果slot |
| Collector jobs | 8，workers2 | 不派超期cycle；每Collector一次未commit job |
| ingress raw＋decoded＋collection結果 | 32批，總計8 MiB | 先reserve bytes；可decode空間不足時暫停讀pipe，deadline到後整批拒絕＋gap |
| IPC frame | wire≤1 MiB、record≤256、nesting≤32、decoded≤4 MiB | 先驗4-byte length，再reserve；超限關host，不能任意grow Vec |
| archive journal | 8 MiB（算入reserve區）、最多8 transaction batch | 滿時標 persistence gap，不阻塞HotStore |
| history query | 4個request，最多2 running；每result≤2000點及4 MiB | deadline200 ms／容量不足Busy；舊query session generation過時可取消 |
| presentation | 每session最新pending1＋active1；各≤1 MiB；session≤2 | 合併中間畫面；revision gap明示。不得保存無限Arc版本 |
| metadata/error tombstone cache | sources2048、series512、error每source最近8筆 | TTL與epoch回收；持續熱插拔不能只增不減 |
| log | 1 MiB、10 MiB×3輪替 | rate-limit/丟低優先debug並計數；不丟動作稽核 |

crossbeam bounded只限制元素數，不限制payload bytes，因此ByteBudget不能省略。[官方 bounded 說明](https://docs.rs/crossbeam-channel/latest/crossbeam_channel/fn.bounded.html)。實作BudgetPermit RAII，ownership move時轉帳，不在同一父／子queue重複計入；wire與decode同時存在則兩份都算。try_reserve失敗發可見capacity狀態，不能OOM後才「降級」。

## 6. 256 MiB 預設資料預算

| 分區 | MiB | 包含 |
|---|---:|---|
| catalog／metadata／descriptors／仲裁前態 | 24 | interned strings、tombstone、profile索引與committed基準 |
| raw candidate retention | 32 | 60秒soft retention與證據，不含另外clone |
| hot state／process即時表 | 16 | 512 canonical、256 ProcessKey、1024 GPU links |
| 原解析canonical歷史 | 32 | 300秒soft retention，chunked slots |
| minute aggregates／segments | 96 | 24h soft retention、source changes與gap索引 |
| ingress | 8 | 所有同時存活wire/decode/collection buffers |
| presentation | 12 | bounded projection、UI formatted cache與retired frame |
| history query | 16 | snapshots引用pin成本、scan scratch、result |
| 控制／archive／log／突發安全reserve | 20 | command/result reservations、8MiB archive journal、log1MiB |
| **合計** | **256** | 禁止每partition各算一套256MiB |

是admission帳目，不是已測RSS。allocator metadata、thread stack、SQLite caches、vendor library、共享頁、kernel pipe都會使RSS大於payload；PERFORMANCE以**父＋全部child的RSS總和**另外驗收，重複共享頁的保守計數也不得偷改分母。partition按`memory_limit_mib/256`縮放，總和必須一致；reserve不可借給可丟歷史。active控制/稽核不可被eviction取消。

容器採惰性配置，不能啟動立即實配全部256 MiB。約`512×1440×96B=67.5MiB`只是無頻繁source change的聚合payload規劃；96B是設計上界待size_of／配置量測驗證，不是Rust實際大小結論。原解析採樣增至10Hz時要縮短retention，而非突破32MiB。

所有allocation wrapper／Vec容量與hash容量都計帳，Arc被慢reader持有時仍算存活；每reader最多固定chunk集合，有deadline／quota，新query被拒絕而不是永遠等待GC。正常範圍內不得以反覆容量拒絕達到低CPU「達標」。retention縮減必回傳actual_start／coverage／truncated。

## 7. 有界過載控制

狀態 `normal → pressured → throttled → recovering → normal`。任一queue bytes≥75%或採集service p95超過effective period，連續3次／至少3秒進pressured；memory≥90%或ingress持續滿進throttled。恢復需所有受控queue<50%、memory<75%、service正常連續30秒；每30秒最多降低一級倍率，避免抖動。

pressured：停可選debug/raw dump、metadata延後、render最多2fps、archive減低提交頻率但守上限。throttled：先query限流、丟最舊可丟raw、減短高解析history；fast telemetry period倍率2再4（最高60秒且必顯示effective），critical health/控制仍服務。恢復不補跑遺失tick，不內插成真實值。配置變更和overload epoch各自單調，計算dt不依倍率猜測。

Adaptive mode是固定實作、預設開啟；基準REF若進throttled、丟健康樣本或覆蓋<99.9%則效能測試失敗，不能靠自動降採樣隱藏正常負載不足。

## 8. host timeout、重啟與關閉

host manager以read_deadline判定結果過期，遲到回覆不可回寫，stale age不刷新。要求內部host結束時先protocol shutdown 200ms，再SIGTERM等待200ms，再對**確認為受管child的穩定handle**SIGKILL；最多等1000ms確認reap。這個自動終止只針對dgxtop受管host，不是使用者工作程序的ActionPort。

未reap保持quarantined，對該backend不再spawn；可見offline/requires intervention。連續3失敗開circuit，backoff 1/2/4…60秒並有±10% jitter、固定seed可測；只有reap成功且deadline到才half-open一次probe。probe成功先warmup，禁止延用舊generation counter。不假定future timeout或spawn_blocking abort能取消正在執行的阻塞呼叫。[Tokio官方的取消界線](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)。

Shutdown先撤銷所有尚未sending的token（包括intent_pending／intent_committed）、停止派新工作、發布shutting_down，還原terminal目標≤250ms；再flush必要config/audit、bounded停止host，全部應用程式控制等待≤3s。kernel不可中斷I/O、driver bug或slow terminal本身可能超出此界線，必須留下quarantined/unknown狀態，不宣稱已回收。NSS使用者名解析禁止在render／Manager做同步查詢；無cache時顯示數字UID。

## 9. 跨程序payload配額

父程序在派發ReadPlan前預留該request的wire＋decoded最大credit；所有host共用8MiB ingress帳目，尚未取得credit只保留有限read plan而不啟動資料buffer。每host由manifest限制bounded scratch（算入catalog/state分區），傳輸與解析buffer的最大容量也在parent ledger保留，即使實體位於child。byte permit直到host送完、parent解析與Collector交接完成才釋放；不能因跨process不可見就不計。vendor library內部未知配置無法由此wrapper保證，因此另外量測程序樹RSS；達不到上限時功能標degraded，不宣稱已限制驅動內部分配。最大5MiB批次預留會降低同時在途數，正常小批按readplan可證明上界預留，不能先跑driver後才要quota。
