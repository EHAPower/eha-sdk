// Copyright The eha-sdk Contributors

use protocol::{Direction, Message, OperationKey, encode};

use super::cobs::{CobsCursor, cobs_decode, cobs_decode_in_place, cobs_encode, next_cobs_byte};
use super::{
    Lane, MAX_COBS_LEN, Phase, PhaseError, ReceiveEvent, ReceivedMessage, Receiver, RejectReason,
    SendError, Sender,
};

#[test]
fn cobs_contract_vector_round_trips() {
    let raw = [1, 2, 0, 0, 0x11, 0];
    let expected = [3, 1, 2, 1, 2, 0x11, 1];
    let mut encoded = [0; MAX_COBS_LEN];
    let len = cobs_encode(&raw, &mut encoded).expect("固定 COBS 向量可编码");
    assert_eq!(&encoded[..len], expected);
    let mut decoded = [0; 260];
    assert_eq!(cobs_decode(&expected, &mut decoded), Ok(raw.len()));
    assert_eq!(&decoded[..raw.len()], raw);
    let mut in_place = expected;
    assert_eq!(
        cobs_decode_in_place(&mut in_place, expected.len()),
        Ok(raw.len())
    );
    assert_eq!(&in_place[..raw.len()], raw);
}

#[test]
fn streaming_cobs_matches_buffered_at_254_byte_boundaries() {
    for fragment_len in [250usize, 251, 256] {
        let fragment = [0x5a; 256];
        let mut raw = [0; 260];
        raw[..4].copy_from_slice(&[2, 1, 1, 0]);
        raw[4..4 + fragment_len].copy_from_slice(&fragment[..fragment_len]);
        let mut encoded = [0; MAX_COBS_LEN];
        let encoded_len =
            cobs_encode(&raw[..4 + fragment_len], &mut encoded).expect("边界 COBS 向量可编码");

        let mut cursor = CobsCursor::StartDelimiter;
        let mut streamed = [0; 264];
        let mut streamed_len = 0;
        while let Some(byte) =
            next_cobs_byte(&mut cursor, Lane::Long, 1, 1, &fragment[..fragment_len])
        {
            streamed[streamed_len] = byte;
            streamed_len += 1;
        }
        assert_eq!(streamed[0], 0);
        assert_eq!(streamed[streamed_len - 1], 0);
        assert_eq!(&streamed[1..streamed_len - 1], &encoded[..encoded_len]);
    }
}

#[test]
fn phase_never_wraps_to_reuse_old_input_or_completion_identity() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut long = [0; 16_440];
    let mut sender = Sender::new(
        Direction::HostToFirmware,
        &mut heartbeat,
        &mut short,
        &mut long,
    );
    sender.phase = Phase(u64::MAX);
    assert_eq!(sender.phase_changed(0), Err(PhaseError::Exhausted));
    assert_eq!(
        sender.begin_buffer(Lane::Short, 0, 0),
        Err(SendError::PhaseExhausted)
    );

    let mut rx_heartbeat = [0; 8];
    let mut rx_short = [0; 256];
    let mut rx_long = [0; 16_440];
    let mut receiver = Receiver::new(
        Direction::HostToFirmware,
        &mut rx_heartbeat,
        &mut rx_short,
        &mut rx_long,
    );
    receiver.phase = Phase(u64::MAX);
    assert_eq!(receiver.phase_changed(0), Err(PhaseError::Exhausted));
    assert_eq!(
        receiver.feed(receiver.phase(), &[0], 0).event,
        ReceiveEvent::PhaseExhausted
    );
}

