// Copyright The eha-sdk Contributors

//! 桌面 USB Type-C 接入。
//!
//! 此模块只装配 `nusb` 的厂商 Bulk 端点，并把实际字节交给
//! [`transport::usb`]。它不解释公共业务消息，也不因超时、关闭或 I/O
//! 错误重发、停止或复位设备。当前实现支持 nusb 0.2.7 所支持的 Windows、
//! macOS 和 Linux 主机；主机必须能以用户态独占 `ff:45:01` 接口。

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use nusb::{
    MaybeFuture,
    transfer::{Buffer, Bulk, Direction, In, Out},
};

use crate::host::backend::{Backend, EventSink, Pump};

const VENDOR_ID: u16 = 0x1209;
const PRODUCT_ID: u16 = 0x0001;
const INTERFACE_CLASS: u8 = 0xff;
const INTERFACE_SUBCLASS: u8 = 0x45;
const INTERFACE_PROTOCOL: u8 = 0x01;
const MAX_MESSAGE_LEN: usize = 16_440;
const IO_IDLE_WAIT: Duration = Duration::from_millis(1);
const IO_ISOLATION_WAIT: Duration = Duration::from_secs(1);

struct ActiveRequest {
    id: u64,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl ActiveRequest {
    fn expired(&self) -> bool {
        // id=0 是 Pump 的周期心跳槽位。它在冻结前仍由 Pump 丢弃过期项，
        // 但一旦已交给 Bulk OUT，只能由实际 completion、USB 错误或显式断开
        // 结束；不能把一个错过的调度节拍当作业务请求结果未知而关闭会话。
        self.id != 0 && (self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline)
    }
}

struct ReconnectState {
    sender: transport::usb::SenderReconnectState,
    receiver: transport::usb::ReceiverReconnectState,
    epoch: Instant,
}

struct OpenEndpoints {
    interface: nusb::Interface,
    input: nusb::Endpoint<Bulk, In>,
    output: nusb::Endpoint<Bulk, Out>,
}

#[derive(Clone)]
struct ConnectorState {
    reconnect: Arc<Mutex<Option<ReconnectState>>>,
    active: Arc<AtomicBool>,
    reopen_allowed: Arc<AtomicBool>,
}

/// 可重复打开同一 USB 设备的连接器。
///
/// 同一实例一次只允许一个活跃后端。前一个后端结束后，下一次 [`Self::open`]
/// 从已隔离端点导出的阶段、发送编号及接收保护期恢复；它不携带部分 COBS 块、
/// 未完成请求或任何业务结果，也不会自动重发。
pub struct UsbConnector {
    serial: String,
    state: ConnectorState,
}

impl UsbConnector {
    /// 建立只包含持久 USB 序列号的连接器，不枚举、打开或发送。
    pub fn new(serial: impl Into<String>) -> Result<Self, String> {
        let serial = serial.into();
        if serial.is_empty() {
            return Err("USB 序列号不能为空。".to_owned());
        }
        Ok(Self {
            serial,
            state: ConnectorState {
                reconnect: Arc::new(Mutex::new(None)),
                active: Arc::new(AtomicBool::new(false)),
                reopen_allowed: Arc::new(AtomicBool::new(true)),
            },
        })
    }

    /// 打开当前设备并创建一个独占 I/O 后端。
    pub fn open(&mut self) -> Result<Backend, String> {
        if !self.state.reopen_allowed.load(Ordering::Acquire) {
            return Err("上一 USB I/O 未能确认隔离，不能安全重开此连接器。".to_owned());
        }
        if self.state.active.swap(true, Ordering::AcqRel) {
            return Err("此 USB 连接器已有活跃后端。".to_owned());
        }
        let result = self.open_active();
        if result.is_err() {
            self.state.active.store(false, Ordering::Release);
        }
        result
    }

