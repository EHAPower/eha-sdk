// Copyright The eha-sdk Contributors
//! CANable2 SLCAN 桌面接入。
//!
//! 此模块只支持经串口枚举、运行原厂 CANable2 SLCAN 固件的适配器。它在 macOS、
//! Windows 和 Linux 上使用同一 `serialport` 实现；本轮不把 Linux SocketCAN 或其他
//! 固件的枚举结果表述为已支持的接入。一个完整业务消息仍由 [`transport::can`] 编码、
//! 分片和重组，本模块只在 SLCAN 文本与真实 CAN 帧之间转换。
//!
//! `fd_500k_500k` 与 `fd_1m_8m` 没有 CANable2 原厂 SLCAN 的可用配置命令，打开时会
//! 明确拒绝。SLCAN 坏行（包括截断的 FD 行）会保留原始字节并作为传输错误交给后端，
//! 不会补字节或静默丢弃。

use std::{
    collections::BTreeSet,
    io::{self, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use config::ExternalCanProfile;
use serialport::{SerialPort, SerialPortInfo, SerialPortType};
use transport::can::{
    Frame, Lane, Mode, ReceiveResult, Receiver, RxBuffers, RxReconnectState, SubmitEvent,
    SubmitResult, Transmitter, TxBuffers, TxReconnectState,
};

use crate::host::backend::{Backend, Event, EventSink, Pump, SendRequest};

mod network;

pub use network::{CanNetwork, CanNetworkStatus, CanNodeConnector};

const SERIAL_TIMEOUT: Duration = Duration::from_millis(2);
const ADAPTER_QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_SLCAN_LINE: usize = 256;
const MAX_RX_CHUNKS_PER_TURN: usize = 4;
// A reopened channel may still have CAN telemetry queued before the `V` response. Preserve a
// bounded sample for diagnostics while continuing to consume input within the fixed deadline.
const MAX_ADAPTER_OBSERVATIONS: usize = 8;
const GENERATION: u32 = 1;

type ReconnectSlot = Arc<Mutex<Option<CanReconnectState>>>;
type TerminalSlot = Arc<Mutex<Option<String>>>;

/// 只能在旧 I/O 线程退出后使用的 CAN 传输续接状态。
///
/// 这不保存任何未完成的业务请求或已接收但未交付的消息；它只保存 CAN 绑定所需的
/// 世代、编号与保护窗口。业务关联由 [`crate::host::ClientState`] 单独显式保存。
struct CanReconnectState {
    tx: TxReconnectState,
    rx: RxReconnectState,
    generation: u32,
}

struct AdapterProbe {
    firmware: Vec<u8>,
    observations: Vec<Vec<u8>>,
}

struct CanWorker {
    node: u8,
    mode: Mode,
    reconnect: ReconnectSlot,
    terminal: TerminalSlot,
    epoch: std::time::Instant,
    active: Arc<AtomicBool>,
}

/// 一个 CANable2 的显式、同进程通信恢复入口。
///
/// 一个连接器同时只允许一个 [`Backend`]。在该 `Backend` 被关闭或因断连退出后，再次
/// [`Self::open`] 会在旧串口已关闭后恢复 CAN 的世代、传输编号和保护窗口。它不会保存
/// 或重发待发送的业务消息，也不会恢复 [`crate::host::Client`] 的身份或业务会话。
/// 调用方须先取得 `ClientState`、重新打开，再以 `Client::resume` 和 `identify` 恢复
/// 业务关联。
pub struct CanConnector {
    options: CanOptions,
    reconnect: ReconnectSlot,
    terminal: TerminalSlot,
    active: Arc<AtomicBool>,
    epoch: std::time::Instant,
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

/// CANable2 SLCAN 的明确连接参数。
///
/// `serial_baud` 是主机与适配器的 USB CDC 串口速率，不是 CAN 位速率；CAN 位速率由
/// `profile` 与控制器已采用的配置一致时才有可能通信。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanOptions {
    /// 串口设备路径，例如 macOS 的 `/dev/cu.usbmodem…`、Linux 的 `/dev/ttyACM0` 或
    /// Windows 的 `COM3`。
    pub port: String,
    /// 主机到 CANable2 虚拟串口的速率。
    pub serial_baud: u32,
    /// 已知的 EHA 外部 CAN 逻辑节点号；它不是原始 CAN ID。
    pub node: u8,
    /// 必须与目标控制器本次启动实际采用的 CAN 配置组相同。
    pub profile: ExternalCanProfile,
}

/// 本机可见的 CANable2 USB 串口候选。
///
/// 此类型来自操作系统枚举，不会打开串口、认领接口或向适配器写入任何字节。USB 串口
/// 描述符不足以确认它运行 CANable2 原厂 SLCAN 固件；该确认仍只发生在
/// [`CanConnector::open`] 的 `V` 查询中。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialCandidate {
    /// 操作系统提供的串口路径或名称。
    pub port: String,
    /// USB 描述符中的可选序列号。
    pub serial: Option<String>,
    /// 可用于选择列表的人读 USB 描述。
    pub description: String,
}

/// 枚举本机可见的 CANable2 USB 串口候选，且不打开任何端口。
///
/// 候选只限 USB VID/PID 为 `16d0:117e` 的设备。这个描述符筛选避免把其他 USB
/// 串口交给 CANable2 连接器，但不能证明 SLCAN 固件或当前 CAN profile。调用方在用户
/// 选择后仍须用 [`CanConnector::open`] 和同一通路的 Identity 查询核对实际对象。
pub fn discover_serial_candidates() -> Result<Vec<SerialCandidate>, String> {
    serialport::available_ports()
        .map(serial_candidates_from_ports)
        .map_err(|error| format!("无法枚举本机 USB 串口：{error}"))
}

fn serial_candidates_from_ports(ports: Vec<SerialPortInfo>) -> Vec<SerialCandidate> {
    let mut candidates = ports
        .into_iter()
        .filter_map(|port| {
            let SerialPortType::UsbPort(info) = port.port_type else {
                return None;
            };
            if (info.vid, info.pid) != (0x16d0, 0x117e) {
                return None;
            }
            let description = [info.manufacturer, info.product]
                .into_iter()
                .flatten()
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            Some(SerialCandidate {
                port: port.port_name,
                serial: info.serial_number,
                description: if description.is_empty() {
                    "USB 串口（连接时核对 CANable2 SLCAN）".into()
                } else {
                    format!("{description}（连接时核对 CANable2 SLCAN）")
                },
            })
        })
        .collect::<Vec<_>>();
    // macOS exposes one CDC device through paired `/dev/cu.*` and `/dev/tty.*` paths. Sort
    // first for stable UI ordering, then remove only a `/dev/tty.*` path whose call-out peer is
    // present. Do not treat a USB serial number as an OS-level unique path.
    candidates.sort_by(|left, right| left.port.cmp(&right.port));
    let callout_suffixes = candidates
        .iter()
        .filter_map(|candidate| candidate.port.strip_prefix("/dev/cu."))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    candidates.retain(|candidate| match candidate.port.strip_prefix("/dev/tty.") {
        Some(suffix) => !callout_suffixes.contains(suffix),
        None => true,
    });
    candidates.dedup_by(|left, right| left.port == right.port);
    candidates
}

impl CanOptions {
    /// 为 CANable2 常用的 115200 bit/s CDC 串口创建连接参数。
    #[must_use]
    pub fn canable2(port: impl Into<String>, node: u8, profile: ExternalCanProfile) -> Self {
        Self {
            port: port.into(),
            serial_baud: 115_200,
            node,
            profile,
        }
    }
}

/// CANable2 原厂 SLCAN 不支持的 EHA CAN 配置组。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedProfile {
    /// 原厂 SLCAN 没有 500 kbit/s FD 数据段命令。
    Fd500K500K,
    /// 原厂 SLCAN 没有 8 Mbit/s FD 数据段命令。
    Fd1M8M,
}

