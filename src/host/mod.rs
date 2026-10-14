// Copyright The eha-sdk Contributors
//! 桌面客户 API。`Client` 串行提交业务，独立 I/O 线程维持收发和显式心跳。
//! 所有数据副本拥有自己的字节；本地提交绝不表示目标采用或设备执行。
pub mod backend;

use crate::session::{MutationKind, QueryKind, Session};
pub use backend::{AdapterStatus, Backend, HeartbeatStatus, TransportStatus};
use backend::{Command, Event, Received, SendRequest};
use protocol::responses::{ConfigView, DataUnavailableFields};
use protocol::{Direction, ExpectedIdentity, Message, Response};
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

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::{Backend, Client, Failure, LocalStage, Reply, Wait};
    use crate::protocol::{
        Response, SampleData,
        responses::{
            self, CanProfile, ContactState, DesiredAxis, EvidenceState, FallbackReason,
            IdentityFields, IdentityText, ReferenceKind, RunIdentity, SourceQuality, StartupSource,
            TargetIngress, TargetMode, TelemetryFacts, TelemetryFields, ValueFields, ValueResult,
            ValueState,
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
        responses::encode_telemetry(&fields, &mut bytes).expect("telemetry encodes");
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
            update_route: responses::UpdateRoute::Present,
            active_can_node: 1,
            active_can_profile: CanProfile::Classical500k,
            host_heartbeat_hz: 50,
            telemetry_hz: 100,
            host_contact_max_age_ms: 100,
            config_format_version: 1,
            build_evidence: EvidenceState::Available,
            odrive_evidence: EvidenceState::Unavailable,
            brt27_evidence: EvidenceState::Unavailable,
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
        let mut bytes = vec![0; fields.encoded_len().expect("identity length")];
        responses::encode_identity(&fields, &mut bytes).expect("identity encodes");
        bytes
    }
    fn verified_client(run_nonce: [u8; 16]) -> Client {
        let backend = Backend::spawn("telemetry-queue-test", |receiver, _| {
            while let Ok(command) = receiver.recv() {
                if matches!(command, crate::host::backend::Command::Close) {
                    break;
                }
            }
        })
        .expect("backend starts");
        let mut client = Client::new(backend);
        let reply = Reply {
            bytes: identity(run_nonce),
            received_at: Instant::now(),
        };
        let Response::Identity(identity) = reply.response().expect("identity decodes") else {
            panic!("identity expected")
        };
        client
            .session
            .observe_identity(&identity)
            .expect("identity binds");
        client.identity_verified = true;
        client
    }

    #[test]
    fn telemetry_queue_keeps_current_run_in_order_and_reports_real_overflow() {
        let run = [1; 16];
        let mut client = verified_client(run);
        client.enable_telemetry_queue(3).expect("queue enabled");
        client.backend.sink.received(telemetry([2; 16], 1));
        client.backend.sink.received(telemetry(run, 2));
        client.backend.sink.received(telemetry(run, 3));
        let initial = client.drain_telemetry();
        assert_eq!(initial.dropped, 0);
        assert_eq!(initial.replies.len(), 2);
        client.backend.sink.received(telemetry(run, 4));
        client.backend.sink.received(telemetry(run, 5));
        client.backend.sink.received(telemetry(run, 6));
        client.backend.sink.received(telemetry(run, 7));
        let overflow = client.drain_telemetry();
        assert_eq!(overflow.dropped, 1);
        assert_eq!(overflow.replies.len(), 3);
        let latest = client.telemetry().expect("latest telemetry");
        let Response::Telemetry(value) = latest.response().expect("telemetry decodes") else {
            panic!("telemetry expected")
        };
        assert_eq!(value.fields().sample.snapshot_sequence, 7);
    }

    #[test]
    fn ordinary_event_fifo_overflow_is_observable_without_claiming_not_submitted() {
        let client = verified_client([1; 16]);
        for id in 1..=65 {
            client.backend.sink.submitted(id);
        }
        let wait = Wait::new(Duration::from_millis(10));
        let error = client
            .await_submission(
                None,
                1,
                Instant::now() + wait.timeout,
                Arc::new(AtomicBool::new(false)),
                &wait,
                true,
            )
            .expect_err("evicted submission remains unknown");
        assert!(matches!(error.failure, Failure::ResultUnknown(_)));
        assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
        assert!(
            error
                .observed_transport
                .as_deref()
                .is_some_and(|status| status.errors >= 1)
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
            .expect_err("evicted early reply cannot become success");
        assert!(matches!(error.failure, Failure::Timeout));
        assert_eq!(error.local, LocalStage::FullySubmitted);
        assert!(
            error
                .observed_transport
                .as_deref()
                .is_some_and(|status| status.errors >= 1)
        );
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
    Transport {
        detail: String,
        raw: Vec<u8>,
    },
    DataUnavailable(DataUnavailableFields),
    /// 本地提交边界不明，设备副作用可能已经开始。
    ResultUnknown(String),
}

#[derive(Clone, Debug)]
pub struct Error {
    pub failure: Failure,
    pub local: LocalStage,
    /// 接收路径独立异常，未声称它就是本次请求失败的原因。
    pub observed_transport: Option<Box<TransportStatus>>,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}; local={:?}", self.failure, self.local)
    }
}
impl std::error::Error for Error {}

