// Copyright The eha_controller Contributors

use super::*;

#[test]
fn observation_narrowing_preserves_quality_and_reports_representation_failure() {
    let state = ValueState::new(ValueResult::Available, SourceQuality::Faulted, true);
    let rounded = ValueFields::from_f64(16_777_217.0, state);
    assert_eq!(rounded, ValueFields::new(16_777_216.0, state));
    for value in [f64::MAX, -f64::MAX, 1e-50, -1e-50, f64::NAN, f64::INFINITY] {
        let field = ValueFields::from_f64(value, state);
        assert_eq!(field.value.to_bits(), 0, "失败值必须使用正零占位");
        assert_eq!(field.state.result(), ValueResult::CalculationFailed);
        assert_eq!(field.state.quality(), SourceQuality::Faulted);
        assert!(field.state.is_stale());
    }
    for value in [-0.0_f32, f32::from_bits(1)] {
        let field = ValueFields::from_f64(f64::from(value), state);
        assert_eq!(field.value.to_bits(), value.to_bits());
        assert_eq!(field.state, state);
    }
    let unavailable = ValueState::new(
        ValueResult::HistoryInsufficient,
        SourceQuality::Qualified,
        false,
    );
    assert_eq!(
        ValueFields::from_f64(f64::NAN, unavailable),
        ValueFields::new(0.0, unavailable)
    );
}

#[test]
fn measurements_match_the_contract_13_2_payload_vector() {
    let fields = MeasurementFields {
        sample: SampleData {
            query_id: 9,
            run_nonce: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            snapshot_sequence: 7,
            snapshot_time_us: 1_000_000,
        },
        values: MeasurementValues {
            position_mm: available(101.0),
            velocity_mm_s: ValueFields::new(
                0.0,
                ValueState::new(
                    ValueResult::HistoryInsufficient,
                    SourceQuality::Qualified,
                    false,
                ),
            ),
            raw_pressure_a_mpa: available(1.0),
            raw_pressure_b_mpa: available(2.0),
            filtered_pressure_a_mpa: available(1.0),
            filtered_pressure_b_mpa: ValueFields::new(
                0.0,
                ValueState::new(
                    ValueResult::CalculationFailed,
                    SourceQuality::Qualified,
                    false,
                ),
            ),
            main_force_n: ValueFields::new(
                0.0,
                ValueState::new(
                    ValueResult::CalculationFailed,
                    SourceQuality::Qualified,
                    false,
                ),
            ),
            protection_force_n: available(188.5),
        },
        position_age_us: 100,
        velocity_age_us: 100,
        pressure_pair_age_us: 200,
        position_reference_count: 10_000,
        effective_area_mm2: 188.5,
        positive_force_channel: PositiveForceChannel::ElectricalB,
        reference_state: true,
        model_state: ModelState::Available,
        pressure_batch_sequence: 20,
    };
    let mut output = [0xa5; 108];
    assert_eq!(MeasurementFields::encoded_len(), 108);
    assert_eq!(
        encode_measurements(&fields, &mut [0; 107]),
        Err(Error::BufferTooSmall)
    );
    let length = encode_measurements(&fields, &mut output).expect("合同样例可编码");
    assert_eq!(length, 108);
    assert_eq!(&output[..4], &[1, MessageKind::Measurements as u8, 100, 0]);
    assert_eq!(
        &output[4..36],
        &[
            9, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 7, 0, 0, 0, 0x40,
            0x42, 0x0f, 0, 0, 0, 0, 0
        ]
    );
    assert_eq!(
        &output[36..68],
        &[
            0, 0, 0xca, 0x42, 0, 0, 0, 0, 0, 0, 0x80, 0x3f, 0, 0, 0, 0x40, 0, 0, 0x80, 0x3f, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0x80, 0x3c, 0x43,
        ]
    );
    assert_eq!(
        &output[68..104],
        &[
            9, 11, 9, 9, 9, 10, 10, 9, 100, 0, 0, 0, 100, 0, 0, 0, 200, 0, 0, 0, 0x10, 0x27, 0, 0,
            0, 0x80, 0x3c, 0x43, 2, 1, 1, 0, 20, 0, 0, 0
        ]
    );
    assert!(validate(&output, Direction::FirmwareToHost).is_ok());
}

