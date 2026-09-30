// Copyright The eha_controller Contributors

use protocol::{Direction, MAX_MESSAGE_LEN, crc32c};
use transport::usb::*;

use super::common::*;

#[test]
fn maximum_message_uses_65_long_blocks_without_copying_partial_ranges() {
    let mut message = [0; MAX_MESSAGE_LEN];
    message[..4].copy_from_slice(&[1, 0x94, 0x30, 0x40]);
    message[4..8].copy_from_slice(&1_u32.to_le_bytes());
    message[40..44].copy_from_slice(&16_384_u32.to_le_bytes());
    let crc = crc32c(&message[..MAX_MESSAGE_LEN - 4]);
    message[MAX_MESSAGE_LEN - 4..].copy_from_slice(&crc.to_le_bytes());

    let mut tx_heartbeat = [0; 8];
    let mut tx_short = [0; 256];
    let mut tx_large = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(
        Direction::FirmwareToHost,
        &mut tx_heartbeat,
        &mut tx_short,
        &mut tx_large,
    );
    sender
        .buffer_mut(Lane::Long)
        .expect("空闲大消息缓冲")
        .copy_from_slice(&message);
    sender
        .begin_buffer(Lane::Long, MAX_MESSAGE_LEN, 0)
        .expect("最大记录可发送");
    let stream = drain(&mut sender, Lane::Long, 0);
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut receiver = Receiver::new(
        Direction::FirmwareToHost,
        &mut heartbeat,
        &mut short,
        &mut large,
    );
    let phase = receiver.phase();
    let mut offset = 0;
    loop {
        let result = receiver.feed(phase, &stream[offset..], 0);
        offset += result.consumed;
        if result.event == ReceiveEvent::Ignored {
            continue;
        }
        if matches!(result.event, ReceiveEvent::Complete(message) if message.lane == Lane::Long) {
            break;
        }
        assert_eq!(result.event, ReceiveEvent::NeedMore);
    }
    assert_eq!(receiver.message(Lane::Long), Some(message.as_slice()));
}

#[test]
fn discard_completed_reuses_the_completed_long_lane_and_keeps_its_guard() {
    let message = maximum_host_config();
    let mut tx_heartbeat = [0; 8];
    let mut tx_short = [0; 256];
    let mut tx_large = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(
        Direction::HostToFirmware,
        &mut tx_heartbeat,
        &mut tx_short,
        &mut tx_large,
    );
    begin(&mut sender, Lane::Long, &message, 0);
    let first = drain(&mut sender, Lane::Long, 0);
    begin(&mut sender, Lane::Long, &message, 1);
    let next = drain(&mut sender, Lane::Long, 1);

    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut receiver = Receiver::new(
        Direction::HostToFirmware,
        &mut heartbeat,
        &mut short,
        &mut large,
    );
    assert_eq!(feed_complete(&mut receiver, &first, 0).lane, Lane::Long);
    assert_eq!(receiver.message(Lane::Long), Some(message.as_slice()));
    assert!(receiver.discard_completed(Lane::Long));
    assert_eq!(receiver.message(Lane::Long), None);

    let phase = receiver.phase();
    assert_eq!(
        receiver.feed(phase, &first[..1], 1).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &first[1..], 1).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(feed_complete(&mut receiver, &next, 2).lane, Lane::Long);
}

#[test]
fn discard_completed_does_not_abandon_an_assembling_long_lane() {
    let message = maximum_host_config();
    let mut tx_heartbeat = [0; 8];
    let mut tx_short = [0; 256];
    let mut tx_large = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(
        Direction::HostToFirmware,
        &mut tx_heartbeat,
        &mut tx_short,
        &mut tx_large,
    );
    begin(&mut sender, Lane::Long, &message, 0);
    let stream = drain(&mut sender, Lane::Long, 0);
    let first_end = first_block_end(&stream);

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
        receiver.feed(phase, &stream[..1], 0).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &stream[1..first_end + 1], 0).event,
        ReceiveEvent::NeedMore
    );
    assert!(!receiver.discard_completed(Lane::Long));

    let mut offset = first_end + 1;
    loop {
        let result = receiver.feed(phase, &stream[offset..], 0);
        offset += result.consumed;
        if matches!(result.event, ReceiveEvent::Complete(done) if done.lane == Lane::Long) {
            break;
        }
        assert!(matches!(
            result.event,
            ReceiveEvent::Ignored | ReceiveEvent::NeedMore
        ));
        assert!(offset < stream.len(), "stream ended before completion");
    }
    assert_eq!(receiver.message(Lane::Long), Some(message.as_slice()));
}

