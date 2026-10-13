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
    can::{CanOptions, open as open_can_backend},
    config::Config,
    host::{Client, Reply, Wait},
    protocol::{
        Response,
        responses::{
            ConfigRecordState, ConfigView, ContactState, DesiredAxis, TargetMode, TelemetryFields,
        },
    },
    usb,
};

use crate::{
    connections::{ConnectionProfile, ConnectionStore, ManagedAdapter},
    session::{Command as SessionCommand, ConnectionRequest, ToolSession},
};

/// 通过一个已明确选择的 CAN 或 USB SDK adapter 执行业务操作。
#[derive(Args)]
pub struct DeviceCommand {
    /// 以稳定的结构化 JSON 输出动作结果与会话快照。
    #[arg(long)]
    json: bool,
    /// 显式附着现有共享服务；必须是私有实例描述符的绝对路径，失败不回落直连。
    #[arg(long, requires = "target_id", conflicts_with_all = ["usb", "can_channel", "connection", "node", "can_mode", "can_python"])]
    via_service: Option<PathBuf>,
    /// 共享服务内明确的目标 ID，例如 usb:完整序列号 或 can:节点号。
    #[arg(long, requires = "via_service")]
    target_id: Option<String>,
    /// USB 设备的完整序列号；SDK 会重新枚举并认领 `ff:45:01` Bulk 接口。
    #[arg(long, conflicts_with_all = ["can_channel", "connection"])]
    usb: Option<String>,
    /// 外部 python-can 配置中的 CAN 通道上下文名。
    #[arg(long, conflicts_with_all = ["usb", "connection"])]
    can_channel: Option<String>,
    /// 已保存的受管 CANable2 连接档案 ID。
    #[arg(long, conflicts_with_all = ["usb", "can_channel"])]
    connection: Option<String>,
    /// CAN 逻辑节点号；仅与 `--can-channel` 一起使用。
    #[arg(long, requires = "can_channel")]
    node: Option<u8>,
    /// CAN 帧格式：`classic` 或 `fd`；省略时为 `fd`。
    #[arg(long, requires = "can_channel", default_value = "fd")]
    can_mode: String,
    /// 运行 python-can 的 Python 路径；省略时使用随包运行时，源码树须显式配置开发解释器。
    #[arg(long, requires = "can_channel")]
    can_python: Option<String>,
    /// 单次本地提交或固件回复等待上限，单位秒。
    #[arg(long, default_value_t = 5)]
    timeout_secs: u64,
    #[command(subcommand)]
    action: Action,
}

/// 管理本机受管 CANable2 连接档案。档案只保存适配器稳定序列号，不保存 Python 路径。
#[derive(Args)]
pub struct ConnectionCommand {
    #[command(subcommand)]
    action: ConnectionAction,
}

#[derive(Subcommand)]
enum ConnectionAction {
    /// 按稳定 USB 序列号列出当前可用的 CANable2 适配器。
    Adapters {
        /// 用于本次发现的 Python；省略时使用随包运行时。
        #[arg(long)]
        can_python: Option<String>,
    },
    /// 列出本机连接档案。
    List,
    /// 核对 Identity 后保存或更新受管 CANable2 档案。
    Save {
        id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        serial: String,
        #[arg(long)]
        node: u8,
        #[arg(long, default_value = "fd")]
        mode: String,
        #[arg(long)]
        default: bool,
        #[arg(long)]
        autoconnect: bool,
        /// 仅用于本次 Identity 核对，不写入档案。
        #[arg(long)]
        can_python: Option<String>,
    },
    /// 使用已保存档案连接并核对 Identity。
    Connect {
        id: String,
        /// 仅用于本次连接，不写入档案。
        #[arg(long)]
        can_python: Option<String>,
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
    },
    /// 删除一份连接档案。
    Delete { id: String },
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
        /// `factory`、`user` 或 `startup`。
        #[arg(long, default_value = "user")]
        view: String,
        /// 将完整配置原始字节写入新文件，不覆盖已有文件。
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
        #[arg(value_parser = crate::parse_f32)]
        mm_s: f32,
    },
    /// 显式提交位置目标，单位 mm；返回仅表示本地 I/O 完整提交。
    Position {
        #[arg(value_parser = crate::parse_f32)]
        mm: f32,
    },
    /// 显式提交力目标，单位 N；返回仅表示本地 I/O 完整提交。
    Force {
        #[arg(value_parser = crate::parse_f32)]
        n: f32,
    },
    /// 显式提交阻抗目标：平衡位置 mm、刚度 N/mm、阻尼 N·s/mm。
    Impedance {
        #[arg(value_parser = crate::parse_f32)]
        equilibrium_mm: f32,
        #[arg(value_parser = crate::parse_f32)]
        stiffness_n_per_mm: f32,
        #[arg(value_parser = crate::parse_f32)]
        damping_ns_per_mm: f32,
    },
    /// 在同一会话内显式启停心跳和控制，进行受启动配置限值约束的短时速度试验。
    VelocityTest {
        /// 目标速度，单位 mm/s；必须为非零有限值，且不超过实际启动配置的速度软幅值。
        #[arg(value_parser = crate::parse_f32)]
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
    /// 提交完整本地 JSON，请求设备保存并重启；只报告本地提交。
    SaveAndRestart {
        /// 待保存的完整配置 JSON 文件。
        file: PathBuf,
    },
    /// 读取当前设备出厂记录后提交保存并重启；只报告本地提交。
    RestoreFactoryAndRestart,
}

