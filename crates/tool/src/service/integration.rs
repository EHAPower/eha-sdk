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
    io::{Read, Write},
    path::Path,
    sync::mpsc::Receiver,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Response, StatusCode};

const PROTOCOL: &str = "eha-tool.integration.v1";
const RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_BINDINGS: usize = 8;
pub(super) const MIN_LEASE_MS: u64 = 1_000;
pub(super) const MAX_LEASE_MS: u64 = 121_000;

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
    lease_ms: Option<u64>,
    expires_at: Option<Instant>,
}

#[derive(Clone, Debug)]
pub(super) struct CheckedBinding {
    pub(super) binding_id: String,
    pub(super) target_id: String,
    pub(super) uid: String,
    pub(super) run_nonce: String,
    pub(super) connection_generation: String,
    pub(super) lease_ms: Option<u64>,
}

pub(super) struct BindingError {
    pub(super) status: u16,
    pub(super) code: &'static str,
    pub(super) message: String,
}

pub(super) struct IntegrationState {
    token: String,
    pub(super) instance_id: String,
    version: u8,
    bindings: BTreeMap<String, Binding>,
    next_binding: u64,
    #[cfg(test)]
    clock_offset: Duration,
    pub(super) started_at_unix_ms: String,
    pub(super) endpoint: String,
    pub(super) shutdown: Option<(Instant, Receiver<()>)>,
}

