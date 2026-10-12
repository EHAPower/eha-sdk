// Copyright The eha-sdk Contributors
#![allow(clippy::panic)]

use eha_sdk::{
    protocol::{Direction, Message, Response, SampleData, decode, responses},
    session::{IdentityEvent, MutationKind, QueryKind, Session, SessionError},
};

fn decode_response(bytes: &[u8]) -> Response<'_> {
    match decode(bytes, Direction::FirmwareToHost).expect("test response is contract-valid") {
        Message::Response(response) => response,
        _ => panic!("expected firmware response"),
    }
}

fn identity_bytes(uid: [u8; 12], nonce: [u8; 16]) -> Vec<u8> {
    let mut bytes = vec![0; 512];
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
    };
    let mut bytes = [0; responses::DataUnavailableFields::encoded_len()];
    let length =
        responses::encode_data_unavailable(&fields, &mut bytes).expect("unavailable encodes");
    bytes[..length].to_vec()
}

fn bind(session: &mut Session, uid: [u8; 12], nonce: [u8; 16]) {
    let bytes = identity_bytes(uid, nonce);
    let Response::Identity(identity) = decode_response(&bytes) else {
        panic!("identity response expected");
    };
    session
        .observe_identity(&identity)
        .expect("identity establishes or refreshes the session");
}

#[test]
fn mutation_association_accepts_only_its_query_run_and_subject() {
    let uid = [1; 12];
    let nonce = [2; 16];
    let mut session = Session::new();
    bind(&mut session, uid, nonce);

    let save = session
        .next_mutation(MutationKind::SaveAndRestart)
        .expect("bound identity allocates a mutation matcher");
    assert_eq!(save.query_id, 1);
    assert_eq!(save.identity.uid, uid);
    assert_eq!(save.identity.run_nonce, nonce);

    let rejected_bytes = unavailable_bytes(
        save.query_id,
        nonce,
        responses::UnavailableSubject::SaveAndRestart,
    );
    let rejected = decode_response(&rejected_bytes);
    assert!(save.matches(&rejected));
    assert!(!save.matches(&decode_response(&unavailable_bytes(
        save.query_id + 1,
        nonce,
        responses::UnavailableSubject::SaveAndRestart,
    ))));
    assert!(!save.matches(&decode_response(&unavailable_bytes(
        save.query_id,
        [3; 16],
        responses::UnavailableSubject::SaveAndRestart,
    ))));
    assert!(!save.matches(&decode_response(&unavailable_bytes(
        save.query_id,
        nonce,
        responses::UnavailableSubject::ResetApplication,
    ))));
}

#[test]
fn unbound_session_allows_only_identity_query() {
    let mut session = Session::new();
    let identity = session
        .next_query(QueryKind::Identity)
        .expect("identity is the bootstrap query");
    assert!(identity.matches(&decode_response(&unavailable_bytes(
        identity.query_id,
        [0; 16],
        responses::UnavailableSubject::Identity,
    ))));
    assert!(matches!(
        session.next_query(QueryKind::Status),
        Err(SessionError::IdentityRequired)
    ));
    assert_eq!(
        session.next_mutation(MutationKind::ResetApplication),
        Err(SessionError::IdentityRequired)
    );
}

#[test]
fn ordinary_query_rejects_wrong_subject_id_and_run_nonce() {
    let mut session = Session::new();
    let uid = [1; 12];
    let nonce = [2; 16];
    bind(&mut session, uid, nonce);

    let status = session.next_query(QueryKind::Status).expect("status query");
    assert!(!status.matches(&decode_response(&unavailable_bytes(
        status.query_id.wrapping_add(1),
        nonce,
        responses::UnavailableSubject::Status,
    ))));
    assert!(!status.matches(&decode_response(&unavailable_bytes(
        status.query_id,
        nonce,
        responses::UnavailableSubject::Measurements,
    ))));
    assert!(!status.matches(&decode_response(&unavailable_bytes(
        status.query_id,
        [3; 16],
        responses::UnavailableSubject::Status,
    ))));
    assert!(status.matches(&decode_response(&unavailable_bytes(
        status.query_id,
        nonce,
        responses::UnavailableSubject::Status,
    ))));
}

#[test]
fn identity_change_invalidates_old_query_and_preserves_query_id_monotonicity() {
    let mut session = Session::new();
    let uid = [1; 12];
    let old_nonce = [2; 16];
    bind(&mut session, uid, old_nonce);
    let old = session
        .next_query(QueryKind::Status)
        .expect("old run query");

    let new_nonce = [3; 16];
    let bytes = identity_bytes(uid, new_nonce);
    let Response::Identity(identity) = decode_response(&bytes) else {
        panic!("identity response expected");
    };
    assert!(matches!(
        session.observe_identity(&identity),
        Ok(IdentityEvent::Changed { .. })
    ));
    let next = session
        .next_query(QueryKind::Status)
        .expect("new run query");
    assert_eq!(next.query_id, old.query_id + 1);
    assert!(!old.matches(&decode_response(&unavailable_bytes(
        old.query_id,
        new_nonce,
        responses::UnavailableSubject::Status,
    ))));
    assert!(next.matches(&decode_response(&unavailable_bytes(
        next.query_id,
        new_nonce,
        responses::UnavailableSubject::Status,
    ))));
}
