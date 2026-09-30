// Copyright The eha_controller Contributors
#![allow(clippy::panic)]

use eha_sdk::{
    protocol::{Direction, Message, Response, SampleData, decode, responses},
    session::{IdentityEvent, QueryKind, Session, SessionError},
};

fn decode_response(bytes: &[u8]) -> Response<'_> {
    match decode(bytes, Direction::FirmwareToHost).expect("test response is contract-valid") {
        Message::Response(response) => response,
        _ => panic!("expected firmware response"),
    }
}

fn identity_bytes(uid: [u8; 12], nonce: [u8; 16], high_water: u64) -> Vec<u8> {
    let mut bytes = vec![0; 528];
    let fields = responses::IdentityFields {
        sample: SampleData {
            query_id: 1,
            run_nonce: nonce,
            snapshot_sequence: 1,
            snapshot_time_us: 1,
        },
        uid,
        config_source: responses::StartupSource::User,
        fallback_reason: responses::FallbackReason::None,
        run_identity: responses::RunIdentity::Random,
        update_route: responses::UpdateRoute::Absent,
        active_can_node: 1,
        active_can_profile: responses::CanProfile::Fd1m2m,
        host_heartbeat_hz: 10,
        telemetry_hz: 20,
        host_contact_max_age_ms: 300,
        config_format_version: 1,
        build_evidence: responses::EvidenceState::Available,
        odrive_evidence: responses::EvidenceState::Unavailable,
        brt27_evidence: responses::EvidenceState::Unavailable,
        high_water_operation_id: high_water,
        retained_operation_id: 0,
        text: responses::IdentityText {
            version: "test",
            source_revision: "",
            build_information: "",
            odrive_binding: "",
            odrive_observed_version: "",
            brt27_binding: "",
            brt27_observed_version: "",
            update_binding: "",
        },
    };
    let length = responses::encode_identity(&fields, &mut bytes).expect("identity encodes");
    bytes.truncate(length);
    bytes
}

fn unavailable_bytes(
    query_id: u32,
    nonce: [u8; 16],
    subject: responses::UnavailableSubject,
    operation_id: u64,
) -> Vec<u8> {
    let fields = responses::DataUnavailableFields {
        sample: SampleData {
            query_id,
            run_nonce: nonce,
            snapshot_sequence: 2,
            snapshot_time_us: 2,
        },
        subject,
        reason: 1,
        next_actions: 0,
        retained: operation_id != 0,
        operation_id,
    };
    let mut bytes = [0; 56];
    let length =
        responses::encode_data_unavailable(&fields, &mut bytes).expect("unavailable encodes");
    bytes[..length].to_vec()
}

fn operation_result_bytes(query_id: u32, nonce: [u8; 16], operation_id: u64) -> Vec<u8> {
    let fields = responses::OperationResultFields {
        sample: SampleData {
            query_id,
            run_nonce: nonce,
            snapshot_sequence: 3,
            snapshot_time_us: 10,
        },
        operation_id,
        operation: responses::MaintenanceOperation::SaveConfig,
        state: responses::MaintenanceState::FirmwareStepComplete,
        phase: responses::OperationPhase::StorageReplacement,
        evidence: responses::OperationEvidence {
            storage_replace_succeeded: true,
            settled: true,
            ..Default::default()
        },
        reason: 0,
        next_actions: 0,
        constraints: responses::Constraints {
            result_slot: true,
            ..Default::default()
        },
        started_time_us: 1,
        deadline_us: 2,
        finished_time_us: 2,
        content_length: 0,
        content_crc32c: 0,
        revision: 1,
    };
    let mut bytes = [0; responses::OperationResultFields::encoded_len()];
    let length =
        responses::encode_operation_result(&fields, &mut bytes).expect("operation result encodes");
    bytes[..length].to_vec()
}

#[test]
fn query_association_rejects_wrong_subject_and_late_query_id() {
    let nonce = [2; 16];
    let identity = identity_bytes([1; 12], nonce, 0);
    let Response::Identity(identity) = decode_response(&identity) else {
        panic!("identity response")
    };
    let mut session = Session::new();
    assert!(matches!(
        session.observe_identity(&identity),
        Ok(IdentityEvent::Established(_))
    ));

    let first = session
        .next_query(QueryKind::Status)
        .expect("bound status query");
    let second = session.next_query(QueryKind::Status).expect("new query id");
    assert_eq!(first.query_id + 1, second.query_id);
    let late = unavailable_bytes(
        first.query_id,
        nonce,
        responses::UnavailableSubject::Status,
        0,
    );
    let late = decode_response(&late);
    assert!(first.matches(&late));
    assert!(
        !second.matches(&late),
        "late response must not satisfy the new request"
    );

    let wrong_subject = unavailable_bytes(
        second.query_id,
        nonce,
        responses::UnavailableSubject::Measurements,
        0,
    );
    assert!(
        !second.matches(&decode_response(&wrong_subject)),
        "a matching query id alone does not cross categories"
    );
}

