// Copyright The eha-sdk Contributors
//! 独立服务 v2 的线格式适配；只读执行和会话门禁复用 v1 核心。

use super::{Method, Workbench, integration, request_method, request_path};
use serde_json::{Value, json};
use std::{path::Path, sync::mpsc, thread, time::Instant};

const PROTOCOL: &str = "eha-tool.service.v2";

pub(super) fn envelope(instance_id: &str) -> Value {
    json!({"protocol":PROTOCOL,"api_version":2,"instance_id":instance_id})
}

pub(super) fn ready(
    port: u16,
    state: &integration::IntegrationState,
    instance_file: &Path,
) -> Value {
    let mut value = envelope(&state.instance_id);
    value["event"] = json!("ready");
    value["host"] = json!("127.0.0.1");
    value["port"] = json!(port);
    value["endpoint"] = json!(state.endpoint);
    value["instance_file"] = json!(instance_file);
    value
}

pub(super) fn describe(instance_id: &str, response_bytes: usize, max_bindings: usize) -> Value {
    let mut value = envelope(instance_id);
    value["tool_version"] = json!(env!("EHA_TOOL_VERSION"));
    value["dto_schema_version"] = json!(1);
    value["config_format_version"] = json!(eha_sdk::config::FORMAT_VERSION);
    value["capabilities"] = json!([
        "instance_status",
        "service_shutdown",
        "targets",
        "binding_attach",
        "binding_lease",
        "binding_renew",
        "usb_connect",
        "snapshot",
        "read_status",
        "read_measurements",
        "read_diagnostics",
        "read_config_startup",
        "read_config_user"
    ]);
    value["limits"] = json!({"request_bytes":65536,"response_bytes":response_bytes,
        "sdk_read_timeout_ms":5000,"max_bindings":max_bindings,"shutdown_response_grace_ms":500,
        "binding_lease_min_ms":integration::MIN_LEASE_MS,"binding_lease_max_ms":integration::MAX_LEASE_MS});
    value["limitations"] = json!({
        "mutations_supported":false,"control_supported":false,
        "can_connect_supported":false,"can_attach_existing_session_only":true,
        "read_gate":"existing_global_readonly_gate",
        "control_effect_evidence":"unresolved_target_latch_only",
        "sdk_read_execution":"synchronous_owner_loop",
        "shutdown_stops_devices":false,"shutdown_response_delivery":"bounded_best_effort",
        "instance_liveness":"authenticated_describe_instance_match",
        "mutation_ledger_supported":false
    });
    value
}

pub(super) fn shared_path(path: &str) -> String {
    let Some(suffix) = path.strip_prefix("/service/v2/") else {
        return String::new();
    };
    // 只允许规范的 reads 复数路径，避免不慎继承 v1 的 /read。
    if suffix.ends_with("/read") {
        return String::new();
    }
    let suffix = suffix
        .strip_suffix("/reads")
        .map(|prefix| format!("{prefix}/read"))
        .unwrap_or_else(|| suffix.to_owned());
    format!("/integration/v1/{suffix}")
}

pub(super) fn validate_version(fields: &mut crate::RawFields) -> Result<(), String> {
    if fields.decode_field::<u64>("api_version")? != 2 {
        return Err("不支持的 api_version；需要版本 2".into());
    }
    fields.0.remove("api_version");
    Ok(())
}

pub(super) fn snapshot_permits(value: &mut Value, blocker: Option<&str>, unresolved_target: bool) {
    value["read_permit"] = json!({"ready":blocker.is_none(),"reason_code":blocker});
    // 读取成功和遥测 Idle 都不能证明历史控制效果；此阶段没有 mutation ledger。
    value["control_effect"] = json!({
        "state":if unresolved_target { "unknown" } else { "not_observed" },
        "reason_code":if unresolved_target { Some("unresolved_target") } else { None }
    });
}

