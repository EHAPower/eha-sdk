// Copyright The eha-sdk Contributors

//! 唯一 ToolSession 的有界读取槽；服务 owner 仍负责先 tick/Stop 再 poll。

use std::task::Poll;

use eha_sdk::{host::LocalStage, session::QueryKind};

use super::*;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ReadOwner {
    Standalone,
    TrialPreflight,
    RecordingStart,
}

pub(super) struct ReadTask {
    owner: ReadOwner,
    command: Command,
    generation: u64,
    result: Option<Result<CommandResult, SessionError>>,
}

impl ToolSession {
    /// 开始一个读取；保留既有 v2 读取门禁，不开启控制期间主动读取。
    /// 槽位包括尚未领取的终态；重复 begin 明确拒绝，绝不覆盖原结果。
    #[allow(dead_code)] // P4 owner 接入前保持 v2 路由门禁；测试先验证唯一 Client 边界。
    pub(crate) fn begin_read(&mut self, command: Command) -> Result<(), SessionError> {
        self.begin_read_owned(command, ReadOwner::Standalone)
    }

    pub(super) fn begin_read_owned(
        &mut self,
        command: Command,
        owner: ReadOwner,
    ) -> Result<(), SessionError> {
        self.retire_disconnected_usb();
        self.require_read_slot_idle()?;
        let kind = match &command {
            Command::Status => QueryKind::Status,
            Command::Measurements => QueryKind::Measurements,
            Command::Diagnostics => QueryKind::Diagnostics,
            Command::ConfigRead { view } => QueryKind::Config(parse_view(view)? as u8),
            _ => return Err(invalid("begin_read 只接受状态、测量、诊断或配置读取")),
        };
        if owner == ReadOwner::Standalone {
            if let Some(reason) = self.integration_read_blocker() {
                return Err(invalid(format!("主动读取门禁未满足：{reason}")));
            }
        } else if owner == ReadOwner::RecordingStart
            && kind != QueryKind::Config(ConfigView::Startup as u8)
        {
            return Err(invalid("记录准备只接受 Startup"));
        } else if kind != QueryKind::Status && kind != QueryKind::Config(ConfigView::Startup as u8)
        {
            return Err(invalid("试验预检只接受 Startup 或 Status"));
        }
        self.record_event("command_input", json!({"command": &command}));
        let wait = self.wait();
        let result = self
            .client
            .as_mut()
            .ok_or_else(|| invalid("没有已核对的 Client"))?
            .begin_query(kind, &wait)
            .map_err(sdk_error);
        if let Err(error) = result {
            self.record_event("command_error", json!({"error": &error}));
            return Err(error);
        }
        self.pending_read = Some(ReadTask {
            owner,
            command,
            generation: self.connection_generation,
            result: None,
        });
        Ok(())
    }

    /// 每次只调用一次 SDK poll；Pending 不等待设备。调用方须继续 tick 和处理 Stop。
    /// 领取后释放槽；取消或代次变化的终态也只能被原读取调用者领取一次。
    #[allow(dead_code)] // 与 begin_read 一起在后续 P4 owner 片接入。
    pub(crate) fn poll_read(&mut self) -> Poll<Result<CommandResult, SessionError>> {
        self.poll_read_owned(ReadOwner::Standalone)
    }

