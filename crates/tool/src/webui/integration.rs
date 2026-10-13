// Copyright The eha-sdk Contributors
//! Opt-in, loopback-only read-only projection of the existing WebUI session owner.

use super::{
    Method, Workbench, common_headers, header, request_method, request_path, respond_async,
};
use crate::session::{Command, ConnectionRequest};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};
use tiny_http::{Response, StatusCode};

const PROTOCOL: &str = "eha-tool.integration.v1";
const RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_BINDINGS: usize = 8;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    uid: String,
    run_nonce: String,
}

impl Identity {
    fn json(&self) -> Value {
        json!({"uid":self.uid,"run_nonce":self.run_nonce})
    }
}

#[derive(Clone)]
struct Binding {
    target_id: String,
    generation: String,
    identity: Identity,
}

pub(super) struct IntegrationState {
    token: String,
    instance_id: String,
    bindings: BTreeMap<String, Binding>,
    next_binding: u64,
}

fn random_hex<const N: usize>() -> Result<String, String> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|error| format!("系统随机源不可用：{error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(unix)]
fn token_from_file(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let token = match fs::symlink_metadata(path) {
        Ok(before) => {
            if !before.is_file()
                || before.file_type().is_symlink()
                || before.permissions().mode() & 0o777 != 0o600
            {
                return Err("集成 token 文件必须是权限 0600 的普通文件".into());
            }
            if before.uid() != rustix::process::geteuid().as_raw() {
                return Err("集成 token 文件所有者与当前用户不一致".into());
            }
            if before.len() != 65 {
                return Err("集成 token 文件格式无效".into());
            }
            let mut file = File::open(path).map_err(|error| error.to_string())?;
            let after = file.metadata().map_err(|error| error.to_string())?;
            if before.dev() != after.dev() || before.ino() != after.ino() {
                return Err("集成 token 文件在打开期间被替换".into());
            }
            let mut text = String::new();
            file.read_to_string(&mut text)
                .map_err(|error| error.to_string())?;
            text
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let token = random_hex::<32>()? + "\n";
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|error| error.to_string())?;
            file.write_all(token.as_bytes())
                .map_err(|error| error.to_string())?;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            token
        }
        Err(error) => return Err(error.to_string()),
    };
    let value = token
        .strip_suffix('\n')
        .ok_or("集成 token 文件缺少结尾换行")?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("集成 token 文件格式无效".into());
    }
    Ok(value.into())
}

#[cfg(not(unix))]
fn token_from_file(_path: &Path) -> Result<String, String> {
    Err("本机集成 token 权限校验目前仅支持 Unix".into())
}

