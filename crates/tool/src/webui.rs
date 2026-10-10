// Copyright The eha-sdk Contributors
//! `eha-tool webui` 的仅本机 SDK 会话与静态资源服务。

use self::{
    http_body::{BodyReader, ReadyRequest},
    workbench::{Transport as WebTransport, TrialAction, Workbench},
};
use crate::session::{Command, ConnectionRequest, ToolSession};
use eha_sdk::can::CanNodeConnector;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tiny_http::{Header, Method as HttpMethod, Response, Server, StatusCode};

mod http_body;
mod workbench;

#[cfg(test)]
struct Sessions(Workbench);

#[cfg(test)]
impl Sessions {
    fn new() -> Self {
        Self(Workbench::new(
            std::env::temp_dir().join("eha-tool-webui-tests"),
        ))
    }
}

#[cfg(test)]
impl std::ops::Deref for Sessions {
    type Target = Workbench;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
impl std::ops::DerefMut for Sessions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

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
        route: "/config-editor.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/config-editor.js"),
    },
    Asset {
        route: "/trial-ui.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/trial-ui.js"),
    },
    Asset {
        route: "/trial-state.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/trial-state.js"),
    },
    Asset {
        route: "/recording-ui.js",
        content_type: "application/javascript; charset=utf-8",
        bytes: include_bytes!("../webui/static/recording-ui.js"),
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

struct CanScanView {
    jobs: SyncSender<CanScanJob>,
    busy: bool,
    completed: Receiver<()>,
}
struct CanScanJob {
    request: tiny_http::Request,
    channel: String,
    mode: String,
    python: Option<String>,
    connectors: Vec<(u8, CanNodeConnector)>,
}
impl CanScanView {
    fn new() -> Self {
        let (jobs, receiver) = mpsc::sync_channel::<CanScanJob>(1);
        let (done, completed) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(job) = receiver.recv() {
                let mut nodes = Vec::new();
                let mut errors = Vec::new();
                for (node, connector) in job.connectors {
                    let request = ConnectionRequest::Can {
                        channel: job.channel.clone(),
                        node,
                        mode: job.mode.clone(),
                        python: job.python.clone(),
                    };
                    let mut session = ToolSession::new().with_timeout(Duration::from_millis(250));
                    match session.connect_shared_can(request, connector) {
                    Ok(snapshot) => nodes.push(json!({"node":node,"identity":snapshot.identity})),
                    Err(error) => errors.push(json!({"node":node,"message":error.message,"unknown":error.unknown,"error":error})),
                }
                }
                let _ = respond_json(job.request, 200, scan_response(nodes, errors));
                let _ = done.send(());
            }
        });
        Self {
            jobs,
            busy: false,
            completed,
        }
    }
    fn reap(&mut self) {
        while self.completed.try_recv().is_ok() {
            self.busy = false;
        }
    }
    #[allow(clippy::result_large_err)] // Caller must retain the request to report a busy scan.
    fn start(&mut self, job: CanScanJob) -> Result<(), CanScanJob> {
        self.reap();
        if self.busy {
            return Err(job);
        }
        match self.jobs.try_send(job) {
            Ok(()) => {
                self.busy = true;
                Ok(())
            }
            Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => Err(job),
        }
    }
}
fn scan_response(nodes: Vec<Value>, errors: Vec<Value>) -> Value {
    // A scan is complete even when individual nodes do not reply.  `ok:false` would make the
    // browser discard successful discoveries before it can present the per-node errors.
    json!({"ok":true,"nodes":nodes,"errors":errors})
}

