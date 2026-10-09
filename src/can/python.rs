// Copyright The eha-sdk Contributors
//! 通用的 [`python-can`](https://python-can.readthedocs.io/) 通道接入。
//!
//! 用户负责 `python-can` 配置及其加载的系统驱动。本模块通过持久进程调用公共
//! `Bus` 接口，只交换 CAN 帧；不枚举、配置、探测或识别具体适配器。
//! 帧调用把等待预算传给驱动，并额外允许 100 ms 进程通信开销；超过此期限即终止
//! 通道，不重放未知提交。启动最多等待 10 s，正常关闭最多等待 2.1 s。

use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};
use transport::can::Frame;

use super::{CanChannel, CanChannelFactory};

const BRIDGE: &str = include_str!("python_can.py");
const MAX_LINE: usize = 8 * 1024;
const MAX_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_OVERHEAD: Duration = Duration::from_millis(100);
const RESPONSE_QUEUE: usize = 4;

/// 选择外部配置的 `python-can` 通道。
///
/// 命名 `context` 必须明确提供驱动接口与通道；其余属性由 `python-can` 按自身规则
/// 解析。解释器、驱动安装、设备选择、位时序与访问权限均由部署方负责。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PythonCanOptions {
    /// 已安装 `python-can >=4.6.1,<5` 的 Python 解释器。
    pub python: String,
    /// 用户维护的 `python-can` 命名配置上下文。
    pub context: String,
}

impl PythonCanOptions {
    /// 使用平台默认 Python 命令创建通道选项。
    #[must_use]
    pub fn new(context: impl Into<String>) -> Self {
        Self {
            python: default_python().to_owned(),
            context: context.into(),
        }
    }

    /// 使用明确指定的 Python 解释器创建通道选项。
    #[must_use]
    pub fn with_python(context: impl Into<String>, python: impl Into<String>) -> Self {
        Self {
            python: python.into(),
            context: context.into(),
        }
    }
}

#[cfg(windows)]
const fn default_python() -> &'static str {
    "python"
}

#[cfg(not(windows))]
const fn default_python() -> &'static str {
    "python3"
}

impl CanChannelFactory for PythonCanOptions {
    fn open(&self) -> Result<Box<dyn CanChannel>, String> {
        if self.context.trim().is_empty() {
            return Err("python-can 配置 context 不能为空".into());
        }
        if self.python.trim().is_empty() {
            return Err("Python 解释器不能为空".into());
        }
        PythonCanChannel::open(self).map(|channel| Box::new(channel) as Box<dyn CanChannel>)
    }
}

/// [`PythonCanOptions`] 打开的、由单一 I/O 拥有者持有的 Python 进程。
pub struct PythonCanChannel {
    stdin: ChildStdin,
    child: Child,
    responses: Receiver<Result<Value, String>>,
    stderr: Arc<Mutex<String>>,
    next_id: u64,
    terminal: Option<String>,
}

impl PythonCanChannel {
    fn open(options: &PythonCanOptions) -> Result<Self, String> {
        Self::open_script(options, BRIDGE)
    }

