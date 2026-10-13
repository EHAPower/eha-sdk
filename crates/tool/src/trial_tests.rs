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
            ContactState, DesiredAxis, DiagnosticsFields, DriverState, EvidenceState,
            FallbackReason, IdentityFields, IdentityText, MeasurementFields, MeasurementValues,
            ModelState, PositiveForceChannel, ReferenceKind, RunIdentity, SourceQuality,
            StartupSource, TargetIngress, TargetMode, TelemetryFacts, TelemetryFields, ValueFields,
            ValueResult, ValueState, encode_diagnostics, encode_identity, encode_measurements,
            encode_status, encode_telemetry,
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
    UiLive,
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
    identity_bytes_with_nonce_for(query_id, uid, node, [9; 16])
}

fn identity_bytes_with_nonce_for(
    query_id: u32,
    uid: [u8; 12],
    node: u8,
    run_nonce: [u8; 16],
) -> Vec<u8> {
    identity_bytes_named(query_id, uid, node, run_nonce, "trial-test")
}

fn identity_bytes_named(
    query_id: u32,
    uid: [u8; 12],
    node: u8,
    run_nonce: [u8; 16],
    version: &str,
) -> Vec<u8> {
    let fields = IdentityFields {
        sample: SampleData {
            query_id,
            run_nonce,
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
        text: IdentityText {
            version,
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

pub(super) fn trial_request(command: Command) -> TrialRequest {
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
        stop_confirmation_timeout: Duration::from_millis(100),
        stop_confirmation_overdue: false,
        stop_recovery_after_received_at: None,
        started_sample_time_us,
        stop_after_sample_time_us: None,
        settled_sample_time_us: None,
    }
}

fn startup_config_bytes(query_id: u32) -> Vec<u8> {
    config_read_bytes(query_id, ConfigView::Startup)
}

fn config_read_bytes(query_id: u32, view: ConfigView) -> Vec<u8> {
    let record = br#"{"config":{"protection":{"hard_position_min_mm":-10.0,"hard_position_max_mm":10.0,"hard_velocity_max_mm_s":2.0,"hard_force_max_n":10.0}}}"#;
    let fields = ConfigDataFields {
        sample: SampleData {
            query_id,
            run_nonce: [9; 16],
            snapshot_sequence: 5,
            snapshot_time_us: 5_000,
        },
        view,
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

fn readonly_observation_bytes(query_id: u32, category: u8) -> Vec<u8> {
    let sample = SampleData {
        query_id,
        run_nonce: [9; 16],
        snapshot_sequence: 12,
        snapshot_time_us: 12_000,
    };
    if category == 3 {
        let fields = DiagnosticsFields {
            sample,
            decision_age_us: 0,
            device_evidence_age_us: 0,
            overflow: false,
            entries: &[],
        };
        let mut bytes = vec![0; fields.encoded_len().expect("diagnostic length")];
        encode_diagnostics(&fields, &mut bytes).expect("diagnostics encode");
        return bytes;
    }
    let value = ValueFields::new(
        0.0,
        ValueState::new(ValueResult::Available, SourceQuality::Qualified, true),
    );
    let fields = MeasurementFields {
        sample,
        values: MeasurementValues {
            position_mm: value,
            velocity_mm_s: value,
            raw_pressure_a_mpa: value,
            raw_pressure_b_mpa: value,
            filtered_pressure_a_mpa: value,
            filtered_pressure_b_mpa: value,
            main_force_n: value,
            protection_force_n: value,
        },
        position_age_us: 0,
        velocity_age_us: 0,
        pressure_pair_age_us: 0,
        position_reference_count: 0,
        effective_area_mm2: 1.0,
        positive_force_channel: PositiveForceChannel::ElectricalB,
        reference_state: true,
        model_state: ModelState::Available,
        pressure_batch_sequence: 1,
    };
    let mut bytes = vec![0; MeasurementFields::encoded_len()];
    encode_measurements(&fields, &mut bytes).expect("measurements encode");
    bytes
}

fn can_observation_bytes(query_id: u32, sequence: u32, qualified: bool) -> Vec<u8> {
    can_ui_observation(
        query_id,
        sequence,
        qualified,
        TargetMode::None,
        [0.0; 3],
        false,
        false,
    )
}

fn can_ui_observation(
    query_id: u32,
    sequence: u32,
    qualified: bool,
    target: TargetMode,
    values: [f32; 3],
    unknown: bool,
    non_idle: bool,
) -> Vec<u8> {
    let fields = TelemetryFields {
        sample: SampleData {
            query_id,
            run_nonce: [9; 16],
            snapshot_sequence: sequence,
            snapshot_time_us: u64::from(sequence) * 1_000,
        },
        decision_age_us: 0,
        target_mode: target,
        target_ingress: if target == TargetMode::None {
            TargetIngress::None
        } else {
            TargetIngress::Can
        },
        facts: TelemetryFacts {
            output_allowed: target != TargetMode::None,
            unknown_effect: unknown,
            ..Default::default()
        },
        target_values: values,
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
        axis_state_raw: if target == TargetMode::None && !non_idle {
            1
        } else {
            8
        },
        axis_error_raw: 0,
        driver_age_us: 0,
        driver_state: DriverState {
            has_status: true,
            qualified,
            stale: false,
            faulted: false,
        },
        desired_axis: if target == TargetMode::None && !non_idle {
            DesiredAxis::Idle
        } else {
            DesiredAxis::ClosedLoop
        },
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
    pub(crate) fn disconnect(&self) {
        self.sink
            .lock()
            .expect("fixture sink")
            .as_ref()
            .expect("started fixture")
            .disconnected("synthetic passive disconnect");
    }

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
    let live = if behavior == FixtureBehavior::UiLive {
        let (trace, _) = crate::service::p5_fixture::Trace::new(Duration::from_secs(15));
        Some(UiOptions {
            scenario: crate::service::p5_fixture::Scenario::Healthy,
            trace,
        })
    } else {
        None
    };
    session_fixture_configured(node, uid, behavior, live)
}

impl ToolSession {
    pub(crate) fn arm_ui_trial_audit_failure(&mut self) -> bool {
        let Some(recording) = self.recording.as_mut() else {
            return false;
        };
        recording.arm_trial_audit_failure();
        true
    }
}

pub(crate) struct UiOptions {
    pub(crate) scenario: crate::service::p5_fixture::Scenario,
    pub(crate) trace: crate::service::p5_fixture::Trace,
}

pub(crate) fn ui_session_fixture(
    node: u8,
    uid: [u8; 12],
    options: UiOptions,
) -> (ToolSession, FixtureProbe) {
    session_fixture_configured(node, uid, FixtureBehavior::UiLive, Some(options))
}

fn session_fixture_configured(
    node: u8,
    uid: [u8; 12],
    behavior: FixtureBehavior,
    live: Option<UiOptions>,
) -> (ToolSession, FixtureProbe) {
    let is_live = live.is_some();
    // 只改变合成观测轴状态，不生成目标、心跳或未决效果。
    let non_idle = live
        .as_ref()
        .is_some_and(|options| options.scenario == crate::service::p5_fixture::Scenario::NotIdle);
    let observed = Arc::new(Mutex::new(Observed::default()));
    let sink_slot = Arc::new(Mutex::new(None));
    let sequence = Arc::new(std::sync::atomic::AtomicU64::new(30));
    let backend_observed = Arc::clone(&observed);
    let backend_sink_slot = Arc::clone(&sink_slot);
    let backend_sequence = Arc::clone(&sequence);
    let backend = Backend::spawn("eha-tool-group-fixture", move |commands, sink| {
        *backend_sink_slot.lock().expect("fixture sink lock") = Some(sink.clone());
        let mut pump = Pump::new(commands, sink);
        let mut next_passive = Instant::now();
        let mut delayed_reads = std::collections::VecDeque::<(Instant, Vec<u8>, u64)>::new();
        let mut target = TargetMode::None;
        let mut target_values = [0.0; 3];
        let mut target_unknown = false;
        let mut pending_stop_observation = false;
        let mut queued_noted = false;
        while pump.poll() {
            if let Some(live) = &live {
                if Instant::now() >= next_passive {
                    let seq = backend_sequence.fetch_add(1, Ordering::Relaxed) as u32;
                    pump.sink.received(can_ui_observation(
                        0,
                        seq,
                        true,
                        target,
                        target_values,
                        target_unknown,
                        non_idle,
                    ));
                    if pending_stop_observation {
                        live.trace.event(
                            "synthetic_post_stop_observation",
                            node,
                            None,
                            json!({"sequence":seq,"target_mode":"none","driver":if non_idle { "closed_loop" } else { "idle" }}),
                        );
                        pending_stop_observation = false;
                    }
                    next_passive = Instant::now() + Duration::from_millis(50);
                }
                while delayed_reads
                    .front()
                    .is_some_and(|(due, _, _)| Instant::now() >= *due)
                {
                    let (_, bytes, id) = delayed_reads.pop_front().expect("due reply");
                    pump.sink.received(bytes);
                    live.trace
                        .event("read_reply", node, Some(id), json!({"delayed":true}));
                }
            }
            for lane in 0..3 {
                while let Some(request) = pump.take(lane) {
                    let message = protocol::decode(&request.bytes, Direction::HostToFirmware)
                        .expect("tool emits contract-valid request");
                    if let Some(live) = &live {
                        use crate::service::p5_fixture::Scenario;
                        let is_target = matches!(
                            message,
                            Message::Position(_)
                                | Message::Velocity(_)
                                | Message::Force(_)
                                | Message::Impedance { .. }
                        );
                        if is_target && live.scenario == Scenario::QueuedTarget {
                            if !queued_noted {
                                live.trace.event(
                                    "target_returned_unsent",
                                    node,
                                    Some(request.id),
                                    json!({"handoff_to_driver":false}),
                                );
                                queued_noted = true;
                            }
                            pump.put_back(request);
                            break;
                        }
                        let action = match message {
                            Message::Position(_) => "position",
                            Message::Velocity(_) => "velocity",
                            Message::Force(_) => "force",
                            Message::Impedance { .. } => "impedance",
                            Message::Stop => "stop",
                            Message::Heartbeat => "heartbeat",
                            _ => "read",
                        };
                        live.trace.event(
                            "pump_taken",
                            node,
                            Some(request.id),
                            json!({"action":action}),
                        );
                        if is_target && live.scenario == Scenario::TakenTarget {
                            live.trace.event(
                                "target_receipt_held",
                                node,
                                Some(request.id),
                                json!({"handoff_to_driver":true,"submission_receipt":false}),
                            );
                            continue;
                        }
                    }
                    match message {
                        Message::Query {
                            query_id,
                            category: 0,
                        } => {
                            pump.sink.submitted(request.id);
                            pump.sink.received(if is_live {
                                identity_bytes_named(query_id, uid, node, [9; 16], "UI_SYNTHETIC")
                            } else {
                                identity_bytes_for(query_id, uid, node)
                            });
                        }
                        Message::ReadConfig { query_id, view: 2 } => {
                            pump.sink.submitted(request.id);
                            pump.sink.received(startup_config_bytes(query_id));
                        }
                        Message::ReadConfig { query_id, view: 1 } => {
                            pump.sink.submitted(request.id);
                            pump.sink
                                .received(config_read_bytes(query_id, ConfigView::UserRecord));
                        }
                        Message::ReadConfig { query_id, view: 0 } => {
                            pump.sink.submitted(request.id);
                            pump.sink
                                .received(config_read_bytes(query_id, ConfigView::Factory));
                        }
                        Message::Query {
                            query_id,
                            category: category @ (2 | 3),
                        } => {
                            if delayed_reads.len() >= 2 {
                                pump.sink.failed(
                                    request.id,
                                    "UI_SYNTHETIC 延迟回复容量已满",
                                    false,
                                );
                                if let Some(live) = &live {
                                    live.trace.event(
                                        "read_capacity_rejected",
                                        node,
                                        Some(request.id),
                                        json!({"query_id":query_id.to_string()}),
                                    );
                                }
                                continue;
                            }
                            pump.sink.submitted(request.id);
                            let bytes = readonly_observation_bytes(query_id, category);
                            if live.as_ref().is_some_and(|options| {
                                options.scenario == crate::service::p5_fixture::Scenario::SlowRead
                            }) {
                                // 已取消等待的旧 query 仍可迟到；两个固定槽保留各自原 wire ID。
                                delayed_reads.push_back((
                                    Instant::now() + Duration::from_secs(3),
                                    bytes,
                                    request.id,
                                ));
                                live.as_ref().expect("live").trace.event(
                                    "read_reply_held",
                                    node,
                                    Some(request.id),
                                    json!({"delay_ms":3000,"query_id":query_id.to_string()}),
                                );
                            } else {
                                pump.sink.received(bytes);
                            }
                        }
                        Message::Query {
                            query_id,
                            category: 1,
                        } => {
                            pump.sink.submitted(request.id);
                            let qualified = behavior != FixtureBehavior::PreflightFails;
                            // 先交付启动所需的被动事实，再唤醒 Status 等待者；测试不依赖
                            // 两次 EventSink 写入之间的线程调度。设备快照时间仍晚于 Status。
                            if is_live {
                                let seq = backend_sequence.fetch_add(2, Ordering::Relaxed) as u32;
                                pump.sink.received(can_ui_observation(
                                    0,
                                    seq + 1,
                                    qualified,
                                    target,
                                    target_values,
                                    target_unknown,
                                    non_idle,
                                ));
                                pump.sink.received(can_ui_observation(
                                    query_id,
                                    seq,
                                    qualified,
                                    target,
                                    target_values,
                                    target_unknown,
                                    non_idle,
                                ));
                            } else {
                                pump.sink.received(can_observation_bytes(0, 11, qualified));
                                pump.sink
                                    .received(can_observation_bytes(query_id, 10, qualified));
                            }
                        }
                        Message::Position(mm) | Message::Velocity(mm) | Message::Force(mm) => {
                            let mut observed = backend_observed.lock().expect("observed lock");
                            observed.targets += 1;
                            if matches!(message, Message::Position(_)) {
                                observed.positions.push(mm);
                            }
                            drop(observed);
                            if is_live {
                                target = match message {
                                    Message::Position(_) => TargetMode::Position,
                                    Message::Velocity(_) => TargetMode::Velocity,
                                    _ => TargetMode::Force,
                                };
                                target_values = [mm, 0.0, 0.0];
                            }
                            if behavior == FixtureBehavior::StartUnknown
                                || live.as_ref().is_some_and(|options| {
                                    options.scenario
                                        == crate::service::p5_fixture::Scenario::TargetUnknown
                                })
                            {
                                target_unknown = true;
                                pump.sink.failed(request.id, "synthetic target loss", true);
                            } else {
                                pump.sink.submitted(request.id);
                            }
                        }
                        Message::Impedance { .. } => {
                            backend_observed.lock().expect("observed lock").targets += 1;
                            if is_live {
                                target = TargetMode::Impedance;
                            }
                            if behavior == FixtureBehavior::StartUnknown
                                || live.as_ref().is_some_and(|options| {
                                    options.scenario
                                        == crate::service::p5_fixture::Scenario::TargetUnknown
                                })
                            {
                                target_unknown = true;
                                pump.sink.failed(request.id, "synthetic target loss", true);
                            } else {
                                pump.sink.submitted(request.id);
                            }
                        }
                        Message::Stop => {
                            backend_observed.lock().expect("observed lock").stops += 1;
                            if behavior == FixtureBehavior::StopUnknown
                                || live.as_ref().is_some_and(|options| {
                                    options.scenario
                                        == crate::service::p5_fixture::Scenario::StopUnknown
                                })
                            {
                                pump.sink.failed(request.id, "synthetic stop loss", true);
                            } else {
                                pump.sink.submitted(request.id);
                                if is_live {
                                    target = TargetMode::None;
                                    target_values = [0.0; 3];
                                    target_unknown = false;
                                    pending_stop_observation = true;
                                }
                            }
                        }
                        Message::Heartbeat if is_live => {
                            pump.sink.submitted(request.id);
                        }
                        other => panic!("unexpected group fixture request: {other:?}"),
                    }
                    if let Some(live) = &live {
                        let failed = matches!(message, Message::Stop)
                            && live.scenario == crate::service::p5_fixture::Scenario::StopUnknown
                            || matches!(
                                message,
                                Message::Position(_)
                                    | Message::Velocity(_)
                                    | Message::Force(_)
                                    | Message::Impedance { .. }
                            ) && live.scenario
                                == crate::service::p5_fixture::Scenario::TargetUnknown;
                        let action = match message {
                            Message::Position(_) => "position",
                            Message::Velocity(_) => "velocity",
                            Message::Force(_) => "force",
                            Message::Impedance { .. } => "impedance",
                            Message::Stop => "stop",
                            Message::Heartbeat => "heartbeat",
                            _ => "read",
                        };
                        live.trace.event(
                            if failed {
                                "submission_unknown"
                            } else {
                                "local_submitted"
                            },
                            node,
                            Some(request.id),
                            json!({"action":action,"device_execution_confirmed":false}),
                        );
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
    let mut session = ToolSession::new().with_timeout(if is_live {
        Duration::from_secs(5)
    } else {
        Duration::from_millis(100)
    });
    session.request = Some(ConnectionRequest::Can {
        channel: if is_live {
            format!("UI_SYNTHETIC-{node}")
        } else {
            format!("fixture-{node}")
        },
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

#[test]
fn ui_live_fixture_keeps_fresh_observations_without_polling_from_ui() {
    let (mut session, _probe) = session_fixture(1, [0xa1; 12], FixtureBehavior::UiLive);
    session
        .execute(Command::Status)
        .expect("initial observation");
    thread::sleep(Duration::from_millis(350));
    let snapshot = session.snapshot();
    let age = snapshot.telemetry.expect("passive telemetry")["received_age_ms"]
        .as_u64()
        .expect("integer age");
    assert!(age < 150, "UI-independent observation stale: {age}ms");
    assert_eq!(
        snapshot.identity.expect("identity")["text"]["version"],
        "UI_SYNTHETIC"
    );
}

#[test]
fn ui_live_fixture_trial_deadline_advances_without_ui_reads() {
    let (mut session, probe) = session_fixture(2, [0xa2; 12], FixtureBehavior::UiLive);
    let mut request = trial_request(Command::Position { mm: 1.0 });
    request.duration_s = Some(0.3);
    session
        .start_trial(request)
        .expect("real preflight and local target");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        session.tick();
        let trial = session.trial_snapshot();
        if trial["stop_reason"] == "duration_elapsed" && trial["stop_observed"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "trial {trial}");
        thread::sleep(Duration::from_millis(5));
    }
    let observed = probe.observed.lock().expect("counts");
    assert_eq!(observed.targets, 1);
    assert_eq!(observed.stops, 1);
}

#[test]
fn ui_slow_read_cancel_then_new_query_keeps_late_reply_ownership() {
    use crate::service::p5_fixture::{Scenario, Trace};
    use eha_sdk::{host::Wait, session::QueryKind};
    use std::task::Poll;
    let (trace, events) = Trace::new(Duration::from_secs(10));
    let (mut session, _probe) = ui_session_fixture(
        1,
        [0xa1; 12],
        UiOptions {
            scenario: Scenario::SlowRead,
            trace,
        },
    );
    let client = session.client.as_mut().expect("unique actual Client");
    let wait = Wait::new(Duration::from_secs(5));
    client
        .begin_query(QueryKind::Diagnostics, &wait)
        .expect("first query");
    let held = |events: &std::sync::mpsc::Receiver<Value>| {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("read held stage");
            if event["kind"] == "read_reply_held" {
                break event;
            }
        }
    };
    let first = held(&events);
    client
        .cancel_query()
        .expect("cancel waiting, not old bytes");
    client
        .begin_query(QueryKind::Diagnostics, &wait)
        .expect("second query");
    let second = held(&events);
    assert_ne!(first["transport_local_id"], second["transport_local_id"]);
    let deadline = Instant::now() + Duration::from_secs(4);
    let reply = loop {
        match client.poll_query() {
            Poll::Ready(result) => {
                break result.expect("second reply, backend survives old cancellation");
            }
            Poll::Pending => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(2));
            }
        }
    };
    let Response::Diagnostics(data) = reply.response().expect("official diagnostics") else {
        panic!("wrong kind");
    };
    assert_eq!(
        data.sample().query_id().to_string(),
        second["facts"]["query_id"]
            .as_str()
            .expect("second actual wire query ID")
    );
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

fn reconnect_connector(
    observed: Arc<Mutex<Observed>>,
    sink_slot: Arc<Mutex<Option<eha_sdk::host::backend::EventSink>>>,
    run_nonce: [u8; 16],
) -> Connector {
    Connector::Test(Box::new(move || {
        let observed = Arc::clone(&observed);
        let sink_slot = Arc::clone(&sink_slot);
        Backend::spawn("eha-tool-reconnect-test", move |commands, sink| {
            *sink_slot.lock().expect("reconnect sink lock") = Some(sink.clone());
            let mut pump = Pump::new(commands, sink);
            while pump.poll() {
                for lane in 0..3 {
                    while let Some(request) = pump.take(lane) {
                        match protocol::decode(&request.bytes, Direction::HostToFirmware)
                            .expect("tool emits contract-valid request")
                        {
                            Message::Query {
                                query_id,
                                category: 0,
                            } => {
                                pump.sink.submitted(request.id);
                                pump.sink.received(identity_bytes_with_nonce_for(
                                    query_id, [7; 12], 1, run_nonce,
                                ));
                            }
                            Message::Stop => {
                                observed.lock().expect("observed lock").stops += 1;
                                pump.sink.submitted(request.id);
                            }
                            other => panic!("recovery must not replay a target: {other:?}"),
                        }
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        })
        .map_err(|error| error.to_string())
    }))
}

pub(super) fn settle_control(session: &mut ToolSession) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let Some(pending) = session.pending_control.as_ref() else {
            return;
        };
        if pending.autonomous_stop {
            let _ = session.poll_control();
        } else if pending.trial_position {
            session.flush_trial_position(Instant::now());
        } else {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "local control submission did not complete"
        );
        thread::yield_now();
    }
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
        settle_control(session);
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
    settle_control(&mut session);
    session.tick();
    settle_control(&mut session);

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
fn managed_reconnect_binds_first_verified_uid_and_rejects_a_mismatch_without_live_client() {
    let (mut session, harness) = harness();
    session.request = Some(ConnectionRequest::ManagedCan {
        serial: "stable-adapter".into(),
        node: 1,
        mode: "fd".into(),
        python: None,
        expected_uid: None,
    });
    let client = session.client.take().expect("fixture client");
    session.disconnected = Some(client.disconnect());
    let observed = Arc::new(Mutex::new(Observed::default()));
    let sink = Arc::new(Mutex::new(None));
    session.connector = Some(reconnect_connector(
        Arc::clone(&observed),
        Arc::clone(&sink),
        [9; 16],
    ));
    session.reconnect().expect("same UID reconnects");
    assert_eq!(
        session
            .request
            .as_ref()
            .and_then(ConnectionRequest::expected_uid),
        Some("070707070707070707070707")
    );

    let client = session.client.take().expect("reconnected client");
    session.disconnected = Some(client.disconnect());
    if let Some(ConnectionRequest::ManagedCan { expected_uid, .. }) = session.request.as_mut() {
        *expected_uid = Some("080808080808080808080808".into());
    }
    let error = session
        .reconnect()
        .expect_err("profile UID must gate reconnect");
    assert_eq!(error.kind.as_ref(), "invalid_input");
    assert!(session.client.is_none());
    assert!(session.disconnected.is_some(), "retry state is retained");
    assert!(
        session.identity.is_none(),
        "stale identity is not projected"
    );
    assert!(session.execute(Command::Velocity { mm_s: 1.0 }).is_err());
    assert_eq!(harness.observed.lock().expect("observed").stops, 0);
}

#[test]
fn interrupted_trial_reconnects_then_explicit_stop_needs_a_fresh_idle() {
    let (mut session, _) = harness();
    session.trial.active = Some(active(
        trial_request(Command::Velocity { mm_s: 0.5 }),
        Instant::now(),
        0,
    ));
    session.interrupt_trial_unknown("transport_disconnected:synthetic");
    assert_eq!(session.trial_snapshot()["state"], "interrupted_unknown");
    assert_eq!(session.trial_snapshot()["unknown"], true);
    assert!(
        session.execute(Command::Velocity { mm_s: 0.5 }).is_err(),
        "an unknown interruption must block a replacement target"
    );

    let client = session.client.take().expect("fixture client exists");
    session.disconnected = Some(client.disconnect());
    let observed = Arc::new(Mutex::new(Observed::default()));
    let sink_slot = Arc::new(Mutex::new(None));
    session.connector = Some(reconnect_connector(
        Arc::clone(&observed),
        Arc::clone(&sink_slot),
        [9; 16],
    ));

    session.reconnect().expect("same instance reconnects");
    assert_eq!(session.trial_snapshot()["stop_available"], true);
    session
        .disconnect()
        .expect("normal disconnect only closes local I/O");
    assert_eq!(session.trial_snapshot()["stop_available"], false);
    assert!(
        session.stop_trial().is_err(),
        "a disconnected session cannot Stop"
    );
    assert_eq!(session.trial_snapshot()["state"], "interrupted_unknown");
    assert_eq!(observed.lock().expect("observed lock").stops, 0);
    session.reconnect().expect("same instance reconnects again");
    session
        .stop_trial()
        .expect("explicit Stop resumes confirmation");
    assert_eq!(observed.lock().expect("observed lock").stops, 1);
    assert_eq!(session.trial_snapshot()["state"], "stopping");

    let deadline = Instant::now() + Duration::from_millis(100);
    let sink = loop {
        if let Some(sink) = sink_slot.lock().expect("reconnect sink lock").clone() {
            break sink;
        }
        assert!(Instant::now() < deadline, "reconnect backend did not start");
        thread::sleep(Duration::from_millis(1));
    };
    sink.received(telemetry_bytes(
        [9; 16],
        200,
        TargetMode::None,
        0.0,
        0.0,
        1,
        true,
        false,
    ));
    let deadline = Instant::now() + Duration::from_millis(100);
    while session.trial_snapshot()["state"] != "completed" {
        session.tick();
        settle_control(&mut session);
        assert!(Instant::now() < deadline, "fresh Idle did not confirm Stop");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn interrupted_trial_reconnect_to_new_run_never_stops_new_instance() {
    let (mut session, _) = harness();
    session.trial.active = Some(active(
        trial_request(Command::Velocity { mm_s: 0.5 }),
        Instant::now(),
        0,
    ));
    session.interrupt_trial_unknown("transport_disconnected:synthetic");

    let client = session.client.take().expect("fixture client exists");
    session.disconnected = Some(client.disconnect());
    let observed = Arc::new(Mutex::new(Observed::default()));
    let sink_slot = Arc::new(Mutex::new(None));
    session.connector = Some(reconnect_connector(
        Arc::clone(&observed),
        sink_slot,
        [8; 16],
    ));

    session
        .reconnect()
        .expect("new instance still permits identity recovery");
    assert_eq!(
        session.trial_snapshot()["state"],
        "interrupted_unrecoverable"
    );
    assert_eq!(session.trial_snapshot()["unknown"], true);
    assert_eq!(session.trial_snapshot()["stop_available"], false);
    assert!(
        session.stop_trial().is_err(),
        "a new run must not receive the interrupted run's Stop"
    );
    assert_eq!(
        session.trial_snapshot()["state"],
        "interrupted_unrecoverable"
    );
    assert_eq!(observed.lock().expect("observed lock").stops, 0);
}

#[test]
fn trial_instance_match_uses_uid_and_run_nonce() {
    assert!(same_trial_instance(
        IdentityFacts {
            uid: [7; 12],
            run_nonce: [9; 16],
        },
        IdentityFacts {
            uid: [7; 12],
            run_nonce: [9; 16],
        },
    ));
    assert!(!same_trial_instance(
        IdentityFacts {
            uid: [7; 12],
            run_nonce: [9; 16],
        },
        IdentityFacts {
            uid: [7; 12],
            run_nonce: [8; 16],
        },
    ));
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
    settle_control(&mut session);
    assert_eq!(
        harness.observed.lock().expect("observed lock").positions,
        vec![2.0]
    );

    session
        .update_trial_position(3.0)
        .expect("next target merges");
    session.tick();
    settle_control(&mut session);
    assert_eq!(
        harness.observed.lock().expect("observed lock").positions,
        vec![2.0],
        "50 ms cadence prevents a second wire target"
    );
    thread::sleep(Duration::from_millis(55));
    session.tick();
    settle_control(&mut session);
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
        .begin_initial_submission(Command::Position { mm: 5.0 })
        .expect("initial begins");
    loop {
        match session.poll_control() {
            std::task::Poll::Ready(result) => {
                result.expect("initial position submits locally");
                break;
            }
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
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
    settle_control(&mut session);
    assert_eq!(
        harness.observed.lock().expect("observed lock").positions,
        vec![4.0]
    );
    assert_eq!(harness.observed.lock().expect("observed lock").stops, 1);
    assert!(session.update_trial_position(5.0).is_err());
    thread::sleep(Duration::from_millis(55));
    session.tick();
    settle_control(&mut session);

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
    settle_control(&mut session);
    probe.inject_idle(101);
    session.stop_trial().expect("Stop submits locally");
    session.tick();
    settle_control(&mut session);
    assert_eq!(session.trial_snapshot()["state"], "stopping");

    // A fresh receipt after the local Stop submission is needed in addition to its
    // newer device timestamp and strict idle/driver/fact fields.
    probe.inject_idle(102);
    session.tick();
    settle_control(&mut session);
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
    settle_control(&mut session);

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
    settle_control(&mut session);
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
    settle_control(&mut session);

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
    settle_control(&mut session);
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

#[test]
fn stop_confirmation_overdue_keeps_owner_and_preserves_late_confirmation() {
    let (mut session, probe) = session_fixture(1, [3; 12], FixtureBehavior::Ready);
    let root = std::env::temp_dir().join(format!("eha-trial-overdue-{}", std::process::id()));
    let recording = session.start_recording(&root).expect("开始记录");
    session
        .start_trial(trial_request(Command::Position { mm: 1.0 }))
        .expect("开始试验");
    session.stop_trial().expect("Stop 本地提交");
    session
        .trial
        .active
        .as_mut()
        .expect("停止中")
        .stop_attempt_finished_at = Some(Instant::now() - Duration::from_secs(1));

    session.tick();
    settle_control(&mut session);
    assert_eq!(session.trial_snapshot()["state"], "stopping");
    assert_eq!(session.trial_snapshot()["stop_confirmation_overdue"], true);
    assert!(session.execute(Command::Status).is_err());
    assert!(session.update_trial_position(2.0).is_err());
    assert!(
        session
            .prepare_trial(&trial_request(Command::Position { mm: 2.0 }))
            .is_err()
    );
    session.stop_trial().expect("重复停止不重发");
    session.tick();
    settle_control(&mut session);
    assert_eq!(probe.observed.lock().expect("提交观测").stops, 1);

    probe.inject_idle(100);
    session.tick();
    settle_control(&mut session);
    let completed = session.trial_snapshot();
    assert_eq!(completed["state"], "completed");
    assert_eq!(completed["stop_observed"], true);
    assert_eq!(completed["stop_confirmation_overdue"], true);
    session.stop_recording().expect("结束记录");
    let directory = Path::new(recording["directory"].as_str().expect("记录目录"));
    let events = std::fs::read_to_string(directory.join("events.jsonl")).expect("读取事件");
    assert_eq!(
        events
            .matches("\"event\":\"trial_stop_confirmation_overdue\"")
            .count(),
        1
    );
    assert!(events.contains("\"event\":\"trial_stop_confirmed\""));
    std::fs::remove_dir_all(root).expect("清理测试记录");
}

#[test]
fn failed_recording_blocks_initial_target_and_cannot_finish_as_complete() {
    let (mut session, probe) = session_fixture(1, [3; 12], FixtureBehavior::Ready);
    let root =
        std::env::temp_dir().join(format!("eha-trial-recording-before-{}", std::process::id()));
    let recording = session.start_recording(&root).expect("开始记录");
    session
        .recording
        .as_mut()
        .expect("记录器")
        .fail_event_writes()
        .expect("只读文件故障");

    let error = session
        .start_trial(trial_request(Command::Position { mm: 1.0 }))
        .expect_err("记录无法落盘时不发送目标");
    assert_eq!(error.kind.as_ref(), "recording");
    assert_eq!(error.local_stage.as_deref(), Some("not_submitted"));
    assert!(!error.unknown);
    assert_eq!(session.trial_snapshot()["state"], "idle");
    assert_eq!(probe.observed.lock().expect("提交观测").targets, 0);
    assert_eq!(probe.observed.lock().expect("提交观测").stops, 0);
    assert!(session.stop_recording().is_err());
    let directory = Path::new(recording["directory"].as_str().expect("记录目录"));
    let metadata: Value = serde_json::from_slice(
        &std::fs::read(directory.join("metadata.json")).expect("读取元数据"),
    )
    .expect("元数据 JSON");
    assert!(metadata.get("completion").is_none());
    std::fs::remove_dir_all(root).expect("清理测试记录");
}

#[test]
fn recording_failure_stops_once_without_flushing_queued_position_or_completing_recording() {
    let (mut session, probe) = session_fixture(1, [3; 12], FixtureBehavior::Ready);
    let root =
        std::env::temp_dir().join(format!("eha-trial-recording-during-{}", std::process::id()));
    let recording = session.start_recording(&root).expect("开始记录");
    session
        .start_trial(trial_request(Command::Position { mm: 1.0 }))
        .expect("开始试验");
    session
        .recording
        .as_mut()
        .expect("记录器")
        .fail_event_writes()
        .expect("只读文件故障");
    session.update_trial_position(2.0).expect("合并目标");
    session
        .trial
        .active
        .as_mut()
        .expect("试验中")
        .last_position_submit = None;

    session.tick();
    settle_control(&mut session);
    session.tick();
    settle_control(&mut session);
    assert_eq!(session.trial_snapshot()["stop_reason"], "recording_failed");
    assert_eq!(session.trial_snapshot()["state"], "stopping");
    assert_eq!(session.recording_snapshot()["failed"], true);
    {
        let observed = probe.observed.lock().expect("提交观测");
        assert_eq!(observed.positions, vec![1.0]);
        assert_eq!(observed.stops, 1);
    }
    probe.inject_idle(100);
    session.tick();
    settle_control(&mut session);
    assert_eq!(session.trial_snapshot()["state"], "completed");
    assert_eq!(session.trial_snapshot()["stop_reason"], "recording_failed");
    session.finish_recording_boundary("disconnect");
    let directory = Path::new(recording["directory"].as_str().expect("记录目录"));
    let metadata: Value = serde_json::from_slice(
        &std::fs::read(directory.join("metadata.json")).expect("读取元数据"),
    )
    .expect("元数据 JSON");
    assert!(metadata.get("completion").is_none());
    std::fs::remove_dir_all(root).expect("清理测试记录");
}

#[test]
fn recording_finish_failure_cannot_bypass_trial_stop() {
    let (mut session, probe) = session_fixture(1, [3; 12], FixtureBehavior::Ready);
    let root =
        std::env::temp_dir().join(format!("eha-trial-recording-finish-{}", std::process::id()));
    session.start_recording(&root).expect("开始记录");
    session
        .start_trial(trial_request(Command::Position { mm: 1.0 }))
        .expect("开始试验");
    session
        .recording
        .as_mut()
        .expect("记录器")
        .fail_event_writes()
        .expect("只读文件故障");

    assert!(session.stop_recording().is_err());
    assert_eq!(session.trial_snapshot()["stop_reason"], "recording_failed");
    assert_eq!(session.trial_snapshot()["state"], "stopping");
    assert!(session.recording_error.is_some());
    session.tick();
    settle_control(&mut session);
    assert_eq!(probe.observed.lock().expect("提交观测").stops, 1);
    std::fs::remove_dir_all(root).expect("清理测试记录");
}

#[test]
fn stop_confirmation_deadline_uses_receipt_time_instead_of_tick_time() {
    for (receipt_delay, overdue) in [
        (Duration::from_millis(25), false),
        (Duration::from_millis(75), true),
    ] {
        let (mut session, probe) = session_fixture(1, [3; 12], FixtureBehavior::Ready);
        session
            .start_trial(trial_request(Command::Position { mm: 1.0 }))
            .expect("开始试验");
        session.stop_trial().expect("Stop 本地提交");
        let finished = Instant::now() - Duration::from_millis(100);
        let active = session.trial.active.as_mut().expect("停止中");
        active.stop_attempt_finished_at = Some(finished);
        active.stop_confirmation_timeout = Duration::from_millis(50);
        probe.inject_idle_at(100, finished + receipt_delay);

        session.tick();
        settle_control(&mut session);
        assert_eq!(session.trial_snapshot()["state"], "completed");
        assert_eq!(
            session.trial_snapshot()["stop_confirmation_overdue"],
            overdue
        );
        assert_eq!(probe.observed.lock().expect("提交观测").stops, 1);
    }
}

pub(crate) fn begin_autonomous_stop_fixture(session: &mut ToolSession) {
    session.trial.active = Some(active(
        trial_request(Command::Position { mm: 1.0 }),
        Instant::now(),
        10,
    ));
    session
        .submit_trial_stop("duration_elapsed")
        .expect("autonomous Stop queued");
}

fn initial_waiting_for_receipt(session: &mut ToolSession, probe: &FixtureProbe) {
    let request = trial_request(Command::Position { mm: 1.0 });
    session.prepare_trial(&request).expect("preflight");
    session.begin_trial_start(request).expect("begin start");
    let deadline = Instant::now() + Duration::from_secs(1);
    while session.pending_control.is_none() {
        assert!(session.poll_trial_start().is_pending());
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    while probe.observed.lock().expect("observed").targets == 0 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}

fn disconnect_fixture(probe: &FixtureProbe) {
    let sink = probe
        .sink
        .lock()
        .expect("sink")
        .as_ref()
        .expect("live sink")
        .clone();
    sink.event(eha_sdk::host::backend::Event::Disconnected(
        "test disconnect".into(),
    ));
}

#[test]
fn recording_stop_cancels_inflight_initial_with_unknown_receipt_and_releases_stop_slot() {
    let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
    let root =
        std::env::temp_dir().join(format!("eha-recording-start-cancel-{}", std::process::id()));
    session.start_recording(&root).expect("recording");
    initial_waiting_for_receipt(&mut session, &probe);
    session.begin_recording_stop().expect("finish admitted");
    assert!(
        matches!(session.poll_trial_start(),std::task::Poll::Ready(Err(error)) if error.unknown && error.local_stage.as_deref()==Some("may_have_been_submitted"))
    );
    settle_control(&mut session);
    assert_eq!(probe.observed.lock().expect("observed").stops, 1);
    assert!(session.pending_control.is_none());
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match session.poll_recording_stop() {
            std::task::Poll::Ready(result) => {
                result.expect("finished");
                break;
            }
            std::task::Poll::Pending => {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
        }
    }
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn disconnect_preserves_initial_unknown_receipt_and_clears_its_pending_control() {
    let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
    initial_waiting_for_receipt(&mut session, &probe);
    disconnect_fixture(&probe);
    session.tick();
    assert!(
        matches!(session.poll_trial_start(),std::task::Poll::Ready(Err(error)) if error.unknown && error.local_stage.as_deref()==Some("may_have_been_submitted"))
    );
    assert!(session.pending_control.is_none());
    assert!(session.trial.interrupted.is_some());
}

#[test]
fn disconnect_keeps_stop_terminal_on_the_interrupted_trial() {
    let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
    session
        .start_trial(trial_request(Command::Position { mm: 1.0 }))
        .expect("start");
    session.begin_control(Command::Stop).expect("Stop begins");
    let deadline = Instant::now() + Duration::from_secs(1);
    while probe.observed.lock().expect("observed").stops == 0 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    disconnect_fixture(&probe);
    session.tick();
    assert!(matches!(session.poll_control(),std::task::Poll::Ready(Err(error)) if error.unknown));
    let interrupted = session.trial.interrupted.as_ref().expect("interrupted");
    assert!(interrupted.active.stop_attempt_finished_at.is_some());
    assert_eq!(
        interrupted
            .active
            .stop_submission
            .as_ref()
            .expect("receipt")["error"]["unknown"],
        true
    );
}

#[test]
fn first_position_completion_starts_the_twenty_hz_cadence() {
    let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
    initial_waiting_for_receipt(&mut session, &probe);
    session
        .trial
        .active
        .as_mut()
        .expect("active")
        .last_position_submit = Some(Instant::now() - Duration::from_secs(1));
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match session.poll_trial_start() {
            std::task::Poll::Ready(result) => {
                result.expect("initial submitted");
                break;
            }
            std::task::Poll::Pending => {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
        }
    }
    session
        .begin_trial_target(2.0)
        .expect("next target admitted");
    assert!(session.poll_trial_target().is_pending());
    assert!(
        session.pending_control.is_none(),
        "initial actual completion starts the 50ms wait"
    );
    assert_eq!(
        probe.observed.lock().expect("observed").positions,
        vec![1.0]
    );
}

#[test]
fn pending_observation_preserves_connection_scope_without_hiding_known_disconnect() {
    let (mut session, _) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
    assert!(session.connection_observation_connected(std::task::Poll::Pending));
    session.disconnected_reason = Some("known disconnect".into());
    assert!(!session.connection_observation_connected(std::task::Poll::Pending));
}

#[test]
fn invalid_recording_stop_does_not_cancel_an_initial_submission() {
    let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
    initial_waiting_for_receipt(&mut session, &probe);
    assert!(session.begin_recording_stop().is_err());
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match session.poll_trial_start() {
            std::task::Poll::Ready(result) => {
                result.expect("initial submitted");
                break;
            }
            std::task::Poll::Pending => {
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
        }
    }
    assert_eq!(probe.observed.lock().expect("observed").stops, 0);
}

#[test]
fn audited_trial_rechecks_recording_failure_before_initial_submission() {
    for (projected, released) in [(false, false), (true, false), (false, true), (true, true)] {
        let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let root = std::env::temp_dir().join(format!(
            "eha-audit-recheck-{}-{projected}-{released}",
            std::process::id()
        ));
        session.start_recording(&root).expect("recording");
        let request = trial_request(Command::Position { mm: 1.0 });
        session.prepare_trial(&request).expect("preflight");
        session.begin_trial_audit(request.clone()).expect("audit");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match session.poll_trial_audit() {
                std::task::Poll::Ready(result) => {
                    result.expect("audited");
                    break;
                }
                std::task::Poll::Pending => {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
            }
        }
        if released {
            session
                .begin_trial_start(request.clone())
                .expect("release before failure");
        }
        let error = session
            .recording
            .as_mut()
            .expect("recording")
            .event("oversized", json!({"payload":"x".repeat(65_537)}))
            .expect_err("writer marks evidence partial");
        if projected {
            session.recording_error = Some(error);
        }
        if !released {
            session
                .begin_trial_start(request)
                .expect("release before submission check");
        }
        let rejected = matches!(session.poll_trial_start(),std::task::Poll::Ready(Err(error)) if error.local_stage.as_deref()==Some("not_submitted") && !error.unknown);
        let targets = probe.observed.lock().expect("observed").targets;
        drop(session);
        std::fs::remove_dir_all(root).expect("cleanup");
        assert!(
            rejected,
            "failed recording must invalidate earlier audit (projected={projected}, released={released})"
        );
        assert_eq!(targets, 0);
    }
}
