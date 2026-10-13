// Copyright The eha-sdk Contributors

//! v2 binding 只读操作的异步 owner。
//!
//! HTTP 请求在这里等待 SDK 的非阻塞 read slot；每次 poll 只驱动一个 target，给
//! Stop、deadline 和其他 owner 工作留出执行权。

use super::{
    integration::{self, CheckedBinding, IntegrationState},
    reservations::MemberKey,
    v2,
    workbench::Workbench,
};
use crate::session::{Command, CommandResult, SessionError};
use serde_json::{Value, json};
use std::{collections::VecDeque, task::Poll};

const CAPACITY: usize = 16;

pub(super) struct State {
    pending: VecDeque<Pending>,
}

struct Pending {
    request: AbortRequest,
    binding: CheckedBinding,
    action: String,
    envelope: Value,
    error_envelope: Value,
}

/// owner 在把请求交给响应写入器前消失时，排队请求必须主动中止。
struct AbortRequest {
    request: Option<tiny_http::Request>,
}

impl AbortRequest {
    fn new(request: tiny_http::Request) -> Self {
        Self {
            request: Some(request),
        }
    }

    fn into_request(mut self) -> tiny_http::Request {
        self.request.take().expect("pending request is present")
    }
}

impl Drop for AbortRequest {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            request.abort();
        }
    }
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(CAPACITY),
        }
    }

    /// 每次 owner tick 只驱动一个 target；待处理读取轮转且绝不等待回复。
    pub(super) fn poll(&mut self, workbench: &mut Workbench) {
        let Some(pending) = self.pending.pop_front() else {
            return;
        };
        let target_id = pending.binding.target_id.clone();
        let result = match workbench.integration_session_mut(&target_id) {
            Some(session) => match session.poll_read() {
                Poll::Pending => {
                    self.pending.push_back(pending);
                    return;
                }
                Poll::Ready(result) => {
                    let unchanged = session.integration_connected()
                        && session.connection_generation().to_string()
                            == pending.binding.connection_generation
                        && session.integration_identity().as_ref()
                            == Some(&(
                                pending.binding.uid.clone(),
                                pending.binding.run_nonce.clone(),
                            ));
                    (result, unchanged)
                }
            },
            None => (Err(session_missing()), false),
        };
        let (result, unchanged) = result;
        if !unchanged {
            let _ = respond_error(pending, 409, "identity_changed", "读取期间身份或代次改变");
            return;
        }
        match result {
            Ok(result) => {
                let _ = respond_success(pending, result, workbench);
            }
            Err(error) => {
                let _ = respond_session_error(pending, error);
            }
        }
    }
}

pub(super) fn handles(path: &str) -> bool {
    binding_id(path).is_some()
}

pub(super) fn dispatch(
    mut request: tiny_http::Request,
    state: &mut IntegrationState,
    workbench: &mut Workbench,
    parsed: Option<Result<crate::RawFields, String>>,
) -> Result<(), String> {
    if request.method() != &tiny_http::Method::Post {
        return integration::respond_error(request, Some(state), 404, "not_found", "未知集成路径");
    }
    let Some(binding_id) = binding_id(request.url()).map(str::to_owned) else {
        return integration::respond_error(request, Some(state), 404, "not_found", "未知集成路径");
    };
    let mut fields = match parsed.unwrap_or_else(|| super::parse_json_body(&mut request)) {
        Ok(fields) => fields,
        Err(message) => {
            let status = if message == "JSON 请求过大" {
                413
            } else {
                400
            };
            return integration::respond_error(
                request,
                Some(state),
                status,
                "bad_request",
                &message,
            );
        }
    };
    if let Err(message) = v2::validate_version(&mut fields) {
        return integration::respond_error(request, Some(state), 400, "bad_request", &message);
    }
    if let Err(message) = fields.reject_unknown(&[
        "api_version",
        "instance_id",
        "connection_generation",
        "expected_identity",
        "action",
    ]) {
        return integration::respond_error(request, Some(state), 400, "bad_request", &message);
    }
    let action = match fields.string("action") {
        Ok(action) => action,
        Err(message) => {
            return integration::respond_error(request, Some(state), 400, "bad_request", &message);
        }
    };
    let command = match command_for(&action) {
        Some(command) => command,
        None => {
            return integration::respond_error(
                request,
                Some(state),
                400,
                "bad_request",
                "未知只读动作",
            );
        }
    };
    let binding = match state.checked_binding(&binding_id, &fields, workbench) {
        Ok(binding) => binding,
        Err(error) => {
            return integration::respond_error(
                request,
                Some(state),
                error.status,
                error.code,
                &error.message,
            );
        }
    };
    let Some(mut reads) = workbench.control_reads.take() else {
        return integration::respond_error(
            request,
            Some(state),
            503,
            "owner_busy",
            "读取 owner 不可用",
        );
    };
    let result = reads.begin(request, state, workbench, binding, action, command);
    workbench.control_reads = Some(reads);
    result
}

