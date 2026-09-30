// Copyright The eha_controller Contributors

use super::{common::*, *};

#[test]
fn large_assembly_does_not_hold_heartbeat_or_short_lane_buffers() {
    let mut message = [0; MAX_MESSAGE_LEN];
    let length = long_save_config(&mut message);
    assert_eq!(length, 16_428);
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        0,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");

    assert_eq!(
        rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Large, 0, 0),
                false,
                8,
                &message[..8],
            ),
            0,
            0,
        ),
        ReceiveResult::Incomplete { lane: Lane::Large }
    );
    assert!(matches!(
        rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Heartbeat, 0, 0),
                false,
                8,
                &HEARTBEAT,
            ),
            1,
            0,
        ),
        ReceiveResult::Complete {
            lane: Lane::Heartbeat,
            ..
        }
    ));
    let completed = rx.take_completed(Lane::Heartbeat).expect("test invariant");
    assert_eq!(completed.as_bytes(), HEARTBEAT);
    rx.replace_buffer(Lane::Heartbeat, completed.into_buffer())
        .expect("test invariant");

    assert_eq!(
        rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Short, 0, 0),
                false,
                8,
                &POSITION[..8],
            ),
            2,
            0,
        ),
        ReceiveResult::Incomplete { lane: Lane::Short }
    );
    assert!(matches!(
        rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Short, 0, 1),
                false,
                4,
                &POSITION[8..],
            ),
            3,
            0,
        ),
        ReceiveResult::Complete {
            lane: Lane::Short,
            ..
        }
    ));
    let completed = rx.take_completed(Lane::Short).expect("test invariant");
    assert_eq!(completed.as_bytes(), POSITION);
    rx.replace_buffer(Lane::Short, completed.into_buffer())
        .expect("test invariant");

    for fragment in 1..2054_u16 {
        let at = usize::from(fragment) * 8;
        let end = (at + 8).min(length);
        let result = rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Large, 0, fragment),
                false,
                (end - at) as u8,
                &message[at..end],
            ),
            4,
            0,
        );
        if fragment == 2053 {
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
    assert_eq!(rx.message(Lane::Large), Some(message[..length].as_ref()));
}

#[test]
fn large_lane_deadline_is_absolute_despite_fragment_progress() {
    let mut message = [0; MAX_MESSAGE_LEN];
    let length = long_save_config(&mut message);
    let mut heartbeat = [0; 8];
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Classic,
        0,
        rx_buffers(&mut heartbeat, &mut short, &mut large),
    )
    .expect("test invariant");
    let first = frame(
        can_id(1, Direction::HostToFirmware, Lane::Large, 0, 0),
        false,
        8,
        &message[..8],
    );
    assert_eq!(
        rx.receive(&first, 10, 0),
        ReceiveResult::Incomplete { lane: Lane::Large }
    );
    assert_eq!(
        rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Large, 0, 1),
                false,
                8,
                &message[8..16],
            ),
            30_009,
            0,
        ),
        ReceiveResult::Incomplete { lane: Lane::Large }
    );
    assert_eq!(rx.poll(30_010), 1);
    assert_eq!(
        rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Large, 0, 2),
                false,
                8,
                &message[16..24],
            ),
            30_011,
            0,
        ),
        ReceiveResult::Ignored
    );
    assert_eq!(
        rx.receive(&first, 30_011, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::TransferProtected
        }
    );
    assert_eq!(length, 16_428);
}

#[test]
fn first_fragment_replacement_and_invalid_frames_preserve_or_guard_the_right_group() {
    // An invalid first fragment with another transfer number cannot damage the active group.
    {
        let mut heartbeat = [0; 8];
        let mut short = [0; 256];
        let mut large = [0; MAX_MESSAGE_LEN];
        let mut rx = Receiver::new(
            1,
            Direction::HostToFirmware,
            Mode::Classic,
            0,
            rx_buffers(&mut heartbeat, &mut short, &mut large),
        )
        .expect("test invariant");
        rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Short, 0, 0),
                false,
                8,
                &POSITION[..8],
            ),
            0,
            0,
        );
        assert_eq!(
            rx.receive(
                &frame(
                    can_id(1, Direction::HostToFirmware, Lane::Short, 1, 0),
                    false,
                    8,
                    &[2, 1, 4, 0, 0, 0, 0, 0],
                ),
                1,
                0,
            ),
            ReceiveResult::Rejected {
                reason: RejectReason::InvalidPrefix
            }
        );
        assert!(matches!(
            rx.receive(
                &frame(
                    can_id(1, Direction::HostToFirmware, Lane::Short, 0, 1),
                    false,
                    4,
                    &POSITION[8..],
                ),
                2,
                0,
            ),
            ReceiveResult::Complete { .. }
        ));
    }

    // A valid new first fragment replaces and guards the previous group.
    {
        let mut heartbeat = [0; 8];
        let mut short = [0; 256];
        let mut large = [0; MAX_MESSAGE_LEN];
        let mut rx = Receiver::new(
            1,
            Direction::HostToFirmware,
            Mode::Classic,
            0,
            rx_buffers(&mut heartbeat, &mut short, &mut large),
        )
        .expect("test invariant");
        for transfer in [0, 1] {
            assert_eq!(
                rx.receive(
                    &frame(
                        can_id(1, Direction::HostToFirmware, Lane::Short, transfer, 0),
                        false,
                        8,
                        &POSITION[..8],
                    ),
                    u64::from(transfer),
                    0,
                ),
                ReceiveResult::Incomplete { lane: Lane::Short }
            );
        }
        assert_eq!(
            rx.receive(
                &frame(
                    can_id(1, Direction::HostToFirmware, Lane::Short, 0, 0),
                    false,
                    8,
                    &POSITION[..8],
                ),
                39,
                0,
            ),
            ReceiveResult::Rejected {
                reason: RejectReason::TransferProtected
            }
        );
        assert_eq!(
            rx.receive(
                &frame(
                    can_id(1, Direction::HostToFirmware, Lane::Short, 0, 1),
                    false,
                    4,
                    &POSITION[8..],
                ),
                3,
                0,
            ),
            ReceiveResult::Ignored
        );
    }

    // An invalid metadata frame matching an active id abandons and guards that id.
    {
        let mut heartbeat = [0; 8];
        let mut short = [0; 256];
        let mut large = [0; MAX_MESSAGE_LEN];
        let mut rx = Receiver::new(
            1,
            Direction::HostToFirmware,
            Mode::Classic,
            0,
            rx_buffers(&mut heartbeat, &mut short, &mut large),
        )
        .expect("test invariant");
        rx.receive(
            &frame(
                can_id(1, Direction::HostToFirmware, Lane::Short, 0, 0),
                false,
                8,
                &POSITION[..8],
            ),
            0,
            0,
        );
        let mut bad = frame(
            can_id(1, Direction::HostToFirmware, Lane::Short, 0, 1),
            false,
            4,
            &POSITION[8..],
        );
        bad.rtr = true;
        assert_eq!(
            rx.receive(&bad, 1, 0),
            ReceiveResult::Rejected {
                reason: RejectReason::InvalidForm
            }
        );
        assert_eq!(
            rx.receive(
                &frame(
                    can_id(1, Direction::HostToFirmware, Lane::Short, 0, 0),
                    false,
                    8,
                    &POSITION[..8],
                ),
                39,
                0,
            ),
            ReceiveResult::Rejected {
                reason: RejectReason::TransferProtected
            }
        );
    }
}