/// 收到的不可变完整回复及主机接收时刻；读取它不会刷新来源时间。
#[derive(Clone, Debug)]
pub struct Reply {
    bytes: Vec<u8>,
    pub received_at: Instant,
}
impl Reply {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn response(&self) -> Result<Response<'_>, Error> {
        match protocol::decode(&self.bytes, Direction::FirmwareToHost) {
            Ok(Message::Response(response)) => Ok(response),
            _ => Err(invalid("无效的固件回复")),
        }
    }
}
impl From<Received> for Reply {
    fn from(received: Received) -> Self {
        Self {
            bytes: received.bytes,
            received_at: received.at,
        }
    }
}

/// 一次从显式开启的周期遥测队列取得的增量。
#[derive(Clone, Debug, Default)]
pub struct TelemetryDrain {
    pub replies: Vec<Reply>,
    pub dropped: u64,
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

/// 可携带至重新打开连接的会话状态。保存查询编号和本地调用编号，防止重连重用编号。
#[derive(Clone, Debug)]
pub struct ClientState {
    session: Session,
    sequence: u64,
    heartbeat_hz: Option<u32>,
}

/// 一台设备的串行业务调用方；打开后须先核对身份，资源释放不发送控制或停止。
pub struct Client {
    backend: Backend,
    identity_verified: bool,
    session: Session,
    sequence: u64,
    heartbeat_hz: Option<u32>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error {
        failure: Failure::InvalidInput(message.into()),
        local: LocalStage::NotSubmitted,
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
            heartbeat_hz: state.heartbeat_hz,
        }
    }

    /// 关闭本地 I/O 并保留关联状态，供通信恢复后 resume 使用。
    pub fn disconnect(self) -> ClientState {
        ClientState {
            session: self.session,
            sequence: self.sequence,
            heartbeat_hz: self.heartbeat_hz,
        }
    }

    /// 关闭只回收本地连接，绝不发送 Stop、Reset 或重放目标。
    pub fn close(self) {}
    pub fn session(&self) -> &Session {
        &self.session
    }

