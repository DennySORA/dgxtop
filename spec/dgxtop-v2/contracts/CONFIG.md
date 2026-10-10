# 設定契約 v2

`config.schema.json` 定義完整形狀、型別與範圍，`../examples/config.default.json` 是唯一預設值來源。所有欄位必填、拒絕未知鍵；schema 不會自動套預設值。CLI 只覆寫明確給定欄位，未給參數不能覆蓋持久設定。

## 語意驗證（在 JSON Schema 之後）

每個有限浮點數還須 `is_finite()`；十進位整數須實際解析確認不超出 u64。revision 為 CAS 版本，candidate.revision 必須等於 current.revision，由 Manager 成功提交後 +1。達到上限則拒絕並要求受控遷移，不回繞。pin 必須存在於相同 MetricKey、不得同時 exclude、不得重複；模組必須已註冊且 API 相容。

安全常數：same_uid_only=true、deny_root_session=true、allow_suspect_as_canonical=false、persist_full_command_line=false、adaptive_enabled=true。actions.enabled=false 或 read_only=true 拒絕所有 Signal。預設 allow_force=false；經明確設定啟用才允許另一次完整 SIGKILL 確認，不自動升級。

多欄容量驗證不能只逐欄通過就算有效：commands ≤ runtime.max_pending_requests；wire bytes ≤ decoded batch bytes；wire＋decoded 的單一最大批次 ≤ ingress bytes；commands×64 KiB＋archive bytes＋log bytes＋控制 bookkeeping 必須落在 20/256 的 reserve 分區，必要時限制單一命令實際上限而不是偷偷超額。預設 reserve 20 MiB 可容納 command 8 MiB、archive 8 MiB、log 1 MiB，餘 3 MiB 保留控制紀錄；command payload 在排隊／執行時移交所有權，不複製。history_requests ≥ history_workers；session 數與 projection 大小符合 presentation 分區；2 個查詢結果與 pinned view 應計入各分區。

`memory_limit_mib` 是整個受管資料預算，不是只有 Store。依 RUNTIME 的分區同比縮放；validate admission 同時計算 catalog＋process baseline、每 series 最少2樣本與聚合目錄、最大 command/request/IPC、query/projection，以及既有被 pin 的 chunk。無法容納回 CapacityExceeded。保留時長是軟目標；各分區與總額是硬上限。預設 max_series=512 僅非程序歷史；程序 current/raw 另用 max_processes=256、max_process_gpu_links=1024 管理，避免每個短命 PID 永久占一個 series。

## 生效矩陣

| 分區／欄位 | 預設目的 | 生效與失敗行為 |
|---|---|---|
| sampling.interval_ms / gpu_enabled | 1s；GPU 可關閉 | staged scheduler→持久保存→新 epoch；NVML 關閉不殺使用者 GPU 程序 |
| capacity / slow_health / discovery / unsupported_probe | 5s / 5s / 10s / 60s | 多速率 read group；unsupported 不每秒反覆探測 |
| quality | 遲滯、對齊、容差與 pin/exclude | 新 policy_revision；不改寫既有歷史；8°C 是設計容差，非通用真偽標準 |
| store retention | raw 候選60s、canonical300s、聚合24h | quota 先於時長；容量縮小只在安全 admission 後做，不能丟未到期稽核換取 kill |
| actions | 同 UID 安全程序操作 | 政策變更立刻撤銷未進入 sending 的 token；確認期10s |
| viewer | 4fps、最多2session、10模組 | 切換不可變投影；不改採樣；network_max_visible／redline 只影響顯示 |
| queues、runtime、sampling.max_adapters | thread／queue／IPC 拓樸 | 全部 restart-only；ReplaceConfig 回 RequiresRestart，不先寫檔 |
| store.memory_limit_mib、max_series、max_sources、max_processes、max_process_gpu_links | 全域 admission | restart-only，避免在線重新配置引入瞬時雙倍記憶體 |
| privacy | 不持久化完整命令列 | 不可變安全政策 |

每源 effective interval = max(requested, verified backend minimum, group minimum) × adaptive multiplier，上限60s。未知 backend 最短週期保守用1000ms；source_update_interval 已知時也納入 freshness。read deadline可以長於週期，但 host 最多一個在途，skip 過期 tick 不補跑。fresh deadline = last_success + stale_factor × max(effective interval, 已知 source update interval)；讀取失敗不更新 last_success。裝置 sample timestamp 缺失時 measurement_freshness=unknown。

## 原子性、路徑與恢復

stage 先驗證容量、modules、來源與排程；temp 0600→fsync→同目錄 rename→directory fsync→不可失敗的 epoch 交換→Ack。失敗不回成功；目錄同步失敗而 rename 已發生時，回 PersistFailed/commit_uncertain 並安全重載磁碟版本，不宣稱舊版必定仍在。詳 STORAGE、OPERATIONS。

路徑 `$XDG_CONFIG_HOME/dgxtop/v2/config.json`、`$XDG_STATE_HOME/dgxtop/v2/{audit,history}.sqlite3`；缺 XDG 時使用 `$HOME/.config`／`$HOME/.local/state`。0700目錄、0600檔案、防不安全 symlink。v1／legacy 僅明確 --import-legacy，先dry-run保留原檔；舊版本 key 不默默改義。
