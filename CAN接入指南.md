<!-- Copyright The eha-sdk Contributors -->

# 客户 CAN 接入指南

本指南组织自行实现 CAN 客户端的接入顺序，不要求 Rust SDK。线上字节和分片以
[公共应用通信协议](docs/通信协议/公共应用通信协议.md)、[CAN 绑定](docs/通信协议/CAN通信绑定.md)为准；
目标、停止和结果处理共用[公共交互规则](README.md#公共交互与结果)。

## 接入前提与职责

部署方提供控制器本次启动采用的节点号、Classic／FD 配置组和总线访问条件。节点号
`config.external_can.id` 是逻辑值，不是原始 CAN ID；不能猜测默认值，或用同型设备、USB
描述符替代身份。取得 Identity 后再读配置视图核对实际设置。

| 客户端负责 | 实现要点 |
|---|---|
| 完整消息 | 公共头、精确长度和 CRC32C；不补字段，不把 CAN 填充计入消息 |
| 分片与重组 | 冻结完整消息；维护每方向、节点、通道的编号保护、组装期限和通信世代 |
| 查询关联 | 非零且递增、不复用的 `query_id`；Identity 后绑定 UID、`run_nonce` 和类别 |
| 联系维持 | 独立调度显式 `Heartbeat`；普通流量和本地定时器不能代替设备收到心跳 |
| 维护关联 | 保存维护键与候选；超时或断连后查询原结果和实际状态，不自动重发 |

## 主机 CAN 通道

SDK 的 `CanChannel` 只发送和接收原始帧：Rust 应用以 `CanChannelFactory` 接入自己的系统／驱动服务，SDK 不发现、初始化或恢复物理 CAN 设备，也不解释厂商协议或指定比特率。Python 不是公共协议或 Rust SDK 的必需依赖。

官方 `eha-tool` 提供两条明确边界的接入路径。普通路径使用每用户 `connections.json` 中保存的连接档案：位置为 `XDG_CONFIG_HOME/eha-tool/connections.json`，macOS 为 `~/Library/Application Support/eha-tool/connections.json`，未设 `XDG_CONFIG_HOME` 的其它系统为 `~/.config/eha-tool/connections.json`。工具只支持其中 `adapter.kind="canable2"` 的 CANable2/SLCAN 档案：按保存的稳定 USB 序列号寻找唯一串口，并应用 1 Mbps 仲裁速率、2 Mbps FD 数据速率的预设。它不识别或实现厂商 USB 协议，也不尝试复位或恢复驱动。

连接档案包含名称、适配器序列号、EHA 节点、帧形态、可选自动连接与默认标记；`connection save` 先通过 Identity 和已有 UID 门禁，才写入或绑定预期 EHA UID，Python 路径只用于本次操作而不写入档案。普通用户用 `connection adapters [--can-python PATH]` 列出候选、`connection list` 查看档案，再用下列命令保存、连接或删除档案：

```sh
eha-tool connection save ID --name NAME --serial SERIAL --node N \
  [--mode fd] [--default] [--autoconnect] [--can-python PATH]
eha-tool connection connect ID [--can-python PATH] [--timeout-secs N]
eha-tool connection delete ID
eha-tool device --connection ID ACTION
```

精确参数和输出以各子命令的 `--help` 为准。发布 bundle 通过私有 `runtime/python` 运行已锁定的 `python-can` 和 `pyserial`，因此普通使用不依赖 `PATH`、`pip`、虚拟环境或 `can.conf`。

同序列号候选不唯一、适配器缺失、通道打不开、Identity 失败或返回 UID 不等于档案预期 UID 时，工具拒绝连接；绝不改用另一串口、适配器、USB 或历史目标。首次连接和物理重连只执行 Identity：不自启 Heartbeat、不恢复控制、不重放目标或维护操作。用户先处理实际物理连接，再显式发起通信恢复。

更换电脑时复制完整发行目录，在新主机首次执行 `connection adapters`，选择实际可见适配器并 `connection save`，再显式连接；普通路径不需要手改设备路径、Python 或 `can.conf`。档案保存在运行 backend 的该用户目录，不在浏览器中；换 CAN 适配器须新建档案或明确重新绑定，旧档案不会静默指向新硬件。一个运行中的 WebUI CAN 网络绑定其首次打开的适配器；要切换到另一档案/适配器，先结束该网络上的会话，再重启 WebUI 后连接。

高级路径保留 `--can-channel CONTEXT --node N [--can-mode classic|fd] [--can-python PATH]`，供其它 `python-can` 后端使用。`CONTEXT` 仍按 `python-can` 的命名配置或等价 `CAN_CONFIG_<CONTEXT>` 环境配置提供；调用方负责所选设备的驱动、权限、总线设置及恢复。`--can-mode` 和 SDK 的 `transport::can::Mode` 只选择 EHA CAN 绑定所要求的帧形态，必须与控制器实际启动配置一致；`fd` 仍要求操作系统、驱动和设备实际支持 CAN FD 与 BRS。可参照 [python-can 配置说明](https://python-can.readthedocs.io/en/stable/configuration.html) 与 [总线接口](https://python-can.readthedocs.io/en/stable/bus.html)。

发布包制作者为每个已验证的 OS／架构构建工具与 runtime。当前受支持的普通 bundle 使用：

```sh
python3 scripts/package_tool.py --binary /abs/target/release/eha-tool \
  --target aarch64-apple-darwin --output /abs/eha-tool-aarch64-apple-darwin
```

输出目录含 `eha-tool` 与 `runtime/`；这只证明包已生成，不证明任一 CAN 适配器、EHA 身份、总线、控制或机构效果。驱动打开、写调用返回或工具显示“已连接”也只证明本地阶段；仍以同一通路的 Identity 核对身份与通信设置。

## 报文定义入口

Classic 和 FD 重组后的完整消息须逐字节一致。按下表实现规则，再用样例核对：

| 需要实现的部分 | 唯一维护位置 |
|---|---|
| 公共消息头、长度、字节顺序、CRC32C | [完整消息与基本表示](docs/通信协议/公共应用通信协议.md#2-完整消息与基本表示) |
| 命令、回复及各业务字段 | [消息目录](docs/通信协议/公共应用通信协议.md#3-消息目录)和[公共交互与结果](README.md#公共交互与结果) |
| 节点号、速率配置与 Classic／FD 帧形态 | [启动配置与帧形态](docs/通信协议/CAN通信绑定.md#1-启动配置与帧形态) |
| 扩展 ID 的组成、方向、通道、传输编号与片序 | [扩展标识符](docs/通信协议/CAN通信绑定.md#2-扩展标识符) |
| 分片长度、DLC 与末片填充 | [公共消息和分片载荷](docs/通信协议/CAN通信绑定.md#3-公共消息和分片载荷) |
| 乱序、重复、超时、编号重用及连接恢复 | [组装、失效和重连](docs/通信协议/CAN通信绑定.md#4-组装失效和重连) |
| 实际发送结果与完整消息交付的区别 | [接收分类和发送边界](docs/通信协议/CAN通信绑定.md#5-接收分类和发送边界) |

## 查询、心跳与维护结果

```mermaid
sequenceDiagram
    participant Host as 客户 CAN 客户端
    participant EHA as EHA 控制器
    Host->>EHA: Query Identity
    EHA-->>Host: UID、run_nonce、当前通信设置
    Host->>EHA: 查询配置与当前反馈
    EHA-->>Host: 对应 query_id 的快照
    Note over Host,EHA: 控制前核对配置、位置、限值与停止条件
    Host->>EHA: 独立调度 Heartbeat
    EHA-->>Host: 后续新鲜遥测：观察该入口联系
    Host->>EHA: 完整目标
    EHA-->>Host: 持续遥测，观察采用与执行事实
    Host->>EHA: Stop
    Note over Host,EHA: 暂不发后续用户指令；Heartbeat 独立继续
    EHA-->>Host: 新鲜遥测确认无目标、合格 Idle
    Host->>Host: 停心跳、关闭连接
```

遥测独立发布，不是逐心跳或逐目标确认；图中箭头不承诺消息送达或指令被采用。`Stop` 与查询、目标、维护共用最新消息槽；
连续发送 Stop 和查询不能证明停止执行。首次 Identity 查询可尚不知道运行实例，后续
`Query`、`ReadConfig`、`ReadResult` 必须按[运行实例与快照](docs/通信协议/公共应用通信协议.md#4-运行实例快照和通用字段)关联回复。

维护使用 UID、`run_nonce`、递增 `operation_id` 组成的键。结果查询、留存和释放见
[维护结果会话](docs/通信协议/公共应用通信协议.md#10-维护结果会话)：本地发送、CAN ACK 或预期断连不证明业务完成；
`OperationResult`、`DataUnavailable`、实际记录读回分别表达操作事实、不可取得事实和可供比较的字节。

## 可核对报文

使用[公共协议的可复算样例](docs/通信协议/公共应用通信协议.md#13-可复算样例与失败场景)
与[CAN 绑定的代表性承载](docs/通信协议/CAN通信绑定.md#8-代表性承载)核对独立实现：

| 场景 | 核对内容 |
|---|---|
| 主机 Heartbeat | `node=1`、主机到固件、`transfer_id=1` 的扩展 ID、8 B 数据与 Classic／FD 一致的承载。 |
| Position `10 mm` | 同一完整 12 B 公共消息在 Classic 的两帧与 FD 的一帧之间逐字节重组一致。 |

这些例子只证明编码、分片和重组是否符合合同；不证明目标已采用、控制仍获准、设备已经
执行，或维护操作已经完成。
