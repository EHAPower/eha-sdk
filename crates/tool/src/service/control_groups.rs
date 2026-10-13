// Copyright The eha-sdk Contributors
//! 固定原始成员的群组编排；本地占用准入原子，设备提交与效果逐台报告。

use super::{
    Method, Workbench, control, integration,
    maintenance::{ApiError, book_error, denied, member, member_json},
    mutations,
    operations::{Admission, OperationScope},
    reservations::{Access, MemberKey},
};
use crate::RawFields;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

const MAX_GROUPS: usize = 16;
const MAX_MEMBERS: usize = 16;

struct Member {
    binding_id: String,
    key: MemberKey,
    scope: OperationScope,
    stop_request_id: String,
    operation_id: Option<String>,
    error: Option<Value>,
}

struct Group {
    id: String,
    reservation_id: String,
    claimant: String,
    stop_request_id: String,
    members: Vec<Member>,
    released: bool,
    barrier: Option<Vec<String>>,
}

pub(super) struct State {
    groups: BTreeMap<String, Group>,
}
impl State {
    pub(super) fn new() -> Self {
        Self {
            groups: BTreeMap::new(),
        }
    }

    fn reserve(
        &mut self,
        claimant: &str,
        references: Vec<(String, MemberKey)>,
        common: &mut mutations::State,
    ) -> Result<String, ApiError> {
        if self.groups.len() >= MAX_GROUPS {
            return Err(denied("operation_capacity", "固定群组范围容量已满"));
        }
        validate_members(&references)?;
        let mut members = Vec::with_capacity(references.len());
        for (binding_id, key) in references {
            let scope = common.scope(&key)?;
            let stop_request_id = common
                .ledger
                .stop_id(&scope)
                .ok_or_else(|| denied("bad_request", "成员没有固定 Stop 范围"))?
                .to_owned();
            members.push(Member {
                binding_id,
                key,
                scope,
                stop_request_id,
                operation_id: None,
                error: None,
            });
        }
        // 唯一一次完整 Book reserve；任何成员失败均不留下部分 writer claim。
        let reservation = common
            .book
            .reserve(
                claimant,
                members.iter().map(|member| member.key.clone()).collect(),
                Access::Control,
            )
            .map_err(book_error)?;
        let id = format!("{}-group", reservation.id);
        let group = Group {
            stop_request_id: format!("{id}-stop"),
            id: id.clone(),
            reservation_id: reservation.id,
            claimant: claimant.into(),
            members,
            released: false,
            barrier: None,
        };
        self.groups.insert(id.clone(), group);
        Ok(id)
    }

    fn group_mut(&mut self, id: &str, claimant: Option<&str>) -> Result<&mut Group, ApiError> {
        let group = self
            .groups
            .get_mut(id)
            .ok_or_else(|| denied("group_not_found", "群组不存在"))?;
        if claimant.is_some_and(|claimant| claimant != group.claimant) {
            return Err(denied("claimant_mismatch", "群组属于另一会话"));
        }
        Ok(group)
    }

