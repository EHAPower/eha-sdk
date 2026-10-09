---
name: eha-tool-usage
description: 使用 eha-tool 对明确选择的 EHA 执行查询、维护、持续控制，或操作其持久 Shell 和本机 Web UI 时使用。
---

<!-- Copyright The eha-sdk Contributors -->

# eha-tool 使用

按本仓 [AGENTS.md](../../../AGENTS.md) 的项目边界工作。完整命令、参数、示例和结果边界见
[eha-tool README](../../../crates/tool/README.md)，构建与软件检查见[CONTRIBUTING.md](../../../CONTRIBUTING.md)；
控制、心跳、跨入口和未知结果语义见[SDK README](../../../README.md#公共交互与结果)。

## 选择入口

- 单次查询、维护或动作使用 `device`；先枚举候选，再以完整 USB 序列号，或 CAN 串口、节点和实际配置组明确对象。每次动作建立本地会话并核对 Identity。
- 本地完整配置检查使用 `config-check FILE`；需要连续查询、心跳或串行自动化时使用 `shell`，具体会话和批处理语义见[持久 Shell](../../../crates/tool/README.md#持久-shell)。
- 本机页面使用 `webui`；ODrive USB 独立只读快照使用 `odrive`。两者的边界与页面流程见[eha-tool README](../../../crates/tool/README.md)。
