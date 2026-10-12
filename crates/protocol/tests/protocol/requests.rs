// Copyright The eha-sdk Contributors

use super::*;
use protocol::{ExpectedIdentity, ExpectedIdentityFields};

fn identity() -> ExpectedIdentity<'static> {
    const BYTES: [u8; 28] = [1; 28];
    ExpectedIdentity::new(&BYTES).expect("固定期望身份")
}

const TERMINAL_UID: [u8; 12] = [
    0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c,
];
const TERMINAL_NONCE: [u8; 16] = [
    0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90,
];
const SAVE_AND_RESTART_V3: [u8; 42] = [
    0x03, 0x20, 0x22, 0x00, 0x78, 0x56, 0x34, 0x12, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x2b, 0x2c, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c,
    0x8d, 0x8e, 0x8f, 0x90, 0x7b, 0x7d, 0xf7, 0x6f, 0xcb, 0x83,
];
const RESET_APPLICATION_V3: [u8; 40] = [
    0x03, 0x22, 0x20, 0x00, 0xef, 0xcd, 0xab, 0x89, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x2b, 0x2c, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c,
    0x8d, 0x8e, 0x8f, 0x90, 0x10, 0xfe, 0xcc, 0x5c,
];
const ENTER_UPDATE_V3: [u8; 40] = [
    0x03, 0x23, 0x20, 0x00, 0x0d, 0xf0, 0xad, 0x0b, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x2b, 0x2c, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c,
    0x8d, 0x8e, 0x8f, 0x90, 0x6f, 0xd8, 0x05, 0x69,
];

fn terminal_identity() -> ExpectedIdentityFields {
    ExpectedIdentityFields::new(TERMINAL_UID, TERMINAL_NONCE)
}

#[test]
fn save_and_restart_v3_has_an_independent_golden_vector() {
    let decoded = decode(&SAVE_AND_RESTART_V3, Direction::HostToFirmware).expect("手写向量有效");
    match decoded {
        Message::SaveAndRestart {
            query_id,
            identity,
            record,
        } => {
            assert_eq!(query_id, 0x1234_5678);
            assert_eq!(identity.fields(), terminal_identity());
            assert_eq!(record, b"{}");
        }
        other => assert_eq!(other.kind(), MessageKind::SaveAndRestart),
    }
    let identity_bytes = terminal_identity().to_bytes();
    let identity = ExpectedIdentity::new(&identity_bytes).expect("固定身份");
    let mut output = [0; SAVE_AND_RESTART_V3.len()];
    assert_eq!(
        encode(
            Message::SaveAndRestart {
                query_id: 0x1234_5678,
                identity,
                record: b"{}"
            },
            &mut output
        ),
        Ok(SAVE_AND_RESTART_V3.len())
    );
    assert_eq!(output, SAVE_AND_RESTART_V3);
}

#[test]
fn reset_application_v3_has_an_independent_golden_vector() {
    let decoded = decode(&RESET_APPLICATION_V3, Direction::HostToFirmware).expect("手写向量有效");
    match decoded {
        Message::ResetApplication { query_id, identity } => {
            assert_eq!(query_id, 0x89ab_cdef);
            assert_eq!(identity.fields(), terminal_identity());
        }
        other => assert_eq!(other.kind(), MessageKind::ResetApplication),
    }
    let identity_bytes = terminal_identity().to_bytes();
    let identity = ExpectedIdentity::new(&identity_bytes).expect("固定身份");
    let mut output = [0; RESET_APPLICATION_V3.len()];
    assert_eq!(
        encode(
            Message::ResetApplication {
                query_id: 0x89ab_cdef,
                identity
            },
            &mut output
        ),
        Ok(RESET_APPLICATION_V3.len())
    );
    assert_eq!(output, RESET_APPLICATION_V3);
}

#[test]
fn enter_update_v3_has_an_independent_golden_vector() {
    let decoded = decode(&ENTER_UPDATE_V3, Direction::HostToFirmware).expect("手写向量有效");
    match decoded {
        Message::EnterUpdate { query_id, identity } => {
            assert_eq!(query_id, 0x0bad_f00d);
            assert_eq!(identity.fields(), terminal_identity());
        }
        other => assert_eq!(other.kind(), MessageKind::EnterUpdate),
    }
    let identity_bytes = terminal_identity().to_bytes();
    let identity = ExpectedIdentity::new(&identity_bytes).expect("固定身份");
    let mut output = [0; ENTER_UPDATE_V3.len()];
    assert_eq!(
        encode(
            Message::EnterUpdate {
                query_id: 0x0bad_f00d,
                identity
            },
            &mut output
        ),
        Ok(ENTER_UPDATE_V3.len())
    );
    assert_eq!(output, ENTER_UPDATE_V3);
}

