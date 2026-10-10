// Copyright The eha-sdk Contributors

//! 行为级试验回归：使用真实 `Client`、协议编解码和后端提交边界，而非复制试验判断。

#![allow(clippy::panic)] // 测试后端遇到未约定请求时必须立即失败。

use super::*;
use eha_sdk::{
    host::{
        Client,
        backend::{Backend, Pump},
    },
    protocol::{
        self, Direction, Message, SampleData,
        responses::{
            CanProfile, ConfigDataFields, ConfigDataWriter, ConfigRecordState, ConfigView,
            ContactState, DesiredAxis, DriverState, EvidenceState, FallbackReason, IdentityFields,
            IdentityText, ReferenceKind, RunIdentity, SourceQuality, StartupSource, TargetIngress,
            TargetMode, TelemetryFacts, TelemetryFields, ValueFields, ValueResult, ValueState,
            encode_identity, encode_status, encode_telemetry,
        },
    },
};
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Observed {
    pub(crate) positions: Vec<f32>,
    pub(crate) targets: usize,
    pub(crate) stops: usize,
}

/// WebUI group test 的节点行为。仅在 `cfg(test)` 的 `trial_tests` 模块可见。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FixtureBehavior {
    Ready,
    PreflightFails,
    StartUnknown,
    StopUnknown,
}

struct TrialHarness {
    observed: Arc<Mutex<Observed>>,
    fail_next_position: Arc<AtomicBool>,
    telemetry: Arc<Mutex<Vec<Vec<u8>>>>,
}

fn value(number: f32) -> ValueFields {
    ValueFields::new(
        number,
        ValueState::new(ValueResult::Available, SourceQuality::Qualified, false),
    )
}

fn unavailable_value() -> ValueFields {
    ValueFields::new(
        0.0,
        ValueState::new(ValueResult::Never, SourceQuality::Unknown, false),
    )
}

fn identity_bytes(query_id: u32) -> Vec<u8> {
    identity_bytes_for(query_id, [7; 12], 1)
}

