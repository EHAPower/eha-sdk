// Copyright The eha-sdk Contributors
//! 多个 EHA 逻辑端点共享一条外部提供的 CAN 通道。

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use transport::can::{
    Frame, Mode, Receiver as CanReceiver, RxBuffers, RxReconnectState, SubmitEvent, SubmitResult,
    Transmitter, TxBuffers, TxReconnectState, TxToken,
};

use super::{
    CanChannel, CanChannelFactory, GENERATION, RECEIVE_TIMEOUT, lane_from_index, receive_frame,
    send_timeout,
};
use crate::host::backend::{Backend, Command, EventSink, Pump, SendRequest};

const MAX_NETWORK_CONTROL_PER_TURN: usize = 32;
const MAX_NETWORK_FRAMES_PER_TURN: usize = 4;
const MAX_NETWORK_RETIREMENTS_PER_TURN: usize = 128;
const MAX_NODE_EVENTS_PER_TURN: usize = 32;
const NODE_EVENT_QUEUE: usize = 64;
const NETWORK_IO_BUDGET: Duration = Duration::from_millis(2);

/// 共享通道的当前生命周期。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanNetworkStatus {
    Active,
    Quiescing { reason: String },
    Terminated { reason: String },
}

/// 多个 EHA 逻辑节点共享通道时唯一的 I/O 拥有者。
///
/// 通道错误会断开所有已附加节点，绝不自动重开或重放。全部节点发布续接保护状态后，
/// 调用方才可显式调用 `recover`。
pub struct CanNetwork {
    control: SyncSender<NetworkCommand>,
    status: Arc<Mutex<CanNetworkStatus>>,
    join: Option<JoinHandle<()>>,
}

/// 经由 CanNetwork 连接的固定 EHA 逻辑节点。
#[derive(Clone)]
pub struct CanNodeConnector {
    node: u8,
    control: SyncSender<NetworkCommand>,
}

impl CanNetwork {
    /// 打开一条外部已配置通道，并令本网络成为它唯一的 I/O 拥有者。
    pub fn new(factory: Arc<dyn CanChannelFactory>, mode: Mode) -> Result<Self, String> {
        let channel = factory
            .open()
            .map_err(|error| format!("无法打开外部 CAN 通道: {error}"))?;
        let (control, commands) = mpsc::sync_channel(32);
        let status = Arc::new(Mutex::new(CanNetworkStatus::Active));
        let worker_status = Arc::clone(&status);
        let join = std::thread::Builder::new()
            .name("eha-sdk-can-network".into())
            .spawn(move || run_network(channel, factory, mode, commands, worker_status))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            control,
            status,
            join: Some(join),
        })
    }

    #[must_use]
    pub fn status(&self) -> CanNetworkStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| CanNetworkStatus::Terminated {
                reason: "共享 CAN 网络状态锁中毒".into(),
            })
    }

    /// 网络终止后显式取得一条新的外部通道。
    pub fn recover(&self) -> Result<(), String> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.control
            .send(NetworkCommand::Recover { reply })
            .map_err(|_| "共享 CAN 网络已关闭".to_owned())?;
        receive
            .recv()
            .map_err(|_| "共享 CAN 网络在恢复完成前已关闭".to_owned())?
    }

    /// 取得某个节点的连接器；不会发送任何帧。
    pub fn node(&self, node: u8) -> Result<CanNodeConnector, String> {
        if node > 127 {
            return Err("EHA CAN 逻辑节点号必须在 0..=127".into());
        }
        Ok(CanNodeConnector {
            node,
            control: self.control.clone(),
        })
    }
}

