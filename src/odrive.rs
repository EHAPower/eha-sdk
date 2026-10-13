// Copyright The eha-sdk Contributors
//! 通过已安装的 ODrive 0.5.1 Python/Fibre 栈读取指定 USB 驱动器。
//!
//! 本模块只启动一次性只读子进程。它不会选择第一块设备、清错、喂狗、写入或后台轮询；
//! 调用方须明确提供 Python 解释器和 ODrive USB 序列号。

use std::{
    fmt,
    io::Read,
    path::Path,
    process::{Command, ExitStatus, Stdio},
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

/// ODrive USB 只读调用失败；`detail` 保留桥接异常、进程退出状态与结果标记。
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
        .arg(format!(
            "{}.{:09}",
            timeout.as_secs(),
            timeout.subsec_nanos()
        ))
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
        detail: Some(bridge_diagnostic(&status, None, &stderr, None)),
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
        .map(str::to_owned);
    let detail = Some(bridge_diagnostic(
        &status,
        Some(&response),
        &stderr,
        detail.as_deref(),
    ));
    Err(Error {
        kind,
        message,
        detail,
    })
}

fn bridge_diagnostic(
    status: &ExitStatus,
    response: Option<&Value>,
    stderr: &str,
    bridge_detail: Option<&str>,
) -> String {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status
            .signal()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".into())
    };
    #[cfg(not(unix))]
    let signal = "unavailable";
    let code = status
        .code()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".into());
    let ok = response
        .and_then(|value| value.get("ok"))
        .and_then(Value::as_bool)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "missing_or_invalid".into());
    let mut detail =
        format!("status={status}; exit_code={code}; signal={signal}; response.ok={ok}");
    if let Some(original) = bridge_detail {
        detail.push_str(&format!("; bridge_detail={original}"));
    }
    if !stderr.is_empty() {
        detail.push_str(&format!("; stderr={stderr}"));
    }
    detail
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
    fn abnormal_exit_never_accepts_success_payload_and_keeps_status() {
        let success = run_bridge_script(
            Path::new("/bin/sh"),
            "printf '{\"ok\":true}'",
            "unused",
            "unused",
            Duration::from_secs(1),
        )
        .expect("零退出成功载荷仍可采用");
        assert_eq!(success["ok"], true);
        for (script, expected) in [
            ("printf '{\"ok\":true}'; exit 7", "exit_code=7"),
            ("printf '{\"ok\":true}'; kill -TERM $$", "signal=15"),
            ("exit 7", "exit_code=7"),
        ] {
            let error = run_bridge_script(
                Path::new("/bin/sh"),
                script,
                "unused",
                "unused",
                Duration::from_secs(1),
            )
            .expect_err("非零退出不能采纳成功载荷");
            assert_eq!(error.kind, ErrorKind::Bridge);
            let detail = error.detail.expect("退出诊断不可为空");
            assert!(detail.contains(expected), "{detail}");
            if script.starts_with("printf") {
                assert!(detail.contains("response.ok=true"), "{detail}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn structured_failure_keeps_category_detail_stderr_and_exit_status() {
        for code in [0, 1] {
            let script = format!(
                "printf '%s' '{{\"ok\":false,\"error\":{{\"kind\":\"dependency\",\"message\":\"missing dependency\",\"detail\":\"original detail\"}}}}'; printf 'original stderr' >&2; exit {code}"
            );
            let error = run_bridge_script(
                Path::new("/bin/sh"),
                &script,
                "unused",
                "unused",
                Duration::from_secs(1),
            )
            .expect_err("失败载荷不能采用");
            assert_eq!(error.kind, ErrorKind::Dependency);
            assert_eq!(error.message, "missing dependency");
            let detail = error.detail.expect("原始诊断");
            for expected in [
                "original detail",
                "original stderr",
                "response.ok=false",
                &format!("exit_code={code}"),
            ] {
                assert!(detail.contains(expected), "{detail}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn one_shot_entry_flushes_streams_preserves_code_and_skips_atexit() {
        let encoded = serde_json::to_string(BRIDGE).expect("脚本文本");
        for code in [0, 7] {
            let script = format!(
                "namespace={{'__name__':'test_bridge'}}\nexec({encoded}, namespace)\nimport atexit,sys\natexit.register(lambda: sys.stderr.write('atexit-ran'))\ndef main(argv):\n sys.stdout.write('{{\"ok\":true}}')\n sys.stderr.write('flushed-stderr')\n return {code}\nnamespace['main']=main\nnamespace['run_once']()\n"
            );
            let output = Command::new("python3")
                .arg("-c")
                .arg(script)
                .output()
                .expect("合成 Python 入口");
            assert_eq!(output.status.code(), Some(code));
            assert_eq!(output.stdout, br#"{"ok":true}"#);
            assert_eq!(output.stderr, b"flushed-stderr");
        }
    }

    #[cfg(unix)]
    #[test]
    fn one_shot_entry_main_or_flush_failure_is_nonzero() {
        let encoded = serde_json::to_string(BRIDGE).expect("脚本文本");
        for setup in [
            "def main(argv):\n raise RuntimeError('main failed')\nnamespace['main']=main",
            "class BadFlush:\n def write(self, value): return len(value)\n def flush(self): raise RuntimeError('flush failed')\nsys.stdout=BadFlush()\nnamespace['main']=lambda argv:0",
        ] {
            let script = format!(
                "namespace={{'__name__':'test_bridge'}}\nexec({encoded}, namespace)\nimport atexit,sys\natexit.register(lambda: sys.stderr.write('atexit-ran'))\n{setup}\nnamespace['run_once']()\n"
            );
            let output = Command::new("python3")
                .arg("-c")
                .arg(script)
                .output()
                .expect("合成失败入口");
            assert_eq!(output.status.code(), Some(1));
            assert!(!String::from_utf8_lossy(&output.stderr).contains("atexit-ran"));
        }
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
