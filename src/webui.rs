// Copyright The eha_controller Contributors
//! `eha-tool webui` 的仅本机 SDK 会话与静态资源服务。

use crate::session::{Command, ConnectionRequest, ToolSession};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{self, Read},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tiny_http::{Header, Method as HttpMethod, Response, Server, StatusCode};

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";
const MAX_JSON_BODY: u64 = 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Method {
    Get,
    Head,
    Post,
    Other,
}

/// HTTP 请求明确选择的本地 SDK 会话。
///
/// 这只决定本次请求投影或调用哪条已持有的通路；浏览器选择不会修改服务端的全局
/// “当前设备”。USB 与 CAN 会话可同时连接，并各自保存身份、心跳、维护键和趋势。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WebTransport {
    Usb,
    Can,
}

impl WebTransport {
    fn from_connection(connection: &ConnectionRequest) -> Self {
        match connection {
            ConnectionRequest::Usb { .. } => Self::Usb,
            ConnectionRequest::Can { .. } => Self::Can,
        }
    }
}

/// WebUI 固定持有两条互不替换的本地 SDK 会话。
struct Sessions {
    usb: ToolSession,
    can: ToolSession,
}

impl Sessions {
    fn new() -> Self {
        Self {
            usb: ToolSession::new().with_telemetry_trend(4_096),
            can: ToolSession::new().with_telemetry_trend(4_096),
        }
    }

    fn selected(&mut self, transport: WebTransport) -> &mut ToolSession {
        match transport {
            WebTransport::Usb => &mut self.usb,
            WebTransport::Can => &mut self.can,
        }
    }

    /// 一次轮询同时驱动两条会话的被动接收，并只把趋势投影给浏览器选中的通路。
    fn snapshot_json(
        &mut self,
        selected: WebTransport,
        trend_after: Option<Option<u64>>,
        odrive: Option<&Value>,
    ) -> Result<Value, String> {
        // `snapshot` 负责 drain SDK 遥测队列。两条会话都恰好调用一次，避免选中通路
        // 的重复 drain，也避免未选中通路因页面停留在另一页而积压。
        let usb = serde_json::to_value(self.usb.snapshot()).map_err(|error| error.to_string())?;
        let can = serde_json::to_value(self.can.snapshot()).map_err(|error| error.to_string())?;
        let mut snapshot = match selected {
            WebTransport::Usb => usb.clone(),
            WebTransport::Can => can.clone(),
        };
        if let Some(after) = trend_after {
            let trend = match selected {
                WebTransport::Usb => self.usb.telemetry_trend_json(after),
                WebTransport::Can => self.can.telemetry_trend_json(after),
            };
            snapshot["trend"] = trend;
        }
        snapshot["odrive_usb"] = odrive.cloned().unwrap_or(Value::Null);
        Ok(json!({
            "snapshot": snapshot,
            "sessions": {"usb": usb, "can": can},
            "transport": selected,
        }))
    }
}
struct StaticResponse {
    status: u16,
    content_type: &'static str,
    content_length: usize,
    body: Vec<u8>,
}
struct Asset {
    route: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}