#[test]
fn long_transfer_allows_heartbeat_and_short_blocks_between_its_fragments() {
    let long_message = maximum_host_config();
    let mut long_h = [0; 8];
    let mut long_s = [0; 256];
    let mut long_l = [0; MAX_MESSAGE_LEN];
    let mut long_tx = Sender::new(
        Direction::HostToFirmware,
        &mut long_h,
        &mut long_s,
        &mut long_l,
    );
    begin(&mut long_tx, Lane::Long, &long_message, 0);
    let long_stream = drain(&mut long_tx, Lane::Long, 0);
    let end = first_block_end(&long_stream);

    let mut heart_h = [0; 8];
    let mut heart_s = [0; 256];
    let mut heart_l = [0; MAX_MESSAGE_LEN];
    let mut heart_tx = Sender::new(
        Direction::HostToFirmware,
        &mut heart_h,
        &mut heart_s,
        &mut heart_l,
    );
    begin(&mut heart_tx, Lane::Heartbeat, &HEARTBEAT, 0);
    let heartbeat_block = drain(&mut heart_tx, Lane::Heartbeat, 0);
    let mut short_h = [0; 8];
    let mut short_s = [0; 256];
    let mut short_l = [0; MAX_MESSAGE_LEN];
    let mut short_tx = Sender::new(
        Direction::HostToFirmware,
        &mut short_h,
        &mut short_s,
        &mut short_l,
    );
    begin(&mut short_tx, Lane::Short, &POSITION, 0);
    let position_block = drain(&mut short_tx, Lane::Short, 0);

    let mut stream = Vec::new();
    stream.extend_from_slice(&long_stream[..end + 1]);
    stream.extend_from_slice(&heartbeat_block);
    stream.extend_from_slice(&position_block);
    stream.extend_from_slice(&long_stream[end + 1..]);

    let mut rx_h = [0; 8];
    let mut rx_s = [0; 256];
    let mut rx_l = [0; MAX_MESSAGE_LEN];
    let mut receiver = Receiver::new(Direction::HostToFirmware, &mut rx_h, &mut rx_s, &mut rx_l);
    let phase = receiver.phase();
    let mut offset = 0;
    let mut saw_heartbeat = false;
    let mut saw_short = false;
    loop {
        let result = receiver.feed(phase, &stream[offset..], 0);
        offset += result.consumed;
        match result.event {
            ReceiveEvent::Complete(done) if done.lane == Lane::Heartbeat => {
                saw_heartbeat = receiver.message(Lane::Heartbeat) == Some(HEARTBEAT.as_slice());
            }
            ReceiveEvent::Complete(done) if done.lane == Lane::Short => {
                saw_short = receiver.message(Lane::Short) == Some(POSITION.as_slice());
            }
            ReceiveEvent::Complete(done) if done.lane == Lane::Long => break,
            ReceiveEvent::Ignored | ReceiveEvent::NeedMore => {}
            event => assert!(matches!(
                event,
                ReceiveEvent::Ignored | ReceiveEvent::NeedMore
            )),
        }
    }
    assert!(saw_heartbeat && saw_short);
    assert_eq!(receiver.message(Lane::Long), Some(long_message.as_slice()));
}

#[test]
fn long_deadline_is_absolute_and_phase_cleanup_keeps_its_guard() {
    let message = maximum_host_config();
    let mut tx_h = [0; 8];
    let mut tx_s = [0; 256];
    let mut tx_l = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(Direction::HostToFirmware, &mut tx_h, &mut tx_s, &mut tx_l);
    begin(&mut sender, Lane::Long, &message, 0);
    let stream = drain(&mut sender, Lane::Long, 0);
    let end = first_block_end(&stream);
    let first = &stream[..end + 1];

    let mut rx_h = [0; 8];
    let mut rx_s = [0; 256];
    let mut rx_l = [0; MAX_MESSAGE_LEN];
    let mut receiver = Receiver::new(Direction::HostToFirmware, &mut rx_h, &mut rx_s, &mut rx_l);
    let phase = receiver.phase();
    assert_eq!(
        receiver.feed(phase, &first[..1], 0).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &first[1..], 0).event,
        ReceiveEvent::NeedMore
    );
    assert!(!receiver.poll(29_999).lane_timed_out[Lane::Long as usize]);
    assert!(receiver.poll(30_000).lane_timed_out[Lane::Long as usize]);

    assert_eq!(
        receiver.feed(phase, &first[..1], 30_001).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &first[1..], 30_001).event,
        ReceiveEvent::Ignored
    );
    let mut phase_h = [0; 8];
    let mut phase_s = [0; 256];
    let mut phase_l = [0; MAX_MESSAGE_LEN];
    let mut phase_receiver = Receiver::new(
        Direction::HostToFirmware,
        &mut phase_h,
        &mut phase_s,
        &mut phase_l,
    );
    let old_phase = phase_receiver.phase();
    assert_eq!(
        phase_receiver.feed(old_phase, &first[..1], 0).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        phase_receiver.feed(old_phase, &first[1..], 0).event,
        ReceiveEvent::NeedMore
    );
    let new_phase = phase_receiver.phase_changed(1).expect("阶段可推进");
    assert_eq!(
        phase_receiver.feed(old_phase, first, 2).event,
        ReceiveEvent::StalePhase
    );
    assert_eq!(
        phase_receiver.feed(new_phase, &first[..1], 2).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        phase_receiver.feed(new_phase, &first[1..], 2).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        phase_receiver.feed(new_phase, &first[..1], 60_001).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        phase_receiver.feed(new_phase, &first[1..], 60_001).event,
        ReceiveEvent::NeedMore
    );
}

fn feed_complete(receiver: &mut Receiver<'_>, stream: &[u8], now_ms: u64) -> ReceivedMessage {
    let phase = receiver.phase();
    let mut offset = 0;
    loop {
        let result = receiver.feed(phase, &stream[offset..], now_ms);
        offset += result.consumed;
        if let ReceiveEvent::Complete(done) = result.event {
            assert_eq!(offset, stream.len());
            return done;
        }
        assert!(
            matches!(result.event, ReceiveEvent::Ignored | ReceiveEvent::NeedMore),
            "unexpected receive event: {:?}",
            result.event
        );
        assert!(offset < stream.len(), "stream ended before completion");
    }
}
