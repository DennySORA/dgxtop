# 有序實作工作包

這是執行順序，不是另一輪架構規劃。每包完成程式、成功與失敗測試、實際證據才向後；可並行處理無依賴的fixture，但不能以空介面佔位宣布功能完成。T13硬體不可得時保留BLOCKED，仍完成可執行的T14/T15工作，禁止以阻擋為由不交付其他實作。

<a id="t01"></a>
## T01｜鎖定基準、既有規則與完整工具鏈

前置：無。產出：`Cargo.toml、Cargo.lock、rust-toolchain.toml、既有規則合併`。

讀取當前repo全樹與CLAUDE/AGENTS，將基準SHA差異列出；重現舊F1～F10案例；編譯並固定相容版本；保留release與CLI功能，不直接搬用錯誤零值。

涵蓋：R19 / AT19。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t02"></a>
## T02｜建立core契約、識別與配額基礎

前置：T01。產出：`crates/core、typed DTO、契約測試、allocation ledger`。

實作精確u64、來源/ProcessKey、quality/time、錯誤enum與ports；BytePermit先於buffer配置，所有所有權移動保留計帳。

涵蓋：R02 / AT02, R21 / AT21。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t03"></a>
## T03｜建立runtime、supervisor與IPC

前置：T02。產出：`crates/runtime、adapter-host模式、scheduler/Broker`。

常駐受管子程序、單在途、讀組去重、絕對deadline、epoch/generation、回收/quarantine；控制公平性與背壓先用Replay測。

涵蓋：R14 / AT14, R22 / AT22, R24 / AT24, R33 / AT33。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t04"></a>
## T04｜完成全部唯讀Adapter

前置：T03。產出：`crates/adapters、7live＋Replay、capability fixtures`。

Procfs、Hwmon、Thermal、Powercap、Nvml、Network、Filesystem全部有真實讀取路徑；unsupported局部降級、catalog去重；fixture不冒充live。

涵蓋：R01 / AT01, R15 / AT15, R34 / AT34。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t05"></a>
## T05｜完成Collector與來源仲裁

前置：T04。產出：`crates/collectors、CPU/GPU/Memory/Disk/Network/Process`。

正規化、同scope/origin、conflict、遲滯、provenance、counter/rate與唯一ProcessKey；state_delta僅commit後前進。

涵蓋：R03 / AT03, R04 / AT04, R09 / AT09。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t06"></a>
## T06｜完成HotStore與精確歷史

前置：T05。產出：`crates/store/hot、chunked rings、history query、bridges`。

有限候選/raw/聚合、真實時間與coverage、source segments、counter跨桶bridge、query deadline/pointcap；滿額驅逐與quota可證明。

涵蓋：R07 / AT07, R27 / AT27。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t07"></a>
## T07｜完成設定與持久化Store

前置：T06。產出：`config/audit/archive repositories、DDL、migration`。

SQLite版gate、兩DB、FULL稽核、NORMAL archive、WAL/容量與取消；JSON stage/CAS/fsync與commit_uncertain；legacy dry-run。

涵蓋：R06 / AT06, R08 / AT08, R31 / AT31, R36 / AT36。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t08"></a>
## T08｜完成Manager用例與會話

前置：T07。產出：`crates/manager各service、commands/results/subscription`。

組合生命週期/排程/catalog/policy/ingestion/history/config/process/presentation/session；有限回覆槽、分權ports、不阻塞控制迴圈。

涵蓋：R05 / AT05, R23 / AT23。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t09"></a>
## T09｜完成安全程序Action Adapter

前置：T08。產出：`adapters/action、manager/process、disposable tests`。

held pidfd/身份/UID/namespace、一次token、稽核後signal、late/cancel防護、至多一次／unknown、退出觀察；不得操作真實工作。

涵蓋：R10 / AT10, R32 / AT32。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t10"></a>
## T10｜完成Viewer、Controller與全部頁面

前置：T09。產出：`crates/ui、10模組、keymap、Ratatui PTY harness`。

dirty-only與bounded projection，三種寬度布局，完整sources/history/settings/actions，nonblocking output、穩定ProcessKey/HitMap與安全確認。

涵蓋：R11 / AT11, R12 / AT12, R13 / AT13, R16 / AT16, R25 / AT25, R26 / AT26。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t11"></a>
## T11｜整合可觀測性與故障注入

前置：T10。產出：`diagnostics、metrics registry、replay scenarios`。

控制loop禁止I/O，queue/ledger histograms、privacy、低基數、整host卡住、滿disk/WAL、source churn、UI慢讀、shutdown。

涵蓋：R17 / AT17, R30 / AT30, R38 / AT38, R39 / AT39。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t12"></a>
## T12｜效能剖析與REF門檻

前置：T11。產出：`release bench harness、raw metrics、performance reports`。

三輪固定REF，包含parent+host全部成本與coverage；以profiler定位去重、配置、排序/渲染熱點；不得削功能或放寬門檻。

涵蓋：R28 / AT28, R35 / AT35。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t13"></a>
## T13｜72h soak與硬體矩陣

前置：T12。產出：`soak資源曲線、GNU x86_64/aarch64 GPU報告`。

固定來源與churn各72h，真實DGX Spark與獨立GPU、同PID多GPU、NVML缺能力；不可取得項明示BLOCKED。

涵蓋：R29 / AT29。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t14"></a>
## T14｜完整包裝、CI與遷移回滾

前置：T13。產出：`workflows、install.sh、version/SBOM/checksums、docs`。

所有unit/contract/PTY/failure通過才pack，GNU基線與lockfile，安裝驗checksum再同fs交換，舊binary/config可回滾，不自動發布。

涵蓋：R18 / AT18, R20 / AT20, R37 / AT37。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。


<a id="t15"></a>
## T15｜逐需求封版驗收

前置：T14。產出：`實作VALIDATION、40需求證據索引、交付報告`。

逐AT登錄命令、退出碼、環境、證據hash及NOT_RUN；核對fullscope/所有文件；留BLOCKED不冒稱release-ready。

涵蓋：R40 / AT40。

完成閘門：上述產物不是空白模板，對應 ACCEPTANCE 條件均有命令、退出碼、log。失敗時保留 failing case 並修復，不能單獨改測試期待以配合錯誤輸出。