    fn error(&self, failure: Failure, local: LocalStage) -> Error {
        Error {
            failure,
            local,
            observed_transport: Some(Box::new(self.transport_status())),
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

    fn encode(message: &Message<'_>) -> Result<Vec<u8>, Error> {
        let length = message
            .encoded_len()
            .map_err(|error| invalid(format!("{error:?}")))?;
        let mut bytes = vec![0; length];
        protocol::encode(*message, &mut bytes).map_err(|error| invalid(format!("{error:?}")))?;
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
            .map_err(|error| {
                self.error(
                    Failure::Disconnected(error.to_string()),
                    LocalStage::NotSubmitted,
                )
            })?;
        Ok((self.sequence, deadline, cancelled))
    }

    fn next_event(&self, deadline: Instant, wait: &Wait) -> Result<Event, Failure> {
        let (lock, condition) = &*self.backend.sink.0;
        let mut inbox = lock
            .lock()
            .map_err(|_| Failure::Disconnected("I/O 状态锁失效".into()))?;
        loop {
            if wait.cancellation.is_cancelled() {
                return Err(Failure::Cancelled);
            }
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
            inbox = condition
                .wait_timeout(inbox, remaining.min(Duration::from_millis(10)))
                .map_err(|_| Failure::Disconnected("I/O 等待锁失效".into()))?
                .0;
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

    fn await_submission(
        &self,
        matcher: Option<crate::session::MutationMatch>,
        id: u64,
        deadline: Instant,
        cancel: Arc<AtomicBool>,
        wait: &Wait,
        mutation: bool,
    ) -> Result<LocalSubmission, Error> {
        loop {
            match self.next_event(deadline, wait) {
                Ok(Event::Submitted(got)) if got == id => {
                    return Ok(LocalSubmission {
                        id,
                        submitted_at: Instant::now(),
                        boundary: self.backend.boundary,
                    });
                }
                Ok(Event::Failed(got, failure)) if got == id => {
                    return Err(self.wait_error(
                        Failure::Transport {
                            detail: failure.detail,
                            raw: Vec::new(),
                        },
                        if failure.may_have_sent {
                            LocalStage::MayHaveBeenSubmitted
                        } else {
                            LocalStage::NotSubmitted
                        },
                        mutation && failure.may_have_sent,
                    ));
                }
                Ok(Event::Message(received)) => {
                    let reply: Reply = received.into();
                    if let (Some(matcher), Ok(response @ Response::DataUnavailable(value))) =
                        (matcher, reply.response())
                        && matcher.matches(&response)
                    {
                        return Err(self.error(
                            Failure::DataUnavailable(value.fields()),
                            LocalStage::MayHaveBeenSubmitted,
                        ));
                    }
                }
                Ok(Event::Disconnected(reason)) => {
                    return Err(self.wait_error(
                        Failure::Disconnected(reason),
                        LocalStage::MayHaveBeenSubmitted,
                        mutation,
                    ));
                }
                Ok(Event::TransportError { detail, raw }) => {
                    cancel.store(true, Ordering::Release);
                    return Err(self.wait_error(
                        Failure::Transport { detail, raw },
                        LocalStage::MayHaveBeenSubmitted,
                        mutation,
                    ));
                }
                Ok(_) => {}
                Err(failure) => {
                    cancel.store(true, Ordering::Release);
                    return Err(self.wait_error(
                        failure,
                        LocalStage::MayHaveBeenSubmitted,
                        mutation,
                    ));
                }
            }
        }
    }

    /// 查询身份，并可严格匹配外部交付的 UID。
    pub fn identify(
        &mut self,
        expected_uid: Option<[u8; 12]>,
        wait: &Wait,
    ) -> Result<Reply, Error> {
        self.identity_verified = false;
        let expected_uid =
            expected_uid.or_else(|| self.session.identity().map(|identity| identity.uid));
        let reply = self.query(QueryKind::Identity, wait)?;
        let Response::Identity(identity) = reply.response()? else {
            return Err(invalid("身份回复类别不符"));
        };
        let fields = identity.fields();
        if expected_uid.is_some_and(|uid| uid != fields.uid) {
            return Err(self.error(
                Failure::Session("设备 UID 不匹配".into()),
                LocalStage::FullySubmitted,
            ));
        }
        self.session.observe_identity(&identity).map_err(|error| {
            self.error(
                Failure::Session(format!("{error:?}")),
                LocalStage::FullySubmitted,
            )
        })?;
        self.identity_verified = true;
        self.heartbeat_hz = Some(fields.host_heartbeat_hz);
        Ok(reply)
    }

    pub fn status(&mut self, wait: &Wait) -> Result<Reply, Error> {
        self.query(QueryKind::Status, wait)
    }
    pub fn measurements(&mut self, wait: &Wait) -> Result<Reply, Error> {
        self.query(QueryKind::Measurements, wait)
    }
    pub fn diagnostics(&mut self, wait: &Wait) -> Result<Reply, Error> {
        self.query(QueryKind::Diagnostics, wait)
    }
    pub fn read_config(&mut self, view: ConfigView, wait: &Wait) -> Result<Reply, Error> {
        self.query(QueryKind::Config(view as u8), wait)
    }

    fn query(&mut self, kind: QueryKind, wait: &Wait) -> Result<Reply, Error> {
        if kind != QueryKind::Identity {
            self.require_identity()?;
        }
        let matcher = self.session.next_query(kind).map_err(|error| {
            self.error(
                Failure::Session(format!("{error:?}")),
                LocalStage::NotSubmitted,
            )
        })?;
        let message = matcher.request();
        let (id, deadline, cancel) = self.send(&message, wait)?;
        let mut stage = LocalStage::MayHaveBeenSubmitted;
        let mut early_reply = None;
        loop {
            match self.next_event(deadline, wait) {
                Ok(Event::Submitted(got)) if got == id => {
                    stage = LocalStage::FullySubmitted;
                    if let Some(reply) = early_reply.take() {
                        return self.finish_query_reply(reply, stage);
                    }
                }
                Ok(Event::Failed(got, failure)) if got == id => {
                    return Err(self.error(
                        Failure::Transport {
                            detail: failure.detail,
                            raw: Vec::new(),
                        },
                        if failure.may_have_sent {
                            stage
                        } else {
                            LocalStage::NotSubmitted
                        },
                    ));
                }
                Ok(Event::Message(received)) => {
                    let reply: Reply = received.into();
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
                Ok(Event::Disconnected(reason)) => {
                    return Err(self.error(Failure::Disconnected(reason), stage));
                }
                Ok(Event::TransportError { detail, raw }) => {
                    cancel.store(true, Ordering::Release);
                    return Err(self.error(Failure::Transport { detail, raw }, stage));
                }
                Ok(_) => {}
                Err(failure) => {
                    cancel.store(true, Ordering::Release);
                    return Err(self.error(failure, stage));
                }
            }
        }
    }

    fn finish_query_reply(&self, reply: Reply, stage: LocalStage) -> Result<Reply, Error> {
        match reply.response()? {
            Response::DataUnavailable(unavailable) => {
                Err(self.error(Failure::DataUnavailable(unavailable.fields()), stage))
            }
            _ => Ok(reply),
        }
    }

    /// 取得独立接收的最新遥测，保留主机接收时刻。旧 run 的副本不会冒充当前数据。
    ///
    /// 缓存不会在本地断连时清除；调用方须结合 [`Self::connection_status`] 和
    /// `received_at` 判断连接与数据年龄。
    pub fn telemetry(&self) -> Option<Reply> {
        if !self.identity_verified {
            return None;
        }
        let reply: Reply = self.backend.sink.0.0.lock().ok()?.telemetry.clone()?.into();
        let identity = self.session.identity()?;
        matches!(reply.response().ok(), Some(Response::Telemetry(value)) if value.fields().sample.run_nonce == identity.run_nonce)
            .then_some(reply)
    }

    /// 为连续展示显式启用有界 Telemetry 接收队列。
    ///
    /// 默认仍只保留最新一帧。重新打开连接会创建新后端，调用方须再次显式开启。
    pub fn enable_telemetry_queue(&mut self, capacity: usize) -> Result<(), Error> {
        self.backend
            .sink
            .enable_telemetry_queue(capacity)
            .map_err(invalid)
    }
    /// 取走自上次调用后保留的当前运行实例 Telemetry。
    ///
    /// 这是单消费者队列；尚未完成 Identity 核对时不取走任何项目，旧运行实例的帧会被丢弃。
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
            .filter(|reply| matches!(reply.response(), Ok(Response::Telemetry(value)) if value.fields().sample.run_nonce == identity.run_nonce))
            .collect();
        TelemetryDrain { replies, dropped }
    }
    /// 取得本地连接断开和最近遥测缓存的事实，不将二者解释为固件状态或新鲜度。
    pub fn connection_status(&self) -> ConnectionStatus {
        let Ok(inbox) = self.backend.sink.0.0.lock() else {
            return ConnectionStatus::default();
        };
        ConnectionStatus {
            disconnected: inbox.disconnected.clone(),
            last_telemetry_received_at: inbox.telemetry.as_ref().map(|telemetry| telemetry.at),
        }
    }
    pub fn transport_status(&self) -> TransportStatus {
        self.backend
            .sink
            .0
            .0
            .lock()
            .map(|inbox| inbox.transport.clone())
            .unwrap_or_default()
    }
    pub fn adapter_status(&self) -> AdapterStatus {
        self.backend
            .sink
            .0
            .0
            .lock()
            .map(|inbox| inbox.adapter.clone())
            .unwrap_or_default()
    }
    pub fn heartbeat_status(&self) -> HeartbeatStatus {
        self.backend
            .sink
            .0
            .0
            .lock()
            .map(|inbox| inbox.heartbeat.clone())
            .unwrap_or_default()
    }
    pub fn start_heartbeat(&mut self) -> Result<(), Error> {
        self.require_identity()?;
        let hz = self
            .heartbeat_hz
            .filter(|hz| *hz > 0)
            .ok_or_else(|| invalid("身份未给出有效心跳频率"))?;
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
            .map_err(|error| invalid(error.to_string()))
    }
    pub fn stop_heartbeat(&mut self) -> Result<(), Error> {
        self.backend
            .tx
            .try_send(Command::Heartbeat(None))
            .map_err(|error| invalid(error.to_string()))
    }
    pub fn heartbeat(&mut self, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Heartbeat, wait)
    }
    pub fn position(&mut self, mm: f32, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Position(mm), wait)
    }
    pub fn velocity(&mut self, mm_s: f32, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Velocity(mm_s), wait)
    }
    pub fn force(&mut self, n: f32, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit(Message::Force(n), wait)
    }
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
        self.await_submission(None, id, deadline, cancel, wait, true)
    }

