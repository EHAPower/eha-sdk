// Copyright The eha-sdk Contributors

//! 维护操作使用原独占 Client。取消仅结束其等待。

use super::*;
use eha_sdk::{host::LocalStage, session::QueryKind};
use std::task::Poll;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MaintenancePhase {
    FactoryRead,
    Submission,
}

pub(crate) struct MaintenanceCompletion {
    pub(crate) phase: MaintenancePhase,
    pub(crate) result: Result<CommandResult, SessionError>,
}

pub(super) struct Pending {
    command: Command,
    generation: u64,
    phase: MaintenancePhase,
    deadline: Instant,
    completion: Option<MaintenanceCompletion>,
    cancellation_reason: Option<String>,
}

impl ToolSession {
    #[cfg(test)]
    pub(crate) fn test_mark_target_pending(&mut self) {
        self.pending_target = true;
    }

    #[cfg(test)]
    pub(crate) fn test_maintenance_fixture() -> (
        Self,
        std::sync::mpsc::Receiver<eha_sdk::host::backend::Command>,
        eha_sdk::host::backend::EventSink,
    ) {
        tests::fixture()
    }

    pub(crate) fn begin_maintenance(&mut self, command: Command) -> Result<(), SessionError> {
        self.retire_disconnected_usb();
        self.require_read_slot_idle()?;
        if self.maintenance_pending.is_some() || self.pending_recording_finish.is_some() {
            return Err(invalid("维护或记录终态尚未领取"));
        }
        if !matches!(
            command,
            Command::ConfigSaveAndRestart { .. }
                | Command::RestoreFactoryAndRestart
                | Command::ResetApplication
                | Command::EnterUpdate
        ) {
            return Err(invalid("begin_maintenance 仅接受固定四项维护动作"));
        }
        if let Some(reason) = self.integration_read_blocker() {
            return Err(invalid(format!("维护准入门禁未满足：{reason}")));
        }
        let wait = self.wait();
        let deadline = Instant::now()
            .checked_add(wait.timeout)
            .ok_or_else(|| invalid("维护期限超出单调时钟范围"))?;
        let phase = if matches!(command, Command::RestoreFactoryAndRestart) {
            MaintenancePhase::FactoryRead
        } else {
            MaintenancePhase::Submission
        };
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| invalid("没有已核对的 Client"))?;
        match &command {
            Command::ConfigSaveAndRestart { record } => {
                client.begin_save_and_restart(record.as_bytes(), &wait)
            }
            Command::RestoreFactoryAndRestart => {
                client.begin_query(QueryKind::Config(ConfigView::Factory as u8), &wait)
            }
            Command::ResetApplication => client.begin_reset_application(&wait),
            Command::EnterUpdate => client.begin_enter_update(&wait),
            _ => return Err(invalid("未知维护动作")),
        }
        .map_err(sdk_error)?;
        self.record_event("command_input", json!({"command": &command}));
        self.maintenance_pending = Some(Pending {
            command,
            generation: self.connection_generation,
            phase,
            deadline,
            completion: None,
            cancellation_reason: None,
        });
        Ok(())
    }

    /// 槽位持续保留所有权，直到原操作消费该完成结果。
    pub(crate) fn poll_maintenance(&mut self) -> Poll<MaintenanceCompletion> {
        // 锁忙后继续原取消，不回到期限优先的普通 poll 丢失已排队证据。
        if let Some(reason) = self
            .maintenance_pending
            .as_ref()
            .and_then(|task| task.cancellation_reason.clone())
        {
            self.cancel_maintenance(&reason);
            if self.maintenance_pending() {
                return Poll::Pending;
            }
        }

        if self
            .maintenance_pending
            .as_ref()
            .is_some_and(|task| task.generation != self.connection_generation)
        {
            self.cancel_maintenance("connection_generation_changed");
        }
        let Some(mut task) = self.maintenance_pending.take() else {
            return Poll::Ready(MaintenanceCompletion {
                phase: MaintenancePhase::Submission,
                result: Err(invalid("没有待领取的维护")),
            });
        };
        if let Some(completion) = task.completion.take() {
            return Poll::Ready(completion);
        }
        if task.phase == MaintenancePhase::FactoryRead {
            let result = match self.client.as_mut() {
                Some(client) => match client.poll_query() {
                    Poll::Pending => {
                        self.maintenance_pending = Some(task);
                        return Poll::Pending;
                    }
                    Poll::Ready(result) => result.map_err(sdk_error),
                },
                None => Err(maintenance_disconnected()),
            };
            let reply = match result {
                Ok(reply) => reply,
                Err(error) => return Poll::Ready(self.maintenance_completed(task, Err(error))),
            };
            // 查询匹配、身份和全部记录解析均由 SDK 负责；保留原始字节。
            let record = match reply.response() {
                Ok(Response::ConfigData(data)) => data.data().to_vec(),
                Ok(_) => {
                    return Poll::Ready(
                        self.maintenance_completed(task, Err(invalid("缺少出厂记录"))),
                    );
                }
                Err(error) => {
                    return Poll::Ready(self.maintenance_completed(task, Err(sdk_error(error))));
                }
            };
            let compatible = self.identity.as_ref().and_then(|reply| reply.response().ok()).is_some_and(|response| matches!(response, Response::Identity(identity) if identity.fields().config_format_version == eha_sdk::config::FORMAT_VERSION));
            if !compatible {
                return Poll::Ready(
                    self.maintenance_completed(task, Err(invalid("设备配置格式版本与 SDK 不匹配"))),
                );
            }
            task.phase = MaintenancePhase::Submission;
            if let Some(reason) = self.integration_read_blocker() {
                return Poll::Ready(self.maintenance_completed(
                    task,
                    Err(invalid(format!("Factory读取后维护门禁改变：{reason}"))),
                ));
            }

            let remaining = task.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Poll::Ready(self.maintenance_completed(
                    task,
                    Err(sdk_error(SdkError {
                        failure: Failure::Timeout,
                        local: LocalStage::NotSubmitted,
                        observed_transport: None,
                    })),
                ));
            }
            let result = self
                .client
                .as_mut()
                .ok_or_else(maintenance_disconnected)
                .and_then(|client| {
                    client
                        .begin_save_and_restart(&record, &Wait::new(remaining))
                        .map_err(sdk_error)
                });
            if let Err(error) = result {
                return Poll::Ready(self.maintenance_completed(task, Err(error)));
            }
            self.maintenance_pending = Some(task);
            return Poll::Pending;
        }
        let result = match self.client.as_mut() {
            Some(client) => match client.poll_submission() {
                Poll::Pending => {
                    self.maintenance_pending = Some(task);
                    return Poll::Pending;
                }
                Poll::Ready(result) => result.map_err(sdk_error),
            },
            None => Err(maintenance_disconnected()),
        };
        let result = result.map(|local| {
            let mut local_submission = None;
            let data = submission_data(local, &mut local_submission);
            CommandResult {
                action: command_name(&task.command).into(),
                local_submission,
                data,
                snapshot: self.snapshot(),
            }
        });
        Poll::Ready(self.maintenance_completed(task, result))
    }

    fn maintenance_completed(
        &mut self,
        task: Pending,
        result: Result<CommandResult, SessionError>,
    ) -> MaintenanceCompletion {
        match &result {
            Ok(value) => self.record_event("command_completed", json!({"action": value.action, "local_submission": value.local_submission, "data": value.data})),
            Err(error) => self.record_event("command_error", json!({"action": command_name(&task.command), "error": error})),
        }
        MaintenanceCompletion {
            phase: task.phase,
            result,
        }
    }

    /// 仅活跃的 Client 等待阻断普通工作。取消后的回执仍可领取。
    pub(crate) fn maintenance_pending(&self) -> bool {
        self.maintenance_pending
            .as_ref()
            .is_some_and(|task| task.completion.is_none())
    }

    pub(crate) fn cancel_maintenance(&mut self, reason: &str) -> bool {
        let Some(mut task) = self.maintenance_pending.take() else {
            return false;
        };
        if task.completion.is_some() {
            self.maintenance_pending = Some(task);
            return false;
        }
        // 先领取当前提交已入队的真实终态；断连缓存不能覆盖更早的本地提交证据。
        if !matches!(reason, "explicit_stop" | "operator_stop")
            && task.phase == MaintenancePhase::Submission
            && let Some(client) = self.client.as_mut()
        {
            match client.try_finish_submission() {
                Poll::Pending => {
                    task.cancellation_reason
                        .get_or_insert_with(|| reason.to_owned());
                    self.maintenance_pending = Some(task);
                    return false;
                }
                Poll::Ready(Some(result)) => {
                    let result = result.map_err(sdk_error).map(|local| {
                        let mut local_submission = None;
                        let data = submission_data(local, &mut local_submission);
                        CommandResult {
                            action: command_name(&task.command).into(),
                            local_submission,
                            data,
                            // 取消可能由 USB 退役调用，不能重入 snapshot 的退役路径。
                            snapshot: self.snapshot_cached(),
                        }
                    });
                    match &result {
                        Ok(value) => self.record_event("command_completed", json!({"action":value.action,"local_submission":value.local_submission,"data":value.data})),
                        Err(error) => self.record_event("command_error", json!({"action":command_name(&task.command),"error":error})),
                    }
                    task.completion = Some(MaintenanceCompletion {
                        phase: task.phase,
                        result,
                    });
                    self.maintenance_pending = Some(task);
                    return true;
                }
                Poll::Ready(None) => {}
            }
        }
        let result = match self.client.as_mut() {
            Some(client) => {
                if task.phase == MaintenancePhase::FactoryRead {
                    client.cancel_query()
                } else {
                    client.cancel_submission()
                }
            }
            None => Err(SdkError {
                failure: Failure::Disconnected("维护所属连接已关闭".into()),
                local: LocalStage::MayHaveBeenSubmitted,
                observed_transport: None,
            }),
        };
        let mut error = match result {
            Ok(local) => sdk_error(SdkError {
                failure: if task.phase == MaintenancePhase::Submission {
                    Failure::ResultUnknown(format!("维护本地等待已取消：{reason}"))
                } else {
                    Failure::Cancelled
                },
                local,
                observed_transport: None,
            }),
            Err(error) => sdk_error(error),
        };
        error.message = format!("维护本地等待已取消：{reason}").into_boxed_str();
        self.record_event(
            "command_error",
            json!({"action": command_name(&task.command), "error": &error}),
        );
        task.completion = Some(MaintenanceCompletion {
            phase: task.phase,
            result: Err(error),
        });
        self.maintenance_pending = Some(task);
        true
    }
}

