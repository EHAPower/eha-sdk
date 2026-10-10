// Copyright The eha-sdk Contributors
//! 桌面客户 API。`Client` 串行提交业务，独立 I/O 线程维持收发和显式心跳。
//! 所有数据副本拥有自己的字节；本地提交绝不表示目标采用或设备执行。
pub mod backend;
use crate::session::{MaintenanceKey, QueryKind, Session};
pub use backend::{AdapterStatus, Backend, HeartbeatStatus, TransportStatus};
use backend::{Command, Event, Received, SendRequest};
use protocol::responses::{
    ConfigView, DataUnavailableFields, MaintenanceOperation, MaintenanceState,
    OperationResultFields,
};
use protocol::{Direction, Message, MessageKind, Response};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// 调用方可从另一线程取消本次等待；不撤回已提交的字节或设备副作用。
#[derive(Clone, Default, Debug)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
/// 总期限从发送准备开始计算，不等同于固件业务期限。
#[derive(Clone, Debug)]
pub struct Wait {
    pub timeout: Duration,
    pub cancellation: Cancellation,
}
impl Wait {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            cancellation: Cancellation::default(),
        }
    }
}
impl Default for Wait {
    fn default() -> Self {
        Self::new(Duration::from_secs(5))
    }
}
/// 仅说明本地 I/O 已完整接受公共消息，不是总线 ACK 或设备确认。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalSubmission {
    pub id: u64,
    pub submitted_at: Instant,
    pub boundary: &'static str,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalStage {
    NotSubmitted,
    MayHaveBeenSubmitted,
    FullySubmitted,
}
/// 失败种类；没有收到回复不会被伪造为设备拒绝。
#[derive(Clone, Debug)]
pub enum Failure {
    InvalidInput(String),
    Session(String),
    Timeout,
    Cancelled,
    Disconnected(String),
    Transport { detail: String, raw: Vec<u8> },
    DataUnavailable(DataUnavailableFields),
    ResultUnknown(String),
    PendingOperation,
    ReadbackMismatch,
    ReadbackUnavailable(protocol::responses::ConfigRecordState),
    DeviceResult(Box<OperationResultFields>),
}
#[derive(Clone, Debug)]
pub struct Error {
    pub failure: Failure,
    pub local: LocalStage,
    /// 维护键位于堆上，以免只读错误证据放大所有业务结果的大小。
    pub operation: Option<Box<MaintenanceKey>>,
    /// 已取得的原维护结果；后续只读失败不会抹去这些事实。
    pub last_result: Option<Box<OperationResultFields>>,
    /// 接收路径独立异常，未声称它就是本次请求失败的原因。
    pub observed_transport: Option<Box<TransportStatus>>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}; local={:?}; operation={:?}",
            self.failure, self.local, self.operation
        )
    }
}
impl std::error::Error for Error {}
/// 收到的不可变完整回复及主机接收时刻；读取它不会刷新来源时间。
#[derive(Clone, Debug)]
pub struct Reply {
    bytes: Vec<u8>,
    pub received_at: Instant,
}

