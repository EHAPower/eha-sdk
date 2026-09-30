// Copyright The eha_controller Contributors

//! 仅在宿主内存中运行：同一公共字节经 Classic、FD、USB 恢复。
//! 本例的“本地提交”由软件管道推进，不提供真实驱动或设备证据。

use protocol::{Direction, MAX_MESSAGE_LEN, Message, OperationKey, decode, encode};
use transport::{can, usb};

fn through_can(bytes: &[u8], direction: Direction, mode: can::Mode) -> Result<usize, String> {
    let (mut th, mut ts, mut tl) = ([0; 8], [0; 256], [0; MAX_MESSAGE_LEN]);
    let (mut rh, mut rs, mut rl) = ([0; 8], [0; 256], [0; MAX_MESSAGE_LEN]);
    let mut sender = can::Transmitter::new(
        1,
        direction,
        mode,
        0,
        can::TxBuffers {
            heartbeat: &mut th,
            short: &mut ts,
            large: &mut tl,
        },
    )
    .expect("合法逻辑节点和三个有界发送缓冲");
    let mut receiver = can::Receiver::new(
        1,
        direction,
        mode,
        0,
        can::RxBuffers {
            heartbeat: &mut rh,
            short: &mut rs,
            large: &mut rl,
        },
    )
    .expect("三个有界接收缓冲");

    // 此处显式复制是为了比较同一份已编码向量；日常发布可在buffer_mut直接编码。
    let lane = sender.copy_and_start(bytes, 0).expect("完整合法输入");
    let mut frames = 0;
    let mut delivered = false;
    let mut committed = false;
    while let Some(pending) = sender.next_frame(lane) {
        frames += 1;
        match receiver.receive(&pending.frame, 0, 0) {
            can::ReceiveResult::Incomplete { .. } => {}
            can::ReceiveResult::Complete { lane, .. } => {
                let recovered = receiver.message(lane).expect("完整接收借用");
                assert_eq!(recovered, bytes);
                assert_eq!(decode(recovered, direction), decode(bytes, direction));
                delivered = true;
            }
            event => return Err(format!("合法软件向量意外结果: {event:?}")),
        }
        committed = matches!(
            sender.complete(pending.token, can::SubmitResult::Accepted, 0),
            Ok(can::SubmitEvent::MessageCommitted { .. })
        );
    }
    assert!(delivered && committed);

    // 接收借用已结束，可以把缓冲所有权交给上层，再把空闲缓冲放回接收通道。
    // 此处只读取；固件侧的非心跳请求仍须更新共同最新缓存后再由业务所有者取走。
    let complete = receiver.take_completed(lane).expect("唯一取走完整消息");
    assert_eq!(complete.as_bytes(), bytes);
    receiver
        .replace_buffer(lane, complete.into_buffer())
        .expect("原缓冲可复用");
    assert!(sender.buffer_mut(lane).is_ok());
    assert_eq!(receiver.poll(30_000), 0); // 没有新数据时照常推进期限。
    sender.reconfigure(1, mode, 1, 30_000).expect("新通信世代");
    receiver
        .reconfigure(1, mode, 1, 30_000)
        .expect("同一新世代");
    Ok(frames)
}