pub(crate) fn serve(
    port: u16,
    odrive_python: Option<PathBuf>,
    runs_dir: Option<PathBuf>,
) -> Result<(), String> {
    let server = Server::http(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    println!("WebUI 本地入口：http://127.0.0.1:{port}/");
    let mut workbench = Workbench::new(runs_dir.unwrap_or_else(Workbench::default_runs_dir));
    let mut odrive = OdriveView::new(odrive_python);
    let mut scan = CanScanView::new();
    let body_reader = BodyReader::new();
    loop {
        workbench.tick();
        odrive.reap();
        scan.reap();
        while let Ok(ReadyRequest { request, body }) = body_reader.try_recv() {
            if let Err(error) = handle_request_with_scan_body(
                request,
                port,
                &mut workbench,
                &mut odrive,
                &mut scan,
                Some(body),
            ) {
                eprintln!("warning: WebUI response failed: {error}");
            }
        }
        let Some(request) = server
            .recv_timeout(Duration::from_millis(10))
            .map_err(|error| error.to_string())?
        else {
            continue;
        };
        if let Err(error) = handle_received_request(
            request,
            port,
            &mut workbench,
            &mut odrive,
            &mut scan,
            &body_reader,
        ) {
            eprintln!("warning: WebUI response failed: {error}");
        }
    }
}

fn handle_received_request(
    mut request: tiny_http::Request,
    port: u16,
    workbench: &mut Workbench,
    odrive: &mut OdriveView,
    scan: &mut CanScanView,
    body_reader: &BodyReader,
) -> Result<(), String> {
    let is_json_write =
        request_path(&request).starts_with("/api/") && request_method(&request) == Method::Post;
    if !is_json_write {
        return handle_request_with_scan_body(request, port, workbench, odrive, scan, None);
    }
    if let Err(message) = validate_write_request(&request, port) {
        return respond_json(request, 403, json!({"ok":false,"message":message}));
    }
    if json_body_is_prebuffered(&request) {
        // tiny-http 0.12's `request::new_request` has already copied this exact shape into a
        // Cursor.  Keeping it on this fast path means a stalled large upload cannot queue an
        // explicit Stop behind the single bounded body reader.
        let body = parse_json_body(&mut request);
        return handle_request_with_scan_body(request, port, workbench, odrive, scan, Some(body));
    }
    match body_reader.submit(request) {
        Ok(()) => Ok(()),
        Err(request) => respond_json(
            request,
            503,
            json!({"ok":false,"message":"WebUI 请求队列已满；请稍后重试。"}),
        ),
    }
}

/// True only for the body form tiny-http 0.12 has synchronously copied while creating the
/// request.  Do not relax these checks: transfer encoding and upgrade bypass its small-body
/// buffer, while `Expect` changes the receive handshake.
fn json_body_is_prebuffered(request: &tiny_http::Request) -> bool {
    header(request, "Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length <= 1024)
        && header(request, "Transfer-Encoding").is_none()
        && header(request, "Expect").is_none()
        && !header(request, "Connection")
            .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"))
}

#[cfg(test)]
fn handle_request_with_scan(
    request: tiny_http::Request,
    port: u16,
    workbench: &mut Workbench,
    odrive: &mut OdriveView,
    scan: &mut CanScanView,
) -> Result<(), String> {
    handle_request_with_scan_body(request, port, workbench, odrive, scan, None)
}

fn handle_request_with_scan_body(
    mut request: tiny_http::Request,
    port: u16,
    workbench: &mut Workbench,
    odrive: &mut OdriveView,
    scan: &mut CanScanView,
    parsed_body: Option<Result<crate::RawFields, String>>,
) -> Result<(), String> {
    odrive.reap();
    scan.reap();
    let method = request_method(&request);
    let path = request_path(&request).to_owned();
    if path == "/api/snapshot" && method == Method::Get {
        let url = request.url().to_owned();
        let after = snapshot_after(&url);
        let transport = match query_transport(&url) {
            Ok(transport) => transport,
            Err(message) => {
                return respond_json(request, 400, json!({"ok":false,"message":message}));
            }
        };
        let node = match query_node(&url) {
            Ok(node) => node,
            Err(message) => {
                return respond_json(request, 400, json!({"ok":false,"message":message}));
            }
        };
        return session_response(
            request,
            200,
            workbench,
            transport,
            node,
            Some(after),
            odrive.last_read.as_ref(),
            json!({"ok":true}),
        );
    }
    if path == "/api/config/schema" && method == Method::Get {
        return serde_json::from_str(eha_sdk::config::SCHEMA_JSON)
            .map_err(|error| error.to_string())
            .and_then(|schema| respond_json(request, 200, schema));
    }
    if path == "/api/recordings" && method == Method::Get {
        if workbench.has_active_trial() {
            return respond_json(
                request,
                409,
                json!({"ok":false,"message":"试验进行中；记录目录查询已延后，避免阻塞后台 tick。"}),
            );
        }
        return match workbench.list_runs() {
            Ok(value) => respond_json(request, 200, value),
            Err(message) => respond_json(request, 503, json!({"ok":false,"message":message})),
        };
    }
    if let Some((id, file)) = recording_route(&path) {
        if method != Method::Get {
            return respond_json(
                request,
                405,
                json!({"ok":false,"message":"记录下载只接受 GET"}),
            );
        }
        if workbench.has_active_trial() {
            return respond_json(
                request,
                409,
                json!({"ok":false,"message":"试验进行中；记录下载已延后，避免阻塞后台 tick。"}),
            );
        }
        return match workbench
            .recording_file(id, file)
            .and_then(|path| fs::read(path).map_err(|error| error.to_string()))
        {
            Ok(bytes) => respond_bytes(request, 200, recording_content_type(file), bytes),
            Err(message) => respond_json(request, 404, json!({"ok":false,"message":message})),
        };
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
    if path.starts_with("/api/") {
        if method != Method::Post {
            return respond_json(
                request,
                405,
                json!({"ok":false,"message":"API 只接受 POST"}),
            );
        }
        let body = match parsed_body {
            Some(body) => body,
            None => {
                if let Err(message) = validate_write_request(&request, port) {
                    return respond_json(request, 403, json!({"ok":false,"message":message}));
                }
                parse_json_body(&mut request)
            }
        };
        let body = match body {
            Ok(body) => body,
            Err(message) => {
                return respond_json(request, 400, json!({"ok":false,"message":message}));
            }
        };
        if let Err(message) = validate_body_node(&body) {
            return respond_json(request, 400, json!({"ok":false,"message":message}));
        }
        if scan.busy
            && !matches!(
                path.as_str(),
                "/api/action" | "/api/recording" | "/api/trial" | "/api/group"
            )
        {
            return respond_json(
                request,
                409,
                json!({"ok":false,"message":"CAN 扫描进行中；该操作已拒绝。"}),
            );
        }
        return match path.as_str() {
            "/api/odrive/read" => {
                let Ok(serial) = body.string("serial") else {
                    return respond_json(
                        request,
                        400,
                        json!({"ok":false,"message":"请明确选择 ODrive USB 序列号。"}),
                    );
                };
                start_odrive_operation(request, odrive, OdriveOperation::Read { serial })
            }
            "/api/connect" => match ConnectionRequest::from_fields(&body) {
                Ok(connection) => {
                    let transport = transport_from_connection(&connection);
                    if workbench.has_active_trial() {
                        return respond_json(
                            request,
                            409,
                            json!({"ok":false,"message":"试验进行中；连接操作已拒绝，避免跨通路查询覆盖 Stop。"}),
                        );
                    }
                    let node = connection_node(&connection);
                    match workbench.connect(connection) {
                        Ok(_) => session_response(
                            request,
                            200,
                            workbench,
                            transport,
                            node,
                            None,
                            odrive.last_read.as_ref(),
                            json!({"ok":true,"message":"连接并已核对身份。"}),
                        ),
                        Err(error) => session_structured_error(
                            request,
                            workbench,
                            transport,
                            node,
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
                let node = body_node(&body);
                if workbench.has_active_trial() {
                    return respond_json(
                        request,
                        409,
                        json!({"ok":false,"message":"试验进行中；普通断连已拒绝。"}),
                    );
                }
                match workbench.disconnect(transport, node) {
                    Ok(_) => session_response(
                        request,
                        200,
                        workbench,
                        transport,
                        node,
                        None,
                        odrive.last_read.as_ref(),
                        json!({"ok":true,"message":"本地连接已关闭；没有发送 Stop。"}),
                    ),
                    Err(error) => session_structured_error(
                        request,
                        workbench,
                        transport,
                        node,
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
                let node = body_node(&body);
                if workbench.has_active_trial() {
                    return respond_json(
                        request,
                        409,
                        json!({"ok":false,"message":"试验进行中；恢复连接已拒绝。"}),
                    );
                }
                match workbench.reconnect(transport, node) {
                    Ok(_) => session_response(
                        request,
                        200,
                        workbench,
                        transport,
                        node,
                        None,
                        odrive.last_read.as_ref(),
                        json!({"ok":true,"message":"已重新连接并核对身份。"}),
                    ),
                    Err(error) => session_structured_error(
                        request,
                        workbench,
                        transport,
                        node,
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
                match Command::from_fields(&body) {
                    Ok(command) => {
                        let node = body_node(&body);
                        if (workbench.has_active_trial() || scan.busy)
                            && !matches!(command, Command::Stop)
                        {
                            return respond_json(
                                request,
                                409,
                                json!({"ok":false,"message":"试验进行中；只允许被动快照、记录、显式 Stop 或合并位置目标。"}),
                            );
                        }
                        if command_needs_identity_refresh(&command)
                            && let Err(error) = workbench.refresh_identity(transport, node)
                        {
                            return session_structured_error(
                                request,
                                workbench,
                                transport,
                                node,
                                odrive.last_read.as_ref(),
                                error,
                            );
                        }
                        match workbench.execute(transport, node, command) {
                            Ok(result) => session_response(
                                request,
                                200,
                                workbench,
                                transport,
                                node,
                                None,
                                odrive.last_read.as_ref(),
                                json!({"ok":true,"message":if result.local_submission.is_some() { "请求已本地提交；设备采用与执行请查看状态。" } else { "已取得操作结果；设备状态单独展示。" },"result":result}),
                            ),
                            Err(error) => session_structured_error(
                                request,
                                workbench,
                                transport,
                                node,
                                odrive.last_read.as_ref(),
                                error,
                            ),
                        }
                    }
                    Err(error) => session_response(
                        request,
                        400,
                        workbench,
                        transport,
                        body_node(&body),
                        None,
                        odrive.last_read.as_ref(),
                        json!({"ok":false,"message":format!("动作参数无效：{error}")}),
                    ),
                }
            }
            "/api/can/connect" => {
                if workbench.has_active_trial() {
                    return respond_json(
                        request,
                        409,
                        json!({"ok":false,"message":"试验进行中；CAN 连接已拒绝。"}),
                    );
                }
                let Ok(channel) = body.string("channel") else {
                    return respond_json(
                        request,
                        400,
                        json!({"ok":false,"message":"必须明确外部 CAN 通道"}),
                    );
                };
                let Ok(mode) = body.string("mode") else {
                    return respond_json(
                        request,
                        400,
                        json!({"ok":false,"message":"必须明确 CAN 帧格式"}),
                    );
                };
                let python = match body.optional_string("python") {
                    Ok(python) => python,
                    Err(_) => {
                        return respond_json(
                            request,
                            400,
                            json!({"ok":false,"message":"python 必须是字符串或 null"}),
                        );
                    }
                };
                let nodes = match body_nodes(&body) {
                    Ok(nodes) => nodes,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                match workbench.connect_can(channel, mode, python, &nodes, false) {
                    Ok(value) => respond_json(request, 200, value),
                    Err(message) => {
                        respond_json(request, 409, json!({"ok":false,"message":message}))
                    }
                }
            }
            "/api/can/scan" => {
                if workbench.has_active_trial() || scan.busy {
                    return respond_json(
                        request,
                        409,
                        json!({"ok":false,"message":"试验或另一轮 CAN 扫描正在进行；已拒绝扫描。"}),
                    );
                }
                if let Err(message) = workbench.can_scan_guard() {
                    return respond_json(request, 409, json!({"ok":false,"message":message}));
                }
                let Ok(channel) = body.string("channel") else {
                    return respond_json(
                        request,
                        400,
                        json!({"ok":false,"message":"必须明确外部 CAN 通道"}),
                    );
                };
                let Ok(mode) = body.string("mode") else {
                    return respond_json(
                        request,
                        400,
                        json!({"ok":false,"message":"必须明确 CAN 帧格式"}),
                    );
                };
                let python = match body.optional_string("python") {
                    Ok(python) => python,
                    Err(_) => {
                        return respond_json(
                            request,
                            400,
                            json!({"ok":false,"message":"python 必须是字符串或 null"}),
                        );
                    }
                };
                let nodes = match scan_nodes(&body) {
                    Ok(nodes) => nodes,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                if let Err(message) =
                    workbench.ensure_network(channel.clone(), mode.clone(), python.clone())
                {
                    return respond_json(request, 409, json!({"ok":false,"message":message}));
                }
                let mut connectors = Vec::with_capacity(nodes.len());
                for node in nodes {
                    match workbench.network_connector(node) {
                        Ok(connector) => connectors.push((node, connector)),
                        Err(message) => {
                            return respond_json(
                                request,
                                409,
                                json!({"ok":false,"message":message}),
                            );
                        }
                    }
                }
                let job = CanScanJob {
                    request,
                    channel,
                    mode,
                    python,
                    connectors,
                };
                match scan.start(job) {
                    Ok(()) => Ok(()),
                    Err(job) => respond_json(
                        job.request,
                        409,
                        json!({"ok":false,"message":"CAN 扫描工作线程不可用。"}),
                    ),
                }
            }
            "/api/recording" => {
                let transport = match body_transport(&body) {
                    Ok(value) => value,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                let action = body.string("action").ok();
                let Some(start) = (action.as_deref() == Some("start"))
                    .then_some(true)
                    .or_else(|| (action.as_deref() == Some("stop")).then_some(false))
                else {
                    return respond_json(
                        request,
                        400,
                        json!({"ok":false,"message":"recording action 必须为 start 或 stop"}),
                    );
                };
                if start && scan.busy {
                    return respond_json(
                        request,
                        409,
                        json!({"ok":false,"message":"CAN 扫描进行中；不能启动记录。"}),
                    );
                }
                match workbench.recording(transport, body_node(&body), start) {
                    Ok(recording) => session_response(
                        request,
                        200,
                        workbench,
                        transport,
                        body_node(&body),
                        None,
                        odrive.last_read.as_ref(),
                        json!({"ok":true,"recording":recording}),
                    ),
                    Err(error) => session_structured_error(
                        request,
                        workbench,
                        transport,
                        body_node(&body),
                        odrive.last_read.as_ref(),
                        error,
                    ),
                }
            }
            "/api/trial" => {
                let transport = match body_transport(&body) {
                    Ok(value) => value,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                let action = match trial_action(&body) {
                    Ok(action) => action,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                if scan.busy && matches!(action, TrialAction::Start(_)) {
                    return respond_json(
                        request,
                        409,
                        json!({"ok":false,"message":"CAN 扫描进行中；不能启动试验。"}),
                    );
                }
                match workbench.trial(transport, body_node(&body), action) {
                    Ok(trial) => session_response(
                        request,
                        200,
                        workbench,
                        transport,
                        body_node(&body),
                        None,
                        odrive.last_read.as_ref(),
                        json!({"ok":true,"trial":trial}),
                    ),
                    Err(error) => session_structured_error(
                        request,
                        workbench,
                        transport,
                        body_node(&body),
                        odrive.last_read.as_ref(),
                        error,
                    ),
                }
            }
            "/api/group" => {
                let action = body.string("action").ok();
                let nodes = if action.as_deref() == Some("stop") && !body.contains("nodes") {
                    Ok(Vec::new())
                } else {
                    body_nodes(&body)
                };
                let nodes = match nodes {
                    Ok(nodes) => nodes,
                    Err(message) => {
                        return respond_json(request, 400, json!({"ok":false,"message":message}));
                    }
                };
                let result = match action.as_deref() {
                    Some("trial_start") if !workbench.has_active_trial() && !scan.busy => body
                        .decode_field("trial")
                        .map_err(|error| format!("trial 参数无效：{error}"))
                        .and_then(|trial| workbench.group_trial_start(&nodes, trial)),
                    Some("stop") => workbench.group_stop(&nodes),
                    Some("heartbeat_start") if !workbench.has_active_trial() && !scan.busy => {
                        workbench.group_heartbeat(&nodes, true)
                    }
                    Some("heartbeat_stop") if !workbench.has_active_trial() => {
                        workbench.group_heartbeat(&nodes, false)
                    }
                    Some(_) => Err("当前试验运行中；该群组操作已拒绝".into()),
                    None => Err("必须明确 group action".into()),
                };
                match result {
                    Ok(value) => respond_json(request, 200, value),
                    Err(message) => {
                        respond_json(request, 409, json!({"ok":false,"message":message}))
                    }
                }
            }
            _ => respond_json(request, 404, json!({"ok":false,"message":"未知 API 路径"})),
        };
    }
    respond_static(request, static_response(method, &path))
}

#[cfg(test)]
fn handle_request(
    request: tiny_http::Request,
    port: u16,
    workbench: &mut Workbench,
    odrive: &mut OdriveView,
) -> Result<(), String> {
    let mut scan = CanScanView::new();
    handle_request_with_scan(request, port, workbench, odrive, &mut scan)
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
fn query_node(url: &str) -> Result<Option<u8>, &'static str> {
    query_field(url, "node")
        .map(|node| {
            node.parse::<u8>()
                .ok()
                .filter(|node| *node <= 127)
                .ok_or("node 必须为 0..=127 的整数")
        })
        .transpose()
}
fn body_node(body: &crate::RawFields) -> Option<u8> {
    body.decode_field("node").ok()
}
fn validate_body_node(body: &crate::RawFields) -> Result<(), &'static str> {
    if !body.contains("node") {
        return Ok(());
    }
    body.decode_field::<u8>("node")
        .ok()
        .filter(|node| *node <= 127)
        .map(|_| ())
        .ok_or("node 必须为 0..=127 的整数")
}
fn body_nodes(body: &crate::RawFields) -> Result<Vec<u8>, &'static str> {
    body.decode_field::<Vec<u8>>("nodes")
        .map_err(|_| "必须提供 nodes 数组")?
        .into_iter()
        .map(|node| {
            (node <= 127)
                .then_some(node)
                .ok_or("node 必须为 0..=127 的整数")
        })
        .collect()
}
fn scan_nodes(body: &crate::RawFields) -> Result<Vec<u8>, &'static str> {
    let start = body
        .decode_field("start_node")
        .map_err(|_| "start_node 必须为 0..=127 的整数")?;
    let end = body
        .decode_field("end_node")
        .map_err(|_| "end_node 必须为 0..=127 的整数")?;
    if start > 127 || end > 127 || start > end {
        return Err("扫描范围必须为 0..=127 且 start_node 不大于 end_node");
    }
    Ok((start..=end).collect())
}
fn transport_from_connection(connection: &ConnectionRequest) -> WebTransport {
    match connection {
        ConnectionRequest::Usb { .. } => WebTransport::Usb,
        ConnectionRequest::Can { .. } => WebTransport::Can,
    }
}
fn connection_node(connection: &ConnectionRequest) -> Option<u8> {
    match connection {
        ConnectionRequest::Usb { .. } => None,
        ConnectionRequest::Can { node, .. } => Some(*node),
    }
}
fn trial_action(body: &crate::RawFields) -> Result<TrialAction, String> {
    match body.optional_string("action")?.as_deref() {
        Some("start") => body
            .decode_field("trial")
            .map(TrialAction::Start)
            .map_err(|error| format!("trial 参数无效：{error}")),
        Some("stop") => Ok(TrialAction::Stop),
        Some("target") => body.decode_f32_field("mm").map(TrialAction::Target),
        _ => Err("trial action 必须为 start、stop 或 target".into()),
    }
}
fn recording_route(path: &str) -> Option<(&str, &str)> {
    let tail = path.strip_prefix("/api/recordings/")?;
    let (id, file) = tail.split_once('/')?;
    (!id.is_empty() && !file.contains('/')).then_some((id, file))
}
fn recording_content_type(file: &str) -> &'static str {
    match file {
        "metadata.json" | "events.jsonl" => "application/json; charset=utf-8",
        "telemetry.csv" => "text/csv; charset=utf-8",
        _ => "application/octet-stream",
    }
}
fn body_transport(body: &crate::RawFields) -> Result<WebTransport, &'static str> {
    body.string("transport")
        .map_err(|_| "请求必须明确指定 transport=usb 或 transport=can")
        .and_then(|value| parse_transport_value(&value))
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
#[allow(clippy::too_many_arguments)] // transport/node/trend/ODrive are one snapshot projection.
fn session_response(
    request: tiny_http::Request,
    status: u16,
    workbench: &mut Workbench,
    transport: WebTransport,
    node: Option<u8>,
    trend_after: Option<Option<u64>>,
    odrive: Option<&Value>,
    mut value: Value,
) -> Result<(), String> {
    let state = workbench.snapshot_json(transport, node, trend_after, odrive)?;
    let object = value
        .as_object_mut()
        .ok_or("WebUI session response must be a JSON object")?;
    object.insert("snapshot".into(), state["snapshot"].clone());
    object.insert("sessions".into(), state["sessions"].clone());
    object.insert("transport".into(), state["transport"].clone());
    object.insert("can_nodes".into(), state["can_nodes"].clone());
    object.insert("group_nodes".into(), state["group_nodes"].clone());
    respond_json(request, status, value)
}
fn session_structured_error(
    request: tiny_http::Request,
    workbench: &mut Workbench,
    transport: WebTransport,
    node: Option<u8>,
    odrive: Option<&Value>,
    error: crate::session::SessionError,
) -> Result<(), String> {
    let message = error.message.clone();
    let unknown = error.unknown;
    let operation_key = error.operation_key.clone();
    session_response(
        request,
        409,
        workbench,
        transport,
        node,
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
pub(super) fn parse_json_body(
    request: &mut tiny_http::Request,
) -> Result<crate::RawFields, String> {
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_JSON_BODY + 1)
        .read_to_string(&mut body)
        .map_err(|error| error.to_string())?;
    if body.len() as u64 > MAX_JSON_BODY {
        return Err("JSON 请求过大".into());
    }
    crate::RawFields::parse(&body).map_err(|error| format!("JSON 无效：{error}"))
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
    respond_async(request, http_response)
}
pub(super) fn respond_json(
    request: tiny_http::Request,
    status: u16,
    value: Value,
) -> Result<(), String> {
    let body = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    let length = body.len();
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    common_headers(
        &mut response,
        "application/json; charset=utf-8",
        length,
        (status == 405).then_some("GET, POST"),
    )?;
    respond_async(request, response)
}
fn respond_bytes(
    request: tiny_http::Request,
    status: u16,
    content_type: &str,
    body: Vec<u8>,
) -> Result<(), String> {
    let length = body.len();
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    common_headers(&mut response, content_type, length, None)?;
    respond_async(request, response)
}
fn respond_async(
    request: tiny_http::Request,
    response: Response<std::io::Cursor<Vec<u8>>>,
) -> Result<(), String> {
    thread::Builder::new()
        .name("eha-tool-webui-response".into())
        .spawn(move || {
            let _ = request.respond(response);
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
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
    #[test]
    fn numeric_requests_decode_original_decimal_once() {
        let decimal = "1.0000000596046447753906250000000000000000000001";
        let action = crate::RawFields::parse(&format!(
            r#"{{"action":"position","mm":{decimal},"future":1e-999}}"#
        ))
        .expect("原始动作");
        let mm = match crate::session::Command::from_fields(&action).expect("动作 f32") {
            crate::session::Command::Position { mm } => mm,
            _ => f32::NAN,
        };
        assert_eq!(mm.to_bits(), 0x3f800001);
        let direct: crate::session::Command =
            serde_json::from_str(&format!(r#"{{"action":"position","mm":{decimal}}}"#))
                .expect("嵌套 Command 入口");
        assert!(
            matches!(direct, crate::session::Command::Position { mm } if mm.to_bits() == 0x3f800001)
        );
        let target = crate::RawFields::parse(&format!(r#"{{"action":"target","mm":{decimal}}}"#))
            .expect("目标");
        let mm = match super::trial_action(&target).expect("目标 f32") {
            super::TrialAction::Target(mm) => mm,
            _ => f32::NAN,
        };
        assert_eq!(mm.to_bits(), 0x3f800001);
        for action_name in ["start", "trial_start"] {
            let body = crate::RawFields::parse(&format!(r#"{{"action":"{action_name}","trial":{{"command":{{"action":"position","mm":{decimal},"future":1e-999}},"envelope":{{"position_min_mm":-10,"position_max_mm":10,"velocity_abs_max_mm_s":1,"force_abs_max_n":2,"stiffness_max_n_per_mm":3,"damping_max_ns_per_mm":4,"duration_max_s":2}},"duration_s":1,"reach":null}}}}"#)).expect("试验原文");
            let trial: crate::session::TrialRequest =
                body.decode_field("trial").expect("单台／群组试验 f32");
            let mm = match trial.command {
                crate::session::Command::Position { mm } => mm,
                _ => f32::NAN,
            };
            assert_eq!(mm.to_bits(), 0x3f800001);
        }
    }

    #[test]
    fn routing_ignores_unknown_numbers_without_adopting_them() {
        for token in ["1e-999", "1e100", "1e999", "18446744073709551615"] {
            let stop = crate::RawFields::parse(&format!(
                r#"{{"transport":"usb","action":"stop","future":{token}}}"#
            ))
            .expect("原始 Stop");
            assert!(matches!(
                crate::session::Command::from_fields(&stop),
                Ok(crate::session::Command::Stop)
            ));
        }
        let connection =
            crate::RawFields::parse(r#"{"transport":"usb","serial":"selected","future":1e-999}"#)
                .expect("原始连接");
        assert_eq!(
            crate::session::ConnectionRequest::from_fields(&connection)
                .expect("未知数值字段不被采用"),
            crate::session::ConnectionRequest::Usb {
                serial: "selected".into()
            }
        );
    }

    #[test]
    fn routing_integer_fields_do_not_coerce_decimal_or_signed_zero() {
        for token in ["0", "127"] {
            let body =
                crate::RawFields::parse(&format!(r#"{{"node":{token}}}"#)).expect("整数节点 JSON");
            assert!(super::validate_body_node(&body).is_ok(), "{token}");
        }
        for token in ["128", "0.0", "0e0", "-0", "18446744073709551615", "\"0\""] {
            let body =
                crate::RawFields::parse(&format!(r#"{{"node":{token}}}"#)).expect("JSON 语法有效");
            assert!(super::validate_body_node(&body).is_err(), "{token}");
        }
    }

    #[test]
    fn optional_python_accepts_missing_or_null_but_rejects_other_types() {
        for raw in [
            r#"{"transport":"can","channel":"vcan0","node":1,"mode":"fd"}"#,
            r#"{"transport":"can","channel":"vcan0","node":1,"mode":"fd","python":null}"#,
            r#"{"transport":"can","channel":"vcan0","node":1,"mode":"fd","python":"python3"}"#,
        ] {
            let fields = crate::RawFields::parse(raw).expect("JSON");
            assert!(
                crate::session::ConnectionRequest::from_fields(&fields).is_ok(),
                "{raw}"
            );
        }
        let fields = crate::RawFields::parse(
            r#"{"transport":"can","channel":"vcan0","node":1,"mode":"fd","python":1}"#,
        )
        .expect("JSON");
        assert!(crate::session::ConnectionRequest::from_fields(&fields).is_err());
    }

    #[test]
    fn numeric_requests_reject_underflow_and_overflow_before_submission() {
        for raw in [
            r#"{"action":"position","mm":1,"mm":2}"#,
            r#"{"action":"target","mm":1,"mm":2}"#,
            r#"{"action":"trial_start","trial":{},"trial":{}}"#,
            r#"{"action":"stop","action":"target"}"#,
        ] {
            assert!(crate::RawFields::parse(raw).is_err());
        }
        for raw in [
            r#"{"action":"position","mm":null}"#,
            r#"{"action":"position"}"#,
            r#"{"action":"position","mm":{"$serde_json::private::Number":"1"}}"#,
            r#"{"action":"stop","future":1,"future":2}"#,
        ] {
            assert!(
                serde_json::from_str::<crate::session::Command>(raw).is_err(),
                "Command 必须拒绝：{raw}"
            );
        }
        for token in [
            "1e-999",
            "-1e-999",
            "1e100",
            "1000000000000000000000000000000000000000000000000000",
        ] {
            assert!(
                serde_json::from_str::<crate::session::Command>(&format!(
                    r#"{{"action":"position","mm":{token}}}"#
                ))
                .is_err(),
                "Command 不能采用 {token}"
            );
            let body = crate::RawFields::parse(&format!(r#"{{"action":"target","mm":{token}}}"#));
            assert!(
                body.map_err(|error| error.to_string())
                    .and_then(|body| super::trial_action(&body))
                    .is_err()
            );
        }
        let trial = r#"{"command":{"action":"position","mm":1},"envelope":{"position_min_mm":-10,"position_max_mm":10,"velocity_abs_max_mm_s":1,"force_abs_max_n":2,"stiffness_max_n_per_mm":3,"damping_max_ns_per_mm":4,"duration_max_s":2},"duration_s":1,"reach":{"tolerance_mm":1,"settle_ms":1}}"#;
        for field in ["mm", "velocity_abs_max_mm_s", "duration_s", "tolerance_mm"] {
            let bad = trial.replace(&format!("\"{field}\":1"), &format!("\"{field}\":1e-999"));
            let body =
                crate::RawFields::parse(&format!("{{\"trial\":{bad}}}")).expect("合法试验 JSON");
            assert!(
                body.decode_field::<crate::session::TrialRequest>("trial")
                    .is_err(),
                "{field} 下溢不能被采用"
            );
        }
        let body = crate::RawFields::parse(r#"{"action":"target","mm":1.401298464324817e-45}"#)
            .expect("次正规数");
        let mm = match super::trial_action(&body).expect("保留次正规数") {
            super::TrialAction::Target(mm) => mm,
            _ => f32::NAN,
        };
        assert_eq!(mm.to_bits(), 1);
    }

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
        for module in [
            "/config-editor.js",
            "/trial-ui.js",
            "/trial-state.js",
            "/recording-ui.js",
        ] {
            let response = send_request(|_| {
                format!("GET {module} HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n")
            })?;
            assert!(response.starts_with("HTTP/1.1 200"), "missing {module}");
        }
        let bad_post = send_request(|_| {
            "POST /api/action HTTP/1.1\r\nHost: evil.test\r\nOrigin: http://evil.test\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"action\":\"stop\"}".into()
        })?;
        assert!(bad_post.starts_with("HTTP/1.1 403"));
        Ok(())
    }

    #[test]
    fn scan_keeps_found_nodes_when_other_nodes_do_not_reply() {
        let response = super::scan_response(
            vec![json!({"node":1,"identity":{"uid":"known"}})],
            vec![json!({"node":2,"message":"timeout"})],
        );
        assert_eq!(response["ok"], true);
        assert_eq!(response["nodes"][0]["node"], 1);
        assert_eq!(response["errors"][0]["node"], 2);
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
    fn explicit_invalid_can_node_is_rejected_before_default_selection()
    -> Result<(), Box<dyn Error + Send + Sync>> {
        for node in ["-1", "1.5", "256", "\"x\""] {
            let response = session_request(
                "POST",
                "/api/action",
                Some(&format!(
                    r#"{{"transport":"can","node":{node},"action":"stop"}}"#
                )),
            )?;
            assert!(
                response.starts_with("HTTP/1.1 400"),
                "node={node}: {response}"
            );
            assert!(response.contains("node 必须为 0..=127 的整数"));
        }
        let snapshot = session_request("GET", "/api/snapshot?transport=can&node=256", None)?;
        assert!(snapshot.starts_with("HTTP/1.1 400"));
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

        let invalid = request_client(
            address,
            request_text(port, "POST", "/api/odrive/read", Some(r#"{"serial":1}"#)),
        );
        handle_request(server.recv()?, port, &mut sessions, &mut odrive)?;
        let invalid = invalid.join().map_err(|_| "invalid client panicked")??;
        assert!(invalid.starts_with("HTTP/1.1 400"));
        assert!(!odrive.busy, "错误类型不得启动 ODrive 工作线程");

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