/// 一次从显式开启的周期遥测队列取得的增量。
///
/// `dropped` 只计队列容量不足时已接收但无法保留的 Telemetry，不包括调用方自行
/// 截短展示窗口，也不根据 `snapshot_sequence` 推断缺失。
#[derive(Clone, Debug, Default)]
pub struct TelemetryDrain {
    pub replies: Vec<Reply>,
    pub dropped: u64,
}
impl Reply {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn response(&self) -> Result<Response<'_>, Error> {
        match protocol::decode(&self.bytes, Direction::FirmwareToHost) {
            Ok(Message::Response(r)) => Ok(r),
            _ => Err(invalid("无效的固件回复")),
        }
    }
}
impl From<Received> for Reply {
    fn from(r: Received) -> Self {
        Self {
            bytes: r.bytes,
            received_at: r.at,
        }
    }
}
#[derive(Clone, Debug)]
pub struct OperationSubmission {
    pub key: MaintenanceKey,
    pub local: LocalSubmission,
}
#[derive(Clone, Debug)]
pub struct SavedConfig {
    pub submission: OperationSubmission,
    pub result: OperationResultFields,
    pub readback: Reply,
}
/// 本地 I/O 连接与遥测缓存事实。`disconnected` 为空只表示尚未记录本地断连，
/// 不表示固件在线、可控制或主机联系有效。
#[derive(Clone, Debug, Default)]
pub struct ConnectionStatus {
    /// 后端报告的本地断连原因；保留到该 Backend 被释放。
    pub disconnected: Option<String>,
    /// 最近一份已解码 Telemetry 到达主机的时刻；它是缓存接收时间，不是新鲜度承诺。
    pub last_telemetry_received_at: Option<Instant>,
}
#[derive(Clone, Debug)]
struct PendingOperation {
    key: MaintenanceKey,
    kind: Option<MaintenanceOperation>,
    result: Option<OperationResultFields>,
}
/// 可携带至重新打开连接的会话状态。保存查询编号、原维护键和已取得结果，防止重连重用编号。
#[derive(Clone, Debug)]
pub struct ClientState {
    session: Session,
    sequence: u64,
    pending: Option<PendingOperation>,
    heartbeat_hz: Option<u32>,
}
/// 一台设备的串行业务调用方；打开后须先核对身份，资源释放不发送控制或停止。
pub struct Client {
    backend: Backend,
    identity_verified: bool,
    session: Session,
    sequence: u64,
    pending: Option<PendingOperation>,
    heartbeat_hz: Option<u32>,
}
fn invalid(s: impl Into<String>) -> Error {
    Error {
        failure: Failure::InvalidInput(s.into()),
        local: LocalStage::NotSubmitted,
        operation: None,
        last_result: None,
        observed_transport: None,
    }
}
impl Client {
    /// 装配独占后端，初始身份未核对且周期心跳未启动。
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            identity_verified: false,
            session: Session::new(),
            sequence: 0,
            pending: None,
            heartbeat_hz: None,
        }
    }
    /// 重新装配已显式保存的会话。先 identify 核对现设备；不自动重发或开启心跳。
    ///
    /// `backend` 必须由之前同一个 CAN/USB Connector 重开，或由调用方后端保留等价
    /// 的传输序号和 guard 历史。不能以一次性 `can::open` / `usb::open` 重置传输状态。
    pub fn resume(backend: Backend, state: ClientState) -> Self {
        Self {
            backend,
            identity_verified: false,
            session: state.session,
            sequence: state.sequence,
            pending: state.pending,
            heartbeat_hz: state.heartbeat_hz,
        }
    }
    /// 关闭本地 I/O 并保留关联状态，供通信恢复后 resume 使用。
    pub fn disconnect(self) -> ClientState {
        ClientState {
            session: self.session,
            sequence: self.sequence,
            pending: self.pending,
            heartbeat_hz: self.heartbeat_hz,
        }
    }
    /// 关闭只回收本地连接，绝不发送 Stop、Reset 或重放目标。
    pub fn close(self) {}
    pub fn session(&self) -> &Session {
        &self.session
    }
    pub fn pending_operation(&self) -> Option<MaintenanceKey> {
        self.pending.as_ref().map(|p| p.key)
    }
    fn error(&self, failure: Failure, local: LocalStage) -> Error {
        Error {
            failure,
            local,
            operation: self.pending_operation().map(Box::new),
            observed_transport: Some(Box::new(self.transport_status())),
            last_result: self.pending.as_ref().and_then(|p| p.result).map(Box::new),
        }
    }
    fn require_identity(&self) -> Result<(), Error> {
        if !self.identity_verified || self.session.identity().is_none() {
            Err(self.error(
                Failure::Session("请先 identify 核对身份".into()),
                LocalStage::NotSubmitted,
            ))
        } else {
            Ok(())
        }
    }
    fn require_key_identity(&self, key: MaintenanceKey) -> Result<(), Error> {
        self.require_identity()?;
        if self
            .session
            .identity()
            .is_some_and(|identity| identity.uid == key.uid)
        {
            Ok(())
        } else {
            Err(self.error(
                Failure::Session("维护键 UID 与已核对设备不匹配".into()),
                LocalStage::NotSubmitted,
            ))
        }
    }
    fn encode(message: &Message<'_>) -> Result<Vec<u8>, Error> {
        let len = message
            .encoded_len()
            .map_err(|e| invalid(format!("{e:?}")))?;
        let mut bytes = vec![0; len];
        protocol::encode(*message, &mut bytes).map_err(|e| invalid(format!("{e:?}")))?;
        Ok(bytes)
    }
    fn send(
        &mut self,
        message: &Message<'_>,
        wait: &Wait,
    ) -> Result<(u64, Instant, Arc<AtomicBool>), Error> {
        let deadline = Instant::now()
            .checked_add(wait.timeout)
            .ok_or_else(|| invalid("等待期限无法表示"))?;
        if wait.timeout.is_zero() || wait.cancellation.is_cancelled() {
            return Err(self.error(Failure::Cancelled, LocalStage::NotSubmitted));
        }
        let bytes = Self::encode(message)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("本地调用编号耗尽"))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.backend
            .tx
            .try_send(Command::Send(SendRequest {
                id: self.sequence,
                bytes,
                deadline,
                cancelled: cancelled.clone(),
            }))
            .map_err(|e| {
                self.error(
                    Failure::Disconnected(e.to_string()),
                    LocalStage::NotSubmitted,
                )
            })?;
        Ok((self.sequence, deadline, cancelled))
    }
    fn next_event(&self, deadline: Instant, wait: &Wait) -> Result<Event, Failure> {
        let (lock, cv) = &*self.backend.sink.0;
        let mut inbox = lock
            .lock()
            .map_err(|_| Failure::Disconnected("I/O 状态锁失效".into()))?;
        loop {
            if wait.cancellation.is_cancelled() {
                return Err(Failure::Cancelled);
            }
            // 截止期是等待的绝对边界。不能让不匹配的迟到事件持续占用队列，
            // 从而把超时变成成功或无限延后。
            if Instant::now() >= deadline {
                return Err(Failure::Timeout);
            }
            if let Some(event) = inbox.events.pop_front() {
                return Ok(event);
            }
            if let Some(reason) = &inbox.disconnected {
                return Err(Failure::Disconnected(reason.clone()));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            inbox = cv
                .wait_timeout(inbox, remaining.min(Duration::from_millis(10)))
                .map_err(|_| Failure::Disconnected("I/O 等待锁失效".into()))?
                .0;
        }
    }
    fn observe(&mut self, reply: &Reply) {
        if let (Some(p), Ok(Response::OperationResult(result))) =
            (&mut self.pending, reply.response())
        {
            let f = result.fields();
            if f.operation_id == p.key.operation_id
                && f.sample.run_nonce == p.key.run_nonce
                && p.kind.is_none_or(|kind| kind == f.operation)
                && p.result.is_none_or(|old| f.revision >= old.revision)
            {
                p.result = Some(f);
                p.kind = Some(f.operation);
            }
        }
    }
    fn await_submission(
        &mut self,
        id: u64,
        deadline: Instant,
        cancel: Arc<AtomicBool>,
        wait: &Wait,
        mutation: bool,
    ) -> Result<LocalSubmission, Error> {
        loop {
            let event = match self.next_event(deadline, wait) {
                Ok(e) => e,
                Err(e) => {
                    cancel.store(true, Ordering::Release);
                    return Err(self.wait_error(e, LocalStage::MayHaveBeenSubmitted, mutation));
                }
            };
            match event {
                Event::Submitted(got) if got == id => {
                    return Ok(LocalSubmission {
                        id,
                        submitted_at: Instant::now(),
                        boundary: self.backend.boundary,
                    });
                }
                Event::Failed(got, e) if got == id => {
                    return Err(self.wait_error(
                        Failure::Transport {
                            detail: e.detail,
                            raw: vec![],
                        },
                        if e.may_have_sent {
                            LocalStage::MayHaveBeenSubmitted
                        } else {
                            LocalStage::NotSubmitted
                        },
                        mutation,
                    ));
                }
                Event::Message(r) => self.observe(&r.into()),
                Event::Disconnected(s) => {
                    return Err(self.wait_error(
                        Failure::Disconnected(s),
                        LocalStage::MayHaveBeenSubmitted,
                        mutation,
                    ));
                }
                Event::TransportError { detail, raw } => {
                    cancel.store(true, Ordering::Release);
                    return Err(self.wait_error(
                        Failure::Transport { detail, raw },
                        LocalStage::MayHaveBeenSubmitted,
                        mutation,
                    ));
                }
                _ => {}
            }
        }
    }
    fn wait_error(&self, reason: Failure, local: LocalStage, mutation: bool) -> Error {
        let failure = if mutation && local != LocalStage::NotSubmitted {
            Failure::ResultUnknown(format!("{reason:?}"))
        } else {
            reason
        };
        self.error(failure, local)
    }
    /// 查询身份，并可严格匹配外部交付的 UID。返回原始具名 Identity 视图及构建证据。
    pub fn identify(
        &mut self,
        expected_uid: Option<[u8; 12]>,
        wait: &Wait,
    ) -> Result<Reply, Error> {
        self.identity_verified = false;
        let expected_uid = expected_uid.or_else(|| self.session.identity().map(|i| i.uid));
        let reply = self.query(QueryKind::Identity, wait)?;
        let Response::Identity(identity) = reply.response()? else {
            return Err(invalid("身份回复类别不符"));
        };
        let f = identity.fields();
        if expected_uid.is_some_and(|uid| uid != f.uid) {
            return Err(self.error(
                Failure::Session("设备 UID 不匹配".into()),
                LocalStage::FullySubmitted,
            ));
        }
        self.session.observe_identity(&identity).map_err(|e| {
            self.error(
                Failure::Session(format!("{e:?}")),
                LocalStage::FullySubmitted,
            )
        })?;
        self.identity_verified = true;
        self.heartbeat_hz = Some(f.host_heartbeat_hz);
        if self.pending.is_none() && f.retained_operation_id != 0 {
            self.pending = Some(PendingOperation {
                key: MaintenanceKey {
                    uid: f.uid,
                    run_nonce: f.sample.run_nonce,
                    operation_id: f.retained_operation_id,
                },
                kind: None,
                result: None,
            });
        }
        Ok(reply)
    }
    /// 查询当前目标、采用阻止原因、驱动状态和入口联系；不是某个控制请求的确认。
    pub fn status(&mut self, wait: &Wait) -> Result<Reply, Error> {
        self.query(QueryKind::Status, wait)
    }
    /// 查询测量值及其来源时间和质量；不把不可用值改写成零值有效。
    pub fn measurements(&mut self, wait: &Wait) -> Result<Reply, Error> {
        self.query(QueryKind::Measurements, wait)
    }
    /// 查询当前有界诊断和缺失证据；诊断不代替原维护结果。
    pub fn diagnostics(&mut self, wait: &Wait) -> Result<Reply, Error> {
        self.query(QueryKind::Diagnostics, wait)
    }
    /// 读取指定视图的实际字节及记录状态，不以内存候选代替设备读回。
    pub fn read_config(&mut self, view: ConfigView, wait: &Wait) -> Result<Reply, Error> {
        self.query(QueryKind::Config(view as u8), wait)
    }
    fn finish_query_reply(&self, reply: Reply, stage: LocalStage) -> Result<Reply, Error> {
        match reply.response()? {
            Response::DataUnavailable(unavailable) => {
                Err(self.error(Failure::DataUnavailable(unavailable.fields()), stage))
            }
            _ => Ok(reply),
        }
    }
    fn query(&mut self, kind: QueryKind, wait: &Wait) -> Result<Reply, Error> {
        if kind != QueryKind::Identity {
            self.require_identity()?;
        }
        let matcher = self.session.next_query(kind).map_err(|e| {
            self.error(Failure::Session(format!("{e:?}")), LocalStage::NotSubmitted)
        })?;
        let message = matcher.request().map_err(|e| invalid(format!("{e:?}")))?;
        let (id, deadline, cancel) = self.send(&message, wait)?;
        let mut stage = LocalStage::MayHaveBeenSubmitted;
        // USB 的 IN completion 可以先于 OUT completion 到达。只为本查询保留一份已匹配
        // 回复，仍须等待本地完整提交，不能以任意回复伪造提交事实。
        let mut early_reply = None;
        loop {
            let event = match self.next_event(deadline, wait) {
                Ok(e) => e,
                Err(e) => {
                    cancel.store(true, Ordering::Release);
                    return Err(self.error(e, stage));
                }
            };
            match event {
                Event::Submitted(got) if got == id => {
                    stage = LocalStage::FullySubmitted;
                    if let Some(reply) = early_reply.take() {
                        return self.finish_query_reply(reply, stage);
                    }
                }
                Event::Failed(got, e) if got == id => {
                    return Err(self.error(
                        Failure::Transport {
                            detail: e.detail,
                            raw: vec![],
                        },
                        if e.may_have_sent {
                            stage
                        } else {
                            LocalStage::NotSubmitted
                        },
                    ));
                }
                Event::Message(r) => {
                    let reply: Reply = r.into();
                    self.observe(&reply);
                    let response = reply.response()?;
                    if matcher.matches(&response) {
                        if stage == LocalStage::FullySubmitted {
                            return self.finish_query_reply(reply, stage);
                        }
                        if early_reply.is_none() {
                            early_reply = Some(reply);
                        }
                    }
                }
                Event::Disconnected(s) => return Err(self.error(Failure::Disconnected(s), stage)),
                Event::TransportError { detail, raw } => {
                    cancel.store(true, Ordering::Release);
                    return Err(self.error(Failure::Transport { detail, raw }, stage));
                }
                _ => {}
            }
        }
    }
    /// 取得独立接收的最新遥测，保留主机接收时刻。旧 run 的副本不会冒充当前数据。
    /// 缓存不会在本地断连时清除；调用方须结合 [`Self::connection_status`] 和
    /// `received_at` 判断连接与数据年龄。
    pub fn telemetry(&self) -> Option<Reply> {
        if !self.identity_verified {
            return None;
        }
        let reply: Reply = self.backend.sink.0.0.lock().ok()?.telemetry.clone()?.into();
        let identity = self.session.identity()?;
        match reply.response().ok()? {
            Response::Telemetry(t) if t.fields().sample.run_nonce == identity.run_nonce => {
                Some(reply)
            }
            _ => None,
        }
    }
    /// 为连续展示显式启用有界 Telemetry 接收队列。
    ///
    /// 默认仍只保留最新一帧，避免未消费的展示用途改变普通 SDK 调用方的内存边界。
    /// 必须在后端存活期间调用；重新打开连接会创建新后端，调用方须再次显式开启。
    pub fn enable_telemetry_queue(&mut self, capacity: usize) -> Result<(), Error> {
        self.backend
            .sink
            .enable_telemetry_queue(capacity)
            .map_err(invalid)
    }
    /// 取走自上次调用后保留的当前运行实例 Telemetry。
    ///
    /// 这是单消费者队列。尚未完成 Identity 核对时不取走任何项目；完成核对后，旧运行
    /// 实例的帧会被丢弃，不能混入当前运行。`telemetry()` 的最新缓存语义不受影响。
    pub fn drain_telemetry(&self) -> TelemetryDrain {
        if !self.identity_verified {
            return TelemetryDrain::default();
        }
        let Some(identity) = self.session.identity() else {
            return TelemetryDrain::default();
        };
        let (received, dropped) = self.backend.sink.drain_telemetry_queue();
        let replies = received
            .into_iter()
            .map(Reply::from)
            .filter(|reply| {
                matches!(reply.response(), Ok(Response::Telemetry(value)) if value.fields().sample.run_nonce == identity.run_nonce)
            })
            .collect();
        TelemetryDrain { replies, dropped }
    }
    /// 取得本地连接断开和最近遥测缓存的事实，不将二者解释为固件状态或新鲜度。
    pub fn connection_status(&self) -> ConnectionStatus {
        let Ok(inbox) = self.backend.sink.0.0.lock() else {
            return ConnectionStatus::default();
        };
        let disconnected = inbox.disconnected.clone();
        let telemetry = inbox.telemetry.clone();
        drop(inbox);
        ConnectionStatus {
            disconnected,
            last_telemetry_received_at: telemetry.as_ref().map(|telemetry| telemetry.at),
        }
    }
    /// 独立传输异常及普通事件 FIFO 淘汰；不把未关联的异常归因于当前业务。
    pub fn transport_status(&self) -> TransportStatus {
        self.backend
            .sink
            .0
            .0
            .lock()
            .map(|i| i.transport.clone())
            .unwrap_or_default()
    }
    /// 适配器原始本地通知摘要，不表示设备采用或总线 ACK。
    pub fn adapter_status(&self) -> AdapterStatus {
        self.backend
            .sink
            .0
            .0
            .lock()
            .map(|i| i.adapter.clone())
            .unwrap_or_default()
    }
    /// 周期心跳的本地提交与失败事实；入口联系以固件回复为准。
    pub fn heartbeat_status(&self) -> HeartbeatStatus {
        self.backend
            .sink
            .0
            .0
            .lock()
            .map(|i| i.heartbeat.clone())
            .unwrap_or_default()
    }
    /// 采用本次 Identity 的心跳频率；周期调度独立于所有业务等待。
    pub fn start_heartbeat(&mut self) -> Result<(), Error> {
        self.require_identity()?;
        let hz = self
            .heartbeat_hz
            .filter(|hz| *hz > 0)
            .ok_or_else(|| invalid("身份未给出有效心跳频率"))?;
        // 整数纳秒按最近取偶舍入，避免将整数频率转为浮点时基。
        let divisor = u64::from(hz);
        let quotient = 1_000_000_000 / divisor;
        let remainder = 1_000_000_000 % divisor;
        let round_up = remainder * 2 > divisor || (remainder * 2 == divisor && quotient % 2 != 0);
        let period = Duration::from_nanos(quotient + u64::from(round_up));
        if period.is_zero() {
            return Err(invalid("心跳周期无法以主机时钟表示"));
        }
        self.backend
            .tx
            .try_send(Command::Heartbeat(Some(period)))
            .map_err(|e| invalid(e.to_string()))
    }
    /// 结束显式心跳调度；不发送停止，不将联系过期误作已停止。
    /// 显式请求停止周期心跳；已经本地提交的心跳不能撤回，也不发送 Stop。
    pub fn stop_heartbeat(&mut self) -> Result<(), Error> {
        self.backend
            .tx
            .try_send(Command::Heartbeat(None))
            .map_err(|e| invalid(e.to_string()))
    }
    /// 显式发送一次心跳，只返回本地提交。
    /// 显式发送一次心跳；返回本地提交，不证明固件已刷新入口联系。
    pub fn heartbeat(&mut self, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Heartbeat, wait)
    }
    /// 提交持续位置目标，单位 mm；只返回本地提交，限值与采用由固件决定。
    pub fn position(&mut self, mm: f32, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Position(mm), wait)
    }
    /// 提交持续线速度目标，单位 mm/s；零值也是控制目标，不等于显式停止。
    pub fn velocity(&mut self, mm_s: f32, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Velocity(mm_s), wait)
    }
    /// 提交持续力目标，单位 N；返回不证明目标采用或执行。
    pub fn force(&mut self, n: f32, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Force(n), wait)
    }
    /// 提交平衡位置 mm、刚度 N/mm 和阻尼 N·s/mm 的完整持续目标。
    pub fn impedance(
        &mut self,
        mm: f32,
        n_per_mm: f32,
        ns_per_mm: f32,
        wait: &Wait,
    ) -> Result<LocalSubmission, Error> {
        self.submit(
            Message::Impedance {
                equilibrium_mm: mm,
                stiffness_n_per_mm: n_per_mm,
                damping_ns_per_mm: ns_per_mm,
            },
            wait,
        )
    }
    /// 仅此入口表示用户显式停止意图。其返回值不证明设备已取走停止。
    pub fn stop_control(&mut self, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Stop, wait)
    }
    fn submit(&mut self, message: Message<'_>, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.require_identity()?;
        let (id, deadline, cancel) = self.send(&message, wait)?;
        self.await_submission(id, deadline, cancel, wait, true)
    }
    /// 保存前完成全量静态检查；保留原始字节，绝不重排 JSON 或自动停止控制。
    pub fn begin_save(&mut self, record: &[u8], wait: &Wait) -> Result<OperationSubmission, Error> {
        crate::configuration::validate_json(record).map_err(|e| invalid(e.to_string()))?;
        self.begin_operation(MaintenanceOperation::SaveConfig, Some(record), wait)
    }
    fn begin_operation(
        &mut self,
        kind: MaintenanceOperation,
        record: Option<&[u8]>,
        wait: &Wait,
    ) -> Result<OperationSubmission, Error> {
        self.require_identity()?;
        if self.pending.is_some() {
            return Err(self.error(Failure::PendingOperation, LocalStage::NotSubmitted));
        }
        let key = self
            .session
            .next_operation()
            .map_err(|e| invalid(format!("{e:?}")))?;
        let key_bytes = key.to_bytes();
        let wire_key =
            protocol::OperationKey::new(&key_bytes).map_err(|e| invalid(format!("{e:?}")))?;
        let message = match kind {
            MaintenanceOperation::SaveConfig => Message::SaveConfig {
                key: wire_key,
                record: record.ok_or_else(|| invalid("缺少完整记录"))?,
            },
            _ => Message::Maintenance {
                key: wire_key,
                kind: match kind {
                    MaintenanceOperation::RestoreFactory => MessageKind::RestoreFactory,
                    MaintenanceOperation::ResetApplication => MessageKind::ResetApplication,
                    MaintenanceOperation::EnterUpdate => MessageKind::EnterUpdate,
                    _ => return Err(invalid("维护类型")),
                },
            },
        };
        self.pending = Some(PendingOperation {
            key,
            kind: Some(kind),
            result: None,
        });
        let result = self
            .send(&message, wait)
            .and_then(|(id, deadline, cancel)| {
                self.await_submission(id, deadline, cancel, wait, true)
            });
        match result {
            Ok(local) => Ok(OperationSubmission { key, local }),
            Err(e) => {
                if e.local == LocalStage::NotSubmitted {
                    self.pending = None;
                }
                Err(e)
            }
        }
    }
    /// 核对同设备出厂记录后请求恢复；不自动复位，仍须实际 UserRecord 读回。
    pub fn restore_factory(&mut self, wait: &Wait) -> Result<SavedConfig, Error> {
        let started = Instant::now();
        let factory = self.read_config(ConfigView::Factory, wait)?;
        let Response::ConfigData(data) = factory.response()? else {
            return Err(invalid("缺少出厂记录"));
        };
        let record = data.data().to_vec();
        crate::configuration::validate_json(&record).map_err(|e| invalid(e.to_string()))?;
        let submission = self.begin_operation(MaintenanceOperation::RestoreFactory, None, wait)?;
        self.finish_save(submission, &record, started, wait)
    }
    /// 明确请求应用复位；断连不证明新应用已启动。随后须核对同 UID 的新 nonce。
    pub fn reset_application(&mut self, wait: &Wait) -> Result<OperationSubmission, Error> {
        self.begin_operation(MaintenanceOperation::ResetApplication, None, wait)
    }
    /// 明确请求进入已交付的更新入口；本地提交不证明路由成立或更新成功。
    pub fn enter_update(&mut self, wait: &Wait) -> Result<OperationSubmission, Error> {
        self.begin_operation(MaintenanceOperation::EnterUpdate, None, wait)
    }
    /// 读取已核对设备的指定原维护键；不存在不等于未执行，绝不重发原操作。
    ///
    /// 键的 UID 必须与最近一次 [`Self::identify`] 核对的设备一致；可使用该设备先前运行实例的
    /// `run_nonce` 恢复结果。
    pub fn read_result(&mut self, key: MaintenanceKey, wait: &Wait) -> Result<Reply, Error> {
        self.require_key_identity(key)?;
        self.query(QueryKind::Result(key), wait)
    }
    /// 重连后继续跟踪已核对设备的调用方原键。不会改变键或提交维护。
    ///
    /// 键的 UID 必须与最近一次 [`Self::identify`] 核对的设备一致；可跟踪该设备先前运行实例的
    /// `run_nonce`。
    pub fn track_operation(&mut self, key: MaintenanceKey) -> Result<(), Error> {
        self.require_key_identity(key)?;
        match self.pending.as_ref() {
            Some(p) if p.key != key => {
                return Err(self.error(Failure::PendingOperation, LocalStage::NotSubmitted));
            }
            // 同一个显式键已在跟踪时，保留已取得的操作类型和结果证据。
            Some(_) => return Ok(()),
            None => {}
        }
        self.pending = Some(PendingOperation {
            key,
            kind: None,
            result: None,
        });
        Ok(())
    }
    /// 只释放已核对设备上已取得并处理的已结清结果的精确版本。随后 ReadResult 确认回收。
    ///
    /// 已跟踪键的 UID 必须与最近一次 [`Self::identify`] 核对的设备一致。
    pub fn release_result(&mut self, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.require_identity()?;
        let (key, result) = self
            .pending
            .as_ref()
            .map(|p| (p.key, p.result))
            .ok_or_else(|| invalid("没有已跟踪操作"))?;
        self.require_key_identity(key)?;
        let result = result.ok_or_else(|| invalid("尚未取得维护结果"))?;
        if !result.evidence.settled
            || result.revision == 0
            || !matches!(
                result.state,
                MaintenanceState::NotStarted
                    | MaintenanceState::FirmwareStepComplete
                    | MaintenanceState::Failed
            )
        {
            return Err(invalid("结果尚不可释放"));
        }
        let bytes = key.to_bytes();
        let (id, deadline, cancel) = self.send(
            &Message::ReleaseResult {
                key: protocol::OperationKey::new(&bytes).map_err(|e| invalid(format!("{e:?}")))?,
                revision: result.revision,
            },
            wait,
        )?;
        self.await_submission(id, deadline, cancel, wait, true)
    }
    /// 用新的查询确认指定原结果已释放。只有明确 0x0306 才清本地跟踪。
    pub fn confirm_released(&mut self, key: MaintenanceKey, wait: &Wait) -> Result<(), Error> {
        match self.read_result(key, wait) {
            Err(Error {
                failure: Failure::DataUnavailable(f),
                ..
            }) if f.reason == 0x0306 && f.operation_id == key.operation_id => {
                if self.pending_operation() == Some(key) {
                    self.pending = None;
                }
                Ok(())
            }
            Err(e) => Err(e),
            Ok(_) => Err(invalid("固件仍留存结果")),
        }
    }
    /// 保存一次，然后只读核对原结果及完整实际用户记录；不自动释放或复位。
    pub fn save_and_readback(&mut self, record: &[u8], wait: &Wait) -> Result<SavedConfig, Error> {
        let started = Instant::now();
        let submission = self.begin_save(record, wait)?;
        self.finish_save(submission, record, started, wait)
    }
    fn finish_save(
        &mut self,
        submission: OperationSubmission,
        record: &[u8],
        started: Instant,
        wait: &Wait,
    ) -> Result<SavedConfig, Error> {
        // 不发送新业务指令，直到原申请被固件取走的关联事实出现。
        // 否则后发 ReadResult 可覆盖共同缓存中的 SaveConfig。
        let deadline = started
            .checked_add(wait.timeout)
            .ok_or_else(|| invalid("等待期限无法表示"))?;
        while self.pending.as_ref().and_then(|p| p.result).is_none() {
            match self.next_event(deadline, wait) {
                Ok(Event::Message(r)) => self.observe(&r.into()),
                Ok(Event::Disconnected(s)) => {
                    return Err(self.wait_error(
                        Failure::Disconnected(s),
                        LocalStage::FullySubmitted,
                        true,
                    ));
                }
                Ok(Event::TransportError { detail, raw }) => {
                    return Err(self.wait_error(
                        Failure::Transport { detail, raw },
                        LocalStage::FullySubmitted,
                        true,
                    ));
                }
                Err(f) => return Err(self.wait_error(f, LocalStage::FullySubmitted, true)),
                _ => {}
            }
        }
        if let Some(f) = self.pending.as_ref().and_then(|p| p.result)
            && f.revision == 0
        {
            return Err(self.error(
                Failure::DeviceResult(Box::new(f)),
                LocalStage::FullySubmitted,
            ));
        }
        loop {
            let left = wait.timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Err(self.error(
                    Failure::ResultUnknown("等待维护结果到期".into()),
                    LocalStage::FullySubmitted,
                ));
            }
            let step = Wait {
                timeout: left,
                cancellation: wait.cancellation.clone(),
            };
            let reply = self.read_result(submission.key, &step)?;
            let Response::OperationResult(result) = reply.response()? else {
                return Err(invalid("维护结果类型不符"));
            };
            let f = result.fields();
            if f.evidence.settled {
                if f.state != MaintenanceState::FirmwareStepComplete {
                    return Err(self.error(
                        Failure::DeviceResult(Box::new(f)),
                        LocalStage::FullySubmitted,
                    ));
                }
                let step = Wait {
                    timeout: wait.timeout.saturating_sub(started.elapsed()),
                    cancellation: wait.cancellation.clone(),
                };
                let readback = self.read_config(ConfigView::UserRecord, &step)?;
                let Response::ConfigData(data) = readback.response()? else {
                    return Err(invalid("实际读回类型不符"));
                };
                if data.fields().record_state != protocol::responses::ConfigRecordState::Complete {
                    return Err(self.error(
                        Failure::ReadbackUnavailable(data.fields().record_state),
                        LocalStage::FullySubmitted,
                    ));
                }
                if data.data() != record {
                    return Err(self.error(Failure::ReadbackMismatch, LocalStage::FullySubmitted));
                }
                return Ok(SavedConfig {
                    submission,
                    result: f,
                    readback,
                });
            }
            std::thread::sleep(Duration::from_millis(10).min(left));
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::{Backend, Client, Failure, LocalStage, Reply, Wait};
    use crate::protocol::{
        Response, SampleData,
        responses::{
            CanProfile, ContactState, DesiredAxis, EvidenceState, FallbackReason, IdentityFields,
            IdentityText, ReferenceKind, RunIdentity, SourceQuality, StartupSource, TargetIngress,
            TargetMode, TelemetryFacts, TelemetryFields, ValueFields, ValueResult, ValueState,
            encode_identity, encode_telemetry,
        },
    };
    use std::{
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };

    fn available(value: f32) -> ValueFields {
        ValueFields::new(
            value,
            ValueState::new(ValueResult::Available, SourceQuality::Qualified, false),
        )
    }

    fn never() -> ValueFields {
        ValueFields::new(
            0.0,
            ValueState::new(ValueResult::Never, SourceQuality::Unknown, false),
        )
    }

    fn telemetry(run_nonce: [u8; 16], sequence: u32) -> Vec<u8> {
        let fields = TelemetryFields {
            sample: SampleData {
                query_id: 0,
                run_nonce,
                snapshot_sequence: sequence,
                snapshot_time_us: u64::from(sequence) * 1_000,
            },
            decision_age_us: 0,
            target_mode: TargetMode::None,
            target_ingress: TargetIngress::None,
            facts: TelemetryFacts::default(),
            target_values: [0.0; 3],
            can_adoption_blockers: 0,
            usb_adoption_blockers: 0,
            output_blockers: 0,
            last_end_reason: 0,
            limits: Default::default(),
            main_values: [available(1.0); 5],
            position_age_us: 0,
            velocity_age_us: 0,
            pressure_pair_age_us: 0,
            reference: never(),
            reference_kind: ReferenceKind::None,
            candidate_rpm: available(0.0),
            last_submitted_rpm: available(0.0),
            submitted_age_us: 0,
            axis_state_raw: 0,
            axis_error_raw: 0,
            driver_age_us: u32::MAX,
            driver_state: Default::default(),
            desired_axis: DesiredAxis::Idle,
            can_heartbeat_age_us: 0,
            usb_heartbeat_age_us: 0,
            can_contact: ContactState::Expired,
            usb_contact: ContactState::Expired,
        };
        let mut bytes = vec![0; TelemetryFields::encoded_len()];
        encode_telemetry(&fields, &mut bytes).expect("测试遥测符合公共合同");
        bytes
    }

    fn identity(run_nonce: [u8; 16]) -> Vec<u8> {
        let fields = IdentityFields {
            sample: SampleData {
                query_id: 1,
                run_nonce,
                snapshot_sequence: 1,
                snapshot_time_us: 1,
            },
            uid: [7; 12],
            config_source: StartupSource::User,
            fallback_reason: FallbackReason::None,
            run_identity: RunIdentity::Random,
            update_route: crate::protocol::responses::UpdateRoute::Present,
            active_can_node: 1,
            active_can_profile: CanProfile::Classical500k,
            host_heartbeat_hz: 50,
            telemetry_hz: 100,
            host_contact_max_age_ms: 100,
            config_format_version: 1,
            build_evidence: EvidenceState::Available,
            odrive_evidence: EvidenceState::Unavailable,
            brt27_evidence: EvidenceState::Unavailable,
            high_water_operation_id: 0,
            retained_operation_id: 0,
            text: IdentityText {
                version: "",
                source_revision: "",
                build_information: "",
                odrive_binding: "",
                odrive_observed_version: "",
                brt27_binding: "",
                brt27_observed_version: "",
                update_binding: "",
            },
        };
        let mut bytes = vec![0; fields.encoded_len().expect("测试身份长度")];
        encode_identity(&fields, &mut bytes).expect("测试身份符合公共合同");
        bytes
    }

    fn verified_client(run_nonce: [u8; 16]) -> Client {
        let backend = Backend::spawn("telemetry-queue-test", |rx, _| {
            while let Ok(command) = rx.recv() {
                if matches!(command, crate::host::backend::Command::Close) {
                    break;
                }
            }
        })
        .expect("测试后端可启动");
        let mut client = Client::new(backend);
        let reply = Reply {
            bytes: identity(run_nonce),
            received_at: Instant::now(),
        };
        let Response::Identity(identity) = reply.response().expect("测试身份可解码") else {
            panic!("测试身份必须是 Identity");
        };
        client
            .session
            .observe_identity(&identity)
            .expect("测试身份可绑定");
        client.identity_verified = true;
        client
    }

    #[test]
    fn telemetry_queue_keeps_current_run_in_order_and_reports_real_overflow() {
        let run = [1; 16];
        let old_run = [2; 16];
        let mut client = verified_client(run);
        client
            .enable_telemetry_queue(3)
            .expect("非零容量可以开启队列");
        client.backend.sink.received(telemetry(old_run, 1));
        client.backend.sink.received(telemetry(run, 2));
        client.backend.sink.received(telemetry(run, 3));

        let first = client.drain_telemetry();
        assert_eq!(first.dropped, 0);
        let first_sequences: Vec<_> = first
            .replies
            .iter()
            .map(|reply| match reply.response().expect("队列回复可解码") {
                Response::Telemetry(value) => value.fields().sample.snapshot_sequence,
                _ => panic!("队列只保留 Telemetry"),
            })
            .collect();
        assert_eq!(first_sequences, [2, 3]);
        assert!(client.drain_telemetry().replies.is_empty());

        client.backend.sink.received(telemetry(run, 4));
        client.backend.sink.received(telemetry(run, 5));
        client.backend.sink.received(telemetry(run, 6));
        client.backend.sink.received(telemetry(run, 7));
        let overflow = client.drain_telemetry();
        assert_eq!(overflow.dropped, 1);
        let overflow_sequences: Vec<_> = overflow
            .replies
            .iter()
            .map(|reply| match reply.response().expect("队列回复可解码") {
                Response::Telemetry(value) => value.fields().sample.snapshot_sequence,
                _ => panic!("队列只保留 Telemetry"),
            })
            .collect();
        assert_eq!(overflow_sequences, [5, 6, 7]);
        let latest = client.telemetry().expect("latest telemetry 仍可读取");
        let Response::Telemetry(latest) = latest.response().expect("latest 可解码") else {
            panic!("latest 必须是 Telemetry");
        };
        assert_eq!(
            latest.fields().sample.snapshot_sequence,
            7,
            "开启队列不改变 latest-only telemetry 读取语义"
        );
    }

    #[test]
    fn ordinary_event_fifo_overflow_is_observable_without_claiming_not_submitted() {
        let mut client = verified_client([1; 16]);
        for id in 1..=65 {
            client.backend.sink.submitted(id);
        }

        let wait = Wait::new(Duration::from_millis(10));
        let error = client
            .await_submission(
                1,
                Instant::now() + wait.timeout,
                Arc::new(AtomicBool::new(false)),
                &wait,
                true,
            )
            .expect_err("第 65 个普通事件会淘汰等待中的 Submitted");
        assert!(matches!(error.failure, Failure::ResultUnknown(_)));
        assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
        assert!(
            error.observed_transport.as_deref().is_some_and(|status| {
                status.errors >= 1
                    && matches!(
                        status.last_error.as_ref(),
                        Some((detail, raw, _))
                            if detail == "普通事件 FIFO 已满，已淘汰最早事件" && raw.is_empty()
                    )
            }),
            "调用方必须能观察普通事件 FIFO 的本地淘汰"
        );
    }

    #[test]
    fn ordinary_event_fifo_reply_overflow_keeps_fully_submitted_stage() {
        let mut client = verified_client([1; 16]);
        client.backend.sink.received(identity([1; 16]));
        for id in 2..=65 {
            client.backend.sink.submitted(id);
        }
        client.backend.sink.submitted(1);

        let error = client
            .query(
                crate::session::QueryKind::Identity,
                &Wait::new(Duration::from_millis(10)),
            )
            .expect_err("早到的匹配 Reply 被淘汰后不能伪造未提交");
        assert!(matches!(error.failure, Failure::Timeout));
        assert_eq!(error.local, LocalStage::FullySubmitted);
        assert!(
            error.observed_transport.as_deref().is_some_and(|status| {
                status.errors >= 1
                    && matches!(
                        status.last_error.as_ref(),
                        Some((detail, raw, _))
                            if detail == "普通事件 FIFO 已满，已淘汰最早事件" && raw.is_empty()
                    )
            }),
            "调用方必须能观察普通事件 FIFO 的本地淘汰"
        );
    }
}