    fn open_active(&mut self) -> Result<Backend, String> {
        let endpoints = open_endpoints(&self.serial)?;
        let state = self.state.clone();
        Backend::spawn("eha-usb", move |rx, sink| {
            let reconnect = match state.reconnect.lock() {
                Ok(mut store) => store.take(),
                Err(_) => {
                    sink.disconnected("USB 重连状态锁已中毒。");
                    state.active.store(false, Ordering::Release);
                    return;
                }
            };
            run(endpoints, rx, sink, reconnect, state);
        })
        .map(|backend| backend.with_boundary("USB 整条消息最后一个 Bulk 包本地完成"))
    }
}

/// 供应用展示和选择的 USB 发现快照。
///
/// 该值不持有 USB 句柄；发现成功也不表示设备应用固件、端点或业务状态已经
/// 核对。`open` 会重新枚举，并以完整序列号和实际活动描述符完成严格核对。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    /// 描述符中的完整序列号；缺失时不能用作持久设备身份。
    pub serial: Option<String>,
    /// 供使用者核对候选的本地描述。
    pub description: String,
}

/// 枚举 VID/PID 与厂商接口摘要相符的 USB 候选，而不打开、认领或发送。
///
/// 某些主机的枚举 API 不提供接口摘要。此时仍保留 VID/PID 候选，并标明尚未
/// 核对接口；实际打开会要求当前活动接口具有一对 64-byte Bulk 端点。
pub fn discover() -> Result<Vec<Device>, String> {
    let devices = nusb::list_devices()
        .wait()
        .map_err(|error| format!("USB 设备枚举失败：{error}"))?
        .filter(|info| info.vendor_id() == VENDOR_ID && info.product_id() == PRODUCT_ID)
        .filter_map(|info| {
            let interfaces: Vec<_> = info.interfaces().collect();
            let verified = interfaces
                .iter()
                .any(|interface| interface_matches(interface));
            if !verified && !interfaces.is_empty() {
                return None;
            }
            let product = info.product_string().unwrap_or("产品字符串不可用");
            let interface = if verified {
                "接口 ff:45:01 已核对"
            } else {
                "接口信息不可用，ff:45:01 尚未核对"
            };
            Some(Device {
                serial: info.serial_number().map(str::to_owned),
                description: format!("USB 1209:0001；{product}；{interface}"),
            })
        })
        .collect();
    Ok(devices)
}

/// 从一次发现快照选择设备。序列号优先于一开始的列表序号解析。
pub fn choose(devices: &[Device], selector: &str) -> Result<Device, String> {
    if selector.is_empty() {
        return Err("请选择列表序号或完整 USB 序列号。".to_owned());
    }
    if devices
        .iter()
        .any(|device| device.serial.as_deref() == Some(selector))
    {
        return find_serial(devices, selector);
    }
    let index = selector
        .parse::<usize>()
        .map_err(|_| "请选择从 1 开始的列表序号或完整 USB 序列号。".to_owned())?;
    if index == 0 {
        return Err("列表序号从 1 开始。".to_owned());
    }
    let serial = devices
        .get(index - 1)
        .and_then(|device| device.serial.as_deref())
        .filter(|serial| !serial.is_empty())
        .ok_or_else(|| "该设备未提供 USB 序列号，不能持久选择。".to_owned())?;
    find_serial(devices, serial)
}

/// 在新的发现快照中严格按完整 USB 序列号查找设备。
pub fn find_serial(devices: &[Device], serial: &str) -> Result<Device, String> {
    if serial.is_empty() {
        return Err("USB 序列号不能为空。".to_owned());
    }
    let matches: Vec<_> = devices
        .iter()
        .filter(|device| device.serial.as_deref() == Some(serial))
        .collect();
    match matches.len() {
        0 => Err("未发现指定 USB 序列号的设备。".to_owned()),
        1 => Ok(matches[0].clone()),
        _ => Err("USB 序列号重复，不能唯一选择设备。".to_owned()),
    }
}

/// 重新枚举、按完整序列号唯一选择，并独占打开当前的厂商 Bulk 接口。
///
/// 本函数不发送 USB reset、控制请求、心跳或任何应用消息。返回的后台仅在
/// 调用方明确提交业务消息或开启心跳后才会写 OUT 端点。
pub fn open(serial: &str) -> Result<Backend, String> {
    UsbConnector::new(serial)?.open()
}

