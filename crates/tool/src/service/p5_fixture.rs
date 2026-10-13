// Copyright The eha-sdk Contributors
//! 显式 ignored 的 UI_SYNTHETIC 长驻服务；只替换测试会话，不复制正式 owner。

use super::{Method, integration, mutations, workbench::Workbench};
use crate::session::{
    ToolSession,
    trial_tests::{self, UiOptions},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Scenario {
    Healthy,
    SlowRead,
    QueuedTarget,
    TakenTarget,
    TargetUnknown,
    StopUnknown,
    RecordingAuditFailure,
    NotIdle,
}
impl Scenario {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "healthy" => Ok(Self::Healthy),
            "slow_read" => Ok(Self::SlowRead),
            "queued_target" => Ok(Self::QueuedTarget),
            "taken_target" => Ok(Self::TakenTarget),
            "target_unknown" => Ok(Self::TargetUnknown),
            "stop_unknown" => Ok(Self::StopUnknown),
            "recording_audit_failure" => Ok(Self::RecordingAuditFailure),
            "not_idle" => Ok(Self::NotIdle),
            _ => Err(
                "仅允许 healthy/slow_read/queued_target/taken_target/target_unknown/stop_unknown/recording_audit_failure/not_idle"
                    .into(),
            ),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Trace {
    start: Instant,
    deadline: Instant,
    sender: SyncSender<Value>,
    dropped: Arc<AtomicU64>,
}
impl Trace {
    pub(crate) fn new(ttl: Duration) -> (Self, Receiver<Value>) {
        let (sender, receiver) = mpsc::sync_channel(256);
        let start = Instant::now();
        (
            Self {
                start,
                deadline: start + ttl,
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            receiver,
        )
    }
    pub(crate) fn event(&self, kind: &str, node: u8, local_id: Option<u64>, facts: Value) {
        let entry = json!({"source":"UI_SYNTHETIC", "mono_us":self.start.elapsed().as_micros().to_string(),
            "kind":kind,"node":node,"transport_local_id":local_id.map(|id|id.to_string()),"facts":facts});
        if self.sender.try_send(entry).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(super) struct Context {
    pub(super) trace: Trace,
    last: BTreeMap<String, Value>,
    audit_failure_pending: bool,
}
impl Context {
    fn new(trace: Trace, scenario: Scenario) -> Self {
        Self {
            trace,
            last: BTreeMap::new(),
            audit_failure_pending: scenario == Scenario::RecordingAuditFailure,
        }
    }
    pub(super) fn expired(&self) -> bool {
        Instant::now() >= self.trace.deadline
    }
    pub(super) fn capture(&mut self, workbench: &mut Workbench) {
        if self.audit_failure_pending
            && workbench
                .integration_session_mut("can:1")
                .is_some_and(ToolSession::arm_ui_trial_audit_failure)
        {
            self.audit_failure_pending = false;
            self.trace.event(
                "recording_fault_armed",
                1,
                None,
                json!({"trigger":"trial_command_input", "failure":"readonly_event_file_flush"}),
            );
        }

        let mut current = BTreeMap::new();
        if let Some(common) = workbench.mutations.as_ref() {
            for id in common.receipts.keys() {
                if let Ok(op) = common.projection(id) {
                    let native = op["native_json_text"]
                        .as_str()
                        .and_then(|text| serde_json::from_str::<Value>(text).ok());
                    let local_id = native
                        .as_ref()
                        .and_then(|value| value.pointer("/local_submission/id"))
                        .and_then(Value::as_u64)
                        .map(|id| id.to_string());
                    let fact = json!({"instance_id":common.instance_id,"request_id":id,
                        "members":op["members"],"action":op["action"],"admission":op["admission"],
                        "submission":op["submission"],"control_effect":op["control_effect"],"transport_local_id":local_id,
                        "phase":op["phase"],"trial_id":op["trial_id"],"stop_attempt_delay_ms":op["stop_attempt_delay_ms"]});
                    current.insert(id.clone(), fact);
                }
            }
        }
        for node in [1, 2] {
            if let Some(session) = workbench.integration_session_mut(&format!("can:{node}")) {
                let trial = session.trial_snapshot();
                let fact = json!({"target_id":format!("can:{node}"),"state":trial["state"],
                    "stop_reason":trial["stop_reason"],"stop_observed":trial["stop_observed"],
                    "stop_confirmation_overdue":trial["stop_confirmation_overdue"]});
                current.insert(format!("trial:{node}"), fact);
            }
        }
        for (id, fact) in &current {
            if self.last.get(id) != Some(fact) {
                self.trace.event("owner_transition", 0, None, fact.clone());
            }
        }
        self.last = current;
    }
}

// 只允许实际会话的观察、明确授权/范围/固定控制、记录与生命周期路径。
// 不允许 v1、维护、物理连接、档案连接、适配器/ODrive 发现或扫描绕行。
pub(super) fn allowed_path(path: &str, method: Method) -> bool {
    let Some(rest) = path.strip_prefix("/service/v2/") else {
        return false;
    };
    if method == Method::Get {
        return matches!(
            rest,
            "describe" | "instance" | "targets" | "connections" | "events" | "control-scopes"
        ) || rest.starts_with("bindings/") && !rest.ends_with("/maintenance")
            || rest.starts_with("operations/")
            || rest.starts_with("recordings")
            || rest.starts_with("groups/")
            || rest.starts_with("jobs/");
    }
    matches!(
        rest,
        "bindings" | "shutdown" | "control-reservations" | "operation-ids" | "groups"
    ) || rest.starts_with("bindings/") && !rest.ends_with("/maintenance")
        || rest.starts_with("control-reservations/")
        || rest.starts_with("control-scopes/")
        || rest.starts_with("groups/")
}

fn workbench(
    runs: PathBuf,
    credentials: integration::IntegrationState,
    scenario: Scenario,
    trace: Trace,
) -> Workbench {
    let mut nodes = BTreeMap::new();
    for node in [1, 2] {
        let options = UiOptions {
            scenario: if node == 1 {
                scenario
            } else {
                Scenario::Healthy
            },
            trace: trace.clone(),
        };
        let (session, _) = trial_tests::ui_session_fixture(node, [0xa0 + node; 12], options);
        nodes.insert(node, session);
    }
    let mut workbench = Workbench::with_test_sessions_in(ToolSession::new(), nodes, runs, None);
    workbench.configure_p5_fixture(Context::new(trace.clone(), scenario));
    workbench.mutations =
        Some(mutations::State::new(&credentials.instance_id).expect("fixed limits"));
    workbench.v2 = Some(credentials.for_v2());
    trace.event("instance", 0, None, json!({"instance_id":credentials.instance_id,"endpoint":credentials.endpoint,
        "pid":std::process::id().to_string(),"scenario":format!("{scenario:?}"),"expiry_ms":trace.deadline.saturating_duration_since(Instant::now()).as_millis().to_string()}));
    workbench.integration = Some(credentials);
    workbench
}

#[cfg(unix)]
fn serve(directory: PathBuf, port: u16, scenario: Scenario, ttl: Duration) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    crate::owner_lock::private_directory(&directory).map_err(|error| error.to_string())?;
    let runs = directory.join("runs");
    crate::owner_lock::private_directory(&runs).map_err(|error| error.to_string())?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(directory.join("events.jsonl"))
        .map_err(|error| error.to_string())?;
    let (trace, receiver) = Trace::new(ttl);
    let writer_trace = trace.clone();
    let (done, ending) = mpsc::channel();
    let writer = thread::spawn(move || -> Result<(), String> {
        let mut entries = 0;
        loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(entry) if entries < 32768 => {
                    writeln!(file, "{entry}").map_err(|error| error.to_string())?;
                    entries += 1;
                }
                Ok(_) => {
                    writer_trace.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) if ending.try_recv().is_ok() => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        writeln!(
            file,
            "{}",
            json!({"source":"UI_SYNTHETIC","kind":"trace_end","entries":entries,
            "dropped":writer_trace.dropped.load(Ordering::Relaxed)})
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    });
    let result = super::run_with_workbench(
        port,
        None,
        Some(runs.clone()),
        Some(directory.join(format!("port-{port}.token"))),
        Some(directory.join(format!("port-{port}.json"))),
        false,
        move |_, credentials, _| {
            workbench(runs, credentials.expect("v2 credentials"), scenario, trace)
        },
    );
    let _ = done.send(());
    writer
        .join()
        .map_err(|_| "fixture trace writer panicked".to_owned())??;
    result
}

#[test]
#[ignore = "仅人工启动 UI_SYNTHETIC 本机服务，最多运行十五分钟；不是自动 UI 验收"]
#[cfg(unix)]
fn serve_for_official_ui() {
    let directory = PathBuf::from(
        std::env::var("EHA_P5_FIXTURE_DIR").expect("EHA_P5_FIXTURE_DIR absolute private directory"),
    );
    assert!(
        directory.is_absolute(),
        "fixture directory must be absolute"
    );
    let port: u16 = std::env::var("EHA_P5_FIXTURE_PORT")
        .expect("fixture port")
        .parse()
        .expect("port integer");
    assert!(port != 0, "fixed UI port required");
    let scenario =
        Scenario::parse(&std::env::var("EHA_P5_SCENARIO").unwrap_or_else(|_| "healthy".into()))
            .expect("fixed scenario");
    serve(directory, port, scenario, Duration::from_secs(900)).expect("UI_SYNTHETIC service");
}

#[cfg(unix)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Read,
        net::{TcpListener, TcpStream},
    };

    struct Running {
        root: PathBuf,
        port: u16,
        token: String,
        instance: String,
        worker: Option<thread::JoinHandle<Result<(), String>>>,
    }
    impl Running {
        // 测试启动失败保留线程的原始错误诊断。
        #[allow(clippy::panic)]
        fn start(scenario: Scenario, ttl: Duration) -> Self {
            let root = fs::canonicalize(std::env::temp_dir())
                .expect("canonical system temporary directory")
                .join(format!(
                    "eha-p5-{}-{}",
                    std::process::id(),
                    integration::random_hex::<8>().expect("random")
                ));
            let listener = TcpListener::bind("127.0.0.1:0").expect("unused port");
            let port = listener.local_addr().expect("port").port();
            drop(listener);
            let serving_root = root.clone();
            let worker = thread::spawn(move || serve(serving_root, port, scenario, ttl));
            let deadline = Instant::now() + Duration::from_secs(3);
            let descriptor = loop {
                if let Ok(bytes) = fs::read(root.join(format!("port-{port}.json"))) {
                    break serde_json::from_slice::<Value>(&bytes).expect("descriptor");
                }
                if worker.is_finished() {
                    panic!("fixture startup failed: {:?}", worker.join());
                }
                assert!(Instant::now() < deadline, "ready deadline");
                thread::sleep(Duration::from_millis(5));
            };
            let token = fs::read_to_string(root.join(format!("port-{port}.token"))).expect("token");
            Self {
                root,
                port,
                token,
                instance: descriptor["instance_id"].as_str().expect("instance").into(),
                worker: Some(worker),
            }
        }
        fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
            http(self.port, &self.token, method, path, body)
        }
        fn attach(&self, node: u8) -> (String, Value) {
            let (status, value) = self.request("GET", "/service/v2/targets", None);
            assert_eq!(status, 200);
            let target = value["targets"]
                .as_array()
                .expect("targets")
                .iter()
                .find(|t| t["target_id"] == format!("can:{node}"))
                .expect("fixed node");
            let fields = json!({"api_version":2,"instance_id":self.instance,"expected_identity":target["identity"],"connection_generation":target["connection_generation"]});
            let mut attach = fields.clone();
            attach["target_id"] = json!(format!("can:{node}"));
            attach["intent"] = json!("attach");
            attach["lease_ms"] = json!(120000);
            let (status, value) = self.request("POST", "/service/v2/bindings", Some(attach));
            assert_eq!(status, 200, "{value}");
            (
                value["binding_id"].as_str().expect("binding").into(),
                fields,
            )
        }
        fn events(&self) -> Vec<Value> {
            fs::read_to_string(self.root.join("events.jsonl"))
                .expect("events")
                .split_inclusive('\n')
                .filter(|line| line.ends_with('\n'))
                .map(|line| serde_json::from_str(line).expect("event"))
                .collect()
        }
    }
    impl Drop for Running {
        fn drop(&mut self) {
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
            {
                let _=self.request("POST","/service/v2/shutdown",Some(json!({"api_version":2,"instance_id":self.instance,"confirmation":"shutdown_service_without_stopping_devices"})));
            }
            if let Some(worker) = self.worker.take() {
                worker
                    .join()
                    .expect("service thread")
                    .expect("service result");
            }
            fs::remove_dir_all(&self.root).expect("private test cleanup");
        }
    }
    fn http(port: u16, token: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("loopback");
        socket
            .set_read_timeout(Some(Duration::from_secs(6)))
            .expect("read deadline");
        let body = body.map(|v| v.to_string()).unwrap_or_default();
        write!(socket,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).expect("request");
        let mut text = String::new();
        socket.read_to_string(&mut text).expect("response");
        let (head, body) = text.split_once("\r\n\r\n").expect("HTTP response");
        let status = head
            .split_whitespace()
            .nth(1)
            .expect("status")
            .parse()
            .expect("status integer");
        (status, serde_json::from_str(body).expect("JSON response"))
    }

    #[test]
    fn recordings_job_is_readable_without_opening_physical_job_creation() {
        assert!(allowed_path("/service/v2/jobs/read-1", Method::Get));
        assert!(!allowed_path("/service/v2/jobs/read-1", Method::Post));
        assert!(!allowed_path("/service/v2/read-jobs/read-1", Method::Get));
        for (path, method) in [
            ("/service/v2/profiles", Method::Get),
            ("/service/v2/adapters", Method::Get),
            ("/service/v2/odrive/devices", Method::Get),
            ("/service/v2/odrive/read", Method::Post),
        ] {
            assert!(!allowed_path(path, method), "physical creation {path}");
        }
        let service = Running::start(Scenario::Healthy, Duration::from_secs(10));
        let (status, queued) = service.request("GET", "/service/v2/recordings", None);
        assert_eq!(status, 202, "{queued}");
        assert_eq!(queued["state"], "queued");
        let id = queued["job_id"].as_str().expect("actual recordings job ID");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let (status, value) = service.request("GET", &format!("/service/v2/jobs/{id}"), None);
            assert_eq!(status, 200, "{value}");
            assert_eq!(value["job_id"], id);
            if value["state"] == "completed" {
                assert_eq!(value["data"]["runs"], json!([]));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "recordings job did not complete: {value}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn not_idle_scene_rejects_read_without_target_trial_or_heartbeat() {
        let service = Running::start(Scenario::NotIdle, Duration::from_secs(10));
        let (binding, mut fields) = service.attach(1);
        let deadline = Instant::now() + Duration::from_secs(1);
        let snapshot = loop {
            let (status, value) = service.request(
                "GET",
                &format!("/service/v2/bindings/{binding}/snapshot"),
                None,
            );
            assert_eq!(status, 200, "{value}");
            if value["read_permit"]["reason_code"] == "not_idle" {
                break value;
            }
            assert!(
                Instant::now() < deadline,
                "expected independent not_idle: {value}"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(snapshot["read_permit"]["ready"], false);
        assert_eq!(snapshot["control_effect"]["state"], "not_observed");
        assert!(
            snapshot["telemetry_received_age_ms"]
                .as_str()
                .expect("age")
                .parse::<u64>()
                .expect("age integer")
                < 3000
        );
        let native: Value =
            serde_json::from_str(snapshot["native_json_text"].as_str().expect("snapshot"))
                .expect("native JSON");
        assert_eq!(native["heartbeat"]["enabled"], false);
        assert_eq!(native["telemetry"]["target_mode"], 0);
        assert_eq!(native["telemetry"]["axis_state_raw"], 8);
        assert_eq!(native["telemetry"]["facts"]["unknown_effect"], false);
        assert_eq!(
            native["telemetry"]["facts"]["device_operation_pending"],
            false
        );
        let before = service
            .events()
            .iter()
            .filter(|entry| entry["kind"] == "pump_taken")
            .count();
        fields["action"] = json!("status");
        let (status, error) = service.request(
            "POST",
            &format!("/service/v2/bindings/{binding}/reads"),
            Some(fields),
        );
        assert_eq!(status, 409, "{error}");
        assert!(
            error["message"]
                .as_str()
                .expect("gate reason")
                .contains("not_idle")
        );
        let events = service.events();
        assert_eq!(
            events
                .iter()
                .filter(|entry| entry["kind"] == "pump_taken")
                .count(),
            before,
            "gate must precede query submission"
        );
        assert!(
            !events
                .iter()
                .any(|entry| entry["kind"] == "pump_taken" && entry["facts"]["action"] != "read")
        );
        assert!(
            events
                .iter()
                .filter(|entry| entry["kind"] == "owner_transition"
                    && entry["facts"]["target_id"] == "can:1")
                .all(|entry| entry["facts"]["state"] == "idle")
        );
    }

    #[test]
    fn recording_audit_failure_blocks_first_target_with_real_writer_error() {
        let service = Running::start(Scenario::RecordingAuditFailure, Duration::from_secs(10));
        let (binding, base) = service.attach(1);
        let mut permission = base.clone();
        permission["permissions"] = json!({"maintenance":false,"control":true});
        permission["expected_grant_id"] = Value::Null;
        assert_eq!(
            service
                .request(
                    "POST",
                    &format!("/service/v2/bindings/{binding}/permissions"),
                    Some(permission)
                )
                .0,
            200
        );
        let (status, held) = service.request("POST", "/service/v2/control-reservations", Some(json!({"api_version":2,"instance_id":service.instance,"claimant_id":"UI_SYNTHETIC_operator","permission":"control","members":[{"binding_id":binding,"expected_identity":base["expected_identity"],"connection_generation":base["connection_generation"]}]})));
        assert_eq!(status, 200, "{held}");
        let reservation = held["reservation_id"].as_str().expect("reservation");
        let execute = |suffix: &str, trial: Option<Value>| {
            let (status, ticket) = service.request("POST", "/service/v2/operation-ids", Some(json!({"api_version":2,"instance_id":service.instance,"claimant_id":"UI_SYNTHETIC_operator","reservation_id":reservation})));
            assert_eq!(status, 200, "{ticket}");
            let id = ticket["request_id"].as_str().expect("ticket");
            let mut body = base.clone();
            body["claimant_id"] = json!("UI_SYNTHETIC_operator");
            body["reservation_id"] = json!(reservation);
            body["request_id"] = json!(id);
            if let Some(trial) = trial {
                body["trial"] = trial;
            }
            let (status, queued) = service.request(
                "POST",
                &format!("/service/v2/bindings/{binding}/{suffix}"),
                Some(body),
            );
            assert_eq!(status, 202, "{queued}");
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let (status, value) =
                    service.request("GET", &format!("/service/v2/operations/{id}"), None);
                assert_eq!(status, 200, "{value}");
                if value["operation"]["admission"] == "finished" {
                    break value["operation"].clone();
                }
                assert!(Instant::now() < deadline, "operation deadline: {value}");
                thread::sleep(Duration::from_millis(5));
            }
        };
        let started = execute("recording/start", None);
        assert_eq!(started["submission"], "locally_submitted", "{started}");
        let operation = execute(
            "trials",
            Some(
                json!({"command":{"action":"position","mm":1},"envelope":{"position_min_mm":-10,"position_max_mm":10,"velocity_abs_max_mm_s":2,"force_abs_max_n":10,"stiffness_max_n_per_mm":10,"damping_max_ns_per_mm":10,"duration_max_s":5},"duration_s":5}),
            ),
        );
        assert_eq!(operation["submission"], "not_submitted", "{operation}");
        assert_eq!(operation["control_effect"], "not_attempted");
        assert_eq!(operation["error"]["code"], "recording");
        let native: Value = serde_json::from_str(
            operation["native_json_text"]
                .as_str()
                .expect("native error"),
        )
        .expect("native JSON");
        assert_eq!(native["unknown"], false);
        assert_eq!(native["local_stage"], "not_submitted");
        assert!(
            native["message"]
                .as_str()
                .expect("IO error")
                .contains("Bad file descriptor"),
            "{native}"
        );
        let stopped = execute("recording/stop", None);
        assert_eq!(stopped["submission"], "not_submitted", "{stopped}");
        assert_eq!(stopped["error"]["code"], "recording");
        let events = service.events();
        assert!(
            events
                .iter()
                .any(|entry| entry["kind"] == "recording_fault_armed")
        );
        assert!(!events.iter().any(|entry| entry["kind"] == "pump_taken"
            && matches!(
                entry["facts"]["action"].as_str(),
                Some("position" | "velocity" | "force" | "impedance" | "stop")
            )));
        let runs: Vec<_> = fs::read_dir(service.root.join("runs"))
            .expect("runs")
            .map(|entry| entry.expect("run").path())
            .collect();
        assert_eq!(runs.len(), 1);
        let metadata: Value =
            serde_json::from_slice(&fs::read(runs[0].join("metadata.json")).expect("metadata"))
                .expect("metadata JSON");
        assert!(metadata.get("completion").is_none());
        assert!(
            !fs::read_to_string(runs[0].join("events.jsonl"))
                .expect("partial events")
                .contains("trial_started")
        );
    }

    #[test]
    fn fixture_guard_allows_fixed_control_scope_listing_only() {
        assert!(allowed_path("/service/v2/control-scopes", Method::Get));
        for path in [
            "/service/v2/control-scopes/arbitrary",
            "/service/v2/profiles",
            "/service/v2/adapters",
            "/service/v2/bindings/fixed/maintenance",
        ] {
            assert!(!allowed_path(path, Method::Get), "restricted read {path}");
        }
        assert!(!allowed_path("/service/v2/control-scopes", Method::Post));
        let service = Running::start(Scenario::Healthy, Duration::from_secs(10));
        let (status, value) = service.request("GET", "/service/v2/control-scopes", None);
        assert_eq!(status, 200, "{value}");
        assert_eq!(value["scopes"], json!([]));
    }

    #[test]
    fn real_owner_authentication_and_slow_read_do_not_block_fixed_scope_stop() {
        let service = Running::start(Scenario::SlowRead, Duration::from_secs(10));
        assert_eq!(
            http(
                service.port,
                "incorrect",
                "GET",
                "/service/v2/describe",
                None
            )
            .0,
            401
        );
        for path in [
            "/service/v2/adapters",
            "/service/v2/odrive/devices",
            "/service/v2/scans",
            "/integration/v1/targets",
        ] {
            assert_eq!(
                service.request("GET", path, None).0,
                403,
                "physical route {path}"
            );
        }
        assert_eq!(service.request("POST","/service/v2/connections",Some(json!({"api_version":2,"instance_id":service.instance,"transport":"can","channel":"physical"}))).0,403);
        assert_eq!(service.request("POST","/service/v2/bindings",Some(json!({"api_version":2,"instance_id":service.instance,"target_id":"usb:UI_SYNTHETIC_FORBIDDEN","intent":"connect"}))).0,403);
        let (first, mut read) = service.attach(1);
        let (second, mut permission) = service.attach(2);
        permission["permissions"] = json!({"maintenance":false,"control":true});
        permission["expected_grant_id"] = Value::Null;
        let (status, grant) = service.request(
            "POST",
            &format!("/service/v2/bindings/{second}/permissions"),
            Some(permission.clone()),
        );
        assert_eq!(status, 200, "{grant}");
        read["action"] = json!("diagnostics");
        let port = service.port;
        let token = service.token.clone();
        let waiting = thread::spawn(move || {
            http(
                port,
                &token,
                "POST",
                &format!("/service/v2/bindings/{first}/reads"),
                Some(read),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !service
            .events()
            .iter()
            .any(|e| e["kind"] == "read_reply_held")
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!waiting.is_finished(), "actual HTTP read still pending");
        let mut stop = permission;
        stop.as_object_mut().expect("object").remove("permissions");
        stop.as_object_mut()
            .expect("object")
            .remove("expected_grant_id");
        stop["claimant_id"] = json!("UI_SYNTHETIC_operator");
        stop["request_id"] = grant["stop_request_id"].clone();
        let (status, receipt) = service.request(
            "POST",
            &format!("/service/v2/bindings/{second}/stop"),
            Some(stop),
        );
        assert_eq!(status, 202, "{receipt}");
        let request_id = receipt["operation"]["request_id"]
            .as_str()
            .expect("original Stop ID");
        let stop_deadline = Instant::now() + Duration::from_secs(1);
        let submission_id = loop {
            let (_, result) =
                service.request("GET", &format!("/service/v2/operations/{request_id}"), None);
            if result["operation"]["admission"] == "finished" {
                assert_eq!(result["operation"]["submission"], "locally_submitted");
                let native: Value = serde_json::from_str(
                    result["operation"]["native_json_text"]
                        .as_str()
                        .expect("native receipt"),
                )
                .expect("native receipt JSON");
                break native["local_submission"]["id"]
                    .as_u64()
                    .expect("actual SDK transport ID")
                    .to_string();
            }
            assert!(Instant::now() < stop_deadline);
            thread::sleep(Duration::from_millis(5));
        };
        assert!(
            !waiting.is_finished(),
            "Stop attempted and received while slow read pending"
        );
        assert_eq!(waiting.join().expect("read thread").0, 200);
        let events = service.events();
        let stopped = events
            .iter()
            .position(|e| {
                e["kind"] == "pump_taken" && e["node"] == 2 && e["facts"]["action"] == "stop"
            })
            .expect("actual Stop taken");
        let replied = events
            .iter()
            .position(|e| e["kind"] == "read_reply" && e["node"] == 1)
            .expect("slow reply");
        assert!(stopped < replied);
        assert_eq!(events[stopped]["transport_local_id"], submission_id);
        assert!(
            events
                .iter()
                .any(|event| event["kind"] == "owner_transition"
                    && event["facts"]["request_id"] == request_id
                    && event["facts"]["transport_local_id"] == submission_id)
        );
    }

    #[test]
    fn fixed_crash_stages_preserve_queued_taken_and_unknown_boundaries() {
        for scenario in [
            Scenario::QueuedTarget,
            Scenario::TakenTarget,
            Scenario::TargetUnknown,
            Scenario::StopUnknown,
        ] {
            let service = Running::start(scenario, Duration::from_secs(10));
            let (binding, base) = service.attach(1);
            let mut fields = base.clone();
            fields["permissions"] = json!({"maintenance":false,"control":true});
            fields["expected_grant_id"] = Value::Null;
            let (status, grant) = service.request(
                "POST",
                &format!("/service/v2/bindings/{binding}/permissions"),
                Some(fields),
            );
            assert_eq!(status, 200, "{grant}");
            let (status,reservation)=service.request("POST","/service/v2/control-reservations",Some(json!({"api_version":2,"instance_id":service.instance,"claimant_id":"UI_SYNTHETIC_operator","permission":"control","members":[{"binding_id":binding,"expected_identity":base["expected_identity"],"connection_generation":base["connection_generation"]}]})));
            assert_eq!(status, 200, "{reservation}");
            let (status,ticket)=service.request("POST","/service/v2/operation-ids",Some(json!({"api_version":2,"instance_id":service.instance,"claimant_id":"UI_SYNTHETIC_operator","reservation_id":reservation["reservation_id"]})));
            assert_eq!(status, 200, "{ticket}");
            let mut target = base.clone();
            target["claimant_id"] = json!("UI_SYNTHETIC_operator");
            target["reservation_id"] = reservation["reservation_id"].clone();
            target["request_id"] = ticket["request_id"].clone();
            target["command"] = json!({"action":"position","mm":1});
            let (status, result) = service.request(
                "POST",
                &format!("/service/v2/bindings/{binding}/controls"),
                Some(target),
            );
            assert_eq!(status, 202, "{result}");
            let kind = match scenario {
                Scenario::QueuedTarget => "target_returned_unsent",
                Scenario::TakenTarget => "target_receipt_held",
                Scenario::TargetUnknown => "submission_unknown",
                _ => "local_submitted",
            };
            let deadline = Instant::now() + Duration::from_secs(1);
            while !service.events().iter().any(|event| {
                event["kind"] == kind
                    && event["node"] == 1
                    && (scenario != Scenario::StopUnknown || event["facts"]["action"] == "position")
            }) {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            let mut stop = base;
            stop["claimant_id"] = json!("UI_SYNTHETIC_operator");
            stop["request_id"] = grant["stop_request_id"].clone();
            let (status, result) = service.request(
                "POST",
                &format!("/service/v2/bindings/{binding}/stop"),
                Some(stop),
            );
            assert_eq!(status, 202, "{scenario:?} {result}");
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let (_, result) = service.request(
                    "GET",
                    &format!(
                        "/service/v2/operations/{}",
                        grant["stop_request_id"].as_str().expect("Stop")
                    ),
                    None,
                );
                if result["operation"]["admission"] == "finished" {
                    assert_eq!(
                        result["operation"]["submission"],
                        if scenario == Scenario::StopUnknown {
                            "unknown"
                        } else {
                            "locally_submitted"
                        }
                    );
                    break;
                }
                assert!(Instant::now() < deadline, "{scenario:?} {result}");
                thread::sleep(Duration::from_millis(5));
            }
            let (_, original) = service.request(
                "GET",
                &format!(
                    "/service/v2/operations/{}",
                    ticket["request_id"].as_str().expect("target")
                ),
                None,
            );
            assert_eq!(
                original["operation"]["submission"],
                match scenario {
                    Scenario::QueuedTarget => "not_submitted",
                    Scenario::TakenTarget | Scenario::TargetUnknown => "unknown",
                    _ => "locally_submitted",
                }
            );
        }
    }

    #[test]
    fn http_started_trial_reaches_deadline_without_any_ui_polling() {
        let service = Running::start(Scenario::Healthy, Duration::from_secs(10));
        let (binding, base) = service.attach(2);
        let mut permission = base.clone();
        permission["permissions"] = json!({"maintenance":false,"control":true});
        permission["expected_grant_id"] = Value::Null;
        assert_eq!(
            service
                .request(
                    "POST",
                    &format!("/service/v2/bindings/{binding}/permissions"),
                    Some(permission)
                )
                .0,
            200
        );
        let (status,reservation)=service.request("POST","/service/v2/control-reservations",Some(json!({"api_version":2,"instance_id":service.instance,"claimant_id":"UI_SYNTHETIC_operator","permission":"control","members":[{"binding_id":binding,"expected_identity":base["expected_identity"],"connection_generation":base["connection_generation"]}]})));
        assert_eq!(status, 200, "{reservation}");
        let (_,ticket)=service.request("POST","/service/v2/operation-ids",Some(json!({"api_version":2,"instance_id":service.instance,"claimant_id":"UI_SYNTHETIC_operator","reservation_id":reservation["reservation_id"]})));
        let mut fields = base;
        fields["claimant_id"] = json!("UI_SYNTHETIC_operator");
        fields["reservation_id"] = reservation["reservation_id"].clone();
        fields["request_id"] = ticket["request_id"].clone();
        fields["trial"] = json!({"command":{"action":"position","mm":1},"envelope":{"position_min_mm":-10,"position_max_mm":10,"velocity_abs_max_mm_s":2,"force_abs_max_n":10,"stiffness_max_n_per_mm":10,"damping_max_ns_per_mm":10,"duration_max_s":5},"duration_s":0.3,"reach":null});
        let (status, admitted) = service.request(
            "POST",
            &format!("/service/v2/bindings/{binding}/trials"),
            Some(fields),
        );
        assert_eq!(status, 202, "{admitted}");
        let trial_id = ticket["request_id"]
            .as_str()
            .expect("original trial operation");
        // 此后不发送 UI 读取/推进请求；完整正式服务循环独立执行 preflight/start/deadline。
        thread::sleep(Duration::from_millis(700));
        let (status, trial) = service.request(
            "GET",
            &format!("/service/v2/bindings/{binding}/trials/{trial_id}"),
            None,
        );
        assert_eq!(status, 200, "{trial}");
        let native: Value = serde_json::from_str(
            trial["trial"]["native_json_text"]
                .as_str()
                .expect("native trial"),
        )
        .expect("native JSON");
        assert_eq!(native["stop_reason"], "duration_elapsed", "{native}");
        assert_eq!(
            native["stop_observed"], true,
            "synthetic fresh observation only"
        );
        let events = service.events();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["kind"] == "pump_taken"
                    && event["node"] == 2
                    && event["facts"]["action"] == "position")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["kind"] == "pump_taken"
                    && event["node"] == 2
                    && event["facts"]["action"] == "stop")
                .count(),
            1
        );
    }

    #[test]
    fn explicit_fixture_expiry_returns_shared_loop_and_removes_descriptor() {
        let mut service = Running::start(Scenario::Healthy, Duration::from_millis(100));
        service
            .worker
            .take()
            .expect("worker")
            .join()
            .expect("join")
            .expect("automatic bounded exit");
        assert!(
            !service
                .root
                .join(format!("port-{}.json", service.port))
                .exists()
        );
        assert!(
            service
                .events()
                .iter()
                .any(|event| event["kind"] == "trace_end" && event["dropped"] == 0)
        );
    }
}
