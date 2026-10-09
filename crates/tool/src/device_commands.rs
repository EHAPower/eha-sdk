// Copyright The eha-sdk Contributors

//! 官方工具对桌面 SDK 的最小单次调用入口。
//!
//! 每次调用先以同一通路核对 Identity。命令结束只关闭本地句柄；只有 `stop` 与明确编排
//! 启动、停止边界的 `velocity-test` 会发送 Stop，心跳也只由相应显式命令启停。

use std::{
    io::Write,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use clap::{Args, Subcommand};
use eha_sdk::{
    can::{self, CanOptions},
    config::{Config, ExternalCanProfile},
    host::{Client, Failure, Reply, Wait},
    protocol::{
        Response,
        responses::{
            ConfigRecordState, ConfigView, ContactState, DesiredAxis, TargetMode, TelemetryFields,
        },
    },
    session::MaintenanceKey,
    usb,
};

use crate::session::{Command as SessionCommand, ConnectionRequest, ToolSession};

/// 通过一个已明确选择的 CAN 或 USB SDK adapter 执行业务操作。
#[derive(Args)]
pub struct DeviceCommand {
    /// 以稳定的结构化 JSON 输出动作结果与会话快照。
    #[arg(long)]
    json: bool,
    /// USB 设备的完整序列号；SDK 会重新枚举并认领 `ff:45:01` Bulk 接口。
    #[arg(long, conflicts_with = "can_port")]
    usb: Option<String>,
    /// CANable2 SLCAN 串口路径，例如 `/dev/cu.usbmodem…` 或 `COM3`。
    #[arg(long, conflicts_with = "usb")]
    can_port: Option<String>,
    /// CAN 逻辑节点号；仅与 `--can-port` 一起使用。
    #[arg(long, requires = "can_port")]
    node: Option<u8>,
    /// 已由目标实际采用的 CAN 配置，例如 `fd_1m_5m`；仅与 `--can-port` 一起使用。
    #[arg(long, requires = "can_port")]
    profile: Option<String>,
    /// 单次本地提交或固件回复等待上限，单位秒。
    #[arg(long, default_value_t = 5)]
    timeout_secs: u64,
    #[command(subcommand)]
    action: Action,
}

/// 只提供 SDK 已经覆盖的最小维护与控制编排。
#[derive(Subcommand)]
enum Action {
    /// 核对固件 Identity 并输出实际身份与运行实例。
    Inspect {
        /// 只输出 UID、运行实例及更新路由的 JSON，供操作后身份核对。
        #[arg(long)]
        json: bool,
    },
    /// 读取当前状态。
    Status,
    /// 读取详细测量快照。
    Measurements,
    /// 读取详细诊断快照。
    Diagnostics,
    /// 读取指定配置视图，默认实际用户记录。
    ConfigRead {
        /// `factory`、`user`、`startup` 或 `communication`。
        #[arg(long, default_value = "user")]
        view: String,
        /// 将完整配置原始字节写入新文件；不支持 communication 视图，不覆盖已有文件。
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// 在指定秒数内观察异步遥测；不会隐式启动心跳或控制。
    Telemetry {
        /// 观察时长，单位秒。
        seconds: u64,
    },
    /// 显式启用 SDK 心跳，持续指定秒数后显式停止；不发送 Stop。
    HeartbeatTest {
        /// 心跳持续时长，单位秒。
        seconds: u64,
    },
    /// 显式提交速度目标，单位 mm/s；返回仅表示本地 I/O 完整提交。
    Velocity {
        /// 目标速度，单位 mm/s。
        mm_s: f32,
    },
    /// 显式提交位置目标，单位 mm；返回仅表示本地 I/O 完整提交。
    Position { mm: f32 },
    /// 显式提交力目标，单位 N；返回仅表示本地 I/O 完整提交。
    Force { n: f32 },
    /// 显式提交阻抗目标：平衡位置 mm、刚度 N/mm、阻尼 N·s/mm。
    Impedance {
        equilibrium_mm: f32,
        stiffness_n_per_mm: f32,
        damping_ns_per_mm: f32,
    },
    /// 在同一会话内显式启停心跳和控制，进行受启动配置限值约束的短时速度试验。
    VelocityTest {
        /// 目标速度，单位 mm/s；必须为非零有限值，且不超过实际启动配置的速度软幅值。
        mm_s: f32,
        /// 保持目标并观察遥测的时长，单位秒；只能是 1 至 10 秒。
        #[arg(value_parser = clap::value_parser!(u8).range(1..=10))]
        seconds: u8,
    },
    /// 显式提交停止控制；返回仅表示本地 I/O 完整提交。
    Stop,
    /// 显式请求应用系统复位；返回仅表示本地 I/O 完整提交。
    ResetApplication,
    /// 显式请求进入已交付的更新入口；返回仅表示本地 I/O 完整提交。
    EnterUpdate,
    /// 保存完整本地 JSON，并等待维护终态和实际用户记录逐字节读回。
    Save {
        /// 待保存的完整配置 JSON 文件。
        file: PathBuf,
    },
    /// 读取同设备出厂记录、请求恢复并以实际用户记录逐字节读回核对。
    RestoreFactory,
    /// 使用已有的 36-byte 操作键读取维护结果；不会重发原维护操作。
    Result {
        /// 72 个十六进制字符的操作键（UID 12 B + run nonce 16 B + operation id 8 B）。
        #[arg(long)]
        operation_key: String,
    },
    /// 读取已结清结果、请求释放，并再次读取确认释放；不会重发原维护操作。
    Release {
        /// 72 个十六进制字符的操作键（UID 12 B + run nonce 16 B + operation id 8 B）。
        #[arg(long)]
        operation_key: String,
    },
}

pub fn run(command: DeviceCommand) -> Result<(), String> {
    let (mm_s, seconds) = match command.action {
        Action::VelocityTest { mm_s, seconds } => (mm_s, seconds),
        _ => return run_session(command),
    };
    validate_velocity_test_shape(mm_s)?;
    if command.json {
        return Err(
            "velocity-test 输出逐步试验记录，不支持 --json；自动化持续操作使用 shell --batch"
                .into(),
        );
    }
    let wait = Wait::new(Duration::from_secs(command.timeout_secs));
    let selected_usb = command.usb.is_some();
    let backend = match (command.usb, command.can_port) {
        (Some(serial), None) => usb::open(&serial)?,
        (None, Some(port)) => {
            let node = command.node.ok_or("CAN 通路需要 --node")?;
            let profile =
                parse_profile(command.profile.as_deref().ok_or("CAN 通路需要 --profile")?)?;
            can::open(CanOptions::canable2(port, node, profile))?
        }
        (None, None) => {
            return Err("请选择 --usb SERIAL 或 --can-port PORT --node N --profile NAME".into());
        }
        (Some(_), Some(_)) => return Err("USB 与 CAN 通路不能同时选择".into()),
    };
    let mut client = Client::new(backend);
    let identity = client.identify(None, &wait).map_err(format_sdk_error)?;
    print_reply("固件 Identity 回复", &identity)?;
    velocity_test(&mut client, &wait, selected_usb, mm_s, seconds)?;
    client.close();
    Ok(())
}

fn run_session(command: DeviceCommand) -> Result<(), String> {
    let request = match (command.usb, command.can_port) {
        (Some(serial), None) => ConnectionRequest::Usb { serial },
        (None, Some(port)) => ConnectionRequest::Can {
            port,
            node: command.node.ok_or("CAN 通路需要 --node")?,
            profile: command.profile.ok_or("CAN 通路需要 --profile")?,
        },
        (None, None) => {
            return Err("请选择 --usb SERIAL 或 --can-port PORT --node N --profile NAME".into());
        }
        (Some(_), Some(_)) => return Err("USB 与 CAN 通路不能同时选择".into()),
    };
    let mut session = ToolSession::new().with_timeout(Duration::from_secs(command.timeout_secs));
    let connected = session
        .connect(request)
        .map_err(|error| cli_session_error(command.json, error))?;
    if let Action::Inspect { json } = command.action {
        if command.json || json {
            println!(
                "{}",
                serde_json::to_string(&inspect_json(connected.identity.as_ref()))
                    .map_err(|error| error.to_string())?
            );
        } else {
            print_json("固件 Identity 回复", connected.identity.as_ref());
        }
        return Ok(());
    }
    let mut config_output = None;
    let action = match command.action {
        Action::Status => SessionCommand::Status,
        Action::Measurements => SessionCommand::Measurements,
        Action::Diagnostics => SessionCommand::Diagnostics,
        Action::ConfigRead { view, output } => {
            config_output = output;
            SessionCommand::ConfigRead { view }
        }
        Action::Telemetry { seconds } => {
            let deadline = Instant::now() + Duration::from_secs(seconds);
            let mut latest = None;
            while Instant::now() < deadline {
                let result = session
                    .execute(SessionCommand::Telemetry)
                    .map_err(|error| cli_session_error(command.json, error))?;
                if !result.data.is_null() {
                    latest = Some(result.data);
                }
                thread::sleep(Duration::from_millis(20));
            }
            let output = serde_json::json!({"action":"telemetry", "data": latest, "snapshot": session.snapshot()});
            print_result(command.json, &output)?;
            return Ok(());
        }
        Action::HeartbeatTest { seconds } => {
            session
                .execute(SessionCommand::HeartbeatStart)
                .map_err(|error| cli_session_error(command.json, error))?;
            thread::sleep(Duration::from_secs(seconds));
            let result = session
                .execute(SessionCommand::HeartbeatStop)
                .map_err(|error| cli_session_error(command.json, error))?;
            print_result(
                command.json,
                &serde_json::to_value(result).map_err(|error| error.to_string())?,
            )?;
            return Ok(());
        }
        Action::Velocity { mm_s } => SessionCommand::Velocity { mm_s },
        Action::Position { mm } => SessionCommand::Position { mm },
        Action::Force { n } => SessionCommand::Force { n },
        Action::Impedance {
            equilibrium_mm,
            stiffness_n_per_mm,
            damping_ns_per_mm,
        } => SessionCommand::Impedance {
            equilibrium_mm,
            stiffness_n_per_mm,
            damping_ns_per_mm,
        },
        Action::Stop => SessionCommand::Stop,
        Action::ResetApplication => SessionCommand::ResetApplication,
        Action::EnterUpdate => SessionCommand::EnterUpdate,
        Action::Save { file } => SessionCommand::ConfigSave {
            record: std::fs::read_to_string(&file)
                .map_err(|error| format!("配置文件读取失败 `{}`: {error}", file.display()))?,
        },
        Action::RestoreFactory => SessionCommand::RestoreFactory,
        Action::Result { operation_key } => SessionCommand::MaintenanceResult { operation_key },
        Action::Release { operation_key } => SessionCommand::MaintenanceRelease { operation_key },
        Action::Inspect { .. } | Action::VelocityTest { .. } => {
            return Err("该动作未进入持久工具会话分发".into());
        }
    };
    let result = session
        .execute(action)
        .map_err(|error| cli_session_error(command.json, error))?;
    if let Some(path) = config_output {
        if result
            .data
            .get("record_state")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        {
            return Err("固件配置记录不完整，未创建文件".into());
        }
        if result.data.get("view").and_then(serde_json::Value::as_u64) == Some(3) {
            return Err("communication 是通信设置视图，不能导出为配置文件".into());
        }
        let data = result
            .data
            .get("data_utf8")
            .and_then(serde_json::Value::as_str)
            .ok_or("固件配置记录不是完整 UTF-8，未创建文件")?;
        let mut file = std::fs::File::create_new(&path)
            .map_err(|error| format!("无法创建配置文件 `{}`: {error}", path.display()))?;
        file.write_all(data.as_bytes())
            .map_err(|error| format!("配置文件 `{}` 写入失败：{error}", path.display()))?;
    }
    print_result(
        command.json,
        &serde_json::to_value(result).map_err(|error| error.to_string())?,
    )
}

fn print_json(label: &str, value: Option<&serde_json::Value>) {
    match value {
        Some(value) => println!(
            "{label}：\n{}",
            serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into())
        ),
        None => println!("{label}不可取得。"),
    }
}

fn inspect_json(identity: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(identity) = identity else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        "uid": identity.get("uid"),
        "run_nonce": identity.pointer("/sample/run_nonce"),
        "run_identity": identity.get("run_identity"),
        "update_route": identity.get("update_route"),
    })
}

