# 實作目錄與依賴圖

本文件定義重寫要產出的Rust repo布局，不表示這些程式已在規格ZIP實作。不要在ZIP內建空crate冒充完成。

```text
Cargo.toml / Cargo.lock / rust-toolchain.toml
crates/
  core/src/{identity,metric,quality,clock,ports,errors,commands,budget}.rs
  runtime/src/{scheduler,broker,supervisor,ipc,queues}.rs
  adapters/src/{procfs,hwmon,thermal,powercap,nvml,network,filesystem,replay,process_action}.rs
  collectors/src/{cpu,gpu,memory,disk,network,process,arbitration}.rs
  store/src/{hot,candidates,history,query,config,audit,archive}.rs
  manager/src/{supervisor,scheduler,catalog,policy,ingestion,history,config,process,presentation,session}.rs
  ui/src/{viewer,controller,layout,keymap,terminal,modules}.rs
  dgxtop/src/{main,cli,composition}.rs
tests/{contracts,replay,pty,action_disposable,fault_injection,hardware}/
benches/{ref,history,arbitration,projection}/
```

Cargo package names固定 `dgxtop-core`, `dgxtop-runtime`, `dgxtop-adapters`, `dgxtop-collectors`, `dgxtop-store`, `dgxtop-manager`, `dgxtop-ui`, `dgxtop`。`runtime`是執行機制支援crate，不是第七個業務元件；Manager仍決定排程policy與生命週期。這比前版7-crate建議多獨立runtimecrate，以便邊界與資源機制測試，外部六元件不變。

允許workspace直接依賴：每個非core實作crate只依core；composition `dgxtop`可依全部，其餘service只透過ports裝配，不直接引用其他concrete實作。runtime透過core callback/trait分派，不 importmanager。Integrationtests放composition/test harness，可以組裝多concrete不打破production dependency。測試dev-dependencies可依受控test support，但不得引入cycle或把不當依賴放dev繞過功能crate檢查。

core只用std及明確DTO/序列化小依賴；不得使用libc/nix/NVML/SQLite/Ratatui/crossterm/tokio。ui可用Ratatui/crossterm/unicode-width，禁止nix/libc/NVML/SQLite/drivers。非阻塞terminal transport的fcntl操作實作在runtime的受限TerminalTransport port，由composition注入ui；UI不是為了fcntl就自行取得通用libc能力。

因此Viewer/render即使使用Ratatui也不得取得Store。`tools/check_boundaries.py`檢查cargo metadata直接邊，仍須clippy/人工檢查非法std::fs/process逃逸；程式不能將FFI以字串/feature掩藏。全部runtime buffer以core BytePermit支持，在持有狀態及IPC載荷移交保持同一配額。
