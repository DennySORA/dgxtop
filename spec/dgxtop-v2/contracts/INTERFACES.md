# 跨層契約與實作埠

本文件定義本機 Rust 介面語意；JSON 是 fixture／adapter-host 的版本化 DTO，不表示新增 HTTP 服務。所有命令與回覆均為 typed enum／struct；不提供任意 execute(string) 或 query SQL。

## 1. 共用身分與數值

| 型別 | 固定欄位／驗證 |
|---|---|
| MetricKey | host_id、boot_id、entity_id、metric_id、scope；不得省略 scope 後混用 package／SoC |
| SourceKey | adapter_id、device_id、channel、generation；source_id 穩定於該來源 incarnation，不使用 hwmonN 排序當唯一身分 |
| OriginIdentity | origin_id＋confirmed／unknown；只有已驗證拓樸/profile 能確認同源；unknown 不形成獨立票數 |
| ProcessKey | host_id、boot_id、pid_namespace、pid>0 且 ≤ i32::MAX、start_time_ticks；同 PID 不同起始時間不同身分 |
| Scalar | f64（只允許有限數）或 u64（精確）；JSON u64 是十進位字串，語意上 ≤ 18446744073709551615 |
| EntityRecord | entity_id、kind、parent_id、generation、discovered_boot_ns、removed_boot_ns|null、capabilities |
| SourceDescriptor | SourceKey、OriginIdentity、driver、API version、path、channel、native unit、raw kind、update interval|null、profile id/revision |
| Capability | metric/action id、scope、supported／unsupported／unknown、reason、min_interval_ms|null |
| TimeSpan | 同一 boot 的 start_boot_ns／end_boot_ns；start ≤ end；跨 boot 不可相減 |

字串上限採 schema；公開顯示移除終端控制碼但 Store 保留有限、可稽核的 escaped 形式。cmdline 不是 MetricKey，不作時序標籤。程序主機資源按 ProcessKey 採樣一次，GPU 程序資料按 `(ProcessKey, gpu_entity_id)` 關聯。

## 2. 資料工作埠

| 埠／操作 | 輸入 | 輸出與語意 | 呼叫者 |
|---|---|---|---|
| TelemetryPort.discover | DiscoverRequest {host, boot, previous_generation, deadline} | CatalogDelta {upserts, removed, capabilities, errors}；局部失敗不變全空成功 | DeviceCatalog／runner |
| TelemetryPort.read | ReadPlan {cycle_id, config_revision, sources, read_groups, deadline_boot_ns} | ObservationBatch {records, individual_errors, duration, generation}；wire 最大1 MiB、decoded 最大4 MiB、最多256筆，合計受8 MiB ingress限制；局部來源錯誤逐項保留 | Collector，經 AdapterBroker |
| CollectorPort.collect | CollectionPlan {catalog_snapshot, policy_snapshot, committed_state, read_plan} | CollectionBatch {canonical, observations, explanations, state_delta, failures, epoch} | Manager scheduler |
| StoreWriter.commit_collection | CollectionBatch＋expected_epoch | CommitResult {snapshot_revision, accepted, rejected_reason}；全部驗證／邏輯原子提交 | Manager IngestionService |
| SnapshotReader.read | SnapshotQuery {topics, entity_ids, max_revision|null} | ImmutableSnapshot {revision, samples, resources, health}；不得拿 mutable handle | Manager PresentationService |
| HistoryReader.query | HistoryQuery（下表） | HistoryResult {points, bounds, coverage, gaps, method, truncated, source_changes} | Manager HistoryService |
| ConfigRepository.load/CAS | 讀取；或 expected_revision＋validated candidate | CurrentConfig 或新 revision／Conflict／PersistFailed | ConfigurationService |
| AuditRepository.append_intent | ActionIntent {request_id, ProcessKey, policy_revision, signal, session_digest} | DurableReceipt 或錯誤；沒 receipt 不執行 signal | ProcessService |
| AuditRepository.append_outcome | intent_id＋ActionOutcome | 提交結果；重複 id＋相同內容冪等，不同內容 Conflict | ProcessService |
| ProcessActionPort.prepare | ProcessKey＋已核准的 CapabilityContext | PreparedProcess {opaque_handle, verified_identity, display_summary}；真正 OS handle 不送 UI | ProcessService |
| ProcessActionPort.send | opaque_handle＋允許的 Signal＋durable intent | SignalResult {sent, permission_denied, exited, changed, unavailable, error} | ProcessService |
| ProcessActionPort.observe_exit | 同一 opaque_handle＋非阻塞輪詢 | running／exit_observed／unknown；不對任意非 child 誤用 waitpid | ProcessService |

