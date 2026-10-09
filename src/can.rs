// Copyright The eha-sdk Contributors
//! 通用 EHA CAN 协议端点。
//!
//! 此模块负责 EHA 帧、消息重组、提交语义和续接保护窗口。驱动选择、实体设备配置和
//! 硬件生命周期由外部 CAN 通道提供方负责。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use transport::can::{
    Frame, Lane, Mode, ReceiveResult, Receiver, RxBuffers, RxReconnectState, SubmitEvent,
    SubmitResult, Transmitter, TxBuffers, TxReconnectState,
};

use crate::host::backend::{Backend, Event, EventSink, Pump, SendRequest};

mod network;
pub mod python;
pub use network::{CanNetwork, CanNetworkStatus, CanNodeConnector};

pub(crate) const GENERATION: u32 = 1;
pub(crate) const RECEIVE_TIMEOUT: Duration = Duration::from_millis(1);
pub(crate) const MAX_SEND_TIMEOUT: Duration = Duration::from_millis(10);
const MAX_RX_FRAMES_PER_TURN: usize = 32;
const RECEIVE_BUDGET: Duration = Duration::from_millis(2);

/// 已由外部系统或驱动配置好的本机 CAN 通道。
///
/// timeout 是单次驱动调用的最大等待时间。send 成功只表示本机驱动接受了帧，
/// 不证明 CAN 总线 ACK、目标设备接收或 EHA 应用效果。send 报错可能发生在驱动
/// 部分提交之后，因此调用方将其视作可能已发送。receive 在超时前没有帧时
/// 返回 Ok(None)。
pub trait CanChannel: Send {
    /// 向外部驱动提交一条完整 CAN 帧。
    fn send(&mut self, frame: &Frame, timeout: Duration) -> Result<(), String>;
    /// 从外部驱动读取一条完整 CAN 帧。
    fn receive(&mut self, timeout: Duration) -> Result<Option<Frame>, String>;
}

/// 打开已配置的通道，而不向 SDK 暴露具体 CAN 适配器。
///
/// 工厂可以创建和关闭通道句柄；设备发现、位速率、固件和硬件恢复属于外部系统或驱动。
pub trait CanChannelFactory: Send + Sync {
    /// 打开一个新的通道句柄。
    fn open(&self) -> Result<Box<dyn CanChannel>, String>;
}

type ReconnectSlot = Arc<Mutex<Option<CanReconnectState>>>;
type TerminalSlot = Arc<Mutex<Option<String>>>;

struct CanReconnectState {
    tx: TxReconnectState,
    rx: RxReconnectState,
    generation: u32,
}

struct CanWorker {
    node: u8,
    mode: Mode,
    reconnect: ReconnectSlot,
    terminal: TerminalSlot,
    epoch: Instant,
    active: Arc<AtomicBool>,
}

/// 与实体 CAN 适配器无关的 EHA 端点参数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanOptions {
    pub node: u8,
    pub mode: Mode,
}

/// 单节点连接器，在调用方显式重开时保留协议保护窗口。
pub struct CanConnector {
    options: CanOptions,
    factory: Arc<dyn CanChannelFactory>,
    reconnect: ReconnectSlot,
    terminal: TerminalSlot,
    active: Arc<AtomicBool>,
    epoch: Instant,
}

struct ActiveLease(Arc<AtomicBool>);
impl Drop for ActiveLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn mark_terminal(terminal: &TerminalSlot, reason: impl Into<String>) {
    if let Ok(mut state) = terminal.lock() {
        *state = Some(reason.into());
    }
}

/// 通过外部通道工厂打开一次性的 EHA CAN 后端。
pub fn open(options: CanOptions, factory: Arc<dyn CanChannelFactory>) -> Result<Backend, String> {
    CanConnector::new(options, factory)?.open()
}