    /// 静态校验后提交保存并重启；只等待本地 `Submitted` 或本次前置拒绝。
    pub fn save_and_restart(
        &mut self,
        record: &[u8],
        wait: &Wait,
    ) -> Result<LocalSubmission, Error> {
        crate::configuration::validate_json(record).map_err(|error| invalid(error.to_string()))?;
        self.submit_mutation(MutationKind::SaveAndRestart, Some(record), wait)
    }
    /// 读取同一已核对设备的 Factory 原字节，校验后提交保存并重启。
    pub fn restore_factory_and_restart(&mut self, wait: &Wait) -> Result<LocalSubmission, Error> {
        let started = Instant::now();
        let factory = self.read_config(ConfigView::Factory, wait)?;
        let Response::ConfigData(data) = factory.response()? else {
            return Err(invalid("缺少出厂记录"));
        };
        let identity = self
            .session
            .identity()
            .ok_or_else(|| invalid("缺少已核对身份"))?;
        if identity.config_format_version != config::FORMAT_VERSION {
            return Err(invalid(format!(
                "设备配置格式版本 {} 与本地校验器版本 {} 不匹配",
                identity.config_format_version,
                config::FORMAT_VERSION
            )));
        }
        let remaining = wait.timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(self.error(Failure::Timeout, LocalStage::NotSubmitted));
        }
        self.save_and_restart(
            data.data(),
            &Wait {
                timeout: remaining,
                cancellation: wait.cancellation.clone(),
            },
        )
    }
    /// 明确请求应用复位；断连不证明新应用已启动。随后须核对同 UID 的新 nonce。
    pub fn reset_application(&mut self, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit_mutation(MutationKind::ResetApplication, None, wait)
    }
    /// 明确请求进入已交付的更新入口；本地提交不证明路由成立或更新成功。
    pub fn enter_update(&mut self, wait: &Wait) -> Result<LocalSubmission, Error> {
        self.submit_mutation(MutationKind::EnterUpdate, None, wait)
    }
    fn submit_mutation(
        &mut self,
        kind: MutationKind,
        record: Option<&[u8]>,
        wait: &Wait,
    ) -> Result<LocalSubmission, Error> {
        self.require_identity()?;
        let matcher = self.session.next_mutation(kind).map_err(|error| {
            self.error(
                Failure::Session(format!("{error:?}")),
                LocalStage::NotSubmitted,
            )
        })?;
        let bytes = matcher.identity.to_bytes();
        let identity =
            ExpectedIdentity::new(&bytes).map_err(|error| invalid(format!("{error:?}")))?;
        let message = match kind {
            MutationKind::SaveAndRestart => Message::SaveAndRestart {
                query_id: matcher.query_id,
                identity,
                record: record.ok_or_else(|| invalid("缺少完整记录"))?,
            },
            MutationKind::ResetApplication => Message::ResetApplication {
                query_id: matcher.query_id,
                identity,
            },
            MutationKind::EnterUpdate => Message::EnterUpdate {
                query_id: matcher.query_id,
                identity,
            },
        };
        let (id, deadline, cancel) = self.send(&message, wait)?;
        self.await_submission(Some(matcher), id, deadline, cancel, wait, true)
    }
}