fn open_endpoints(serial: &str) -> Result<OpenEndpoints, String> {
    if serial.is_empty() {
        return Err("USB 序列号不能为空。".to_owned());
    }
    let candidates: Vec<_> = nusb::list_devices()
        .wait()
        .map_err(|error| format!("USB 设备枚举失败：{error}"))?
        .filter(|info| {
            info.vendor_id() == VENDOR_ID
                && info.product_id() == PRODUCT_ID
                && info.serial_number() == Some(serial)
        })
        .collect();
    let info = match candidates.len() {
        0 => return Err("未发现指定 USB 序列号的设备。".to_owned()),
        1 => candidates
            .into_iter()
            .next()
            .ok_or_else(|| "USB 唯一候选在重新枚举后消失。".to_owned())?,
        _ => return Err("USB 序列号重复，不能唯一选择设备。".to_owned()),
    };

    let device = info
        .open()
        .wait()
        .map_err(|error| format!("USB 打开失败：{error}"))?;
    let mut interface_numbers: Vec<_> = device
        .active_configuration()
        .map_err(|error| format!("USB 当前配置描述符不可用：{error}"))?
        .interface_alt_settings()
        .filter(|interface| {
            interface.class() == INTERFACE_CLASS
                && interface.subclass() == INTERFACE_SUBCLASS
                && interface.protocol() == INTERFACE_PROTOCOL
        })
        .map(|interface| interface.interface_number())
        .collect();
    interface_numbers.sort_unstable();
    interface_numbers.dedup();
    let interface_number = match interface_numbers.as_slice() {
        [number] => *number,
        [] => return Err("USB 当前配置没有 ff:45:01 厂商接口。".to_owned()),
        _ => return Err("USB 当前配置有多个 ff:45:01 接口，不能唯一配对。".to_owned()),
    };
    // 不 detach 内核驱动，不发 reset；若接口无法独占，直接如实失败。
    let interface = device
        .claim_interface(interface_number)
        .wait()
        .map_err(|error| {
            format!(
                "USB 接口认领失败：{error}。可能已有 SDK、eha-tool 或 WebUI 会话占用该接口，也可能是系统或驱动错误；请保留原错误排查。若确认有旧会话，先在该会话中显式 Stop 并以状态或遥测确认，再关闭或断开旧会话后重试。"
            )
        })?;
    let descriptor = interface
        .descriptor()
        .ok_or_else(|| "USB 当前活动接口没有描述符。".to_owned())?;
    if descriptor.class() != INTERFACE_CLASS
        || descriptor.subclass() != INTERFACE_SUBCLASS
        || descriptor.protocol() != INTERFACE_PROTOCOL
    {
        return Err("USB 当前活动接口不是 ff:45:01。".to_owned());
    }

    let mut input = None;
    let mut output = None;
    for endpoint in descriptor.endpoints() {
        if endpoint.transfer_type() != nusb::descriptors::TransferType::Bulk {
            continue;
        }
        if endpoint.max_packet_size() != transport::usb::PACKET_LEN {
            return Err("USB Bulk 端点最大包长不是 64 字节。".to_owned());
        }
        match endpoint.direction() {
            Direction::In if input.replace(endpoint.address()).is_some() => {
                return Err("USB 接口存在多个 Bulk IN 端点，不能唯一配对。".to_owned());
            }
            Direction::Out if output.replace(endpoint.address()).is_some() => {
                return Err("USB 接口存在多个 Bulk OUT 端点，不能唯一配对。".to_owned());
            }
            Direction::In | Direction::Out => {}
        }
    }
    let input = input.ok_or_else(|| "USB 接口缺少 Bulk IN 端点。".to_owned())?;
    let output = output.ok_or_else(|| "USB 接口缺少 Bulk OUT 端点。".to_owned())?;
    let input = interface
        .endpoint::<Bulk, In>(input)
        .map_err(|error| format!("USB Bulk IN 打开失败：{error}"))?;
    let output = interface
        .endpoint::<Bulk, Out>(output)
        .map_err(|error| format!("USB Bulk OUT 打开失败：{error}"))?;

    Ok(OpenEndpoints {
        interface,
        input,
        output,
    })
}