const ASSETS: &[Asset] = &[
    Asset {
        route: "/",
        content_type: "text/html; charset=utf-8",
        bytes: include_bytes!("../webui/static/index.html"),
    },
    Asset {
        route: "/index.html",
        content_type: "text/html; charset=utf-8",
        bytes: include_bytes!("../webui/static/index.html"),
    },
    Asset {
        route: "/styles.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../webui/static/styles.css"),
    },
    Asset {
        route: "/app.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/app.js"),
    },
    Asset {
        route: "/telemetry.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/telemetry.js"),
    },
    Asset {
        route: "/favicon.svg",
        content_type: "image/svg+xml",
        bytes: include_bytes!("../webui/static/favicon.svg"),
    },
    Asset {
        route: "/vendor/echarts.min.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/echarts.min.js"),
    },
    Asset {
        route: "/vendor/echarts.LICENSE.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/echarts.LICENSE.txt"),
    },
    Asset {
        route: "/vendor/echarts.LICENSE-d3.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/echarts.LICENSE-d3.txt"),
    },
    Asset {
        route: "/vendor/echarts.NOTICE.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/echarts.NOTICE.txt"),
    },
    Asset {
        route: "/vendor/tslib.CopyrightNotice.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/tslib.CopyrightNotice.txt"),
    },
    Asset {
        route: "/vendor/tslib.LICENSE.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/tslib.LICENSE.txt"),
    },
    Asset {
        route: "/vendor/zrender.LICENSE.txt",
        content_type: "text/plain; charset=utf-8",
        bytes: include_bytes!("../webui/static/vendor/zrender.LICENSE.txt"),
    },
];
fn request_method(request: &tiny_http::Request) -> Method {
    match request.method() {
        HttpMethod::Get => Method::Get,
        HttpMethod::Head => Method::Head,
        HttpMethod::Post => Method::Post,
        _ => Method::Other,
    }
}
fn request_path(request: &tiny_http::Request) -> &str {
    request
        .url()
        .split_once('?')
        .map_or(request.url(), |(path, _)| path)
}
fn static_response(method: Method, path: &str) -> StaticResponse {
    if !matches!(method, Method::Get | Method::Head) {
        return StaticResponse {
            status: 405,
            content_type: "text/plain; charset=utf-8",
            content_length: 19,
            body: b"method not allowed\n".to_vec(),
        };
    }
    let Some(asset) = ASSETS.iter().find(|asset| asset.route == path) else {
        return StaticResponse {
            status: 404,
            content_type: "text/plain; charset=utf-8",
            content_length: 10,
            body: b"not found\n".to_vec(),
        };
    };
    StaticResponse {
        status: 200,
        content_type: asset.content_type,
        content_length: asset.bytes.len(),
        body: if method == Method::Head {
            Vec::new()
        } else {
            asset.bytes.to_vec()
        },
    }
}

struct OdriveView {
    last_read: Option<Value>,
    busy: bool,
    jobs: SyncSender<OdriveJob>,
    completed: Receiver<OdriveResult>,
    _worker: JoinHandle<()>,
}

impl OdriveView {
    fn new(python: Option<PathBuf>) -> Self {
        let python = crate::odrive_commands::python_path(python);
        Self::with_runner(Arc::new(move |operation| match operation {
            OdriveOperation::Discover => {
                match eha_sdk::odrive::discover(&python, Duration::from_secs(5)) {
                    Ok(mut value) => {
                        value["ok"] = json!(true);
                        OdriveResult {
                            status: 200,
                            value,
                            cache_read: false,
                        }
                    }
                    Err(error) => OdriveResult {
                        status: 503,
                        value: crate::odrive_commands::error_json(&error),
                        cache_read: false,
                    },
                }
            }
            OdriveOperation::Read { serial } => {
                match eha_sdk::odrive::read(&python, &serial, Duration::from_secs(5)) {
                    Ok(snapshot) => OdriveResult::success(snapshot),
                    Err(error) => {
                        OdriveResult::failure(&serial, crate::odrive_commands::error_json(&error))
                    }
                }
            }
        }))
    }

    fn with_runner(runner: OdriveRunner) -> Self {
        let (jobs, receiver) = mpsc::sync_channel::<OdriveJob>(1);
        let (sender, completed) = mpsc::channel();
        let worker = thread::spawn(move || {
            while let Ok(job) = receiver.recv() {
                let result = runner(job.operation);
                if sender.send(result.clone()).is_err() {
                    break;
                }
                if let Err(error) = respond_json(job.request, result.status, result.value.clone()) {
                    eprintln!("warning: ODrive WebUI response failed: {error}");
                }
            }
        });
        Self {
            last_read: None,
            busy: false,
            jobs,
            completed,
            _worker: worker,
        }
    }

    fn reap(&mut self) {
        loop {
            match self.completed.try_recv() {
                Ok(result) => {
                    self.busy = false;
                    if result.cache_read {
                        self.last_read = Some(result.value);
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.busy = false;
                    return;
                }
            }
        }
    }

    fn start(
        &mut self,
        request: tiny_http::Request,
        operation: OdriveOperation,
    ) -> Result<(), OdriveStartError> {
        self.reap();
        if self.busy {
            return Err(OdriveStartError::Busy(Box::new(request)));
        }
        match self.jobs.try_send(OdriveJob { request, operation }) {
            Ok(()) => {
                self.busy = true;
                Ok(())
            }
            Err(TrySendError::Full(job)) => Err(OdriveStartError::Busy(Box::new(job.request))),
            Err(TrySendError::Disconnected(job)) => {
                Err(OdriveStartError::Unavailable(Box::new(job.request)))
            }
        }
    }
}

type OdriveRunner = Arc<dyn Fn(OdriveOperation) -> OdriveResult + Send + Sync>;

enum OdriveOperation {
    Discover,
    Read { serial: String },
}

struct OdriveJob {
    request: tiny_http::Request,
    operation: OdriveOperation,
}

#[derive(Clone)]
struct OdriveResult {
    status: u16,
    value: Value,
    cache_read: bool,
}

impl OdriveResult {
    fn success(snapshot: Value) -> Self {
        Self {
            status: 200,
            value: json!({"ok":true,"message":"已取得 ODrive USB 只读快照。","snapshot":snapshot}),
            cache_read: true,
        }
    }

