# 完整契約案例

本資料夾是**合成fixture**，不是真實硬體讀值。fixture source path、host、PID與SHA示例不是外部操作授權。`config.default.json`是實際設計預設值，不是假量測。

observation.cpu-package / conflicting-source展示原始來源；canonical.conflict的null代表拒絕不一致來源。command.prepare-signal只描述Prepare，不發signal；command-result.interval是範例Ack。ipc-request.read、adapter-manifest.nvml、event.gap、history-query對應同名schema。所有範例以validation腳本可檢查；缺測量值不塞0。

正式live裝置API應由T04記錄driver/version/nativeunit與fixture provenance後另加入repo；不要把這些synthetic sources編譯成live fallback。