    pub(super) fn poll_read_owned(
        &mut self,
        owner: ReadOwner,
    ) -> Poll<Result<CommandResult, SessionError>> {
        if self
            .pending_read
            .as_ref()
            .is_some_and(|task| task.owner != owner)
        {
            return Poll::Ready(Err(invalid("读取结果属于另一项操作")));
        }
        if self
            .pending_read
            .as_ref()
            .is_some_and(|task| task.generation != self.connection_generation)
        {
            self.cancel_read("connection_generation_changed");
        }
        let Some(mut task) = self.pending_read.take() else {
            return Poll::Ready(Err(invalid("没有待领取的读取")));
        };
        if let Some(result) = task.result.take() {
            return Poll::Ready(result);
        }
        let result = match self.client.as_mut() {
            Some(client) => match client.poll_query() {
                Poll::Pending => {
                    self.pending_read = Some(task);
                    return Poll::Pending;
                }
                Poll::Ready(result) => result.map_err(sdk_error),
            },
            None => Err(sdk_error(SdkError {
                failure: Failure::Disconnected("读取所属连接已关闭".into()),
                local: LocalStage::MayHaveBeenSubmitted,
                observed_transport: None,
            })),
        };
        let result = result.and_then(|reply| {
            let data = reply_json(&reply)?;
            match task.command {
                Command::Status => self.last_status = Some(reply),
                Command::Measurements => self.last_measurements = Some(reply),
                Command::Diagnostics => self.last_diagnostics = Some(reply),
                _ => {}
            }
            Ok(CommandResult {
                action: command_name(&task.command).into(),
                local_submission: None,
                data,
                snapshot: self.snapshot(),
            })
        });
        match &result {
            Ok(value) => self.record_event("command_completed", json!({"action": value.action, "local_submission": value.local_submission, "data": value.data})),
            Err(error) => self.record_event("command_error", json!({"action": command_name(&task.command), "error": error})),
        }
        Poll::Ready(result)
    }

    /// 仅结束读取等待并保留终态。返回 true 表示本次终结了一项读取；不发送 Stop。
    pub(crate) fn cancel_read(&mut self, reason: &str) -> bool {
        self.end_read_with_message(
            Failure::Cancelled,
            Some(format!("读取等待已取消：{reason}")),
        )
    }

    pub(super) fn end_read(&mut self, failure: Failure) -> bool {
        self.end_read_with_message(failure, None)
    }

    fn end_read_with_message(&mut self, failure: Failure, message: Option<String>) -> bool {
        let Some(mut task) = self.pending_read.take() else {
            return false;
        };
        if task.result.is_some() {
            self.pending_read = Some(task);
            return false;
        }
        let error = match self.client.as_mut() {
            Some(client) => match client.cancel_query() {
                Ok(local) => {
                    let mut error = sdk_error(SdkError {
                        failure,
                        local,
                        observed_transport: None,
                    });
                    if let Some(message) = message {
                        error.message = message.into_boxed_str();
                    }
                    error
                }
                Err(error) => sdk_error(error),
            },
            None => sdk_error(SdkError {
                failure: Failure::Disconnected("读取所属连接已关闭".into()),
                local: LocalStage::MayHaveBeenSubmitted,
                observed_transport: None,
            }),
        };
        self.record_event(
            "command_error",
            json!({"action": command_name(&task.command), "error": &error}),
        );
        task.result = Some(Err(error));
        self.pending_read = Some(task);
        true
    }

    pub(super) fn require_read_slot_idle(&self) -> Result<(), SessionError> {
        if self.pending_read.is_some()
            || self.pending_preflight.is_some()
            || self.pending_recording_start.is_some()
            || self.pending_trial_start.is_some()
        {
            let mut error = invalid("读取槽已占用；请先领取原读取的完成或取消结果");
            error.kind = "busy".into();
            error.local_stage = Some("not_submitted".into());
            Err(error)
        } else {
            Ok(())
        }
    }

