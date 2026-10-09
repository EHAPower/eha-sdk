// Copyright The eha-sdk Contributors

use super::{common::*, *};

#[test]
fn classic_golden_position_reassembles_exact_bytes() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        7,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");

    assert_eq!(
        rx.receive(&frame(0x0048_1000, false, 8, &POSITION[..8]), 10, 7),
        ReceiveResult::Incomplete { lane: Lane::Short }
    );
    let complete = rx.receive(&frame(0x0048_1001, false, 4, &POSITION[8..]), 11, 7);
    assert!(matches!(
        complete,
        ReceiveResult::Complete {
            lane: Lane::Short,
            ..
        }
    ));
    assert_eq!(rx.message(Lane::Short), Some(POSITION.as_slice()));
    let completed = rx.take_completed(Lane::Short).expect("test invariant");
    assert_eq!(completed.as_bytes(), POSITION);
    rx.replace_buffer(Lane::Short, completed.into_buffer())
        .expect("test invariant");
}

#[test]
fn validated_completion_transfers_the_original_buffer_without_revalidating() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        7,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");

    assert!(matches!(
        rx.receive_validated(&frame(0x0048_1000, false, 8, &POSITION[..8]), 10, 7),
        ValidatedReceiveResult::Incomplete { lane: Lane::Short }
    ));
    let complete = rx.receive_validated(&frame(0x0048_1001, false, 4, &POSITION[8..]), 11, 7);
    assert!(matches!(complete, ValidatedReceiveResult::Complete { .. }));
    let ValidatedReceiveResult::Complete { lane, message } = complete else {
        return;
    };
    assert_eq!(lane, Lane::Short);
    assert_eq!(message.bytes(), POSITION);
    assert!(matches!(message.decode(), Ok(Message::Position(v)) if v == 10.0));
    assert_eq!(rx.message(Lane::Short), None);
    assert_eq!(
        rx.reconfigure(1, Mode::Classic, 8, 12),
        Err(Error::LaneBusy)
    );

    rx.replace_buffer(lane, message.into_buffer())
        .expect("proof returns its original buffer");
    assert_eq!(rx.reconfigure(1, Mode::Classic, 8, 12), Ok(()));
}

#[test]
fn fd_last_padding_must_be_zero_and_is_not_public_bytes() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Fd,
        0,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");

    let mut padded = [0; 16];
    padded[..READ_USER.len()].copy_from_slice(&READ_USER);
    assert!(matches!(
        rx.receive(&frame(0x0048_1000, true, 10, &padded), 0, 0),
        ReceiveResult::Complete {
            lane: Lane::Short,
            ..
        }
    ));
    assert_eq!(rx.message(Lane::Short), Some(READ_USER.as_slice()));
    rx.discard_completed(Lane::Short);

    padded[13] = 1;
    assert_eq!(
        rx.receive(&frame(0x0048_1000, true, 10, &padded), 41, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::InvalidFragment
        }
    );
}

#[test]
fn actual_driver_length_cannot_be_inferred_from_dlc() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        0,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    let mut malformed = frame(0x0048_1000, false, 8, &POSITION[..7]);
    malformed.data_len = 7;
    assert_eq!(
        rx.receive(&malformed, 0, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::InvalidDlc
        }
    );
}

#[test]
fn duplicate_or_out_of_order_fragment_abandons_the_group() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        0,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    assert!(matches!(
        rx.receive(&frame(0x0048_1000, false, 8, &POSITION[..8]), 0, 0),
        ReceiveResult::Incomplete { .. }
    ));
    assert_eq!(
        rx.receive(&frame(0x0048_1000, false, 8, &POSITION[..8]), 1, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::InvalidFragment
        }
    );
    assert_eq!(
        rx.receive(&frame(0x0048_1001, false, 4, &POSITION[8..]), 2, 0),
        ReceiveResult::Ignored
    );
    assert_eq!(
        rx.receive(&frame(0x0048_1000, false, 8, &POSITION[..8]), 39, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::TransferProtected
        }
    );
}