#[test]
fn telemetry_and_status_write_target_reference_and_independent_ages() {
    let fields = TelemetryFields {
        sample: sample(0),
        decision_age_us: 21,
        target_mode: TargetMode::Position,
        target_ingress: TargetIngress::Can,
        facts: TelemetryFacts {
            output_allowed: true,
            reference_limited: true,
            ..Default::default()
        },
        target_values: [50.0, 0.0, 0.0],
        can_adoption_blockers: 0x0102_0304,
        usb_adoption_blockers: 0x1112_1314,
        output_blockers: 0,
        last_end_reason: 0x0400,
        limits: Default::default(),
        main_values: [
            available(1.0),
            available(2.0),
            available(3.0),
            available(4.0),
            available(5.0),
        ],
        position_age_us: 31,
        velocity_age_us: 32,
        pressure_pair_age_us: 33,
        reference: available(44.0),
        reference_kind: ReferenceKind::PositionMm,
        candidate_rpm: available(55.0),
        last_submitted_rpm: available(66.0),
        submitted_age_us: 34,
        axis_state_raw: 0x1122_3344,
        axis_error_raw: 0x5566_7788,
        driver_age_us: 35,
        driver_state: DriverState {
            has_status: true,
            qualified: true,
            ..Default::default()
        },
        desired_axis: DesiredAxis::ClosedLoop,
        can_heartbeat_age_us: 36,
        usb_heartbeat_age_us: 37,
        can_contact: ContactState::Active,
        usb_contact: ContactState::Expired,
    };
    let mut telemetry = [0xa5; 160];
    assert_eq!(TelemetryFields::encoded_len(), 160);
    assert_eq!(
        encode_telemetry(&fields, &mut [0; 159]),
        Err(Error::BufferTooSmall)
    );
    assert_eq!(
        encode_status(&fields, &mut [0; 159]),
        Err(Error::BufferTooSmall)
    );
    let length = encode_telemetry(&fields, &mut telemetry).expect("遥测字段符合合同");
    assert_eq!(length, 160);
    assert_eq!(&telemetry[..4], &[1, 0x80, 152, 0]);
    assert_eq!(&telemetry[36..40], &[21, 0, 0, 0]);
    assert_eq!(&telemetry[40..44], &[1, 1, 3, 0]);
    assert_eq!(
        &telemetry[44..56],
        &[0, 0, 0x48, 0x42, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        &telemetry[56..68],
        &[4, 3, 2, 1, 0x14, 0x13, 0x12, 0x11, 0, 0, 0, 0]
    );
    assert_eq!(
        &telemetry[100..112],
        &[31, 0, 0, 0, 32, 0, 0, 0, 33, 0, 0, 0]
    );
    assert_eq!(
        &telemetry[112..128],
        &[
            0, 0, 0x30, 0x42, 1, 9, 9, 9, 0, 0, 0x5c, 0x42, 0, 0, 0x84, 0x42
        ]
    );
    assert_eq!(
        &telemetry[128..140],
        &[34, 0, 0, 0, 0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55]
    );
    assert_eq!(
        &telemetry[140..156],
        &[35, 0, 0, 0, 3, 2, 36, 0, 0, 0, 37, 0, 0, 0, 1, 2]
    );
    assert!(validate(&telemetry, Direction::FirmwareToHost).is_ok());
    let mut status = [0xa5; 160];
    let mut status_fields = fields;
    status_fields.sample.query_id = 19;
    encode_status(&status_fields, &mut status).expect("查询状态要求非零 query_id");
    assert_eq!(&status[..8], &[1, 0x91, 152, 0, 19, 0, 0, 0]);
    assert_eq!(&status[36..156], &telemetry[36..156]);
}

#[test]
fn identity_accepts_eight_bounded_strings_at_its_maximum_length() {
    let fields = IdentityFields {
        sample: sample(4),
        uid: [7; 12],
        config_source: StartupSource::User,
        fallback_reason: FallbackReason::None,
        run_identity: RunIdentity::Random,
        update_route: UpdateRoute::Present,
        active_can_node: 0x1122_3344,
        active_can_profile: CanProfile::Fd1m8m,
        host_heartbeat_hz: 10,
        telemetry_hz: 20,
        host_contact_max_age_ms: 30,
        config_format_version: 40,
        build_evidence: EvidenceState::Available,
        odrive_evidence: EvidenceState::Available,
        brt27_evidence: EvidenceState::Available,
        high_water_operation_id: 60,
        retained_operation_id: 50,
        text: IdentityText {
            version: "12345678901234567890123456789012",
            source_revision: "1234567890123456789012345678901234567890",
            build_information: "123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456",
            odrive_binding: "1234567890123456789012345678901234567890123456789012345678901234",
            odrive_observed_version: "1234567890123456789012345678901234567890123456789012345678901234",
            brt27_binding: "1234567890123456789012345678901234567890123456789012345678901234",
            brt27_observed_version: "12345678901234567890123456789012",
            update_binding: "1234567890123456789012345678",
        },
    };
    let mut output = [0xa5; 528];
    assert_eq!(fields.encoded_len(), Ok(528));
    assert_eq!(
        encode_identity(&fields, &mut [0; 527]),
        Err(Error::BufferTooSmall)
    );
    let length = encode_identity(&fields, &mut output).expect("八个最大字符串可编码");
    assert_eq!(length, 528);
    assert_eq!(&output[..4], &[1, 0x90, 0x08, 0x02]);
    assert_eq!(&output[36..48], &[7; 12]);
    assert_eq!(&output[48..57], &[1, 0, 1, 1, 0x44, 0x33, 0x22, 0x11, 6]);
    assert_eq!(
        &output[60..76],
        &[10, 0, 0, 0, 20, 0, 0, 0, 30, 0, 0, 0, 40, 0, 0, 0]
    );
    assert_eq!(&output[76..80], &[1, 1, 1, 0]);
    assert_eq!(
        &output[80..96],
        &[60, 0, 0, 0, 0, 0, 0, 0, 50, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(&output[96..129], b"\x2012345678901234567890123456789012");
    assert_eq!(output[129], 40);
    assert_eq!(output[495], 28);
    assert_eq!(&output[496..524], b"1234567890123456789012345678");
    assert!(validate(&output, Direction::FirmwareToHost).is_ok());
}

#[test]
fn diagnostics_supports_sixteen_entries_and_rejects_a_seventeenth() {
    let entry = DiagnosticFields {
        domain: DiagnosticDomain::Maintenance,
        object: DiagnosticObject::UserRecord,
        impact: DiagnosticImpact {
            limits_maintenance: true,
            current: true,
            ..Default::default()
        },
        phase: OperationPhase::ActualReadback,
        reason: 0x0204,
        next_actions: 1 << 4,
        event_time_us: 11,
        started_time_us: 12,
        deadline_us: 13,
        operation_id: 14,
        native_code: 7,
        native_domain: NativeDomain::Flash,
        evidence: DiagnosticEvidence {
            actual_readback: true,
            ..Default::default()
        },
        detail_value: available(15.0),
        detail_unit: DetailUnit::Bytes,
        constraints: Constraints {
            user_storage: true,
            result_slot: true,
            ..Default::default()
        },
        occurrences: 16,
        missing_evidence: MissingEvidence {
            same_device_comparison: true,
            ..Default::default()
        },
    };
    let entries = [entry; 16];
    let fields = DiagnosticsFields {
        sample: sample(2),
        overflow: true,
        entries: &entries,
    };
    let mut output = [0xa5; 1068];
    assert_eq!(fields.encoded_len(), Ok(1068));
    assert_eq!(
        encode_diagnostics(&fields, &mut [0; 1067]),
        Err(Error::BufferTooSmall)
    );
    let length = encode_diagnostics(&fields, &mut output).expect("合同上限16条诊断");
    assert_eq!(length, 1068);
    assert_eq!(&output[..4], &[1, 0x93, 0x24, 0x04]);
    assert_eq!(&output[36..40], &[16, 0, 1, 0]);
    assert_eq!(
        &output[40..104],
        &[
            8, 8, 0x48, 8, 4, 2, 0x10, 0, 11, 0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 13, 0,
            0, 0, 0, 0, 0, 0, 14, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 6, 0x10, 0, 0, 0, 0, 0x70, 0x41,
            9, 7, 9, 0, 16, 0, 0, 0, 8, 0, 0, 0
        ]
    );
    assert_eq!(&output[1000..1064], &output[40..104]);
    assert!(validate(&output, Direction::FirmwareToHost).is_ok());
    let too_many = [entry; 17];
    let mut oversized = [0u8; 1132];
    assert_eq!(
        encode_diagnostics(
            &DiagnosticsFields {
                sample: sample(2),
                overflow: true,
                entries: &too_many
            },
            &mut oversized
        ),
        Err(Error::InvalidPayloadLength)
    );
}
