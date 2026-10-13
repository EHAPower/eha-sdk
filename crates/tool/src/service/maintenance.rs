// Copyright The eha-sdk Contributors
//! 固定的 P3 维护准入与 owner 轮询。不包含协议或配置解析器。

use super::{
    Method, Workbench, integration,
    mutations::{Held, Job, Receipt, State},
    operations::{self, Intent},
    reservations::{self, Access, MemberKey, Permissions},
    v2,
};
use crate::{
    RawFields,
    session::{Command, SessionError},
};
use serde_json::{Value, json, value::RawValue};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub(super) const MAX_SCOPES: usize = 16;
pub(super) const MAX_OPERATIONS: usize = 32;
pub(super) const TTL: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(super) struct ApiError {
    pub(super) status: u16,
    pub(super) code: &'static str,
    pub(super) message: String,
}
impl From<String> for ApiError {
    fn from(message: String) -> Self {
        Self {
            status: 400,
            code: "bad_request",
            message,
        }
    }
}
impl From<integration::BindingError> for ApiError {
    fn from(error: integration::BindingError) -> Self {
        Self {
            status: error.status,
            code: error.code,
            message: error.message,
        }
    }
}
pub(super) fn denied(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        status: 409,
        code,
        message: message.into(),
    }
}
pub(super) fn book_error(error: reservations::Error) -> ApiError {
    denied(
        match error {
            reservations::Error::PermissionDenied => "permission_denied",
            reservations::Error::ClaimantMismatch => "claimant_mismatch",
            reservations::Error::NotFound => "reservation_unknown",
            reservations::Error::Busy => "reservation_busy",
            reservations::Error::Capacity | reservations::Error::SequenceExhausted => {
                "operation_capacity"
            }
            _ => "bad_request",
        },
        format!("{error:?}"),
    )
}
pub(super) fn ledger_error(error: operations::Error) -> ApiError {
    denied(
        match error {
            operations::Error::Expired | operations::Error::InvalidId => "request_id_expired",
            operations::Error::Conflict => "request_id_conflict",
            operations::Error::Capacity | operations::Error::SequenceExhausted => {
                "operation_capacity"
            }
            operations::Error::Unresolved | operations::Error::Busy => "reservation_busy",
            _ => "bad_request",
        },
        format!("{error:?}"),
    )
}
pub(super) fn member(
    instance: &str,
    binding: &integration::CheckedBinding,
) -> Result<MemberKey, ApiError> {
    MemberKey::new(
        instance,
        &binding.target_id,
        &binding.uid,
        &binding.run_nonce,
        &binding.connection_generation,
    )
    .map_err(book_error)
}
pub(super) fn member_json(member: &MemberKey) -> Value {
    json!({"target_id":member.target_id,"expected_identity":{"uid":member.uid,"run_nonce":member.run_nonce},"connection_generation":member.connection_generation})
}
fn action(value: &str) -> Result<operations::Action, ApiError> {
    Ok(match value {
        "save_and_restart" => operations::Action::SaveAndRestart,
        "restore_factory_and_restart" => operations::Action::RestoreFactoryAndRestart,
        "reset_application" => operations::Action::ResetApplication,
        "enter_update" => operations::Action::EnterUpdate,
        _ => return Err("未知维护动作".to_owned().into()),
    })
}
pub(super) fn action_name(action: operations::Action) -> &'static str {
    match action {
        operations::Action::SaveAndRestart => "save_and_restart",
        operations::Action::RestoreFactoryAndRestart => "restore_factory_and_restart",
        operations::Action::ResetApplication => "reset_application",
        operations::Action::EnterUpdate => "enter_update",
        operations::Action::HeartbeatStart => "heartbeat_start",
        operations::Action::HeartbeatStop => "heartbeat_stop",
        operations::Action::HeartbeatOnce => "heartbeat_once",
        operations::Action::Position => "position",
        operations::Action::Velocity => "velocity",
        operations::Action::Force => "force",
        operations::Action::Impedance => "impedance",
        operations::Action::TrialStart => "trial_start",
        operations::Action::TrialTarget => "trial_target",
        operations::Action::Stop => "stop",
        operations::Action::TrialStop => "trial_stop",
        operations::Action::RecordingStart => "recording_start",
        operations::Action::RecordingStop => "recording_stop",
    }
}
pub(super) fn command(intent: &Intent) -> Result<Command, ApiError> {
    Ok(match intent.action {
        operations::Action::SaveAndRestart => Command::ConfigSaveAndRestart {
            record: intent
                .parameters
                .iter()
                .find(|(name, _)| name == "config_text")
                .ok_or_else(|| denied("bad_request", "缺少配置原文"))?
                .1
                .clone(),
        },
        operations::Action::RestoreFactoryAndRestart => Command::RestoreFactoryAndRestart,
        operations::Action::ResetApplication => Command::ResetApplication,
        operations::Action::EnterUpdate => Command::EnterUpdate,
        _ => return Err(denied("bad_request", "未知维护动作")),
    })
}

