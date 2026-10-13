// Copyright The eha-sdk Contributors

//! Pollable recording admission, file completion, and initial-target audit barriers.

use super::{scheduler::ReadOwner, *};
use std::task::Poll;

pub(super) enum StartPhase {
    Reading(std::path::PathBuf),
    Writing { recording: Recording, barrier: u64 },
    Terminal(Result<Value, SessionError>),
}

pub(super) struct RecordingStart {
    generation: u64,
    phase: StartPhase,
}

pub(super) struct RecordingFinish {
    recording: Recording,
    result: Option<Result<Value, SessionError>>,
    user_visible: bool,
}

pub(super) struct TrialStart {
    request: TrialRequest,
    generation: u64,
    barrier: Option<u64>,
    error: Option<SessionError>,
}

fn cancelled(reason: &str) -> SessionError {
    let mut error = invalid(format!("本地准备已取消：{reason}"));
    error.kind = "cancelled".into();
    error.local_stage = Some("not_submitted".into());
    error
}

impl ToolSession {
    pub(super) fn pending_recording_snapshot(&self) -> Option<Value> {
        if self.pending_recording_start.is_some() {
            return Some(json!({"active":false,"pending_start":true,"error":self.recording_error}));
        }
        self.pending_recording_finish.as_ref().map(|pending| {
            json!({"active":false,"pending_finish":pending.result.is_none(),"partial":pending.recording.snapshot(),"error":self.recording_error})
        })
    }

    pub(crate) fn begin_recording_start(&mut self, root: &Path) -> Result<(), SessionError> {
        self.require_read_slot_idle()?;
        if self.pending_recording_start.is_some() || self.pending_recording_finish.is_some() {
            return Err(invalid("上一次记录准备或收尾尚未完成"));
        }
        if self.recording.is_some() {
            return Err(invalid("已有进行中的记录"));
        }
        if self.client.is_none() || self.identity.is_none() {
            return Err(invalid("记录需要已核对 Identity 的活跃会话"));
        }
        if self.trial.active.is_some() {
            return Err(invalid("试验进行中不读取配置启动记录；请在试验前开始记录"));
        }
        let capacity = self.telemetry_queue_capacity.unwrap_or(1_024);
        self.telemetry_queue_capacity = Some(capacity);
        self.client
            .as_mut()
            .ok_or_else(|| invalid("没有活跃会话"))?
            .enable_telemetry_queue(capacity)
            .map_err(sdk_error)?;
        self.begin_read_owned(
            Command::ConfigRead {
                view: "startup".into(),
            },
            ReadOwner::RecordingStart,
        )?;
        self.pending_recording_start = Some(RecordingStart {
            generation: self.connection_generation,
            phase: StartPhase::Reading(root.into()),
        });
        Ok(())
    }