    fn open_script(options: &PythonCanOptions, script: &str) -> Result<Self, String> {
        let mut child = Command::new(&options.python)
            .arg("-u")
            .arg("-c")
            .arg(script)
            .arg("--context")
            .arg(&options.context)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("无法启动 python-can 解释器 {}: {error}", options.python))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "无法取得 python-can 输入管道".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "无法取得 python-can 输出管道".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "无法取得 python-can 错误管道".to_owned())?;

        let (sender, responses) = mpsc::sync_channel(RESPONSE_QUEUE);
        thread::spawn(move || read_responses(stdout, sender));
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let stderr_for_thread = Arc::clone(&stderr_text);
        thread::spawn(move || read_stderr(stderr, stderr_for_thread));

        let mut channel = Self {
            stdin,
            child,
            responses,
            stderr: stderr_text,
            next_id: 1,
            terminal: None,
        };
        // 部分上游驱动在打开时会执行文档规定的稳定等待。这只影响构造过程；每个业务请求
        // 仍受调用方超时限制。
        let ready = channel.wait_response(Duration::from_secs(10), 0, "启动")?;
        if ready.get("ok") != Some(&Value::Bool(true))
            || ready.get("op") != Some(&Value::String("ready".into()))
        {
            let reason = response_error(&ready, "python-can 未确认就绪");
            channel.fail(&reason);
            return Err(reason);
        }
        Ok(channel)
    }

    fn request(
        &mut self,
        operation: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        if timeout > MAX_COMMAND_TIMEOUT {
            return Err(format!(
                "python-can {operation} 期限不能超过 {} ms",
                MAX_COMMAND_TIMEOUT.as_millis()
            ));
        }
        if let Some(reason) = self.terminal.as_ref() {
            return Err(format!("python-can 通路已终止: {reason}"));
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "python-can 请求序号耗尽，通路已终止".to_owned())?;
        let mut request = serde_json::Map::new();
        request.insert("id".into(), Value::from(id));
        request.insert("op".into(), Value::from(operation));
        request.insert("payload".into(), payload);
        let mut line = serde_json::to_vec(&Value::Object(request))
            .map_err(|error| format!("无法编码 python-can 请求: {error}"))?;
        line.push(b'\n');
        if line.len() > MAX_LINE {
            return Err("python-can 请求超过长度上限".into());
        }
        if let Err(error) = self
            .stdin
            .write_all(&line)
            .and_then(|()| self.stdin.flush())
        {
            let reason = format!("无法发送 python-can {operation} 请求: {error}");
            self.fail(&reason);
            return Err(reason);
        }
        self.wait_response(timeout.saturating_add(RESPONSE_OVERHEAD), id, operation)
    }

    fn wait_response(
        &mut self,
        timeout: Duration,
        expected_id: u64,
        operation: &str,
    ) -> Result<Value, String> {
        let response = match self.responses.recv_timeout(timeout) {
            Ok(Ok(response)) => response,
            Ok(Err(reason)) => {
                let reason = self.with_stderr(format!("python-can {operation} 响应无效: {reason}"));
                self.fail(&reason);
                return Err(reason);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let reason = self.with_stderr(format!(
                    "python-can {operation} 在 {} ms 内没有响应",
                    timeout.as_millis()
                ));
                self.fail(&reason);
                return Err(reason);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let reason = self.with_stderr(format!("python-can {operation} 进程已退出"));
                self.fail(&reason);
                return Err(reason);
            }
        };
        if response.get("id").and_then(Value::as_u64) != Some(expected_id) {
            let reason = self.with_stderr(format!("python-can {operation} 返回了不匹配的请求序号"));
            self.fail(&reason);
            return Err(reason);
        }
        if response.get("ok") != Some(&Value::Bool(true)) {
            let reason = self.with_stderr(response_error(&response, "python-can 操作失败"));
            self.fail(&reason);
            return Err(reason);
        }
        Ok(response)
    }

    fn with_stderr(&self, mut message: String) -> String {
        let detail = self.stderr.lock().ok().map(|value| value.trim().to_owned());
        if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
            message.push_str(": ");
            message.push_str(&detail);
        }
        message
    }

    fn fail(&mut self, reason: &str) {
        self.terminal = Some(reason.to_owned());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl CanChannel for PythonCanChannel {
    fn send(&mut self, frame: &Frame, timeout: Duration) -> Result<(), String> {
        let mut payload = frame_to_value(frame)?;
        if let Some(object) = payload.as_object_mut() {
            object.insert("timeout_ms".into(), Value::from(timeout.as_millis() as u64));
        }
        let response = self.request("send", payload, timeout)?;
        if response.get("submitted") != Some(&Value::Bool(true)) {
            let reason = self.with_stderr("python-can 未确认本地驱动已提交 CAN 帧".into());
            self.fail(&reason);
            return Err(reason);
        }
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<Frame>, String> {
        let response = self.request(
            "receive",
            json!({"timeout_ms": timeout.as_millis()}),
            timeout,
        )?;
        match response.get("frame") {
            Some(Value::Null) | None => Ok(None),
            Some(frame) => frame_from_value(frame).map(Some),
        }
    }
}

impl Drop for PythonCanChannel {
    fn drop(&mut self) {
        if self.terminal.is_none() {
            let _ = self
                .stdin
                .write_all(b"{\"id\":0,\"op\":\"close\",\"payload\":{}}\n");
            let _ = self.stdin.flush();
            let _ = self
                .responses
                .recv_timeout(MAX_COMMAND_TIMEOUT.saturating_add(RESPONSE_OVERHEAD));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_responses(stdout: impl std::io::Read, sender: SyncSender<Result<Value, String>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let line = match read_line_bounded(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) => {
                let _ = sender.send(Err("python-can 输出已关闭".into()));
                return;
            }
            Err(error) => {
                let _ = sender.send(Err(format!("无法读取 python-can 输出: {error}")));
                return;
            }
        };
        let response = serde_json::from_slice::<Value>(&line)
            .map_err(|error| format!("python-can 输出不是 JSON: {error}"));
        if sender.send(response).is_err() {
            return;
        }
    }
}

fn read_stderr(stderr: impl std::io::Read, target: Arc<Mutex<String>>) {
    let mut reader = BufReader::new(stderr);
    let mut chunk = [0_u8; 512];
    let mut tail = Vec::new();
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        tail.extend_from_slice(&chunk[..count]);
        if tail.len() > MAX_LINE {
            let keep_from = tail.len() - MAX_LINE;
            tail.drain(..keep_from);
        }
        if let Ok(mut value) = target.lock() {
            *value = String::from_utf8_lossy(&tail).trim().to_owned();
        }
    }
}

fn read_line_bounded(reader: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut output = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return if output.is_empty() {
                Ok(None)
            } else {
                Ok(Some(output))
            };
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(chunk.len(), |index| index + 1);
        if output.len().saturating_add(take) > MAX_LINE {
            reader.consume(take);
            while newline.is_none() {
                let rest = reader.fill_buf()?;
                if rest.is_empty() {
                    break;
                }
                let rest_newline = rest.iter().position(|byte| *byte == b'\n');
                let rest_take = rest_newline.map_or(rest.len(), |index| index + 1);
                reader.consume(rest_take);
                if rest_newline.is_some() {
                    break;
                }
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "一行超过上限",
            ));
        }
        output.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if newline.is_some() {
            output.pop();
            return Ok(Some(output));
        }
    }
}

