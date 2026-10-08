---
name: eha-tool-usage
description: 使用 eha-tool 对明确选择的 EHA 执行查询、维护、持续控制，或操作其持久 Shell 和本机 Web UI 时使用。
---

<!-- Copyright The eha-tool Contributors -->

# eha-tool 使用

按本仓 [AGENTS.md](../../../AGENTS.md) 的项目边界工作。此 skill 仍在开发中，遇到问题按其中的技能维护
规则核对实现，在用户已授权范围内修正并验证原失败步骤。

先读本仓 [README 的使用方式](../../../README.md#使用方式)。README 是完整的用户操作说明，包含每个当前
CLI、Shell、批处理和 Web UI 的参数、示例和结果边界；本 skill 保留命令速查和决策边界，具体操作示例按
README 执行。构建、安装和软件检查使用 [CONTRIBUTING.md](../../../CONTRIBUTING.md)。

## 选择入口

| 目标 | 使用方式 | 关键参数与结果 |
|---|---|---|
| 单次设备动作 | `devices`、`can-devices`、`device` | `device` 每次都以 USB 完整序列号，或 CAN 串口、节点、实际配置组建立新会话并核对 Identity；等待上限使用 `--timeout-secs`，机器输出使用 `--json`。`velocity-test` 不支持 `--json`。 |
| 本地配置检查 | `config-check FILE` | 不访问设备。Shell 的 `config check FILE` 同样不要求连接；`config validate FILE` 是已连接会话内的本地 JSON 校验。 |
| 持久设备会话 | `shell` | USB 依次执行 `device list`、`device select`、`device connect`；CAN 用 `device connect-can PORT NODE PROFILE`。Shell 的 `telemetry` 只读缓存，`wait` 只在本地等待。 |
| 串行自动化 | `shell --script FILE` 或 `shell --batch` | 共用一个 Shell 会话并在首个失败处停止。批处理输入忽略空行和以 `#` 开头的整行；`help` 在批处理中会以非零状态结束，应先在普通 CLI 或交互 Shell 查看帮助。 |
| 本机浏览器操作 | `webui [--port PORT] [--odrive-python PATH]` | 只监听 `127.0.0.1`；当前操作通路决定请求目的地。页面刷新、关闭、切换通路、断连和停止心跳不发送 Stop。 |
| ODrive USB 只读 | `odrive [--python PATH] [--timeout-secs 1..60] [--json] devices` 或 `odrive ... status --serial HEX` | 不清错、不保存、不控制，独立于 EHA 会话，不能证明内部 CAN 状态。 |

## 命令速查

下面的 `SERIAL`、`PORT`、`NODE`、`PROFILE`、`FILE`、`KEY` 和数值参数均须替换为本次实际值。
单次 CLI 的设备命令前缀二选一：`eha-tool device --usb SERIAL` 或
`eha-tool device --can-port PORT --node NODE --profile PROFILE`；在动作前放置可选的 `--json` 和
`--timeout-secs SECONDS`（默认 5 秒）。以下各 CLI 动作接在该前缀后，Shell 命令直接输入已连接的 Shell。

| 用途 | CLI 动作 | Shell 命令 |
|---|---|---|
| 核对身份 | `inspect [--json]` | 连接时核对；`device snapshot` 查看本地会话快照 |
| 读取快照 | `status`、`measurements`、`diagnostics` | 同名命令 |
| 读取遥测 | `telemetry SECONDS` | `telemetry`，读取最新缓存 |
| 心跳 | `heartbeat-test SECONDS` | `heartbeat-start`、`heartbeat-once`、`heartbeat-stop` |
| 持续目标 | `position MM`、`velocity MM_S`、`force N`、`impedance EQUILIBRIUM_MM STIFFNESS_N_PER_MM DAMPING_NS_PER_MM` | 同名命令与参数 |
| 受限速度试验 | `velocity-test MM_S SECONDS`，时长 1 至 10 秒 | 无对应命令，使用单次 CLI |
| 普通停止 | `stop` | `stop` |
| 配置读取 | `config-read --view VIEW [--output FILE]` | `config-read --view VIEW`，不支持文件导出选项 |
| 保存完整配置 | `save FILE` | `config-save FILE` |
| 恢复出厂 | `restore-factory` | `restore-factory` |
| 查询、释放原维护结果 | `result --operation-key KEY`、`release --operation-key KEY` | 同名命令与参数 |
| 应用维护 | `reset-application`、`enter-update` | 同名命令 |

Shell 的 USB 入口为 `device list` → `device select SERIAL` → `device connect`；CAN 入口为
`device list-can` → `device connect-can PORT NODE PROFILE`。`device show` 显示本地选择，
`device clear` 清除本地选择但不影响已有会话；`device disconnect` 关闭会话，`device reconnect` 按已有对象重新连接。
`config-check FILE`、`config check FILE` 不要求连接；`config validate FILE` 要求已连接。
`version` 读取本地版本，`wait SECONDS` 本地等待，`quit`／`exit` 退出。Shell 不展开环境变量；
交互示例里的 `#` 注释只供阅读。自动化需要变量替换时在外层系统 shell 完成，再交给批处理入口。

独立入口还包括 `eha-tool --version`、`eha-tool --help`、`eha-tool config-check FILE`、
`eha-tool devices [--json]`、`eha-tool can-devices [--json]`。ODrive 命令在单次 CLI 前加 `eha-tool`，
在 Shell 中直接使用上表的 `odrive` 入口；未指定 `--python` 时依次使用 `EHA_ODRIVE_PYTHON`、`python3`。
Web UI 默认端口为 8080，启动后打开对应的 `http://127.0.0.1:PORT/`，按 README 的页面流程操作。

## 当前动作与约束

1. 先通过枚举入口明确对象；候选列表不代表连接或 Identity 已核对。连接后按需要执行 `inspect`、
   `status`、`measurements`、`diagnostics`、`telemetry SECONDS` 或 `config-read --view VIEW`。`config-read`
   的视图为 `factory`、`user`、`startup`、`communication`；只有完整 UTF-8 的非 communication 视图可导出。
2. `position MM`、`velocity MM_S`、`force N` 和 `impedance EQUILIBRIUM_MM STIFFNESS_N_PER_MM
   DAMPING_NS_PER_MM` 是持续目标。先确认实际参数、持续意图、限值和普通停止条件；它们只表达本地完整
   提交，不自动建立持续心跳。进程退出、断连、重连和停止心跳都不是停止；只用显式 `stop` 表达普通停止，
   并以新鲜遥测分别判断采用、输出许可和设备影响。
3. 低速短时试动只使用 `device ... velocity-test MM_S 1..10`。该入口在读取 Startup 速度软幅值后拒绝非
   零有限值以外或超幅值的目标，核对 Idle 与选定通路的联系，并在已尝试目标后显式发送一次 Stop。它成功
   不证明机构效果。
4. 使用 `heartbeat-test SECONDS` 执行单次 CLI 的显式启停心跳；在 Shell 或 Web UI 中使用
   `heartbeat-start`、`heartbeat-stop`、`heartbeat-once`。停止心跳不发送 Stop。
5. `save FILE`、`restore-factory`、`reset-application`、`enter-update` 返回或关联操作键。超时、断连或
   未知结果时保留该键，用 `result --operation-key KEY` 查询原结果，绝不自动重放。`release --operation-key
   KEY` 只处理已结清结果。`enter-update` 只请求更新入口，镜像传输与启动核对属于外部流程。
6. Web UI 中先连接并核对 Identity，再选择当前操作通路。配置页可读取 user、factory、startup 记录，先
   校验再保存读回；诊断页的 H723 状态、H723 所见 ODrive 与 ODrive USB 快照是独立数据来源。控制页先开启
   当前通路心跳，再提交已确定的目标；停止使用当前通路的 Stop 控件。

结果只陈述已取得的通信和设备回复事实。不要将本地提交、HTTP 成功、心跳、枚举或一次遥测表述为目标采用、
输出或机械效果已经确认。