fn cli_session_error(json: bool, error: crate::session::SessionError) -> String {
    if json {
        println!("{}", serde_json::json!({"ok": false, "error": error}));
    }
    error.to_string()
}

fn print_result(json: bool, value: &serde_json::Value) -> Result<(), String> {
    let text = if json {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    }
    .map_err(|error| error.to_string())?;
    println!("{text}");
    Ok(())
}

const MAX_VELOCITY_TEST_SECS: u8 = 10;
const ODRIVE_AXIS_STATE_IDLE: u32 = 1;

fn velocity_test(
    client: &mut Client,
    wait: &Wait,
    selected_usb: bool,
    mm_s: f32,
    seconds: u8,
) -> Result<(), String> {
    let startup = client
        .read_config(ConfigView::Startup, wait)
        .map_err(format_sdk_error)?;
    let velocity_limit = startup_velocity_limit(&startup)?;
    let initial_status = client.status(wait).map_err(format_sdk_error)?;
    if !idle_without_unresolved(status_fields(&initial_status)?) {
        return Err("速度试验预检拒绝：初始 Status 不是合格、无错且无未明影响的 Idle".into());
    }
    validate_velocity_test_input(mm_s, seconds, velocity_limit)?;
    println!(
        "速度试验预检通过：启动配置速度软幅值为 ±{velocity_limit} mm/s；目标为 {mm_s} mm/s，保持 {} 秒。",
        seconds
    );
    print_reply("速度试验前的固件 Status 回复", &initial_status)?;

    client.start_heartbeat().map_err(format_sdk_error)?;
    let heartbeat_started_at = Instant::now();
    println!("已显式启动心跳调度；等待新 Telemetry 确认本入口联系与实际 Idle 条件。");
    if let Err(error) = await_telemetry(
        client,
        wait,
        heartbeat_started_at,
        "心跳后的固件 Telemetry 回复",
        |fields| selected_contact_active(fields, selected_usb),
    ) {
        return finish_heartbeat_only(client, error);
    }

    let primary = match client.velocity(mm_s, wait) {
        Ok(local) => {
            print_local_submission("速度试验目标", local.id);
            observe_velocity_test_telemetry(client, local.submitted_at, seconds)
        }
        Err(error) => Err(format_sdk_error(error)),
    };

    // 一旦已经尝试提交速度目标，即使本地结果未知也只显式发送一次 Stop；不重发目标。
    let stop = client.stop_control(wait).map_err(format_sdk_error);
    if let Ok(local) = &stop {
        print_local_submission("速度试验 Stop", local.id);
    }
    let stopped_at = stop
        .as_ref()
        .map_or_else(|_| Instant::now(), |local| local.submitted_at);
    let stopped = await_telemetry(
        client,
        wait,
        stopped_at,
        "速度试验 Stop 后的固件 Telemetry 回复",
        idle_without_unresolved,
    );
    finish_velocity_test(client, primary, stop.map(|_| ()), stopped)
}

