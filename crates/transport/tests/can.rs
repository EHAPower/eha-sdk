// Copyright The eha-sdk Contributors

use protocol::{
    Direction, ExpectedIdentity, MAX_MESSAGE_LEN, Message, SampleData, encode,
    responses::{
        ConfigDataFields, ConfigDataWriter, ConfigRecordState, ConfigView, FallbackReason,
        StartupSource,
    },
};
use transport::can::{
    Error, Frame, Lane, Mode, PendingFrame, ReceiveResult, Receiver, RejectReason, RxBuffers,
    SubmitEvent, SubmitResult, Transmitter, TxBuffers, ValidatedReceiveResult,
};

mod common {
    use super::*;

    pub(super) const POSITION: [u8; 12] = [
        0x03, 0x01, 0x04, 0x00, 0x00, 0x00, 0x20, 0x41, 0xe1, 0x53, 0x01, 0x41,
    ];
    pub(super) const HEARTBEAT: [u8; 8] = [0x03, 0x06, 0x00, 0x00, 0xe9, 0x34, 0xf4, 0xfb];
    pub(super) const READ_USER: [u8; 13] = [
        0x03, 0x11, 0x05, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01, 0x92, 0x71, 0xef, 0x95,
    ];

    pub(super) fn can_id(
        node: u8,
        direction: Direction,
        lane: Lane,
        transfer: u8,
        fragment: u16,
    ) -> u32 {
        (u32::from(node) << 22)
            | (match direction {
                Direction::HostToFirmware => 0,
                Direction::FirmwareToHost => 1,
            } << 21)
            | ((lane as u32) << 19)
            | (u32::from(transfer) << 12)
            | u32::from(fragment)
    }

    pub(super) fn maximum_config_data(output: &mut [u8; MAX_MESSAGE_LEN]) -> usize {
        let fields = ConfigDataFields {
            sample: SampleData {
                query_id: 7,
                run_nonce: [1; 16],
                snapshot_sequence: 2,
                snapshot_time_us: 3,
            },
            view: ConfigView::UserRecord,
            record_state: ConfigRecordState::Complete,
            startup_source: StartupSource::User,
            fallback_reason: FallbackReason::None,
            data_time_us: 4,
        };
        let mut writer = ConfigDataWriter::start(fields, 16_384, output).expect("test invariant");
        for (index, byte) in writer.data_mut().iter_mut().enumerate() {
            *byte = index as u8;
        }
        writer.finish().expect("test invariant")
    }

    pub(super) fn long_save_and_restart(output: &mut [u8; MAX_MESSAGE_LEN]) -> usize {
        let identity = [1; 28];
        let identity = ExpectedIdentity::new(&identity).expect("test invariant");
        let mut record = [0; 16_384];
        for (index, byte) in record.iter_mut().enumerate() {
            *byte = index as u8;
        }
        encode(
            Message::SaveAndRestart {
                query_id: 1,
                identity,
                record: &record,
            },
            output,
        )
        .expect("test invariant")
    }

    pub(super) fn frame(id: u32, fdf: bool, dlc: u8, data: &[u8]) -> Frame {
        let mut storage = [0; 64];
        storage[..data.len()].copy_from_slice(data);
        Frame {
            id,
            extended: true,
            rtr: false,
            fdf,
            brs: fdf,
            esi: true,
            dlc,
            data_len: data.len() as u8,
            data: storage,
        }
    }

    pub(super) fn rx_buffers<'a>(
        heartbeat: &'a mut [u8; 8],
        short: &'a mut [u8; 256],
        large: &'a mut [u8; 16_440],
    ) -> RxBuffers<'a> {
        RxBuffers {
            heartbeat,
            short,
            large,
        }
    }

    pub(super) fn tx_buffers<'a>(
        heartbeat: &'a mut [u8; 8],
        short: &'a mut [u8; 256],
        large: &'a mut [u8; 16_440],
    ) -> TxBuffers<'a> {
        TxBuffers {
            heartbeat,
            short,
            large,
        }
    }

    pub(super) fn start_position(tx: &mut Transmitter<'_>, now_ms: u64) {
        tx.buffer_mut(Lane::Short).expect("test invariant")[..POSITION.len()]
            .copy_from_slice(&POSITION);
        tx.start_buffer(Lane::Short, POSITION.len(), now_ms)
            .expect("test invariant");
    }
}

#[path = "can/buffers.rs"]
mod buffers;
#[path = "can/isolation.rs"]
mod isolation;
#[path = "can/receive.rs"]
mod receive;
#[path = "can/reconnect.rs"]
mod reconnect;
#[path = "can/route.rs"]
mod route;
#[path = "can/transmit.rs"]
mod transmit;