CapabilityContext 由 Manager 依本機程序真實 euid 與受管 session 建立，不接受從 JSON 傳來的 admin=true。ProcessActionAdapter 與 telemetry 的持有權／trait 分開；runtime 裝配時用私有 constructor 使只有 ProcessService 拿到 ActionPort。

CollectorState 在 commit 成功之前不成為下一次 delta 基準。如果批次因 epoch 過期被丟棄，下一次計算必須用同 epoch 的正確前態或 warmup；不能讓持有舊 policy 的 worker 偷改全域基準。

## 3. 仲裁政策的精確界線

每個 source 必須有 MetricKey、profile_rank（小者優先；政策順序，不是信任機率）、quality、freshness、sample_kind 與 native/canonical unit。只有 ok＋讀取 fresh＋verified/usable＋允許資料 namespace，且量測未被確認 stale 的來源能進入 canonical。CanonicalSample.freshness 是讀取新鮮度，measurement_freshness 是量測新鮮度；缺 device timestamp 時後者是 unknown，不能提升為 fresh。read_finished_boot_ns 與 observed_boot_ns 分開保存；後者無法映射或沒有單一來源時為 null。

先按 key、origin 去重，再比較有時間對齊證據的不同 confirmed origin；來源身份／獨立性未知時保留 uncertainty，不提高整體信心。符合容差才依等級／profile_rank／age／source_id 選一個。不同 confirmed origin 超過容差則產生 conflict，value=null；manual pin 也不能隱藏衝突或越過硬失敗。使用者可明確改政策排除某來源，但必須保留被排除理由和 policy_revision，不能刪原始證據。

pin 不存在、unit未知、fault、stale、suspect 或與 key 不一致時，不默默選其他來源而宣稱「已鎖定」；回 pinned_source_unavailable，並在診斷提供可用替代建議。沒有 pin 才按備援政策自動切換。遲滯只作用於健康來源間的切換，不阻止硬失敗後立即降級。

`temp_input` 的 0 值不預設無效；fault=1 可排除，alarm=1 僅代表警報，不可因此把真正高溫資料丟掉。平台 profile 的有效範圍不等於警報門檻。未經實證的「固定不變」只作診斷，不直接 invalid。

## 4. Manager 公開入口

CommandPort.submit(CommandEnvelope) 非阻塞：accepted {request_id} 或 rejected {request_id,error}。接受不代表完成。CommandResult 用相同 request_id 回 completed／rejected／in_progress／unknown。

QueryPort.get_snapshot(SnapshotQuery)、query_history(HistoryQuery)、explain_source(MetricKey)、get_config()、get_action(request_id) 回異步 query result；各自有 deadline 與容量。SubscriptionPort.subscribe(topics,session) 發最新 ViewModel；只保留一份 pending projection，revision gap 允許重取快照。

### Commands

| 命令 | 必要輸入 | 結果／政策 |
|---|---|---|
| SetSamplingInterval | interval_ms、expected_config_revision | 完整設定驗證＋scheduler Ack，回新的 configured／effective interval 與 revision |
| ReplaceConfig | expected_config_revision、完整 candidate | 全文 CAS，版本由 Manager +1；queue 變更運作中回 RequiresRestart |
| UpdateViewSession | view、selected_entity_id／selected_process、filter、sort、ascending、frame_revision | 依穩定 entity 身分更新；stale frame 引用回 StaleView，不轉成另一列 |
| PrepareSignal | ProcessKey、SIGTERM／SIGKILL | 返回 pending confirmation 描述及 token，不送 signal |
| ConfirmSignal | confirmation_token | 核對 session、TTL、policy、identity、稽核 intent；回 signal_sent 或具體拒絕 |
| CancelSignal | confirmation_token | 冪等取消，關閉 held pidfd；未存在亦不執行任何動作 |
| SetModuleEnabled | module_id、enabled、expected_config_revision | 驗證 registry；更新 config／layout，不能啟用未註冊程式碼 |
| Shutdown | reason | 撤銷 token、停止新採樣、要求 worker 結束、還原 terminal；不替使用者殺外部程序 |