impl Drop for CanNetwork {
    fn drop(&mut self) {
        let _ = self.control.send(NetworkCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl CanNodeConnector {
    /// 打开独立主机后端；实体通道仍由网络独占。
    pub fn open(&self) -> Result<Backend, String> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.control
            .send(NetworkCommand::Attach {
                node: self.node,
                reply,
            })
            .map_err(|_| "共享 CAN 网络已关闭".to_owned())?;
        receive
            .recv()
            .map_err(|_| "共享 CAN 网络在附加节点前已关闭".to_owned())?
    }
}

enum NetworkCommand {
    Attach {
        node: u8,
        reply: SyncSender<Result<Backend, String>>,
    },
    Recover {
        reply: SyncSender<Result<(), String>>,
    },
    Shutdown,
}

enum BusEvent {
    Frame(NodeFrame),
    Abort {
        node: u8,
        lease: u64,
        reason: String,
    },
}

struct NodeFrame {
    node: u8,
    lease: u64,
    lane: usize,
    pending: transport::can::PendingFrame,
    deadline: Instant,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

struct RetiredNode {
    node: u8,
    lease: u64,
    state: Option<NodeReconnectState>,
}

enum NodeEvent {
    Frame(Frame),
    Submitted {
        lane: usize,
        token: TxToken,
        result: SubmitResult,
        may_have_sent: bool,
        detail: Option<String>,
    },
    TransportError(String),
    Disconnect(String),
}

struct NodeHandle {
    lease: u64,
    events: SyncSender<NodeEvent>,
}

struct NodeReconnectState {
    tx: TxReconnectState,
    rx: RxReconnectState,
}

pub(crate) struct Routes {
    nodes: BTreeSet<u8>,
}

impl Routes {
    pub(crate) fn new() -> Self {
        Self {
            nodes: BTreeSet::new(),
        }
    }
    pub(crate) fn attach(&mut self, node: u8) -> Result<(), String> {
        if node > 127 {
            return Err("EHA CAN 逻辑节点号必须在 0..=127".into());
        }
        if !self.nodes.insert(node) {
            return Err("该 CAN node 已有活跃会话".into());
        }
        Ok(())
    }
    pub(crate) fn detach(&mut self, node: u8) {
        self.nodes.remove(&node);
    }
    pub(crate) fn recipient(&self, id: u32) -> Option<u8> {
        if id > 0x1fff_ffff {
            return None;
        }
        let node = ((id >> 22) & 0x7f) as u8;
        self.nodes.contains(&node).then_some(node)
    }
}

enum NetworkPhase {
    Active(Box<dyn CanChannel>),
    Quiescing { reason: String },
    Terminated { reason: String },
}

fn set_status(status: &Arc<Mutex<CanNetworkStatus>>, next: CanNetworkStatus) {
    if let Ok(mut status) = status.lock() {
        *status = next;
    }
}

fn run_network(
    channel: Box<dyn CanChannel>,
    factory: Arc<dyn CanChannelFactory>,
    mode: Mode,
    commands: Receiver<NetworkCommand>,
    status: Arc<Mutex<CanNetworkStatus>>,
) {
    let (events, bus_events) = mpsc::channel::<BusEvent>();
    let (retired, retired_nodes) = mpsc::channel::<RetiredNode>();
    let mut routes = Routes::new();
    let mut nodes = BTreeMap::<u8, NodeHandle>::new();
    let mut reconnect = BTreeMap::<u8, NodeReconnectState>::new();
    let epoch = Instant::now();
    let mut next_lease = 1_u64;
    let mut phase = NetworkPhase::Active(channel);
    loop {
        match &mut phase {
            NetworkPhase::Active(channel) => {
                retire_nodes(&retired_nodes, &mut nodes, &mut routes, &mut reconnect);
                let mut failure = None;
                let io_deadline = Instant::now() + NETWORK_IO_BUDGET;
                for _ in 0..MAX_NETWORK_FRAMES_PER_TURN {
                    if Instant::now() >= io_deadline {
                        break;
                    }
                    let Ok(event) = bus_events.try_recv() else {
                        break;
                    };
                    match event {
                        BusEvent::Abort {
                            node,
                            lease,
                            reason,
                        } => {
                            if nodes.get(&node).is_some_and(|handle| handle.lease == lease) {
                                failure = Some(reason);
                                break;
                            }
                        }
                        BusEvent::Frame(frame) => {
                            let Some(handle) = nodes
                                .get(&frame.node)
                                .filter(|handle| handle.lease == frame.lease)
                            else {
                                continue;
                            };
                            let Some(timeout) = send_timeout(frame.deadline, &frame.cancelled)
                            else {
                                let _ = handle.events.send(NodeEvent::Submitted {
                                    lane: frame.lane,
                                    token: frame.pending.token,
                                    result: SubmitResult::Failed,
                                    may_have_sent: false,
                                    detail: Some("CAN 帧交给驱动前已取消或到期".into()),
                                });
                                continue;
                            };
                            match channel.send(&frame.pending.frame, timeout) {
                                Ok(()) => {
                                    let _ = handle.events.send(NodeEvent::Submitted {
                                        lane: frame.lane,
                                        token: frame.pending.token,
                                        result: SubmitResult::Accepted,
                                        may_have_sent: false,
                                        detail: None,
                                    });
                                }
                                Err(error) => {
                                    let reason = format!("外部 CAN 驱动提交失败: {error}");
                                    let _ = handle.events.send(NodeEvent::Submitted {
                                        lane: frame.lane,
                                        token: frame.pending.token,
                                        result: SubmitResult::Failed,
                                        may_have_sent: true,
                                        detail: Some(reason.clone()),
                                    });
                                    failure = Some(reason);
                                    break;
                                }
                            }
                        }
                    }
                }
                if failure.is_none() {
                    for _ in 0..MAX_NETWORK_CONTROL_PER_TURN {
                        let Ok(command) = commands.try_recv() else {
                            break;
                        };
                        match command {
                            NetworkCommand::Shutdown => {
                                disconnect_all(&nodes, "共享 CAN 网络已关闭");
                                return;
                            }
                            NetworkCommand::Recover { reply } => {
                                let _ = reply.send(Err("共享 CAN 网络仍处于运行状态".into()));
                            }
                            NetworkCommand::Attach { node, reply } => attach_node(
                                node,
                                reply,
                                mode,
                                epoch,
                                &mut next_lease,
                                &mut routes,
                                &mut reconnect,
                                &mut nodes,
                                &events,
                                &retired,
                            ),
                        }
                    }
                }
                if failure.is_none() {
                    let receive_deadline = Instant::now() + NETWORK_IO_BUDGET;
                    for _ in 0..MAX_NETWORK_FRAMES_PER_TURN {
                        if Instant::now() >= receive_deadline {
                            break;
                        }
                        match channel.receive(RECEIVE_TIMEOUT) {
                            Ok(Some(frame)) => route_frame(frame, &routes, &nodes),
                            Ok(None) => break,
                            Err(error) => {
                                let reason = format!("外部 CAN 通道接收失败: {error}");
                                transport_error_all(&nodes, &reason);
                                failure = Some(reason);
                                break;
                            }
                        }
                    }
                }
                if let Some(reason) = failure {
                    disconnect_all(&nodes, &reason);
                    set_status(
                        &status,
                        CanNetworkStatus::Quiescing {
                            reason: reason.clone(),
                        },
                    );
                    phase = NetworkPhase::Quiescing { reason };
                }
            }
            NetworkPhase::Quiescing { reason } => {
                retire_nodes(&retired_nodes, &mut nodes, &mut routes, &mut reconnect);
                if nodes.is_empty() {
                    set_status(
                        &status,
                        CanNetworkStatus::Terminated {
                            reason: reason.clone(),
                        },
                    );
                    phase = NetworkPhase::Terminated {
                        reason: reason.clone(),
                    };
                    continue;
                }
                for _ in 0..MAX_NETWORK_CONTROL_PER_TURN {
                    let Ok(command) = commands.try_recv() else {
                        break;
                    };
                    if reject_while_quiescing(command) {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            NetworkPhase::Terminated { reason } => match commands.recv() {
                Ok(NetworkCommand::Shutdown) | Err(_) => return,
                Ok(NetworkCommand::Attach { reply, .. }) => {
                    let _ = reply.send(Err(format!(
                        "共享 CAN 网络已终止（{reason}）；请先显式恢复"
                    )));
                }
                Ok(NetworkCommand::Recover { reply }) => match factory.open() {
                    Ok(channel) => {
                        set_status(&status, CanNetworkStatus::Active);
                        phase = NetworkPhase::Active(channel);
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(format!("无法打开外部 CAN 通道: {error}")));
                    }
                },
            },
        }
    }
}

fn reject_while_quiescing(command: NetworkCommand) -> bool {
    match command {
        NetworkCommand::Attach { reply, .. } => {
            let _ = reply.send(Err(
                "共享 CAN 网络正在隔离旧节点传输状态；请稍后显式恢复".into()
            ));
            false
        }
        NetworkCommand::Recover { reply } => {
            let _ = reply.send(Err("共享 CAN 网络尚未保存全部节点传输状态".into()));
            false
        }
        NetworkCommand::Shutdown => true,
    }
}

#[allow(clippy::too_many_arguments)]
fn attach_node(
    node: u8,
    reply: SyncSender<Result<Backend, String>>,
    mode: Mode,
    epoch: Instant,
    next_lease: &mut u64,
    routes: &mut Routes,
    reconnect: &mut BTreeMap<u8, NodeReconnectState>,
    nodes: &mut BTreeMap<u8, NodeHandle>,
    events: &Sender<BusEvent>,
    retired: &Sender<RetiredNode>,
) {
    if let Err(error) = routes.attach(node) {
        let _ = reply.send(Err(error));
        return;
    }
    let lease = *next_lease;
    let Some(next) = next_lease.checked_add(1) else {
        routes.detach(node);
        let _ = reply.send(Err("CAN node lease 已耗尽，拒绝重用旧事件".into()));
        return;
    };
    *next_lease = next;
    match spawn_node(
        node,
        lease,
        mode,
        epoch,
        reconnect.remove(&node),
        events.clone(),
        retired.clone(),
    ) {
        Ok((backend, handle)) => {
            nodes.insert(node, handle);
            let _ = reply.send(Ok(backend));
        }
        Err(error) => {
            routes.detach(node);
            let _ = reply.send(Err(error));
        }
    }
}

fn retire_nodes(
    retired: &Receiver<RetiredNode>,
    nodes: &mut BTreeMap<u8, NodeHandle>,
    routes: &mut Routes,
    reconnect: &mut BTreeMap<u8, NodeReconnectState>,
) {
    for _ in 0..MAX_NETWORK_RETIREMENTS_PER_TURN {
        let Ok(retired) = retired.try_recv() else {
            break;
        };
        if nodes
            .get(&retired.node)
            .is_none_or(|handle| handle.lease != retired.lease)
        {
            continue;
        }
        nodes.remove(&retired.node);
        routes.detach(retired.node);
        if let Some(state) = retired.state {
            reconnect.insert(retired.node, state);
        }
    }
}

fn disconnect_all(nodes: &BTreeMap<u8, NodeHandle>, reason: &str) {
    for handle in nodes.values() {
        let _ = handle.events.send(NodeEvent::Disconnect(reason.into()));
    }
}

fn transport_error_all(nodes: &BTreeMap<u8, NodeHandle>, reason: &str) {
    for handle in nodes.values() {
        let _ = handle.events.send(NodeEvent::TransportError(reason.into()));
    }
}

fn route_frame(frame: Frame, routes: &Routes, nodes: &BTreeMap<u8, NodeHandle>) {
    let Some(node) = routes.recipient(frame.id) else {
        return;
    };
    if let Some(handle) = nodes.get(&node) {
        let _ = handle.events.send(NodeEvent::Frame(frame));
    }
}

fn spawn_node(
    node: u8,
    lease: u64,
    mode: Mode,
    epoch: Instant,
    state: Option<NodeReconnectState>,
    events: Sender<BusEvent>,
    retired: Sender<RetiredNode>,
) -> Result<(Backend, NodeHandle), String> {
    let (mut backend, commands, sink) = Backend::endpoint();
    let (node_events, receive) = mpsc::sync_channel(NODE_EVENT_QUEUE);
    let join = std::thread::Builder::new()
        .name(format!("eha-sdk-can-node-{node}"))
        .spawn(move || {
            run_node(NodeRuntime {
                node,
                lease,
                mode,
                epoch,
                state,
                commands,
                sink,
                receive,
                events,
                retired,
            });
        })
        .map_err(|error| error.to_string())?;
    backend.attach_join(join);
    Ok((
        backend,
        NodeHandle {
            lease,
            events: node_events,
        },
    ))
}

struct NodeRuntime {
    node: u8,
    lease: u64,
    mode: Mode,
    epoch: Instant,
    state: Option<NodeReconnectState>,
    commands: Receiver<Command>,
    sink: EventSink,
    receive: Receiver<NodeEvent>,
    events: Sender<BusEvent>,
    retired: Sender<RetiredNode>,
}

fn run_node(runtime: NodeRuntime) {
    let NodeRuntime {
        node,
        lease,
        mode,
        epoch,
        state,
        commands,
        sink,
        receive: events,
        events: bus,
        retired,
    } = runtime;
    let mut tx_heartbeat = [0_u8; 8];
    let mut tx_short = [0_u8; 256];
    let mut tx_large = [0_u8; protocol::MAX_MESSAGE_LEN];
    let tx_buffers = TxBuffers {
        heartbeat: &mut tx_heartbeat,
        short: &mut tx_short,
        large: &mut tx_large,
    };
    let mut rx_heartbeat = [0_u8; 8];
    let mut rx_short = [0_u8; 256];
    let mut rx_large = [0_u8; protocol::MAX_MESSAGE_LEN];
    let rx_buffers = RxBuffers {
        heartbeat: &mut rx_heartbeat,
        short: &mut rx_short,
        large: &mut rx_large,
    };
    let (mut transmitter, mut receiver) = match state {
        Some(state) => match (
            Transmitter::from_reconnect_state(state.tx, tx_buffers),
            CanReceiver::from_reconnect_state(state.rx, rx_buffers),
        ) {
            (Ok(tx), Ok(rx)) => (tx, rx),
            _ => {
                sink.disconnected("CAN 节点传输状态不能安全续接");
                let _ = retired.send(RetiredNode {
                    node,
                    lease,
                    state: None,
                });
                return;
            }
        },
        None => match (
            Transmitter::new(
                node,
                protocol::Direction::HostToFirmware,
                mode,
                GENERATION,
                tx_buffers,
            ),
            CanReceiver::new(
                node,
                protocol::Direction::FirmwareToHost,
                mode,
                GENERATION,
                rx_buffers,
            ),
        ) {
            (Ok(tx), Ok(rx)) => (tx, rx),
            _ => {
                sink.disconnected("CAN 节点传输状态初始化失败");
                let _ = retired.send(RetiredNode {
                    node,
                    lease,
                    state: None,
                });
                return;
            }
        },
    };
    let mut pump = Pump::new(commands, sink.clone()).with_epoch(epoch);
    let mut active: [Option<NodeActive>; 3] = [None, None, None];
    let mut awaiting = [false; 3];
    let mut aborting = false;
    let mut running = true;
    while running && pump.poll() {
        for _ in 0..MAX_NODE_EVENTS_PER_TURN {
            let Ok(event) = events.try_recv() else {
                break;
            };
            match event {
                NodeEvent::Frame(frame) => {
                    receive_frame(&mut receiver, &sink, &frame, pump.now_ms())
                }
                NodeEvent::TransportError(reason) => sink.transport_error(reason, Vec::new()),
                NodeEvent::Submitted {
                    lane,
                    token,
                    result,
                    may_have_sent,
                    detail,
                } => {
                    awaiting[lane] = false;
                    if aborting {
                        // 外部驱动结果未知时，请求已过期。共享通道正在隔离，不能将迟到的
                        // 本地确认变成一次成功的 EHA 提交。
                        continue;
                    }
                    let request = active[lane].as_mut();
                    match transmitter.complete(token, result, pump.now_ms()) {
                        Ok(SubmitEvent::MessageCommitted { .. }) => {
                            if let Some(request) = active[lane].take() {
                                sink.submitted(request.request.id);
                            }
                        }
                        Ok(SubmitEvent::FrameAccepted { .. }) => {
                            if let Some(request) = request {
                                request.may_have_sent = true;
                            }
                        }
                        Ok(SubmitEvent::Abandoned { .. }) | Err(_) => {
                            if let Some(request) = active[lane].take() {
                                sink.failed(
                                    request.request.id,
                                    detail.unwrap_or_else(|| "CAN 发送状态异常".into()),
                                    may_have_sent || request.may_have_sent,
                                );
                            }
                        }
                    }
                }
                NodeEvent::Disconnect(reason) => {
                    sink.disconnected(reason);
                    running = false;
                    break;
                }
            }
        }
        let expired = receiver.poll(pump.now_ms());
        if expired != 0 {
            sink.transport_error(
                format!("CAN 完整消息组装期限到达，废弃 {expired} 个组"),
                Vec::new(),
            );
        }
        if aborting {
            std::thread::yield_now();
            continue;
        }
        for (lane, awaiting_lane) in awaiting.iter_mut().enumerate() {
            start_node_request(lane, &mut pump, &mut transmitter, &mut active);
            if *awaiting_lane {
                let expired_while_waiting = active[lane]
                    .as_ref()
                    .is_some_and(|request| request.request.expired());
                if expired_while_waiting {
                    let request = active[lane]
                        .as_mut()
                        .expect("awaiting lane owns an active request");
                    request.may_have_sent = true;
                    let _ = bus.send(BusEvent::Abort {
                        node,
                        lease,
                        reason: "等待外部 CAN 驱动结果时请求已取消或到期；已隔离共享通道".into(),
                    });
                    aborting = true;
                    break;
                }
                continue;
            }
            if let Some(pending) = transmitter.next_frame(lane_from_index(lane)) {
                *awaiting_lane = true;
                if bus
                    .send(BusEvent::Frame(NodeFrame {
                        node,
                        lease,
                        lane,
                        pending,
                        deadline: active[lane]
                            .as_ref()
                            .expect("active request owns pending frame")
                            .request
                            .deadline,
                        cancelled: Arc::clone(
                            &active[lane]
                                .as_ref()
                                .expect("active request owns pending frame")
                                .request
                                .cancelled,
                        ),
                    }))
                    .is_err()
                {
                    sink.disconnected("共享 CAN 网络已关闭");
                    running = false;
                    break;
                }
            }
        }
        std::thread::yield_now();
    }
    for request in active.into_iter().flatten() {
        sink.failed(
            request.request.id,
            "CAN 节点关闭前未取得完整本地提交结果",
            request.may_have_sent,
        );
    }
    let now = pump.now_ms();
    let state = match (
        transmitter.into_reconnect_state(now),
        receiver.into_reconnect_state(now),
    ) {
        (Ok(tx), Ok(rx)) => Some(NodeReconnectState { tx, rx }),
        _ => None,
    };
    let _ = retired.send(RetiredNode { node, lease, state });
}

struct NodeActive {
    request: SendRequest,
    may_have_sent: bool,
}

fn start_node_request(
    lane: usize,
    pump: &mut Pump,
    transmitter: &mut Transmitter<'_>,
    active: &mut [Option<NodeActive>; 3],
) {
    if active[lane].is_some() {
        return;
    }
    let Some(request) = pump.take(lane) else {
        return;
    };
    match transmitter.copy_and_start(&request.bytes, pump.now_ms()) {
        Ok(_) => {
            active[lane] = Some(NodeActive {
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use super::*;

    struct FakeFactory(Mutex<VecDeque<Box<dyn CanChannel>>>);

    impl CanChannelFactory for FakeFactory {
        fn open(&self) -> Result<Box<dyn CanChannel>, String> {
            self.0
                .lock()
                .map_err(|_| "fake channel lock poisoned".to_owned())?
                .pop_front()
                .ok_or_else(|| "no fake channel".into())
        }
    }

    struct FakeChannel {
        receives_fail: Arc<AtomicBool>,
        sends_fail: Arc<AtomicBool>,
        sends: Arc<AtomicUsize>,
    }

    impl CanChannel for FakeChannel {
        fn send(&mut self, _: &Frame, _: Duration) -> Result<(), String> {
            self.sends.fetch_add(1, Ordering::Relaxed);
            if self.sends_fail.load(Ordering::Acquire) {
                Err("fake send fault".into())
            } else {
                Ok(())
            }
        }

        fn receive(&mut self, _: Duration) -> Result<Option<Frame>, String> {
            if self.receives_fail.load(Ordering::Acquire) {
                Err("fake receive fault".into())
            } else {
                Ok(None)
            }
        }
    }

    fn wait_for_status(network: &CanNetwork, expected: impl Fn(&CanNetworkStatus) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !expected(&network.status()) {
            assert!(
                Instant::now() < deadline,
                "network did not reach expected status"
            );
            std::thread::yield_now();
        }
    }

    fn position_request(id: u64) -> Command {
        let message = protocol::Message::Position(1.0);
        let mut bytes = vec![0; message.encoded_len().expect("position length")];
        protocol::encode(message, &mut bytes).expect("position encodes");
        Command::Send(SendRequest {
            id,
            bytes,
            deadline: Instant::now() + Duration::from_secs(1),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    #[test]
    fn receive_failure_is_reported_to_all_nodes_and_requires_explicit_recover() {
        let receive_fault = Arc::new(AtomicBool::new(false));
        let send_fault = Arc::new(AtomicBool::new(false));
        let factory = Arc::new(FakeFactory(Mutex::new(VecDeque::from([
            Box::new(FakeChannel {
                receives_fail: Arc::clone(&receive_fault),
                sends_fail: Arc::clone(&send_fault),
                sends: Arc::new(AtomicUsize::new(0)),
            }) as Box<dyn CanChannel>,
            Box::new(FakeChannel {
                receives_fail: Arc::new(AtomicBool::new(false)),
                sends_fail: Arc::new(AtomicBool::new(false)),
                sends: Arc::new(AtomicUsize::new(0)),
            }) as Box<dyn CanChannel>,
        ]))));
        let network = CanNetwork::new(factory, Mode::Fd).expect("network opens");
        let first = network
            .node(1)
            .expect("valid node")
            .open()
            .expect("first opens");
        let second = network
            .node(2)
            .expect("valid node")
            .open()
            .expect("second opens");
        receive_fault.store(true, Ordering::Release);
        wait_for_status(&network, |status| {
            matches!(status, CanNetworkStatus::Terminated { .. })
        });
        for backend in [&first, &second] {
            let (lock, _) = &*backend.sink.0;
            let inbox = lock.lock().expect("event inbox available");
            assert_eq!(inbox.transport.errors, 1);
            assert!(inbox.disconnected.is_some());
        }
        assert!(network.node(3).expect("valid node").open().is_err());
        network
            .recover()
            .expect("explicit recovery opens the next channel");
        assert_eq!(network.status(), CanNetworkStatus::Active);
    }

    #[test]
    fn send_failure_is_never_replayed_after_explicit_recover() {
        let first_sends = Arc::new(AtomicUsize::new(0));
        let second_sends = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(FakeFactory(Mutex::new(VecDeque::from([
            Box::new(FakeChannel {
                receives_fail: Arc::new(AtomicBool::new(false)),
                sends_fail: Arc::new(AtomicBool::new(true)),
                sends: Arc::clone(&first_sends),
            }) as Box<dyn CanChannel>,
            Box::new(FakeChannel {
                receives_fail: Arc::new(AtomicBool::new(false)),
                sends_fail: Arc::new(AtomicBool::new(false)),
                sends: Arc::clone(&second_sends),
            }) as Box<dyn CanChannel>,
        ]))));
        let network = CanNetwork::new(factory, Mode::Fd).expect("network opens");
        let first = network
            .node(1)
            .expect("valid node")
            .open()
            .expect("node opens");
        first
            .tx
            .send(position_request(7))
            .expect("request reaches node");
        wait_for_status(&network, |status| {
            matches!(status, CanNetworkStatus::Terminated { .. })
        });
        assert_eq!(first_sends.load(Ordering::Relaxed), 1);
        network
            .recover()
            .expect("explicit recovery opens next channel");
        let recovered = network
            .node(1)
            .expect("valid node")
            .open()
            .expect("node explicitly reopens");
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            second_sends.load(Ordering::Relaxed),
            0,
            "old request is not replayed"
        );
        drop(recovered);
    }
}