impl std::fmt::Display for UnsupportedProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fd500K500K => formatter.write_str("CANable2 SLCAN 不支持 fd_500k_500k"),
            Self::Fd1M8M => formatter.write_str("CANable2 SLCAN 不支持 fd_1m_8m"),
        }
    }
}

/// 打开一条 CANable2 SLCAN 通路。
///
/// 成功仅代表已取得串口、写入本地 `C` 后收到格式匹配的适配器 `V` 查询回复，并把所选
/// SLCAN 配置命令写入主机串口。原厂 CANable2 SLCAN 固件不为配置命令提供 ACK/NACK，
/// 故这不证明适配器采用配置、总线 ACK 或固件收到消息；调用 [`crate::host::Client`] 的
/// 身份查询后才能把该连接用于业务操作。每次调用都会创建新的传输状态；需要在同一进程
/// 显式重开并保留 CAN 保护窗口时，使用 [`CanConnector`]。
pub fn open(options: CanOptions) -> Result<Backend, String> {
    CanConnector::new(options).open()
}

impl CanConnector {
    /// 创建可供同一进程显式重开的 CANable2 连接器。
    #[must_use]
    pub fn new(options: CanOptions) -> Self {
        Self {
            options,
            reconnect: Arc::new(Mutex::new(None)),
            terminal: Arc::new(Mutex::new(None)),
            active: Arc::new(AtomicBool::new(false)),
            epoch: std::time::Instant::now(),
        }
    }

    /// 打开或在已结束的旧 I/O 后恢复这条 CAN 通路。
    ///
    /// 打开失败不会消耗先前保存的传输状态。不能在先前返回的 [`Backend`] 仍活跃时调用；
    /// 先关闭旧 `Client`／`Backend` 并保存 [`crate::host::ClientState`]，才能重新打开。
    pub fn open(&mut self) -> Result<Backend, String> {
        match self.terminal.lock() {
            Ok(state) => {
                if let Some(reason) = state.as_ref() {
                    return Err(format!("CANable2 连接器不能安全续接: {reason}"));
                }
            }
            Err(_) => return Err("CANable2 连接器终态锁已损坏，不能安全续接".to_owned()),
        }
        let mode = profile_mode(self.options.profile)?;
        if self.options.node > 127 {
            return Err("EHA CAN 逻辑节点号必须在 0..=127".to_owned());
        }
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "CANable2 连接器已有活跃的本地 I/O owner".to_owned())?;

        let mut port = match serialport::new(&self.options.port, self.options.serial_baud)
            // The validated CDC opener asserted DTR. serialport otherwise preserves the host's
            // prior state, which is not an equivalent request on every supported desktop OS.
            .dtr_on_open(true)
            .timeout(SERIAL_TIMEOUT)
            .open()
        {
            Ok(port) => port,
            Err(error) => {
                self.active.store(false, Ordering::Release);
                return Err(format!(
                    "无法打开 CANable2 串口 {}: {error}",
                    self.options.port
                ));
            }
        };
        let adapter_probe = match configure(&mut *port, self.options.profile) {
            Ok(info) => info,
            Err(error) => {
                self.active.store(false, Ordering::Release);
                return Err(format!("CANable2 SLCAN 配置失败: {error}"));
            }
        };

        let reconnect = Arc::clone(&self.reconnect);
        let terminal = Arc::clone(&self.terminal);
        let terminal_after_panic = Arc::clone(&terminal);
        let active = Arc::clone(&self.active);
        let epoch = self.epoch;
        let node = self.options.node;
        match Backend::spawn("eha-sdk-can", move |commands, sink| {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.adapter_notice(
                    format!(
                        "CANable2 SLCAN 固件标识 {}",
                        escaped(&adapter_probe.firmware)
                    ),
                    adapter_probe.firmware,
                );
                for raw in adapter_probe.observations {
                    sink.adapter_notice("等待 SLCAN V 回复时观察到的适配器原始行", raw);
                }
                run(
                    port,
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
                    &terminal_after_panic,
                    "CAN I/O 线程 panic，旧 I/O 状态不能安全续接",
                );
            }
        }) {
            Ok(backend) => {
                Ok(backend.with_boundary("CANable2 串口 write/flush；不证明适配器或总线接收"))
            }
            Err(error) => {
                self.active.store(false, Ordering::Release);
                Err(error)
            }
        }
    }
}

