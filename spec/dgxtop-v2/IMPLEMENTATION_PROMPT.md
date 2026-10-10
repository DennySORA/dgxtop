# 給 Codex／Claude Code 的實作任務

你在 DennySORA/dgxtop repo工作，已核准的重寫規格位於本文件所在資料夾。請完成全部Rust實作與測試，不只重新規劃或生成骨架。先遵守repo既有AGENTS/CLAUDE、保留未提交變更，讀本包AGENTS及README閱讀順序。基準SHA b4b5a8d87d3c148cedacf91daf6f926719694a18；若repo已更新，讀真實diff後移植，不重置成舊版。

依TASKS T01～T15落實Adapter/Collector/Manager/Store/Viewer/Controller。Adapter可擴充且telemetry/action隔離，Collector有可解釋多來源仲裁，Manager模組化，Store有精確歷史/設定/稽核，Viewer全部10模組，Controller typedcommands。保持真實缺值、同源去重、ProcessKey、generation/epoch、sameUID pidfd安全kill與結果。

效率與穩定性是接受條件：固定worker、常駐host、readgroup合併、多速率、queue項目與bytes雙限制、全域256MiB受管data、不可變小投影、nonblockingterminal、typed boundedqueries、晚到結果拒絕。bench/budgets.json不能降低以通過。REF含parent與全部host，正常情況不可靠掉資料/節流過關，跑三次30min及兩組72hsoak。

每個R/AT先有測試，包括timeout、diskfull、WALreader、PIDreuse/tokenexpired、lateaudit、重複commands、熱插拔/clockjump、慢PTY、sourceconflict與overflow。硬體測試僅自建子程序和受控環境，不終止使用者真實工作。不要自動commit/push/tag/deploy，也不要套用需要外部權限的操作。

完成fmt/check/clippy/test --locked、boundary檢查、單元與integration/PTY、安裝隔離及效能證據。實作VALIDATION逐AT填PASS/FAIL/NOT_RUN/BLOCKED、命令/退出碼/環境/證據hash。缺硬體時完成所有其他範圍並保留硬體gate BLOCKED，不以樣板/假資料或未測成功取代；software acceptance與spec文件驗證分開。交付實際可執行程式、必要遷移/操作文件與真實測試結果。
