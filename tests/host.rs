// Copyright The eha-sdk Contributors
//! Host API 的端到端边界：夹具只收取 SDK 实际编码的公共消息，并回送协议编码回复。
#![cfg(feature = "desktop")]
#![allow(clippy::panic)]

use eha_sdk::session::QueryKind;
use std::{
    sync::{Arc, Mutex},
    task::Poll,
    thread,
    time::{Duration, Instant},
};

use eha_sdk::{
    host::{
        Cancellation, Client, Failure, LocalStage, Wait,
        backend::{Backend, Event, EventSink, Pump},
    },
    protocol::{
        self, Direction, Message, MessageKind, SampleData,
        responses::{
            self, ConfigDataFields, ConfigDataWriter, ConfigRecordState, ConfigView,
            FallbackReason, IdentityFields, IdentityText, StartupSource, UnavailableSubject,
        },
    },
};

const UID: [u8; 12] = [7; 12];
const NONCE: [u8; 16] = [9; 16];
const VALID_CONFIG: &[u8] = include_bytes!("fixtures/config.json");

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<MessageKind>>>);
impl Seen {
    fn record(&self, kind: MessageKind) {
        self.0.lock().expect("seen lock").push(kind);
    }
    fn count(&self, kind: MessageKind) -> usize {
        self.0
            .lock()
            .expect("seen lock")
            .iter()
            .filter(|seen| **seen == kind)
            .count()
    }
    fn snapshot(&self) -> Vec<MessageKind> {
        self.0.lock().expect("seen lock").clone()
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

fn sample_with_nonce(query_id: u32, run_nonce: [u8; 16]) -> SampleData {
    SampleData {
        query_id,
        run_nonce,
        snapshot_sequence: 1,
        snapshot_time_us: 10,
    }
}

fn identity(query_id: u32) -> Vec<u8> {
    identity_with(query_id, UID, NONCE, 1)
}

fn identity_with(
    query_id: u32,
    uid: [u8; 12],
    run_nonce: [u8; 16],
    config_format_version: u32,
) -> Vec<u8> {
    let fields = IdentityFields {
        sample: sample_with_nonce(query_id, run_nonce),
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
        config_format_version,
        build_evidence: responses::EvidenceState::Available,
        odrive_evidence: responses::EvidenceState::Unavailable,
        brt27_evidence: responses::EvidenceState::Unavailable,
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

fn unavailable(query_id: u32, subject: UnavailableSubject) -> Vec<u8> {
    unavailable_with(query_id, NONCE, subject, 0x0101)
}

fn unavailable_with(
    query_id: u32,
    run_nonce: [u8; 16],
    subject: UnavailableSubject,
    reason: u16,
) -> Vec<u8> {
    let fields = responses::DataUnavailableFields {
        sample: sample_with_nonce(query_id, run_nonce),
        subject,
        reason,
        next_actions: 0,
    };
    let mut bytes = [0; responses::DataUnavailableFields::encoded_len()];
    let length =
        responses::encode_data_unavailable(&fields, &mut bytes).expect("unavailable encodes");
    bytes[..length].to_vec()
}

fn config(query_id: u32, view: ConfigView, record: &[u8]) -> Vec<u8> {
    config_with_nonce(query_id, NONCE, view, record)
}

fn config_with_nonce(
    query_id: u32,
    run_nonce: [u8; 16],
    view: ConfigView,
    record: &[u8],
) -> Vec<u8> {
    let fields = ConfigDataFields {
        sample: sample_with_nonce(query_id, run_nonce),
        view,
        record_state: ConfigRecordState::Complete,
        startup_source: StartupSource::User,
        fallback_reason: FallbackReason::None,
        data_time_us: 10,
    };
    let mut bytes = vec![0; ConfigDataWriter::required_len(record.len()).expect("config length")];
    let mut writer =
        ConfigDataWriter::start(fields, record.len(), &mut bytes).expect("config starts");
    writer.data_mut().copy_from_slice(record);
    let length = writer.finish().expect("config encodes");
    bytes.truncate(length);
    bytes
}

fn identify_client(client: &mut Client) {
    client
        .identify(Some(UID), &Wait::new(Duration::from_millis(100)))
        .expect("identity succeeds");
}

// 按测试步骤交付事件，查询永不自动回复；不靠 sleep 或墙钟上限判断 poll 是否有界。
fn controlled_client() -> (
    Client,
    EventSink,
    std::sync::mpsc::Receiver<eha_sdk::host::backend::SendRequest>,
) {
    use eha_sdk::host::backend::Command;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (request_tx, request_rx) = std::sync::mpsc::sync_channel(8);
    let backend = Backend::spawn("eha-sdk-poll-test", move |commands, sink| {
        ready_tx.send(sink.clone()).expect("publish sink");
        while let Ok(command) = commands.recv() {
            match command {
                Command::Send(request) => {
                    match protocol::decode(&request.bytes, Direction::HostToFirmware)
                        .expect("valid request")
                    {
                        Message::Query {
                            query_id,
                            category: 0,
                        } => {
                            sink.submitted(request.id);
                            sink.received(identity(query_id));
                        }
                        Message::Stop => sink.submitted(request.id),
                        _ => request_tx.send(request).expect("publish request"),
                    }
                }
                Command::Close => break,
                Command::Heartbeat(_) => {}
                Command::CancellableSend(_, _) => {
                    panic!("read-only fixture does not drive async control")
                }
            }
        }
    })
    .expect("backend starts");
    let sink = ready_rx.recv().expect("sink ready");
    let mut client = Client::new(backend);
    identify_client(&mut client);
    (client, sink, request_rx)
}

fn query_id(request: &eha_sdk::host::backend::SendRequest) -> u32 {
    match protocol::decode(&request.bytes, Direction::HostToFirmware).expect("request decodes") {
        Message::Query { query_id, .. } | Message::ReadConfig { query_id, .. } => query_id,
        other => panic!("expected query, got {other:?}"),
    }
}

#[test]
fn polling_cancel_releases_stop_and_ignores_late_query_events() {
    let (mut client, sink, requests) = controlled_client();
    let wait = Wait::default();
    client.begin_query(QueryKind::Status, &wait).expect("begin");
    let old = requests.recv().expect("query handed to worker");
    assert!(client.poll_query().is_pending());
    assert!(client.poll_query().is_pending());
    assert_eq!(
        client.cancel_query().expect("cancel"),
        LocalStage::MayHaveBeenSubmitted
    );
    assert!(old.expired());
    sink.submitted(old.id);
    sink.failed(old.id, "late cancellation result", false);
    sink.received(unavailable(query_id(&old), UnavailableSubject::Status));
    let stop = client
        .stop_control(&wait)
        .expect("next call submits explicit Stop");
    assert!(stop.id > old.id);

    client
        .begin_query(QueryKind::Status, &wait)
        .expect("next query");
    let next = requests.recv().expect("next request");
    sink.received(unavailable(query_id(&old), UnavailableSubject::Status));
    sink.submitted(next.id);
    assert!(client.poll_query().is_pending());
    sink.received(unavailable_with(
        query_id(&next),
        NONCE,
        UnavailableSubject::Status,
        0x0104,
    ));
    let Poll::Ready(Err(error)) = client.poll_query() else {
        panic!("matching reply completes")
    };
    assert!(matches!(error.failure, Failure::DataUnavailable(fields) if fields.reason == 0x0104));
    assert_eq!(error.local, LocalStage::FullySubmitted);
}

#[test]
fn pending_query_rejects_other_consumers_without_changing_identity_or_losing_events() {
    let (mut client, sink, requests) = controlled_client();
    let wait = Wait::default();
    client
        .begin_query(QueryKind::Measurements, &wait)
        .expect("begin");
    let request = requests.recv().expect("query request");
    let errors = [
        client
            .begin_query(QueryKind::Status, &wait)
            .expect_err("one pending"),
        client
            .identify(None, &wait)
            .expect_err("identity cannot change"),
        client.status(&wait).expect_err("sync query cannot consume"),
        client
            .measurements(&wait)
            .expect_err("sync query cannot consume"),
        client
            .diagnostics(&wait)
            .expect_err("sync query cannot consume"),
        client
            .read_config(ConfigView::Factory, &wait)
            .expect_err("config cannot consume"),
        client.stop_control(&wait).expect_err("cancel first"),
        client
            .heartbeat(&wait)
            .expect_err("manual heartbeat cannot consume"),
        client
            .position(1.0, &wait)
            .expect_err("control cannot consume"),
        client
            .velocity(1.0, &wait)
            .expect_err("control cannot consume"),
        client
            .force(1.0, &wait)
            .expect_err("control cannot consume"),
        client
            .impedance(1.0, 1.0, 1.0, &wait)
            .expect_err("control cannot consume"),
        client
            .reset_application(&wait)
            .expect_err("mutation cannot consume"),
        client
            .enter_update(&wait)
            .expect_err("mutation cannot consume"),
        client
            .save_and_restart(VALID_CONFIG, &wait)
            .expect_err("save cannot consume"),
        client
            .restore_factory_and_restart(&wait)
            .expect_err("restore cannot consume"),
    ];
    assert!(
        errors
            .iter()
            .all(|error| matches!(error.failure, Failure::Session(_))
                && error.local == LocalStage::NotSubmitted)
    );
    client
        .start_heartbeat()
        .expect("background heartbeat allowed");
    client
        .stop_heartbeat()
        .expect("background heartbeat switch allowed");
    client
        .enable_telemetry_queue(2)
        .expect("cache configuration allowed");
    let _ = (
        client.telemetry(),
        client.drain_telemetry(),
        client.connection_status(),
        client.transport_status(),
        client.adapter_status(),
        client.heartbeat_status(),
        client.session(),
    );
    sink.submitted(request.id);
    sink.received(unavailable(
        query_id(&request),
        UnavailableSubject::Measurements,
    ));
    assert!(
        matches!(client.poll_query(), Poll::Ready(Err(error)) if matches!(error.failure, Failure::DataUnavailable(_)))
    );
    client.stop_control(&wait).expect("identity still verified");
    assert!(requests.try_recv().is_err());
}

#[test]
fn polling_matches_nonce_subject_and_id_and_retains_early_reply_until_submission() {
    let (mut client, sink, requests) = controlled_client();
    client
        .begin_query(QueryKind::Status, &Wait::default())
        .expect("begin");
    let request = requests.recv().expect("query request");
    let id = query_id(&request);
    sink.received(unavailable_with(id, [8; 16], UnavailableSubject::Status, 1));
    sink.received(unavailable_with(
        id - 1,
        NONCE,
        UnavailableSubject::Status,
        2,
    ));
    sink.received(unavailable_with(
        id,
        NONCE,
        UnavailableSubject::Diagnostics,
        3,
    ));
    assert!(client.poll_query().is_pending());
    sink.received(unavailable_with(id, NONCE, UnavailableSubject::Status, 4));
    assert!(client.poll_query().is_pending());
    sink.submitted(request.id);
    let Poll::Ready(Err(error)) = client.poll_query() else {
        panic!("early reply completes after Submitted")
    };
    assert!(matches!(error.failure, Failure::DataUnavailable(fields) if fields.reason == 4));
    assert_eq!(error.local, LocalStage::FullySubmitted);
    assert!(
        matches!(client.poll_query(), Poll::Ready(Err(error)) if matches!(error.failure, Failure::InvalidInput(_)))
    );
}

#[test]
fn polling_covers_all_read_topics_and_validates_public_kinds() {
    let (mut client, sink, requests) = controlled_client();
    let wait = Wait::default();
    assert!(client.begin_query(QueryKind::Identity, &wait).is_err());
    assert!(client.begin_query(QueryKind::Config(3), &wait).is_err());
    for (kind, subject) in [
        (QueryKind::Status, UnavailableSubject::Status),
        (QueryKind::Measurements, UnavailableSubject::Measurements),
        (QueryKind::Diagnostics, UnavailableSubject::Diagnostics),
    ] {
        client.begin_query(kind, &wait).expect("begin topic");
        let request = requests.recv().expect("query request");
        sink.submitted(request.id);
        sink.received(unavailable(query_id(&request), subject));
        assert!(
            matches!(client.poll_query(), Poll::Ready(Err(error)) if matches!(error.failure, Failure::DataUnavailable(fields) if fields.subject == subject))
        );
    }
    for view in [
        ConfigView::Factory,
        ConfigView::Startup,
        ConfigView::UserRecord,
    ] {
        client
            .begin_query(QueryKind::Config(view as u8), &wait)
            .expect("begin config");
        let request = requests.recv().expect("config request");
        sink.submitted(request.id);
        sink.received(config(query_id(&request), view, VALID_CONFIG));
        let Poll::Ready(Ok(reply)) = client.poll_query() else {
            panic!("matching config reply")
        };
        assert!(
            matches!(reply.response().expect("reply decodes"), protocol::Response::ConfigData(data) if data.fields().view == view && data.data() == VALID_CONFIG)
        );
    }
}

#[test]
fn polling_cancel_and_transport_failures_preserve_submission_stage() {
    for submitted in [false, true] {
        let (mut client, sink, requests) = controlled_client();
        let wait = Wait::default();
        client.begin_query(QueryKind::Status, &wait).expect("begin");
        let request = requests.recv().expect("query request");
        if submitted {
            sink.submitted(request.id);
        }
        assert!(client.poll_query().is_pending());
        wait.cancellation.cancel();
        let Poll::Ready(Err(error)) = client.poll_query() else {
            panic!("cancel completes")
        };
        assert!(matches!(error.failure, Failure::Cancelled));
        assert_eq!(
            error.local,
            if submitted {
                LocalStage::FullySubmitted
            } else {
                LocalStage::MayHaveBeenSubmitted
            }
        );
        assert!(request.expired());
        assert!(client.cancel_query().is_err());
    }
    for event in [
        Event::Failed(
            2,
            eha_sdk::host::backend::IoFailure {
                detail: "not sent".into(),
                may_have_sent: false,
            },
        ),
        Event::Failed(
            2,
            eha_sdk::host::backend::IoFailure {
                detail: "partly sent".into(),
                may_have_sent: true,
            },
        ),
        Event::TransportError {
            detail: "framing".into(),
            raw: vec![1, 2],
        },
        Event::Disconnected("gone".into()),
    ] {
        let (mut client, sink, requests) = controlled_client();
        client
            .begin_query(QueryKind::Status, &Wait::default())
            .expect("begin");
        let request = requests.recv().expect("query request");
        let expected_stage = if matches!(event, Event::Failed(_, ref failure) if !failure.may_have_sent)
        {
            LocalStage::NotSubmitted
        } else {
            LocalStage::MayHaveBeenSubmitted
        };
        let disconnected = matches!(event, Event::Disconnected(_));
        sink.event(event);
        let Poll::Ready(Err(error)) = client.poll_query() else {
            panic!("transport failure completes")
        };
        assert_eq!(error.local, expected_stage);
        assert_eq!(
            matches!(error.failure, Failure::Disconnected(_)),
            disconnected
        );
        assert!(request.expired());
    }
}

#[test]
fn pending_state_is_not_transferable_to_another_or_resumed_client() {
    let (mut client, _, requests) = controlled_client();
    client
        .begin_query(QueryKind::Status, &Wait::default())
        .expect("begin");
    let request = requests.recv().expect("query request");
    let (mut other, _, _) = controlled_client();
    assert!(matches!(other.poll_query(), Poll::Ready(Err(_))));
    let state = client.disconnect();
    assert!(request.expired());
    let backend = backend_with(Seen::default(), |message, sink, id| {
        if let Message::Query {
            query_id,
            category: 0,
        } = message
        {
            sink.submitted(id);
            sink.received(identity_with(query_id, UID, [6; 16], 1));
        }
    });
    let mut resumed = Client::resume(backend, state);
    assert!(matches!(resumed.poll_query(), Poll::Ready(Err(_))));
    assert!(
        resumed
            .begin_query(QueryKind::Status, &Wait::default())
            .is_err()
    );
    identify_client(&mut resumed);
    assert!(matches!(resumed.poll_query(), Poll::Ready(Err(_))));
    let (mut closing, _, requests) = controlled_client();
    closing
        .begin_query(QueryKind::Status, &Wait::default())
        .expect("begin");
    let closing_request = requests.recv().expect("query request");
    closing.close();
    assert!(closing_request.expired());
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
            sink.received(unavailable_with(
                query_id.wrapping_sub(1),
                NONCE,
                UnavailableSubject::Status,
                0x0101,
            ));
            sink.received(unavailable_with(
                query_id,
                NONCE,
                UnavailableSubject::Measurements,
                0x0102,
            ));
            sink.received(unavailable_with(
                query_id,
                NONCE,
                UnavailableSubject::Status,
                0x0103,
            ));
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify_client(&mut client);

    let error = client
        .status(&Wait::new(Duration::from_millis(100)))
        .expect_err("only the matching status reply may finish the query");
    assert!(matches!(error.failure, Failure::DataUnavailable(fields) if fields.reason == 0x0103));
    assert_eq!(seen.count(MessageKind::Query), 2);
}

#[test]
fn matching_reply_before_local_submission_is_preserved() {
    let backend = backend_with(Seen::default(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.received(identity(query_id));
            sink.submitted(request_id);
        }
        Message::Query {
            query_id,
            category: 1,
        } => {
            sink.received(unavailable_with(
                query_id,
                NONCE,
                UnavailableSubject::Status,
                0x0105,
            ));
            sink.submitted(request_id);
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify_client(&mut client);

    let error = client
        .status(&Wait::new(Duration::from_millis(100)))
        .expect_err("early matching response remains associated with this query");
    assert!(matches!(error.failure, Failure::DataUnavailable(fields) if fields.reason == 0x0105));
    assert_eq!(error.local, LocalStage::FullySubmitted);
}

#[test]
fn synchronous_query_cancel_after_take_keeps_stage_and_allows_explicit_stop() {
    let wait = Wait::default();
    let cancel = wait.cancellation.clone();
    let (taken_tx, taken_rx) = std::sync::mpsc::sync_channel(1);
    let backend = Backend::spawn("eha-sdk-sync-cancel-test", move |commands, sink| {
        let mut pump = Pump::new(commands, sink);
        while pump.poll() {
            while let Some(request) = pump.take(1) {
                match protocol::decode(&request.bytes, Direction::HostToFirmware).expect("request")
                {
                    Message::Query {
                        query_id,
                        category: 0,
                    } => {
                        pump.sink.submitted(request.id);
                        pump.sink.received(identity(query_id));
                    }
                    Message::Query { category: 1, .. } => {
                        cancel.cancel();
                        taken_tx.send(request).expect("record taken request");
                    }
                    Message::Stop => pump.sink.submitted(request.id),
                    other => panic!("unexpected request: {other:?}"),
                }
            }
            thread::yield_now();
        }
    })
    .expect("backend starts");
    let mut client = Client::new(backend);
    identify_client(&mut client);
    let error = client.status(&wait).expect_err("wait cancelled after take");
    assert!(matches!(error.failure, Failure::Cancelled));
    assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
    let taken = taken_rx.recv().expect("worker owns request");
    assert!(!taken.expired(), "查询等待取消不能中止在途 I/O");
    client
        .stop_control(&Wait::default())
        .expect("explicit stop remains available");
}

#[test]
fn unmatched_events_do_not_extend_deadline() {
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
                pump.sink
                    .received(unavailable(wrong_query_id, UnavailableSubject::Status));
            }
            thread::sleep(Duration::from_millis(1));
        }
    })
    .expect("test backend starts");
    let mut client = Client::new(backend);
    identify_client(&mut client);

    let started = Instant::now();
    let error = client
        .status(&Wait::new(Duration::from_millis(25)))
        .expect_err("unmatched replies must not keep this query alive");
    assert!(matches!(error.failure, Failure::Timeout));
    assert_eq!(error.local, LocalStage::FullySubmitted);
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[test]
fn matching_pre_submit_rejection_is_returned_without_sending_a_second_request() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::SaveAndRestart { query_id, .. } => {
            sink.received(unavailable(query_id, UnavailableSubject::SaveAndRestart));
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify_client(&mut client);

    let error = client
        .save_and_restart(VALID_CONFIG, &Wait::new(Duration::from_millis(100)))
        .expect_err("matching rejection before Submitted is observable");
    assert!(
        matches!(error.failure, Failure::DataUnavailable(fields) if fields.subject == UnavailableSubject::SaveAndRestart)
    );
    assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
    assert_eq!(seen.count(MessageKind::SaveAndRestart), 1);
}

#[test]
fn late_mutation_rejection_does_not_pollute_the_next_query_or_mutation() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::SaveAndRestart { query_id, .. } => {
            sink.submitted(request_id);
            sink.received(unavailable(query_id, UnavailableSubject::SaveAndRestart));
        }
        Message::Query {
            query_id,
            category: 1,
        } => {
            sink.submitted(request_id);
            sink.received(unavailable_with(
                query_id,
                NONCE,
                UnavailableSubject::Status,
                0x0101,
            ));
        }
        Message::ResetApplication { .. } => sink.submitted(request_id),
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify_client(&mut client);

    client
        .save_and_restart(VALID_CONFIG, &Wait::new(Duration::from_millis(100)))
        .expect("Submitted wins over a later device rejection");
    let status = client
        .status(&Wait::new(Duration::from_millis(100)))
        .expect_err("late mutation rejection is not reused as a status reply");
    assert!(matches!(status.failure, Failure::DataUnavailable(fields) if fields.reason == 0x0101));
    client
        .reset_application(&Wait::new(Duration::from_millis(100)))
        .expect("late rejection is not cached or used to block the next explicit request");
    assert_eq!(seen.count(MessageKind::SaveAndRestart), 1);
    assert_eq!(seen.count(MessageKind::Query), 2);
    assert_eq!(seen.count(MessageKind::ResetApplication), 1);
}

#[test]
fn restore_factory_reuses_the_read_factory_bytes_for_one_save_and_restart() {
    let seen = Seen::default();
    let sent_record = Arc::new(Mutex::new(None));
    let expected_record = VALID_CONFIG.to_vec();
    let backend = backend_with(seen.clone(), {
        let sent_record = sent_record.clone();
        let expected_record = expected_record.clone();
        move |message, sink, request_id| match message {
            Message::Query {
                query_id,
                category: 0,
            } => {
                sink.submitted(request_id);
                sink.received(identity(query_id));
            }
            Message::ReadConfig { query_id, view } if view == ConfigView::Factory as u8 => {
                sink.submitted(request_id);
                sink.received(config(query_id, ConfigView::Factory, &expected_record));
            }
            Message::SaveAndRestart {
                identity, record, ..
            } => {
                let identity = identity.fields();
                assert_eq!(identity.uid, UID);
                assert_eq!(identity.run_nonce, NONCE);
                *sent_record.lock().expect("record lock") = Some(record.to_vec());
                sink.submitted(request_id);
            }
            other => panic!("unexpected request: {other:?}"),
        }
    });
    let mut client = Client::new(backend);
    identify_client(&mut client);

    client
        .restore_factory_and_restart(&Wait::new(Duration::from_millis(100)))
        .expect("factory bytes are submitted locally");
    assert_eq!(
        *sent_record.lock().expect("record lock"),
        Some(expected_record)
    );
    assert_eq!(seen.count(MessageKind::ReadConfig), 1);
    assert_eq!(seen.count(MessageKind::SaveAndRestart), 1);
}

#[test]
fn invalid_save_is_rejected_before_the_write_side_effect() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        other => panic!("invalid config must not send {other:?}"),
    });
    let mut client = Client::new(backend);
    identify_client(&mut client);

    let error = client
        .save_and_restart(b"{not json", &Wait::new(Duration::from_millis(100)))
        .expect_err("invalid record is local input failure");
    assert!(matches!(error.failure, Failure::InvalidInput(_)));
    assert_eq!(error.local, LocalStage::NotSubmitted);
    assert_eq!(seen.count(MessageKind::SaveAndRestart), 0);
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
        identify_client(&mut client);
        if explicit_close {
            client.close();
        } else {
            drop(client);
        }
        assert_eq!(seen.snapshot(), vec![MessageKind::Query]);
    }
}

