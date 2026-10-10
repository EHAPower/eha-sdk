// Copyright The eha-sdk Contributors

//! 持久桌面工具会话。
//!
//! 本模块只把 SDK 的单一 `Client` 会话提供给 CLI、Shell 和 WebUI。它不重新实现
//! 协议、传输或控制规则；关闭和通信恢复都保留 SDK 的编号与维护关联，也绝不发送 Stop
//! 或重放任何动作。

use std::{
    collections::VecDeque,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use eha_sdk::{
    can::{
        CanChannelFactory, CanConnector, CanNodeConnector, CanOptions, python::PythonCanOptions,
    },
    host::{
        Client, ClientState, Error as SdkError, Failure, LocalSubmission, OperationSubmission,
        Reply, Wait,
    },
    protocol::{
        Response,
        responses::{ConfigView, ValueFields},
    },
    session::MaintenanceKey,
    usb::UsbConnector,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[path = "recording.rs"]
mod recording;
#[path = "trial.rs"]
mod trial;

use recording::Recording;
use trial::{ActiveTrial, InterruptedTrial, PreparedTrial, TrialState};
#[allow(unused_imports)]
pub use trial::{ReachCondition, TrialEnvelope, TrialRequest};

/// Web、Shell 和单次 CLI 共用的明确连接选择。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum ConnectionRequest {
    Usb {
        serial: String,
    },
    Can {
        channel: String,
        node: u8,
        mode: String,
        python: Option<String>,
    },
}

impl ConnectionRequest {
    /// 从已验重的顶层请求字段读取连接选择，不把路由整数转换为浮点表示。
    pub(crate) fn from_fields(fields: &crate::RawFields) -> Result<Self, String> {
        match fields.string("transport")?.as_str() {
            "usb" => Ok(Self::Usb {
                serial: fields.string("serial")?,
            }),
            "can" => Ok(Self::Can {
                channel: fields.string("channel")?,
                node: fields.decode_field("node")?,
                mode: fields.string("mode")?,
                python: fields.optional_string("python")?,
            }),
            transport => Err(format!("未知 transport：{transport}")),
        }
    }
}

/// 一个会话内可执行的业务动作。所有控制动作的成功只表示 SDK 的本地完整提交。
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Command {
    Status,
    Measurements,
    Diagnostics,
    /// 只读取 SDK 缓存的最新周期遥测，绝不发查询。
    Telemetry,
    HeartbeatStart,
    HeartbeatStop,
    HeartbeatOnce,
    Position {
        mm: f32,
    },
    Velocity {
        mm_s: f32,
    },
    Force {
        n: f32,
    },
    Impedance {
        equilibrium_mm: f32,
        stiffness_n_per_mm: f32,
        damping_ns_per_mm: f32,
    },
    Stop,
    ConfigRead {
        view: String,
    },
    /// 仅校验 JSON，不要求已连接，也不访问设备。
    ConfigValidate {
        record: String,
    },
    ConfigSave {
        record: String,
    },
    RestoreFactory,
    MaintenanceResult {
        operation_key: String,
    },
    MaintenanceRelease {
        operation_key: String,
    },
    ResetApplication,
    EnterUpdate,
}

impl Command {
    /// 由已检查字段重复的原文表一次解码动作；业务浮点只通过共享 binary32 入口量化。
    pub(crate) fn from_fields(fields: &crate::RawFields) -> Result<Self, String> {
        let action = fields.string("action")?;
        Ok(match action.as_str() {
            "status" => Self::Status,
            "measurements" => Self::Measurements,
            "diagnostics" => Self::Diagnostics,
            "telemetry" => Self::Telemetry,
            "heartbeat_start" => Self::HeartbeatStart,
            "heartbeat_stop" => Self::HeartbeatStop,
            "heartbeat_once" => Self::HeartbeatOnce,
            "position" => Self::Position {
                mm: fields.decode_f32_field("mm")?,
            },
            "velocity" => Self::Velocity {
                mm_s: fields.decode_f32_field("mm_s")?,
            },
            "force" => Self::Force {
                n: fields.decode_f32_field("n")?,
            },
            "impedance" => Self::Impedance {
                equilibrium_mm: fields.decode_f32_field("equilibrium_mm")?,
                stiffness_n_per_mm: fields.decode_f32_field("stiffness_n_per_mm")?,
                damping_ns_per_mm: fields.decode_f32_field("damping_ns_per_mm")?,
            },
            "stop" => Self::Stop,
            "config_read" => Self::ConfigRead {
                view: fields.decode_field("view")?,
            },
            "config_validate" => Self::ConfigValidate {
                record: fields.decode_field("record")?,
            },
            "config_save" => Self::ConfigSave {
                record: fields.decode_field("record")?,
            },
            "restore_factory" => Self::RestoreFactory,
            "maintenance_result" => Self::MaintenanceResult {
                operation_key: fields.decode_field("operation_key")?,
            },
            "maintenance_release" => Self::MaintenanceRelease {
                operation_key: fields.decode_field("operation_key")?,
            },
            "reset_application" => Self::ResetApplication,
            "enter_update" => Self::EnterUpdate,
            _ => return Err(format!("未知 action：{action}")),
        })
    }
}

/// 嵌套输入直接建立唯一字段表后复用同一动作解码，避免 serde internally-tagged
/// enum 的通用数值缓存路径。
impl<'de> Deserialize<'de> for Command {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = crate::RawFields::deserialize(deserializer)?;
        Self::from_fields(&fields).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionError {
    pub kind: Box<str>,
    pub message: Box<str>,
    pub local_stage: Option<Box<str>>,
    pub operation_key: Option<Box<str>>,
    pub unknown: bool,
    /// 本次失败携带的完整协议事实；它不等同于本地提交或设备执行结论。
    pub failure: Option<Box<Value>>,
    pub last_result: Option<Box<Value>>,
    pub observed_transport: Option<Box<Value>>,
    /// 仅本次 `Failure::Transport` 的原始字节，与后续观察到的传输状态分开保留。
    pub transport_raw_hex: Option<Box<str>>,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_string(self) {
            Ok(json) => formatter.write_str(&json),
            Err(_) => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for SessionError {}

/// 一个动作的结构化事实。`data` 是固件回复或本地提交事实，不是设备执行结论。
#[derive(Clone, Debug, Serialize)]
pub struct CommandResult {
    pub action: String,
    pub local_submission: Option<LocalSubmissionSnapshot>,
    pub operation_key: Option<String>,
    pub data: Value,
    pub snapshot: Snapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalSubmissionSnapshot {
    pub id: u64,
    pub boundary: String,
}

/// 页面和脚本均可直接序列化的当前本地会话观察。
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub connected: bool,
    pub connection: Option<ConnectionRequest>,
    pub identity: Option<Value>,
    pub heartbeat: Value,
    pub transport: Value,
    pub adapter: Value,
    pub telemetry: Option<Value>,
    pub last_status: Option<Value>,
    pub last_measurements: Option<Value>,
    pub last_diagnostics: Option<Value>,
    pub pending_operation_key: Option<String>,
    pub last_operation: Option<Value>,
}

const TELEMETRY_TREND_WINDOW_US: u64 = 30_000_000;
const TELEMETRY_TREND_MAX_POINTS: usize = 3_000;

/// 图表所需的最小遥测投影。它保留原值及质量事实，不将不可用或旧值伪装成零值。
#[derive(Clone, Debug, Serialize)]
struct TelemetryTrendPoint {
    cursor: u64,
    time_us: u64,
    sequence: u32,
    position: Value,
    velocity: Value,
    force: Value,
    target_mode: u8,
    target_values: [f32; 3],
    gap: bool,
}

/// WebUI 的当前会话趋势窗口；它不进入 SDK 的通用 latest-only 默认路径。
struct TelemetryTrend {
    source: Option<Value>,
    generation_start_cursor: u64,
    next_cursor: u64,
    points: VecDeque<TelemetryTrendPoint>,
    latest_time_us: Option<u64>,
    dropped: u64,
    pending_gap: bool,
}

impl TelemetryTrend {
    fn new(_queue_capacity: usize) -> Self {
        Self {
            source: None,
            generation_start_cursor: 1,
            next_cursor: 0,
            points: VecDeque::new(),
            latest_time_us: None,
            dropped: 0,
            pending_gap: false,
        }
    }

    fn reset(&mut self) {
        self.source = None;
        self.generation_start_cursor = self.next_cursor.saturating_add(1);
        self.points.clear();
        self.latest_time_us = None;
        self.dropped = 0;
        self.pending_gap = false;
    }

    fn set_source(&mut self, source: Option<Value>) {
        if self.source == source {
            return;
        }
        self.source = source;
        self.generation_start_cursor = self.next_cursor.saturating_add(1);
        self.points.clear();
        self.latest_time_us = None;
        self.dropped = 0;
        self.pending_gap = false;
    }

    fn record(&mut self, replies: Vec<Reply>, dropped: u64) {
        if dropped != 0 {
            self.dropped = self.dropped.saturating_add(dropped);
            self.pending_gap = true;
        }
        for reply in replies {
            let Ok(Response::Telemetry(response)) = reply.response() else {
                continue;
            };
            let fields = response.fields();
            let time_us = fields.sample.snapshot_time_us;
            if self.latest_time_us.is_some_and(|latest| time_us <= latest) {
                continue;
            }
            self.next_cursor = self.next_cursor.saturating_add(1);
            self.latest_time_us = Some(
                self.latest_time_us
                    .map_or(time_us, |latest| latest.max(time_us)),
            );
            self.points.push_back(TelemetryTrendPoint {
                cursor: self.next_cursor,
                time_us,
                sequence: fields.sample.snapshot_sequence,
                position: trend_value_json(fields.main_values[0]),
                velocity: trend_value_json(fields.main_values[1]),
                force: trend_value_json(fields.main_values[4]),
                target_mode: fields.target_mode as u8,
                target_values: fields.target_values,
                gap: std::mem::take(&mut self.pending_gap),
            });
            let earliest_time = self
                .latest_time_us
                .unwrap_or(time_us)
                .saturating_sub(TELEMETRY_TREND_WINDOW_US);
            while self.points.len() > TELEMETRY_TREND_MAX_POINTS
                || self
                    .points
                    .front()
                    .is_some_and(|point| point.time_us < earliest_time)
            {
                self.points.pop_front();
            }
        }
    }

    fn json(&self, after: Option<u64>) -> Value {
        let newest = self
            .points
            .back()
            .map_or(self.next_cursor, |point| point.cursor);
        let oldest = self.points.front().map_or(0, |point| point.cursor);
        let source_changed = after.is_some_and(|cursor| cursor < self.generation_start_cursor);
        let cursor_too_old =
            after.is_some_and(|cursor| oldest != 0 && cursor.saturating_add(1) < oldest);
        let cursor_ahead = after.is_some_and(|cursor| cursor > newest);
        let reset =
            after.is_none() || after == Some(0) || source_changed || cursor_too_old || cursor_ahead;
        let points: Vec<_> = if reset {
            self.points.iter().cloned().collect()
        } else {
            self.points
                .iter()
                .filter(|point| after.is_some_and(|cursor| point.cursor > cursor))
                .cloned()
                .collect()
        };
        let source = self.source.as_ref().map_or(Value::Null, |source| {
            let mut source = source.clone();
            source["generation"] = json!(self.generation_start_cursor);
            source
        });
        json!({
            "source": source,
            "reset": reset,
            "truncated": cursor_too_old,
            "points": points,
            "cursor": newest,
            "dropped": self.dropped,
        })
    }
}

enum Connector {
    Usb(UsbConnector),
    Can(CanConnector),
    SharedCan(CanNodeConnector),
    #[cfg(test)]
    Test(Box<dyn FnMut() -> Result<eha_sdk::host::backend::Backend, String>>),
}

impl Connector {
    fn open(&mut self) -> Result<eha_sdk::host::backend::Backend, String> {
        match self {
            Self::Usb(connector) => connector.open(),
            Self::Can(connector) => connector.open(),
            Self::SharedCan(connector) => connector.open(),
            #[cfg(test)]
            Self::Test(open) => open(),
        }
    }
}

/// 一个独占、串行的 SDK 客户端会话。调用方以 `&mut self` 执行操作，因此不会并发发送。
pub struct ToolSession {
    connector: Option<Connector>,
    request: Option<ConnectionRequest>,
    client: Option<Client>,
    disconnected: Option<ClientState>,
    timeout: Duration,
    identity: Option<Reply>,
    last_telemetry: Option<Reply>,
    last_status: Option<Reply>,
    last_measurements: Option<Reply>,
    last_diagnostics: Option<Reply>,
    last_operation: Option<Value>,
    disconnected_reason: Option<String>,
    disconnected_pending_operation: Option<String>,
    telemetry_trend: Option<TelemetryTrend>,
    telemetry_queue_capacity: Option<usize>,
    recording: Option<Recording>,
    recording_error: Option<String>,
    trial: TrialState,
}

impl Default for ToolSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSession {
    pub fn new() -> Self {
        Self {
            connector: None,
            request: None,
            client: None,
            disconnected: None,
            timeout: Duration::from_secs(5),
            identity: None,
            last_telemetry: None,
            last_status: None,
            last_measurements: None,
            last_diagnostics: None,
            last_operation: None,
            disconnected_reason: None,
            disconnected_pending_operation: None,
            telemetry_trend: None,
            telemetry_queue_capacity: None,
            recording: None,
            recording_error: None,
            trial: TrialState::default(),
        }
    }

    /// 为 WebUI 等连续展示显式保留被动接收的有限遥测窗口。
    /// 普通 CLI 与 Shell 不调用此构造器，仍保持 SDK 的 latest-only 默认开销。
    pub fn with_telemetry_trend(mut self, queue_capacity: usize) -> Self {
        if queue_capacity != 0 {
            self.telemetry_trend = Some(TelemetryTrend::new(queue_capacity));
            self.telemetry_queue_capacity = Some(queue_capacity);
        }
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 打开、核对 Identity 并保留同一 Client，期间不启动心跳或发送控制。
    pub fn connect(&mut self, request: ConnectionRequest) -> Result<Snapshot, SessionError> {
        if self.trial.active.is_some() {
            return Err(invalid("试验进行中；拒绝切换连接"));
        }
        if self.client.is_some() {
            return Err(invalid("已有活跃会话；先显式 disconnect"));
        }
        if self.disconnected.is_some() && self.request.as_ref() == Some(&request) {
            return self.reconnect();
        }
        self.clear_observations();
        self.last_operation = None;
        let mut connector = connector_for(&request)?;
        let backend = connector.open().map_err(open_error)?;
        let mut client = Client::new(backend);
        if let Some(capacity) = self.telemetry_queue_capacity {
            client.enable_telemetry_queue(capacity).map_err(sdk_error)?;
        }
        let identity = match client.identify(None, &self.wait()) {
            Ok(identity) => identity,
            Err(error) => {
                self.disconnected_reason = client.connection_status().disconnected;
                self.disconnected_pending_operation = client.pending_operation().map(key_hex);
                self.request = Some(request);
                self.connector = Some(connector);
                self.disconnected = Some(client.disconnect());
                return Err(sdk_error(error));
            }
        };
        self.identity = Some(identity);
        self.reconcile_interrupted_trial();
        self.request = Some(request);
        self.connector = Some(connector);
        self.client = Some(client);
        self.disconnected = None;
        self.disconnected_reason = None;
        self.disconnected_pending_operation = None;
        Ok(self.snapshot())
    }

    /// 绑定到已建立的共享 CAN 网络中的一个节点；网络本身由调用方唯一拥有。
    pub fn connect_shared_can(
        &mut self,
        request: ConnectionRequest,
        connector: CanNodeConnector,
    ) -> Result<Snapshot, SessionError> {
        if self.trial.active.is_some() {
            return Err(invalid("试验进行中；拒绝切换连接"));
        }
        if !matches!(request, ConnectionRequest::Can { .. }) {
            return Err(invalid("共享 CAN 节点必须使用 CAN 连接请求"));
        }
        if self.client.is_some() {
            return Err(invalid("已有活跃会话；先显式 disconnect"));
        }
        self.clear_observations();
        self.last_operation = None;
        let mut shared = Connector::SharedCan(connector);
        let backend = shared.open().map_err(open_error)?;
        let mut client = Client::new(backend);
        if let Some(capacity) = self.telemetry_queue_capacity {
            client.enable_telemetry_queue(capacity).map_err(sdk_error)?;
        }
        let identity = match client.identify(None, &self.wait()) {
            Ok(identity) => identity,
            Err(error) => {
                self.disconnected_reason = client.connection_status().disconnected;
                self.disconnected_pending_operation = client.pending_operation().map(key_hex);
                self.request = Some(request);
                self.connector = Some(shared);
                self.disconnected = Some(client.disconnect());
                return Err(sdk_error(error));
            }
        };
        self.identity = Some(identity);
        self.reconcile_interrupted_trial();
        self.request = Some(request);
        self.connector = Some(shared);
        self.client = Some(client);
        self.disconnected = None;
        self.disconnected_reason = None;
        self.disconnected_pending_operation = None;
        Ok(self.snapshot())
    }

    /// 只关闭本地 I/O 并保存 ClientState；不发送 Stop、Reset 或重放目标。
    pub fn disconnect(&mut self) -> Result<Snapshot, SessionError> {
        if self.trial.active.is_some() {
            return Err(invalid("试验进行中；普通 disconnect 不停止也不切换设备"));
        }
        self.finish_recording_boundary("disconnect");
        self.mark_interrupted_reconnect_required();
        self.trial.prepared = None;
        let client = self
            .client
            .take()
            .ok_or_else(|| invalid("当前没有活跃会话"))?;
        self.capture_telemetry(&client);
        self.disconnected_reason = client.connection_status().disconnected;
        self.disconnected_pending_operation = client.pending_operation().map(key_hex);
        self.disconnected = Some(client.disconnect());
        Ok(self.snapshot())
    }

    /// 在同一个 Connector 上恢复传输编号和业务关联，随后重新核对 Identity。
    pub fn reconnect(&mut self) -> Result<Snapshot, SessionError> {
        if self
            .client
            .as_ref()
            .is_some_and(|client| client.connection_status().disconnected.is_none())
        {
            return Err(invalid("会话仍处于连接状态；无需 reconnect"));
        }
        if self.trial.active.is_some() {
            self.interrupt_trial_unknown("reconnect_during_active_trial");
        }
        if let Some(client) = self.client.take() {
            self.capture_telemetry(&client);
            self.disconnected_reason = client.connection_status().disconnected;
            self.disconnected_pending_operation = client.pending_operation().map(key_hex);
            self.disconnected = Some(client.disconnect());
        }
        let previous = self.identity.as_ref().and_then(identity_facts);
        let state = self
            .disconnected
            .take()
            .ok_or_else(|| invalid("没有可恢复的断开会话"))?;
        let connector = self
            .connector
            .as_mut()
            .ok_or_else(|| invalid("没有连接器"))?;
        let backend = match connector.open() {
            Ok(backend) => backend,
            Err(error) => {
                self.disconnected = Some(state);
                return Err(open_error(error));
            }
        };
        let mut client = Client::resume(backend, state);
        if let Some(capacity) = self.telemetry_queue_capacity {
            client.enable_telemetry_queue(capacity).map_err(sdk_error)?;
        }
        let identity = match client.identify(None, &self.wait()) {
            Ok(identity) => identity,
            Err(error) => {
                self.disconnected = Some(client.disconnect());
                return Err(sdk_error(error));
            }
        };
        if previous.is_some_and(|previous| {
            identity_facts(&identity).is_some_and(|current| {
                previous.uid != current.uid || previous.run_nonce != current.run_nonce
            })
        }) {
            self.finish_recording_boundary("reconnect_identity_changed");
        }
        self.clear_observations();
        self.identity = Some(identity);
        self.reconcile_interrupted_trial();
        self.client = Some(client);
        self.disconnected_reason = None;
        self.disconnected_pending_operation = None;
        Ok(self.snapshot())
    }

    /// 在当前已连接通路上重新读取 Identity，供 WebUI 在新建维护键前刷新操作高水位。
    ///
    /// 同一 UID 与运行实例只更新 SDK 的维护编号下界，连续遥测历史保持不变；同一 UID
    /// 的新运行实例会清除旧运行的观测和趋势。刷新失败时主动把未核对的 `Client` 转为
    /// 可恢复的断开状态：它不再能执行业务操作，调用方可显式 `reconnect` 后重新核对。
    /// 此入口不由普通 CLI/Shell 的 [`Self::execute`] 隐式调用。
    pub fn refresh_identity(&mut self) -> Result<Snapshot, SessionError> {
        let mut client = self
            .client
            .take()
            .ok_or_else(|| invalid("请先 connect 并核对 Identity"))?;
        let previous = self.identity.as_ref().and_then(identity_facts);
        let identity = match client.identify(None, &self.wait()) {
            Ok(identity) => identity,
            Err(error) => {
                self.disconnected_reason = Some(format!("维护前身份刷新失败：{error}"));
                self.disconnected_pending_operation = client.pending_operation().map(key_hex);
                self.disconnected = Some(client.disconnect());
                self.clear_observations();
                return Err(sdk_error(error));
            }
        };
        let current = identity_facts(&identity).ok_or_else(|| invalid("Identity 回复无效"))?;
        if previous.is_some_and(|previous| {
            previous.uid != current.uid || previous.run_nonce != current.run_nonce
        }) {
            self.interrupt_trial_unknown("identity_changed");
            self.finish_recording_boundary("identity_changed");
            self.clear_observations();
        }
        self.identity = Some(identity);
        self.reconcile_interrupted_trial();
        self.client = Some(client);
        self.disconnected = None;
        self.disconnected_reason = None;
        self.disconnected_pending_operation = None;
        let pending = self.client.as_ref().and_then(Client::pending_operation);
        if let Some(key) = pending
            && key.uid == current.uid
            && key.run_nonce == current.run_nonce
            && current.retained_operation_id == 0
        {
            // 另一通路可能已经释放该键。只接受固件对同一键返回的 0x0306；任何其他
            // 读取结果或错误都保留 SDK 的 pending 门禁，绝不据 Identity 猜测清除。
            let wait = self.wait();
            self.client
                .as_mut()
                .expect("connected Client was just stored")
                .confirm_released(key, &wait)
                .map_err(sdk_error)?;
        }
        Ok(self.snapshot())
    }

    pub fn snapshot(&mut self) -> Snapshot {
        if let Some(client) = self.client.take() {
            self.capture_telemetry(&client);
            let (heartbeat, transport, adapter, connection_status, pending_operation_key) = (
                client.heartbeat_status(),
                client.transport_status(),
                client.adapter_status(),
                client.connection_status(),
                client.pending_operation().map(key_hex),
            );
            let snapshot = Snapshot {
                connected: true,
                connection: self.request.clone(),
                identity: self
                    .identity
                    .as_ref()
                    .and_then(|reply| reply_json(reply).ok()),
                heartbeat: json!({
                    "enabled": heartbeat.enabled,
                    "submitted": heartbeat.submitted,
                    "missed": heartbeat.missed,
                    "error": heartbeat.error,
                }),
                transport: transport_json(&transport, connection_status.disconnected),
                adapter: json!({"notices": adapter.notices, "last_notice": adapter.last_notice.as_ref().map(|(detail, raw, _)| json!({"detail": detail, "raw_hex": hex(raw)}))}),
                telemetry: self
                    .last_telemetry
                    .as_ref()
                    .and_then(|reply| reply_json(reply).ok()),
                last_status: self
                    .last_status
                    .as_ref()
                    .and_then(|reply| reply_json(reply).ok()),
                last_measurements: self
                    .last_measurements
                    .as_ref()
                    .and_then(|reply| reply_json(reply).ok()),
                last_diagnostics: self
                    .last_diagnostics
                    .as_ref()
                    .and_then(|reply| reply_json(reply).ok()),
                pending_operation_key,
                last_operation: self.last_operation.clone(),
            };
            self.client = Some(client);
            return snapshot;
        }
        Snapshot {
            connected: false,
            connection: self.request.clone(),
            identity: self
                .identity
                .as_ref()
                .and_then(|reply| reply_json(reply).ok()),
            heartbeat: json!({"enabled": false, "submitted": 0, "missed": 0, "error": Value::Null}),
            transport: json!({"disconnected": self.disconnected_reason}),
            adapter: json!({}),
            telemetry: self
                .last_telemetry
                .as_ref()
                .and_then(|reply| reply_json(reply).ok()),
            last_status: self
                .last_status
                .as_ref()
                .and_then(|reply| reply_json(reply).ok()),
            last_measurements: self
                .last_measurements
                .as_ref()
                .and_then(|reply| reply_json(reply).ok()),
            last_diagnostics: self
                .last_diagnostics
                .as_ref()
                .and_then(|reply| reply_json(reply).ok()),
            pending_operation_key: self.disconnected_pending_operation.clone(),
            last_operation: self.last_operation.clone(),
        }
    }

    /// 执行普通 SDK 动作。试验活跃时，控制及阻塞查询必须通过试验入口协调。
    pub fn execute(&mut self, command: Command) -> Result<CommandResult, SessionError> {
        if self.trial.active.is_some()
            || self
                .trial
                .interrupted
                .as_ref()
                .is_some_and(|trial| !trial.unrecoverable)
        {
            return Err(invalid(
                "试验进行中；仅允许读取快照、显式 stop_trial 或 update_trial_position",
            ));
        }
        self.record_event("command_input", json!({"command": &command}));
        let result = self.execute_inner(command);
        match &result {
            Ok(value) => {
                if matches!(
                    value.action.as_str(),
                    "config_save" | "restore_factory" | "reset_application" | "enter_update"
                ) {
                    self.trial.prepared = None;
                }
                self.record_event(
                    "command_completed",
                    json!({
                        "action": value.action,
                        "local_submission": value.local_submission,
                        "operation_key": value.operation_key,
                        "data": value.data,
                    }),
                );
            }
            Err(error) => self.record_event("command_error", json!({"error": error})),
        }
        result
    }

    fn execute_inner(&mut self, command: Command) -> Result<CommandResult, SessionError> {
        if let Command::ConfigValidate { record } = &command {
            eha_sdk::configuration::validate_json(record.as_bytes())
                .map_err(|error| invalid(error.to_string()))?;
            let snapshot = self.snapshot();
            return Ok(CommandResult {
                action: "config_validate".into(),
                local_submission: None,
                operation_key: None,
                data: json!({"valid": true}),
                snapshot,
            });
        }

        let action = command_name(&command).to_owned();
        let wait = self.wait();
        let mut local_submission = None;
        let mut operation_key = None;
        let data = {
            let client = self
                .client
                .as_mut()
                .ok_or_else(|| invalid("请先 connect 并核对 Identity"))?;
            match command {
                Command::Status => {
                    let reply = client.status(&wait).map_err(sdk_error)?;
                    let value = reply_json(&reply)?;
                    self.last_status = Some(reply);
                    value
                }
                Command::Measurements => {
                    let reply = client.measurements(&wait).map_err(sdk_error)?;
                    let value = reply_json(&reply)?;
                    self.last_measurements = Some(reply);
                    value
                }
                Command::Diagnostics => {
                    let reply = client.diagnostics(&wait).map_err(sdk_error)?;
                    let value = reply_json(&reply)?;
                    self.last_diagnostics = Some(reply);
                    value
                }
                Command::Telemetry => client
                    .telemetry()
                    .map(|reply| reply_json(&reply))
                    .transpose()?
                    .unwrap_or(Value::Null),
                Command::HeartbeatStart => {
                    client.start_heartbeat().map_err(sdk_error)?;
                    json!({"requested": true})
                }
                Command::HeartbeatStop => {
                    client.stop_heartbeat().map_err(sdk_error)?;
                    json!({"requested": true})
                }
                Command::HeartbeatOnce => submission_data(
                    client.heartbeat(&wait).map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::Position { mm } => submission_data(
                    client.position(mm, &wait).map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::Velocity { mm_s } => submission_data(
                    client.velocity(mm_s, &wait).map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::Force { n } => submission_data(
                    client.force(n, &wait).map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::Impedance {
                    equilibrium_mm,
                    stiffness_n_per_mm,
                    damping_ns_per_mm,
                } => submission_data(
                    client
                        .impedance(equilibrium_mm, stiffness_n_per_mm, damping_ns_per_mm, &wait)
                        .map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::Stop => submission_data(
                    client.stop_control(&wait).map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::ConfigRead { view } => reply_json(
                    &client
                        .read_config(parse_view(&view)?, &wait)
                        .map_err(sdk_error)?,
                )?,
                Command::ConfigSave { record } => {
                    let saved = client
                        .save_and_readback(record.as_bytes(), &wait)
                        .map_err(sdk_error)?;
                    local_submission = Some(local_submission_json(saved.submission.local));
                    let key = key_hex(saved.submission.key);
                    operation_key = Some(key.clone());
                    let value = json!({"result": operation_fields_json(saved.result), "readback": reply_json(&saved.readback)?, "readback_matches_requested": true});
                    self.last_operation = Some(value.clone());
                    value
                }
                Command::RestoreFactory => {
                    let saved = client.restore_factory(&wait).map_err(sdk_error)?;
                    local_submission = Some(local_submission_json(saved.submission.local));
                    let key = key_hex(saved.submission.key);
                    operation_key = Some(key.clone());
                    let value = json!({"result": operation_fields_json(saved.result), "readback": reply_json(&saved.readback)?, "readback_matches_factory": true});
                    self.last_operation = Some(value.clone());
                    value
                }
                Command::MaintenanceResult { operation_key: key } => {
                    let key = parse_key(&key)?;
                    client.track_operation(key).map_err(sdk_error)?;
                    let value = reply_json(&client.read_result(key, &wait).map_err(sdk_error)?)?;
                    self.last_operation = Some(value.clone());
                    value
                }
                Command::MaintenanceRelease { operation_key: key } => {
                    let key = parse_key(&key)?;
                    client.track_operation(key).map_err(sdk_error)?;
                    let before = reply_json(&client.read_result(key, &wait).map_err(sdk_error)?)?;
                    let local = client.release_result(&wait).map_err(sdk_error)?;
                    local_submission = Some(local_submission_json(local));
                    client.confirm_released(key, &wait).map_err(sdk_error)?;
                    json!({"before_release": before, "released": true})
                }
                Command::ResetApplication => operation_submission_data(
                    client.reset_application(&wait).map_err(sdk_error)?,
                    &mut local_submission,
                    &mut operation_key,
                ),
                Command::EnterUpdate => operation_submission_data(
                    client.enter_update(&wait).map_err(sdk_error)?,
                    &mut local_submission,
                    &mut operation_key,
                ),
                Command::ConfigValidate { .. } => {
                    return Err(invalid("配置校验已在本地完成"));
                }
            }
        };
        let snapshot = self.snapshot();
        Ok(CommandResult {
            action,
            local_submission,
            operation_key,
            data,
            snapshot,
        })
    }

    /// 开始一个独立目录的原始会话记录。记录启动不发送控制、心跳或维护动作。
    pub fn start_recording(&mut self, root: &Path) -> Result<Value, SessionError> {
        if self.recording.is_some() {
            return Err(invalid("已有进行中的记录"));
        }
        if self.client.is_none() || self.identity.is_none() {
            return Err(invalid("记录需要已核对 Identity 的活跃会话"));
        }
        if self.trial.active.is_some() {
            return Err(invalid("试验进行中不读取配置启动记录；请在试验前开始记录"));
        }
        let capacity = self.telemetry_queue_capacity.unwrap_or(1_024);
        self.telemetry_queue_capacity = Some(capacity);
        self.client
            .as_mut()
            .expect("checked connected client")
            .enable_telemetry_queue(capacity)
            .map_err(sdk_error)?;
        let wait = self.wait();
        let startup = self
            .client
            .as_mut()
            .expect("checked connected client")
            .read_config(ConfigView::Startup, &wait)
            .map_err(sdk_error)
            .and_then(|reply| reply_json(&reply))?;
        let snapshot = self.snapshot();
        let identity = snapshot.identity.clone().unwrap_or(Value::Null);
        let metadata = json!({
            "format": "eha-tool-recording-v1",
            "tool_version": crate::build_metadata::TOOL_VERSION,
            "connection": snapshot.connection.clone(),
            "identity": identity.clone(),
            "device_uid": identity["uid"].clone(),
            "run_nonce": identity["sample"]["run_nonce"].clone(),
            "startup_config_source": identity["config_source"].clone(),
            "startup_config_source_label": identity["config_source_label"].clone(),
            "startup_config": startup,
        });
        let mut recording = Recording::start(root, metadata).map_err(recording_error)?;
        recording
            .event(
                "session_snapshot",
                serde_json::to_value(&snapshot)
                    .map_err(|error| recording_error(error.to_string()))?,
            )
            .map_err(recording_error)?;
        let value = recording.snapshot();
        self.recording = Some(recording);
        self.recording_error = None;
        Ok(value)
    }

    /// 结束并刷新当前记录；它不改变 SDK 连接或控制状态。
    pub fn stop_recording(&mut self) -> Result<Value, SessionError> {
        let recording = self
            .recording
            .take()
            .ok_or_else(|| invalid("当前没有进行中的记录"))?;
        recording.finish().map_err(recording_error)
    }

    pub fn recording_snapshot(&self) -> Value {
        match (&self.recording, &self.recording_error) {
            (Some(recording), Some(error)) => {
                json!({"active": false, "failed": true, "error": error, "partial": recording.snapshot()})
            }
            (Some(recording), None) => recording.snapshot(),
            (None, error) => json!({"active": false, "error": error}),
        }
    }

    /// 用本次读到的实际 Startup 配置和 Status 建立试验准入，不提交控制需求。
    pub fn prepare_trial(&mut self, request: &TrialRequest) -> Result<(), SessionError> {
        if self.trial.active.is_some() {
            return Err(invalid("已有进行中的试验"));
        }
        if self
            .trial
            .interrupted
            .as_ref()
            .is_some_and(|trial| !trial.unrecoverable)
        {
            return Err(invalid(
                "上次试验中断且结果未知；重连并核对同一运行实例后请显式 Stop",
            ));
        }
        self.trial.interrupted = None;
        validate_trial_request(request)?;
        let wait = self.wait();
        let (startup_reply, status_reply) = {
            let client = self
                .client
                .as_mut()
                .ok_or_else(|| invalid("请先 connect 并核对 Identity"))?;
            let startup = client
                .read_config(ConfigView::Startup, &wait)
                .map_err(sdk_error)?;
            let status = client.status(&wait).map_err(sdk_error)?;
            (startup, status)
        };
        let startup = reply_json(&startup_reply)?;
        let status = reply_json(&status_reply)?;
        self.last_status = Some(status_reply);
        validate_trial_facts(request, &startup, &status, self.request.as_ref())?;
        self.record_event(
            "trial_prepared",
            json!({"request": request, "startup": startup, "status": status}),
        );
        self.trial.completed = None;
        self.trial.prepared = Some(PreparedTrial {
            request: request.clone(),
            status,
            startup,
            identity: self
                .identity
                .as_ref()
                .and_then(|reply| reply_json(reply).ok())
                .ok_or_else(|| invalid("Identity 不可读"))?,
            connection: self.request.clone(),
        });
        Ok(())
    }

    /// 对已预检且完全相同的单次请求提交一次持续需求。
    pub fn start_prepared_trial(&mut self, request: TrialRequest) -> Result<Value, SessionError> {
        if self.trial.active.is_some() {
            return Err(invalid("已有进行中的试验"));
        }
        let prepared = self
            .trial
            .prepared
            .take()
            .ok_or_else(|| invalid("请先 prepare_trial"))?;
        if serde_json::to_value(&prepared.request).ok() != serde_json::to_value(&request).ok() {
            self.trial.prepared = Some(prepared);
            return Err(invalid("试验请求与已预检范围不一致；请重新预检"));
        }
        let current_identity = self
            .identity
            .as_ref()
            .and_then(|reply| reply_json(reply).ok())
            .ok_or_else(|| invalid("请先 connect 并核对 Identity"))?;
        if current_identity["uid"] != prepared.identity["uid"]
            || current_identity["sample"]["run_nonce"] != prepared.identity["sample"]["run_nonce"]
            || self.request != prepared.connection
        {
            return Err(invalid("预检所属设备、运行实例或通路已经变化；请重新预检"));
        }
        let fresh = self
            .client
            .as_ref()
            .and_then(Client::telemetry)
            .ok_or_else(|| invalid("开始试验前需要同一运行实例的新鲜被动遥测"))?;
        let fresh_value = reply_json(&fresh)?;
        validate_trial_facts(
            &request,
            &prepared.startup,
            &fresh_value,
            self.request.as_ref(),
        )?;
        self.last_telemetry = Some(fresh);
        self.record_event("trial_command_input", json!({"request": &request}));
        let started_sample_time_us = prepared.status["sample"]["snapshot_time_us"]
            .as_u64()
            .unwrap_or(0);
        let initial_position = matches!(request.command, Command::Position { .. });
        self.trial.active = Some(ActiveTrial {
            request: request.clone(),
            duration: request.duration_limit()?,
            started_at: Instant::now(),
            pending_position_mm: None,
            last_position_submit: initial_position.then(Instant::now),
            settled_since: None,
            stop_submission: None,
            stop_reason: None,
            stop_observed: false,
            started_sample_time_us,
            stop_after_sample_time_us: None,
            stop_attempt_finished_at: None,
            stop_recovery_after_received_at: None,
            settled_sample_time_us: None,
        });
        let result = match self.execute_trial_command(request.command.clone()) {
            Ok(result) => result,
            Err(error) => {
                self.record_event("trial_command_error", json!({"error": &error}));
                if error.local_stage.as_deref() == Some("not_submitted") {
                    self.trial.active = None;
                } else {
                    let _ = self.submit_trial_stop("initial_submission_unknown");
                }
                return Err(error);
            }
        };
        self.record_event(
            "trial_command_submitted",
            json!({
                "local_submission": result.local_submission,
                "data": result.data,
            }),
        );
        Ok(json!({"submitted": result, "trial": self.trial_snapshot()}))
    }

    pub fn start_trial(&mut self, request: TrialRequest) -> Result<Value, SessionError> {
        self.prepare_trial(&request)?;
        self.start_prepared_trial(request)
    }

    /// 只合并位置试验的最新目标；实际发送由 `tick` 限为最多 20 Hz。
    pub fn update_trial_position(&mut self, mm: f32) -> Result<Value, SessionError> {
        let active = self
            .trial
            .active
            .as_mut()
            .ok_or_else(|| invalid("当前没有进行中的试验"))?;
        if !matches!(active.request.command, Command::Position { .. }) {
            return Err(invalid("当前试验不是位置试验"));
        }
        if active.stop_submission.is_some() {
            return Err(invalid("试验已开始停止；不再提交位置目标"));
        }
        if !mm.is_finite()
            || mm < active.request.envelope.position_min_mm
            || mm > active.request.envelope.position_max_mm
        {
            return Err(invalid("位置目标超出本次试验范围"));
        }
        active.pending_position_mm = Some(mm);
        self.record_event("trial_position_merged", json!({"mm": mm}));
        Ok(self.trial_snapshot())
    }

    /// 显式发送一次 Stop，并仅以其后的新遥测确认目标清除；任何失败均不重放。
    pub fn stop_trial(&mut self) -> Result<Value, SessionError> {
        self.resume_interrupted_trial_for_stop()?;
        self.submit_trial_stop("explicit")?;
        Ok(self.trial_snapshot())
    }

    pub fn trial_snapshot(&self) -> Value {
        self.trial.snapshot()
    }

    /// 服务后台每约 10 ms 调用。它只消费 SDK 的被动遥测队列和推进已授权试验。
    pub fn tick(&mut self) {
        if let Some(client) = self.client.take() {
            self.capture_telemetry(&client);
            let disconnected = client.connection_status().disconnected;
            self.client = Some(client);
            if let Some(reason) = disconnected {
                self.interrupt_trial_unknown(&format!("transport_disconnected:{reason}"));
                self.finish_recording_boundary("transport_disconnected");
            }
        }
        let stop_reason = self.trial_stop_reason();
        if let Some(reason) = stop_reason {
            let _ = self.submit_trial_stop(&reason);
        }
        self.flush_trial_position();
        self.observe_trial_stop();
        self.flush_recording();
    }

    fn trial_stop_reason(&mut self) -> Option<String> {
        let active = self.trial.active.as_mut()?;
        if active.stop_submission.is_some() {
            return None;
        }
        if active.started_at.elapsed() >= active.duration {
            return Some("duration_elapsed".into());
        }
        let telemetry = match self
            .last_telemetry
            .as_ref()
            .and_then(|reply| reply_json(reply).ok())
        {
            Some(telemetry) => telemetry,
            None if active.started_at.elapsed() > Duration::from_millis(500) => {
                return Some("telemetry_missing".into());
            }
            None => return None,
        };
        if telemetry["received_age_ms"]
            .as_u64()
            .is_none_or(|age| age > 250)
        {
            return Some("telemetry_stale".into());
        }
        let time_us = match telemetry["sample"]["snapshot_time_us"].as_u64() {
            Some(time) if time > active.started_sample_time_us => time,
            _ => return None,
        };
        if telemetry["facts"]["retained_result"].as_bool() != Some(false)
            || telemetry["facts"]["unknown_effect"].as_bool() != Some(false)
            || telemetry["driver_state"]["qualified"].as_bool() != Some(true)
            || telemetry["driver_state"]["stale"].as_bool() != Some(false)
            || telemetry["driver_state"]["faulted"].as_bool() != Some(false)
            || !trial_contact_active(&telemetry, self.request.as_ref())
            || telemetry["main_values"]
                .as_array()
                .is_none_or(|values| values.len() < 5 || !values.iter().take(5).all(fresh_value))
        {
            return Some("runtime_facts_invalid".into());
        }
        let values = telemetry["main_values"].as_array().expect("checked length");
        if observed_f32(&values[0]["value"]).is_none_or(|value| {
            value < active.request.envelope.position_min_mm
                || value > active.request.envelope.position_max_mm
        }) || observed_f32(&values[1]["value"])
            .is_none_or(|value| value.abs() > active.request.envelope.velocity_abs_max_mm_s)
            || observed_f32(&values[4]["value"])
                .is_none_or(|value| value.abs() > active.request.envelope.force_abs_max_n)
        {
            return Some("runtime_envelope_exceeded".into());
        }
        let reach = active.request.reach.as_ref()?;
        let Command::Position { mm } = active.request.command else {
            return None;
        };
        if !trial_target_matches(&telemetry, &active.request.command) {
            active.settled_since = None;
            return None;
        }
        if active.settled_sample_time_us == Some(time_us) {
            return None;
        }
        active.settled_sample_time_us = Some(time_us);
        if !fresh_value(&telemetry["main_values"][0]) {
            active.settled_since = None;
            return None;
        }
        let position = observed_f32(&telemetry["main_values"][0]["value"])?;
        if !eha_sdk::config::within_tolerance(position, mm, reach.tolerance_mm, 0.0) {
            active.settled_since = None;
            return None;
        }
        let now = Instant::now();
        let settled_since = active.settled_since.get_or_insert(now);
        (now.duration_since(*settled_since).as_millis() >= u128::from(reach.settle_ms))
            .then(|| "position_reached".into())
    }

    fn flush_trial_position(&mut self) {
        let position = {
            let Some(active) = self.trial.active.as_ref() else {
                return;
            };
            if active.stop_submission.is_some()
                || active
                    .last_position_submit
                    .is_some_and(|last| last.elapsed() < Duration::from_millis(50))
            {
                return;
            }
            active.pending_position_mm
        };
        let Some(mm) = position else {
            return;
        };
        self.record_event("trial_position_submit", json!({"mm": mm}));
        let result = self.execute_trial_command(Command::Position { mm });
        match result {
            Ok(result) => {
                if let Some(active) = self.trial.active.as_mut() {
                    active.pending_position_mm = None;
                    active.last_position_submit = Some(Instant::now());
                    active.request.command = Command::Position { mm };
                    active.settled_since = None;
                    active.settled_sample_time_us = None;
                }
                self.record_event(
                    "trial_position_submitted",
                    json!({"mm": mm, "local_submission": result.local_submission}),
                );
            }
            Err(error) => {
                self.record_event("trial_position_error", json!({"mm": mm, "error": error}));
                if let Some(active) = self.trial.active.as_mut() {
                    active.pending_position_mm = None;
                    active.stop_reason = Some("position_submit_failed".into());
                }
                let _ = self.submit_trial_stop("position_submit_failed");
            }
        }
    }

    fn submit_trial_stop(&mut self, reason: &str) -> Result<(), SessionError> {
        let baseline = self
            .last_telemetry
            .as_ref()
            .and_then(|reply| reply_json(reply).ok())
            .and_then(|value| value["sample"]["snapshot_time_us"].as_u64())
            .unwrap_or(0);
        {
            let active = self
                .trial
                .active
                .as_mut()
                .ok_or_else(|| invalid("当前没有进行中的试验"))?;
            if active.stop_submission.is_some() {
                return Ok(());
            }
            active.pending_position_mm = None;
            active.stop_reason = Some(reason.into());
            active.stop_after_sample_time_us = Some(baseline);
        }
        self.record_event("trial_stop_input", json!({"reason": reason}));
        match self.execute_trial_command(Command::Stop) {
            Ok(result) => {
                let submission = json!({
                    "local_submission": result.local_submission,
                    "data": result.data,
                    "device_execution": "awaiting_new_telemetry",
                });
                if let Some(active) = self.trial.active.as_mut() {
                    active.stop_submission = Some(submission.clone());
                    active.stop_attempt_finished_at = Some(Instant::now());
                }
                self.record_event("trial_stop_submitted", submission);
                Ok(())
            }
            Err(error) => {
                let failure = json!({"error": &error, "device_execution": "unknown_no_retry"});
                if let Some(active) = self.trial.active.as_mut() {
                    active.stop_submission = Some(failure.clone());
                    active.stop_attempt_finished_at = Some(Instant::now());
                }
                self.record_event("trial_stop_error", failure);
                Err(error)
            }
        }
    }

    fn observe_trial_stop(&mut self) {
        let received_at = self.last_telemetry.as_ref().map(|reply| reply.received_at);
        let telemetry = self
            .last_telemetry
            .as_ref()
            .and_then(|reply| reply_json(reply).ok());
        let Some(telemetry) = telemetry else {
            return;
        };
        let sample_time_us = telemetry["sample"]["snapshot_time_us"].as_u64();
        let no_target = telemetry["target_mode"].as_u64() == Some(0);
        let idle = telemetry["desired_axis"].as_u64() == Some(1);
        let driver_idle = telemetry["axis_state_raw"].as_u64() == Some(1)
            && telemetry["driver_state"]["has_status"].as_bool() == Some(true)
            && telemetry["driver_state"]["qualified"].as_bool() == Some(true)
            && telemetry["driver_state"]["stale"].as_bool() == Some(false)
            && telemetry["driver_state"]["faulted"].as_bool() == Some(false);
        let clear_facts = telemetry["facts"]["device_operation_pending"].as_bool() == Some(false)
            && telemetry["facts"]["retained_result"].as_bool() == Some(false)
            && telemetry["facts"]["unknown_effect"].as_bool() == Some(false);
        let same_identity = self
            .identity
            .as_ref()
            .and_then(|reply| reply_json(reply).ok())
            .is_some_and(|identity| {
                telemetry["sample"]["run_nonce"] == identity["sample"]["run_nonce"]
            });
        let fresh = telemetry["received_age_ms"]
            .as_u64()
            .is_some_and(|age| age <= 250);
        let Some(active) = self.trial.active.as_ref() else {
            return;
        };
        let confirmed = active.stop_submission.is_some()
            && sample_time_us
                .is_some_and(|time| time > active.stop_after_sample_time_us.unwrap_or(0))
            && no_target
            && idle
            && driver_idle
            && clear_facts
            && same_identity
            && fresh
            && received_at.is_some_and(|received| {
                active
                    .stop_recovery_after_received_at
                    .or(active.stop_attempt_finished_at)
                    .is_some_and(|not_before| received > not_before)
            });
        if confirmed {
            let completed = json!({
                "state": "completed",
                "request": active.request.clone(),
                "stop_reason": active.stop_reason.clone(),
                "stop_submission": active.stop_submission.clone(),
                "stop_observed": true,
                "evidence": &telemetry,
            });
            self.record_event("trial_stop_confirmed", json!({"telemetry": &telemetry}));
            self.trial.completed = Some(completed);
            self.trial.active = None;
        }
    }

    fn execute_trial_command(&mut self, command: Command) -> Result<CommandResult, SessionError> {
        let original = self.timeout;
        self.timeout = original.min(Duration::from_millis(100));
        let result = self.execute_inner(command);
        self.timeout = original;
        result
    }

    fn wait(&self) -> Wait {
        Wait::new(self.timeout)
    }

    fn clear_observations(&mut self) {
        self.identity = None;
        self.last_telemetry = None;
        self.last_status = None;
        self.last_measurements = None;
        self.last_diagnostics = None;
        self.trial.prepared = None;
        if let Some(trend) = self.telemetry_trend.as_mut() {
            trend.reset();
        }
    }

    fn capture_telemetry(&mut self, client: &Client) {
        let trend_source = self.trend_source();
        let drained = self
            .telemetry_queue_capacity
            .map(|_| client.drain_telemetry());
        if let (Some(trend), Some(drained)) = (self.telemetry_trend.as_mut(), drained.as_ref()) {
            trend.set_source(trend_source);
            trend.record(drained.replies.clone(), drained.dropped);
        }
        if let Some(drained) = drained {
            if drained.dropped != 0 {
                self.record_telemetry_dropped(drained.dropped);
            }
            let mut gap = drained.dropped != 0;
            for reply in drained.replies {
                match reply_json(&reply) {
                    Ok(value) => self.record_telemetry(&value, std::mem::take(&mut gap)),
                    Err(error) => {
                        self.record_event("telemetry_decode_error", json!({"error": error}))
                    }
                }
            }
        }
        if let Some(reply) = client.telemetry()
            && reply_json(&reply).is_ok()
        {
            self.last_telemetry = Some(reply);
        }
    }

    fn trend_source(&self) -> Option<Value> {
        let identity = self
            .identity
            .as_ref()
            .and_then(|reply| reply_json(reply).ok())?;
        Some(json!({
            "connection": self.request.clone(),
            "uid": identity["uid"],
            "run_nonce": identity["sample"]["run_nonce"],
        }))
    }

    /// 取得 WebUI 的不破坏性趋势读取投影。`after` 仅是本会话的游标，不是协议序号。
    pub fn telemetry_trend_json(&self, after: Option<u64>) -> Value {
        self.telemetry_trend
            .as_ref()
            .map_or(Value::Null, |trend| trend.json(after))
    }

    fn record_event(&mut self, event: &str, data: Value) {
        if self.recording_error.is_some() {
            return;
        }
        let error = self
            .recording
            .as_mut()
            .and_then(|recording| recording.event(event, data).err());
        if let Some(error) = error {
            self.recording_error = Some(error);
        }
    }

    fn record_telemetry(&mut self, telemetry: &Value, gap: bool) {
        if self.recording_error.is_some() {
            return;
        }
        let error = self
            .recording
            .as_mut()
            .and_then(|recording| recording.telemetry(telemetry, gap).err());
        if let Some(error) = error {
            self.recording_error = Some(error);
        }
    }

    fn record_telemetry_dropped(&mut self, count: u64) {
        if self.recording_error.is_some() {
            return;
        }
        let error = self
            .recording
            .as_mut()
            .and_then(|recording| recording.dropped(count).err());
        if let Some(error) = error {
            self.recording_error = Some(error);
        }
    }

    fn flush_recording(&mut self) {
        if self.recording_error.is_some() {
            return;
        }
        let error = self
            .recording
            .as_mut()
            .and_then(|recording| recording.flush().err());
        if let Some(error) = error {
            self.recording_error = Some(error);
        }
    }

    fn finish_recording_boundary(&mut self, reason: &str) {
        let Some(mut recording) = self.recording.take() else {
            return;
        };
        let result = recording.event("recording_boundary", json!({"reason": reason}));
        if let Err(error) = result.and_then(|_| recording.finish()) {
            self.recording_error = Some(error);
        }
    }

    fn interrupt_trial_unknown(&mut self, reason: &str) {
        let Some(active) = self.trial.active.take() else {
            return;
        };
        let evidence = self
            .last_telemetry
            .as_ref()
            .and_then(|reply| reply_json(reply).ok());
        self.trial.interrupted = Some(InterruptedTrial {
            active,
            identity: self.identity.as_ref().and_then(identity_facts),
            reason: reason.into(),
            last_telemetry: evidence,
            stop_available: false,
            unrecoverable: false,
        });
        self.record_event("trial_interrupted_unknown", self.trial.snapshot());
    }

    fn reconcile_interrupted_trial(&mut self) {
        let Some(interrupted) = self.trial.interrupted.as_mut() else {
            return;
        };
        let current = self.identity.as_ref().and_then(identity_facts);
        let same_instance = interrupted
            .identity
            .zip(current)
            .is_some_and(|(original, current)| same_trial_instance(original, current));
        interrupted.stop_available = same_instance;
        interrupted.unrecoverable = current.is_some() && !same_instance;
    }

    fn mark_interrupted_reconnect_required(&mut self) {
        if let Some(interrupted) = self.trial.interrupted.as_mut()
            && !interrupted.unrecoverable
        {
            interrupted.stop_available = false;
        }
    }

    fn resume_interrupted_trial_for_stop(&mut self) -> Result<(), SessionError> {
        let Some(mut interrupted) = self.trial.interrupted.take() else {
            return Ok(());
        };
        let same_instance = self
            .client
            .as_ref()
            .is_some_and(|client| client.connection_status().disconnected.is_none())
            && self
                .identity
                .as_ref()
                .and_then(identity_facts)
                .zip(interrupted.identity)
                .is_some_and(|(current, original)| same_trial_instance(original, current));
        if !same_instance {
            self.trial.interrupted = Some(interrupted);
            return Err(invalid(
                "中断试验尚未重连到同一设备和运行实例；不能向未知对象发送 Stop",
            ));
        }
        interrupted.active.pending_position_mm = None;
        if interrupted.active.stop_submission.is_some() {
            interrupted.active.stop_recovery_after_received_at = Some(Instant::now());
        }
        self.trial.active = Some(interrupted.active);
        Ok(())
    }
}

fn recording_error(message: impl Into<String>) -> SessionError {
    SessionError {
        kind: "recording".into(),
        message: message.into().into_boxed_str(),
        local_stage: None,
        operation_key: None,
        unknown: false,
        failure: None,
        last_result: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}

fn fresh_value(value: &Value) -> bool {
    value["result"].as_u64() == Some(1)
        && value["quality"].as_u64() == Some(1)
        && value["stale"].as_bool() == Some(false)
}

fn trial_contact_active(telemetry: &Value, connection: Option<&ConnectionRequest>) -> bool {
    match connection {
        Some(ConnectionRequest::Usb { .. }) => telemetry["usb_contact"].as_u64() == Some(1),
        Some(ConnectionRequest::Can { .. }) => telemetry["can_contact"].as_u64() == Some(1),
        None => false,
    }
}

fn trial_target_matches(telemetry: &Value, command: &Command) -> bool {
    let values = telemetry["target_values"].as_array();
    let expected = match command {
        Command::Position { mm } => Some((1_u64, [*mm, 0.0, 0.0])),
        Command::Velocity { mm_s } => Some((2, [*mm_s, 0.0, 0.0])),
        Command::Force { n } => Some((3, [*n, 0.0, 0.0])),
        Command::Impedance {
            equilibrium_mm,
            stiffness_n_per_mm,
            damping_ns_per_mm,
        } => Some((
            4,
            [*equilibrium_mm, *stiffness_n_per_mm, *damping_ns_per_mm],
        )),
        _ => None,
    };
    let Some((mode, expected)) = expected else {
        return false;
    };
    telemetry["target_mode"].as_u64() == Some(mode)
        && values.is_some_and(|values| {
            values.len() == 3
                && values.iter().zip(expected).all(|(actual, expected)| {
                    observed_f32(actual).is_some_and(|value| value == expected)
                })
        })
}

fn validate_trial_request(request: &TrialRequest) -> Result<(), SessionError> {
    let envelope = &request.envelope;
    request.duration_limit()?;
    if !matches!(
        request.command,
        Command::Position { .. }
            | Command::Velocity { .. }
            | Command::Force { .. }
            | Command::Impedance { .. }
    ) {
        return Err(invalid(
            "试验仅允许 Position、Velocity、Force 或 Impedance 持续需求",
        ));
    }
    if !envelope.position_min_mm.is_finite()
        || !envelope.position_max_mm.is_finite()
        || envelope.position_min_mm > envelope.position_max_mm
        || !envelope.velocity_abs_max_mm_s.is_finite()
        || !envelope.force_abs_max_n.is_finite()
        || !envelope.stiffness_max_n_per_mm.is_finite()
        || !envelope.damping_max_ns_per_mm.is_finite()
        || !envelope.duration_max_s.is_finite()
        || envelope.velocity_abs_max_mm_s < 0.0
        || envelope.force_abs_max_n < 0.0
        || envelope.stiffness_max_n_per_mm < 0.0
        || envelope.damping_max_ns_per_mm < 0.0
        || envelope.duration_max_s <= 0.0
    {
        return Err(invalid("试验范围必须为有限且有效的上限/位置区间"));
    }
    if request.duration_s.is_some_and(|duration| {
        !duration.is_finite() || duration <= 0.0 || duration > envelope.duration_max_s
    }) {
        return Err(invalid("试验时长必须为正且不超过本次 duration_max_s"));
    }
    if let Some(reach) = &request.reach
        && (!reach.tolerance_mm.is_finite() || reach.tolerance_mm < 0.0)
    {
        return Err(invalid("到位容差必须为有限非负值"));
    }
    match request.command {
        Command::Position { mm }
            if mm.is_finite()
                && mm >= envelope.position_min_mm
                && mm <= envelope.position_max_mm => {}
        Command::Velocity { mm_s }
            if mm_s.is_finite() && mm_s.abs() <= envelope.velocity_abs_max_mm_s => {}
        Command::Force { n } if n.is_finite() && n.abs() <= envelope.force_abs_max_n => {}
        Command::Impedance {
            equilibrium_mm,
            stiffness_n_per_mm,
            damping_ns_per_mm,
        } if equilibrium_mm.is_finite()
            && equilibrium_mm >= envelope.position_min_mm
            && equilibrium_mm <= envelope.position_max_mm
            && stiffness_n_per_mm.is_finite()
            && stiffness_n_per_mm >= 0.0
            && stiffness_n_per_mm <= envelope.stiffness_max_n_per_mm
            && damping_ns_per_mm.is_finite()
            && damping_ns_per_mm >= 0.0
            && damping_ns_per_mm <= envelope.damping_max_ns_per_mm => {}
        _ => return Err(invalid("控制请求超出本次试验范围")),
    }
    Ok(())
}

fn validate_trial_facts(
    request: &TrialRequest,
    startup: &Value,
    status: &Value,
    connection: Option<&ConnectionRequest>,
) -> Result<(), SessionError> {
    if status["received_age_ms"]
        .as_u64()
        .is_none_or(|age| age > 250)
    {
        return Err(invalid("试验预检需要新鲜 Status"));
    }
    let facts = &status["facts"];
    let driver = &status["driver_state"];
    if facts["device_operation_pending"].as_bool() != Some(false)
        || facts["retained_result"].as_bool() != Some(false)
        || facts["unknown_effect"].as_bool() != Some(false)
        || driver["has_status"].as_bool() != Some(true)
        || driver["qualified"].as_bool() != Some(true)
        || driver["stale"].as_bool() != Some(false)
        || driver["faulted"].as_bool() != Some(false)
    {
        return Err(invalid("当前维护、驱动或未知副作用事实不允许开始试验"));
    }
    if status["target_mode"].as_u64() != Some(0)
        || status["desired_axis"].as_u64() != Some(1)
        || status["axis_state_raw"].as_u64() != Some(1)
    {
        return Err(invalid(
            "当前仍有目标或驱动未处于 Idle；不会覆盖其它通路的持续控制",
        ));
    }
    let contact_active = match connection {
        Some(ConnectionRequest::Usb { .. }) => status["usb_contact"].as_u64() == Some(1),
        Some(ConnectionRequest::Can { .. }) => status["can_contact"].as_u64() == Some(1),
        None => false,
    };
    if !contact_active {
        return Err(invalid(
            "当前通路联系未激活；不会以旧 Ready 或已提交目标代替",
        ));
    }
    let values = status["main_values"]
        .as_array()
        .ok_or_else(|| invalid("Status 缺少测量事实"))?;
    if values.len() < 5 || !values.iter().take(5).all(fresh_value) {
        return Err(invalid("位置、速度、压力或力测量不新鲜/不合格"));
    }
    if observed_f32(&values[0]["value"]).is_none_or(|value| {
        value < request.envelope.position_min_mm || value > request.envelope.position_max_mm
    }) || observed_f32(&values[1]["value"])
        .is_none_or(|value| value.abs() > request.envelope.velocity_abs_max_mm_s)
        || observed_f32(&values[4]["value"])
            .is_none_or(|value| value.abs() > request.envelope.force_abs_max_n)
    {
        return Err(invalid("当前位置、速度或力超出本次试验范围"));
    }
    let config = startup["data_utf8"]
        .as_str()
        .ok_or_else(|| invalid("实际 Startup 配置不可读"))?;
    #[derive(Deserialize)]
    struct Protection {
        hard_position_min_mm: f32,
        hard_position_max_mm: f32,
        hard_velocity_max_mm_s: f32,
        hard_force_max_n: f32,
    }
    #[derive(Deserialize)]
    struct StartupConfig {
        protection: Protection,
    }
    #[derive(Deserialize)]
    struct StartupRecord {
        config: StartupConfig,
    }
    eha_sdk::config::validate_f32_tokens(config.as_bytes())
        .map_err(|_| invalid("实际 Startup 配置包含不可表示数值"))?;
    let config: StartupRecord = serde_json::from_str(config)
        .map_err(|_| invalid("实际 Startup 配置缺少硬保护范围或不是有效 JSON"))?;
    let protection = config.config.protection;
    let (position_min, position_max, velocity_max, force_max) = (
        protection.hard_position_min_mm,
        protection.hard_position_max_mm,
        protection.hard_velocity_max_mm_s,
        protection.hard_force_max_n,
    );
    let envelope = &request.envelope;
    if envelope.position_min_mm < position_min
        || envelope.position_max_mm > position_max
        || envelope.velocity_abs_max_mm_s > velocity_max
        || envelope.force_abs_max_n > force_max
    {
        return Err(invalid("试验范围超过实际 Startup 硬保护范围"));
    }
    Ok(())
}

pub(crate) fn open_can(
    channel: String,
    node: u8,
    mode: &str,
    python: Option<String>,
) -> Result<eha_sdk::host::backend::Backend, String> {
    let mut connector = CanConnector::new(
        CanOptions {
            node,
            mode: parse_can_mode(mode).map_err(|error| error.message)?,
        },
        python_factory(channel, python),
    )?;
    connector.open()
}

fn connector_for(request: &ConnectionRequest) -> Result<Connector, SessionError> {
    match request {
        ConnectionRequest::Usb { serial } => UsbConnector::new(serial.clone())
            .map(Connector::Usb)
            .map_err(open_error),
        ConnectionRequest::Can {
            channel,
            node,
            mode,
            python,
        } => CanConnector::new(
            CanOptions {
                node: *node,
                mode: parse_can_mode(mode)?,
            },
            python_factory(channel.clone(), python.clone()),
        )
        .map(Connector::Can)
        .map_err(open_error),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Status => "status",
        Command::Measurements => "measurements",
        Command::Diagnostics => "diagnostics",
        Command::Telemetry => "telemetry",
        Command::HeartbeatStart => "heartbeat_start",
        Command::HeartbeatStop => "heartbeat_stop",
        Command::HeartbeatOnce => "heartbeat_once",
        Command::Position { .. } => "position",
        Command::Velocity { .. } => "velocity",
        Command::Force { .. } => "force",
        Command::Impedance { .. } => "impedance",
        Command::Stop => "stop",
        Command::ConfigRead { .. } => "config_read",
        Command::ConfigValidate { .. } => "config_validate",
        Command::ConfigSave { .. } => "config_save",
        Command::RestoreFactory => "restore_factory",
        Command::MaintenanceResult { .. } => "maintenance_result",
        Command::MaintenanceRelease { .. } => "maintenance_release",
        Command::ResetApplication => "reset_application",
        Command::EnterUpdate => "enter_update",
    }
}

fn local_submission_json(local: LocalSubmission) -> LocalSubmissionSnapshot {
    LocalSubmissionSnapshot {
        id: local.id,
        boundary: local.boundary.into(),
    }
}
fn submission_data(local: LocalSubmission, slot: &mut Option<LocalSubmissionSnapshot>) -> Value {
    *slot = Some(local_submission_json(local));
    json!({"local_submission": true, "device_execution": "unconfirmed"})
}
fn operation_submission_data(
    submission: OperationSubmission,
    slot: &mut Option<LocalSubmissionSnapshot>,
    key: &mut Option<String>,
) -> Value {
    *slot = Some(local_submission_json(submission.local));
    let value = key_hex(submission.key);
    *key = Some(value.clone());
    json!({"local_submission": true, "operation_key": value, "device_execution": "unconfirmed"})
}

pub(crate) fn parse_can_mode(value: &str) -> Result<eha_sdk::transport::can::Mode, SessionError> {
    match value {
        "classic" => Ok(eha_sdk::transport::can::Mode::Classic),
        "fd" => Ok(eha_sdk::transport::can::Mode::Fd),
        _ => Err(invalid("未知 CAN 帧格式；可用值：classic、fd")),
    }
}

fn python_factory(channel: String, python: Option<String>) -> Arc<dyn CanChannelFactory> {
    Arc::new(match python {
        Some(python) => PythonCanOptions::with_python(channel, python),
        None => PythonCanOptions::new(channel),
    })
}
fn parse_view(value: &str) -> Result<ConfigView, SessionError> {
    match value {
        "factory" => Ok(ConfigView::Factory),
        "user" => Ok(ConfigView::UserRecord),
        "startup" => Ok(ConfigView::Startup),
        "communication" => Ok(ConfigView::Communication),
        _ => Err(invalid("未知配置视图")),
    }
}
fn parse_key(value: &str) -> Result<MaintenanceKey, SessionError> {
    let mut bytes = [0_u8; 36];
    if value.len() != 72 || !value.is_ascii() {
        return Err(invalid("操作键必须为 72 个 ASCII 十六进制字符"));
    }
    for (index, output) in bytes.iter_mut().enumerate() {
        *output = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| invalid("操作键必须只包含十六进制字符"))?;
    }
    MaintenanceKey::from_bytes(&bytes).map_err(|_| invalid("操作键无效"))
}
fn key_hex(key: MaintenanceKey) -> String {
    hex(&key.to_bytes())
}
#[derive(Clone, Copy, Eq, PartialEq)]
struct IdentityFacts {
    uid: [u8; 12],
    run_nonce: [u8; 16],
    retained_operation_id: u64,
}

fn same_trial_instance(left: IdentityFacts, right: IdentityFacts) -> bool {
    left.uid == right.uid && left.run_nonce == right.run_nonce
}
fn identity_facts(reply: &Reply) -> Option<IdentityFacts> {
    let Response::Identity(identity) = reply.response().ok()? else {
        return None;
    };
    let fields = identity.fields();
    Some(IdentityFacts {
        uid: fields.uid,
        run_nonce: fields.sample.run_nonce,
        retained_operation_id: fields.retained_operation_id,
    })
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn invalid(message: impl Into<String>) -> SessionError {
    SessionError {
        kind: "invalid_input".into(),
        message: message.into().into_boxed_str(),
        local_stage: Some("not_submitted".into()),
        operation_key: None,
        unknown: false,
        failure: None,
        last_result: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}
fn open_error(message: String) -> SessionError {
    SessionError {
        kind: "connection".into(),
        message: message.into_boxed_str(),
        local_stage: None,
        operation_key: None,
        unknown: false,
        failure: None,
        last_result: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}
fn sdk_error(error: SdkError) -> SessionError {
    let SdkError {
        failure,
        local,
        operation,
        last_result,
        observed_transport,
    } = error;
    let operation_key = operation
        .as_deref()
        .copied()
        .map(key_hex)
        .map(String::into_boxed_str);
    let last_result = last_result.map(|fields| Box::new(operation_fields_json(*fields)));
    let observed_transport =
        observed_transport.map(|transport| Box::new(transport_json(&transport, None)));
    let (kind, message, unknown, failure, transport_raw_hex) = match failure {
        Failure::InvalidInput(message) => ("invalid_input", message, false, None, None),
        Failure::Session(message) => ("session", message, false, None, None),
        Failure::Timeout => ("timeout", "等待 SDK 操作超时".into(), false, None, None),
        Failure::Cancelled => ("cancelled", "SDK 操作已取消".into(), false, None, None),
        Failure::Disconnected(message) => ("disconnected", message, false, None, None),
        Failure::Transport { detail, raw } => (
            "transport",
            detail,
            false,
            None,
            Some(hex(&raw).into_boxed_str()),
        ),
        Failure::DataUnavailable(fields) => {
            let value = data_unavailable_fields_json(fields);
            (
                "data_unavailable",
                format!(
                    "固件报告数据不可取得：{}；只说明本次读取未取得数据",
                    eha_sdk::diagnostics::reason_label(fields.reason)
                ),
                false,
                Some(Box::new(value)),
                None,
            )
        }
        Failure::ResultUnknown(message) => ("result_unknown", message, true, None, None),
        Failure::PendingOperation => (
            "pending_operation",
            "已有未结清维护操作".into(),
            false,
            None,
            None,
        ),
        Failure::ReadbackMismatch => (
            "readback_mismatch",
            "实际读回与请求记录不一致".into(),
            false,
            None,
            None,
        ),
        Failure::ReadbackUnavailable(state) => (
            "readback_unavailable",
            format!("实际读回不可用：{}", state as u8),
            false,
            None,
            None,
        ),
        Failure::DeviceResult(fields) => (
            "device_result",
            format!(
                "固件返回维护结果：{}；这只是读取到的设备结果，不表示本地提交成功",
                eha_sdk::diagnostics::operation_state_label(fields.state)
            ),
            fields.state == eha_sdk::protocol::responses::MaintenanceState::ResultUnknown,
            Some(Box::new(operation_fields_json(*fields))),
            None,
        ),
    };
    SessionError {
        kind: kind.into(),
        message: message.into_boxed_str(),
        local_stage: Some(
            match local {
                eha_sdk::host::LocalStage::NotSubmitted => "not_submitted",
                eha_sdk::host::LocalStage::MayHaveBeenSubmitted => "may_have_been_submitted",
                eha_sdk::host::LocalStage::FullySubmitted => "fully_submitted",
            }
            .into(),
        ),
        operation_key,
        unknown,
        failure,
        last_result,
        observed_transport,
        transport_raw_hex,
    }
}

fn transport_json(
    transport: &eha_sdk::host::TransportStatus,
    disconnected: Option<String>,
) -> Value {
    json!({"errors": transport.errors, "last_error": transport.last_error.as_ref().map(|(detail, raw, _)| json!({"detail": detail, "raw_hex": hex(raw)})), "disconnected": disconnected})
}

fn sample_json(sample: eha_sdk::protocol::SampleData) -> Value {
    json!({"query_id": sample.query_id, "run_nonce": hex(&sample.run_nonce), "snapshot_sequence": sample.snapshot_sequence, "snapshot_time_us": sample.snapshot_time_us})
}
fn sample_view_json(sample: eha_sdk::protocol::Sample<'_>) -> Value {
    json!({"query_id": sample.query_id(), "run_nonce": hex(sample.run_nonce()), "snapshot_sequence": sample.snapshot_sequence(), "snapshot_time_us": sample.snapshot_time_us()})
}
// 趋势只传绘图需要的数值和质量；完整遥测及压力由运行记录保留。
fn trend_value_json(value: ValueFields) -> Value {
    json!({
        "value": value.value,
        "result": value.state.result() as u8,
        "quality": value.state.quality() as u8,
        "stale": value.state.is_stale(),
    })
}

fn value_json(value: ValueFields) -> Value {
    let result = value.state.result();
    let quality = value.state.quality();
    json!({
        "value": value.value,
        "result": result as u8,
        "result_label": eha_sdk::diagnostics::value_result_label(result),
        "quality": quality as u8,
        "quality_label": eha_sdk::diagnostics::value_quality_label(quality),
        "stale": value.state.is_stale(),
    })
}
fn operation_fields_json(fields: eha_sdk::protocol::responses::OperationResultFields) -> Value {
    json!({
        "sample": sample_json(fields.sample),
        "operation_id": fields.operation_id,
        "operation": fields.operation as u8,
        "operation_label": eha_sdk::diagnostics::maintenance_operation_label(fields.operation),
        "state": fields.state as u8,
        "state_label": eha_sdk::diagnostics::operation_state_label(fields.state),
        "phase": fields.phase as u8,
        "phase_label": eha_sdk::diagnostics::phase_label(fields.phase),
        "reason": fields.reason,
        "reason_label": eha_sdk::diagnostics::reason_label(fields.reason),
        "next_actions": fields.next_actions,
        "next_action_labels": eha_sdk::diagnostics::next_actions_labels(fields.next_actions),
        "evidence": {
            "not_started": fields.evidence.not_started,
            "storage_replace_succeeded": fields.evidence.storage_replace_succeeded,
            "actual_storage_read": fields.evidence.actual_storage_read,
            "effect_may_have_occurred": fields.evidence.effect_may_have_occurred,
            "original_result_missing": fields.evidence.original_result_missing,
            "settled": fields.evidence.settled,
        },
        "constraints": constraints_json(fields.constraints),
        "started_time_us": fields.started_time_us,
        "deadline_us": fields.deadline_us,
        "finished_time_us": fields.finished_time_us,
        "content_length": fields.content_length,
        "content_crc32c": fields.content_crc32c,
        "revision": fields.revision,
    })
}

fn constraints_json(constraints: eha_sdk::protocol::responses::Constraints) -> Value {
    json!({
        "user_storage": constraints.user_storage,
        "driver_transition": constraints.driver_transition,
        "application_handoff": constraints.application_handoff,
        "result_slot": constraints.result_slot,
    })
}

fn diagnostic_entry_json(entry: eha_sdk::protocol::responses::DiagnosticFields) -> Value {
    json!({
        "domain": entry.domain as u8,
        "domain_label": eha_sdk::diagnostics::diagnostic_domain_label(entry.domain),
        "object": entry.object as u8,
        "object_label": eha_sdk::diagnostics::object_label(entry.object),
        "reason": entry.reason,
        "reason_label": eha_sdk::diagnostics::reason_label(entry.reason),
        "phase": entry.phase as u8,
        "phase_label": eha_sdk::diagnostics::phase_label(entry.phase),
        "next_actions": entry.next_actions,
        "next_action_labels": eha_sdk::diagnostics::next_actions_labels(entry.next_actions),
        "event_time_us": entry.event_time_us,
        "started_time_us": entry.started_time_us,
        "deadline_us": entry.deadline_us,
        "operation_id": entry.operation_id,
        "native_code": entry.native_code,
        "native_domain": entry.native_domain as u8,
        "native_domain_label": eha_sdk::diagnostics::native_domain_label(entry.native_domain),
        "native_code_description": eha_sdk::diagnostics::native_code_description(entry.native_domain, entry.native_code),
        "evidence": {
            "not_started": entry.evidence.not_started,
            "send_submitted": entry.evidence.send_submitted,
            "matched_feedback": entry.evidence.matched_feedback,
            "storage_replaced": entry.evidence.storage_replaced,
            "actual_readback": entry.evidence.actual_readback,
            "missing_original_result": entry.evidence.missing_original_result,
        },
        "detail": value_json(entry.detail_value),
        "detail_unit": entry.detail_unit as u8,
        "detail_unit_label": eha_sdk::diagnostics::detail_unit_label(entry.detail_unit),
        "constraints": constraints_json(entry.constraints),
        "occurrences": entry.occurrences,
        "missing_evidence": {
            "call_result": entry.missing_evidence.call_result,
            "device_feedback": entry.missing_evidence.device_feedback,
            "actual_readback": entry.missing_evidence.actual_readback,
            "same_device_comparison": entry.missing_evidence.same_device_comparison,
            "update_route_identity": entry.missing_evidence.update_route_identity,
            "new_application_start": entry.missing_evidence.new_application_start,
        },
        "impact": {
            "blocks_adoption": entry.impact.blocks_adoption,
            "limits_output": entry.impact.limits_output,
            "ends_target": entry.impact.ends_target,
            "limits_maintenance": entry.impact.limits_maintenance,
            "affects_observation": entry.impact.affects_observation,
            "unknown_side_effect": entry.impact.unknown_side_effect,
            "current": entry.impact.current,
            "historical": entry.impact.historical,
        },
    })
}

fn data_unavailable_fields_json(
    fields: eha_sdk::protocol::responses::DataUnavailableFields,
) -> Value {
    json!({
        "sample": sample_json(fields.sample),
        "subject": fields.subject as u8,
        "subject_label": eha_sdk::diagnostics::unavailable_subject_label(fields.subject),
        "reason": fields.reason,
        "reason_label": eha_sdk::diagnostics::reason_label(fields.reason),
        "next_actions": fields.next_actions,
        "next_action_labels": eha_sdk::diagnostics::next_actions_labels(fields.next_actions),
        "retained": fields.retained,
        "operation_id": fields.operation_id,
    })
}

fn limits_json(limits: eha_sdk::protocol::responses::LimitFlags) -> Value {
    json!({
        "position_soft_lower": limits.position_soft_lower,
        "position_soft_upper": limits.position_soft_upper,
        "position_hard_lower": limits.position_hard_lower,
        "position_hard_upper": limits.position_hard_upper,
        "velocity_soft": limits.velocity_soft,
        "velocity_hard": limits.velocity_hard,
        "pressure_a_soft": limits.pressure_a_soft,
        "pressure_a_hard": limits.pressure_a_hard,
        "pressure_b_soft": limits.pressure_b_soft,
        "pressure_b_hard": limits.pressure_b_hard,
        "force_soft": limits.force_soft,
        "force_hard": limits.force_hard,
        "motor_output_soft": limits.motor_output_soft,
        "motor_output_hard": limits.motor_output_hard,
    })
}

fn driver_summary_json(
    driver_state: eha_sdk::protocol::responses::DriverState,
    driver_age_us: u32,
    axis_state_raw: u32,
    axis_error_raw: u32,
) -> Value {
    let state = if !driver_state.has_status {
        "unavailable"
    } else if driver_state.stale {
        "stale"
    } else if driver_state.faulted {
        "faulted"
    } else if !driver_state.qualified {
        "unqualified"
    } else {
        "available"
    };
    let current = driver_state.has_status && !driver_state.stale;
    json!({
        "state": state,
        "label": eha_sdk::diagnostics::driver_state_summary_label(driver_state),
        "age_ms": u64::from(driver_age_us) / 1_000,
        "axis_state_label": current.then(|| eha_sdk::diagnostics::axis_state_label(axis_state_raw)),
        "axis_error_description": current.then(|| eha_sdk::diagnostics::native_code_description(eha_sdk::protocol::responses::NativeDomain::OdriveAxisError, axis_error_raw)),
    })
}

fn reply_json(reply: &Reply) -> Result<Value, SessionError> {
    let age_ms = Instant::now()
        .saturating_duration_since(reply.received_at)
        .as_millis() as u64;
    let value = match reply.response().map_err(sdk_error)? {
        Response::Identity(response) => {
            let fields = response.fields();
            json!({"kind":"identity", "received_age_ms": age_ms, "sample": sample_json(fields.sample), "uid": hex(&fields.uid), "config_source": fields.config_source as u8, "config_source_label": eha_sdk::diagnostics::startup_source_label(fields.config_source), "fallback_reason": fields.fallback_reason as u8, "fallback_reason_label": eha_sdk::diagnostics::fallback_reason_label(fields.fallback_reason), "run_identity": fields.run_identity as u8, "run_identity_label": eha_sdk::diagnostics::run_identity_label(fields.run_identity), "update_route": fields.update_route as u8, "update_route_label": eha_sdk::diagnostics::update_route_label(fields.update_route), "active_can_node": fields.active_can_node, "active_can_profile": fields.active_can_profile as u8, "active_can_profile_label": eha_sdk::diagnostics::can_profile_label(fields.active_can_profile), "host_heartbeat_hz": fields.host_heartbeat_hz, "telemetry_hz": fields.telemetry_hz, "host_contact_max_age_ms": fields.host_contact_max_age_ms, "config_format_version": fields.config_format_version, "high_water_operation_id": fields.high_water_operation_id, "retained_operation_id": fields.retained_operation_id, "text": {"version":fields.text.version, "source_revision":fields.text.source_revision, "build_information":fields.text.build_information, "odrive_binding":fields.text.odrive_binding, "odrive_observed_version":fields.text.odrive_observed_version, "brt27_binding":fields.text.brt27_binding, "brt27_observed_version":fields.text.brt27_observed_version, "update_binding":fields.text.update_binding}})
        }
        Response::Telemetry(response) | Response::Status(response) => {
            let fields = response.fields();
            let mut output = serde_json::Map::new();
            output.insert(
                "kind".into(),
                json!(if fields.sample.query_id == 0 {
                    "telemetry"
                } else {
                    "status"
                }),
            );
            output.insert("received_age_ms".into(), json!(age_ms));
            output.insert("sample".into(), sample_json(fields.sample));
            output.insert("decision_age_us".into(), json!(fields.decision_age_us));
            output.insert("target_mode".into(), json!(fields.target_mode as u8));
            output.insert(
                "target_mode_label".into(),
                json!(eha_sdk::diagnostics::target_mode_label(fields.target_mode)),
            );
            output.insert("target_ingress".into(), json!(fields.target_ingress as u8));
            output.insert(
                "target_ingress_label".into(),
                json!(eha_sdk::diagnostics::target_ingress_label(
                    fields.target_ingress
                )),
            );
            output.insert("target_values".into(), json!(fields.target_values));
            output.insert(
                "can_adoption_blockers".into(),
                json!(fields.can_adoption_blockers),
            );
            output.insert(
                "can_adoption_blocker_labels".into(),
                json!(eha_sdk::diagnostics::reason_bitmap_labels(
                    fields.can_adoption_blockers
                )),
            );
            output.insert(
                "usb_adoption_blockers".into(),
                json!(fields.usb_adoption_blockers),
            );
            output.insert(
                "usb_adoption_blocker_labels".into(),
                json!(eha_sdk::diagnostics::reason_bitmap_labels(
                    fields.usb_adoption_blockers
                )),
            );
            output.insert("output_blockers".into(), json!(fields.output_blockers));
            output.insert(
                "output_blocker_labels".into(),
                json!(eha_sdk::diagnostics::reason_bitmap_labels(
                    fields.output_blockers
                )),
            );
            output.insert("last_end_reason".into(), json!(fields.last_end_reason));
            output.insert(
                "last_end_reason_label".into(),
                json!(eha_sdk::diagnostics::reason_label(fields.last_end_reason)),
            );
            output.insert("facts".into(), json!({"output_allowed":fields.facts.output_allowed,"reference_limited":fields.facts.reference_limited,"candidate_limited":fields.facts.candidate_limited,"position_direction_inhibited":fields.facts.position_direction_inhibited,"device_operation_pending":fields.facts.device_operation_pending,"retained_result":fields.facts.retained_result,"unknown_effect":fields.facts.unknown_effect}));
            output.insert("limits".into(), limits_json(fields.limits));
            output.insert(
                "main_values".into(),
                Value::Array(fields.main_values.map(value_json).into()),
            );
            output.insert("position_age_us".into(), json!(fields.position_age_us));
            output.insert("velocity_age_us".into(), json!(fields.velocity_age_us));
            output.insert(
                "pressure_pair_age_us".into(),
                json!(fields.pressure_pair_age_us),
            );
            output.insert("reference".into(), value_json(fields.reference));
            output.insert("reference_kind".into(), json!(fields.reference_kind as u8));
            output.insert(
                "reference_kind_label".into(),
                json!(eha_sdk::diagnostics::reference_kind_label(
                    fields.reference_kind
                )),
            );
            output.insert("candidate_rpm".into(), value_json(fields.candidate_rpm));
            output.insert(
                "last_submitted_rpm".into(),
                value_json(fields.last_submitted_rpm),
            );
            output.insert("submitted_age_us".into(), json!(fields.submitted_age_us));
            output.insert("axis_state_raw".into(), json!(fields.axis_state_raw));
            output.insert("axis_error_raw".into(), json!(fields.axis_error_raw));
            output.insert("driver_age_us".into(), json!(fields.driver_age_us));
            output.insert("driver_state".into(), json!({"has_status":fields.driver_state.has_status,"qualified":fields.driver_state.qualified,"stale":fields.driver_state.stale,"faulted":fields.driver_state.faulted}));
            output.insert(
                "driver_summary".into(),
                driver_summary_json(
                    fields.driver_state,
                    fields.driver_age_us,
                    fields.axis_state_raw,
                    fields.axis_error_raw,
                ),
            );
            output.insert("desired_axis".into(), json!(fields.desired_axis as u8));
            output.insert(
                "desired_axis_label".into(),
                json!(eha_sdk::diagnostics::desired_axis_label(
                    fields.desired_axis
                )),
            );
            output.insert(
                "can_heartbeat_age_us".into(),
                json!(fields.can_heartbeat_age_us),
            );
            output.insert(
                "usb_heartbeat_age_us".into(),
                json!(fields.usb_heartbeat_age_us),
            );
            output.insert("can_contact".into(), json!(fields.can_contact as u8));
            output.insert(
                "can_contact_label".into(),
                json!(eha_sdk::diagnostics::contact_state_label(
                    fields.can_contact
                )),
            );
            output.insert("usb_contact".into(), json!(fields.usb_contact as u8));
            output.insert(
                "usb_contact_label".into(),
                json!(eha_sdk::diagnostics::contact_state_label(
                    fields.usb_contact
                )),
            );
            Value::Object(output)
        }
        Response::Measurements(response) => {
            let fields = response.fields();
            json!({"kind":"measurements","received_age_ms":age_ms,"sample":sample_json(fields.sample),"position_mm":value_json(fields.values.position_mm),"velocity_mm_s":value_json(fields.values.velocity_mm_s),"raw_pressure_a_mpa":value_json(fields.values.raw_pressure_a_mpa),"raw_pressure_b_mpa":value_json(fields.values.raw_pressure_b_mpa),"filtered_pressure_a_mpa":value_json(fields.values.filtered_pressure_a_mpa),"filtered_pressure_b_mpa":value_json(fields.values.filtered_pressure_b_mpa),"main_force_n":value_json(fields.values.main_force_n),"protection_force_n":value_json(fields.values.protection_force_n),"position_age_us":fields.position_age_us,"velocity_age_us":fields.velocity_age_us,"pressure_pair_age_us":fields.pressure_pair_age_us,"position_reference_count":fields.position_reference_count,"effective_area_mm2":fields.effective_area_mm2,"positive_force_channel":fields.positive_force_channel as u8,"positive_force_channel_label":eha_sdk::diagnostics::positive_force_channel_label(fields.positive_force_channel),"reference_state":fields.reference_state,"model_state":fields.model_state as u8,"model_state_label":eha_sdk::diagnostics::model_state_label(fields.model_state),"pressure_batch_sequence":fields.pressure_batch_sequence})
        }
        Response::Diagnostics(response) => {
            let entries: Vec<_> = response
                .entries()
                .map(|entry| diagnostic_entry_json(entry.fields()))
                .collect();
            json!({"kind":"diagnostics","received_age_ms":age_ms,"sample":sample_view_json(response.sample()),"overflow":response.overflow(),"entries":entries})
        }
        Response::ConfigData(response) => {
            let fields = response.fields();
            json!({"kind":"config_data","received_age_ms":age_ms,"sample":sample_json(fields.sample),"view":fields.view as u8,"view_label":eha_sdk::diagnostics::config_view_label(fields.view),"record_state":fields.record_state as u8,"record_state_label":eha_sdk::diagnostics::config_record_state_label(fields.record_state),"startup_source":fields.startup_source as u8,"startup_source_label":eha_sdk::diagnostics::startup_source_label(fields.startup_source),"fallback_reason":fields.fallback_reason as u8,"fallback_reason_label":eha_sdk::diagnostics::fallback_reason_label(fields.fallback_reason),"data_time_us":fields.data_time_us,"data_utf8":std::str::from_utf8(response.data()).ok(),"data_hex":hex(response.data())})
        }
        Response::OperationResult(response) => {
            json!({"kind":"operation_result","received_age_ms":age_ms,"fields":operation_fields_json(response.fields())})
        }
        Response::DataUnavailable(response) => {
            let fields = response.fields();
            json!({"kind":"data_unavailable","received_age_ms":age_ms,"fields":data_unavailable_fields_json(fields)})
        }
    };
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::{
        Command, TelemetryTrend, TelemetryTrendPoint, ToolSession, data_unavailable_fields_json,
        diagnostic_entry_json, driver_summary_json, sdk_error,
    };
    use eha_sdk::{
        host::{
            Client, Error, Failure, LocalStage, Wait,
            backend::{Backend, Pump},
        },
        protocol::{self, Direction, Message, SampleData, responses::*},
    };
    use serde_json::json;
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    fn sample() -> SampleData {
        SampleData {
            query_id: 7,
            run_nonce: [3; 16],
            snapshot_sequence: 11,
            snapshot_time_us: 13,
        }
    }

    fn value() -> ValueFields {
        ValueFields::new(
            1.0,
            ValueState::new(ValueResult::Available, SourceQuality::Qualified, false),
        )
    }

    fn trend_point(cursor: u64, time_us: u64) -> TelemetryTrendPoint {
        TelemetryTrendPoint {
            cursor,
            time_us,
            sequence: cursor as u32,
            position: json!({"value": cursor}),
            velocity: json!({"value": cursor}),
            force: json!({"value": cursor}),
            target_mode: 1,
            target_values: [cursor as f32, 0.0, 0.0],
            gap: false,
        }
    }

    fn identity_bytes(
        query_id: u32,
        high_water_operation_id: u64,
        retained_operation_id: u64,
    ) -> Vec<u8> {
        let fields = IdentityFields {
            sample: SampleData {
                query_id,
                run_nonce: [9; 16],
                snapshot_sequence: 1,
                snapshot_time_us: 10,
            },
            uid: [7; 12],
            config_source: StartupSource::User,
            fallback_reason: FallbackReason::None,
            run_identity: RunIdentity::Random,
            update_route: UpdateRoute::Absent,
            active_can_node: 1,
            active_can_profile: CanProfile::Fd1m2m,
            host_heartbeat_hz: 100,
            telemetry_hz: 20,
            host_contact_max_age_ms: 300,
            config_format_version: 1,
            build_evidence: EvidenceState::Available,
            odrive_evidence: EvidenceState::Unavailable,
            brt27_evidence: EvidenceState::Unavailable,
            high_water_operation_id,
            retained_operation_id,
            text: IdentityText {
                version: "test",
                source_revision: "",
                build_information: "",
                odrive_binding: "",
                odrive_observed_version: "",
                brt27_binding: "",
                brt27_observed_version: "",
                update_binding: "",
            },
        };
        let mut bytes = vec![0; fields.encoded_len().expect("identity length")];
        let length = encode_identity(&fields, &mut bytes).expect("identity encodes");
        bytes.truncate(length);
        bytes
    }

    fn maintenance_backend(
        high_water: Arc<AtomicU64>,
        retained_operation: Arc<AtomicU64>,
        reply_identity: Arc<AtomicBool>,
        operations: Arc<Mutex<Vec<u64>>>,
    ) -> Backend {
        let identity_queries = Arc::new(AtomicUsize::new(0));
        Backend::spawn("eha-tool-session-test", move |commands, sink| {
            let mut pump = Pump::new(commands, sink);
            while pump.poll() {
                for lane in 0..3 {
                    while let Some(request) = pump.take(lane) {
                        let message = protocol::decode(&request.bytes, Direction::HostToFirmware)
                            .expect("tool emits a contract-valid request");
                        match message {
                            Message::Query {
                                query_id,
                                category: 0,
                            } => {
                                pump.sink.submitted(request.id);
                                let index = identity_queries.fetch_add(1, Ordering::AcqRel);
                                if index == 0 || reply_identity.load(Ordering::Acquire) {
                                    pump.sink.received(identity_bytes(
                                        query_id,
                                        high_water.load(Ordering::Acquire),
                                        retained_operation.load(Ordering::Acquire),
                                    ));
                                }
                            }
                            Message::ReadResult { query_id, key } => {
                                pump.sink.submitted(request.id);
                                pump.sink
                                    .received(released_result_bytes(query_id, key.operation_id()));
                            }
                            Message::Maintenance { key, .. } => {
                                operations
                                    .lock()
                                    .expect("operation list lock")
                                    .push(key.operation_id());
                                pump.sink.submitted(request.id);
                            }
                            _ => panic!("unexpected SDK request in maintenance test"),
                        }
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        })
        .expect("memory backend starts")
    }

    fn released_result_bytes(query_id: u32, operation_id: u64) -> Vec<u8> {
        let fields = DataUnavailableFields {
            sample: SampleData {
                query_id,
                run_nonce: [9; 16],
                snapshot_sequence: 2,
                snapshot_time_us: 20,
            },
            subject: UnavailableSubject::OperationResult,
            reason: 0x0306,
            next_actions: 0,
            retained: false,
            operation_id,
        };
        let mut bytes = [0; DataUnavailableFields::encoded_len()];
        let length =
            encode_data_unavailable(&fields, &mut bytes).expect("released-result response encodes");
        bytes[..length].to_vec()
    }

    fn identified_tool_session(backend: Backend, timeout: Duration) -> ToolSession {
        let mut client = Client::new(backend);
        let identity = client
            .identify(None, &Wait::new(timeout))
            .expect("initial Identity succeeds");
        let mut session = ToolSession::new().with_timeout(timeout);
        session.client = Some(client);
        session.identity = Some(identity);
        session
    }

    #[test]
    fn webui_identity_refresh_advances_maintenance_key_to_latest_high_water() {
        let high_water = Arc::new(AtomicU64::new(4));
        let retained_operation = Arc::new(AtomicU64::new(0));
        let reply_identity = Arc::new(AtomicBool::new(true));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let backend = maintenance_backend(
            Arc::clone(&high_water),
            Arc::clone(&retained_operation),
            Arc::clone(&reply_identity),
            Arc::clone(&operations),
        );
        let mut session = identified_tool_session(backend, Duration::from_millis(100));

        high_water.store(9, Ordering::Release);
        session
            .refresh_identity()
            .expect("Identity refresh succeeds");
        session
            .execute(Command::ResetApplication)
            .expect("maintenance request submits after refresh");

        assert_eq!(*operations.lock().expect("operation list lock"), vec![10]);
    }

    #[test]
    fn failed_webui_identity_refresh_disconnects_and_blocks_new_maintenance() {
        let high_water = Arc::new(AtomicU64::new(4));
        let retained_operation = Arc::new(AtomicU64::new(0));
        let reply_identity = Arc::new(AtomicBool::new(true));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let backend = maintenance_backend(
            Arc::clone(&high_water),
            Arc::clone(&retained_operation),
            Arc::clone(&reply_identity),
            Arc::clone(&operations),
        );
        let mut session = identified_tool_session(backend, Duration::from_millis(30));

        reply_identity.store(false, Ordering::Release);
        let error = session
            .refresh_identity()
            .expect_err("missing refreshed Identity must fail");
        assert_eq!(error.kind.as_ref(), "timeout");
        let snapshot = session.snapshot();
        assert!(!snapshot.connected);
        assert!(snapshot.identity.is_none());
        assert!(session.execute(Command::ResetApplication).is_err());
        assert!(operations.lock().expect("operation list lock").is_empty());
    }

    #[test]
    fn identity_refresh_confirms_a_released_other_transport_key_before_new_maintenance() {
        let high_water = Arc::new(AtomicU64::new(5));
        let retained_operation = Arc::new(AtomicU64::new(5));
        let reply_identity = Arc::new(AtomicBool::new(true));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let backend = maintenance_backend(
            Arc::clone(&high_water),
            Arc::clone(&retained_operation),
            Arc::clone(&reply_identity),
            Arc::clone(&operations),
        );
        let mut session = identified_tool_session(backend, Duration::from_millis(100));

        // 同一控制器上的另一通路已释放此前留存键；本通路只能通过其精确 0x0306
        // ReadResult 确认后清除本地 pending，而不能只根据新的 Identity 猜测。
        retained_operation.store(0, Ordering::Release);
        session
            .refresh_identity()
            .expect("exact released result clears the old pending key");
        session
            .execute(Command::ResetApplication)
            .expect("new maintenance key submits only after release confirmation");

        assert_eq!(*operations.lock().expect("operation list lock"), vec![6]);
    }

    #[test]
    fn full_trend_keeps_all_samples_with_bounded_payload() {
        let mut trend = TelemetryTrend::new(4096);
        for cursor in 1..=3000 {
            let mut point = trend_point(cursor, 7_000_000_000 + cursor * 10_000);
            point.position = super::trend_value_json(value());
            point.velocity = super::trend_value_json(value());
            point.force = super::trend_value_json(value());
            trend.points.push_back(point);
        }
        let response = trend.json(None);
        assert_eq!(response["points"].as_array().map(Vec::len), Some(3000));
        assert_eq!(response["points"][0]["velocity"]["quality"], 1);
        assert!(serde_json::to_vec(&response).expect("trend JSON").len() < 1_100_000);
    }

    #[test]
    fn trend_cursor_is_non_destructive_for_two_browsers_and_source_reset() {
        let mut trend = TelemetryTrend::new(4);
        trend.set_source(Some(
            json!({"connection":"usb","uid":"a","run_nonce":"one"}),
        ));
        trend.next_cursor = 3;
        trend.latest_time_us = Some(3_000);
        trend.points.extend([
            trend_point(1, 1_000),
            trend_point(2, 2_000),
            trend_point(3, 3_000),
        ]);

        let first_browser = trend.json(None);
        assert_eq!(first_browser["reset"], true);
        assert_eq!(first_browser["cursor"], 3);
        assert_eq!(first_browser["points"].as_array().map(Vec::len), Some(3));
        let second_browser = trend.json(Some(1));
        assert_eq!(second_browser["reset"], false);
        assert_eq!(second_browser["points"].as_array().map(Vec::len), Some(2));
        let first_increment = trend.json(Some(2));
        assert_eq!(first_increment["points"][0]["cursor"], 3);

        trend.set_source(Some(
            json!({"connection":"usb","uid":"a","run_nonce":"one"}),
        ));
        assert_eq!(trend.json(Some(3))["reset"], false, "同一来源不清空趋势");
        trend.set_source(Some(
            json!({"connection":"usb","uid":"a","run_nonce":"two"}),
        ));
        let reset = trend.json(Some(3));
        assert_eq!(reset["reset"], true);
        assert_eq!(reset["source"]["generation"], 4);
        assert_eq!(reset["cursor"], 3, "重置后游标仍在同一会话单调递增");
    }

    #[test]
    fn diagnostic_keeps_native_source_and_incomplete_evidence() {
        let projected = diagnostic_entry_json(DiagnosticFields {
            domain: DiagnosticDomain::Driver,
            object: DiagnosticObject::DriverAxis,
            impact: DiagnosticImpact {
                current: true,
                limits_output: true,
                ..DiagnosticImpact::default()
            },
            phase: OperationPhase::WaitingFeedback,
            reason: 0x000b,
            next_actions: 1 << 8,
            event_time_us: 101,
            started_time_us: 99,
            deadline_us: 120,
            operation_id: 17,
            native_code: 0x0802,
            native_domain: NativeDomain::OdriveAxisError,
            evidence: DiagnosticEvidence {
                send_submitted: true,
                ..DiagnosticEvidence::default()
            },
            detail_value: value(),
            detail_unit: DetailUnit::None,
            constraints: Constraints {
                driver_transition: true,
                ..Constraints::default()
            },
            occurrences: 2,
            missing_evidence: MissingEvidence {
                device_feedback: true,
                ..MissingEvidence::default()
            },
        });

        assert_eq!(
            projected["native_domain"],
            NativeDomain::OdriveAxisError as u8
        );
        assert_eq!(projected["phase"], OperationPhase::WaitingFeedback as u8);
        assert_eq!(projected["evidence"]["send_submitted"], true);
        assert_eq!(projected["constraints"]["driver_transition"], true);
        assert_eq!(projected["missing_evidence"]["device_feedback"], true);
        assert_eq!(projected["detail"]["result_label"], "可表示值");
        assert_eq!(projected["detail"]["quality_label"], "来源合格");
        assert!(
            projected["native_code_description"]
                .as_str()
                .is_some_and(|text| text.contains("watchdog"))
        );
    }

    #[test]
    fn unavailable_error_preserves_the_full_firmware_reply() {
        let fields = DataUnavailableFields {
            sample: sample(),
            subject: UnavailableSubject::Diagnostics,
            reason: 0x0307,
            next_actions: (1 << 0) | (1 << 9),
            retained: true,
            operation_id: 41,
        };
        let error = sdk_error(Error {
            failure: Failure::DataUnavailable(fields),
            local: LocalStage::FullySubmitted,
            operation: None,
            last_result: None,
            observed_transport: None,
        });

        assert_eq!(
            error.failure.as_deref(),
            Some(&data_unavailable_fields_json(fields))
        );
        assert_eq!(
            error
                .failure
                .as_deref()
                .and_then(|value| value["retained"].as_bool()),
            Some(true)
        );
        assert_eq!(
            error
                .failure
                .as_deref()
                .and_then(|value| value["operation_id"].as_u64()),
            Some(41)
        );
    }

    #[test]
    fn stale_or_missing_driver_state_never_claims_current_idle() {
        let unavailable = driver_summary_json(DriverState::default(), 0, 1, 0);
        let stale = driver_summary_json(
            DriverState {
                has_status: true,
                qualified: true,
                stale: true,
                faulted: false,
            },
            200_000,
            1,
            0,
        );

        assert_eq!(unavailable["state"], "unavailable");
        assert!(unavailable["axis_state_label"].is_null());
        assert_eq!(stale["state"], "stale");
        assert!(stale["axis_state_label"].is_null());
    }
}

#[cfg(test)]
#[path = "trial_tests.rs"]
pub(crate) mod trial_tests;

/// 仅接受 SDK 实际 f32 序列化的内部遥测 Number，展示字符串不参与保护判断。
pub(crate) fn observed_f32(value: &Value) -> Option<f32> {
    let Value::Number(number) = value else {
        return None;
    };
    eha_sdk::config::parse_f32(&number.to_string()).ok()
}