CommandEnvelope 的 request_id 是 UUID；schema format 須開啟驗證。session_id 只是路由 key，受管 session 綁定 caller context 才有權。readonly、actions.disabled、root session、wrong UID／namespace 一律拒絕。同 request_id＋不同 payload 回 Conflict；同 payload 回既有結果，不能重複發 signal。

### HistoryQuery

`key: MetricKey`、`start_boot_ns: u64`、`end_boot_ns: u64`、`resolution_ms: 0|≥100`、`stat: mean|min|max|total|raw`、`max_points: 1..設定上限`。end > start；range 跨 boot 必須拆成多個 query。source_filter 可為 null 或有效 source_id，查詢後仍保留 canonical／raw 區分，不能把 raw 當 canonical。

結果 `points[]` 每筆含 time_span、value|null、source_id|null、quality、method、covered_ns、missing_reason|null；整體含 requested_span、available_span、coverage_ratio、truncated、clock_discontinuities、snapshot_revision、next_start_boot_ns|null。只有全部落在時窗內的已聚合桶可當精確結果；部分邊界若原解析已淘汰，就省略該段並降低 coverage，不能假定整桶均勻分布後按比例偽裝精確。仍有原解析時按STORAGE.md的步階模型切段，method 必須標示 time_weighted_step。

## 5. 事件、錯誤與動作狀態

事件必有 event_id、schema_version、boot_id、occurred_boot_ns、correlation_id、kind、typed payload。事件 kind 包含 CatalogChanged、CollectionCommitted、SourceChanged、SourceConflict、SourceUnavailable、ConfigApplied、ModuleHealthChanged、GapRecorded、CommandCompleted、ActionPrepared、ActionIntentCommitted、ActionSignalSent、ActionExitObserved、ActionOutcomeUnknown。事件大小≤64 KiB；metrics 不放完整 cmdline。

控制／稽核事件不得當作可丟的畫面事件。UI 中間快照可合併；歷史／稽核儲存的 committed event 由 Store 保證記錄。丟採樣批次要產生有界計數／Gap，不為每一筆丟棄無界排事件。

錯誤共用 `code`、`message`、`retryable`、`request_id`、`affected_ids`。允許 code：ValidationFailed、Conflict、Unsupported、Unavailable、PermissionDenied、CapabilityDenied、Busy、TimedOut、StaleView、SourceConflict、PinnedSourceUnavailable、CapacityExceeded、PersistFailed、RequiresRestart、ProcessExited、ProcessIdentityChanged、NamespaceMismatch、ConfirmationExpired、ConfirmationUsed、UnknownOutcome、InternalError。UnknownOutcome 不可自動重試破壞性操作。錯誤文字須去秘密及終端控制字元。

動作狀態：requested → prepared → confirming → intent_pending → intent_committed → sending → signal_sent → exit_observed；requested/prepared 可轉 rejected/cancelled/expired；intent_committed 之後若崩潰且結果無法證明，轉 unknown。取消只保證尚未送出前生效；若已進入 send，回 in_progress／signal_sent，不能承諾已撤回訊號。Manager ProcessService 序列化 token 狀態轉移；專用 action worker 只執行已核准工作，DB I/O 不阻塞 Manager。

## 6. Viewer／Controller 契約

DisplayModule 描述使用 display-module.schema.json；handler 介面 `render(&DisplayContext, &ModuleViewModel, area) -> RenderOutput`，其中 RenderOutput={cells/draw_commands, hit_targets, accessibility_labels}。實際 Rust 可借用 Frame 避免大量分配，但不得因此共享領域 mutable state。

DisplayContext 只有 theme、viewport、tick_for_animation、formatting locale；無 TelemetryPort、ActionPort 或 StoreWriter。ModuleViewModel 是 manager 準備的子 projection，含 source explanations、sample age、unit 與 missing state。

