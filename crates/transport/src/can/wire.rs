// Copyright The eha_controller Contributors

use protocol::Direction;

use super::{Frame, Lane, Mode, RejectReason, types::SHORT_CAPACITY};

#[derive(Clone, Copy)]
pub(super) struct ExpectedPayload {
    pub(super) logical: usize,
    pub(super) data_len: usize,
    pub(super) dlc: u8,
    pub(super) is_last: bool,
}

pub(super) fn expected_payload(mode: Mode, length: usize, fragment: u16) -> ExpectedPayload {
    let quantum = mode.quantum();
    let offset = usize::from(fragment) * quantum;
    let remaining = length.saturating_sub(offset);
    let logical = remaining.min(quantum);
    let is_last = remaining <= quantum;
    match mode {
        Mode::Classic => ExpectedPayload {
            logical,
            data_len: logical,
            dlc: logical as u8,
            is_last,
        },
        Mode::Fd => {
            let data_len = fd_data_len(logical);
            ExpectedPayload {
                logical,
                data_len,
                dlc: fd_dlc(data_len),
                is_last,
            }
        }
    }
}

pub(super) fn matches_payload(data: &[u8], data_len: usize, expected: ExpectedPayload) -> bool {
    data_len == expected.data_len
        && data.len() >= data_len
        && data[expected.logical..data_len]
            .iter()
            .all(|byte| *byte == 0)
}

pub(super) fn frame_length(frame: &Frame, mode: Mode) -> Result<usize, RejectReason> {
    if frame.rtr
        || frame.data_len > 64
        || (matches!(mode, Mode::Classic) && (frame.fdf || frame.brs))
        || (matches!(mode, Mode::Fd) && (!frame.fdf || !frame.brs))
    {
        return Err(RejectReason::InvalidForm);
    }
    let Some(dlc_length) = dlc_data_len(frame.dlc) else {
        return Err(RejectReason::InvalidDlc);
    };
    if matches!(mode, Mode::Classic) && frame.dlc > 8 {
        return Err(RejectReason::InvalidDlc);
    }
    if usize::from(frame.data_len) != dlc_length {
        return Err(RejectReason::InvalidDlc);
    }
    Ok(dlc_length)
}

pub(super) const fn lane_for(heartbeat: bool, length: usize) -> Lane {
    if heartbeat {
        Lane::Heartbeat
    } else if length <= SHORT_CAPACITY {
        Lane::Short
    } else {
        Lane::Large
    }
}

pub(super) const fn direction_bit(direction: Direction) -> u32 {
    match direction {
        Direction::HostToFirmware => 0,
        Direction::FirmwareToHost => 1,
    }
}

pub(super) const fn compose_id(
    node: u8,
    direction: Direction,
    lane: Lane,
    transfer_id: u8,
    fragment: u16,
) -> u32 {
    ((node as u32) << 22)
        | (direction_bit(direction) << 21)
        | ((lane as u32) << 19)
        | ((transfer_id as u32) << 12)
        | (fragment as u32)
}

const fn dlc_data_len(dlc: u8) -> Option<usize> {
    Some(match dlc {
        0..=8 => dlc as usize,
        9 => 12,
        10 => 16,
        11 => 20,
        12 => 24,
        13 => 32,
        14 => 48,
        15 => 64,
        _ => return None,
    })
}

const fn fd_data_len(length: usize) -> usize {
    match length {
        0..=8 => length,
        9..=12 => 12,
        13..=16 => 16,
        17..=20 => 20,
        21..=24 => 24,
        25..=32 => 32,
        33..=48 => 48,
        _ => 64,
    }
}

const fn fd_dlc(length: usize) -> u8 {
    match length {
        0..=8 => length as u8,
        12 => 9,
        16 => 10,
        20 => 11,
        24 => 12,
        32 => 13,
        48 => 14,
        _ => 15,
    }
}