fn response_error(response: &Value, fallback: &str) -> String {
    response
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

fn frame_to_value(frame: &Frame) -> Result<Value, String> {
    validate_frame(frame)?;
    Ok(json!({
        "id": frame.id,
        "extended": frame.extended,
        "rtr": frame.rtr,
        "fdf": frame.fdf,
        "brs": frame.brs,
        "esi": frame.esi,
        "dlc": frame.dlc,
        "data": frame.data(),
    }))
}

fn frame_from_value(value: &Value) -> Result<Frame, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "python-can 帧不是对象".to_owned())?;
    let id = value_u32(object.get("id"), "id")?;
    let extended = value_bool(object.get("extended"), "extended")?;
    let rtr = value_bool(object.get("rtr"), "rtr")?;
    let fdf = value_bool(object.get("fdf"), "fdf")?;
    let brs = value_bool(object.get("brs"), "brs")?;
    let esi = value_bool(object.get("esi"), "esi")?;
    let dlc = value_u8(object.get("dlc"), "dlc")?;
    let values = object
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "python-can 帧 data 不是数组".to_owned())?;
    if values.len() > 64 {
        return Err("python-can 帧 data 超过 64 字节".into());
    }
    let mut data = [0_u8; 64];
    for (index, value) in values.iter().enumerate() {
        data[index] = value_u8(Some(value), "data")?;
    }
    let frame = Frame {
        id,
        extended,
        rtr,
        fdf,
        brs,
        esi,
        dlc,
        data_len: u8::try_from(values.len()).map_err(|_| "python-can 帧 data 长度无效")?,
        data,
    };
    validate_frame(&frame)?;
    Ok(frame)
}

fn value_u32(value: Option<&Value>, field: &str) -> Result<u32, String> {
    value
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("python-can 帧 {field} 无效"))
}

fn value_u8(value: Option<&Value>, field: &str) -> Result<u8, String> {
    value
        .and_then(Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
        .ok_or_else(|| format!("python-can 帧 {field} 无效"))
}

fn value_bool(value: Option<&Value>, field: &str) -> Result<bool, String> {
    value
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("python-can 帧 {field} 无效"))
}

fn validate_frame(frame: &Frame) -> Result<(), String> {
    if frame.id > if frame.extended { 0x1fff_ffff } else { 0x7ff } {
        return Err("CAN 标识符超出帧形态范围".into());
    }
    if frame.rtr {
        if frame.fdf || frame.brs || frame.esi || frame.data_len != 0 || frame.dlc > 8 {
            return Err("RTR 必须是无数据的经典 CAN 帧".into());
        }
        return Ok(());
    }
    if frame.data_len > 64 || usize::from(frame.data_len) != expected_len(frame.dlc, frame.fdf)? {
        return Err("CAN DLC 与实际 data 长度不一致".into());
    }
    if !frame.fdf && (frame.brs || frame.esi) {
        return Err("经典 CAN 帧不能带 BRS 或 ESI".into());
    }
    Ok(())
}

