# dgxtop

[English](../README.md) | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md)

> 面向 NVIDIA DGX 与 GPU Linux 主机的稳定、高效本地终端监控工具：CPU、GPU、内存、
> 磁盘、网络与进程——具备可解释的多来源仲裁、精确历史与受保护的进程控制。

dgxtop v2 依据 [`spec/dgxtop-v2/`](../spec/dgxtop-v2/README.md) 规格完全重写。它不会显示假的 0：
读取不到的值会标示为 *unsupported*（不支持）、*read failed*（读取失败）、*stale*（过期）或
*source conflict*（来源冲突），并附上数据来源。

## 特色

- **隔离且不阻塞的采集。** 每个 live 数据源（procfs、hwmon、thermal、powercap、NVML、network、
  filesystem）都运行在各自常驻的 adapter-host 进程中。驱动调用卡住（例如 NVML 卡死）只影响该 host，
  CPU、内存与 UI 照常更新；超时的 host 以有上限的退避重启或隔离（quarantine），不会无限重生。
- **可解释的来源仲裁。** 两个来源报告同一指标时（例如 CPU package 的 coretemp 与 thermal zone），
  dgxtop 会去除已证实的同源别名、只比较独立来源，差距超过容差时显示“来源冲突”，不取平均也不悄悄挑选一个。
  *Sources* 页面列出每个候选、被选中或排除的原因、age 与来源身份。
- **诚实的单位与质量。** 读取状态、数据种类（测量／派生／估算／模拟）、质量与新鲜度是相互独立的轴。
  未经证实的单位（例如 NVML PCIe 的“KB/s”）不靠猜测换算，也不会从利用率百分比捏造带宽。
- **精确的历史。** 速率来自精确的 `u64` 计数器差值除以真实经过时间；时间窗总量只计入完全落在窗内的区间
  （跨分钟的区间以精确的 *bridge* 保存）。空窗、覆盖率与来源切换都可见。
- **有界的资源。** 单一全局受管内存预算（默认 256 MiB），每条队列都有字节数与条目数上限；固定数量的线程；
  仅在有变化时重绘且最多 4 fps；终端输出不阻塞，终端很慢时也不会卡住输入。
- **受保护的进程控制。** `K` 对选中的进程请求 SIGTERM：仅限同一用户、root 会话一律拒绝、永远不对 PID 1／
  dgxtop 自身／其 host 操作；需要 10 秒内有效的一次性确认；发送信号**之前**先写入持久化审计；信号通过预先持有的
  `pidfd` 发送（没有 PID 复用竞态）。`SIGKILL` 默认禁用。结果会区分“已发送信号”与“已观察到退出”。

## 视图与模块

十个显示模块：`cpu`、`gpu`、`memory`、`disk`、`network`、`processes`、`history`、`sources`、
`settings`、`actions`。标签页为 Overview、GPU、History、Settings。待确认的进程操作会在每个页面以横幅显示，结果（已发送、仍在运行、已结束、未知）以通知显示。

- **Overview：** 由上到下四行。
  - **CPU 与内存并排，内存较宽。** CPU 显示温度、功耗与最高频率（放得下时同一行接着负载与任务数）、利用率条形与其下方的图，以及 usr/sys/iowait/steal 拆分；内存显示使用量、used 条形与其下方的图、swap、buffers、cache 与可用内存。点击 CPU 面板或按 `c` 会加入所有传感器与各核心条形图。统一内存 GPU（DGX Spark）会以不同颜色标出 GPU 进程占用的 RAM，并写出用量。
  - **GPU。** 每张 GPU 一行温度、功耗、频率与风扇，接着是利用率条形与其下方的图，再是显存条形（`VRAM`；统一内存则为 `RAM`，即 GPU 进程占用的 RAM）；终端较矮时 GPU 优先获得空间。
  - **磁盘 I/O 与网络并排。** 各自是一张表格（磁盘会带出所在文件系统的容量），接着是最忙设备（标题所示）的读写或收发速率。
  - **GPU 进程表**占用剩余的行，只列出使用 GPU 的进程；`TYPE` 列按 `nvidia-smi` 的写法标出进程在该 GPU 上是计算（`C`）、图形（`G`）还是两者都有（`C+G`）。选择、排序、筛选、终止都在这里操作。

  图表以快速阅读为目标：方块列，每列一次采样、最新在最右，每一列都使用条形在该数值的颜色，繁忙的时段一眼就是红色。利用率与内存使用固定刻度；磁盘与网络速率按自身峰值缩放，峰值直接标在旁边。