#[test]
fn long_first_fragment_replacement_validates_before_abandoning() {
    let original = long_config_message(0x41);
    let replacement = long_config_message(0x42);
    let complete = ReceiveEvent::Complete(ReceivedMessage {
        lane: Lane::Long,
        length: 344,
    });
    for (transfer, first_len, first_event, completed_transfer) in [
        (
            11,
            8,
            ReceiveEvent::Rejected(RejectReason::Fragment),
            Some(10),
        ),
        (
            11,
            255,
            ReceiveEvent::Rejected(RejectReason::Fragment),
            Some(10),
        ),
        (11, 256, ReceiveEvent::NeedMore, Some(11)),
        (
            10,
            256,
            ReceiveEvent::Rejected(RejectReason::Fragment),
            None,
        ),
    ] {
        let mut heartbeat = [0; 8];
        let mut short = [0; 256];
        let mut long = [0; 344];
        let mut receiver = Receiver::new(
            Direction::HostToFirmware,
            &mut heartbeat,
            &mut short,
            &mut long,
        );
        assert_eq!(
            feed_long_fragment(&mut receiver, 10, 0, &original[..256], 0),
            ReceiveEvent::NeedMore
        );
        assert_eq!(
            feed_long_fragment(&mut receiver, transfer, 0, &replacement[..first_len], 1),
            first_event
        );
        assert_eq!(
            feed_long_fragment(&mut receiver, 10, 1, &original[256..], 2),
            if completed_transfer == Some(10) {
                complete
            } else {
                ReceiveEvent::Ignored
            },
            "新 transfer={transfer}、首片长度={first_len} 时原消息的结果"
        );
        if completed_transfer == Some(11) {
            assert_eq!(
                feed_long_fragment(&mut receiver, 11, 1, &replacement[256..], 3),
                complete
            );
        }
        assert_eq!(
            receiver.message(Lane::Long),
            completed_transfer.map(|completed| if completed == 10 {
                original.as_slice()
            } else {
                replacement.as_slice()
            })
        );
    }
}

#[test]
fn invalid_new_first_fragment_preserves_original_absolute_deadline() {
    let message = long_config_message(0x41);
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut long = [0; 344];
    let mut receiver = Receiver::new(
        Direction::HostToFirmware,
        &mut heartbeat,
        &mut short,
        &mut long,
    );
    assert_eq!(
        feed_long_fragment(&mut receiver, 10, 0, &message[..256], 0),
        ReceiveEvent::NeedMore
    );
    assert_eq!(
        feed_long_fragment(&mut receiver, 11, 0, &message[..8], 29_999),
        ReceiveEvent::Rejected(RejectReason::Fragment)
    );
    assert!(!receiver.poll(29_999).lane_timed_out[Lane::Long as usize]);
    assert!(receiver.poll(30_000).lane_timed_out[Lane::Long as usize]);
    assert_eq!(
        feed_long_fragment(&mut receiver, 10, 1, &message[256..], 30_001),
        ReceiveEvent::Ignored
    );
}

fn long_config_message(record_byte: u8) -> [u8; 344] {
    let key_bytes = [1; 36];
    let key = OperationKey::new(&key_bytes).expect("固定维护键有效");
    let record = [record_byte; 300];
    let mut message = [0; 344];
    let length = encode(
        Message::SaveConfig {
            key,
            record: &record,
        },
        &mut message,
    )
    .expect("双片配置消息可编码");
    assert_eq!(length, message.len());
    message
}

fn feed_long_fragment(
    receiver: &mut Receiver<'_>,
    transfer: u8,
    index: u16,
    fragment: &[u8],
    now_ms: u64,
) -> ReceiveEvent {
    let mut raw = [0; 260];
    raw[..2].copy_from_slice(&[2, transfer]);
    raw[2..4].copy_from_slice(&index.to_le_bytes());
    raw[4..4 + fragment.len()].copy_from_slice(fragment);
    let mut block = [0; 264];
    let length = cobs_encode(&raw[..4 + fragment.len()], &mut block[1..263])
        .expect("含首尾分隔符的完整 COBS 块可编码");
    let phase = receiver.phase();
    assert_eq!(
        receiver.feed(phase, &block[..1], now_ms).event,
        ReceiveEvent::Ignored
    );
    let result = receiver.feed(phase, &block[1..length + 2], now_ms);
    assert_eq!(result.consumed, length + 1);
    result.event
}