fn profile_mode(profile: ExternalCanProfile) -> Result<Mode, String> {
    match profile {
        ExternalCanProfile::Classical500K | ExternalCanProfile::Classical1M => Ok(Mode::Classic),
        ExternalCanProfile::Fd500K2M | ExternalCanProfile::Fd1M2M | ExternalCanProfile::Fd1M5M => {
            Ok(Mode::Fd)
        }
        ExternalCanProfile::Fd500K500K => Err(UnsupportedProfile::Fd500K500K.to_string()),
        ExternalCanProfile::Fd1M8M => Err(UnsupportedProfile::Fd1M8M.to_string()),
    }
}

fn configure(port: &mut dyn SerialPort, profile: ExternalCanProfile) -> io::Result<AdapterProbe> {
    // `C` first makes a previously opened adapter configurable. This only controls the local
    // adapter; it is not an EHA stop or reset command.
    command_submit(port, b"C\r", "C close channel")?;
    let adapter_probe =
        command_response(port, b"V\r", "V firmware identity", is_canable2_firmware)?;
    match profile {
        ExternalCanProfile::Classical500K => {
            command_submit(port, b"S6\r", "S6 nominal 500 kbit/s")?
        }
        ExternalCanProfile::Classical1M => command_submit(port, b"S8\r", "S8 nominal 1 Mbit/s")?,
        ExternalCanProfile::Fd500K2M => {
            command_submit(port, b"S6\r", "S6 nominal 500 kbit/s")?;
            command_submit(port, b"Y2\r", "Y2 data 2 Mbit/s")?;
        }
        ExternalCanProfile::Fd1M2M => {
            command_submit(port, b"S8\r", "S8 nominal 1 Mbit/s")?;
            command_submit(port, b"Y2\r", "Y2 data 2 Mbit/s")?;
        }
        ExternalCanProfile::Fd1M5M => {
            command_submit(port, b"S8\r", "S8 nominal 1 Mbit/s")?;
            command_submit(port, b"Y5\r", "Y5 data 5 Mbit/s")?;
        }
        ExternalCanProfile::Fd500K500K | ExternalCanProfile::Fd1M8M => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "此 CANable2 SLCAN 配置组没有对应命令",
            ));
        }
    }
    command_submit(port, b"O\r", "O open channel")?;
    Ok(adapter_probe)
}

fn command_submit(port: &mut dyn SerialPort, command: &[u8], label: &str) -> io::Result<()> {
    port.write_all(command)?;
    port.flush().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("提交 SLCAN {label} 到主机串口失败: {error}"),
        )
    })
}

fn command_response(
    port: &mut dyn SerialPort,
    command: &[u8],
    label: &str,
    matches_response: fn(&[u8]) -> bool,
) -> io::Result<AdapterProbe> {
    command_submit(port, command, label)?;
    let deadline = std::time::Instant::now() + ADAPTER_QUERY_TIMEOUT;
    let mut line = Vec::new();
    let mut observations = Vec::new();
    let mut bytes = [0_u8; 64];
    while std::time::Instant::now() < deadline {
        match port.read(&mut bytes) {
            Ok(0) => continue,
            Ok(length) => {
                for byte in &bytes[..length] {
                    if *byte == b'\x07' {
                        line.push(*byte);
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("SLCAN {label} 被适配器拒绝，原始字节={}", escaped(&line)),
                        ));
                    }
                    if *byte == b'\r' {
                        if matches_response(&line) {
                            return Ok(AdapterProbe {
                                firmware: line,
                                observations,
                            });
                        }
                        if observations.len() < MAX_ADAPTER_OBSERVATIONS {
                            observations.push(std::mem::take(&mut line));
                        } else {
                            line.clear();
                        }
                        continue;
                    }
                    line.push(*byte);
                    if line.len() > MAX_SLCAN_LINE {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("SLCAN {label} 返回行过长，原始字节={}", escaped(&line)),
                        ));
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "等待 SLCAN {label} 回复超时，当前原始字节={}，此前非匹配行={}",
            escaped(&line),
            observations
                .iter()
                .map(|raw| escaped(raw))
                .collect::<Vec<_>>()
                .join(",")
        ),
    ))
}

fn is_canable2_firmware(raw: &[u8]) -> bool {
    raw.windows(b"normaldotcom/canable2".len())
        .any(|part| part == b"normaldotcom/canable2")
}