fn startup_velocity_limit(reply: &Reply) -> Result<f32, String> {
    let Response::ConfigData(data) = reply.response().map_err(format_sdk_error)? else {
        return Err("速度试验预检未取得启动配置记录".into());
    };
    let fields = data.fields();
    if fields.view != ConfigView::Startup {
        return Err("速度试验预检收到的不是启动配置视图".into());
    }
    if fields.record_state != ConfigRecordState::Complete {
        return Err(format!(
            "速度试验预检拒绝：启动配置记录不可用（{:?}）",
            fields.record_state
        ));
    }
    let limit = Config::from_json(data.data())
        .map(|config| config.protection.soft_velocity_max_mm_s)
        .map_err(|error| format!("速度试验预检拒绝：启动配置无法按固件类型读取：{error:?}"))?;
    if !limit.is_finite() || limit <= 0.0 {
        return Err("速度试验预检拒绝：启动配置速度软幅值无效".into());
    }
    Ok(limit)
}

fn status_fields(reply: &Reply) -> Result<TelemetryFields, String> {
    let Response::Status(status) = reply.response().map_err(format_sdk_error)? else {
        return Err("速度试验未取得 Status 回复".into());
    };
    Ok(status.fields())
}

fn telemetry_fields(reply: &Reply) -> Result<TelemetryFields, String> {
    let Response::Telemetry(telemetry) = reply.response().map_err(format_sdk_error)? else {
        return Err("速度试验未取得 Telemetry 回复".into());
    };
    Ok(telemetry.fields())
}

