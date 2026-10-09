// Copyright The eha-sdk Contributors

use super::{common::*, *};

#[test]
fn frame_form_and_route_mismatches_are_never_accepted() {
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
    let id = can_id(1, Direction::HostToFirmware, Lane::Heartbeat, 0, 0);

    let mut standard = frame(id, false, 8, &HEARTBEAT);
    standard.extended = false;
    assert_eq!(rx.receive(&standard, 0, 0), ReceiveResult::Ignored);
    let mut rtr = frame(id, false, 8, &HEARTBEAT);
    rtr.rtr = true;
    assert_eq!(
        rx.receive(&rtr, 0, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::InvalidForm
        }
    );
    let mut fdf = frame(id, false, 8, &HEARTBEAT);
    fdf.fdf = true;
    assert_eq!(
        rx.receive(&fdf, 0, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::InvalidForm
        }
    );
    let mut brs = frame(id, false, 8, &HEARTBEAT);
    brs.brs = true;
    assert_eq!(
        rx.receive(&brs, 0, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::InvalidForm
        }
    );
    assert_eq!(
        rx.receive(
            &frame(
                can_id(1, Direction::FirmwareToHost, Lane::Heartbeat, 0, 0),
                false,
                8,
                &HEARTBEAT,
            ),
            0,
            0,
        ),
        ReceiveResult::Ignored
    );
    assert_eq!(
        rx.receive(
            &frame(
                can_id(2, Direction::HostToFirmware, Lane::Heartbeat, 0, 0),
                false,
                8,
                &HEARTBEAT,
            ),
            0,
            0,
        ),
        ReceiveResult::Ignored
    );
    let reserved = (u32::from(1_u8) << 22) | (3 << 19);
    assert_eq!(
        rx.receive(&frame(reserved, false, 8, &HEARTBEAT), 0, 0),
        ReceiveResult::Ignored
    );

    let mut fd_heartbeat = [0; 8];
    let mut fd_short = [0; 256];
    let mut fd_large = [0; MAX_MESSAGE_LEN];
    let mut fd_rx = Receiver::new(
        1,
        Direction::HostToFirmware,
        Mode::Fd,
        0,
        rx_buffers(&mut fd_heartbeat, &mut fd_short, &mut fd_large),
    )
    .expect("test invariant");
    let mut no_fdf = frame(id, false, 8, &HEARTBEAT);
    no_fdf.brs = true;
    assert_eq!(
        fd_rx.receive(&no_fdf, 0, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::InvalidForm
        }
    );
    let mut no_brs = frame(id, true, 8, &HEARTBEAT);
    no_brs.brs = false;
    assert_eq!(
        fd_rx.receive(&no_brs, 0, 0),
        ReceiveResult::Rejected {
            reason: RejectReason::InvalidForm
        }
    );
}