#[test]
fn resumed_client_requires_fresh_identity_before_control_can_resume() {
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
    identify_client(&mut first);
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
        Err(error) if matches!(error.failure, Failure::Session(_))
    ));
    assert_eq!(seen.count(MessageKind::Velocity), 0);
    identify_client(&mut resumed);
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
                    pump.sink.received(unavailable_with(
                        query_id,
                        NONCE,
                        UnavailableSubject::Status,
                        0x0201,
                    ));
                    delayed_status = None;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    })
    .expect("test backend starts");
    let mut client = Client::new(backend);
    identify_client(&mut client);
    client
        .start_heartbeat()
        .expect("identity enables heartbeat");

    let error = client
        .status(&Wait::new(Duration::from_millis(200)))
        .expect_err("fixture returns a status-unavailable reply after waiting");
    assert!(matches!(error.failure, Failure::DataUnavailable(_)));
    assert!(client.heartbeat_status().submitted >= 2);
    assert!(seen.count(MessageKind::Heartbeat) >= 2);
    client.stop_heartbeat().expect("explicitly stop scheduler");
}

#[test]
fn ordinary_transport_error_propagates_without_becoming_a_device_reply() {
    let backend = backend_with(Seen::default(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::Query { category: 1, .. } => {
            sink.submitted(request_id);
            sink.event(Event::TransportError {
                detail: "test transport failure".into(),
                raw: vec![1, 2, 3],
            });
        }
        other => panic!("unexpected request: {other:?}"),
    });
    let mut client = Client::new(backend);
    identify_client(&mut client);

    let error = client
        .status(&Wait::new(Duration::from_millis(100)))
        .expect_err("transport failure is surfaced to the caller");
    assert!(
        matches!(
            error.failure,
        Failure::Transport { ref detail, ref raw } if detail == "test transport failure" && raw.as_slice() == [1, 2, 3]
        ),
        "unexpected transport boundary: {error:?}"
    );
    assert_eq!(error.local, LocalStage::FullySubmitted);
}

