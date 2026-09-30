// Copyright The eha_controller Contributors

use protocol::{Direction, MAX_MESSAGE_LEN};
use transport::usb::*;

use super::common::*;

#[test]
fn undersized_packet_and_receive_buffers_never_advance_or_deliver() {
    let mut tx_h = [0; 8];
    let mut tx_s = [0; 256];
    let mut tx_l = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(Direction::HostToFirmware, &mut tx_h, &mut tx_s, &mut tx_l);
    begin(&mut sender, Lane::Short, &POSITION, 0);
    let mut too_small_packet = [0; 63];
    assert_eq!(
        sender.prepare_packet(Lane::Short, &mut too_small_packet),
        Err(SendError::PacketBufferTooSmall)
    );
    let mut packet = [0; 64];
    assert!(matches!(
        sender.prepare_packet(Lane::Short, &mut packet),
        Ok(Some(_))
    ));

    let mut valid_h = [0; 8];
    let mut valid_s = [0; 256];
    let mut valid_l = [0; MAX_MESSAGE_LEN];
    let mut valid_sender = Sender::new(
        Direction::HostToFirmware,
        &mut valid_h,
        &mut valid_s,
        &mut valid_l,
    );
    begin(&mut valid_sender, Lane::Short, &POSITION, 0);
    let stream = drain(&mut valid_sender, Lane::Short, 0);
    let mut rx_h = [0; 8];
    let mut rx_s = [0; 11];
    let mut rx_l = [0; MAX_MESSAGE_LEN];
    let mut receiver = Receiver::new(Direction::HostToFirmware, &mut rx_h, &mut rx_s, &mut rx_l);
    let phase = receiver.phase();
    assert_eq!(
        receiver.feed(phase, &stream[..1], 0).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        receiver.feed(phase, &stream[1..], 0).event,
        ReceiveEvent::Rejected(RejectReason::BufferTooSmall)
    );
    assert_eq!(receiver.message(Lane::Short), None);
}
