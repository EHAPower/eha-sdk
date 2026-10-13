// Copyright The eha-sdk Contributors
//! v2 的连接与观察适配。设备 I/O 仍归唯一 Workbench，文件与发现归有界 worker。

use super::{
    CanScanView, Method, OdriveOperation, OdriveView, Workbench,
    connection_profiles::{AdvancedCanInput, UsbProfile, UsbProfileStore},
    events, integration, read_jobs, v2,
};
use crate::{
    RawFields,
    connections::{ConnectionProfile, ManagedAdapter},
    session::{ConnectionRequest, ToolSession},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const EVENT_PAYLOAD_BYTES: usize = 8192;
const HISTORY: usize = 16;

pub(super) struct State {
    events: events::EventLog,
    sources: BTreeMap<String, Source>,
    pub read_jobs: read_jobs::ReadJobs,
    profile_jobs: read_jobs::ProfileJobs,
    usb_profiles: Option<UsbProfileStore>,
    usb_expected: BTreeMap<String, String>,
    usb_profile_ids: BTreeMap<String, String>,
    batch: Option<Batch>,
    next_batch: u64,
    scan_contexts: BTreeMap<String, u64>,
    network_jobs: super::network_jobs::NetworkJobs,
    network_pending: Option<(NetworkAction, bool)>,
    profiles: BTreeMap<String, Value>,
    observed_jobs: BTreeMap<String, String>,
}
enum NetworkAction {
    Connect {
        id: String,
        node: u8,
        profile: Option<ConnectionProfile>,
    },
    Scan {
        id: String,
        nodes: Vec<u8>,
    },
}
struct Batch {
    id: String,
    nodes: VecDeque<(u8, Value)>,
    connections: Vec<Value>,
    errors: Vec<Value>,
}
struct Source {
    context: Value,
    cursor: u64,
    dropped: u64,
}
impl State {
    pub fn new(runs_dir: PathBuf) -> Self {
        Self {
            events: events::EventLog::new(events::Limits {
                max_events: 4096,
                max_bytes: 4 * 1024 * 1024,
                max_batch: 100,
                retention: Duration::from_secs(30),
            })
            .expect("固定非零事件上限"),
            sources: BTreeMap::new(),
            #[cfg(not(test))]
            usb_profiles: Some(UsbProfileStore::load_default()),
            #[cfg(test)]
            usb_profiles: Some(UsbProfileStore::load(runs_dir.join("usb-profiles.json"))),
            usb_expected: BTreeMap::new(),
            usb_profile_ids: BTreeMap::new(),
            batch: None,
            next_batch: 1,
            scan_contexts: BTreeMap::new(),
            network_jobs: super::network_jobs::NetworkJobs::new(),
            network_pending: None,
            read_jobs: read_jobs::ReadJobs::new(runs_dir),
            profile_jobs: read_jobs::ProfileJobs::new(),
            profiles: BTreeMap::new(),
            observed_jobs: BTreeMap::new(),
        }
    }
    pub fn reap(&mut self, workbench: &mut Workbench) {
        self.read_jobs.reap();
        let values = self
            .read_jobs
            .results()
            .map(|job| {
                (
                    job.job_id.clone(),
                    serde_json::to_value(job).expect("job JSON"),
                )
            })
            .collect::<Vec<_>>();
        for (id, value) in values {
            self.observe_job(&id, &value);
        }
        while let Some((id, store, usb_store, result)) = self.profile_jobs.take_completed() {
            workbench.profiles = Some(store);
            self.usb_profiles = Some(usb_store);
            let value = match result {
                Ok(data) => json!({"job_id":id,"state":"completed","data":data,"error":null}),
                Err(error) => json!({"job_id":id,"state":"failed","data":null,"error":error}),
            };
            self.profiles.insert(id.clone(), value.clone());
            self.observe_job(&id, &value);
        }
    }
    pub fn advance_batch(&mut self, workbench: &mut Workbench) {
        if let Some(mut batch) = self.batch.take() {
            if let Some((node, expected)) = batch.nodes.pop_front() {
                match workbench.service_connect_new_node(node, Some(&expected)) {
                    Ok(connection) => batch.connections.push(connection),
                    Err(message) => {
                        let code = if message.starts_with("binding_stale:") {
                            "binding_stale"
                        } else if message.starts_with("identity_changed:") {
                            "identity_changed"
                        } else if message.starts_with("read_not_ready:") {
                            "read_not_ready"
                        } else if message.contains("owner_busy") {
                            "owner_busy"
                        } else {
                            "sdk_error"
                        };
                        let native = serde_json::from_str::<Value>(&message).ok();
                        batch.errors.push(json!({"node":node,"target_id":format!("can:{node}"),"code":code,
                            "message":message,"unknown":native.as_ref().is_some_and(|v|v["unknown"]==true),
                            "error_json_text":native.map(|_|message.clone())}));
                    }
                }
            }
            if batch.nodes.is_empty() {
                let value = json!({"job_id":batch.id,"state":"completed","data":{"connections":batch.connections,"errors":batch.errors},"error":null});
                self.profiles.insert(batch.id.clone(), value.clone());
                self.observe_job(&batch.id, &value);
            } else {
                self.batch = Some(batch);
            }
        }
    }
    pub fn connection_busy(&self) -> bool {
        self.batch.is_some() || self.network_pending.is_some()
    }
    fn poll_network(&mut self, workbench: &mut Workbench, scan: &mut CanScanView) {
        let Some((_, waiting)) = self.network_pending.as_ref() else {
            return;
        };
        let result = if *waiting {
            let Some(result) = self.network_jobs.take_completed() else {
                return;
            };
            result.and_then(|prepared| workbench.service_install_network(prepared))
        } else {
            Ok(())
        };
        let (action, _) = self.network_pending.take().expect("pending action");
        match action {
            NetworkAction::Connect { id, node, profile } => {
                let result = result.and_then(|()| {
                    if let Some(reason) = workbench.integration_global_read_blocker() {
                        return Err(format!("read_not_ready: {reason}"));
                    }
                    if workbench
                        .service_connections()
                        .iter()
                        .any(|entry| entry["target_id"] == format!("can:{node}"))
                    {
                        return Err("binding_stale: 已有节点不能隐式重连".into());
                    }
                    if let Some(profile) = profile {
                        workbench
                            .connect_managed(profile)
                            .map_err(|error| error.message.to_string())?;
                        Ok(connection_value(
                            &format!("can:{node}"),
                            workbench
                                .integration_session_mut(&format!("can:{node}"))
                                .ok_or("连接会话丢失")?,
                        ))
                    } else {
                        workbench.service_connect_new_node(node, None)
                    }
                });
                let value = match result {
                    Ok(connection) => {
                        json!({"job_id":id,"state":"completed","data":{"connection":connection},"error":null})
                    }
                    Err(error) => json!({"job_id":id,"state":"failed","data":null,"error":error}),
                };
                self.profiles.insert(id.clone(), value.clone());
                self.observe_job(&id, &value);
            }
            NetworkAction::Scan { id, nodes } => {
                let result = result.and_then(|()| workbench.service_scan_job(&nodes));
                scan.busy = false;
                let result = result.and_then(|mut job| {
                    job.scan_id = Some(id.clone());
                    scan.start(job)
                        .map_err(|_| "CAN扫描worker不可用".to_owned())
                });
                if let Err(error) = result {
                    scan.v2_results.insert(id.clone(),json!({"scan_id":id,"state":"completed","nodes":[],"errors":nodes.iter().map(|node|json!({"node":node,"message":error,"unknown":false,"error_json_text":null})).collect::<Vec<_>>()}));
                } else if let Some(generation) = workbench.service_network_generation() {
                    self.scan_contexts.insert(id, generation);
                }
            }
        }
    }
    fn observe_job(&mut self, id: &str, value: &Value) {
        let status = value["state"].as_str().unwrap_or("unknown");
        if self.observed_jobs.get(id).is_some_and(|old| old == status) {
            return;
        }
        self.observed_jobs.insert(id.into(), status.into());
        let is_scan = value.get("scan_id").is_some();
        let payload = if is_scan {
            json!({"scan_id":id,"state":status})
        } else {
            json!({"job_id":id,"state":status})
        };
        self.append(
            if is_scan { "scan" } else { "job" },
            id.into(),
            payload,
            None,
            None,
        );
    }
    fn append(
        &mut self,
        kind: &'static str,
        source_id: String,
        payload: Value,
        time_us: Option<u64>,
        gap: Option<events::SourceGap>,
    ) {
        let Ok(text) = serde_json::to_string(&payload) else {
            return;
        };
        let oversized = text.len() > EVENT_PAYLOAD_BYTES;
        let payload = if oversized {
            // 只记录缺口事实，绝不把大事实截断后冒充完整数据。
            RawValue::from_string("{\"reason\":\"payload_too_large\"}".into()).expect("固定 JSON")
        } else {
            RawValue::from_string(text).expect("已序列化 JSON")
        };
        let received_at = unix_ms();
        let _ = self.events.append(
            Instant::now(),
            events::EventInput {
                kind,
                source_id,
                source_timestamp: time_us.map_or(
                    events::SourceTimestamp::HostUnixMs(received_at),
                    events::SourceTimestamp::DeviceMonotonicUs,
                ),
                received_at,
                payload,
                source_gap: if oversized {
                    Some(events::SourceGap {
                        reason: "payload_too_large",
                        dropped: None,
                    })
                } else {
                    gap
                },
            },
        );
    }
    pub fn capture_session(&mut self, target: &str, session: &mut ToolSession) {
        let Some((uid, run_nonce)) = session.integration_identity() else {
            return;
        };
        let context = json!({"target_id":target,"identity":{"uid":uid,"run_nonce":run_nonce},
            "connection_generation":session.connection_generation().to_string(),"connected":session.integration_connected()});
        let mut source = self.sources.remove(target).unwrap_or(Source {
            context: Value::Null,
            cursor: 0,
            dropped: 0,
        });
        let changed = source.context != context;
        let source_id = format!(
            "{}:{}:{}:{}",
            target,
            context["connection_generation"].as_str().unwrap_or(""),
            uid,
            run_nonce
        );
        if changed {
            self.append("connection", source_id.clone(), context.clone(), None, None);
            if !source.context.is_null() {
                self.append(
                    "source_gap",
                    source_id.clone(),
                    context.clone(),
                    None,
                    Some(events::SourceGap {
                        reason: "connection_changed",
                        dropped: None,
                    }),
                );
            }
            source.context = context.clone();
        }
        let trend = session.telemetry_trend_json(Some(source.cursor));
        let cursor = trend["cursor"].as_u64().unwrap_or(source.cursor);
        let dropped = trend["dropped"].as_u64().unwrap_or(source.dropped);
        if cursor != source.cursor || dropped != source.dropped {
            if trend["truncated"] == true
                || (trend["reset"] == true && source.cursor != 0)
                || dropped != source.dropped
            {
                self.append(
                    "source_gap",
                    source_id.clone(),
                    context.clone(),
                    None,
                    Some(events::SourceGap {
                        reason: if trend["truncated"] == true {
                            "source_truncated"
                        } else if dropped != source.dropped {
                            "source_dropped"
                        } else {
                            "source_reset"
                        },
                        dropped: dropped
                            .checked_sub(source.dropped)
                            .filter(|count| *count != 0),
                    }),
                );
            }
            if let Some(points) = trend["points"].as_array() {
                for point in points {
                    let Some(time_us) = point["time_us"].as_u64() else {
                        continue;
                    };
                    let mut payload = context.clone();
                    payload
                        .as_object_mut()
                        .expect("context")
                        .remove("connected");
                    payload["native_json_text"] =
                        json!(serde_json::to_string(point).expect("trend JSON"));
                    self.append(
                        "telemetry",
                        source_id.clone(),
                        payload,
                        Some(time_us),
                        (point["gap"] == true).then_some(events::SourceGap {
                            reason: "source_gap",
                            dropped: None,
                        }),
                    );
                }
            }
        }
        source.cursor = cursor;
        source.dropped = dropped;
        if self.sources.len() >= 16 && !self.sources.contains_key(target) {
            let key = self.sources.keys().next().cloned().expect("nonempty");
            self.sources.remove(&key);
        }
        self.sources.insert(target.into(), source);
    }
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(super) fn capture_jobs(workbench: &mut Workbench, odrive: &OdriveView, scan: &mut CanScanView) {
    if workbench.v2.is_none() {
        return;
    }
    if let Some(mut p2) = workbench.p2.take() {
        p2.poll_network(workbench, scan);
        workbench.p2 = Some(p2);
    }
    if let Some(p2) = workbench.p2.as_mut() {
        let active = p2
            .read_jobs
            .results()
            .map(|job| job.job_id.clone())
            .chain(p2.profiles.keys().cloned())
            .chain(odrive.v2_results.keys().cloned())
            .chain(scan.v2_results.keys().cloned())
            .collect::<std::collections::BTreeSet<_>>();
        p2.observed_jobs.retain(|id, _| active.contains(id));
        for (id, value) in odrive.v2_results.iter().chain(scan.v2_results.iter()) {
            p2.observe_job(id, value);
        }
    }
}

pub(super) fn connection_value(target: &str, session: &mut ToolSession) -> Value {
    let identity = session
        .integration_identity()
        .map(|(uid, run_nonce)| json!({"uid":uid,"run_nonce":run_nonce}));
    let connected = session.integration_connected();
    let generation = session.connection_generation().to_string();
    let age = session
        .integration_telemetry_age_ms()
        .map(|n| n.to_string());
    let text = serde_json::to_string(&session.snapshot()).expect("会话 JSON");
    json!({"connection_id":target,"target_id":target,"identity":identity,
        "connection_generation":generation,"connected":connected,"snapshot_json_text":text,
        "telemetry_received_age_ms":age})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    transport: Option<String>,
    id: String,
    name: String,
    adapter: Adapter,
    node: u8,
    mode: String,
    expected_uid: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Adapter {
    kind: String,
    serial: String,
}
fn profile_value(value: &Value) -> Value {
    json!({"transport":"can","id":value["id"],"name":value["name"],"adapter":{"kind":value["adapter"]["kind"],"serial":value["adapter"]["serial"]},
        "node":value["node"],"mode":value["mode"],"expected_uid":value.get("expected_uid").unwrap_or(&Value::Null)})
}

pub(super) fn handles(path: &str) -> bool {
    [
        "adapters",
        "profiles",
        "connections",
        "scans",
        "events",
        "recordings",
        "odrive",
        "jobs",
    ]
    .iter()
    .any(|name| {
        path.strip_prefix("/service/v2/")
            .is_some_and(|suffix| suffix == *name || suffix.starts_with(&format!("{name}/")))
    })
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    code: &'static str,
    message: String,
    native: Option<Value>,
}
impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "bad_request",
            message: message.into(),
            native: None,
        }
    }
    fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: 409,
            code,
            message: message.into(),
            native: None,
        }
    }
    fn missing() -> Self {
        Self {
            status: 404,
            code: "not_found",
            message: "对象或路径不存在".into(),
            native: None,
        }
    }
}
impl From<String> for ApiError {
    fn from(value: String) -> Self {
        Self::bad(value)
    }
}
impl From<crate::session::SessionError> for ApiError {
    fn from(error: crate::session::SessionError) -> Self {
        let (status, code) = match error.kind.as_ref() {
            "timeout" => (504, "sdk_timeout"),
            "owner_busy" => (409, "owner_busy"),
            "identity_changed" => (409, "identity_changed"),
            _ => (502, "sdk_error"),
        };
        Self {
            status,
            code,
            message: if code == "owner_busy" {
                format!(
                    "{}；请附着已有设备服务，或先显式释放原 owner。",
                    error.message
                )
            } else {
                error.message.to_string()
            },
            native: serde_json::to_value(&error).ok(),
        }
    }
}
fn scoped(fields: &RawFields, state: &integration::IntegrationState) -> Result<(), ApiError> {
    if fields.string("instance_id")? != state.instance_id {
        return Err(ApiError::conflict("instance_mismatch", "服务实例已改变"));
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    uid: String,
    run_nonce: String,
}
/// 仅解码已分离的连接 ID 段一次；不改变路径分隔或查询边界。
fn connection_segment(encoded: &str) -> Result<std::borrow::Cow<'_, str>, ApiError> {
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && !(bytes.next().is_some_and(|value| value.is_ascii_hexdigit())
                && bytes.next().is_some_and(|value| value.is_ascii_hexdigit()))
        {
            return Err(ApiError::bad("连接 ID 包含非法百分号转义"));
        }
    }
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .map_err(|_| ApiError::bad("连接 ID 必须是有效 UTF-8"))?;
    if decoded.is_empty()
        || decoded
            .chars()
            .any(|value| matches!(value, '/' | '\\') || value.is_control())
    {
        return Err(ApiError::bad("连接 ID 不能含路径分隔符或控制字符"));
    }
    Ok(decoded)
}

