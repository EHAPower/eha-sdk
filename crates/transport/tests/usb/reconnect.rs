// Copyright The eha_controller Contributors

use protocol::{Direction, MAX_MESSAGE_LEN};
use transport::usb::*;

use super::common::*;

#[test]
fn sender_reconnect_keeps_transfer_number_and_ignores_old_completion() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut sender = Sender::new(
        Direction::HostToFirmware,
        &mut heartbeat,
        &mut short,
        &mut large,
    );
    begin(&mut sender, Lane::Short, &POSITION, 0);
    let mut packet = [0; 64];
    let old = sender
        .prepare_packet(Lane::Short, &mut packet)
        .expect("packet preparation")
        .expect("first packet");

    let state = sender.into_reconnect_state(1).expect("state");
    let mut restored_heartbeat = [0; 8];
    let mut restored_short = [0; 256];
    let mut restored_large = [0; MAX_MESSAGE_LEN];
    let mut restored = Sender::from_reconnect_state(
        state,
        &mut restored_heartbeat,
        &mut restored_short,
        &mut restored_large,
    );

    assert_eq!(restored.accepted(old.token), SendEvent::Ignored);
    begin(&mut restored, Lane::Short, &POSITION, 2);
    let current = restored
        .prepare_packet(Lane::Short, &mut packet)
        .expect("packet preparation")
        .expect("new packet");
    assert_eq!(restored.completed(old.token, 2), SendEvent::Ignored);
    assert_eq!(restored.accepted(current.token), SendEvent::PacketAccepted);
}

#[test]
fn receiver_reconnect_rejects_old_phase_and_keeps_transfer_guard() {
    let mut source_heartbeat = [0; 8];
    let mut source_short = [0; 256];
    let mut source_large = [0; MAX_MESSAGE_LEN];
    let mut source = Sender::new(
        Direction::HostToFirmware,
        &mut source_heartbeat,
        &mut source_short,
        &mut source_large,
    );
    begin(&mut source, Lane::Short, &POSITION, 0);
    let stream = drain(&mut source, Lane::Short, 0);

    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut receiver = Receiver::new(
        Direction::HostToFirmware,
        &mut heartbeat,
        &mut short,
        &mut large,
    );
    let old_phase = receiver.phase();
    assert_eq!(
        receiver.feed(old_phase, &stream[..1], 0).event,
        ReceiveEvent::Ignored
    );
    assert!(matches!(
        receiver.feed(old_phase, &stream[1..], 0).event,
        ReceiveEvent::Complete(message) if message.lane == Lane::Short
    ));

    let state = receiver.into_reconnect_state(1).expect("state");
    let mut restored_heartbeat = [0; 8];
    let mut restored_short = [0; 256];
    let mut restored_large = [0; MAX_MESSAGE_LEN];
    let mut restored = Receiver::from_reconnect_state(
        state,
        &mut restored_heartbeat,
        &mut restored_short,
        &mut restored_large,
    );
    let phase = restored.phase();

    assert_eq!(
        restored.feed(old_phase, &stream, 2).event,
        ReceiveEvent::StalePhase
    );
    assert_eq!(
        restored.feed(phase, &stream[..1], 2).event,
        ReceiveEvent::Ignored
    );
    assert_eq!(
        restored.feed(phase, &stream[1..], 2).event,
        ReceiveEvent::Ignored
    );
}
