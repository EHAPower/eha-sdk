---
name: eha-tool-usage
description: 使用 eha-tool 对明确选择的 EHA 执行查询、维护、持续控制，或操作其持久 Shell 和本机 Web UI 时使用。
---

<!-- Copyright The eha-sdk Contributors -->

# eha-tool 使用

按本仓 [AGENTS.md](../../../AGENTS.md) 的项目边界工作。工具的完整命令、参数、示例与结果边界由
[eha-tool README](../../../crates/tool/README.md) 唯一维护；构建、安装和软件检查见
[CONTRIBUTING.md](../../../CONTRIBUTING.md)。本 skill 只说明选择入口和执行时会改变决策的约束。

## 选择入口

- 用单次 CLI 查询、维护或执行一次动作；每个 `device` 动作都会新建本地会话并核对 Identity。先枚举候选，
  再以完整 USB 序列号，或 CAN 串口、节点和实际采用的配置组明确对象。候选列表不代表连接或 Identity 已核对。
- 用 `config-check FILE` 检查完整本地配置，不访问设备。持久 Shell 中的 `config check FILE` 同样不要求连接；
  `config validate FILE` 要求已连接会话。配置格式与完整导出、保存流程见
  [配置与维护](../../../crates/tool/README.md#配置与维护)。
- 用 `shell` 保留同一个设备会话，适合需要连续查询、心跳或串行自动化的操作。`shell --script` 与
  `shell --batch` 在首个错误处停止；每个批处理仍要先完成对象选择和连接。具体连接和批处理语义见
  [持久 Shell](../../../crates/tool/README.md#持久-shell)。
- 用 `webui` 操作本机页面；它只监听回环地址。页面刷新、关闭、切换通路、断连和停止心跳都不发送 Stop；
  当前操作通路决定请求的设备会话。页面流程见 [Web UI](../../../crates/tool/README.md#web-ui)。
- 用 `odrive` 读取 ODrive USB 的独立只读快照。它不清错、保存或控制，且不能证明 H723 与 ODrive 的内部
  CAN 状态；详见 [ODrive USB 只读](../../../crates/tool/README.md#odrive-usb-只读)。

## 控制、维护与结果

1. `position`、`velocity`、`force` 和 `impedance` 都提交持续目标，不自动建立持续心跳。提交前明确实际参数、
   持续意图、限值和普通停止条件；提交后用新鲜遥测分别判断采用、输出许可和设备影响。
   完整行为和受限短时试动的条件见
   [控制与停止](../../../crates/tool/README.md#控制与停止)。
2. 进程退出、超时、断连、重连、页面关闭和停止心跳都不表示停止控制。普通停止只能显式使用 `stop`；
   停止心跳也不发送 Stop。`velocity-test` 是唯一内建的短时速度编排，不以其成功推断机构效果。
3. `save`、`restore-factory`、`reset-application` 和 `enter-update` 的超时、断连或未知结果，保留原操作键并用
   `result` 查询原结果，绝不自动重放；`release` 只处理已结清结果。`enter-update` 只请求更新入口，镜像传输
   与新应用启动核对属于外部流程。
4. 结果只陈述通信与设备回复事实。枚举、HTTP 成功、本地提交、心跳或一次遥测都不证明目标采用、输出或
   机械效果；完整适用边界见 [结果边界](../../../crates/tool/README.md#结果边界)。