pub fn run(command: DeviceCommand) -> Result<(), String> {
    if command.via_service.is_some() {
        return run_via_service(command);
    }
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
    let request = connection_request(&command)?;
    let selected_usb = matches!(request, ConnectionRequest::Usb { .. });
    // owner 先声明，Client 后声明；所有错误返回均先关闭 Backend 再释放 USB owner。
    let (_owner, backend) = open_backend(&request)?;
    let mut client = Client::new(backend);
    let identity = client.identify(None, &wait).map_err(format_sdk_error)?;
    print_reply("固件 Identity 回复", &identity)?;
    verify_expected_uid(&request, &identity)?;
    velocity_test(&mut client, &wait, selected_usb, mm_s, seconds)?;
    client.close();
    Ok(())
}

fn run_via_service(command: DeviceCommand) -> Result<(), String> {
    use crate::service_client::ReadAction;
    let action = match &command.action {
        Action::Status => ReadAction::Status,
        Action::Measurements => ReadAction::Measurements,
        Action::Diagnostics => ReadAction::Diagnostics,
        Action::ConfigRead { view, output: None } => match view.as_str() {
            "startup" => ReadAction::ConfigStartup,
            "user" => ReadAction::ConfigUser,
            _ => return Err("unsupported: P1 共享服务只支持 startup/user 配置读取".into()),
        },
        Action::ConfigRead {
            output: Some(_), ..
        } => {
            return Err(
                "unsupported: P1 共享服务 config-read 输出原生 JSON；尚不支持 --output".into(),
            );
        }
        _ => {
            return Err(
                "unsupported: P1 共享服务只提供 status、measurements、diagnostics、config-read"
                    .into(),
            );
        }
    };
    let text = crate::service_client::read(
        command.via_service.as_deref().expect("显式共享服务分支"),
        command
            .target_id
            .as_deref()
            .ok_or("bad_request: --via-service 必须带 --target-id")?,
        action,
        Duration::from_secs(command.timeout_secs),
    )?;
    println!("{text}");
    Ok(())
}

fn run_session(command: DeviceCommand) -> Result<(), String> {
    let request = connection_request(&command)?;
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
        Action::SaveAndRestart { file } => SessionCommand::ConfigSaveAndRestart {
            record: std::fs::read_to_string(&file)
                .map_err(|error| format!("配置文件读取失败 `{}`: {error}", file.display()))?,
        },
        Action::RestoreFactoryAndRestart => SessionCommand::RestoreFactoryAndRestart,
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

pub fn run_connection(command: ConnectionCommand) -> Result<(), String> {
    match command.action {
        ConnectionAction::Adapters { can_python } => {
            let python = crate::runtime::python(can_python.as_deref())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::connections::adapters(&python)?)
                    .map_err(|error| error.to_string())?
            );
        }
        ConnectionAction::List => {
            println!(
                "{}",
                serde_json::to_string_pretty(&ConnectionStore::load_default().response())
                    .map_err(|error| error.to_string())?
            );
        }
        ConnectionAction::Delete { id } => {
            let mut store = ConnectionStore::load_default();
            store.delete(&id)?;
            println!("已删除连接档案：{id}");
        }
        ConnectionAction::Connect {
            id,
            can_python,
            timeout_secs,
        } => {
            let request = saved_connection_request(&id, can_python)?;
            let mut session = ToolSession::new().with_timeout(Duration::from_secs(timeout_secs));
            let snapshot = session
                .connect(request)
                .map_err(|error| error.to_string())?;
            print_result(
                false,
                &serde_json::to_value(snapshot).map_err(|error| error.to_string())?,
            )?;
        }
        ConnectionAction::Save {
            id,
            name,
            serial,
            node,
            mode,
            default,
            autoconnect,
            can_python,
        } => {
            let mut store = ConnectionStore::load_default();
            let existing = store.profile_optional(&id)?;
            let expected_uid = existing
                .as_ref()
                .and_then(|profile| profile.expected_uid.clone());
            let mut profile = ConnectionProfile {
                id,
                name,
                adapter: ManagedAdapter {
                    kind: "canable2".into(),
                    serial: serial.clone(),
                },
                node,
                mode: mode.clone(),
                expected_uid,
                autoconnect,
                default,
            };
            profile.validate()?;
            let mut session = ToolSession::new();
            let snapshot = session
                .connect(ConnectionRequest::ManagedCan {
                    serial: serial.clone(),
                    node,
                    mode: mode.clone(),
                    python: can_python,
                    expected_uid: profile.expected_uid.clone(),
                })
                .map_err(|error| error.to_string())?;
            let uid = snapshot
                .identity
                .as_ref()
                .and_then(|identity| identity.get("uid"))
                .and_then(serde_json::Value::as_str)
                .ok_or("Identity 回复没有 EHA UID，不能保存连接档案")?;
            profile.expected_uid = Some(uid.to_ascii_lowercase());
            let profile = store.save(profile)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&profile).map_err(|error| error.to_string())?
            );
        }
    }
    Ok(())
}

