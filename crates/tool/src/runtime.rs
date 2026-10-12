// Copyright The eha-sdk Contributors

//! `eha-tool` 随发行目录携带的 Python CAN 运行时定位。
//!
//! 正式发行目录的根包含工具和 `runtime/python`。解析时只接受该位置的解释器，避免把
//! 损坏的发行目录悄悄切换到调用者机器上的 Python。源码构建保留一个受环境变量明确
//! 启用的开发入口，方便维护者继续使用自己的 Python 环境。

use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const DEVELOPER_PYTHON: &str = "EHA_TOOL_DEVELOPER_PYTHON";
const HEALTH_CHECK: &str =
    "import can, serial; assert can.__version__ == '4.6.1'; assert serial.VERSION == '3.5'";
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    Bundled(PathBuf),
    Developer(String),
}

/// 返回供 `python-can` 子进程使用的解释器。
///
/// 显式参数永远优先，用于维护者选择自己的完整 Python 环境。未指定时，正式发行目录
/// 只能使用与 `eha-tool` 并列的受管运行时；检测到不完整运行时会直接报错。源码树中
/// 可设置 `EHA_TOOL_DEVELOPER_PYTHON`，此入口不适用于正式发行包。
pub(crate) fn python(explicit: Option<&str>) -> Result<String, String> {
    let executable = current_executable()?;
    match resolve(
        &executable,
        explicit,
        env::var(DEVELOPER_PYTHON).ok().as_deref(),
    )? {
        Resolution::Bundled(path) => {
            verify(&path)?;
            Ok(path.to_string_lossy().into_owned())
        }
        Resolution::Developer(path) => Ok(path),
    }
}

/// 返回当前可见运行时的诊断信息，供状态输出使用。
///
/// 此函数不回退到系统 Python；`healthy` 仅在已经找到随包解释器并可在隔离模式下导入
/// 锁定依赖时为真。
pub(crate) fn status() -> Value {
    let executable = match current_executable() {
        Ok(path) => path,
        Err(error) => return json!({"mode": "unknown", "healthy": false, "error": error}),
    };
    match resolve(
        &executable,
        None,
        env::var(DEVELOPER_PYTHON).ok().as_deref(),
    ) {
        Ok(Resolution::Bundled(path)) => match verify(&path) {
            Ok(()) => json!({
                "mode": "bundled",
                "python": path,
                "healthy": true,
            }),
            Err(error) => json!({
                "mode": "bundled",
                "python": path,
                "healthy": false,
                "error": error,
            }),
        },
        Ok(Resolution::Developer(path)) => json!({
            "mode": "developer",
            "python": path,
            "healthy": null,
        }),
        Err(error) => json!({"mode": "missing", "healthy": false, "error": error}),
    }
}

fn current_executable() -> Result<PathBuf, String> {
    let path =
        env::current_exe().map_err(|error| format!("无法定位 eha-tool 自身路径: {error}"))?;
    fs::canonicalize(path).map_err(|error| format!("无法解析 eha-tool 实际路径: {error}"))
}

fn resolve(
    executable: &Path,
    explicit: Option<&str>,
    developer_python: Option<&str>,
) -> Result<Resolution, String> {
    if let Some(value) = explicit {
        if value.is_empty() {
            return Err("--can-python 不能为空".into());
        }
        return Ok(Resolution::Developer(value.into()));
    }

    let root = executable
        .parent()
        .ok_or_else(|| "eha-tool 实际路径没有发行目录".to_owned())?;
    let runtime = root.join("runtime");
    if runtime.exists() {
        let path = bundled_python(&runtime);
        if !path.is_file() {
            return Err(format!(
                "随 eha-tool 提供的 Python 运行时不完整：缺少 {}；请重新取得完整发行目录，或显式传 --can-python",
                path.display()
            ));
        }
        return Ok(Resolution::Bundled(path));
    }

    if is_developer_tree(executable) {
        if let Some(path) = developer_python.filter(|path| !path.is_empty()) {
            return Ok(Resolution::Developer(path.into()));
        }
        return Err(format!(
            "源码构建未配置 Python；设置 {DEVELOPER_PYTHON}，或显式传 --can-python"
        ));
    }

    Err("未找到随 eha-tool 提供的 Python 运行时；正式发行目录必须包含 runtime/python，或显式传 --can-python".into())
}

