// Copyright The eha-sdk Contributors

use super::{common::*, *};

#[test]
fn undersized_rx_tx_buffers_and_declared_tx_length_return_errors() {
    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut too_small_heartbeat = [0; 7];
    assert!(matches!(
        Receiver::new(
            1,
            Direction::HostToFirmware,
            Mode::Classic,
            0,
            RxBuffers {
                heartbeat: &mut too_small_heartbeat,
                short: &mut short,
                large: &mut large,
            },
        ),
        Err(Error::BufferTooSmall)
    ));

    let mut short = [0; 256];
    let mut large = [0; MAX_MESSAGE_LEN];
    let mut too_small_heartbeat = [0; 7];
    assert!(matches!(
        Transmitter::new(
            1,
            Direction::HostToFirmware,
            Mode::Classic,
            0,
            TxBuffers {
                heartbeat: &mut too_small_heartbeat,
                short: &mut short,
                large: &mut large,
            },
        ),
        Err(Error::BufferTooSmall)
    ));

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
    .expect("valid buffers");
    assert_eq!(
        tx.start_buffer(Lane::Short, 257, 0),
        Err(Error::BufferTooSmall)
    );

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
    .expect("valid buffers");
    assert!(matches!(
        rx.receive(&frame(0x0040_0000, false, 8, &HEARTBEAT), 0, 0,),
        ReceiveResult::Complete {
            lane: Lane::Heartbeat,
            ..
        }
    ));
    let completed = rx
        .take_completed(Lane::Heartbeat)
        .expect("completed heartbeat");
    let mut replacement = [0; 7];
    assert_eq!(
        rx.replace_buffer(Lane::Heartbeat, &mut replacement),
        Err(Error::BufferTooSmall)
    );
    rx.replace_buffer(Lane::Heartbeat, completed.into_buffer())
        .expect("original buffer remains usable after rejected replacement");
}
