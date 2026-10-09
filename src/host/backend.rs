// Copyright The eha-sdk Contributors
//! 桌面运行装配的有界消息交接。平台后端负责独占 I/O 与传输绑定。
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct IoFailure {
    pub detail: String,
    pub may_have_sent: bool,
}
#[derive(Clone, Debug)]
pub struct Received {
    pub bytes: Vec<u8>,
    pub at: Instant,
}
#[derive(Clone, Debug, Default)]
pub struct HeartbeatStatus {
    pub enabled: bool,
    pub submitted: u64,
    pub missed: u64,
    pub last_submitted: Option<Instant>,
    pub error: Option<String>,
}
#[derive(Clone, Debug)]
pub enum Event {
    Submitted(u64),
    Failed(u64, IoFailure),
    Message(Received),
    Disconnected(String),
    TransportError { detail: String, raw: Vec<u8> },
}
/// 适配器本地通知，不能关联为某条 EHA 消息接收或执行确认。
#[derive(Clone, Debug, Default)]
pub struct AdapterStatus {
    pub notices: u64,
    pub last_notice: Option<(String, Vec<u8>, Instant)>,
}
/// 实际接收异常的独立诊断；没有足够信息时不能归因给某条业务请求。
#[derive(Clone, Debug, Default)]
pub struct TransportStatus {
    pub errors: u64,
    pub last_error: Option<(String, Vec<u8>, Instant)>,
}
#[derive(Default)]
pub(crate) struct Inbox {
    pub events: VecDeque<Event>,
    pub telemetry: Option<Received>,
    /// 仅在调用方明确开启时保留的周期遥测。默认只保留 `telemetry` 的最新值，
    /// 以免普通 SDK 客户端因未消费的展示数据无限占用内存。
    pub telemetry_queue: Option<TelemetryQueue>,
    pub heartbeat: HeartbeatStatus,
    pub adapter: AdapterStatus,
    pub transport: TransportStatus,
    pub dropped: u64,
    pub disconnected: Option<String>,
}

pub(crate) struct TelemetryQueue {
    capacity: usize,
    received: VecDeque<Received>,
    dropped: u64,
}
#[derive(Clone)]
pub struct EventSink(pub(crate) Arc<(Mutex<Inbox>, Condvar)>);
impl EventSink {
    pub fn event(&self, event: Event) {
        let (lock, cv) = &*self.0;
        if let Ok(mut inbox) = lock.lock() {
            if let Event::Message(message) = &event
                && matches!(
                    protocol::decode(&message.bytes, protocol::Direction::FirmwareToHost),
                    Ok(protocol::Message::Response(protocol::Response::Telemetry(
                        _
                    )))
                )
            {
                inbox.telemetry = Some(message.clone());
                if let Some(queue) = inbox.telemetry_queue.as_mut() {
                    if queue.received.len() == queue.capacity {
                        queue.received.pop_front();
                        queue.dropped = queue.dropped.saturating_add(1);
                    }
                    queue.received.push_back(message.clone());
                }
                cv.notify_all();
                return;
            }
            if let Event::Disconnected(reason) = &event {
                inbox.disconnected = Some(reason.clone());
            }
            if inbox.events.len() == 64 {
                inbox.events.pop_front();
                inbox.dropped += 1;
            }
            inbox.events.push_back(event);
            cv.notify_all();
        }
    }
    pub fn adapter_notice(&self, detail: impl Into<String>, raw: Vec<u8>) {
        if let Ok(mut i) = self.0.0.lock() {
            i.adapter.notices += 1;
            i.adapter.last_notice = Some((detail.into(), raw, Instant::now()));
        }
    }
    pub fn disconnected(&self, reason: impl Into<String>) {
        self.event(Event::Disconnected(reason.into()));
    }
    pub fn transport_error(&self, detail: impl Into<String>, raw: Vec<u8>) {
        let detail = detail.into();
        if let Ok(mut i) = self.0.0.lock() {
            i.transport.errors += 1;
            i.transport.last_error = Some((detail, raw, Instant::now()));
        }
    }
    pub fn received(&self, bytes: Vec<u8>) {
        self.event(Event::Message(Received {
            bytes,
            at: Instant::now(),
        }));
    }
    pub fn submitted(&self, id: u64) {
        if id == 0 {
            if let Ok(mut i) = self.0.0.lock() {
                i.heartbeat.submitted += 1;
                i.heartbeat.last_submitted = Some(Instant::now());
            }
        } else {
            self.event(Event::Submitted(id));
        }
    }
    pub fn failed(&self, id: u64, detail: impl Into<String>, may_have_sent: bool) {
        let detail = detail.into();
        if id == 0 {
            if let Ok(mut i) = self.0.0.lock() {
                i.heartbeat.error = Some(detail);
                i.heartbeat.missed += 1;
            }
        } else {
            self.event(Event::Failed(
                id,
                IoFailure {
                    detail,
                    may_have_sent,
                },
            ));
        }
    }

    pub(crate) fn enable_telemetry_queue(&self, capacity: usize) -> Result<(), String> {
        if capacity == 0 {
            return Err("遥测队列容量必须大于零".into());
        }
        let mut inbox = self.0.0.lock().map_err(|_| "遥测队列锁已中毒")?;
        inbox.telemetry_queue = Some(TelemetryQueue {
            capacity,
            received: VecDeque::with_capacity(capacity),
            dropped: 0,
        });
        Ok(())
    }

