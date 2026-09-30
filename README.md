<!-- Copyright The eha_controller Contributors -->

# `eha-tool`

`eha-tool` 是 EHA 的官方命令行工具，提供单次命令、交互式 Shell 和本机 WebUI。它通过相邻
[`eha-sdk`](https://github.com/EHAPower/eha-sdk) 访问一台明确选择的控制器；协议、配置格式和
客户接入说明由 SDK 维护。本仓维护工具的使用、输出和本机资源边界。

构建依赖和检查入口见[贡献指南](CONTRIBUTING.md)。完整命令、选项和默认值以
`eha-tool --help` 及相应子命令的 `--help` 为准。

## 使用方式

无参数或 `eha-tool shell` 进入交互式 Shell。`shell --script <FILE>` 和 `shell --batch`
分别从文件和标准输入串行执行命令；首个失败即停止。`help`、`help device` 与
`help config` 可在 Shell 中查看帮助。Shell 中 USB 先用 `device select <完整序列号>`、`device connect`，CAN 用
`device connect-can <port> <node> <profile>` 核对 Identity，再执行 `status` 等业务命令。

`eha-tool webui [--port <PORT>]` 启动仅监听本机回环地址的 WebUI，默认端口为 `8080`。
跨主机使用时，应通过符合部署访问控制要求的端口转发访问同一端口。页面可分别连接 USB 或
CAN；每次操作都显示所选通路，连接、切换通路和关闭页面不会发送目标、Stop 或 Reset。

`eha-tool --version` 只显示本地工具版本和构建信息，不访问设备。`config-check <FILE>`
只检查本地配置文件，也不访问设备。

## 选择对象并查询

先枚举候选，再用完整身份执行动作。多个、重复或缺少身份的候选必须由使用者明确选择；枚举不
代表已连接或已核对固件身份。

| 通路 | 发现与参数 | 连接后的首项操作 |
|---|---|---|
| USB | `devices`；`device --usb <完整序列号>` | `inspect` 核对 Identity |
| CAN | `can-devices`；`device --can-port <串口> --node <节点号> --profile <配置组>` | `inspect` 核对 Identity |
| ODrive USB | `odrive devices`；`odrive status --serial <十六进制序列号>` | 只读独立快照 |

CAN 当前使用 CANable2 原厂 SLCAN。`devices` 与 `can-devices` 只读取主机候选信息，不打开设备或
扫描总线。`odrive` 只发现或读取明确指定的 ODrive USB，不清错、重启、保存、标定、喂狗或提交
控制；它与 EHA 查询是两次独立 I/O，不能据此判断内部 CAN 状态。

常用查询命令为 `inspect`、`status`、`measurements`、`diagnostics`、`telemetry <秒>` 与
`config-read --view <factory|user|startup|communication>`。`--json` 的具体可用范围见各命令帮助。

```sh
eha-tool devices
eha-tool device --usb <完整序列号> inspect
eha-tool device --usb <完整序列号> status
eha-tool can-devices
eha-tool device --can-port <CANable2-串口> --node <节点号> --profile <实际配置组> inspect
eha-tool odrive --json status --serial 0x0123456789AB
```

## 控制与停止

`position <mm>`、`velocity <mm/s>`、`force <N>` 和
`impedance <mm> <N/mm> <N·s/mm>` 提交持续目标；`stop` 是唯一表达普通停止意图的命令。零速度
仍是速度目标。Shell 中的 `heartbeat-start`、`heartbeat-stop` 与 `heartbeat-once` 只控制心跳，
停止心跳、关闭 Shell 或结束单次命令不会发送 Stop。

首次低速试动应先在同一通路读取 `status`、`diagnostics` 和 `startup` 配置，再使用
`velocity-test <低速目标-mm/s> <1..10秒>`。目标必须非零、有限，且绝对值不超过 Startup 的
`protection.soft_velocity_max_mm_s`。该命令只作短时试动并显式提交一次 Stop；成功表示已取得其
要求的通信和停止结果，不证明机构行程或负载正确。

所有命令输出的是工具与 SDK 已取得的本地通信事实。本地提交、HTTP 成功、心跳运行或收到遥测都
不单独证明固件采用目标或机构已执行；超时、断连、身份不符和读取失败如实输出。

## 配置与维护

`config-check` 完成 SDK 的静态配置检查；字段、schema 和换算规则见
[SDK 配置检查](https://github.com/EHAPower/eha-sdk#配置检查)。检查通过不替代设备身份、通信、
写后读回或实际运行条件的核对。

```sh
eha-tool device --usb <完整序列号> config-read --view user --output before.json
eha-tool config-check candidate.json
eha-tool device --usb <完整序列号> save candidate.json
eha-tool device --usb <完整序列号> result --operation-key <保存返回的操作键>
eha-tool device --usb <完整序列号> release --operation-key <保存返回的操作键>
eha-tool device --usb <完整序列号> reset-application
eha-tool device --usb <完整序列号> enter-update
```

`save` 的完成结果包含同一设备用户记录的读回；保存不自动复位、采用新启动配置或释放维护结果。
`restore-factory`、`reset-application` 与 `enter-update` 也会返回操作键。超时、断连或结果未知时，
保留原操作键并用 `result` 查询原结果，绝不自动重发原维护操作；`release` 仅释放已结清的结果。

`enter-update` 只请求应用进入更新入口，不传输镜像。镜像传输、DFU 对象核对和新应用启动核对由
外部更新流程负责。

## 结果边界

工具不模拟设备，也不隐式停止、复位或重放目标。关闭进程只回收本地资源。配置导出仅在固件报告
完整记录时写入文件，且不会覆盖已有文件；本地写入失败可能留下不完整文件。设备适用性、安装条件、
保护结果和机构效果须由相应产品流程核对。
