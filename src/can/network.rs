// Copyright The eha-sdk Contributors

//! 一个 CANable2 串口上的多节点逻辑 endpoint。
//!
//! 物理串口、SLCAN 配置与 line parser 只由一个 Bus worker 持有。每个节点另有自己的
//! 传输编号、分片组装、心跳 pump 与 host backend，因此节点之间不会共用业务会话。

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use config::ExternalCanProfile;
use serialport::SerialPort;
use transport::can::{
    Frame, Mode, ReceiveResult, Receiver as CanReceiver, RxBuffers, RxReconnectState, SubmitEvent,
    SubmitResult, Transmitter, TxBuffers, TxReconnectState, TxToken,
};

use super::{
    AdapterProbe, MAX_RX_CHUNKS_PER_TURN, MAX_SLCAN_LINE, SERIAL_TIMEOUT, configure, decode_frame,
    encode_frame, lane_from_index, profile_mode,
};
use crate::host::backend::{Backend, Command, EventSink, Pump, SendRequest};

const MAX_NETWORK_CONTROL_PER_TURN: usize = 32;
const MAX_NETWORK_FRAMES_PER_TURN: usize = 32;
const MAX_NETWORK_RETIREMENTS_PER_TURN: usize = 128;
const MAX_NODE_EVENTS_PER_TURN: usize = 32;
const NODE_EVENT_QUEUE: usize = 64;

/// 共享 CANable2 SLCAN 串口的当前状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanNetworkStatus {
    /// 串口已配置，节点可以附加。
    Active,
    /// 已发现串口故障，正在等待所有旧 node 保存 CAN 传输保护状态。
    Quiescing { reason: String },
    /// 所有旧 node 都已隔离并保存状态；调用方可显式 [`CanNetwork::recover`]。
    Terminated { reason: String },
}

/// 共享 CANable2 SLCAN 串口的唯一所有者。
///
/// 调用方必须在全部 [`CanNodeConnector`] 和其 [`Backend`] 存活期间保留此对象。串口读写
/// 发生故障时，网络会终止全部节点，且不会自动重开串口或重放任何业务消息、心跳；调用方
/// 必须在确认 [`Self::status`] 为 [`CanNetworkStatus::Terminated`] 后，显式调用
/// [`Self::recover`]。恢复只重新打开同一 port/profile 并保留 CAN 编号/保护窗口；它不附加
/// 节点，也不恢复业务消息、心跳或 [`crate::host::Client`] 身份。
pub struct CanNetwork {
    control: SyncSender<NetworkCommand>,
    status: Arc<Mutex<CanNetworkStatus>>,
    join: Option<JoinHandle<()>>,
}

/// 一个固定 CAN node 的可重开逻辑 endpoint 工厂。
///
/// 同一时刻一个 node 只能打开一个 [`Backend`]。关闭旧 backend 后可在同一
/// [`CanNetwork`] 中再次 [`Self::open`]；续接只保留 CAN 编号和保护窗口，绝不保存或
/// 重放待发送业务、心跳或 [`crate::host::Client`] 的业务身份。
#[derive(Clone)]
pub struct CanNodeConnector {
    node: u8,
    control: SyncSender<NetworkCommand>,
}

