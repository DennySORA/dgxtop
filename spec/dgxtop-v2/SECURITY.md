# 程序控制、IPC 與本機安全

## 1. 信任邊界

本機使用者按鍵不是任意driver權限；Controller僅產生意圖，Manager從真正euid／受管session建立caller context。JSON內的session_id只作路由，不能自行指定admin／UID。讀取drivers不寫sysfs，不以root啟動来「修好」權限。

ProcessActionAdapter是唯一發送**使用者工作程序**signal的實作；獨立於TelemetryPort，也不出現在Collector／Viewer型別中。所有Command允許清單，禁用shell／arbitrary execute／load library path。設定只能選已編譯registry，不載入任意模組。

## 2. 穩定身分與準備

ProcessKey={host_id,boot_id,pid_namespace,pid,start_time_ticks}。只接受同host、同boot、同namespace、同UID、正PID≤i32::MAX；拒絕PID1、自身與所有受管host。euid=0 session拒絕外部程序動作。一般SIGTERM功能預設開，SIGKILL需allow_force=true且單獨確認，絕不因TERM逾時自動升級。

PrepareSignal接收明確選取的ProcessKey與frame_revision，不能把row index轉成當下另一列。先取得pidfd（flags=0，whole process不使用thread/process-group flag），再以`/proc/<pid>/stat`、status、namespace、fdinfo及liveness重核預期身分；讀取失敗、UID變動、對應不明或已退出就關fd拒絕。保留pidfd給整個確認生命週期，不在send時重開同PID。[pidfd_open](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)、[pidfd_send_signal](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html)。

ProcessKey並非可跨PID namespace通用標識，本版僅same namespace操作；容器的host PID對不準就唯讀。pidfd保護的是process instance，不是禁止該process exec；確認資訊變更採保守拒絕，不能宣稱完全原子凍結身份與credentials。

## 3. 狀態機與至多一次

```text
requested → prepared → confirming → intent_pending → intent_committed → sending → signal_sent
                │           │              │                    │                     └→ exit_observed
                └ cancel/expire/reject ──────┘                    └→ not_sent/unknown
```

prepared產生隨機opaque token，TTL10秒、同session、同policy revision、最多8個。Confirm以原子take狀態由prepared轉confirming，同token並發只有一次成功。先重核policy／process／TTL，向AuditStore請求`append_intent(FULL)`；尚未拿到durable receipt不得send。intent期限1000ms，且token原TTL仍是最終send期限。

Audit結果可能在timeout後才到：Manager／ProcessWorker要檢查confirm still active、未取消、session有效、policy未變、send期限未過；否則記not_sent，不得因late receipt突然執行。Cancel在sending之前可取消；已到sending後回in_progress/已送出，不承諾撤回。Config政策收緊先撤銷尚未sending的token。這些transition在單一ProcessWorker序列化，不靠兩個async callback搶bool。

以held pidfd、signal與已消费intent調用一次；errno分類到result。success是signal_sent，不是process已退出；以非阻塞pidfd readiness觀察最多10秒，再報still_running並關fd，不無限持有。對一般非child不呼叫waitpid。任何crash發生在intent committed到durable outcome之間，重啟標unknown、不重試；使用者新操作須重新準備與确认並看到舊unknown警告。

request_id相同payload回既有result，payload不同Conflict。冪等記錄至少保留action retention；新session不接受舊token。signal沒有可逆rollback，資料庫transaction不能使OS副作用exactly-once。資料庫滿／busy／corrupt／版本不合格都使新動作fail closed；效能模式不能更改這些條件。

## 4. IPC 限制

Adapter host仅繼承指定parent pipes，其他FD CLOEXEC；protocol stdin/stdout不混日志，stderr專用有界日志。匿名pipe framing=4-byte BE length+UTF-8 JSON，最大1MiB／256 records／depth32。父程序只能launch自身可信binary的內部參數，無shell、無網路listener；透過建立的child與pipe驗證握手，nonce/generation回應一致，不接受另一host聲稱其他adapter。

因IPC是同權限受管程式，本版不以pipe nonce當作對恶意root的安全保證。protocol未知type、generation過期、超長／nonfinite／非法u64、repeat request mismatch，一律protocol error並隔離該host。source path只允許該Adapter的proc/sys範圍與已探索項目，禁止config把讀取變成任意檔案抽取。

## 5. 檔案、隱私與terminal

state/config目錄0700、檔案0600，openat類安全相對路徑與no-follow write，檢查owner／unexpected symlink；同state dir排他lock。NVML使用已安裝driver／官方library路徑，不用CUDA stub當正常來源、不搜尋任意cwd的動態庫。錯誤與debug去除控制碼，長字串按byte與display width限制，避免ANSI／OSC注入。

不记录env、full cmdline或secret token；可視cmdline清理後暫存有界，audit只記basename與所需身份。Crash dump／recording要明確opt-in、檔案權限限制，Replay文件顯示simulated/replay，不自動執行其中command。

## 6. 必須測的安全案例

PID reuse、prepare前退出、確認後exec／credentials改變、跨UID／root／namespace、same token並發10次、same id不同payload、取消與late audit receipt競態、1000ms audit超時、ENOSPC、force未允許、broken pipe、user config injection。所有真signal測試只能對測試建立且驗證child handle的可拋棄程序；不挑任意現有PID、不觸碰使用者训练工作。

本包僅定義與測試模型；未在此環境執行外部process kill驗收。
