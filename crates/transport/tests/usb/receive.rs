// Copyright The eha-sdk Contributors

use protocol::{Direction, MAX_MESSAGE_LEN};
use transport::usb::*;

use super::common::*;

#[test]
fn contract_heartbeat_block_has_the_independent_golden_bytes() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(
        Direction::HostToFirmware,
        &mut heartbeat,
        &mut short,
        &mut large,
    );
    begin(&mut sender, Lane::Heartbeat, &HEARTBEAT, 0);
    let _ = drain(&mut sender, Lane::Heartbeat, 0);
    begin(&mut sender, Lane::Heartbeat, &HEARTBEAT, 40);
    let bytes = drain(&mut sender, Lane::Heartbeat, 40);
    assert_eq!(
        bytes,
        [0, 1, 2, 1, 1, 3, 1, 6, 1, 5, 0x68, 0x17, 0x93, 0x44, 0],
    );
}

#[test]
fn validated_completion_transfers_the_original_buffer_without_revalidating() {
    let mut tx_heartbeat = [0; 8];
    let mut tx_short = [0; 256];
    let mut tx_large = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(
        Direction::HostToFirmware,
        &mut tx_heartbeat,
        &mut tx_short,
        &mut tx_large,
    );
    begin(&mut sender, Lane::Short, &POSITION, 0);
    let stream = drain(&mut sender, Lane::Short, 0);

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
    assert!(matches!(
        receiver.feed_validated(phase, &stream[..1], 0).event,
        ValidatedReceiveEvent::Ignored
    ));
    let result = receiver.feed_validated(phase, &stream[1..], 0);
    assert!(matches!(
        result.event,
        ValidatedReceiveEvent::Complete { .. }
    ));
    let ValidatedReceiveEvent::Complete { lane, message } = result.event else {
        return;
    };
    assert_eq!(lane, Lane::Short);
    assert_eq!(message.bytes(), POSITION);
    assert!(matches!(message.decode(), Ok(protocol::Message::Position(v)) if v == 10.0));
    assert_eq!(receiver.message(lane), None);
    assert_eq!(receiver.phase_changed(1), Err(PhaseError::BufferBusy));
    assert!(receiver.restore_buffer(lane, message.into_buffer()));
    assert!(receiver.phase_changed(1).is_ok());
}

#[test]
fn heartbeat_and_short_crc_failures_guard_their_transfer_numbers() {
    assert_bad_crc_starts_receiver_guard(Lane::Heartbeat, &HEARTBEAT);
    assert_bad_crc_starts_receiver_guard(Lane::Short, &POSITION);
}

#[test]
fn arbitrary_read_boundaries_and_two_messages_preserve_remaining_bytes() {
    let mut heartbeat_tx_buffer = [0; 8];
    let mut heartbeat_tx_short = [0; 256];
    let mut heartbeat_tx_large = [0; MAX_MESSAGE_LEN];
    let mut heartbeat_tx = Sender::new(
        Direction::HostToFirmware,
        &mut heartbeat_tx_buffer,
        &mut heartbeat_tx_short,
        &mut heartbeat_tx_large,
    );
    begin(&mut heartbeat_tx, Lane::Heartbeat, &HEARTBEAT, 0);
    let heartbeat_block = drain(&mut heartbeat_tx, Lane::Heartbeat, 0);
    let mut position_tx_heartbeat = [0; 8];
    let mut position_tx_buffer = [0; 256];
    let mut position_tx_large = [0; MAX_MESSAGE_LEN];
    let mut position_tx = Sender::new(
        Direction::HostToFirmware,
        &mut position_tx_heartbeat,
        &mut position_tx_buffer,
        &mut position_tx_large,
    );
    begin(&mut position_tx, Lane::Short, &POSITION, 0);
    let position_block = drain(&mut position_tx, Lane::Short, 0);
    let mut stream = heartbeat_block;
    stream.extend_from_slice(&position_block);

    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut heartbeat_replacement = [0; 8];
    let mut short_replacement = [0; 256];
    let mut receiver = Receiver::new(
        Direction::HostToFirmware,
        &mut heartbeat,
        &mut short,
        &mut large,
    );
    let phase = receiver.phase();

    let separator = receiver.feed(phase, &stream[..1], 0);
    assert_eq!(separator.event, ReceiveEvent::Ignored);
    let first = receiver.feed(phase, &stream[1..3], 0);
    assert_eq!(first.event, ReceiveEvent::NeedMore);
    let heartbeat_done = receiver.feed(phase, &stream[3..], 0);
    assert!(
        matches!(heartbeat_done.event, ReceiveEvent::Complete(message) if message.lane == Lane::Heartbeat && message.length == 8)
    );
    assert_eq!(
        receiver.message(Lane::Heartbeat),
        Some(HEARTBEAT.as_slice())
    );
    let returned = receiver.replace_buffer(Lane::Heartbeat, &mut heartbeat_replacement);
    assert!(matches!(returned, Some(buffer) if buffer[..8] == HEARTBEAT));

    let rest = &stream[3 + heartbeat_done.consumed..];
    let position_separator = receiver.feed(phase, &rest[..1], 0);
    assert_eq!(position_separator.event, ReceiveEvent::Ignored);
    let position_done = receiver.feed(phase, &rest[1..], 0);
    assert!(
        matches!(position_done.event, ReceiveEvent::Complete(message) if message.lane == Lane::Short && message.length == 12)
    );
    assert_eq!(receiver.message(Lane::Short), Some(POSITION.as_slice()));
    let returned = receiver.replace_buffer(Lane::Short, &mut short_replacement);
    assert!(matches!(returned, Some(buffer) if buffer[..12] == POSITION));
}

#[test]
fn malformed_end_delimiter_recovers_at_that_same_delimiter() {
    let mut tx_heartbeat = [0; 8];
    let mut tx_short = [0; 256];
    let mut tx_large = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(
        Direction::HostToFirmware,
        &mut tx_heartbeat,
        &mut tx_short,
        &mut tx_large,
    );
    begin(&mut sender, Lane::Heartbeat, &HEARTBEAT, 0);
    let valid = drain(&mut sender, Lane::Heartbeat, 0);
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

    let malformed = [0, 5, 1, 0];
    assert_eq!(
        receiver.feed(phase, &malformed[..1], 0).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &malformed[1..], 0).event,
        ReceiveEvent::Rejected(RejectReason::Cobs)
    );
    assert_eq!(
        receiver.feed(phase, &valid[..1], 0).event,
        ReceiveEvent::Ignored
    );
    assert!(matches!(
        receiver.feed(phase, &valid[1..], 0).event,
        ReceiveEvent::Complete(message) if message.lane == Lane::Heartbeat
    ));
}

#[test]
fn deadlines_are_absolute_without_new_bytes() {
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
    assert_eq!(receiver.feed(phase, &[0], 100).event, ReceiveEvent::Ignored);
    assert_eq!(
        receiver.feed(phase, &[1], 100).event,
        ReceiveEvent::NeedMore
    );
    assert!(!receiver.poll(119).block_timed_out);
    assert!(receiver.poll(120).block_timed_out);
}
