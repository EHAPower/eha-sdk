// Copyright The eha-sdk Contributors
//! 可轮询的控制提交，保留唯一 Client 及其本地结果。
use super::*;
use std::task::Poll;

pub(super) struct PendingControl {
    command: Command,
    generation: u64,
    pending_target_before: bool,
    trial_stop: bool,
    pub(super) trial_position: bool,
    pub(super) trial_initial: bool,
    pub(super) autonomous_stop: bool,
    stop_claim_ready: bool,
    result: Option<Result<CommandResult, SessionError>>,
}

impl ToolSession {
    pub(crate) fn begin_control(&mut self, command: Command) -> Result<(), SessionError> {
        if matches!(command, Command::Stop)
            && self
                .trial
                .active
                .as_ref()
                .is_some_and(|trial| trial.stop_submission.is_some())
            && let Some(pending) = self.pending_control.as_mut()
            && pending.stop_claim_ready
            && pending.generation == self.connection_generation
        {
            // 显式 Stop 持有已有尝试及其回执，绝不重放。
            pending.stop_claim_ready = false;
            return Ok(());
        }
        if self.pending_control.is_some() {
            return Err(control_busy());
        }
        if !matches!(
            command,
            Command::Stop
                | Command::HeartbeatStart
                | Command::HeartbeatStop
                | Command::HeartbeatOnce
                | Command::Position { .. }
                | Command::Velocity { .. }
                | Command::Force { .. }
                | Command::Impedance { .. }
        ) {
            return Err(invalid("不是可轮询控制动作"));
        }
        let trial_stop = matches!(command, Command::Stop) && self.integration_unresolved_trial();
        if trial_stop {
            self.resume_interrupted_trial_for_stop()?;
        } else if self.integration_unresolved_trial() {
            return Err(invalid("试验进行中；控制须通过试验入口"));
        }
        self.prepare_read_boundary(&command)?;
        if trial_stop {
            let active = self
                .trial
                .active
                .as_ref()
                .ok_or_else(|| invalid("没有可停止的原试验"))?;
            if active.stop_submission.is_some() {
                return Err(invalid("原试验已尝试 Stop；不可重放"));
            }
            self.position_audit = None;
            let baseline = self
                .last_telemetry
                .as_ref()
                .and_then(|reply| reply_json(reply).ok())
                .and_then(|v| v["sample"]["snapshot_time_us"].as_u64())
                .unwrap_or(0);
            let active = self.trial.active.as_mut().expect("checked active");
            active.pending_position_mm = None;
            active.stop_reason = Some("explicit".into());
            active.stop_after_sample_time_us = Some(baseline);
            // 向 SDK 交付字节之前，先预留这唯一一次尝试。
            active.stop_submission =
                Some(json!({"local_submission":"pending","device_execution":"unknown"}));
        }
        self.record_event("command_input", json!({"command": &command}));
        let wait = Wait::new(if trial_stop {
            self.timeout.min(Duration::from_millis(100))
        } else {
            self.timeout
        });
        let pending_target_before = self.pending_target;
        let started = self.begin_control_sdk(&command, &wait);
        match started {
            Ok(immediate) => {
                if matches!(
                    command,
                    Command::Position { .. }
                        | Command::Velocity { .. }
                        | Command::Force { .. }
                        | Command::Impedance { .. }
                ) {
                    self.pending_target = true;
                }
                let result = immediate.map(|data| {
                    Ok(CommandResult {
                        action: command_name(&command).into(),
                        local_submission: None,
                        data,
                        snapshot: self.snapshot_cached(),
                    })
                });
                self.pending_control = Some(PendingControl {
                    command,
                    generation: self.connection_generation,
                    pending_target_before,
                    trial_stop,
                    trial_position: false,
                    trial_initial: false,
                    autonomous_stop: false,
                    stop_claim_ready: false,
                    result,
                });
                Ok(())
            }
            Err(error) => {
                if trial_stop {
                    self.finish_control_trial_stop(&Err(error.clone()));
                }
                Err(error)
            }
        }
    }

