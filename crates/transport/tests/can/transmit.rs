// Copyright The eha_controller Contributors

use super::{common::*, *};

#[test]
fn tx_uses_golden_id_and_requires_explicit_local_acceptance() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut tx = Transmitter::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        9,
        tx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    start_position(&mut tx, 0);
    let PendingFrame {
        frame: first,
        token,
    } = tx.next_frame(Lane::Short).expect("test invariant");
    assert_eq!(first.id, 0x0048_0000);
    assert_eq!(first.dlc, 8);
    assert_eq!(&first.data[..8], &POSITION[..8]);
    assert!(tx.next_frame(Lane::Short).is_none());
    assert_eq!(
        tx.complete(token, SubmitResult::Accepted, 0),
        Ok(SubmitEvent::FrameAccepted { lane: Lane::Short })
    );
    let PendingFrame { frame: last, token } = tx.next_frame(Lane::Short).expect("test invariant");
    assert_eq!(last.id, 0x0048_0001);
    assert_eq!(last.dlc, 4);
    assert_eq!(
        tx.complete(token, SubmitResult::Accepted, 1),
        Ok(SubmitEvent::MessageCommitted { lane: Lane::Short })
    );
    // Completion frees this same caller buffer; no sender drop or guard rebuild is needed.
    start_position(&mut tx, 2);
}

#[test]
fn old_completion_cannot_match_after_transfer_id_wrap() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut tx = Transmitter::new(
        1,
        Direction::HostToFirmware,
        Mode::Fd,
        0,
        tx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    start_position(&mut tx, 0);
    let first = tx.next_frame(Lane::Short).expect("test invariant");
    tx.complete(first.token, SubmitResult::Accepted, 0)
        .expect("test invariant");

    for i in 1..128_u64 {
        start_position(&mut tx, i * 40);
        let pending = tx.next_frame(Lane::Short).expect("test invariant");
        tx.complete(pending.token, SubmitResult::Accepted, i * 40)
            .expect("test invariant");
    }
    start_position(&mut tx, 128 * 40);
    let wrapped = tx.next_frame(Lane::Short).expect("test invariant");
    assert_eq!(wrapped.frame.id, first.frame.id);
    assert_eq!(
        tx.complete(first.token, SubmitResult::Accepted, 128 * 40),
        Err(Error::StaleCompletion)
    );
    tx.complete(wrapped.token, SubmitResult::Accepted, 128 * 40)
        .expect("test invariant");
}

#[test]
fn large_tx_snapshot_allows_heartbeat_and_short_before_its_next_fragment() {
    let mut message = [0; MAX_MESSAGE_LEN];
    let length = long_save_config(&mut message);
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut tx = Transmitter::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        0,
        tx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");

    tx.buffer_mut(Lane::Large).expect("large lane is idle")[..length]
        .copy_from_slice(&message[..length]);
    tx.start_buffer(Lane::Large, length, 0)
        .expect("complete large message freezes its lane");
    let first_large = tx.next_frame(Lane::Large).expect("first large frame");
    assert_eq!(&first_large.frame.data[..8], &message[..8]);
    assert_eq!(tx.buffer_mut(Lane::Large), Err(Error::SendBusy));

    tx.buffer_mut(Lane::Heartbeat)
        .expect("heartbeat lane remains idle")
        .copy_from_slice(&HEARTBEAT);
    tx.start_buffer(Lane::Heartbeat, HEARTBEAT.len(), 1)
        .expect("heartbeat can start while large frame awaits completion");
    let heartbeat_frame = tx.next_frame(Lane::Heartbeat).expect("heartbeat frame");
    assert_eq!(&heartbeat_frame.frame.data[..8], &HEARTBEAT);
    assert_eq!(
        tx.complete(heartbeat_frame.token, SubmitResult::Accepted, 1),
        Ok(SubmitEvent::MessageCommitted {
            lane: Lane::Heartbeat
        })
    );

    start_position(&mut tx, 2);
    let short_frame = tx.next_frame(Lane::Short).expect("short frame");
    assert_eq!(&short_frame.frame.data[..8], &POSITION[..8]);
    assert_eq!(
        tx.complete(short_frame.token, SubmitResult::Accepted, 2),
        Ok(SubmitEvent::FrameAccepted { lane: Lane::Short })
    );

    assert!(tx.next_frame(Lane::Large).is_none());
    assert_eq!(
        tx.complete(first_large.token, SubmitResult::Accepted, 3),
        Ok(SubmitEvent::FrameAccepted { lane: Lane::Large })
    );
    let second_large = tx.next_frame(Lane::Large).expect("second large frame");
    assert_eq!(&second_large.frame.data[..8], &message[8..16]);
}

#[test]
fn failed_cancelled_and_old_generation_tokens_cannot_advance_tx() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut tx = Transmitter::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        0,
        tx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");

    start_position(&mut tx, 0);
    let failed = tx.next_frame(Lane::Short).expect("short frame");
    assert_eq!(
        tx.complete(failed.token, SubmitResult::Failed, 0),
        Ok(SubmitEvent::Abandoned {
            lane: Lane::Short,
            result: SubmitResult::Failed,
        })
    );
    assert_eq!(
        tx.complete(failed.token, SubmitResult::Accepted, 1),
        Err(Error::StaleCompletion)
    );

    tx.buffer_mut(Lane::Heartbeat)
        .expect("heartbeat lane is idle")
        .copy_from_slice(&HEARTBEAT);
    tx.start_buffer(Lane::Heartbeat, HEARTBEAT.len(), 1)
        .expect("heartbeat can start");
    let cancelled = tx.next_frame(Lane::Heartbeat).expect("heartbeat frame");
    assert_eq!(
        tx.complete(cancelled.token, SubmitResult::Cancelled, 1),
        Ok(SubmitEvent::Abandoned {
            lane: Lane::Heartbeat,
            result: SubmitResult::Cancelled,
        })
    );
    assert_eq!(
        tx.complete(cancelled.token, SubmitResult::Accepted, 2),
        Err(Error::StaleCompletion)
    );

    start_position(&mut tx, 2);
    let old_generation = tx.next_frame(Lane::Short).expect("short frame");
    tx.reconfigure(1, Mode::Classic, 1, 3)
        .expect("strictly newer generation");
    assert_eq!(
        tx.complete(old_generation.token, SubmitResult::Accepted, 3),
        Err(Error::StaleCompletion)
    );
}