fn bundled_python(runtime: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        runtime.join("python").join("python.exe")
    }
    #[cfg(not(windows))]
    {
        runtime.join("python").join("bin").join("python3")
    }
}

fn is_developer_tree(executable: &Path) -> bool {
    executable.ancestors().any(|path| {
        path.join("Cargo.toml").is_file() && path.join("crates/tool/Cargo.toml").is_file()
    })
}

fn verify(python: &Path) -> Result<(), String> {
    let mut child = Command::new(python)
        .args(["-I", "-c", HEALTH_CHECK])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动随包 Python {}: {error}", python.display()))?;
    let deadline = Instant::now() + HEALTH_CHECK_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "随包 Python 运行时健康检查在 {} 秒内没有完成；请重新取得完整发行目录，或显式传 --can-python",
                    HEALTH_CHECK_TIMEOUT.as_secs()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "无法等待随包 Python 运行时健康检查: {error}；请重新取得完整发行目录，或显式传 --can-python"
                ));
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("无法读取随包 Python 健康检查结果: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let diagnostic = if diagnostic.is_empty() {
        format!("退出状态 {}", output.status)
    } else {
        diagnostic
    };
    Err(format!(
        "随包 Python 运行时缺少或不匹配锁定的 python-can 依赖：{diagnostic}；请重新取得完整发行目录，或显式传 --can-python"
    ))
}

#[cfg(test)]
mod tests {
    use super::{Resolution, bundled_python, resolve};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("eha-tool-runtime-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).expect("temp directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn explicit_python_has_priority() {
        let temporary = TempDir::new();
        let executable = temporary.0.join("eha-tool");
        assert_eq!(
            resolve(&executable, Some("/opt/python"), None),
            Ok(Resolution::Developer("/opt/python".into()))
        );
    }

    #[test]
    fn bundle_runtime_is_selected_from_actual_distribution_root() {
        let temporary = TempDir::new();
        let executable = temporary.0.join("eha-tool");
        let python = bundled_python(&temporary.0.join("runtime"));
        fs::create_dir_all(python.parent().expect("python parent")).expect("runtime directory");
        fs::write(&python, []).expect("python placeholder");
        assert_eq!(
            resolve(&executable, None, None),
            Ok(Resolution::Bundled(python))
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_symlink_uses_the_actual_distribution_root() {
        use std::os::unix::fs::symlink;

        let distribution = TempDir::new();
        let launcher = TempDir::new();
        let executable = distribution.0.join("eha-tool");
        fs::write(&executable, []).expect("tool placeholder");
        let python = bundled_python(&distribution.0.join("runtime"));
        fs::create_dir_all(python.parent().expect("python parent")).expect("runtime directory");
        fs::write(&python, []).expect("python placeholder");
        let link = launcher.0.join("eha-tool");
        symlink(&executable, &link).expect("launcher symlink");
        assert_eq!(
            resolve(
                &fs::canonicalize(link).expect("canonical executable"),
                None,
                None
            ),
            Ok(Resolution::Bundled(python))
        );
    }

    #[test]
    fn incomplete_bundle_cannot_fall_back_to_developer_python() {
        let temporary = TempDir::new();
        fs::create_dir_all(temporary.0.join("runtime")).expect("runtime directory");
        let error = resolve(
            &temporary.0.join("eha-tool"),
            None,
            Some("developer-python"),
        )
        .expect_err("incomplete bundle must be rejected");
        assert!(error.contains("不完整"));
    }

    #[test]
    fn developer_fallback_requires_source_tree_and_explicit_environment() {
        let temporary = TempDir::new();
        fs::create_dir_all(temporary.0.join("crates/tool")).expect("source layout");
        fs::write(temporary.0.join("Cargo.toml"), []).expect("workspace manifest");
        fs::write(temporary.0.join("crates/tool/Cargo.toml"), []).expect("tool manifest");
        let executable = temporary.0.join("target/release/eha-tool");
        assert_eq!(
            resolve(&executable, None, Some("developer-python")),
            Ok(Resolution::Developer("developer-python".into()))
        );
        assert!(resolve(&executable, None, None).is_err());
    }
}
