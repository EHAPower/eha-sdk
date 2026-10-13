// Copyright The eha-sdk Contributors
//! 显式共享服务观察客户端；不打开设备，不重试，也没有直连回退。

use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    time::{Duration, Instant},
};

const PROTOCOL: &str = "eha-tool.service.v2";
const RESPONSE_BYTES: usize = 1024 * 1024;
const HEADER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
pub(crate) enum ReadAction {
    Status,
    Measurements,
    Diagnostics,
    ConfigStartup,
    ConfigUser,
}

impl ReadAction {
    fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Measurements => "measurements",
            Self::Diagnostics => "diagnostics",
            Self::ConfigStartup => "config_startup",
            Self::ConfigUser => "config_user",
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    protocol: String,
    api_version: u8,
    instance_id: String,
    code: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct Describe {
    capabilities: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    uid: String,
    run_nonce: String,
}

#[derive(Deserialize)]
struct Target {
    target_id: String,
    connected: bool,
    identity: Option<Identity>,
    connection_generation: Option<String>,
}

#[derive(Deserialize)]
struct Targets {
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Bound {
    binding_id: String,
    target_id: String,
    connection_generation: String,
    identity: Identity,
}

#[derive(Deserialize)]
struct ReadReply {
    #[serde(flatten)]
    bound: Bound,
    action: String,
    native_json_text: String,
}

struct Failure {
    message: String,
}

impl Failure {
    fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

struct Client {
    address: SocketAddr,
    instance_id: String,
    token: String,
    timeout: Duration,
}

/// 一次认证、显式选择、attach、读取、detach；输出保持服务原生 JSON 文本。
pub(crate) fn read(
    instance_file: &Path,
    target_id: &str,
    action: ReadAction,
    timeout: Duration,
) -> Result<String, String> {
    let client = Client::from_file(instance_file, timeout)?;
    client
        .read(target_id, action)
        .map_err(|error| error.message)
}

impl Client {
    fn from_file(path: &Path, timeout: Duration) -> Result<Self, String> {
        let descriptor: crate::service::instance::Descriptor =
            serde_json::from_slice(&private_bytes(path, 65536)?)
                .map_err(|_| "descriptor_invalid: 服务发现记录格式无效".to_owned())?;
        if descriptor.protocol != PROTOCOL || descriptor.api_version != 2 {
            return Err("version_mismatch: 服务发现记录协议或版本不支持".into());
        }
        if descriptor.instance_id.is_empty() {
            return Err("descriptor_invalid: 服务实例标识缺失".into());
        }
        let address = descriptor
            .endpoint
            .strip_prefix("http://")
            .and_then(|text| text.parse::<SocketAddr>().ok())
            .filter(|address| address.ip().is_loopback() && address.port() != 0)
            .ok_or("endpoint_invalid: 服务端点必须是固定 IP 的 loopback HTTP 地址")?;
        let bytes = private_bytes(&descriptor.token_file, 65)?;
        let token = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.strip_suffix('\n'))
            .filter(|text| {
                text.len() == 64
                    && text
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
            .ok_or("credential_invalid: 服务凭据文件格式无效")?;
        if timeout.is_zero() || timeout > Duration::from_secs(60) {
            return Err("bad_request: 共享服务 timeout-secs 必须为 1 至 60 秒".into());
        }
        Ok(Self {
            address,
            instance_id: descriptor.instance_id,
            token: token.into(),
            timeout,
        })
    }

    fn read(&self, target_id: &str, action: ReadAction) -> Result<String, Failure> {
        if target_id.is_empty() {
            return Err(Failure::local("bad_request: 必须明确指定 target-id"));
        }
        let describe: Describe = self.request("GET", "/service/v2/describe", None)?;
        for capability in [
            "targets",
            "binding_attach",
            "binding_lease",
            &format!("read_{}", action.name()),
        ] {
            if !describe
                .capabilities
                .iter()
                .any(|value| value == capability)
            {
                return Err(Failure::local(format!(
                    "unsupported: 服务未提供 {capability}"
                )));
            }
        }
        let targets: Targets = self.request("GET", "/service/v2/targets", None)?;
        let mut matching = targets
            .targets
            .into_iter()
            .filter(|target| target.target_id == target_id);
        let target = matching
            .next()
            .ok_or_else(|| Failure::local("not_found: 服务中没有指定目标"))?;
        if matching.next().is_some() {
            return Err(Failure::local("ambiguous_target: 服务返回重复目标"));
        }
        let identity = target
            .identity
            .filter(|identity| !identity.uid.is_empty() && !identity.run_nonce.is_empty())
            .ok_or_else(|| Failure::local("identity_changed: 指定目标没有可绑定身份"))?;
        let generation = target
            .connection_generation
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| Failure::local("binding_stale: 指定目标没有有效连接代次"))?;
        if !target.connected {
            return Err(Failure::local(
                "not_connected: 共享服务只附着既有 owner 会话",
            ));
        }
        let lease_ms = self.timeout.as_millis() * 2 + 1_000;
        let attach = serde_json::json!({
            "lease_ms":lease_ms,
            "api_version":2, "instance_id":self.instance_id, "intent":"attach",
            "target_id":target_id, "expected_identity":identity, "connection_generation":generation,
        });
        let bound: Bound = self.request("POST", "/service/v2/bindings", Some(&attach))?;
        if bound.target_id != target_id
            || bound.identity != identity
            || bound.connection_generation != generation
        {
            return Err(Failure::local(
                "identity_changed: 服务 binding 与所选目标身份或代次不符",
            ));
        }
        let prefix = format!("{}-v2-b", self.instance_id);
        if !bound
            .binding_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !bound
                .binding_id
                .strip_prefix(&prefix)
                .is_some_and(|sequence| {
                    !sequence.is_empty() && sequence.bytes().all(|byte| byte.is_ascii_digit())
                })
        {
            return Err(Failure::local("binding_invalid: 服务 binding 标识格式无效"));
        }
        let path = format!("/service/v2/bindings/{}", bound.binding_id);
        let payload = serde_json::json!({
            "api_version":2, "instance_id":self.instance_id,
            "expected_identity":identity, "connection_generation":generation, "action":action.name(),
        });
        let result = self
            .request::<ReadReply>("POST", &format!("{path}/reads"), Some(&payload))
            .and_then(|reply| {
                if reply.bound.binding_id != bound.binding_id
                    || reply.bound.target_id != target_id
                    || reply.bound.identity != identity
                    || reply.bound.connection_generation != generation
                    || reply.action != action.name()
                {
                    return Err(Failure {
                        message: "identity_changed: 读取回复身份、代次或动作不符".into(),
                    });
                }
                serde_json::from_str::<&serde_json::value::RawValue>(&reply.native_json_text)
                    .map_err(|_| Failure {
                        message: "response_invalid: 设备原生 JSON 无效".into(),
                    })?;
                Ok(reply.native_json_text)
            });
        // 清理只针对已验证的当次实例观察 ID；从不重放原读取或改变 owner。
        let detached = self.request::<Envelope>("DELETE", &path, None);
        if result.is_ok() {
            detached?;
        }
        result
    }

    fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, Failure> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| Failure::local("request_invalid: 服务请求编码失败"))?
            .unwrap_or_default();
        let (status, bytes) = http(self.address, &self.token, method, path, &body, self.timeout)
            .map_err(Failure::local)?;
        let envelope: Envelope = serde_json::from_slice(&bytes)
            .map_err(|_| Failure::local("response_invalid: 服务响应不是有效 envelope"))?;
        if envelope.protocol != PROTOCOL || envelope.api_version != 2 {
            return Err(Failure::local("version_mismatch: 服务响应协议或版本不支持"));
        }
        if envelope.instance_id != self.instance_id {
            return Err(Failure::local("instance_mismatch: 服务实例已经改变"));
        }
        if !(200..300).contains(&status) {
            return Err(Failure {
                message: format!(
                    "{}: {}",
                    envelope.code.as_deref().unwrap_or("service_error"),
                    envelope.message.as_deref().unwrap_or("共享服务请求失败")
                ),
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| Failure {
            message: "response_invalid: 服务响应缺少所需字段".into(),
        })
    }
}

#[cfg(unix)]
fn private_bytes(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let (directory, name) = crate::service::instance::parent(path)
        .map_err(|error| format!("private_file_invalid: {error}"))?;
    let file = crate::service::instance::open_private(&directory, name, false)
        .map_err(|error| format!("private_file_invalid: {error}"))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "private_file_invalid: 服务私有文件读取失败".to_owned())?;
    if bytes.len() > limit {
        return Err("private_file_invalid: 服务私有文件过大".into());
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn private_bytes(_path: &Path, _limit: usize) -> Result<Vec<u8>, String> {
    Err("unsupported: 共享服务私有文件校验目前仅支持 Unix".into())
}

// 固定 loopback IP 直接 TCP，不解析 DNS、不使用代理、不发 Origin、不跟随 redirect。
fn http(
    address: SocketAddr,
    token: &str,
    method: &str,
    path: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<(u16, Vec<u8>), String> {
    let deadline = Instant::now() + timeout;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|_| "service_unreachable: 无法连接共享服务".to_owned())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| "service_io: 无法设置写超时")?;
    let headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|_| "service_io: 请求写入失败，未重试".to_owned())?;
    let mut bytes = Vec::new();
    let (header_end, status, length) = loop {
        read_more(
            &mut stream,
            &mut bytes,
            deadline,
            HEADER_BYTES + RESPONSE_BYTES,
        )?;
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            if end + 4 > HEADER_BYTES {
                return Err("response_oversize: HTTP 头过大".into());
            }
            let headers =
                std::str::from_utf8(&bytes[..end]).map_err(|_| "response_invalid: HTTP 头无效")?;
            let mut lines = headers.split("\r\n");
            let mut status_line = lines.next().unwrap_or("").split_whitespace();
            if !matches!(status_line.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
                return Err("response_invalid: HTTP 版本无效".into());
            }
            let status: u16 = status_line
                .next()
                .and_then(|text| text.parse().ok())
                .ok_or("response_invalid: HTTP 状态无效")?;
            if (300..400).contains(&status) {
                return Err("redirect_rejected: 共享服务禁止 redirect".into());
            }
            let mut length = None;
            let mut json_type = false;
            for line in lines {
                let (key, value) = line
                    .split_once(':')
                    .ok_or("response_invalid: HTTP 头格式无效")?;
                if key.eq_ignore_ascii_case("content-length") {
                    if length.is_some() {
                        return Err("response_invalid: 重复 Content-Length".into());
                    }
                    length = Some(
                        value
                            .trim()
                            .parse::<usize>()
                            .map_err(|_| "response_invalid: Content-Length 无效")?,
                    );
                } else if key.eq_ignore_ascii_case("transfer-encoding") {
                    return Err("response_invalid: 共享服务只支持固定 Content-Length".into());
                } else if key.eq_ignore_ascii_case("content-type") {
                    json_type = value
                        .trim()
                        .split(';')
                        .next()
                        .is_some_and(|value| value.eq_ignore_ascii_case("application/json"));
                }
            }
            let length = length.ok_or("response_invalid: 缺少 Content-Length")?;
            if length > RESPONSE_BYTES {
                return Err("response_oversize: 服务响应超过 1 MiB".into());
            }
            if !json_type {
                return Err("response_invalid: 服务响应必须是 JSON".into());
            }
            break (end + 4, status, length);
        }
        if bytes.len() >= HEADER_BYTES {
            return Err("response_oversize: HTTP 头过大".into());
        }
    };
    let total = header_end + length;
    while bytes.len() < total {
        read_more(&mut stream, &mut bytes, deadline, total)?;
    }
    if bytes.len() != total {
        return Err("response_invalid: HTTP 长度不符".into());
    }
    Ok((status, bytes[header_end..].to_vec()))
}

fn read_more(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    deadline: Instant,
    limit: usize,
) -> Result<(), String> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or("service_timeout: 共享服务读取超时，未重试")?;
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|_| "service_io: 无法设置读超时")?;
    let mut chunk = [0_u8; 8192];
    let capacity = chunk.len().min(limit.saturating_sub(bytes.len()));
    if capacity == 0 {
        return Err("response_oversize: 服务响应过大".into());
    }
    let count = stream.read(&mut chunk[..capacity]).map_err(|error| {
        if matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) {
            "service_timeout: 共享服务读取超时，未重试"
        } else {
            "service_io: 服务响应读取失败，未重试"
        }
    })?;
    if count == 0 {
        return Err("response_invalid: 服务响应提前结束".into());
    }
    bytes.extend_from_slice(&chunk[..count]);
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        net::TcpListener,
        os::unix::fs::{PermissionsExt, symlink},
        thread,
    };

    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct Fixture {
        path: std::path::PathBuf,
        server: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(server) = self.server.take() {
                server.join().expect("mock server");
            }
            let _ = fs::remove_dir_all(self.path.parent().expect("private directory"));
        }
    }
    fn envelope() -> serde_json::Value {
        json!({"protocol":PROTOCOL, "api_version":2, "instance_id":"i-mock"})
    }
    fn response(status: u16, value: serde_json::Value) -> String {
        let text = value.to_string();
        format!(
            "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}",
            text.len()
        )
    }
    fn fixture(replies: Vec<(&'static str, String)>, delay: Duration) -> Fixture {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback mock");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let path = std::env::temp_dir().join(format!(
            "eha-service-client-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("private directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("permissions");
        let path = fs::canonicalize(path).expect("canonical path");
        let token_file = path.join("token");
        fs::write(&token_file, "a".repeat(64) + "\n").expect("token");
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600))
            .expect("token permissions");
        let descriptor = crate::service::instance::Descriptor {
            protocol: PROTOCOL.into(),
            api_version: 2,
            tool_version: "mock".into(),
            instance_id: "i-mock".into(),
            pid: "0".into(),
            started_at_unix_ms: "0".into(),
            endpoint,
            token_file,
        };
        let file = path.join("instance.json");
        fs::write(&file, serde_json::to_vec(&descriptor).expect("descriptor")).expect("write");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
            .expect("descriptor permissions");
        let server = thread::spawn(move || {
            for (expected, reply) in replies {
                let (mut stream, _) = listener.accept().expect("mock accept");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("timeout");
                let mut request = Vec::new();
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    read_more(&mut stream, &mut request, deadline, 65536).expect("request");
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end]).expect("headers");
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("Content-Length: "))
                            .expect("length")
                            .trim()
                            .parse::<usize>()
                            .expect("length number");
                        if request.len() < end + 4 + length {
                            continue;
                        }
                        let text = std::str::from_utf8(&request).expect("request UTF8");
                        assert!(text.starts_with(expected), "{text}");
                        assert!(
                            text.contains(&format!("Authorization: Bearer {}\r\n", "a".repeat(64)))
                        );
                        assert!(!text.to_ascii_lowercase().contains("origin:"));
                        if expected == "POST /service/v2/bindings " {
                            let body: serde_json::Value =
                                serde_json::from_slice(&request[end + 4..]).expect("attach body");
                            assert_eq!(body["intent"], "attach");
                            assert_eq!(body["api_version"], 2);
                            assert_eq!(body["instance_id"], "i-mock");
                            assert_eq!(body["target_id"], "can:7");
                            assert_eq!(body["connection_generation"], "19");
                            assert_eq!(
                                body["expected_identity"],
                                json!({"uid":"uid-7", "run_nonce":"nonce-7"})
                            );
                        }
                        if expected.starts_with("POST /service/v2/bindings/i-mock-v2-b1/reads") {
                            let body: serde_json::Value =
                                serde_json::from_slice(&request[end + 4..]).expect("read body");
                            assert_eq!(body["api_version"], 2);
                            assert_eq!(body["instance_id"], "i-mock");
                            assert_eq!(body["connection_generation"], "19");
                            assert_eq!(
                                body["expected_identity"],
                                json!({"uid":"uid-7", "run_nonce":"nonce-7"})
                            );
                        }
                        break;
                    }
                }
                if reply == "delayed-read" {
                    thread::sleep(Duration::from_millis(100));
                    let mut late = bound();
                    late["action"] = json!("status");
                    late["native_json_text"] = json!("{}");
                    let _ = stream.write_all(response(200, late).as_bytes());
                } else {
                    thread::sleep(delay);
                    let _ = stream.write_all(reply.as_bytes());
                }
            }
        });
        Fixture {
            path: file,
            server: Some(server),
        }
    }
    fn describe() -> serde_json::Value {
        let mut value = envelope();
        value["capabilities"] = json!([
            "targets",
            "binding_attach",
            "binding_lease",
            "read_status",
            "read_measurements",
            "read_diagnostics",
            "read_config_startup",
            "read_config_user"
        ]);
        value
    }
    fn targets() -> serde_json::Value {
        let mut value = envelope();
        value["targets"] = json!([
            {"target_id":"can:1","connected":true,"identity":{"uid":"other", "run_nonce":"other"},"connection_generation":"2"},
            {"target_id":"can:7","connected":true,"identity":{"uid":"uid-7", "run_nonce":"nonce-7"},"connection_generation":"19"}
        ]);
        value
    }
    fn bound() -> serde_json::Value {
        let mut value = envelope();
        value["binding_id"] = json!("i-mock-v2-b1");
        value["target_id"] = json!("can:7");
        value["connection_generation"] = json!("19");
        value["identity"] = json!({"uid":"uid-7", "run_nonce":"nonce-7"});
        value
    }
    fn prefix() -> Vec<(&'static str, String)> {
        vec![
            ("GET /service/v2/describe ", response(200, describe())),
            ("GET /service/v2/targets ", response(200, targets())),
            ("POST /service/v2/bindings ", response(200, bound())),
        ]
    }

    #[test]
    fn five_read_actions_attach_explicit_target_preserve_native_text_and_detach() {
        let native = "{ \"value\" : 1.0000000596046447753906250000000000000000000001, \"large\":18446744073709551615 }";
        for action in [
            ReadAction::Status,
            ReadAction::Measurements,
            ReadAction::Diagnostics,
            ReadAction::ConfigStartup,
            ReadAction::ConfigUser,
        ] {
            let mut reply = bound();
            reply["action"] = json!(action.name());
            reply["native_json_text"] = json!(native);
            let mut steps = prefix();
            steps.extend([
                (
                    "POST /service/v2/bindings/i-mock-v2-b1/reads ",
                    response(200, reply),
                ),
                (
                    "DELETE /service/v2/bindings/i-mock-v2-b1 ",
                    response(200, envelope()),
                ),
            ]);
            let fixture = fixture(steps, Duration::ZERO);
            assert_eq!(
                read(&fixture.path, "can:7", action, Duration::from_secs(1)).expect("read"),
                native
            );
        }
    }

    #[test]
    fn version_instance_credentials_and_unsupported_are_terminal() {
        for (field, value, status, expected) in [
            ("api_version", json!(3), 200, "version_mismatch"),
            ("protocol", json!("wrong"), 200, "version_mismatch"),
            (
                "instance_id",
                json!("i-restarted"),
                200,
                "instance_mismatch",
            ),
            ("capabilities", json!([]), 200, "unsupported"),
            ("code", json!("unauthorized"), 401, "unauthorized"),
        ] {
            let mut value_out = describe();
            value_out[field] = value;
            let fixture = fixture(
                vec![("GET /service/v2/describe ", response(status, value_out))],
                Duration::ZERO,
            );
            let error = read(
                &fixture.path,
                "can:7",
                ReadAction::Status,
                Duration::from_secs(1),
            )
            .expect_err("reject");
            assert!(error.starts_with(expected), "{error}");
            assert!(!error.contains(&"a".repeat(64)));
        }
    }

    #[test]
    fn stale_or_changed_read_is_never_replayed_and_only_detaches_observation() {
        for (field, value, expected, detach) in [
            ("instance_id", json!("i-new"), "instance_mismatch", true),
            (
                "identity",
                json!({"uid":"changed", "run_nonce":"new"}),
                "identity_changed",
                true,
            ),
            (
                "connection_generation",
                json!("20"),
                "identity_changed",
                true,
            ),
            ("code", json!("identity_changed"), "identity_changed", true),
        ] {
            let mut reply = bound();
            reply["action"] = json!("status");
            reply["native_json_text"] = json!("{}");
            reply[field] = value;
            let mut steps = prefix();
            steps.push((
                "POST /service/v2/bindings/i-mock-v2-b1/reads ",
                response(if field == "code" { 409 } else { 200 }, reply),
            ));
            if detach {
                steps.push((
                    "DELETE /service/v2/bindings/i-mock-v2-b1 ",
                    response(200, envelope()),
                ));
            }
            let fixture = fixture(steps, Duration::ZERO);
            assert!(
                read(
                    &fixture.path,
                    "can:7",
                    ReadAction::Status,
                    Duration::from_secs(1)
                )
                .expect_err("reject")
                .starts_with(expected)
            );
        }
    }

    #[test]
    fn foreign_or_malformed_binding_ids_never_read_or_detach_another_observer() {
        for id in ["i-old-v2-b1", "i-mock-v2-b1/stop", "i-mock-v2-b1\r\n"] {
            let mut reply = bound();
            reply["binding_id"] = json!(id);
            let mut steps = prefix();
            steps[2].1 = response(200, reply);
            let fixture = fixture(steps, Duration::ZERO);
            assert!(
                read(
                    &fixture.path,
                    "can:7",
                    ReadAction::Status,
                    Duration::from_secs(1)
                )
                .expect_err("foreign binding")
                .starts_with("binding_invalid:")
            );
        }
    }

    #[test]
    fn redirect_oversize_and_timeout_do_not_issue_second_request() {
        for (reply, delay, expected) in [
            (
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n"
                    .into(),
                Duration::ZERO,
                "redirect_rejected",
            ),
            (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    RESPONSE_BYTES + 1
                ),
                Duration::ZERO,
                "response_oversize",
            ),
            (
                response(200, describe()),
                Duration::from_millis(100),
                "service_timeout",
            ),
        ] {
            let fixture = fixture(vec![("GET /service/v2/describe ", reply)], delay);
            assert!(
                read(
                    &fixture.path,
                    "can:7",
                    ReadAction::Status,
                    Duration::from_millis(30)
                )
                .expect_err("reject")
                .starts_with(expected)
            );
        }
    }

    #[test]
    fn read_timeout_after_attach_uses_only_scoped_best_effort_cleanup() {
        let mut steps = prefix();
        steps.push((
            "POST /service/v2/bindings/i-mock-v2-b1/reads ",
            "delayed-read".into(),
        ));
        steps.push((
            "DELETE /service/v2/bindings/i-mock-v2-b1 ",
            response(200, envelope()),
        ));
        let fixture = fixture(steps, Duration::ZERO);
        let error = read(
            &fixture.path,
            "can:7",
            ReadAction::Status,
            Duration::from_millis(30),
        )
        .expect_err("读取等待超时");
        assert!(error.starts_with("service_timeout:"), "{error}");
    }

    #[test]
    fn descriptor_endpoint_token_permissions_and_links_are_checked_before_http() {
        let fixture = fixture(vec![], Duration::ZERO);
        let original = fs::read(&fixture.path).expect("descriptor");
        let mut descriptor: crate::service::instance::Descriptor =
            serde_json::from_slice(&original).expect("decode");
        for endpoint in [
            "http://example.com:80",
            "http://192.168.1.1:80",
            "https://127.0.0.1:80",
            "http://127.0.0.1:80/path",
            "http://user@127.0.0.1:80",
        ] {
            descriptor.endpoint = endpoint.into();
            fs::write(
                &fixture.path,
                serde_json::to_vec(&descriptor).expect("encode"),
            )
            .expect("write");
            assert!(
                Client::from_file(&fixture.path, Duration::from_secs(1))
                    .err()
                    .expect("reject")
                    .starts_with("endpoint_invalid")
            );
        }
        fs::write(&fixture.path, &original).expect("restore");
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o644)).expect("unsafe");
        assert!(Client::from_file(&fixture.path, Duration::from_secs(1)).is_err());
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))
            .expect("restore mode");
        let token = fixture.path.parent().expect("parent").join("token");
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).expect("unsafe token");
        assert!(Client::from_file(&fixture.path, Duration::from_secs(1)).is_err());
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).expect("restore mode");
        fs::write(&token, b"bad\n").expect("bad token");
        assert!(Client::from_file(&fixture.path, Duration::from_secs(1)).is_err());
        fs::remove_file(&fixture.path).expect("remove");
        symlink(&token, &fixture.path).expect("symlink");
        assert!(Client::from_file(&fixture.path, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn exact_target_never_selects_first_or_implicitly_connects() {
        for target_id in ["can:missing", "can:7"] {
            let mut candidates = targets();
            candidates["targets"][1]["connected"] = json!(false);
            let fixture = fixture(
                vec![
                    ("GET /service/v2/describe ", response(200, describe())),
                    ("GET /service/v2/targets ", response(200, candidates)),
                ],
                Duration::ZERO,
            );
            let error = read(
                &fixture.path,
                target_id,
                ReadAction::Status,
                Duration::from_secs(1),
            )
            .expect_err("reject");
            assert!(error.starts_with(if target_id == "can:7" {
                "not_connected"
            } else {
                "not_found"
            }));
        }
    }
}