#[test]
fn contract_vectors_do_not_depend_on_encoder() {
    assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    assert_eq!(
        probe_prefix(&H[..3], Direction::HostToFirmware),
        Ok(Prefix::Incomplete)
    );
    assert_eq!(
        probe_prefix(&P10[..4], Direction::HostToFirmware),
        Ok(Prefix::Complete {
            length: 12,
            kind: MessageKind::Position
        })
    );
    assert_eq!(
        validate(&P10, Direction::HostToFirmware)
            .expect("独立合同向量有效")
            .kind,
        MessageKind::Position
    );
    assert!(
        matches!(decode(&P10, Direction::HostToFirmware), Ok(Message::Position(v)) if v == 10.0)
    );
}

#[test]
fn validated_message_keeps_the_checked_bytes_direction_and_info_together() {
    let validated =
        ValidatedMessage::validate(&P10, Direction::HostToFirmware).expect("独立合同向量有效");
    assert_eq!(validated.bytes(), P10);
    assert_eq!(validated.direction(), Direction::HostToFirmware);
    assert_eq!(validated.info().kind, MessageKind::Position);
    assert!(matches!(validated.decode(), Ok(Message::Position(v)) if v == 10.0));
}

#[test]
fn invalid_header_never_becomes_waiting_for_more() {
    let mut unknown = P10;
    unknown[1] = 7;
    assert_eq!(
        probe_prefix(&unknown[..4], Direction::HostToFirmware),
        Err(Error::UnknownKind)
    );
    let mut bad_length = P10;
    bad_length[2] = 3;
    assert_eq!(
        probe_prefix(&bad_length[..4], Direction::HostToFirmware),
        Err(Error::InvalidPayloadLength)
    );
    assert_eq!(
        probe_prefix(&H[..4], Direction::FirmwareToHost),
        Err(Error::WrongDirection)
    );
}

#[test]
fn full_message_requires_exact_length_and_crc() {
    let mut bad_crc = P10;
    bad_crc[8] ^= 1;
    assert_eq!(
        validate(&bad_crc, Direction::HostToFirmware),
        Err(Error::CrcMismatch)
    );
    assert_eq!(
        validate(&P10[..11], Direction::HostToFirmware),
        Err(Error::IncorrectMessageLength)
    );
    let mut excess = [0u8; 13];
    excess[..12].copy_from_slice(&P10);
    assert_eq!(
        validate(&excess, Direction::HostToFirmware),
        Err(Error::IncorrectMessageLength)
    );
}

#[test]
fn non_finite_target_is_rejected_even_with_valid_crc() {
    let mut nan = [3, 1, 4, 0, 0, 0, 0xc0, 0x7f, 0, 0, 0, 0];
    let crc = crc32c(&nan[..8]);
    nan[8..12].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
        validate(&nan, Direction::HostToFirmware),
        Err(Error::InvalidField)
    );
}

#[test]
fn bounded_encoding_preserves_borrowed_config_record() {
    let fields = ExpectedIdentityFields::new([1; 12], [2; 16]);
    let bytes = fields.to_bytes();
    let identity = ExpectedIdentity::new(&bytes).expect("固定身份");
    let record = b"{\"format_version\":1}";
    let mut output = [0u8; 64];
    let n = encode(
        Message::SaveAndRestart {
            query_id: 1,
            identity,
            record,
        },
        &mut output,
    )
    .expect("候选记录可编码");
    assert_eq!(n, 8 + 32 + record.len());
    assert!(
        matches!(decode(&output[..n], Direction::HostToFirmware), Ok(Message::SaveAndRestart { record: decoded, .. }) if decoded == record)
    );
    let mut short = [0u8; 4];
    assert_eq!(
        encode(Message::Heartbeat, &mut short),
        Err(Error::BufferTooSmall)
    );
    assert_eq!(MAX_MESSAGE_LEN, 16_440);
}

#[test]
fn v3_rejects_v2_before_kind_or_payload_are_admitted() {
    assert_eq!(
        probe_prefix(&[2, 6, 0, 0], Direction::HostToFirmware),
        Err(Error::UnsupportedVersion)
    );
}

#[test]
fn v3_has_no_legacy_maintenance_kinds() {
    for kind in [0x12, 0x21, 0x24, 0x95] {
        assert_eq!(MessageKind::from_byte(kind), None);
        assert_eq!(
            probe_prefix(&[3, kind, 0, 0], Direction::HostToFirmware),
            Err(Error::UnknownKind)
        );
    }
}

#[test]
fn mutation_messages_require_a_nonzero_query_and_run_nonce() {
    let mut bytes = [0u8; 28];
    bytes[..12].fill(1);
    let identity = ExpectedIdentity::new(&bytes).expect("尺寸正确");
    let mut output = [0u8; 64];
    assert_eq!(
        encode(
            Message::ResetApplication {
                query_id: 1,
                identity
            },
            &mut output
        ),
        Err(Error::InvalidField)
    );
    bytes[12..].fill(2);
    let identity = ExpectedIdentity::new(&bytes).expect("尺寸正确");
    assert_eq!(
        encode(
            Message::EnterUpdate {
                query_id: 0,
                identity
            },
            &mut output
        ),
        Err(Error::InvalidField)
    );
}