fn expected_len(dlc: u8, fdf: bool) -> Result<usize, String> {
    match dlc {
        0..=8 => Ok(usize::from(dlc)),
        9 if fdf => Ok(12),
        10 if fdf => Ok(16),
        11 if fdf => Ok(20),
        12 if fdf => Ok(24),
        13 if fdf => Ok(32),
        14 if fdf => Ok(48),
        15 if fdf => Ok(64),
        _ => Err("CAN DLC 不适用于此帧形态".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_dlc_is_preserved_as_wire_code() {
        let mut data = [0_u8; 64];
        data[..12].copy_from_slice(&[1; 12]);
        let frame = Frame {
            id: 0x1fff_ffff,
            extended: true,
            rtr: false,
            fdf: true,
            brs: true,
            esi: false,
            dlc: 9,
            data_len: 12,
            data,
        };
        let encoded = frame_to_value(&frame).expect("有效 FD 帧");
        assert_eq!(encoded["dlc"], 9);
        assert_eq!(frame_from_value(&encoded).expect("往返帧"), frame);
    }

    #[test]
    fn malformed_dlc_is_not_padded_or_truncated() {
        let value = json!({
            "id": 1, "extended": false, "rtr": false, "fdf": true,
            "brs": true, "esi": false, "dlc": 9, "data": vec![0; 8]
        });
        assert!(frame_from_value(&value).is_err());
    }

    #[test]
    fn remote_frame_is_preserved_for_transport_to_classify() {
        let value = json!({
            "id": 0x321, "extended": false, "rtr": true, "fdf": false,
            "brs": false, "esi": false, "dlc": 5, "data": []
        });
        let frame = frame_from_value(&value).expect("有效 RTR 是总线输入，不是驱动失败");
        assert!(frame.rtr);
        assert_eq!(frame.dlc, 5);
        assert_eq!(frame.data_len, 0);
    }

    #[test]
    fn child_acknowledgement_is_required_for_submission() {
        let options = PythonCanOptions::new("test");
        if Command::new(&options.python)
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let script = r#"
import json, sys
print('{"id":0,"ok":true,"op":"ready"}', flush=True)
for line in sys.stdin:
    request = json.loads(line)
    if request["op"] == "send":
        assert request["payload"]["timeout_ms"] == 1
        print(json.dumps({"id":request["id"],"ok":True,"submitted":True}), flush=True)
    elif request["op"] == "close":
        print(json.dumps({"id":request["id"],"ok":True}), flush=True)
        break
"#;
        let mut channel = PythonCanChannel::open_script(&options, script).expect("假桥接进程启动");
        let frame = Frame {
            id: 1,
            extended: false,
            rtr: false,
            fdf: false,
            brs: false,
            esi: false,
            dlc: 1,
            data_len: 1,
            data: [7; 64],
        };
        channel
            .send(&frame, Duration::from_millis(1))
            .expect("只有子进程确认提交才成功");
    }

    #[test]
    fn child_eof_ends_channel_without_retry() {
        let options = PythonCanOptions::new("test");
        if Command::new(&options.python)
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let script = "import sys; print('{\"id\":0,\"ok\":true,\"op\":\"ready\"}', flush=True)";
        let mut channel = PythonCanChannel::open_script(&options, script).expect("假桥接进程启动");
        assert!(channel.receive(Duration::from_millis(1)).is_err());
        assert!(channel.receive(Duration::from_millis(1)).is_err());
    }

    #[test]
    fn missing_submit_ack_ends_channel_without_replay() {
        let options = PythonCanOptions::new("test");
        if Command::new(&options.python)
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let script = r#"
import json, sys, time
print('{"id":0,"ok":true,"op":"ready"}', flush=True)
for line in sys.stdin:
    if json.loads(line)["op"] == "send":
        time.sleep(10)
"#;
        let mut channel = PythonCanChannel::open_script(&options, script).expect("假桥接进程启动");
        let frame = Frame {
            id: 1,
            extended: false,
            rtr: false,
            fdf: false,
            brs: false,
            esi: false,
            dlc: 0,
            data_len: 0,
            data: [0; 64],
        };
        assert!(channel.send(&frame, Duration::from_millis(1)).is_err());
        assert!(channel.send(&frame, Duration::from_millis(1)).is_err());
    }

    #[test]
    fn mismatched_response_id_ends_channel() {
        let options = PythonCanOptions::new("test");
        if Command::new(&options.python)
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let script = r#"
import json, sys
print('{"id":0,"ok":true,"op":"ready"}', flush=True)
for line in sys.stdin:
    request = json.loads(line)
    if request["op"] == "receive":
        print(json.dumps({"id":request["id"] + 1,"ok":True,"frame":None}), flush=True)
"#;
        let mut channel = PythonCanChannel::open_script(&options, script).expect("假桥接进程启动");
        assert!(channel.receive(Duration::from_millis(1)).is_err());
        assert!(channel.receive(Duration::from_millis(1)).is_err());
    }
}