impl IntegrationState {
    pub(super) fn new(path: &Path) -> Result<Self, String> {
        Ok(Self {
            token: token_from_file(path)?,
            instance_id: format!("i-{}", random_hex::<16>()?),
            bindings: BTreeMap::new(),
            next_binding: 1,
        })
    }

    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self {
            token: "test-token".into(),
            instance_id: "i-test".into(),
            bindings: BTreeMap::new(),
            next_binding: 1,
        }
    }

    fn envelope(&self) -> Value {
        json!({"protocol":PROTOCOL,"instance_id":self.instance_id})
    }

    fn error(
        &self,
        request: tiny_http::Request,
        status: u16,
        code: &str,
        message: impl Into<String>,
    ) -> Result<(), String> {
        let mut value = self.envelope();
        value["code"] = json!(code);
        value["message"] = json!(message.into());
        respond(request, status, value)
    }

    fn route(
        &mut self,
        mut request: tiny_http::Request,
        workbench: &mut Workbench,
        parsed_body: Option<Result<crate::RawFields, String>>,
    ) -> Result<(), String> {
        let path = request_path(&request).to_owned();
        let method = request_method(&request);
        if path == "/integration/v1/describe" && method == Method::Get {
            let mut value = self.envelope();
            value["tool_version"] = json!(env!("EHA_TOOL_VERSION"));
            value["dto_schema_version"] = json!(1);
            value["config_format_version"] = json!(eha_sdk::config::FORMAT_VERSION);
            value["capabilities"] = json!([
                "targets",
                "binding_attach",
                "usb_connect",
                "snapshot",
                "read_status",
                "read_measurements",
                "read_diagnostics",
                "read_config_startup",
                "read_config_user"
            ]);
            value["limits"] = json!({"request_bytes":65536,"response_bytes":RESPONSE_BYTES,"sdk_read_timeout_ms":5000,"max_bindings":MAX_BINDINGS});
            return respond(request, 200, value);
        }
        if path == "/integration/v1/targets" && method == Method::Get {
            return match workbench.integration_targets() {
                Ok((targets, discovery_reason)) => {
                    let mut value = self.envelope();
                    value["targets"] = json!(targets);
                    value["discovery_limited"] = json!(discovery_reason.is_some());
                    value["discovery_reason"] = json!(discovery_reason);
                    respond(request, 200, value)
                }
                Err(message) => self.error(request, 502, "sdk_error", message),
            };
        }
        if path == "/integration/v1/bindings" && method == Method::Post {
            let body = match body(&mut request, parsed_body) {
                Ok(body) => body,
                Err(message) => {
                    let status = if message == "JSON 请求过大" {
                        413
                    } else {
                        400
                    };
                    return self.error(request, status, "bad_request", message);
                }
            };
            if let Err(message) = body.reject_unknown(&[
                "instance_id",
                "target_id",
                "intent",
                "expected_identity",
                "connection_generation",
            ]) {
                return self.error(request, 400, "bad_request", message);
            }
            let instance_id = match body.string("instance_id") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            if instance_id != self.instance_id {
                return self.error(request, 409, "instance_mismatch", "工具实例已改变");
            }
            let target_id = match body.string("target_id") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            let intent = match body.string("intent") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            if self.bindings.len() >= MAX_BINDINGS {
                return self.error(request, 409, "busy", "绑定数量已达上限");
            }
            match intent.as_str() {
                "attach" => {
                    let expected: Identity = match body.decode_field("expected_identity") {
                        Ok(value) => value,
                        Err(message) => return self.error(request, 400, "bad_request", message),
                    };
                    let expected_generation = match body.string("connection_generation") {
                        Ok(value) => value,
                        Err(message) => return self.error(request, 400, "bad_request", message),
                    };
                    let Some(session) = workbench.integration_session_mut(&target_id) else {
                        return self.error(request, 404, "not_found", "目标不在当前 owner 会话中");
                    };
                    if !session.integration_connected()
                        || session.connection_generation().to_string() != expected_generation
                    {
                        return self.error(request, 409, "binding_stale", "目标连接代次已改变");
                    }
                    if session.integration_identity().as_ref()
                        != Some(&(expected.uid.clone(), expected.run_nonce.clone()))
                    {
                        return self.error(request, 409, "identity_changed", "目标身份已改变");
                    }
                }
                "connect" => {
                    if body.contains("expected_identity") || body.contains("connection_generation")
                    {
                        return self.error(
                            request,
                            400,
                            "bad_request",
                            "USB 新连接不得携带旧身份或代次",
                        );
                    }
                    let Some(serial) = target_id.strip_prefix("usb:") else {
                        return self.error(request, 400, "bad_request", "只允许显式连接 USB 候选");
                    };
                    if workbench.integration_global_read_blocker().is_some() {
                        return self.error(
                            request,
                            409,
                            "read_not_ready",
                            "当前 owner 有未决控制或运行状态，拒绝新连接",
                        );
                    }
                    let devices = match crate::devices::discover() {
                        Ok(devices) => devices,
                        Err(message) => return self.error(request, 502, "sdk_error", message),
                    };
                    if let Err(message) = crate::devices::find_serial(&devices, serial) {
                        return self.error(request, 404, "not_found", message);
                    }
                    if let Err(error) = workbench.connect(ConnectionRequest::Usb {
                        serial: serial.into(),
                    }) {
                        return self.session_error(request, error);
                    }
                }
                _ => return self.error(request, 400, "bad_request", "未知绑定意图"),
            }
            let Some(session) = workbench.integration_session_mut(&target_id) else {
                return self.error(request, 409, "binding_stale", "连接后目标会话不可用");
            };
            let Some((uid, run_nonce)) = session.integration_identity() else {
                return self.error(request, 409, "identity_changed", "连接后身份不可读");
            };
            let binding = Binding {
                target_id: target_id.clone(),
                generation: session.connection_generation().to_string(),
                identity: Identity { uid, run_nonce },
            };
            let binding_id = format!("{}-b{}", self.instance_id, self.next_binding);
            self.next_binding = self.next_binding.saturating_add(1);
            self.bindings.insert(binding_id.clone(), binding.clone());
            let mut value = self.envelope();
            value["binding_id"] = json!(binding_id);
            value["target_id"] = json!(target_id);
            value["connection_generation"] = json!(binding.generation);
            value["identity"] = binding.identity.json();
            return respond(request, 200, value);
        }
        let Some(remainder) = path.strip_prefix("/integration/v1/bindings/") else {
            return self.error(request, 404, "not_found", "未知集成路径");
        };
        let (binding_id, suffix) = remainder.split_once('/').unwrap_or((remainder, ""));
        if binding_id.is_empty() {
            return self.error(request, 404, "not_found", "绑定不存在");
        }
        if suffix.is_empty() && method == Method::Delete {
            if self.bindings.remove(binding_id).is_none() {
                return self.error(request, 404, "not_found", "绑定不存在");
            }
            let mut value = self.envelope();
            value["binding_id"] = json!(binding_id);
            value["detached"] = json!(true);
            return respond(request, 200, value);
        }
        let Some(binding) = self.bindings.get(binding_id).cloned() else {
            return self.error(request, 404, "not_found", "绑定不存在");
        };
        let Some(session) = workbench.integration_session_mut(&binding.target_id) else {
            return self.error(request, 409, "binding_stale", "owner 会话已改变");
        };
        if !session.integration_connected()
            || session.connection_generation().to_string() != binding.generation
        {
            return self.error(request, 409, "binding_stale", "连接代次已改变");
        }
        if session.integration_identity().as_ref()
            != Some(&(
                binding.identity.uid.clone(),
                binding.identity.run_nonce.clone(),
            ))
        {
            return self.error(request, 409, "identity_changed", "运行身份已改变");
        }
        if suffix == "snapshot" && method == Method::Get {
            let blocker = workbench.integration_global_read_blocker();
            let session = workbench
                .integration_session_mut(&binding.target_id)
                .expect("validated session");
            let snapshot = session.snapshot();
            let mut value = self.bound_envelope(binding_id, &binding);
            value["readiness"] = json!({"ready":blocker.is_none(),"reason_code":blocker});
            value["telemetry_received_age_ms"] = json!(
                session
                    .integration_telemetry_age_ms()
                    .map(|age| age.to_string())
            );
            value["native_json_text"] =
                json!(serde_json::to_string(&snapshot).map_err(|error| error.to_string())?);
            return respond(request, 200, value);
        }
        if suffix == "read" && method == Method::Post {
            let body = match body(&mut request, parsed_body) {
                Ok(body) => body,
                Err(message) => {
                    let status = if message == "JSON 请求过大" {
                        413
                    } else {
                        400
                    };
                    return self.error(request, status, "bad_request", message);
                }
            };
            if let Err(message) = body.reject_unknown(&[
                "instance_id",
                "connection_generation",
                "expected_identity",
                "action",
            ]) {
                return self.error(request, 400, "bad_request", message);
            }
            let instance = match body.string("instance_id") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            if instance != self.instance_id {
                return self.error(request, 409, "instance_mismatch", "工具实例已改变");
            }
            let generation = match body.string("connection_generation") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            let identity: Identity = match body.decode_field("expected_identity") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            if generation != binding.generation || identity != binding.identity {
                return self.error(request, 409, "binding_stale", "绑定预期身份或代次不一致");
            }
            let action = match body.string("action") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            let command = match action.as_str() {
                "status" => Command::Status,
                "measurements" => Command::Measurements,
                "diagnostics" => Command::Diagnostics,
                "config_startup" => Command::ConfigRead {
                    view: "startup".into(),
                },
                "config_user" => Command::ConfigRead {
                    view: "user".into(),
                },
                _ => return self.error(request, 400, "bad_request", "未知只读动作"),
            };
            if let Some(reason) = workbench.integration_global_read_blocker() {
                return self.error(
                    request,
                    409,
                    "read_not_ready",
                    format!("主动读取门禁未就绪：{reason}"),
                );
            }
            let session = workbench
                .integration_session_mut(&binding.target_id)
                .expect("validated session");
            let result = match session.execute(command) {
                Ok(result) => result,
                Err(error) => return self.session_error(request, error),
            };
            if !session.integration_connected()
                || session.connection_generation().to_string() != binding.generation
                || session.integration_identity().as_ref()
                    != Some(&(
                        binding.identity.uid.clone(),
                        binding.identity.run_nonce.clone(),
                    ))
            {
                return self.error(request, 409, "identity_changed", "读取期间身份或代次改变");
            }
            let mut value = self.bound_envelope(binding_id, &binding);
            value["action"] = json!(action);
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
            if action.starts_with("config_") {
                value["data_utf8"] = result.data["data_utf8"].clone();
                value["data_hex"] = result.data["data_hex"].clone();
            }
            return respond(request, 200, value);
        }
        self.error(request, 404, "not_found", "未知集成路径")
    }

    fn bound_envelope(&self, binding_id: &str, binding: &Binding) -> Value {
        let mut value = self.envelope();
        value["binding_id"] = json!(binding_id);
        value["target_id"] = json!(binding.target_id);
        value["connection_generation"] = json!(binding.generation);
        value["identity"] = binding.identity.json();
        value
    }

    fn session_error(
        &self,
        request: tiny_http::Request,
        error: crate::session::SessionError,
    ) -> Result<(), String> {
        let status = if error.kind.as_ref() == "timeout" {
            504
        } else {
            502
        };
        let code = if status == 504 {
            "sdk_timeout"
        } else {
            "sdk_error"
        };
        let mut value = self.envelope();
        value["code"] = json!(code);
        value["message"] = json!(error.message);
        value["unknown"] = json!(error.unknown);
        value["native_error_json_text"] =
            json!(serde_json::to_string(&error).map_err(|error| error.to_string())?);
        respond(request, status, value)
    }
}