    pub(crate) fn drain_telemetry_queue(&self) -> (Vec<Received>, u64) {
        let Ok(mut inbox) = self.0.0.lock() else {
            return (Vec::new(), 0);
        };
        let Some(queue) = inbox.telemetry_queue.as_mut() else {
            return (Vec::new(), 0);
        };
        let dropped = std::mem::take(&mut queue.dropped);
        (queue.received.drain(..).collect(), dropped)
    }
}

pub struct SendRequest {
    pub id: u64,
    pub bytes: Vec<u8>,
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
}
impl SendRequest {
    pub fn expired(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline
    }
    pub fn lane(&self) -> usize {
        if self.bytes.get(1) == Some(&(protocol::MessageKind::Heartbeat as u8)) {
            0
        } else if self.bytes.len() <= 256 {
            1
        } else {
            2
        }
    }
}
pub enum Command {
    Send(SendRequest),
    Heartbeat(Option<Duration>),
    Close,
}
/// 后端公共调度：只显式开启心跳，业务等待不影响该线程运行。
pub struct Pump {
    rx: Receiver<Command>,
    pub sink: EventSink,
    pending: [Option<SendRequest>; 3],
    period: Option<Duration>,
    next: Instant,
    epoch: Instant,
    closed: bool,
}
impl Pump {
    pub fn new(rx: Receiver<Command>, sink: EventSink) -> Self {
        Self {
            rx,
            sink,
            pending: [None, None, None],
            period: None,
            next: Instant::now(),
            epoch: Instant::now(),
            closed: false,
        }
    }
    pub fn with_epoch(mut self, epoch: Instant) -> Self {
        self.epoch = epoch;
        self
    }
    pub fn epoch(&self) -> Instant {
        self.epoch
    }
    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    /// 必须持续调用，并在每个 CAN 帧／USB 完整 COBS 块边界检查心跳。
    pub fn poll(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(Command::Send(req)) => {
                    let lane = req.lane();
                    if self.pending[lane].is_some() {
                        self.sink.failed(req.id, "本地发送槽占用", false);
                    } else {
                        self.pending[lane] = Some(req);
                    }
                }
                Ok(Command::Heartbeat(period)) => {
                    self.period = period;
                    self.next = Instant::now();
                    if period.is_none() {
                        self.pending[0] = None;
                    }
                    if let Ok(mut i) = self.sink.0.0.lock() {
                        i.heartbeat.enabled = period.is_some();
                    }
                }
                Ok(Command::Close) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.closed = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        let now = Instant::now();
        if let Some(period) = self.period
            && now >= self.next
        {
            let missed = now.duration_since(self.next).as_nanos() / period.as_nanos();
            if let Ok(mut i) = self.sink.0.0.lock() {
                i.heartbeat.missed = i
                    .heartbeat
                    .missed
                    .saturating_add(missed.min(u128::from(u64::MAX)) as u64);
            }
            self.next = now + period;
            if self.pending[0].is_none() {
                let mut bytes = vec![0; 8];
                if protocol::encode(protocol::Message::Heartbeat, &mut bytes).is_ok() {
                    self.pending[0] = Some(SendRequest {
                        id: 0,
                        bytes,
                        deadline: now + period,
                        cancelled: Arc::new(AtomicBool::new(false)),
                    });
                }
            } else if let Ok(mut i) = self.sink.0.0.lock() {
                i.heartbeat.missed += 1;
            }
        }
        !self.closed
    }
    pub fn take(&mut self, lane: usize) -> Option<SendRequest> {
        let req = self.pending[lane].take()?;
        if req.expired() {
            self.sink.failed(req.id, "发送前已取消或到期", false);
            None
        } else {
            Some(req)
        }
    }
    pub fn put_back(&mut self, req: SendRequest) {
        let lane = req.lane();
        self.pending[lane] = Some(req);
    }
    pub fn disconnected(&self, reason: impl Into<String>) {
        self.sink.event(Event::Disconnected(reason.into()));
    }
}
/// 一个 I/O owner；Drop 仅关闭通道并归还本地句柄。
pub struct Backend {
    pub(crate) tx: SyncSender<Command>,
    pub(crate) sink: EventSink,
    join: Option<JoinHandle<()>>,
    pub(crate) boundary: &'static str,
}
impl Backend {
    /// 建立由另一条 I/O worker 驱动的逻辑 endpoint。
    ///
    /// 该 endpoint 不取得物理 I/O 所有权；调用方必须把返回的命令接收端和事件 sink
    /// 交给唯一的物理 I/O worker。`join` 可随后由该 worker 附加，使释放 endpoint 时
    /// 先结束其逻辑调度再返回。
    pub(crate) fn endpoint() -> (Self, Receiver<Command>, EventSink) {
        let (tx, rx) = mpsc::sync_channel(8);
        let sink = EventSink(Arc::new((Mutex::new(Inbox::default()), Condvar::new())));
        (
            Self {
                tx,
                sink: sink.clone(),
                join: None,
                boundary: "共享 CAN 驱动 send 返回；不证明总线 ACK 或设备采用",
            },
            rx,
            sink,
        )
    }

    pub(crate) fn attach_join(&mut self, join: JoinHandle<()>) {
        debug_assert!(self.join.is_none());
        self.join = Some(join);
    }

    pub fn with_boundary(mut self, boundary: &'static str) -> Self {
        self.boundary = boundary;
        self
    }
    pub fn spawn(
        name: &str,
        run: impl FnOnce(Receiver<Command>, EventSink) + Send + 'static,
    ) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel(8);
        let sink = EventSink(Arc::new((Mutex::new(Inbox::default()), Condvar::new())));
        let output = sink.clone();
        let join = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || run(rx, output))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            tx,
            sink,
            join: Some(join),
            boundary: "平台 I/O 本地提交",
        })
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Close);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