    pub(super) fn poll(
        &mut self,
        control: &mut control::State,
        common: &mut mutations::State,
        workbench: &mut Workbench,
    ) {
        for group in self.groups.values_mut() {
            if common.book.reservation(&group.reservation_id).is_none() {
                group.released = true;
                common.invalidate_reservation_tickets(&group.reservation_id);
            }
            let Some(ids) = group.barrier.as_ref() else {
                continue;
            };
            let failed = group.released
                || ids.iter().any(|id| {
                    common
                        .ledger
                        .get(id)
                        .is_none_or(|record| record.admission == Admission::Finished)
                });
            if failed {
                for id in ids {
                    control.cancel(id, common, workbench, "群组预检未全部通过；未开始目标");
                }
                group.barrier = None;
            } else if ids.iter().all(|id| control.trial_prepared(id)) {
                // 同一个 owner 步骤打开全部已准备的 barrier，实际 TX 仍逐台且非原子。
                for id in ids {
                    control.release_trial(id);
                }
                group.barrier = None;
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // 一个 owner 步骤同时核对绑定、原始参数和两份共享状态。
    pub(super) fn route(
        &mut self,
        path: &str,
        method: Method,
        fields: &RawFields,
        bindings: &mut integration::IntegrationState,
        common: &mut mutations::State,
        control: &mut control::State,
        workbench: &mut Workbench,
    ) -> Result<(u16, Value), ApiError> {
        let path = path.strip_prefix("/service/v2/").unwrap_or("");
        if method != Method::Get {
            if fields.string("instance_id")? != common.instance_id {
                return Err(denied("instance_mismatch", "服务实例已改变"));
            }
            if !common.control_enabled {
                return Err(denied("unsupported_permission", "控制发布门槛尚未开放"));
            }
        }
        if path == "groups" && method == Method::Post {
            fields.reject_unknown(&["instance_id", "claimant_id", "members"])?;
            let entries: Vec<RawFields> = fields.decode_field("members")?;
            if !(1..=MAX_MEMBERS).contains(&entries.len()) {
                return Err("群组必须明确包含 1..16 个成员".to_owned().into());
            }
            let mut members = Vec::with_capacity(entries.len());
            for mut entry in entries {
                entry.reject_unknown(&[
                    "binding_id",
                    "expected_identity",
                    "connection_generation",
                ])?;
                let id = entry.string("binding_id")?;
                entry
                    .0
                    .insert("instance_id".into(), fields.0["instance_id"].clone());
                let binding = bindings.checked_binding(&id, &entry, workbench)?;
                members.push((id, member(&common.instance_id, &binding)?));
            }
            if let Some(reason) = workbench.integration_global_read_blocker() {
                return Err(denied(
                    "control_busy",
                    format!("群组占用门禁未满足：{reason}"),
                ));
            }
            let id = self.reserve(&fields.string("claimant_id")?, members, common)?;
            return Ok((200, self.group_mut(&id, None)?.projection(common)));
        }
        let rest = path
            .strip_prefix("groups/")
            .ok_or_else(|| "未知群组路径".to_owned())?;
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        if method == Method::Get && action.is_empty() {
            return Ok((200, self.group_mut(id, None)?.projection(common)));
        }
        let claimant = fields.string("claimant_id")?;
        let group = self.group_mut(
            id,
            if action == "stop" {
                None
            } else {
                Some(&claimant)
            },
        )?;
        if action.is_empty() && method == Method::Delete {
            fields.reject_unknown(&["instance_id", "claimant_id"])?;
            if !group.released {
                common
                    .book
                    .release(&group.reservation_id, &claimant)
                    .map_err(book_error)?;
                common.invalidate_reservation_tickets(&group.reservation_id);
                group.released = true;
            }
            // release 不是 Stop；固定 Stop 范围、原成员和未决记录都留存。
            return Ok((200, group.projection(common)));
        }
        if method != Method::Post {
            return Err("不支持的群组方法".to_owned().into());
        }
        if action == "operation-ids" {
            fields.reject_unknown(&["instance_id", "claimant_id"])?;
            return Ok((200, group.issue_tickets(common)?));
        }
        if action == "stop" {
            fields.reject_unknown(&["instance_id", "claimant_id", "request_id"])?;
            if fields.string("request_id")? != group.stop_request_id {
                return Err(denied("request_id_conflict", "群组 Stop 编号不匹配原范围"));
            }
            let mut new_submission = false;
            for member in &mut group.members {
                let result = control.admit(
                    common,
                    workbench,
                    member.key.clone(),
                    member.scope.clone(),
                    None,
                    &claimant,
                    &member.stop_request_id,
                    control::Task::Stop,
                    vec![],
                    Some(group.id.clone()),
                );
                new_submission |= result.as_ref().is_ok_and(|(new, _)| *new);
                member.capture(&member.stop_request_id.clone(), result);
            }
            if let Some(ids) = group.barrier.take() {
                for id in ids {
                    control.cancel(&id, common, workbench, "explicit_group_stop");
                }
            }
            return Ok((
                if new_submission { 202 } else { 200 },
                group.results(common),
            ));
        }
        if !matches!(
            action,
            "controls" | "trials" | "trials/target" | "recording/start" | "recording/stop"
        ) {
            return Err("未知群组动作".to_owned().into());
        }
        fields.reject_unknown(&["instance_id", "claimant_id", "requests"])?;
        let requests: Vec<RawFields> = fields.decode_field("requests")?;
        let requests = exact_requests(&group.members, requests)?;
        let mut barrier = Vec::new();
        let mut failed = false;
        let mut new_submissions = 0;
        for (member, request) in group.members.iter_mut().zip(requests) {
            let request_id = request.string("request_id")?;
            let allowed = match action {
                "controls" => &["binding_id", "request_id", "command"][..],
                "trials" => &["binding_id", "request_id", "trial"][..],
                "trials/target" => &["binding_id", "request_id", "trial_id", "mm"][..],
                _ => &["binding_id", "request_id"][..],
            };
            let result = request
                .reject_unknown(allowed)
                .map_err(ApiError::from)
                .and_then(|()| control::decode_task(action, &request))
                .and_then(|(task, parameters)| {
                    control.admit(
                        common,
                        workbench,
                        member.key.clone(),
                        member.scope.clone(),
                        Some(group.reservation_id.clone()),
                        &claimant,
                        &request_id,
                        task,
                        parameters,
                        Some(group.id.clone()),
                    )
                });
            if result.is_err() {
                failed = true;
            }
            if matches!(&result, Ok((true, _))) {
                new_submissions += 1;
            }
            member.capture(&request_id, result);
            barrier.push(request_id);
        }
        if action == "trials" && new_submissions != 0 {
            if failed || new_submissions != group.members.len() {
                for id in &barrier {
                    control.cancel(
                        id,
                        common,
                        workbench,
                        "群组存在成员未获准入或混用历史编号；未开始目标",
                    );
                }
            } else {
                group.barrier = Some(barrier);
            }
        }
        Ok((
            if !failed && new_submissions == 0 {
                200
            } else {
                202
            },
            group.results(common),
        ))
    }
}

pub(super) fn handles(path: &str) -> bool {
    path == "/service/v2/groups" || path.starts_with("/service/v2/groups/")
}

fn validate_members(members: &[(String, MemberKey)]) -> Result<(), ApiError> {
    if !(1..=MAX_MEMBERS).contains(&members.len()) {
        return Err("群组必须明确包含 1..16 个成员".to_owned().into());
    }
    let mut bindings = BTreeSet::new();
    let mut uids = BTreeSet::new();
    for (binding, member) in members {
        member.validate().map_err(book_error)?;
        if binding.is_empty()
            || binding.len() > 256
            || !bindings.insert(binding)
            || !uids.insert(&member.uid)
        {
            return Err("群组不得重复 binding 或物理 UID 别名".to_owned().into());
        }
    }
    Ok(())
}

fn exact_requests(
    members: &[Member],
    requests: Vec<RawFields>,
) -> Result<Vec<RawFields>, ApiError> {
    if requests.len() != members.len() {
        return Err("请求必须包含每个原始成员且恰好一次".to_owned().into());
    }
    let mut by_binding = BTreeMap::new();
    for request in requests {
        let id = request.string("binding_id")?;
        let request_id = request.string("request_id")?;
        if request_id.is_empty() || request_id.len() > 256 {
            return Err("成员 request_id 无效".to_owned().into());
        }
        if !members.iter().any(|member| member.binding_id == id)
            || by_binding.insert(id, request).is_some()
        {
            return Err("请求不得替换、重复或缩小原始成员范围".to_owned().into());
        }
    }
    members
        .iter()
        .map(|member| {
            by_binding
                .remove(&member.binding_id)
                .ok_or_else(|| "缺少原始成员".to_owned().into())
        })
        .collect()
}

impl Member {
    fn capture(&mut self, request_id: &str, result: Result<(bool, Value), ApiError>) {
        match result {
            Ok(_) => {
                self.operation_id = Some(request_id.into());
                self.error = None;
            }
            Err(error) => {
                self.error = Some(json!({"code":error.code,"message":error.message}));
            }
        }
    }
    fn facts(&self, common: &mutations::State) -> (Value, Value) {
        match self.operation_id.as_deref() {
            Some(id) => match common.projection(id) {
                Ok(value) => (value, self.error.clone().unwrap_or(Value::Null)),
                Err(error) => (
                    Value::Null,
                    json!({"code":error.code,"message":error.message}),
                ),
            },
            None => (Value::Null, self.error.clone().unwrap_or(Value::Null)),
        }
    }
}
impl Group {
    fn keys(&self) -> Vec<MemberKey> {
        self.members
            .iter()
            .map(|member| member.key.clone())
            .collect()
    }
    fn issue_tickets(&self, common: &mut mutations::State) -> Result<Value, ApiError> {
        common
            .book
            .authorize(
                &self.reservation_id,
                &self.claimant,
                &self.keys(),
                Access::Control,
            )
            .map_err(book_error)?;
        let now = Instant::now();
        let mut issued: Vec<(String, String, u64)> = Vec::new();
        for member in &self.members {
            if common.ledger.stop_fenced(&member.scope) {
                for (_, id, _) in &issued {
                    let _ = common.ids.invalidate(id);
                    common.tickets.remove(id);
                }
                return Err(denied(
                    "stopped_epoch",
                    "原成员已进入 Stop 代次，不能自动换范围",
                ));
            }
            match common.issue_reservation_ticket(
                &self.reservation_id,
                &self.claimant,
                member.scope.clone(),
                now,
            ) {
                Ok(id) => issued.push((member.binding_id.clone(), id, member.scope.control_epoch)),
                Err(error) => {
                    for (_, id, _) in &issued {
                        let _ = common.ids.invalidate(id);
                        common.tickets.remove(id);
                    }
                    return Err(error);
                }
            }
        }
        Ok(
            json!({"group_id":self.id,"expires_in_ms":30000,"requests":issued.into_iter().map(|(binding_id, request_id, epoch)|json!({"binding_id":binding_id,"request_id":request_id,"control_epoch":epoch.to_string()})).collect::<Vec<_>>() }),
        )
    }
    fn results(&self, common: &mutations::State) -> Value {
        json!({"group_id":self.id,"members":self.members.iter().map(|member| { let (operation,error)=member.facts(common); json!({"binding_id":member.binding_id,"operation":operation,"error":error}) }).collect::<Vec<_>>(),"device_atomicity":false})
    }
    fn projection(&self, common: &mutations::State) -> Value {
        json!({"group_id":self.id,"reservation_id":self.reservation_id,"claimant_id":self.claimant,"stop_request_id":self.stop_request_id,"released":self.released,"members":self.members.iter().map(|member| { let mut value=member_json(&member.key); let (operation,error)=member.facts(common); value["binding_id"]=json!(member.binding_id); value["control_epoch"]=json!(member.scope.control_epoch.to_string()); value["stop_request_id"]=json!(member.stop_request_id); value["operation"]=operation; value["error"]=error; value }).collect::<Vec<_>>(),"device_atomicity":false})
    }
}

#[cfg(test)]
mod tests {
    use super::super::reservations::{EffectDisposition, Permissions};
    use super::*;
    use crate::session::{
        ToolSession,
        trial_tests::{FixtureBehavior, FixtureProbe, session_fixture},
    };

    #[test]
    fn public_group_create_inherits_top_instance_and_stop_replays_original_claimant() {
        let (session, probe) = session_fixture(1, [1; 12], FixtureBehavior::Ready);
        let (uid, run) = session.integration_identity().expect("identity");
        let generation = session.connection_generation().to_string();
        let key = MemberKey::new("i-test", "can:1", &uid, &run, &generation).expect("key");
        let mut wb =
            Workbench::with_test_sessions(ToolSession::new(), BTreeMap::from([(1, session)]));
        wb.v2 = Some(integration::IntegrationState::for_test().for_v2());
        let mut common = mutations::State::new("i-test").expect("common");
        common.control_enabled = true;
        common
            .book
            .set_permissions(
                key,
                Permissions {
                    maintenance: false,
                    control: true,
                },
            )
            .expect("grant");
        wb.mutations = Some(common);
        probe.inject_idle(20);
        wb.tick();
        let (status,binding)=super::super::integration::tests::call(&mut wb,"POST","/service/v2/bindings",Some(json!({"api_version":2,"instance_id":"i-test","target_id":"can:1","intent":"attach","expected_identity":{"uid":uid,"run_nonce":run},"connection_generation":generation})),true,false).expect("attach route");
        assert_eq!(status, 200);
        let (status,group)=super::super::integration::tests::call(&mut wb,"POST","/service/v2/groups",Some(json!({"api_version":2,"instance_id":"i-test","claimant_id":"first","members":[{"binding_id":binding["binding_id"],"expected_identity":{"uid":uid,"run_nonce":run},"connection_generation":generation}]})),true,false).expect("create route");
        assert_eq!(status, 200, "{group}");
        let path = format!(
            "/service/v2/groups/{}/stop",
            group["group_id"].as_str().expect("group id")
        );
        for (claimant, expected_status) in [("first", 202), ("other", 200)] {
            let (status,response)=super::super::integration::tests::call(&mut wb,"POST",&path,Some(json!({"api_version":2,"instance_id":"i-test","claimant_id":claimant,"request_id":group["stop_request_id"]})),true,false).expect("stop route");
            assert_eq!(status, expected_status, "{response}");
            assert_eq!(response["members"][0]["operation"]["claimant_id"], "first");
        }
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
    }

    fn key(index: usize) -> MemberKey {
        MemberKey::new(
            "instance",
            &format!("can:{index}"),
            &format!("uid-{index}"),
            "run",
            "1",
        )
        .expect("valid synthetic member identity")
    }
    fn common(keys: &[MemberKey]) -> mutations::State {
        let mut common =
            mutations::State::new("instance").expect("initialize synthetic mutation owner");
        common.control_enabled = true;
        for key in keys {
            common
                .book
                .set_permissions(
                    key.clone(),
                    Permissions {
                        maintenance: false,
                        control: true,
                    },
                )
                .expect("grant synthetic member control");
        }
        common
    }
    fn reserve(keys: &[MemberKey], common: &mut mutations::State) -> (State, String) {
        let mut state = State::new();
        let id = state
            .reserve(
                "agent",
                keys.iter()
                    .enumerate()
                    .map(|(i, key)| (format!("binding-{i}"), key.clone()))
                    .collect(),
                common,
            )
            .expect("reserve fixed synthetic group");
        (state, id)
    }
    #[allow(clippy::too_many_arguments)] // 在合成测试中对应 owner 路由。
    fn call(
        state: &mut State,
        id: &str,
        action: &str,
        method: Method,
        body: Value,
        common: &mut mutations::State,
        control: &mut control::State,
        workbench: &mut Workbench,
    ) -> Result<(u16, Value), ApiError> {
        let suffix = if action.is_empty() {
            String::new()
        } else {
            format!("/{action}")
        };
        state.route(
            &format!("/service/v2/groups/{id}{suffix}"),
            method,
            &RawFields::parse(&body.to_string()).expect("decode fixed synthetic JSON request"),
            &mut integration::IntegrationState::for_test(),
            common,
            control,
            workbench,
        )
    }
    fn empty_workbench() -> Workbench {
        Workbench::with_test_sessions(ToolSession::new(), BTreeMap::new())
    }
    fn body() -> Value {
        json!({"instance_id":"instance","claimant_id":"agent"})
    }

    #[test]
    fn reserve_is_atomic_when_a_later_member_is_busy_or_not_granted() {
        let keys = [key(1), key(2)];
        for busy in [false, true] {
            let mut common = common(&keys[..if busy { 2 } else { 1 }]);
            if busy {
                common
                    .book
                    .reserve("other", vec![keys[1].clone()], Access::Control)
                    .expect("reserve competing synthetic writer");
            }
            let mut groups = State::new();
            assert!(
                groups
                    .reserve(
                        "agent",
                        vec![("a".into(), keys[0].clone()), ("b".into(), keys[1].clone())],
                        &mut common
                    )
                    .is_err()
            );
            assert!(groups.groups.is_empty());
            assert!(
                common
                    .book
                    .reserve("other", vec![keys[0].clone()], Access::Control)
                    .is_ok(),
                "failed group must leave no partial writer claim"
            );
        }
    }

    #[test]
    fn explicit_member_limit_and_physical_aliases_are_validated_before_reserving() {
        let keys: Vec<_> = (1..=17).map(key).collect();
        let mut common = common(&keys[..16]);
        let mut groups = State::new();
        assert!(groups.reserve("agent", vec![], &mut common).is_err());
        assert!(
            groups
                .reserve(
                    "agent",
                    keys.iter()
                        .enumerate()
                        .map(|(i, k)| (format!("b-{i}"), k.clone()))
                        .collect(),
                    &mut common
                )
                .is_err()
        );
        let mut alias = keys[0].clone();
        alias.target_id = "usb:alias".into();
        assert!(
            groups
                .reserve(
                    "agent",
                    vec![("can".into(), keys[0].clone()), ("usb".into(), alias)],
                    &mut common
                )
                .is_err()
        );
        assert!(
            groups
                .reserve(
                    "agent",
                    keys[..16]
                        .iter()
                        .enumerate()
                        .map(|(i, k)| (format!("b-{i}"), k.clone()))
                        .collect(),
                    &mut common
                )
                .is_ok()
        );
    }

    #[test]
    fn independent_tickets_reorder_exact_requests_and_reject_scope_substitution() {
        let keys = [key(1), key(2)];
        let mut common = common(&keys);
        let (state, id) = reserve(&keys, &mut common);
        let group = &state.groups[&id];
        let tickets = group
            .issue_tickets(&mut common)
            .expect("issue independent fixed group tickets");
        let requests = tickets["requests"]
            .as_array()
            .expect("fixed request or result member array");
        assert_ne!(requests[0]["request_id"], requests[1]["request_id"]);
        assert_ne!(requests[0]["control_epoch"], requests[1]["control_epoch"]);
        assert_ne!(
            group.members[0].stop_request_id,
            group.members[1].stop_request_id
        );
        assert!(
            group
                .members
                .iter()
                .all(|member| member.scope.members == vec![member.key.clone()])
        );
        let decode = |values: &[Value]| {
            values
                .iter()
                .map(|value| {
                    RawFields::parse(&value.to_string())
                        .expect("decode fixed synthetic JSON request")
                })
                .collect()
        };
        let reordered = exact_requests(
            &group.members,
            decode(&[requests[1].clone(), requests[0].clone()]),
        )
        .expect("reorder every original member request");
        assert_eq!(
            reordered[0]
                .string("binding_id")
                .expect("original binding identifier"),
            "binding-0"
        );
        assert!(exact_requests(&group.members, decode(&requests[..1])).is_err());
        assert!(
            exact_requests(
                &group.members,
                decode(&[requests[0].clone(), requests[0].clone()])
            )
            .is_err()
        );
        let mut substitute = requests[1].clone();
        substitute["binding_id"] = json!("another-target");
        assert!(
            exact_requests(&group.members, decode(&[requests[0].clone(), substitute])).is_err()
        );
    }

    #[test]
    fn release_preserves_original_stop_scope_and_unknown_uid_pin_without_stop() {
        let keys = [key(1), key(2)];
        let mut common = common(&keys);
        let (mut groups, id) = reserve(&keys, &mut common);
        let original = groups.groups[&id].projection(&common);
        let reservation = groups.groups[&id].reservation_id.clone();
        groups.groups[&id]
            .issue_tickets(&mut common)
            .expect("issue independent fixed group tickets");
        common
            .book
            .begin_member_pending(&reservation, "unknown", "agent", &keys[0], Access::Control)
            .expect("pin synthetic unknown member operation");
        common
            .book
            .finish_pending(
                &reservation,
                "unknown",
                &keys[0],
                EffectDisposition::Unresolved,
            )
            .expect("retain unresolved original effect");
        let mut control = control::State::new();
        let (_, released) = call(
            &mut groups,
            &id,
            "",
            Method::Delete,
            body(),
            &mut common,
            &mut control,
            &mut empty_workbench(),
        )
        .expect("release synthetic group without Stop");
        assert_eq!(released["released"], true);
        assert_eq!(released["members"], original["members"]);
        assert_eq!(released["stop_request_id"], original["stop_request_id"]);
        assert!(!control.busy(), "release never queues Stop");
        assert!(common.tickets.is_empty());
        assert!(
            common
                .book
                .reserve("other", vec![keys[0].clone()], Access::Control)
                .is_err(),
            "unknown retains original physical UID"
        );
        assert!(
            common
                .book
                .reserve("other", vec![keys[1].clone()], Access::Control)
                .is_ok()
        );
        assert!(groups.groups[&id].issue_tickets(&mut common).is_err());
    }

    struct Fixture {
        groups: State,
        id: String,
        common: mutations::State,
        control: control::State,
        workbench: Workbench,
        probes: Vec<FixtureProbe>,
    }
    fn fixture(behaviors: [FixtureBehavior; 2]) -> Fixture {
        let mut sessions = BTreeMap::new();
        let mut probes = Vec::new();
        let mut keys = Vec::new();
        for (i, behavior) in behaviors.into_iter().enumerate() {
            let node = (i + 1) as u8;
            let (session, probe) = session_fixture(node, [node; 12], behavior);
            let (uid, nonce) = session
                .integration_identity()
                .expect("fixture has identified original UID and nonce");
            keys.push(
                MemberKey::new(
                    "instance",
                    &format!("can:{node}"),
                    &uid,
                    &nonce,
                    &session.connection_generation().to_string(),
                )
                .expect("identified synthetic member scope"),
            );
            probes.push(probe);
            sessions.insert(node, session);
        }
        let mut common = common(&keys);
        let (groups, id) = reserve(&keys, &mut common);
        Fixture {
            groups,
            id,
            common,
            control: control::State::new(),
            workbench: Workbench::with_test_sessions(ToolSession::new(), sessions),
            probes,
        }
    }
    fn trial_body(f: &mut Fixture) -> Value {
        let tickets = f.groups.groups[&f.id]
            .issue_tickets(&mut f.common)
            .expect("issue independent fixed group tickets");
        let mut body = body();
        body["requests"] = tickets["requests"].clone();
        for request in body["requests"]
            .as_array_mut()
            .expect("fixed mutable member request array")
        {
            request
                .as_object_mut()
                .expect("fixed member request object")
                .remove("control_epoch");
            request["trial"] = json!({"command":{"action":"position","mm":1},"envelope":{"position_min_mm":-10,"position_max_mm":10,"velocity_abs_max_mm_s":2,"force_abs_max_n":10,"stiffness_max_n_per_mm":10,"damping_max_ns_per_mm":10,"duration_max_s":5},"duration_s":5});
        }
        body
    }
    fn tick(f: &mut Fixture) {
        f.workbench.tick();
        f.control.poll(&mut f.common, &mut f.workbench);
        f.groups
            .poll(&mut f.control, &mut f.common, &mut f.workbench);
    }

    #[test]
    fn group_stop_reports_every_original_member_even_after_release_and_disconnect() {
        let mut f = fixture([FixtureBehavior::Ready; 2]);
        call(
            &mut f.groups,
            &f.id,
            "",
            Method::Delete,
            body(),
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("release original group before disconnect");
        f.workbench
            .integration_session_mut("can:2")
            .expect("original second CAN member session")
            .disconnect()
            .expect("disconnect synthetic original member");
        let mut request = body();
        request["request_id"] = json!(f.groups.groups[&f.id].stop_request_id);
        let (_, results) = call(
            &mut f.groups,
            &f.id,
            "stop",
            Method::Post,
            request,
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("admit group Stop over original members");
        let members = results["members"]
            .as_array()
            .expect("fixed request or result member array");
        assert_eq!(members.len(), 2);
        assert!(members[0]["operation"].is_object());
        assert_eq!(members[1]["error"]["code"], "binding_stale");
        assert_eq!(results["device_atomicity"], false);
        assert_eq!(f.groups.groups[&f.id].keys().len(), 2);
    }

    #[test]
    fn partial_admission_cancels_group_trial_before_any_target() {
        let mut f = fixture([FixtureBehavior::Ready; 2]);
        let mut request = trial_body(&mut f);
        request["requests"][1]["trial"]["command"]["action"] = json!("unsupported");
        let (_, results) = call(
            &mut f.groups,
            &f.id,
            "trials",
            Method::Post,
            request,
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("report partial trial admission");
        assert!(results["members"][1]["error"].is_object());
        assert!(!f.control.busy());
        for probe in &f.probes {
            assert_eq!(
                probe
                    .observed
                    .lock()
                    .expect("synthetic device observations lock")
                    .targets,
                0
            );
        }
    }

    #[test]
    fn exact_group_replay_uses_original_receipts_and_changed_raw_intent_conflicts() {
        let mut f = fixture([FixtureBehavior::Ready; 2]);
        let mut request = trial_body(&mut f);
        for member in request["requests"]
            .as_array_mut()
            .expect("fixed mutable member request array")
        {
            member
                .as_object_mut()
                .expect("fixed member request object")
                .remove("trial");
            member["command"] = json!({"action":"position","mm":1});
        }
        let (status, original) = call(
            &mut f.groups,
            &f.id,
            "controls",
            Method::Post,
            request.clone(),
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("admit original group control intents");
        assert_eq!(status, 202);
        assert!(
            original["members"]
                .as_array()
                .expect("fixed request or result member array")
                .iter()
                .all(|member| member["operation"].is_object())
        );
        let (status, replay) = call(
            &mut f.groups,
            &f.id,
            "controls",
            Method::Post,
            request.clone(),
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("replay original group control receipts");
        assert_eq!(status, 200);
        assert_eq!(original, replay);
        request["requests"][0]["command"]["mm"] = json!(2);
        let (_, changed) = call(
            &mut f.groups,
            &f.id,
            "controls",
            Method::Post,
            request,
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("report changed raw intent conflict");
        assert_eq!(
            changed["members"][0]["error"]["code"],
            "request_id_conflict"
        );
        assert_eq!(
            changed["members"][0]["operation"], original["members"][0]["operation"],
            "conflicting request preserves prior receipt"
        );
        for probe in &f.probes {
            assert_eq!(
                probe
                    .observed
                    .lock()
                    .expect("synthetic device observations lock")
                    .targets,
                0
            );
        }
    }

    #[test]
    fn missing_member_request_id_fails_before_any_admission() {
        let mut f = fixture([FixtureBehavior::Ready; 2]);
        let mut request = trial_body(&mut f);
        request["requests"][1]
            .as_object_mut()
            .expect("fixed member request object")
            .remove("request_id");
        assert!(
            call(
                &mut f.groups,
                &f.id,
                "trials",
                Method::Post,
                request,
                &mut f.common,
                &mut f.control,
                &mut f.workbench
            )
            .is_err()
        );
        assert!(!f.control.busy());
        assert!(
            f.groups.groups[&f.id]
                .members
                .iter()
                .all(|member| member.operation_id.is_none())
        );
    }

    #[test]
    fn partial_unknown_submission_is_per_member_and_release_cannot_transfer_it() {
        let mut f = fixture([FixtureBehavior::Ready, FixtureBehavior::StartUnknown]);
        let mut request = trial_body(&mut f);
        for member in request["requests"]
            .as_array_mut()
            .expect("fixed mutable member request array")
        {
            member
                .as_object_mut()
                .expect("fixed member request object")
                .remove("trial");
            member["command"] = json!({"action":"position","mm":1});
        }
        call(
            &mut f.groups,
            &f.id,
            "controls",
            Method::Post,
            request.clone(),
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("admit per-member group control submission");
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while f.control.busy() && Instant::now() < deadline {
            tick(&mut f);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!f.control.busy());
        let results = f.groups.groups[&f.id].results(&f.common);
        assert_eq!(
            results["members"][0]["operation"]["submission"],
            "locally_submitted"
        );
        assert_eq!(results["members"][1]["operation"]["submission"], "unknown");
        assert_eq!(
            results["members"][1]["operation"]["control_effect"],
            "unresolved"
        );
        let submitted: Vec<_> = f
            .probes
            .iter()
            .map(|probe| {
                probe
                    .observed
                    .lock()
                    .expect("synthetic device observations lock")
                    .targets
            })
            .collect();
        let (status, _) = call(
            &mut f.groups,
            &f.id,
            "controls",
            Method::Post,
            request,
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("replay original unknown group receipt");
        assert_eq!(
            status, 200,
            "unknown replay reads receipt without resubmitting"
        );
        call(
            &mut f.groups,
            &f.id,
            "",
            Method::Delete,
            body(),
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("release unknown original group scope");
        let unknown_member = f.groups.groups[&f.id].members[1].key.clone();
        assert!(
            f.common
                .book
                .reserve("other", vec![unknown_member], Access::Control)
                .is_err()
        );
        assert_eq!(
            f.probes
                .iter()
                .map(|probe| probe
                    .observed
                    .lock()
                    .expect("synthetic device observations lock")
                    .targets)
                .collect::<Vec<_>>(),
            submitted
        );
    }

    #[test]
    fn revoked_group_keeps_original_members_and_cancels_all_waiting_trials() {
        let mut f = fixture([FixtureBehavior::Ready; 2]);
        let request = trial_body(&mut f);
        call(
            &mut f.groups,
            &f.id,
            "trials",
            Method::Post,
            request,
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("admit group trials before revocation");
        let original = f.groups.groups[&f.id].keys();
        f.common
            .book
            .set_permissions(original[1].clone(), Permissions::default())
            .expect("revoke second original member control grant");
        f.groups
            .poll(&mut f.control, &mut f.common, &mut f.workbench);
        assert_eq!(f.groups.groups[&f.id].keys(), original);
        assert!(f.groups.groups[&f.id].released);
        assert!(!f.control.busy());
        for probe in &f.probes {
            assert_eq!(
                probe
                    .observed
                    .lock()
                    .expect("synthetic device observations lock")
                    .targets,
                0
            );
        }
    }

    #[test]
    fn prepared_original_members_remain_target_free_until_the_shared_barrier_opens() {
        let mut f = fixture([FixtureBehavior::Ready; 2]);
        for probe in &f.probes {
            probe.inject_idle(1);
        }
        let request = trial_body(&mut f);
        let ids: Vec<String> = request["requests"]
            .as_array()
            .expect("fixed request or result member array")
            .iter()
            .map(|member| {
                member["request_id"]
                    .as_str()
                    .expect("issued member request identifier")
                    .into()
            })
            .collect();
        call(
            &mut f.groups,
            &f.id,
            "trials",
            Method::Post,
            request,
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("admit group trials behind preparation barrier");
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while !ids.iter().all(|id| f.control.trial_prepared(id)) && Instant::now() < deadline {
            f.workbench.tick();
            f.control.poll(&mut f.common, &mut f.workbench);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            ids.iter().all(|id| f.control.trial_prepared(id)),
            "every original member must prepare"
        );
        for probe in &f.probes {
            assert_eq!(
                probe
                    .observed
                    .lock()
                    .expect("synthetic device observations lock")
                    .targets,
                0
            );
        }
        f.groups
            .poll(&mut f.control, &mut f.common, &mut f.workbench);
        assert!(f.groups.groups[&f.id].barrier.is_none());
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while f.control.busy() && Instant::now() < deadline {
            tick(&mut f);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!f.control.busy());
        for probe in &f.probes {
            assert!(
                probe
                    .observed
                    .lock()
                    .expect("synthetic device observations lock")
                    .targets
                    > 0
            );
        }
    }

    #[test]
    fn all_original_preflights_must_succeed_before_group_target_barrier_opens() {
        let mut f = fixture([FixtureBehavior::Ready, FixtureBehavior::PreflightFails]);
        for probe in &f.probes {
            probe.inject_idle(1);
        }
        let request = trial_body(&mut f);
        call(
            &mut f.groups,
            &f.id,
            "trials",
            Method::Post,
            request,
            &mut f.common,
            &mut f.control,
            &mut f.workbench,
        )
        .expect("admit group trial with failing original preflight");
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while f.control.busy() && Instant::now() < deadline {
            tick(&mut f);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            !f.control.busy(),
            "failed preflight must cancel every remaining original trial"
        );
        assert!(f.groups.groups[&f.id].barrier.is_none());
        for probe in &f.probes {
            assert_eq!(
                probe
                    .observed
                    .lock()
                    .expect("synthetic device observations lock")
                    .targets,
                0
            );
        }
    }
}