    pub(super) fn prepare_read_boundary(&mut self, command: &Command) -> Result<(), SessionError> {
        match command {
            Command::Stop | Command::HeartbeatStart | Command::HeartbeatStop => {
                self.cancel_pending_operations(command_name(command));
                Ok(())
            }
            Command::Telemetry | Command::ConfigValidate { .. } => Ok(()),
            _ => self.require_read_slot_idle(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;
    use eha_sdk::host::backend::{Backend, EventSink, Pump, SendRequest};
    use eha_sdk::protocol::{self, Direction, Message, SampleData};
    use std::sync::{Arc, atomic::AtomicUsize, mpsc};

    struct Fixture {
        session: ToolSession,
        sink: EventSink,
        requests: mpsc::Receiver<SendRequest>,
        stops: Arc<AtomicUsize>,
        status: Vec<u8>,
    }

    fn fixture() -> Fixture {
        let (mut session, _) = super::super::trial_tests::session_fixture(
            1,
            [7; 12],
            super::super::trial_tests::FixtureBehavior::Ready,
        );
        session
            .execute(Command::Status)
            .expect("initial idle facts");
        let identity = session
            .identity
            .as_ref()
            .expect("identity")
            .bytes()
            .to_vec();
        let status = session
            .last_status
            .as_ref()
            .expect("status")
            .bytes()
            .to_vec();
        let telemetry = session
            .last_telemetry
            .as_ref()
            .expect("telemetry")
            .bytes()
            .to_vec();
        let (ready, sink_rx) = mpsc::sync_channel(1);
        let (sent, requests) = mpsc::sync_channel(8);
        let stops = Arc::new(AtomicUsize::new(0));
        let counted = stops.clone();
        let backend = Backend::spawn("session-scheduler-test", move |commands, sink| {
            ready.send(sink.clone()).expect("sink ready");
            let mut pump = Pump::new(commands, sink);
            let mut heartbeat_notified = false;
            while pump.poll() {
                for lane in 0..3 {
                    while let Some(request) = pump.take(lane) {
                        match protocol::decode(&request.bytes, Direction::HostToFirmware)
                            .expect("request decodes")
                        {
                            Message::Query { category: 0, .. } => {
                                pump.sink.submitted(request.id);
                                pump.sink.received(identity.clone());
                                pump.sink.received(telemetry.clone());
                            }
                            Message::Stop => {
                                counted.fetch_add(1, Ordering::SeqCst);
                                pump.sink.submitted(request.id);
                            }
                            Message::Heartbeat => {
                                pump.sink.submitted(request.id);
                                if request.id == 0 && !heartbeat_notified {
                                    sent.send(request).expect("heartbeat observed");
                                    heartbeat_notified = true;
                                }
                            }
                            Message::Query { .. } | Message::ReadConfig { .. } => {
                                sent.send(request).expect("pending request");
                            }
                            other => panic!("unexpected request: {other:?}"),
                        }
                    }
                }
                std::thread::yield_now();
            }
        })
        .expect("backend starts");
        let sink = sink_rx.recv().expect("sink ready");
        let mut client = Client::new(backend);
        session.identity = Some(
            client
                .identify(None, &Wait::default())
                .expect("same identity"),
        );
        session.client = Some(client);
        session.timeout = Duration::from_secs(5);
        session.last_status = None;
        session.snapshot();
        assert!(session.integration_read_blocker().is_none());
        Fixture {
            session,
            sink,
            requests,
            stops,
            status,
        }
    }

    fn request(fixture: &Fixture) -> SendRequest {
        fixture
            .requests
            .recv_timeout(Duration::from_secs(1))
            .expect("worker received query")
    }

    fn trial() -> TrialRequest {
        TrialRequest {
            command: Command::Position { mm: 1.0 },
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
    fn never_reply_read_yields_and_full_slot_preserves_cancelled_result_before_stop() {
        let mut f = fixture();
        f.session.begin_read(Command::Status).expect("begin");
        let _query = request(&f);
        assert!(f.session.poll_read().is_pending());
        assert!(f.session.poll_read().is_pending());
        assert_eq!(
            &*f.session
                .begin_read(Command::Diagnostics)
                .expect_err("full slot")
                .kind,
            "busy"
        );
        assert_eq!(
            &*f.session
                .execute(Command::Position { mm: 1.0 })
                .expect_err("no unrelated control")
                .kind,
            "busy"
        );
        assert!(!f.session.pending_target);
        assert!(f.session.prepare_trial(&trial()).is_err());
        assert!(
            f.session
                .start_recording(Path::new("unused-full-read-slot"))
                .is_err()
        );
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
        f.session
            .execute(Command::Stop)
            .expect("explicit Stop preempts read");
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
        assert!(
            f.session.begin_read(Command::Status).is_err(),
            "unclaimed result stays owned"
        );
        let Poll::Ready(Err(error)) = f.session.poll_read() else {
            panic!("cancel result retained")
        };
        assert_eq!(&*error.kind, "cancelled");
        assert_eq!(
            error.local_stage.as_deref(),
            Some("may_have_been_submitted")
        );
        assert!(!error.unknown);
        assert!(matches!(f.session.poll_read(), Poll::Ready(Err(_))));
    }

    #[test]
    fn delayed_reply_updates_cache_only_after_submission_and_keeps_reply_semantics() {
        let mut f = fixture();
        f.session.begin_read(Command::Status).expect("begin");
        let query = request(&f);
        f.sink.received(f.status.clone());
        assert!(f.session.poll_read().is_pending());
        assert!(f.session.last_status.is_none());
        f.sink.submitted(query.id);
        let Poll::Ready(Ok(result)) = f.session.poll_read() else {
            panic!("submitted early reply completes")
        };
        assert_eq!(result.action, "status");
        assert!(result.local_submission.is_none());
        assert!(result.snapshot.last_status.is_some());
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn read_topics_and_config_views_preserve_data_unavailable() {
        use protocol::responses::{
            DataUnavailableFields, UnavailableSubject, encode_data_unavailable,
        };
        let mut f = fixture();
        for (command, subject) in [
            (Command::Measurements, UnavailableSubject::Measurements),
            (Command::Diagnostics, UnavailableSubject::Diagnostics),
            (
                Command::ConfigRead {
                    view: "factory".into(),
                },
                UnavailableSubject::Config,
            ),
            (
                Command::ConfigRead {
                    view: "startup".into(),
                },
                UnavailableSubject::Config,
            ),
            (
                Command::ConfigRead {
                    view: "user".into(),
                },
                UnavailableSubject::Config,
            ),
        ] {
            f.session.begin_read(command).expect("begin topic");
            let query = request(&f);
            let query_id = match protocol::decode(&query.bytes, Direction::HostToFirmware)
                .expect("request")
            {
                Message::Query { query_id, .. } | Message::ReadConfig { query_id, .. } => query_id,
                _ => panic!("query expected"),
            };
            f.sink.submitted(query.id);
            let mut bytes = vec![0; DataUnavailableFields::encoded_len()];
            encode_data_unavailable(
                &DataUnavailableFields {
                    sample: SampleData {
                        query_id,
                        run_nonce: [9; 16],
                        snapshot_sequence: 1,
                        snapshot_time_us: 1,
                    },
                    subject,
                    reason: 0x0101,
                    next_actions: 0,
                },
                &mut bytes,
            )
            .expect("unavailable encodes");
            f.sink.received(bytes);
            let Poll::Ready(Err(error)) = f.session.poll_read() else {
                panic!("unavailable preserved")
            };
            assert_eq!(&*error.kind, "data_unavailable");
            assert_eq!(error.local_stage.as_deref(), Some("fully_submitted"));
            assert!(error.failure.is_some());
        }
    }

    #[test]
    fn heartbeat_switch_preempts_wait_but_does_not_open_v2_active_read_gate() {
        let mut f = fixture();
        f.session.begin_read(Command::Status).expect("begin");
        let _query = request(&f);
        f.session
            .execute(Command::HeartbeatStart)
            .expect("heartbeat priority");
        assert_eq!(request(&f).id, 0, "original worker drives heartbeat");
        assert!(
            matches!(f.session.poll_read(), Poll::Ready(Err(error)) if &*error.kind == "cancelled")
        );
        assert_eq!(
            f.session.integration_read_blocker(),
            Some("heartbeat_active")
        );
        assert!(f.session.begin_read(Command::Status).is_err());
        f.session
            .execute(Command::HeartbeatStop)
            .expect("explicit heartbeat stop");
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn controlled_trial_deadline_cancels_wait_and_submits_one_stop_without_releasing_confirmation()
    {
        let mut f = fixture();
        f.session
            .begin_read(Command::Status)
            .expect("begin before trial");
        let _query = request(&f);
        let started = Instant::now();
        // 注入既有已授权 trial，仅覆盖 scheduler 交错；生产 begin_read 仍拒绝 trial_active。
        f.session.trial.active = Some(ActiveTrial {
            request: trial(),
            started_at: started,
            duration: Duration::from_millis(20),
            pending_position_mm: Some(2.0),
            last_position_submit: None,
            settled_since: None,
            stop_submission: None,
            stop_reason: None,
            stop_observed: false,
            started_sample_time_us: 0,
            stop_after_sample_time_us: None,
            stop_attempt_finished_at: None,
            stop_confirmation_timeout: Duration::from_millis(100),
            stop_confirmation_overdue: false,
            stop_recovery_after_received_at: None,
            settled_sample_time_us: None,
        });
        f.session.tick_at(started + Duration::from_millis(20));
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
        let active = f
            .session
            .trial
            .active
            .as_ref()
            .expect("confirmation retains owner");
        assert_eq!(active.stop_reason.as_deref(), Some("duration_elapsed"));
        assert!(active.pending_position_mm.is_none());
        assert!(
            matches!(f.session.poll_read(), Poll::Ready(Err(error)) if &*error.kind == "cancelled")
        );
        f.session.tick_at(started + Duration::from_secs(1));
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
        assert!(
            f.session
                .trial
                .active
                .as_ref()
                .expect("unknown remains")
                .stop_confirmation_overdue
        );
        assert!(f.session.begin_read(Command::Status).is_err());
    }

    #[test]
    fn trial_deadline_is_sampled_after_buffered_recording_flush_before_pending_position() {
        let mut f = fixture();
        let started = Instant::now();
        f.session.trial.active = Some(ActiveTrial {
            request: trial(),
            started_at: started,
            duration: Duration::from_millis(20),
            pending_position_mm: Some(2.0),
            last_position_submit: None,
            settled_since: None,
            stop_submission: None,
            stop_reason: None,
            stop_observed: false,
            started_sample_time_us: 0,
            stop_after_sample_time_us: None,
            stop_attempt_finished_at: None,
            stop_confirmation_timeout: Duration::from_millis(100),
            stop_confirmation_overdue: false,
            stop_recovery_after_received_at: None,
            settled_sample_time_us: None,
        });
        let root = std::env::temp_dir().join(format!(
            "eha-scheduler-clock-{}-{}",
            std::process::id(),
            f.session.connection_generation
        ));
        let mut recording = Recording::start(&root, json!({})).expect("recording");
        recording
            .event("before_deadline", json!({}))
            .expect("queued marker");
        let flush_probe = recording.flush_probe();
        f.session.recording = Some(recording);
        // The writer now owns real I/O. This probe locates the owner's flush request;
        // sampling before that checkpoint still returns the pre-deadline time.
        f.session.tick_with_clock(|| {
            let requested = flush_probe.load(Ordering::SeqCst) != 0;
            started + Duration::from_millis(if requested { 21 } else { 19 })
        });
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
        let active = f
            .session
            .trial
            .active
            .as_ref()
            .expect("Stop awaits confirmation");
        assert_eq!(active.stop_reason.as_deref(), Some("duration_elapsed"));
        assert!(active.pending_position_mm.is_none());
        f.session
            .stop_recording()
            .expect("writer completed recording");
        std::fs::remove_dir_all(root).expect("remove recording fixture");
    }

    #[test]
    fn recording_startup_never_reply_is_owned_and_stop_cancels_without_creating_recording() {
        let mut f = fixture();
        f.session
            .begin_recording_start(Path::new("unused-pending-recording"))
            .expect("begin recording");
        let _query = request(&f);
        assert!(f.session.poll_recording_start().is_pending());
        assert!(matches!(f.session.poll_read(), Poll::Ready(Err(_))));
        f.session.execute(Command::Stop).expect("Stop");
        assert!(
            matches!(f.session.poll_recording_start(), Poll::Ready(Err(error)) if &*error.kind == "cancelled")
        );
        assert!(f.session.recording.is_none());
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn preflight_never_reply_startup_yields_and_stop_keeps_target_unsubmitted() {
        let mut f = fixture();
        f.session
            .begin_trial_preflight(trial())
            .expect("begin preflight");
        let query = request(&f);
        assert!(matches!(
            protocol::decode(&query.bytes, Direction::HostToFirmware),
            Ok(Message::ReadConfig { view: 2, .. })
        ));
        assert!(f.session.poll_trial_preflight().is_pending());
        assert!(f.session.poll_trial_preflight().is_pending());
        assert!(
            matches!(f.session.poll_read(), Poll::Ready(Err(_))),
            "wrong owner cannot take result"
        );
        assert!(f.session.begin_read(Command::Status).is_err());
        f.session
            .execute(Command::Stop)
            .expect("Stop preempts preflight");
        assert!(
            matches!(f.session.poll_trial_preflight(), Poll::Ready(Err(error)) if &*error.kind == "cancelled")
        );
        assert!(f.session.trial.prepared.is_none());
        assert!(f.session.start_prepared_trial(trial()).is_err());
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
    }

    fn advance_preflight_to_status(session: &mut ToolSession) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !session
            .pending_read
            .as_ref()
            .is_some_and(|read| matches!(read.command, Command::Status))
        {
            assert!(
                session.poll_trial_preflight().is_pending(),
                "Startup must yield before Status completes"
            );
            assert!(Instant::now() < deadline, "worker responds to Startup");
            std::thread::yield_now();
        }
    }

    #[test]
    fn preflight_status_phase_remains_cancellable_and_bound_to_its_generation() {
        for action in [
            Some(Command::Stop),
            Some(Command::HeartbeatStart),
            Some(Command::HeartbeatStop),
            None,
        ] {
            let (mut session, probe) = super::super::trial_tests::session_fixture(
                1,
                [7; 12],
                super::super::trial_tests::FixtureBehavior::Ready,
            );
            session
                .begin_trial_preflight(trial())
                .expect("begin preflight");
            advance_preflight_to_status(&mut session);
            assert!(session.trial.prepared.is_none());
            assert!(matches!(session.poll_read(), Poll::Ready(Err(_))));
            if let Some(command) = action {
                session.execute(command).expect("priority action");
            } else {
                session.connection_generation = next_connection_generation();
            }
            assert!(
                matches!(session.poll_trial_preflight(), Poll::Ready(Err(error)) if &*error.kind == "cancelled")
            );
            assert!(session.trial.prepared.is_none());
            assert_eq!(probe.observed.lock().expect("observed").targets, 0);
        }
    }

    #[test]
    fn preflight_complete_is_admission_only_and_new_attempt_invalidates_old_prepared() {
        let (mut session, probe) = super::super::trial_tests::session_fixture(
            1,
            [7; 12],
            super::super::trial_tests::FixtureBehavior::Ready,
        );
        session
            .begin_trial_preflight(trial())
            .expect("begin preflight");
        advance_preflight_to_status(&mut session);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match session.poll_trial_preflight() {
                Poll::Ready(result) => {
                    result.expect("prepared");
                    break;
                }
                Poll::Pending => {
                    assert!(Instant::now() < deadline, "worker responds to Status");
                    std::thread::yield_now();
                }
            }
        }
        assert!(session.trial.prepared.is_some());
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
        session
            .begin_trial_preflight(trial())
            .expect("new preflight");
        assert!(
            session.trial.prepared.is_none(),
            "old evidence cannot survive a new cancelled attempt"
        );
        session.execute(Command::Stop).expect("Stop");
        assert!(matches!(
            session.poll_trial_preflight(),
            Poll::Ready(Err(_))
        ));
        assert!(session.start_prepared_trial(trial()).is_err());
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
    }

    #[test]
    fn transport_disconnect_stays_a_disconnect_after_owner_tick_changes_generation() {
        let mut f = fixture();
        f.session.begin_read(Command::Status).expect("begin");
        let query = request(&f);
        f.sink.submitted(query.id);
        assert!(f.session.poll_read().is_pending());
        f.sink.disconnected("synthetic connection loss");
        f.session.tick();
        let Poll::Ready(Err(error)) = f.session.poll_read() else {
            panic!("disconnect terminal")
        };
        assert_eq!(&*error.kind, "disconnected");
        assert_eq!(error.local_stage.as_deref(), Some("fully_submitted"));
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn generation_change_and_disconnect_preserve_read_terminal_without_adopting_old_reply() {
        let mut f = fixture();
        f.session.begin_read(Command::Status).expect("begin");
        let query = request(&f);
        f.sink.submitted(query.id);
        f.sink.received(f.status.clone());
        f.session.connection_generation = next_connection_generation();
        assert!(
            matches!(f.session.poll_read(), Poll::Ready(Err(error)) if &*error.kind == "cancelled")
        );
        assert!(f.session.last_status.is_none());
        f.session
            .begin_read(Command::Diagnostics)
            .expect("new generation read");
        let _query = request(&f);
        f.session
            .disconnect()
            .expect("disconnect only closes local owner");
        assert!(
            matches!(f.session.poll_read(), Poll::Ready(Err(error)) if &*error.kind == "cancelled")
        );
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
    }
}
