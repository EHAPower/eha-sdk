// Copyright The eha-sdk Contributors

//! 持久桌面工具会话。
//!
//! 本模块只把 SDK 的单一 `Client` 会话提供给 CLI、Shell 和 WebUI。它不重新实现
//! 协议、传输或控制规则；关闭和通信恢复绝不发送 Stop 或重放任何动作。

use std::{
    collections::VecDeque,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use crate::owner_lock::{OwnerGuard, ResourceKey};
use eha_sdk::{
    can::{CanConnector, CanNodeConnector, CanOptions},
    host::{Client, ClientState, Error as SdkError, Failure, LocalSubmission, Reply, Wait},
    protocol::{
        Response,
        responses::{ConfigView, DesiredAxis, TargetMode, ValueFields},
    },
    usb::UsbConnector,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[path = "session_retirement.rs"]
mod retirement;

#[path = "session_control.rs"]
mod control;

#[path = "session_maintenance.rs"]
mod maintenance;
pub(crate) use maintenance::{MaintenanceCompletion, MaintenancePhase};

#[path = "recording.rs"]
mod recording;
#[path = "session_recording.rs"]
mod recording_tasks;
#[path = "session_scheduler.rs"]
mod scheduler;
#[path = "trial.rs"]
mod trial;
#[path = "session_trial_preflight.rs"]
mod trial_preflight;

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
    /// `eha-tool` 持有的 CANable2 档案。实际串口路径只在每次打开时按 USB 序列号解析。
    #[serde(rename = "can")]
    ManagedCan {
        serial: String,
        node: u8,
        mode: String,
        python: Option<String>,
        expected_uid: Option<String>,
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

    pub(crate) fn expected_uid(&self) -> Option<&str> {
        match self {
            Self::ManagedCan { expected_uid, .. } => expected_uid.as_deref(),
            Self::Usb { .. } | Self::Can { .. } => None,
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
    ConfigSaveAndRestart {
        record: String,
    },
    RestoreFactoryAndRestart,
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
            "config_save_and_restart" => Self::ConfigSaveAndRestart {
                record: fields.decode_field("record")?,
            },
            "restore_factory_and_restart" => Self::RestoreFactoryAndRestart,
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
    pub unknown: bool,
    /// 本次失败携带的完整协议事实；它不等同于本地提交或设备执行结论。
    pub failure: Option<Box<Value>>,
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
    fn acquire_usb_owner(
        &self,
        request: &ConnectionRequest,
    ) -> Result<Option<OwnerGuard>, SessionError> {
        // CAN guard 由物理 channel worker 持有；共享节点没有第二个物理 owner。
        if let (Self::Usb(_), ConnectionRequest::Usb { serial }) = (self, request) {
            let key = ResourceKey::usb(serial).map_err(owner_error)?;
            return OwnerGuard::acquire(&key).map(Some).map_err(owner_error);
        }
        Ok(None)
    }

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
static NEXT_CONNECTION_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_connection_generation() -> u64 {
    NEXT_CONNECTION_GENERATION.fetch_add(1, Ordering::Relaxed)
}

pub struct ToolSession {
    connector: Option<Connector>,
    request: Option<ConnectionRequest>,
    client: Option<Client>,
    // 字段 drop 顺序保证 Backend 关闭并 join worker 后才释放 USB 通路。
    usb_owner: Option<OwnerGuard>,
    retirement_slot: Option<retirement::RetirementSlot>,
    retiring: Option<retirement::Retirement>,
    disconnected: Option<ClientState>,
    timeout: Duration,
    identity: Option<Reply>,
    last_telemetry: Option<Reply>,
    observed: Option<eha_sdk::host::ClientObservation>,
    last_status: Option<Reply>,
    last_measurements: Option<Reply>,
    last_diagnostics: Option<Reply>,
    disconnected_reason: Option<String>,
    telemetry_trend: Option<TelemetryTrend>,
    telemetry_queue_capacity: Option<usize>,
    recording: Option<Recording>,
    recording_error: Option<String>,
    trial: TrialState,
    connection_generation: u64,
    pending_target: bool,
    pending_read: Option<scheduler::ReadTask>,
    pending_preflight: Option<trial_preflight::PendingTrial>,
    pending_recording_start: Option<recording_tasks::RecordingStart>,
    pending_recording_finish: Option<recording_tasks::RecordingFinish>,
    pending_trial_start: Option<recording_tasks::TrialStart>,
    position_audit: Option<(f32, u64)>,
    position_submission_mm: Option<f32>,
    maintenance_pending: Option<maintenance::Pending>,
    pending_control: Option<control::PendingControl>,
    pending_trial_target: Option<control::TrialTarget>,
}

impl Drop for ToolSession {
    fn drop(&mut self) {
        if let Some(slot) = self.retirement_slot.take()
            && let Some(client) = self.client.take()
        {
            drop(retirement::Retirement::begin(
                client,
                self.usb_owner.take(),
                slot,
            ));
        }
    }
}

impl Default for ToolSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolSession {
    /// Tests may label a synthetic backend as a USB scope without opening hardware.
    #[cfg(test)]
    pub(crate) fn test_usb_request(&mut self, serial: &str) {
        self.request = Some(ConnectionRequest::Usb {
            serial: serial.into(),
        });
    }

    pub fn new() -> Self {
        Self {
            connector: None,
            request: None,
            client: None,
            usb_owner: None,
            retirement_slot: None,
            retiring: None,
            disconnected: None,
            timeout: Duration::from_secs(5),
            identity: None,
            last_telemetry: None,
            observed: None,
            last_status: None,
            last_measurements: None,
            last_diagnostics: None,
            disconnected_reason: None,
            telemetry_trend: None,
            telemetry_queue_capacity: None,
            recording: None,
            recording_error: None,
            trial: TrialState::default(),
            connection_generation: next_connection_generation(),
            pending_target: false,
            pending_read: None,
            pending_preflight: None,
            pending_recording_start: None,
            pending_recording_finish: None,
            pending_trial_start: None,
            position_audit: None,
            position_submission_mm: None,
            maintenance_pending: None,
            pending_control: None,
            pending_trial_target: None,
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
    pub fn connect(&mut self, mut request: ConnectionRequest) -> Result<Snapshot, SessionError> {
        self.require_maintenance_idle()?;
        self.require_retirement_idle()?;
        if self.trial.active.is_some() {
            return Err(invalid("试验进行中；拒绝切换连接"));
        }
        if self.client.is_some() {
            return Err(invalid("已有活跃会话；先显式 disconnect"));
        }
        if self.disconnected.is_some() && self.request.as_ref() == Some(&request) {
            return self.reconnect();
        }
        self.connection_generation = next_connection_generation();
        self.clear_observations();
        let mut connector = connector_for(&request)?;
        let owner = connector.acquire_usb_owner(&request)?;
        let retirement_slot = retirement::RetirementSlot::reserve().map_err(invalid)?;
        let backend = connector.open().map_err(open_error)?;
        let mut client = Client::new(backend);
        if let Some(capacity) = self.telemetry_queue_capacity {
            client.enable_telemetry_queue(capacity).map_err(sdk_error)?;
        }
        let identity = match client.identify(None, &self.wait()) {
            Ok(identity) => identity,
            Err(error) => {
                self.disconnected_reason = client.connection_status().disconnected;
                self.request = Some(request);
                self.connector = Some(connector);
                self.disconnected = Some(client.disconnect());
                return Err(sdk_error(error));
            }
        };
        expected_uid_matches(&request, &identity)?;
        bind_managed_request(&mut request, &identity)?;
        self.identity = Some(identity);
        self.reconcile_interrupted_trial();
        self.request = Some(request);
        self.connector = Some(connector);
        self.client = Some(client);
        self.retirement_slot = Some(retirement_slot);
        self.usb_owner = owner;
        self.disconnected = None;
        self.disconnected_reason = None;
        Ok(self.snapshot())
    }

    /// 绑定到已建立的共享 CAN 网络中的一个节点；网络本身由调用方唯一拥有。
    pub fn connect_shared_can(
        &mut self,
        mut request: ConnectionRequest,
        connector: CanNodeConnector,
    ) -> Result<Snapshot, SessionError> {
        self.require_maintenance_idle()?;
        self.require_retirement_idle()?;
        if self.trial.active.is_some() {
            return Err(invalid("试验进行中；拒绝切换连接"));
        }
        if !matches!(
            request,
            ConnectionRequest::Can { .. } | ConnectionRequest::ManagedCan { .. }
        ) {
            return Err(invalid("共享 CAN 节点必须使用 CAN 连接请求"));
        }
        if self.client.is_some() {
            return Err(invalid("已有活跃会话；先显式 disconnect"));
        }
        self.connection_generation = next_connection_generation();
        self.clear_observations();
        let mut shared = Connector::SharedCan(connector);
        let retirement_slot = retirement::RetirementSlot::reserve().map_err(invalid)?;
        let backend = shared.open().map_err(open_error)?;
        let mut client = Client::new(backend);
        if let Some(capacity) = self.telemetry_queue_capacity {
            client.enable_telemetry_queue(capacity).map_err(sdk_error)?;
        }
        let identity = match client.identify(None, &self.wait()) {
            Ok(identity) => identity,
            Err(error) => {
                self.disconnected_reason = client.connection_status().disconnected;
                self.request = Some(request);
                self.connector = Some(shared);
                self.disconnected = Some(client.disconnect());
                return Err(sdk_error(error));
            }
        };
        expected_uid_matches(&request, &identity)?;
        bind_managed_request(&mut request, &identity)?;
        self.identity = Some(identity);
        self.reconcile_interrupted_trial();
        self.request = Some(request);
        self.connector = Some(shared);
        self.client = Some(client);
        self.retirement_slot = Some(retirement_slot);
        self.disconnected = None;
        self.disconnected_reason = None;
        Ok(self.snapshot())
    }

    /// 只关闭本地 I/O 并保存 ClientState；不发送 Stop、Reset 或重放目标。
    pub fn disconnect(&mut self) -> Result<Snapshot, SessionError> {
        self.require_maintenance_idle()?;
        self.require_retirement_idle()?;
        if self.trial.active.is_some() {
            return Err(invalid("试验进行中；普通 disconnect 不停止也不切换设备"));
        }
        if self.client.is_none() {
            return Err(invalid("当前没有活跃会话"));
        }
        self.cancel_pending_operations("disconnect");
        self.connection_generation = next_connection_generation();
        self.finish_recording_boundary("disconnect");
        self.mark_interrupted_reconnect_required();
        self.trial.prepared = None;
        let client = self
            .client
            .take()
            .ok_or_else(|| invalid("当前没有活跃会话"))?;
        self.capture_telemetry(&client);
        self.disconnected_reason = client.connection_status().disconnected;
        self.disconnected = Some(client.disconnect());
        self.retirement_slot = None;
        self.usb_owner = None;
        Ok(self.snapshot())
    }

    /// 在同一个 Connector 上恢复传输编号和业务关联，随后重新核对 Identity。
    pub fn reconnect(&mut self) -> Result<Snapshot, SessionError> {
        self.require_maintenance_idle()?;
        self.require_retirement_idle()?;
        if self
            .client
            .as_ref()
            .is_some_and(|client| client.connection_status().disconnected.is_none())
        {
            return Err(invalid("会话仍处于连接状态；无需 reconnect"));
        }
        if self.client.is_none() && self.disconnected.is_none() {
            return Err(invalid("没有可恢复的断开会话"));
        }
        self.cancel_pending_operations("reconnect");
        self.connection_generation = next_connection_generation();
        if self.trial.active.is_some() {
            self.interrupt_trial_unknown("reconnect_during_active_trial");
        }
        if let Some(client) = self.client.take() {
            self.capture_telemetry(&client);
            self.disconnected_reason = client.connection_status().disconnected;
            self.disconnected = Some(client.disconnect());
            self.retirement_slot = None;
            self.usb_owner = None;
        }
        let previous = self.identity.as_ref().and_then(identity_facts);
        let owner = self
            .connector
            .as_ref()
            .ok_or_else(|| invalid("没有连接器"))?
            .acquire_usb_owner(
                self.request
                    .as_ref()
                    .ok_or_else(|| invalid("没有连接请求"))?,
            )?;
        let retirement_slot = retirement::RetirementSlot::reserve().map_err(invalid)?;
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
        if let Err(error) = expected_uid_matches(
            self.request
                .as_ref()
                .ok_or_else(|| invalid("没有连接请求"))?,
            &identity,
        ) {
            self.disconnected = Some(client.disconnect());
            self.identity = None;
            self.clear_observations();
            return Err(error);
        }
        if let Some(request) = self.request.as_mut() {
            bind_managed_request(request, &identity)?;
        }
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
        self.retirement_slot = Some(retirement_slot);
        self.usb_owner = owner;
        self.disconnected_reason = None;
        Ok(self.snapshot())
    }

    /// 共享 CAN owner 不变时采用新的受管档案身份约束。它不打开 I/O、不发送帧；若已
    /// 有缓存 Identity，先以该事实拒绝明显的档案错配，下一次 reconnect 仍会核对新回复。
    pub fn adopt_managed_request(
        &mut self,
        mut request: ConnectionRequest,
    ) -> Result<(), SessionError> {
        if !matches!(request, ConnectionRequest::ManagedCan { .. }) {
            return Err(invalid("只能采用受管 CANable2 连接档案"));
        }
        if let Some(identity) = self.identity.as_ref() {
            expected_uid_matches(&request, identity)?;
            bind_managed_request(&mut request, identity)?;
        }
        self.request = Some(request);
        Ok(())
    }

    pub fn snapshot(&mut self) -> Snapshot {
        self.retire_disconnected_usb();
        if let Some(client) = self.client.take() {
            self.capture_telemetry(&client);
            self.client = Some(client);
        }
        self.snapshot_cached()
    }

    fn snapshot_cached(&self) -> Snapshot {
        let observation = self.observed.clone().unwrap_or_default();
        let heartbeat = observation.heartbeat;
        let adapter = observation.adapter;
        Snapshot {
            connected: self.client.is_some(),
            connection: self.request.clone(),
            identity: self
                .identity
                .as_ref()
                .and_then(|reply| reply_json(reply).ok()),
            heartbeat: json!({"enabled":heartbeat.enabled,"submitted":heartbeat.submitted,"missed":heartbeat.missed,"error":heartbeat.error}),
            transport: transport_json(
                &observation.transport,
                observation
                    .connection
                    .disconnected
                    .or_else(|| self.disconnected_reason.clone()),
            ),
            adapter: json!({"notices":adapter.notices,"last_notice":adapter.last_notice.as_ref().map(|(detail,raw,_)|json!({"detail":detail,"raw_hex":hex(raw)}))}),
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
        }
    }

    /// 执行普通 SDK 动作。试验活跃时，控制及阻塞查询必须通过试验入口协调。
    pub fn execute(&mut self, command: Command) -> Result<CommandResult, SessionError> {
        self.retire_disconnected_usb();
        self.prepare_read_boundary(&command)?;
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
        if matches!(
            command,
            Command::Position { .. }
                | Command::Velocity { .. }
                | Command::Force { .. }
                | Command::Impedance { .. }
        ) && self.client.is_some()
        {
            self.pending_target = true;
        }
        self.record_event("command_input", json!({"command": &command}));
        let result = self.execute_inner(command);
        match &result {
            Ok(value) => {
                if matches!(
                    value.action.as_str(),
                    "config_save_and_restart"
                        | "restore_factory"
                        | "reset_application"
                        | "enter_update"
                ) {
                    self.trial.prepared = None;
                }
                self.record_event(
                    "command_completed",
                    json!({
                        "action": value.action,
                        "local_submission": value.local_submission,
                        "data": value.data,
                    }),
                );
            }
            Err(error) => self.record_event("command_error", json!({"error": error})),
        }
        result
    }

    fn execute_inner(&mut self, command: Command) -> Result<CommandResult, SessionError> {
        self.prepare_read_boundary(&command)?;
        if let Command::ConfigValidate { record } = &command {
            eha_sdk::configuration::validate_json(record.as_bytes())
                .map_err(|error| invalid(error.to_string()))?;
            let snapshot = self.snapshot();
            return Ok(CommandResult {
                action: "config_validate".into(),
                local_submission: None,
                data: json!({"valid": true}),
                snapshot,
            });
        }

        let action = command_name(&command).to_owned();
        let wait = self.wait();
        let mut local_submission = None;
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
                Command::ConfigSaveAndRestart { record } => submission_data(
                    client
                        .save_and_restart(record.as_bytes(), &wait)
                        .map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::RestoreFactoryAndRestart => submission_data(
                    client
                        .restore_factory_and_restart(&wait)
                        .map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::ResetApplication => submission_data(
                    client.reset_application(&wait).map_err(sdk_error)?,
                    &mut local_submission,
                ),
                Command::EnterUpdate => submission_data(
                    client.enter_update(&wait).map_err(sdk_error)?,
                    &mut local_submission,
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
            data,
            snapshot,
        })
    }

    /// 开始一个独立目录的原始会话记录。记录启动不发送控制、心跳或维护动作。
    pub fn start_recording(&mut self, root: &Path) -> Result<Value, SessionError> {
        self.begin_recording_start(root)?;
        loop {
            match self.poll_recording_start() {
                std::task::Poll::Ready(result) => return result,
                std::task::Poll::Pending => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// Legacy synchronous wrapper; service owners use begin/poll_recording_stop.
    pub fn stop_recording(&mut self) -> Result<Value, SessionError> {
        self.begin_recording_stop()?;
        loop {
            match self.poll_recording_stop() {
                std::task::Poll::Ready(result) => return result,
                std::task::Poll::Pending => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    pub fn recording_snapshot(&self) -> Value {
        if let Some(value) = self.pending_recording_snapshot() {
            return value;
        }
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
        self.begin_trial_preflight(request.clone())?;
        loop {
            match self.poll_trial_preflight() {
                std::task::Poll::Ready(result) => return result,
                // Legacy callers remain synchronous. The service scheduler must use
                // begin/poll so Stop and other trial deadlines can run between steps.
                std::task::Poll::Pending => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// 对已预检且完全相同的单次请求提交一次持续需求。
    pub fn start_prepared_trial(&mut self, request: TrialRequest) -> Result<Value, SessionError> {
        self.begin_trial_start(request)?;
        loop {
            match self.poll_trial_start() {
                std::task::Poll::Ready(result) => return result,
                std::task::Poll::Pending => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    fn validate_prepared_trial_start(&self, request: &TrialRequest) -> Result<Reply, SessionError> {
        if self.trial.active.is_some() {
            return Err(invalid("已有进行中的试验"));
        }
        let prepared = self
            .trial
            .prepared
            .as_ref()
            .ok_or_else(|| invalid("请先 prepare_trial"))?;
        if serde_json::to_value(&prepared.request).ok() != serde_json::to_value(request).ok() {
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
            .and_then(|client| match client.try_observe(0) {
                std::task::Poll::Ready(Ok(observed)) => observed.telemetry,
                _ => None,
            })
            .ok_or_else(|| invalid("开始试验前需要同一运行实例的新鲜被动遥测"))?;
        let fresh_value = reply_json(&fresh)?;
        validate_trial_facts(
            request,
            &prepared.startup,
            &fresh_value,
            self.request.as_ref(),
        )?;
        Ok(fresh)
    }

    fn start_prepared_trial_after_audit(
        &mut self,
        request: TrialRequest,
    ) -> Result<(), SessionError> {
        self.last_telemetry = Some(self.validate_prepared_trial_start(&request)?);
        let prepared = self
            .trial
            .prepared
            .take()
            .ok_or_else(|| invalid("请先 prepare_trial"))?;
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
            stop_confirmation_timeout: self.timeout,
            stop_confirmation_overdue: false,
            stop_recovery_after_received_at: None,
            settled_sample_time_us: None,
        });
        if let Err(error) = self.begin_initial_submission(request.command) {
            self.trial.active = None;
            return Err(error);
        }
        Ok(())
    }

    fn finish_initial_submission(
        &mut self,
        result: Result<CommandResult, SessionError>,
    ) -> Result<Value, SessionError> {
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.record_event("trial_command_error", json!({"error":&error}));
                if error.local_stage.as_deref() == Some("not_submitted") {
                    self.trial.active = None;
                } else {
                    let _ = self.submit_trial_stop("initial_submission_unknown");
                }
                return Err(error);
            }
        };
        if let Some(active) = self.trial.active.as_mut()
            && matches!(active.request.command, Command::Position { .. })
        {
            active.last_position_submit = Some(Instant::now());
        }
        self.record_event(
            "trial_command_submitted",
            json!({"local_submission":result.local_submission,"data":result.data}),
        );
        Ok(json!({"submitted":result,"trial":self.trial_snapshot()}))
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
        self.position_audit = None;
        self.record_event("trial_position_merged", json!({"mm": mm}));
        Ok(self.trial_snapshot())
    }

    /// 显式发送一次 Stop，并仅以其后的新遥测确认目标清除；任何失败均不重放。
    pub fn stop_trial(&mut self) -> Result<Value, SessionError> {
        self.resume_interrupted_trial_for_stop()?;
        self.submit_trial_stop("explicit")?;
        while self
            .pending_control
            .as_ref()
            .is_some_and(|pending| pending.autonomous_stop)
        {
            match self.poll_control() {
                std::task::Poll::Ready(result) => {
                    result?;
                    break;
                }
                std::task::Poll::Pending => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        Ok(self.trial_snapshot())
    }

    pub fn trial_snapshot(&self) -> Value {
        self.trial.snapshot()
    }

    /// 服务后台每约 10 ms 调用。它只消费 SDK 的被动遥测队列和推进已授权试验。
    pub fn tick(&mut self) {
        self.tick_with_clock(Instant::now);
    }

    #[cfg(test)]
    fn tick_at(&mut self, now: Instant) {
        self.tick_with_clock(|| now);
    }

    fn tick_with_clock(&mut self, mut now: impl FnMut() -> Instant) {
        self.retire_disconnected_usb();
        if let Some(client) = self.client.take() {
            self.capture_telemetry(&client);
            let disconnected = self
                .observed
                .as_ref()
                .and_then(|value| value.connection.disconnected.clone());
            self.client = Some(client);
            if let Some(reason) = disconnected {
                self.end_read(Failure::Disconnected(reason.clone()));
                self.fence_control("transport_disconnected");
                if !self.maintenance_pending() && self.disconnected_reason.is_none() {
                    self.connection_generation = next_connection_generation();
                    self.disconnected_reason = Some(reason.clone());
                }
                self.interrupt_trial_unknown(&format!("transport_disconnected:{reason}"));
                self.finish_recording_boundary("transport_disconnected");
            }
        }
        self.flush_recording();
        self.tick_recording_writer();
        let stop_reason = self.trial_stop_reason(now());
        if let Some(reason) = stop_reason {
            let _ = self.submit_trial_stop(&reason);
        }
        if self
            .pending_control
            .as_ref()
            .is_some_and(|pending| pending.autonomous_stop)
        {
            let _ = self.poll_control();
        }
        self.flush_trial_position(now());
        self.observe_trial_stop();
        self.observe_stop_confirmation_deadline_at(now());
        self.flush_recording();
    }

    /// 断连通知可能早于 USB endpoint 清理；必须 drop/join Backend 后才能归还物理锁。
    /// 保留 ClientState 与原身份/未知试验，不自动 Stop、心跳或续接。
    fn poll_retirement(&mut self) {
        let Some(retiring) = self.retiring.as_mut() else {
            return;
        };
        match retiring.poll() {
            std::task::Poll::Ready(Ok(state)) => {
                self.disconnected = Some(state);
                self.retiring = None;
            }
            // Error is not physical completion. Keep the handle and its lease.
            std::task::Poll::Ready(Err(_)) | std::task::Poll::Pending => {}
        }
    }

    fn require_retirement_idle(&mut self) -> Result<(), SessionError> {
        self.poll_retirement();
        if self.retiring.is_some() {
            Err(invalid("原 I/O owner 尚未完成关闭；不可开启新 Client"))
        } else {
            Ok(())
        }
    }

    fn retire_disconnected_usb(&mut self) {
        self.poll_retirement();
        if self.usb_owner.is_none() {
            return;
        }
        let Some(client) = self.client.take() else {
            return;
        };
        self.capture_telemetry(&client);
        self.client = Some(client);
        let Some(reason) = self
            .observed
            .as_ref()
            .and_then(|o| o.connection.disconnected.clone())
        else {
            return;
        };
        self.end_read(Failure::Disconnected(reason.clone()));
        self.fence_control("transport_disconnected");
        // Inbox 暂忙时保留原 Client/owner，下一 tick 继续领取维护终态；不阻塞退役调用。
        if self.maintenance_pending() {
            return;
        }
        if self.disconnected_reason.is_none() {
            self.connection_generation = next_connection_generation();
        }
        self.disconnected_reason = Some(reason.clone());
        self.interrupt_trial_unknown(&format!("transport_disconnected:{reason}"));
        self.mark_interrupted_reconnect_required();
        self.trial.prepared = None;
        self.finish_recording_boundary("transport_disconnected");
        // Official connection paths reserve before open. Synthetic/legacy sessions
        // may acquire here; on exhaustion keep their Client and physical owner.
        if self.retirement_slot.is_none() {
            self.retirement_slot = retirement::RetirementSlot::reserve().ok();
        }
        let Some(slot) = self.retirement_slot.take() else {
            return;
        };
        let client = self.client.take().expect("live client checked");
        self.retiring = Some(retirement::Retirement::begin(
            client,
            self.usb_owner.take(),
            slot,
        ));
        self.poll_retirement();
    }

    fn trial_stop_reason(&mut self, now: Instant) -> Option<String> {
        let active = self.trial.active.as_mut()?;
        if active.stop_submission.is_some() {
            return None;
        }
        if self.recording.is_some() && self.recording_error.is_some() {
            return Some("recording_failed".into());
        }
        if now.saturating_duration_since(active.started_at) >= active.duration {
            return Some("duration_elapsed".into());
        }
        let telemetry = match self
            .last_telemetry
            .as_ref()
            .and_then(|reply| reply_json(reply).ok())
        {
            Some(telemetry) => telemetry,
            None if now.saturating_duration_since(active.started_at)
                > Duration::from_millis(500) =>
            {
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
        if telemetry["facts"]["unknown_effect"].as_bool() != Some(false)
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
        let settled_since = active.settled_since.get_or_insert(now);
        (now.duration_since(*settled_since).as_millis() >= u128::from(reach.settle_ms))
            .then(|| "position_reached".into())
    }

    fn flush_trial_position(&mut self, now: Instant) {
        if self
            .pending_control
            .as_ref()
            .is_some_and(|pending| pending.trial_position)
        {
            if let std::task::Poll::Ready(result) = self.poll_control() {
                self.finish_position_submission(&result);
                if let Some(target) = self.pending_trial_target.as_mut() {
                    target.result = Some(result);
                }
            }
            return;
        }
        if self.pending_control.is_some() {
            return;
        }
        let position = {
            let Some(active) = self.trial.active.as_ref() else {
                return;
            };
            if active.stop_submission.is_some()
                || active.last_position_submit.is_some_and(|last| {
                    now.saturating_duration_since(last) < Duration::from_millis(50)
                })
            {
                return;
            }
            active.pending_position_mm
        };
        let Some(mm) = position else {
            return;
        };
        if self.recording.is_some() {
            if self.position_audit.is_none() {
                self.record_event("trial_position_submit", json!({"mm": mm}));
                match self
                    .recording
                    .as_mut()
                    .expect("recording checked")
                    .barrier()
                {
                    Ok(barrier) => self.position_audit = Some((mm, barrier)),
                    Err(error) => self.recording_error = Some(error),
                }
            }
            if self.recording_error.is_some() {
                let _ = self.submit_trial_stop("recording_failed");
                return;
            }
            if let Some((_, barrier)) = self.position_audit {
                match self
                    .recording
                    .as_mut()
                    .expect("recording checked")
                    .poll_barrier(barrier)
                {
                    std::task::Poll::Pending => return,
                    std::task::Poll::Ready(Err(error)) => {
                        self.recording_error = Some(error);
                        let _ = self.submit_trial_stop("recording_failed");
                        return;
                    }
                    std::task::Poll::Ready(Ok(())) => {}
                }
            }
        }
        self.position_audit = None;
        if let Err(error) = self.begin_position_submission(mm) {
            let result = Err(error);
            self.finish_position_submission(&result);
            if let Some(target) = self.pending_trial_target.as_mut() {
                target.result = Some(result);
            }
        }
    }

    fn finish_position_submission(&mut self, result: &Result<CommandResult, SessionError>) {
        let mm = self.position_submission_mm.take();
        let Some(mm) = mm else {
            return;
        };
        match result {
            Ok(result) => {
                if let Some(active) = self.trial.active.as_mut() {
                    if active
                        .pending_position_mm
                        .is_some_and(|pending| pending.to_bits() == mm.to_bits())
                    {
                        active.pending_position_mm = None;
                    }
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
        let active = self
            .trial
            .active
            .as_ref()
            .ok_or_else(|| invalid("当前没有进行中的试验"))?;
        if active.stop_submission.is_some() {
            return Ok(());
        }
        self.fence_control(reason);
        let result = self.begin_control(Command::Stop);
        if let Some(active) = self.trial.active.as_mut() {
            active.stop_reason = Some(reason.into());
        }
        if let Some(pending) = self.pending_control.as_mut() {
            pending.autonomous_stop = true;
        }
        result
    }

    fn observe_stop_confirmation_deadline(&mut self) {
        self.observe_stop_confirmation_deadline_at(Instant::now());
    }

    fn observe_stop_confirmation_deadline_at(&mut self, now: Instant) {
        let active = self.trial.active.as_mut().or_else(|| {
            self.trial
                .interrupted
                .as_mut()
                .map(|interrupted| &mut interrupted.active)
        });
        let Some(active) = active else {
            return;
        };
        if !active.stop_confirmation_overdue
            && active.stop_attempt_finished_at.is_some_and(|finished| {
                now.saturating_duration_since(finished) >= active.stop_confirmation_timeout
            })
        {
            // 超期只留存事实；继续持有试验并等待被动证据，不重发 Stop 或解锁命令。
            active.stop_confirmation_overdue = true;
            self.record_event("trial_stop_confirmation_overdue", self.trial.snapshot());
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
            let overdue = active.stop_confirmation_overdue
                || active
                    .stop_attempt_finished_at
                    .zip(received_at)
                    .is_some_and(|(finished, received)| {
                        received.saturating_duration_since(finished)
                            >= active.stop_confirmation_timeout
                    });
            let newly_overdue = overdue && !active.stop_confirmation_overdue;
            let completed = json!({
                "state": "completed",
                "request": active.request.clone(),
                "stop_reason": active.stop_reason.clone(),
                "stop_submission": active.stop_submission.clone(),
                "stop_observed": true,
                "stop_confirmation_timeout_ms": active.stop_confirmation_timeout.as_millis(),
                "stop_confirmation_overdue": overdue,
                "evidence": &telemetry,
            });
            if newly_overdue {
                self.observe_stop_confirmation_deadline();
            }
            self.record_event("trial_stop_confirmed", json!({"telemetry": &telemetry}));
            self.trial.completed = Some(completed);
            self.trial.active = None;
        }
    }

    fn wait(&self) -> Wait {
        Wait::new(self.timeout)
    }

    fn clear_observations(&mut self) {
        self.identity = None;
        self.last_telemetry = None;
        self.observed = None;
        self.last_status = None;
        self.last_measurements = None;
        self.last_diagnostics = None;
        self.trial.prepared = None;
        if let Some(trend) = self.telemetry_trend.as_mut() {
            trend.reset();
        }
    }

    fn capture_telemetry(&mut self, client: &Client) {
        let budget = if self.telemetry_queue_capacity.is_some() {
            64
        } else {
            0
        };
        let std::task::Poll::Ready(Ok(mut observation)) = client.try_observe(budget) else {
            return;
        };
        let drained = std::mem::take(&mut observation.drained);
        if self.telemetry_queue_capacity.is_some() {
            let trend_source = self.trend_source();
            if let Some(trend) = self.telemetry_trend.as_mut() {
                trend.set_source(trend_source);
                trend.record(drained.replies.clone(), drained.dropped);
            }
            if drained.dropped != 0 {
                self.record_telemetry_dropped(drained.dropped);
            }
            let mut gap = drained.dropped != 0;
            for reply in drained.replies {
                match reply_json(&reply) {
                    Ok(value) => self.record_telemetry(&value, std::mem::take(&mut gap)),
                    Err(error) => {
                        self.record_event("telemetry_decode_error", json!({"error":error}))
                    }
                }
            }
        }
        if let Some(reply) = observation.telemetry.clone() {
            self.last_telemetry = Some(reply);
        }
        self.observed = Some(observation);
    }

    pub(crate) fn connection_generation(&self) -> u64 {
        self.connection_generation
    }

    pub(crate) fn integration_identity(&self) -> Option<(String, String)> {
        let reply = self.identity.as_ref()?;
        let Response::Identity(identity) = reply.response().ok()? else {
            return None;
        };
        let fields = identity.fields();
        Some((hex(&fields.uid), hex(&fields.sample.run_nonce)))
    }

    pub(crate) fn integration_connected(&self) -> bool {
        let Some(client) = self.client.as_ref() else {
            return false;
        };
        self.connection_observation_connected(client.try_observe(0))
    }

    fn connection_observation_connected(
        &self,
        observation: std::task::Poll<Result<eha_sdk::host::ClientObservation, SdkError>>,
    ) -> bool {
        match observation {
            std::task::Poll::Ready(Ok(observed)) => observed.connection.disconnected.is_none(),
            std::task::Poll::Pending => {
                self.client.is_some()
                    && self.disconnected_reason.is_none()
                    && self
                        .observed
                        .as_ref()
                        .is_none_or(|observed| observed.connection.disconnected.is_none())
            }
            std::task::Poll::Ready(Err(_)) => false,
        }
    }

    pub(crate) fn integration_pending_target(&self) -> bool {
        self.pending_target
    }

    pub(crate) fn integration_unresolved_trial(&self) -> bool {
        self.trial.active.is_some()
            || self
                .trial
                .interrupted
                .as_ref()
                .is_some_and(|trial| !trial.unrecoverable)
    }

    pub(crate) fn integration_telemetry_age_ms(&self) -> Option<u64> {
        self.last_telemetry.as_ref().map(|reply| {
            Instant::now()
                .saturating_duration_since(reply.received_at)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64
        })
    }

    /// Active reads may contend with ongoing control. This gate uses decoded SDK fields.
    pub(crate) fn integration_read_blocker(&self) -> Option<&'static str> {
        if self.pending_target {
            return Some("unresolved_target");
        }
        if self.integration_unresolved_trial() {
            return Some("trial_active");
        }
        let Some(client) = self.client.as_ref() else {
            return Some("transport_unknown");
        };
        let std::task::Poll::Ready(Ok(observed)) = client.try_observe(0) else {
            return Some("observation_busy");
        };
        if observed.connection.disconnected.is_some() {
            return Some("transport_unknown");
        }
        if observed.heartbeat.enabled {
            return Some("heartbeat_active");
        }
        let Some(reply) = self.last_telemetry.as_ref() else {
            return Some("awaiting_telemetry");
        };
        if Instant::now().saturating_duration_since(reply.received_at) > Duration::from_secs(3) {
            return Some("stale_telemetry");
        }
        let Ok(Response::Telemetry(telemetry)) = reply.response() else {
            return Some("transport_unknown");
        };
        let fields = telemetry.fields();
        if self
            .identity
            .as_ref()
            .and_then(identity_facts)
            .is_none_or(|identity| identity.run_nonce != fields.sample.run_nonce)
        {
            return Some("identity_changed");
        }
        if fields.target_mode != TargetMode::None
            || fields.desired_axis != DesiredAxis::Idle
            || fields.axis_state_raw != 1
            || !fields.driver_state.has_status
            || !fields.driver_state.qualified
            || fields.driver_state.stale
            || fields.driver_state.faulted
            || fields.facts.device_operation_pending
            || fields.facts.unknown_effect
        {
            return Some("not_idle");
        }
        None
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
        self.finish_recording_async_boundary(reason);
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
        let same_instance = self.integration_connected()
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
        unknown: false,
        failure: None,
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
        Some(ConnectionRequest::Can { .. } | ConnectionRequest::ManagedCan { .. }) => {
            telemetry["can_contact"].as_u64() == Some(1)
        }
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
        Some(ConnectionRequest::Can { .. } | ConnectionRequest::ManagedCan { .. }) => {
            status["can_contact"].as_u64() == Some(1)
        }
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
        python_factory(channel, python)?,
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
            python_factory(channel.clone(), python.clone()).map_err(open_error)?,
        )
        .map(Connector::Can)
        .map_err(open_error),
        ConnectionRequest::ManagedCan {
            serial,
            node,
            mode,
            python,
            ..
        } => CanConnector::new(
            CanOptions {
                node: *node,
                mode: parse_can_mode(mode)?,
            },
            crate::connections::managed_factory(
                serial.clone(),
                crate::runtime::python(python.as_deref()).map_err(open_error)?,
            ),
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
        Command::ConfigSaveAndRestart { .. } => "config_save_and_restart",
        Command::RestoreFactoryAndRestart => "restore_factory_and_restart",
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

pub(crate) fn parse_can_mode(value: &str) -> Result<eha_sdk::transport::can::Mode, SessionError> {
    match value {
        "classic" => Ok(eha_sdk::transport::can::Mode::Classic),
        "fd" => Ok(eha_sdk::transport::can::Mode::Fd),
        _ => Err(invalid("未知 CAN 帧格式；可用值：classic、fd")),
    }
}

use crate::connections::advanced_factory as python_factory;

fn expected_uid_matches(request: &ConnectionRequest, identity: &Reply) -> Result<(), SessionError> {
    let identity = reply_json(identity)?;
    crate::connections::expected_uid_matches(request.expected_uid(), &identity).map_err(invalid)
}

fn bind_managed_request(
    request: &mut ConnectionRequest,
    identity: &Reply,
) -> Result<(), SessionError> {
    let ConnectionRequest::ManagedCan { expected_uid, .. } = request else {
        return Ok(());
    };
    if expected_uid.is_none() {
        let identity = reply_json(identity)?;
        let uid = identity["uid"]
            .as_str()
            .ok_or_else(|| invalid("Identity 回复没有 EHA UID"))?;
        *expected_uid = Some(uid.to_ascii_lowercase());
    }
    Ok(())
}
fn parse_view(value: &str) -> Result<ConfigView, SessionError> {
    match value {
        "factory" => Ok(ConfigView::Factory),
        "user" => Ok(ConfigView::UserRecord),
        "startup" => Ok(ConfigView::Startup),
        _ => Err(invalid("未知配置视图")),
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
struct IdentityFacts {
    uid: [u8; 12],
    run_nonce: [u8; 16],
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
        unknown: false,
        failure: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}
fn open_error(message: String) -> SessionError {
    let kind = crate::connections::owner_error_code(&message).unwrap_or("connection");
    SessionError {
        kind: kind.into(),
        message: message.into_boxed_str(),
        local_stage: None,
        unknown: false,
        failure: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}

fn owner_error(error: crate::owner_lock::OwnerLockError) -> SessionError {
    open_error(error.to_string())
}
fn sdk_error(error: SdkError) -> SessionError {
    let SdkError {
        failure,
        local,
        observed_transport,
    } = error;
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
        unknown,
        failure,
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
fn constraints_json(constraints: eha_sdk::protocol::responses::Constraints) -> Value {
    json!({
        "driver_transition": constraints.driver_transition,
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
        "native_code": entry.native_code,
        "native_domain": entry.native_domain as u8,
        "native_domain_label": eha_sdk::diagnostics::native_domain_label(entry.native_domain),
        "native_code_description": eha_sdk::diagnostics::native_code_description(entry.native_domain, entry.native_code),
        "evidence": {
            "not_started": entry.evidence.not_started,
            "send_submitted": entry.evidence.send_submitted,
            "matched_feedback": entry.evidence.matched_feedback,
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
            json!({"kind":"identity", "received_age_ms": age_ms, "sample": sample_json(fields.sample), "uid": hex(&fields.uid), "config_source": fields.config_source as u8, "config_source_label": eha_sdk::diagnostics::startup_source_label(fields.config_source), "fallback_reason": fields.fallback_reason as u8, "fallback_reason_label": eha_sdk::diagnostics::fallback_reason_label(fields.fallback_reason), "run_identity": fields.run_identity as u8, "run_identity_label": eha_sdk::diagnostics::run_identity_label(fields.run_identity), "update_route": fields.update_route as u8, "update_route_label": eha_sdk::diagnostics::update_route_label(fields.update_route), "active_can_node": fields.active_can_node, "active_can_profile": fields.active_can_profile as u8, "active_can_profile_label": eha_sdk::diagnostics::can_profile_label(fields.active_can_profile), "host_heartbeat_hz": fields.host_heartbeat_hz, "telemetry_hz": fields.telemetry_hz, "host_contact_max_age_ms": fields.host_contact_max_age_ms, "config_format_version": fields.config_format_version, "text": {"version":fields.text.version, "source_revision":fields.text.source_revision, "build_information":fields.text.build_information, "odrive_binding":fields.text.odrive_binding, "odrive_observed_version":fields.text.odrive_observed_version, "brt27_binding":fields.text.brt27_binding, "brt27_observed_version":fields.text.brt27_observed_version, "update_binding":fields.text.update_binding}})
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
            output.insert("facts".into(), json!({"output_allowed":fields.facts.output_allowed,"reference_limited":fields.facts.reference_limited,"candidate_limited":fields.facts.candidate_limited,"position_direction_inhibited":fields.facts.position_direction_inhibited,"device_operation_pending":fields.facts.device_operation_pending,"unknown_effect":fields.facts.unknown_effect}));
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
            json!({"kind":"diagnostics","received_age_ms":age_ms,"sample":sample_view_json(response.sample()),"decision_age_us":response.decision_age_us(),"device_evidence_age_us":response.device_evidence_age_us(),"overflow":response.overflow(),"entries":entries})
        }
        Response::ConfigData(response) => {
            let fields = response.fields();
            json!({"kind":"config_data","received_age_ms":age_ms,"sample":sample_json(fields.sample),"view":fields.view as u8,"view_label":eha_sdk::diagnostics::config_view_label(fields.view),"record_state":fields.record_state as u8,"record_state_label":eha_sdk::diagnostics::config_record_state_label(fields.record_state),"startup_source":fields.startup_source as u8,"startup_source_label":eha_sdk::diagnostics::startup_source_label(fields.startup_source),"fallback_reason":fields.fallback_reason as u8,"fallback_reason_label":eha_sdk::diagnostics::fallback_reason_label(fields.fallback_reason),"data_time_us":fields.data_time_us,"data_utf8":std::str::from_utf8(response.data()).ok(),"data_hex":hex(response.data())})
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
        Command, TelemetryTrend, TelemetryTrendPoint, data_unavailable_fields_json,
        diagnostic_entry_json, driver_summary_json, sdk_error,
    };
    use eha_sdk::{
        host::{Error, Failure, LocalStage},
        protocol::{SampleData, responses::*},
    };
    use serde_json::json;

    #[test]
    fn usb_owner_busy_is_structured_before_physical_open() {
        let serial = format!("eha-session-busy-{}", std::process::id());
        let key = crate::owner_lock::ResourceKey::usb(&serial).expect("key");
        let _owner = crate::owner_lock::OwnerGuard::acquire(&key).expect("外部 owner");
        let error = super::ToolSession::new()
            .connect(super::ConnectionRequest::Usb { serial })
            .expect_err("不能打开已占用 USB");
        assert_eq!(&*error.kind, "owner_busy");
        assert!(!error.unknown);
    }

    #[test]
    fn usb_disconnect_drop_and_busy_reconnect_preserve_state() {
        let serial = format!("eha-session-lifecycle-{}", std::process::id());
        let key = crate::owner_lock::ResourceKey::usb(&serial).expect("key");
        let (mut session, _) = super::trial_tests::session_fixture(
            0,
            [41; 12],
            super::trial_tests::FixtureBehavior::Ready,
        );
        session.request = Some(super::ConnectionRequest::Usb {
            serial: serial.clone(),
        });
        session.connector = Some(super::Connector::Usb(
            eha_sdk::usb::UsbConnector::new(serial).expect("连接器"),
        ));
        session.usb_owner = Some(crate::owner_lock::OwnerGuard::acquire(&key).expect("会话 owner"));
        assert_eq!(
            crate::owner_lock::OwnerGuard::acquire(&key)
                .expect_err("Backend 持有期间 busy")
                .code,
            "owner_busy"
        );
        session.disconnect().expect("断开并结束 worker");
        let other = crate::owner_lock::OwnerGuard::acquire(&key).expect("断连释放 owner");
        assert_eq!(
            &*session.reconnect().expect_err("重连竞争失败").kind,
            "owner_busy"
        );
        assert!(session.disconnected.is_some(), "失败保留可恢复会话状态");
        drop(other);
        drop(session);
        let (mut session, _) = super::trial_tests::session_fixture(
            0,
            [42; 12],
            super::trial_tests::FixtureBehavior::Ready,
        );
        session.usb_owner =
            Some(crate::owner_lock::OwnerGuard::acquire(&key).expect("第二会话 owner"));
        drop(session);
        drop(crate::owner_lock::OwnerGuard::acquire(&key).expect("会话 drop 后释放 owner"));
    }

    #[test]
    #[cfg(unix)]
    fn passive_usb_failure_joins_before_unlock_and_preserves_unknown_reconnect_state() {
        use eha_sdk::host::backend::{Backend, Command as BackendCommand};
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        use std::time::{Duration, Instant};
        for use_tick in [false, true] {
            let serial = format!("eha-passive-usb-{}-{use_tick}", std::process::id());
            let key = crate::owner_lock::ResourceKey::usb(&serial).expect("key");
            let (mut session, _) = super::trial_tests::session_fixture(
                0,
                [43; 12],
                super::trial_tests::FixtureBehavior::Ready,
            );
            let identity = session.integration_identity();
            session.client.take().expect("旧合成client").close();
            session.request = Some(super::ConnectionRequest::Usb {
                serial: serial.clone(),
            });
            session.connector = Some(super::Connector::Usb(
                eha_sdk::usb::UsbConnector::new(serial.clone()).expect("连接器"),
            ));
            let physical_done = Arc::new(AtomicBool::new(false));
            let worker_done = Arc::clone(&physical_done);
            let cleanup = Arc::new(Barrier::new(2));
            let worker_cleanup = Arc::clone(&cleanup);
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (close_tx, close_rx) = mpsc::sync_channel(1);
            let backend = Backend::spawn("eha-passive-usb-failure", move |commands, sink| {
                sink.disconnected("合成断连，物理清理尚未结束");
                ready_tx.send(()).expect("断连可见");
                assert!(matches!(
                    commands.recv().expect("只等待归还Backend"),
                    BackendCommand::Close
                ));
                close_tx.send(()).expect("join已开始");
                worker_cleanup.wait();
                worker_done.store(true, Ordering::SeqCst);
            })
            .expect("合成worker");
            session.client = Some(eha_sdk::host::Client::new(backend));
            session.usb_owner =
                Some(crate::owner_lock::OwnerGuard::acquire(&key).expect("USB owner"));
            session.pending_target = true;
            session.trial.active = Some(super::ActiveTrial {
                request: super::TrialRequest {
                    command: super::Command::Velocity { mm_s: 0.5 },
                    envelope: super::TrialEnvelope {
                        position_min_mm: -10.0,
                        position_max_mm: 10.0,
                        velocity_abs_max_mm_s: 2.0,
                        force_abs_max_n: 10.0,
                        stiffness_max_n_per_mm: 10.0,
                        damping_max_ns_per_mm: 10.0,
                        duration_max_s: 5.0,
                    },
                    duration_s: Some(5.0),
                    reach: None,
                },
                started_at: Instant::now(),
                duration: Duration::from_secs(5),
                pending_position_mm: None,
                last_position_submit: None,
                settled_since: None,
                stop_submission: None,
                stop_reason: None,
                stop_observed: false,
                started_sample_time_us: 0,
                stop_after_sample_time_us: None,
                stop_attempt_finished_at: None,
                stop_confirmation_timeout: Duration::from_secs(1),
                stop_confirmation_overdue: false,
                stop_recovery_after_received_at: None,
                settled_sample_time_us: None,
            });
            ready_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("已发布断连");
            let observer = std::thread::spawn(move || {
                close_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("snapshot/tick开始回收Backend");
                let key = crate::owner_lock::ResourceKey::usb(&serial).expect("key");
                let busy = crate::owner_lock::OwnerGuard::acquire(&key)
                    .err()
                    .is_some_and(|error| error.code == "owner_busy");
                cleanup.wait();
                busy
            });
            if use_tick {
                session.tick();
            } else {
                assert!(!session.snapshot().connected);
            }
            assert!(observer.join().expect("观察线程"), "物理清理完成前仍排他");
            let deadline = Instant::now() + Duration::from_secs(1);
            while session.retiring.is_some() {
                session.poll_retirement();
                assert!(
                    Instant::now() < deadline,
                    "retirement completion not observed"
                );
                std::thread::yield_now();
            }
            assert!(
                physical_done.load(Ordering::SeqCst),
                "释放owner前已完成worker join"
            );
            assert!(session.client.is_none());
            assert!(session.disconnected.is_some());
            assert_eq!(session.integration_identity(), identity);
            assert_eq!(session.trial_snapshot()["state"], "interrupted_unknown");
            assert_eq!(session.trial_snapshot()["stop_available"], false);
            assert!(session.pending_target, "未知目标事实不能因归还句柄而清除");
            let competitor =
                crate::owner_lock::OwnerGuard::acquire(&key).expect("旧会话仍存活但已释放物理锁");
            assert_eq!(
                &*session.reconnect().expect_err("竞争中的显式重连").kind,
                "owner_busy"
            );
            assert!(session.disconnected.is_some());
            assert_eq!(session.trial_snapshot()["state"], "interrupted_unknown");
            drop(competitor);
        }
    }

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

    #[test]
    fn maintenance_actions_accept_only_restart_submission_names() {
        let save = crate::RawFields::parse(r#"{"action":"config_save_and_restart","record":"{}"}"#)
            .expect("new save action parses");
        assert!(matches!(
            Command::from_fields(&save),
            Ok(Command::ConfigSaveAndRestart { .. })
        ));
        let restore = crate::RawFields::parse(r#"{"action":"restore_factory_and_restart"}"#)
            .expect("new restore action parses");
        assert!(matches!(
            Command::from_fields(&restore),
            Ok(Command::RestoreFactoryAndRestart)
        ));
        let old = crate::RawFields::parse(r#"{"action":"maintenance_result"}"#)
            .expect("legacy action remains valid JSON");
        assert!(Command::from_fields(&old).is_err());
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
    fn unavailable_error_preserves_current_reply_without_legacy_result_metadata() {
        let fields = DataUnavailableFields {
            sample: sample(),
            subject: UnavailableSubject::Diagnostics,
            reason: 0x0307,
            next_actions: (1 << 0) | (1 << 9),
        };
        let error = sdk_error(Error {
            failure: Failure::DataUnavailable(fields),
            local: LocalStage::FullySubmitted,
            observed_transport: None,
        });

        assert_eq!(
            error.failure.as_deref(),
            Some(&data_unavailable_fields_json(fields))
        );
        assert!(
            error
                .failure
                .as_deref()
                .is_some_and(|value| value.get("retained").is_none())
        );
        assert!(
            error
                .failure
                .as_deref()
                .is_some_and(|value| value.get("operation_id").is_none())
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
