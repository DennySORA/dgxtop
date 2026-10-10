# 既有規則的保留與相容性

來源：[b4b5a8d87d3c148cedacf91daf6f926719694a18 的 CLAUDE.md](https://github.com/DennySORA/dgxtop/blob/b4b5a8d87d3c148cedacf91daf6f926719694a18/CLAUDE.md)，2026-10-04 已透過 GitHub 連接工具完整讀取。blob SHA：`58557cdc7bb5a9605826e87016b3fefe5399bbcd`。

以下是內容摘要，不冒充 byte-for-byte 原始副本。本次重新透過連接工具讀取該原檔；此處只提供摘要，並非原文副本。既有檔案不得由本包自動覆寫。

保留原有規則：SOLID、簡潔單一責任、Domain 不依賴 Infrastructure、handler 不放業務規則、依賴可注入、錯誤可追蹤、不提交秘密；新行為要測成功／失敗／邊界，修 bug 先寫失敗案例。

原有驗證命令：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace --all-targets --all-features
cargo test --workspace --all-features
```

提交規則保留英文 Conventional Commits、scope、imperative summary、非簡單修改需 What／Why／Tests、breaking change 註記。原檔要求 author name DennySORA 與真實 email；本包不設定 git identity、不推測 email、不自行 commit／push。實際授權提交時先檢查既有設定。

新增本輪的六元件邊界、可解釋缺值、多來源仲裁、pidfd、資料來源／測試真實性與不自動部署規則。若現有倉庫已加入更細的 AGENTS.md，先讀取並保留，再做最小合併。

本包使用薄 `CLAUDE.md` 匯入 `AGENTS.md`，符合官方 @path 匯入語法。不要假設所有工具會自動讀取其他工具的檔名，也不要建立互相匯入的迴圈。實際 coding session 應用 `/context` 確認；本次沒有啟動 Codex／Claude Code CLI。[官方文件](https://code.claude.com/docs/en/memory)。