fn interface_matches(interface: &nusb::InterfaceInfo) -> bool {
    interface.class() == INTERFACE_CLASS
        && interface.subclass() == INTERFACE_SUBCLASS
        && interface.protocol() == INTERFACE_PROTOCOL
}

fn run(
    endpoints: OpenEndpoints,
    rx: std::sync::mpsc::Receiver<crate::host::backend::Command>,
    sink: EventSink,
    reconnect: Option<ReconnectState>,
    connector: ConnectorState,
) {
    let OpenEndpoints {
        interface,
        mut input,
        mut output,
    } = endpoints;
    let epoch = reconnect
        .as_ref()
        .map_or_else(Instant::now, |state| state.epoch);
    let mut pump = Pump::new(rx, sink.clone()).with_epoch(epoch);
    let mut tx_heartbeat = [0_u8; 8];
    let mut tx_short = [0_u8; 256];
    let mut tx_long = [0_u8; MAX_MESSAGE_LEN];
    let mut rx_heartbeat = [0_u8; 8];
    let mut rx_short = [0_u8; 256];
    let mut rx_long = [0_u8; MAX_MESSAGE_LEN];
    let (mut sender, mut receiver) = match reconnect {
        Some(ReconnectState {
            sender, receiver, ..
        }) => (
            transport::usb::Sender::from_reconnect_state(
                sender,
                &mut tx_heartbeat,
                &mut tx_short,
                &mut tx_long,
            ),
            transport::usb::Receiver::from_reconnect_state(
                receiver,
                &mut rx_heartbeat,
                &mut rx_short,
                &mut rx_long,
            ),
        ),
        None => (
            transport::usb::Sender::new(
                protocol::Direction::HostToFirmware,
                &mut tx_heartbeat,
                &mut tx_short,
                &mut tx_long,
            ),
            transport::usb::Receiver::new(
                protocol::Direction::FirmwareToHost,
                &mut rx_heartbeat,
                &mut rx_short,
                &mut rx_long,
            ),
        ),
    };
    let mut packet = [0_u8; transport::usb::PACKET_LEN];
    let mut pending_tx: Option<(transport::usb::TxToken, usize)> = None;
    let mut active = [None, None, None];

    input.submit(Buffer::new(transport::usb::PACKET_LEN));
    loop {
        if !pump.poll() {
            break;
        }
        let now_ms = pump.now_ms();
        report_timeouts(&sink, receiver.poll(now_ms), Vec::new());

        // worker 再次取得 CPU 时，Bulk OUT completion 可能已经就绪。先结清它，
        // 再检查 ActiveRequest 期限，避免主机调度延迟把已本地完成的心跳误判为断连。
        if !complete_outgoing(
            &mut output,
            &mut sender,
            &mut pending_tx,
            &mut active,
            &sink,
            now_ms,
        ) {
            input.cancel_all();
            output.cancel_all();
            pump.disconnected("USB Bulk OUT 传输失败或断开；未完成操作结果未知");
            break;
        }

        if active_request_expired(&active) {
            if let Some((_, lane)) = pending_tx {
                if let Some(request) = active[lane].take() {
                    pump.sink.failed(
                        request.id,
                        "USB 本地提交后请求已取消或到期，设备采用与执行结果未知",
                        true,
                    );
                }
            } else {
                fail_expired_active(&mut active, &pump.sink);
            }
            // 此实例不会重用端点或进入新阶段。丢弃端点会取消尚未完成的 nusb
            // I/O，因此迟到完成也没有机会被下一连接当作新阶段字节。
            input.cancel_all();
            output.cancel_all();
            pump.disconnected("USB 请求取消或到期后关闭连接，未自动重发");
            break;
        }

        if let Err(reason) =
            drain_received(&mut input, &mut receiver, &sink, now_ms, Duration::ZERO)
        {
            input.cancel_all();
            output.cancel_all();
            pump.disconnected(reason);
            break;
        }

        if pending_tx.is_none()
            && !start_outgoing(
                &mut pump,
                &mut sender,
                &mut output,
                &mut packet,
                &mut pending_tx,
                &mut active,
            )
        {
            input.cancel_all();
            output.cancel_all();
            pump.disconnected("USB 请求取消或到期后关闭连接，未自动重发");
            break;
        }
        // 仅在这一轮没有待完成的 OUT 包时有限等待 IN；这避免空闲自旋，且不会
        // 为连续发送增加固定的 1 ms 包间隔。
        if let Err(reason) = idle_receive(&mut input, &output, &mut receiver, &sink, pump.now_ms())
        {
            input.cancel_all();
            output.cancel_all();
            pump.disconnected(reason);
            break;
        }
    }
    let isolated = isolate_endpoints(&mut input, &mut output);
    drop(input);
    drop(output);
    drop(interface);
    for request in active.into_iter().flatten() {
        pump.sink.failed(
            request.id,
            "USB 后端关闭前未取得完整本地提交结果，设备采用与执行结果未知",
            true,
        );
    }
    if isolated {
        match (
            sender.into_reconnect_state(pump.now_ms()),
            receiver.into_reconnect_state(pump.now_ms()),
        ) {
            (Ok(sender), Ok(receiver)) => {
                if let Ok(mut store) = connector.reconnect.lock() {
                    *store = Some(ReconnectState {
                        sender,
                        receiver,
                        epoch: pump.epoch(),
                    });
                } else {
                    connector.reopen_allowed.store(false, Ordering::Release);
                    sink.disconnected("USB 重连状态锁已中毒，不能保留阶段隔离状态");
                }
            }
            _ => {
                connector.reopen_allowed.store(false, Ordering::Release);
                sink.disconnected("USB 传输阶段已耗尽，不能安全重连");
            }
        }
    } else {
        connector.reopen_allowed.store(false, Ordering::Release);
        sink.disconnected("USB 取消后未能收齐端点完成项，不能安全重连");
    }
    connector.active.store(false, Ordering::Release);
}

