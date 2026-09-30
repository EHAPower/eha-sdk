// Copyright The eha_controller Contributors

use super::common::ValueFields;
use crate::{Error, MAX_PAYLOAD_LEN, SampleData};

pub(super) fn payload(output: &mut [u8], length: usize) -> Result<&mut [u8], Error> {
    if length > MAX_PAYLOAD_LEN {
        return Err(Error::InvalidPayloadLength);
    }
    let total = 8usize.checked_add(length).ok_or(Error::BufferTooSmall)?;
    if output.len() < total {
        return Err(Error::BufferTooSmall);
    }
    Ok(&mut output[4..4 + length])
}

pub(super) fn write_sample(out: &mut [u8], sample: SampleData) {
    put_u32(out, 0, sample.query_id);
    out[4..20].copy_from_slice(&sample.run_nonce);
    put_u32(out, 20, sample.snapshot_sequence);
    put_u64(out, 24, sample.snapshot_time_us);
}
pub(super) fn put_value(out: &mut [u8], value_at: usize, state_at: usize, value: ValueFields) {
    put_f32(out, value_at, value.value);
    out[state_at] = value.state.bits();
}
pub(super) fn put_u16(out: &mut [u8], at: usize, value: u16) {
    out[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
pub(super) fn put_u32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
pub(super) fn put_u64(out: &mut [u8], at: usize, value: u64) {
    out[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
pub(super) fn put_f32(out: &mut [u8], at: usize, value: f32) {
    put_u32(out, at, value.to_bits());
}
