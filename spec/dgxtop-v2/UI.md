# Viewer 與 Controller 的完整交互規格

## 1. 資料與排版

Viewer只讀不可變ViewModel，不直接Query Store。Manager的PresentationService依已訂閱topic、view/session generation生成小投影；全圖历史range由非同步HistoryService返回。ModuleViewModel保留value／unit／source／kind／quality／age／conflict／coverage；viewer只是格式化，不重新仲裁或推算权威速率。

十個module全部實作：cpu、gpu、memory、disk、network、processes、history、sources、settings、actions。cpu涵蓋package/core與負載，gpu涵蓋逐實體與已支援健康欄位，disk分device I/O及filesystem，processes顯示single ProcessKey的CPU/RSS與多GPU關聯；來源診斷可看原始候選／排除理由／policy；actions呈現pending/sent/still_running/unknown，不能把sent畫成已終止。

DisplayModuleManifest提供api_version=1、module_id、topics、min_width/height、priority、default_enabled。編譯registry可加module無須修改LayoutEngine核心；缺topic顯示無資料／不支援，不panic。Viewer模組panic在unwind範圍內隔離為module error；不假稱能catch abort或UB。trusted built-ins不允許渲染時阻塞或忙迴圈，以static lint與budget測試約束。

## 2. 尺寸與品質顯示

寬≥120用左右欄；80～119以單欄概要＋active detail；寬<80或高<24降為最小摘要與keyboard導航；低於40×10显示「終端過小」，仍可退出。确认／错誤／overload banner优先，不裁掉安全資訊。測160×50、100×30、80×24、40×10、20×5及resize churn。

有效0正常顯示0。無值顯示「不支援／讀取失敗／來源衝突／過期」，last_good另示最後成功時間而不是替代目前值。`estimated`標「估算」，`simulated`標「模擬」，source_conflict有文字與符號；不能只用色彩或單一信任百分比。

## 3. 按鍵與狀態

| 輸入 | Controller命令／行為 |
|---|---|
| q／Ctrl-C | Shutdown；filter input內Ctrl-C仍可退出，普通q是搜尋字元 |
| 1／2／3、Tab／Shift-Tab | 切Overview／GPU／Processes，更多module由目錄選擇 |
| j/k或方向 | 依ProcessKey/entity移動；不可只存row index |
| h/l | GPU detail切實體 |
| / | 進filter mode；輸入最長256 UTF-8 bytes，Esc清除或退出 |
| s／r | 排序欄／升降，穩定tie-breaker是ProcessKey |
| +/- | SetSamplingInterval(expected_revision)，顯示pending→configured/effective |
| K | 只對可見明確選取的ProcessKey PrepareSignal(SIGTERM) |
| y/Enter | 只有confirmation mode且active token才ConfirmSignal；其他mode不確認 |
| n/Esc | CancelSignal；顯示cancelled或too late，不重送 |
| ?／F1 | 幫助，列出同一份keymap資料 |
| o | 開來源診斷；filter/pin/exclude由ReplaceConfig CAS提交 |
| F2 | 設定；顯示需重啟欄位，不默默套用 |

focus state在Manager ViewSessionService只有一份，Controller暫存未提交文字編輯buffer與cursor但不自行變更權威選取。event包含frame_revision/HitMap；舊frame點擊破壞性動作回StaleView要求重選。輸入過快可coalesce純navigation，但已accepted destructive command永不coalesce。

## 4. frame與allocation

data-driven frame預設上限4fps，最高20，沒有revision/viewport/age-display變動不重繪。age以1秒粒度更新；沒有資料每秒最多一次狀態檢查，禁止0ms poll。字串、行樣式與圖表降採樣結果依revision/width快取；只格式化viewport列與固定overscan。選取色變化只更新相關列，不每幀clone來源詳細JSON。

render p95計時只含layout、module render、Ratatui diff，不含終端模擬器實際顯示。輸入Ack≤50ms不代表50ms畫面已更新；正常4fps可見更新上界還有250ms週期和終端I/O，測試要分開報告。效能門檻不得藉不處理輸入或不出frame達到。

## 5. 慢terminal輸出也必須有界

Unix backend寫出採O_NONBLOCK及poll writable，保存／還原原來FD flags；使用受控輸出buffer≤1MiB，單loop最多8KiB或2ms寫入budget。一個frame已開始輸出就保留剩餘bytes順序完成，不能從ANSI escape中間截斷或夾入新frame；未輸出新revision只留latest。Ratatui下一次draw必須等上一frame完整flush後再提交其diff基準，失敗要求完整重繪。

EAGAIN時Controller仍處理鍵盤／shutdown、Manager仍采樣；持續30秒不讀PTY輸出的測試不能增長buffer、丟命令或卡死。關閉時best effort還原mouse/raw/alternate screen与flags；若終端完全不接受bytes，不能保證復原序列可見。使用者終端resize/斷線造成EPIPE/EIO時停止render、撤銷未執行动作，安全退出。

## 6. 設定與操作視圖

Settings顯示已保存／CLI override／effective／overload四種來源與revision；存盤失敗保持舊值。Action確認顯示UID、basename、PID、start identity摘要、signal、倒數TTL與不可回復警告；token不顯示、不寫log。操作回覆獨立於telemetry訂閱，避免中間frame合併丟失結果。歷史视图同時顯示requested range與實際coverage；部分分鐘或來源切換缺口畫斷線，不填零。