pub(super) fn not_found(
    request: tiny_http::Request,
    state: Option<&integration::IntegrationState>,
) -> Result<(), String> {
    let mut value = envelope(state.map(|state| state.instance_id.as_str()).unwrap_or(""));
    value["code"] = json!("not_found");
    value["message"] = json!("独立服务未提供此路径");
    integration::respond(request, 404, value)
}

pub(super) fn dispatch(
    request: tiny_http::Request,
    port: u16,
    workbench: &mut Workbench,
    parsed_body: Option<Result<crate::RawFields, String>>,
) -> Result<(), String> {
    if let Err((status, code, message)) =
        integration::authorize(&request, port, workbench.v2.as_ref())
    {
        if workbench.v2.is_none() {
            return not_found(request, None);
        }
        return integration::respond_error(request, workbench.v2.as_ref(), status, code, message);
    }
    let mut state = workbench.v2.take().ok_or("集成认证状态已丢失")?;
    let path = request_path(&request).to_owned();
    let method = request_method(&request);
    let result = if path == "/service/v2/instance" && method == Method::Get {
        instance_status(&state, workbench)
            .and_then(|value| integration::respond(request, 200, value))
    } else if path == "/service/v2/shutdown" && method == Method::Post {
        shutdown(request, &mut state, workbench, parsed_body)
    } else {
        state.route(request, workbench, parsed_body)
    };
    workbench.v2 = Some(state);
    result
}

fn instance_status(
    state: &integration::IntegrationState,
    workbench: &mut Workbench,
) -> Result<Value, String> {
    let pending_target = workbench.has_unresolved_target();
    let read_blocker = workbench.integration_global_read_blocker();
    let snapshot = workbench.snapshot_json(super::WebTransport::Usb, None, None, None)?;
    let summary = |id: Value, snapshot: &Value| {
        json!({
            "session_id":id,"connected":snapshot["connected"] == true,
            "heartbeat_active":snapshot["heartbeat"]["enabled"] == true,
            "trial_state":snapshot["trial"]["state"],"recording_active":snapshot["recording"]["active"] == true
        })
    };
    let mut sessions = vec![summary(json!("usb"), &snapshot["sessions"]["usb"])];
    if let Some(nodes) = snapshot["can_nodes"].as_array() {
        for node in nodes {
            sessions.push(summary(
                json!(format!("can:{}", node["node"])),
                &node["snapshot"],
            ));
        }
    }
    let mut value = envelope(&state.instance_id);
    value["tool_version"] = json!(env!("EHA_TOOL_VERSION"));
    value["pid"] = json!(std::process::id().to_string());
    value["started_at_unix_ms"] = json!(state.started_at_unix_ms);
    value["endpoint"] = json!(state.endpoint);
    value["service_state"] = json!(if state.shutdown.is_some() {
        "shutting_down"
    } else {
        "running"
    });
    value["sessions"] = json!(sessions);
    value["activity"] = json!({"trial_active":workbench.has_active_trial(),
        "trial_unresolved":workbench.has_pending_interrupted_trial(),"read_blocker":read_blocker});
    value["control_effect"] = json!({"state":if pending_target { "unknown" } else { "not_observed" },
        "reason_code":if pending_target { Some("unresolved_target") } else { None },"mutation_ledger_supported":false});
    value["device_stop_requested"] = json!(false);
    value["message"] = json!(
        "退出只结束服务会话，不请求设备 Stop；活动心跳、试验、记录和未决效果须由人工判断，不能据此认定设备已停止。"
    );
    Ok(value)
}