    pub(super) fn begin_control_sdk(
        &mut self,
        command: &Command,
        wait: &Wait,
    ) -> Result<Option<Value>, SessionError> {
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| invalid("请先 connect 并核对 Identity"))?;
        match *command {
            Command::Stop => client.begin_stop_control(wait),
            Command::HeartbeatOnce => client.begin_heartbeat(wait),
            Command::Position { mm } => client.begin_position(mm, wait),
            Command::Velocity { mm_s } => client.begin_velocity(mm_s, wait),
            Command::Force { n } => client.begin_force(n, wait),
            Command::Impedance {
                equilibrium_mm,
                stiffness_n_per_mm,
                damping_ns_per_mm,
            } => {
                client.begin_impedance(equilibrium_mm, stiffness_n_per_mm, damping_ns_per_mm, wait)
            }
            Command::HeartbeatStart => {
                return client
                    .start_heartbeat()
                    .map(|()| Some(json!({"requested":true})))
                    .map_err(sdk_error);
            }
            Command::HeartbeatStop => {
                return client
                    .stop_heartbeat()
                    .map(|()| Some(json!({"requested":true})))
                    .map_err(sdk_error);
            }
            _ => return Err(invalid("不是可轮询控制动作")),
        }
        .map_err(sdk_error)?;
        Ok(None)
    }

    pub(super) fn begin_initial_submission(
        &mut self,
        command: Command,
    ) -> Result<(), SessionError> {
        if self.pending_control.is_some() {
            return Err(control_busy());
        }
        self.begin_control_sdk(
            &command,
            &Wait::new(self.timeout.min(Duration::from_millis(100))),
        )?;
        self.pending_control = Some(PendingControl {
            command,
            generation: self.connection_generation,
            pending_target_before: self.pending_target,
            trial_stop: false,
            trial_position: false,
            trial_initial: true,
            autonomous_stop: false,
            stop_claim_ready: false,
            result: None,
        });
        Ok(())
    }

    pub(super) fn begin_position_submission(&mut self, mm: f32) -> Result<(), SessionError> {
        if self.pending_control.is_some() {
            return Err(control_busy());
        }
        let command = Command::Position { mm };
        self.position_submission_mm = Some(mm);
        self.begin_control_sdk(
            &command,
            &Wait::new(self.timeout.min(Duration::from_millis(100))),
        )?;
        self.pending_control = Some(PendingControl {
            command,
            generation: self.connection_generation,
            pending_target_before: self.pending_target,
            trial_stop: false,
            trial_position: true,
            trial_initial: false,
            autonomous_stop: false,
            stop_claim_ready: false,
            result: None,
        });
        Ok(())
    }

    pub(crate) fn poll_control(&mut self) -> Poll<Result<CommandResult, SessionError>> {
        if self
            .pending_control
            .as_ref()
            .is_some_and(|p| p.generation != self.connection_generation)
        {
            self.cancel_pending_control("connection_generation_changed");
        }
        let Some(mut pending) = self.pending_control.take() else {
            return Poll::Ready(Err(invalid("没有待领取的控制提交")));
        };
        let result = if let Some(result) = pending.result.take() {
            result
        } else {
            let result = match self.client.as_mut() {
                Some(client) => client
                    .poll_submission()
                    .map(|result| result.map_err(sdk_error)),
                None => Poll::Ready(Err(invalid("控制通路已经释放"))),
            };
            match result {
                Poll::Pending => {
                    self.pending_control = Some(pending);
                    return Poll::Pending;
                }
                Poll::Ready(result) => result.map(|local| {
                    let mut local_submission = None;
                    let data = submission_data(local, &mut local_submission);
                    CommandResult {
                        action: command_name(&pending.command).into(),
                        local_submission,
                        data,
                        snapshot: self.snapshot_cached(),
                    }
                }),
            }
        };
        if result
            .as_ref()
            .is_err_and(|error| error.local_stage.as_deref() == Some("not_submitted"))
        {
            self.pending_target = pending.pending_target_before;
        }
        if pending.trial_stop {
            self.finish_control_trial_stop(&result);
        }
        match &result {
            Ok(result) => self.record_event("command_completed",json!({"action":result.action,"local_submission":result.local_submission,"data":result.data})),
            Err(error) => self.record_event("command_error",json!({"error":error})),
        }
        Poll::Ready(result)
    }

    fn finish_control_trial_stop(&mut self, result: &Result<CommandResult, SessionError>) {
        let value = match result {
            Ok(result) => {
                json!({"local_submission":result.local_submission,"data":result.data,"device_execution":"awaiting_new_telemetry"})
            }
            Err(error) => json!({"error":error,"device_execution":"unknown_no_retry"}),
        };
        if let Some(active) = self.trial.active.as_mut().or_else(|| {
            self.trial
                .interrupted
                .as_mut()
                .map(|trial| &mut trial.active)
        }) {
            active.stop_submission = Some(value.clone());
            active.stop_attempt_finished_at = Some(Instant::now());
        }
        self.record_event(
            if result.is_ok() {
                "trial_stop_submitted"
            } else {
                "trial_stop_error"
            },
            value,
        );
    }

    pub(super) fn cancel_pending_control(&mut self, reason: &str) {
        let Some(pending) = self.pending_control.as_mut() else {
            return;
        };
        if pending.result.is_some() {
            return;
        }
        let stage = self
            .client
            .as_mut()
            .and_then(|client| client.cancel_submission().ok())
            .unwrap_or(eha_sdk::host::LocalStage::MayHaveBeenSubmitted);
        let mut error = invalid(format!("控制等待已取消：{reason}"));
        error.kind = if stage == eha_sdk::host::LocalStage::NotSubmitted {
            "cancelled"
        } else {
            "result_unknown"
        }
        .into();
        error.unknown = stage != eha_sdk::host::LocalStage::NotSubmitted;
        error.local_stage = Some(
            if stage == eha_sdk::host::LocalStage::NotSubmitted {
                "not_submitted"
            } else {
                "may_have_been_submitted"
            }
            .into(),
        );
        pending.result = Some(Err(error));
    }

    pub(super) fn cancel_initial_control(&mut self, reason: &str) -> Option<SessionError> {
        if !self
            .pending_control
            .as_ref()
            .is_some_and(|pending| pending.trial_initial)
        {
            return None;
        }
        self.cancel_pending_control(reason);
        self.pending_control
            .take()
            .and_then(|pending| pending.result)
            .and_then(Result::err)
    }

    /// 阻断本地生产者；原 begin/poll 回执仍可领取。
    pub(crate) fn fence_control(&mut self, reason: &str) {
        self.cancel_preparations(reason);
        let preserve_stop = reason == "explicit_stop"
            && self
                .trial
                .active
                .as_ref()
                .is_some_and(|trial| trial.stop_submission.is_some())
            && self.pending_control.as_ref().is_some_and(|pending| {
                pending.trial_stop
                    && matches!(pending.command, Command::Stop)
                    && pending.generation == self.connection_generation
            });
        if preserve_stop {
            let pending = self.pending_control.as_mut().expect("existing trial Stop");
            // 准入时即转交回执所有权，先于期间可能发生的任何 tick。
            pending.autonomous_stop = false;
            pending.stop_claim_ready = true;
        } else {
            self.cancel_pending_control(reason);
        }
        if self
            .pending_control
            .as_ref()
            .is_some_and(|p| p.trial_position)
        {
            let result = self
                .pending_control
                .take()
                .and_then(|p| p.result)
                .expect("cancel preserves terminal");
            if let Some(target) = self.pending_trial_target.as_mut() {
                target.result = Some(result);
            }
        }
        if let Some(target) = self.pending_trial_target.as_mut()
            && target.result.is_none()
        {
            target.result = Some(Err(control_cancelled_before_send(reason)));
        }
        self.trial.prepared = None;
        self.position_audit = None;
        self.position_submission_mm = None;
        if let Some(active) = self.trial.active.as_mut() {
            active.pending_position_mm = None;
        }
    }
}

