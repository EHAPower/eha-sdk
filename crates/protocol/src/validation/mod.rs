// Copyright The eha-sdk Contributors

use crate::{Error, MessageKind};

mod responses;
use responses::{
    finite_at, validate_config_data, validate_diagnostics, validate_identity,
    validate_measurements, validate_operation_result, validate_status, validate_unavailable,
};

pub(super) fn payload_len_is_valid(kind: MessageKind, n: usize) -> bool {
    match kind {
        MessageKind::Position | MessageKind::Velocity | MessageKind::Force => n == 4,
        MessageKind::Impedance => n == 12,
        MessageKind::Stop | MessageKind::Heartbeat => n == 0,
        MessageKind::Query | MessageKind::ReadConfig => n == 5,
        MessageKind::ReadResult | MessageKind::ReleaseResult => n == 40,
        MessageKind::SaveConfig => (37..=16_420).contains(&n),
        MessageKind::RestoreFactory | MessageKind::ResetApplication | MessageKind::EnterUpdate => {
            n == 36
        }
        MessageKind::Telemetry | MessageKind::Status => n == 152,
        MessageKind::Identity => (100..=520).contains(&n),
        MessageKind::Measurements => n == 100,
        MessageKind::Diagnostics => (36..=1060).contains(&n) && (n - 36).is_multiple_of(64),
        MessageKind::ConfigData => (48..=16_432).contains(&n),
        MessageKind::OperationResult => n == 88,
        MessageKind::DataUnavailable => n == 48,
    }
}

pub(super) fn validate_payload(kind: MessageKind, p: &[u8]) -> Result<(), Error> {
    match kind {
        MessageKind::Position | MessageKind::Velocity | MessageKind::Force => finite_at(p, 0),
        MessageKind::Impedance => {
            finite_at(p, 0)?;
            finite_at(p, 4)?;
            finite_at(p, 8)
        }
        MessageKind::Stop | MessageKind::Heartbeat => Ok(()),
        MessageKind::Query => {
            nonzero(u32_at(p, 0))?;
            if p[4] <= 3 {
                Ok(())
            } else {
                Err(Error::InvalidField)
            }
        }
        MessageKind::ReadConfig => {
            nonzero(u32_at(p, 0))?;
            if p[4] <= 3 {
                Ok(())
            } else {
                Err(Error::InvalidField)
            }
        }
        MessageKind::ReadResult => {
            nonzero(u32_at(p, 0))?;
            valid_key(&p[4..40])
        }
        MessageKind::SaveConfig => valid_key(&p[..36]),
        MessageKind::RestoreFactory | MessageKind::ResetApplication | MessageKind::EnterUpdate => {
            valid_key(p)
        }
        MessageKind::ReleaseResult => {
            valid_key(&p[..36])?;
            nonzero(u32_at(p, 36))
        }
        MessageKind::Telemetry | MessageKind::Status => validate_status(kind, p),
        MessageKind::Identity => validate_identity(p),
        MessageKind::Measurements => validate_measurements(p),
        MessageKind::Diagnostics => validate_diagnostics(p),
        MessageKind::ConfigData => validate_config_data(p),
        MessageKind::OperationResult => validate_operation_result(p),
        MessageKind::DataUnavailable => validate_unavailable(p),
    }
}

pub(super) fn valid_key(p: &[u8]) -> Result<(), Error> {
    if p.len() != 36 || p[12..28].iter().all(|&x| x == 0) || u64_at(p, 28) == 0 {
        Err(Error::InvalidField)
    } else {
        Ok(())
    }
}

pub(super) fn nonzero(v: u32) -> Result<(), Error> {
    if v == 0 {
        Err(Error::InvalidField)
    } else {
        Ok(())
    }
}
pub(super) fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(super) fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub(super) fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}
pub(super) fn f32_at(b: &[u8], o: usize) -> f32 {
    f32::from_bits(u32_at(b, o))
}
pub(super) fn put_u16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
pub(super) fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
pub(super) fn put_f32(b: &mut [u8], o: usize, v: f32) {
    put_u32(b, o, v.to_bits());
}
