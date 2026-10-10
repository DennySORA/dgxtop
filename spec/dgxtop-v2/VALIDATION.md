# 本次交付驗證

文件版：2.0.0；日期：2026-10-04。**規格完成與軟體驗收是兩件事。** 本包完成已核准範圍的實作規格；不含已實作的Rust重寫，也沒有真實GPU／pidfd／效能認證。

## 四種狀態

| 面向 | 狀態 | 意義 |
|---|---|---|
| 規格範圍 | COMPLETE | 六元件、全部功能、40需求／15工作包／40驗收ID，包含失敗與回滾 |
| 封裝 | VALIDATED | 必備內容、契約、路徑、MANIFEST逐檔SHA256、ZIP CRC均由附屬工具檢查 |
| 開始實作 | READY_FOR_IMPLEMENTATION | 可按IMPLEMENTATION_PROMPT開始；不必依賴這段對話或舊ZIP |
| 軟體／效能／硬體接受 | NOT_RUN | Rust未實作、依賴組合未編譯、GPU/pidfd未驗收、30min/72h未執行；硬體認證BLOCKED |

## 實際執行

| 類型 | 執行與結果 | 證據／限制 |
|---|---|---|
| 合成參考模型／契約／工具單元 | **83 tests通過**；`python -m unittest discover -s validation -p 'test_*.py' -v`，exit 0 | `validation/model-tests.log`；來源仲裁/counter/gauge/bytequota/deadline/token/lateaudit/配額/DDL/效能評估器/依賴檢查器。不是Rust測試或硬體量測 |
| JSON Schema與範例 | **13份Schema、13份範例通過** | `validation/validate_package.py`；型別/未知欄位/缺值語意/u64/有限數/預設配額cross-field |
| 需求與內容 | **40R、15T、40AT映射通過** | SPEC/TASKS/ACCEPTANCE/requirements.json；必備檔案非空，Markdown相對連結有效，薄CLAUDE匯入無循環 |
| SQLite DDL | audit/history建表、FK/unique/固定20位時間/user_version通過 | 本地sqlite3 3.46.1；**只是DDL語法／約束測試，不是重寫runtime的SQLite≥3.51.3 gate通過** |
| HTML靜態 | zh-Hant-TW、唯一ID、內部錨點、4個SVG、無外部腳本/樣式/圖片依賴通過 | 可讀正文與表格內嵌，不靠JS產生主要內容 |
| HTML瀏覽器 | **20組檢查通過，pageerror 0** | `validation/browser-results.json`；標記、篩選、快捷鍵、文字輸入保護、匯出、fallback、4圖放大、手機目錄、文件展開/直達、無橫向溢出 |
| 視覺檢查 | 桌面1440×1000、手機390×844已實際查看 | PNG用於交付前檢查，不當硬體證據，不納入ZIP；SVG原檔在assets/ |
| 封裝 | MANIFEST全檔size/SHA256＋ZIP CRC／逐成員hash一致 | `validation/seal_package.py`封裝後即重開檢查；MANIFEST不對自己產生循環hash |

Python：3.13.5。瀏覽器：Chromium 144.0.7559.96 built on Debian GNU/Linux 13 (trixie)。Python工具依賴版本已記錄並固定於validation/requirements*.txt。

## 已遇到並處理的驗證問題

首次瀏覽器腳本的`h1`選取器命中多個完整文件標題，已改為精確第一個主標題並重跑。測試也檢查了SVG放大後CSS仍生效、marker/aria ID不重複，以及篩選後快捷鍵不越界；封裝採最後通過版本，不沿用v1的測試紀錄。

本環境Chromium對`file://`回ERR_BLOCKED_BY_ADMINISTRATOR；實際互動測試改為`page.set_content`載入相同HTML。**原生file開檔與localStorage跨重新載入尚未驗證**。localStorage拒絕時記憶體回饋分支已測；clipboard拒絕時文字全選fallback已測，尚未驗證貼到外部OS應用程式。這些限制寫入browser-results，不把記憶體載入說成真實開檔PASS。

## 尚未執行的軟體接受條件

全部AT01～AT40是實作後的軟體驗收規範，現在不是PASS。未跑Rust fmt/check/clippy/test、cross-target build、NVML真實driver、PID reuse與同UID信號、release installer、REF三輪各30min與固定/churn各72h。不能將83個Python tests／20組HTML tests當成低CPU或安全kill已經達標。未取得硬體先完成可執行程式與可測部分，其餘標BLOCKED，不造報告。

## 重跑方式

```bash
python -m pip install -r validation/requirements.txt
python -m unittest discover -s validation -p 'test_*.py' -v
python validation/validate_package.py
# 修改權威文件後要重新產生HTML，舊MANIFEST會正確地失效：
python validation/build_html.py
python -m pip install -r validation/requirements-browser.txt
# 需要本機Chromium；可用CHROMIUM指定路徑，DGX_SPEC_SCREENSHOTS指定暫存截圖位置。
python validation/browser_check.py
python validation/validate_package.py --before-seal
python validation/seal_package.py ../dgxtop-spec-v2.zip
```

重跑browser腳本會更新browser-results；請重新seal才會得到與新結果一致的MANIFEST。可拋棄validation輸出不影響目標repo；腳本沒有發driver命令或kill使用者程序。文件本身保持相同門檻，實作驗收不得偷偷降低標準。