fn isolate_endpoints(
    input: &mut nusb::Endpoint<Bulk, In>,
    output: &mut nusb::Endpoint<Bulk, Out>,
) -> bool {
    input.cancel_all();
    output.cancel_all();
    let deadline = Instant::now() + IO_ISOLATION_WAIT;
    drain_cancelled_input(input, deadline) && drain_cancelled_output(output, deadline)
}

fn drain_cancelled_input(input: &mut nusb::Endpoint<Bulk, In>, deadline: Instant) -> bool {
    while input.pending() != 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || input.wait_next_complete(remaining).is_none() {
            return false;
        }
    }
    true
}

fn drain_cancelled_output(output: &mut nusb::Endpoint<Bulk, Out>, deadline: Instant) -> bool {
    while output.pending() != 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || output.wait_next_complete(remaining).is_none() {
            return false;
        }
    }
    true
}

fn idle_receive(
    input: &mut nusb::Endpoint<Bulk, In>,
    output: &nusb::Endpoint<Bulk, Out>,
    receiver: &mut transport::usb::Receiver<'_>,
    sink: &EventSink,
    now_ms: u64,
) -> Result<(), String> {
    if output.pending() == 0 {
        drain_received(input, receiver, sink, now_ms, IO_IDLE_WAIT)
    } else {
        Ok(())
    }
}