- **GPU：** 一次查看一张 GPU：上方四条实时条形（利用率、内存、温度、功耗），下方是这张 GPU 全部实时与深度信息。数据全部从 NVIDIA 驱动（NVML）与驱动、CUDA toolkit 的文件读取，不联网、不执行任何命令。信息按分类排成从上往下读的「名称　值」区块，需要比较的值上下对齐，并按终端大小排成最少且高度平衡的列数：
  - **State：** P-state、throttle 原因、各原因累计降频时间。
  - **Utilization：** 内存控制器、编码器、解码器。
  - **Clocks：** graphics、SM、memory、video 时钟，当前值与最高值并排。
  - **Memory：** 总量、已用、free、reserved、进程内存、BAR1、总线宽度与理论峰值带宽。
  - **PCIe：** link 与理论带宽的当前值与最高值并排，transmit 在 receive 正上方，replay。
  - **Thermal：** 温度与风扇（多风扇的 GPU 会列出每一个），接着是设备自身的温度阈值，由低到高排列。
  - **Power：** 功耗与驱动的瞬时读数（Ampere 及以后的 GPU，功耗是 NVML 的 1 秒平均）、上限、其他限制压低生效上限时另列设置的上限、默认上限、可设置范围、自驱动加载以来的能耗。
  - **CUDA：** 驱动支持的 CUDA 版本、compute capability（`sm_120`）、已安装的 toolkit、路径与 `nvcc`。
  - **Driver：** 版本、open／proprietary kernel module、NVML、persistence、compute mode。
  - **Device、PCI & firmware：** 架构、品牌、CUDA cores、board part、序列号、UUID、设备节点；PCI 身份、VBIOS、InfoROM、GSP 固件。
  - **Health：** GPU 支持时的 ECC、retired pages 与 NVLink 计数。
  - **Topology：** 离 GPU 最近的 NUMA node 与 CPU，以及到每张其他 GPU 的 PCIe 路径（`nvidia-smi topo -m` 的 `PIX` 到 `SYS`）和能否直接读写对方显存（P2P）。不支持 P2P 时，GPU 之间的复制会经过系统内存。

  GPU 不提供的数据不占行；终端太矮、放不下全部区块时，上方会列出未显示的区块。各 GPU 的进程列在 overview 的进程表（按 `g` 只看一张 GPU）。
- **History：** overview 的各项资源在更长时间范围内的变化（5 分钟到 24 小时，`w`/`W` 切换），排成监控仪表板：面板两两并排——CPU 与内存、GPU 利用率与内存、GPU 温度与功耗（每张 GPU 一条），以及最忙磁盘的读写与网卡的收发（每组共用一个刻度，以两者最高的一列为准）。每一条都有刻度、时间窗内的方块列（平均值按等级上色，没有记录的时段显示虚线底线），以及「当前／平均／峰值」图例（峰值是整个时间窗的最高值）；每列面板下方有时间轴。历史会保存到磁盘（`history.sqlite3`，默认开启）：之前几次运行与重启前的数据会读回，画在同一条时间轴上；窗口栏会标出时间窗已记录多久（只在 dgxtop 运行时记录）。时间轴下方是每张 GPU 最近 1、6、12、24 小时的统计：利用率、内存、温度、功耗的平均与峰值及能耗，只列出确实有新增数据的时间窗。设计分析见 [docs/process/v2-rewrite/history-redesign.md](process/v2-rewrite/history-redesign.md)。

布局随终端大小调整：

- 80×24 以上显示完整布局；
- 小于 80×24 显示精简摘要；
- 小于 40×10 显示尺寸提示（仍可按 q 退出）。

## 安装

```bash
curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
```

安装脚本会把 x86_64 或 aarch64 的 GNU 版本下载到目标文件系统上的临时目录，校验 `SHA256SUMS` 与压缩包结构，
运行 `dgxtop --version --verbose` 确认可在本机加载，再以原子方式替换可执行文件，并把旧版保留为 `dgxtop.previous`。
可用 `./install.sh --rollback` 回滚（不会改动配置与状态；已发送的信号无法撤回）。不会自动改用 musl 版本。

从源码构建（`rust-toolchain.toml` 固定 Rust 1.92.0）：

```bash
git clone https://github.com/DennySORA/dgxtop.git
cd dgxtop
cargo build --release --locked
./target/release/dgxtop
```

## 使用方法

```bash
dgxtop                       # 实时监控
dgxtop -i 0.5                # 本次运行采样 500 ms（不会修改已保存的配置）
dgxtop --no-gpu              # 不启动 NVML host
dgxtop --read-only           # 禁用所有进程操作
dgxtop --replay fixture.json # 仅使用模拟数据、只读、独立的临时状态
dgxtop --check-config        # 校验并列出配置值与实际生效值后退出
dgxtop --diagnostics-json    # 以 JSON 输出版本、SQLite、能力与健康摘要
dgxtop --import-legacy ~/.config/dgxtop/config.json --dry-run   # 显示 v1 → v2 映射
dgxtop --version --verbose   # 版本、commit、工具链、target、SQLite runtime
```

