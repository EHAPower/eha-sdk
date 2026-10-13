// Copyright The eha-sdk Contributors
//! 固定的控制准入与有界 owner 续执行。此处不创建 Client。
use super::{
    Method, Workbench, integration,
    maintenance::{ApiError, book_error, denied, ledger_error, member, scope_current},
    mutations,
    operations::{Action, Admission, Effect, Intent, OperationScope, Submission},
    reservations::{Access, EffectDisposition, MemberKey},
};
use crate::{
    RawFields,
    session::{Command, SessionError, TrialRequest},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    task::Poll,
    time::{Duration, Instant},
};

/// 准入与 describe 共用一个发布开关；仅在独立 R2 通过后打开。
pub(super) const RELEASED: bool = true;

pub(super) enum Task {
    Command(Command),
    Trial(TrialRequest),
    TrialTarget { trial_id: String, mm: f32 },
    RecordingStart,
    RecordingStop,
    Stop,
}
impl Task {
    fn action(&self) -> Result<Action, ApiError> {
        Ok(match self {
            Self::Command(command) => match command {
                Command::HeartbeatStart => Action::HeartbeatStart,
                Command::HeartbeatStop => Action::HeartbeatStop,
                Command::HeartbeatOnce => Action::HeartbeatOnce,
                Command::Position { .. } => Action::Position,
                Command::Velocity { .. } => Action::Velocity,
                Command::Force { .. } => Action::Force,
                Command::Impedance { .. } => Action::Impedance,
                _ => return Err("不是固定控制动作".to_owned().into()),
            },
            Self::Trial(_) => Action::TrialStart,
            Self::TrialTarget { .. } => Action::TrialTarget,
            Self::RecordingStart => Action::RecordingStart,
            Self::RecordingStop => Action::RecordingStop,
            Self::Stop => Action::Stop,
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Queued,
    Preflight,
    Auditing,
    Prepared,
    Starting,
    Running,
}
struct Job {
    id: String,
    member: MemberKey,
    task: Task,
    phase: Phase,
    group: Option<String>,
    admitted: Instant,
    cleanup_grant: Option<String>,
}
struct CleanupTicket {
    member: MemberKey,
    grant_id: String,
    claimant: String,
    expires: Instant,
}
pub(super) struct State {
    jobs: VecDeque<Job>,
    stops: VecDeque<Job>,
    trials: BTreeMap<MemberKey, String>,
    cleanup_tickets: BTreeMap<String, CleanupTicket>,
}
impl State {
    pub(super) fn new() -> Self {
        Self {
            jobs: VecDeque::new(),
            stops: VecDeque::new(),
            trials: BTreeMap::new(),
            cleanup_tickets: BTreeMap::new(),
        }
    }
    pub(super) fn busy(&self) -> bool {
        !self.jobs.is_empty() || !self.stops.is_empty()
    }
    pub(super) fn trial_prepared(&self, id: &str) -> bool {
        self.jobs
            .iter()
            .any(|job| job.id == id && job.phase == Phase::Prepared)
    }
    pub(super) fn release_trial(&mut self, id: &str) {
        if let Some(job) = self
            .jobs
            .iter_mut()
            .find(|job| job.id == id && job.phase == Phase::Prepared)
        {
            job.group = None;
        }
    }
    #[allow(clippy::too_many_arguments)] // 同一准入步骤同时接收不可变身份、范围与原始意图。
    pub(super) fn admit(
        &mut self,
        common: &mut mutations::State,
        workbench: &mut Workbench,
        member: MemberKey,
        scope: OperationScope,
        reservation: Option<String>,
        claimant: &str,
        id: &str,
        task: Task,
        parameters: Vec<(String, String)>,
        group: Option<String>,
    ) -> Result<(bool, Value), ApiError> {
        if !common.control_enabled {
            return Err(denied("control_unavailable", "控制发布门尚未开放"));
        }
        let stop = matches!(task, Task::Stop);
        let cleanup = reservation.is_none()
            && matches!(
                task,
                Task::Command(Command::HeartbeatStop) | Task::RecordingStop
            );
        if !cleanup && self.cleanup_tickets.contains_key(id) {
            return Err(denied(
                "request_id_conflict",
                "本地收尾票据不可用于其他动作",
            ));
        }
        if !stop
            && workbench
                .p2
                .as_ref()
                .is_some_and(super::p2::State::connection_busy)
        {
            return Err(denied("connection_busy", "等待连接工作完成后再明确控制"));
        }
        if !stop && !cleanup && common.readback_required(&member) {
            return Err(denied("maintenance_busy", "重连后先读取 User 和 Startup"));
        }
        let intent = Intent::new(scope, task.action()?, reservation.clone(), parameters)
            .map_err(ledger_error)?;
        if stop || cleanup {
            common.book.authorize_stop(&member).map_err(book_error)?;
        }
        if let Some(record) = common.ledger.get(id) {
            if record.intent != intent || (!stop && record.provenance != claimant) {
                return Err(denied(
                    "request_id_conflict",
                    "编号原始参数或请求会话不一致",
                ));
            }
            return Ok((false, common.projection(id)?));
        }
        if !scope_current(workbench, &member) {
            return Err(denied("binding_stale", "固定身份或代次已改变"));
        }
        let cleanup_grant = if cleanup {
            let ticket = self
                .cleanup_tickets
                .get(id)
                .ok_or_else(|| denied("request_id_unknown", "需要原范围本地收尾票据"))?;
            let grant = common
                .book
                .grant(&member)
                .ok_or_else(|| denied("permission_denied", "原授权已撤销"))?;
            if ticket.member != member
                || ticket.claimant != claimant
                || ticket.grant_id != grant.id
                || ticket.expires <= Instant::now()
            {
                return Err(denied(
                    "permission_conflict",
                    "收尾票据身份、会话、授权或期限已改变",
                ));
            }
            Some(grant.id.clone())
        } else {
            None
        };
        if stop {
            common.book.authorize_stop(&member).map_err(book_error)?;
        } else {
            if !cleanup {
                let reservation = reservation
                    .as_deref()
                    .ok_or_else(|| denied("reservation_unknown", "控制需要占用"))?;
                common
                    .book
                    .authorize_member(reservation, claimant, &member, Access::Control)
                    .map_err(book_error)?;
            }
            if self
                .jobs
                .iter()
                .chain(&self.stops)
                .any(|job| job.member.uid == member.uid)
                || common.jobs.values().any(|job| {
                    common.ledger.get(&job.id).is_none_or(|record| {
                        record
                            .intent
                            .scope
                            .members
                            .iter()
                            .any(|held| held.uid == member.uid)
                    })
                })
            {
                return Err(denied("control_busy", "设备已有等待动作"));
            }
            if self.jobs.len() >= 32 {
                return Err(denied("operation_capacity", "控制等待容量已满"));
            }
            if let Task::TrialTarget { trial_id, .. } = &task
                && self.trials.get(&member) != Some(trial_id)
            {
                return Err(denied("trial_stale", "试验编号或原成员不一致"));
            }
        }
        let admitted = if stop {
            common
                .ledger
                .admit_stop(id, intent, claimant, &mut common.ids, Instant::now())
                .map_err(ledger_error)?
        } else {
            common.admit_normal(id, intent, claimant, Instant::now())?
        };
        common.tickets.remove(id);
        self.cleanup_tickets.remove(id);
        if stop {
            // 下一 owner 步骤之前，阻断此物理 UID/run 的全部别名。
            let cancelled: Vec<_> = self
                .jobs
                .iter()
                .filter(|job| {
                    job.member.uid == member.uid && job.member.run_nonce == member.run_nonce
                })
                .map(|job| job.id.clone())
                .collect();
            for cancelled_id in cancelled {
                self.cancel(&cancelled_id, common, workbench, "explicit_stop");
            }
            workbench.service_fence_control(&member.uid, &member.run_nonce, "explicit_stop");
            for cancelled_id in admitted.cancelled_ids {
                finish_cancelled(common, &cancelled_id, "explicit_stop");
            }
        } else if !cleanup {
            common
                .book
                .begin_member_pending(
                    reservation.as_deref().expect("validated reservation"),
                    id,
                    claimant,
                    &member,
                    Access::Control,
                )
                .map_err(book_error)?;
        }
        common.receipts.insert(
            id.into(),
            mutations::Receipt {
                phase: None,
                native: None,
                error: None,
            },
        );
        let job = Job {
            id: id.into(),
            member,
            task,
            phase: Phase::Queued,
            group,
            admitted: Instant::now(),
            cleanup_grant,
        };
        common.control_facts.insert(
            id.into(),
            json!({"phase":"queued","stop_attempt_delay_ms":null}),
        );
        if stop {
            self.stops.push_back(job);
        } else {
            self.jobs.push_back(job);
        }
        Ok((true, common.projection(id)?))
    }
    pub(super) fn cancel(
        &mut self,
        id: &str,
        common: &mut mutations::State,
        workbench: &mut Workbench,
        reason: &str,
    ) {
        let Some(index) = self.jobs.iter().position(|job| job.id == id) else {
            return;
        };
        let job = self.jobs.remove(index).expect("located job");
        self.cancel_job(&job, common, workbench, reason);
    }
    fn cancel_job(
        &mut self,
        job: &Job,
        common: &mut mutations::State,
        workbench: &mut Workbench,
        reason: &str,
    ) {
        let id = &job.id;
        let mut terminal = None;
        if job.phase != Phase::Queued
            && let Some(session) = workbench.integration_session_mut(&job.member.target_id)
        {
            session.fence_control(reason);
            let polled=match job.phase {
                Phase::Preflight=>session.poll_trial_preflight().map(|result|result.map(|()|Value::Null)),
                Phase::Auditing | Phase::Prepared=>session.poll_trial_audit().map(|result|result.map(|()|Value::Null)),
                Phase::Starting=>session.poll_trial_start(),
                Phase::Running=>match &job.task {
                    Task::Command(_)=>session.poll_control().map(|result|result.map(|value|json!({"local_submission":value.local_submission,"data":value.data}))),
                    Task::TrialTarget{..}=>session.poll_trial_target().map(|result|result.map(|value|json!({"local_submission":value.local_submission,"data":value.data}))),
                    Task::RecordingStart=>session.poll_recording_start(),
                    Task::RecordingStop=>{
                        let result = session.poll_recording_stop();
                        if result.is_pending() {
                            session.detach_recording_stop();
                        }
                        result
                    },
                    _=>Poll::Pending,
                },
                _=>Poll::Ready(Err(super::maintenance::local_error(reason))),
            };
            if let Poll::Ready(result) = polled {
                terminal = Some(result);
            }
        }
        let Some(record) = common.ledger.get(id).cloned() else {
            return;
        };
        if record.admission == Admission::Queued {
            let _ = common.ledger.cancel_queued(id, Instant::now());
            finish_cancelled(common, id, reason);
        } else if record.admission == Admission::Attempting {
            let unknown =
                terminal.is_none() && matches!(job.phase, Phase::Starting | Phase::Running);
            self.finish(
                common,
                job,
                terminal.unwrap_or_else(|| Err(super::maintenance::local_error(reason))),
                unknown,
            );
        } else {
            finish_cancelled(common, id, reason);
        }
    }
    /// Stop 在准入后获得首次尝试。普通续执行每次推进一个成员。
    pub(super) fn poll(&mut self, common: &mut mutations::State, workbench: &mut Workbench) {
        self.cleanup_tickets.retain(|id, ticket| {
            let valid = ticket.expires > Instant::now()
                && common
                    .book
                    .grant(&ticket.member)
                    .is_some_and(|grant| grant.permissions.control && grant.id == ticket.grant_id);
            if !valid {
                let _ = common.ids.invalidate(id);
            }
            valid
        });
        let stop_count = self.stops.len();
        for _ in 0..stop_count {
            if let Some(mut job) = self.stops.pop_front()
                && !self.step(&mut job, common, workbench)
            {
                self.stops.push_back(job);
            }
        }
        if let Some(mut job) = self.jobs.pop_front()
            && !self.step(&mut job, common, workbench)
        {
            self.jobs.push_back(job);
        }
    }
    fn step(
        &mut self,
        job: &mut Job,
        common: &mut mutations::State,
        workbench: &mut Workbench,
    ) -> bool {
        let Some(record) = common.ledger.get(&job.id).cloned() else {
            return true;
        };
        if record.admission == Admission::Finished {
            finish_cancelled(common, &job.id, "cancelled_before_submission");
            return true;
        }
        if !scope_current(workbench, &job.member) {
            if job.phase == Phase::Queued {
                let _ = common.ledger.cancel_queued(&job.id, Instant::now());
                finish_cancelled(common, &job.id, "binding_stale");
            } else {
                self.cancel_job(job, common, workbench, "binding_stale");
            }
            return true;
        }
        if let Some(grant_id) = &job.cleanup_grant {
            if common
                .book
                .grant(&job.member)
                .is_none_or(|grant| !grant.permissions.control || grant.id != *grant_id)
            {
                self.cancel_job(job, common, workbench, "cleanup_permission_changed");
                return true;
            }
        } else if !matches!(job.task, Task::Stop) {
            let authorized = record
                .intent
                .reservation_id
                .as_deref()
                .and_then(|id| common.book.reservation(id))
                .is_some_and(|reservation| {
                    reservation.claimant == record.provenance
                        && reservation.access == Access::Control
                        && reservation
                            .members
                            .iter()
                            .position(|member| member == &job.member)
                            .is_some_and(|index| {
                                common.book.grant(&job.member).is_some_and(|grant| {
                                    grant.permissions.control
                                        && reservation.grant_ids[index] == grant.id
                                })
                            })
                        && common.book.claim(&job.member.uid).is_some_and(|claim| {
                            claim.pending_operation_id.as_deref() == Some(&job.id)
                                && !claim.released
                                && !claim.unresolved
                        })
                });
            if !authorized {
                self.cancel_job(
                    job,
                    common,
                    workbench,
                    "reservation_released_or_permission_changed",
                );
                return true;
            }
        }
        if job.phase == Phase::Queued && common.ledger.begin_attempt(&job.id).is_err() {
            return true;
        }
        let runs_dir = workbench.service_runs_dir();
        let Some(session) = workbench.integration_session_mut(&job.member.target_id) else {
            return true;
        };
        let result: Poll<Result<Value, SessionError>> = match job.phase {
            Phase::Queued => {
                if matches!(job.task, Task::Stop) {
                    common.control_facts.insert(job.id.clone(), json!({"phase":"submission","stop_attempt_delay_ms":job.admitted.elapsed().as_millis().to_string()}));
                }
                let started = match &job.task {
                    Task::Command(command) => session.begin_control(command.clone()),
                    Task::Stop => session.begin_control(Command::Stop),
                    Task::Trial(request) => session.begin_trial_preflight(request.clone()),
                    Task::RecordingStart => session.begin_recording_start(&runs_dir),
                    Task::RecordingStop => session.begin_recording_stop(),
                    Task::TrialTarget { mm, .. } => session.begin_trial_target(*mm),
                };
                match started {
                    Ok(()) => {
                        job.phase = if matches!(job.task, Task::Trial(_)) {
                            Phase::Preflight
                        } else {
                            Phase::Running
                        };
                        Poll::Pending
                    }
                    Err(error) => Poll::Ready(Err(error)),
                }
            }
            Phase::Preflight => match session.poll_trial_preflight() {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => match &job.task {
                    Task::Trial(request) => match session.begin_trial_audit(request.clone()) {
                        Ok(()) => {
                            job.phase = Phase::Auditing;
                            Poll::Pending
                        }
                        Err(error) => Poll::Ready(Err(error)),
                    },
                    _ => Poll::Ready(Err(super::maintenance::local_error("invalid_trial_phase"))),
                },
            },
            Phase::Auditing => match session.poll_trial_audit() {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {
                    job.phase = Phase::Prepared;
                    Poll::Pending
                }
            },
            Phase::Prepared => {
                if job.group.is_some() {
                    return false;
                }
                match &job.task {
                    Task::Trial(request) => match session.begin_trial_start(request.clone()) {
                        Ok(()) => {
                            job.phase = Phase::Starting;
                            Poll::Pending
                        }
                        Err(error) => Poll::Ready(Err(error)),
                    },
                    _ => Poll::Ready(Err(super::maintenance::local_error("invalid_trial_phase"))),
                }
            }
            Phase::Starting => session.poll_trial_start(),
            Phase::Running => match &job.task {
                Task::Command(_) | Task::Stop => session
                    .poll_control()
                    .map(|result| result.map(command_result)),
                Task::RecordingStart => session.poll_recording_start(),
                Task::RecordingStop => session.poll_recording_stop(),
                Task::TrialTarget { .. } => session
                    .poll_trial_target()
                    .map(|result| result.map(command_result)),
                _ => Poll::Ready(Err(super::maintenance::local_error(
                    "invalid_control_phase",
                ))),
            },
        };
        match result {
            Poll::Pending => {
                let phase = match job.phase {
                    Phase::Queued => "queued",
                    Phase::Preflight => "preflight",
                    Phase::Auditing => "recording",
                    Phase::Prepared => "prepared",
                    Phase::Starting | Phase::Running => "submission",
                };
                common
                    .control_facts
                    .entry(job.id.clone())
                    .or_insert_with(|| json!({}))["phase"] = json!(phase);
                false
            }
            Poll::Ready(result) => {
                if result.is_ok() && matches!(job.task, Task::Trial(_)) {
                    self.trials.insert(job.member.clone(), job.id.clone());
                    common
                        .control_facts
                        .entry(job.id.clone())
                        .or_insert_with(|| json!({}))["trial_id"] = json!(job.id);
                }
                self.finish(common, job, result, false);
                true
            }
        }
    }
    fn finish(
        &mut self,
        common: &mut mutations::State,
        job: &Job,
        result: Result<Value, SessionError>,
        forced_unknown: bool,
    ) {
        let (submission, effect, disposition, native, error) = match result {
            Ok(value) => (
                if job.cleanup_grant.is_some() {
                    Submission::NotSubmitted
                } else {
                    Submission::LocallySubmitted
                },
                if job.cleanup_grant.is_some() {
                    Effect::NotApplicable
                } else {
                    Effect::Known
                },
                EffectDisposition::KnownEffect,
                serde_json::to_string(&value).ok(),
                None,
            ),
            Err(error) => {
                let effect_possible = matches!(
                    job.task,
                    Task::Command(_) | Task::Stop | Task::TrialTarget { .. }
                ) || matches!(job.task, Task::Trial(_))
                    && job.phase == Phase::Starting;
                let unknown = effect_possible
                    && (forced_unknown
                        || error.unknown
                        || error
                            .local_stage
                            .as_deref()
                            .is_some_and(|stage| stage != "not_submitted"));
                (
                    if unknown {
                        Submission::Unknown
                    } else {
                        Submission::NotSubmitted
                    },
                    if unknown {
                        Effect::Unresolved
                    } else {
                        Effect::NotAttempted
                    },
                    if unknown {
                        EffectDisposition::Unresolved
                    } else {
                        EffectDisposition::NotSubmitted
                    },
                    serde_json::to_string(&error).ok(),
                    Some(json!({"code":error.kind,"message":error.message})),
                )
            }
        };
        if common
            .ledger
            .finish_attempt(&job.id, submission, effect, Instant::now())
            .is_err()
        {
            return;
        }
        if let Some(reservation) = common
            .ledger
            .get(&job.id)
            .and_then(|record| record.intent.reservation_id.as_deref())
        {
            let _ = common
                .book
                .finish_pending(reservation, &job.id, &job.member, disposition);
        }
        common
            .control_facts
            .entry(job.id.clone())
            .or_insert_with(|| json!({}))["phase"] = json!("confirmation");
        common.receipts.insert(
            job.id.clone(),
            mutations::Receipt {
                phase: None,
                native,
                error,
            },
        );
    }
}
fn command_result(value: crate::session::CommandResult) -> Value {
    json!({"local_submission":value.local_submission,"data":value.data})
}

fn finish_cancelled(common: &mut mutations::State, id: &str, reason: &str) {
    if let Some(record) = common.ledger.get(id)
        && let Some(reservation) = record.intent.reservation_id.as_deref()
    {
        let _ = common.book.finish_pending(
            reservation,
            id,
            &record.intent.scope.members[0],
            EffectDisposition::NotSubmitted,
        );
    }
    common.receipts.insert(
        id.into(),
        mutations::Receipt {
            phase: None,
            native: None,
            error: Some(json!({"code":"cancelled","message":reason})),
        },
    );
}

fn check_control_command_fields(fields: &RawFields) -> Result<(), ApiError> {
    let action = fields.string("action")?;
    let allowed: &[&str] = match action.as_str() {
        "position" => &["action", "mm"],
        "velocity" => &["action", "mm_s"],
        "force" => &["action", "n"],
        "impedance" => &[
            "action",
            "equilibrium_mm",
            "stiffness_n_per_mm",
            "damping_ns_per_mm",
        ],
        "heartbeat_start" | "heartbeat_stop" | "heartbeat_once" => &["action"],
        _ => return Err("不支持的固定控制动作".to_owned().into()),
    };
    fields.reject_unknown(allowed)?;
    Ok(())
}

pub(super) fn decode_task(
    kind: &str,
    fields: &RawFields,
) -> Result<(Task, Vec<(String, String)>), ApiError> {
    let raw = |name: &str| -> Result<Vec<(String, String)>, ApiError> {
        Ok(vec![(
            name.into(),
            fields
                .0
                .get(name)
                .ok_or_else(|| format!("缺少{name}"))?
                .get()
                .into(),
        )])
    };
    match kind {
        "controls" => {
            let command_fields: RawFields = fields.decode_field("command")?;
            check_control_command_fields(&command_fields)?;
            Ok((
                Task::Command(Command::from_fields(&command_fields)?),
                raw("command")?,
            ))
        }
        "trials/target" => Ok((
            Task::TrialTarget {
                trial_id: fields.string("trial_id")?,
                mm: fields.decode_f32_field("mm")?,
            },
            vec![
                ("trial_id".into(), fields.0["trial_id"].get().into()),
                ("mm".into(), fields.0["mm"].get().into()),
            ],
        )),
        "trials" => {
            let trial_fields: RawFields = fields.decode_field("trial")?;
            let command_fields: RawFields = trial_fields.decode_field("command")?;
            check_control_command_fields(&command_fields)?;
            Ok((Task::Trial(fields.decode_field("trial")?), raw("trial")?))
        }
        "recording/start" => Ok((Task::RecordingStart, vec![])),
        "recording/stop" => Ok((Task::RecordingStop, vec![])),
        "stop" => Ok((Task::Stop, vec![])),
        _ => Err("未知控制动作".to_owned().into()),
    }
}
pub(super) fn handles(path: &str) -> bool {
    if path == "/service/v2/control-scopes"
        || path
            .strip_prefix("/service/v2/control-scopes/")
            .is_some_and(|rest| {
                rest.split_once('/')
                    .is_some_and(|(_, action)| matches!(action, "stop" | "cleanup-ids" | "cleanup"))
            })
    {
        return true;
    }
    path.strip_prefix("/service/v2/bindings/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, suffix)| {
            matches!(
                suffix,
                "controls" | "trials" | "stop" | "recording" | "recording/start" | "recording/stop"
            ) || suffix.starts_with("trials/")
        })
}

#[allow(clippy::too_many_arguments)] // 固定请求准入共享唯一 owner 状态。
pub(super) fn route(
    path: &str,
    method: Method,
    fields: &RawFields,
    bindings: &mut integration::IntegrationState,
    common: &mut mutations::State,
    control: &mut State,
    workbench: &mut Workbench,
) -> Result<(u16, Value), ApiError> {
    if path == "/service/v2/control-scopes" && method == Method::Get {
        let scopes: Vec<_> = common.scopes.values().cloned().collect();
        let entries=scopes.iter().map(|scope| {
            let member=&scope.members[0];
            json!({"stop_request_id":common.ledger.stop_id(scope),"scope":{"target_id":member.target_id,"expected_identity":{"uid":member.uid,"run_nonce":member.run_nonce},"connection_generation":member.connection_generation,"control_epoch":scope.control_epoch.to_string()},"current":scope_current(workbench,member),"control_granted":common.book.authorize_stop(member).is_ok()})
        }).collect::<Vec<_>>();
        return Ok((200, json!({"scopes":entries})));
    }
    if let Some((id, action)) = path
        .strip_prefix("/service/v2/control-scopes/")
        .and_then(|rest| rest.split_once('/'))
    {
        if method != Method::Post || id.contains('/') {
            return Err("未知固定范围路径".to_owned().into());
        }
        if action == "cleanup" {
            fields.reject_unknown(&[
                "instance_id",
                "claimant_id",
                "expected_scope",
                "request_id",
                "action",
            ])?;
        } else {
            fields.reject_unknown(&["instance_id", "claimant_id", "expected_scope"])?;
        }
        if fields.string("instance_id")? != common.instance_id {
            return Err(denied("instance_mismatch", "服务实例已改变"));
        }
        let expected: RawFields = fields.decode_field("expected_scope")?;
        expected.reject_unknown(&[
            "target_id",
            "expected_identity",
            "connection_generation",
            "control_epoch",
        ])?;
        let identity: RawFields = expected.decode_field("expected_identity")?;
        identity.reject_unknown(&["uid", "run_nonce"])?;
        let member = MemberKey::new(
            &common.instance_id,
            &expected.string("target_id")?,
            &identity.string("uid")?,
            &identity.string("run_nonce")?,
            &expected.string("connection_generation")?,
        )
        .map_err(book_error)?;
        let scope = common
            .scopes
            .get(&member)
            .cloned()
            .ok_or_else(|| denied("binding_stale", "原固定范围不存在"))?;
        if scope.control_epoch.to_string() != expected.string("control_epoch")?
            || common.ledger.stop_id(&scope) != Some(id)
        {
            return Err(denied("binding_stale", "原Stop编号或代次不一致"));
        }
        let claimant = fields.string("claimant_id")?;
        if action == "cleanup-ids" {
            if !common.control_enabled {
                return Err(denied("control_unavailable", "控制发布门尚未开放"));
            }
            common.book.authorize_stop(&member).map_err(book_error)?;
            if !scope_current(workbench, &member) {
                return Err(denied("binding_stale", "原身份或代次已改变"));
            }
            control
                .cleanup_tickets
                .retain(|_, ticket| ticket.expires > Instant::now());
            if control.cleanup_tickets.len() >= 32 {
                return Err(denied("operation_capacity", "收尾票据容量已满"));
            }
            let grant_id = common
                .book
                .grant(&member)
                .expect("authorized grant")
                .id
                .clone();
            let now = Instant::now();
            let request_id = common.issue_cleanup_ticket(&claimant, scope, now)?;
            control.cleanup_tickets.insert(
                request_id.clone(),
                CleanupTicket {
                    member,
                    grant_id: grant_id.clone(),
                    claimant,
                    expires: now + Duration::from_secs(30),
                },
            );
            return Ok((
                200,
                json!({"request_id":request_id,"expires_in_ms":30000,"grant_id":grant_id}),
            ));
        }
        let (request_id, task) = match action {
            "stop" => (id.to_owned(), Task::Stop),
            "cleanup" => (
                fields.string("request_id")?,
                match fields.string("action")?.as_str() {
                    "heartbeat_stop" => Task::Command(Command::HeartbeatStop),
                    "recording_stop" => Task::RecordingStop,
                    _ => return Err("只允许停止本地心跳或记录".to_owned().into()),
                },
            ),
            _ => return Err("未知原范围动作".to_owned().into()),
        };
        let (new_submission, operation) = control.admit(
            common,
            workbench,
            member,
            scope,
            None,
            &claimant,
            &request_id,
            task,
            vec![],
            None,
        )?;
        return Ok((
            if new_submission { 202 } else { 200 },
            json!({"operation":operation,"new_submission":new_submission}),
        ));
    }
    let (binding_id, suffix) = path
        .strip_prefix("/service/v2/bindings/")
        .and_then(|rest| rest.split_once('/'))
        .ok_or_else(|| "未知控制路径".to_owned())?;
    if method == Method::Get {
        let binding = bindings.checked_current_binding(binding_id, workbench)?;
        let member = member(&common.instance_id, &binding)?;
        if suffix == "recording" {
            let session = workbench
                .integration_session_mut(&member.target_id)
                .ok_or_else(|| denied("binding_stale", "会话不存在"))?;
            return Ok((
                200,
                json!({"native_json_text":serde_json::to_string(&session.recording_snapshot()).map_err(|e|e.to_string())?,"active":session.recording_snapshot()["active"]==true,"failed":session.recording_snapshot()["failed"]==true,"recording_id":session.recording_snapshot()["id"].as_str()}),
            ));
        }
        if let Some(trial_id) = suffix.strip_prefix("trials/") {
            if trial_id.contains('/') || control.trials.get(&member).is_none_or(|id| id != trial_id)
            {
                return Err(denied("trial_stale", "试验编号不一致"));
            }
            let trial = workbench
                .integration_session_mut(&member.target_id)
                .ok_or_else(|| denied("binding_stale", "会话不存在"))?
                .trial_snapshot();
            return Ok((
                200,
                json!({"trial_id":trial_id,"operation":common.projection(trial_id)?,"trial":{"native_json_text":serde_json::to_string(&trial).map_err(|e|e.to_string())?,"state":trial["state"],"stop_observed":trial["stop_observed"]==true,"stop_confirmation_overdue":trial["stop_confirmation_overdue"]==true,"elapsed_ms":trial["elapsed_ms"].as_u64().map(|value|value.to_string())}}),
            ));
        }
        return Err("未知控制查询".to_owned().into());
    }
    if method != Method::Post {
        return Err("不支持控制方法".to_owned().into());
    }
    let binding = bindings.checked_binding(binding_id, fields, workbench)?;
    let member = member(&common.instance_id, &binding)?;
    let mut allowed = vec![
        "instance_id",
        "expected_identity",
        "connection_generation",
        "claimant_id",
        "request_id",
    ];
    let stop = suffix == "stop" || suffix.starts_with("trials/") && suffix.ends_with("/stop");
    if !stop {
        allowed.push("reservation_id");
    }
    let (task, parameters) = if let Some(trial_suffix) = suffix.strip_prefix("trials/") {
        let (trial_id, action) = trial_suffix
            .split_once('/')
            .ok_or_else(|| "试验动作缺失".to_owned())?;
        if control.trials.get(&member).is_none_or(|id| id != trial_id) {
            return Err(denied("trial_stale", "试验编号不一致"));
        }
        match action {
            "stop" => (Task::Stop, vec![]),
            "target" => {
                allowed.push("mm");
                (
                    Task::TrialTarget {
                        trial_id: trial_id.into(),
                        mm: fields.decode_f32_field("mm")?,
                    },
                    vec![
                        (
                            "trial_id".into(),
                            serde_json::to_string(trial_id).map_err(|e| e.to_string())?,
                        ),
                        ("mm".into(), fields.0["mm"].get().into()),
                    ],
                )
            }
            _ => return Err("未知试验动作".to_owned().into()),
        }
    } else {
        match suffix {
            "controls" => allowed.push("command"),
            "trials" => allowed.push("trial"),
            _ => {}
        }
        decode_task(suffix, fields)?
    };
    fields.reject_unknown(&allowed)?;
    let reservation = if stop {
        None
    } else {
        Some(fields.string("reservation_id")?)
    };
    let id = fields.string("request_id")?;
    let scope = common
        .ledger
        .get(&id)
        .map(|record| record.intent.scope.clone())
        .or_else(|| common.scopes.get(&member).cloned())
        .ok_or_else(|| denied("reservation_unknown", "请先申请固定范围占用"))?;
    if scope.members != vec![member.clone()] {
        return Err(denied("binding_stale", "操作范围不同"));
    }
    let (new_submission, operation) = control.admit(
        common,
        workbench,
        member,
        scope,
        reservation,
        &fields.string("claimant_id")?,
        &id,
        task,
        parameters,
        None,
    )?;
    Ok((
        if new_submission { 202 } else { 200 },
        json!({"operation":operation,"new_submission":new_submission}),
    ))
}
pub(super) fn dispatch(
    mut request: tiny_http::Request,
    bindings: &mut integration::IntegrationState,
    workbench: &mut Workbench,
    parsed: Option<Result<RawFields, String>>,
) -> Result<(), String> {
    let path = super::request_path(&request).to_owned();
    let method = super::request_method(&request);
    let fields = if matches!(method, Method::Post | Method::Delete) {
        parsed
            .unwrap_or_else(|| super::parse_json_body(&mut request))
            .and_then(|mut fields| {
                super::v2::validate_version(&mut fields)?;
                Ok(fields)
            })
    } else {
        Ok(RawFields(BTreeMap::new()))
    };
    let mut common = workbench.mutations.take().ok_or("mutation owner missing")?;
    let mut control = workbench.control.take().ok_or("control owner missing")?;
    let mut groups = workbench
        .control_groups
        .take()
        .ok_or("group owner missing")?;
    let result = fields.map_err(ApiError::from).and_then(|fields| {
        if super::control_groups::handles(&path) {
            groups.route(
                &path,
                method,
                &fields,
                bindings,
                &mut common,
                &mut control,
                workbench,
            )
        } else {
            route(
                &path,
                method,
                &fields,
                bindings,
                &mut common,
                &mut control,
                workbench,
            )
        }
    });
    workbench.control_groups = Some(groups);
    workbench.control = Some(control);
    workbench.mutations = Some(common);
    let (status, value) = match result {
        Ok(result) => result,
        Err(error) => (
            error.status,
            json!({"code":error.code,"message":error.message}),
        ),
    };
    let mut envelope = super::v2::envelope(&bindings.instance_id);
    envelope
        .as_object_mut()
        .ok_or("invalid envelope")?
        .extend(value.as_object().ok_or("invalid response")?.clone());
    integration::respond(request, status, envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        service::reservations::Permissions,
        session::{
            ToolSession,
            trial_tests::{FixtureBehavior, FixtureProbe, session_fixture},
        },
    };
    fn fixture() -> (
        Workbench,
        State,
        mutations::State,
        MemberKey,
        OperationScope,
        String,
        FixtureProbe,
    ) {
        let (session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let (uid, run) = session.integration_identity().expect("identity");
        let member = MemberKey::new(
            "test",
            "can:1",
            &uid,
            &run,
            &session.connection_generation().to_string(),
        )
        .expect("member");
        let wb = Workbench::with_test_sessions(ToolSession::new(), BTreeMap::from([(1, session)]));
        let mut common = mutations::State::new("test").expect("common");
        common.control_enabled = true;
        common
            .book
            .set_permissions(
                member.clone(),
                Permissions {
                    control: true,
                    maintenance: false,
                },
            )
            .expect("grant");
        let scope = common.scope(&member).expect("scope");
        let reservation = common
            .book
            .reserve("operator", vec![member.clone()], Access::Control)
            .expect("reserve");
        (
            wb,
            State::new(),
            common,
            member,
            scope,
            reservation.id,
            probe,
        )
    }
    #[test]
    fn stop_fences_queued_target_and_attempts_before_normal_work() {
        let (mut wb, mut control, mut common, member, scope, reservation, probe) = fixture();
        let target = common
            .issue_reservation_ticket(&reservation, "operator", scope.clone(), Instant::now())
            .expect("ticket");
        control
            .admit(
                &mut common,
                &mut wb,
                member.clone(),
                scope.clone(),
                Some(reservation),
                "operator",
                &target,
                Task::Command(Command::Position { mm: 2.0 }),
                vec![(
                    "command".into(),
                    "{\"action\":\"position\",\"mm\":2}".into(),
                )],
                None,
            )
            .expect("target admission");
        let stop = common.ledger.stop_id(&scope).expect("stop").to_owned();
        control
            .admit(
                &mut common,
                &mut wb,
                member.clone(),
                scope.clone(),
                None,
                "operator",
                &stop,
                Task::Stop,
                vec![],
                None,
            )
            .expect("stop admission");
        assert_eq!(
            common.ledger.get(&target).expect("target").submission,
            Submission::NotSubmitted
        );
        control.poll(&mut common, &mut wb);
        assert_eq!(
            common.ledger.get(&stop).expect("stop").admission,
            Admission::Attempting
        );
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        while common.ledger.get(&stop).expect("stop").admission != Admission::Finished
            && Instant::now() < deadline
        {
            control.poll(&mut common, &mut wb);
            std::thread::yield_now();
        }
        assert_eq!(
            common.ledger.get(&stop).expect("stop").submission,
            Submission::LocallySubmitted
        );
        assert_eq!(probe.observed.lock().expect("observed").stops, 1);
        assert!(
            !control
                .admit(
                    &mut common,
                    &mut wb,
                    member,
                    scope,
                    None,
                    "operator",
                    &stop,
                    Task::Stop,
                    vec![],
                    None
                )
                .expect("replay")
                .0
        );
        control.poll(&mut common, &mut wb);
        assert_eq!(probe.observed.lock().expect("observed").stops, 1);
    }
    #[test]
    fn stale_member_and_revoked_reservation_never_start_target() {
        let (mut wb, mut control, mut common, mut member, scope, reservation, probe) = fixture();
        let id = common
            .issue_reservation_ticket(&reservation, "operator", scope.clone(), Instant::now())
            .expect("ticket");
        member.connection_generation = "999".into();
        assert_eq!(
            control
                .admit(
                    &mut common,
                    &mut wb,
                    member,
                    scope,
                    Some(reservation),
                    "operator",
                    &id,
                    Task::Command(Command::Velocity { mm_s: 1.0 }),
                    vec![],
                    None
                )
                .expect_err("stale")
                .code,
            "binding_stale"
        );
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
        let (mut wb, mut control, mut common, member, scope, reservation, probe) = fixture();
        let id = common
            .issue_reservation_ticket(&reservation, "operator", scope.clone(), Instant::now())
            .expect("ticket");
        control
            .admit(
                &mut common,
                &mut wb,
                member,
                scope,
                Some(reservation.clone()),
                "operator",
                &id,
                Task::Command(Command::Velocity { mm_s: 1.0 }),
                vec![],
                None,
            )
            .expect("admit");
        common
            .book
            .release(&reservation, "operator")
            .expect("release");
        control.poll(&mut common, &mut wb);
        assert_eq!(
            common.ledger.get(&id).expect("operation").submission,
            Submission::NotSubmitted
        );
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
        assert_eq!(probe.observed.lock().expect("observed").stops, 0);
    }
    #[test]
    fn explicit_original_scope_stop_survives_observation_binding_absence() {
        let (mut wb, mut control, mut common, member, scope, _reservation, probe) = fixture();
        let mut bindings = integration::IntegrationState::for_test().for_v2();
        let stop = common.ledger.stop_id(&scope).expect("stop").to_owned();
        let fields=RawFields::parse(&json!({"instance_id":"test","claimant_id":"other_operator","expected_scope":{"target_id":member.target_id,"expected_identity":{"uid":member.uid,"run_nonce":member.run_nonce},"connection_generation":member.connection_generation,"control_epoch":scope.control_epoch.to_string()}}).to_string()).expect("fields");
        let result = route(
            &format!("/service/v2/control-scopes/{stop}/stop"),
            Method::Post,
            &fields,
            &mut bindings,
            &mut common,
            &mut control,
            &mut wb,
        )
        .expect("explicit original scope");
        assert_eq!(result.0, 202);
        let mut second = fields;
        second.0.insert(
            "claimant_id".into(),
            serde_json::value::RawValue::from_string("\"second_operator\"".into())
                .expect("claimant raw"),
        );
        let replay = route(
            &format!("/service/v2/control-scopes/{stop}/stop"),
            Method::Post,
            &second,
            &mut bindings,
            &mut common,
            &mut control,
            &mut wb,
        )
        .expect("different claimant canonical replay");
        assert_eq!(replay.0, 200);
        assert_eq!(replay.1["operation"]["claimant_id"], "other_operator");
        control.poll(&mut common, &mut wb);
        assert_eq!(
            common.ledger.get(&stop).expect("attempt").admission,
            Admission::Attempting
        );
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
    }

    #[test]
    fn original_scope_cleanup_after_stop_preserves_unknown_claim_and_rejects_stale_grant() {
        let (mut wb, mut control, mut common, member, scope, reservation, probe) = fixture();
        probe.inject_idle(20);
        let root = std::env::temp_dir().join(format!(
            "eha-control-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let session = wb
            .integration_session_mut(&member.target_id)
            .expect("session");
        session
            .begin_recording_start(&root)
            .expect("begin actual recording");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match session.poll_recording_start() {
                Poll::Ready(result) => {
                    result.expect("actual recording ready");
                    break;
                }
                Poll::Pending => {
                    assert!(Instant::now() < deadline);
                    std::thread::yield_now();
                }
            }
        }
        assert_eq!(session.recording_snapshot()["active"], true);
        let mut bindings = integration::IntegrationState::for_test().for_v2();
        let unknown = common
            .ids
            .issue_normal(scope.clone(), Instant::now())
            .expect("unknown ticket");
        common
            .ledger
            .admit_normal(
                &unknown,
                Intent::new(
                    scope.clone(),
                    Action::Position,
                    Some(reservation.clone()),
                    vec![],
                )
                .expect("intent"),
                "operator",
                &mut common.ids,
                Instant::now(),
            )
            .expect("original admission");
        common
            .book
            .begin_member_pending(&reservation, &unknown, "operator", &member, Access::Control)
            .expect("original pin");
        common.ledger.begin_attempt(&unknown).expect("attempt");
        common
            .ledger
            .finish_attempt(
                &unknown,
                Submission::Unknown,
                Effect::Unresolved,
                Instant::now(),
            )
            .expect("unknown receipt");
        common
            .book
            .finish_pending(
                &reservation,
                &unknown,
                &member,
                EffectDisposition::Unresolved,
            )
            .expect("unknown claim");
        common
            .book
            .release(&reservation, "operator")
            .expect("release keeps unknown");
        let stop = common.ledger.stop_id(&scope).expect("stop").to_owned();
        control
            .admit(
                &mut common,
                &mut wb,
                member.clone(),
                scope.clone(),
                None,
                "operator",
                &stop,
                Task::Stop,
                vec![],
                None,
            )
            .expect("stop");
        let deadline = Instant::now() + Duration::from_secs(1);
        while control.busy() && Instant::now() < deadline {
            control.poll(&mut common, &mut wb);
            std::thread::yield_now();
        }
        assert!(!control.busy());
        let base = json!({"instance_id":"test","claimant_id":"cleanup_operator","expected_scope":{"target_id":member.target_id,"expected_identity":{"uid":member.uid,"run_nonce":member.run_nonce},"connection_generation":member.connection_generation,"control_epoch":scope.control_epoch.to_string()}});
        let prefix = format!("/service/v2/control-scopes/{stop}");
        for action in ["heartbeat_stop", "recording_stop"] {
            let fields = RawFields::parse(&base.to_string()).expect("fields");
            let (_, ticket) = route(
                &format!("{prefix}/cleanup-ids"),
                Method::Post,
                &fields,
                &mut bindings,
                &mut common,
                &mut control,
                &mut wb,
            )
            .expect("cleanup ticket");
            let mut request = base.clone();
            request["request_id"] = ticket["request_id"].clone();
            request["action"] = json!("heartbeat_start");
            let fields = RawFields::parse(&request.to_string()).expect("fields");
            assert!(
                route(
                    &format!("{prefix}/cleanup"),
                    Method::Post,
                    &fields,
                    &mut bindings,
                    &mut common,
                    &mut control,
                    &mut wb
                )
                .is_err()
            );
            request["action"] = json!(action);
            let fields = RawFields::parse(&request.to_string()).expect("fields");
            assert_eq!(
                route(
                    &format!("{prefix}/cleanup"),
                    Method::Post,
                    &fields,
                    &mut bindings,
                    &mut common,
                    &mut control,
                    &mut wb
                )
                .expect("cleanup")
                .0,
                202
            );
            let deadline = Instant::now() + Duration::from_secs(1);
            while control.busy() && Instant::now() < deadline {
                control.poll(&mut common, &mut wb);
                std::thread::yield_now();
            }
            assert!(!control.busy());
            let completed = common
                .ledger
                .get(ticket["request_id"].as_str().expect("cleanup id"))
                .expect("cleanup receipt");
            assert_eq!(completed.admission, Admission::Finished);
            assert_eq!(completed.submission, Submission::NotSubmitted);
            assert_eq!(completed.effect, Effect::NotApplicable);
            let receipt = common
                .receipts
                .get(&completed.request_id)
                .expect("native receipt");
            assert!(receipt.error.is_none());
            assert!(receipt.native.is_some());
            if action == "recording_stop" {
                assert_eq!(
                    wb.integration_session_mut(&member.target_id)
                        .expect("session")
                        .recording_snapshot()["active"],
                    false
                );
            }

            assert_eq!(
                route(
                    &format!("{prefix}/cleanup"),
                    Method::Post,
                    &fields,
                    &mut bindings,
                    &mut common,
                    &mut control,
                    &mut wb
                )
                .expect("same cleanup replay")
                .0,
                200
            );
            assert_eq!(
                common.ledger.get(&unknown).expect("original").effect,
                Effect::Unresolved
            );
            let claim = common
                .book
                .claim(&member.uid)
                .expect("original pin retained");
            assert!(claim.unresolved && claim.released);
            assert_eq!(claim.reservation_id, reservation);
            assert_eq!(claim.claimant, "operator");
            assert_eq!(claim.member, member);
            assert!(claim.pending_operation_id.is_none());
        }
        let fields = RawFields::parse(&base.to_string()).expect("fields");
        let (_, busy_ticket) = route(
            &format!("{prefix}/cleanup-ids"),
            Method::Post,
            &fields,
            &mut bindings,
            &mut common,
            &mut control,
            &mut wb,
        )
        .expect("busy test ticket");
        common.jobs.insert(
            "usb:alias".into(),
            mutations::Job {
                id: unknown.clone(),
                started: true,
            },
        );
        let mut busy_request = base.clone();
        busy_request["request_id"] = busy_ticket["request_id"].clone();
        busy_request["action"] = json!("heartbeat_stop");
        let fields = RawFields::parse(&busy_request.to_string()).expect("fields");
        assert_eq!(
            route(
                &format!("{prefix}/cleanup"),
                Method::Post,
                &fields,
                &mut bindings,
                &mut common,
                &mut control,
                &mut wb
            )
            .expect_err("same UID pending through another target")
            .code,
            "control_busy"
        );
        assert_eq!(common.jobs["usb:alias"].id, unknown);
        common.jobs.remove("usb:alias");
        let fields = RawFields::parse(&base.to_string()).expect("fields");
        let (_, ticket) = route(
            &format!("{prefix}/cleanup-ids"),
            Method::Post,
            &fields,
            &mut bindings,
            &mut common,
            &mut control,
            &mut wb,
        )
        .expect("old grant ticket");
        common
            .book
            .set_permissions(member.clone(), Permissions::default())
            .expect("revoke");
        common
            .book
            .set_permissions(
                member.clone(),
                Permissions {
                    control: true,
                    maintenance: false,
                },
            )
            .expect("new grant");
        let mut request = base;
        request["request_id"] = ticket["request_id"].clone();
        request["action"] = json!("heartbeat_stop");
        let fields = RawFields::parse(&request.to_string()).expect("fields");
        assert_eq!(
            route(
                &format!("{prefix}/cleanup"),
                Method::Post,
                &fields,
                &mut bindings,
                &mut common,
                &mut control,
                &mut wb
            )
            .expect_err("old ticket cannot revive")
            .code,
            "permission_conflict"
        );
        let mut queued_request = request.clone();
        queued_request
            .as_object_mut()
            .expect("object")
            .remove("request_id");
        queued_request
            .as_object_mut()
            .expect("object")
            .remove("action");
        let fields = RawFields::parse(&queued_request.to_string()).expect("fields");
        let (_, ticket) = route(
            &format!("{prefix}/cleanup-ids"),
            Method::Post,
            &fields,
            &mut bindings,
            &mut common,
            &mut control,
            &mut wb,
        )
        .expect("new grant ticket");
        queued_request["request_id"] = ticket["request_id"].clone();
        queued_request["action"] = json!("heartbeat_stop");
        let fields = RawFields::parse(&queued_request.to_string()).expect("fields");
        route(
            &format!("{prefix}/cleanup"),
            Method::Post,
            &fields,
            &mut bindings,
            &mut common,
            &mut control,
            &mut wb,
        )
        .expect("queued cleanup");
        common
            .book
            .set_permissions(member.clone(), Permissions::default())
            .expect("revoke queued cleanup");
        control.poll(&mut common, &mut wb);
        let receipt = common
            .ledger
            .get(ticket["request_id"].as_str().expect("id"))
            .expect("cancelled receipt");
        assert_eq!(receipt.admission, Admission::Finished);
        assert_eq!(receipt.submission, Submission::NotSubmitted);
        assert_eq!(
            common.ledger.get(&unknown).expect("original").effect,
            Effect::Unresolved
        );
        assert_eq!(probe.observed.lock().expect("observed").stops, 1);
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
        std::fs::remove_dir_all(root).expect("remove finished fixture recording");
    }

    #[test]
    fn control_grant_blocks_synchronous_discovery_without_hiding_known_targets() {
        let (mut wb, _control, common, _member, _scope, _reservation, _probe) = fixture();
        wb.mutations = Some(common);
        let (targets, reason) = wb.integration_targets().expect("cached targets");
        assert_eq!(reason, Some("control_authorized_or_pending"));
        assert!(
            targets
                .iter()
                .any(|target| target["target_id"] == "can:1" && target["connected"] == true)
        );
    }
    #[test]
    fn legacy_slow_gets_are_rejected_before_discovery_or_disk()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use std::io::{Read, Write};
        let (mut wb, _control, common, _member, _scope, _reservation, _probe) = fixture();
        wb.mutations = Some(common);
        wb.legacy_routes = true;
        let server = tiny_http::Server::http(("127.0.0.1", 0))?;
        let address = server.server_addr().to_ip().ok_or("address")?;
        let reader = super::super::http_body::BodyReader::new();
        let stop_reader = super::super::stop_body::StopBodyReader::new();
        for path in [
            "/api/adapters",
            "/api/devices",
            "/api/recordings",
            "/api/recordings/run-test/events.jsonl",
        ] {
            let client = std::thread::spawn(move || -> std::io::Result<String> {
                let mut stream = std::net::TcpStream::connect(address)?;
                stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
                write!(
                    stream,
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                    address.port()
                )?;
                let mut response = String::new();
                stream.read_to_string(&mut response)?;
                Ok(response)
            });
            super::super::handle_received_request(
                server.recv()?,
                address.port(),
                &mut wb,
                &mut super::super::OdriveView::new(None),
                &mut super::super::CanScanView::new(),
                &reader,
                &stop_reader,
            )?;
            let response = client.join().map_err(|_| "client panic")??;
            assert!(response.starts_with("HTTP/1.1 409"), "{response}");
            assert!(response.contains("control_busy"));
        }
        Ok(())
    }

    #[test]
    fn manual_stop_adopts_queued_or_completed_autonomous_stop_without_resending() {
        for outcome in ["queued", "submitted", "unknown"] {
            let (mut session, commands, sink) = ToolSession::test_maintenance_fixture();
            let (uid, run) = session.integration_identity().expect("identity");
            let member = MemberKey::new(
                "test",
                "can:1",
                &uid,
                &run,
                &session.connection_generation().to_string(),
            )
            .expect("member");
            crate::session::trial_tests::begin_autonomous_stop_fixture(&mut session);
            let first = commands
                .recv_timeout(Duration::from_secs(1))
                .expect("autonomous Stop reaches backend");
            let (pump_tx, pump_rx) = std::sync::mpsc::channel();
            pump_tx.send(first).expect("forward to actual Pump");
            let mut pump = eha_sdk::host::backend::Pump::new(pump_rx, sink.clone());
            assert!(pump.poll());
            let mut taken = None;
            if outcome != "queued" {
                let request = (0..3)
                    .find_map(|lane| pump.take(lane))
                    .expect("one Stop handed off");
                if outcome == "submitted" {
                    sink.submitted(request.id);
                } else {
                    sink.failed(request.id, "synthetic partial Stop", true);
                }
                taken = Some(request);
            }
            let mut wb =
                Workbench::with_test_sessions(ToolSession::new(), BTreeMap::from([(1, session)]));
            let mut common = mutations::State::new("test").expect("state");
            common.control_enabled = true;
            common
                .book
                .set_permissions(
                    member.clone(),
                    Permissions {
                        control: true,
                        maintenance: false,
                    },
                )
                .expect("grant");
            let scope = common.scope(&member).expect("scope");
            let id = common.ledger.stop_id(&scope).expect("stop id").to_owned();
            let mut control = State::new();
            control
                .admit(
                    &mut common,
                    &mut wb,
                    member,
                    scope,
                    None,
                    "manual",
                    &id,
                    Task::Stop,
                    vec![],
                    None,
                )
                .expect("manual admitted");
            // 即使在准入与规范 begin 之间发生 tick，也不能抢走其回执。
            wb.tick();
            let request = taken.unwrap_or_else(|| {
                (0..3)
                    .find_map(|lane| pump.take(lane))
                    .expect("queued Stop preserved")
            });
            assert!(!request.cancelled.load(std::sync::atomic::Ordering::Acquire));
            if outcome == "queued" {
                sink.submitted(request.id);
            }
            wb.tick();
            control.poll(&mut common, &mut wb);
            control.poll(&mut common, &mut wb);
            let record = common.ledger.get(&id).expect("canonical receipt");
            assert_eq!(record.admission, Admission::Finished);
            assert_eq!(
                record.submission,
                if outcome == "unknown" {
                    Submission::Unknown
                } else {
                    Submission::LocallySubmitted
                }
            );
            wb.tick();
            assert!(pump.poll());
            assert!(
                (0..3).all(|lane| pump.take(lane).is_none()),
                "canonical must not enqueue another Stop"
            );
            assert!(
                commands.try_recv().is_err(),
                "no replacement command queued"
            );
        }
    }

    #[test]
    fn stop_during_recording_finish_transfers_completion_without_waiting() {
        let (mut wb, mut control, mut common, member, scope, reservation, probe) = fixture();
        let root =
            std::env::temp_dir().join(format!("eha-owner-cancel-finish-{}", std::process::id()));
        let (entered, release) = wb
            .integration_session_mut("can:1")
            .expect("session")
            .test_install_stalled_recording(&root);
        let id = common
            .issue_reservation_ticket(&reservation, "operator", scope.clone(), Instant::now())
            .expect("ticket");
        control
            .admit(
                &mut common,
                &mut wb,
                member.clone(),
                scope.clone(),
                Some(reservation),
                "operator",
                &id,
                Task::RecordingStop,
                vec![],
                None,
            )
            .expect("finish admitted");
        control.poll(&mut common, &mut wb);
        entered
            .recv_timeout(Duration::from_secs(1))
            .expect("finish stalled");
        let stop = common.ledger.stop_id(&scope).expect("Stop id").to_owned();
        control
            .admit(
                &mut common,
                &mut wb,
                member,
                scope,
                None,
                "operator",
                &stop,
                Task::Stop,
                vec![],
                None,
            )
            .expect("Stop admitted");
        assert_eq!(
            common.ledger.get(&id).expect("cancelled finish").admission,
            Admission::Finished
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while control.busy() {
            control.poll(&mut common, &mut wb);
            assert!(Instant::now() < deadline, "Stop must not wait for disk");
            std::thread::yield_now();
        }
        assert_eq!(probe.observed.lock().expect("observed").stops, 1);
        let session = wb.integration_session_mut("can:1").expect("session");
        assert_eq!(session.recording_snapshot()["pending_finish"], true);
        assert!(
            session.begin_recording_start(&root).is_err(),
            "pending physical finish still owns recording slot"
        );
        release.send(()).expect("release disk");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            wb.tick();
            let snapshot = wb
                .integration_session_mut("can:1")
                .expect("session")
                .recording_snapshot();
            if snapshot["pending_finish"] != true {
                break;
            }
            assert!(Instant::now() < deadline, "finish completion");
            std::thread::yield_now();
        }
        wb.integration_session_mut("can:1")
            .expect("session")
            .begin_recording_start(&root)
            .expect("background owner consumed actual finish terminal");
        drop(wb);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    #[allow(clippy::panic)] // 此夹具解码固定的 Trial 请求。
    fn releasing_prepared_trial_cancels_before_any_target() {
        let (mut wb, mut control, mut common, member, scope, reservation, probe) = fixture();
        let id = common
            .issue_reservation_ticket(&reservation, "operator", scope.clone(), Instant::now())
            .expect("ticket");
        let fields=RawFields::parse(r#"{"trial":{"command":{"action":"position","mm":1},"envelope":{"position_min_mm":-10,"position_max_mm":10,"velocity_abs_max_mm_s":2,"force_abs_max_n":10,"stiffness_max_n_per_mm":10,"damping_max_ns_per_mm":10,"duration_max_s":5},"duration_s":5}}"#).expect("trial JSON");
        let (task, params) = decode_task("trials", &fields).expect("trial");
        control
            .admit(
                &mut common,
                &mut wb,
                member,
                scope,
                Some(reservation.clone()),
                "operator",
                &id,
                task,
                params,
                Some("group-barrier".into()),
            )
            .expect("admit");
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        while !control.trial_prepared(&id) && Instant::now() < deadline {
            control.poll(&mut common, &mut wb);
            wb.tick();
            std::thread::yield_now();
        }
        assert!(control.trial_prepared(&id));
        control.release_trial(&id);
        common
            .book
            .release(&reservation, "operator")
            .expect("release");
        control.poll(&mut common, &mut wb);
        assert_eq!(
            common.ledger.get(&id).expect("receipt").submission,
            Submission::NotSubmitted
        );
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
        assert_eq!(probe.observed.lock().expect("observed").stops, 0);
        let (Task::Trial(request), _) = decode_task("trials", &fields).expect("same request")
        else {
            panic!("trial");
        };
        wb.integration_session_mut("can:1")
            .expect("original session")
            .begin_trial_preflight(request)
            .expect("cancelled Prepared job must consume its session audit receipt");
    }

    #[test]
    #[allow(clippy::panic)]
    fn trial_decoder_rejects_nested_extra_fields_without_changing_numeric_tokens() {
        let wrap = |command: &str| {
            RawFields::parse(&format!(
                r#"{{"trial":{{"command":{command},"envelope":{{"position_min_mm":-10,"position_max_mm":10,"velocity_abs_max_mm_s":2,"force_abs_max_n":10,"stiffness_max_n_per_mm":10,"damping_max_ns_per_mm":10,"duration_max_s":5}},"duration_s":1}}}}"#
            ))
            .expect("固定试验原文")
        };
        for command in [
            r#"{"action":"position","mm":1.0000000596046447753906250000000000000001}"#,
            r#"{"action":"velocity","mm_s":0.5}"#,
            r#"{"action":"force","n":1}"#,
            r#"{"action":"impedance","equilibrium_mm":0,"stiffness_n_per_mm":1,"damping_ns_per_mm":1}"#,
        ] {
            let fields = wrap(command);
            let (Task::Trial(trial), parameters) =
                decode_task("trials", &fields).expect("固定动作")
            else {
                panic!("必须是试验");
            };
            assert_eq!(parameters[0].1, fields.0["trial"].get());
            if let Command::Position { mm } = trial.command {
                assert_eq!(mm.to_bits(), 0x3f80_0001);
            }
            let extra = format!(
                "{},\"unexpected_control_override\":true}}",
                command.strip_suffix('}').expect("动作对象")
            );
            assert!(decode_task("trials", &wrap(&extra)).is_err());
        }
    }

    #[test]
    fn fixed_decoder_retains_exact_numeric_text_and_rejects_extra_fields() {
        let fields =
            RawFields::parse(r#"{"command":{"action":"position","mm":1.000000000000000000001}}"#)
                .expect("raw");
        let (_, parameters) = decode_task("controls", &fields).expect("decode");
        assert_eq!(
            parameters[0].1,
            r#"{"action":"position","mm":1.000000000000000000001}"#
        );
        for raw in [
            r#"{"command":{"action":"position","mm":1,"extra":2}}"#,
            r#"{"command":{"action":"stop"}}"#,
            r#"{"command":{"action":"position","mm":"1.0"}}"#,
        ] {
            assert!(decode_task("controls", &RawFields::parse(raw).expect("json")).is_err());
        }
    }
}