#[test]
fn run_nonce_change_keeps_old_keys_and_invalidates_new_query_association() {
    let old_nonce = [3; 16];
    let first_identity = identity_bytes([7; 12], old_nonce, 6);
    let Response::Identity(first_identity) = decode_response(&first_identity) else {
        panic!("identity response")
    };
    let mut session = Session::new();
    session
        .observe_identity(&first_identity)
        .expect("usable identity");
    let old_query = session.next_query(QueryKind::Status).expect("old query");
    let key = session
        .next_operation()
        .expect("operation key after high water");
    assert_eq!(key.operation_id, 7);
    let persisted = key.to_bytes();
    let restored = eha_sdk::session::MaintenanceKey::from_bytes(&persisted)
        .expect("exact persisted key restores");
    assert_eq!(restored, key);

    let new_nonce = [4; 16];
    let restart_identity = identity_bytes([7; 12], new_nonce, 12);
    let Response::Identity(restart_identity) = decode_response(&restart_identity) else {
        panic!("identity response")
    };
    assert!(matches!(
        session.observe_identity(&restart_identity),
        Ok(IdentityEvent::Changed { .. })
    ));
    assert_eq!(
        key.run_nonce, old_nonce,
        "existing unknown-result key is immutable"
    );
    let key_bytes = key.to_bytes();
    assert_eq!(&key_bytes[12..28], &old_nonce);

    let new_query = session.next_query(QueryKind::Status).expect("new query");
    let after_restart = unavailable_bytes(
        new_query.query_id,
        new_nonce,
        responses::UnavailableSubject::Status,
        0,
    );
    assert!(new_query.matches(&decode_response(&after_restart)));
    assert!(
        !old_query.matches(&decode_response(&after_restart)),
        "old session matcher cannot adopt a restart reply"
    );

    let result = session
        .next_query(QueryKind::Result(key))
        .expect("old maintenance key remains usable for result recovery");
    let unavailable_result = unavailable_bytes(
        result.query_id,
        new_nonce,
        responses::UnavailableSubject::OperationResult,
        key.operation_id,
    );
    assert!(result.matches(&decode_response(&unavailable_result)));
}

#[test]
fn old_maintenance_key_accepts_only_its_run_result_but_current_run_unavailable() {
    let old_nonce = [5; 16];
    let first_identity = identity_bytes([8; 12], old_nonce, 6);
    let Response::Identity(first_identity) = decode_response(&first_identity) else {
        panic!("identity response")
    };
    let mut session = Session::new();
    session
        .observe_identity(&first_identity)
        .expect("usable old run identity");
    let key = session.next_operation().expect("old maintenance key");

    let current_nonce = [6; 16];
    let current_identity = identity_bytes([8; 12], current_nonce, 7);
    let Response::Identity(current_identity) = decode_response(&current_identity) else {
        panic!("identity response")
    };
    assert!(matches!(
        session.observe_identity(&current_identity),
        Ok(IdentityEvent::Changed { .. })
    ));
    let result = session
        .next_query(QueryKind::Result(key))
        .expect("recover old key result");

    let wrong_run = operation_result_bytes(result.query_id, current_nonce, key.operation_id);
    assert!(
        !result.matches(&decode_response(&wrong_run)),
        "a current-run result with the same operation id cannot satisfy an old key"
    );
    let original_run = operation_result_bytes(result.query_id, old_nonce, key.operation_id);
    assert!(
        result.matches(&decode_response(&original_run)),
        "the positive result belongs to the key's original run"
    );

    let current_unavailable = unavailable_bytes(
        result.query_id,
        current_nonce,
        responses::UnavailableSubject::OperationResult,
        key.operation_id,
    );
    assert!(
        result.matches(&decode_response(&current_unavailable)),
        "the current run may explicitly report that the old retained result is unavailable"
    );
}

#[test]
fn exhausted_operation_id_stays_exhausted_after_a_lower_identity_refresh() {
    let nonce = [7; 16];
    let exhausted = identity_bytes([9; 12], nonce, u64::MAX);
    let Response::Identity(exhausted) = decode_response(&exhausted) else {
        panic!("identity response")
    };
    let mut session = Session::new();
    session
        .observe_identity(&exhausted)
        .expect("identity records exhausted high water");
    assert_eq!(
        session.next_operation(),
        Err(SessionError::OperationIdExhausted)
    );

    let lower = identity_bytes([9; 12], nonce, 4);
    let Response::Identity(lower) = decode_response(&lower) else {
        panic!("identity response")
    };
    assert!(matches!(
        session.observe_identity(&lower),
        Ok(IdentityEvent::Refreshed(_))
    ));
    assert_eq!(
        session.next_operation(),
        Err(SessionError::OperationIdExhausted),
        "a lower refreshed high-water mark cannot restore an exhausted local allocator"
    );
}

#[test]
fn identity_is_the_only_query_allowed_before_binding() {
    let mut session = Session::new();
    let identity_query = session
        .next_query(QueryKind::Identity)
        .expect("identity is allowed before binding");
    let unavailable = unavailable_bytes(
        identity_query.query_id,
        [0; 16],
        responses::UnavailableSubject::Identity,
        0,
    );
    assert!(
        identity_query.matches(&decode_response(&unavailable)),
        "identity-unavailable is still the response to the initial identity query"
    );
    assert_eq!(
        session.next_query(QueryKind::Diagnostics),
        Err(SessionError::IdentityRequired)
    );
    assert_eq!(
        session.next_operation(),
        Err(SessionError::IdentityRequired)
    );
}