fn drain_received(
    input: &mut nusb::Endpoint<Bulk, In>,
    receiver: &mut transport::usb::Receiver<'_>,
    sink: &EventSink,
    now_ms: u64,
    wait: Duration,
) -> Result<(), String> {
    let mut wait = wait;
    while let Some(completion) = input.wait_next_complete(wait) {
        wait = Duration::ZERO;
        let nusb::transfer::Completion {
            buffer,
            actual_len,
            status,
        } = completion;
        if let Err(error) = status {
            return Err(format!("USB Bulk IN 失败或断开：{error}"));
        }
        let mut consumed = 0;
        while consumed < actual_len {
            let before = consumed;
            let result = receiver.feed(receiver.phase(), &buffer[consumed..actual_len], now_ms);
            consumed += result.consumed;
            let raw = buffer[before..consumed].to_vec();
            match result.event {
                transport::usb::ReceiveEvent::Complete(message) => {
                    if let Some(bytes) = receiver.message(message.lane) {
                        sink.received(bytes.to_vec());
                    }
                    let _ = receiver.discard_completed(message.lane);
                }
                transport::usb::ReceiveEvent::Rejected(reason) => {
                    sink.transport_error(format!("USB 块不满足绑定规则：{reason:?}"), raw)
                }
                transport::usb::ReceiveEvent::TimedOut(timeouts) => {
                    report_timeouts(sink, timeouts, raw);
                }
                transport::usb::ReceiveEvent::PhaseExhausted => {
                    return Err("USB 传输阶段编号耗尽，不能安全继续接收".to_owned());
                }
                // 一个未闭合 COBS 块、跨 Bulk 包的块或孤立尾片只是当前流的
                // 组装事实；等待后续字节或按绑定丢弃，不能据此认定设备断连。
                transport::usb::ReceiveEvent::NeedMore
                | transport::usb::ReceiveEvent::Ignored
                | transport::usb::ReceiveEvent::StalePhase => {}
            }
            if result.consumed == 0 {
                // TimedOut 会先清除过期状态；用同一字节再次进入 Receiver，使其
                // 依新的定界状态处理，而不是静默丢掉这次 Bulk 完成中的尾字节。
                if matches!(result.event, transport::usb::ReceiveEvent::TimedOut(_)) {
                    continue;
                }
                break;
            }
        }
        input.submit(buffer);
    }
    Ok(())
}

fn report_timeouts(sink: &EventSink, timeouts: transport::usb::PollResult, raw: Vec<u8>) {
    if timeouts.block_timed_out || timeouts.lane_timed_out.iter().any(|timed_out| *timed_out) {
        sink.transport_error(
            format!(
                "USB COBS/分片绝对期限到达：block={}，lane={:?}",
                timeouts.block_timed_out, timeouts.lane_timed_out
            ),
            raw,
        );
    }
}

fn complete_outgoing(
    output: &mut nusb::Endpoint<Bulk, Out>,
    sender: &mut transport::usb::Sender<'_>,
    pending: &mut Option<(transport::usb::TxToken, usize)>,
    active: &mut [Option<ActiveRequest>; 3],
    sink: &EventSink,
    now_ms: u64,
) -> bool {
    if pending.is_none() {
        return true;
    }
    let Some(completion) = output.wait_next_complete(Duration::ZERO) else {
        return true;
    };
    let Some((token, lane)) = pending.take() else {
        return false;
    };
    let id = active[lane].as_ref().map_or(0, |request| request.id);
    if let Err(error) = completion.status {
        sender.failed(token, now_ms);
        sink.failed(id, format!("USB Bulk OUT 失败：{error}"), true);
        return false;
    }
    match sender.completed(token, now_ms) {
        transport::usb::SendEvent::MessageCommitted { .. } => {
            active[lane] = None;
            sink.submitted(id);
        }
        transport::usb::SendEvent::PacketCommitted => {}
        transport::usb::SendEvent::PacketAccepted
        | transport::usb::SendEvent::Ignored
        | transport::usb::SendEvent::Aborted { .. }
        | transport::usb::SendEvent::Failed { .. }
        | transport::usb::SendEvent::PhaseChanged { .. }
        | transport::usb::SendEvent::PhaseExhausted => {
            active[lane] = None;
            sink.failed(id, "USB 发送器完成状态异常，设备采用与执行结果未知", true);
            return false;
        }
    }
    true
}

