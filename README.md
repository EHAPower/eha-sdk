<!-- Copyright The eha-sdk Contributors -->

# EHA SDK 与官方工具

本仓交付客户直接 CAN 接入资料、Rust `eha-sdk` 和官方工具 `eha-tool`。完整应用消息的字段、字节顺序、版本、
长度与 CRC32C 由[公共应用通信协议](docs/通信协议/公共应用通信协议.md)唯一维护；CAN
帧和 USB 字节流绑定分别由[CAN 通信绑定](docs/通信协议/CAN通信绑定.md)与
[USB 通信绑定](docs/通信协议/USB通信绑定.md)维护。工作区中的 `eha-tool` 是 SDK 的官方调用方，
客户程序不依赖它。

| 使用方式 | 入口 |
|---|---|
| 直接 CAN 接入 | [CAN 接入指南](CAN接入指南.md)，自行实现公共消息、CAN 分片和结果处理。 |
| Rust 程序 | 本文、crate rustdoc 和示例；CAN、USB 使用相同的 `host::Client` 业务 API。 |
| 官方工具 | [`eha-tool`](crates/tool/README.md)，控制与停止见其[使用说明](crates/tool/README.md#控制与停止)。 |

## 公共交互与结果

本节是 CAN、USB、Rust SDK 和 `eha-tool` 共用的客户可观察语义入口。精确线上字段、传输
期限和维护阶段仍以三份通信合同为准；Rust API 的参数、生命周期和错误以 rustdoc 为准。

| 事项 | 客户必须据此处理的事实 |
|---|---|
| 提交、完整性与频率 | CAN 和 USB 合并后只支持一个调用方串行提交，调用方无需等待前一条的接纳、执行或遥测结果。除独立的 `Heartbeat` 外，所有类型的用户指令合计正常最高 1000 Hz；CAN 多帧或 USB 多块只有完整组装并通过公共协议校验后才各算一条。此频率不承诺每条消息都会执行、任何端到端时延，或任一 CAN 配置下的组合带宽。 |
| 共同最新消息 | 除 `Heartbeat` 外，所有入口和指令类型共用一个最新消息槽，后到而尚未取走的完整消息覆盖先到消息，没有类型优先级；每个控制周期最多取走一条。相同入口按完整消息恢复的顺序交付，跨 CAN／USB 不还原物理到达先后。未取走的 `Stop` 可被后续查询、目标或维护请求覆盖；只有固件取走的 `Stop` 才清除当时目标，再次控制需要新目标。 |
| 无效输入 | 未收齐、组装超时或公共协议校验失败的内容不进入上述槽，不刷新联系、不改变目标、不开始维护、不写入配置，也不产生逐条错误确认。通过协议校验不表示业务已采用：不满足目标或维护开始条件的完整请求按其可观察结果被拒绝或报告未开始。 |
| 完整持续目标 | `Position`、`Velocity`、`Force` 各携带一个有限值；`Impedance` 同时携带平衡位置、刚度和阻尼。零值不是停止，没有隐含到位、时长或逐条接纳确认，也没有字段补丁或沿用旧参数。获准的新目标整份替换当前模式和参数；被拒绝的新目标不排队，也不因拒绝本身改变旧目标。保护、入口联系或设备条件仍可能独立结束目标；条件恢复不会恢复旧目标。 |
| 入口联系与切换 | 采用目标时会关联其实际接收的 CAN 或 USB 入口。只有该入口完整、合法的 `Heartbeat` 维持联系；打开连接、目标、查询、遥测和任一入口的其他流量均不能替代它。关联入口失联会结束目标，之后即使原入口恢复或另一入口一直有效也须重新提交完整目标。另一入口的心跳不接管或续期，另一入口的目标不采用、不排队且不改变原目标。两入口的 `Stop` 都可停止当前目标，但仍须先作为共同最新消息被固件取走。 |
| 查询与遥测 | 回复是形成时快照。数值、质量、来源年龄、快照时间和主机收到时间分别表达不同事实；缓存、转发或重新读取不刷新数据。过期、断连前或不同运行实例的数据不能当作当前状态。 |
| 本地提交与设备效果 | `LocalSubmission` 只表示 CAN 串口 `write/flush` 或 USB 最后一个 Bulk 包已在本地完成；它不证明 CAN ACK、固件收到、目标采用、驱动器执行或机构运动。`Reply` 只证明主机取得并解码回复。 |
| 维护与未知结果 | 保存、恢复出厂、复位和进入更新都以维护键关联结果。`NotStarted` 表示副作用尚未开始；修正条件后以新键明确提出下一次操作。`ResultUnknown` 时保存键，重连后重新 `identify`，再查询原结果和实际状态，不得自动重发。读回一致只证明同设备记录与候选逐字节一致，不证明已重启或采用。 |
| 复位与更新 | `ResetApplication` 和 `EnterUpdate` 都可能在回复前结束应用。复位后核对同一 UID 的新应用实例、版本和启动配置；进入更新只请求切换更新入口、不传输镜像，外部更新后同样核对身份和新应用。 |

## 依赖与连接能力

`eha-sdk` 是独立 Cargo 工作区，包含根包、`protocol`、`transport`、`eha-config` 和官方
`eha-tool`。二次开发使用同一修订的完整仓库及 `Cargo.lock`，不拼接不同修订的路径依赖；版本来自
`workspace.package.version`。独立克隆、依赖和检查见[贡献指南](CONTRIBUTING.md)。

| 通路 | 当前 SDK 能力与限制 |
|---|---|
| USB Type-C | `usb::discover` 只列出 `1209:0001` 候选；`UsbConnector::new(完整序列号)?.open()` 按实际描述符认领唯一 64 B Bulk IN/OUT 对。它不发 USB reset 或控制请求；打开后仍须 `identify`。 |
| 外部 CAN | 仅支持原厂 SLCAN 的 CANable2：`can::discover_serial_candidates()` 只列候选，`CanConnector::new(CanOptions::canable2(...)).open()` 实际打开。支持 `classical_500k`、`classical_1m`、`fd_500k_2m`、`fd_1m_2m`、`fd_1m_5m`；不支持 SocketCAN、其他适配器/固件，以及 `fd_500k_500k`、`fd_1m_8m`。 |

打开适配器或认领 USB 接口只证明本地资源取得；首次业务操作必须在同一通路
`Client::identify`，必要时提供预期 UID。描述符、CAN 节点或同型设备都不能替代身份。
USB 认领失败可能是占用、权限、系统或驱动问题；若确认旧会话存在，先在旧会话显式
`stop_control` 并观察状态，再释放连接。

## 最小 Rust 调用

`CanConnector` 或 `UsbConnector` 打开后提供 `host::Backend`，再由 `host::Client` 调用
身份、查询、遥测、心跳、控制和维护 API。`Wait` 只限定本地提交或回复等待；不取消已发送
操作，也不构成设备停止。

```rust,no_run
use std::time::Duration;
use eha_sdk::{
    can::{CanConnector, CanOptions},
    config::ExternalCanProfile,
    host::{Client, Wait},
};

fn main() -> Result<(), String> {
    let mut connector = CanConnector::new(CanOptions::canable2(
        "SERIAL_PORT", 1, ExternalCanProfile::Fd1M2M,
    ));
    let backend = connector.open()?;
    let mut client = Client::new(backend);
    let wait = Wait::new(Duration::from_secs(5));
    client.identify(None, &wait).map_err(|error| error.to_string())?;
    client.status(&wait).map_err(|error| error.to_string())?;
    client.close();
    Ok(())
}
```

USB 仅替换为 `UsbConnector::new("完整 USB 序列号")?.open()`。控制前核对身份、配置、
当前位置、限值和停止条件，调用 `start_heartbeat` 并从新鲜遥测确认联系；结束时显式
`stop_control`，观察无目标和合格 Idle 后再 `stop_heartbeat`、关闭连接。

关闭、`Drop`、`disconnect`、普通错误、取消和超时都不发送 Stop、Reset、Heartbeat，也不重放
旧目标。`disconnect()` 的 `ClientState` 只用于同一 Connector 重开后的 `Client::resume`；
新 Client 必须重新 `identify`，心跳不会自启。不要新建 Connector、重开进程或适配器来绕过
`transfer_id` 保护期；重新打开成功仍只证明本地资源取得。

## 配置检查

`configuration::validate_json` 不访问设备。它以 [`eha-config`](crates/config/README.md) 的
schema、字段表示和纯换算为基础，检查完整记录、跨字段关系、派生值和名义心跳周期；不补齐
字段、不重排 JSON，也不证明目标固件、现场设备或安装方向匹配。`begin_save` 与
`save_and_readback` 只报告本地提交、固件结果和同设备实际读回；结果已结清后才可显式
`release_result` 与 `confirm_released`。

## 独立 ODrive 读取

默认 `desktop` feature 的 `eha_sdk::odrive` 只读地发现或读取调用方明确指定的 ODrive USB
序列号。它不自动选择设备，不清错、重启、保存、标定、喂狗或提交控制。EHA 的
`Status`/`Telemetry`/`Diagnostics` 与 ODrive USB 快照没有共同时间或身份基准，必须分别保留
来源和读取时间，不能合成为原子双板状态或据此断言内部 CAN 正常。

## 示例与检查

[`reconnect_readonly`](examples/reconnect_readonly.rs) 演示同一 `UsbConnector` 的只读重开：

```sh
cargo run -p eha-sdk --example reconnect_readonly -- usb <完整 USB 序列号>
```

[`board_check`](examples/board_check.rs) 通过公开 SDK 查询、心跳、停止和维护读回；它不创建
运动目标。USB 的实际调用形式为 `cargo run -p eha-sdk --example board_check -- usb SERIAL OUTDIR`；
`decode` 模式只解码已有原始回复，示例不自动复位或重发。

构建与软件检查统一按[贡献指南](CONTRIBUTING.md#构建与检查)执行，并按改动影响选择范围。
软件检查不访问设备，不能代替实板收发、读回或设备执行证据。
