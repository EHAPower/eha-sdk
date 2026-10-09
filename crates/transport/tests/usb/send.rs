// Copyright The eha-sdk Contributors

use protocol::{Direction, MAX_MESSAGE_LEN};
use transport::usb::*;

use super::common::*;

#[test]
fn accepted_cancel_changes_phase_and_old_completion_is_ignored() {
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
    let mut packet = [0; 64];
    let old = sender
        .prepare_packet(Lane::Short, &mut packet)
        .expect("可准备")
        .expect("有首包");
    assert_eq!(
        sender.prepare_packet(Lane::Short, &mut packet),
        Err(transport::usb::SendError::AwaitingCompletion)
    );
    assert_eq!(sender.accepted(old.token), SendEvent::PacketAccepted);
    assert_eq!(
        sender.prepare_packet(Lane::Short, &mut packet),
        Err(transport::usb::SendError::AwaitingCompletion)
    );
    let cancelled = sender.cancel(old.token, 1);
    assert!(matches!(
        cancelled,
        SendEvent::PhaseChanged { failed: false, .. }
    ));
    let SendEvent::PhaseChanged {
        phase: new_phase, ..
    } = cancelled
    else {
        return;
    };
    assert_eq!(new_phase, sender.phase());
    begin(&mut sender, Lane::Short, &POSITION, 2);
    let current = sender
        .prepare_packet(Lane::Short, &mut packet)
        .expect("可准备")
        .expect("有首包");
    assert_eq!(sender.completed(old.token, 2), SendEvent::Ignored);
    assert_eq!(sender.accepted(current.token), SendEvent::PacketAccepted);
}
