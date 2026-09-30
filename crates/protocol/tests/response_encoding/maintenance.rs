// Copyright The eha_controller Contributors

use super::*;

#[test]
fn config_data_preserves_raw_invalid_utf8_at_the_maximum_message_length() {
    let fields = ConfigDataFields {
        sample: sample(8),
        view: ConfigView::UserRecord,
        record_state: ConfigRecordState::Incomplete,
        startup_source: StartupSource::User,
        fallback_reason: FallbackReason::ParseFailed,
        data_time_us: 99,
    };
    let mut output = vec![0xa5; 16_440];
    assert_eq!(ConfigDataWriter::required_len(16_384), Ok(16_440));
    assert_eq!(
        ConfigDataWriter::required_len(16_385),
        Err(Error::InvalidPayloadLength)
    );
    assert!(matches!(
        ConfigDataWriter::start(fields, 0, &mut [0; 55]),
        Err(Error::BufferTooSmall)
    ));
    let mut writer =
        ConfigDataWriter::start(fields, 16_384, &mut output).expect("最终TX窗口覆盖最大读回");
    let data = writer.data_mut();
    assert!(
        data.iter().all(|byte| *byte == 0xa5),
        "固定前缀不得改写记录窗口"
    );
    data.fill(0xff);
    data[..4].copy_from_slice(&[0xff, 0x80, 0, 0xfe]);
    let length = writer.finish().expect("原始记录无需UTF-8或JSON解析");
    assert_eq!(length, 16_440);
    assert_eq!(&output[..4], &[1, 0x94, 0x30, 0x40]);
    assert_eq!(
        &output[36..52],
        &[1, 2, 1, 3, 0, 64, 0, 0, 99, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(&output[52..56], &[0xff, 0x80, 0, 0xfe]);
    assert_eq!(output[16_435], 0xff);
    assert!(validate(&output, Direction::FirmwareToHost).is_ok());
}

#[test]
fn communication_settings_are_a_typed_twenty_byte_config_projection() {
    let fields = ConfigDataFields {
        sample: sample(14),
        view: ConfigView::Communication,
        record_state: ConfigRecordState::Complete,
        startup_source: StartupSource::User,
        fallback_reason: FallbackReason::None,
        data_time_us: 99,
    };
    let settings = CommunicationSettingsFields {
        active_can_node: 0x1122_3344,
        profile: CanProfile::Fd1m5m,
        host_heartbeat_hz: 10,
        telemetry_hz: 20,
        host_contact_max_age_ms: 30,
    };
    let mut output = [0xa5; 76];
    assert_eq!(CommunicationSettingsFields::encoded_len(), 76);
    assert_eq!(
        encode_communication_settings(fields, settings, &mut [0; 75]),
        Err(Error::BufferTooSmall)
    );
    let length = encode_communication_settings(fields, settings, &mut output)
        .expect("通信视图具有确定的二十字节投影");
    assert_eq!(length, 76);
    assert_eq!(&output[..4], &[1, 0x94, 68, 0]);
    assert_eq!(
        &output[36..52],
        &[3, 0, 1, 0, 20, 0, 0, 0, 99, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        &output[52..72],
        &[
            0x44, 0x33, 0x22, 0x11, 5, 0, 0, 0, 10, 0, 0, 0, 20, 0, 0, 0, 30, 0, 0, 0,
        ]
    );
    assert!(validate(&output, Direction::FirmwareToHost).is_ok());
    let decoded = decode(&output, Direction::FirmwareToHost).expect("固定投影可类型化读取");
    assert!(matches!(
        decoded,
        Message::Response(Response::ConfigData(_))
    ));
    let view = match decoded {
        Message::Response(Response::ConfigData(view)) => view,
        _ => return,
    };
    assert_eq!(view.fields(), fields);
    assert_eq!(view.communication_settings(), Some(settings));

    let wrong_view = ConfigDataFields {
        view: ConfigView::Factory,
        ..fields
    };
    assert_eq!(
        encode_communication_settings(wrong_view, settings, &mut output),
        Err(Error::InvalidField)
    );
    let wrong_state = ConfigDataFields {
        record_state: ConfigRecordState::Incomplete,
        ..fields
    };
    assert_eq!(
        encode_communication_settings(wrong_state, settings, &mut output),
        Err(Error::InvalidField)
    );
}

#[test]
fn operation_result_matches_the_contract_13_2_fixed_fields() {
    // 用稳定原始记录替代会随产品演变的 factory.json；CRC 来自独立合同实现。
    let record = b"{\"format_version\":1}";
    let fields = OperationResultFields {
        sample: SampleData {
            query_id: 10,
            run_nonce: [1; 16],
            snapshot_sequence: 9,
            snapshot_time_us: 1_002_000,
        },
        operation_id: 1,
        operation: MaintenanceOperation::SaveConfig,
        state: MaintenanceState::FirmwareStepComplete,
        phase: OperationPhase::StorageReplacement,
        evidence: OperationEvidence {
            storage_replace_succeeded: true,
            settled: true,
            ..Default::default()
        },
        reason: 0,
        next_actions: 0x10,
        constraints: Constraints {
            result_slot: true,
            ..Default::default()
        },
        started_time_us: 800_000,
        deadline_us: u64::MAX,
        finished_time_us: 900_000,
        content_length: record.len() as u32,
        content_crc32c: 0x93af_7345,
        revision: 3,
    };
    let mut output = [0xa5; 96];
    assert_eq!(OperationResultFields::encoded_len(), 96);
    assert_eq!(
        encode_operation_result(&fields, &mut [0; 95]),
        Err(Error::BufferTooSmall)
    );
    let length = encode_operation_result(&fields, &mut output).expect("已结清保存步骤符合合同");
    let expected_tail = [
        1, 0, 0, 0, 0, 0, 0, 0, 1, 3, 7, 0x22, 0, 0, 0x10, 0, 8, 0, 0, 0, 0, 0x35, 0x0c, 0, 0, 0,
        0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xa0, 0xbb, 0x0d, 0, 0, 0, 0, 0, 20,
        0, 0, 0, 0x45, 0x73, 0xaf, 0x93, 3, 0, 0, 0,
    ];
    assert_eq!(length, 96);
    assert_eq!(&output[..4], &[1, 0x95, 88, 0]);
    assert_eq!(&output[36..92], expected_tail);
    assert!(validate(&output, Direction::FirmwareToHost).is_ok());
}

#[test]
fn data_unavailable_has_a_named_subject_and_maintenance_association() {
    let fields = DataUnavailableFields {
        sample: sample(12),
        subject: UnavailableSubject::OperationResult,
        reason: 0x0307,
        next_actions: 1,
        retained: true,
        operation_id: 42,
    };
    let mut output = [0xa5; 56];
    assert_eq!(DataUnavailableFields::encoded_len(), 56);
    assert_eq!(
        encode_data_unavailable(&fields, &mut [0; 55]),
        Err(Error::BufferTooSmall)
    );
    let length = encode_data_unavailable(&fields, &mut output).expect("不可取得事实符合合同");
    assert_eq!(length, 56);
    assert_eq!(&output[..4], &[1, 0x96, 48, 0]);
    assert_eq!(
        &output[36..52],
        &[5, 7, 3, 1, 0, 1, 0, 0, 42, 0, 0, 0, 0, 0, 0, 0]
    );
    assert!(validate(&output, Direction::FirmwareToHost).is_ok());
}

#[test]
fn invalid_typed_content_never_returns_a_successful_message_length() {
    let fields = ConfigDataFields {
        sample: sample(3),
        view: ConfigView::Factory,
        record_state: ConfigRecordState::Complete,
        startup_source: StartupSource::Factory,
        fallback_reason: FallbackReason::None,
        data_time_us: u64::MAX,
    };
    let mut output = [0u8; 56];
    let writer = ConfigDataWriter::start(fields, 0, &mut output).expect("窗口本身有界");
    assert_eq!(writer.finish(), Err(Error::InvalidField));
}
