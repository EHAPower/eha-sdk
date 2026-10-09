// Copyright The eha-sdk Contributors

//! Host API 的端到端边界：夹具只收取 SDK 实际编码的公共消息，并回送协议编码回复。
#![cfg(feature = "desktop")]
#![allow(clippy::panic)]

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use eha_sdk::{
    host::{
        Client, Failure, LocalStage, Wait,
        backend::{Backend, EventSink, Pump},
    },
    protocol::{
        self, Direction, Message, MessageKind, SampleData,
        responses::{
            self, ConfigDataFields, ConfigDataWriter, ConfigRecordState, ConfigView,
            FallbackReason, IdentityFields, IdentityText, MaintenanceOperation, MaintenanceState,
            OperationEvidence, OperationPhase, OperationResultFields, StartupSource,
            UnavailableSubject,
        },
    },
    session::MaintenanceKey,
};

const UID: [u8; 12] = [7; 12];
const NONCE: [u8; 16] = [9; 16];
// 人工合法记录，仅用于软件验证，不代表产品出厂值或设备标定。
const VALID_CONFIG: &[u8] = include_bytes!("fixtures/config.json");

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<MessageKind>>>);

impl Seen {
    fn record(&self, message: MessageKind) {
        self.0.lock().expect("seen lock").push(message);
    }

    fn snapshot(&self) -> Vec<MessageKind> {
        self.0.lock().expect("seen lock").clone()
    }

    fn count(&self, message: MessageKind) -> usize {
        self.snapshot()
            .into_iter()
            .filter(|observed| *observed == message)
            .count()
    }
}

