// Copyright The eha-sdk Contributors
//! P3/P4 共享唯一的实例自有变更身份、预留与操作账本。
use super::maintenance::{
    ApiError, MAX_OPERATIONS, MAX_SCOPES, TTL, action_name, book_error, command, denied,
    ledger_error, local_error, member_json, scope_current, unknown_error,
};
use super::{
    Workbench,
    operations::{
        Action, Admission, Admitted, Effect, Intent, Ledger, OperationScope, RequestIds, Submission,
    },
    reservations::{Access, EffectDisposition, MemberKey, Permissions, ReservationBook},
};
use crate::session::{MaintenanceCompletion, MaintenancePhase};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    task::Poll,
    time::Instant,
};
pub(super) struct State {
    pub(super) instance_id: String,
    pub(super) book: ReservationBook,
    pub(super) ids: RequestIds,
    pub(super) ledger: Ledger,
    pub(super) scopes: BTreeMap<MemberKey, OperationScope>,
    pub(super) reservations: BTreeMap<String, Held>,
    pub(super) tickets: BTreeMap<String, Ticket>,
    pub(super) receipts: BTreeMap<String, Receipt>,
    pub(super) jobs: BTreeMap<String, Job>,
    next_epoch: u64,
    pub(super) control_enabled: bool,
    pub(super) control_facts: BTreeMap<String, Value>,
    pub(super) reconnect_required: BTreeSet<MemberKey>,
    readbacks: BTreeMap<String, Readback>,
    readback_reserved: BTreeSet<String>,
}
struct Readback {
    original: MemberKey,
    current: Option<MemberKey>,
    user: bool,
    startup: bool,
}
pub(super) struct Held {
    pub(super) scope: OperationScope,
}
enum TicketPurpose {
    Reservation {
        reservation_id: String,
        access: Access,
    },
    LocalCleanup,
}
pub(super) struct Ticket {
    purpose: TicketPurpose,
    scope: OperationScope,
    claimant: String,
    grant_id: String,
    expires: Instant,
}
impl Ticket {
    fn reservation_id(&self) -> Option<&str> {
        match &self.purpose {
            TicketPurpose::Reservation { reservation_id, .. } => Some(reservation_id),
            TicketPurpose::LocalCleanup => None,
        }
    }
    fn current(&self, book: &ReservationBook) -> bool {
        let member = &self.scope.members[0];
        if !book
            .grant(member)
            .is_some_and(|grant| grant.id == self.grant_id)
        {
            return false;
        }
        match &self.purpose {
            TicketPurpose::Reservation {
                reservation_id,
                access,
            } => book.reservation(reservation_id).is_some_and(|reservation| {
                reservation.claimant == self.claimant
                    && reservation.access == *access
                    && reservation.members.contains(member)
            }),
            TicketPurpose::LocalCleanup => book
                .grant(member)
                .is_some_and(|grant| grant.permissions.control),
        }
    }
}
pub(super) struct Job {
    pub(super) id: String,
    pub(super) started: bool,
}
pub(super) struct Receipt {
    pub(super) phase: Option<MaintenancePhase>,
    pub(super) native: Option<String>,
    pub(super) error: Option<Value>,
}