pub(super) fn local_error(message: &str) -> SessionError {
    SessionError {
        kind: "maintenance_busy".into(),
        message: message.into(),
        local_stage: Some("not_submitted".into()),
        unknown: false,
        failure: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}
pub(super) fn unknown_error(message: &str) -> SessionError {
    SessionError {
        kind: "result_unknown".into(),
        message: message.into(),
        local_stage: Some("may_have_been_submitted".into()),
        unknown: true,
        failure: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}
pub(super) fn scope_current(workbench: &mut Workbench, member: &MemberKey) -> bool {
    workbench
        .integration_session_mut(&member.target_id)
        .is_some_and(|session| {
            session.integration_connected()
                && session.connection_generation().to_string() == member.connection_generation
                && session.integration_identity().as_ref()
                    == Some(&(member.uid.clone(), member.run_nonce.clone()))
        })
}

pub(super) fn handles(path: &str) -> bool {
    matches!(
        path,
        "/service/v2/config/schema"
            | "/service/v2/config/validate"
            | "/service/v2/control-reservations"
            | "/service/v2/operation-ids"
    ) || path.starts_with("/service/v2/control-reservations/")
        || path.starts_with("/service/v2/operations/")
        || (path.starts_with("/service/v2/bindings/")
            && (path.ends_with("/permissions") || path.ends_with("/maintenance")))
}

fn permission_projection(
    binding: &integration::CheckedBinding,
    state: &State,
) -> Result<Value, ApiError> {
    let member = member(&state.instance_id, binding)?;
    let grant = state.book.grant(&member);
    let permissions = grant.map_or(Permissions::default(), |grant| grant.permissions);
    let scope = if permissions.control {
        state.scopes.get(&member)
    } else {
        None
    };
    Ok(
        json!({"binding_id":binding.binding_id,"target_id":binding.target_id,"expected_identity":{"uid":binding.uid,"run_nonce":binding.run_nonce},"connection_generation":binding.connection_generation,"grant_id":grant.map(|grant|&grant.id),"control_epoch":scope.map(|scope|scope.control_epoch.to_string()),"stop_request_id":scope.and_then(|scope|state.ledger.stop_id(scope)),"permissions":{"maintenance":permissions.maintenance,"control":permissions.control}}),
    )
}

pub(super) fn route(
    path: &str,
    method: Method,
    fields: &RawFields,
    bindings: &mut integration::IntegrationState,
    state: &mut State,
    workbench: &mut Workbench,
) -> Result<(u16, Value), ApiError> {
    let path = path.strip_prefix("/service/v2/").unwrap_or("");
    if method == Method::Get {
        if let Some(rest) = path.strip_prefix("bindings/")
            && let Some(binding_id) = rest.strip_suffix("/permissions")
        {
            let binding = bindings.checked_current_binding(binding_id, workbench)?;
            return Ok((200, permission_projection(&binding, state)?));
        }

        if path == "config/schema" {
            return Ok((
                200,
                json!({"native_json_text":eha_sdk::config::SCHEMA_JSON,"config_format_version":eha_sdk::config::FORMAT_VERSION}),
            ));
        }
        if let Some(id) = path.strip_prefix("operations/") {
            return Ok((200, json!({"operation":state.projection(id)?})));
        }
        return Err("未知维护路径".to_owned().into());
    }
    if fields.string("instance_id")? != state.instance_id {
        return Err(denied("instance_mismatch", "工具实例已改变"));
    }
    if path == "config/validate" && method == Method::Post {
        fields.reject_unknown(&["instance_id", "config_text"])?;
        eha_sdk::configuration::validate_json(fields.string("config_text")?.as_bytes()).map_err(
            |error| ApiError {
                status: 400,
                code: "invalid_config",
                message: error.to_string(),
            },
        )?;
        return Ok((200, json!({"valid":true})));
    }
    if let Some(rest) = path.strip_prefix("bindings/") {
        let (binding_id, suffix) = rest
            .split_once('/')
            .ok_or_else(|| "未知绑定维护路径".to_owned())?;
        if method != Method::Post {
            return Err("不支持维护方法".to_owned().into());
        }
        let binding = bindings.checked_binding(binding_id, fields, workbench)?;
        let member = member(&state.instance_id, &binding)?;
        if suffix == "permissions" {
            fields.reject_unknown(&[
                "instance_id",
                "expected_identity",
                "connection_generation",
                "permissions",
                "expected_grant_id",
            ])?;
            let expected: Option<String> = fields.decode_field("expected_grant_id")?;
            if expected.as_deref() != state.book.grant(&member).map(|grant| grant.id.as_str()) {
                return Err(denied(
                    "permission_conflict",
                    "授权版本已改变；请先读取完整权限后再明确修改",
                ));
            }
            let permissions: Box<RawValue> = fields.decode_field("permissions")?;
            let permissions =
                RawFields::parse(permissions.get()).map_err(|error| error.to_string())?;
            permissions.reject_unknown(&["maintenance", "control"])?;
            let control = permissions.decode_field::<bool>("control")?;
            if control && !state.control_enabled {
                return Err(denied("unsupported_permission", "控制门槛尚未开放"));
            }
            if control
                && workbench
                    .p2
                    .as_ref()
                    .is_some_and(super::p2::State::connection_busy)
            {
                return Err(denied("connection_busy", "等待连接工作完成后再明确控制"));
            }
            let enabled = permissions.decode_field::<bool>("maintenance")?;
            if (enabled || control)
                && (state.reconnect_required.contains(&member) || state.readback_required(&member))
            {
                return Err(denied(
                    "maintenance_busy",
                    "请先明确重连并核对 User/Startup",
                ));
            }
            let requested = Permissions {
                maintenance: enabled,
                control,
            };
            let previous = state.book.grant(&member).map(|grant| grant.permissions);
            let changed = previous.is_some_and(|permissions| permissions != requested);
            let explicitly_revoked_control =
                previous.is_some_and(|permissions| permissions.control && !requested.control);
            let _control_scope = if control {
                state
                    .book
                    .can_set_permissions(&member, requested)
                    .map_err(book_error)?;
                Some(state.scope_for_control_grant(&member, workbench)?)
            } else {
                None
            };
            state
                .book
                .set_permissions(member.clone(), requested)
                .map_err(book_error)?;
            if explicitly_revoked_control {
                state.note_explicit_control_revoke(&member);
            }
            if control {
                state.note_control_grant(&member);
            }
            if changed {
                state.retire_member_resources(&member, workbench);
            }
            return Ok((200, permission_projection(&binding, state)?));
        }
        if suffix == "maintenance" {
            fields.reject_unknown(&[
                "instance_id",
                "expected_identity",
                "connection_generation",
                "reservation_id",
                "claimant_id",
                "request_id",
                "action",
                "config_text",
            ])?;
            let request_id = fields.string("request_id")?;
            let reservation_id = fields.string("reservation_id")?;
            let claimant = fields.string("claimant_id")?;
            let action = action(&fields.string("action")?)?;
            let raw = fields.optional_string("config_text")?;
            let parameters = if action == operations::Action::SaveAndRestart {
                vec![(
                    "config_text".into(),
                    raw.ok_or_else(|| "save 需要 config_text".to_owned())?,
                )]
            } else {
                if raw.is_some() {
                    return Err("此动作不接受 config_text".to_owned().into());
                }
                vec![]
            };
            let scope = state
                .ledger
                .get(&request_id)
                .map(|record| record.intent.scope.clone())
                .or_else(|| {
                    state
                        .reservations
                        .get(&reservation_id)
                        .map(|held| held.scope.clone())
                })
                .ok_or_else(|| denied("reservation_unknown", "原占用不存在"))?;
            if scope.members != vec![member.clone()] {
                return Err(denied("bad_request", "占用成员已改变"));
            }
            let intent = Intent::new(scope, action, Some(reservation_id.clone()), parameters)
                .map_err(ledger_error)?;
            if let Some(record) = state.ledger.get(&request_id) {
                if record.intent != intent || record.provenance != claimant {
                    return Err(denied(
                        "request_id_conflict",
                        "编号对应的原始参数或会话不一致",
                    ));
                }
                return Ok((
                    200,
                    json!({"operation":state.projection(&request_id)?,"new_submission":false}),
                ));
            }
            if state.jobs.contains_key(&member.target_id) {
                return Err(denied("maintenance_busy", "原维护尚未完成"));
            }
            state
                .book
                .authorize(
                    &reservation_id,
                    &claimant,
                    &intent.scope.members,
                    Access::Maintenance,
                )
                .map_err(book_error)?;
            if let Some(reason) = workbench.integration_global_read_blocker() {
                return Err(denied(
                    "maintenance_busy",
                    format!("维护门禁未满足：{reason}"),
                ));
            }
            if action == operations::Action::SaveAndRestart {
                eha_sdk::configuration::validate_json(intent.parameters[0].1.as_bytes()).map_err(
                    |error| ApiError {
                        status: 400,
                        code: "invalid_config",
                        message: error.to_string(),
                    },
                )?;
            }
            if let Err(error) = state.reserve_readback(&member) {
                let _ = state.ids.invalidate(&request_id);
                state.tickets.remove(&request_id);
                return Err(error);
            }

            if let Err(error) =
                state.admit_normal(&request_id, intent.clone(), &claimant, Instant::now())
            {
                state.release_readback_reservation(&member);
                return Err(error);
            }
            state.tickets.remove(&request_id);
            state
                .book
                .begin_pending(
                    &reservation_id,
                    &request_id,
                    &claimant,
                    &intent.scope.members,
                    Access::Maintenance,
                )
                .map_err(book_error)?;
            state.receipts.insert(
                request_id.clone(),
                Receipt {
                    phase: None,
                    native: None,
                    error: None,
                },
            );
            state.jobs.insert(
                member.target_id,
                Job {
                    id: request_id.clone(),
                    started: false,
                },
            );
            return Ok((
                202,
                json!({"operation":state.projection(&request_id)?,"new_submission":true}),
            ));
        }
    }
    if path == "control-reservations" && method == Method::Post {
        fields.reject_unknown(&["instance_id", "claimant_id", "permission", "members"])?;
        let permission = fields.string("permission")?;
        let access = match permission.as_str() {
            "maintenance" => Access::Maintenance,
            "control" if state.control_enabled => Access::Control,
            _ => return Err(denied("unsupported_permission", "权限尚未开放")),
        };
        let members: Vec<Box<RawValue>> = fields.decode_field("members")?;
        if members.len() != 1 {
            return Err("P3 占用恰好一个明确成员".to_owned().into());
        }
        let mut item = RawFields::parse(members[0].get()).map_err(|error| error.to_string())?;
        item.reject_unknown(&["binding_id", "expected_identity", "connection_generation"])?;
        let binding_id = item.string("binding_id")?;
        item.0
            .insert("instance_id".into(), fields.0["instance_id"].clone());
        let binding = bindings.checked_binding(&binding_id, &item, workbench)?;
        let member = member(&state.instance_id, &binding)?;
        if state.reconnect_required.contains(&member) || state.readback_required(&member) {
            return Err(denied("maintenance_busy", "请先明确重连并读回"));
        }
        if let Some(reason) = workbench.integration_global_read_blocker() {
            return Err(denied(
                "maintenance_busy",
                format!("维护门禁未满足：{reason}"),
            ));
        }
        let reservation = state
            .book
            .reserve(&fields.string("claimant_id")?, vec![member.clone()], access)
            .map_err(book_error)?;
        let scope = match state.scope(&member) {
            Ok(scope) => scope,
            Err(error) => {
                let _ = state.book.release(&reservation.id, &reservation.claimant);
                return Err(error);
            }
        };
        let members = json!([{"binding_id":binding.binding_id,"target_id":member.target_id,"expected_identity":{"uid":member.uid,"run_nonce":member.run_nonce},"connection_generation":member.connection_generation,"grant_id":reservation.grant_ids[0]}]);
        let value = json!({"reservation_id":reservation.id,"claimant_id":reservation.claimant,"permission":permission,"control_epoch":scope.control_epoch.to_string(),"stop_request_id":state.ledger.stop_id(&scope),"members":members});
        state.reservations.insert(reservation.id, Held { scope });
        return Ok((200, value));
    }
    if let Some(id) = path.strip_prefix("control-reservations/")
        && method == Method::Delete
    {
        fields.reject_unknown(&["instance_id", "claimant_id"])?;
        let release = state
            .book
            .release(id, &fields.string("claimant_id")?)
            .map_err(book_error)?;
        state.invalidate_reservation_tickets(id);
        state.reservations.remove(id);
        return Ok((
            200,
            json!({"reservation_id":id,"released":release.released.iter().map(|member|&member.target_id).collect::<Vec<_>>(),"blocked":release.blocked.iter().map(|member|&member.target_id).collect::<Vec<_>>() }),
        ));
    }
    if path == "operation-ids" && method == Method::Post {
        fields.reject_unknown(&["instance_id", "reservation_id", "claimant_id"])?;
        let id = fields.string("reservation_id")?;
        let held = state
            .reservations
            .get(&id)
            .ok_or_else(|| denied("reservation_unknown", "占用不存在"))?;
        state
            .book
            .authorize(
                &id,
                &fields.string("claimant_id")?,
                &held.scope.members,
                state
                    .book
                    .reservation(&id)
                    .ok_or_else(|| denied("reservation_unknown", "占用不存在"))?
                    .access,
            )
            .map_err(book_error)?;
        let now = Instant::now();
        let scope = held.scope.clone();
        let request_id = state.issue_reservation_ticket(
            &id,
            &fields.string("claimant_id")?,
            scope.clone(),
            now,
        )?;
        return Ok((
            200,
            json!({"request_id":request_id,"reservation_id":id,"control_epoch":scope.control_epoch.to_string(),"expires_in_ms":30000}),
        ));
    }
    Err("未知维护路径".to_owned().into())
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
                v2::validate_version(&mut fields)?;
                Ok(fields)
            })
    } else {
        Ok(RawFields(BTreeMap::new()))
    };
    let mut state = workbench
        .mutations
        .take()
        .ok_or("maintenance owner 尚未配置")?;
    let result = fields
        .map_err(ApiError::from)
        .and_then(|fields| route(&path, method, &fields, bindings, &mut state, workbench));
    workbench.mutations = Some(state);
    let (status, value) = match result {
        Ok(value) => value,
        Err(error) => (
            error.status,
            json!({"code":error.code,"message":error.message}),
        ),
    };
    let mut envelope = v2::envelope(&bindings.instance_id);
    if let Some(object) = value.as_object() {
        envelope
            .as_object_mut()
            .ok_or("envelope无效")?
            .extend(object.clone());
    }
    integration::respond(request, status, envelope)
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;
    use crate::session::ToolSession;
    use eha_sdk::host::backend::{Command as BackendCommand, EventSink};
    use std::sync::mpsc::Receiver;

    struct Fixture {
        owner: Workbench,
        bindings: integration::IntegrationState,
        state: State,
        base: Value,
        binding_id: String,
        commands: Receiver<BackendCommand>,
        sink: EventSink,
    }
    fn fixture() -> Fixture {
        let (mut session, commands, sink) = ToolSession::test_maintenance_fixture();
        session.test_usb_request("maint");
        let generation = session.connection_generation().to_string();
        let (uid, run_nonce) = session.integration_identity().expect("test invariant");
        let mut owner = Workbench::with_test_sessions(session, BTreeMap::new());
        let mut bindings = integration::IntegrationState::for_test().for_v2();
        let binding_id = bindings.test_binding(&mut owner, "usb:maint");
        let state = State::new(&bindings.instance_id).expect("test invariant");
        let base = json!({"instance_id":bindings.instance_id,"expected_identity":{"uid":uid,"run_nonce":run_nonce},"connection_generation":generation});
        Fixture {
            owner,
            bindings,
            state,
            base,
            binding_id,
            commands,
            sink,
        }
    }
    fn call(
        f: &mut Fixture,
        path: &str,
        method: Method,
        mut value: Value,
    ) -> Result<(u16, Value), ApiError> {
        if method == Method::Post
            && path.ends_with("/permissions")
            && value.get("expected_grant_id").is_none()
        {
            let binding = f
                .bindings
                .checked_current_binding(&f.binding_id, &mut f.owner)?;
            let member = member(&f.state.instance_id, &binding)?;
            value["expected_grant_id"] = json!(f.state.book.grant(&member).map(|grant| &grant.id));
        }
        let raw = if method == Method::Get {
            "{}".to_owned()
        } else {
            value.to_string()
        };
        let fields = RawFields::parse(&raw).expect("test invariant");
        route(
            path,
            method,
            &fields,
            &mut f.bindings,
            &mut f.state,
            &mut f.owner,
        )
    }
    fn prepared(f: &mut Fixture) -> (String, String, Value) {
        let mut grant = f.base.clone();
        grant["permissions"] = json!({"maintenance":true,"control":false});
        let path = format!("/service/v2/bindings/{}/permissions", f.binding_id);
        let granted = call(f, &path, Method::Post, grant)
            .expect("test invariant")
            .1;
        assert!(granted["grant_id"].as_str().is_some());
        let held=call(f,"/service/v2/control-reservations",Method::Post,json!({"instance_id":f.bindings.instance_id,"claimant_id":"trusted-agent","permission":"maintenance","members":[{"binding_id":f.binding_id,"expected_identity":f.base["expected_identity"],"connection_generation":f.base["connection_generation"]}]})).expect("test invariant").1;
        let reservation = held["reservation_id"]
            .as_str()
            .expect("test invariant")
            .to_owned();
        let issued=call(f,"/service/v2/operation-ids",Method::Post,json!({"instance_id":f.bindings.instance_id,"claimant_id":"trusted-agent","reservation_id":reservation})).expect("test invariant").1;
        let id = issued["request_id"]
            .as_str()
            .expect("test invariant")
            .to_owned();
        let mut request = f.base.clone();
        request["claimant_id"] = json!("trusted-agent");
        request["reservation_id"] = json!(reservation);
        request["request_id"] = json!(id);
        request["action"] = json!("reset_application");
        (reservation, id, request)
    }
    #[test]
    fn sdk_owner_maintenance_known_submit_requires_reconnect_and_replay_is_receipt_only() {
        let mut f = fixture();
        let (_, id, request) = prepared(&mut f);
        let path = format!("/service/v2/bindings/{}/maintenance", f.binding_id);
        let accepted = call(&mut f, &path, Method::Post, request.clone()).expect("test invariant");
        assert_eq!(accepted.0, 202);
        assert_eq!(accepted.1["operation"]["submission"], "not_attempted");
        assert!(f.commands.try_recv().is_err());
        assert_eq!(
            call(&mut f, &path, Method::Post, request.clone())
                .expect("test invariant")
                .1["new_submission"],
            false
        );
        f.state.poll(&mut f.owner);
        let BackendCommand::Send(sent) = f
            .commands
            .recv_timeout(Duration::from_secs(1))
            .expect("test invariant")
        else {
            panic!("maintenance send expected");
        };
        f.sink.submitted(sent.id);
        f.state.poll(&mut f.owner);
        let receipt = f.state.projection(&id).expect("test invariant");
        assert_eq!(receipt["submission"], "locally_submitted");
        assert_eq!(receipt["control_effect"], "not_applicable");
        assert_eq!(receipt["observation"], "unconfirmed");
        assert_eq!(receipt["follow_up"]["reconnect_required"], true);
        assert!(
            receipt["native_json_text"]
                .as_str()
                .expect("test invariant")
                .contains("local_submission")
        );
        assert_eq!(
            call(&mut f, &path, Method::Post, request)
                .expect("test invariant")
                .1["new_submission"],
            false
        );
        assert!(f.commands.try_recv().is_err());
        let mut grant = f.base.clone();
        grant["permissions"] = json!({"maintenance":true,"control":false});
        let path = format!("/service/v2/bindings/{}/permissions", f.binding_id);
        assert_eq!(
            call(&mut f, &path, Method::Post, grant)
                .expect_err("expected rejection")
                .code,
            "maintenance_busy"
        );
    }
    #[test]
    fn revoked_grant_cannot_revive_old_ticket_and_queued_release_never_sends() {
        let mut f = fixture();
        let (reservation, _, request) = prepared(&mut f);
        let permissions_path = format!("/service/v2/bindings/{}/permissions", f.binding_id);
        let mut grant = f.base.clone();
        grant["permissions"] = json!({"maintenance":false,"control":false});
        call(&mut f, &permissions_path, Method::Post, grant.clone()).expect("test invariant");
        grant["permissions"]["maintenance"] = json!(true);
        call(&mut f, &permissions_path, Method::Post, grant).expect("test invariant");
        let path = format!("/service/v2/bindings/{}/maintenance", f.binding_id);
        assert_eq!(
            call(&mut f, &path, Method::Post, request)
                .expect_err("expected rejection")
                .code,
            "reservation_unknown"
        );
        assert!(f.commands.try_recv().is_err());
        assert!(f.state.book.reservation(&reservation).is_none());
        let (reservation, _, request) = prepared(&mut f);
        call(&mut f, &path, Method::Post, request).expect("test invariant");
        let instance = f.bindings.instance_id.clone();
        call(
            &mut f,
            &format!("/service/v2/control-reservations/{reservation}"),
            Method::Delete,
            json!({"instance_id":instance,"claimant_id":"trusted-agent"}),
        )
        .expect("test invariant");
        f.state.poll(&mut f.owner);
        assert!(f.commands.try_recv().is_err());
        let (_, _, _) = prepared(&mut f);
    }
    #[test]
    fn owner_disconnect_revocation_preserves_prior_local_submission() {
        let mut f = fixture();
        let (_, id, request) = prepared(&mut f);
        let path = format!("/service/v2/bindings/{}/maintenance", f.binding_id);
        call(&mut f, &path, Method::Post, request).expect("admitted reset");
        f.owner.mutations = Some(f.state);
        f.owner.tick();
        let BackendCommand::Send(sent) = f
            .commands
            .recv_timeout(Duration::from_secs(1))
            .expect("started reset")
        else {
            panic!("maintenance send expected");
        };
        f.sink.submitted(sent.id);
        f.sink
            .disconnected("synthetic reset closes original transport");
        f.owner.tick();
        let state = f.owner.mutations.as_mut().expect("common owner state");
        let receipt = state.projection(&id).expect("original operation");
        assert_eq!(receipt["submission"], "locally_submitted", "{receipt}");
        assert_eq!(receipt["control_effect"], "not_applicable");
        assert_eq!(receipt["observation"], "unconfirmed");
        let native: Value = serde_json::from_str(
            receipt["native_json_text"]
                .as_str()
                .expect("native evidence"),
        )
        .expect("native JSON");
        assert_eq!(native["local_submission"]["id"], sent.id);
        assert!(state.jobs.is_empty());
        let original = state
            .ledger
            .get(&id)
            .expect("original operation")
            .intent
            .scope
            .members[0]
            .clone();
        assert!(state.book.claim(&original.uid).is_some());
        let mut fresh = original.clone();
        fresh.connection_generation = "after-reset".into();
        fresh.run_nonce = "after-reset".into();
        state.note_config_read(&fresh, "user");
        assert!(
            state.book.claim(&original.uid).is_some(),
            "one view cannot release the known maintenance claim"
        );
        state.note_config_read(&fresh, "startup");
        assert!(
            state.book.claim(&original.uid).is_none(),
            "the required new MemberKey User/Startup pair releases only this known claim"
        );
        assert_eq!(
            state.projection(&id).expect("retained operation")["submission"],
            "locally_submitted",
            "readback releases occupancy, never the original receipt"
        );
        state
            .book
            .set_permissions(
                fresh.clone(),
                Permissions {
                    maintenance: true,
                    control: false,
                },
            )
            .expect("new manual grant");
        state
            .book
            .reserve("trusted-agent", vec![fresh], Access::Maintenance)
            .expect("known claim no longer blocks a new reservation");
        assert!(f.commands.try_recv().is_err(), "no resend");
    }

    #[test]
    fn cancelled_real_submission_pins_unknown_and_late_submitted_cannot_resolve_it() {
        let mut f = fixture();
        let (reservation, id, request) = prepared(&mut f);
        let path = format!("/service/v2/bindings/{}/maintenance", f.binding_id);
        call(&mut f, &path, Method::Post, request).expect("test invariant");
        f.state.poll(&mut f.owner);
        let BackendCommand::Send(sent) = f
            .commands
            .recv_timeout(Duration::from_secs(1))
            .expect("test invariant")
        else {
            panic!("maintenance send expected");
        };
        f.owner
            .integration_session_mut("usb:maint")
            .expect("test invariant")
            .cancel_maintenance("explicit_stop");
        f.state.poll(&mut f.owner);
        let receipt = f.state.projection(&id).expect("test invariant");
        assert_eq!(receipt["submission"], "unknown");
        assert_eq!(receipt["control_effect"], "unresolved");
        assert!(!sent.cancelled.load(std::sync::atomic::Ordering::Acquire));
        f.sink.submitted(sent.id);
        f.state.poll(&mut f.owner);
        assert_eq!(
            f.state.projection(&id).expect("test invariant")["submission"],
            "unknown"
        );
        let instance = f.bindings.instance_id.clone();
        let released = call(
            &mut f,
            &format!("/service/v2/control-reservations/{reservation}"),
            Method::Delete,
            json!({"instance_id":instance,"claimant_id":"trusted-agent"}),
        )
        .expect("test invariant")
        .1;
        assert_eq!(released["blocked"], json!(["usb:maint"]));
        f.state
            .ledger
            .prune(Instant::now() + Duration::from_secs(100));
        assert!(f.state.ledger.get(&id).is_some());
    }
    #[test]
    fn readback_obligation_requires_original_target_and_two_views_of_one_new_scope() {
        let mut f = fixture();
        let (_, id, request) = prepared(&mut f);
        let path = format!("/service/v2/bindings/{}/maintenance", f.binding_id);
        call(&mut f, &path, Method::Post, request).expect("test invariant");
        f.state.poll(&mut f.owner);
        let BackendCommand::Send(sent) = f
            .commands
            .recv_timeout(Duration::from_secs(1))
            .expect("test invariant")
        else {
            panic!("send expected");
        };
        f.sink.submitted(sent.id);
        f.state.poll(&mut f.owner);
        let original = f
            .state
            .ledger
            .get(&id)
            .expect("test invariant")
            .intent
            .scope
            .members[0]
            .clone();
        for view in ["user", "startup"] {
            f.state.note_config_read(&original, view);
        }
        assert!(f.state.readback_required(&original));
        let mut alias = original.clone();
        alias.target_id = "can:1".into();
        alias.connection_generation = "new-generation".into();
        for view in ["user", "startup"] {
            f.state.note_config_read(&alias, view);
        }
        assert!(f.state.readback_required(&original));
        let mut fresh = original.clone();
        fresh.connection_generation = "new-generation".into();
        f.state.note_config_read(&fresh, "user");
        assert!(f.state.readback_required(&fresh));
        let mut later = fresh.clone();
        later.connection_generation = "later-generation".into();
        f.state.note_config_read(&later, "startup");
        assert!(f.state.readback_required(&later));
        f.state.note_config_read(&later, "user");
        assert!(!f.state.readback_required(&later));
        assert_eq!(
            f.state.projection(&id).expect("test invariant")["observation"],
            "unconfirmed"
        );
    }
    #[test]
    fn global_gate_rechecked_after_admission_blocks_actual_sdk_mutation() {
        let mut f = fixture();
        let (_, id, request) = prepared(&mut f);
        let path = format!("/service/v2/bindings/{}/maintenance", f.binding_id);
        call(&mut f, &path, Method::Post, request).expect("test invariant");
        f.owner
            .integration_session_mut("usb:maint")
            .expect("test invariant")
            .test_mark_target_pending();
        f.state.poll(&mut f.owner);
        assert_eq!(
            f.state.projection(&id).expect("test invariant")["submission"],
            "not_submitted"
        );
        while let Ok(command) = f.commands.try_recv() {
            assert!(!matches!(command, BackendCommand::Send(_)));
        }
    }
    #[test]
    fn http_schema_validate_are_authenticated_fixed_routes_without_device_commands()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut f = fixture();
        f.owner.v2 = Some(f.bindings);
        f.owner.mutations = Some(f.state);
        let (status, _) = integration::tests::call(
            &mut f.owner,
            "GET",
            "/service/v2/config/schema",
            None,
            false,
            false,
        )?;
        assert_eq!(status, 401);
        let (status, value) = integration::tests::call(
            &mut f.owner,
            "GET",
            "/service/v2/config/schema",
            None,
            true,
            false,
        )?;
        assert_eq!(status, 200);
        assert_eq!(value["native_json_text"], eha_sdk::config::SCHEMA_JSON);
        assert_eq!(value["api_version"], 2);
        let valid = std::str::from_utf8(include_bytes!("../../../../tests/fixtures/config.json"))?;
        let (status, value) = integration::tests::call(
            &mut f.owner,
            "POST",
            "/service/v2/config/validate",
            Some(json!({"api_version":2,"instance_id":"i-test","config_text":valid})),
            true,
            false,
        )?;
        assert_eq!(status, 200);
        assert_eq!(value["valid"], true);
        let (status, value) = integration::tests::call(
            &mut f.owner,
            "POST",
            "/service/v2/config/validate",
            Some(json!({"api_version":2,"instance_id":"i-test","config_text":"{}"})),
            true,
            false,
        )?;
        assert_eq!(status, 400);
        assert_eq!(value["code"], "invalid_config");
        assert!(f.commands.try_recv().is_err());
        Ok(())
    }
    #[test]
    fn cleanup_ticket_never_crosses_into_maintenance_with_current_or_rotated_grant() {
        for rotate in [false, true] {
            let mut f = fixture();
            f.state.control_enabled = true;
            let permissions_path = format!("/service/v2/bindings/{}/permissions", f.binding_id);
            let mut grant = f.base.clone();
            grant["expected_grant_id"] = Value::Null;
            grant["permissions"] = json!({"maintenance":true,"control":true});
            let first = call(&mut f, &permissions_path, Method::Post, grant.clone())
                .expect("grant")
                .1;
            let scope = f
                .state
                .scopes
                .values()
                .next()
                .expect("explicit scope")
                .clone();
            let member = &scope.members[0];
            let stop_id = f
                .state
                .ledger
                .stop_id(&scope)
                .expect("stop slot")
                .to_owned();
            let cleanup_body = json!({"instance_id":f.bindings.instance_id,"claimant_id":"operator","expected_scope":{"target_id":member.target_id,"expected_identity":{"uid":member.uid,"run_nonce":member.run_nonce},"connection_generation":member.connection_generation,"control_epoch":scope.control_epoch.to_string()}});
            let mut control = super::super::control::State::new();
            let raw = cleanup_body.to_string();
            let issued = super::super::control::route(
                &format!("/service/v2/control-scopes/{stop_id}/cleanup-ids"),
                Method::Post,
                &RawFields::parse(&raw).expect("body"),
                &mut f.bindings,
                &mut f.state,
                &mut control,
                &mut f.owner,
            )
            .expect("cleanup id")
            .1;
            let cleanup_id = issued["request_id"]
                .as_str()
                .expect("cleanup id")
                .to_owned();
            if rotate {
                grant["expected_grant_id"] = first["grant_id"].clone();
                grant["permissions"] = json!({"maintenance":false,"control":false});
                call(&mut f, &permissions_path, Method::Post, grant.clone()).expect("revoke");
                grant["expected_grant_id"] = Value::Null;
                grant["permissions"] = json!({"maintenance":true,"control":true});
                let second = call(&mut f, &permissions_path, Method::Post, grant)
                    .expect("new grant")
                    .1;
                assert_ne!(first["grant_id"], second["grant_id"]);
            }
            let reservation_body = json!({"instance_id":f.bindings.instance_id,"claimant_id":"operator","permission":"maintenance","members":[{"binding_id":f.binding_id,"expected_identity":f.base["expected_identity"],"connection_generation":f.base["connection_generation"]}]});
            let reservation = call(
                &mut f,
                "/service/v2/control-reservations",
                Method::Post,
                reservation_body,
            )
            .expect("maintenance reservation")
            .1;
            let mut mutation = f.base.clone();
            mutation["claimant_id"] = json!("operator");
            mutation["reservation_id"] = reservation["reservation_id"].clone();
            mutation["request_id"] = json!(cleanup_id);
            mutation["action"] = json!("reset_application");
            let path = format!("/service/v2/bindings/{}/maintenance", f.binding_id);
            let error = call(&mut f, &path, Method::Post, mutation)
                .expect_err("restricted cleanup ticket must not submit maintenance");
            assert_eq!(
                error.code,
                if rotate {
                    "request_id_expired"
                } else {
                    "request_id_conflict"
                }
            );
            assert!(f.state.jobs.is_empty());
            assert!(f.state.ledger.get(&cleanup_id).is_none());
            f.state.poll(&mut f.owner);
            assert!(f.commands.try_recv().is_err(), "no SDK mutation bytes");
        }
    }

    #[test]
    fn permissions_get_and_compare_exchange_preserve_new_grant_and_idempotency() {
        let mut f = fixture();
        let path = format!("/service/v2/bindings/{}/permissions", f.binding_id);
        let initial = call(&mut f, &path, Method::Get, Value::Null)
            .expect("metadata")
            .1;
        assert!(initial["grant_id"].is_null());
        let mut request = f.base.clone();
        request["expected_grant_id"] = Value::Null;
        request["permissions"] = json!({"maintenance":true,"control":false});
        let first = call(&mut f, &path, Method::Post, request.clone())
            .expect("grant")
            .1;
        request["expected_grant_id"] = first["grant_id"].clone();
        let repeated = call(&mut f, &path, Method::Post, request.clone())
            .expect("idempotent")
            .1;
        assert_eq!(first["grant_id"], repeated["grant_id"]);
        request["permissions"]["maintenance"] = json!(false);
        call(&mut f, &path, Method::Post, request.clone()).expect("revoke");
        let mut reenable = request.clone();
        reenable["expected_grant_id"] = Value::Null;
        reenable["permissions"]["maintenance"] = json!(true);
        let fresh = call(&mut f, &path, Method::Post, reenable)
            .expect("new grant")
            .1;
        assert_ne!(first["grant_id"], fresh["grant_id"]);
        assert_eq!(
            call(&mut f, &path, Method::Post, request)
                .expect_err("stale CAS rejected")
                .code,
            "permission_conflict"
        );
        assert_eq!(
            call(&mut f, &path, Method::Get, Value::Null)
                .expect("current metadata")
                .1["grant_id"],
            fresh["grant_id"]
        );
        assert!(f.commands.try_recv().is_err());
    }

    #[test]
    fn explicit_new_alias_control_grant_retires_only_a_quiescent_revoked_scope() {
        let mut f = fixture();
        f.state.control_enabled = true;
        let old = MemberKey::new(
            &f.state.instance_id,
            "can:1",
            f.base["expected_identity"]["uid"].as_str().expect("uid"),
            f.base["expected_identity"]["run_nonce"]
                .as_str()
                .expect("nonce"),
            "6",
        )
        .expect("old alias");
        f.state
            .book
            .set_permissions(
                old.clone(),
                Permissions {
                    maintenance: false,
                    control: true,
                },
            )
            .expect("old grant");
        let old_scope = f.state.scope(&old).expect("old scope");
        let old_stop = f
            .state
            .ledger
            .stop_id(&old_scope)
            .expect("old stop")
            .to_owned();
        let old_intent = Intent::new(old_scope.clone(), operations::Action::Stop, None, vec![])
            .expect("old Stop intent");
        let now = Instant::now();
        f.state
            .ledger
            .admit_stop(&old_stop, old_intent, "operator", &mut f.state.ids, now)
            .expect("known old Stop");
        f.state
            .ledger
            .begin_attempt(&old_stop)
            .expect("old Stop attempt");
        f.state
            .ledger
            .finish_attempt(
                &old_stop,
                operations::Submission::LocallySubmitted,
                operations::Effect::Known,
                now,
            )
            .expect("known old Stop");
        f.state
            .book
            .set_permissions(old.clone(), Permissions::default())
            .expect("explicit revoke");
        f.state.note_explicit_control_revoke(&old);

        let path = format!("/service/v2/bindings/{}/permissions", f.binding_id);
        let mut request = f.base.clone();
        request["permissions"] = json!({"maintenance":false,"control":true});
        let granted = call(&mut f, &path, Method::Post, request)
            .expect("new alias control grant")
            .1;

        assert_ne!(granted["stop_request_id"], old_stop);
        assert!(
            granted["control_epoch"]
                .as_str()
                .expect("new epoch")
                .parse::<u64>()
                .expect("numeric epoch")
                > old_scope.control_epoch
        );
        assert!(!f.state.scopes.contains_key(&old));
        assert_eq!(f.state.ledger.stop_id(&old_scope), None);
        assert!(
            f.state.ledger.get(&old_stop).is_some(),
            "known Stop receipt remains readable by its operation id"
        );
        let old_stop_body = json!({
            "instance_id":f.state.instance_id,
            "claimant_id":"operator",
            "expected_scope":{
                "target_id":old.target_id,
                "expected_identity":{"uid":old.uid,"run_nonce":old.run_nonce},
                "connection_generation":old.connection_generation,
                "control_epoch":old_scope.control_epoch.to_string()
            }
        });
        let mut control = super::super::control::State::new();
        let error = super::super::control::route(
            &format!("/service/v2/control-scopes/{old_stop}/stop"),
            Method::Post,
            &RawFields::parse(&old_stop_body.to_string()).expect("old Stop body"),
            &mut f.bindings,
            &mut f.state,
            &mut control,
            &mut f.owner,
        )
        .expect_err("retired scope must reject old canonical Stop POST before ledger replay");
        assert_eq!(error.code, "binding_stale");
        assert!(
            f.commands.try_recv().is_err(),
            "permission changes send no SDK bytes"
        );
    }
}
