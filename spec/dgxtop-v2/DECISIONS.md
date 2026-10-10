# 決策紀錄

使用者已批准六元件架構及完整重寫；本版效能數值與具體實作策略為依「穩定、高效率」細化的設計決策，尚非實測達標承諾。需要偏離時先記錄原因、代價及驗證，不默默改語意。

| ID | 決策 | 理由／替代方案／代價 | 可逆性 |
|---|---|---|---|
| ADR01 | 六元件、兩路徑 | 保留批准的邊界；不把讀取/操作/畫面塞入AppState | 模組可增，不改權限界線 |
| ADR02 | TelemetryPort與ActionPort隔離 | 防Collector取得kill；拒絕單一任意execute接口 | 擴充action可另定capability |
| ADR03 | Collector決定canonical，來源證據保留 | 不是信任driver品牌或多數表決；衝突可見 | policy revision可改，舊歷史不改 |
| ADR04 | HotStore單寫／Manager單一邏輯寫入 | 多writer鎖競爭替代方案無需求；讀投影低成本 | 可換storage實作 |
| ADR05 | safe kill完整納入 | 使用者已要求Manager具有kill；唯讀只是模式非砍功能 | 動作不可回滾 |
| ADR06 | ProcessKey＋heldpidfd＋先durableintent | barePID快但有reuse競態；不接受 | 不支援核心就拒絕action |
| ADR07 | 編譯期Adapter/Display registry | dynlib ABI與不可信plugin成本不值得；維持可擴充trait | 將来IPC外掛需新安全設計 |
| ADR08 | live/replay分namespace | 模擬可測但不得污染真實資料 | 固定安全不變量 |
| ADR09 | Linux GNU先驗證 | 編譯不等於GPU driver相容；不隱含macOS或所有MIG支援 | 新平台按矩陣追加 |
| ADR10 | 規格驗證與軟體驗收分開 | 文件的測試不是Rust已完成 | 固定交付慣例 |
| ADR11 | 所有liveAdapter常駐host | 純thread较省RSS但無法可靠終止卡住FFI；多程序增加IPC/常駐成本，計入REF預算 | 經profiling可合併安全backend，但需重新批准故障邊界 |
| ADR12 | 專用thread＋crossbeam，不導入Tokio | 現在沒有大型async網路I/O；避免spawn_blocking無界等待誤當取消 | 未來有需求再測比較 |
| ADR13 | 32batch與8MiB ingress雙上限 | 前版256×1MiB理論就用完總預算；byte permits先配置 | 容量restart-only且需bench |
| ADR14 | raw300s＋分鐘24h，分區硬上限 | 全精度24h與多候選成本過大；retain不是無條件容量保證 | quota可配置並rebenchmark |
| ADR15 | counter bridge保精確、不假均攤 | 跨分鐘不能只取整桶也不能憑空均勻分配精確bytes | 丟bridge必回boundary_gap |
| ADR16 | AuditFULL，ArchiveNORMAL且預設關閉 | 保護動作可追溯；掉電可能失去近期archive，明示不同耐久性 | 可開archive但不能降audit |
| ADR17 | SQLite≥3.51.3＋writer checkpoint | 官方WAL-reset修正版門檻；版本pin不是替代runtime驗證 | 安全更新可升版，需測 |
| ADR18 | dirty render4fps，終端nonblocking | 監控不需60fps；單純少畫仍不能解慢終端阻塞 | 配置fps最高20需量測 |
| ADR19 | REF完整工作量與72hsoak | 小demo/平均FPS不能證明穩定；需量測程序樹與長期洩漏 | 門檻變更需使用者同意 |
| ADR20 | 不宣称exactly-once signal | DBcommit與syscall無共同交易，crash窗口無法消除 | at-most-once＋unknown，不自動重送 |

數值全部是設計目標／上限。資源不足優先可見降級而非靜默失真。高信心部分是可分離的責任與不變量；硬體driver成本、RSS與性能目標要由實作量測，不提供虛構信心百分比。