fn checked_connection<'a>(
    workbench: &'a mut Workbench,
    target: &str,
    fields: &RawFields,
) -> Result<&'a mut ToolSession, ApiError> {
    let identity: Option<Identity> = fields.decode_field("expected_identity")?;
    let generation = fields.string("connection_generation")?;
    let session = workbench
        .integration_session_mut(target)
        .ok_or_else(ApiError::missing)?;
    if session.connection_generation().to_string() != generation {
        return Err(ApiError::conflict("binding_stale", "连接代次已改变"));
    }
    if session.integration_identity() != identity.map(|identity| (identity.uid, identity.run_nonce))
    {
        return Err(ApiError::conflict("identity_changed", "设备身份已改变"));
    }
    Ok(session)
}
fn query(url: &str, allowed: &[&str]) -> Result<BTreeMap<String, String>, ApiError> {
    let mut result = BTreeMap::new();
    if let Some((_, query)) = url.split_once('?') {
        for pair in query.split('&') {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| ApiError::bad("查询参数必须是 key=value"))?;
            if !allowed.contains(&key) || result.insert(key.into(), value.into()).is_some() {
                return Err(ApiError::bad("未知或重复查询参数"));
            }
        }
    }
    Ok(result)
}
fn decimal(value: &str) -> Result<u64, ApiError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::bad("必须使用非负十进制整数"));
    }
    value.parse().map_err(|_| ApiError::bad("整数超出 u64"))
}
fn merged(state: &integration::IntegrationState, value: Value) -> Value {
    let mut envelope = v2::envelope(&state.instance_id);
    if let Some(fields) = value.as_object() {
        envelope
            .as_object_mut()
            .expect("envelope")
            .extend(fields.clone());
    }
    envelope
}

