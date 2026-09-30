// Copyright The eha_controller Contributors

use super::*;

fn wire(kind: MessageKind, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(payload.len() + 8);
    message.extend_from_slice(&[
        1,
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
    telemetry[136..140].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[96..100].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[100..104].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[104..108].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[124..128].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[142..146].copy_from_slice(&u32::MAX.to_le_bytes());
    telemetry[146..150].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut identity = [0u8; 100];
    identity[..32].copy_from_slice(&response_sample(1));
    identity[32] = 1;
    let mut measurement = [0u8; 100];
    measurement[..32].copy_from_slice(&response_sample(1));
    measurement[72..76].copy_from_slice(&u32::MAX.to_le_bytes());
    measurement[76..80].copy_from_slice(&u32::MAX.to_le_bytes());
    measurement[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut diagnostics = [0u8; 36];
    diagnostics[..32].copy_from_slice(&response_sample(1));
    let mut config = [0u8; 48];
    config[..32].copy_from_slice(&response_sample(1));
    config[32] = 1;
    config[33] = 1;
    config[34] = 1;
    config[40..48].copy_from_slice(&1u64.to_le_bytes());
    let mut result = [0u8; 88];
    result[..32].copy_from_slice(&response_sample(0));
    result[32..40].copy_from_slice(&1u64.to_le_bytes());
    result[40] = 1;
    result[42] = 1;
    result[43] = 0x21;
    result[52..60].copy_from_slice(&u64::MAX.to_le_bytes());
    result[60..68].copy_from_slice(&u64::MAX.to_le_bytes());
    result[68..76].copy_from_slice(&1u64.to_le_bytes());
    let mut unavailable = [0u8; 48];
    unavailable[..32].copy_from_slice(&response_sample(1));
    unavailable[32] = 0;
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
        (MessageKind::OperationResult, &result[..]),
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
                assert_eq!(view.fields().sample.snapshot_time_us, 1);
            }
            Message::Response(Response::Identity(view)) => {
                assert_eq!(
                    view.fields().config_source,
                    protocol::responses::StartupSource::Unavailable
                );
            }
            Message::Response(Response::Measurements(view)) => {
                assert_eq!(
                    view.fields().model_state,
                    protocol::responses::ModelState::Unavailable
                );
            }
            Message::Response(Response::Diagnostics(view)) => {
                assert!(view.entries().next().is_none())
            }
            Message::Response(Response::ConfigData(view)) => {
                assert_eq!(
                    view.fields().record_state,
                    protocol::responses::ConfigRecordState::Empty
                );
            }
            Message::Response(Response::OperationResult(view)) => {
                assert_eq!(
                    view.fields().operation,
                    protocol::responses::MaintenanceOperation::SaveConfig
                );
            }
            Message::Response(Response::DataUnavailable(view)) => {
                assert_eq!(
                    view.fields().subject,
                    protocol::responses::UnavailableSubject::Identity
                );
            }
            _ => assert_eq!("unexpected decoded kind", "expected response"),
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
    payload[32..34].copy_from_slice(&1u16.to_le_bytes());
    // Entry offset16 is started_time_us and offset24 is deadline_us.
    payload[52..60].copy_from_slice(&u64::MAX.to_le_bytes());
    let message = wire(MessageKind::Diagnostics, &payload);
    assert_eq!(
        validate(&message, Direction::FirmwareToHost),
        Err(Error::InvalidField)
    );
}

#[test]
fn all_zero_uid_is_a_valid_wire_value_not_a_format_error() {
    let mut payload = [0u8; 100];
    payload[..32].copy_from_slice(&response_sample(1));
    // run_identity=0 and all-zero nonce consistently express unavailable runtime identity.
    let message = wire(MessageKind::Identity, &payload);
    assert_eq!(
        validate(&message, Direction::FirmwareToHost)
            .expect("格式层不判UID来源")
            .kind,
        MessageKind::Identity
    );
}

#[test]
fn reset_operation_cannot_claim_storage_evidence() {
    let mut payload = [0u8; 88];
    payload[..32].copy_from_slice(&response_sample(0));
    payload[24..32].copy_from_slice(&100u64.to_le_bytes());
    payload[32..40].copy_from_slice(&1u64.to_le_bytes());
    payload[40] = 3;
    payload[41] = 5;
    payload[42] = 1;
    payload[43] = 0x02;
    payload[44..46].copy_from_slice(&1u16.to_le_bytes());
    payload[48..52].copy_from_slice(&8u32.to_le_bytes());
    payload[52..60].copy_from_slice(&1u64.to_le_bytes());
    payload[60..68].copy_from_slice(&100u64.to_le_bytes());
    payload[68..76].copy_from_slice(&u64::MAX.to_le_bytes());
    payload[84..88].copy_from_slice(&1u32.to_le_bytes());
    let message = wire(MessageKind::OperationResult, &payload);
    assert_eq!(
        validate(&message, Direction::FirmwareToHost),
        Err(Error::InvalidField)
    );
}

#[test]
fn identity_text_rejects_leading_utf8_bom_with_a_correct_crc() {
    let mut payload = [0u8; 103];
    payload[..32].copy_from_slice(&response_sample(1));
    payload[72] = 1;
    payload[92] = 3;
    payload[93..96].copy_from_slice(&[0xef, 0xbb, 0xbf]);
    let message = wire(MessageKind::Identity, &payload);
    assert_eq!(
        validate(&message, Direction::FirmwareToHost),
        Err(Error::InvalidField)
    );
}

#[test]
fn operation_not_started_may_report_a_missing_original_result() {
    let mut payload = [0u8; 88];
    payload[..32].copy_from_slice(&response_sample(0));
    payload[32..40].copy_from_slice(&1u64.to_le_bytes());
    payload[40] = 1;
    payload[41] = 4;
    payload[42] = 6;
    payload[43] = 0x11;
    payload[44..46].copy_from_slice(&0x11u16.to_le_bytes());
    payload[48..52].copy_from_slice(&8u32.to_le_bytes());
    payload[52..60].copy_from_slice(&1u64.to_le_bytes());
    payload[60..68].copy_from_slice(&u64::MAX.to_le_bytes());
    payload[68..76].copy_from_slice(&u64::MAX.to_le_bytes());
    payload[84..88].copy_from_slice(&1u32.to_le_bytes());
    let message = wire(MessageKind::OperationResult, &payload);
    assert_eq!(
        validate(&message, Direction::FirmwareToHost)
            .expect("bit0与bit4可共同表达未开始且缺原结果")
            .kind,
        MessageKind::OperationResult
    );
}
