// Copyright The eha-sdk Contributors

use super::*;

fn wire(kind: MessageKind, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(payload.len() + 8);
    message.extend_from_slice(&[
        3,
        kind as u8,
        payload.len() as u8,
        (payload.len() >> 8) as u8,
    ]);
    message.extend_from_slice(payload);
    message.extend_from_slice(&crc32c(&message).to_le_bytes());
    message
}

fn response_sample(query_id: u32) -> [u8; 32] {
    let mut sample = [0u8; 32];
    sample[..4].copy_from_slice(&query_id.to_le_bytes());
    sample[24..].copy_from_slice(&1u64.to_le_bytes());
    sample
}

#[test]
fn independent_minimal_response_vectors_cover_every_firmware_type() {
    let mut telemetry = [0u8; 152];
    telemetry[..32].copy_from_slice(&response_sample(0));
    telemetry[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[96..100].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[100..104].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[104..108].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[124..128].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[136..140].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[142..146].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[146..150].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut identity = [0u8; 84];
    identity[..32].copy_from_slice(&response_sample(1));
    identity[44] = 1;
    let mut measurement = [0u8; 100];
    measurement[..32].copy_from_slice(&response_sample(1));
    measurement[72..76].copy_from_slice(&u32::MAX.to_le_bytes());
    measurement[76..80].copy_from_slice(&u32::MAX.to_le_bytes());
    measurement[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut diagnostics = [0u8; 44];
    diagnostics[..32].copy_from_slice(&response_sample(1));
    diagnostics[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
    diagnostics[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut config = [0u8; 48];
    config[..32].copy_from_slice(&response_sample(1));
    config[32] = 1;
    config[33] = 1;
    config[34] = 1;
    config[40..48].copy_from_slice(&1u64.to_le_bytes());
    let mut unavailable = [0u8; 40];
    unavailable[..32].copy_from_slice(&response_sample(1));
    unavailable[33..35].copy_from_slice(&1u16.to_le_bytes());
    for (kind, payload) in [
        (MessageKind::Telemetry, &telemetry[..]),
        (
            MessageKind::Status,
            &{
                let mut s = telemetry;
                s[..4].copy_from_slice(&1u32.to_le_bytes());
                s
            }[..],
        ),
        (MessageKind::Identity, &identity[..]),
        (MessageKind::Measurements, &measurement[..]),
        (MessageKind::Diagnostics, &diagnostics[..]),
        (MessageKind::ConfigData, &config[..]),
        (MessageKind::DataUnavailable, &unavailable[..]),
    ] {
        let message = wire(kind, payload);
        assert_eq!(
            validate(&message, Direction::FirmwareToHost)
                .expect("独立响应向量有效")
                .kind,
            kind
        );
        match decode(&message, Direction::FirmwareToHost).expect("已校验消息可具名解码") {
            Message::Response(Response::Telemetry(view))
            | Message::Response(Response::Status(view)) => {
                assert_eq!(view.fields().sample.snapshot_time_us, 1)
            }
            Message::Response(Response::Identity(view)) => assert_eq!(
                view.fields().config_source,
                protocol::responses::StartupSource::User
            ),
            Message::Response(Response::Measurements(view)) => assert_eq!(
                view.fields().model_state,
                protocol::responses::ModelState::Unavailable
            ),
            Message::Response(Response::Diagnostics(view)) => {
                assert_eq!(view.decision_age_us(), u32::MAX);
                assert_eq!(view.device_evidence_age_us(), u32::MAX);
                assert!(view.entries().next().is_none())
            }
            Message::Response(Response::ConfigData(view)) => assert_eq!(
                view.fields().record_state,
                protocol::responses::ConfigRecordState::Empty
            ),
            Message::Response(Response::DataUnavailable(view)) => assert_eq!(
                view.fields().subject,
                protocol::responses::UnavailableSubject::Identity
            ),
            other => assert!(matches!(other, Message::Response(_)), "必须为固件响应"),
        }
        let mut corrupt = message.clone();
        corrupt[4] ^= 1;
        assert_eq!(
            validate(&corrupt, Direction::FirmwareToHost),
            Err(Error::CrcMismatch)
        );
    }
}

#[test]
fn diagnostic_without_operation_rejects_a_real_deadline() {
    let mut payload = [0u8; 100];
    payload[..32].copy_from_slice(&response_sample(1));
    payload[40..42].copy_from_slice(&1u16.to_le_bytes());
    payload[60..68].copy_from_slice(&u64::MAX.to_le_bytes());
    payload[68..76].copy_from_slice(&1u64.to_le_bytes());
    assert_eq!(
        validate(
            &wire(MessageKind::Diagnostics, &payload),
            Direction::FirmwareToHost
        ),
        Err(Error::InvalidField)
    );
}

#[test]
fn all_zero_uid_is_a_valid_wire_value_not_a_format_error() {
    let mut payload = [0u8; 84];
    payload[..32].copy_from_slice(&response_sample(1));
    let message = wire(MessageKind::Identity, &payload);
    assert_eq!(
        validate(&message, Direction::FirmwareToHost)
            .expect("格式层不判UID来源")
            .kind,
        MessageKind::Identity
    );
}

#[test]
fn identity_text_rejects_leading_utf8_bom_with_a_correct_crc() {
    let mut payload = [0u8; 87];
    payload[..32].copy_from_slice(&response_sample(1));
    payload[72] = 1;
    payload[76] = 3;
    payload[77..80].copy_from_slice(&[0xef, 0xbb, 0xbf]);
    assert_eq!(
        validate(
            &wire(MessageKind::Identity, &payload),
            Direction::FirmwareToHost
        ),
        Err(Error::InvalidField)
    );
}

#[test]
fn data_unavailable_reserved_bytes_are_rejected_after_crc_is_recomputed() {
    let mut payload = [0u8; 40];
    payload[..32].copy_from_slice(&response_sample(1));
    payload[37] = 1;
    assert_eq!(
        validate(
            &wire(MessageKind::DataUnavailable, &payload),
            Direction::FirmwareToHost
        ),
        Err(Error::InvalidField)
    );
}

fn invalid_diagnostics(mutator: fn(&mut [u8])) {
    let mut payload = [0u8; 100];
    payload[..32].copy_from_slice(&response_sample(1));
    payload[40..42].copy_from_slice(&1u16.to_le_bytes());
    let entry = &mut payload[44..];
    entry[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    entry[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(
        validate(
            &wire(MessageKind::Diagnostics, &payload),
            Direction::FirmwareToHost
        )
        .is_ok()
    );
    mutator(&mut payload);
    assert_eq!(
        validate(
            &wire(MessageKind::Diagnostics, &payload),
            Direction::FirmwareToHost
        ),
        Err(Error::InvalidField)
    );
}

#[test]
fn diagnostics_rejects_retired_bits_phases_and_reasons_after_crc_is_recomputed() {
    invalid_diagnostics(|payload| payload[44 + 37] = 1 << 3);
    invalid_diagnostics(|payload| payload[44 + 46] = 1);
    invalid_diagnostics(|payload| payload[44 + 52] = 1 << 2);
    invalid_diagnostics(|payload| payload[44 + 3] = 6);
    invalid_diagnostics(|payload| {
        payload[44 + 4..44 + 6].copy_from_slice(&0x0205u16.to_le_bytes())
    });
}
