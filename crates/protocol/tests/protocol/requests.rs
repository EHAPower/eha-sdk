// Copyright The eha_controller Contributors

use super::*;

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
    let mut nan = [1, 1, 4, 0, 0, 0, 0xc0, 0x7f, 0, 0, 0, 0, 0];
    let crc = crc32c(&nan[..8]);
    nan[8..12].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
        validate(&nan[..12], Direction::HostToFirmware),
        Err(Error::InvalidField)
    );
}

#[test]
fn bounded_encoding_preserves_borrowed_config_record() {
    let key = [
        1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        0, 0, 0, 0, 0, 0, 0,
    ];
    let record = b"{\"format_version\":1}";
    let message = Message::SaveConfig {
        key: protocol::OperationKey::new(&key).expect("固定36字节键"),
        record,
    };
    let mut output = [0u8; 64];
    let n = encode(message, &mut output).expect("候选记录可编码");
    assert_eq!(n, 8 + 36 + record.len());
    assert!(
        matches!(decode(&output[..n], Direction::HostToFirmware), Ok(Message::SaveConfig { record: decoded, .. }) if decoded == record)
    );
    let mut short = [0u8; 4];
    assert_eq!(
        encode(Message::Heartbeat, &mut short),
        Err(Error::BufferTooSmall)
    );
    assert_eq!(MAX_MESSAGE_LEN, 16_440);
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
        (MessageKind::ReadResult, Direction::HostToFirmware, 40),
        (MessageKind::SaveConfig, Direction::HostToFirmware, 37),
        (MessageKind::RestoreFactory, Direction::HostToFirmware, 36),
        (MessageKind::ResetApplication, Direction::HostToFirmware, 36),
        (MessageKind::EnterUpdate, Direction::HostToFirmware, 36),
        (MessageKind::ReleaseResult, Direction::HostToFirmware, 40),
        (MessageKind::Telemetry, Direction::FirmwareToHost, 152),
        (MessageKind::Identity, Direction::FirmwareToHost, 100),
        (MessageKind::Status, Direction::FirmwareToHost, 152),
        (MessageKind::Measurements, Direction::FirmwareToHost, 100),
        (MessageKind::Diagnostics, Direction::FirmwareToHost, 36),
        (MessageKind::ConfigData, Direction::FirmwareToHost, 48),
        (MessageKind::OperationResult, Direction::FirmwareToHost, 88),
        (MessageKind::DataUnavailable, Direction::FirmwareToHost, 48),
    ];
    for (kind, direction, payload) in cases {
        let header = [1, kind as u8, payload as u8, (payload >> 8) as u8];
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
    let key_bytes = [
        1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        0, 0, 0, 0, 0, 0, 0,
    ];
    let key = protocol::OperationKey::new(&key_bytes).expect("固定维护键");
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
        Message::ReadResult { query_id: 1, key },
        Message::SaveConfig { key, record: b"{}" },
        Message::Maintenance {
            kind: MessageKind::RestoreFactory,
            key,
        },
        Message::Maintenance {
            kind: MessageKind::ResetApplication,
            key,
        },
        Message::Maintenance {
            kind: MessageKind::EnterUpdate,
            key,
        },
        Message::ReleaseResult { key, revision: 1 },
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
    for (kind, direction, valid) in [(MessageKind::Position, Direction::HostToFirmware, 4usize)] {
        for length in [valid - 1, valid + 1] {
            let header = [1, kind as u8, length as u8, (length >> 8) as u8];
            assert_eq!(
                probe_prefix(&header, direction),
                Err(Error::InvalidPayloadLength)
            );
        }
    }
    for (kind, direction, invalid) in [
        (MessageKind::SaveConfig, Direction::HostToFirmware, 36usize),
        (MessageKind::SaveConfig, Direction::HostToFirmware, 16_421),
        (MessageKind::Identity, Direction::FirmwareToHost, 99),
        (MessageKind::Identity, Direction::FirmwareToHost, 521),
        (MessageKind::Diagnostics, Direction::FirmwareToHost, 35),
        (MessageKind::Diagnostics, Direction::FirmwareToHost, 1061),
        (MessageKind::ConfigData, Direction::FirmwareToHost, 47),
    ] {
        let header = [1, kind as u8, invalid as u8, (invalid >> 8) as u8];
        assert_eq!(
            probe_prefix(&header, direction),
            Err(Error::InvalidPayloadLength)
        );
    }
    let header = [1, MessageKind::Diagnostics as u8, 100, 0];
    assert_eq!(
        probe_prefix(&header, Direction::FirmwareToHost),
        Ok(Prefix::Complete {
            length: 108,
            kind: MessageKind::Diagnostics
        })
    );
    let malformed_nested = [1, MessageKind::Diagnostics as u8, 37, 0];
    assert_eq!(
        probe_prefix(&malformed_nested, Direction::FirmwareToHost),
        Err(Error::InvalidPayloadLength)
    );
}