pub(super) fn random_hex<const N: usize>() -> Result<String, String> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|error| format!("系统随机源不可用：{error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(unix)]
fn token_from_file(path: &Path) -> Result<String, String> {
    let (directory, name) = super::instance::parent(path)?;
    let (file, created) = super::instance::credential_file(&directory, name)?;
    token_from_open_file(file, created)
}

#[cfg(unix)]
fn legacy_token_from_file(path: &Path) -> Result<String, String> {
    use rustix::fs::{Mode, OFlags, open};
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let (file, created) = match open(
        path,
        flags | OFlags::CREATE | OFlags::EXCL,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(fd) => {
            use std::os::unix::fs::PermissionsExt;
            let file = std::fs::File::from(fd);
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
            (file, true)
        }
        Err(error) if error == rustix::io::Errno::EXIST => {
            let fd = open(path, flags, Mode::empty()).map_err(|error| error.to_string())?;
            (std::fs::File::from(fd), false)
        }
        Err(error) => return Err(error.to_string()),
    };
    token_from_open_file(super::instance::validate_private(file)?, created)
}

#[cfg(unix)]
fn token_from_open_file(mut file: std::fs::File, created: bool) -> Result<String, String> {
    file.try_lock()
        .map_err(|error| format!("集成 token 正由另一启动者读取：{error}"))?;
    if file.metadata().map_err(|error| error.to_string())?.len() == 0 && created {
        file.write_all((random_hex::<32>()? + "\n").as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        use std::io::Seek;
        file.rewind().map_err(|error| error.to_string())?;
    }
    let mut token = String::new();
    file.take(66)
        .read_to_string(&mut token)
        .map_err(|error| error.to_string())?;
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

#[cfg(not(unix))]
fn legacy_token_from_file(path: &Path) -> Result<String, String> {
    token_from_file(path)
}

impl IntegrationState {
    pub(super) fn new(path: &Path) -> Result<Self, String> {
        Self::with_token(token_from_file(path)?)
    }

    pub(super) fn legacy(path: &Path) -> Result<Self, String> {
        Self::with_token(legacy_token_from_file(path)?)
    }

    fn with_token(token: String) -> Result<Self, String> {
        Ok(Self {
            token,
            version: 1,
            instance_id: format!("i-{}", random_hex::<16>()?),
            bindings: BTreeMap::new(),
            next_binding: 1,
            #[cfg(test)]
            clock_offset: Duration::ZERO,
            started_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_millis()
                .to_string(),
            endpoint: String::new(),
            shutdown: None,
        })
    }

    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self {
            token: "test-token".into(),
            version: 1,
            instance_id: "i-test".into(),
            bindings: BTreeMap::new(),
            next_binding: 1,
            #[cfg(test)]
            clock_offset: Duration::ZERO,
            started_at_unix_ms: "0".into(),
            endpoint: "http://127.0.0.1:12345".into(),
            shutdown: None,
        }
    }

    pub(super) fn for_v2(&self) -> Self {
        Self {
            token: self.token.clone(),
            instance_id: self.instance_id.clone(),
            version: 2,
            bindings: BTreeMap::new(),
            next_binding: 1,
            #[cfg(test)]
            clock_offset: Duration::ZERO,
            started_at_unix_ms: self.started_at_unix_ms.clone(),
            endpoint: self.endpoint.clone(),
            shutdown: None,
        }
    }

    fn observation_now(&self) -> Instant {
        #[cfg(test)]
        {
            Instant::now() + self.clock_offset
        }
        #[cfg(not(test))]
        {
            Instant::now()
        }
    }

    fn reclaim_expired_observations(&mut self) {
        let now = self.observation_now();
        self.bindings
            .retain(|_, binding| binding.expires_at.is_none_or(|deadline| deadline > now));
    }

    #[cfg(test)]
    pub(super) fn test_binding(&mut self, workbench: &mut Workbench, target_id: &str) -> String {
        let session = workbench
            .integration_session_mut(target_id)
            .expect("synthetic owner");
        let (uid, run_nonce) = session
            .integration_identity()
            .expect("verified synthetic identity");
        let id = format!("{}-v2-b-{}", self.instance_id, self.next_binding);
        self.next_binding += 1;
        self.bindings.insert(
            id.clone(),
            Binding {
                target_id: target_id.into(),
                generation: session.connection_generation().to_string(),
                identity: Identity { uid, run_nonce },
                lease_ms: None,
                expires_at: None,
            },
        );
        id
    }

    pub(super) fn checked_current_binding(
        &mut self,
        binding_id: &str,
        workbench: &mut Workbench,
    ) -> Result<CheckedBinding, BindingError> {
        self.reclaim_expired_observations();
        let binding = self.bindings.get(binding_id).ok_or_else(|| BindingError {
            status: 404,
            code: "not_found",
            message: "绑定不存在".into(),
        })?;
        let fields=crate::RawFields::parse(&json!({"instance_id":self.instance_id,"expected_identity":binding.identity.json(),"connection_generation":binding.generation}).to_string()).map_err(|error|BindingError {status:500,code:"bad_request",message:error.to_string()})?;
        self.checked_binding(binding_id, &fields, workbench)
    }

    pub(super) fn checked_binding(
        &mut self,
        binding_id: &str,
        fields: &crate::RawFields,
        workbench: &mut Workbench,
    ) -> Result<CheckedBinding, BindingError> {
        self.reclaim_expired_observations();
        let bad = |message| BindingError {
            status: 400,
            code: "bad_request",
            message,
        };
        if fields.string("instance_id").map_err(bad)? != self.instance_id {
            return Err(BindingError {
                status: 409,
                code: "instance_mismatch",
                message: "工具实例已改变".into(),
            });
        }
        let binding = self.bindings.get(binding_id).ok_or_else(|| BindingError {
            status: 404,
            code: "not_found",
            message: "绑定不存在".into(),
        })?;
        let identity: Identity = fields.decode_field("expected_identity").map_err(bad)?;
        let generation = fields.string("connection_generation").map_err(bad)?;
        if identity != binding.identity || generation != binding.generation {
            return Err(BindingError {
                status: 409,
                code: "binding_stale",
                message: "绑定预期身份或代次不一致".into(),
            });
        }
        let session = workbench
            .integration_session_mut(&binding.target_id)
            .ok_or_else(|| BindingError {
                status: 409,
                code: "binding_stale",
                message: "owner 会话已改变".into(),
            })?;
        if !session.integration_connected()
            || session.connection_generation().to_string() != generation
        {
            return Err(BindingError {
                status: 409,
                code: "binding_stale",
                message: "连接代次已改变".into(),
            });
        }
        if session.integration_identity().as_ref()
            != Some(&(identity.uid.clone(), identity.run_nonce.clone()))
        {
            return Err(BindingError {
                status: 409,
                code: "identity_changed",
                message: "运行身份已改变".into(),
            });
        }
        Ok(CheckedBinding {
            binding_id: binding_id.into(),
            target_id: binding.target_id.clone(),
            uid: identity.uid,
            run_nonce: identity.run_nonce,
            connection_generation: generation,
            lease_ms: binding.lease_ms,
        })
    }

    pub(super) fn shutdown_ready(&self) -> bool {
        self.shutdown.as_ref().is_some_and(|(started, response)| {
            response.try_recv().is_ok() || started.elapsed() >= Duration::from_millis(500)
        })
    }

    pub(super) fn envelope(&self) -> Value {
        if self.version == 2 {
            super::v2::envelope(&self.instance_id)
        } else {
            json!({"protocol":PROTOCOL,"instance_id":self.instance_id})
        }
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

    pub(super) fn route(
        &mut self,
        mut request: tiny_http::Request,
        workbench: &mut Workbench,
        parsed_body: Option<Result<crate::RawFields, String>>,
    ) -> Result<(), String> {
        if self.version == 2 {
            self.reclaim_expired_observations();
        }
        let path = if self.version == 2 {
            super::v2::shared_path(request_path(&request))
        } else {
            request_path(&request).to_owned()
        };
        let method = request_method(&request);
        if path == "/integration/v1/describe" && method == Method::Get {
            if self.version == 2 {
                return respond(
                    request,
                    200,
                    super::v2::describe(&self.instance_id, RESPONSE_BYTES, MAX_BINDINGS),
                );
            }
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
            let body = match self.body(&mut request, parsed_body) {
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
            let allowed = if self.version == 2 {
                &[
                    "instance_id",
                    "target_id",
                    "intent",
                    "expected_identity",
                    "connection_generation",
                    "lease_ms",
                ][..]
            } else {
                &[
                    "instance_id",
                    "target_id",
                    "intent",
                    "expected_identity",
                    "connection_generation",
                ][..]
            };
            if let Err(message) = body.reject_unknown(allowed) {
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
            #[cfg(test)]
            if workbench.p5_fixture.is_some() && intent != "attach" {
                return self.error(
                    request,
                    403,
                    "fixture_physical_access_forbidden",
                    "UI_SYNTHETIC 仅允许附着合成会话",
                );
            }
            let lease_ms = if body.contains("lease_ms") {
                match body.decode_field::<u64>("lease_ms") {
                    Ok(value) if (MIN_LEASE_MS..=MAX_LEASE_MS).contains(&value) => Some(value),
                    _ => {
                        return self.error(
                            request,
                            400,
                            "bad_request",
                            "lease_ms 必须为 1000 至 121000 的整数毫秒",
                        );
                    }
                }
            } else {
                None
            };
            if self.bindings.len() >= MAX_BINDINGS {
                return self.error(request, 409, "busy", "绑定数量已达上限");
            }
            let Some(next_binding) = self.next_binding.checked_add(1) else {
                return self.error(request, 409, "busy", "观察绑定序号已耗尽");
            };
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
                    if workbench.control_blocks_sync() {
                        return self.error(
                            request,
                            409,
                            "control_busy",
                            "控制授权或工作尚未退出，拒绝新连接",
                        );
                    }
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
                lease_ms,
                expires_at: lease_ms.map(|milliseconds| {
                    self.observation_now() + Duration::from_millis(milliseconds)
                }),
            };
            let binding_id = if self.version == 2 {
                format!("{}-v2-b{}", self.instance_id, self.next_binding)
            } else {
                format!("{}-b{}", self.instance_id, self.next_binding)
            };
            self.next_binding = next_binding;
            self.bindings.insert(binding_id.clone(), binding.clone());
            let mut value = self.envelope();
            value["binding_id"] = json!(binding_id);
            value["target_id"] = json!(target_id);
            value["connection_generation"] = json!(binding.generation);
            value["identity"] = binding.identity.json();
            if self.version == 2 {
                value["lease_ms"] = json!(lease_ms);
            }
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
        if self.version == 2 && suffix == "renew" && method == Method::Post {
            let fields = match self.body(&mut request, parsed_body) {
                Ok(fields) => fields,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            if let Err(message) = fields.reject_unknown(&[
                "instance_id",
                "expected_identity",
                "connection_generation",
            ]) {
                return self.error(request, 400, "bad_request", message);
            }
            let instance = match fields.string("instance_id") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            if instance != self.instance_id {
                return self.error(request, 409, "instance_mismatch", "工具实例已改变");
            }
            let generation = match fields.string("connection_generation") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            let identity: Identity = match fields.decode_field("expected_identity") {
                Ok(value) => value,
                Err(message) => return self.error(request, 400, "bad_request", message),
            };
            if generation != binding.generation || identity != binding.identity {
                return self.error(request, 409, "binding_stale", "绑定预期身份或代次不一致");
            }
            let Some(milliseconds) = binding.lease_ms else {
                return self.error(
                    request,
                    400,
                    "unsupported",
                    "未申请观察租约的绑定不支持续约",
                );
            };
            let deadline = self.observation_now() + Duration::from_millis(milliseconds);
            self.bindings
                .get_mut(binding_id)
                .expect("validated binding")
                .expires_at = Some(deadline);
            return respond(request, 200, self.bound_envelope(binding_id, &binding));
        }
        if suffix == "snapshot" && method == Method::Get {
            let blocker = workbench.integration_global_read_blocker();
            let unresolved_target = workbench.has_unresolved_target();
            let session = workbench
                .integration_session_mut(&binding.target_id)
                .expect("validated session");
            let snapshot = session.snapshot();
            let mut value = self.bound_envelope(binding_id, &binding);
            if self.version == 2 {
                super::v2::snapshot_permits(&mut value, blocker, unresolved_target);
            } else {
                value["readiness"] = json!({"ready":blocker.is_none(),"reason_code":blocker});
            }
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
            let body = match self.body(&mut request, parsed_body) {
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
                "config_factory" if self.version == 2 => Command::ConfigRead {
                    view: "factory".into(),
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
            if self.version == 2
                && matches!(action.as_str(), "config_user" | "config_startup")
                && let Some(state) = workbench.mutations.as_mut()
                && let Ok(member) = super::reservations::MemberKey::new(
                    &self.instance_id,
                    &binding.target_id,
                    &binding.identity.uid,
                    &binding.identity.run_nonce,
                    &binding.generation,
                )
            {
                state.note_config_read(
                    &member,
                    if action == "config_user" {
                        "user"
                    } else {
                        "startup"
                    },
                );
            }
            if action.starts_with("config_") {
                value["data_utf8"] = result.data["data_utf8"].clone();
                value["data_hex"] = result.data["data_hex"].clone();
            }
            if self.version == 2 {
                super::v2::snapshot_permits(
                    &mut value,
                    workbench.integration_global_read_blocker(),
                    workbench.has_unresolved_target(),
                );
            }
            return respond(request, 200, value);
        }
        self.error(request, 404, "not_found", "未知集成路径")
    }

    fn body(
        &self,
        request: &mut tiny_http::Request,
        parsed: Option<Result<crate::RawFields, String>>,
    ) -> Result<crate::RawFields, String> {
        let mut fields = body(request, parsed)?;
        if self.version == 2 {
            super::v2::validate_version(&mut fields)?;
        }
        Ok(fields)
    }

    fn bound_envelope(&self, binding_id: &str, binding: &Binding) -> Value {
        let mut value = self.envelope();
        value["binding_id"] = json!(binding_id);
        value["target_id"] = json!(binding.target_id);
        value["connection_generation"] = json!(binding.generation);
        value["identity"] = binding.identity.json();
        if self.version == 2 {
            value["lease_ms"] = json!(binding.lease_ms);
        }
        value
    }

    fn session_error(
        &self,
        request: tiny_http::Request,
        error: crate::session::SessionError,
    ) -> Result<(), String> {
        let (status, code) = match error.kind.as_ref() {
            "timeout" => (504, "sdk_timeout"),
            "owner_busy" => (409, "owner_busy"),
            kind if kind.starts_with("owner_lock_") => (503, kind),
            _ => (502, "sdk_error"),
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

pub(super) fn respond(
    request: tiny_http::Request,
    status: u16,
    value: Value,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    if bytes.len() > RESPONSE_BYTES {
        let mut error = json!({"protocol":value["protocol"],"instance_id":value["instance_id"],
            "code":"response_too_large","message":"集成响应超过 1 MiB"});
        if let Some(version) = value.get("api_version") {
            error["api_version"] = version.clone();
        }
        return super::respond_json(request, 500, error);
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
    let v2_query = state.version == 2
        && request_method(request) == Method::Get
        && (request_path(request) == "/service/v2/events"
            || request_path(request).starts_with("/service/v2/recordings/"));
    if request.url().contains('?') && !v2_query {
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
    if request_method(request) == Method::Post
        || (state.version == 2
            && request_method(request) == Method::Delete
            && request_path(request).starts_with("/service/v2/profiles/"))
    {
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
    let mut value = state
        .map(IntegrationState::envelope)
        .unwrap_or_else(|| json!({"protocol":PROTOCOL,"instance_id":""}));
    value["code"] = json!(code);
    value["message"] = json!(message);
    respond(request, status, value)
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
    let mut state = workbench.integration.take().ok_or("集成认证状态已丢失")?;
    let result = state.route(request, workbench, parsed_body);
    workbench.integration = Some(state);
    result
}

#[cfg(test)]
pub(super) mod tests {
    use super::{IntegrationState, dispatch};
    use crate::{
        service::workbench::Workbench,
        session::trial_tests::{FixtureBehavior, session_fixture},
    };
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpStream,
        thread,
    };
    use tiny_http::Server;

    pub(in crate::service) fn call(
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
        if path.starts_with("/service/v2/") {
            super::super::v2::dispatch(
                server.recv()?,
                port,
                workbench,
                &mut super::super::OdriveView::new(None),
                &mut super::super::CanScanView::new(),
                None,
            )?;
        } else {
            dispatch(server.recv()?, port, workbench, None)?;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !client.is_finished() {
            if std::time::Instant::now() >= deadline {
                return Err("HTTP fixture owner deadline exceeded".into());
            }
            workbench.tick();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
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
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("private root");
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

    #[cfg(unix)]
    #[test]
    fn legacy_relative_token_path_remains_supported_while_serve_is_strict() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::path::PathBuf::from(format!(
            ".eha-legacy-token-{}",
            super::random_hex::<8>().expect("random")
        ));
        std::fs::create_dir(&root).expect("relative test directory");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let path = root.join("token");
        let first = IntegrationState::legacy(&path).expect("legacy creates relative credential");
        let second = IntegrationState::legacy(&path).expect("legacy reuses relative credential");
        assert_eq!(first.token, second.token);
        assert!(IntegrationState::new(&path).is_err());
        assert_eq!(
            std::fs::metadata(&path)
                .expect("token mode")
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn legacy_ordinary_parent_is_supported_without_relaxing_token_or_serve_rules() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = std::env::temp_dir().join(format!(
            "eha-legacy-parent-{}",
            super::random_hex::<8>().expect("random")
        ));
        std::fs::create_dir(&root).expect("test directory");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
            .expect("ordinary parent");
        let root = std::fs::canonicalize(root).expect("absolute parent");
        let path = root.join("token");
        let first =
            IntegrationState::legacy(&path).expect("legacy creates credential in 0755 parent");
        let second =
            IntegrationState::legacy(&path).expect("legacy reuses credential in 0755 parent");
        assert_eq!(first.token, second.token);
        assert!(IntegrationState::new(&path).is_err());
        let before = std::fs::read(&path).expect("credential before strict rejection");
        assert_eq!(before, (first.token + "\n").as_bytes());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("unsafe token");
        assert!(IntegrationState::legacy(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("restore private token");
        let link = root.join("symlink");
        symlink(&path, &link).expect("link");
        assert!(IntegrationState::legacy(&link).is_err());
        let hard = root.join("hardlink");
        std::fs::hard_link(&path, &hard).expect("hard link");
        assert!(IntegrationState::legacy(&hard).is_err());
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

#[cfg(test)]
mod observation_lease_tests {
    use super::tests::call;
    use super::*;
    use crate::session::{
        Command as SessionCommand, ToolSession,
        trial_tests::{FixtureBehavior, session_fixture},
    };

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
    fn fields(workbench: &mut Workbench) -> Value {
        let session = workbench.integration_session_mut("can:1").expect("owner");
        let (uid, run_nonce) = session.integration_identity().expect("identity");
        json!({"api_version":2,"instance_id":"i-test","target_id":"can:1","intent":"attach",
            "expected_identity":{"uid":uid,"run_nonce":run_nonce},"connection_generation":session.connection_generation().to_string()})
    }
    fn attach(workbench: &mut Workbench, lease: Option<u64>) -> String {
        let mut body = fields(workbench);
        if let Some(lease) = lease {
            body["lease_ms"] = json!(lease);
        }
        let (status, value) = call(
            workbench,
            "POST",
            "/service/v2/bindings",
            Some(body),
            true,
            false,
        )
        .expect("attach");
        assert_eq!(status, 200, "{value}");
        value["binding_id"].as_str().expect("binding ID").into()
    }
    fn advance(workbench: &mut Workbench, milliseconds: u64) {
        workbench.v2.as_mut().expect("state").clock_offset += Duration::from_millis(milliseconds);
    }
    fn renew_fields(workbench: &mut Workbench) -> Value {
        let mut body = fields(workbench);
        let object = body.as_object_mut().expect("fields");
        object.remove("intent");
        object.remove("target_id");
        body
    }

    #[test]
    fn leases_expire_without_affecting_other_observers_owner_heartbeat_or_unknown_effect() {
        let (mut workbench, probe) = fixture();
        let persistent = attach(&mut workbench, None);
        let expired = attach(&mut workbench, Some(1_000));
        let active = attach(&mut workbench, Some(120_000));
        let session = workbench.integration_session_mut("can:1").expect("owner");
        session
            .execute(SessionCommand::Position { mm: 2.0 })
            .expect("existing target");
        let before = session.snapshot();
        assert_eq!(before.heartbeat["enabled"], false);
        advance(&mut workbench, 1_001);
        let payload = renew_fields(&mut workbench);
        let (status, error) = call(
            &mut workbench,
            "POST",
            &format!("/service/v2/bindings/{expired}/renew"),
            Some(payload),
            true,
            false,
        )
        .expect("expired renewal");
        assert_eq!(status, 404);
        assert_eq!(error["code"], "not_found");
        for id in [&persistent, &active] {
            assert_eq!(
                call(
                    &mut workbench,
                    "GET",
                    &format!("/service/v2/bindings/{id}/snapshot"),
                    None,
                    true,
                    false
                )
                .expect("live observer")
                .0,
                200
            );
        }
        let state = workbench.v2.as_ref().expect("state");
        assert_eq!(state.bindings.len(), 2);
        let session = workbench
            .integration_session_mut("can:1")
            .expect("owner retained");
        assert!(session.integration_connected());
        assert!(session.integration_pending_target());
        assert_eq!(session.snapshot().heartbeat, before.heartbeat);
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
    }

    #[test]
    fn renewal_checks_scope_and_never_revives_expired_or_persistent_bindings() {
        let (mut workbench, _) = fixture();
        let persistent = attach(&mut workbench, None);
        let leased = attach(&mut workbench, Some(1_000));
        let path = format!("/service/v2/bindings/{leased}/renew");
        let valid = renew_fields(&mut workbench);
        for (field, value, expected) in [
            ("instance_id", json!("i-old"), 409),
            ("api_version", json!(1), 400),
            ("connection_generation", json!("old"), 409),
            (
                "expected_identity",
                json!({"uid":"old","run_nonce":"old"}),
                409,
            ),
            ("lease_ms", json!(120_000), 400),
        ] {
            let before = workbench.v2.as_ref().expect("state").bindings[&leased].expires_at;
            let mut body = valid.clone();
            body[field] = value;
            assert_eq!(
                call(&mut workbench, "POST", &path, Some(body), true, false)
                    .expect("reject")
                    .0,
                expected
            );
            assert_eq!(
                workbench.v2.as_ref().expect("state").bindings[&leased].expires_at,
                before
            );
        }
        assert_eq!(
            call(
                &mut workbench,
                "POST",
                &format!("/service/v2/bindings/{persistent}/renew"),
                Some(valid.clone()),
                true,
                false
            )
            .expect("no implicit lease")
            .1["code"],
            "unsupported"
        );
        advance(&mut workbench, 900);
        let (status, response) = call(
            &mut workbench,
            "POST",
            &path,
            Some(valid.clone()),
            true,
            false,
        )
        .expect("renew");
        assert_eq!(status, 200);
        assert_eq!(response["lease_ms"], 1_000);
        advance(&mut workbench, 900);
        assert_eq!(
            call(
                &mut workbench,
                "GET",
                &format!("/service/v2/bindings/{leased}/snapshot"),
                None,
                true,
                false
            )
            .expect("renewed lease lives")
            .0,
            200
        );
        advance(&mut workbench, 101);
        assert_eq!(
            call(&mut workbench, "POST", &path, Some(valid), true, false)
                .expect("expired never revived")
                .0,
            404
        );
        assert_eq!(workbench.v2.as_ref().expect("state").bindings.len(), 1);
    }

    #[test]
    fn exhausted_binding_sequence_fails_before_attach_or_connect_and_never_reuses_id() {
        let (mut workbench, probe) = fixture();
        workbench.v2.as_mut().expect("state").next_binding = u64::MAX - 1;
        let last = attach(&mut workbench, Some(1_000));
        assert!(last.ends_with(&(u64::MAX - 1).to_string()));
        let mut attach_fields = fields(&mut workbench);
        attach_fields["lease_ms"] = json!(1_000);
        for body in [
            attach_fields,
            json!({"api_version":2,"instance_id":"i-test","target_id":"usb:must-not-enumerate","intent":"connect"}),
        ] {
            let (status, value) = call(
                &mut workbench,
                "POST",
                "/service/v2/bindings",
                Some(body),
                true,
                false,
            )
            .expect("exhausted counter");
            assert_eq!(status, 409);
            assert_eq!(value["code"], "busy");
        }
        assert_eq!(
            call(
                &mut workbench,
                "DELETE",
                &format!("/service/v2/bindings/{last}"),
                None,
                true,
                false
            )
            .expect("cleanup last")
            .0,
            200
        );
        let body = fields(&mut workbench);
        assert_eq!(
            call(
                &mut workbench,
                "POST",
                "/service/v2/bindings",
                Some(body),
                true,
                false
            )
            .expect("capacity free but IDs exhausted")
            .0,
            409
        );
        assert!(workbench.v2.as_ref().expect("state").bindings.is_empty());
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
    }

    #[test]
    fn lease_ranges_v1_compatibility_and_scoped_duplicate_cleanup() {
        let (mut workbench, _) = fixture();
        for lease in [
            json!(0),
            json!(999),
            json!(121001),
            json!("1000"),
            json!(1000.5),
        ] {
            let mut body = fields(&mut workbench);
            body["lease_ms"] = lease;
            assert_eq!(
                call(
                    &mut workbench,
                    "POST",
                    "/service/v2/bindings",
                    Some(body),
                    true,
                    false
                )
                .expect("bounds")
                .0,
                400
            );
        }
        let old = attach(&mut workbench, Some(1_000));
        let live = attach(&mut workbench, Some(120_000));
        let old_path = format!("/service/v2/bindings/{old}");
        assert_eq!(
            call(&mut workbench, "DELETE", &old_path, None, true, false)
                .expect("detach")
                .0,
            200
        );
        assert_eq!(
            call(&mut workbench, "DELETE", &old_path, None, true, false)
                .expect("repeat")
                .0,
            404
        );
        assert!(
            workbench
                .v2
                .as_ref()
                .expect("state")
                .bindings
                .contains_key(&live)
        );
        workbench.v2.as_mut().expect("new process").instance_id = "i-new".into();
        workbench.v2.as_mut().expect("new process").bindings.clear();
        workbench.v2.as_mut().expect("new process").next_binding = 1;
        let mut body = fields(&mut workbench);
        body["instance_id"] = json!("i-new");
        body["lease_ms"] = json!(120_000);
        let (status, fresh) = call(
            &mut workbench,
            "POST",
            "/service/v2/bindings",
            Some(body),
            true,
            false,
        )
        .expect("new observer");
        assert_eq!(status, 200);
        assert_eq!(
            call(&mut workbench, "DELETE", &old_path, None, true, false)
                .expect("old instance cleanup")
                .0,
            404
        );
        assert!(
            workbench
                .v2
                .as_ref()
                .expect("new state")
                .bindings
                .contains_key(fresh["binding_id"].as_str().expect("new ID"))
        );
        let mut legacy = fields(&mut workbench);
        legacy
            .as_object_mut()
            .expect("object")
            .remove("api_version");
        legacy["lease_ms"] = json!(1_000);
        assert_eq!(
            call(
                &mut workbench,
                "POST",
                "/integration/v1/bindings",
                Some(legacy),
                true,
                false
            )
            .expect("v1 rejects extension")
            .0,
            400
        );
    }
}

#[cfg(all(test, unix))]
mod observation_timeout_regression {
    use super::*;
    use crate::session::{
        Command as SessionCommand, ToolSession,
        trial_tests::{FixtureBehavior, session_fixture},
    };
    use std::{fs, os::unix::fs::PermissionsExt, sync::mpsc, thread};

    #[test]
    fn cli_read_and_unknown_attach_timeouts_do_not_exhaust_observation_capacity() {
        for timeout_on_attach in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "eha-observation-timeouts-{}-{}",
                std::process::id(),
                timeout_on_attach
            ));
            fs::create_dir_all(&root).expect("private root");
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("mode");
            let root = fs::canonicalize(root).expect("canonical root");
            let token_file = root.join("token");
            let mut state = IntegrationState::new(&token_file)
                .expect("credentials")
                .for_v2();
            state.instance_id = "i-timeouts".into();
            let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("mock listener");
            let address = server.server_addr().to_ip().expect("address");
            let descriptor = super::super::instance::Descriptor {
                protocol: "eha-tool.service.v2".into(),
                api_version: 2,
                tool_version: "test".into(),
                instance_id: state.instance_id.clone(),
                pid: "1".into(),
                started_at_unix_ms: "1".into(),
                endpoint: format!("http://{address}"),
                token_file,
            };
            let path = root.join("instance.json");
            fs::write(&path, serde_json::to_vec(&descriptor).expect("descriptor")).expect("write");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("descriptor mode");
            let (complete, receiver) = mpsc::channel();
            let owner = thread::spawn(move || {
                let (mut can, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
                can.execute(SessionCommand::Position { mm: 2.0 })
                    .expect("existing target");
                let before = can.snapshot();
                let (uid, run_nonce) = can.integration_identity().expect("identity");
                let generation = can.connection_generation().to_string();
                let binding = Binding {
                    target_id: "can:1".into(),
                    generation: generation.clone(),
                    identity: Identity {
                        uid: uid.clone(),
                        run_nonce: run_nonce.clone(),
                    },
                    lease_ms: None,
                    expires_at: None,
                };
                state
                    .bindings
                    .insert("i-timeouts-v2-b-persistent".into(), binding.clone());
                let mut active = binding;
                active.lease_ms = Some(120_000);
                active.expires_at = Some(state.observation_now() + Duration::from_secs(120));
                state
                    .bindings
                    .insert("i-timeouts-v2-b-active".into(), active);
                let mut workbench =
                    Workbench::with_test_sessions(ToolSession::new(), BTreeMap::from([(1, can)]));
                workbench.v2 = Some(state);
                workbench.legacy_routes = false;
                let mut delayed = 0;
                loop {
                    if let Some(request) = server
                        .recv_timeout(Duration::from_millis(10))
                        .expect("request")
                    {
                        let path = request.url().to_owned();
                        if path == "/service/v2/describe" {
                            // 仅推进观察单调时钟；原owner心跳/试验时钟不受影响。
                            workbench.v2.as_mut().expect("state").clock_offset +=
                                Duration::from_secs(2);
                        }
                        if path == "/service/v2/targets" {
                            let mut value = workbench.v2.as_ref().expect("state").envelope();
                            value["targets"] = json!([{"target_id":"can:1","connected":true,"identity":{"uid":uid,"run_nonce":run_nonce},"connection_generation":generation}]);
                            respond(request, 200, value).expect("targets");
                        } else {
                            if delayed < MAX_BINDINGS
                                && ((timeout_on_attach && path == "/service/v2/bindings")
                                    || (!timeout_on_attach && path.ends_with("/reads")))
                            {
                                delayed += 1;
                                thread::sleep(Duration::from_millis(50));
                            }
                            super::super::v2::dispatch(
                                request,
                                address.port(),
                                &mut workbench,
                                &mut super::super::OdriveView::new(None),
                                &mut super::super::CanScanView::new(),
                                None,
                            )
                            .expect("actual service dispatch");
                        }
                    } else if receiver.try_recv().is_ok() {
                        break;
                    }
                }
                let state = workbench.v2.as_ref().expect("state");
                assert_eq!(state.bindings.len(), 2);
                assert!(state.bindings.contains_key("i-timeouts-v2-b-persistent"));
                assert!(state.bindings.contains_key("i-timeouts-v2-b-active"));
                let session = workbench.integration_session_mut("can:1").expect("owner");
                assert!(session.integration_connected());
                assert!(session.integration_pending_target());
                assert_eq!(session.snapshot().heartbeat, before.heartbeat);
                assert_eq!(session.integration_identity(), Some((uid, run_nonce)));
                assert_eq!(session.connection_generation().to_string(), generation);
                let observed = probe.observed.lock().expect("probe");
                assert_eq!(observed.targets, 1);
                assert_eq!(observed.stops, 0);
            });
            for attempt in 1..=MAX_BINDINGS {
                let error = crate::service_client::read(
                    &path,
                    "can:1",
                    crate::service_client::ReadAction::Status,
                    Duration::from_millis(30),
                )
                .expect_err("timeout");
                assert!(
                    error.starts_with("service_timeout:"),
                    "attempt {attempt}: {error}"
                );
            }
            let error = crate::service_client::read(
                &path,
                "can:1",
                crate::service_client::ReadAction::Status,
                Duration::from_millis(30),
            )
            .expect_err("owner read gate");
            assert!(
                error.starts_with("read_not_ready:"),
                "ninth observer attaches: {error}"
            );
            complete.send(()).expect("complete");
            owner.join().expect("owner facts");
            fs::remove_dir_all(root).expect("cleanup");
        }
    }
}