fn validate_velocity_test_input(mm_s: f32, seconds: u8, velocity_limit: f32) -> Result<(), String> {
    validate_velocity_test_shape(mm_s)?;
    if !(1..=MAX_VELOCITY_TEST_SECS).contains(&seconds) {
        return Err(format!(
            "速度试验时长必须在 1 至 {MAX_VELOCITY_TEST_SECS} 秒之间"
        ));
    }
    if mm_s.abs() > velocity_limit {
        return Err(format!(
            "速度试验目标 {mm_s} mm/s 超出启动配置速度软幅值 ±{velocity_limit} mm/s"
        ));
    }
    Ok(())
}

/// 在接触设备前拒绝不可能的低速试验输入。
///
/// 软幅值需要读取实际 Startup 配置后才可判断，因此只把与设备无关的形状校验提前。
fn validate_velocity_test_shape(mm_s: f32) -> Result<(), String> {
    if !mm_s.is_finite() || mm_s == 0.0 {
        return Err("速度试验目标必须是非零有限 mm/s 值".into());
    }
    Ok(())
}

fn idle_without_unresolved(fields: TelemetryFields) -> bool {
    fields.target_mode == TargetMode::None
        && fields.desired_axis == DesiredAxis::Idle
        && fields.driver_state.has_status
        && fields.driver_state.qualified
        && !fields.driver_state.stale
        && !fields.driver_state.faulted
        && fields.axis_state_raw == ODRIVE_AXIS_STATE_IDLE
        && fields.axis_error_raw == 0
        && !fields.facts.device_operation_pending
        && !fields.facts.unknown_effect
}