fn body(
    request: &mut tiny_http::Request,
    parsed: Option<Result<crate::RawFields, String>>,
) -> Result<crate::RawFields, String> {
    match parsed {
        Some(value) => value,
        None => super::parse_json_body(request),
    }
}

fn respond(request: tiny_http::Request, status: u16, value: Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    if bytes.len() > RESPONSE_BYTES {
        return super::respond_json(
            request,
            500,
            json!({
                "protocol":PROTOCOL, "instance_id":value["instance_id"],
                "code":"response_too_large","message":"集成响应超过 1 MiB"
            }),
        );
    }
    let length = bytes.len();
    let mut response = Response::from_data(bytes).with_status_code(StatusCode(status));
    common_headers(
        &mut response,
        "application/json; charset=utf-8",
        length,
        None,
    )?;
    respond_async(request, response)
}

pub(super) fn authorize(
    request: &tiny_http::Request,
    port: u16,
    state: Option<&IntegrationState>,
) -> Result<(), (u16, &'static str, &'static str)> {
    let Some(state) = state else {
        return Err((404, "not_found", "本机集成未启用"));
    };
    let hosts: Vec<_> = request
        .headers()
        .iter()
        .filter(|item| item.field.equiv("Host"))
        .collect();
    if hosts.len() != 1 || hosts[0].value.as_str() != format!("127.0.0.1:{port}") {
        return Err((403, "origin_rejected", "Host 不是本机集成端口"));
    }
    if request.url().contains('?') {
        return Err((400, "bad_request", "集成路由不接受查询参数"));
    }
    if header(request, "Origin").is_some() {
        return Err((403, "origin_rejected", "本机集成拒绝浏览器 Origin"));
    }
    let headers: Vec<_> = request
        .headers()
        .iter()
        .filter(|item| item.field.equiv("Authorization"))
        .collect();
    if headers.len() != 1 || headers[0].value.as_str() != format!("Bearer {}", state.token) {
        return Err((401, "unauthorized", "集成凭据无效"));
    }
    if request_method(request) == Method::Post {
        if !header(request, "Content-Type").is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|part| part.trim().eq_ignore_ascii_case("application/json"))
        }) {
            return Err((400, "bad_request", "请求需要 application/json"));
        }
        if header(request, "Content-Length")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > 65536)
        {
            return Err((413, "bad_request", "JSON 请求超过 64 KiB"));
        }
    }
    Ok(())
}