    fn failure(serial: &str, mut value: Value) -> Self {
        value["serial_number"] = json!(serial);
        Self {
            status: 503,
            value,
            cache_read: true,
        }
    }
}

enum OdriveStartError {
    Busy(Box<tiny_http::Request>),
    Unavailable(Box<tiny_http::Request>),
}

pub(crate) fn serve(port: u16, odrive_python: Option<PathBuf>) -> Result<(), String> {
    let server = Server::http(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    println!("WebUI 本地入口：http://127.0.0.1:{port}/");
    let mut sessions = Sessions::new();
    let mut odrive = OdriveView::new(odrive_python);
    for request in server.incoming_requests() {
        if let Err(error) = handle_request(request, port, &mut sessions, &mut odrive) {
            eprintln!("warning: WebUI response failed: {error}");
        }
    }
    Ok(())
}
fn handle_request(
    mut request: tiny_http::Request,
    port: u16,
    sessions: &mut Sessions,
    odrive: &mut OdriveView,
) -> Result<(), String> {
    odrive.reap();
    let method = request_method(&request);
    let path = request_path(&request).to_owned();
    if path == "/api/snapshot" && method == Method::Get {
        let after = snapshot_after(request.url());
        let transport = match query_transport(request.url()) {
            Ok(transport) => transport,
            Err(message) => {
                return respond_json(request, 400, json!({"ok":false,"message":message}));
            }
        };
        return session_response(
            request,
            200,
            sessions,
            transport,
            Some(after),
            odrive.last_read.as_ref(),
            json!({"ok":true}),
        );
    }
    if path == "/api/odrive/devices" && method == Method::Get {
        return start_odrive_operation(request, odrive, OdriveOperation::Discover);
    }
    if path == "/api/devices" && method == Method::Get {
        return match crate::devices::discover() {
            Ok(devices) => respond_json(
                request,
                200,
                json!({"ok":true,"devices":devices.into_iter().map(|device| json!({"serial":device.serial,"description":device.description})).collect::<Vec<_>>() }),
            ),
            Err(error) => respond_json(request, 503, json!({"ok":false,"message":error})),
        };
    }
    if path == "/api/can/devices" && method == Method::Get {
        return match eha_sdk::can::discover_serial_candidates() {
            Ok(devices) => respond_json(
                request,
                200,
                json!({"ok":true,"devices":devices.into_iter().map(|device| json!({"port":device.port,"serial":device.serial,"description":device.description})).collect::<Vec<_>>() }),
            ),
            Err(error) => respond_json(request, 503, json!({"ok":false,"message":error})),
        };
    }
    if path.starts_with("/api/") {
        if method != Method::Post {
            return respond_json(
                request,
                405,
                json!({"ok":false,"message":"API 只接受 POST"}),
            );
        }
        if let Err(message) = validate_write_request(&request, port) {
            return respond_json(request, 403, json!({"ok":false,"message":message}));
        }
        let body = match parse_json_body(&mut request) {
            Ok(body) => body,
            Err(message) => {
                return respond_json(request, 400, json!({"ok":false,"message":message}));
            }
        };
        return match path.as_str() {
            "/api/odrive/read" => {
                let Some(serial) = body.get("serial").and_then(Value::as_str) else {
                    return respond_json(
                        request,
                        400,
                        json!({"ok":false,"message":"请明确选择 ODrive USB 序列号。"}),
                    );
                };
                start_odrive_operation(
                    request,
                    odrive,
                    OdriveOperation::Read {
                        serial: serial.to_owned(),
                    },
                )
            }
            "/api/connect" => match serde_json::from_value::<ConnectionRequest>(body) {
                Ok(connection) => {
                    let transport = WebTransport::from_connection(&connection);
                    match sessions.selected(transport).connect(connection) {
                        Ok(_) => session_response(
                            request,
                            200,
                            sessions,
                            transport,
                            None,
                            odrive.last_read.as_ref(),
                            json!({"ok":true,"message":"连接并已核对身份。"}),
                        ),
                        Err(error) => session_structured_error(
                            request,
                            sessions,
                            transport,
                            odrive.last_read.as_ref(),
                            error,
                        ),
                    }
                }
                Err(error) => respond_json(
                    request,
                    400,
                    json!({"ok":false,"message":format!("连接参数无效：{error}")}),
                ),
            },
            "/api/disconnect" => {
                let transport = match body_transport(&body) {
                    Ok(transport) => transport,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                match sessions.selected(transport).disconnect() {
                    Ok(_) => session_response(
                        request,
                        200,
                        sessions,
                        transport,
                        None,
                        odrive.last_read.as_ref(),
                        json!({"ok":true,"message":"本地连接已关闭；没有发送 Stop。"}),
                    ),
                    Err(error) => session_structured_error(
                        request,
                        sessions,
                        transport,
                        odrive.last_read.as_ref(),
                        error,
                    ),
                }
            }
            "/api/reconnect" => {
                let transport = match body_transport(&body) {
                    Ok(transport) => transport,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                match sessions.selected(transport).reconnect() {
                    Ok(_) => session_response(
                        request,
                        200,
                        sessions,
                        transport,
                        None,
                        odrive.last_read.as_ref(),
                        json!({"ok":true,"message":"已重新连接并核对身份。"}),
                    ),
                    Err(error) => session_structured_error(
                        request,
                        sessions,
                        transport,
                        odrive.last_read.as_ref(),
                        error,
                    ),
                }
            }
            "/api/action" => {
                let transport = match body_transport(&body) {
                    Ok(transport) => transport,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                match serde_json::from_value::<Command>(body) {
                    Ok(command) => {
                        if command_needs_identity_refresh(&command)
                            && let Err(error) = sessions.selected(transport).refresh_identity()
                        {
                            return session_structured_error(
                                request,
                                sessions,
                                transport,
                                odrive.last_read.as_ref(),
                                error,
                            );
                        }
                        match sessions.selected(transport).execute(command) {
                            Ok(result) => session_response(
                                request,
                                200,
                                sessions,
                                transport,
                                None,
                                odrive.last_read.as_ref(),
                                json!({"ok":true,"message":if result.local_submission.is_some() { "请求已本地提交；设备采用与执行请查看状态。" } else { "已取得操作结果；设备状态单独展示。" },"result":result}),
                            ),
                            Err(error) => session_structured_error(
                                request,
                                sessions,
                                transport,
                                odrive.last_read.as_ref(),
                                error,
                            ),
                        }
                    }
                    Err(error) => session_response(
                        request,
                        400,
                        sessions,
                        transport,
                        None,
                        odrive.last_read.as_ref(),
                        json!({"ok":false,"message":format!("动作参数无效：{error}")}),
                    ),
                }
            }
            _ => respond_json(request, 404, json!({"ok":false,"message":"未知 API 路径"})),
        };
    }
    respond_static(request, static_response(method, &path))
}

fn start_odrive_operation(
    request: tiny_http::Request,
    odrive: &mut OdriveView,
    operation: OdriveOperation,
) -> Result<(), String> {
    match odrive.start(request, operation) {
        Ok(()) => Ok(()),
        Err(OdriveStartError::Busy(request)) => respond_json(
            *request,
            409,
            json!({"ok":false,"message":"ODrive USB 只读正在进行；请等待当前读取完成。"}),
        ),
        Err(OdriveStartError::Unavailable(request)) => respond_json(
            *request,
            503,
            json!({"ok":false,"message":"ODrive USB 只读工作线程不可用。"}),
        ),
    }
}

fn snapshot_after(url: &str) -> Option<u64> {
    query_field(url, "after")?.parse().ok()
}
fn query_field<'a>(url: &'a str, name: &str) -> Option<&'a str> {
    url.split_once('?')?
        .1
        .split('&')
        .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
}
fn query_transport(url: &str) -> Result<WebTransport, &'static str> {
    let Some(value) = query_field(url, "transport") else {
        return Err("snapshot 请求必须明确指定 transport=usb 或 transport=can");
    };
    parse_transport_value(value)
}
fn body_transport(body: &Value) -> Result<WebTransport, &'static str> {
    let Some(value) = body.get("transport").and_then(Value::as_str) else {
        return Err("请求必须明确指定 transport=usb 或 transport=can");
    };
    parse_transport_value(value)
}
fn parse_transport_value(value: &str) -> Result<WebTransport, &'static str> {
    match value {
        "usb" => Ok(WebTransport::Usb),
        "can" => Ok(WebTransport::Can),
        _ => Err("transport 必须为 usb 或 can"),
    }
}
fn command_needs_identity_refresh(command: &Command) -> bool {
    matches!(
        command,
        Command::ConfigSave { .. }
            | Command::RestoreFactory
            | Command::ResetApplication
            | Command::EnterUpdate
    )
}
fn session_response(
    request: tiny_http::Request,
    status: u16,
    sessions: &mut Sessions,
    transport: WebTransport,
    trend_after: Option<Option<u64>>,
    odrive: Option<&Value>,
    mut value: Value,
) -> Result<(), String> {
    let state = sessions.snapshot_json(transport, trend_after, odrive)?;
    let object = value
        .as_object_mut()
        .ok_or("WebUI session response must be a JSON object")?;
    object.insert("snapshot".into(), state["snapshot"].clone());
    object.insert("sessions".into(), state["sessions"].clone());
    object.insert("transport".into(), state["transport"].clone());
    respond_json(request, status, value)
}
fn session_structured_error(
    request: tiny_http::Request,
    sessions: &mut Sessions,
    transport: WebTransport,
    odrive: Option<&Value>,
    error: crate::session::SessionError,
) -> Result<(), String> {
    let message = error.message.clone();
    let unknown = error.unknown;
    let operation_key = error.operation_key.clone();
    session_response(
        request,
        409,
        sessions,
        transport,
        None,
        odrive,
        json!({"ok":false,"message":message,"unknown":unknown,"operation_key":operation_key,"error":error}),
    )
}
fn header<'a>(request: &'a tiny_http::Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}
fn validate_write_request(request: &tiny_http::Request, port: u16) -> Result<(), &'static str> {
    let host = format!("127.0.0.1:{port}");
    if header(request, "Host") != Some(host.as_str()) {
        return Err("Host 不是本机 WebUI，已拒绝写入请求");
    }
    if header(request, "Origin") != Some(format!("http://{host}").as_str()) {
        return Err("跨来源或缺少 Origin 的写入请求已拒绝");
    }
    let content_type =
        header(request, "Content-Type").ok_or("缺少 JSON Content-Type，已拒绝写入请求")?;
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err("写入请求必须使用 application/json");
    }
    Ok(())
}
fn parse_json_body(request: &mut tiny_http::Request) -> Result<Value, String> {
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_JSON_BODY + 1)
        .read_to_string(&mut body)
        .map_err(|error| error.to_string())?;
    if body.len() as u64 > MAX_JSON_BODY {
        return Err("JSON 请求过大".into());
    }
    serde_json::from_str(&body).map_err(|error| format!("JSON 无效：{error}"))
}
fn common_headers(
    response: &mut Response<std::io::Cursor<Vec<u8>>>,
    content_type: &str,
    length: usize,
    allow: Option<&str>,
) -> Result<(), String> {
    for (name, value) in [
        ("Content-Type", content_type),
        ("Cache-Control", "no-store"),
        ("Content-Security-Policy", CONTENT_SECURITY_POLICY),
        ("X-Content-Type-Options", "nosniff"),
        ("Content-Length", &length.to_string()),
    ] {
        response.add_header(Header::from_bytes(name, value).map_err(|_| "invalid HTTP header")?);
    }
    if let Some(allow) = allow {
        response.add_header(Header::from_bytes("Allow", allow).map_err(|_| "invalid HTTP header")?);
    }
    Ok(())
}
fn respond_static(request: tiny_http::Request, response: StaticResponse) -> Result<(), String> {
    let mut http_response =
        Response::from_data(response.body).with_status_code(StatusCode(response.status));
    common_headers(
        &mut http_response,
        response.content_type,
        response.content_length,
        (response.status == 405).then_some("GET, HEAD"),
    )?;
    request
        .respond(http_response)
        .map_err(|error: io::Error| error.to_string())
}
fn respond_json(request: tiny_http::Request, status: u16, value: Value) -> Result<(), String> {
    let body = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    let length = body.len();
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    common_headers(
        &mut response,
        "application/json; charset=utf-8",
        length,
        (status == 405).then_some("GET, POST"),
    )?;
    request
        .respond(response)
        .map_err(|error: io::Error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        CONTENT_SECURITY_POLICY, OdriveOperation, OdriveResult, OdriveView, Sessions,
        handle_request,
    };
    use serde_json::{Value, json};
    use std::{
        error::Error,
        io::{Read, Write},
        net::TcpStream,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };
    use tiny_http::Server;
    fn send_request(
        request: impl FnOnce(u16) -> String,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let server = Server::http(("127.0.0.1", 0))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or("test server did not use an IP socket")?;
        let port = address.port();
        let request = request(port);
        let client = thread::spawn(move || -> Result<String, std::io::Error> {
            let mut stream = TcpStream::connect(address)?;
            stream.write_all(request.as_bytes())?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        let mut sessions = Sessions::new();
        let mut odrive = OdriveView::new(None);
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)
            .map_err(std::io::Error::other)?;
        Ok(client.join().map_err(|_| "HTTP client thread panicked")??)
    }
    #[test]
    fn static_routes_and_cross_origin_writes_are_rejected_before_session_use()
    -> Result<(), Box<dyn Error + Send + Sync>> {
        let get = send_request(|_| {
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n".into()
        })?;
        assert!(get.starts_with("HTTP/1.1 200"));
        assert!(get.contains(CONTENT_SECURITY_POLICY));
        let head = send_request(|_| {
            "HEAD / HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n".into()
        })?;
        assert!(head.starts_with("HTTP/1.1 200"));
        assert!(head.ends_with("\r\n\r\n"));
        let bad_post = send_request(|_| {
            "POST /api/action HTTP/1.1\r\nHost: evil.test\r\nOrigin: http://evil.test\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"action\":\"stop\"}".into()
        })?;
        assert!(bad_post.starts_with("HTTP/1.1 403"));
        Ok(())
    }

    fn session_request(
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        send_request(|port| {
            let mut request = format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
            );
            if let Some(body) = body {
                request.push_str(&format!(
                    "Origin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
                    body.len()
                ));
                request.push_str("\r\n");
                request.push_str(body);
            } else {
                request.push_str("\r\n");
            }
            request
        })
    }

    fn response_json(response: &str) -> Result<Value, Box<dyn Error + Send + Sync>> {
        let (_, body) = response
            .split_once("\r\n\r\n")
            .ok_or("HTTP response has no body")?;
        Ok(serde_json::from_str(body)?)
    }

    fn request_text(port: u16, method: &str, path: &str, body: Option<&str>) -> String {
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
        if let Some(body) = body {
            request.push_str(&format!(
                "Origin: http://127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
            request.push_str("\r\n");
            request.push_str(body);
        } else {
            request.push_str("\r\n");
        }
        request
    }

    fn request_client(
        address: std::net::SocketAddr,
        request: String,
    ) -> thread::JoinHandle<Result<String, std::io::Error>> {
        thread::spawn(move || {
            let mut stream = TcpStream::connect(address)?;
            stream.write_all(request.as_bytes())?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        })
    }

    fn reap_worker(odrive: &mut OdriveView) -> Result<(), &'static str> {
        for _ in 0..100 {
            odrive.reap();
            if !odrive.busy {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(1));
        }
        Err("ODrive worker did not finish")
    }

    #[test]
    fn session_routes_require_an_explicit_transport_before_any_action()
    -> Result<(), Box<dyn Error + Send + Sync>> {
        let missing = session_request("POST", "/api/action", Some(r#"{"action":"stop"}"#))?;
        assert!(missing.starts_with("HTTP/1.1 400"));
        assert!(missing.contains("必须明确指定 transport"));

        let invalid = session_request(
            "POST",
            "/api/action",
            Some(r#"{"transport":"serial","action":"stop"}"#),
        )?;
        assert!(invalid.starts_with("HTTP/1.1 400"));
        assert!(invalid.contains("transport 必须为 usb 或 can"));

        let missing_snapshot = session_request("GET", "/api/snapshot", None)?;
        assert!(missing_snapshot.starts_with("HTTP/1.1 400"));

        let rejected = session_request(
            "POST",
            "/api/action",
            Some(r#"{"transport":"can","action":"stop"}"#),
        )?;
        assert!(rejected.starts_with("HTTP/1.1 409"));
        assert!(response_json(&rejected)?["snapshot"].get("trend").is_none());
        Ok(())
    }

    #[test]
    fn odrive_worker_keeps_snapshot_responsive_and_caches_only_completed_results()
    -> Result<(), Box<dyn Error + Send + Sync>> {
        let server = Server::http(("127.0.0.1", 0))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or("test server did not use an IP socket")?;
        let port = address.port();
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let runner_started = started.clone();
        let runner_release = release.clone();
        let runner = Arc::new(move |operation| match operation {
            OdriveOperation::Read { serial } if serial == "slow" => {
                runner_started.wait();
                runner_release.wait();
                OdriveResult::success(json!({"source":"odrive_usb","serial_number":"slow"}))
            }
            OdriveOperation::Read { serial } if serial == "fail" => OdriveResult::failure(
                "fail",
                json!({"ok":false,"source":"odrive_usb","message":"fixture read failed","error":{"kind":"read","message":"fixture read failed"}}),
            ),
            OdriveOperation::Read { serial } => {
                OdriveResult::success(json!({"source":"odrive_usb","serial_number":serial}))
            }
            OdriveOperation::Discover => OdriveResult {
                status: 200,
                value: json!({"ok":true,"devices":[]}),
                cache_read: false,
            },
        });
        let mut sessions = Sessions::new();
        let mut odrive = OdriveView::with_runner(runner);

        let slow = request_client(
            address,
            request_text(
                port,
                "POST",
                "/api/odrive/read",
                Some(r#"{"serial":"slow"}"#),
            ),
        );
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)?;
        started.wait();

        let snapshot = request_client(
            address,
            request_text(port, "GET", "/api/snapshot?transport=usb", None),
        );
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)?;
        let snapshot = snapshot.join().map_err(|_| "snapshot client panicked")??;
        assert!(snapshot.starts_with("HTTP/1.1 200"));
        assert!(response_json(&snapshot)?["snapshot"]["odrive_usb"].is_null());

        let duplicate = request_client(
            address,
            request_text(port, "GET", "/api/odrive/devices", None),
        );
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)?;
        let duplicate = duplicate
            .join()
            .map_err(|_| "duplicate client panicked")??;
        assert!(duplicate.starts_with("HTTP/1.1 409"));

        release.wait();
        let slow = slow.join().map_err(|_| "slow client panicked")??;
        assert!(slow.starts_with("HTTP/1.1 200"));
        let slow_body = response_json(&slow)?;
        assert_eq!(slow_body["ok"], true);
        assert_eq!(slow_body["snapshot"]["serial_number"], "slow");
        reap_worker(&mut odrive)?;

        let cached = request_client(
            address,
            request_text(port, "GET", "/api/snapshot?transport=usb", None),
        );
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)?;
        let cached = response_json(&cached.join().map_err(|_| "cache client panicked")??)?;
        assert_eq!(cached["snapshot"]["odrive_usb"], slow_body);

        let failed = request_client(
            address,
            request_text(
                port,
                "POST",
                "/api/odrive/read",
                Some(r#"{"serial":"fail"}"#),
            ),
        );
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)?;
        let failed = failed.join().map_err(|_| "failure client panicked")??;
        assert!(failed.starts_with("HTTP/1.1 503"));
        let failed_body = response_json(&failed)?;
        assert_eq!(failed_body["error"]["kind"], "read");
        reap_worker(&mut odrive)?;

        let cached_failure = request_client(
            address,
            request_text(port, "GET", "/api/snapshot?transport=usb", None),
        );
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)?;
        let cached_failure = response_json(
            &cached_failure
                .join()
                .map_err(|_| "failure cache client panicked")??,
        )?;
        assert_eq!(cached_failure["snapshot"]["odrive_usb"], failed_body);

        let retry = request_client(
            address,
            request_text(
                port,
                "POST",
                "/api/odrive/read",
                Some(r#"{"serial":"recovered"}"#),
            ),
        );
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)?;
        let retry = response_json(&retry.join().map_err(|_| "retry client panicked")??)?;
        assert_eq!(retry["snapshot"]["serial_number"], "recovered");
        reap_worker(&mut odrive)?;
        Ok(())
    }
}
