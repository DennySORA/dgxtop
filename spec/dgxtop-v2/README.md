# dgxtop 實作規格 v2：穩定、高效率的本機監控工具

文件 ID：`dgxtop-spec`｜文件版本：`2.0.0`｜2026-10-04｜基準 commit：`b4b5a8d87d3c148cedacf91daf6f926719694a18`。

使用者已核准 Adapter、Collector、Manager、Store、Viewer、Controller 架構。本包把它變成**完整重寫的工程規格**，不是重提架構，也不是僅供展示的骨架；程序控制、來源仲裁、歷史、設定、十個顯示模組及發行路徑全部在範圍內。

## 開始使用

先以瀏覽器開啟 `index.html`；單檔、深色、離線。交給 coding agent 時將**整個解壓目錄**放入目標 repo 的 `spec/dgxtop-v2/`，不要只貼 HTML，也不要把本包的 AGENTS.md 直接覆蓋 repo 原有檔案。包內相對路徑均以本目錄為根。

讀取順序：`AGENTS.md` → `IMPLEMENTATION_PROMPT.md` → `SPEC.md` → `ARCHITECTURE.md` → `IMPLEMENTATION_LAYOUT.md` → `RUNTIME.md` → `STORAGE.md` → `PERFORMANCE.md` → `contracts/INTERFACES.md`／`contracts/CONFIG.md`／`contracts/METRICS.md` → `SECURITY.md` → `UI.md` → `TASKS.md` → `ACCEPTANCE.md` → `OPERATIONS.md`／`DECISIONS.md`／`VALIDATION.md`。

可直接貼給 Codex／Claude Code：

```text
請在目前 dgxtop 倉庫實作 spec/dgxtop-v2/ 的完整規格。
先讀倉庫現有 AGENTS.md／CLAUDE.md，再讀該目錄 AGENTS.md 與 IMPLEMENTATION_PROMPT.md。
按 TASKS.md 實作、執行 ACCEPTANCE.md 的測試並提供真實結果，不要重新交付計畫或只建立骨架。
不可縮減已核准的六元件、來源仲裁、歷史、可擴充 Viewer 與受保護的程序控制。
所有效能數字都是驗收目標；沒有測量不得宣稱達標。不要修改門檻來掩蓋效能回歸。
不執行使用者工作程序的 kill、不自行 commit／push／部署；安全測試僅使用測試建立的可拋棄 child。
```

## 權威、狀態與界線

`SPEC.md` 與專章 Markdown 規定語意；JSON Schema 規定序列化形狀；`examples/config.default.json` 是唯一預設值；`bench/budgets.json` 是效能門檻的機器可讀來源。HTML 由這些檔案產生，不是另一份可自行改值的規格。遇到不一致必須修正文件與測試，不能擇易實作。

規格範圍 COMPLETE；可開始實作 READY_FOR_IMPLEMENTATION。**Rust 重寫尚未實作，軟體功能／效能／硬體驗收 NOT_RUN**。沒有硬體不阻擋開發，但對應 hardware certification 必須維持 BLOCKED，不能以 synthetic 測試取代。封裝與本次工具測試結果見 VALIDATION.md。

這不是前一份審查 HTML 的修改版；它承接已核准 architecture v1，保留 B1～B6 與 V1～V3，加入 V4 的執行／資源圖。變更見 CHANGELOG.md。沒有在遠端或使用者電腦修改程式、安裝驅動或發送訊號。

## 本包可立即執行的檢查

Python 3.11+。以下僅驗證規格與參考模型，不執行目標 Rust 應用程式：

```bash
python -m pip install -r validation/requirements.txt
python -m unittest discover -s validation -p 'test_*.py' -v
python validation/validate_package.py
```

`tools/evaluate_performance.py` 可檢查實作產生的效能報告；合成報告會被拒絕為正式驗收。`tools/generate_fixture.py` 產生有界、標記為 simulated 的仲裁fixture（完整REF負載由Rust harness實作）。`tools/check_boundaries.py` 檢查實際 Cargo metadata 的跨層依賴。這些是可用工具，不是替代 runtime 的假實作。

本包不包含未經解析的 Cargo.lock。實作 T01 必須完成依賴解析、鎖定與跨 target 編譯，提交 lockfile 是實作產物；不能捏造 lockfile 或把規格封裝有效解讀成 Rust 已可編譯。