impl CanConnector {
    /// 创建连接器；此时不会打开本机 CAN 通道。
    pub fn new(options: CanOptions, factory: Arc<dyn CanChannelFactory>) -> Result<Self, String> {
        if options.node > 127 {
            return Err("EHA CAN 逻辑节点号必须在 0..=127".into());
        }
        Ok(Self {
            options,
            factory,
            reconnect: Arc::new(Mutex::new(None)),
            terminal: Arc::new(Mutex::new(None)),
            active: Arc::new(AtomicBool::new(false)),
            epoch: Instant::now(),
        })
    }

    /// 打开连接器，或在旧后端已结束后显式续接。
    pub fn open(&mut self) -> Result<Backend, String> {
        match self.terminal.lock() {
            Ok(state) if state.is_some() => {
                return Err(format!(
                    "CAN 连接器不能安全续接: {}",
                    state.as_deref().unwrap_or_default()
                ));
            }
            Ok(_) => {}
            Err(_) => return Err("CAN 连接器终态锁已损坏，不能安全续接".into()),
        }
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "CAN 连接器已有活跃的本地 I/O owner".to_owned())?;
        let channel = match self.factory.open() {
            Ok(channel) => channel,
            Err(error) => {
                self.active.store(false, Ordering::Release);
                return Err(format!("无法打开外部 CAN 通道: {error}"));
            }
        };
        let reconnect = Arc::clone(&self.reconnect);
        let terminal = Arc::clone(&self.terminal);
        let panic_terminal = Arc::clone(&terminal);
        let active = Arc::clone(&self.active);
        let (node, mode, epoch) = (self.options.node, self.options.mode, self.epoch);
        match Backend::spawn("eha-sdk-can", move |commands, sink| {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    channel,
                    commands,
                    sink,
                    CanWorker {
                        node,
                        mode,
                        reconnect,
                        terminal,
                        epoch,
                        active,
                    },
                );
            }))
            .is_err()
            {
                mark_terminal(
                    &panic_terminal,
                    "CAN I/O 线程 panic，旧 I/O 状态不能安全续接",
                );
            }
        }) {
            Ok(backend) => {
                Ok(backend.with_boundary("外部 CAN 驱动提交；不证明总线 ACK 或设备接收"))
            }
            Err(error) => {
                self.active.store(false, Ordering::Release);
                Err(error)
            }
        }
    }
}