fn control_busy() -> SessionError {
    let mut error = invalid("控制完成结果尚未领取");
    error.kind = "busy".into();
    error.local_stage = Some("not_submitted".into());
    error
}

pub(super) struct TrialTarget {
    pub(super) result: Option<Result<CommandResult, SessionError>>,
}
impl ToolSession {
    pub(crate) fn begin_trial_target(&mut self, mm: f32) -> Result<(), SessionError> {
        if self.pending_trial_target.is_some() || self.pending_control.is_some() {
            return Err(control_busy());
        }
        self.update_trial_position(mm)?;
        self.pending_trial_target = Some(TrialTarget { result: None });
        Ok(())
    }
    pub(crate) fn poll_trial_target(&mut self) -> Poll<Result<CommandResult, SessionError>> {
        if self.pending_trial_target.is_none() {
            return Poll::Ready(Err(invalid("没有待领取的位置目标")));
        }
        if self
            .pending_trial_target
            .as_ref()
            .is_some_and(|target| target.result.is_none())
        {
            self.flush_trial_position(Instant::now());
        }
        let mut target = self.pending_trial_target.take().expect("checked target");
        if let Some(result) = target.result.take() {
            Poll::Ready(result)
        } else {
            self.pending_trial_target = Some(target);
            Poll::Pending
        }
    }
}
fn control_cancelled_before_send(reason: &str) -> SessionError {
    let mut error = invalid(format!("目标尚未提交已取消：{reason}"));
    error.kind = "cancelled".into();
    error.local_stage = Some("not_submitted".into());
    error
}
