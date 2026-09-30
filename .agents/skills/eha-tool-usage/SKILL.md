---
name: eha-tool-usage
description: 使用 eha-tool 对明确选择的 EHA 执行查询、维护、控制，或启动本机 Shell 与 WebUI 时使用。
---

<!-- Copyright The eha_tool Contributors -->

# eha-tool 使用

按本仓 [README](../../../README.md) 选择 CLI、Shell 或回环 WebUI；精确命令、参数和默认值使用
`eha-tool --help` 与子命令帮助。设备访问复用公开 SDK，不另写 USB 或 CAN 报文。

通过所选模式的枚举入口明确完整 USB 序列号，或 CAN 串口、节点和配置组。
单次 CLI 用 `device ... inspect` 核对 Identity；Shell 的 USB 用 `device select`、`device connect`，
CAN 用 `device connect-can` 核对。`odrive` 是明确序列号的独立 USB 只读入口，不能证明 EHA 内部 CAN 状态。

控制前核对状态、诊断、目标、限值、持续时间和停止条件。`stop` 是显式停止；进程退出、断连、
重连或停止心跳都不发送 Stop。查询和本地提交只表达已取得的通信事实。

保存、恢复、复位或更新切换出现超时、断连或未知结果时，保留操作键并用 `result` 查询原结果，
绝不自动重发。`enter-update` 不传输镜像；刷写、镜像传输和设备恢复使用相应交付流程。