fn run(
    mut channel: Box<dyn CanChannel>,
    commands: std::sync::mpsc::Receiver<crate::host::backend::Command>,
    sink: EventSink,
    worker: CanWorker,
) {
    let _active = ActiveLease(worker.active);
    let reconnect_state = match worker.reconnect.lock() {
        Ok(mut state) => state.take(),
        Err(_) => {
            mark_terminal(&worker.terminal, "CAN 续接状态锁已损坏");
            sink.event(Event::Disconnected(
                "CAN 续接状态锁已损坏，不能安全恢复".into(),
            ));
            return;
        }
    };
    let had_reconnect_state = reconnect_state.is_some();
    let (tx_state, rx_state, generation) = match reconnect_state {
        Some(state) => (Some(state.tx), Some(state.rx), state.generation),
        None => (None, None, GENERATION),
    };
    let mut tx_heartbeat = [0_u8; 8];
    let mut tx_short = [0_u8; 256];
    let mut tx_large = [0_u8; protocol::MAX_MESSAGE_LEN];
    let tx_buffers = TxBuffers {
        heartbeat: &mut tx_heartbeat,
        short: &mut tx_short,
        large: &mut tx_large,
    };
    let transmitter = match tx_state {
        Some(state) => Transmitter::from_reconnect_state(state, tx_buffers),
        None => Transmitter::new(
            worker.node,
            protocol::Direction::HostToFirmware,
            worker.mode,
            GENERATION,
            tx_buffers,
        ),
    };
    let mut rx_heartbeat = [0_u8; 8];
    let mut rx_short = [0_u8; 256];
    let mut rx_large = [0_u8; protocol::MAX_MESSAGE_LEN];
    let rx_buffers = RxBuffers {
        heartbeat: &mut rx_heartbeat,
        short: &mut rx_short,
        large: &mut rx_large,
    };
    let receiver = match rx_state {
        Some(state) => Receiver::from_reconnect_state(state, rx_buffers),
        None => Receiver::new(
            worker.node,
            protocol::Direction::FirmwareToHost,
            worker.mode,
            GENERATION,
            rx_buffers,
        ),
    };
    let (Ok(mut transmitter), Ok(mut receiver)) = (transmitter, receiver) else {
        if had_reconnect_state {
            mark_terminal(&worker.terminal, "CAN 传输状态不能安全续接");
        }
        sink.disconnected("CAN 发送或接收器初始化失败");
        return;
    };
    let mut pump = Pump::new(commands, sink.clone()).with_epoch(worker.epoch);
    let mut active: [Option<ActiveRequest>; 3] = [None, None, None];
    'service: loop {
        if !pump.poll() {
            break;
        }
        let receive_deadline = Instant::now() + RECEIVE_BUDGET;
        for _ in 0..MAX_RX_FRAMES_PER_TURN {
            if Instant::now() >= receive_deadline {
                break;
            }
            match channel.receive(RECEIVE_TIMEOUT) {
                Ok(Some(frame)) => receive_frame(&mut receiver, &pump.sink, &frame, pump.now_ms()),
                Ok(None) => break,
                Err(error) => {
                    pump.sink
                        .transport_error(format!("外部 CAN 通道接收失败: {error}"), Vec::new());
                    pump.disconnected("外部 CAN 通道接收失败");
                    break 'service;
                }
            }
        }
        let expired = receiver.poll(pump.now_ms());
        if expired != 0 {
            pump.sink.transport_error(
                format!("CAN 完整消息组装期限到达，废弃 {expired} 个组"),
                Vec::new(),
            );
        }
        for lane in 0..3 {
            if !pump.poll() {
                break;
            }
            start_request(lane, &mut pump, &mut transmitter, &mut active);
            if active[lane]
                .as_ref()
                .is_some_and(|request| request.request.expired())
            {
                transmitter.abandon(lane_from_index(lane), pump.now_ms());
                let request = active[lane].take().expect("request was checked");
                pump.sink.failed(
                    request.request.id,
                    "CAN 分片发送期间已取消或到期",
                    request.may_have_sent,
                );
                continue;
            }
            if let Some(pending) = transmitter.next_frame(lane_from_index(lane)) {
                let Some(request_id) = active[lane].as_ref().map(|request| request.request.id)
                else {
                    pump.disconnected("CAN 发送状态丢失");
                    break 'service;
                };
                let Some(timeout) = active[lane].as_ref().and_then(|request| {
                    send_timeout(request.request.deadline, &request.request.cancelled)
                }) else {
                    let _ =
                        transmitter.complete(pending.token, SubmitResult::Failed, pump.now_ms());
                    let request = active[lane]
                        .take()
                        .expect("active request owns pending frame");
                    pump.sink
                        .failed(request.request.id, "CAN 帧交给驱动前已取消或到期", false);
                    continue;
                };
                match channel.send(&pending.frame, timeout) {
                    Ok(()) => match transmitter.complete(
                        pending.token,
                        SubmitResult::Accepted,
                        pump.now_ms(),
                    ) {
                        Ok(SubmitEvent::MessageCommitted { .. }) => {
                            pump.sink.submitted(request_id);
                            active[lane] = None;
                        }
                        Ok(SubmitEvent::FrameAccepted { .. }) => {
                            if let Some(request) = active[lane].as_mut() {
                                request.may_have_sent = true;
                            }
                        }
                        Ok(SubmitEvent::Abandoned { .. }) | Err(_) => {
                            pump.sink.failed(request_id, "CAN 发送状态异常", true);
                            active[lane] = None;
                        }
                    },
                    Err(error) => {
                        let _ = transmitter.complete(
                            pending.token,
                            SubmitResult::Failed,
                            pump.now_ms(),
                        );
                        pump.sink.failed(
                            request_id,
                            format!("外部 CAN 驱动提交失败: {error}"),
                            true,
                        );
                        active[lane] = None;
                        pump.disconnected("外部 CAN 驱动提交失败后连接不可继续使用");
                        break 'service;
                    }
                }
            }
        }
        std::thread::yield_now();
    }
    for request in active.into_iter().flatten() {
        pump.sink.failed(
            request.request.id,
            "CAN 后端关闭前未取得完整本地提交结果",
            request.may_have_sent,
        );
    }
    drop(channel);
    let now_ms = pump.now_ms();
    match (
        transmitter.into_reconnect_state(now_ms),
        receiver.into_reconnect_state(now_ms),
        generation.checked_add(1),
    ) {
        (Ok(tx), Ok(rx), Some(generation)) => match worker.reconnect.lock() {
            Ok(mut state) => *state = Some(CanReconnectState { tx, rx, generation }),
            Err(_) => {
                mark_terminal(&worker.terminal, "CAN 续接状态锁已损坏");
                pump.sink
                    .transport_error("CAN 续接状态锁已损坏，已放弃本地传输状态", Vec::new());
            }
        },
        _ => {
            mark_terminal(
                &worker.terminal,
                "CAN 传输状态导出失败，不能继续复用编号或保护窗口",
            );
            pump.sink
                .transport_error("CAN 不能导出续接状态", Vec::new());
        }
    }
}