pub(super) fn dispatch(
    mut request: tiny_http::Request,
    state: &integration::IntegrationState,
    workbench: &mut Workbench,
    odrive: &mut OdriveView,
    scan: &mut CanScanView,
    parsed: Option<Result<RawFields, String>>,
) -> Result<(), String> {
    let method = super::request_method(&request);
    let path = super::request_path(&request)
        .strip_prefix("/service/v2/")
        .unwrap_or("")
        .to_owned();
    let url = request.url().to_owned();
    let mut p2 = workbench.p2.take().expect("P2 owner state");
    p2.reap(workbench);
    let fields = if matches!(method, Method::Post | Method::Delete) {
        parsed
            .unwrap_or_else(|| super::parse_json_body(&mut request))
            .map_err(ApiError::from)
            .and_then(|mut fields| {
                v2::validate_version(&mut fields)?;
                scoped(&fields, state)?;
                Ok(fields)
            })
    } else {
        Ok(RawFields(BTreeMap::new()))
    };
    if path == "events" && method == Method::Get {
        let result = events_response(&mut p2, state, &url);
        workbench.p2 = Some(p2);
        return match result {
            Ok(bytes) => {
                super::respond_bytes(request, 200, "application/json; charset=utf-8", bytes)
            }
            Err(error) => error_response(request, state, error),
        };
    }
    let result = fields.and_then(|fields| {
        route(
            &path, &url, method, &fields, workbench, &mut p2, odrive, scan,
        )
    });
    workbench.p2 = Some(p2);
    workbench.capture_service_events();
    match result {
        Ok((status, value)) => integration::respond(request, status, merged(state, value)),
        Err(error) => error_response(request, state, error),
    }
}
fn error_response(
    request: tiny_http::Request,
    state: &integration::IntegrationState,
    error: ApiError,
) -> Result<(), String> {
    let mut value = v2::envelope(&state.instance_id);
    value["code"] = json!(error.code);
    value["message"] = json!(error.message);
    if let Some(native) = error.native {
        value["unknown"] = native["unknown"].clone();
        value["native_error_json_text"] =
            json!(serde_json::to_string(&native).map_err(|e| e.to_string())?);
    }
    integration::respond(request, error.status, value)
}
fn events_response(
    p2: &mut State,
    state: &integration::IntegrationState,
    url: &str,
) -> Result<Vec<u8>, ApiError> {
    let params = query(url, &["after", "limit"])?;
    let after = params.get("after").map(|s| decimal(s)).transpose()?;
    let limit = params
        .get("limit")
        .map(|s| decimal(s))
        .transpose()?
        .unwrap_or(p2.events.limits().max_batch as u64);
    let limit = usize::try_from(limit).map_err(|_| ApiError::bad("limit 超出范围"))?;
    let batch = p2
        .events
        .batch(Instant::now(), after, limit)
        .map_err(ApiError::bad)?;
    #[derive(Serialize)]
    struct Response<'a> {
        protocol: &'static str,
        api_version: u8,
        instance_id: &'a str,
        #[serde(flatten)]
        batch: events::Batch<'a>,
    }
    serde_json::to_vec(&Response {
        protocol: "eha-tool.service.v2",
        api_version: 2,
        instance_id: &state.instance_id,
        batch,
    })
    .map_err(|e| ApiError::bad(e.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn route(
    path: &str,
    url: &str,
    method: Method,
    fields: &RawFields,
    workbench: &mut Workbench,
    p2: &mut State,
    odrive: &mut OdriveView,
    scan: &mut CanScanView,
) -> Result<(u16, Value), ApiError> {
    if path == "connections" && method == Method::Get {
        return Ok((200, json!({"connections":workbench.service_connections()})));
    }
    if path == "profiles" && method == Method::Get {
        let result = workbench
            .profiles
            .as_ref()
            .map(crate::connections::ConnectionStore::response)
            .unwrap_or_else(|| json!({"profiles":[],"load_error":null}));
        let mut profiles = result["profiles"]
            .as_array()
            .map(|items| items.iter().map(profile_value).collect::<Vec<_>>())
            .unwrap_or_default();
        let usb = p2
            .usb_profiles
            .as_ref()
            .map(UsbProfileStore::response)
            .unwrap_or_else(|| json!({"profiles":[],"load_error":null}));
        profiles.extend(usb["profiles"].as_array().cloned().unwrap_or_default());
        let conflict = match (p2.usb_profiles.as_ref(), workbench.profiles.as_ref()) {
            (Some(usb), Some(can)) => usb.check_namespace(can).err(),
            _ => None,
        };
        let errors = [
            result["load_error"].as_str().map(str::to_owned),
            usb["load_error"].as_str().map(str::to_owned),
            conflict,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        return Ok((
            200,
            json!({"profiles":profiles,"load_error":if errors.is_empty(){None}else{Some(errors.join("；"))},
            "busy":workbench.profiles.is_none()||p2.usb_profiles.is_none()}),
        ));
    }
    if (path == "profiles" && method == Method::Post)
        || (path.starts_with("profiles/") && method == Method::Delete)
    {
        let operation = if method == Method::Post {
            fields.reject_unknown(&["instance_id", "profile"])?;
            let raw: Value = fields.decode_field("profile")?;
            if raw["transport"] == "usb" {
                let profile: UsbProfile = fields.decode_field("profile")?;
                profile.validate()?;
                read_jobs::ProfileOperation::SaveUsb(profile)
            } else {
                let input: Profile = fields.decode_field("profile")?;
                if input
                    .transport
                    .as_deref()
                    .is_some_and(|value| value != "can")
                {
                    return Err(ApiError::bad("profile transport 无效"));
                }
                let profile = ConnectionProfile {
                    id: input.id,
                    name: input.name,
                    adapter: ManagedAdapter {
                        kind: input.adapter.kind,
                        serial: input.adapter.serial,
                    },
                    node: input.node,
                    mode: input.mode,
                    expected_uid: input.expected_uid,
                    autoconnect: false,
                    default: false,
                };
                profile.validate()?;
                read_jobs::ProfileOperation::SaveCan(profile)
            }
        } else {
            fields.reject_unknown(&["instance_id"])?;
            let id = path.trim_start_matches("profiles/");
            p2.usb_profiles
                .as_ref()
                .ok_or_else(|| ApiError::conflict("busy", "档案写入正忙"))?
                .check_namespace(
                    workbench
                        .profiles
                        .as_ref()
                        .ok_or_else(|| ApiError::conflict("busy", "档案写入正忙"))?,
                )?;
            let usb = p2
                .usb_profiles
                .as_ref()
                .ok_or_else(|| ApiError::conflict("busy", "档案写入正忙"))?;
            if usb.profile_optional(id)?.is_some() {
                read_jobs::ProfileOperation::DeleteUsb(id.into())
            } else {
                read_jobs::ProfileOperation::DeleteCan(id.into())
            }
        };
        return start_profile_job(p2, workbench, operation);
    }
    if path == "adapters" && method == Method::Get {
        query(url, &[])?;
        return start_read_job(p2, read_jobs::Operation::Adapters);
    }
    if path == "recordings" && method == Method::Get {
        query(url, &[])?;
        return start_read_job(p2, read_jobs::Operation::Recordings);
    }
    if let Some(id) = path.strip_prefix("recordings/")
        && method == Method::Get
    {
        let params = query(url, &["file", "offset"])?;
        let file = params
            .get("file")
            .ok_or_else(|| ApiError::bad("下载必须明确 file"))?
            .clone();
        let offset = params
            .get("offset")
            .map(|value| decimal(value))
            .transpose()?
            .unwrap_or(0);
        return start_read_job(
            p2,
            read_jobs::Operation::Recording {
                run_id: id.into(),
                file,
                offset,
            },
        );
    }
    if let Some(id) = path.strip_prefix("jobs/")
        && method == Method::Get
    {
        let value = if let Some(job) = p2.read_jobs.get(id) {
            serde_json::to_value(job).map_err(|e| ApiError::bad(e.to_string()))?
        } else if let Some(value) = p2.profiles.get(id).or_else(|| odrive.v2_results.get(id)) {
            value.clone()
        } else {
            return Err(ApiError::missing());
        };
        p2.observe_job(id, &value);
        return Ok((200, value));
    }
    if path == "odrive/devices" && method == Method::Get {
        let value = odrive
            .start_v2(OdriveOperation::Discover)
            .map_err(|e| ApiError::conflict("busy", e))?;
        return Ok((202, value));
    }
    if path == "odrive/read" && method == Method::Post {
        fields.reject_unknown(&["instance_id", "serial"])?;
        let serial = fields.string("serial")?;
        if serial.is_empty() || serial.len() > 128 {
            return Err(ApiError::bad("serial 必须为完整序列号"));
        }
        let value = odrive
            .start_v2(OdriveOperation::Read { serial })
            .map_err(|e| ApiError::conflict("busy", e))?;
        return Ok((202, value));
    }
    if path == "connections/batch" && method == Method::Post {
        fields.reject_unknown(&["instance_id", "scan_id", "nodes"])?;
        connection_gate(workbench, p2, scan)?;
        let nodes: Vec<u8> = fields.decode_field("nodes")?;
        validate_nodes(&nodes)?;
        let id = fields.string("scan_id")?;
        let result = scan
            .v2_results
            .get(&id)
            .filter(|value| value["state"] == "completed")
            .filter(|_| {
                p2.scan_contexts.get(&id).is_some_and(|generation| {
                    Some(*generation) == workbench.service_network_generation()
                })
            })
            .ok_or_else(|| ApiError::conflict("binding_stale", "扫描结果已失效或尚未完成"))?;
        let mut selected = VecDeque::new();
        for node in nodes {
            let identity = result["nodes"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["node"] == node))
                .map(|item| item["identity"].clone())
                .ok_or_else(|| ApiError::bad("只能选择本次扫描成功节点"))?;
            selected.push_back((node, identity));
        }
        let id = format!("connect-{}", p2.next_batch);
        p2.next_batch = p2
            .next_batch
            .checked_add(1)
            .ok_or_else(|| ApiError::conflict("busy", "连接任务序号耗尽"))?;
        let value = json!({"job_id":id,"state":"running","data":null,"error":null});
        retain_job_slot(p2);
        p2.profiles.insert(id.clone(), value.clone());
        p2.batch = Some(Batch {
            id: id.clone(),
            nodes: selected,
            connections: Vec::new(),
            errors: Vec::new(),
        });
        p2.observe_job(&id, &value);
        return Ok((202, value));
    }
    if path == "connections" && method == Method::Post {
        if fields.contains("profile_id") {
            fields.reject_unknown(&["instance_id", "profile_id"])?;
        } else if fields.contains("advanced") {
            fields.reject_unknown(&["instance_id", "transport", "advanced", "node"])?;
            if fields.string("transport")? != "can" {
                return Err(ApiError::bad("advanced必须选择can"));
            }
        } else {
            fields.reject_unknown(&["instance_id", "transport", "serial"])?;
            if fields.string("transport")? != "usb" {
                return Err(ApiError::bad("CAN连接须明确profile_id或advanced"));
            }
        }
        connection_gate(workbench, p2, scan)?;
        let current = workbench.service_connections();
        if current.len() >= 16 {
            return Err(ApiError::conflict("busy", "已达会话上限"));
        }
        if fields.contains("advanced") {
            fields.reject_unknown(&["instance_id", "transport", "advanced", "node"])?;
            if fields.string("transport")? != "can" {
                return Err(ApiError::bad("advanced 必须选择 can"));
            }
            let advanced: AdvancedCanInput = fields.decode_field("advanced")?;
            let node: u8 = fields.decode_field("node")?;
            advanced.requests(&[node])?;
            if current
                .iter()
                .any(|entry| entry["target_id"] == format!("can:{node}"))
            {
                return Err(ApiError::conflict("binding_stale", "已有节点请显式重连"));
            }
            return start_network_connect(
                p2,
                workbench,
                super::network_jobs::Operation::Advanced {
                    channel: advanced.channel,
                    mode: advanced.mode,
                    python: advanced.python,
                },
                node,
                None,
            );
        }
        let mut usb_profile = None;
        let (target, usb, profile) = if fields.contains("profile_id") {
            fields.reject_unknown(&["instance_id", "profile_id"])?;
            let id = fields.string("profile_id")?;
            let can = workbench
                .profiles
                .as_ref()
                .ok_or_else(|| ApiError::conflict("busy", "档案写入正忙"))?;
            let usb = p2
                .usb_profiles
                .as_ref()
                .ok_or_else(|| ApiError::conflict("busy", "档案写入正忙"))?;
            usb.check_namespace(can)?;
            if let Some(profile) = usb.profile_optional(&id)? {
                profile.validate()?;
                let serial = profile.serial.clone();
                usb_profile = Some(profile);
                (format!("usb:{serial}"), Some(serial), None)
            } else {
                let profile = can.profile(&id)?;
                (format!("can:{}", profile.node), None, Some(profile))
            }
        } else {
            fields.reject_unknown(&["instance_id", "transport", "serial"])?;
            if fields.string("transport")? != "usb" {
                return Err(ApiError::bad("CAN 连接须明确 profile_id 或 advanced"));
            }
            let serial = fields.string("serial")?;
            crate::owner_lock::ResourceKey::usb(&serial)
                .map_err(|error| ApiError::bad(error.to_string()))?;
            (format!("usb:{serial}"), Some(serial), None)
        };
        if current.iter().any(|entry| entry["target_id"] == target) {
            return Err(ApiError::conflict(
                "binding_stale",
                "已有会话请显式附着或重连",
            ));
        }
        if let Some(serial) = usb {
            if current.iter().any(|entry| {
                entry["connected"] == true
                    && entry["target_id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("usb:"))
            }) {
                return Err(ApiError::conflict("busy", "USB 会话已连接；请先显式断连"));
            }
            if let Some(uid) = usb_profile
                .as_ref()
                .and_then(|profile| profile.expected_uid.as_ref())
            {
                workbench.service_pin_usb_identity(&serial, uid)?;
                p2.usb_expected.insert(target.clone(), uid.clone());
            }
            if let Some(profile) = usb_profile.as_ref() {
                p2.usb_profile_ids
                    .insert(target.clone(), profile.id.clone());
            }
            let devices = crate::devices::discover()?;
            crate::devices::find_serial(&devices, &serial)?;
            let request = usb_profile
                .as_ref()
                .map(UsbProfile::request)
                .transpose()?
                .unwrap_or(ConnectionRequest::Usb { serial });
            p2.usb_expected.retain(|key, _| key == &target);
            p2.usb_profile_ids.retain(|key, _| key == &target);
            workbench.connect(request)?;
        } else if let Some(profile) = profile {
            return start_network_connect(
                p2,
                workbench,
                super::network_jobs::Operation::Managed(profile.clone()),
                profile.node,
                Some(profile),
            );
        }
        check_usb_uid(p2, workbench, &target)?;
        let mut value = connection_value(
            &target,
            workbench
                .integration_session_mut(&target)
                .ok_or_else(ApiError::missing)?,
        );
        value["profile_job_id"] = Value::Null;
        bind_usb_profile(p2, workbench, &target, &mut value)?;
        return Ok((200, value));
    }
    if let Some(suffix) = path.strip_prefix("connections/")
        && method == Method::Post
    {
        let (target, action) = suffix.rsplit_once('/').ok_or_else(ApiError::missing)?;
        if !matches!(action, "reconnect" | "disconnect") {
            return Err(ApiError::missing());
        }
        let target = connection_segment(target)?;
        let target = target.as_ref();
        fields.reject_unknown(&["instance_id", "expected_identity", "connection_generation"])?;
        if scan.busy || p2.connection_busy() {
            return Err(ApiError::conflict("busy", "扫描或连接批次期间不能改变连接"));
        }
        checked_connection(workbench, target, fields)?;
        let (transport, node) = if target.starts_with("usb:") {
            (super::WebTransport::Usb, None)
        } else {
            (
                super::WebTransport::Can,
                Some(
                    target
                        .strip_prefix("can:")
                        .ok_or_else(ApiError::missing)?
                        .parse::<u8>()
                        .map_err(|_| ApiError::bad("CAN node 无效"))?,
                ),
            )
        };
        if action == "disconnect" {
            workbench.disconnect(transport, node)?;
        } else {
            workbench.reconnect(transport, node)?;
            check_usb_uid(p2, workbench, target)?;
        }
        let mut value = connection_value(
            target,
            workbench
                .integration_session_mut(target)
                .ok_or_else(ApiError::missing)?,
        );
        if action == "reconnect" {
            bind_usb_profile(p2, workbench, target, &mut value)?;
        }
        return Ok((200, value));
    }
    if path == "scans" && method == Method::Post {
        connection_gate(workbench, p2, scan)?;
        let nodes: Vec<u8> = fields.decode_field("nodes")?;
        validate_nodes(&nodes)?;
        let operation = if fields.contains("advanced") {
            fields.reject_unknown(&["instance_id", "advanced", "nodes"])?;
            let advanced: AdvancedCanInput = fields.decode_field("advanced")?;
            advanced.requests(&nodes)?;
            Some(super::network_jobs::Operation::Advanced {
                channel: advanced.channel,
                mode: advanced.mode,
                python: advanced.python,
            })
        } else if fields.contains("profile_id") {
            fields.reject_unknown(&["instance_id", "profile_id", "nodes"])?;
            let can = workbench
                .profiles
                .as_ref()
                .ok_or_else(|| ApiError::conflict("busy", "档案写入正忙"))?;
            p2.usb_profiles
                .as_ref()
                .ok_or_else(|| ApiError::conflict("busy", "档案写入正忙"))?
                .check_namespace(can)?;
            Some(super::network_jobs::Operation::Managed(
                can.profile(&fields.string("profile_id")?)?,
            ))
        } else {
            fields.reject_unknown(&[
                "instance_id",
                "connection_id",
                "expected_identity",
                "connection_generation",
                "nodes",
            ])?;
            let target = fields.string("connection_id")?;
            if !target.starts_with("can:") {
                return Err(ApiError::bad("扫描须选择CAN通路"));
            }
            if !checked_connection(workbench, &target, fields)?.integration_connected() {
                return Err(ApiError::conflict("binding_stale", "CAN通路已断开"));
            }
            None
        };
        if let Some(operation) = operation {
            let waiting = !workbench.service_network_matches(&operation)?;
            let next = scan
                .next_v2
                .checked_add(1)
                .ok_or_else(|| ApiError::conflict("busy", "扫描序号耗尽"))?;
            if waiting {
                submit_network_job(p2, workbench, operation)?;
            }
            let id = format!("scan-{}", scan.next_v2);
            scan.next_v2 = next;
            while scan.v2_results.len() >= 16 {
                let key = scan.v2_results.keys().next().cloned().expect("nonempty");
                scan.v2_results.remove(&key);
            }
            let value = json!({"scan_id":id,"state":"running","nodes":[],"errors":[]});
            scan.v2_results.insert(id.clone(), value.clone());
            scan.busy = true;
            p2.scan_contexts
                .retain(|id, _| scan.v2_results.contains_key(id));
            p2.network_pending = Some((
                NetworkAction::Scan {
                    id: id.clone(),
                    nodes,
                },
                waiting,
            ));
            p2.observe_job(&id, &value);
            return Ok((202, value));
        }
        let job = workbench
            .service_scan_job(&nodes)
            .map_err(|error| ApiError::conflict("read_not_ready", error))?;
        let value = scan
            .start_v2(job)
            .map_err(|error| ApiError::conflict("busy", error))?;
        let id = value["scan_id"].as_str().expect("scan id");
        p2.scan_contexts
            .retain(|id, _| scan.v2_results.contains_key(id));
        p2.scan_contexts.insert(
            id.into(),
            workbench
                .service_network_generation()
                .expect("scan network"),
        );
        p2.observe_job(id, &value);
        return Ok((202, value));
    }
    if let Some(id) = path.strip_prefix("scans/")
        && method == Method::Get
    {
        let value = scan
            .v2_results
            .get(id)
            .ok_or_else(ApiError::missing)?
            .clone();
        p2.observe_job(id, &value);
        return Ok((200, value));
    }
    Err(ApiError::missing())
}
fn submit_network_job(
    p2: &mut State,
    workbench: &mut Workbench,
    operation: super::network_jobs::Operation,
) -> Result<(), ApiError> {
    let retired = workbench.service_take_retired_network();
    match p2.network_jobs.start_replacing(operation, retired) {
        Ok(()) => Ok(()),
        Err((error, retired)) => {
            if let Some(retired) = retired {
                workbench.service_install_network(retired)?;
            }
            Err(ApiError::conflict("busy", error))
        }
    }
}
fn start_network_connect(
    p2: &mut State,
    workbench: &mut Workbench,
    operation: super::network_jobs::Operation,
    node: u8,
    profile: Option<ConnectionProfile>,
) -> Result<(u16, Value), ApiError> {
    let waiting = !workbench.service_network_matches(&operation)?;
    let next = p2
        .next_batch
        .checked_add(1)
        .ok_or_else(|| ApiError::conflict("busy", "连接任务序号耗尽"))?;
    if waiting {
        submit_network_job(p2, workbench, operation)?;
    }
    let id = format!("connect-{}", p2.next_batch);
    p2.next_batch = next;
    let value = json!({"job_id":id,"state":"running","data":null,"error":null});
    retain_job_slot(p2);
    p2.profiles.insert(id.clone(), value.clone());
    p2.network_pending = Some((
        NetworkAction::Connect {
            id: id.clone(),
            node,
            profile,
        },
        waiting,
    ));
    p2.observe_job(&id, &value);
    Ok((202, value))
}
fn connection_gate(workbench: &Workbench, p2: &State, scan: &CanScanView) -> Result<(), ApiError> {
    if scan.busy || p2.connection_busy() {
        return Err(ApiError::conflict("busy", "扫描或连接批次正忙"));
    }
    if let Some(reason) = workbench.integration_global_read_blocker() {
        return Err(ApiError::conflict("read_not_ready", reason));
    }
    Ok(())
}
fn validate_nodes(nodes: &[u8]) -> Result<(), ApiError> {
    if nodes.is_empty()
        || nodes.len() > 16
        || nodes.iter().any(|node| *node > 127)
        || nodes
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != nodes.len()
    {
        return Err(ApiError::bad("nodes 须为1至16个互异的0..127节点"));
    }
    Ok(())
}
fn bind_usb_profile(
    p2: &mut State,
    workbench: &mut Workbench,
    target: &str,
    value: &mut Value,
) -> Result<(), ApiError> {
    value["profile_job_id"] = Value::Null;
    let Some(id) = p2.usb_profile_ids.get(target).cloned() else {
        return Ok(());
    };
    let uid = value["identity"]["uid"]
        .as_str()
        .ok_or_else(|| ApiError::conflict("identity_changed", "Identity缺少UID"))?
        .to_owned();
    p2.usb_expected.insert(target.into(), uid.clone());
    if let Some(serial) = target.strip_prefix("usb:") {
        workbench.service_pin_usb_identity(serial, &uid)?;
    }
    let store = p2
        .usb_profiles
        .as_ref()
        .ok_or_else(|| ApiError::conflict("busy", "USB档案写入正忙"))?;
    if let Some(profile) = store.profile_optional(&id)? {
        if target != format!("usb:{}", profile.serial) {
            return Err(ApiError::conflict(
                "binding_stale",
                "原USB档案id已重建为另一序列号；拒绝沿用旧会话写入",
            ));
        }
        profile.identity_matches(&value["identity"])?;
        if profile
            .expected_uid
            .as_ref()
            .is_none_or(|expected| !expected.eq_ignore_ascii_case(&uid))
        {
            let (_, job) = start_profile_job(
                p2,
                workbench,
                read_jobs::ProfileOperation::BindUsb { id, uid },
            )?;
            value["profile_job_id"] = job["job_id"].clone();
        }
    }
    Ok(())
}
fn check_usb_uid(p2: &State, workbench: &mut Workbench, target: &str) -> Result<(), ApiError> {
    let Some(expected) = p2.usb_expected.get(target) else {
        return Ok(());
    };
    let session = workbench
        .integration_session_mut(target)
        .ok_or_else(ApiError::missing)?;
    if session
        .integration_identity()
        .is_none_or(|(uid, _)| !uid.eq_ignore_ascii_case(expected))
    {
        session.disconnect()?;
        return Err(ApiError::conflict(
            "identity_changed",
            "USB Identity 与档案预期 UID 不同；已断连",
        ));
    }
    Ok(())
}
fn retain_job_slot(p2: &mut State) {
    while p2.profiles.len() >= HISTORY {
        let key = p2
            .profiles
            .iter()
            .find(|(_, value)| value["state"] != "running")
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            p2.profiles.remove(&key);
        } else {
            break;
        }
    }
}
fn start_profile_job(
    p2: &mut State,
    workbench: &mut Workbench,
    operation: read_jobs::ProfileOperation,
) -> Result<(u16, Value), ApiError> {
    if workbench.profiles.is_none() || p2.usb_profiles.is_none() {
        return Err(ApiError::conflict("busy", "档案写入正忙"));
    }
    let can = workbench.profiles.take().expect("checked store");
    let usb = p2.usb_profiles.take().expect("checked store");
    match p2.profile_jobs.start(can, usb, operation) {
        Ok(id) => {
            retain_job_slot(p2);
            let value = json!({"job_id":id,"state":"running","data":null,"error":null});
            p2.profiles.insert(id.clone(), value.clone());
            p2.observe_job(&id, &value);
            Ok((202, value))
        }
        Err((message, can, usb)) => {
            workbench.profiles = Some(can);
            p2.usb_profiles = Some(usb);
            Err(ApiError::conflict("busy", message))
        }
    }
}
fn start_read_job(
    p2: &mut State,
    operation: read_jobs::Operation,
) -> Result<(u16, Value), ApiError> {
    let id = p2
        .read_jobs
        .start(operation)
        .map_err(|e| ApiError::conflict("busy", e))?;
    let value = serde_json::to_value(p2.read_jobs.get(&id).expect("已建立任务"))
        .map_err(|e| ApiError::bad(e.to_string()))?;
    p2.observe_job(&id, &value);
    Ok((202, value))
}

#[cfg(test)]
mod tests {
    use super::super::integration::{IntegrationState, tests::call};
    use super::*;
    use crate::session::trial_tests::{FixtureBehavior, FixtureProbe, session_fixture};

    fn fixture() -> (Workbench, FixtureProbe) {
        let (can, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        let mut owner =
            Workbench::with_test_sessions(ToolSession::new(), BTreeMap::from([(1, can)]));
        owner.v2 = Some(IntegrationState::for_test().for_v2());
        owner.legacy_routes = false;
        (owner, probe)
    }
    fn lifecycle(owner: &mut Workbench) -> Value {
        let session = owner.integration_session_mut("can:1").expect("session");
        let (uid, run_nonce) = session.integration_identity().expect("identity");
        json!({"api_version":2,"instance_id":"i-test","expected_identity":{"uid":uid,"run_nonce":run_nonce},
            "connection_generation":session.connection_generation().to_string()})
    }
    #[test]
    fn encoded_connection_segments_reach_usb_and_can_lifecycle_routes() {
        for (target, encoded) in [
            ("can:1", "can%3A1"),
            ("usb:SERIAL", "usb%3ASERIAL"),
            ("usb:A+B%3A", "usb%3AA%2BB%253A"),
            ("usb:A+B%3A", "usb%3aA+B%253A"),
        ] {
            let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
            let mut owner = if let Some(serial) = target.strip_prefix("usb:") {
                session.test_usb_request(serial);
                Workbench::with_test_sessions(session, BTreeMap::new())
            } else {
                Workbench::with_test_sessions(ToolSession::new(), BTreeMap::from([(1, session)]))
            };
            owner.v2 = Some(IntegrationState::for_test().for_v2());
            owner.legacy_routes = false;
            let session = owner.integration_session_mut(target).expect("原会话");
            let (uid, run_nonce) = session.integration_identity().expect("identity");
            let original = json!({"api_version":2,"instance_id":"i-test","expected_identity":{"uid":uid,"run_nonce":run_nonce},"connection_generation":session.connection_generation().to_string()});
            let mut stale = original.clone();
            stale["connection_generation"] = json!("0");
            for action in ["reconnect", "disconnect"] {
                let (status, value) = call(
                    &mut owner,
                    "POST",
                    &format!("/service/v2/connections/{encoded}/{action}"),
                    Some(stale.clone()),
                    true,
                    false,
                )
                .expect("真实HTTP身份门");
                assert_eq!(status, 409, "{target} {action}: {value}");
                assert_eq!(value["code"], "binding_stale");
            }
            let (status, value) = call(
                &mut owner,
                "POST",
                &format!("/service/v2/connections/{encoded}/disconnect"),
                Some(original),
                true,
                false,
            )
            .expect("真实HTTP断开");
            assert_eq!(status, 200, "{target}: {value}");
            assert_eq!(value["connection_id"], target);
            assert_eq!(value["connected"], false);
            assert_eq!(probe.observed.lock().expect("probe").stops, 0);
            assert_eq!(probe.observed.lock().expect("probe").targets, 0);
        }
    }

    #[test]
    fn invalid_connection_segments_are_rejected_before_session_lookup() {
        let (mut owner, probe) = fixture();
        let body = lifecycle(&mut owner);
        for segment in [
            "",
            "can%",
            "can%3",
            "can%GG1",
            "can%FF1",
            "can%3A1%2Freconnect",
            "can%3A1%5Cextra",
            "can%3A1%00",
            "can%3A1%0A",
            "can:1/extra",
        ] {
            for action in ["reconnect", "disconnect"] {
                let (status, _) = call(
                    &mut owner,
                    "POST",
                    &format!("/service/v2/connections/{segment}/{action}"),
                    Some(body.clone()),
                    true,
                    false,
                )
                .expect("非法路径HTTP响应");
                assert_eq!(status, 400, "{segment} {action}");
            }
        }
        let (status, value) = call(
            &mut owner,
            "POST",
            "/service/v2/connections/can%253A1/disconnect",
            Some(body),
            true,
            false,
        )
        .expect("重复编码HTTP响应");
        assert_eq!(status, 404);
        assert_eq!(value["code"], "not_found");
        assert!(
            owner
                .integration_session_mut("can:1")
                .expect("原会话")
                .integration_connected()
        );
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
        assert_eq!(probe.observed.lock().expect("probe").targets, 0);
    }

    #[test]
    fn lifecycle_scope_rejections_do_not_stop_or_invalidate_owner() {
        let (mut owner, probe) = fixture();
        let original = lifecycle(&mut owner);
        for (key, bad, code) in [
            ("instance_id", json!("old-instance"), "instance_mismatch"),
            ("connection_generation", json!("0"), "binding_stale"),
            (
                "expected_identity",
                json!({"uid":"00","run_nonce":"00"}),
                "identity_changed",
            ),
        ] {
            let mut body = original.clone();
            body[key] = bad;
            let (status, value) = call(
                &mut owner,
                "POST",
                "/service/v2/connections/can:1/disconnect",
                Some(body),
                true,
                false,
            )
            .expect("response");
            assert_eq!(status, 409);
            assert_eq!(value["code"], code);
            assert_eq!(lifecycle(&mut owner), original);
        }
        let (status, value) = call(
            &mut owner,
            "POST",
            "/service/v2/connections/can:1/disconnect",
            Some(original.clone()),
            true,
            false,
        )
        .expect("disconnect");
        assert_eq!(status, 200);
        assert_eq!(value["connected"], false);
        assert_ne!(
            value["connection_generation"],
            original["connection_generation"]
        );
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
        assert_eq!(probe.observed.lock().expect("probe").targets, 0);
        let (status, _) = call(
            &mut owner,
            "POST",
            "/service/v2/connections/can:1/disconnect",
            Some(original),
            true,
            false,
        )
        .expect("stale");
        assert_eq!(status, 409);
        let (status, current) = call(
            &mut owner,
            "GET",
            "/service/v2/connections",
            None,
            true,
            false,
        )
        .expect("cache");
        assert_eq!(status, 200);
        assert_eq!(current["connections"][0]["connected"], false);
        assert_eq!(
            current["connections"][0]["connection_generation"],
            value["connection_generation"]
        );
    }
    #[test]
    fn event_http_preserves_raw_numbers_and_independent_cursors() {
        let (mut owner, _) = fixture();
        let state = owner.p2.as_mut().expect("p2");
        state
            .events
            .append(
                Instant::now(),
                events::EventInput {
                    kind: "telemetry",
                    source_id: "can:1:g1".into(),
                    source_timestamp: events::SourceTimestamp::DeviceMonotonicUs(
                        9_007_199_254_740_993,
                    ),
                    received_at: 42,
                    payload: RawValue::from_string(
                        "{\"number\":9007199254740993,\"decimal\":1.0000000000000000001}".into(),
                    )
                    .expect("raw"),
                    source_gap: None,
                },
            )
            .expect("event");
        let credentials = owner.v2.as_ref().expect("credentials");
        let bytes = events_response(
            owner.p2.as_mut().expect("p2"),
            credentials,
            "/service/v2/events?after=0&limit=1",
        )
        .expect("raw response");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("9007199254740993"));
        assert!(text.contains("1.0000000000000000001"));
        for _ in 0..2 {
            let (status, value) = call(
                &mut owner,
                "GET",
                "/service/v2/events?after=0&limit=1",
                None,
                true,
                false,
            )
            .expect("batch");
            assert_eq!(status, 200);
            assert_eq!(value["cursor"], "1");
            assert_eq!(value["events"].as_array().expect("events").len(), 1);
        }
        let (_, future) = call(
            &mut owner,
            "GET",
            "/service/v2/events?after=999",
            None,
            true,
            false,
        )
        .expect("future");
        assert_eq!(future["gap"], true);
        assert_eq!(future["gap_reasons"], json!(["cursor_ahead"]));
        let (status, _) = call(
            &mut owner,
            "GET",
            "/service/v2/events?limit=101",
            None,
            true,
            false,
        )
        .expect("limit");
        assert_eq!(status, 400);
    }
    #[test]
    fn p2_routes_authenticate_and_reject_unsupported_profile_inputs() {
        let (mut owner, _) = fixture();
        for route in [
            "connections",
            "profiles",
            "adapters",
            "recordings",
            "odrive/devices",
            "events",
        ] {
            let (status, _) = call(
                &mut owner,
                "GET",
                &format!("/service/v2/{route}"),
                None,
                false,
                false,
            )
            .expect("auth");
            assert_eq!(status, 401);
            let (status, _) = call(
                &mut owner,
                "GET",
                &format!("/service/v2/{route}"),
                None,
                true,
                true,
            )
            .expect("origin");
            assert_eq!(status, 403);
        }
        let body = json!({"api_version":2,"instance_id":"i-test","profile":{"id":"fixture","name":"fixture", "adapter":{"kind":"canable2","serial":"stable"},"node":1,"mode":"fd","expected_uid":null,"autoconnect":true}});
        let (status, _) = call(
            &mut owner,
            "POST",
            "/service/v2/profiles",
            Some(body),
            true,
            false,
        )
        .expect("profile");
        assert_eq!(status, 400);
        let (status,_)=call(&mut owner,"POST","/service/v2/connections",Some(json!({"api_version":2,"instance_id":"i-test","transport":"can","channel":"arbitrary"})),true,false).expect("advanced CAN");
        assert_eq!(status, 400);
        let (status, _) = call(
            &mut owner,
            "GET",
            "/service/v2/events?after=0&after=1",
            None,
            true,
            false,
        )
        .expect("duplicate");
        assert_eq!(status, 400);
    }
    #[test]
    fn owner_tick_collects_telemetry_without_event_requests() {
        let (mut owner, probe) = fixture();
        probe.inject_idle(10);
        owner.tick();
        let p2 = owner.p2.as_mut().expect("events");
        let batch = p2.events.batch(Instant::now(), None, 100).expect("batch");
        assert!(batch.events.iter().any(|e| e.kind == "connection"));
        // 合成会话未启用趋势时也不能伪造主动读，目标/Stop 均保持零。
        assert_eq!(probe.observed.lock().expect("probe").targets, 0);
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
    }
    #[test]
    fn scan_projection_keeps_successful_nodes_when_other_nodes_fail() {
        let (sender, completed) = std::sync::mpsc::channel();
        let (jobs, _receiver) = std::sync::mpsc::sync_channel(1);
        let mut scan = CanScanView {
            jobs,
            busy: true,
            completed,
            v2_results: BTreeMap::new(),
            next_v2: 2,
        };
        sender.send((Some("scan-1".into()),super::super::scan_response(vec![json!({"node":1,"identity":{"uid":"abc","sample":{"run_nonce":"def"},"extra":true}})],vec![json!({"node":2,"message":"timeout","unknown":false,"error":{"kind":"timeout"}})]))).expect("result");
        scan.reap();
        assert!(!scan.busy);
        let result = &scan.v2_results["scan-1"];
        assert_eq!(
            result["nodes"][0]["identity"],
            json!({"uid":"abc","run_nonce":"def"})
        );
        assert_eq!(result["state"], "completed");
        assert_eq!(result["nodes"][0]["node"], 1);
        assert_eq!(result["errors"][0]["node"], 2);
        assert!(result["errors"][0]["error_json_text"].is_string());
    }
    #[test]
    fn slow_read_jobs_leave_owner_tick_and_cached_connections_available() {
        let (mut owner, probe) = fixture();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let runner: read_jobs::ReadRunner = Box::new(move |_, _, _| {
            entered_tx.send(()).expect("entered");
            release_rx.recv().expect("release");
            Ok(json!({"adapters":[]}))
        });
        owner.p2.as_mut().expect("p2").read_jobs =
            read_jobs::ReadJobs::with_test_runner(PathBuf::new(), runner);
        let id = owner
            .p2
            .as_mut()
            .expect("p2")
            .read_jobs
            .start(read_jobs::Operation::Adapters)
            .expect("job");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker entered");
        assert!(
            owner
                .p2
                .as_mut()
                .expect("p2")
                .read_jobs
                .start(read_jobs::Operation::Recordings)
                .is_err()
        );
        let start = Instant::now();
        probe.inject_idle(20);
        owner.tick();
        assert!(start.elapsed() < Duration::from_millis(100));
        let (status, cache) = call(
            &mut owner,
            "GET",
            "/service/v2/connections",
            None,
            true,
            false,
        )
        .expect("cache while worker blocked");
        assert_eq!(status, 200);
        assert_eq!(cache["connections"][0]["target_id"], "can:1");
        release_tx.send(()).expect("release");
        let deadline = Instant::now() + Duration::from_secs(1);
        while owner
            .p2
            .as_ref()
            .expect("p2")
            .read_jobs
            .get(&id)
            .expect("job")
            .state
            != read_jobs::JobState::Completed
        {
            assert!(Instant::now() < deadline);
            owner.tick();
            std::thread::yield_now();
        }
        let batch = owner
            .p2
            .as_mut()
            .expect("p2")
            .events
            .batch(Instant::now(), None, 100)
            .expect("events");
        assert!(
            batch
                .events
                .iter()
                .any(|event| event.kind == "job" && event.payload.get().contains("completed"))
        );
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
    }
    #[test]
    fn unidentified_connection_recovery_requires_current_generation_and_explicit_null() {
        // 首次握手失败可保留 request/disconnected 而无 Identity；门禁必须允许显式恢复该状态。
        let mut owner = Workbench::with_test_sessions(
            ToolSession::new(),
            BTreeMap::from([(1, ToolSession::new())]),
        );
        let generation = owner
            .integration_session_mut("can:1")
            .expect("session")
            .connection_generation()
            .to_string();
        let fields = RawFields::parse(
            &json!({"expected_identity":null,"connection_generation":generation}).to_string(),
        )
        .expect("body");
        assert!(checked_connection(&mut owner, "can:1", &fields).is_ok());
        let stale = RawFields::parse(r#"{"expected_identity":null,"connection_generation":"0"}"#)
            .expect("body");
        assert_eq!(
            checked_connection(&mut owner, "can:1", &stale)
                .err()
                .expect("stale")
                .code,
            "binding_stale"
        );
        let (mut identified, _) = fixture();
        let generation = identified
            .integration_session_mut("can:1")
            .expect("session")
            .connection_generation()
            .to_string();
        let null = RawFields::parse(
            &json!({"expected_identity":null,"connection_generation":generation}).to_string(),
        )
        .expect("body");
        assert_eq!(
            checked_connection(&mut identified, "can:1", &null)
                .err()
                .expect("known identity")
                .code,
            "identity_changed"
        );
    }
    #[test]
    fn slow_network_factory_is_a_job_and_duplicate_open_is_rejected() {
        use std::sync::mpsc;
        let mut owner = Workbench::new(std::env::temp_dir());
        let mut state = owner.p2.take().expect("state");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        state.network_jobs =
            super::super::network_jobs::NetworkJobs::with_test_runner(Box::new(move |_| {
                entered_tx.send(()).expect("entered");
                release_rx.recv().expect("release");
                Err("fixture factory failed".into())
            }));
        let fields=RawFields::parse(r#"{"transport":"can","advanced":{"channel":"fixture","mode":"classic","python":null},"node":2}"#).expect("fields");
        let mut odrive = OdriveView::new(None);
        let mut scan = CanScanView::new();
        let (status, job) = route(
            "connections",
            "",
            Method::Post,
            &fields,
            &mut owner,
            &mut state,
            &mut odrive,
            &mut scan,
        )
        .expect("accepted");
        assert_eq!(status, 202);
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker entered");
        let start = Instant::now();
        owner.tick();
        assert!(start.elapsed() < Duration::from_millis(100));
        let error = route(
            "connections",
            "",
            Method::Post,
            &fields,
            &mut owner,
            &mut state,
            &mut odrive,
            &mut scan,
        )
        .expect_err("duplicate rejected");
        assert_eq!(error.code, "busy");
        assert!(owner.service_connections().is_empty());
        release_tx.send(()).expect("release");
        let deadline = Instant::now() + Duration::from_secs(1);
        while state.connection_busy() {
            state.poll_network(&mut owner, &mut scan);
            assert!(Instant::now() < deadline, "factory completion");
            std::thread::yield_now();
        }
        assert_eq!(
            state.profiles[job["job_id"].as_str().expect("job id")]["state"],
            "failed"
        );
        assert!(owner.service_network_generation().is_none());
    }
    #[test]
    fn empty_network_scan_factory_failure_keeps_each_selected_node_and_rejects_batch() {
        let mut owner = Workbench::new(std::env::temp_dir());
        let mut state = owner.p2.take().expect("state");
        state.network_jobs =
            super::super::network_jobs::NetworkJobs::with_test_runner(Box::new(|_| {
                Err("fixture adapter unavailable".into())
            }));
        let mut odrive = OdriveView::new(None);
        let mut scan = CanScanView::new();
        let fields = RawFields::parse(
            r#"{"advanced":{"channel":"fixture","mode":"fd","python":null},"nodes":[2,3]}"#,
        )
        .expect("fields");
        let (status, result) = route(
            "scans",
            "",
            Method::Post,
            &fields,
            &mut owner,
            &mut state,
            &mut odrive,
            &mut scan,
        )
        .expect("scan accepted");
        assert_eq!(status, 202);
        let id = result["scan_id"].as_str().expect("id");
        let deadline = Instant::now() + Duration::from_secs(1);
        while state.connection_busy() {
            state.poll_network(&mut owner, &mut scan);
            assert!(Instant::now() < deadline, "completion");
            std::thread::yield_now();
        }
        assert!(!scan.busy);
        let result = &scan.v2_results[id];
        assert_eq!(result["state"], "completed");
        assert_eq!(result["errors"].as_array().expect("errors").len(), 2);
        assert_eq!(result["nodes"], json!([]));
        let fields =
            RawFields::parse(&json!({"scan_id":id,"nodes":[2]}).to_string()).expect("fields");
        let error = route(
            "connections/batch",
            "",
            Method::Post,
            &fields,
            &mut owner,
            &mut state,
            &mut odrive,
            &mut scan,
        )
        .expect_err("failed scan cannot bind");
        assert_eq!(error.code, "binding_stale");
    }
    #[test]
    fn batch_never_reconnects_an_existing_disconnected_target() {
        let (mut owner, probe) = fixture();
        let mut state = owner.p2.take().expect("state");
        let expected = connection_value(
            "can:1",
            owner.integration_session_mut("can:1").expect("session"),
        )["identity"]
            .clone();
        owner
            .disconnect(super::super::WebTransport::Can, Some(1))
            .expect("explicit disconnect");
        let generation = owner
            .integration_session_mut("can:1")
            .expect("session")
            .connection_generation();
        state.batch = Some(Batch {
            id: "connect-test".into(),
            nodes: VecDeque::from([(1, expected)]),
            connections: Vec::new(),
            errors: Vec::new(),
        });
        state.advance_batch(&mut owner);
        assert_eq!(
            state.profiles["connect-test"]["data"]["errors"][0]["code"],
            "binding_stale"
        );
        let session = owner.integration_session_mut("can:1").expect("session");
        assert_eq!(session.connection_generation(), generation);
        assert!(!session.integration_connected());
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
    }
    #[test]
    fn pinned_profile_uid_mismatch_disconnects_without_implicit_stop() {
        let (mut owner, probe) = fixture();
        let mut state = owner.p2.take().expect("state");
        state
            .usb_expected
            .insert("can:1".into(), "ffffffffffffffffffffffff".into());
        let error = check_usb_uid(&state, &mut owner, "can:1").expect_err("mismatch");
        assert_eq!(error.code, "identity_changed");
        assert!(
            !owner
                .integration_session_mut("can:1")
                .expect("session")
                .integration_connected()
        );
        assert_eq!(probe.observed.lock().expect("probe").stops, 0);
        assert_eq!(probe.observed.lock().expect("probe").targets, 0);
    }
    #[test]
    fn review_recreated_profile_id_cannot_receive_old_session_uid() {
        let (mut owner, _) = fixture();
        let mut state = owner.p2.take().expect("state");
        let root = std::env::temp_dir().join(format!("eha-profile-reuse-{}", unix_ms()));
        let can = crate::connections::ConnectionStore::load(root.join("can.json"));
        let mut usb = UsbProfileStore::load(root.join("usb.json"));
        let make = |serial: &str| UsbProfile {
            transport: super::super::connection_profiles::UsbTransport::Usb,
            id: "same".into(),
            name: "fixture".into(),
            serial: serial.into(),
            expected_uid: None,
        };
        usb.save(make("serial-A"), &can).expect("save A");
        usb.delete("same").expect("delete A");
        usb.save(make("serial-B"), &can).expect("save B");
        state.usb_profiles = Some(usb);
        state
            .usb_profile_ids
            .insert("usb:serial-A".into(), "same".into());
        owner.profiles = Some(can);
        let mut value =
            json!({"identity":{"uid":"070707070707070707070707","run_nonce":"fixture"}});
        let error = bind_usb_profile(&mut state, &mut owner, "usb:serial-A", &mut value)
            .expect_err("old profile reference");
        assert_eq!(error.code, "binding_stale");
        assert_eq!(
            state
                .usb_profiles
                .as_ref()
                .expect("store")
                .profile("same")
                .expect("B")
                .expected_uid,
            None
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