impl State {
    fn begin(
        &mut self,
        request: tiny_http::Request,
        state: &IntegrationState,
        workbench: &mut Workbench,
        binding: CheckedBinding,
        action: String,
        command: Command,
    ) -> Result<(), String> {
        if self.pending.len() >= CAPACITY {
            return integration::respond_error(
                request,
                Some(state),
                409,
                "busy",
                "异步读取队列已满",
            );
        }
        if self
            .pending
            .iter()
            .any(|pending| pending.binding.target_id == binding.target_id)
        {
            return integration::respond_error(
                request,
                Some(state),
                409,
                "owner_busy",
                "目标已有待领取的读取",
            );
        }
        if let Some(reason) = workbench.integration_global_read_blocker() {
            return integration::respond_error(
                request,
                Some(state),
                409,
                "read_not_ready",
                &format!("主动读取门禁未就绪：{reason}"),
            );
        }
        let Some(session) = workbench.integration_session_mut(&binding.target_id) else {
            return integration::respond_error(
                request,
                Some(state),
                409,
                "binding_stale",
                "owner 会话已改变",
            );
        };
        if let Err(error) = session.begin_read(command) {
            return respond_session_error_with_state(request, state, error);
        }
        let error_envelope = state.envelope();
        let mut envelope = error_envelope.clone();
        envelope["binding_id"] = json!(binding.binding_id.clone());
        envelope["target_id"] = json!(binding.target_id.clone());
        envelope["connection_generation"] = json!(binding.connection_generation.clone());
        envelope["identity"] =
            json!({"uid":binding.uid.clone(),"run_nonce":binding.run_nonce.clone()});
        envelope["lease_ms"] = json!(binding.lease_ms);
        self.pending.push_back(Pending {
            request: AbortRequest::new(request),
            binding,
            action,
            envelope,
            error_envelope,
        });
        Ok(())
    }
}