struct ActiveRequest {
    request: SendRequest,
    may_have_sent: bool,
}

fn start_request(
    index: usize,
    pump: &mut Pump,
    transmitter: &mut Transmitter<'_>,
    active: &mut [Option<ActiveRequest>; 3],
) {
    if active[index].is_some() {
        return;
    }
    let Some(request) = pump.take(index) else {
        return;
    };
    match transmitter.copy_and_start(&request.bytes, pump.now_ms()) {
        Ok(_) => {
            active[index] = Some(ActiveRequest {
                request,
                may_have_sent: false,
            })
        }
        Err(transport::can::Error::TransferProtected) => pump.put_back(request),
        Err(error) => pump.sink.failed(
            request.id,
            format!("CAN 完整消息不能开始分片: {error:?}"),
            false,
        ),
    }
}

pub(crate) fn receive_frame(
    receiver: &mut Receiver<'_>,
    sink: &EventSink,
    frame: &Frame,
    now_ms: u64,
) {
    match receiver.receive(frame, now_ms, receiver.generation()) {
        ReceiveResult::Complete { lane, .. } => {
            let Some(message) = receiver.take_completed(lane) else {
                sink.transport_error("CAN 完整消息缓冲丢失", Vec::new());
                return;
            };
            let bytes = message.as_bytes().to_vec();
            match receiver.replace_buffer(lane, message.into_buffer()) {
                Ok(()) => sink.received(bytes),
                Err(error) => {
                    sink.transport_error(format!("CAN 完整消息缓冲无法归还: {error:?}"), Vec::new())
                }
            }
        }
        ReceiveResult::Rejected { reason } => {
            sink.transport_error(format!("CAN 帧不满足绑定: {reason:?}"), Vec::new())
        }
        ReceiveResult::Ignored | ReceiveResult::Incomplete { .. } => {}
    }
}

pub(crate) fn lane_from_index(index: usize) -> Lane {
    match index {
        0 => Lane::Heartbeat,
        1 => Lane::Short,
        _ => Lane::Large,
    }
}