impl CanNetwork {
    /// 打开一个明确 profile 的 CANable2 串口，并取得唯一 I/O owner。
    pub fn new(port: String, profile: ExternalCanProfile) -> Result<Self, String> {
        let (serial, mode, probe) = open_serial(&port, profile)?;
        let (control, commands) = mpsc::sync_channel(32);
        let status = Arc::new(Mutex::new(CanNetworkStatus::Active));
        let worker_status = Arc::clone(&status);
        let join = std::thread::Builder::new()
            .name("eha-sdk-can-network".into())
            .spawn(move || run_network(serial, mode, probe, port, profile, commands, worker_status))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            control,
            status,
            join: Some(join),
        })
    }

    /// 返回当前网络状态；[`CanNetworkStatus::Terminated`] 后只能由用户明确调用
    /// [`Self::recover`]，不会因 [`CanNodeConnector::open`] 自动重开。
    #[must_use]
    pub fn status(&self) -> CanNetworkStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| CanNetworkStatus::Terminated {
                reason: "共享 CAN 网络状态锁中毒".into(),
            })
    }

    /// 显式重新打开 `new` 时锁定的同一串口和 profile。
    ///
    /// 仅当 [`Self::status`] 为 [`CanNetworkStatus::Terminated`] 时成功。它只恢复总线及保存的
    /// CAN 传输保护状态，不附加任何 node；调用方必须显式选择一个已断开的会话并重新核对
    /// Identity，其他 node 保持断开。
    pub fn recover(&self) -> Result<(), String> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.control
            .send(NetworkCommand::Recover { reply })
            .map_err(|_| "共享 CAN 网络已关闭".to_owned())?;
        receive
            .recv()
            .map_err(|_| "共享 CAN 网络在恢复完成前已关闭".to_owned())?
    }

    #[cfg(test)]
    pub(crate) fn inject_transport_fault_for_test(&self) -> Result<(), String> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.control
            .send(NetworkCommand::InjectTransportFault { reply })
            .map_err(|_| "共享 CAN 网络已关闭".to_owned())?;
        receive
            .recv()
            .map_err(|_| "共享 CAN 网络在注入测试故障前已关闭".to_owned())?
    }

    /// 取得一个明确 node 的 connector；不发送任何 CAN 帧。
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
    /// 打开此 node 的独立 host backend；不会打开第二个串口。
    ///
    /// 若相同 node 仍有活跃 backend，或网络已终止，则返回错误；调用方不得把该错误当作
    /// 可以自动重放原业务的依据。
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
    #[cfg(test)]
    InjectTransportFault {
        reply: SyncSender<Result<(), String>>,
    },
    Shutdown,
}

enum BusEvent {
    Frame(NodeFrame),
}

struct NodeFrame {
    node: u8,
    lease: u64,
    lane: usize,
    pending: transport::can::PendingFrame,
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

/// 已注册 node 的集合与 CAN ID 路由。它只接受已附加节点，避免把未知节点的帧误交给
/// 任意 Client。
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
    Active(Box<dyn SerialPort>),
    Quiescing { reason: String },
    Terminated { reason: String },
}

fn open_serial(
    port: &str,
    profile: ExternalCanProfile,
) -> Result<(Box<dyn SerialPort>, Mode, AdapterProbe), String> {
    let mode = profile_mode(profile)?;
    let mut serial = serialport::new(port, 115_200)
        .dtr_on_open(true)
        .timeout(SERIAL_TIMEOUT)
        .open()
        .map_err(|error| format!("无法打开 CANable2 串口 {port}: {error}"))?;
    let probe = configure(&mut *serial, profile)
        .map_err(|error| format!("CANable2 SLCAN 配置失败: {error}"))?;
    Ok((serial, mode, probe))
}

fn set_status(status: &Arc<Mutex<CanNetworkStatus>>, next: CanNetworkStatus) {
    if let Ok(mut status) = status.lock() {
        *status = next;
    }
}