fn selected_contact_active(fields: TelemetryFields, usb: bool) -> bool {
    let contact = if usb {
        fields.usb_contact
    } else {
        fields.can_contact
    };
    contact == ContactState::Active && idle_without_unresolved(fields)
}

fn await_telemetry(
    client: &Client,
    wait: &Wait,
    observed_after: Instant,
    label: &str,
    accepts: impl Fn(TelemetryFields) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + wait.timeout;
    let mut last = None;
    let mut received_at = None;
    loop {
        if wait.cancellation.is_cancelled() {
            return Err(format!("{label} 等待已取消"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{label} 未在等待期限内满足停止/准入条件；最后 Telemetry：{last:#?}"
            ));
        }
        if let Some(reply) = client.telemetry().filter(|reply| {
            reply.received_at > observed_after && received_at != Some(reply.received_at)
        }) {
            received_at = Some(reply.received_at);
            let fields = telemetry_fields(&reply)?;
            last = Some(fields);
            if accepts(fields) {
                print_reply(label, &reply)?;
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn observe_velocity_test_telemetry(
    client: &Client,
    submitted_at: Instant,
    seconds: u8,
) -> Result<(), String> {
    let deadline = submitted_at + Duration::from_secs(u64::from(seconds));
    let mut latest = None;
    let mut count = 0_u64;
    while Instant::now() < deadline {
        if let Some(reply) = client.telemetry() {
            let is_new = reply.received_at > submitted_at
                && latest
                    .as_ref()
                    .is_none_or(|previous: &Reply| previous.received_at != reply.received_at);
            if is_new {
                latest = Some(reply);
                count += 1;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    let reply = latest.ok_or("速度试验期间未收到速度目标本地提交后的新 Telemetry")?;
    print_reply("速度试验期间最后一个新 Telemetry 回复", &reply)?;
    println!(
        "速度试验观察结束：收到 {count} 个目标本地提交后的新遥测快照。这些回复不单独证明 ODrive 执行或机构物理运动。"
    );
    Ok(())
}

fn finish_heartbeat_only(client: &mut Client, primary: String) -> Result<(), String> {
    let heartbeat_stop = client.stop_heartbeat().map_err(format_sdk_error);
    if let Err(error) = heartbeat_stop {
        eprintln!("原始失败：{primary}\n随后停止心跳也失败：{error}");
    } else {
        println!("已显式请求停止心跳；未发送速度目标或 Stop。");
    }
    Err(primary)
}

fn finish_velocity_test(
    client: &mut Client,
    primary: Result<(), String>,
    stop: Result<(), String>,
    stopped: Result<(), String>,
) -> Result<(), String> {
    let heartbeat_stop = client.stop_heartbeat().map_err(format_sdk_error);
    if heartbeat_stop.is_ok() {
        println!(
            "已显式请求停止心跳。本地调度状态：{:#?}",
            client.heartbeat_status()
        );
    }

    let mut first_error = primary.err();
    for (label, result) in [
        ("速度试验 Stop", stop),
        ("Stop 后 Telemetry", stopped),
        ("停止心跳", heartbeat_stop),
    ] {
        if let Err(error) = result {
            if first_error.is_none() {
                first_error = Some(error);
            } else {
                eprintln!("{label} 的附加失败证据：{error}");
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn print_local_submission(action: &str, id: u64) {
    println!(
        "{action} 已本地完整提交（请求 {id}）。这不表示 CAN ACK、固件收到完整消息、目标采用或实际执行。"
    );
}

fn print_reply(label: &str, reply: &Reply) -> Result<(), String> {
    let response = reply.response().map_err(format_sdk_error)?;
    println!("{label}（主机接收时刻 {:?}）：", reply.received_at);
    match response {
        Response::Identity(value) => println!("{:#?}", value.fields()),
        Response::Status(value) | Response::Telemetry(value) => println!("{:#?}", value.fields()),
        Response::Measurements(value) => println!("{:#?}", value.fields()),
        Response::Diagnostics(value) => {
            let entries: Vec<_> = value.entries().map(|entry| entry.fields()).collect();
            println!("overflow={}; entries={entries:#?}", value.overflow());
        }
        Response::ConfigData(value) => print_config_data(value),
        Response::OperationResult(value) => println!("{:#?}", value.fields()),
        Response::DataUnavailable(value) => println!("{:#?}", value.fields()),
    }
    Ok(())
}

fn print_config_data(value: eha_sdk::protocol::ConfigData<'_>) {
    let fields = value.fields();
    println!("字段：{fields:#?}");
    let data = value.data();
    if fields.view == ConfigView::Communication {
        println!("通信设置：{:#?}", value.communication_settings());
        println!("通信视图原始字节（hex）：{}", hex_bytes(data));
    } else {
        match std::str::from_utf8(data) {
            Ok(text) => println!("实际配置原文：\n{text}"),
            Err(_) => println!("实际配置不是 UTF-8；原始字节（hex）：{}", hex_bytes(data)),
        }
    }
}

fn format_sdk_error(error: eha_sdk::host::Error) -> String {
    let mut output = format!("SDK 操作失败证据：{error:#?}");
    if let Some(key) = error.operation.as_deref() {
        let key = key_hex(*key);
        output.push_str(&format!(
            "\n关联维护键：{}。可使用 `device … result --operation-key {}` 只读追踪，不能重发原维护操作。",
            key, key
        ));
        if matches!(&error.failure, Failure::ResultUnknown(_)) {
            output.push_str("\n本次副作用结果未知；以上操作键用于读取实际结果。");
        }
    }
    output
}

fn parse_profile(value: &str) -> Result<ExternalCanProfile, String> {
    match value {
        "classical_500k" => Ok(ExternalCanProfile::Classical500K),
        "classical_1m" => Ok(ExternalCanProfile::Classical1M),
        "fd_500k_2m" => Ok(ExternalCanProfile::Fd500K2M),
        "fd_500k_500k" => Ok(ExternalCanProfile::Fd500K500K),
        "fd_1m_2m" => Ok(ExternalCanProfile::Fd1M2M),
        "fd_1m_5m" => Ok(ExternalCanProfile::Fd1M5M),
        "fd_1m_8m" => Ok(ExternalCanProfile::Fd1M8M),
        _ => Err("未知 --profile；可用值：classical_500k、classical_1m、fd_500k_2m、fd_500k_500k、fd_1m_2m、fd_1m_5m、fd_1m_8m".into()),
    }
}

fn key_hex(key: MaintenanceKey) -> String {
    hex_bytes(&key.to_bytes())
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{Action, DeviceCommand, run};

    fn missing_usb_velocity_test(mm_s: f32) -> DeviceCommand {
        DeviceCommand {
            json: false,
            usb: Some("missing-usb-serial".into()),
            can_port: None,
            node: None,
            profile: None,
            timeout_secs: 5,
            action: Action::VelocityTest { mm_s, seconds: 1 },
        }
    }

    #[test]
    fn velocity_test_rejects_invalid_speed_before_opening_missing_usb() {
        for mm_s in [0.0, f32::NAN] {
            let error = run(missing_usb_velocity_test(mm_s))
                .expect_err("invalid speed must fail before USB enumeration or connection");
            assert_eq!(error, "速度试验目标必须是非零有限 mm/s 值");
        }
    }
}