pub(crate) fn saved_connection_request(
    id: &str,
    python: Option<String>,
) -> Result<ConnectionRequest, String> {
    let profile = ConnectionStore::load_default().profile(id)?;
    Ok(ConnectionRequest::ManagedCan {
        serial: profile.adapter.serial,
        node: profile.node,
        mode: profile.mode,
        python,
        expected_uid: profile.expected_uid,
    })
}

fn connection_request(command: &DeviceCommand) -> Result<ConnectionRequest, String> {
    match (&command.usb, &command.can_channel, &command.connection) {
        (Some(serial), None, None) => Ok(ConnectionRequest::Usb {
            serial: serial.clone(),
        }),
        (None, Some(channel), None) => Ok(ConnectionRequest::Can {
            channel: channel.clone(),
            node: command.node.ok_or("CAN 通路需要 --node")?,
            mode: command.can_mode.clone(),
            python: command.can_python.clone(),
        }),
        (None, None, Some(id)) => saved_connection_request(id, None),
        (None, None, None) => {
            Err("请选择 --usb SERIAL、--can-channel CONTEXT --node N 或 --connection ID".into())
        }
        _ => Err("USB、外部 CAN 上下文和受管连接档案不能同时选择".into()),
    }
}

fn open_backend(
    request: &ConnectionRequest,
) -> Result<
    (
        Option<crate::owner_lock::OwnerGuard>,
        eha_sdk::host::backend::Backend,
    ),
    String,
> {
    let owner = match request {
        ConnectionRequest::Usb { serial } => {
            let key =
                crate::owner_lock::ResourceKey::usb(serial).map_err(|error| error.to_string())?;
            Some(crate::owner_lock::OwnerGuard::acquire(&key).map_err(|error| error.to_string())?)
        }
        _ => None,
    };
    let backend = match request {
        ConnectionRequest::Usb { serial } => usb::open(serial),
        ConnectionRequest::Can {
            channel,
            node,
            mode,
            python,
        } => crate::session::open_can(channel.clone(), *node, mode, python.clone()),
        ConnectionRequest::ManagedCan {
            serial,
            node,
            mode,
            python,
            ..
        } => open_can_backend(
            CanOptions {
                node: *node,
                mode: crate::session::parse_can_mode(mode).map_err(|error| error.to_string())?,
            },
            crate::connections::managed_factory(
                serial.clone(),
                crate::runtime::python(python.as_deref())?,
            ),
        ),
    }?;
    Ok((owner, backend))
}

fn verify_expected_uid(request: &ConnectionRequest, identity: &Reply) -> Result<(), String> {
    let Response::Identity(identity) = identity.response().map_err(format_sdk_error)? else {
        return Err("Identity 请求没有收到 Identity 回复".into());
    };
    let uid: String = identity
        .fields()
        .uid
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    crate::connections::expected_uid_matches(
        request.expected_uid(),
        &serde_json::json!({"uid": uid}),
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
            println!(
                "decision_age_us={}; device_evidence_age_us={}; overflow={}; entries={entries:#?}",
                value.decision_age_us(),
                value.device_evidence_age_us(),
                value.overflow()
            );
        }
        Response::ConfigData(value) => print_config_data(value),
        Response::DataUnavailable(value) => println!("{:#?}", value.fields()),
    }
    Ok(())
}

fn print_config_data(value: eha_sdk::protocol::ConfigData<'_>) {
    let fields = value.fields();
    println!("字段：{fields:#?}");
    let data = value.data();
    match std::str::from_utf8(data) {
        Ok(text) => println!("实际配置原文：\n{text}"),
        Err(_) => println!("实际配置不是 UTF-8；原始字节（hex）：{}", hex_bytes(data)),
    }
}