fn identity_bytes_for(query_id: u32, uid: [u8; 12], node: u8) -> Vec<u8> {
    let fields = IdentityFields {
        sample: SampleData {
            query_id,
            run_nonce: [9; 16],
            snapshot_sequence: 1,
            snapshot_time_us: 1,
        },
        uid,
        config_source: StartupSource::User,
        fallback_reason: FallbackReason::None,
        run_identity: RunIdentity::Random,
        update_route: eha_sdk::protocol::responses::UpdateRoute::Absent,
        active_can_node: u32::from(node),
        active_can_profile: CanProfile::Fd1m2m,
        host_heartbeat_hz: 100,
        telemetry_hz: 100,
        host_contact_max_age_ms: 300,
        config_format_version: 1,
        build_evidence: EvidenceState::Available,
        odrive_evidence: EvidenceState::Unavailable,
        brt27_evidence: EvidenceState::Unavailable,
        high_water_operation_id: 0,
        retained_operation_id: 0,
        text: IdentityText {
            version: "trial-test",
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
    let length = encode_identity(&fields, &mut bytes).expect("identity encodes");
    bytes.truncate(length);
    bytes
}

#[allow(clippy::too_many_arguments)] // 按协议字段显式构造相互独立的测量事实。
fn telemetry_bytes(
    run_nonce: [u8; 16],
    sequence: u32,
    target: TargetMode,
    target_position_mm: f32,
    position_mm: f32,
    axis_state_raw: u32,
    qualified: bool,
    unknown_effect: bool,
) -> Vec<u8> {
    telemetry_bytes_with_facts(
        run_nonce,
        sequence,
        target,
        target_position_mm,
        position_mm,
        axis_state_raw,
        qualified,
        unknown_effect,
        target != TargetMode::None,
        false,
    )
}

#[allow(clippy::too_many_arguments)] // 测试需独立控制目标、驱动状态和执行事实。
fn telemetry_bytes_with_facts(
    run_nonce: [u8; 16],
    sequence: u32,
    target: TargetMode,
    target_position_mm: f32,
    position_mm: f32,
    axis_state_raw: u32,
    qualified: bool,
    unknown_effect: bool,
    output_allowed: bool,
    device_operation_pending: bool,
) -> Vec<u8> {
    let fields = TelemetryFields {
        sample: SampleData {
            query_id: 0,
            run_nonce,
            snapshot_sequence: sequence,
            snapshot_time_us: u64::from(sequence) * 1_000,
        },
        decision_age_us: 0,
        target_mode: target,
        target_ingress: if target == TargetMode::None {
            TargetIngress::None
        } else {
            TargetIngress::Usb
        },
        facts: TelemetryFacts {
            output_allowed,
            device_operation_pending,
            unknown_effect,
            ..Default::default()
        },
        target_values: if target == TargetMode::None {
            [0.0, 0.0, 0.0]
        } else {
            [target_position_mm, 0.0, 0.0]
        },
        can_adoption_blockers: 0,
        usb_adoption_blockers: 0,
        output_blockers: 0,
        last_end_reason: 0,
        limits: Default::default(),
        main_values: [
            value(position_mm),
            value(0.0),
            value(0.0),
            value(0.0),
            value(0.0),
        ],
        position_age_us: 0,
        velocity_age_us: 0,
        pressure_pair_age_us: 0,
        reference: unavailable_value(),
        reference_kind: ReferenceKind::None,
        candidate_rpm: value(0.0),
        last_submitted_rpm: value(0.0),
        submitted_age_us: 0,
        axis_state_raw,
        axis_error_raw: 0,
        driver_age_us: 0,
        driver_state: DriverState {
            has_status: true,
            qualified,
            stale: false,
            faulted: false,
        },
        desired_axis: if target == TargetMode::None {
            DesiredAxis::Idle
        } else {
            DesiredAxis::ClosedLoop
        },
        // `Never` requires its age sentinel; the selected USB contact is active.
        can_heartbeat_age_us: u32::MAX,
        usb_heartbeat_age_us: 0,
        can_contact: ContactState::Never,
        usb_contact: ContactState::Active,
    };
    let mut bytes = vec![0; TelemetryFields::encoded_len()];
    encode_telemetry(&fields, &mut bytes).expect("telemetry encodes");
    bytes
}

fn trial_request(command: Command) -> TrialRequest {
    TrialRequest {
        command,
        envelope: TrialEnvelope {
            position_min_mm: -10.0,
            position_max_mm: 10.0,
            velocity_abs_max_mm_s: 2.0,
            force_abs_max_n: 10.0,
            stiffness_max_n_per_mm: 10.0,
            damping_max_ns_per_mm: 10.0,
            duration_max_s: 5.0,
        },
        duration_s: Some(5.0),
        reach: None,
    }
}

#[test]
fn preflight_rejects_measured_velocity_or_force_outside_the_trial_envelope() {
    let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
    let request = trial_request(Command::Velocity { mm_s: 0.5 });
    session
        .prepare_trial(&request)
        .expect("valid idle preflight");
    let prepared = session.trial.prepared.as_ref().expect("prepared");
    for (index, limit) in [
        (1, request.envelope.velocity_abs_max_mm_s),
        (4, request.envelope.force_abs_max_n),
    ] {
        let mut status = prepared.status.clone();
        status["main_values"][index]["value"] = json!(-(limit + 1.0));
        assert!(
            validate_trial_facts(
                &request,
                &prepared.startup,
                &status,
                session.request.as_ref()
            )
            .is_err()
        );
    }
    assert_eq!(probe.observed.lock().expect("observations").targets, 0);
}

fn active(request: TrialRequest, started_at: Instant, started_sample_time_us: u64) -> ActiveTrial {
    ActiveTrial {
        duration: request.duration_limit().expect("测试试验时长"),
        request,
        started_at,
        pending_position_mm: None,
        last_position_submit: None,
        settled_since: None,
        stop_submission: None,
        stop_reason: None,
        stop_observed: false,
        stop_attempt_finished_at: None,
        started_sample_time_us,
        stop_after_sample_time_us: None,
        settled_sample_time_us: None,
    }
}

fn startup_config_bytes(query_id: u32) -> Vec<u8> {
    let record = br#"{"config":{"protection":{"hard_position_min_mm":-10.0,"hard_position_max_mm":10.0,"hard_velocity_max_mm_s":2.0,"hard_force_max_n":10.0}}}"#;
    let fields = ConfigDataFields {
        sample: SampleData {
            query_id,
            run_nonce: [9; 16],
            snapshot_sequence: 5,
            snapshot_time_us: 5_000,
        },
        view: ConfigView::Startup,
        record_state: ConfigRecordState::Complete,
        startup_source: StartupSource::User,
        fallback_reason: FallbackReason::None,
        data_time_us: 5_000,
    };
    let mut bytes = vec![0; ConfigDataWriter::required_len(record.len()).expect("config length")];
    let mut writer =
        ConfigDataWriter::start(fields, record.len(), &mut bytes).expect("config starts");
    writer.data_mut().copy_from_slice(record);
    let length = writer.finish().expect("config encodes");
    bytes.truncate(length);
    bytes
}

fn can_observation_bytes(query_id: u32, sequence: u32, qualified: bool) -> Vec<u8> {
    let fields = TelemetryFields {
        sample: SampleData {
            query_id,
            run_nonce: [9; 16],
            snapshot_sequence: sequence,
            snapshot_time_us: u64::from(sequence) * 1_000,
        },
        decision_age_us: 0,
        target_mode: TargetMode::None,
        target_ingress: TargetIngress::None,
        facts: TelemetryFacts::default(),
        target_values: [0.0; 3],
        can_adoption_blockers: 0,
        usb_adoption_blockers: 0,
        output_blockers: 0,
        last_end_reason: 0,
        limits: Default::default(),
        main_values: [value(0.0); 5],
        position_age_us: 0,
        velocity_age_us: 0,
        pressure_pair_age_us: 0,
        reference: unavailable_value(),
        reference_kind: ReferenceKind::None,
        candidate_rpm: value(0.0),
        last_submitted_rpm: value(0.0),
        submitted_age_us: 0,
        axis_state_raw: 1,
        axis_error_raw: 0,
        driver_age_us: 0,
        driver_state: DriverState {
            has_status: true,
            qualified,
            stale: false,
            faulted: false,
        },
        desired_axis: DesiredAxis::Idle,
        can_heartbeat_age_us: 0,
        usb_heartbeat_age_us: u32::MAX,
        can_contact: ContactState::Active,
        usb_contact: ContactState::Never,
    };
    let mut bytes = vec![0; TelemetryFields::encoded_len()];
    let length = if query_id == 0 {
        encode_telemetry(&fields, &mut bytes)
    } else {
        encode_status(&fields, &mut bytes)
    }
    .expect("CAN observation encodes");
    bytes.truncate(length);
    bytes
}

/// 由群组 Workbench 测试持有的观测端。它不接触生产连接器，`complete_stop`
/// 仅向测试 backend 注入一个严格的停止确认遥测。
pub(crate) struct FixtureProbe {
    pub(crate) observed: Arc<Mutex<Observed>>,
    sink: Arc<Mutex<Option<eha_sdk::host::backend::EventSink>>>,
    next_sequence: Arc<std::sync::atomic::AtomicU64>,
}

impl FixtureProbe {
    pub(crate) fn inject_idle(&self, sequence: u32) {
        self.inject_idle_at(sequence, Instant::now());
    }

    pub(crate) fn inject_idle_at(&self, sequence: u32, received_at: Instant) {
        let sink = self
            .sink
            .lock()
            .expect("fixture sink lock")
            .as_ref()
            .expect("fixture backend starts before Identity")
            .clone();
        sink.event(eha_sdk::host::backend::Event::Message(
            eha_sdk::host::backend::Received {
                bytes: can_observation_bytes(0, sequence, true),
                at: received_at,
            },
        ));
    }

    pub(crate) fn complete_stop(&self) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::AcqRel) as u32;
        self.inject_idle(sequence);
    }
}