    pub(crate) fn poll_recording_start(&mut self) -> Poll<Result<Value, SessionError>> {
        if self
            .pending_recording_start
            .as_ref()
            .is_some_and(|pending| pending.generation != self.connection_generation)
        {
            self.cancel_recording_start("connection_generation_changed");
        }
        let Some(mut pending) = self.pending_recording_start.take() else {
            return Poll::Ready(Err(invalid("没有待领取的记录准备")));
        };
        match pending.phase {
            StartPhase::Terminal(result) => Poll::Ready(result),
            StartPhase::Reading(root) => {
                let startup = match self.poll_read_owned(ReadOwner::RecordingStart) {
                    Poll::Pending => {
                        pending.phase = StartPhase::Reading(root);
                        self.pending_recording_start = Some(pending);
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(result)) => result.data,
                };
                let snapshot = self.snapshot();
                let identity = snapshot.identity.clone().unwrap_or(Value::Null);
                let metadata = json!({
                    "format":"eha-tool-recording-v1", "tool_version":crate::build_metadata::TOOL_VERSION,
                    "connection":snapshot.connection, "identity":identity,
                    "device_uid":identity["uid"], "run_nonce":identity["sample"]["run_nonce"],
                    "startup_config_source":identity["config_source"], "startup_config_source_label":identity["config_source_label"],
                    "startup_config":startup,
                });
                let prepared = (|| {
                    let mut recording =
                        Recording::begin(&root, metadata).map_err(recording_error)?;
                    recording
                        .event(
                            "session_snapshot",
                            serde_json::to_value(snapshot)
                                .map_err(|error| recording_error(error.to_string()))?,
                        )
                        .map_err(recording_error)?;
                    let barrier = recording.barrier().map_err(recording_error)?;
                    Ok::<_, SessionError>(StartPhase::Writing { recording, barrier })
                })();
                match prepared {
                    Ok(phase) => {
                        pending.phase = phase;
                        self.pending_recording_start = Some(pending);
                        Poll::Pending
                    }
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            StartPhase::Writing {
                mut recording,
                barrier,
            } => match recording.poll_barrier(barrier) {
                Poll::Pending => {
                    pending.phase = StartPhase::Writing { recording, barrier };
                    self.pending_recording_start = Some(pending);
                    Poll::Pending
                }
                Poll::Ready(Err(error)) => Poll::Ready(Err(recording_error(error))),
                Poll::Ready(Ok(())) => {
                    let value = recording.snapshot();
                    self.recording = Some(recording);
                    self.recording_error = None;
                    Poll::Ready(Ok(value))
                }
            },
        }
    }

    pub(super) fn cancel_recording_start(&mut self, reason: &str) {
        let Some(mut pending) = self.pending_recording_start.take() else {
            return;
        };
        if matches!(pending.phase, StartPhase::Terminal(_)) {
            self.pending_recording_start = Some(pending);
            return;
        }
        let error = if matches!(pending.phase, StartPhase::Reading(_)) {
            self.cancel_read(reason);
            match self.poll_read_owned(ReadOwner::RecordingStart) {
                Poll::Ready(Err(error)) => error,
                _ => cancelled(reason),
            }
        } else {
            cancelled(reason)
        };
        pending.phase = StartPhase::Terminal(Err(error));
        self.pending_recording_start = Some(pending);
    }

    pub(crate) fn begin_recording_stop(&mut self) -> Result<(), SessionError> {
        if self.pending_recording_finish.is_some() {
            return Err(invalid("记录收尾尚未领取"));
        }
        self.cancel_trial_start("recording_stopped");
        let mut recording = self
            .recording
            .take()
            .ok_or_else(|| invalid("当前没有进行中的记录"))?;
        let result = match &self.recording_error {
            Some(error) => Err(error.clone()),
            None => recording.begin_finish(),
        };
        if let Err(error) = result {
            return Err(self.recording_finish_error(error));
        }
        self.pending_recording_finish = Some(RecordingFinish {
            recording,
            result: None,
            user_visible: true,
        });
        Ok(())
    }

    pub(crate) fn poll_recording_stop(&mut self) -> Poll<Result<Value, SessionError>> {
        self.tick_recording_writer();
        let Some(mut pending) = self.pending_recording_finish.take() else {
            return Poll::Ready(Err(invalid("没有待领取的记录收尾")));
        };
        match pending.result.take() {
            Some(result) => Poll::Ready(result),
            None => {
                self.pending_recording_finish = Some(pending);
                Poll::Pending
            }
        }
    }

    pub(super) fn tick_recording_writer(&mut self) {
        let Some(mut pending) = self.pending_recording_finish.take() else {
            return;
        };
        if pending.result.is_none() {
            match pending.recording.poll_finished() {
                Poll::Pending => {}
                Poll::Ready(Ok(value)) => pending.result = Some(Ok(value)),
                Poll::Ready(Err(error)) => {
                    pending.result = Some(Err(self.recording_finish_error(error)))
                }
            }
        }
        if pending.user_visible || pending.result.is_none() {
            self.pending_recording_finish = Some(pending);
        }
    }

    fn recording_finish_error(&mut self, error: String) -> SessionError {
        self.recording_error = Some(error.clone());
        if self.trial.active.is_some() {
            let _ = self.submit_trial_stop("recording_failed");
        }
        recording_error(error)
    }

    pub(super) fn finish_recording_async_boundary(&mut self, reason: &str) {
        self.cancel_trial_start(reason);
        let Some(mut recording) = self.recording.take() else {
            return;
        };
        if self.recording_error.is_some() {
            return;
        }
        match recording
            .event("recording_boundary", json!({"reason":reason}))
            .and_then(|_| recording.begin_finish())
        {
            Ok(()) => {
                self.pending_recording_finish = Some(RecordingFinish {
                    recording,
                    result: None,
                    user_visible: false,
                })
            }
            Err(error) => self.recording_error = Some(error),
        }
    }

    pub(crate) fn begin_trial_start(&mut self, request: TrialRequest) -> Result<(), SessionError> {
        self.require_read_slot_idle()?;
        if self.pending_trial_start.is_some()
            || self.pending_recording_start.is_some()
            || self.pending_recording_finish.is_some()
        {
            return Err(invalid("本地准备或记录收尾尚未完成"));
        }
        self.validate_prepared_trial_start(&request)?;
        self.record_event("trial_command_input", json!({"request": &request}));
        if self.recording.is_some()
            && let Some(error) = &self.recording_error
        {
            self.trial.prepared = None;
            return Err(SessionError {
                local_stage: Some("not_submitted".into()),
                ..recording_error(error.clone())
            });
        }
        let barrier = self
            .recording
            .as_mut()
            .map(Recording::barrier)
            .transpose()
            .map_err(|error| {
                self.recording_error = Some(error.clone());
                self.trial.prepared = None;
                SessionError {
                    local_stage: Some("not_submitted".into()),
                    ..recording_error(error)
                }
            })?;
        self.pending_trial_start = Some(TrialStart {
            request,
            generation: self.connection_generation,
            barrier,
            error: None,
        });
        Ok(())
    }

    pub(crate) fn poll_trial_start(&mut self) -> Poll<Result<Value, SessionError>> {
        if self
            .pending_trial_start
            .as_ref()
            .is_some_and(|pending| pending.generation != self.connection_generation)
        {
            self.cancel_trial_start("connection_generation_changed");
        }
        let Some(mut pending) = self.pending_trial_start.take() else {
            return Poll::Ready(Err(invalid("没有待领取的试验开始")));
        };
        if let Some(error) = pending.error.take() {
            return Poll::Ready(Err(error));
        }
        if let Some(barrier) = pending.barrier {
            let Some(recording) = self.recording.as_mut() else {
                self.trial.prepared = None;
                return Poll::Ready(Err(cancelled("recording_closed")));
            };
            match recording.poll_barrier(barrier) {
                Poll::Pending => {
                    self.pending_trial_start = Some(pending);
                    return Poll::Pending;
                }
                Poll::Ready(Err(error)) => {
                    self.trial.prepared = None;
                    self.recording_error = Some(error.clone());
                    return Poll::Ready(Err(SessionError {
                        local_stage: Some("not_submitted".into()),
                        ..recording_error(error)
                    }));
                }
                Poll::Ready(Ok(())) => {}
            }
        }
        Poll::Ready(self.start_prepared_trial_after_audit(pending.request))
    }

    pub(super) fn cancel_trial_start(&mut self, reason: &str) {
        if let Some(pending) = self.pending_trial_start.as_mut()
            && pending.error.is_none()
        {
            pending.error = Some(cancelled(reason));
            self.trial.prepared = None;
        }
    }

    pub(super) fn cancel_pending_operations(&mut self, reason: &str) {
        self.cancel_read(reason);
        self.cancel_recording_start(reason);
        self.cancel_trial_start(reason);
    }
}