命令行参数只有在明确指定时才会覆盖已保存的配置，并且只对本次运行有效。

### 按键

| 按键 | 动作 |
|---|---|
| `q`、`Ctrl-C` | 退出（编辑过滤字符串时 `q` 是文本；`Ctrl-C` 总是可以退出） |
| `1`–`4`、`Tab`、`Shift-Tab` | Overview、GPU、History、Settings |
| `w` / `W` | 下一个／上一个 History 时间范围（5m、1h、6h、12h、24h） |
| `c` | 在 Overview 显示／隐藏 CPU 传感器与各核心条形图（或点击 CPU 面板） |
| `j`/`k`、方向键、`PgUp`/`PgDn`、`Home`/`End` | 移动选择（按进程身份，而非行索引） |
| `h`/`l` | GPU 视图切换上一张／下一张 GPU |
| `g` | 切换进程表的 GPU 过滤（全部 → 每张 GPU） |
| `/` | 过滤进程（Enter 应用、Esc 清除） |
| `s` / `r` | 排序列／反向排序 |
| `+` / `-` | 调整采样间隔（由 store 确认写入后才生效） |
| `K` | 对选中的进程请求 SIGTERM（需要确认） |
| `y`/`Enter`、`n`/`Esc` | 确认／取消待处理的进程操作 |
| `F2`、`?`/`F1` | Settings 视图、帮助 |

## 配置与文件

| 用途 | 位置 |
|---|---|
| 配置（schema 2、带版本、原子写入） | `$XDG_CONFIG_HOME/dgxtop/v2/config.json`（`~/.config/dgxtop/v2/config.json`） |
| 进程操作审计（SQLite，`synchronous=FULL`） | `$XDG_STATE_HOME/dgxtop/v2/audit.sqlite3`（`~/.local/state/...`） |
| 历史归档（默认开启，保留 7 天、最多 1 GiB） | `$XDG_STATE_HOME/dgxtop/v2/history.sqlite3` |
| 有上限的日志、instance lock | `$XDG_STATE_HOME/dgxtop/v2/` |

目录以 `0700`、文件以 `0600` 创建。完整命令行、环境变量与 token 永远不会落盘。完整的配置契约见
[`spec/dgxtop-v2/contracts/CONFIG.md`](../spec/dgxtop-v2/contracts/CONFIG.md)，默认值见
[`spec/dgxtop-v2/examples/config.default.json`](../spec/dgxtop-v2/examples/config.default.json)。

从 v1 升级：旧的 `~/.config/dgxtop/config.json` 永远不会被修改。先用 `--import-legacy <path> --dry-run`
检查映射，再用 `--apply` 写出 v2 配置文件。

## 架构

```text
数据：驱动／操作系统 → adapter host（Adapter）→ Collector → Manager → Store
      Store → Manager 投影 → Viewer
控制：输入 → Controller → Manager → 配置／调度／pidfd 操作 adapter → 审计 → Store → Viewer
```

| Crate | 角色 |
|---|---|
| `dgxtop-core` | 类型化身份、单位、质量轴、命令、事件、配置、字节预算、port |
| `dgxtop-runtime` | adapter-host 监管与 IPC framing、有界 worker、不阻塞的终端输出 |
| `dgxtop-adapters` | 只读 adapter（procfs、hwmon、thermal、powercap、NVML、network、filesystem、replay）与独立的 pidfd 操作 adapter |
| `dgxtop-collectors` | 归一化、同源去重、仲裁、计数器速率、进程／GPU 关联 |
| `dgxtop-store` | 热快照、有界精确历史、配置 repository、SQLite 审计与归档 |
| `dgxtop-manager` | 调度、catalog、策略、数据提交、历史、配置、进程操作、投影、会话 |
| `dgxtop-ui` | 显示模块、布局、按键映射、controller、终端运行循环 |
| `dgxtop` | composition root 与 CLI |

只有 composition root 会依赖 `dgxtop-core` 以外的 workspace crate；此边界由
[`spec/dgxtop-v2/tools/check_boundaries.py`](../spec/dgxtop-v2/tools/check_boundaries.py) 检查。

## 开发

```bash
just ci        # fmt、check、clippy -D warnings、测试、依赖边界（全部 --locked）
just e2e       # 需要时才运行的端到端测试：PTY、故障注入、真实的 host 子进程
just hardware  # 真实 procfs/sysfs/NVML 的硬件测试（pidfd 只针对可丢弃的子进程）
```

## 验收状态

每个验收用例（AT01–AT40）的命令、退出码、环境与证据哈希见
[`docs/validation/VALIDATION.md`](validation/VALIDATION.md)。规格中的性能数字是验收目标；只有该报告列出实测证据的平台才称为已验证。

## 许可证

Apache-2.0，见 [LICENSE](../LICENSE)。