fn through_usb(bytes: &[u8], direction: Direction) -> Result<usize, String> {
    let (mut th, mut ts, mut tl) = ([0; 8], [0; 256], [0; MAX_MESSAGE_LEN]);
    let (mut rh, mut rs, mut rl) = ([0; 8], [0; 256], [0; MAX_MESSAGE_LEN]);
    let mut sender = usb::Sender::new(direction, &mut th, &mut ts, &mut tl);
    let mut receiver = usb::Receiver::new(direction, &mut rh, &mut rs, &mut rl);
    let lane = if bytes[1] == 6 {
        usb::Lane::Heartbeat
    } else if bytes.len() <= 256 {
        usb::Lane::Short
    } else {
        usb::Lane::Long
    };
    sender.buffer_mut(lane).expect("空闲发送缓冲")[..bytes.len()].copy_from_slice(bytes);
    sender
        .begin_buffer(lane, bytes.len(), 0)
        .expect("冻结完整发送副本");
    let mut packet = [0; usb::PACKET_LEN];
    let mut packets = 0;
    let mut delivered = false;
    let mut committed = false;
    while let Some(prepared) = sender.prepare_packet(lane, &mut packet).expect("逐包生成") {
        packets += 1;
        // 刻意把每个包再次切成7字节读取，不能由一次read或短包推定消息完成。
        for read in packet[..prepared.length].chunks(7) {
            let mut consumed = 0;
            while consumed < read.len() {
                let feed = receiver.feed(receiver.phase(), &read[consumed..], 0);
                assert!(feed.consumed > 0);
                consumed += feed.consumed;
                match feed.event {
                    usb::ReceiveEvent::Complete(message) => {
                        let recovered = receiver.message(message.lane).expect("完整接收借用");
                        assert_eq!(recovered, bytes);
                        assert_eq!(decode(recovered, direction), decode(bytes, direction));
                        delivered = true;
                    }
                    usb::ReceiveEvent::NeedMore | usb::ReceiveEvent::Ignored => {}
                    event => return Err(format!("合法软件向量意外结果: {event:?}")),
                }
            }
        }
        // 实际EndpointIn::write返回成功时才能报告本地接纳和调用返回；本例没有I/O。
        assert_eq!(
            sender.accepted(prepared.token),
            usb::SendEvent::PacketAccepted
        );
        committed = matches!(
            sender.completed(prepared.token, 0),
            usb::SendEvent::MessageCommitted { .. }
        );
    }
    assert!(delivered && committed);
    assert!(sender.buffer_mut(lane).is_some());
    assert_eq!(receiver.poll(30_000), usb::PollResult::default());
    // 真实调用先结束/隔离旧I/O，再通知双方阶段变化；业务结果不在绑定中清理。
    sender.phase_changed(30_000).expect("发送进入新端点阶段");
    receiver.phase_changed(30_000).expect("接收进入新端点阶段");
    Ok(packets)
}

fn compare(name: &str, bytes: &[u8], direction: Direction) {
    let classic = through_can(bytes, direction, can::Mode::Classic).expect("Classic 恢复完整消息");
    let fd = through_can(bytes, direction, can::Mode::Fd).expect("FD 恢复完整消息");
    let usb = through_usb(bytes, direction).expect("USB 恢复完整消息");
    println!(
        "{name}: {} B，Classic {classic} 帧，FD {fd} 帧，USB {usb} 包；完整字节和解码结果相同",
        bytes.len()
    );
}

fn main() {
    let mut wire = [0; MAX_MESSAGE_LEN];
    for (name, message) in [
        ("位置目标", Message::Position(10.0)),
        (
            "阻抗目标",
            Message::Impedance {
                equilibrium_mm: 50.0,
                stiffness_n_per_mm: 10.0,
                damping_ns_per_mm: 2.0,
            },
        ),
        ("停止", Message::Stop),
        ("显式心跳", Message::Heartbeat),
        (
            "读取实际用户配置",
            Message::ReadConfig {
                query_id: 8,
                view: 1,
            },
        ),
    ] {
        let n = encode(message, &mut wire).expect("完整具名输入");
        compare(name, &wire[..n], Direction::HostToFirmware);
    }
    let key_bytes = [1; 36]; // 仅离线向量，实际键必须来自已核对设备和当前应用会话。
    let key = OperationKey::new(&key_bytes).expect("完整键");
    // 只验证透明传输，不构造设备配置或读取产品出厂参数。
    let record = br#"{"example":"transport-only"}"#;
    let n = encode(Message::SaveConfig { key, record }, &mut wire).expect("完整原始记录");
    compare("保存请求软件向量", &wire[..n], Direction::HostToFirmware);
    let n = encode(Message::ReadResult { query_id: 9, key }, &mut wire).expect("只读结果查询");
    compare("查询维护结果", &wire[..n], Direction::HostToFirmware);

    use protocol::{
        SampleData,
        responses::{
            ConfigDataFields, ConfigDataWriter, ConfigRecordState, ConfigView, FallbackReason,
            StartupSource,
        },
    };
    let fields = ConfigDataFields {
        sample: SampleData {
            query_id: 8,
            run_nonce: [1; 16],
            snapshot_sequence: 1,
            snapshot_time_us: 1000,
        },
        view: ConfigView::UserRecord,
        record_state: ConfigRecordState::Incomplete,
        startup_source: StartupSource::Factory,
        fallback_reason: FallbackReason::ParseFailed,
        data_time_us: 900,
    };
    // 模拟已读到的原始记录字节。实际使用时readback直接写这个最终TX窗口；不重新序列化。
    let mut writer = ConfigDataWriter::start(fields, 16_384, &mut wire).expect("最大实际读回窗口");
    writer.data_mut().fill(0xfe);
    let n = writer.finish().expect("残缺及无效UTF-8仍如实编码");
    compare("最大实际读回", &wire[..n], Direction::FirmwareToHost);
}
