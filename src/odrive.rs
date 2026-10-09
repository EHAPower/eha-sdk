// Copyright The eha-sdk Contributors
//! 通过已安装的 ODrive 0.5.1 Python/Fibre 栈读取指定 USB 驱动器。
//!
//! 本模块只启动一次性只读子进程。它不会选择第一块设备、清错、喂狗、写入或后台轮询；
//! 调用方须明确提供 Python 解释器和 ODrive USB 序列号。

use std::{
    fmt,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

const BRIDGE: &str = include_str!("odrive_readonly.py");

/// ODrive USB 只读调用失败的类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// 调用前参数无效，未启动解释器或接触 USB。
    InvalidInput,
    /// 无法启动指定的 Python 解释器。
    Interpreter,
    /// 子进程在总期限内没有完成，已被终止并回收。
    Timeout,
    /// 缺少 `odrive`、Fibre 或 PyUSB 依赖。
    Dependency,
    /// 找不到、无法连接指定 ODrive，或读取 USB 序列号失败。
    Connection,
    /// Fibre 连接对象的序列号与请求不一致。
    IdentityMismatch,
    /// 已连接设备但读取状态失败。
    Read,
    /// 桥接进程没有给出可识别的结构化结果。
    Bridge,
}

impl ErrorKind {
    /// 面向 JSON/CLI 的稳定错误类别名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Interpreter => "interpreter",
            Self::Timeout => "timeout",
            Self::Dependency => "dependency",
            Self::Connection => "connection",
            Self::IdentityMismatch => "identity_mismatch",
            Self::Read => "read",
            Self::Bridge => "bridge",
        }
    }
}

/// ODrive USB 只读调用失败；`detail` 保留桥接层的原始异常文本。
#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub detail: Option<String>,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for Error {}

/// 列出当前主机可见的 ODrive USB 序列号，不连接 Fibre，也不选择设备。
pub fn discover(python: &Path, timeout: Duration) -> Result<Value, Error> {
    run_bridge(python, "discover", "", timeout)
}

/// 读取一个明确指定的 ODrive USB 设备。
///
/// 返回值的 `source` 始终是 `odrive_usb`；错误也以 [`Error`] 的类别和原始详情表达。
/// `serial` 是十六进制，可省略 `0x` 前缀；它在启动解释器前校验，因此无效输入不会产生
/// USB I/O。
pub fn read(python: &Path, serial: &str, timeout: Duration) -> Result<Value, Error> {
    let serial = normalized_serial(serial)?;
    run_bridge(python, "read", &serial, timeout)
}

fn normalized_serial(input: &str) -> Result<String, Error> {
    let input = input.trim();
    let digits = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .unwrap_or(input);
    let value = u64::from_str_radix(digits, 16).map_err(|_| Error {
        kind: ErrorKind::InvalidInput,
        message: "ODrive USB 序列号必须是十六进制正整数，可省略 0x 前缀".into(),
        detail: None,
    })?;
    if value == 0 {
        return Err(Error {
            kind: ErrorKind::InvalidInput,
            message: "ODrive USB 序列号必须是十六进制正整数，可省略 0x 前缀".into(),
            detail: None,
        });
    }
    Ok(format!("0x{value:X}"))
}

fn run_bridge(python: &Path, mode: &str, serial: &str, timeout: Duration) -> Result<Value, Error> {
    if timeout.is_zero() {
        return Err(Error {
            kind: ErrorKind::InvalidInput,
            message: "ODrive USB 读取期限必须为正数".into(),
            detail: None,
        });
    }
    run_bridge_script(python, BRIDGE, mode, serial, timeout)
}

fn run_bridge_script(
    python: &Path,
    script: &str,
    mode: &str,
    serial: &str,
    timeout: Duration,
) -> Result<Value, Error> {
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(mode)
        .arg(serial)
        .arg(timeout.as_secs_f64().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error {
            kind: ErrorKind::Interpreter,
            message: format!("无法启动 ODrive Python 解释器：{error}"),
            detail: Some(error.to_string()),
        })?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| Error {
            kind: ErrorKind::Bridge,
            message: format!("无法等待 ODrive 只读进程：{error}"),
            detail: Some(error.to_string()),
        })? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error {
                kind: ErrorKind::Timeout,
                message: format!("ODrive USB 只读在 {} ms 内未完成", timeout.as_millis()),
                detail: None,
            });
        }
        thread::sleep(Duration::from_millis(2));
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut output) = child.stdout.take() {
        output.read_to_end(&mut stdout).map_err(|error| Error {
            kind: ErrorKind::Bridge,
            message: format!("无法读取 ODrive 只读结果：{error}"),
            detail: Some(error.to_string()),
        })?;
    }
    if let Some(mut output) = child.stderr.take() {
        output.read_to_end(&mut stderr).map_err(|error| Error {
            kind: ErrorKind::Bridge,
            message: format!("无法读取 ODrive 只读错误输出：{error}"),
            detail: Some(error.to_string()),
        })?;
    }
    let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
    let response: Value = serde_json::from_slice(&stdout).map_err(|error| Error {
        kind: ErrorKind::Bridge,
        message: format!("ODrive 只读进程未返回 JSON：{error}"),
        detail: if stderr.is_empty() {
            None
        } else {
            Some(stderr.clone())
        },
    })?;

    if response.get("ok") == Some(&Value::Bool(true)) && status.success() {
        return Ok(response);
    }
    let bridge_error = response.get("error").and_then(Value::as_object);
    let kind = bridge_error
        .and_then(|error| error.get("kind"))
        .and_then(Value::as_str)
        .map(bridge_kind)
        .unwrap_or(ErrorKind::Bridge);
    let message = bridge_error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("ODrive 只读进程失败")
        .to_owned();
    let detail = bridge_error
        .and_then(|error| error.get("detail"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| (!stderr.is_empty()).then_some(stderr));
    Err(Error {
        kind,
        message,
        detail,
    })
}

fn bridge_kind(kind: &str) -> ErrorKind {
    match kind {
        "invalid_input" => ErrorKind::InvalidInput,
        "dependency" => ErrorKind::Dependency,
        "connection" => ErrorKind::Connection,
        "identity_mismatch" => ErrorKind::IdentityMismatch,
        "read" => ErrorKind::Read,
        _ => ErrorKind::Bridge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_serial_is_rejected_before_subprocess_io() {
        let error = read(
            Path::new("/definitely/not/python"),
            "not-a-serial",
            Duration::from_secs(1),
        )
        .expect_err("无效序列号不得尝试启动解释器");
        assert_eq!(error.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn bare_hex_serial_is_not_interpreted_as_decimal() {
        assert_eq!(
            normalized_serial("327834523034").expect("全数字序列号仍是十六进制"),
            "0x327834523034"
        );
        let error = read(
            Path::new("/definitely/not/python"),
            "327834523034",
            Duration::from_secs(1),
        )
        .expect_err("有效序列号应在解释器启动处失败");
        assert_eq!(error.kind, ErrorKind::Interpreter);
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_bridge_is_terminated_and_reaped() {
        let error = run_bridge_script(
            Path::new("/bin/sh"),
            "exec sleep 10",
            "unused",
            "unused",
            Duration::from_millis(20),
        )
        .expect_err("子进程必须在总期限结束后停止");
        assert_eq!(error.kind, ErrorKind::Timeout);
    }
}