/// 供 `webui::workbench` 的群组状态机测试复用的完整 SDK 会话。它实现 Identity、
/// Startup Config、Status 和本次运行的被动 Telemetry，因此真实走过 prepare/start。
pub(crate) fn session_fixture(
    node: u8,
    uid: [u8; 12],
    behavior: FixtureBehavior,
) -> (ToolSession, FixtureProbe) {
    let observed = Arc::new(Mutex::new(Observed::default()));
    let sink_slot = Arc::new(Mutex::new(None));
    let sequence = Arc::new(std::sync::atomic::AtomicU64::new(30));
    let backend_observed = Arc::clone(&observed);
    let backend_sink_slot = Arc::clone(&sink_slot);
    let backend = Backend::spawn("eha-tool-group-fixture", move |commands, sink| {
        *backend_sink_slot.lock().expect("fixture sink lock") = Some(sink.clone());
        let mut pump = Pump::new(commands, sink);
        while pump.poll() {
            for lane in 0..3 {
                while let Some(request) = pump.take(lane) {
                    let message = protocol::decode(&request.bytes, Direction::HostToFirmware)
                        .expect("tool emits contract-valid request");
                    match message {
                        Message::Query {
                            query_id,
                            category: 0,
                        } => {
                            pump.sink.submitted(request.id);
                            pump.sink.received(identity_bytes_for(query_id, uid, node));
                        }
                        Message::ReadConfig { query_id, view: 2 } => {
                            pump.sink.submitted(request.id);
                            pump.sink.received(startup_config_bytes(query_id));
                        }
                        Message::Query {
                            query_id,
                            category: 1,
                        } => {
                            pump.sink.submitted(request.id);
                            let qualified = behavior != FixtureBehavior::PreflightFails;
                            // 先交付启动所需的被动事实，再唤醒 Status 等待者；测试不依赖
                            // 两次 EventSink 写入之间的线程调度。设备快照时间仍晚于 Status。
                            pump.sink.received(can_observation_bytes(0, 11, qualified));
                            pump.sink
                                .received(can_observation_bytes(query_id, 10, qualified));
                        }
                        Message::Position(mm) | Message::Velocity(mm) | Message::Force(mm) => {
                            let mut observed = backend_observed.lock().expect("observed lock");
                            observed.targets += 1;
                            if matches!(message, Message::Position(_)) {
                                observed.positions.push(mm);
                            }
                            drop(observed);
                            if behavior == FixtureBehavior::StartUnknown {
                                pump.sink.failed(request.id, "synthetic target loss", true);
                            } else {
                                pump.sink.submitted(request.id);
                            }
                        }
                        Message::Impedance { .. } => {
                            backend_observed.lock().expect("observed lock").targets += 1;
                            if behavior == FixtureBehavior::StartUnknown {
                                pump.sink.failed(request.id, "synthetic target loss", true);
                            } else {
                                pump.sink.submitted(request.id);
                            }
                        }
                        Message::Stop => {
                            backend_observed.lock().expect("observed lock").stops += 1;
                            if behavior == FixtureBehavior::StopUnknown {
                                pump.sink.failed(request.id, "synthetic stop loss", true);
                            } else {
                                pump.sink.submitted(request.id);
                            }
                        }
                        other => panic!("unexpected group fixture request: {other:?}"),
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    })
    .expect("group fixture backend starts");
    let mut client = Client::new(backend);
    let identity = client
        .identify(None, &eha_sdk::host::Wait::new(Duration::from_millis(100)))
        .expect("fixture Identity succeeds");
    let mut session = ToolSession::new().with_timeout(Duration::from_millis(100));
    session.request = Some(ConnectionRequest::Can {
        channel: format!("fixture-{node}"),
        node,
        mode: "fd".into(),
        python: None,
    });
    session.identity = Some(identity);
    session.client = Some(client);
    (
        session,
        FixtureProbe {
            observed,
            sink: sink_slot,
            next_sequence: sequence,
        },
    )
}

fn harness() -> (ToolSession, TrialHarness) {
    let observed = Arc::new(Mutex::new(Observed::default()));
    let fail_next_position = Arc::new(AtomicBool::new(false));
    let telemetry = Arc::new(Mutex::new(Vec::new()));
    let backend_observed = Arc::clone(&observed);
    let backend_fail_next_position = Arc::clone(&fail_next_position);
    let backend_telemetry = Arc::clone(&telemetry);
    let backend = Backend::spawn("eha-tool-trial-test", move |commands, sink| {
        let mut pump = Pump::new(commands, sink);
        while pump.poll() {
            let queued = {
                let mut queued = backend_telemetry.lock().expect("telemetry lock");
                std::mem::take(&mut *queued)
            };
            for bytes in queued {
                pump.sink.received(bytes);
            }
            for lane in 0..3 {
                while let Some(request) = pump.take(lane) {
                    let message = protocol::decode(&request.bytes, Direction::HostToFirmware)
                        .expect("tool emits contract-valid request");
                    match message {
                        Message::Query {
                            query_id,
                            category: 0,
                        } => {
                            pump.sink.submitted(request.id);
                            pump.sink.received(identity_bytes(query_id));
                        }
                        Message::Position(mm) => {
                            backend_observed
                                .lock()
                                .expect("observed lock")
                                .positions
                                .push(mm);
                            if backend_fail_next_position.swap(false, Ordering::AcqRel) {
                                pump.sink
                                    .failed(request.id, "synthetic position loss", true);
                            } else {
                                pump.sink.submitted(request.id);
                            }
                        }
                        Message::Stop => {
                            backend_observed.lock().expect("observed lock").stops += 1;
                            pump.sink.submitted(request.id);
                        }
                        other => panic!("unexpected request in trial state test: {other:?}"),
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    })
    .expect("memory backend starts");
    let mut client = Client::new(backend);
    let identity = client
        .identify(None, &eha_sdk::host::Wait::new(Duration::from_millis(100)))
        .expect("Identity succeeds");
    let mut session = ToolSession::new().with_timeout(Duration::from_millis(100));
    session.request = Some(ConnectionRequest::Usb {
        serial: "trial-test".into(),
    });
    session.identity = Some(identity);
    session.client = Some(client);
    (
        session,
        TrialHarness {
            observed,
            fail_next_position,
            telemetry,
        },
    )
}

fn tick_after_telemetry(
    session: &mut ToolSession,
    harness: &TrialHarness,
    bytes: Vec<u8>,
    expected_sequence: Option<u32>,
) {
    harness
        .telemetry
        .lock()
        .expect("telemetry lock")
        .push(bytes);
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        session.tick();
        if expected_sequence.is_some_and(|sequence| {
            session
                .last_telemetry
                .as_ref()
                .and_then(|reply| reply_json(reply).ok())
                .and_then(|telemetry| telemetry["sample"]["snapshot_sequence"].as_u64())
                == Some(u64::from(sequence))
        }) {
            return;
        }
        if expected_sequence.is_none() && Instant::now() + Duration::from_millis(10) >= deadline {
            return;
        }
        assert!(Instant::now() < deadline, "telemetry never reached Client");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn deadline_from_initial_unknown_telemetry_submits_one_stop() {
    let (mut session, harness) = harness();
    let request = trial_request(Command::Velocity { mm_s: 0.5 });
    let mut request = request;
    request.duration_s = Some(0.01);
    session.trial.active = Some(active(
        request,
        Instant::now() - Duration::from_millis(20),
        0,
    ));

    // 起始时没有任何 Telemetry；deadline 仍只构成一次明确 Stop 意图。
    session.tick();
    session.tick();

    assert_eq!(harness.observed.lock().expect("observed lock").stops, 1);
    assert_eq!(session.trial_snapshot()["state"], "stopping");
    assert_eq!(session.trial_snapshot()["stop_reason"], "duration_elapsed");
}

#[test]
fn reconnect_while_connected_preserves_active_trial_without_sending() {
    let (mut session, harness) = harness();
    session.trial.active = Some(active(
        trial_request(Command::Velocity { mm_s: 0.5 }),
        Instant::now(),
        0,
    ));

    let error = session
        .reconnect()
        .expect_err("a connected session does not reconnect");
    assert_eq!(error.kind.as_ref(), "invalid_input");
    assert_eq!(session.trial_snapshot()["state"], "active");
    let observed = harness.observed.lock().expect("observed lock");
    assert!(observed.positions.is_empty());
    assert_eq!(observed.stops, 0);
}

#[test]
fn position_slider_coalesces_to_latest_and_is_limited_to_twenty_hz() {
    let (mut session, harness) = harness();
    let request = trial_request(Command::Position { mm: 0.0 });
    session.trial.active = Some(active(request, Instant::now(), 0));

    session
        .update_trial_position(1.0)
        .expect("first target merges");
    session
        .update_trial_position(2.0)
        .expect("latest target replaces it");
    session.tick();
    assert_eq!(
        harness.observed.lock().expect("observed lock").positions,
        vec![2.0]
    );

    session
        .update_trial_position(3.0)
        .expect("next target merges");
    session.tick();
    assert_eq!(
        harness.observed.lock().expect("observed lock").positions,
        vec![2.0],
        "50 ms cadence prevents a second wire target"
    );
    thread::sleep(Duration::from_millis(55));
    session.tick();
    assert_eq!(
        harness.observed.lock().expect("observed lock").positions,
        vec![2.0, 3.0]
    );
}

#[test]
fn target_adoption_transition_does_not_turn_initial_submit_into_stop() {
    let (mut session, harness) = harness();
    let request = trial_request(Command::Position { mm: 5.0 });
    session.trial.active = Some(active(request, Instant::now(), 10));

    // This is the actual first local target submission, followed by the normal firmware
    // adoption window where it still reports Idle / output disallowed / pending.
    session
        .execute_trial_command(Command::Position { mm: 5.0 })
        .expect("initial position submits locally");
    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes_with_facts(
            [9; 16],
            20,
            TargetMode::Position,
            5.0,
            0.0,
            1,
            true,
            false,
            false,
            true,
        ),
        Some(20),
    );
    assert_eq!(session.trial_snapshot()["state"], "active");
    assert_eq!(harness.observed.lock().expect("observed lock").stops, 0);

    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes_with_facts(
            [9; 16],
            21,
            TargetMode::Position,
            5.0,
            0.0,
            2,
            true,
            false,
            true,
            false,
        ),
        Some(21),
    );
    let observed = harness.observed.lock().expect("observed lock");
    assert_eq!(observed.positions, vec![5.0]);
    assert_eq!(observed.stops, 0);
    assert_eq!(session.trial_snapshot()["state"], "active");
}

#[test]
fn unknown_position_submission_stops_once_and_never_replays_a_target() {
    let (mut session, harness) = harness();
    let request = trial_request(Command::Position { mm: 0.0 });
    session.trial.active = Some(active(request, Instant::now(), 0));
    harness.fail_next_position.store(true, Ordering::Release);

    session.update_trial_position(4.0).expect("target merges");
    session.tick();
    assert_eq!(
        harness.observed.lock().expect("observed lock").positions,
        vec![4.0]
    );
    assert_eq!(harness.observed.lock().expect("observed lock").stops, 1);
    assert!(session.update_trial_position(5.0).is_err());
    thread::sleep(Duration::from_millis(55));
    session.tick();

    let observed = harness.observed.lock().expect("observed lock");
    assert_eq!(observed.positions, vec![4.0]);
    assert_eq!(observed.stops, 1);
    assert_eq!(session.trial_snapshot()["state"], "stopping");
}

#[test]
fn stop_needs_fresh_raw_idle_qualified_and_clear_facts() {
    let (mut session, harness) = harness();
    let request = trial_request(Command::Velocity { mm_s: 0.5 });
    let mut active = active(request, Instant::now(), 10);
    active.stop_submission = Some(json!({"local_submission": {"id": 1}}));
    active.stop_attempt_finished_at = Some(Instant::now() - Duration::from_millis(1));
    active.stop_after_sample_time_us = Some(10);
    session.trial.active = Some(active);

    // desired_axis=Idle alone cannot complete: raw state remains ClosedLoop.
    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes([9; 16], 20, TargetMode::None, 0.0, 0.0, 2, true, false),
        Some(20),
    );
    assert_eq!(session.trial_snapshot()["state"], "stopping");

    // Raw Idle still needs qualified driver evidence.
    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes([9; 16], 21, TargetMode::None, 0.0, 0.0, 1, false, false),
        Some(21),
    );
    assert_eq!(session.trial_snapshot()["state"], "stopping");

    // Unknown device effect prevents a Stop acknowledgement from being inferred.
    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes([9; 16], 22, TargetMode::None, 0.0, 0.0, 1, true, true),
        Some(22),
    );
    assert_eq!(session.trial_snapshot()["state"], "stopping");

    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes([9; 16], 23, TargetMode::None, 0.0, 0.0, 1, true, false),
        Some(23),
    );
    assert_eq!(session.trial_snapshot()["state"], "completed");
}

#[test]
fn stop_confirmation_requires_a_receipt_after_stop_submission() {
    let (mut session, probe) = session_fixture(1, [1; 12], FixtureBehavior::Ready);
    session.trial.active = Some(active(
        trial_request(Command::Velocity { mm_s: 0.5 }),
        Instant::now(),
        0,
    ));

    // Cache T100, then leave a newer valid Idle frame T101 in Client's inbox.  It was
    // physically received before Stop and therefore cannot acknowledge that Stop.
    probe.inject_idle(100);
    session.tick();
    probe.inject_idle(101);
    session.stop_trial().expect("Stop submits locally");
    session.tick();
    assert_eq!(session.trial_snapshot()["state"], "stopping");

    // A fresh receipt after the local Stop submission is needed in addition to its
    // newer device timestamp and strict idle/driver/fact fields.
    probe.inject_idle(102);
    session.tick();
    assert_eq!(session.trial_snapshot()["state"], "completed");
}

#[test]
fn unknown_stop_is_not_retried_and_can_only_finish_from_later_telemetry() {
    let (mut session, probe) = session_fixture(1, [3; 12], FixtureBehavior::StopUnknown);
    session.trial.active = Some(active(
        trial_request(Command::Velocity { mm_s: 0.5 }),
        Instant::now(),
        0,
    ));
    probe.inject_idle(100);
    session.tick();

    let error = session
        .stop_trial()
        .expect_err("Stop transport outcome is unknown");
    assert!(error.unknown);
    assert_eq!(probe.observed.lock().expect("observed lock").stops, 1);
    assert_eq!(
        session.trial_snapshot()["stop_submission"]["device_execution"],
        "unknown_no_retry"
    );
    // A repeated user/tick path is idempotent and must not put a second Stop on the wire.
    assert!(session.stop_trial().is_ok());
    assert_eq!(probe.observed.lock().expect("observed lock").stops, 1);

    probe.inject_idle(101);
    session.tick();
    assert_eq!(session.trial_snapshot()["state"], "completed");
}

#[test]
fn stale_telemetry_stops_even_when_its_device_sample_is_not_new() {
    let (mut session, probe) = session_fixture(1, [2; 12], FixtureBehavior::Ready);
    // The sample-time gate would otherwise ignore this frame.  Receipt freshness must be
    // checked first, so an old retained sample cannot keep a trial alive indefinitely.
    session.trial.active = Some(active(
        trial_request(Command::Velocity { mm_s: 0.5 }),
        Instant::now(),
        200_000,
    ));
    probe.inject_idle_at(100, Instant::now() - Duration::from_millis(300));
    session.tick();

    assert_eq!(probe.observed.lock().expect("observed lock").stops, 1);
    assert_eq!(session.trial_snapshot()["state"], "stopping");
    assert_eq!(session.trial_snapshot()["stop_reason"], "telemetry_stale");
}

#[test]
fn reach_requires_a_new_matching_current_run_sample() {
    let (mut session, harness) = harness();
    let mut request = trial_request(Command::Position { mm: 5.0 });
    request.reach = Some(ReachCondition {
        tolerance_mm: 0.1,
        settle_ms: 2,
    });
    session.trial.active = Some(active(request, Instant::now(), 10));

    // A frame from another run is rejected by Client::telemetry and cannot start settling.
    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes([8; 16], 20, TargetMode::Position, 5.0, 5.0, 2, true, false),
        None,
    );
    assert_eq!(session.trial_snapshot()["state"], "active");

    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes([9; 16], 21, TargetMode::Position, 5.0, 5.0, 2, true, false),
        Some(21),
    );
    thread::sleep(Duration::from_millis(4));
    // Reusing the same sample does not accumulate settling time into a false arrival.
    session.tick();
    assert_eq!(session.trial_snapshot()["state"], "active");

    tick_after_telemetry(
        &mut session,
        &harness,
        telemetry_bytes([9; 16], 22, TargetMode::Position, 5.0, 5.0, 2, true, false),
        Some(22),
    );
    assert_eq!(harness.observed.lock().expect("observed lock").stops, 1);
    assert_eq!(session.trial_snapshot()["state"], "stopping");
    assert_eq!(session.trial_snapshot()["stop_reason"], "position_reached");
}

#[test]
fn trial_duration_rejects_unrepresentable_clock_values() {
    let mut request = trial_request(Command::Position { mm: 0.0 });
    for duration in [
        f32::MAX,
        f32::from_bits(1),
        f32::NAN,
        f32::INFINITY,
        -1.0,
        0.0,
    ] {
        request.duration_s = Some(duration);
        request.envelope.duration_max_s = duration;
        assert!(validate_trial_request(&request).is_err());
    }
    request.duration_s = Some(0.125);
    request.envelope.duration_max_s = 1.0;
    assert_eq!(
        request.duration_limit().expect("可表示期限"),
        Duration::from_millis(125)
    );
}