impl State {
    pub(super) fn new(instance_id: &str) -> Result<Self, String> {
        Ok(Self {
            instance_id: instance_id.into(),
            book: ReservationBook::new(instance_id, MAX_SCOPES, MAX_SCOPES, MAX_SCOPES)
                .map_err(|e| format!("{e:?}"))?,
            ids: RequestIds::new(instance_id, MAX_OPERATIONS, MAX_SCOPES, MAX_SCOPES, TTL)
                .map_err(|e| format!("{e:?}"))?,
            ledger: Ledger::new(instance_id, MAX_OPERATIONS, MAX_SCOPES, TTL)
                .map_err(|e| format!("{e:?}"))?,
            scopes: BTreeMap::new(),
            reservations: BTreeMap::new(),
            tickets: BTreeMap::new(),
            receipts: BTreeMap::new(),
            jobs: BTreeMap::new(),
            next_epoch: 1,
            control_enabled: super::control::RELEASED,
            control_facts: BTreeMap::new(),
            reconnect_required: BTreeSet::new(),
            readbacks: BTreeMap::new(),
            readback_reserved: BTreeSet::new(),
        })
    }
    pub(super) fn issue_reservation_ticket(
        &mut self,
        reservation_id: &str,
        claimant: &str,
        scope: OperationScope,
        now: Instant,
    ) -> Result<String, ApiError> {
        if scope.members.len() != 1 {
            return Err(denied("bad_request", "票据必须固定一名原成员"));
        }
        let member = &scope.members[0];
        let access = self
            .book
            .reservation(reservation_id)
            .ok_or_else(|| denied("reservation_unknown", "原占用不存在"))?
            .access;
        self.book
            .authorize_member(reservation_id, claimant, member, access)
            .map_err(book_error)?;
        let grant_id = self
            .book
            .grant(member)
            .expect("authorized grant")
            .id
            .clone();
        self.issue_ticket(
            scope,
            claimant,
            grant_id,
            TicketPurpose::Reservation {
                reservation_id: reservation_id.into(),
                access,
            },
            now,
        )
    }
    pub(super) fn issue_cleanup_ticket(
        &mut self,
        claimant: &str,
        scope: OperationScope,
        now: Instant,
    ) -> Result<String, ApiError> {
        if scope.members.len() != 1 || claimant.is_empty() || claimant.len() > 256 {
            return Err(denied("bad_request", "收尾票据必须固定成员和请求会话"));
        }
        let member = &scope.members[0];
        self.book.authorize_stop(member).map_err(book_error)?;
        let grant_id = self
            .book
            .grant(member)
            .expect("authorized grant")
            .id
            .clone();
        self.issue_ticket(scope, claimant, grant_id, TicketPurpose::LocalCleanup, now)
    }
    fn issue_ticket(
        &mut self,
        scope: OperationScope,
        claimant: &str,
        grant_id: String,
        purpose: TicketPurpose,
        now: Instant,
    ) -> Result<String, ApiError> {
        let expired: Vec<_> = self
            .tickets
            .iter()
            .filter(|(_, ticket)| ticket.expires <= now || !ticket.current(&self.book))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let _ = self.ids.invalidate(&id);
            self.tickets.remove(&id);
        }
        let expires = now
            .checked_add(TTL)
            .ok_or_else(|| denied("bad_request", "票据期限无效"))?;
        let id = self
            .ids
            .issue_normal(scope.clone(), now)
            .map_err(ledger_error)?;
        self.tickets.insert(
            id.clone(),
            Ticket {
                purpose,
                scope,
                claimant: claimant.into(),
                grant_id,
                expires,
            },
        );
        Ok(id)
    }
    pub(super) fn admit_normal(
        &mut self,
        id: &str,
        intent: Intent,
        claimant: &str,
        now: Instant,
    ) -> Result<Admitted, ApiError> {
        if let Some(record) = self.ledger.get(id) {
            if record.intent != intent || record.provenance != claimant {
                return Err(denied("request_id_conflict", "编号原文或请求会话不一致"));
            }
            return self
                .ledger
                .admit_normal(id, intent, claimant, &mut self.ids, now)
                .map_err(ledger_error);
        }
        let ticket = self
            .tickets
            .get(id)
            .ok_or_else(|| denied("request_id_expired", "缺少原服务票据"))?;
        if ticket.expires <= now || !ticket.current(&self.book) {
            let _ = self.ids.invalidate(id);
            self.tickets.remove(id);
            return Err(denied("request_id_expired", "原票据授权、占用或期限已失效"));
        }
        if ticket.scope != intent.scope || ticket.claimant != claimant {
            return Err(denied(
                "request_id_conflict",
                "票据原成员、代次或请求会话不一致",
            ));
        }
        let member = &intent.scope.members[0];
        match &ticket.purpose {
            TicketPurpose::Reservation {
                reservation_id,
                access,
            } => {
                let maintenance = matches!(
                    intent.action,
                    Action::SaveAndRestart
                        | Action::RestoreFactoryAndRestart
                        | Action::ResetApplication
                        | Action::EnterUpdate
                );
                if intent.reservation_id.as_deref() != Some(reservation_id.as_str())
                    || maintenance != (*access == Access::Maintenance)
                    || matches!(intent.action, Action::Stop | Action::TrialStop)
                {
                    return Err(denied("request_id_conflict", "票据用途或原占用不一致"));
                }
                self.book
                    .authorize_member(reservation_id, claimant, member, *access)
                    .map_err(book_error)?;
            }
            TicketPurpose::LocalCleanup => {
                if intent.reservation_id.is_some()
                    || !matches!(intent.action, Action::HeartbeatStop | Action::RecordingStop)
                {
                    return Err(denied(
                        "request_id_conflict",
                        "本地收尾票据不可用于其他动作",
                    ));
                }
                self.book.authorize_stop(member).map_err(book_error)?;
            }
        }
        let admitted = self
            .ledger
            .admit_normal(id, intent, claimant, &mut self.ids, now)
            .map_err(ledger_error);
        self.tickets.remove(id);
        admitted
    }
    pub(super) fn readback_required(&self, member: &MemberKey) -> bool {
        self.readbacks.contains_key(&member.uid)
    }
    pub(super) fn reserve_readback(&mut self, member: &MemberKey) -> Result<(), ApiError> {
        if self.readbacks.contains_key(&member.uid) || self.readback_reserved.contains(&member.uid)
        {
            return Err(denied("maintenance_busy", "原维护读回义务或预留仍在"));
        }
        if self.readbacks.len() + self.readback_reserved.len() >= MAX_SCOPES {
            return Err(denied("operation_capacity", "维护读回义务容量已满"));
        }
        self.readback_reserved.insert(member.uid.clone());
        Ok(())
    }
    pub(super) fn release_readback_reservation(&mut self, member: &MemberKey) {
        self.readback_reserved.remove(&member.uid);
    }

    pub(super) fn note_config_read(&mut self, member: &MemberKey, view: &str) {
        let Some(obligation) = self.readbacks.get_mut(&member.uid) else {
            return;
        };
        if member.instance_id != obligation.original.instance_id
            || member.target_id != obligation.original.target_id
            || member.connection_generation == obligation.original.connection_generation
            || !matches!(view, "user" | "startup")
        {
            return;
        }
        if obligation.current.as_ref() != Some(member) {
            obligation.current = Some(member.clone());
            obligation.user = false;
            obligation.startup = false;
        }
        if view == "user" {
            obligation.user = true;
        } else {
            obligation.startup = true;
        }
        if obligation.user && obligation.startup {
            self.readbacks.remove(&member.uid);
        }
    }

    pub(super) fn scope(&mut self, member: &MemberKey) -> Result<OperationScope, ApiError> {
        if let Some(scope) = self.scopes.get(member).cloned() {
            if !self.ledger.stop_fenced(&scope) {
                return Ok(scope);
            }
            // 仅此显式新范围请求可退役已知的旧 Stop 代次。
            self.ledger.prune(Instant::now());
            self.ledger
                .retire_stop_scope(&scope, &mut self.ids)
                .map_err(ledger_error)?;
            self.scopes.remove(member);
        }
        if self.scopes.len() >= MAX_SCOPES {
            return Err(denied("operation_capacity", "固定执行范围容量已满"));
        }
        let next = self
            .next_epoch
            .checked_add(1)
            .ok_or_else(|| denied("operation_capacity", "执行代次耗尽"))?;
        let scope =
            OperationScope::new(vec![member.clone()], self.next_epoch).map_err(ledger_error)?;
        self.ledger
            .reserve_stop_scope(scope.clone(), &mut self.ids)
            .map_err(ledger_error)?;
        self.next_epoch = next;
        self.scopes.insert(member.clone(), scope.clone());
        Ok(scope)
    }
    pub(super) fn invalidate_reservation_tickets(&mut self, reservation_id: &str) {
        let ids: Vec<_> = self
            .tickets
            .iter()
            .filter(|(_, ticket)| ticket.reservation_id() == Some(reservation_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let _ = self.ids.invalidate(&id);
            self.tickets.remove(&id);
        }
    }
    pub(super) fn revoke(
        &mut self,
        member: &MemberKey,
        workbench: &mut Workbench,
    ) -> Result<(), ApiError> {
        self.book
            .set_permissions(member.clone(), Permissions::default())
            .map_err(book_error)?;
        self.retire_member_resources(member, workbench);
        Ok(())
    }
    pub(super) fn retire_member_resources(
        &mut self,
        member: &MemberKey,
        workbench: &mut Workbench,
    ) {
        let retired: Vec<_> = self
            .reservations
            .iter()
            .filter(|(_, held)| held.scope.members.contains(member))
            .map(|(id, _)| id.clone())
            .collect();
        for id in retired {
            self.invalidate_reservation_tickets(&id);
            self.reservations.remove(&id);
        }
        // 即使没有单成员 Held，群组预留仍位于同一 Book 中。
        // 撤销退役完整的原群组；立即作废全部孤立票据。
        let orphaned: Vec<_> = self
            .tickets
            .iter()
            .filter(|(_, ticket)| !ticket.current(&self.book))
            .map(|(id, _)| id.clone())
            .collect();
        for id in orphaned {
            let _ = self.ids.invalidate(&id);
            self.tickets.remove(&id);
        }
        if let Some(job) = self.jobs.remove(&member.target_id) {
            if !job.started {
                if self.release_cancelled_readback(&job) {
                    return;
                }
                if self.ledger.cancel_queued(&job.id, Instant::now()).is_ok() {
                    self.release_readback_reservation(member);
                    if let Some(record) = self.ledger.get(&job.id)
                        && let Some(id) = record.intent.reservation_id.as_deref()
                    {
                        let _ = self.book.finish_pending(
                            id,
                            &job.id,
                            member,
                            EffectDisposition::NotSubmitted,
                        );
                    }
                    if let Some(receipt) = self.receipts.get_mut(&job.id) {
                        receipt.error = Some(
                            json!({"code":"permission_denied","message":"原授权已撤销；未提交"}),
                        );
                    }
                }
            } else {
                if let Some(session) = workbench.integration_session_mut(&member.target_id) {
                    session.cancel_maintenance("permission_revoked");
                }
                // 保留原成员范围，仅领取其原完成结果。
                self.jobs.insert(member.target_id.clone(), job);
            }
        }
    }
    fn release_cancelled_readback(&mut self, job: &Job) -> bool {
        let Some(record) = self.ledger.get(&job.id) else {
            return false;
        };
        if job.started
            || record.admission != Admission::Finished
            || record.submission != Submission::NotSubmitted
            || record.effect != Effect::NotAttempted
            || !matches!(
                record.intent.action,
                Action::SaveAndRestart
                    | Action::RestoreFactoryAndRestart
                    | Action::ResetApplication
                    | Action::EnterUpdate
            )
        {
            return false;
        }
        let member = &record.intent.scope.members[0];
        if self.book.claim(&member.uid).is_some_and(|claim| {
            claim.member != *member
                || record.intent.reservation_id.as_deref() != Some(claim.reservation_id.as_str())
                || claim
                    .pending_operation_id
                    .as_deref()
                    .is_some_and(|id| id != job.id)
        }) {
            return false;
        }
        // 仅回收确实未开始且账本已确认未提交的未来配额；真实读回义务及未知保持。
        let member = member.clone();
        self.release_readback_reservation(&member);
        true
    }
    pub(super) fn poll(&mut self, workbench: &mut Workbench) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .tickets
            .iter()
            .filter(|(_, ticket)| ticket.expires <= now || !ticket.current(&self.book))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let _ = self.ids.invalidate(&id);
            self.tickets.remove(&id);
        }
        self.ledger.prune(now);
        self.receipts.retain(|id, _| self.ledger.get(id).is_some());
        self.control_facts
            .retain(|id, _| self.ledger.get(id).is_some());
        for member in self.book.grant_members() {
            if !scope_current(workbench, &member) {
                let _ = self.revoke(&member, workbench);
            }
        }
        let targets: Vec<_> = self.jobs.keys().cloned().collect();
        for target in targets {
            let Some(mut job) = self.jobs.remove(&target) else {
                continue;
            };
            let Some(record) = self.ledger.get(&job.id).cloned() else {
                continue;
            };
            let member = &record.intent.scope.members[0];
            if !job.started {
                if self.release_cancelled_readback(&job) {
                    continue;
                }
                if record
                    .intent
                    .reservation_id
                    .as_ref()
                    .is_none_or(|id| self.book.reservation(id).is_none())
                {
                    if self.ledger.cancel_queued(&job.id, now).is_ok() {
                        self.release_readback_reservation(member);
                        if let Some(id) = record.intent.reservation_id.as_deref() {
                            let _ = self.book.finish_pending(
                                id,
                                &job.id,
                                member,
                                EffectDisposition::NotSubmitted,
                            );
                        }
                        if let Some(receipt) = self.receipts.get_mut(&job.id) {
                            receipt.error = Some(
                                json!({"code":"reservation_unknown","message":"原占用已释放；未提交"}),
                            );
                        }
                    }
                    continue;
                }
                if self.ledger.begin_attempt(&job.id).is_err() {
                    continue;
                }
                let global_blocker = workbench.integration_global_read_blocker();
                let result = if let Some(reason) = global_blocker {
                    Err(local_error(&format!("维护提交前全局门禁改变：{reason}")))
                } else if !scope_current(workbench, member) {
                    Err(local_error("固定维护范围已失效"))
                } else {
                    workbench
                        .integration_session_mut(&target)
                        .ok_or_else(|| local_error("固定维护会话不存在"))
                        .and_then(|session| {
                            command(&record.intent)
                                .map_err(|error| local_error(&error.message))
                                .and_then(|command| session.begin_maintenance(command))
                        })
                };
                if let Err(error) = result {
                    self.complete(
                        &job.id,
                        MaintenanceCompletion {
                            phase: MaintenancePhase::Submission,
                            result: Err(error),
                        },
                        workbench,
                    );
                    continue;
                }
                job.started = true;
                self.jobs.insert(target, job);
                continue;
            }
            let global_blocker = workbench.integration_global_read_blocker();
            let completion = match workbench.integration_session_mut(&target) {
                Some(session) => {
                    if let Some(reason) = global_blocker {
                        session.cancel_maintenance(reason);
                    }
                    session.poll_maintenance()
                }
                None => Poll::Ready(MaintenanceCompletion {
                    phase: MaintenancePhase::Submission,
                    result: Err(unknown_error("原维护会话已关闭")),
                }),
            };
            match completion {
                Poll::Pending => {
                    self.jobs.insert(target, job);
                }
                Poll::Ready(completion) => self.complete(&job.id, completion, workbench),
            }
        }
        // 已知过时的范围可以退役。审计、声明或 Stop 结果未知的范围继续固定保留。
        let obsolete: Vec<_> = self
            .scopes
            .keys()
            .filter(|member| !scope_current(workbench, member))
            .cloned()
            .collect();
        for member in obsolete {
            if let Some(scope) = self.scopes.get(&member)
                && self.ledger.retire_stop_scope(scope, &mut self.ids).is_ok()
            {
                self.scopes.remove(&member);
                self.reconnect_required.remove(&member);
            }
        }
    }
    fn complete(&mut self, id: &str, completion: MaintenanceCompletion, workbench: &mut Workbench) {
        let Some(record) = self.ledger.get(id).cloned() else {
            return;
        };
        let (submission, effect, disposition, native, error) = match &completion.result {
            Ok(result) => (
                Submission::LocallySubmitted,
                Effect::NotApplicable,
                EffectDisposition::NotApplicable,
                serde_json::to_string(result).ok(),
                None,
            ),
            Err(error) => {
                let unknown = completion.phase == MaintenancePhase::Submission
                    && (error.unknown || error.local_stage.as_deref() != Some("not_submitted"));
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
                    serde_json::to_string(error).ok(),
                    Some(json!({"code":error.kind,"message":error.message})),
                )
            }
        };
        // 仅在此精确操作转换成功后才允许更新 Book。
        if self
            .ledger
            .finish_attempt(id, submission, effect, Instant::now())
            .is_err()
        {
            return;
        }
        self.release_readback_reservation(&record.intent.scope.members[0]);
        if let Some(reservation_id) = record.intent.reservation_id.as_deref() {
            let _ = self.book.finish_pending(
                reservation_id,
                id,
                &record.intent.scope.members[0],
                disposition,
            );
        }
        self.receipts.insert(
            id.into(),
            Receipt {
                phase: Some(completion.phase),
                native,
                error,
            },
        );
        if submission == Submission::LocallySubmitted {
            let member = &record.intent.scope.members[0];
            self.reconnect_required.insert(member.clone());
            self.readbacks.insert(
                member.uid.clone(),
                Readback {
                    original: member.clone(),
                    current: None,
                    user: false,
                    startup: false,
                },
            );
            let _ = self.revoke(member, workbench);
        }
    }
    pub(super) fn projection(&self, id: &str) -> Result<Value, ApiError> {
        let record = self.ledger.get(id).ok_or_else(|| {
            denied(
                "operation_not_found",
                "操作回执不存在或已过保留期；旧编号不会再执行",
            )
        })?;
        let receipt = self.receipts.get(id);
        let mut value = json!({"request_id":id,"reservation_id":record.intent.reservation_id,"claimant_id":record.provenance,"action":action_name(record.intent.action),"control_epoch":record.intent.scope.control_epoch.to_string(),"members":record.intent.scope.members.iter().map(member_json).collect::<Vec<_>>(),"admission":match record.admission {Admission::Queued=>"queued",Admission::Attempting=>"attempting",Admission::Finished=>"finished"},"submission":match record.submission {Submission::NotAttempted=>"not_attempted",Submission::NotSubmitted=>"not_submitted",Submission::LocallySubmitted=>"locally_submitted",Submission::Unknown=>"unknown"},"control_effect":match record.effect {Effect::NotAttempted=>"not_attempted",Effect::NotApplicable=>"not_applicable",Effect::Known=>"known",Effect::Unresolved=>"unresolved"},"observation":"unconfirmed","phase":receipt.and_then(|receipt|receipt.phase).map(|phase|match phase {MaintenancePhase::FactoryRead=>"factory_read",MaintenancePhase::Submission=>"submission"}),"native_json_text":receipt.and_then(|receipt|receipt.native.as_ref()),"error":receipt.and_then(|receipt|receipt.error.as_ref()),"follow_up":{"reconnect_required":matches!(record.intent.action, super::operations::Action::SaveAndRestart|super::operations::Action::RestoreFactoryAndRestart|super::operations::Action::ResetApplication|super::operations::Action::EnterUpdate) && record.submission==Submission::LocallySubmitted,"readback_views":if matches!(record.intent.action, super::operations::Action::SaveAndRestart|super::operations::Action::RestoreFactoryAndRestart|super::operations::Action::ResetApplication|super::operations::Action::EnterUpdate) {vec!["user","startup"]}else{vec![]}}});
        if let Some(facts) = self.control_facts.get(id).and_then(Value::as_object) {
            for key in ["phase", "stop_attempt_delay_ms", "trial_id", "recording_id"] {
                if let Some(fact) = facts.get(key) {
                    value[key] = fact.clone();
                }
            }
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ticket_fixture(access: Access) -> (State, OperationScope, String) {
        let mut state = State::new("instance").expect("state");
        let member = MemberKey::new("instance", "usb:a", "uid-a", "nonce", "1").expect("member");
        state
            .book
            .set_permissions(
                member.clone(),
                Permissions {
                    maintenance: true,
                    control: true,
                },
            )
            .expect("grant");
        let scope = state.scope(&member).expect("reserved scope");
        let reservation = state
            .book
            .reserve("operator", vec![member], access)
            .expect("reservation");
        (state, scope, reservation.id)
    }
    #[test]
    fn shared_admission_checks_ticket_purpose_claimant_reservation_and_every_scope_field() {
        for access in [Access::Maintenance, Access::Control] {
            let (mut state, scope, reservation) = ticket_fixture(access);
            let now = Instant::now();
            let id = state
                .issue_reservation_ticket(&reservation, "operator", scope.clone(), now)
                .expect("ticket");
            let action = if access == Access::Maintenance {
                Action::ResetApplication
            } else {
                Action::Position
            };
            let original = Intent::new(scope.clone(), action, Some(reservation.clone()), vec![])
                .expect("intent");
            assert_eq!(
                state
                    .admit_normal(&id, original.clone(), "other", now)
                    .expect_err("claimant")
                    .code,
                "request_id_conflict"
            );
            for field in 0..6 {
                let mut altered = original.clone();
                match field {
                    0 => altered.scope.members[0].instance_id = "other".into(),
                    1 => altered.scope.members[0].target_id = "can:2".into(),
                    2 => altered.scope.members[0].uid = "other-uid".into(),
                    3 => altered.scope.members[0].run_nonce = "other-nonce".into(),
                    4 => altered.scope.members[0].connection_generation = "2".into(),
                    _ => altered.scope.control_epoch += 1,
                }
                assert_eq!(
                    state
                        .admit_normal(&id, altered, "operator", now)
                        .expect_err("fixed scope")
                        .code,
                    "request_id_conflict"
                );
            }
            let mut altered = original.clone();
            altered.reservation_id = Some("other-reservation".into());
            assert_eq!(
                state
                    .admit_normal(&id, altered, "operator", now)
                    .expect_err("fixed reservation")
                    .code,
                "request_id_conflict"
            );
            let mut altered = original.clone();
            altered.action = if access == Access::Maintenance {
                Action::Position
            } else {
                Action::ResetApplication
            };
            assert_eq!(
                state
                    .admit_normal(&id, altered, "operator", now)
                    .expect_err("fixed access")
                    .code,
                "request_id_conflict"
            );
            let cleanup = Intent::new(scope, Action::HeartbeatStop, None, vec![]).expect("cleanup");
            assert_eq!(
                state
                    .admit_normal(&id, cleanup, "operator", now)
                    .expect_err("reservation ticket cannot become cleanup")
                    .code,
                "request_id_conflict"
            );
            assert!(state.ledger.get(&id).is_none());
            assert!(
                state
                    .admit_normal(&id, original.clone(), "operator", now)
                    .expect("unchanged valid ticket")
                    .new_submission
            );
            assert!(
                !state
                    .admit_normal(&id, original, "operator", now)
                    .expect("original replay")
                    .new_submission
            );
        }
    }
    #[test]
    fn shared_cleanup_ticket_expires_and_never_becomes_an_ordinary_action() {
        let (mut state, scope, reservation) = ticket_fixture(Access::Control);
        let now = Instant::now();
        let id = state
            .issue_cleanup_ticket("operator", scope.clone(), now)
            .expect("cleanup ticket");
        let ordinary = Intent::new(scope.clone(), Action::Position, Some(reservation), vec![])
            .expect("target");
        assert_eq!(
            state
                .admit_normal(&id, ordinary, "operator", now)
                .expect_err("cleanup-only purpose")
                .code,
            "request_id_conflict"
        );
        let cleanup =
            Intent::new(scope.clone(), Action::RecordingStop, None, vec![]).expect("cleanup");
        assert_eq!(
            state
                .admit_normal(&id, cleanup.clone(), "operator", now + TTL)
                .expect_err("expired")
                .code,
            "request_id_expired"
        );
        assert_eq!(
            state.ids.claim(&id, &scope, now),
            Err(super::super::operations::Error::Expired)
        );
        assert!(state.ledger.get(&id).is_none());
    }
    #[test]
    fn shared_reservation_ticket_cannot_revive_after_release_or_grant_rotation() {
        for revoke in [false, true] {
            let (mut state, scope, reservation) = ticket_fixture(Access::Maintenance);
            let now = Instant::now();
            let id = state
                .issue_reservation_ticket(&reservation, "operator", scope.clone(), now)
                .expect("old ticket");
            if revoke {
                state
                    .book
                    .set_permissions(scope.members[0].clone(), Permissions::default())
                    .expect("revoke");
                state
                    .book
                    .set_permissions(
                        scope.members[0].clone(),
                        Permissions {
                            maintenance: true,
                            control: true,
                        },
                    )
                    .expect("new grant");
            } else {
                state
                    .book
                    .release(&reservation, "operator")
                    .expect("release");
            }
            let replacement = state
                .book
                .reserve("operator", scope.members.clone(), Access::Maintenance)
                .expect("new reservation");
            let intent = Intent::new(
                scope.clone(),
                Action::ResetApplication,
                Some(replacement.id),
                vec![],
            )
            .expect("intent");
            assert_eq!(
                state
                    .admit_normal(&id, intent, "operator", now)
                    .expect_err("old authorization is retired")
                    .code,
                "request_id_expired"
            );
            assert_eq!(
                state.ids.claim(&id, &scope, now),
                Err(super::super::operations::Error::Expired)
            );
            assert!(state.ledger.get(&id).is_none());
        }
    }

    #[test]
    fn stop_cancelling_unstarted_maintenance_releases_only_its_future_readback_slot() {
        let (mut session, commands, _) = crate::session::ToolSession::test_maintenance_fixture();
        session.test_usb_request("maint");
        let (uid, run_nonce) = session.integration_identity().expect("identity");
        let member = MemberKey::new(
            "instance",
            "usb:maint",
            &uid,
            &run_nonce,
            &session.connection_generation().to_string(),
        )
        .expect("member");
        let mut owner = Workbench::with_test_sessions(session, BTreeMap::new());
        let mut state = State::new("instance").expect("state");
        state.control_enabled = true;
        state
            .book
            .set_permissions(
                member.clone(),
                Permissions {
                    maintenance: true,
                    control: true,
                },
            )
            .expect("grant");
        let scope = state.scope(&member).expect("scope");
        let reservation = state
            .book
            .reserve("operator", vec![member.clone()], Access::Maintenance)
            .expect("reservation");
        let now = Instant::now();
        let id = state
            .issue_reservation_ticket(&reservation.id, "operator", scope.clone(), now)
            .expect("ticket");
        state.reserve_readback(&member).expect("future slot");
        state
            .admit_normal(
                &id,
                Intent::new(
                    scope.clone(),
                    Action::ResetApplication,
                    Some(reservation.id.clone()),
                    vec![],
                )
                .expect("intent"),
                "operator",
                now,
            )
            .expect("queued maintenance");
        state
            .book
            .begin_pending(
                &reservation.id,
                &id,
                "operator",
                std::slice::from_ref(&member),
                Access::Maintenance,
            )
            .expect("pending original operation");
        state.jobs.insert(
            member.target_id.clone(),
            Job {
                id: id.clone(),
                started: false,
            },
        );
        let other =
            MemberKey::new("instance", "can:2", "other-uid", "other-run", "2").expect("other");
        state.reserve_readback(&other).expect("other future slot");
        let known = MemberKey::new("instance", "can:3", "known-uid", "known-run", "3")
            .expect("known readback");
        state.readbacks.insert(
            known.uid.clone(),
            Readback {
                original: known.clone(),
                current: None,
                user: false,
                startup: false,
            },
        );
        let mut control = super::super::control::State::new();
        let stop_id = state.ledger.stop_id(&scope).expect("stop id").to_owned();
        control
            .admit(
                &mut state,
                &mut owner,
                member.clone(),
                scope,
                None,
                "operator",
                &stop_id,
                super::super::control::Task::Stop,
                vec![],
                None,
            )
            .expect("actual Stop admission");
        assert!(
            !state.release_cancelled_readback(&Job {
                id: id.clone(),
                started: true
            }),
            "已开始工作不能走队列预留清理"
        );
        assert!(state.readback_reserved.contains(&member.uid));
        state.poll(&mut owner);
        assert_eq!(
            state.ledger.get(&id).expect("original record").submission,
            Submission::NotSubmitted
        );
        assert!(state.jobs.is_empty());
        state
            .book
            .authorize_member(&reservation.id, "operator", &member, Access::Maintenance)
            .expect("never-submitted pending claim ended");
        assert!(
            state.reserve_readback(&member).is_ok(),
            "Stop cancelled unstarted maintenance must free its unused future slot"
        );
        assert!(state.readback_reserved.contains(&other.uid));
        assert!(state.readback_required(&known));
        assert!(
            commands.try_recv().is_err(),
            "queued maintenance emitted no SDK bytes"
        );
        let (mut unknown, unknown_scope, unknown_reservation) = ticket_fixture(Access::Maintenance);
        let unknown_member = unknown_scope.members[0].clone();
        let unknown_id = unknown
            .issue_reservation_ticket(&unknown_reservation, "operator", unknown_scope.clone(), now)
            .expect("unknown ticket");
        unknown
            .reserve_readback(&unknown_member)
            .expect("unknown future slot");
        unknown
            .admit_normal(
                &unknown_id,
                Intent::new(
                    unknown_scope,
                    Action::ResetApplication,
                    Some(unknown_reservation),
                    vec![],
                )
                .expect("unknown intent"),
                "operator",
                now,
            )
            .expect("admission");
        unknown.ledger.begin_attempt(&unknown_id).expect("attempt");
        unknown
            .ledger
            .finish_attempt(&unknown_id, Submission::Unknown, Effect::Unresolved, now)
            .expect("original unknown");
        let before = unknown
            .ledger
            .get(&unknown_id)
            .expect("unknown record")
            .clone();
        assert!(
            !unknown.release_cancelled_readback(&Job {
                id: unknown_id.clone(),
                started: false
            }),
            "真实未知即使误标unstarted也不能按未提交清理"
        );
        assert!(unknown.readback_reserved.contains(&unknown_member.uid));
        assert_eq!(unknown.ledger.get(&unknown_id), Some(&before));
    }

    #[test]
    fn revoking_group_member_invalidates_all_unsubmitted_child_tickets() {
        let mut state = State::new("instance").expect("state");
        let a = MemberKey::new("instance", "usb:a", "uid-a", "nonce", "1").expect("a");
        let b = MemberKey::new("instance", "can:2", "uid-b", "nonce", "1").expect("b");
        for member in [&a, &b] {
            state
                .book
                .set_permissions(
                    member.clone(),
                    Permissions {
                        maintenance: false,
                        control: true,
                    },
                )
                .expect("grant");
        }
        let reservation = state
            .book
            .reserve(
                "group",
                vec![a.clone(), b.clone()],
                super::super::reservations::Access::Control,
            )
            .expect("atomic group");
        let now = Instant::now();
        let mut ids = Vec::new();
        for member in [&a, &b] {
            let scope = state.scope(member).expect("reserved stop scope");
            let id = state
                .issue_reservation_ticket(&reservation.id, "group", scope, now)
                .expect("child ticket");
            ids.push(id);
        }
        assert!(
            state.reservations.is_empty(),
            "group is not stored as a single-member Held"
        );
        let mut owner =
            Workbench::with_test_sessions(crate::session::ToolSession::new(), BTreeMap::new());
        state.revoke(&a, &mut owner).expect("revoke group grant");
        assert!(state.book.reservation(&reservation.id).is_none());
        assert!(state.tickets.is_empty());
        for (id, member) in ids.iter().zip([&a, &b]) {
            let scope = state.scopes[member].clone();
            assert_eq!(
                state.ids.claim(id, &scope, now),
                Err(super::super::operations::Error::Expired)
            );
        }
    }
    #[test]
    fn retired_owner_scopes_never_erase_or_overflow_readback_obligations() {
        let mut state = State::new("instance").expect("state");
        for index in 0..MAX_SCOPES {
            let member = MemberKey::new(
                "instance",
                &format!("usb:{index}"),
                &format!("uid-{index}"),
                "nonce",
                "1",
            )
            .expect("member");
            state
                .reserve_readback(&member)
                .expect("reserved before mutation");
            state.release_readback_reservation(&member);
            state.readbacks.insert(
                member.uid.clone(),
                Readback {
                    original: member,
                    current: None,
                    user: false,
                    startup: false,
                },
            );
        }
        // 即使当前没有已连接的范围，也保留全部未决读回结果。
        assert!(state.scopes.is_empty());
        let extra =
            MemberKey::new("instance", "usb:extra", "uid-extra", "nonce", "1").expect("extra");
        assert_eq!(
            state.reserve_readback(&extra).expect_err("full").code,
            "operation_capacity"
        );
        assert_eq!(state.readbacks.len(), MAX_SCOPES);
        assert!(state.readback_reserved.is_empty());
        let mut fresh = state.readbacks["uid-0"].original.clone();
        fresh.connection_generation = "2".into();
        state.note_config_read(&fresh, "user");
        state.note_config_read(&fresh, "startup");
        state
            .reserve_readback(&extra)
            .expect("explicit original-scope readback frees one bounded slot");
        assert_eq!(
            state.readbacks.len() + state.readback_reserved.len(),
            MAX_SCOPES
        );
    }
}