#[test]
fn timeout_cancel_and_disconnect_do_not_retry_mutations_or_leave_a_local_fence() {
    #[derive(Clone, Copy)]
    enum FailureCase {
        Timeout,
        Cancel,
        Disconnect,
    }

    for case in [
        FailureCase::Timeout,
        FailureCase::Cancel,
        FailureCase::Disconnect,
    ] {
        let seen = Seen::default();
        let cancellation = Cancellation::default();
        let backend = backend_with(seen.clone(), {
            let cancellation = cancellation.clone();
            move |message, sink, request_id| match message {
                Message::Query {
                    query_id,
                    category: 0,
                } => {
                    sink.submitted(request_id);
                    sink.received(identity(query_id));
                }
                Message::SaveAndRestart { .. } => match case {
                    FailureCase::Cancel => cancellation.cancel(),
                    FailureCase::Disconnect => sink.disconnected("test disconnect"),
                    FailureCase::Timeout => {}
                },
                Message::ResetApplication { .. } if !matches!(case, FailureCase::Disconnect) => {
                    sink.submitted(request_id)
                }
                other => panic!("unexpected request: {other:?}"),
            }
        });
        let mut client = Client::new(backend);
        identify_client(&mut client);
        let wait = Wait {
            timeout: Duration::from_millis(25),
            cancellation,
        };

        let error = client
            .save_and_restart(VALID_CONFIG, &wait)
            .expect_err("locally uncertain mutation cannot be retried by the SDK");
        assert!(matches!(error.failure, Failure::ResultUnknown(_)));
        assert_eq!(seen.count(MessageKind::SaveAndRestart), 1);

        if !matches!(case, FailureCase::Disconnect) {
            client
                .reset_application(&Wait::new(Duration::from_millis(50)))
                .expect("a later explicit mutation is not blocked by hidden host state");
            assert_eq!(seen.count(MessageKind::ResetApplication), 1);
        }
    }
}

