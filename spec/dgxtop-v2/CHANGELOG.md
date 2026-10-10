# 從已核准架構 v1 到實作規格 v2

本版不是重新要求核准六元件，也不是前次審查的F項已修復證明。架構B1～B6、原R01～R20與V1～V3保留；新增R21～R40、V4。

| 使用者／先前內容 | 本版處理 | 位置 |
|---|---|---|
| 架構OK | 六元件、來源仲裁、Manager/Store、Viewer/Controller及安全kill全部保留 | ARCHITECTURE、SPEC |
| 效率很重要 | 固定worker、Broker、分頻、cache、immutable小projection、dirty/nonblockingUI | RUNTIME、UI |
| 必須穩定 | byte/item雙配額、quarantine、fairness、磁碟/WAL界線、lateaction防護、72hsoak | RUNTIME、SECURITY、PERFORMANCE |
| v1 ingress 256 batches | 改32batches＋8MiB總額，1MiBwire/4MiBdecoded雙計；理由寫ADR13 | config schema/defaults |
| v1泛化history總量 | 明定cross-minute counter bridge、boundary_gap、固定寬度nsSQL鍵 | STORAGE、history.sql |
| v1 spec的效能提案 | 定义REF工作量、程序樹口徑、三輪30min、每項閾值與證據格式 | bench/、PERFORMANCE |
| v1配置只stage/保存/交換 | 補rename成功dirfsync失敗的uncertain恢復，新增runtime欄位與重啟界線 | CONFIG、OPERATIONS |
| SQLite未锁安全門檻 | runtime≥3.51.3、FULL/NORMAL差異與WAL占用管控 | STORAGE、SOURCES |
| v1檢查結果 | 不沿用舊PASS，重新跑當前package/model/HTML/ZIP；Rust仍未實作 | VALIDATION |

前版T工作包重新細化為15包；R/AT標識不改。舊architecture ZIP不是實作依賴，本包可獨立閱讀。