/// 返回外部驱动对单次本地提交最多可阻塞的时间。
///
/// 调用完成只证明本地驱动已接受。调用方必须在进入驱动前拒绝已取消或过期的请求，
/// 并且不得重试失败调用。
pub(crate) fn send_timeout(
    deadline: Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Option<Duration> {
    if cancelled.load(Ordering::Acquire) {
        return None;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    (!remaining.is_zero()).then_some(remaining.min(MAX_SEND_TIMEOUT))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Instant,
    };

    use super::*;

    struct FakeFactory(Arc<Mutex<VecDeque<Box<dyn CanChannel>>>>);

    impl CanChannelFactory for FakeFactory {
        fn open(&self) -> Result<Box<dyn CanChannel>, String> {
            self.0
                .lock()
                .map_err(|_| "fake channel lock poisoned".to_owned())?
                .pop_front()
                .ok_or_else(|| "no fake channel".into())
        }
    }

    struct FakeChannel;

    impl CanChannel for FakeChannel {
        fn send(&mut self, _: &Frame, _: Duration) -> Result<(), String> {
            Ok(())
        }

        fn receive(&mut self, _: Duration) -> Result<Option<Frame>, String> {
            Ok(None)
        }
    }

    struct FaultChannel {
        sends: Arc<AtomicUsize>,
        send_error: Option<String>,
        receive_error: Option<String>,
    }

    impl CanChannel for FaultChannel {
        fn send(&mut self, _: &Frame, _: Duration) -> Result<(), String> {
            self.sends.fetch_add(1, Ordering::Relaxed);
            match self.send_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn receive(&mut self, _: Duration) -> Result<Option<Frame>, String> {
            match self.receive_error.take() {
                Some(error) => Err(error),
                None => Ok(None),
            }
        }
    }

    struct TimeoutChannel(Arc<Mutex<Vec<Duration>>>);

    impl CanChannel for TimeoutChannel {
        fn send(&mut self, _: &Frame, timeout: Duration) -> Result<(), String> {
            self.0
                .lock()
                .expect("timeouts lock available")
                .push(timeout);
            Ok(())
        }

        fn receive(&mut self, _: Duration) -> Result<Option<Frame>, String> {
            Ok(None)
        }
    }

    fn request(id: u64) -> crate::host::backend::Command {
        request_with_deadline(id, Instant::now() + Duration::from_secs(1))
    }

    fn request_with_deadline(id: u64, deadline: Instant) -> crate::host::backend::Command {
        let message = protocol::Message::Position(1.0);
        let mut bytes = vec![0; message.encoded_len().expect("position length")];
        protocol::encode(message, &mut bytes).expect("position encodes");
        crate::host::backend::Command::Send(SendRequest {
            id,
            bytes,
            deadline,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    fn wait_for(backend: &Backend, matches: impl Fn(&crate::host::backend::Inbox) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let (lock, condition) = &*backend.sink.0;
            let inbox = lock.lock().expect("event inbox available");
            if matches(&inbox) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "expected backend observation");
            drop(
                condition
                    .wait_timeout(inbox, remaining.min(Duration::from_millis(10)))
                    .expect("event wait available")
                    .0,
            );
        }
    }

    #[test]
    fn connector_rejects_invalid_node_before_opening_a_channel() {
        let factory = Arc::new(FakeFactory(Arc::new(Mutex::new(VecDeque::new()))));
        assert!(
            CanConnector::new(
                CanOptions {
                    node: 128,
                    mode: Mode::Fd,
                },
                factory,
            )
            .is_err()
        );
    }

    #[test]
    fn connector_opens_the_external_channel_only_when_requested() {
        let factory = Arc::new(FakeFactory(Arc::new(Mutex::new(VecDeque::from([
            Box::new(FakeChannel) as Box<dyn CanChannel>,
        ])))));
        let mut connector = CanConnector::new(
            CanOptions {
                node: 1,
                mode: Mode::Fd,
            },
            Arc::clone(&factory) as Arc<dyn CanChannelFactory>,
        )
        .expect("valid connector");
        let backend = connector.open().expect("external channel opens");
        drop(backend);
    }

    #[test]
    fn send_failure_is_reported_as_may_have_sent_and_is_not_replayed_after_reopen() {
        let first_sends = Arc::new(AtomicUsize::new(0));
        let second_sends = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(FakeFactory(Arc::new(Mutex::new(VecDeque::from([
            Box::new(FaultChannel {
                sends: Arc::clone(&first_sends),
                send_error: Some("driver write fault".into()),
                receive_error: None,
            }) as Box<dyn CanChannel>,
            Box::new(FaultChannel {
                sends: Arc::clone(&second_sends),
                send_error: None,
                receive_error: None,
            }) as Box<dyn CanChannel>,
        ])))));
        let mut connector = CanConnector::new(
            CanOptions {
                node: 1,
                mode: Mode::Fd,
            },
            Arc::clone(&factory) as Arc<dyn CanChannelFactory>,
        )
        .expect("valid connector");
        let first = connector.open().expect("first channel opens");
        first.tx.send(request(7)).expect("request reaches backend");
        wait_for(&first, |inbox| {
            inbox.events.iter().any(|event| {
                matches!(
                    event,
                    Event::Failed(7, failure) if failure.may_have_sent
                )
            })
        });
        drop(first);
        assert_eq!(first_sends.load(Ordering::Relaxed), 1);

        let reopened = connector
            .open()
            .expect("explicit reopen opens a fresh channel");
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            second_sends.load(Ordering::Relaxed),
            0,
            "the failed request is never replayed"
        );
        drop(reopened);
    }

    #[test]
    fn receive_failure_surfaces_a_diagnostic_before_disconnect() {
        let factory = Arc::new(FakeFactory(Arc::new(Mutex::new(VecDeque::from([
            Box::new(FaultChannel {
                sends: Arc::new(AtomicUsize::new(0)),
                send_error: None,
                receive_error: Some("driver receive fault".into()),
            }) as Box<dyn CanChannel>,
        ])))));
        let mut connector = CanConnector::new(
            CanOptions {
                node: 1,
                mode: Mode::Fd,
            },
            factory,
        )
        .expect("valid connector");
        let backend = connector.open().expect("channel opens");
        wait_for(&backend, |inbox| {
            inbox.transport.errors == 1 && inbox.disconnected.is_some()
        });
        let (lock, _) = &*backend.sink.0;
        let inbox = lock.lock().expect("event inbox available");
        assert!(
            inbox
                .transport
                .last_error
                .as_ref()
                .is_some_and(|(detail, _, _)| detail.contains("driver receive fault"))
        );
    }

    #[test]
    fn driver_submission_uses_request_deadline_not_receive_poll_timeout() {
        let timeouts = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(FakeFactory(Arc::new(Mutex::new(VecDeque::from([
            Box::new(TimeoutChannel(Arc::clone(&timeouts))) as Box<dyn CanChannel>,
        ])))));
        let mut connector = CanConnector::new(
            CanOptions {
                node: 1,
                mode: Mode::Fd,
            },
            factory,
        )
        .expect("valid connector");
        let backend = connector.open().expect("channel opens");
        backend
            .tx
            .send(request_with_deadline(
                9,
                Instant::now() + Duration::from_secs(1),
            ))
            .expect("request reaches worker");
        wait_for(&backend, |inbox| {
            inbox
                .events
                .iter()
                .any(|event| matches!(event, Event::Submitted(9)))
        });
        let timeout = *timeouts
            .lock()
            .expect("timeouts lock available")
            .first()
            .expect("driver send called");
        assert_eq!(timeout, MAX_SEND_TIMEOUT);
    }

    #[test]
    fn send_timeout_caps_long_deadlines_and_rejects_cancelled_requests() {
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            send_timeout(Instant::now() + Duration::from_secs(1), &cancelled),
            Some(MAX_SEND_TIMEOUT)
        );
        let short = send_timeout(Instant::now() + Duration::from_millis(8), &cancelled)
            .expect("short request is still valid");
        assert!(short <= Duration::from_millis(8));
        cancelled.store(true, Ordering::Release);
        assert_eq!(
            send_timeout(Instant::now() + Duration::from_secs(1), &cancelled),
            None
        );
    }
}
