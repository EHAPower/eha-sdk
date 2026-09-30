// Copyright The eha_controller Contributors

use super::{common::*, *};

#[test]
fn reconnect_keeps_tx_number_and_rejects_old_completion() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut transmitter = Transmitter::new(
        1,
        Direction::HostToFirmware,
        Mode::Fd,
        7,
        tx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    start_position(&mut transmitter, 0);
    let old = transmitter.next_frame(Lane::Short).expect("first frame");

    let state = transmitter.into_reconnect_state(1).expect("state");
    let mut restored_heartbeat = [0; 8];
    let mut restored_short = [0; 256];
    let mut restored_large = [0; 16_440];
    let mut restored = Transmitter::from_reconnect_state(
        state,
        tx_buffers(
            &mut restored_heartbeat,
            &mut restored_short,
            &mut restored_large,
        ),
    )
    .expect("restored transmitter");

    assert_eq!(
        restored.complete(old.token, SubmitResult::Accepted, 2),
        Err(Error::StaleCompletion)
    );
    start_position(&mut restored, 2);
    let current = restored.next_frame(Lane::Short).expect("new frame");
    assert_ne!(current.frame.id, old.frame.id);
    assert_eq!(
        restored.complete(current.token, SubmitResult::Accepted, 2),
        Ok(SubmitEvent::MessageCommitted { lane: Lane::Short })
    );
}

#[test]
fn reconnect_discards_rx_bytes_but_keeps_their_transfer_guard() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut receiver = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        7,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    let old = frame(0x0048_1000, false, 8, &POSITION[..8]);
    assert_eq!(
        receiver.receive(&old, 0, 7),
        ReceiveResult::Incomplete { lane: Lane::Short }
    );

    let state = receiver.into_reconnect_state(1).expect("state");
    let mut restored_heartbeat = [0; 8];
    let mut restored_short = [0; 256];
    let mut restored_large = [0; 16_440];
    let mut restored = Receiver::from_reconnect_state(
        state,
        rx_buffers(
            &mut restored_heartbeat,
            &mut restored_short,
            &mut restored_large,
        ),
    )
    .expect("restored receiver");

    assert_eq!(
        restored.receive(&old, 2, 7),
        ReceiveResult::Rejected {
            reason: RejectReason::StaleGeneration
        }
    );
    assert_eq!(
        restored.receive(&old, 2, 8),
        ReceiveResult::Rejected {
            reason: RejectReason::TransferProtected
        }
    );
}