fn start_outgoing(
    pump: &mut Pump,
    sender: &mut transport::usb::Sender<'_>,
    output: &mut nusb::Endpoint<Bulk, Out>,
    packet: &mut [u8; transport::usb::PACKET_LEN],
    pending: &mut Option<(transport::usb::TxToken, usize)>,
    active: &mut [Option<ActiveRequest>; 3],
) -> bool {
    // 先冻结各空闲 lane 的请求。若 lane-2 的 COBS 块已经开始，heartbeat 或
    // short 仍能排队，但 Sender 会令它们在块结束前返回 StreamBusy；因此绝不
    // 向一个未结束 COBS 块插字节，而在相邻 lane-2 块间可以按 0/1/2 选择。
    for (index, lane) in [
        transport::usb::Lane::Heartbeat,
        transport::usb::Lane::Short,
        transport::usb::Lane::Long,
    ]
    .into_iter()
    .enumerate()
    {
        if active[index].as_ref().is_some_and(ActiveRequest::expired) {
            if let Some(request) = active[index].take() {
                pump.sink.failed(
                    request.id,
                    "USB 本地提交后请求已取消或到期，设备采用与执行结果未知",
                    true,
                );
            }
            return false;
        }
        if active[index].is_none() {
            let Some(request) = pump.take(index) else {
                continue;
            };
            if request.expired() {
                pump.sink.failed(request.id, "发送前已取消或到期", false);
                continue;
            }
            if let Some(buffer) = sender.buffer_mut(lane) {
                buffer[..request.bytes.len()].copy_from_slice(&request.bytes);
            } else {
                pump.put_back(request);
                continue;
            }
            if let Err(error) = sender.begin_buffer(lane, request.bytes.len(), pump.now_ms()) {
                pump.sink.failed(
                    request.id,
                    format!("USB 传输绑定拒绝发送：{error:?}"),
                    false,
                );
                continue;
            }
            active[index] = Some(ActiveRequest {
                id: request.id,
                deadline: request.deadline,
                cancelled: request.cancelled.clone(),
            });
        }
    }

    match prepare_next(sender, packet) {
        Ok(Some((prepared, index))) => {
            output.submit(Buffer::from(&packet[..prepared.length]));
            let _ = sender.accepted(prepared.token);
            *pending = Some((prepared.token, index));
        }
        Ok(None) => {}
        Err(error) => {
            for request in active.iter_mut().filter_map(Option::take) {
                pump.sink.failed(
                    request.id,
                    format!("USB 传输绑定无法准备本地包：{error:?}"),
                    true,
                );
            }
            return false;
        }
    }
    true
}

