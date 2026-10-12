// Copyright The eha-sdk Contributors

use protocol::{
    Direction, ExpectedIdentity, MAX_MESSAGE_LEN, Message, SampleData, encode,
    responses::{
        ConfigDataFields, ConfigDataWriter, ConfigRecordState, ConfigView, FallbackReason,
        StartupSource,
    },
};
use transport::usb::{Lane, ReceiveEvent, Receiver, RejectReason, SendEvent, Sender};

pub(super) const HEARTBEAT: [u8; 8] = [3, 6, 0, 0, 0xe9, 0x34, 0xf4, 0xfb];
pub(super) const POSITION: [u8; 12] = [3, 1, 4, 0, 0, 0, 0x20, 0x41, 0xe1, 0x53, 0x01, 0x41];

pub(super) fn drain(sender: &mut Sender<'_>, lane: Lane, now_ms: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut packet = [0; 64];
    loop {
        let prepared = sender.prepare_packet(lane, &mut packet);
        assert!(matches!(prepared, Ok(Some(_))));
        let Some(pending) = prepared.ok().flatten() else {
            return bytes;
        };
        bytes.extend_from_slice(&packet[..pending.length]);
        assert_eq!(sender.accepted(pending.token), SendEvent::PacketAccepted);
        let event = sender.completed(pending.token, now_ms);
        if matches!(event, SendEvent::MessageCommitted { .. }) {
            break;
        }
        assert_eq!(event, SendEvent::PacketCommitted);
    }
    bytes
}

pub(super) fn begin(sender: &mut Sender<'_>, lane: Lane, message: &[u8], now_ms: u64) {
    let buffer = sender.buffer_mut(lane).expect("空闲 lane 提供发送缓冲");
    buffer[..message.len()].copy_from_slice(message);
    sender
        .begin_buffer(lane, message.len(), now_ms)
        .expect("固定合同向量可发送");
}

pub(super) fn assert_bad_crc_starts_receiver_guard(lane: Lane, message: &[u8]) {
    let mut tx_heartbeat = [0; 8];
    let mut tx_short = [0; 256];
    let mut tx_large = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(
        Direction::HostToFirmware,
        &mut tx_heartbeat,
        &mut tx_short,
        &mut tx_large,
    );
    begin(&mut sender, lane, message, 0);
    let correct = drain(&mut sender, lane, 0);
    let mut bad_crc = correct.clone();
    let crc_byte = bad_crc.len() - 2;
    bad_crc[crc_byte] ^= 1;

    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut receiver = Receiver::new(
        Direction::HostToFirmware,
        &mut heartbeat,
        &mut short,
        &mut large,
    );
    let phase = receiver.phase();
    assert_eq!(
        receiver.feed(phase, &bad_crc[..1], 0).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &bad_crc[1..], 0).event,
        ReceiveEvent::Rejected(RejectReason::Message)
    );
    assert_eq!(
        receiver.feed(phase, &correct[..1], 1).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &correct[1..], 1).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &correct[..1], 40).event,
        ReceiveEvent::Ignored
    );
    assert!(matches!(
        receiver.feed(phase, &correct[1..], 40).event,
        ReceiveEvent::Complete(done) if done.lane == lane && done.length == message.len()
    ));
}

pub(super) fn maximum_save_and_restart() -> [u8; 16_424] {
    let mut identity_bytes = [0; 28];
    identity_bytes[12] = 1;
    let identity = ExpectedIdentity::new(&identity_bytes).expect("固定期望身份有效");
    let record = [0x5a; 16_384];
    let mut message = [0; 16_424];
    let length = encode(
        Message::SaveAndRestart {
            query_id: 1,
            identity,
            record: &record,
        },
        &mut message,
    )
    .expect("最大主机配置可编码");
    assert_eq!(length, message.len());
    message
}

pub(super) fn maximum_config_data(output: &mut [u8; MAX_MESSAGE_LEN]) -> usize {
    let fields = ConfigDataFields {
        sample: SampleData {
            query_id: 7,
            run_nonce: [1; 16],
            snapshot_sequence: 2,
            snapshot_time_us: 3,
        },
        view: ConfigView::UserRecord,
        record_state: ConfigRecordState::Complete,
        startup_source: StartupSource::User,
        fallback_reason: FallbackReason::None,
        data_time_us: 4,
    };
    let mut writer = ConfigDataWriter::start(fields, 16_384, output).expect("测试不变量");
    for (index, byte) in writer.data_mut().iter_mut().enumerate() {
        *byte = index as u8;
    }
    writer.finish().expect("测试不变量")
}

pub(super) fn first_block_end(stream: &[u8]) -> usize {
    stream[1..]
        .iter()
        .position(|byte| *byte == 0)
        .expect("发送器输出完整首块")
        + 1
}