fn binding_id(path: &str) -> Option<&str> {
    if path.contains('?') {
        return None;
    }
    let id = path
        .strip_prefix("/service/v2/bindings/")?
        .strip_suffix("/reads")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

fn command_for(action: &str) -> Option<Command> {
    Some(match action {
        "status" => Command::Status,
        "measurements" => Command::Measurements,
        "diagnostics" => Command::Diagnostics,
        "config_startup" => Command::ConfigRead {
            view: "startup".into(),
        },
        "config_factory" => Command::ConfigRead {
            view: "factory".into(),
        },
        "config_user" => Command::ConfigRead {
            view: "user".into(),
        },
        _ => return None,
    })
}

fn respond_success(
    pending: Pending,
    result: CommandResult,
    workbench: &mut Workbench,
) -> Result<(), String> {
    note_config_read(&pending, workbench);
    let mut value = pending.envelope;
    value["action"] = json!(&pending.action);
    value["source_timestamp"] = result.data["sample"]["snapshot_time_us"].as_u64().map_or(
        Value::Null,
        |time| json!({"clock":"device_monotonic_us","value":time.to_string()}),
    );
    value["received_age_ms"] = json!(
        result.data["received_age_ms"]
            .as_u64()
            .map(|age| age.to_string())
    );
    value["native_json_text"] =
        json!(serde_json::to_string(&result.data).map_err(|error| error.to_string())?);
    if pending.action.starts_with("config_") {
        value["data_utf8"] = result.data["data_utf8"].clone();
        value["data_hex"] = result.data["data_hex"].clone();
    }
    v2::snapshot_permits(
        &mut value,
        workbench.integration_global_read_blocker(),
        workbench.has_unresolved_target(),
    );
    integration::respond(pending.request.into_request(), 200, value)
}

fn note_config_read(pending: &Pending, workbench: &mut Workbench) {
    let view = match pending.action.as_str() {
        "config_startup" => "startup",
        "config_user" => "user",
        _ => return,
    };
    let Some(instance_id) = pending.error_envelope["instance_id"].as_str() else {
        return;
    };
    let Ok(member) = MemberKey::new(
        instance_id,
        &pending.binding.target_id,
        &pending.binding.uid,
        &pending.binding.run_nonce,
        &pending.binding.connection_generation,
    ) else {
        return;
    };
    if let Some(mutations) = workbench.mutations.as_mut() {
        mutations.note_config_read(&member, view);
    }
}

fn respond_error(pending: Pending, status: u16, code: &str, message: &str) -> Result<(), String> {
    let mut value = pending.error_envelope;
    value["code"] = json!(code);
    value["message"] = json!(message);
    integration::respond(pending.request.into_request(), status, value)
}

fn respond_session_error(pending: Pending, error: SessionError) -> Result<(), String> {
    let (status, code) = session_error_status(&error);
    let mut value = pending.error_envelope;
    value["code"] = json!(code);
    value["message"] = json!(error.message);
    value["unknown"] = json!(error.unknown);
    value["native_error_json_text"] =
        json!(serde_json::to_string(&error).map_err(|error| error.to_string())?);
    integration::respond(pending.request.into_request(), status, value)
}

fn respond_session_error_with_state(
    request: tiny_http::Request,
    state: &IntegrationState,
    error: SessionError,
) -> Result<(), String> {
    let (status, code) = session_error_status(&error);
    let mut value = state.envelope();
    value["code"] = json!(code);
    value["message"] = json!(error.message);
    value["unknown"] = json!(error.unknown);
    value["native_error_json_text"] =
        json!(serde_json::to_string(&error).map_err(|error| error.to_string())?);
    integration::respond(request, status, value)
}

fn session_error_status(error: &SessionError) -> (u16, &str) {
    match error.kind.as_ref() {
        "timeout" => (504, "sdk_timeout"),
        "owner_busy" => (409, "owner_busy"),
        kind if kind.starts_with("owner_lock_") => (503, kind),
        _ => (502, "sdk_error"),
    }
}

fn session_missing() -> SessionError {
    SessionError {
        kind: "disconnected".into(),
        message: "读取所属 owner 会话已关闭".into(),
        local_stage: Some("may_have_been_submitted".into()),
        unknown: true,
        failure: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{binding_id, command_for, handles};
    use crate::{
        service::{
            integration::{IntegrationState, tests::call},
            workbench::Workbench,
        },
        session::trial_tests::{FixtureBehavior, FixtureProbe, session_fixture},
        session::{Command, ToolSession, TrialEnvelope, TrialRequest},
    };
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpStream,
        sync::mpsc::{self, Receiver, TryRecvError},
        thread,
        time::Duration,
    };
    use tiny_http::Server;

    #[test]
    fn handles_only_plural_v2_binding_reads() {
        assert!(handles("/service/v2/bindings/b-1/reads"));
        assert!(!handles("/service/v2/bindings/b-1/read"));
        assert!(!handles("/service/v2/bindings//reads"));
        assert!(!handles("/service/v2/bindings/b-1/reads?next=1"));
        assert_eq!(binding_id("/service/v2/bindings/b-1/reads"), Some("b-1"));
    }

    #[test]
    fn read_actions_are_fixed_to_six() {
        for action in [
            "status",
            "measurements",
            "diagnostics",
            "config_startup",
            "config_factory",
            "config_user",
        ] {
            assert!(command_for(action).is_some(), "{action}");
        }
        assert!(command_for("stop").is_none());
    }

    fn fixture() -> (Workbench, FixtureProbe) {
        let (can, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let mut workbench =
            Workbench::with_test_sessions(ToolSession::new(), BTreeMap::from([(1, can)]));
        let credentials = IntegrationState::for_test();
        workbench.v2 = Some(credentials.for_v2());
        workbench.integration = Some(credentials);
        workbench.legacy_routes = false;
        (workbench, probe)
    }

    fn ready(workbench: &mut Workbench, probe: &FixtureProbe) {
        probe.inject_idle(31);
        for _ in 0..20 {
            workbench.tick();
            if workbench.integration_global_read_blocker().is_none() {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(workbench.integration_global_read_blocker(), None);
    }

    fn attach(workbench: &mut Workbench) -> (String, Value) {
        let session = workbench
            .integration_session_mut("can:1")
            .expect("fixture session");
        let (uid, run_nonce) = session.integration_identity().expect("fixture identity");
        let fields = json!({
            "api_version":2,"instance_id":"i-test","target_id":"can:1","intent":"attach",
            "expected_identity":{"uid":uid,"run_nonce":run_nonce},
            "connection_generation":session.connection_generation().to_string(),
        });
        let (_, binding) = call(
            workbench,
            "POST",
            "/service/v2/bindings",
            Some(fields.clone()),
            true,
            false,
        )
        .expect("attach");
        (
            binding["binding_id"].as_str().expect("binding id").into(),
            fields,
        )
    }

    fn read_body(fields: &Value) -> Value {
        json!({
            "api_version":2,"instance_id":"i-test","action":"status",
            "expected_identity":fields["expected_identity"],
            "connection_generation":fields["connection_generation"],
        })
    }

    struct PendingResponse {
        _server: Server,
        response: Receiver<std::io::Result<String>>,
    }

    fn begin_read(
        workbench: &mut Workbench,
        path: &str,
        body: Value,
    ) -> Result<PendingResponse, Box<dyn std::error::Error + Send + Sync>> {
        let server = Server::http(("127.0.0.1", 0))?;
        let address = server.server_addr().to_ip().ok_or("no IP socket")?;
        let port = address.port();
        let body = serde_json::to_string(&body)?;
        let request_text = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer test-token\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        );
        let (sent, response) = mpsc::channel();
        thread::spawn(move || {
            let result = (|| -> std::io::Result<String> {
                let mut stream = TcpStream::connect(address)?;
                stream.write_all(request_text.as_bytes())?;
                let mut response = String::new();
                stream.read_to_string(&mut response)?;
                Ok(response)
            })();
            let _ = sent.send(result);
        });
        let request = server.recv()?;
        super::super::v2::dispatch(
            request,
            port,
            workbench,
            &mut super::super::OdriveView::new(None),
            &mut super::super::CanScanView::new(),
            None,
        )?;
        Ok(PendingResponse {
            _server: server,
            response,
        })
    }

    fn finish_read(
        workbench: &mut Workbench,
        pending: PendingResponse,
    ) -> Result<(u16, Value), Box<dyn std::error::Error + Send + Sync>> {
        for _ in 0..10_000 {
            workbench.tick();
            match pending.response.try_recv() {
                Ok(response) => {
                    let response = response?;
                    let (head, body) =
                        response.split_once("\r\n\r\n").ok_or("missing HTTP body")?;
                    let status = head
                        .split_whitespace()
                        .nth(1)
                        .ok_or("missing HTTP status")?
                        .parse()?;
                    return Ok((status, serde_json::from_str(body)?));
                }
                Err(TryRecvError::Empty) => thread::yield_now(),
                Err(TryRecvError::Disconnected) => return Err("HTTP client disconnected".into()),
            }
        }
        Err("asynchronous read did not finish".into())
    }

    #[test]
    fn read_waits_for_owner_tick_then_returns_original_wire() {
        let (mut workbench, probe) = fixture();
        ready(&mut workbench, &probe);
        let (binding, fields) = attach(&mut workbench);
        let pending = begin_read(
            &mut workbench,
            &format!("/service/v2/bindings/{binding}/reads"),
            read_body(&fields),
        )
        .expect("begin async read");
        assert!(matches!(
            pending.response.try_recv(),
            Err(TryRecvError::Empty)
        ));
        let (status, value) = finish_read(&mut workbench, pending).expect("read response");
        assert_eq!(status, 200);
        assert_eq!(value["action"], "status");
        assert!(value["native_json_text"].is_string());
        assert!(value["source_timestamp"]["value"].is_string());
        assert!(value["received_age_ms"].is_string());
    }

    #[test]
    fn cancelling_read_returns_failure_to_the_original_http_request() {
        let (mut workbench, probe) = fixture();
        ready(&mut workbench, &probe);
        let (binding, fields) = attach(&mut workbench);
        let pending = begin_read(
            &mut workbench,
            &format!("/service/v2/bindings/{binding}/reads"),
            read_body(&fields),
        )
        .expect("begin async read");
        assert!(
            workbench
                .integration_session_mut("can:1")
                .expect("fixture session")
                .cancel_read("test_stop_fence")
        );
        let (status, value) = finish_read(&mut workbench, pending).expect("cancel response");
        assert_eq!(status, 502);
        assert_eq!(value["code"], "sdk_error");
        assert!(value["native_error_json_text"].is_string());
    }

    #[test]
    fn v2_read_gate_rejects_unresolved_target_and_active_trial() {
        let (mut workbench, probe) = fixture();
        ready(&mut workbench, &probe);
        let (binding, fields) = attach(&mut workbench);
        workbench
            .integration_session_mut("can:1")
            .expect("fixture session")
            .execute(Command::Position { mm: 1.0 })
            .expect("target submitted");
        let (status, value) = call(
            &mut workbench,
            "POST",
            &format!("/service/v2/bindings/{binding}/reads"),
            Some(read_body(&fields)),
            true,
            false,
        )
        .expect("unresolved read gate");
        assert_eq!(status, 409);
        assert_eq!(value["code"], "read_not_ready");

        let (mut workbench, probe) = fixture();
        ready(&mut workbench, &probe);
        let (binding, fields) = attach(&mut workbench);
        workbench
            .integration_session_mut("can:1")
            .expect("fixture session")
            .start_trial(TrialRequest {
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
            })
            .expect("trial active");
        let (status, value) = call(
            &mut workbench,
            "POST",
            &format!("/service/v2/bindings/{binding}/reads"),
            Some(read_body(&fields)),
            true,
            false,
        )
        .expect("active read gate");
        assert_eq!(status, 409);
        assert_eq!(value["code"], "read_not_ready");
    }
}