#[test]
fn absolute_timeout_expires_without_new_input() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        0,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    rx.receive(&frame(0x0048_1000, false, 8, &POSITION[..8]), 100, 0);
    assert_eq!(rx.poll(119), 0);
    assert_eq!(rx.poll(120), 1);
    assert_eq!(
        rx.receive(&frame(0x0048_1001, false, 4, &POSITION[8..]), 121, 0),
        ReceiveResult::Ignored
    );
}

#[test]
fn stale_generation_never_advances_a_new_receiver() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        3,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    assert_eq!(
        rx.receive(&frame(0x0040_1000, false, 8, &HEARTBEAT), 0, 2),
        ReceiveResult::Rejected {
            reason: RejectReason::StaleGeneration
        }
    );
    assert!(rx.message(Lane::Heartbeat).is_none());
}

#[test]
fn reconfiguration_requires_a_strictly_new_generation() {
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; 16_440];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        3,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    assert_eq!(
        rx.reconfigure(1, Mode::Fd, 3, 0),
        Err(Error::InvalidGeneration)
    );
    assert_eq!(rx.reconfigure(1, Mode::Fd, 4, 0), Ok(()));
    assert_eq!(
        rx.receive(&frame(0x0040_1000, true, 8, &HEARTBEAT), 1, 3),
        ReceiveResult::Rejected {
            reason: RejectReason::StaleGeneration
        }
    );
}

#[test]
fn maximum_config_data_reassembles_in_classic_and_fd() {
    let mut message = [0; MAX_MESSAGE_LEN];
    assert_eq!(maximum_config_data(&mut message), MAX_MESSAGE_LEN);

    {
        let mut heartbeat = [0; 8];
        let mut short = [0; 256];
        let mut large = [0; MAX_MESSAGE_LEN];
        let mut rx = Receiver::new(
            1,
            Direction::FirmwareToHost,
            Mode::Classic,
            0,
            rx_buffers(&mut heartbeat, &mut short, &mut large),
        )
        .expect("test invariant");
        for fragment in 0..2055_u16 {
            let at = usize::from(fragment) * 8;
            let result = rx.receive(
                &frame(
                    can_id(1, Direction::FirmwareToHost, Lane::Large, 0, fragment),
                    false,
                    8,
                    &message[at..at + 8],
                ),
                u64::from(fragment),
                0,
            );
            if fragment == 2054 {
                assert!(matches!(
                    result,
                    ReceiveResult::Complete {
                        lane: Lane::Large,
                        ..
                    }
                ));
            } else {
                assert_eq!(result, ReceiveResult::Incomplete { lane: Lane::Large });
            }
        }
        let completed = rx.take_completed(Lane::Large).expect("test invariant");
        assert_eq!(completed.as_bytes(), message);
    }

    {
        let mut heartbeat = [0; 8];
        let mut short = [0; 256];
        let mut large = [0; MAX_MESSAGE_LEN];
        let mut rx = Receiver::new(
            1,
            Direction::FirmwareToHost,
            Mode::Fd,
            0,
            rx_buffers(&mut heartbeat, &mut short, &mut large),
        )
        .expect("test invariant");
        for fragment in 0..257_u16 {
            let at = usize::from(fragment) * 64;
            let logical = (MAX_MESSAGE_LEN - at).min(64);
            let mut payload = [0; 64];
            payload[..logical].copy_from_slice(&message[at..at + logical]);
            let result = rx.receive(
                &frame(
                    can_id(1, Direction::FirmwareToHost, Lane::Large, 0, fragment),
                    true,
                    15,
                    &payload,
                ),
                u64::from(fragment),
                0,
            );
            if fragment == 256 {
                assert!(matches!(
                    result,
                    ReceiveResult::Complete {
                        lane: Lane::Large,
                        ..
                    }
                ));
            } else {
                assert_eq!(result, ReceiveResult::Incomplete { lane: Lane::Large });
            }
        }
        let completed = rx.take_completed(Lane::Large).expect("test invariant");
        assert_eq!(completed.as_bytes(), message);
    }
}