pub(super) fn respond_error(
    request: tiny_http::Request,
    state: Option<&IntegrationState>,
    status: u16,
    code: &str,
    message: &str,
) -> Result<(), String> {
    let instance_id = state.map(|state| state.instance_id.as_str()).unwrap_or("");
    respond(
        request,
        status,
        json!({"protocol":PROTOCOL,"instance_id":instance_id,"code":code,"message":message}),
    )
}

pub(super) fn dispatch(
    request: tiny_http::Request,
    port: u16,
    workbench: &mut Workbench,
    parsed_body: Option<Result<crate::RawFields, String>>,
) -> Result<(), String> {
    if let Err((status, code, message)) = authorize(&request, port, workbench.integration.as_ref())
    {
        return respond_error(
            request,
            workbench.integration.as_ref(),
            status,
            code,
            message,
        );
    }
    let Some(mut state) = workbench.integration.take() else {
        unreachable!("authorized state exists")
    };
    let result = state.route(request, workbench, parsed_body);
    workbench.integration = Some(state);
    result
}

#[cfg(test)]
mod tests {
    use super::{IntegrationState, dispatch};
    use crate::{
        session::trial_tests::{FixtureBehavior, session_fixture},
        webui::workbench::Workbench,
    };
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpStream,
        thread,
    };
    use tiny_http::Server;

    fn call(
        workbench: &mut Workbench,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: bool,
        origin: bool,
    ) -> Result<(u16, Value), Box<dyn std::error::Error + Send + Sync>> {
        let server = Server::http(("127.0.0.1", 0))?;
        let address = server.server_addr().to_ip().ok_or("no IP socket")?;
        let port = address.port();
        let body = body
            .map(|value| serde_json::to_string(&value))
            .transpose()?;
        let mut text =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
        if token {
            text.push_str("Authorization: Bearer test-token\r\n");
        }
        if origin {
            text.push_str(&format!("Origin: http://127.0.0.1:{port}\r\n"));
        }
        if let Some(body) = &body {
            text.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        text.push_str("\r\n");
        if let Some(body) = &body {
            text.push_str(body);
        }
        let client = thread::spawn(move || -> std::io::Result<String> {
            let mut stream = TcpStream::connect(address)?;
            stream.write_all(text.as_bytes())?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        dispatch(server.recv()?, port, workbench, None)?;
        let response = client.join().map_err(|_| "client panic")??;
        let (head, body) = response.split_once("\r\n\r\n").ok_or("missing HTTP body")?;
        let status = head
            .split_whitespace()
            .nth(1)
            .ok_or("missing HTTP status")?
            .parse()?;
        Ok((status, serde_json::from_str(body)?))
    }

    #[cfg(unix)]
    #[test]
    fn token_file_restarts_with_same_secret_and_new_instance() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "eha-tool-integration-{}",
            super::random_hex::<8>().expect("random")
        ));
        std::fs::create_dir(&root).expect("test directory");
        let path = root.join("token");
        let first = IntegrationState::new(&path).expect("create token");
        let second = IntegrationState::new(&path).expect("reuse token");
        assert_eq!(first.token, second.token);
        assert_ne!(first.instance_id, second.instance_id);
        assert_eq!(
            std::fs::metadata(&path)
                .expect("token metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("relax test token");
        assert!(IntegrationState::new(&path).is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rejected_lifecycle_call_does_not_invalidate_binding_generation() {
        let (mut can, _) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let before = can.connection_generation();
        assert!(can.reconnect().is_err());
        assert_eq!(can.connection_generation(), before);
        can.disconnect().expect("first disconnect");
        let after = can.connection_generation();
        assert!(can.disconnect().is_err());
        assert_eq!(can.connection_generation(), after);
    }

    #[test]
    fn auth_attach_snapshot_and_detach_leave_sdk_session_owned() {
        let (can, _) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let mut workbench = Workbench::with_test_sessions(
            crate::session::ToolSession::new(),
            BTreeMap::from([(1, can)]),
        );
        workbench.integration = Some(IntegrationState::for_test());
        let (status, _) = call(
            &mut workbench,
            "GET",
            "/integration/v1/describe",
            None,
            false,
            false,
        )
        .expect("unauthenticated");
        assert_eq!(status, 401);
        let (status, _) = call(
            &mut workbench,
            "GET",
            "/integration/v1/describe",
            None,
            true,
            true,
        )
        .expect("origin rejected");
        assert_eq!(status, 403);
        let (status, describe) = call(
            &mut workbench,
            "GET",
            "/integration/v1/describe",
            None,
            true,
            false,
        )
        .expect("describe");
        assert_eq!(status, 200);
        assert_eq!(describe["dto_schema_version"], 1);
        let (status, targets) = call(
            &mut workbench,
            "GET",
            "/integration/v1/targets",
            None,
            true,
            false,
        )
        .expect("limited targets");
        assert_eq!(status, 200);
        assert_eq!(targets["discovery_limited"], true);
        assert_eq!(targets["discovery_reason"], "awaiting_telemetry");
        let session = workbench.integration_session_mut("can:1").expect("fixture");
        let (uid, run_nonce) = session.integration_identity().expect("identity");
        let generation = session.connection_generation().to_string();
        let attach = json!({
            "instance_id":"i-test","target_id":"can:1","intent":"attach",
            "expected_identity":{"uid":uid,"run_nonce":run_nonce},
            "connection_generation":generation
        });
        let (status, binding) = call(
            &mut workbench,
            "POST",
            "/integration/v1/bindings",
            Some(attach),
            true,
            false,
        )
        .expect("attach");
        assert_eq!(status, 200);
        let id = binding["binding_id"].as_str().expect("binding id");
        assert!(id.starts_with("i-test-b"));
        let path = format!("/integration/v1/bindings/{id}/snapshot");
        let (status, snapshot) =
            call(&mut workbench, "GET", &path, None, true, false).expect("snapshot");
        assert_eq!(status, 200);
        assert_eq!(snapshot["readiness"]["ready"], false);
        assert!(snapshot["native_json_text"].is_string());
        let (status, _) = call(
            &mut workbench,
            "DELETE",
            &format!("/integration/v1/bindings/{id}"),
            None,
            true,
            false,
        )
        .expect("detach");
        assert_eq!(status, 200);
        assert!(
            workbench
                .integration_session_mut("can:1")
                .expect("session remains")
                .integration_connected()
        );
        let (status, _) =
            call(&mut workbench, "GET", &path, None, true, false).expect("old binding");
        assert_eq!(status, 404);
    }

    #[test]
    fn fixed_read_uses_existing_session_and_preserves_native_json_text() {
        let (can, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let mut workbench = Workbench::with_test_sessions(
            crate::session::ToolSession::new(),
            BTreeMap::from([(1, can)]),
        );
        workbench.integration = Some(IntegrationState::for_test());
        probe.inject_idle(31);
        for _ in 0..20 {
            workbench.tick();
            if workbench.integration_global_read_blocker().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(workbench.integration_global_read_blocker(), None);
        let session = workbench.integration_session_mut("can:1").expect("session");
        let (uid, run_nonce) = session.integration_identity().expect("identity");
        let generation = session.connection_generation().to_string();
        let identity = json!({"uid":uid,"run_nonce":run_nonce});
        let (_, binding) = call(
            &mut workbench,
            "POST",
            "/integration/v1/bindings",
            Some(json!({
                "instance_id":"i-test","target_id":"can:1","intent":"attach",
                "expected_identity":identity,"connection_generation":generation
            })),
            true,
            false,
        )
        .expect("attach");
        let id = binding["binding_id"].as_str().expect("binding id");
        let read_path = format!("/integration/v1/bindings/{id}/read");
        let read = json!({
            "instance_id":"i-test","connection_generation":generation,
            "expected_identity":identity,"action":"status"
        });
        let (status, value) = call(
            &mut workbench,
            "POST",
            &read_path,
            Some(read.clone()),
            true,
            false,
        )
        .expect("status read");
        assert_eq!(status, 200);
        assert_eq!(value["action"], "status");
        assert!(value["source_timestamp"]["value"].is_string());
        assert!(value["received_age_ms"].is_string());
        let native: Value =
            serde_json::from_str(value["native_json_text"].as_str().expect("native text"))
                .expect("native JSON");
        assert_eq!(native["kind"], "status");
        let mut config = read.clone();
        config["action"] = json!("config_startup");
        let (status, config_value) = call(
            &mut workbench,
            "POST",
            &read_path,
            Some(config),
            true,
            false,
        )
        .expect("startup read");
        assert_eq!(status, 200);
        assert!(config_value["data_utf8"].is_string());
        assert!(config_value["data_hex"].is_string());
        let mut unknown = read;
        unknown["action"] = json!("stop");
        let (status, value) = call(
            &mut workbench,
            "POST",
            &read_path,
            Some(unknown),
            true,
            false,
        )
        .expect("unknown action");
        assert_eq!(status, 400);
        assert_eq!(value["code"], "bad_request");
    }

    #[test]
    fn delayed_idle_after_target_or_stop_keeps_unresolved_latch() {
        let (mut can, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        can.execute(crate::session::Command::Position { mm: 1.0 })
            .expect("target submitted");
        probe.inject_idle(31);
        for _ in 0..10 {
            can.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            can.integration_pending_target(),
            "late old Idle cannot clear target"
        );
        can.execute(crate::session::Command::Stop)
            .expect("explicit Stop submitted");
        probe.inject_idle(32);
        for _ in 0..10 {
            can.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            can.integration_pending_target(),
            "local Stop and later Idle are not device-causal proof"
        );
    }

    #[test]
    fn ordinary_target_latch_survives_disconnect_and_blocks_other_sessions() {
        let (mut can, _) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let _ = can
            .execute(crate::session::Command::Position { mm: 1.0 })
            .expect("local submission");
        assert!(can.integration_pending_target());
        can.disconnect().expect("local disconnect");
        assert!(can.integration_pending_target());
        let workbench = Workbench::with_test_sessions(
            crate::session::ToolSession::new(),
            BTreeMap::from([(1, can)]),
        );
        assert_eq!(
            workbench.integration_global_read_blocker(),
            Some("unresolved_target")
        );
    }
}