fn invalid(message: impl Into<String>) -> SessionError {
    let mut error = super::invalid(message);
    error.local_stage = Some("not_submitted".into());
    error
}

fn maintenance_disconnected() -> SessionError {
    sdk_error(SdkError {
        failure: Failure::Disconnected("维护所属连接已关闭".into()),
        local: LocalStage::MayHaveBeenSubmitted,
        observed_transport: None,
    })
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;
    use eha_sdk::{
        host::backend::{Backend, Command as BackendCommand, EventSink, SendRequest},
        protocol::{
            self, Direction, Message, SampleData,
            responses::{
                ConfigDataFields, ConfigDataWriter, ConfigRecordState, FallbackReason,
                StartupSource,
            },
        },
    };
    use std::sync::mpsc::Receiver;

    pub(super) fn fixture() -> (ToolSession, Receiver<BackendCommand>, EventSink) {
        let (mut session, _) = super::super::trial_tests::session_fixture(
            1,
            [7; 12],
            super::super::trial_tests::FixtureBehavior::Ready,
        );
        session.execute(Command::Status).expect("test invariant");
        let identity = session
            .identity
            .as_ref()
            .expect("test invariant")
            .bytes()
            .to_vec();
        let telemetry = session
            .last_telemetry
            .as_ref()
            .expect("test invariant")
            .bytes()
            .to_vec();
        let (sent, commands) = std::sync::mpsc::sync_channel(16);
        let (ready, sink_rx) = std::sync::mpsc::sync_channel(1);
        let backend = Backend::spawn("maintenance-test", move |commands, sink| {
            ready.send(sink.clone()).expect("test invariant");
            while let Ok(command) = commands.recv() {
                match command {
                    BackendCommand::Send(ref request)
                        if matches!(
                            protocol::decode(&request.bytes, Direction::HostToFirmware),
                            Ok(Message::Query { category: 0, .. })
                        ) =>
                    {
                        sink.submitted(request.id);
                        sink.received(identity.clone());
                        sink.received(telemetry.clone());
                    }
                    BackendCommand::Close => break,
                    command => {
                        sent.send(command).expect("test invariant");
                    }
                }
            }
        })
        .expect("test invariant");
        let sink = sink_rx.recv().expect("test invariant");
        let mut client = Client::new(backend);
        client
            .identify(None, &Wait::default())
            .expect("test invariant");
        session.client = Some(client);
        session.snapshot();
        assert!(session.integration_read_blocker().is_none());
        (session, commands, sink)
    }
    fn sent(commands: &Receiver<BackendCommand>) -> SendRequest {
        let BackendCommand::Send(request) = commands
            .recv_timeout(Duration::from_secs(1))
            .expect("test invariant")
        else {
            panic!("send expected");
        };
        request
    }
    #[test]
    fn disconnect_after_queued_submission_preserves_original_local_receipt() {
        for usb_retirement in [false, true] {
            let (mut session, commands, sink) = fixture();
            if usb_retirement {
                let key = crate::owner_lock::ResourceKey::usb(&format!(
                    "maintenance-retire-test-{}",
                    std::process::id()
                ))
                .expect("synthetic owner key");
                session.usb_owner = Some(
                    crate::owner_lock::OwnerGuard::acquire(&key).expect("synthetic owner guard"),
                );
            }
            session
                .begin_maintenance(Command::ResetApplication)
                .expect("begin reset");
            let request = sent(&commands);
            sink.submitted(request.id);
            sink.disconnected("synthetic reset transport close");
            session.tick();
            let Poll::Ready(completion) = session.poll_maintenance() else {
                panic!("cancelled completion retained");
            };
            let result = completion
                .result
                .expect("already queued local submission survives disconnect fence");
            assert_eq!(completion.phase, MaintenancePhase::Submission);
            assert_eq!(
                result.local_submission.expect("original local receipt").id,
                request.id
            );
            assert!(!request.cancelled.load(Ordering::Acquire));
            assert!(
                commands.try_recv().is_err(),
                "disconnect cannot resend reset"
            );
            if usb_retirement {
                assert!(
                    session.client.is_none(),
                    "Client handed off only after receipt retained"
                );
            }
        }
    }

    #[test]
    fn disconnect_without_prior_matching_submission_keeps_unknown() {
        for wrong_id in [false, true] {
            let (mut session, commands, sink) = fixture();
            session
                .begin_maintenance(Command::ResetApplication)
                .expect("begin reset");
            let request = sent(&commands);
            if wrong_id {
                sink.submitted(request.id + 100);
            }
            sink.disconnected("synthetic close without original receipt");
            sink.submitted(request.id);
            session.tick();
            let Poll::Ready(completion) = session.poll_maintenance() else {
                panic!("terminal error retained");
            };
            let error = completion
                .result
                .expect_err("late receipt cannot cross earlier disconnect terminal");
            assert!(error.unknown);
            assert_eq!(
                error.local_stage.as_deref(),
                Some("may_have_been_submitted")
            );
            assert!(!request.cancelled.load(Ordering::Acquire));
            assert!(commands.try_recv().is_err());
        }
    }

    #[test]
    fn deferred_cancel_preserves_evidence_through_revocation_and_expired_poll() {
        for revoked in [false, true] {
            let (mut session, commands, sink) = fixture();
            session.timeout = Duration::from_millis(1);
            session
                .begin_maintenance(Command::ResetApplication)
                .expect("reset");
            let request = sent(&commands);
            // 对应 try_finish_submission 锁忙返回后的唯一原任务状态。
            session
                .maintenance_pending
                .as_mut()
                .expect("pending")
                .cancellation_reason = Some("transport_disconnected".into());
            std::thread::sleep(Duration::from_millis(2));
            sink.submitted(request.id);
            sink.disconnected("same original disconnect");
            if revoked {
                session.cancel_maintenance("permission_revoked");
            }
            let Poll::Ready(completion) = session.poll_maintenance() else {
                panic!("original terminal");
            };
            assert_eq!(
                completion
                    .result
                    .expect("queued evidence before timeout cancellation")
                    .local_submission
                    .expect("local receipt")
                    .id,
                request.id
            );
            assert!(commands.try_recv().is_err());
        }
    }

    #[test]
    fn maintenance_submission_cancel_retains_original_receipt_without_retraction() {
        let (mut session, commands, sink) = fixture();
        session
            .begin_maintenance(Command::ResetApplication)
            .expect("test invariant");
        let request = sent(&commands);
        assert!(session.poll_maintenance().is_pending());
        assert!(session.begin_read(Command::Status).is_err());
        assert!(session.cancel_maintenance("operator_stop"));
        assert!(!request.cancelled.load(Ordering::Acquire));
        assert!(!session.maintenance_pending());
        assert!(session.begin_maintenance(Command::EnterUpdate).is_err());
        sink.submitted(request.id);
        let Poll::Ready(completion) = session.poll_maintenance() else {
            panic!("receipt expected");
        };
        assert_eq!(completion.phase, MaintenancePhase::Submission);
        let error = completion.result.expect_err("expected rejection");
        assert!(error.unknown);
        assert_eq!(
            error.local_stage.as_deref(),
            Some("may_have_been_submitted")
        );
        session
            .begin_maintenance(Command::EnterUpdate)
            .expect("test invariant");
        let next = sent(&commands);
        assert_ne!(next.id, request.id);
        assert!(session.poll_maintenance().is_pending());
        sink.submitted(next.id);
        assert!(matches!(
            session.poll_maintenance(),
            Poll::Ready(MaintenanceCompletion { result: Ok(_), .. })
        ));
    }
    #[test]
    fn factory_restore_has_one_client_and_preserves_sdk_validated_full_raw_record() {
        let (mut session, commands, sink) = fixture();
        session
            .begin_maintenance(Command::RestoreFactoryAndRestart)
            .expect("test invariant");
        let query = sent(&commands);
        let Message::ReadConfig { query_id, view: 0 } =
            protocol::decode(&query.bytes, Direction::HostToFirmware).expect("test invariant")
        else {
            panic!("factory read expected");
        };
        let record = include_bytes!("../../../tests/fixtures/config.json");
        let fields = ConfigDataFields {
            sample: SampleData {
                query_id,
                run_nonce: [9; 16],
                snapshot_sequence: 20,
                snapshot_time_us: 20000,
            },
            view: ConfigView::Factory,
            record_state: ConfigRecordState::Complete,
            startup_source: StartupSource::User,
            fallback_reason: FallbackReason::None,
            data_time_us: 20000,
        };
        let mut bytes =
            vec![0; ConfigDataWriter::required_len(record.len()).expect("test invariant")];
        let mut writer =
            ConfigDataWriter::start(fields, record.len(), &mut bytes).expect("test invariant");
        writer.data_mut().copy_from_slice(record);
        let length = writer.finish().expect("test invariant");
        bytes.truncate(length);
        sink.submitted(query.id);
        sink.received(bytes);
        assert!(session.poll_maintenance().is_pending());
        let save = sent(&commands);
        assert!(
            matches!(protocol::decode(&save.bytes,Direction::HostToFirmware).expect("test invariant"),Message::SaveAndRestart{record:raw,..} if raw==record)
        );
        sink.submitted(save.id);
        let Poll::Ready(completion) = session.poll_maintenance() else {
            panic!("save local receipt expected");
        };
        assert_eq!(completion.phase, MaintenancePhase::Submission);
        assert!(
            completion
                .result
                .expect("test invariant")
                .local_submission
                .is_some()
        );
        assert!(commands.try_recv().is_err());
    }
    #[test]
    fn cancelled_factory_never_becomes_a_mutation_and_has_factory_phase() {
        let (mut session, commands, _) = fixture();
        session
            .begin_maintenance(Command::RestoreFactoryAndRestart)
            .expect("test invariant");
        let query = sent(&commands);
        session.cancel_maintenance("stop_before_factory");
        assert!(query.cancelled.load(Ordering::Acquire));
        let Poll::Ready(completion) = session.poll_maintenance() else {
            panic!("factory receipt expected");
        };
        assert_eq!(completion.phase, MaintenancePhase::FactoryRead);
        assert!(completion.result.is_err());
        assert!(commands.try_recv().is_err());
    }
    #[test]
    fn factory_reply_cannot_submit_save_after_idle_freshness_gate_changes() {
        let (mut session, commands, sink) = fixture();
        session
            .begin_maintenance(Command::RestoreFactoryAndRestart)
            .expect("test invariant");
        let query = sent(&commands);
        let Message::ReadConfig { query_id, view: 0 } =
            protocol::decode(&query.bytes, Direction::HostToFirmware).expect("test invariant")
        else {
            panic!("factory read expected");
        };
        let record = include_bytes!("../../../tests/fixtures/config.json");
        let fields = ConfigDataFields {
            sample: SampleData {
                query_id,
                run_nonce: [9; 16],
                snapshot_sequence: 20,
                snapshot_time_us: 20000,
            },
            view: ConfigView::Factory,
            record_state: ConfigRecordState::Complete,
            startup_source: StartupSource::User,
            fallback_reason: FallbackReason::None,
            data_time_us: 20000,
        };
        let mut bytes =
            vec![0; ConfigDataWriter::required_len(record.len()).expect("test invariant")];
        let mut writer =
            ConfigDataWriter::start(fields, record.len(), &mut bytes).expect("test invariant");
        writer.data_mut().copy_from_slice(record);
        let length = writer.finish().expect("test invariant");
        bytes.truncate(length);
        sink.submitted(query.id);
        sink.received(bytes);
        session
            .last_telemetry
            .as_mut()
            .expect("test invariant")
            .received_at = Instant::now() - Duration::from_secs(4);
        let Poll::Ready(completion) = session.poll_maintenance() else {
            panic!("gate refusal expected");
        };
        assert_eq!(completion.phase, MaintenancePhase::Submission);
        let error = completion.result.expect_err("expected rejection");
        assert_eq!(error.local_stage.as_deref(), Some("not_submitted"));
        assert!(!error.unknown);
        assert!(commands.try_recv().is_err());
    }
}
