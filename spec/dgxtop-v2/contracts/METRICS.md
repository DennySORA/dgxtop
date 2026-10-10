# Metric 與領域契約

以下為本版必備清單。API不支援時提供capability/缺值狀態仍算完整功能；用常數0冒稱讀到不算。裝置細節、UUID、名字、driver、compute/persistence mode、throttle reasons等是有限metadata，不為每個字串建time-series。所有metric都有完整MetricKey及來源generation，單位使用canonical schema enum。

| metric_id／族群 | scope／單位 | Adapter與Collector規則 |
|---|---|---|
| cpu.utilization.total / user / system / iowait / steal | host或core；percent | Procfs /proc/stat計數差；guest已含user/nice不雙加；hotplug以core_id不是index；iowait回退標reset非巨大負值 |
| cpu.frequency.current / maximum | core；MHz | sysfs cpufreq kHz/1000；scaling_cur_freq語意依driver不保證瞬時hardwareclock；/proc/cpuinfo fallback標來源不冒稱max |
| cpu.temperature.package / core | package或core；degC | Hwmon/thermal只有profile證明scope才合併；nativeunit/label與driver；0合法、fault不同alarm |
| soc.temperature | SoC；degC | 不把thermal_zone0或soc直接標CPUpackage；unknownscope放sources診斷 |
| cpu.power.package / platform | package或platform；watts | Powercap energy_uj delta/1e6/dt；wrap須有max_energy_range；hwmon平均值與瞬時input語意分開 |
| cpu.load.1m / 5m / 15m | host；count | /proc/loadavg gauge，不乘100、不當CPU百分比 |
| cpu.tasks.running / total | host；count | loadavg欄位，不是自行掃全部/proc以取得 |
| memory.total / available / used / buffers / cached / swap_total / swap_used | system；bytes | meminfo kB×1024；used=total-available；不可把available和free混稱；missing保留 |
| gpu.utilization.compute / memory_busy | GPU UUID；percent | NVML utilization語意是時間busy比例；memory_busy不是已量測GB/s |
| gpu.temperature / power.draw / power.limit | GPU；degC / watts | NVML原生溫度與mW/1000；unknown=null；稽核temperaturethreshold非通用固定數字 |
| gpu.clock.graphics / memory / sm / video / maximum | GPU；MHz | NVML能力逐欄探測，靜態上限cache、目前clock fast；零值不由Err產生 |
| gpu.fan.speed | fan/GPU；percent | 百分比而非RPM；沒有fan/未支援不顯0RPM |
| gpu.memory.framebuffer.used / free / total / reserved | GPU memorydomain；bytes | NVML已支援欄位；UMA report不支援不能偷改為systemRAM |
| gpu.memory.process_allocated | GPU；bytes | 可見程序NVML用量按PID去重，unknown不計假0；與framebuffer/system 分開，不推算real free |
| gpu.pcie.tx / rx | GPU link；bytes_per_second | backend回傳原nativeunit，由adapter profile驗證×1000或×1024，未證明單位則unknown；不把KB與KiB任猜 |
| gpu.energy.total | GPU；joules | NVML累計mJ/1000；backend reset建新segment；energy與單次draw不同 |
| gpu.ecc.corrected / uncorrected、retired_pages.single / double | GPU；count | aggregate/volatile維度保留；5s slow，不支援不冒稱無錯 |
| gpu.encoder / decoder.utilization | GPU；percent | NVML能力；不能只憑compute util猜encoding |
| gpu.nvlink.active | GPU link；metadata | linkpresence/topology可讀；沒有counter僅顯presence，throughput=null非0；最多profile允許link數 |
| gpu.memory.bandwidth.estimated | memorydomain；bytes_per_second | 僅有明確模型/基準才estimated，畫面標估算；不得將busy%×峰值稱actual；無模型就不提供值 |
| disk.read / write.bytes_per_second | blockdevice major:minor+generation；bytes_per_second | diskstats sectors delta×512/dt（Linuxdiskstats sector口徑）；讀物理盤/partition分scope，不總和重複 |
| disk.read / write.iops | blockdevice；count_per_second | completed operations delta/dt |
| disk.await | blockdevice；milliseconds | (read_ms_delta+write_ms_delta)/(completed_reads+writes)，0completed則unavailable，不除0 |
| filesystem.total / used / free / available / use_percent | mount_id+device+mountnamespace；bytes / percent | mountinfo+statvfs；bfree與bavail分开；不把整disk容量當mounted filesystem used |
| network.rx / tx.bytes_per_second | netnamespace+ifindex+generation；bytes_per_second | kernel原始bytecounter先u64差再除dt；重新建立interface即reset；只以名稱會錯 |
| network.rx / tx.packets / errors / drops | interface；count | cumulative counters；衍生rate另metric，不把totals當每秒 |
| process.cpu.percent / rss | ProcessKey；percent / bytes | 每key每cycle一次，CPU按一core可>100%；UID numeric保底，不在hotpath反覆NSS |
| process.gpu.utilization / memory_allocated | ProcessKey＋GPU UUID；percent / bytes | GPU关联資料與host資料分表；compute+graphics同PID同GPU合併，history不跨starttime |

## 採樣與能力驗證

driver/API版本、nativeunit、scope、origin、是否真實sample timestamp及最短更新週期都由SourceDescriptor保留。profile不知道時只承認可讀到原生量，不生造 verified CPU package關係。候選選取需可解釋priorities；confidence不用沒有校準的百分比。

canonical的基礎unit採 `degC, watts, bytes, bytes_per_second, percent, MHz, count, count_per_second, milliseconds, joules`；schema必涵蓋這些值。若backend只回KB/s而KB定義未核實，保留native觀測／reason，fixture不足以認證該平台。CPU/GPU功耗不是整機插座功耗。

## 程序與來源基數

程序表current上限256、GPUlinks1024預設；超額保留按穩定排序/選中priority的有限集合並顯truncated。ProcessKey可見期間只保留有界raw300s，退出後TTL清理；不建立每PID24h聚合。來源raw候選60s與canonical300s/24h分區是不同用途，RUNTIME總quota優先。

## 平台確認

Linux hwmon規格允許optional欄位、不同temperature units及fault/alarm語意，不能單看檔名。[kernel hwmon](https://docs.kernel.org/hwmon/sysfs-interface.html)。Procfs、NVML等精確欄位須於T04對照實作版本API與真實fixtures；本版無新的硬體採樣證據，官方NVML頁本次讀取失敗已列SOURCES，不能以此聲稱硬體全面相容。

欄位查證：[Linux /proc](https://docs.kernel.org/filesystems/proc.html)、[diskstats](https://docs.kernel.org/admin-guide/iostats.html)、[Powercap](https://docs.kernel.org/power/powercap/powercap.html)，均於2026-10-04讀取。VmRSS是低成本採樣近似，外部benchmark可加smaps_rollup抽樣核對，不能宣稱1sVmRSS取樣捕捉了所有瞬時尖峰；384MiB gate依已定義的採樣峰值，硬payload上限另以ledger證明。
