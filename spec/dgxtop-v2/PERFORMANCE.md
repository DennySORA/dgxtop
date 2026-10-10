# 效能與長時間穩定性

本文件所有門檻是**驗收目標，不是已達成的 benchmark**。數字唯一來源為 `bench/budgets.json`；目前未執行重寫的 Rust 程式。功能完整、結果正確、程序操作安全不能用效能理由刪減。任何門檻變更須在 DECISIONS 記錄原因與使用者重新同意，不能讓驗收工具自動放寬。

## 1. 固定工作量與環境

REF：512 非程序 canonical series、2048 sources（每key最多4候選）、256唯一程序、1024程序/GPU關聯、7個常駐Adapter host、1s採樣、160×50終端、1個session、4fps上限、archive關閉、audit開啟。所有來源都是健康、範圍合法、含溫度仲裁；要有計數器、gauge、missing/fault間歇樣本，但故障樣本在coverage分母按預先定義能力排除，不臨時挑有利區段。Replay host 模擬固定 API 延遲1ms，只用於軟體成本測試；真實 NVML 成本另以硬體矩陣測量，不能冒充同一結果。

受控 benchmark 主機最低4個實體CPU核心、16GiB RAM、本機SSD、x86_64/aarch64 GNU Linux。核心型號、CPU頻率政策、Rust/SQLite/driver、背景負載、release flags、commit及config hash全部記錄。沒有固定某一CPU型號所以5%門檻是目標，不是所有機器保證；至少在使用者DGX Spark與一台獨立GPU GNU Linux各出報告後才能標「該平台已驗收」。其他平台維持未驗證，不以交叉編譯代替。

## 2. REF 門檻與定義

| 項目 | 門檻 | 定義 |
|---|---:|---|
| 程序樹平均CPU | ≤5%一個邏輯核心 | 100×Σ(parent+所有host+已退出child的CPU秒增量)/wall_seconds；不是除以主機總核心數 |
| 程序樹峰值RSS | ≤384MiB | ΣVmRSS，parent與host都算；shared page可能重複計算，保守使用此口徑 |
| 受管資料配置 | ≤256MiB | RUNTIME分區、active／pending／pinned／retired Arc／IPC，非只有live Store |
| 命令接受／拒絕 | p95≤50ms、p99≤100ms | 完整輸入事件交給Controller→Manager回accepted/rejected；不是動作完成或螢幕flush |
| 單frame計算 | p95≤16ms、p99≤33ms | immutable projection已到→layout/draw diff完成，不含terminal transport |
| 最新資料投影 | p95≤100ms | Adapter read_finished→Manager發布對應revision，排隊時間包含在內 |
| 24h記憶體歷史查詢 | p95≤100ms | submit→最多2000點result完成；預載真實shape24h聚合後測 |
| 正常資料覆蓋 | ≥99.9% | 預定可採樣區間的有效coverage；失敗／skip仍記帳、不能以改分母掩蓋 |
| 非預期丟棄、crash、deadlock | 0 | 正常REF不能啟動throttle／靠丟資料過門檻 |

MiB=2^20bytes；KB/MB與KiB/MiB不混用。分位數用 nearest-rank：排序後取 ceil(p×N)-1；採全部樣本，不平均不同run的p95。至少3次獨立REF，每次warmup60s後有效測量1800s，全部run均通過；不得只交最好一次。每次至少10000命令（以10Hz有界腳本送非破壞性view命令）、1000frames、1000history queries（並行≤2）。命令flood及慢terminal屬另項故障測試，不放進正常benchmark稀釋延遲。

CPU含warmup後完整30min，最多1s間隔收程序樹/cgroup統計；sample成本由外部量測程序報告並保持相同。active module與test workload必須一致。SQLite archive關閉時不能故意停掉audit或少啟host。測量程序自身不計入dgxtopCPU，但其開销／背景負載要記錄。

## 3. 72 小時 soak 與過載

SOAK：同REF，24h讓聚合填滿後，接續48h穩定期：峰值RSS仍≤384MiB，`max(RSS於24..72h)-max(RSS於23..24h)`正值≤16MiB；FD/thread的最後1h上界不得高於23..24h上界，且時間序列不存在未回收線性增長。固定來源與另一次churn（每分鐘替換10%來源、同PID新starttime、interface復用）各跑一次；churn造成正確boundedeviction要記錄，不能造成歷史串接。完整72h，不接受把fakeclock推72h當soak；fakeclock只驗證時間演算法。

OVERLOAD：提供8192sources／4096series、10Hz請求、2session；允許明確CapacityExceeded、skip與節流，不允許OOM、無界排隊、UI死鎖或悄悄把未知寫成0。受管資料始終≤配置上限；保留命令反應與shutdown。外部driver不可中斷/D-state可能無法回收，quarantine+host總數上限必須有效，不能宣稱作業系統保證3s殺掉任何進程。

故障注入另測：NVML host卡60s、熱插拔1000次、invalid IPC長度/深度、FS capacity卡住、PTY不讀30s、SQLite鎖定、磁碟滿、WAL reader卡住、audit ACK延遲超TTL、UTC跳±24h、BOOTTIME跨休眠、連續20次config更新、10萬commands flood。每例明定結果見ACCEPTANCE。

## 4. 優化順序與禁忌

先以 flamegraph／profile（可用perf時）及內建histogram確認成本，再優化 parse/batch、catalogcache、readgroup合併、snapshot增量投影、boundedchunks；最後才考慮額外worker。不要先導入Tokio或複雜lock-free以假設會比較快。固定worker與message passing已足夠表達目前需求。

不得省略 provenance／缺值、錯把相同origin兩票、不記 audit fsync、sampling頻率顯示假值、提前確認signal成功、無限制Arc snapshot／queue。不得用單一 shared Mutex<AppState> 包住採樣到render。不得以 release模式關閉overflow/identity檢查掩蓋漏洞。UI 只dirty render；epoch stale 丟棄必須可觀測。

## 5. 證據輸出

Rust bench harness輸出原始latency、resource series與測試環境，轉成 `contracts/performance-report.schema.json`。`tools/evaluate_performance.py REPORT --evidence-dir DIR` 驗證schema、門檻、每個evidence檔SHA256；kind不是measured即拒絕；config_path所指檔案必須符合config_sha256與REF固定預設。總報告包含3個獨立runs及fixed/churn兩個soak_runs，不平均各run結果。工具只評估輸入，不能證明輸入實測未偽造，審核者仍要查原始報告、命令、Git SHA與硬體矩陣。合成fixture明示synthetic，不會被它當成實測PASS。

CI的時間閾值不使用共享runner波動作硬體保證；共享runner執行正確性、容量／無界增長模型與smoke；指定硬體跑REF、SOAK及release certification。未取到實機或72h結果時該gate是NOT_RUN／BLOCKED，不能改成PASS。