fn backend_with<F>(seen: Seen, mut reply: F) -> Backend
where
    F: for<'message> FnMut(Message<'message>, &EventSink, u64) + Send + 'static,
{
    Backend::spawn("eha-sdk-host-test", move |commands, sink| {
        let mut pump = Pump::new(commands, sink);
        while pump.poll() {
            for lane in 0..3 {
                while let Some(request) = pump.take(lane) {
                    let message = protocol::decode(&request.bytes, Direction::HostToFirmware)
                        .expect("SDK emits a contract-valid host request");
                    seen.record(message.kind());
                    reply(message, &pump.sink, request.id);
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    })
    .expect("test backend starts")
}

fn sample(query_id: u32) -> SampleData {
    SampleData {
        query_id,
        run_nonce: NONCE,
        snapshot_sequence: 1,
        snapshot_time_us: 10,
    }
}

fn identity(query_id: u32) -> Vec<u8> {
    identity_with_uid(query_id, UID)
}

fn identity_with_uid(query_id: u32, uid: [u8; 12]) -> Vec<u8> {
    let fields = IdentityFields {
        sample: sample(query_id),
        uid,
        config_source: StartupSource::User,
        fallback_reason: FallbackReason::None,
        run_identity: responses::RunIdentity::Random,
        update_route: responses::UpdateRoute::Absent,
        active_can_node: 1,
        active_can_profile: responses::CanProfile::Fd1m2m,
        host_heartbeat_hz: 100,
        telemetry_hz: 20,
        host_contact_max_age_ms: 300,
        config_format_version: 1,
        build_evidence: responses::EvidenceState::Available,
        odrive_evidence: responses::EvidenceState::Unavailable,
        brt27_evidence: responses::EvidenceState::Unavailable,
        high_water_operation_id: 0,
        retained_operation_id: 0,
        text: IdentityText {
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
    let mut bytes = vec![0; fields.encoded_len().expect("identity length")];
    let length = responses::encode_identity(&fields, &mut bytes).expect("identity encodes");
    bytes.truncate(length);
    bytes
}

fn unavailable(query_id: u32, subject: UnavailableSubject, reason: u16) -> Vec<u8> {
    unavailable_result(query_id, subject, reason, 0)
}

fn unavailable_result(
    query_id: u32,
    subject: UnavailableSubject,
    reason: u16,
    operation_id: u64,
) -> Vec<u8> {
    let fields = responses::DataUnavailableFields {
        sample: sample(query_id),
        subject,
        reason,
        next_actions: 0,
        retained: false,
        operation_id,
    };
    let mut bytes = [0; responses::DataUnavailableFields::encoded_len()];
    let length =
        responses::encode_data_unavailable(&fields, &mut bytes).expect("unavailable encodes");
    bytes[..length].to_vec()
}

fn operation_result(query_id: u32, operation_id: u64) -> Vec<u8> {
    let fields = OperationResultFields {
        sample: sample(query_id),
        operation_id,
        operation: MaintenanceOperation::SaveConfig,
        state: MaintenanceState::FirmwareStepComplete,
        phase: OperationPhase::StorageReplacement,
        evidence: OperationEvidence {
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
        content_length: VALID_CONFIG.len() as u32,
        content_crc32c: 0,
        revision: 1,
    };
    let mut bytes = [0; OperationResultFields::encoded_len()];
    let length =
        responses::encode_operation_result(&fields, &mut bytes).expect("operation result encodes");
    bytes[..length].to_vec()
}

fn read_failed_config(query_id: u32) -> Vec<u8> {
    let fields = ConfigDataFields {
        sample: sample(query_id),
        view: ConfigView::UserRecord,
        record_state: ConfigRecordState::ReadFailed,
        startup_source: StartupSource::User,
        fallback_reason: FallbackReason::ReadFailed,
        data_time_us: u64::MAX,
    };
    let mut bytes = vec![0; ConfigDataWriter::required_len(0).expect("empty config length")];
    let writer = ConfigDataWriter::start(fields, 0, &mut bytes).expect("config starts");
    let length = writer.finish().expect("config encodes");
    bytes.truncate(length);
    bytes
}

fn complete_config(query_id: u32, record: &[u8]) -> Vec<u8> {
    let fields = ConfigDataFields {
        sample: sample(query_id),
        view: ConfigView::UserRecord,
        record_state: ConfigRecordState::Complete,
        startup_source: StartupSource::User,
        fallback_reason: FallbackReason::None,
        data_time_us: 10,
    };
    let mut bytes =
        vec![0; ConfigDataWriter::required_len(record.len()).expect("complete config length")];
    let mut writer =
        ConfigDataWriter::start(fields, record.len(), &mut bytes).expect("config starts");
    writer.data_mut().copy_from_slice(record);
    let length = writer.finish().expect("config encodes");
    bytes.truncate(length);
    bytes
}

fn identify(client: &mut Client) {
    client
        .identify(Some(UID), &Wait::new(Duration::from_millis(200)))
        .expect("identity query succeeds");
}

#[test]
fn query_ignores_late_and_wrong_subject_replies_before_the_matching_reply() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::Query {
            query_id,
            category: 1,
        } => {
            sink.submitted(request_id);
            sink.received(unavailable(
                query_id - 1,
                UnavailableSubject::Status,
                0x0101,
            ));
            sink.received(unavailable(
                query_id,
                UnavailableSubject::Measurements,
                0x0102,
            ));
            sink.received(unavailable(query_id, UnavailableSubject::Status, 0x0103));
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);

    let error = client
        .status(&Wait::new(Duration::from_millis(200)))
        .expect_err("only the matching status reply may finish the query");
    match error.failure {
        Failure::DataUnavailable(fields) => assert_eq!(fields.reason, 0x0103),
        other => panic!("unexpected failure: {other:?}"),
    }
    assert_eq!(seen.count(MessageKind::Query), 2);
}

#[test]
fn query_keeps_a_matching_reply_that_arrives_before_local_submission() {
    let backend = backend_with(Seen::default(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            // USB IN may complete before the final OUT completion that establishes local submit.
            sink.received(identity(query_id));
            sink.submitted(request_id);
        }
        Message::Query {
            query_id,
            category: 1,
        } => {
            sink.received(unavailable(query_id, UnavailableSubject::Status, 0x0105));
            sink.submitted(request_id);
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);

    client
        .identify(Some(UID), &Wait::new(Duration::from_millis(100)))
        .expect("an early valid Identity reply remains available after local submission");
    let error = client
        .status(&Wait::new(Duration::from_millis(100)))
        .expect_err("an early matching Status result must retain its query error");
    assert!(matches!(error.failure, Failure::DataUnavailable(fields) if fields.reason == 0x0105));
    assert_eq!(error.local, LocalStage::FullySubmitted);
}

#[test]
fn unmatched_events_cannot_extend_a_query_past_its_deadline() {
    let backend = Backend::spawn("eha-sdk-deadline-test", move |commands, sink| {
        let mut pump = Pump::new(commands, sink);
        let mut flood = None;
        while pump.poll() {
            for lane in 0..3 {
                while let Some(request) = pump.take(lane) {
                    let message = protocol::decode(&request.bytes, Direction::HostToFirmware)
                        .expect("SDK emits a contract-valid host request");
                    match message {
                        Message::Query {
                            query_id,
                            category: 0,
                        } => {
                            pump.sink.submitted(request.id);
                            pump.sink.received(identity(query_id));
                        }
                        Message::Query {
                            query_id,
                            category: 1,
                        } => {
                            pump.sink.submitted(request.id);
                            flood = Some((
                                query_id.wrapping_add(1),
                                Instant::now() + Duration::from_millis(150),
                            ));
                        }
                        other => panic!("unexpected request: {other:?}"),
                    }
                }
            }
            if let Some((wrong_query_id, until)) = flood
                && Instant::now() < until
            {
                pump.sink.received(unavailable(
                    wrong_query_id,
                    UnavailableSubject::Status,
                    0x0104,
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
    })
    .expect("test backend starts");
    let mut client = Client::new(backend);
    identify(&mut client);

    let started = Instant::now();
    let error = client
        .status(&Wait::new(Duration::from_millis(25)))
        .expect_err("unmatched replies must not keep this query alive");
    assert!(matches!(error.failure, Failure::Timeout));
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "the 150ms unmatched-event flood must not extend a 25ms wait"
    );
}

#[test]
fn submitted_save_disconnect_is_unknown_and_remains_pending_without_a_retry() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::SaveConfig { .. } => {
            sink.submitted(request_id);
            sink.disconnected("test link lost after local submission");
        }
        Message::ReadResult { .. } => sink.submitted(request_id),
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);

    let error = client
        .save_and_readback(VALID_CONFIG, &Wait::new(Duration::from_millis(200)))
        .expect_err("a disconnect after submission leaves the maintenance result unknown");
    assert!(matches!(error.failure, Failure::ResultUnknown(_)));
    assert_eq!(
        client.connection_status().disconnected.as_deref(),
        Some("test link lost after local submission"),
        "the locally observed disconnect remains available after the operation error"
    );
    assert!(error.operation.is_some());
    assert_eq!(
        client.pending_operation(),
        error.operation.as_deref().copied()
    );
    assert!(matches!(
        client.begin_save(VALID_CONFIG, &Wait::new(Duration::from_millis(20))),
        Err(e) if matches!(e.failure, Failure::PendingOperation)
    ));
    assert_eq!(seen.count(MessageKind::SaveConfig), 1);
}

#[test]
fn foreign_key_cannot_send_read_or_occupy_an_empty_maintenance_slot() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::ReadResult { query_id, .. } => {
            sink.submitted(request_id);
            sink.received(unavailable(
                query_id,
                UnavailableSubject::OperationResult,
                0x0302,
            ));
        }
        Message::SaveConfig { .. } => sink.submitted(request_id),
        other => panic!("unexpected message: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);
    let foreign = MaintenanceKey {
        uid: [8; 12],
        run_nonce: NONCE,
        operation_id: 37,
    };

    let read = client
        .read_result(foreign, &Wait::new(Duration::from_millis(20)))
        .expect_err("foreign key must be rejected before reading");
    assert!(matches!(read.failure, Failure::Session(_)));
    assert_eq!(read.local, LocalStage::NotSubmitted);

    let track = client
        .track_operation(foreign)
        .expect_err("foreign key must be rejected before tracking");
    assert!(matches!(track.failure, Failure::Session(_)));
    assert_eq!(track.local, LocalStage::NotSubmitted);
    assert!(client.pending_operation().is_none());
    assert_eq!(seen.count(MessageKind::ReadResult), 0);

    let started = client
        .begin_save(VALID_CONFIG, &Wait::new(Duration::from_millis(20)))
        .expect("foreign key must not occupy the local maintenance slot");
    assert_eq!(client.pending_operation(), Some(started.key));
    assert_eq!(seen.count(MessageKind::SaveConfig), 1);
}

#[test]
fn foreign_key_cannot_replace_the_tracked_maintenance_operation() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::SaveConfig { .. } => sink.submitted(request_id),
        other => panic!("unexpected message: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);
    let started = client
        .begin_save(VALID_CONFIG, &Wait::new(Duration::from_millis(20)))
        .expect("save is accepted");
    let foreign = MaintenanceKey {
        uid: [8; 12],
        run_nonce: NONCE,
        operation_id: 38,
    };

    let error = client
        .track_operation(foreign)
        .expect_err("foreign key must not replace the tracked operation");
    assert!(matches!(error.failure, Failure::Session(_)));
    assert_eq!(error.local, LocalStage::NotSubmitted);
    assert_eq!(client.pending_operation(), Some(started.key));
    assert_eq!(seen.count(MessageKind::SaveConfig), 1);
}

#[test]
fn same_uid_key_from_previous_run_remains_readable() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::ReadResult { query_id, .. } => {
            sink.submitted(request_id);
            sink.received(unavailable_result(
                query_id,
                UnavailableSubject::OperationResult,
                0x0300,
                39,
            ));
        }
        other => panic!("unexpected message: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);
    let old_key = MaintenanceKey {
        uid: UID,
        run_nonce: [4; 16],
        operation_id: 39,
    };

    client
        .track_operation(old_key)
        .expect("same device key remains trackable after a reboot");
    let error = client
        .read_result(old_key, &Wait::new(Duration::from_millis(20)))
        .expect_err("the device reports the prior-run result unavailable");
    assert!(matches!(
        error.failure,
        Failure::DataUnavailable(fields)
            if fields.subject == UnavailableSubject::OperationResult && fields.reason == 0x0300
    ));
    assert_eq!(client.pending_operation(), Some(old_key));
    assert_eq!(seen.count(MessageKind::ReadResult), 1);
}

#[test]
fn tracking_the_same_key_keeps_the_observed_result_for_release() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::SaveConfig { key, .. } => {
            sink.submitted(request_id);
            sink.received(operation_result(0, key.operation_id()));
        }
        Message::ReadResult { query_id, key } => {
            sink.submitted(request_id);
            sink.received(operation_result(query_id, key.operation_id()));
        }
        Message::ReleaseResult { .. } => sink.submitted(request_id),
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);

    let submission = client
        .begin_save(VALID_CONFIG, &Wait::new(Duration::from_millis(100)))
        .expect("save is locally submitted");
    client
        .read_result(submission.key, &Wait::new(Duration::from_millis(100)))
        .expect("result is observed before explicit re-tracking");
    client
        .track_operation(submission.key)
        .expect("the same key remains tracked without clearing its result");
    client
        .release_result(&Wait::new(Duration::from_millis(100)))
        .expect("the retained result remains releasable");
    assert_eq!(seen.count(MessageKind::ReleaseResult), 1);
}

#[test]
fn identity_change_cannot_release_a_pending_result_from_another_device() {
    let seen = Seen::default();
    let mut identity_count = 0;
    let uid_b = [8; 12];
    let backend = backend_with(
        seen.clone(),
        move |message, sink, request_id| match message {
            Message::Query {
                query_id,
                category: 0,
            } => {
                identity_count += 1;
                sink.submitted(request_id);
                sink.received(identity_with_uid(
                    query_id,
                    if identity_count == 1 { UID } else { uid_b },
                ));
            }
            Message::SaveConfig { key, .. } => {
                sink.submitted(request_id);
                sink.received(operation_result(0, key.operation_id()));
            }
            Message::ReadResult { query_id, key } => {
                sink.submitted(request_id);
                sink.received(operation_result(query_id, key.operation_id()));
            }
            Message::ReleaseResult { .. } => sink.submitted(request_id),
            other => panic!("unexpected request: {other:?}"),
        },
    );
    let mut client = Client::new(backend);
    identify(&mut client);
    let submission = client
        .begin_save(VALID_CONFIG, &Wait::new(Duration::from_millis(100)))
        .expect("A save is locally submitted");
    client
        .read_result(submission.key, &Wait::new(Duration::from_millis(100)))
        .expect("A result is observed before switching identity");
    client
        .identify(Some(uid_b), &Wait::new(Duration::from_millis(100)))
        .expect("B identity is explicitly verified");

    let error = client
        .release_result(&Wait::new(Duration::from_millis(100)))
        .expect_err("B must not receive A's release key");
    assert!(matches!(error.failure, Failure::Session(_)));
    assert_eq!(error.local, LocalStage::NotSubmitted);
    assert_eq!(client.pending_operation(), Some(submission.key));
    assert_eq!(seen.count(MessageKind::ReleaseResult), 0);
}

#[test]
fn cancellation_after_save_submission_keeps_the_original_operation_pending() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::SaveConfig { .. } | Message::ReadResult { .. } => sink.submitted(request_id),
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);
    let wait = Wait::new(Duration::from_secs(1));
    let cancellation = wait.cancellation.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        cancellation.cancel();
    });

    let error = client
        .save_and_readback(VALID_CONFIG, &wait)
        .expect_err("cancelling a result wait cannot revoke a submitted save");
    canceller.join().expect("canceller joins");
    assert!(matches!(error.failure, Failure::ResultUnknown(_)));
    assert!(client.pending_operation().is_some());
    assert!(matches!(
        client.begin_save(VALID_CONFIG, &Wait::new(Duration::from_millis(20))),
        Err(e) if matches!(e.failure, Failure::PendingOperation)
    ));
    assert_eq!(seen.count(MessageKind::SaveConfig), 1);
}

#[test]
fn save_waits_for_its_original_notice_before_reading_the_retained_result() {
    let seen = Seen::default();
    let notice_sent = Arc::new(AtomicBool::new(false));
    let early_read = Arc::new(AtomicBool::new(false));
    let backend = Backend::spawn("eha-sdk-save-notice-test", {
        let seen = seen.clone();
        let notice_sent = notice_sent.clone();
        let early_read = early_read.clone();
        move |commands, sink| {
            let mut pump = Pump::new(commands, sink);
            let mut delayed_notice = None;
            while pump.poll() {
                for lane in 0..3 {
                    while let Some(request) = pump.take(lane) {
                        let message = protocol::decode(&request.bytes, Direction::HostToFirmware)
                            .expect("SDK emits a contract-valid host request");
                        seen.record(message.kind());
                        match message {
                            Message::Query {
                                query_id,
                                category: 0,
                            } => {
                                pump.sink.submitted(request.id);
                                pump.sink.received(identity(query_id));
                            }
                            Message::SaveConfig { key, .. } => {
                                pump.sink.submitted(request.id);
                                delayed_notice = Some((
                                    key.operation_id(),
                                    Instant::now() + Duration::from_millis(45),
                                ));
                            }
                            Message::ReadResult { query_id, key } => {
                                if !notice_sent.load(Ordering::Acquire) {
                                    early_read.store(true, Ordering::Release);
                                }
                                pump.sink.submitted(request.id);
                                if notice_sent.load(Ordering::Acquire) {
                                    pump.sink
                                        .received(operation_result(query_id, key.operation_id()));
                                }
                            }
                            Message::ReadConfig { query_id, .. } => {
                                pump.sink.submitted(request.id);
                                pump.sink.received(complete_config(query_id, VALID_CONFIG));
                            }
                            other => panic!("unexpected request: {other:?}"),
                        }
                    }
                }
                if let Some((operation_id, due)) = delayed_notice
                    && Instant::now() >= due
                {
                    pump.sink.received(operation_result(0, operation_id));
                    notice_sent.store(true, Ordering::Release);
                    delayed_notice = None;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    })
    .expect("test backend starts");
    let mut client = Client::new(backend);
    identify(&mut client);

    client
        .save_and_readback(VALID_CONFIG, &Wait::new(Duration::from_millis(300)))
        .expect("the delayed original notice enables the subsequent result readback");
    assert!(notice_sent.load(Ordering::Acquire));
    assert!(
        !early_read.load(Ordering::Acquire),
        "ReadResult must not overwrite the still-unconsumed original operation notice"
    );
    assert_eq!(seen.count(MessageKind::ReadResult), 1);
}

#[test]
fn invalid_configuration_is_rejected_before_the_save_side_effect() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        other => panic!("invalid configuration must not send {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);

    let error = client
        .begin_save(b"{not json", &Wait::new(Duration::from_millis(50)))
        .expect_err("malformed configuration is a local failure");
    assert!(matches!(error.failure, Failure::InvalidInput(_)));
    assert_eq!(seen.snapshot(), vec![MessageKind::Query]);
}

#[test]
fn close_and_drop_only_release_the_local_backend() {
    for explicit_close in [true, false] {
        let seen = Seen::default();
        let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
            Message::Query {
                query_id,
                category: 0,
            } => {
                sink.submitted(request_id);
                sink.received(identity(query_id));
            }
            other => panic!("close/drop must not send {other:?}"),
        });
        let mut client = Client::new(backend);
        identify(&mut client);
        if explicit_close {
            client.close();
        } else {
            drop(client);
        }
        assert_eq!(seen.snapshot(), vec![MessageKind::Query]);
    }
}

#[test]
fn resumed_client_requires_a_fresh_identity_before_control_can_resume() {
    let original = backend_with(Seen::default(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut first = Client::new(original);
    identify(&mut first);
    let state = first.disconnect();

    let seen = Seen::default();
    let resumed_backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::Velocity(_) => sink.submitted(request_id),
        other => panic!("unexpected request: {other:?}"),
    });
    let mut resumed = Client::resume(resumed_backend, state);

    assert!(matches!(
        resumed.velocity(0.0, &Wait::new(Duration::from_millis(50))),
        Err(e) if matches!(e.failure, Failure::Session(_))
    ));
    assert_eq!(seen.count(MessageKind::Velocity), 0);
    assert!(matches!(
        resumed.release_result(&Wait::new(Duration::from_millis(50))),
        Err(e) if matches!(e.failure, Failure::Session(_))
    ));
    assert_eq!(seen.count(MessageKind::ReleaseResult), 0);
    identify(&mut resumed);
    resumed
        .velocity(0.0, &Wait::new(Duration::from_millis(50)))
        .expect("fresh identity permits a new explicit control request");
    assert_eq!(seen.count(MessageKind::Velocity), 1);
}

#[test]
fn explicit_background_heartbeat_progresses_while_a_query_waits() {
    let seen = Seen::default();
    let backend = Backend::spawn("eha-sdk-heartbeat-test", {
        let seen = seen.clone();
        move |commands, sink| {
            let mut pump = Pump::new(commands, sink);
            let mut delayed_status = None;
            while pump.poll() {
                for lane in 0..3 {
                    while let Some(request) = pump.take(lane) {
                        let message = protocol::decode(&request.bytes, Direction::HostToFirmware)
                            .expect("SDK emits a contract-valid host request");
                        seen.record(message.kind());
                        match message {
                            Message::Query {
                                query_id,
                                category: 0,
                            } => {
                                pump.sink.submitted(request.id);
                                pump.sink.received(identity(query_id));
                            }
                            Message::Query {
                                query_id,
                                category: 1,
                            } => {
                                pump.sink.submitted(request.id);
                                delayed_status =
                                    Some((query_id, Instant::now() + Duration::from_millis(55)));
                            }
                            Message::Heartbeat => pump.sink.submitted(request.id),
                            other => panic!("unexpected request: {other:?}"),
                        }
                    }
                }
                if let Some((query_id, due)) = delayed_status
                    && Instant::now() >= due
                {
                    pump.sink
                        .received(unavailable(query_id, UnavailableSubject::Status, 0x0201));
                    delayed_status = None;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    })
    .expect("test backend starts");
    let mut client = Client::new(backend);
    identify(&mut client);
    client
        .start_heartbeat()
        .expect("identity enables heartbeat");

    let error = client
        .status(&Wait::new(Duration::from_millis(200)))
        .expect_err("fixture returns a status-unavailable reply after waiting");
    assert!(matches!(error.failure, Failure::DataUnavailable(_)));
    let heartbeat = client.heartbeat_status();
    assert!(
        heartbeat.submitted >= 2,
        "heartbeat advances during the status wait"
    );
    assert!(seen.count(MessageKind::Heartbeat) >= 2);
    client.stop_heartbeat().expect("explicitly stop scheduler");
}

#[test]
fn completed_firmware_save_with_failed_readback_is_not_reported_as_success() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::SaveConfig { key, .. } => {
            sink.submitted(request_id);
            sink.received(operation_result(0, key.operation_id()));
        }
        Message::ReadResult { query_id, key } => {
            sink.submitted(request_id);
            sink.received(operation_result(query_id, key.operation_id()));
        }
        Message::ReadConfig { query_id, .. } => {
            sink.submitted(request_id);
            sink.received(read_failed_config(query_id));
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);

    let error = client
        .save_and_readback(VALID_CONFIG, &Wait::new(Duration::from_millis(300)))
        .expect_err("a completed firmware step still needs a usable matching readback");
    assert!(matches!(
        error.failure,
        Failure::ReadbackUnavailable(ConfigRecordState::ReadFailed)
    ));
    assert!(matches!(
        error.last_result.as_deref(),
        Some(result) if result.state == MaintenanceState::FirmwareStepComplete
    ));
    assert_eq!(seen.count(MessageKind::SaveConfig), 1);
    assert_eq!(seen.count(MessageKind::ReadResult), 1);
    assert_eq!(seen.count(MessageKind::ReadConfig), 1);
}

#[test]
fn completed_firmware_save_with_different_readback_is_a_mismatch() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::SaveConfig { key, .. } => {
            sink.submitted(request_id);
            sink.received(operation_result(0, key.operation_id()));
        }
        Message::ReadResult { query_id, key } => {
            sink.submitted(request_id);
            sink.received(operation_result(query_id, key.operation_id()));
        }
        Message::ReadConfig { query_id, .. } => {
            sink.submitted(request_id);
            let mut different = VALID_CONFIG.to_vec();
            different[0] = b' ';
            sink.received(complete_config(query_id, &different));
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify(&mut client);

    let error = client
        .save_and_readback(VALID_CONFIG, &Wait::new(Duration::from_millis(300)))
        .expect_err("a byte-different readback cannot be accepted as the submitted record");
    assert!(matches!(error.failure, Failure::ReadbackMismatch));
    assert!(matches!(
        error.last_result.as_deref(),
        Some(result) if result.state == MaintenanceState::FirmwareStepComplete
    ));
    assert_eq!(seen.count(MessageKind::SaveConfig), 1);
    assert_eq!(seen.count(MessageKind::ReadResult), 1);
    assert_eq!(seen.count(MessageKind::ReadConfig), 1);
}
