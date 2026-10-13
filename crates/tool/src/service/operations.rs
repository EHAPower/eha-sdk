// Copyright The eha-sdk Contributors

//! 仅提供内存中的本地准入。任何请求 ID 都不是固件操作键。

use super::reservations::MemberKey;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

const MAX_RAW_PARAMETER_BYTES: usize = 65_536;
const MAX_PARAMETER_FIELDS: usize = 32;
const MAX_METADATA_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct OperationScope {
    pub(crate) members: Vec<MemberKey>,
    pub(crate) control_epoch: u64,
}

impl OperationScope {
    pub(crate) fn new(members: Vec<MemberKey>, control_epoch: u64) -> Result<Self, Error> {
        if members.is_empty() || control_epoch == 0 {
            return Err(Error::InvalidScope);
        }
        let mut uids = BTreeSet::new();
        let instance = &members[0].instance_id;
        for member in &members {
            member.validate().map_err(|_| Error::InvalidScope)?;
            if member.instance_id != *instance || !uids.insert(&member.uid) {
                return Err(Error::InvalidScope);
            }
        }
        Ok(Self {
            members,
            control_epoch,
        })
    }

    fn member_scope(&self, member: &MemberKey) -> Self {
        Self {
            members: vec![member.clone()],
            control_epoch: self.control_epoch,
        }
    }