fn run_network(
    serial: Box<dyn SerialPort>,
    mode: Mode,
    probe: AdapterProbe,
    port_path: String,
    profile: ExternalCanProfile,
    commands: Receiver<NetworkCommand>,
    status: Arc<Mutex<CanNetworkStatus>>,
) {
    let (events, bus_events) = mpsc::channel::<BusEvent>();
    let (retired, retired_nodes) = mpsc::channel::<RetiredNode>();
    let mut routes = Routes::new();
    let mut nodes = BTreeMap::<u8, NodeHandle>::new();
    let mut reconnect = BTreeMap::<u8, NodeReconnectState>::new();
    let mut line = Vec::with_capacity(MAX_SLCAN_LINE);
    let mut read_buffer = [0_u8; 512];
    let epoch = Instant::now();
    let mut next_lease = 1_u64;

    let _ = probe;
    let mut phase = NetworkPhase::Active(serial);
    loop {
        match &mut phase {
            NetworkPhase::Active(port) => {
                retire_nodes(&retired_nodes, &mut nodes, &mut routes, &mut reconnect);
                let mut failure = None;
                for _ in 0..MAX_NETWORK_FRAMES_PER_TURN {
                    let Ok(BusEvent::Frame(frame)) = bus_events.try_recv() else {
                        break;
                    };
                    let Some(handle) = nodes
                        .get(&frame.node)
                        .filter(|handle| handle.lease == frame.lease)
                    else {
                        continue;
                    };
                    let outcome = encode_frame(&frame.pending.frame).and_then(|line| {
                        port.write_all(&line)
                            .and_then(|()| port.flush())
                            .map_err(|error| error.to_string())
                    });
                    match outcome {
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
                            let reason = format!("CANable2 串口写入失败: {error}");
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
                            #[cfg(test)]
                            NetworkCommand::InjectTransportFault { reply } => {
                                failure = Some("测试注入的 CANable2 串口故障".into());
                                let _ = reply.send(Ok(()));
                                break;
                            }
                            NetworkCommand::Attach { node, reply } => {
                                attach_node(
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
                                );
                            }
                        }
                    }
                }
                if failure.is_none()
                    && let Err(error) = drain_network_received(
                        &mut **port,
                        &mut line,
                        &mut read_buffer,
                        &routes,
                        &nodes,
                    )
                {
                    failure = Some(format!("CANable2 串口接收失败: {error}"));
                }
                if let Some(reason) = failure {
                    line.clear();
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
                Ok(NetworkCommand::Recover { reply }) => match open_serial(&port_path, profile) {
                    Ok((serial, recovered_mode, _probe)) => {
                        if recovered_mode != mode {
                            let _ = reply.send(Err("恢复的 CAN profile 模式与原网络不一致".into()));
                            continue;
                        }
                        set_status(&status, CanNetworkStatus::Active);
                        phase = NetworkPhase::Active(serial);
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                },
                #[cfg(test)]
                Ok(NetworkCommand::InjectTransportFault { reply }) => {
                    let _ = reply.send(Err("共享 CAN 网络已终止；请调用 recover".into()));
                }
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
        #[cfg(test)]
        NetworkCommand::InjectTransportFault { reply } => {
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
    let state = reconnect.remove(&node);
    match spawn_node(
        node,
        lease,
        mode,
        epoch,
        state,
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
            })
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
                1,
                tx_buffers,
            ),
            CanReceiver::new(
                node,
                protocol::Direction::FirmwareToHost,
                mode,
                1,
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
                NodeEvent::Submitted {
                    lane,
                    token,
                    result,
                    may_have_sent,
                    detail,
                } => {
                    awaiting[lane] = false;
                    let request = active[lane].as_mut();
                    let completion = transmitter.complete(token, result, pump.now_ms());
                    match completion {
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
        for (lane, awaiting_lane) in awaiting.iter_mut().enumerate() {
            start_node_request(lane, &mut pump, &mut transmitter, &mut active);
            if *awaiting_lane {
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

fn receive_frame(receiver: &mut CanReceiver<'_>, sink: &EventSink, frame: &Frame, now_ms: u64) {
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

fn drain_network_received(
    port: &mut dyn SerialPort,
    line: &mut Vec<u8>,
    buffer: &mut [u8],
    routes: &Routes,
    nodes: &BTreeMap<u8, NodeHandle>,
) -> std::io::Result<()> {
    let mut remaining = port.bytes_to_read().map_err(std::io::Error::from)?;
    for _ in 0..MAX_RX_CHUNKS_PER_TURN {
        if remaining == 0 {
            break;
        }
        let request = remaining.min(buffer.len() as u32) as usize;
        match port.read(&mut buffer[..request]) {
            Ok(0) => break,
            Ok(length) => {
                remaining = remaining.saturating_sub(length as u32);
                for byte in &buffer[..length] {
                    if *byte == b'\r' {
                        let raw = std::mem::take(line);
                        *line = Vec::with_capacity(MAX_SLCAN_LINE);
                        route_line(raw, routes, nodes);
                    } else if line.len() == MAX_SLCAN_LINE {
                        line.clear();
                    } else {
                        line.push(*byte);
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn route_line(raw: Vec<u8>, routes: &Routes, nodes: &BTreeMap<u8, NodeHandle>) {
    let Ok(Some(frame)) = decode_frame(&raw) else {
        return;
    };
    let Some(node) = routes.recipient(frame.id) else {
        return;
    };
    if let Some(handle) = nodes.get(&node) {
        let _ = handle.events.send(NodeEvent::Frame(frame));
    }
}
