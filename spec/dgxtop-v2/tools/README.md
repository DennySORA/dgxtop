# 規格附屬工具

這些是已可執行的規格驗證工具，不是dgxtop重寫實作。

`python tools/generate_fixture.py --series 8 --cycles 3 --seed 20261004 --out fixture.jsonl` 產生合成溫度來源/alias觀測。它是仲裁案例產生器，不是完整REF硬體負載；REF包含其他metric/程序/host，需要Rust harness依bench/scenarios.json實作。

`cargo metadata --format-version 1 --locked > metadata.json`，再 `python spec/dgxtop-v2/tools/check_boundaries.py metadata.json` 檢查工作區production依賴；若規格不在spec/dgxtop-v2請改相對路徑。不取代程式碼審核與runtime權限。

`python tools/evaluate_performance.py performance-report.json --evidence-dir benchmark-results/` 嚴格驗證3次REF、固定來源/churn兩次72h、門檻及原始檔hash。報告schema見contracts/performance-report.schema.json；未量測時不要填假的measured報告。

0為通過，1為門檻/依賴違反，2為不合法輸入。單元測試中的假報告只測檢查器，禁止匯出作驗收證據。所有腳本預設本地檔案操作，沒有driver/signal/網路與破壞性行為。