    fn overlaps_device_run(&self, other: &Self) -> bool {
        self.members.iter().any(|left| {
            other.members.iter().any(|right| {
                left.instance_id == right.instance_id
                    && left.uid == right.uid
                    && left.run_nonce == right.run_nonce
            })
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Lane {
    Normal,
    Stop,
}

impl Lane {
    fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Stop => "stop",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    InvalidScope,
    InvalidId,
    ScopeMismatch,
    Expired,
    Capacity,
    SequenceExhausted,
    Conflict,
    MissingStopSlot,
    StoppedEpoch,
    Unresolved,
    InvalidTransition,
    WrongLane,
    Busy,
}

struct Ticket {
    scope: OperationScope,
    expires_at: Option<Instant>,
}

struct TicketLane {
    high_water: u64,
    capacity: usize,
    tickets: BTreeMap<u64, Ticket>,
}

pub(crate) struct RequestIds {
    instance_id: String,
    max_members: usize,
    ttl: Duration,
    normal: TicketLane,
    stop: TicketLane,
}

impl RequestIds {
    pub(crate) fn new(
        instance_id: &str,
        normal_capacity: usize,
        stop_capacity: usize,
        max_members: usize,
        ttl: Duration,
    ) -> Result<Self, Error> {
        if instance_id.is_empty()
            || instance_id.len() > MAX_METADATA_BYTES
            || normal_capacity == 0
            || stop_capacity == 0
            || max_members == 0
            || ttl.is_zero()
        {
            return Err(Error::InvalidScope);
        }
        Ok(Self {
            instance_id: instance_id.into(),
            max_members,
            ttl,
            normal: TicketLane {
                high_water: 0,
                capacity: normal_capacity,
                tickets: BTreeMap::new(),
            },
            stop: TicketLane {
                high_water: 0,
                capacity: stop_capacity,
                tickets: BTreeMap::new(),
            },
        })
    }

    fn lane_mut(&mut self, lane: Lane) -> &mut TicketLane {
        match lane {
            Lane::Normal => &mut self.normal,
            Lane::Stop => &mut self.stop,
        }
    }

    pub(crate) fn issue_normal(
        &mut self,
        scope: OperationScope,
        now: Instant,
    ) -> Result<String, Error> {
        self.normal
            .tickets
            .retain(|_, ticket| ticket.expires_at.is_none_or(|expiry| expiry > now));
        let expiry = now.checked_add(self.ttl).ok_or(Error::InvalidScope)?;
        self.issue(Lane::Normal, scope, Some(expiry))
    }

    /// 在公布范围具备 Stop 能力之前，为其规范范围预留；绝不因 TTL 到期而失效。
    pub(crate) fn reserve_stop(&mut self, scope: OperationScope) -> Result<String, Error> {
        self.can_reserve_stop(&scope)?;
        self.issue(Lane::Stop, scope, None)
    }

    pub(crate) fn can_reserve_stop(&self, scope: &OperationScope) -> Result<(), Error> {
        self.can_reserve_stop_after_invalidating(scope, &[])
    }

    pub(crate) fn can_reserve_stop_after_invalidating(
        &self,
        scope: &OperationScope,
        invalidating: &[String],
    ) -> Result<(), Error> {
        if scope.members.len() != 1 {
            return Err(Error::InvalidScope);
        }
        OperationScope::new(scope.members.clone(), scope.control_epoch)?;
        if scope.members.len() > self.max_members
            || scope.members[0].instance_id != self.instance_id
        {
            return Err(Error::InvalidScope);
        }
        let state = &self.stop;
        let mut reclaimed = BTreeSet::new();
        for id in invalidating {
            let (lane, sequence) = self.parse(id)?;
            if lane == Lane::Stop && state.tickets.contains_key(&sequence) {
                reclaimed.insert(sequence);
            }
        }
        if state.tickets.len().saturating_sub(reclaimed.len()) >= state.capacity {
            return Err(Error::Capacity);
        }
        state
            .high_water
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        Ok(())
    }

    fn issue(
        &mut self,
        lane: Lane,
        scope: OperationScope,
        expires_at: Option<Instant>,
    ) -> Result<String, Error> {
        OperationScope::new(scope.members.clone(), scope.control_epoch)?;
        if scope.members.len() > self.max_members
            || scope.members[0].instance_id != self.instance_id
        {
            return Err(Error::InvalidScope);
        }
        let state = self.lane_mut(lane);
        if state.tickets.len() >= state.capacity {
            return Err(Error::Capacity);
        }
        let sequence = state
            .high_water
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        state.tickets.insert(sequence, Ticket { scope, expires_at });
        state.high_water = sequence;
        Ok(format!("{}-{}-{sequence}", self.instance_id, lane.label()))
    }

    fn parse(&self, id: &str) -> Result<(Lane, u64), Error> {
        for lane in [Lane::Normal, Lane::Stop] {
            let prefix = format!("{}-{}-", self.instance_id, lane.label());
            if let Some(text) = id.strip_prefix(&prefix) {
                let sequence: u64 = text.parse().map_err(|_| Error::InvalidId)?;
                if sequence == 0 || sequence.to_string() != text {
                    return Err(Error::InvalidId);
                }
                return Ok((lane, sequence));
            }
        }
        Err(Error::InvalidId)
    }

    pub(crate) fn claim(
        &mut self,
        id: &str,
        scope: &OperationScope,
        now: Instant,
    ) -> Result<Lane, Error> {
        let (lane, sequence) = self.parse(id)?;
        let state = self.lane_mut(lane);
        if sequence > state.high_water {
            return Err(Error::InvalidId);
        }
        let ticket = state.tickets.get(&sequence).ok_or(Error::Expired)?;
        if ticket.expires_at.is_some_and(|expiry| expiry <= now) {
            state.tickets.remove(&sequence);
            return Err(Error::Expired);
        }
        if ticket.scope != *scope {
            return Err(Error::ScopeMismatch);
        }
        state.tickets.remove(&sequence);
        Ok(lane)
    }

    pub(crate) fn invalidate(&mut self, id: &str) -> Result<(), Error> {
        let (lane, sequence) = self.parse(id)?;
        self.lane_mut(lane).tickets.remove(&sequence);
        Ok(())
    }

    pub(crate) fn can_invalidate(&self, id: &str) -> Result<(), Error> {
        self.parse(id).map(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    SaveAndRestart,
    RestoreFactoryAndRestart,
    ResetApplication,
    EnterUpdate,
    HeartbeatStart,
    HeartbeatStop,
    HeartbeatOnce,
    Position,
    Velocity,
    Force,
    Impedance,
    TrialStart,
    TrialTarget,
    RecordingStart,
    RecordingStop,
    Stop,
    TrialStop,
}

impl Action {
    fn is_maintenance(self) -> bool {
        matches!(
            self,
            Self::SaveAndRestart
                | Self::RestoreFactoryAndRestart
                | Self::ResetApplication
                | Self::EnterUpdate
        )
    }

    fn is_stop(self) -> bool {
        matches!(self, Self::Stop | Self::TrialStop)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Intent {
    pub(crate) scope: OperationScope,
    pub(crate) action: Action,
    pub(crate) reservation_id: Option<String>,
    // 字段名排序，原始值不变：1、1.0 和 1e0 仍然不同。
    pub(crate) parameters: Vec<(String, String)>,
}

impl Intent {
    pub(crate) fn new(
        scope: OperationScope,
        action: Action,
        reservation_id: Option<String>,
        mut parameters: Vec<(String, String)>,
    ) -> Result<Self, Error> {
        OperationScope::new(scope.members.clone(), scope.control_epoch)?;
        if parameters.len() > MAX_PARAMETER_FIELDS
            || reservation_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > MAX_METADATA_BYTES)
        {
            return Err(Error::InvalidScope);
        }
        parameters.sort_by(|left, right| left.0.cmp(&right.0));
        let mut bytes = 0usize;
        let mut previous = None;
        for (name, raw) in &parameters {
            if name.is_empty() || name.len() > MAX_METADATA_BYTES || previous == Some(name) {
                return Err(Error::InvalidScope);
            }
            bytes = bytes
                .checked_add(name.len())
                .and_then(|total| total.checked_add(raw.len()))
                .ok_or(Error::InvalidScope)?;
            if bytes > MAX_RAW_PARAMETER_BYTES {
                return Err(Error::InvalidScope);
            }
            previous = Some(name);
        }
        if action.is_stop() && (scope.members.len() != 1 || reservation_id.is_some()) {
            return Err(Error::InvalidScope);
        }
        if action == Action::Stop && !parameters.is_empty() {
            return Err(Error::InvalidScope);
        }
        if action == Action::TrialStop
            && (parameters.len() != 1
                || parameters[0].0 != "trial_id"
                || parameters[0].1.is_empty())
        {
            return Err(Error::InvalidScope);
        }
        Ok(Self {
            scope,
            action,
            reservation_id,
            parameters,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    Queued,
    Attempting,
    Finished,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Submission {
    NotAttempted,
    NotSubmitted,
    LocallySubmitted,
    Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Effect {
    NotAttempted,
    /// 仅知维护操作的本地提交结果；设备观察仍未确认。
    NotApplicable,
    Known,
    Unresolved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Record {
    pub(crate) request_id: String,
    pub(crate) intent: Intent,
    pub(crate) provenance: String,
    pub(crate) admission: Admission,
    pub(crate) submission: Submission,
    pub(crate) effect: Effect,
    pub(crate) finished_at: Option<Instant>,
}

impl Record {
    fn reclaimable(&self) -> bool {
        self.admission == Admission::Finished
            && self.submission != Submission::Unknown
            && self.effect != Effect::Unresolved
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Admitted {
    pub(crate) record: Record,
    /// 新的本地工作项，不证明已实际向传输层提交。
    pub(crate) new_submission: bool,
    pub(crate) coalesced_existing: bool,
    pub(crate) cancelled_ids: Vec<String>,
}

struct StopSlot {
    request_id: String,
    fenced: bool,
}

pub(crate) struct Ledger {
    instance_id: String,
    normal_capacity: usize,
    stop_capacity: usize,
    retention: Duration,
    records: BTreeMap<String, Record>,
    current_trials: BTreeSet<String>,
    stop_slots: BTreeMap<OperationScope, StopSlot>,
}

impl Ledger {
    pub(crate) fn new(
        instance_id: &str,
        normal_capacity: usize,
        stop_capacity: usize,
        retention: Duration,
    ) -> Result<Self, Error> {
        if instance_id.is_empty()
            || instance_id.len() > MAX_METADATA_BYTES
            || normal_capacity == 0
            || stop_capacity == 0
            || retention.is_zero()
        {
            return Err(Error::InvalidScope);
        }
        Ok(Self {
            instance_id: instance_id.into(),
            normal_capacity,
            stop_capacity,
            retention,
            records: BTreeMap::new(),
            current_trials: BTreeSet::new(),
            stop_slots: BTreeMap::new(),
        })
    }

    fn validate_context(&self, scope: &OperationScope, ids: &RequestIds) -> Result<(), Error> {
        OperationScope::new(scope.members.clone(), scope.control_epoch)?;
        if ids.instance_id != self.instance_id || scope.members[0].instance_id != self.instance_id {
            return Err(Error::InvalidScope);
        }
        Ok(())
    }

    pub(crate) fn reserve_stop_scope(
        &mut self,
        scope: OperationScope,
        ids: &mut RequestIds,
    ) -> Result<String, Error> {
        self.validate_context(&scope, ids)?;
        if scope.members.len() != 1 {
            return Err(Error::InvalidScope);
        }
        if let Some(slot) = self.stop_slots.get(&scope) {
            return Ok(slot.request_id.clone());
        }
        // 旧范围仍被保留时，同一物理 UID 不能取得新的执行范围。
        for (old_scope, slot) in &self.stop_slots {
            if old_scope.members[0].uid == scope.members[0].uid {
                return Err(
                    if self
                        .records
                        .get(&slot.request_id)
                        .is_some_and(|record| !record.reclaimable())
                    {
                        Error::Unresolved
                    } else {
                        Error::Busy
                    },
                );
            }
        }
        if self.stop_slots.len() >= self.stop_capacity {
            return Err(Error::Capacity);
        }
        let id = ids.reserve_stop(scope.clone())?;
        self.stop_slots.insert(
            scope,
            StopSlot {
                request_id: id.clone(),
                fenced: false,
            },
        );
        Ok(id)
    }

    /// 检查在指定旧范围已退役后，新的 Stop 范围能否完整分配；不修改账本或请求编号。
    pub(crate) fn can_reserve_stop_scope_after_retiring(
        &self,
        scope: &OperationScope,
        ids: &RequestIds,
        retiring: &[OperationScope],
    ) -> Result<(), Error> {
        self.validate_context(scope, ids)?;
        if scope.members.len() != 1 {
            return Err(Error::InvalidScope);
        }
        for old_scope in self.stop_slots.keys() {
            if retiring.contains(old_scope) {
                continue;
            }
            if old_scope.members[0].uid == scope.members[0].uid {
                return Err(Error::Busy);
            }
        }
        let retained = self
            .stop_slots
            .keys()
            .filter(|old_scope| !retiring.contains(old_scope))
            .count();
        if retained >= self.stop_capacity {
            return Err(Error::Capacity);
        }
        let invalidating = retiring
            .iter()
            .filter_map(|scope| self.stop_slots.get(scope))
            .map(|slot| slot.request_id.clone())
            .collect::<Vec<_>>();
        ids.can_reserve_stop_after_invalidating(scope, &invalidating)
    }

    pub(crate) fn stop_fenced(&self, scope: &OperationScope) -> bool {
        self.stop_slots.get(scope).is_some_and(|slot| slot.fenced)
    }

    pub(crate) fn stop_id(&self, scope: &OperationScope) -> Option<&str> {
        self.stop_slots
            .get(scope)
            .map(|slot| slot.request_id.as_str())
    }

    fn replay(&self, id: &str, intent: &Intent) -> Result<Option<Admitted>, Error> {
        let Some(record) = self.records.get(id) else {
            return Ok(None);
        };
        if record.intent != *intent {
            return Err(Error::Conflict);
        }
        Ok(Some(Admitted {
            record: record.clone(),
            new_submission: false,
            coalesced_existing: intent.action.is_stop(),
            cancelled_ids: Vec::new(),
        }))
    }

    fn consume(
        ids: &mut RequestIds,
        lane: Lane,
        id: &str,
        scope: &OperationScope,
        now: Instant,
    ) -> Result<(), Error> {
        if ids.parse(id)?.0 != lane {
            return Err(Error::WrongLane);
        }
        ids.claim(id, scope, now)?;
        Ok(())
    }

    fn insert(&mut self, id: &str, intent: Intent, provenance: &str) -> Result<Record, Error> {
        if provenance.is_empty() || provenance.len() > MAX_METADATA_BYTES {
            return Err(Error::InvalidScope);
        }
        let record = Record {
            request_id: id.into(),
            intent,
            provenance: provenance.into(),
            admission: Admission::Queued,
            submission: Submission::NotAttempted,
            effect: Effect::NotAttempted,
            finished_at: None,
        };
        self.records.insert(id.into(), record.clone());
        Ok(record)
    }

    pub(crate) fn admit_normal(
        &mut self,
        id: &str,
        intent: Intent,
        provenance: &str,
        ids: &mut RequestIds,
        now: Instant,
    ) -> Result<Admitted, Error> {
        self.validate_context(&intent.scope, ids)?;
        Self::validate_input(&intent, provenance)?;
        if intent.action.is_stop() {
            return Err(Error::WrongLane);
        }
        if let Some(replay) = self.replay(id, &intent)? {
            return Ok(replay);
        }
        Self::consume(ids, Lane::Normal, id, &intent.scope, now)?;
        // 显式本地关闭可发生在已被阻断或结果未知的设备操作之后。
        // 它仍使用原范围和通常的有界票据与回执通路。
        let local_cleanup = matches!(intent.action, Action::HeartbeatStop | Action::RecordingStop);
        for member in &intent.scope.members {
            let slot = self
                .stop_slots
                .get(&intent.scope.member_scope(member))
                .ok_or(Error::MissingStopSlot)?;
            if slot.fenced && !local_cleanup {
                return Err(Error::StoppedEpoch);
            }
        }
        if !local_cleanup
            && self.records.values().any(|record| {
                (record.effect == Effect::Unresolved || record.submission == Submission::Unknown)
                    && record.intent.scope.overlaps_device_run(&intent.scope)
            })
        {
            return Err(Error::Unresolved);
        }
        if self
            .records
            .values()
            .filter(|record| !record.intent.action.is_stop())
            .count()
            >= self.normal_capacity
        {
            return Err(Error::Capacity);
        }
        let record = self.insert(id, intent, provenance)?;
        Ok(Admitted {
            record,
            new_submission: true,
            coalesced_existing: false,
            cancelled_ids: Vec::new(),
        })
    }

    pub(crate) fn admit_stop(
        &mut self,
        id: &str,
        intent: Intent,
        provenance: &str,
        ids: &mut RequestIds,
        now: Instant,
    ) -> Result<Admitted, Error> {
        self.validate_context(&intent.scope, ids)?;
        Self::validate_input(&intent, provenance)?;
        if !intent.action.is_stop() {
            return Err(Error::WrongLane);
        }
        if let Some(replay) = self.replay(id, &intent)? {
            return Ok(replay);
        }
        let slot = self
            .stop_slots
            .get(&intent.scope)
            .ok_or(Error::MissingStopSlot)?;
        if slot.request_id != id {
            return Err(Error::InvalidId);
        }
        Self::consume(ids, Lane::Stop, id, &intent.scope, now)?;
        let record = self.insert(id, intent, provenance)?;
        self.stop_slots
            .get_mut(&record.intent.scope)
            .expect("reserved stop slot")
            .fenced = true;
        let mut cancelled_ids = Vec::new();
        for queued in self.records.values_mut() {
            if !queued.intent.action.is_stop()
                && queued.admission == Admission::Queued
                && queued
                    .intent
                    .scope
                    .overlaps_device_run(&record.intent.scope)
            {
                queued.admission = Admission::Finished;
                queued.submission = Submission::NotSubmitted;
                queued.finished_at = Some(now);
                cancelled_ids.push(queued.request_id.clone());
            }
        }
        Ok(Admitted {
            record,
            new_submission: true,
            coalesced_existing: false,
            cancelled_ids,
        })
    }

    fn validate_input(intent: &Intent, provenance: &str) -> Result<(), Error> {
        if provenance.is_empty() || provenance.len() > MAX_METADATA_BYTES {
            return Err(Error::InvalidScope);
        }
        let canonical = Intent::new(
            intent.scope.clone(),
            intent.action,
            intent.reservation_id.clone(),
            intent.parameters.clone(),
        )?;
        if canonical != *intent {
            return Err(Error::InvalidScope);
        }
        Ok(())
    }

    pub(crate) fn begin_attempt(&mut self, id: &str) -> Result<(), Error> {
        let record = self.records.get_mut(id).ok_or(Error::Expired)?;
        if record.admission != Admission::Queued {
            return Err(Error::InvalidTransition);
        }
        record.admission = Admission::Attempting;
        Ok(())
    }

    pub(crate) fn finish_attempt(
        &mut self,
        id: &str,
        submission: Submission,
        effect: Effect,
        now: Instant,
    ) -> Result<(), Error> {
        let record = self.records.get_mut(id).ok_or(Error::Expired)?;
        let local_cleanup_success = matches!(
            record.intent.action,
            Action::HeartbeatStop | Action::RecordingStop
        ) && submission == Submission::NotSubmitted
            && effect == Effect::NotApplicable;
        if record.admission != Admission::Attempting
            || submission == Submission::NotAttempted
            || (effect == Effect::NotApplicable
                && !local_cleanup_success
                && (submission != Submission::LocallySubmitted
                    || !record.intent.action.is_maintenance()))
            || (submission == Submission::Unknown && effect != Effect::Unresolved)
            || (submission == Submission::NotSubmitted
                && effect != Effect::NotAttempted
                && !local_cleanup_success)
            || (submission == Submission::LocallySubmitted && effect == Effect::NotAttempted)
        {
            return Err(Error::InvalidTransition);
        }
        record.admission = Admission::Finished;
        record.submission = submission;
        if record.effect != Effect::Unresolved {
            record.effect = effect;
        }
        record.finished_at = Some(now);
        Ok(())
    }

    pub(crate) fn cancel_queued(&mut self, id: &str, now: Instant) -> Result<(), Error> {
        let record = self.records.get_mut(id).ok_or(Error::Expired)?;
        if record.admission != Admission::Queued {
            return Err(Error::InvalidTransition);
        }
        record.admission = Admission::Finished;
        record.submission = Submission::NotSubmitted;
        record.effect = Effect::NotAttempted;
        record.finished_at = Some(now);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn mark_unresolved(&mut self, id: &str) -> Result<(), Error> {
        self.records.get_mut(id).ok_or(Error::Expired)?.effect = Effect::Unresolved;
        Ok(())
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Record> {
        self.records.get(id)
    }

    /// 聚合所有 owner 当前试验引用；不复制回执、不改变容量或未知结果语义。
    pub(crate) fn retain_current_trials<'a>(&mut self, ids: impl Iterator<Item = &'a str>) {
        self.current_trials = ids
            .filter(|id| {
                self.records
                    .get(*id)
                    .is_some_and(|record| record.intent.action == Action::TrialStart)
            })
            .map(str::to_owned)
            .collect();
    }

    pub(crate) fn prune(&mut self, now: Instant) {
        self.records.retain(|id, record| {
            self.current_trials.contains(id)
                || !(record.reclaimable()
                    && record
                        .finished_at
                        .is_some_and(|time| now.saturating_duration_since(time) >= self.retention))
        });
    }

    /// 仅允许显式退役；未知记录、执行中工作及保留的回执均阻止退役。
    pub(crate) fn retire_stop_scope(
        &mut self,
        scope: &OperationScope,
        ids: &mut RequestIds,
    ) -> Result<(), Error> {
        let slot = self.stop_slots.get(scope).ok_or(Error::MissingStopSlot)?;
        if self.records.values().any(|record| {
            record
                .intent
                .scope
                .members
                .iter()
                .any(|member| member.uid == scope.members[0].uid)
                && !record.reclaimable()
        }) {
            return Err(Error::Unresolved);
        }
        if self.records.contains_key(&slot.request_id) {
            return Err(Error::Busy);
        }
        ids.invalidate(&slot.request_id)?;
        self.stop_slots.remove(scope);
        Ok(())
    }

    fn check_revoked_stop_scope_retireable(&self, scope: &OperationScope) -> Result<(), Error> {
        self.stop_slots.get(scope).ok_or(Error::MissingStopSlot)?;
        let uid = &scope.members[0].uid;
        if self.records.values().any(|record| {
            record
                .intent
                .scope
                .members
                .iter()
                .any(|member| &member.uid == uid)
                && !record.reclaimable()
        }) {
            return Err(Error::Unresolved);
        }
        Ok(())
    }

    /// 明确撤权后的范围只在全部动作已知时退役；owner 的活动试验门禁由调用方负责。
    /// 已知的 canonical Stop 回执仍可按普通回执规则读取，但其编号不再能占用新范围。
    pub(crate) fn can_retire_revoked_stop_scope(
        &self,
        scope: &OperationScope,
    ) -> Result<(), Error> {
        self.check_revoked_stop_scope_retireable(scope)
    }

    pub(crate) fn retire_revoked_stop_scope(
        &mut self,
        scope: &OperationScope,
        ids: &mut RequestIds,
    ) -> Result<(), Error> {
        self.check_revoked_stop_scope_retireable(scope)?;
        let id = self
            .stop_slots
            .get(scope)
            .expect("checked stop slot")
            .request_id
            .clone();
        ids.can_invalidate(&id)?;
        ids.invalidate(&id)?;
        self.stop_slots.remove(scope);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(target: &str, uid: &str) -> MemberKey {
        MemberKey::new("instance", target, uid, "nonce", "1").expect("test invariant")
    }

    fn scope() -> OperationScope {
        OperationScope::new(vec![member("usb:a", "uid-a")], 1).expect("test invariant")
    }

    fn ids() -> RequestIds {
        RequestIds::new("instance", 2, 2, 2, Duration::from_secs(1)).expect("test invariant")
    }

    fn intent(scope: OperationScope, text: &str) -> Intent {
        Intent::new(
            scope,
            Action::Position,
            Some("reservation".into()),
            vec![("target".into(), text.into())],
        )
        .expect("test invariant")
    }

    #[test]
    fn bounded_ticket_pools_are_independent_and_stop_has_no_ttl() {
        let mut ids = ids();
        let now = Instant::now();
        let scope = scope();
        let one = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        let two = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        assert_eq!(ids.issue_normal(scope.clone(), now), Err(Error::Capacity));
        let stop = ids.reserve_stop(scope.clone()).expect("test invariant");
        assert_eq!(
            ids.claim(&one, &scope, now + Duration::from_secs(1)),
            Err(Error::Expired)
        );
        assert_eq!(
            ids.claim(&two, &scope, now + Duration::from_secs(1)),
            Err(Error::Expired)
        );
        assert_eq!(
            ids.claim(&stop, &scope, now + Duration::from_secs(100)),
            Ok(Lane::Stop)
        );
        assert_eq!(ids.claim(&stop, &scope, now), Err(Error::Expired));
        assert!(
            ids.issue_normal(scope, now + Duration::from_secs(2))
                .is_ok()
        );
    }

    #[test]
    fn old_unknown_foreign_and_wrong_scope_tickets_never_execute() {
        let mut ids = ids();
        let now = Instant::now();
        let scope = scope();
        let id = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        let mut changed = scope.clone();
        changed.control_epoch = 2;
        assert_eq!(ids.claim(&id, &changed, now), Err(Error::ScopeMismatch));
        assert_eq!(ids.claim(&id, &scope, now), Ok(Lane::Normal));
        assert_eq!(ids.claim(&id, &scope, now), Err(Error::Expired));
        for fake in [
            "instance-normal-999",
            "instance-normal-01",
            "other-normal-1",
            "random-uuid",
        ] {
            assert_eq!(ids.claim(fake, &scope, now), Err(Error::InvalidId));
        }
        let replacement = ids.issue_normal(scope, now).expect("test invariant");
        assert_ne!(id, replacement);
    }

    #[test]
    fn counter_exhaustion_never_reuses_ids_or_allocates_tickets() {
        let mut ids = ids();
        ids.normal.high_water = u64::MAX;
        assert_eq!(
            ids.issue_normal(scope(), Instant::now()),
            Err(Error::SequenceExhausted)
        );
        assert!(ids.normal.tickets.is_empty());
        assert!(ids.reserve_stop(scope()).is_ok());
    }

    #[test]
    fn exact_raw_decimal_replay_keeps_original_provenance_and_unknown_pinned() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 1, 2, Duration::from_secs(1)).expect("test invariant");
        let scope = scope();
        ledger
            .reserve_stop_scope(scope.clone(), &mut ids)
            .expect("test invariant");
        let id = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        let original = intent(scope.clone(), "1.0");
        assert!(
            ledger
                .admit_normal(&id, original.clone(), "agent-one", &mut ids, now)
                .expect("test invariant")
                .new_submission
        );
        let replay = ledger
            .admit_normal(&id, original, "agent-two", &mut ids, now)
            .expect("test invariant");
        assert!(!replay.new_submission);
        assert_eq!(replay.record.provenance, "agent-one");
        assert_eq!(
            ledger.admit_normal(&id, intent(scope, "1"), "agent-one", &mut ids, now),
            Err(Error::Conflict)
        );
        ledger.begin_attempt(&id).expect("test invariant");
        ledger
            .finish_attempt(&id, Submission::Unknown, Effect::Unresolved, now)
            .expect("test invariant");
        ledger.prune(now + Duration::from_secs(100));
        assert_eq!(
            ledger.get(&id).expect("test invariant").effect,
            Effect::Unresolved
        );
    }

    #[test]
    fn current_trial_retention_counts_capacity_and_preserves_shared_references_and_unknown() {
        for unresolved in [false, true] {
            let now = Instant::now();
            let mut ids = ids();
            let mut ledger = Ledger::new("instance", 1, 2, Duration::from_secs(1)).expect("ledger");
            let scope = scope();
            ledger
                .reserve_stop_scope(scope.clone(), &mut ids)
                .expect("stop scope");
            let id = ids.issue_normal(scope.clone(), now).expect("trial ticket");
            let mut trial = intent(scope.clone(), "1");
            trial.action = Action::TrialStart;
            ledger
                .admit_normal(&id, trial, "operator", &mut ids, now)
                .expect("trial");
            ledger.begin_attempt(&id).expect("attempt");
            ledger
                .finish_attempt(
                    &id,
                    if unresolved {
                        Submission::Unknown
                    } else {
                        Submission::LocallySubmitted
                    },
                    if unresolved {
                        Effect::Unresolved
                    } else {
                        Effect::Known
                    },
                    now,
                )
                .expect("finish");
            let original = ledger.get(&id).expect("record").clone();
            // 两个 owner 引用同一记录；解除一个仍保留，且不复制记录或改变容量。
            ledger.retain_current_trials([id.as_str(), id.as_str()].into_iter());
            ledger.prune(now + Duration::from_secs(2));
            ledger.retain_current_trials([id.as_str()].into_iter());
            ledger.prune(now + Duration::from_secs(2));
            assert_eq!(ledger.get(&id), Some(&original));
            let next = ids.issue_normal(scope.clone(), now).expect("next ticket");
            assert_eq!(
                ledger.admit_normal(&next, intent(scope, "2"), "operator", &mut ids, now),
                Err(if unresolved {
                    Error::Unresolved
                } else {
                    Error::Capacity
                })
            );
            ledger.retain_current_trials(std::iter::empty());
            ledger.prune(now + Duration::from_secs(2));
            assert_eq!(
                ledger.get(&id),
                if unresolved { Some(&original) } else { None }
            );
        }
    }

    #[test]
    fn retired_receipt_and_capacity_rejection_cannot_become_new_actions() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 1, 2, Duration::from_secs(1)).expect("test invariant");
        let scope = scope();
        ledger
            .reserve_stop_scope(scope.clone(), &mut ids)
            .expect("test invariant");
        let one = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        ledger
            .admit_normal(&one, intent(scope.clone(), "1"), "agent", &mut ids, now)
            .expect("test invariant");
        let two = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        assert_eq!(
            ledger.admit_normal(&two, intent(scope.clone(), "2"), "agent", &mut ids, now),
            Err(Error::Capacity)
        );
        ledger.begin_attempt(&one).expect("test invariant");
        ledger
            .finish_attempt(&one, Submission::NotSubmitted, Effect::NotAttempted, now)
            .expect("test invariant");
        ledger.prune(now + Duration::from_secs(1));
        assert_eq!(
            ledger.admit_normal(&one, intent(scope.clone(), "1"), "agent", &mut ids, now),
            Err(Error::Expired)
        );
        assert_eq!(
            ledger.admit_normal(&two, intent(scope, "2"), "agent", &mut ids, now),
            Err(Error::Expired)
        );
    }

    #[test]
    fn full_normal_ledger_stop_fences_targets_and_coalesces_unknown_without_alias() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 1, 1, Duration::from_secs(1)).expect("test invariant");
        let scope = scope();
        let stop_id = ledger
            .reserve_stop_scope(scope.clone(), &mut ids)
            .expect("test invariant");
        let target_id = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        ledger
            .admit_normal(
                &target_id,
                intent(scope.clone(), "1"),
                "writer",
                &mut ids,
                now,
            )
            .expect("test invariant");
        let stop = Intent::new(scope.clone(), Action::Stop, None, vec![]).expect("test invariant");
        let admitted = ledger
            .admit_stop(&stop_id, stop.clone(), "manual-alias-caller", &mut ids, now)
            .expect("test invariant");
        assert!(admitted.new_submission);
        assert_eq!(admitted.cancelled_ids, vec![target_id.clone()]);
        assert_eq!(
            ledger.get(&target_id).expect("test invariant").submission,
            Submission::NotSubmitted
        );
        assert_eq!(
            ledger.begin_attempt(&target_id),
            Err(Error::InvalidTransition)
        );
        ledger.begin_attempt(&stop_id).expect("test invariant");
        ledger
            .finish_attempt(
                &stop_id,
                Submission::LocallySubmitted,
                Effect::Unresolved,
                now,
            )
            .expect("test invariant");
        let replay = ledger
            .admit_stop(&stop_id, stop, "other-manual-caller", &mut ids, now)
            .expect("test invariant");
        assert!(!replay.new_submission);
        assert!(replay.coalesced_existing);
        assert_eq!(
            ledger.reserve_stop_scope(scope.clone(), &mut ids),
            Ok(stop_id.clone())
        );
        ledger.prune(now + Duration::from_secs(100));
        assert!(ledger.get(&stop_id).is_some());
        assert_eq!(
            ledger.retire_stop_scope(&scope, &mut ids),
            Err(Error::Unresolved)
        );
        let new_id = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        assert_eq!(
            ledger.admit_normal(&new_id, intent(scope, "2"), "writer", &mut ids, now),
            Err(Error::StoppedEpoch)
        );
    }

    #[test]
    fn unknown_old_scope_blocks_replacement_and_groups_need_all_stop_resources() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 2, 2, Duration::from_secs(1)).expect("test invariant");
        let one = scope();
        ledger
            .reserve_stop_scope(one.clone(), &mut ids)
            .expect("test invariant");
        let group = OperationScope::new(vec![one.members[0].clone(), member("can:1", "uid-b")], 1)
            .expect("test invariant");
        let id = ids
            .issue_normal(group.clone(), now)
            .expect("test invariant");
        assert_eq!(
            ledger.admit_normal(&id, intent(group, "1"), "agent", &mut ids, now),
            Err(Error::MissingStopSlot)
        );
        let stop_id = ledger.stop_id(&one).expect("test invariant").to_owned();
        let stop = Intent::new(one.clone(), Action::Stop, None, vec![]).expect("test invariant");
        ledger
            .admit_stop(&stop_id, stop, "manual", &mut ids, now)
            .expect("test invariant");
        ledger.begin_attempt(&stop_id).expect("test invariant");
        ledger
            .finish_attempt(&stop_id, Submission::Unknown, Effect::Unresolved, now)
            .expect("test invariant");
        let mut replacement = one;
        replacement.members[0].connection_generation = "2".into();
        replacement.control_epoch = 2;
        assert_eq!(
            ledger.reserve_stop_scope(replacement, &mut ids),
            Err(Error::Unresolved)
        );
    }

    #[test]
    fn known_stop_retirement_expires_old_id_before_new_epoch() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 1, 1, Duration::from_secs(1)).expect("test invariant");
        let scope = scope();
        let id = ledger
            .reserve_stop_scope(scope.clone(), &mut ids)
            .expect("test invariant");
        let stop = Intent::new(scope.clone(), Action::Stop, None, vec![]).expect("test invariant");
        ledger
            .admit_stop(&id, stop.clone(), "manual", &mut ids, now)
            .expect("test invariant");
        ledger.begin_attempt(&id).expect("test invariant");
        ledger
            .finish_attempt(&id, Submission::NotSubmitted, Effect::NotAttempted, now)
            .expect("test invariant");
        assert_eq!(ledger.retire_stop_scope(&scope, &mut ids), Err(Error::Busy));
        ledger.prune(now + Duration::from_secs(1));
        ledger
            .retire_stop_scope(&scope, &mut ids)
            .expect("test invariant");
        assert_eq!(ids.claim(&id, &scope, now), Err(Error::Expired));
        assert_eq!(
            ledger.admit_stop(&id, stop, "manual", &mut ids, now),
            Err(Error::MissingStopSlot)
        );
        let mut next = scope;
        next.control_epoch = 2;
        assert_ne!(
            ledger
                .reserve_stop_scope(next, &mut ids)
                .expect("test invariant"),
            id
        );
    }

    #[test]
    fn revoked_scope_retires_known_stop_slot_without_erasing_its_receipt() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 2, 2, Duration::from_secs(30)).expect("test invariant");
        let scope = scope();
        let id = ledger
            .reserve_stop_scope(scope.clone(), &mut ids)
            .expect("test invariant");
        let stop = Intent::new(scope.clone(), Action::Stop, None, vec![]).expect("test invariant");
        ledger
            .admit_stop(&id, stop, "manual", &mut ids, now)
            .expect("test invariant");
        ledger.begin_attempt(&id).expect("test invariant");
        ledger
            .finish_attempt(&id, Submission::LocallySubmitted, Effect::Known, now)
            .expect("test invariant");

        ledger
            .retire_revoked_stop_scope(&scope, &mut ids)
            .expect("known revoked scope retires immediately");

        assert_eq!(ledger.stop_id(&scope), None);
        assert_eq!(
            ledger.get(&id).expect("known receipt remains").submission,
            Submission::LocallySubmitted
        );
        assert_eq!(ids.claim(&id, &scope, now), Err(Error::Expired));
    }

    #[test]
    fn known_current_trial_receipt_does_not_block_revoked_scope_retirement() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 2, 2, Duration::from_secs(30)).expect("test invariant");
        let scope = scope();
        ledger
            .reserve_stop_scope(scope.clone(), &mut ids)
            .expect("test invariant");
        let trial_id = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        let mut trial = intent(scope.clone(), "1");
        trial.action = Action::TrialStart;
        ledger
            .admit_normal(&trial_id, trial, "operator", &mut ids, now)
            .expect("test invariant");
        ledger.begin_attempt(&trial_id).expect("test invariant");
        ledger
            .finish_attempt(&trial_id, Submission::LocallySubmitted, Effect::Known, now)
            .expect("test invariant");
        ledger.retain_current_trials([trial_id.as_str()].into_iter());

        ledger
            .retire_revoked_stop_scope(&scope, &mut ids)
            .expect("known current trial receipt does not keep a revoked scope active");
        assert!(ledger.get(&trial_id).is_some());
        assert_eq!(ledger.stop_id(&scope), None);
    }

    #[test]
    fn stop_slot_preflight_reclaims_only_the_retired_unconsumed_ticket_capacity() {
        let mut ids =
            RequestIds::new("instance", 2, 1, 2, Duration::from_secs(1)).expect("test invariant");
        let mut ledger =
            Ledger::new("instance", 2, 2, Duration::from_secs(1)).expect("test invariant");
        let old = scope();
        ledger
            .reserve_stop_scope(old.clone(), &mut ids)
            .expect("old canonical Stop");
        let replacement =
            OperationScope::new(vec![member("usb:b", "uid-a")], 2).expect("test invariant");

        ledger
            .can_reserve_stop_scope_after_retiring(&replacement, &ids, &[old])
            .expect("the retiring unconsumed Stop ticket frees one projected slot");
    }

    #[test]
    fn local_cleanup_success_finishes_only_two_actions_without_clearing_prior_unknown() {
        let now = Instant::now();
        for action in [
            Action::SaveAndRestart,
            Action::RestoreFactoryAndRestart,
            Action::ResetApplication,
            Action::EnterUpdate,
            Action::HeartbeatStart,
            Action::HeartbeatStop,
            Action::HeartbeatOnce,
            Action::Position,
            Action::Velocity,
            Action::Force,
            Action::Impedance,
            Action::TrialStart,
            Action::TrialTarget,
            Action::RecordingStart,
            Action::RecordingStop,
            Action::Stop,
            Action::TrialStop,
        ] {
            let mut ledger = Ledger::new("instance", 2, 1, Duration::from_secs(1)).expect("ledger");
            let parameters = if action == Action::TrialStop {
                vec![("trial_id".into(), "trial".into())]
            } else {
                vec![]
            };
            let intent = Intent::new(scope(), action, None, parameters).expect("intent");
            ledger
                .insert("instance-normal-1", intent, "human")
                .expect("record");
            ledger.begin_attempt("instance-normal-1").expect("attempt");
            let before = ledger.get("instance-normal-1").expect("record").clone();
            assert_eq!(
                ledger.finish_attempt(
                    "instance-normal-1",
                    Submission::Unknown,
                    Effect::NotApplicable,
                    now
                ),
                Err(Error::InvalidTransition)
            );
            assert_eq!(ledger.get("instance-normal-1"), Some(&before));
            let result = ledger.finish_attempt(
                "instance-normal-1",
                Submission::NotSubmitted,
                Effect::NotApplicable,
                now,
            );
            if matches!(action, Action::HeartbeatStop | Action::RecordingStop) {
                result.expect("local cleanup success");
                let record = ledger.get("instance-normal-1").expect("finished record");
                assert_eq!(record.admission, Admission::Finished);
                assert_eq!(record.submission, Submission::NotSubmitted);
                assert_eq!(record.effect, Effect::NotApplicable);
                assert_eq!(
                    ledger.finish_attempt(
                        "instance-normal-1",
                        Submission::NotSubmitted,
                        Effect::NotApplicable,
                        now
                    ),
                    Err(Error::InvalidTransition)
                );
                let unknown_intent =
                    Intent::new(scope(), action, None, vec![]).expect("cleanup intent");
                ledger
                    .insert("instance-normal-2", unknown_intent, "human")
                    .expect("record");
                ledger.begin_attempt("instance-normal-2").expect("attempt");
                ledger
                    .mark_unresolved("instance-normal-2")
                    .expect("prior unknown");
                ledger
                    .finish_attempt(
                        "instance-normal-2",
                        Submission::NotSubmitted,
                        Effect::NotApplicable,
                        now,
                    )
                    .expect("local attempt ends");
                assert_eq!(
                    ledger
                        .get("instance-normal-2")
                        .expect("unknown retained")
                        .effect,
                    Effect::Unresolved
                );
                ledger.prune(now + Duration::from_secs(100));
                assert!(ledger.get("instance-normal-2").is_some());
            } else {
                assert_eq!(result, Err(Error::InvalidTransition), "{action:?}");
                assert_eq!(ledger.get("instance-normal-1"), Some(&before));
            }
        }
    }

    #[test]
    fn local_cleanup_after_stop_preserves_unknown_scope_and_bounded_admission() {
        for action in [Action::HeartbeatStop, Action::RecordingStop] {
            let now = Instant::now();
            let mut ids =
                RequestIds::new("instance", 8, 1, 1, Duration::from_secs(1)).expect("ids");
            let mut ledger = Ledger::new("instance", 2, 1, Duration::from_secs(1)).expect("ledger");
            let scope = scope();
            let stop_id = ledger
                .reserve_stop_scope(scope.clone(), &mut ids)
                .expect("stop slot");
            let target_id = ids.issue_normal(scope.clone(), now).expect("target id");
            ledger
                .admit_normal(
                    &target_id,
                    intent(scope.clone(), "1"),
                    "agent",
                    &mut ids,
                    now,
                )
                .expect("target");
            ledger.begin_attempt(&target_id).expect("attempt target");
            ledger
                .finish_attempt(&target_id, Submission::Unknown, Effect::Unresolved, now)
                .expect("target unknown");
            ledger
                .admit_stop(
                    &stop_id,
                    Intent::new(scope.clone(), Action::Stop, None, vec![]).expect("stop intent"),
                    "human",
                    &mut ids,
                    now,
                )
                .expect("stop");
            ledger.begin_attempt(&stop_id).expect("attempt stop");
            ledger
                .finish_attempt(&stop_id, Submission::Unknown, Effect::Unresolved, now)
                .expect("stop unknown");
            let target_before = ledger.get(&target_id).expect("target record").clone();
            let stop_before = ledger.get(&stop_id).expect("stop record").clone();
            let rejected_id = ids.issue_normal(scope.clone(), now).expect("ordinary id");
            assert_eq!(
                ledger.admit_normal(
                    &rejected_id,
                    intent(scope.clone(), "2"),
                    "agent",
                    &mut ids,
                    now
                ),
                Err(Error::StoppedEpoch)
            );
            let cleanup_id = ids.issue_normal(scope.clone(), now).expect("cleanup id");
            let cleanup = Intent::new(scope.clone(), action, None, vec![]).expect("cleanup intent");
            assert!(
                ledger
                    .admit_normal(&cleanup_id, cleanup.clone(), "human", &mut ids, now)
                    .expect("explicit local cleanup")
                    .new_submission
            );
            assert!(
                !ledger
                    .admit_normal(&cleanup_id, cleanup, "human", &mut ids, now)
                    .expect("same request replay")
                    .new_submission
            );
            let other = if action == Action::HeartbeatStop {
                Action::RecordingStop
            } else {
                Action::HeartbeatStop
            };
            assert_eq!(
                ledger.admit_normal(
                    &cleanup_id,
                    Intent::new(scope.clone(), other, None, vec![]).expect("other cleanup"),
                    "human",
                    &mut ids,
                    now
                ),
                Err(Error::Conflict)
            );
            let extra_id = ids.issue_normal(scope.clone(), now).expect("extra id");
            assert_eq!(
                ledger.admit_normal(
                    &extra_id,
                    Intent::new(scope.clone(), action, None, vec![]).expect("extra cleanup"),
                    "human",
                    &mut ids,
                    now
                ),
                Err(Error::Capacity)
            );
            assert_eq!(ledger.get(&target_id), Some(&target_before));
            assert_eq!(ledger.get(&stop_id), Some(&stop_before));
            ledger.prune(now + Duration::from_secs(100));
            assert_eq!(ledger.get(&target_id), Some(&target_before));
            assert_eq!(ledger.get(&stop_id), Some(&stop_before));
            assert_eq!(
                ledger.retire_stop_scope(&scope, &mut ids),
                Err(Error::Unresolved)
            );
        }
    }

    #[test]
    fn maintenance_unknown_and_late_effect_unknown_remain_pinned() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 2, 2, Duration::from_secs(1)).expect("test invariant");
        let scope = scope();
        ledger
            .reserve_stop_scope(scope.clone(), &mut ids)
            .expect("test invariant");
        let id = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        let maintenance = Intent::new(
            scope.clone(),
            Action::ResetApplication,
            Some("reservation".into()),
            vec![],
        )
        .expect("test invariant");
        ledger
            .admit_normal(&id, maintenance, "agent", &mut ids, now)
            .expect("test invariant");
        ledger.begin_attempt(&id).expect("test invariant");
        assert_eq!(
            ledger.finish_attempt(&id, Submission::Unknown, Effect::Known, now),
            Err(Error::InvalidTransition)
        );
        ledger
            .finish_attempt(&id, Submission::Unknown, Effect::Unresolved, now)
            .expect("test invariant");
        let rejected = ids
            .issue_normal(scope.clone(), now)
            .expect("test invariant");
        assert_eq!(
            ledger.admit_normal(
                &rejected,
                intent(scope.clone(), "2"),
                "agent",
                &mut ids,
                now
            ),
            Err(Error::Unresolved)
        );
        let second_scope =
            OperationScope::new(vec![member("usb:b", "uid-b")], 1).expect("test invariant");
        ledger
            .reserve_stop_scope(second_scope.clone(), &mut ids)
            .expect("test invariant");
        let second = ids
            .issue_normal(second_scope.clone(), now)
            .expect("test invariant");
        ledger
            .admit_normal(&second, intent(second_scope, "2"), "agent", &mut ids, now)
            .expect("test invariant");
        ledger.begin_attempt(&second).expect("test invariant");
        ledger
            .finish_attempt(&second, Submission::LocallySubmitted, Effect::Known, now)
            .expect("test invariant");
        ledger.mark_unresolved(&second).expect("test invariant");
        ledger.prune(now + Duration::from_secs(100));
        assert!(ledger.get(&id).is_some());
        assert!(ledger.get(&second).is_some());
        assert_eq!(
            ledger.retire_stop_scope(&scope, &mut ids),
            Err(Error::Unresolved)
        );
    }

    #[test]
    fn known_maintenance_receipt_expires_but_unknown_submission_stays_pinned() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 2, 2, Duration::from_secs(1)).expect("test invariant");
        for (index, (submission, effect)) in [
            (Submission::LocallySubmitted, Effect::NotApplicable),
            (Submission::Unknown, Effect::Unresolved),
        ]
        .into_iter()
        .enumerate()
        {
            let scope = OperationScope::new(
                vec![member(&format!("usb:{index}"), &format!("uid-{index}"))],
                1,
            )
            .expect("test invariant");
            ledger
                .reserve_stop_scope(scope.clone(), &mut ids)
                .expect("test invariant");
            let id = ids
                .issue_normal(scope.clone(), now)
                .expect("test invariant");
            let intent = Intent::new(
                scope,
                Action::ResetApplication,
                Some("reservation".into()),
                vec![],
            )
            .expect("test invariant");
            ledger
                .admit_normal(&id, intent, "agent", &mut ids, now)
                .expect("test invariant");
            ledger.begin_attempt(&id).expect("test invariant");
            if submission == Submission::Unknown {
                assert_eq!(
                    ledger.finish_attempt(&id, submission, Effect::NotApplicable, now),
                    Err(Error::InvalidTransition)
                );
            }
            ledger
                .finish_attempt(&id, submission, effect, now)
                .expect("test invariant");
        }
        ledger.prune(now + Duration::from_secs(100));
        assert!(ledger.get("instance-normal-1").is_none());
        let pinned = ledger.get("instance-normal-2").expect("test invariant");
        assert_eq!(pinned.submission, Submission::Unknown);
        assert_eq!(pinned.effect, Effect::Unresolved);
    }

    #[test]
    fn stop_fence_matches_all_aliases_and_group_members_but_not_another_run() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 2, 1, Duration::from_secs(1)).expect("test invariant");
        let authoritative = scope();
        let stop_id = ledger
            .reserve_stop_scope(authoritative.clone(), &mut ids)
            .expect("test invariant");
        // 旧版或 P4 导入的生产者可能早于唯一权威执行范围规则。
        let alias = member("can:2", "uid-a");
        let group =
            OperationScope::new(vec![alias, member("usb:b", "uid-b")], 99).expect("test invariant");
        ledger
            .insert("imported-group", intent(group, "1"), "legacy-writer")
            .expect("test invariant");
        let mut other_run = authoritative.clone();
        other_run.members[0].run_nonce = "other-run".into();
        ledger
            .insert("other-run", intent(other_run, "1"), "legacy-writer")
            .expect("test invariant");
        let stop = Intent::new(authoritative, Action::Stop, None, vec![]).expect("test invariant");
        let admitted = ledger
            .admit_stop(&stop_id, stop, "manual-alias", &mut ids, now)
            .expect("test invariant");
        assert_eq!(admitted.cancelled_ids, vec!["imported-group"]);
        assert_eq!(
            ledger
                .get("imported-group")
                .expect("test invariant")
                .admission,
            Admission::Finished
        );
        assert_eq!(
            ledger.get("other-run").expect("test invariant").admission,
            Admission::Queued
        );
    }

    #[test]
    fn invalid_metadata_wrong_lane_and_changed_scope_do_not_consume_reserved_stop() {
        let now = Instant::now();
        let mut ids = ids();
        let mut ledger =
            Ledger::new("instance", 1, 1, Duration::from_secs(1)).expect("test invariant");
        let scope = scope();
        let id = ledger
            .reserve_stop_scope(scope.clone(), &mut ids)
            .expect("test invariant");
        let stop = Intent::new(scope.clone(), Action::Stop, None, vec![]).expect("test invariant");
        assert_eq!(
            ledger.admit_stop(&id, stop.clone(), "", &mut ids, now),
            Err(Error::InvalidScope)
        );
        assert_eq!(
            ledger.admit_normal(&id, intent(scope.clone(), "1"), "agent", &mut ids, now),
            Err(Error::WrongLane)
        );
        assert!(
            ledger
                .admit_stop(&id, stop, "manual", &mut ids, now)
                .expect("test invariant")
                .new_submission
        );
        assert_eq!(ids.claim(&id, &scope, now), Err(Error::Expired));
        let mut foreign = RequestIds::new("other-instance", 2, 2, 2, Duration::from_secs(1))
            .expect("test invariant");
        foreign.instance_id = "other-instance".into();
        assert_eq!(
            ledger.reserve_stop_scope(scope, &mut foreign),
            Err(Error::InvalidScope)
        );
    }

    #[test]
    fn raw_parameter_bounds_and_duplicate_names_are_rejected_without_parsing_sdk_config() {
        assert_eq!(
            Intent::new(
                scope(),
                Action::SaveAndRestart,
                Some("r".into()),
                vec![("config".into(), "a".repeat(MAX_RAW_PARAMETER_BYTES + 1))]
            ),
            Err(Error::InvalidScope)
        );
        assert_eq!(
            Intent::new(
                scope(),
                Action::Position,
                Some("r".into()),
                vec![("target".into(), "1".into()), ("target".into(), "1".into())]
            ),
            Err(Error::InvalidScope)
        );
        let one = Intent::new(
            scope(),
            Action::Position,
            Some("r".into()),
            vec![("z".into(), "1e0".into()), ("a".into(), "1.0".into())],
        )
        .expect("test invariant");
        let two = Intent::new(
            scope(),
            Action::Position,
            Some("r".into()),
            vec![("a".into(), "1.0".into()), ("z".into(), "1e0".into())],
        )
        .expect("test invariant");
        assert_eq!(one, two);
        let a = member("usb:a", "uid-a");
        let alias = member("can:2", "uid-a");
        assert_eq!(
            OperationScope::new(vec![a, alias], 1),
            Err(Error::InvalidScope)
        );
    }
}