fn shutdown(
    mut request: tiny_http::Request,
    state: &mut integration::IntegrationState,
    workbench: &mut Workbench,
    parsed: Option<Result<crate::RawFields, String>>,
) -> Result<(), String> {
    let fields = parsed.unwrap_or_else(|| super::parse_json_body(&mut request));
    let validate = || -> Result<String, String> {
        let fields = fields?;
        fields.reject_unknown(&["api_version", "instance_id", "confirmation"])?;
        if fields.decode_field::<u64>("api_version")? != 2 {
            return Err("不支持的 api_version；需要版本 2".into());
        }
        let instance = fields.decode_field::<String>("instance_id")?;
        if fields.decode_field::<String>("confirmation")?
            != "shutdown_service_without_stopping_devices"
        {
            return Err("退出须明确确认 shutdown_service_without_stopping_devices".into());
        }
        Ok(instance)
    };
    let instance = match validate() {
        Ok(instance) => instance,
        Err(message) => {
            return integration::respond_error(request, Some(state), 400, "bad_request", &message);
        }
    };
    if instance != state.instance_id {
        return integration::respond_error(
            request,
            Some(state),
            409,
            "instance_mismatch",
            "服务实例已改变",
        );
    }
    let mut value = instance_status(state, workbench)?;
    value["service_state"] = json!("shutting_down");
    value["accepted"] = json!(true);
    let bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    let mut response =
        tiny_http::Response::from_data(bytes).with_status_code(tiny_http::StatusCode(202));
    let length = response.data_length().expect("known response length");
    super::common_headers(
        &mut response,
        "application/json; charset=utf-8",
        length,
        None,
    )?;
    let (completed, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("eha-tool-service-shutdown-response".into())
        .spawn(move || {
            let _ = request.respond(response);
            let _ = completed.send(());
        })
        .map_err(|error| error.to_string())?;
    state.shutdown = Some((Instant::now(), receiver));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::integration::{IntegrationState, tests::call};
    use super::*;
    use crate::session::{
        Command, ToolSession,
        trial_tests::{FixtureBehavior, session_fixture},
    };
    use std::collections::BTreeMap;

    fn fixture() -> (Workbench, crate::session::trial_tests::FixtureProbe) {
        let (can, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let mut workbench =
            Workbench::with_test_sessions(ToolSession::new(), BTreeMap::from([(1, can)]));
        let credentials = IntegrationState::for_test();
        workbench.v2 = Some(credentials.for_v2());
        workbench.integration = Some(credentials);
        workbench.legacy_routes = false;
        (workbench, probe)
    }

    fn attach_fields(workbench: &mut Workbench) -> Value {
        let session = workbench
            .integration_session_mut("can:1")
            .expect("fixture session");
        let (uid, run_nonce) = session.integration_identity().expect("fixture identity");
        json!({"api_version":2,"instance_id":"i-test","target_id":"can:1","intent":"attach",
            "expected_identity":{"uid":uid,"run_nonce":run_nonce},
            "connection_generation":session.connection_generation().to_string()})
    }

    #[test]
    fn lifecycle_summary_and_explicit_shutdown_keep_device_effect_unresolved() {
        let (mut workbench, probe) = fixture();
        workbench
            .integration_session_mut("can:1")
            .expect("session")
            .execute(Command::Position { mm: 2.0 })
            .expect("target");
        let (status, value) = call(
            &mut workbench,
            "GET",
            "/service/v2/instance",
            None,
            false,
            false,
        )
        .expect("unauthenticated");
        assert_eq!(status, 401);
        assert_eq!(value["code"], "unauthorized");
        let (status, value) = call(
            &mut workbench,
            "GET",
            "/service/v2/instance",
            None,
            true,
            false,
        )
        .expect("summary");
        assert_eq!(status, 200);
        assert_eq!(value["control_effect"]["state"], "unknown");
        assert_eq!(value["activity"]["read_blocker"], "unresolved_target");
        assert!(value.get("token").is_none());
        assert!(value.get("token_file").is_none());
        let valid = json!({"api_version":2,"instance_id":"i-test","confirmation":"shutdown_service_without_stopping_devices"});
        for (key, replacement, status) in [
            ("api_version", json!(1), 400),
            ("instance_id", json!("stale"), 409),
            ("confirmation", json!(true), 400),
            ("extra", json!(true), 400),
        ] {
            let mut body = valid.clone();
            body[key] = replacement;
            let (actual, _) = call(
                &mut workbench,
                "POST",
                "/service/v2/shutdown",
                Some(body),
                true,
                false,
            )
            .expect("reject");
            assert_eq!(actual, status);
            assert!(workbench.v2.as_ref().expect("state").shutdown.is_none());
        }
        let mut missing = valid.clone();
        missing
            .as_object_mut()
            .expect("object")
            .remove("confirmation");
        assert_eq!(
            call(
                &mut workbench,
                "POST",
                "/service/v2/shutdown",
                Some(missing),
                true,
                false
            )
            .expect("missing")
            .0,
            400
        );
        let (status, value) = call(
            &mut workbench,
            "POST",
            "/service/v2/shutdown",
            Some(valid),
            true,
            false,
        )
        .expect("shutdown");
        assert_eq!(status, 202);
        assert_eq!(value["accepted"], true);
        assert_eq!(value["device_stop_requested"], false);
        assert_eq!(value["control_effect"]["state"], "unknown");
        drop(workbench);
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
    }

    #[test]
    fn shutdown_response_completion_or_deadline_is_bounded() {
        let mut state = IntegrationState::for_test().for_v2();
        let (sender, receiver) = mpsc::channel();
        state.shutdown = Some((Instant::now(), receiver));
        assert!(!state.shutdown_ready());
        // 慢客户端或阻塞的响应线程不能把退出等待扩大到无界。
        state.shutdown.as_mut().expect("shutdown").0 =
            Instant::now() - std::time::Duration::from_millis(501);
        assert!(state.shutdown_ready());
        let (sender2, receiver) = mpsc::channel();
        state.shutdown = Some((Instant::now(), receiver));
        sender2.send(()).expect("response complete");
        assert!(state.shutdown_ready());
        drop(sender);
    }

    #[test]
    fn credentials_version_instance_binding_and_detach_are_enforced() {
        let (mut workbench, _) = fixture();
        for (token, origin, expected) in
            [(false, false, 401), (true, true, 403), (true, false, 200)]
        {
            let (status, value) = call(
                &mut workbench,
                "GET",
                "/service/v2/describe",
                None,
                token,
                origin,
            )
            .expect("describe");
            assert_eq!(status, expected);
            assert_eq!(value["protocol"], PROTOCOL);
            assert_eq!(value["api_version"], 2);
            assert_eq!(value["instance_id"], "i-test");
            if status == 200 {
                assert_eq!(value["limitations"]["control_supported"], false);
                assert!(
                    !value["capabilities"]
                        .as_array()
                        .expect("capabilities")
                        .contains(&json!("stop"))
                );
            }
        }
        let attach = attach_fields(&mut workbench);
        for (key, value, expected_status, expected_code) in [
            ("api_version", json!(1), 400, "bad_request"),
            ("instance_id", json!("stale"), 409, "instance_mismatch"),
            (
                "connection_generation",
                json!("stale"),
                409,
                "binding_stale",
            ),
            (
                "expected_identity",
                json!({"uid":"stale","run_nonce":"stale"}),
                409,
                "identity_changed",
            ),
        ] {
            let mut invalid = attach.clone();
            invalid[key] = value;
            let (status, value) = call(
                &mut workbench,
                "POST",
                "/service/v2/bindings",
                Some(invalid),
                true,
                false,
            )
            .expect("invalid binding");
            assert_eq!(status, expected_status);
            assert_eq!(value["code"], expected_code);
        }
        let (status, binding) = call(
            &mut workbench,
            "POST",
            "/service/v2/bindings",
            Some(attach.clone()),
            true,
            false,
        )
        .expect("attach");
        assert_eq!(status, 200);
        let id = binding["binding_id"].as_str().expect("binding id");
        let (status, snapshot) = call(
            &mut workbench,
            "GET",
            &format!("/service/v2/bindings/{id}/snapshot"),
            None,
            true,
            false,
        )
        .expect("snapshot");
        assert_eq!(status, 200);
        assert_eq!(snapshot["read_permit"]["ready"], false);
        assert_eq!(snapshot["control_effect"]["state"], "not_observed");
        assert!(snapshot.get("readiness").is_none());
        let read = json!({"api_version":2,"instance_id":"i-test","action":"status",
            "expected_identity":attach["expected_identity"],"connection_generation":attach["connection_generation"]});
        let (status, value) = call(
            &mut workbench,
            "POST",
            &format!("/service/v2/bindings/{id}/reads"),
            Some(read),
            true,
            false,
        )
        .expect("read gate");
        assert_eq!(status, 409);
        assert_eq!(value["code"], "read_not_ready");
        let (status, _) = call(
            &mut workbench,
            "DELETE",
            &format!("/service/v2/bindings/{id}"),
            None,
            true,
            false,
        )
        .expect("detach");
        assert_eq!(status, 200);
        assert!(
            workbench
                .integration_session_mut("can:1")
                .expect("retained owner")
                .integration_connected()
        );
        let (status, _) = call(
            &mut workbench,
            "GET",
            &format!("/service/v2/bindings/{id}/snapshot"),
            None,
            true,
            false,
        )
        .expect("stale binding");
        assert_eq!(status, 404);
        let (status, v1) = call(
            &mut workbench,
            "GET",
            "/integration/v1/describe",
            None,
            true,
            false,
        )
        .expect("v1 compatibility");
        assert_eq!(status, 200);
        assert_eq!(v1["protocol"], "eha-tool.integration.v1");
        assert!(v1.get("api_version").is_none());
    }

    #[test]
    fn reads_preserve_native_json_and_only_allow_five_actions() {
        let (mut workbench, probe) = fixture();
        probe.inject_idle(31);
        for _ in 0..20 {
            workbench.tick();
            if workbench.integration_global_read_blocker().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(workbench.integration_global_read_blocker(), None);
        let fields = attach_fields(&mut workbench);
        let (_, binding) = call(
            &mut workbench,
            "POST",
            "/service/v2/bindings",
            Some(fields.clone()),
            true,
            false,
        )
        .expect("attach");
        let id = binding["binding_id"].as_str().expect("binding id");
        let path = format!("/service/v2/bindings/{id}/reads");
        for action in [
            "status",
            "measurements",
            "diagnostics",
            "config_startup",
            "config_user",
            "stop",
            "config_factory",
        ] {
            let read = json!({"api_version":2,"instance_id":"i-test","action":action,
                "expected_identity":fields["expected_identity"],"connection_generation":fields["connection_generation"]});
            let (status, value) =
                call(&mut workbench, "POST", &path, Some(read), true, false).expect("read");
            if matches!(action, "stop" | "config_factory") {
                assert_eq!(status, 400);
                continue;
            }
            assert_eq!(status, 200, "{action}: {value}");
            assert!(value["native_json_text"].is_string());
            assert_eq!(value["action"], action);
        }
        let (status, _) = call(
            &mut workbench,
            "POST",
            &format!("/service/v2/bindings/{id}/read"),
            Some(json!({})),
            true,
            false,
        )
        .expect("singular route");
        assert_eq!(status, 404);
    }

    #[test]
    fn unresolved_control_effect_survives_read_gate_reason_and_snapshot() {
        let (mut workbench, _) = fixture();
        let fields = attach_fields(&mut workbench);
        let (_, binding) = call(
            &mut workbench,
            "POST",
            "/service/v2/bindings",
            Some(fields),
            true,
            false,
        )
        .expect("attach");
        workbench
            .integration_session_mut("can:1")
            .expect("session")
            .execute(Command::Position { mm: 1.0 })
            .expect("synthetic target");
        let path = format!(
            "/service/v2/bindings/{}/snapshot",
            binding["binding_id"].as_str().expect("binding")
        );
        let (_, value) = call(&mut workbench, "GET", &path, None, true, false).expect("snapshot");
        assert_eq!(value["control_effect"]["state"], "unknown");
        assert_eq!(value["control_effect"]["reason_code"], "unresolved_target");
        assert_eq!(value["read_permit"]["ready"], false);
        let mut projected = envelope("i-test");
        snapshot_permits(&mut projected, Some("awaiting_telemetry"), true);
        assert_eq!(projected["control_effect"]["state"], "unknown");
    }
}