HitTarget={frame_revision, module_id, control_id, entity_id|null, process_key|null, rect}。Controller 用 HitMap 與 InputContext 形成 typed Command；不是讓 DisplayModule 執行 callback 直接 kill。過期 frame 的 destructive command 不得自動換到目前選取；PrepareSignal 仍須帶完整 ProcessKey。

Module registry 是受信任的編譯期清單，不能把 config 中的字串當 dynamic library path。api_version 不等於1時啟動拒絕該模組並可見告警；不是 Rust ABI 兼容保證。

## 7. Adapter-host IPC 與生命週期

僅 parent/child 匿名管線，無 TCP／socket listener。使用 4-byte big-endian payload length＋UTF-8 JSON；length 最大1 MiB，先檢查長度再分配。序列 handshake {protocol_version:1, adapter_id, generation}；父程序驗證已建立 child PID／受管 pipe，再允許 read／discover。每次訊息有 request_id 與 deadline；不接受 action command。

host 的 opaque NVML handle 不序列化，source generation 在重新啟動後更新。錯誤 JSON、超長 frame、未知 operation、out-of-order generation 拒絕並標記 host unhealthy。關閉 pipe／EOF 不當作完整空資料。每 host 單一在途，shutdown 要求之後等有界回收；未回收者保留 quarantined 狀態，不無限新增 host。

## 8. v2 版本與提交握手

設定／Command／CommandResult schema version=2；Observation／Canonical／IPC／History／DisplayModule version=1。這些是新設計初始契約，不是已部署協定的互通保證。任何未知 major 拒絕。Rust 的內部 enum 以同名變體實作，不透過 JSON 繞一圈；只有 host IPC、fixture、diagnostics 序列化。

CollectionBatch 額外帶 `batch_id: UUID`、`collector_id`、`host_generation`、`catalog_revision`、`config_revision`、`cycle_id: u64`、`state_base_revision: u64`、`state_delta`。每 Collector 最多一份未 Ack 的 delta；Manager→HotStore 驗 epoch/base，再 commit samples＋history＋state_delta 一次邏輯交易。Ack={batch_id,committed_revision,state_revision,status}；duplicate batch_id 回原 Ack；不同內容 Conflict。epoch/base stale 不能只提交部分 delta。accepted samples/history 指向同一 immutable segment 不深拷貝。

Store 自己實作 memory/chunk、SQL與checkpoint；但只有 Manager 的 IngestionService、ConfigurationService、ProcessService 能提交相應命令。History/Presentation 只有 read port。不可對所有 Manager 模組發一份 unrestricted StoreHandle。

## 9. Epoch 與錯誤補充

SchedulerAck={config_revision,epoch,effective_intervals,applied_boot_ns}。期望 epoch 未來／落後回 Conflict，late host reply 必須記原因且不污染 current。每批 processing deadline 不得超過最早資料 freshness；async timeout 之後回來不算新一輪。

新增錯誤 QueryTooLarge、Cancelled；共用 ErrorDetail 可加 `commit_uncertain: bool`（預設false），表示 fsync/rename 或 audit 结果不可斷定，不能自動再發 signal。CommandResult JSON 的 nullable 欄位必須存在，非相關欄位填 null。

## 10. IPC 具體訊息

4-byte big-endian framing 先檢長度再配置，1MiB wire、4MiB decode、深度32、256 observations。啟動 hello={type:hello,protocol_version:1,adapter_id,generation}；父程式校驗 executable 與實際 child pipe。request={type:discover|read|shutdown,request_id,deadline_boot_ns,host_generation,catalog_revision,config_revision,cycle_id,read_group,source_slots[]}；discover 無 sources，read source_slots≤256，shutdown body空且不等待新drivercall。reply observation批次依 observation-batch.schema.json；discover={type:catalog,request_id,generation,upserts[],removed[],capabilities[],error_code}，每頁最多256 records、總量受 max_sources；完整 catalog 的完成旗標與 generation 一起提交，半頁不能刪掉舊源。stderr 單独有界診斷，stdout 只 IPC，不把任意日誌當frame。

外部 request 不接受路徑或library名稱，source_slots 必須屬 parent catalog。字串intern後的 (slot,generation) 不跨 catalog version重用。reader/writer均非阻塞，長度前綴分段、UTF-8分段、EOF半幀皆須測試。ActionPort 永遠不經 telemetry host IPC。