fn run(
    mut port: Box<dyn SerialPort>,
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
                "CAN 续接状态锁已损坏，不能安全恢复".to_owned(),
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
    let mut transmitter = match transmitter {
        Ok(transmitter) => transmitter,
        Err(error) => {
            if had_reconnect_state {
                mark_terminal(&worker.terminal, "CAN 发送续接状态不能恢复");
            }
            sink.event(Event::Disconnected(format!(
                "CAN 发送器初始化或续接失败: {error:?}"
            )));
            return;
        }
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
    let mut receiver = match receiver {
        Ok(receiver) => receiver,
        Err(error) => {
            if had_reconnect_state {
                mark_terminal(&worker.terminal, "CAN 接收续接状态不能恢复");
            }
            sink.event(Event::Disconnected(format!(
                "CAN 接收器初始化或续接失败: {error:?}"
            )));
            return;
        }
    };
    let mut pump = Pump::new(commands, sink.clone()).with_epoch(worker.epoch);
    let mut active: [Option<ActiveRequest>; 3] = [None, None, None];
    let mut line = Vec::with_capacity(MAX_SLCAN_LINE);
    let mut read_buffer = [0_u8; 512];

    'service: loop {
        if !pump.poll() {
            break;
        }
        if let Err(error) = drain_received(
            &mut *port,
            &mut line,
            &mut read_buffer,
            &mut receiver,
            &pump,
            generation,
        ) {
            pump.disconnected(format!("CANable2 串口接收失败: {error}"));
            break;
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
            if let Some(request) = active[lane].as_ref()
                && request.request.expired()
            {
                transmitter.abandon(lane_from_index(lane), pump.now_ms());
                let request = active[lane].take().expect("active request was checked");
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
                    break;
                };
                match encode_frame(&pending.frame) {
                    Ok(line) => match port.write_all(&line).and_then(|()| port.flush()) {
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
                                format!("CANable2 串口写入失败: {error}"),
                                true,
                            );
                            active[lane] = None;
                            pump.disconnected("CANable2 串口写入后连接不可继续使用");
                            break 'service;
                        }
                    },
                    Err(error) => {
                        let _ = transmitter.complete(
                            pending.token,
                            SubmitResult::Failed,
                            pump.now_ms(),
                        );
                        pump.sink.failed(request_id, error, false);
                        active[lane] = None;
                    }
                }
            }
        }
        // Do not insert an artificial inter-frame delay here. In particular, a successful
        // 400 µs paced diagnostic run is not a product-side cure for issue 11.
        std::thread::yield_now();
    }
    for request in active.into_iter().flatten() {
        pump.sink.failed(
            request.request.id,
            "CAN 后端关闭前未取得完整本地提交结果",
            request.may_have_sent,
        );
    }
    // 本地关闭不追加 `C`；该命令可能撤销已排队、但刚完成本地提交的帧。
    // The serial owner must be gone before incrementing the transport generation and publishing
    // reconnect state. An old OS write or read must never race a newly restored transmitter.
    drop(port);
    let now_ms = pump.now_ms();
    let tx = transmitter.into_reconnect_state(now_ms);
    let rx = receiver.into_reconnect_state(now_ms);
    let next_generation = generation.checked_add(1);
    let (tx_state, tx_error) = match tx {
        Ok(state) => (Some(state), None),
        Err(error) => (None, Some(error)),
    };
    let (rx_state, rx_error) = match rx {
        Ok(state) => (Some(state), None),
        Err(error) => (None, Some(error)),
    };
    match (tx_state, rx_state, next_generation) {
        (Some(tx), Some(rx), Some(generation)) => match worker.reconnect.lock() {
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
            pump.sink.transport_error(
                format!("CAN 不能导出续接状态: tx={tx_error:?}, rx={rx_error:?}"),
                Vec::new(),
            );
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

fn drain_received(
    port: &mut dyn SerialPort,
    line: &mut Vec<u8>,
    buffer: &mut [u8],
    receiver: &mut Receiver<'_>,
    pump: &Pump,
    generation: u32,
) -> io::Result<()> {
    let mut snapshot_remaining = port.bytes_to_read().map_err(io::Error::from)?;
    for _ in 0..MAX_RX_CHUNKS_PER_TURN {
        if snapshot_remaining == 0 {
            break;
        }
        let requested = snapshot_remaining.min(buffer.len() as u32) as usize;
        match port.read(&mut buffer[..requested]) {
            Ok(0) => break,
            Ok(length) => {
                snapshot_remaining = snapshot_remaining.saturating_sub(length as u32);
                for byte in &buffer[..length] {
                    if *byte == b'\r' {
                        let raw = std::mem::take(line);
                        *line = Vec::with_capacity(MAX_SLCAN_LINE);
                        process_line(raw, receiver, pump, generation);
                    } else if *byte == b'\x07' {
                        let mut raw = std::mem::take(line);
                        raw.push(*byte);
                        pump.sink.transport_error("SLCAN 适配器拒绝命令", raw);
                        *line = Vec::with_capacity(MAX_SLCAN_LINE);
                    } else if line.len() == MAX_SLCAN_LINE {
                        let mut raw = std::mem::take(line);
                        raw.push(*byte);
                        pump.sink
                            .transport_error("SLCAN 行超过最大长度且没有 CR 结束符", raw);
                        *line = Vec::with_capacity(MAX_SLCAN_LINE);
                    } else {
                        line.push(*byte);
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn process_line(raw: Vec<u8>, receiver: &mut Receiver<'_>, pump: &Pump, generation: u32) {
    if raw.is_empty() {
        pump.sink
            .adapter_notice("SLCAN 适配器确认一个本地命令", raw);
        return;
    }
    if let Some(detail) = adapter_notice(&raw) {
        pump.sink.adapter_notice(detail, raw);
        return;
    }
    let frame = match decode_frame(&raw) {
        Ok(Some(frame)) => frame,
        Ok(None) => return,
        Err(error) => {
            pump.sink.transport_error(error, raw);
            return;
        }
    };
    match receiver.receive(&frame, pump.now_ms(), generation) {
        ReceiveResult::Complete { lane, .. } => {
            let Some(message) = receiver.take_completed(lane) else {
                pump.sink.transport_error("CAN 完整消息缓冲丢失", raw);
                return;
            };
            let bytes = message.as_bytes().to_vec();
            match receiver.replace_buffer(lane, message.into_buffer()) {
                Ok(()) => pump.sink.received(bytes),
                Err(error) => pump
                    .sink
                    .transport_error(format!("CAN 完整消息缓冲无法归还: {error:?}"), raw),
            }
        }
        ReceiveResult::Rejected { reason } => pump
            .sink
            .transport_error(format!("CAN 帧不满足绑定: {reason:?}"), raw),
        ReceiveResult::Ignored | ReceiveResult::Incomplete { .. } => {}
    }
}

fn adapter_notice(raw: &[u8]) -> Option<&'static str> {
    match raw.first() {
        // Timestamp configuration and controller status are facts reported by the local
        // CANable2. They do not acknowledge an EHA message and `F` is not by itself proof of a
        // failed host submission.
        Some(b'Z' | b'z') => Some("SLCAN 适配器确认时间戳配置状态"),
        Some(b'F') => Some("SLCAN 适配器报告控制器状态"),
        Some(b'V' | b'v' | b'N') => Some("SLCAN 适配器报告本地识别信息"),
        _ => None,
    }
}

fn lane_from_index(index: usize) -> Lane {
    match index {
        0 => Lane::Heartbeat,
        1 => Lane::Short,
        _ => Lane::Large,
    }
}

fn encode_frame(frame: &Frame) -> Result<Vec<u8>, String> {
    if !frame.extended || frame.rtr || frame.id > 0x1fff_ffff {
        return Err("SLCAN 后端只提交有效的扩展数据帧".to_owned());
    }
    let tag = match (frame.fdf, frame.brs) {
        (false, false) => b'T',
        (true, true) => b'B',
        _ => return Err("SLCAN 后端不支持未启用 BRS 的 CAN FD 帧".to_owned()),
    };
    let length = dlc_length(frame.dlc).ok_or_else(|| "CAN DLC 非法".to_owned())?;
    if !frame.fdf && frame.dlc > 8 {
        return Err("经典 CAN 帧的 DLC 不能大于 8".to_owned());
    }
    if usize::from(frame.data_len) != length {
        return Err("CAN 驱动数据长度与 DLC 不符".to_owned());
    }
    let mut output = Vec::with_capacity(11 + length * 2);
    output.push(tag);
    push_hex(&mut output, u64::from(frame.id), 8);
    output.push(hex(frame.dlc));
    for byte in &frame.data[..length] {
        output.push(hex(byte >> 4));
        output.push(hex(byte & 0x0f));
    }
    output.push(b'\r');
    Ok(output)
}

fn decode_frame(raw: &[u8]) -> Result<Option<Frame>, String> {
    let Some(&tag) = raw.first() else {
        return Ok(None); // SLCAN command acknowledgement.
    };
    let (fdf, brs) = match tag {
        b'T' => (false, false),
        b'D' => (true, false),
        b'B' => (true, true),
        // Standard-ID traffic does not belong to an EHA extended-ID binding.
        b't' | b'b' | b'd' => return Ok(None),
        // These are local SLCAN adapter facts, never EHA application frames. Keep their raw
        // line visible to the caller rather than silently accepting a changed adapter state.
        b'F' | b'V' | b'v' | b'N' | b'Z' | b'z' => {
            return Err(format!("意外的 SLCAN 适配器状态行 {}", escaped(raw)));
        }
        _ => return Err(format!("未知 SLCAN 行类型 0x{tag:02x}")),
    };
    if raw.len() < 10 {
        return Err("SLCAN 扩展帧头被截断".to_owned());
    }
    let id = parse_hex(&raw[1..9]).ok_or_else(|| "SLCAN 扩展 ID 非十六进制".to_owned())?;
    if id > 0x1fff_ffff {
        return Err("SLCAN 扩展 ID 超出 29 位".to_owned());
    }
    let dlc = nibble(raw[9]).ok_or_else(|| "SLCAN DLC 非十六进制".to_owned())?;
    let length = dlc_length(dlc).ok_or_else(|| "SLCAN DLC 非法".to_owned())?;
    let frame_length = 10 + length * 2;
    if raw.len() != frame_length && raw.len() != frame_length + 4 {
        return Err(format!(
            "SLCAN 数据长度与 DLC 不符：DLC={dlc:X}，行长={}，期望 {frame_length} 或 {}",
            raw.len(),
            frame_length + 4
        ));
    }
    let mut data = [0_u8; 64];
    let (pairs, []) = raw[10..frame_length].as_chunks::<2>() else {
        return Err("SLCAN 数据字符数不是偶数".to_owned());
    };
    for (index, pair) in pairs.iter().enumerate() {
        let high = nibble(pair[0]).ok_or_else(|| "SLCAN 数据含非十六进制字符".to_owned())?;
        let low = nibble(pair[1]).ok_or_else(|| "SLCAN 数据含非十六进制字符".to_owned())?;
        data[index] = (high << 4) | low;
    }
    if raw.len() == frame_length + 4 && parse_hex(&raw[frame_length..]).is_none() {
        return Err("SLCAN 时间戳含非十六进制字符".to_owned());
    }
    Ok(Some(Frame {
        id: id as u32,
        extended: true,
        rtr: false,
        fdf,
        brs,
        esi: false,
        dlc,
        data_len: length as u8,
        data,
    }))
}

fn dlc_length(dlc: u8) -> Option<usize> {
    match dlc {
        0..=8 => Some(usize::from(dlc)),
        9 => Some(12),
        10 => Some(16),
        11 => Some(20),
        12 => Some(24),
        13 => Some(32),
        14 => Some(48),
        15 => Some(64),
        _ => None,
    }
}

fn parse_hex(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        nibble(*byte).map(|digit| (value << 4) | u64::from(digit))
    })
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex(value: u8) -> u8 {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    HEX[usize::from(value & 0x0f)]
}

fn push_hex(output: &mut Vec<u8>, value: u64, digits: usize) {
    for shift in (0..digits).rev().map(|index| index * 4) {
        output.push(hex((value >> shift) as u8));
    }
}

fn escaped(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_port(
        port_name: &str,
        vid: u16,
        pid: u16,
        serial_number: Option<&str>,
    ) -> SerialPortInfo {
        SerialPortInfo {
            port_name: port_name.into(),
            port_type: SerialPortType::UsbPort(serialport::UsbPortInfo {
                vid,
                pid,
                serial_number: serial_number.map(str::to_owned),
                manufacturer: Some("Normaldotcom".into()),
                product: Some("CANable2".into()),
            }),
        }
    }

    #[test]
    fn candidates_exclude_other_usb_serial_devices_and_prefer_macos_callout_path() {
        let candidates = serial_candidates_from_ports(vec![
            usb_port(
                "/dev/tty.usbmodem207033785743",
                0x16d0,
                0x117e,
                Some("207033785743"),
            ),
            usb_port("/dev/cu.usbmodemother", 0x16d0, 0x117e, None),
            usb_port("/dev/cu.CP210x", 0x10c4, 0xea60, Some("cp210x")),
            usb_port("/dev/tty.usbmodemother", 0x16d0, 0x117e, None),
            usb_port(
                "/dev/cu.usbmodem207033785743",
                0x16d0,
                0x117e,
                Some("207033785743"),
            ),
            usb_port(
                "/dev/cu.usbmodemseparate",
                0x16d0,
                0x117e,
                Some("207033785743"),
            ),
            usb_port("/dev/cu.odrive", 0x1209, 0x0d32, Some("odrive")),
        ]);

        assert_eq!(
            candidates
                .into_iter()
                .map(|candidate| candidate.port)
                .collect::<Vec<_>>(),
            vec![
                "/dev/cu.usbmodem207033785743",
                "/dev/cu.usbmodemother",
                "/dev/cu.usbmodemseparate",
            ]
        );
    }

    #[test]
    fn fd_slcan_round_trip_preserves_full_frame_and_brs() {
        let mut frame = Frame {
            id: 0x0070_0036,
            extended: true,
            rtr: false,
            fdf: true,
            brs: true,
            esi: false,
            dlc: 15,
            data_len: 64,
            data: [0; 64],
        };
        for (index, byte) in frame.data.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let line = encode_frame(&frame).expect("valid FD frame encodes");
        assert_eq!(line.len(), 139); // B + 8 ID + DLC + 128 data + CR
        assert_eq!(decode_frame(&line[..line.len() - 1]), Ok(Some(frame)));
    }

    #[test]
    fn truncated_fd_line_is_an_error_with_no_synthetic_frame() {
        // Issue 10 preserved a 112-byte SLCAN record including CR: the `B` header is 10
        // bytes and only 101 hexadecimal data characters follow, although DLC=F requires 128.
        let mut raw = b"B00700036F".to_vec();
        raw.extend(std::iter::repeat_n(b'0', 101));
        assert_eq!(raw.len() + 1, 112);
        assert!(
            decode_frame(&raw)
                .expect_err("a DLC=F line must contain all 64 bytes")
                .contains("长度与 DLC 不符")
        );
    }

    #[test]
    fn adapter_status_and_non_brs_fd_do_not_masquerade_as_valid_eha_fd() {
        assert!(adapter_notice(b"z1").is_some());
        assert!(adapter_notice(b"F01").is_some());
        let non_brs = decode_frame(b"D00700036100").expect("well-formed line");
        assert!(matches!(
            non_brs,
            Some(Frame {
                fdf: true,
                brs: false,
                ..
            })
        ));
    }

    #[test]
    fn firmware_probe_does_not_accept_a_can_line_as_adapter_identity() {
        assert!(is_canable2_firmware(
            b"b158aa7 github.com/normaldotcom/canable2.git"
        ));
        assert!(!is_canable2_firmware(b"B00700036100"));
        assert!(!is_canable2_firmware(b"F01"));
    }

    #[cfg(unix)]
    #[test]
    fn firmware_probe_consumes_backlogged_can_lines_before_v_response() {
        use std::{
            io::{ErrorKind, Read},
            sync::mpsc,
        };

        use serialport::{SerialPort, TTYPort};

        let (mut master, mut slave) = TTYPort::pair().expect("pseudo serial pair opens");
        master
            .set_timeout(Duration::from_millis(10))
            .expect("master timeout configures");
        slave
            .set_timeout(SERIAL_TIMEOUT)
            .expect("slave timeout configures");
        let _retained_slave = slave.try_clone_native().expect("slave clone opens");
        let (response_sent, response_observed) = mpsc::sync_channel(1);
        let (release_master, await_release) = mpsc::sync_channel(1);
        let responder = std::thread::spawn(move || {
            let mut command = Vec::new();
            let mut byte = [0_u8; 1];
            loop {
                match master.read(&mut byte) {
                    Ok(0) => continue,
                    Ok(_) => {
                        command.push(byte[0]);
                        if command.ends_with(b"V\r") {
                            // This is a well-formed BRS FD frame (`DLC=1`, data byte `00`), not
                            // synthetic malformed input. A real adapter can leave these frames
                            // in the CDC receive queue while a previous channel remains open.
                            let telemetry = b"B00700036100\r";
                            assert!(
                                decode_frame(&telemetry[..telemetry.len() - 1])
                                    .expect("BRS telemetry frame parses")
                                    .is_some()
                            );
                            for _ in 0..=8 {
                                master
                                    .write_all(telemetry)
                                    .expect("backlogged CAN telemetry is written");
                            }
                            master
                                .write_all(b"b158aa7 github.com/normaldotcom/canable2.git\r")
                                .expect("firmware identity is written");
                            master.flush().expect("adapter response is flushed");
                            response_sent
                                .send(())
                                .expect("response completion is observed");
                            await_release
                                .recv_timeout(Duration::from_secs(1))
                                .expect("probe completes before pseudo adapter closes");
                            return;
                        }
                    }
                    Err(error)
                        if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
                    {
                        continue;
                    }
                    Err(_) => return,
                }
            }
        });

        let result = command_response(
            &mut slave,
            b"V\r",
            "V firmware identity",
            is_canable2_firmware,
        );
        response_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("adapter responder completed");
        release_master
            .send(())
            .expect("pseudo adapter may close after probe completes");
        responder.join().expect("adapter responder exits");
        let probe = result.expect("firmware identity follows more than eight queued CAN lines");
        assert!(is_canable2_firmware(&probe.firmware));
        assert_eq!(probe.observations.len(), MAX_ADAPTER_OBSERVATIONS);
        assert!(probe.observations.iter().all(|raw| raw == b"B00700036100"));
    }

    #[test]
    fn only_canable2_supported_profiles_are_accepted() {
        assert_eq!(
            profile_mode(ExternalCanProfile::Classical1M),
            Ok(Mode::Classic)
        );
        assert_eq!(profile_mode(ExternalCanProfile::Fd1M5M), Ok(Mode::Fd));
        assert!(profile_mode(ExternalCanProfile::Fd1M8M).is_err());
        assert!(profile_mode(ExternalCanProfile::Fd500K500K).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn closing_after_submitted_enter_update_does_not_append_slcan_close() {
        use std::{
            io::{ErrorKind, Read},
            sync::{
                Arc, Mutex,
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            time::Instant,
        };

        use serialport::{SerialPort, TTYPort};

        let (mut master, mut slave) = TTYPort::pair().expect("pseudo serial pair opens");
        master
            .set_timeout(Duration::from_millis(10))
            .expect("master timeout configures");
        slave
            .set_timeout(SERIAL_TIMEOUT)
            .expect("worker timeout configures");
        // Retain a peer descriptor so the worker's port close cannot discard the bytes that the
        // actual TTY received before this test inspects them.
        let _retained_slave = slave.try_clone_native().expect("slave clone opens");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let reader_observed = Arc::clone(&observed);
        let reader_done = Arc::new(AtomicBool::new(false));
        let reader_done_in_thread = Arc::clone(&reader_done);
        let (line_ready_tx, line_ready_rx) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut byte = [0_u8; 1];
            loop {
                match master.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {
                        reader_observed
                            .lock()
                            .expect("observed bytes are available")
                            .push(byte[0]);
                        if byte[0] == b'\r' {
                            let _ = line_ready_tx.try_send(());
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::TimedOut => {
                        if reader_done_in_thread.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    Err(error) => {
                        assert!(
                            reader_done_in_thread.load(Ordering::Acquire),
                            "unexpected pseudo serial read error: {error}"
                        );
                        break;
                    }
                }
            }
        });
        let reconnect = Arc::new(Mutex::new(None));
        let terminal = Arc::new(Mutex::new(None));
        let active = Arc::new(AtomicBool::new(false));
        let worker = CanWorker {
            node: 1,
            mode: Mode::Fd,
            reconnect,
            terminal,
            epoch: Instant::now(),
            active,
        };
        let backend = Backend::spawn("eha-sdk-can-close-test", move |commands, sink| {
            run(Box::new(slave), commands, sink, worker);
        })
        .expect("CAN worker starts");

        let key_bytes = protocol::OperationKeyFields::new([1; 12], [2; 16], 1).to_bytes();
        let key = protocol::OperationKey::new(&key_bytes).expect("operation key is exact");
        let message = protocol::Message::Maintenance {
            kind: protocol::MessageKind::EnterUpdate,
            key,
        };
        let mut bytes = vec![0; message.encoded_len().expect("message length")];
        protocol::encode(message, &mut bytes).expect("enter-update encodes");
        backend
            .tx
            .send(crate::host::backend::Command::Send(SendRequest {
                id: 1,
                bytes,
                deadline: Instant::now() + Duration::from_secs(1),
                cancelled: Arc::new(AtomicBool::new(false)),
            }))
            .expect("request reaches worker");

        let submitted_by = Instant::now() + Duration::from_secs(1);
        loop {
            let (lock, cv) = &*backend.sink.0;
            let inbox = lock.lock().expect("event inbox is available");
            if inbox
                .events
                .iter()
                .any(|event| matches!(event, Event::Submitted(1)))
            {
                break;
            }
            let remaining = submitted_by.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "enter-update was not locally submitted"
            );
            drop(
                cv.wait_timeout(inbox, remaining.min(Duration::from_millis(10)))
                    .expect("event wait is available")
                    .0,
            );
        }

        line_ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("submitted SLCAN frame arrives");

        drop(backend);
        reader_done.store(true, Ordering::Release);
        reader
            .join()
            .expect("pseudo serial reader stops after worker close");
        let observed = observed.lock().expect("observed bytes are available");
        let line_end = observed
            .iter()
            .position(|byte| *byte == b'\r')
            .expect("submitted SLCAN frame is complete");
        let (line, tail) = observed.split_at(line_end + 1);
        assert!(line.starts_with(b"B"), "worker submits an FD SLCAN frame");
        assert!(
            !tail.windows(b"C\r".len()).any(|bytes| bytes == b"C\r"),
            "backend close must not append SLCAN C command: {}",
            escaped(tail)
        );
    }

    #[test]
    fn network_routes_interleaved_frames_to_their_own_nodes() {
        let mut routes = super::network::Routes::new();
        routes.attach(3).expect("first node attaches");
        routes.attach(91).expect("second node attaches");

        assert_eq!(routes.recipient(0x00c0_0000), Some(3));
        assert_eq!(routes.recipient(0x16c0_0000), Some(91));
        assert_eq!(routes.recipient(0x00c0_0001), Some(3));
        assert_eq!(routes.recipient(0x0040_0000), None);
    }

    #[cfg(unix)]
    #[test]
    fn network_shares_one_serial_owner_and_reopens_a_retired_node() {
        use std::{
            io::{ErrorKind, Read},
            sync::{Arc, Mutex, mpsc},
            time::Instant,
        };

        use serialport::{SerialPort, TTYPort};

        let (mut master, slave) = TTYPort::pair().expect("pseudo serial pair opens");
        master
            .set_timeout(Duration::from_millis(10))
            .expect("master timeout configures");
        let port = slave.name().expect("pseudo serial path exists");
        let lines = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let seen = Arc::clone(&lines);
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let responder = std::thread::spawn(move || {
            let mut line = Vec::new();
            let mut byte = [0_u8; 1];
            loop {
                match master.read(&mut byte) {
                    Ok(0) => continue,
                    Ok(_) if byte[0] == b'\r' => {
                        if line == b"V" {
                            master
                                .write_all(b"b158aa7 github.com/normaldotcom/canable2.git\r")
                                .expect("adapter identifies itself");
                            master.flush().expect("identity flushes");
                        } else if line.first() == Some(&b'B') {
                            seen.lock().expect("lines lock").push(line.clone());
                        }
                        line.clear();
                    }
                    Ok(_) => line.push(byte[0]),
                    Err(error)
                        if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
                    {
                        if stop_rx.try_recv().is_ok() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        let network =
            CanNetwork::new(port, ExternalCanProfile::Fd1M5M).expect("one SLCAN owner opens");
        drop(slave);
        let node_three = network.node(3).expect("valid first node");
        let node_ninety_one = network.node(91).expect("valid second node");
        let first = node_three.open().expect("first endpoint opens");
        let second = node_ninety_one.open().expect("second endpoint opens");
        let position = protocol::Message::Position(1.25);
        let mut bytes = vec![0; position.encoded_len().expect("position length")];
        protocol::encode(position, &mut bytes).expect("position encodes");
        first
            .tx
            .send(crate::host::backend::Command::Send(SendRequest {
                id: 11,
                bytes: bytes.clone(),
                deadline: Instant::now() + Duration::from_secs(1),
                cancelled: Arc::new(AtomicBool::new(false)),
            }))
            .expect("first request reaches node worker");
        second
            .tx
            .send(crate::host::backend::Command::Send(SendRequest {
                id: 12,
                bytes,
                deadline: Instant::now() + Duration::from_secs(1),
                cancelled: Arc::new(AtomicBool::new(false)),
            }))
            .expect("second request reaches node worker");

        wait_for_submission(&first, 11);
        wait_for_submission(&second, 12);
        drop(first);
        let reopened = node_three.open().expect("retired node immediately reopens");
        let position = protocol::Message::Position(2.5);
        let mut bytes = vec![0; position.encoded_len().expect("position length")];
        protocol::encode(position, &mut bytes).expect("position encodes");
        reopened
            .tx
            .send(crate::host::backend::Command::Send(SendRequest {
                id: 13,
                bytes,
                deadline: Instant::now() + Duration::from_secs(1),
                cancelled: Arc::new(AtomicBool::new(false)),
            }))
            .expect("reopened request reaches node worker");
        wait_for_submission(&reopened, 13);

        let deadline = Instant::now() + Duration::from_secs(1);
        while lines.lock().expect("lines lock").len() < 3 {
            assert!(Instant::now() < deadline, "three CAN frames are written");
            std::thread::yield_now();
        }
        let identifiers = lines
            .lock()
            .expect("lines lock")
            .iter()
            .map(|line| parse_hex(&line[1..9]).expect("encoded identifier") as u32)
            .collect::<Vec<_>>();
        assert!(identifiers.iter().any(|id| (id >> 22) == 3));
        assert!(identifiers.iter().any(|id| (id >> 22) == 91));
        let reopened_id = identifiers
            .iter()
            .rev()
            .find(|id| (**id >> 22) == 3)
            .copied()
            .expect("reopened node sends a frame");
        assert_eq!(
            (reopened_id >> 12) & 0x7f,
            1,
            "node keeps transfer-id guard state"
        );

        network
            .inject_transport_fault_for_test()
            .expect("pseudo adapter fault isolates the network");
        wait_for_network_status(&network, |status| {
            matches!(status, CanNetworkStatus::Terminated { .. })
        });
        network
            .recover()
            .expect("explicit recover reopens the pseudo adapter");
        assert_eq!(network.status(), CanNetworkStatus::Active);
        let recovered = node_three
            .open()
            .expect("only the user-selected node reattaches after recover");
        let position = protocol::Message::Position(3.75);
        let mut bytes = vec![0; position.encoded_len().expect("position length")];
        protocol::encode(position, &mut bytes).expect("position encodes");
        recovered
            .tx
            .send(crate::host::backend::Command::Send(SendRequest {
                id: 14,
                bytes,
                deadline: Instant::now() + Duration::from_secs(1),
                cancelled: Arc::new(AtomicBool::new(false)),
            }))
            .expect("recovered request reaches node worker");
        wait_for_submission(&recovered, 14);
        let deadline = Instant::now() + Duration::from_secs(1);
        while lines.lock().expect("lines lock").len() < 4 {
            assert!(Instant::now() < deadline, "recovered CAN frame is written");
            std::thread::yield_now();
        }
        let identifiers = lines
            .lock()
            .expect("lines lock")
            .iter()
            .map(|line| parse_hex(&line[1..9]).expect("encoded identifier") as u32)
            .collect::<Vec<_>>();
        assert_eq!(
            identifiers.iter().filter(|id| (**id >> 22) == 91).count(),
            1,
            "unselected node stays disconnected after explicit recover"
        );
        let recovered_id = identifiers
            .last()
            .copied()
            .expect("recovered node sends a frame");
        assert_eq!(recovered_id >> 22, 3);
        assert_eq!(
            (recovered_id >> 12) & 0x7f,
            2,
            "explicit recover preserves the selected node transfer-id guard"
        );

        drop(reopened);
        drop(second);
        drop(recovered);
        drop(network);
        let _ = stop_tx.send(());
        responder.join().expect("pseudo adapter exits");
    }

    #[cfg(unix)]
    fn wait_for_submission(backend: &Backend, id: u64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let (lock, cv) = &*backend.sink.0;
            let inbox = lock.lock().expect("event inbox available");
            if inbox
                .events
                .iter()
                .any(|event| matches!(event, Event::Submitted(found) if *found == id))
            {
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "request {id} is locally submitted");
            drop(
                cv.wait_timeout(inbox, remaining.min(Duration::from_millis(10)))
                    .expect("event wait available")
                    .0,
            );
        }
    }

    #[cfg(unix)]
    fn wait_for_network_status(network: &CanNetwork, expected: impl Fn(&CanNetworkStatus) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let status = network.status();
            if expected(&status) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "network reaches expected status, got {status:?}"
            );
            std::thread::yield_now();
        }
    }
}