fn prepare_next(
    sender: &mut transport::usb::Sender<'_>,
    packet: &mut [u8; transport::usb::PACKET_LEN],
) -> Result<Option<(transport::usb::PreparedPacket, usize)>, transport::usb::SendError> {
    for (index, lane) in [
        transport::usb::Lane::Heartbeat,
        transport::usb::Lane::Short,
        transport::usb::Lane::Long,
    ]
    .into_iter()
    .enumerate()
    {
        match sender.prepare_packet(lane, packet) {
            Ok(Some(prepared)) => return Ok(Some((prepared, index))),
            Ok(None) | Err(transport::usb::SendError::StreamBusy) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn active_request_expired(active: &[Option<ActiveRequest>; 3]) -> bool {
    active.iter().flatten().any(ActiveRequest::expired)
}

fn fail_expired_active(active: &mut [Option<ActiveRequest>; 3], sink: &EventSink) {
    for request in active.iter_mut().filter_map(Option::take) {
        sink.failed(
            request.id,
            "USB 传输过程中请求已取消或到期，设备采用与执行结果未知",
            true,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(serial: Option<&str>) -> Device {
        Device {
            serial: serial.map(str::to_owned),
            description: "test".to_owned(),
        }
    }

    #[test]
    fn choose_uses_one_based_snapshot_index() {
        let devices = [device(Some("first")), device(Some("second"))];
        let first = choose(&devices, "1");
        assert!(first.is_ok());
        assert_eq!(
            first.ok().and_then(|device| device.serial),
            Some("first".to_owned())
        );
        assert!(choose(&devices, "0").is_err());
        assert!(choose(&devices, "3").is_err());
    }

    #[test]
    fn persistent_serial_must_be_present_and_unique() {
        assert!(choose(&[device(None)], "1").is_err());
        assert!(choose(&[device(Some("same")), device(Some("same"))], "same").is_err());
        assert!(find_serial(&[device(Some("replacement"))], "1").is_err());
    }

    #[test]
    fn connector_needs_a_persistent_serial_before_any_usb_io() {
        assert!(UsbConnector::new("").is_err());
        assert!(UsbConnector::new("240018000E51343033333232").is_ok());
    }

    #[test]
    fn heartbeat_queued_during_long_block_is_prepared_at_next_block_boundary() {
        let mut tx_heartbeat = [0_u8; 8];
        let mut tx_short = [0_u8; 256];
        let mut tx_long = [0_u8; MAX_MESSAGE_LEN];
        let mut sender = transport::usb::Sender::new(
            protocol::Direction::HostToFirmware,
            &mut tx_heartbeat,
            &mut tx_short,
            &mut tx_long,
        );
        let key_bytes = protocol::OperationKeyFields::new([0; 12], [1; 16], 1).to_bytes();
        let key = protocol::OperationKey::new(&key_bytes).expect("valid test key");
        let record = [b'x'; 300];
        let length = protocol::encode(
            protocol::Message::SaveConfig {
                key,
                record: &record,
            },
            sender
                .buffer_mut(transport::usb::Lane::Long)
                .expect("long lane is idle"),
        )
        .expect("valid test SaveConfig");
        sender
            .begin_buffer(transport::usb::Lane::Long, length, 0)
            .expect("long message starts");

        let mut packet = [0_u8; transport::usb::PACKET_LEN];
        let first = sender
            .prepare_packet(transport::usb::Lane::Long, &mut packet)
            .expect("long packet prepares")
            .expect("long packet exists");
        assert_eq!(
            sender.accepted(first.token),
            transport::usb::SendEvent::PacketAccepted
        );
        assert_eq!(
            sender.completed(first.token, 0),
            transport::usb::SendEvent::PacketCommitted
        );

        let heartbeat_len = protocol::encode(
            protocol::Message::Heartbeat,
            sender
                .buffer_mut(transport::usb::Lane::Heartbeat)
                .expect("heartbeat lane remains available"),
        )
        .expect("heartbeat encodes");
        sender
            .begin_buffer(transport::usb::Lane::Heartbeat, heartbeat_len, 0)
            .expect("heartbeat queues while long COBS block is active");

        let mut saw_heartbeat = false;
        for _ in 0..6 {
            let prepared = prepare_next(&mut sender, &mut packet)
                .expect("scheduler keeps active COBS stream valid")
                .expect("one queued lane has a packet");
            let accepted = sender.accepted(prepared.0.token);
            assert_eq!(accepted, transport::usb::SendEvent::PacketAccepted);
            let event = sender.completed(prepared.0.token, 0);
            if prepared.1 == 0 {
                saw_heartbeat = true;
                break;
            }
            assert_eq!(event, transport::usb::SendEvent::PacketCommitted);
        }
        assert!(
            saw_heartbeat,
            "heartbeat must not wait for the whole long message"
        );
    }

    #[test]
    fn expired_periodic_heartbeat_does_not_close_worker_but_business_requests_do() {
        let expired = Instant::now() - Duration::from_millis(1);
        let future = Instant::now() + Duration::from_secs(1);
        let cancelled = Arc::new(AtomicBool::new(true));

        let periodic = [
            Some(ActiveRequest {
                id: 0,
                deadline: expired,
                cancelled: cancelled.clone(),
            }),
            None,
            None,
        ];
        assert!(!active_request_expired(&periodic));

        let timed_out_business = [
            Some(ActiveRequest {
                id: 1,
                deadline: expired,
                cancelled: Arc::new(AtomicBool::new(false)),
            }),
            None,
            None,
        ];
        assert!(active_request_expired(&timed_out_business));

        let cancelled_business = [
            Some(ActiveRequest {
                id: 2,
                deadline: future,
                cancelled,
            }),
            None,
            None,
        ];
        assert!(active_request_expired(&cancelled_business));
    }
}