#[test]
fn every_kind_has_one_direction_and_one_exact_header_length() {
    let cases = [
        (MessageKind::Position, Direction::HostToFirmware, 4),
        (MessageKind::Velocity, Direction::HostToFirmware, 4),
        (MessageKind::Force, Direction::HostToFirmware, 4),
        (MessageKind::Impedance, Direction::HostToFirmware, 12),
        (MessageKind::Stop, Direction::HostToFirmware, 0),
        (MessageKind::Heartbeat, Direction::HostToFirmware, 0),
        (MessageKind::Query, Direction::HostToFirmware, 5),
        (MessageKind::ReadConfig, Direction::HostToFirmware, 5),
        (MessageKind::SaveAndRestart, Direction::HostToFirmware, 33),
        (MessageKind::ResetApplication, Direction::HostToFirmware, 32),
        (MessageKind::EnterUpdate, Direction::HostToFirmware, 32),
        (MessageKind::Telemetry, Direction::FirmwareToHost, 152),
        (MessageKind::Identity, Direction::FirmwareToHost, 84),
        (MessageKind::Status, Direction::FirmwareToHost, 152),
        (MessageKind::Measurements, Direction::FirmwareToHost, 100),
        (MessageKind::Diagnostics, Direction::FirmwareToHost, 44),
        (MessageKind::ConfigData, Direction::FirmwareToHost, 48),
        (MessageKind::DataUnavailable, Direction::FirmwareToHost, 40),
    ];
    for (kind, direction, payload) in cases {
        let header = [3, kind as u8, payload as u8, (payload >> 8) as u8];
        assert_eq!(
            probe_prefix(&header, direction),
            Ok(Prefix::Complete {
                length: payload + 8,
                kind
            })
        );
        let opposite = match direction {
            Direction::HostToFirmware => Direction::FirmwareToHost,
            Direction::FirmwareToHost => Direction::HostToFirmware,
        };
        assert_eq!(probe_prefix(&header, opposite), Err(Error::WrongDirection));
    }
}

#[test]
fn in_place_encoding_writes_header_and_crc_in_final_buffer() {
    let mut output = [0u8; 8];
    let length = encode_in_place(MessageKind::Heartbeat, 0, &mut output).expect("同一缓冲完成心跳");
    assert_eq!(length, 8);
    assert_eq!(output, H);
    assert!(matches!(
        decode(&output, Direction::HostToFirmware),
        Ok(Message::Heartbeat)
    ));
}

#[test]
fn every_host_message_encodes_as_a_complete_independent_message() {
    let messages = [
        Message::Position(1.0),
        Message::Velocity(-1.0),
        Message::Force(0.0),
        Message::Impedance {
            equilibrium_mm: 1.0,
            stiffness_n_per_mm: 2.0,
            damping_ns_per_mm: 3.0,
        },
        Message::Stop,
        Message::Heartbeat,
        Message::Query {
            query_id: 1,
            category: 0,
        },
        Message::ReadConfig {
            query_id: 1,
            view: 0,
        },
        Message::SaveAndRestart {
            query_id: 1,
            identity: identity(),
            record: b"{}",
        },
        Message::ResetApplication {
            query_id: 1,
            identity: identity(),
        },
        Message::EnterUpdate {
            query_id: 1,
            identity: identity(),
        },
    ];
    let mut output = [0u8; 64];
    for message in messages {
        let length = encode(message, &mut output).expect("全部主机类型可编码");
        assert_eq!(
            validate(&output[..length], Direction::HostToFirmware)
                .expect("完整编码通过格式校验")
                .kind,
            message.kind()
        );
    }
}

#[test]
fn type_lengths_reject_neighbouring_and_nested_lengths() {
    for length in [3, 5] {
        assert_eq!(
            probe_prefix(
                &[3, MessageKind::Position as u8, length, 0],
                Direction::HostToFirmware
            ),
            Err(Error::InvalidPayloadLength)
        );
    }
    for (kind, direction, invalid) in [
        (
            MessageKind::SaveAndRestart,
            Direction::HostToFirmware,
            32usize,
        ),
        (
            MessageKind::SaveAndRestart,
            Direction::HostToFirmware,
            16_417,
        ),
        (MessageKind::Identity, Direction::FirmwareToHost, 83),
        (MessageKind::Identity, Direction::FirmwareToHost, 505),
        (MessageKind::Diagnostics, Direction::FirmwareToHost, 43),
        (MessageKind::Diagnostics, Direction::FirmwareToHost, 941),
        (MessageKind::ConfigData, Direction::FirmwareToHost, 47),
    ] {
        assert_eq!(
            probe_prefix(
                &[3, kind as u8, invalid as u8, (invalid >> 8) as u8],
                direction
            ),
            Err(Error::InvalidPayloadLength)
        );
    }
    assert_eq!(
        probe_prefix(
            &[3, MessageKind::Diagnostics as u8, 100, 0],
            Direction::FirmwareToHost
        ),
        Ok(Prefix::Complete {
            length: 108,
            kind: MessageKind::Diagnostics
        })
    );
    assert_eq!(
        probe_prefix(
            &[3, MessageKind::Diagnostics as u8, 45, 0],
            Direction::FirmwareToHost
        ),
        Err(Error::InvalidPayloadLength)
    );
}