#[test]
fn restore_factory_timeout_does_not_submit_a_save() {
    let seen = Seen::default();
    let backend = backend_with(seen.clone(), |message, sink, request_id| match message {
        Message::Query {
            query_id,
            category: 0,
        } => {
            sink.submitted(request_id);
            sink.received(identity(query_id));
        }
        Message::ReadConfig { view, .. } if view == ConfigView::Factory as u8 => {
            sink.submitted(request_id);
        }
        other => panic!("factory timeout must not send {other:?}"),
    });
    let mut client = Client::new(backend);
    identify_client(&mut client);

    let error = client
        .restore_factory_and_restart(&Wait::new(Duration::from_millis(25)))
        .expect_err("unavailable factory read ends this call before save");
    assert!(matches!(error.failure, Failure::Timeout));
    assert_eq!(seen.count(MessageKind::ReadConfig), 1);
    assert_eq!(seen.count(MessageKind::SaveAndRestart), 0);
}

#[test]
fn restore_factory_rejects_wrong_run_or_format_before_save() {
    for case in ["wrong-run", "wrong-format"] {
        let seen = Seen::default();
        let backend = backend_with(
            seen.clone(),
            move |message, sink, request_id| match message {
                Message::Query {
                    query_id,
                    category: 0,
                } => {
                    sink.submitted(request_id);
                    let format = if case == "wrong-format" { 2 } else { 1 };
                    sink.received(identity_with(query_id, UID, NONCE, format));
                }
                Message::ReadConfig { query_id, view } if view == ConfigView::Factory as u8 => {
                    sink.submitted(request_id);
                    let nonce = if case == "wrong-run" { [8; 16] } else { NONCE };
                    sink.received(config_with_nonce(
                        query_id,
                        nonce,
                        ConfigView::Factory,
                        VALID_CONFIG,
                    ));
                }
                other => panic!("invalid factory source must not send {other:?}"),
            },
        );
        let mut client = Client::new(backend);
        identify_client(&mut client);

        let error = client
            .restore_factory_and_restart(&Wait::new(Duration::from_millis(25)))
            .expect_err("different factory run or format must not become a save");
        assert!(matches!(
            error.failure,
            Failure::Timeout | Failure::InvalidInput(_)
        ));
        assert_eq!(seen.count(MessageKind::SaveAndRestart), 0);
    }
}