fn format_sdk_error(error: eha_sdk::host::Error) -> String {
    format!("SDK 操作失败证据：{error:#?}")
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_shot_usb_busy_fails_before_device_open() {
        let serial = format!("eha-device-busy-{}", std::process::id());
        let key = crate::owner_lock::ResourceKey::usb(&serial).expect("key");
        let _owner = crate::owner_lock::OwnerGuard::acquire(&key).expect("先占用");
        let error = super::open_backend(&super::ConnectionRequest::Usb { serial })
            .err()
            .expect("竞争失败不打开 USB");
        assert!(error.starts_with("owner_busy:"));
    }
    #[test]
    fn cli_control_parses_decimal_once_and_rejects_loss_before_device_io() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            device: DeviceCommand,
        }
        let decimal = "1.0000000596046447753906250000000000000000000001";
        let cli = <TestCli as clap::Parser>::try_parse_from([
            "test", "--usb", "missing", "position", decimal,
        ])
        .expect("CLI 原文");
        let mm = match cli.device.action {
            Action::Position { mm } => mm,
            _ => f32::NAN,
        };
        assert_eq!(mm.to_bits(), 0x3f800001);
        for token in ["1e-999", "NaN", "1e100"] {
            assert!(
                <TestCli as clap::Parser>::try_parse_from([
                    "test", "--usb", "missing", "position", token
                ])
                .is_err()
            );
        }
    }

    use super::{Action, DeviceCommand, run};

    fn missing_usb_velocity_test(mm_s: f32) -> DeviceCommand {
        DeviceCommand {
            json: false,
            via_service: None,
            target_id: None,
            usb: Some("missing-usb-serial".into()),
            can_channel: None,
            connection: None,
            node: None,
            can_mode: "fd".into(),
            can_python: None,
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

    #[test]
    fn saved_connection_cannot_be_mixed_with_legacy_can_context() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            device: DeviceCommand,
        }
        assert!(
            <TestCli as clap::Parser>::try_parse_from([
                "test",
                "--connection",
                "bench",
                "--can-channel",
                "legacy",
                "status",
            ])
            .is_err()
        );
    }

    #[test]
    fn shared_service_cli_requires_explicit_target_and_rejects_direct_parameters() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            device: DeviceCommand,
        }
        use clap::{CommandFactory, Parser};
        let help = TestCli::command().render_long_help().to_string();
        assert!(help.contains("--via-service"));
        assert!(help.contains("--target-id"));
        let base = [
            "test",
            "--via-service",
            "/private/instance.json",
            "--target-id",
            "can:7",
        ];
        for action in ["status", "measurements", "diagnostics", "config-read"] {
            let mut args = base.to_vec();
            args.push(action);
            TestCli::try_parse_from(args).expect("显式共享只读命令");
        }
        assert!(
            TestCli::try_parse_from(["test", "--via-service", "/private/instance.json", "status"])
                .is_err()
        );
        assert!(TestCli::try_parse_from(["test", "--target-id", "can:7", "status"]).is_err());
        for (flag, value) in [
            ("--usb", "serial"),
            ("--can-channel", "can0"),
            ("--connection", "bench"),
            ("--node", "7"),
            ("--can-mode", "classic"),
            ("--can-python", "/usr/bin/python3"),
        ] {
            let mut args = base.to_vec();
            args.extend([flag, value, "status"]);
            assert!(TestCli::try_parse_from(args).is_err(), "{flag}");
        }
    }

    #[test]
    fn shared_service_unsupported_and_connection_failure_never_enter_direct_path() {
        #[derive(clap::Parser)]
        struct TestCli {
            #[command(flatten)]
            device: DeviceCommand,
        }
        use clap::Parser;
        let base = [
            "test",
            "--via-service",
            "/missing/eha-service-instance.json",
            "--target-id",
            "can:7",
        ];
        for action in [
            "stop",
            "reset-application",
            "enter-update",
            "restore-factory-and-restart",
        ] {
            let mut args = base.to_vec();
            args.push(action);
            assert!(
                run(TestCli::try_parse_from(args).expect("parse").device)
                    .expect_err("unsupported")
                    .starts_with("unsupported:")
            );
        }
        let mut args = base.to_vec();
        args.extend(["config-read", "--view", "factory"]);
        assert!(
            run(TestCli::try_parse_from(args).expect("parse").device)
                .expect_err("unsupported")
                .starts_with("unsupported:")
        );
        let mut args = base.to_vec();
        args.push("status");
        // 若进入默认直连路径会报未选择USB/CAN；此处只允许明确服务文件错误。
        assert!(
            run(TestCli::try_parse_from(args).expect("parse").device)
                .expect_err("missing descriptor")
                .starts_with("private_file_invalid:")
        );
    }
}
