// Copyright The eha-sdk Contributors

//! 本机受管 CANable2 连接档案。
//!
//! 档案只保存稳定 USB 序列号，绝不保存会在拔插后变化的 `/dev/cu.*` 路径。每次
//! `ManagedCanFactory::open` 都重新经 pyserial 解析该序列号，再把完整、显式的
//! python-can 配置交给 SDK；这里不实现 SLCAN 协议，也不触碰用户的 `can.conf`。

use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use crate::owner_lock::{OwnerGuard, ResourceKey};
use eha_sdk::can::{CanChannel, CanChannelFactory, python::PythonCanOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// 只解析配置与枚举，不创建 Bus。冻结的配置随后以 ignore_config=True 打开。
const ADVANCED_CONFIG_SCRIPT: &str = r#"
import json, sys, can
context = sys.argv[1]
selected = dict(can.util.load_file_config(section=context) or {})
selected.update(can.util.load_environment_config(context) or {})
if "interface" not in selected and "bustype" in selected:
    selected["interface"] = selected.pop("bustype")
interface, channel = selected.get("interface"), selected.get("channel")
if not isinstance(interface, str) or not interface.strip() or isinstance(channel, bool) or not isinstance(channel, (str, int)) or (isinstance(channel, str) and not channel.strip()):
    raise ValueError("命名 python-can 上下文必须明确选择 interface 和单一 channel")
config = can.util.load_config(config=selected, context=context)
def encode(value):
    if isinstance(value, can.BusState):
        return value.name
    if isinstance(value, can.BitTimingFd):
        return dict(value, _eha_timing_kind="fd")
    if isinstance(value, can.BitTiming):
        return dict(value, _eha_timing_kind="classic")
    raise TypeError("高级 CAN 配置类型无法冻结: " + type(value).__name__)
encoded = json.dumps(config, default=encode, allow_nan=False, separators=(",", ":"))
if len(encoded) > 32768:
    raise ValueError("高级 CAN 配置超过 32768 字节")
print(encoded)
"#;

const DISCOVERY_SCRIPT: &str = r#"
import json
from serial.tools import list_ports
rows=[]
for port in list_ports.comports():
    description = port.description or ""
    if port.vid == 0x16d0 and port.pid == 0x117e and "canable2" in description.lower() and port.serial_number:
        rows.append({"kind":"canable2","serial":port.serial_number,"path":port.device,"description":description,"vid":port.vid,"pid":port.pid})
print(json.dumps(rows, separators=(",", ":")))
"#;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(crate) struct ManagedAdapter {
    pub(crate) kind: String,
    pub(crate) serial: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(crate) struct ConnectionProfile {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) adapter: ManagedAdapter,
    pub(crate) node: u8,
    pub(crate) mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expected_uid: Option<String>,
    #[serde(default)]
    pub(crate) autoconnect: bool,
    #[serde(default)]
    pub(crate) default: bool,
}

impl ConnectionProfile {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) || self.name.trim().is_empty() {
            return Err("连接档案 id 必须为 1..=64 个字母、数字、_ 或 -，name 不能为空".into());
        }
        if self.adapter.kind != "canable2" || self.adapter.serial.trim().is_empty() {
            return Err("当前只支持带完整序列号的 canable2 适配器".into());
        }
        if self.node > 127 || self.mode != "fd" {
            return Err("CANable2 受管连接只支持节点 0..=127 和 fd 模式".into());
        }
        if let Some(uid) = &self.expected_uid
            && !valid_uid(uid)
        {
            return Err("expected_uid 必须是 24 个十六进制字符的 EHA UID".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SavedProfiles {
    version: u8,
    profiles: Vec<ConnectionProfile>,
}

/// 进程内的档案表。写前若读取到损坏文件会拒绝覆盖，避免把用户原始档案静默抹掉。
pub(crate) struct ConnectionStore {
    path: PathBuf,
    profiles: BTreeMap<String, ConnectionProfile>,
    load_error: Option<String>,
    autoconnect: Value,
}

impl ConnectionStore {
    pub(crate) fn load_default() -> Self {
        Self::load(default_path())
    }

    pub(crate) fn load(path: PathBuf) -> Self {
        let mut result = Self {
            path,
            profiles: BTreeMap::new(),
            load_error: None,
            autoconnect: json!({"attempted":false,"status":"idle","error":null}),
        };
        match fs::read_to_string(&result.path) {
            Ok(text) => match serde_json::from_str::<SavedProfiles>(&text) {
                Ok(saved) if saved.version == 1 => {
                    for profile in saved.profiles {
                        match profile.validate() {
                            Ok(())
                                if result
                                    .profiles
                                    .insert(profile.id.clone(), profile)
                                    .is_none() => {}
                            Ok(()) => result.load_error = Some("连接档案包含重复 id".into()),
                            Err(error) => {
                                result.load_error = Some(format!("连接档案无效：{error}"))
                            }
                        }
                    }
                    if result.load_error.is_none() && !profiles_consistent(&result.profiles) {
                        result.load_error = Some(
                            "连接档案的 default/autoconnect 选择不唯一或自动连接未绑定 UID".into(),
                        );
                    }
                }
                Ok(_) => result.load_error = Some("不支持的连接档案版本".into()),
                Err(error) => result.load_error = Some(format!("无法读取连接档案：{error}")),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => result.load_error = Some(format!("无法读取连接档案：{error}")),
        }
        result
    }

    pub(crate) fn response(&self) -> Value {
        json!({
            "ok": self.load_error.is_none(),
            "profiles": self.profiles.values().collect::<Vec<_>>(),
            "path": self.path,
            "load_error": self.load_error,
            "autoconnect": self.autoconnect,
        })
    }

    pub(crate) fn profile(&self, id: &str) -> Result<ConnectionProfile, String> {
        self.writable()?;
        self.profiles
            .get(id)
            .cloned()
            .ok_or_else(|| format!("找不到连接档案：{id}"))
    }

    /// 用于保存前检查同名档案的身份绑定；读取损坏档案时不允许把它当成不存在。
    pub(crate) fn profile_optional(&self, id: &str) -> Result<Option<ConnectionProfile>, String> {
        self.writable()?;
        Ok(self.profiles.get(id).cloned())
    }

    pub(crate) fn save(
        &mut self,
        mut profile: ConnectionProfile,
    ) -> Result<ConnectionProfile, String> {
        self.writable()?;
        if profile.expected_uid.is_none() {
            profile.expected_uid = self
                .profiles
                .get(&profile.id)
                .and_then(|old| old.expected_uid.clone());
        }
        profile.validate()?;
        if let Some(existing) = self.profiles.get(&profile.id)
            && existing.expected_uid.is_some()
            && profile.expected_uid.is_some()
            && existing.expected_uid != profile.expected_uid
        {
            return Err("同名档案已绑定另一 EHA UID；请使用新的 id，不能静默覆盖".into());
        }
        let mut candidate = self.profiles.clone();
        if profile.default || profile.autoconnect {
            for current in candidate.values_mut() {
                current.default = false;
                current.autoconnect = false;
            }
        }
        candidate.insert(profile.id.clone(), profile.clone());
        if !profiles_consistent(&candidate) {
            return Err("默认或自动连接档案不唯一，且自动连接必须已绑定 EHA UID".into());
        }
        self.persist(&candidate)?;
        self.profiles = candidate;
        Ok(profile)
    }

    pub(crate) fn delete(&mut self, id: &str) -> Result<(), String> {
        self.writable()?;
        let mut candidate = self.profiles.clone();
        candidate
            .remove(id)
            .ok_or_else(|| format!("找不到连接档案：{id}"))?;
        self.persist(&candidate)?;
        self.profiles = candidate;
        Ok(())
    }

    pub(crate) fn bind_identity(&mut self, id: &str, uid: &str) -> Result<(), String> {
        self.writable()?;
        if !valid_uid(uid) {
            return Err("Identity 未提供有效 EHA UID，不能绑定连接档案".into());
        }
        let mut candidate = self.profiles.clone();
        let profile = candidate
            .get_mut(id)
            .ok_or_else(|| format!("找不到连接档案：{id}"))?;
        if profile
            .expected_uid
            .as_deref()
            .is_some_and(|old| !old.eq_ignore_ascii_case(uid))
        {
            return Err("档案已绑定另一 EHA UID；拒绝静默覆盖".into());
        }
        profile.expected_uid = Some(uid.to_ascii_lowercase());
        self.persist(&candidate)?;
        self.profiles = candidate;
        Ok(())
    }

    pub(crate) fn autoconnect_profile(&self) -> Option<ConnectionProfile> {
        self.load_error
            .is_none()
            .then(|| {
                self.profiles
                    .values()
                    .find(|profile| profile.autoconnect && profile.expected_uid.is_some())
                    .cloned()
            })
            .flatten()
    }

    pub(crate) fn autoconnect_result(&mut self, result: Result<(), String>) {
        self.autoconnect = match result {
            Ok(()) => json!({"attempted":true,"status":"connected","error":null}),
            Err(message) => {
                json!({"attempted":true,"status":"failed","error":{"code":"autoconnect_failed","message":message}})
            }
        };
    }

    fn writable(&self) -> Result<(), String> {
        self.load_error
            .as_ref()
            .map(|error| Err(format!("连接档案未写入：{error}")))
            .unwrap_or(Ok(()))
    }

    fn persist(&self, profiles: &BTreeMap<String, ConnectionProfile>) -> Result<(), String> {
        let parent = self.path.parent().ok_or("连接档案路径没有父目录")?;
        fs::create_dir_all(parent).map_err(|error| format!("无法创建连接档案目录：{error}"))?;
        // 文件锁随句柄关闭释放，不留下需要猜测进程是否存活的锁文件状态。
        // 锁内重读并拒绝旧快照，防止 CLI 和 WebUI 的全量写入覆盖彼此的新修改。
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .map_err(|error| format!("无法打开连接档案锁：{error}"))?;
        lock.try_lock()
            .map_err(|error| format!("另一实例正在修改连接档案，请稍后重试：{error}"))?;
        let current = Self::load(self.path.clone());
        current.writable()?;
        if current.profiles != self.profiles {
            return Err("连接档案已被另一实例修改；请重启当前 Shell 或 WebUI 后再保存".into());
        }
        let serialized = serde_json::to_vec_pretty(&SavedProfiles {
            version: 1,
            profiles: profiles.values().cloned().collect(),
        })
        .map_err(|error| error.to_string())?;
        let temporary = parent.join(format!(".connections-{}.tmp", std::process::id()));
        fs::write(&temporary, serialized)
            .map_err(|error| format!("无法写入临时连接档案：{error}"))?;
        fs::rename(&temporary, &self.path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            format!("无法原子替换连接档案：{error}")
        })
    }
}

fn profiles_consistent(profiles: &BTreeMap<String, ConnectionProfile>) -> bool {
    profiles.values().filter(|profile| profile.default).count() <= 1
        && profiles
            .values()
            .filter(|profile| profile.autoconnect)
            .count()
            <= 1
        && profiles
            .values()
            .filter(|profile| profile.autoconnect)
            .all(|profile| profile.expected_uid.is_some())
}

pub(crate) fn adapters(python: &str) -> Result<Vec<Value>, String> {
    let mut child = Command::new(python)
        .arg("-I")
        .arg("-c")
        .arg(DISCOVERY_SCRIPT)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动 pyserial 发现：{error}"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("pyserial 发现超过 2 秒；适配器枚举未完成".into());
            }
            Err(error) => return Err(format!("无法等待 pyserial 发现：{error}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("无法读取 pyserial 发现结果：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "pyserial 发现失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| format!("pyserial 输出无效：{error}"))
}

pub(crate) fn managed_factory(serial: String, python: String) -> Arc<dyn CanChannelFactory> {
    Arc::new(LockedCanFactory {
        // serial 在真正 open 前验证；调用者不能通过换 node/mode/python 取得第二把锁。
        key: ResourceKey::managed_can(&serial, "0").map_err(|error| error.to_string()),
        inner: Arc::new(ManagedCanFactory { serial, python }),
        slcan: None,
    })
}

/// 命名上下文只是配置入口；owner 绑定官方加载器解析出的实际通路。
pub(crate) fn advanced_factory(
    context: String,
    python: Option<String>,
) -> Result<Arc<dyn CanChannelFactory>, String> {
    let python = crate::runtime::python(python.as_deref())?;
    let mut config = advanced_config(&python, &context)?;
    freeze_slcan_path(&mut config)?;
    let rows = if config["interface"].as_str() == Some("slcan") {
        adapters(&python)?
    } else {
        Vec::new()
    };
    let key = advanced_key(&config, &rows)?;
    let slcan = (config["interface"].as_str() == Some("slcan")).then(|| SlcanBinding {
        python: python.clone(),
        config: config.clone(),
    });
    Ok(Arc::new(LockedCanFactory {
        key: Ok(key),
        inner: Arc::new(PythonCanOptions::with_explicit_config(python, config)),
        slcan,
    }))
}

fn freeze_slcan_path(config: &mut Value) -> Result<(), String> {
    if config["interface"].as_str() != Some("slcan") {
        return Ok(());
    }
    let channel = config["channel"]
        .as_str()
        .ok_or("SLCAN channel 必须是字符串")?;
    let (path, baud) = channel
        .rsplit_once('@')
        .map_or((channel, None), |(path, baud)| (path, Some(baud)));
    if Path::new(path).is_absolute() || Path::new(path).exists() {
        let canonical =
            fs::canonicalize(path).map_err(|error| format!("无法冻结 SLCAN 物理路径：{error}"))?;
        let canonical = canonical.to_str().ok_or("SLCAN 物理路径必须是 UTF-8")?;
        config["channel"] = json!(match baud {
            Some(baud) => format!("{canonical}@{baud}"),
            None => canonical.to_owned(),
        });
    }
    Ok(())
}

fn advanced_config(python: &str, context: &str) -> Result<Value, String> {
    if context.trim().is_empty() {
        return Err("python-can 配置 context 不能为空".into());
    }
    let mut command = Command::new(python);
    command.args(["-I", "-c", ADVANCED_CONFIG_SCRIPT, context]);
    load_advanced_config(command)
}

fn load_advanced_config(mut command: Command) -> Result<Value, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动 python-can 配置解析：{error}"))?;
    // 等待进程时同步排空管道，长但合法的配置不会堵在 pipe 容量上。
    let stdout = child.stdout.take().ok_or("配置解析没有 stdout")?;
    let stderr = child.stderr.take().ok_or("配置解析没有 stderr")?;
    let drain = |mut stream: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).map(|_| bytes)
        })
    };
    let stdout = drain(Box::new(stdout));
    let stderr = drain(Box::new(stderr));
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("python-can 配置解析超过 2 秒".into());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("无法等待 python-can 配置解析：{error}"));
            }
        }
    };
    let stdout = stdout
        .join()
        .map_err(|_| "配置 stdout worker 失败")?
        .map_err(|error| error.to_string())?;
    let stderr = stderr
        .join()
        .map_err(|_| "配置 stderr worker 失败")?
        .map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!(
            "python-can 配置解析失败：{}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    serde_json::from_slice(&stdout).map_err(|error| format!("python-can 配置无法冻结：{error}"))
}

fn advanced_key(config: &Value, rows: &[Value]) -> Result<ResourceKey, String> {
    let interface = config["interface"]
        .as_str()
        .ok_or("高级 CAN 缺少 interface")?;
    let channel = match &config["channel"] {
        Value::String(channel) => channel.clone(),
        Value::Number(channel) if channel.is_i64() || channel.is_u64() => channel.to_string(),
        _ => return Err("高级 CAN 物理锁仅支持明确的单一字符串或整数 channel".into()),
    };
    let physical = if interface == "slcan" {
        // python-can SLCAN 的 @baudrate 是串口参数，不是另一物理资源。
        channel
            .rsplit_once('@')
            .map_or(channel.as_str(), |(path, _)| path)
    } else {
        &channel
    };
    if interface == "slcan" {
        let matches: Vec<_> = rows
            .iter()
            .filter(|row| {
                row["path"]
                    .as_str()
                    .is_some_and(|path| same_serial_path(path, physical))
            })
            .collect();
        if matches.len() > 1 {
            return Err("高级 SLCAN 路径匹配多个稳定适配器身份；拒绝打开".into());
        }
        if let Some(row) = matches.first() {
            let serial = row["serial"]
                .as_str()
                .ok_or("CANable2 发现缺少稳定序列号")?;
            return ResourceKey::managed_can(serial, "0").map_err(|error| error.to_string());
        }
    }
    ResourceKey::advanced_can(interface, physical).map_err(|error| error.to_string())
}

fn same_serial_path(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    if let (Ok(left), Ok(right)) = (fs::canonicalize(left), fs::canonicalize(right))
        && left == right
    {
        return true;
    }
    // macOS callout 与 dial-in 名称代表同一个已发现的串口，不依赖动态路径作 key。
    #[cfg(target_os = "macos")]
    {
        fn suffix(path: &str) -> Option<&str> {
            path.strip_prefix("/dev/cu.")
                .or_else(|| path.strip_prefix("/dev/tty."))
        }
        if let (Some(left), Some(right)) = (suffix(left), suffix(right)) {
            return left == right;
        }
    }
    false
}

struct LockedCanFactory {
    key: Result<ResourceKey, String>,
    inner: Arc<dyn CanChannelFactory>,
    slcan: Option<SlcanBinding>,
}

struct SlcanBinding {
    python: String,
    config: Value,
}

impl SlcanBinding {
    fn validate(&self, key: &ResourceKey) -> Result<(), String> {
        self.validate_rows(key, &adapters(&self.python)?)
    }

    fn validate_rows(&self, key: &ResourceKey, rows: &[Value]) -> Result<(), String> {
        let mut current_config = self.config.clone();
        freeze_slcan_path(&mut current_config)
            .map_err(|error| format!("owner_lock_invalid: {error}"))?;
        let current = advanced_key(&current_config, rows)?;
        if &current != key {
            return Err("owner_lock_invalid: SLCAN 冻结路径的物理身份已改变或稳定适配器已消失；拒绝打开，请显式重新选择连接".into());
        }
        Ok(())
    }
}

/// SDK 的外部 channel 错误仍是字符串；只识别已知 owner 标记，保留结构化错误码。
pub(crate) fn owner_error_code(message: &str) -> Option<&'static str> {
    [
        "owner_busy",
        "owner_lock_unsafe",
        "owner_lock_invalid",
        "owner_lock_io",
        "owner_lock_unsupported",
    ]
    .into_iter()
    .find(|code| message.split_once(&format!("{code}: ")).is_some())
}

impl CanChannelFactory for LockedCanFactory {
    fn open(&self) -> Result<Box<dyn CanChannel>, String> {
        let key = self.key.as_ref().map_err(Clone::clone)?;
        let owner = OwnerGuard::acquire(key).map_err(|error| error.to_string())?;
        if let Some(binding) = &self.slcan {
            binding.validate(key)?;
        }
        let channel = self.inner.open()?;
        Ok(Box::new(LockedCanChannel {
            channel,
            _owner: owner,
        }))
    }
}

/// 字段按声明顺序 drop：先结束外部驱动/桥接进程，再释放物理通路 owner。
struct LockedCanChannel {
    channel: Box<dyn CanChannel>,
    _owner: OwnerGuard,
}

impl CanChannel for LockedCanChannel {
    fn send(
        &mut self,
        frame: &eha_sdk::transport::can::Frame,
        timeout: Duration,
    ) -> Result<(), String> {
        self.channel.send(frame, timeout)
    }
    fn receive(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<eha_sdk::transport::can::Frame>, String> {
        self.channel.receive(timeout)
    }
}

struct ManagedCanFactory {
    serial: String,
    python: String,
}

impl CanChannelFactory for ManagedCanFactory {
    fn open(&self) -> Result<Box<dyn CanChannel>, String> {
        let matches: Vec<_> = adapters(&self.python)?
            .into_iter()
            .filter(|row| row["serial"].as_str() == Some(&self.serial))
            .collect();
        let row = match matches.len() {
            0 => {
                return Err(format!(
                    "未找到 CANable2 序列号 {}；请检查连接后显式恢复通信",
                    self.serial
                ));
            }
            1 => &matches[0],
            _ => {
                return Err(format!(
                    "CANable2 序列号 {} 匹配多个串口；拒绝选择任意路径",
                    self.serial
                ));
            }
        };
        let path = row["path"]
            .as_str()
            .ok_or("pyserial 返回的 CANable2 路径无效")?;
        let config = json!({
            "interface": "slcan",
            "channel": path,
            "fd": true,
            "bitrate": 1_000_000,
            "data_bitrate": 2_000_000,
            "timing": {
                "f_clock": 80_000_000,
                "nom_brp": 1, "nom_tseg1": 63, "nom_tseg2": 16, "nom_sjw": 16,
                "data_brp": 1, "data_tseg1": 31, "data_tseg2": 8, "data_sjw": 8
            }
        });
        PythonCanOptions::with_explicit_config(self.python.clone(), config).open()
    }
}

pub(crate) fn expected_uid_matches(expected: Option<&str>, identity: &Value) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let uid = identity["uid"]
        .as_str()
        .ok_or("Identity 回复没有 EHA UID")?;
    if expected.eq_ignore_ascii_case(uid) {
        Ok(())
    } else {
        Err(format!(
            "Identity UID {uid} 与连接档案期望 {expected} 不一致；已拒绝控制"
        ))
    }
}

pub(crate) fn default_path() -> PathBuf {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path)
            .join("eha-tool")
            .join("connections.json");
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("eha-tool")
            .join("connections.json");
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(".").to_path_buf())
        .join(".config")
        .join("eha-tool")
        .join("connections.json")
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
fn valid_uid(value: &str) -> bool {
    value.len() == 24 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn lock_serial(label: &str) -> String {
        format!("eha-integration-{}-{label}", std::process::id())
    }

    struct SyntheticChannel {
        serial: String,
        closed: Arc<AtomicUsize>,
        fail_receive: bool,
    }
    impl CanChannel for SyntheticChannel {
        fn send(&mut self, _: &eha_sdk::transport::can::Frame, _: Duration) -> Result<(), String> {
            Ok(())
        }
        fn receive(
            &mut self,
            _: Duration,
        ) -> Result<Option<eha_sdk::transport::can::Frame>, String> {
            if self.fail_receive {
                return Err("合成物理接收失败".into());
            }
            thread::sleep(Duration::from_millis(1));
            Ok(None)
        }
    }
    impl Drop for SyntheticChannel {
        fn drop(&mut self) {
            let key = ResourceKey::managed_can(&self.serial, "0").expect("key");
            assert_eq!(
                OwnerGuard::acquire(&key)
                    .expect_err("驱动 drop 期间仍持锁")
                    .code,
                "owner_busy"
            );
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct SyntheticFactory {
        serial: String,
        opened: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
        fail: bool,
        first_channel_fails: bool,
    }
    impl CanChannelFactory for SyntheticFactory {
        fn open(&self) -> Result<Box<dyn CanChannel>, String> {
            let opened = self.opened.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err("合成 open 失败".into());
            }
            Ok(Box::new(SyntheticChannel {
                serial: self.serial.clone(),
                closed: Arc::clone(&self.closed),
                fail_receive: self.first_channel_fails && opened == 0,
            }))
        }
    }
    fn synthetic_factory(
        label: &str,
        fail: bool,
    ) -> (
        Arc<dyn CanChannelFactory>,
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let serial = lock_serial(label);
        let opened = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(SyntheticFactory {
            serial: serial.clone(),
            opened: Arc::clone(&opened),
            closed: Arc::clone(&closed),
            fail,
            first_channel_fails: false,
        });
        (
            Arc::new(LockedCanFactory {
                key: ResourceKey::managed_can(&serial, "0").map_err(|error| error.to_string()),
                inner,
                slcan: None,
            }),
            serial,
            opened,
            closed,
        )
    }

    #[test]
    fn physical_factory_locks_before_open_and_releases_after_driver_close() {
        let (factory, _, opened, closed) = synthetic_factory("factory", false);
        let channel = factory.open().expect("首次打开");
        assert!(
            factory
                .open()
                .err()
                .expect("第二次必须失败")
                .contains("owner_busy")
        );
        assert_eq!(opened.load(Ordering::SeqCst), 1, "竞争失败不能调用外部驱动");
        drop(channel);
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        drop(factory.open().expect("释放后重新打开"));
        assert_eq!(closed.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn physical_factory_open_failure_does_not_leave_owner_busy() {
        let (factory, serial, opened, _) = synthetic_factory("failed", true);
        assert!(
            factory
                .open()
                .err()
                .expect("合成失败")
                .contains("合成 open 失败")
        );
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        let key = ResourceKey::managed_can(&serial, "0").expect("key");
        drop(OwnerGuard::acquire(&key).expect("失败后释放 owner"));
    }

    #[cfg(unix)]
    struct SlcanFixture {
        root: PathBuf,
        config: Value,
        serial: String,
        opened: Arc<AtomicUsize>,
        factory: Arc<dyn CanChannelFactory>,
    }

    #[cfg(unix)]
    impl SlcanFixture {
        fn new(label: &str, first_channel_fails: bool) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root = std::env::temp_dir().join(lock_serial(label));
            fs::create_dir(&root).expect("合成 SLCAN 目录");
            let root = root.canonicalize().expect("实路径");
            let path = root.join("port");
            fs::write(&path, "synthetic").expect("仅合成文件");
            let serial = lock_serial(label);
            let python = root.join("discovery.py");
            fs::write(&python, "#!/usr/bin/env python3\nimport pathlib, sys\nsys.stdout.write((pathlib.Path(__file__).parent / 'rows.json').read_text())\n").expect("合成枚举解释器");
            fs::set_permissions(&python, fs::Permissions::from_mode(0o700))
                .expect("可执行枚举fixture");
            let config = json!({"interface":"slcan", "channel":path});
            let rows = vec![json!({"serial":serial,"path":path})];
            fs::write(
                root.join("rows.json"),
                serde_json::to_vec(&rows).expect("rows"),
            )
            .expect("枚举A");
            let opened = Arc::new(AtomicUsize::new(0));
            let factory: Arc<dyn CanChannelFactory> = Arc::new(LockedCanFactory {
                key: advanced_key(&config, &rows),
                inner: Arc::new(SyntheticFactory {
                    serial: serial.clone(),
                    opened: Arc::clone(&opened),
                    closed: Arc::new(AtomicUsize::new(0)),
                    fail: false,
                    first_channel_fails,
                }),
                slcan: Some(SlcanBinding {
                    python: python.to_string_lossy().into_owned(),
                    config: config.clone(),
                }),
            });
            Self {
                root,
                config,
                serial,
                opened,
                factory,
            }
        }
        fn reassign(&self, serial: Option<&str>) {
            let rows: Vec<_> = serial
                .into_iter()
                .map(|serial| json!({"serial":serial, "path":self.config["channel"]}))
                .collect();
            fs::write(
                self.root.join("rows.json"),
                serde_json::to_vec(&rows).expect("rows"),
            )
            .expect("合成重枚举");
        }
    }

    #[cfg(unix)]
    impl Drop for SlcanFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    #[cfg(unix)]
    fn slcan_reassignment_before_first_open_fails_before_driver_open() {
        let fixture = SlcanFixture::new("slcan-first", false);
        let other = lock_serial("slcan-other");
        fixture.reassign(Some(&other));
        let _other_owner =
            OwnerGuard::acquire(&ResourceKey::managed_can(&other, "0").expect("B key"))
                .expect("B已占用");
        assert!(
            fixture
                .factory
                .open()
                .err()
                .expect("原路径已变B")
                .starts_with("owner_lock_invalid:")
        );
        assert_eq!(fixture.opened.load(Ordering::SeqCst), 0);
        fixture.reassign(Some(&fixture.serial));
        drop(fixture.factory.open().expect("显式恢复原归属后可打开"));
    }

    #[test]
    #[cfg(unix)]
    fn slcan_recover_revalidates_identity_and_rejects_missing_original() {
        let fixture = SlcanFixture::new("slcan-recover", true);
        let network = eha_sdk::can::CanNetwork::new(
            Arc::clone(&fixture.factory),
            eha_sdk::transport::can::Mode::Fd,
        )
        .expect("打开A网络");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !matches!(
            network.status(),
            eha_sdk::can::CanNetworkStatus::Terminated { .. }
        ) && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            network.status(),
            eha_sdk::can::CanNetworkStatus::Terminated { .. }
        ));
        let other = lock_serial("slcan-recover-b");
        fixture.reassign(Some(&other));
        let _other_owner =
            OwnerGuard::acquire(&ResourceKey::managed_can(&other, "0").expect("B key"))
                .expect("B owner");
        assert!(
            network
                .recover()
                .expect_err("旧path归属B")
                .contains("owner_lock_invalid:")
        );
        fixture.reassign(None);
        assert!(
            network
                .recover()
                .expect_err("原稳定身份已消失")
                .contains("owner_lock_invalid:")
        );
        assert_eq!(fixture.opened.load(Ordering::SeqCst), 1);
        drop(
            OwnerGuard::acquire(&ResourceKey::managed_can(&fixture.serial, "0").expect("A key"))
                .expect("失败不粘A锁"),
        );
        fixture.reassign(Some(&fixture.serial));
        network.recover().expect("只允许重新打开原A");
        drop(network);
    }

    #[test]
    #[cfg(unix)]
    fn unmanaged_slcan_opens_the_frozen_canonical_path_and_rejects_retargeting() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(lock_serial("slcan-symlink"));
        fs::create_dir(&root).expect("目录");
        let a = root.join("a");
        let b = root.join("b");
        let alias = root.join("alias");
        fs::write(&a, "a").expect("a");
        fs::write(&b, "b").expect("b");
        symlink(&a, &alias).expect("alias");
        let mut config =
            json!({"interface":"slcan", "channel":format!("{}@115200", alias.display())});
        freeze_slcan_path(&mut config).expect("冻结实路径");
        // 相对本地路径也须冻结到同一实路径，不能继续把动态 alias 传给 driver。
        let mut relative = PathBuf::new();
        for component in std::env::current_dir().expect("cwd").components() {
            if matches!(component, std::path::Component::Normal(_)) {
                relative.push("..");
            }
        }
        relative.push(alias.strip_prefix("/").expect("绝对fixture路径"));
        let mut relative_config =
            json!({"interface":"slcan", "channel":format!("{}@115200", relative.display())});
        freeze_slcan_path(&mut relative_config).expect("相对alias冻结");
        assert_eq!(relative_config, config);
        let key = advanced_key(&config, &[]).expect("A实路径key");
        fs::remove_file(&alias).expect("移除旧alias");
        symlink(&b, &alias).expect("alias转B");
        assert_eq!(
            config["channel"],
            format!("{}@115200", a.canonicalize().expect("实路径").display())
        );
        let binding = SlcanBinding {
            python: String::new(),
            config,
        };
        binding
            .validate_rows(&key, &[])
            .expect("原alias变化不影响冻结实际路径");
        fs::remove_file(&a).expect("移除A路径");
        symlink(&b, &a).expect("冻结路径自身转B");
        assert!(
            binding
                .validate_rows(&key, &[])
                .expect_err("归属变化拒绝")
                .starts_with("owner_lock_invalid:")
        );
        fs::remove_dir_all(root).expect("清理");
    }

    #[test]
    fn managed_profiles_share_owner_before_runtime_or_adapter_open() {
        let serial = lock_serial("profiles");
        let key = ResourceKey::managed_can(&serial, "0").expect("key");
        let _owner = OwnerGuard::acquire(&key).expect("已有 owner");
        for python in ["/synthetic/profile-a/python", "/synthetic/profile-b/python"] {
            let error = managed_factory(serial.clone(), python.into())
                .open()
                .err()
                .expect("先报物理竞争");
            assert!(error.starts_with("owner_busy:"), "{error}");
        }
    }

    #[test]
    fn shared_can_nodes_reuse_the_single_physical_owner() {
        let (factory, _, opened, closed) = synthetic_factory("network", false);
        let network =
            eha_sdk::can::CanNetwork::new(Arc::clone(&factory), eha_sdk::transport::can::Mode::Fd)
                .expect("网络");
        let first = network.node(1).expect("节点1").open().expect("逻辑打开1");
        let second = network
            .node(127)
            .expect("节点127")
            .open()
            .expect("逻辑打开127");
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert!(
            factory
                .open()
                .err()
                .expect("CLI 与网络竞争")
                .contains("owner_busy")
        );
        drop(first);
        drop(second);
        assert!(
            factory
                .open()
                .err()
                .expect("节点断开仍有物理网络")
                .contains("owner_busy")
        );
        drop(network);
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        drop(factory.open().expect("网络关闭后释放"));
    }

    #[test]
    fn terminated_network_releases_physical_owner_and_explicit_recover_reacquires() {
        let serial = lock_serial("recover");
        let opened = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let key = ResourceKey::managed_can(&serial, "0").expect("key");
        let factory: Arc<dyn CanChannelFactory> = Arc::new(LockedCanFactory {
            key: ResourceKey::managed_can(&serial, "0").map_err(|error| error.to_string()),
            inner: Arc::new(SyntheticFactory {
                serial,
                opened: Arc::clone(&opened),
                closed: Arc::clone(&closed),
                fail: false,
                first_channel_fails: true,
            }),
            slcan: None,
        });
        let network = eha_sdk::can::CanNetwork::new(factory, eha_sdk::transport::can::Mode::Fd)
            .expect("网络");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !matches!(
            network.status(),
            eha_sdk::can::CanNetworkStatus::Terminated { .. }
        ) && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            network.status(),
            eha_sdk::can::CanNetworkStatus::Terminated { .. }
        ));
        assert_eq!(closed.load(Ordering::SeqCst), 1, "终止时物理驱动已关闭");
        let competitor = OwnerGuard::acquire(&key).expect("网络状态对象仍存在但物理 owner 已释放");
        assert!(
            network
                .recover()
                .expect_err("恢复仍须取得同一锁")
                .contains("owner_busy")
        );
        assert_eq!(opened.load(Ordering::SeqCst), 1, "竞争失败不调用外部驱动");
        drop(competitor);
        network.recover().expect("显式恢复重新持锁");
        assert_eq!(opened.load(Ordering::SeqCst), 2);
        assert_eq!(
            OwnerGuard::acquire(&key)
                .expect_err("恢复后物理通路 busy")
                .code,
            "owner_busy"
        );
        drop(network);
        drop(OwnerGuard::acquire(&key).expect("关闭恢复后的网络释放锁"));
    }

    #[test]
    fn advanced_contexts_and_managed_slcan_resolve_to_physical_keys() {
        let a = json!({"interface":"virtual", "channel":"same-physical", "bitrate":1000000});
        let b = json!({"interface":"virtual", "channel":"same-physical", "bitrate":2000000});
        assert_eq!(
            advanced_key(&a, &[]).expect("context a"),
            advanced_key(&b, &[]).expect("context b")
        );
        let rows = vec![json!({"path":"/dev/synthetic-canable", "serial":"FULL-CANABLE-SERIAL"})];
        let config = json!({"interface":"slcan", "channel":"/dev/synthetic-canable@115200"});
        assert_eq!(
            advanced_key(&config, &rows).expect("高级 SLCAN"),
            ResourceKey::managed_can("FULL-CANABLE-SERIAL", "0").expect("受管 key")
        );
        assert_eq!(
            advanced_key(&json!({"interface":"virtual", "channel":7}), &[]).expect("整数"),
            ResourceKey::advanced_can("virtual", "7").expect("key")
        );
        assert!(advanced_key(&json!({"interface":"virtual", "channel":[1,2]}), &[]).is_err());
    }

    #[test]
    fn official_python_can_loader_freezes_two_contexts_without_opening_bus() {
        let python = std::env::var("EHA_TOOL_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
        if !Command::new(&python)
            .args(["-I", "-c", "import can"])
            .status()
            .is_ok_and(|status| status.success())
        {
            return; // 普通无 runtime 检出仍可运行合成测试；devbox 安装官方 python-can。
        }
        let load = |context: &str, config: &str| {
            let mut command = Command::new(&python);
            command.args(["-I", "-c", ADVANCED_CONFIG_SCRIPT, context]);
            command.env(format!("CAN_CONFIG_{context}"), config);
            load_advanced_config(command).expect("官方加载器")
        };
        let config = r#"{"interface":"virtual","channel":7,"receive_own_messages":true}"#;
        let first = load("EHA_OWNER_A", config);
        let second = load("EHA_OWNER_B", config);
        assert_eq!(first["channel"], 7, "整数 channel 不转换为字符串配置");
        assert_eq!(
            first["receive_own_messages"], true,
            "未知合法 option 完整保留"
        );
        let classic = load(
            "EHA_OWNER_CLASSIC",
            r#"{"interface":"virtual","channel":7,"state":"PASSIVE","f_clock":8000000,"brp":1,"tseg1":13,"tseg2":2,"sjw":1,"nof_samples":1}"#,
        );
        assert_eq!(classic["timing"]["_eha_timing_kind"], "classic");
        assert_eq!(classic["state"], "PASSIVE");
        let fd = load(
            "EHA_OWNER_FD",
            r#"{"interface":"virtual","channel":7,"f_clock":80000000,"nom_brp":1,"nom_tseg1":63,"nom_tseg2":16,"nom_sjw":16,"data_brp":1,"data_tseg1":31,"data_tseg2":8,"data_sjw":8}"#,
        );
        assert_eq!(fd["timing"]["_eha_timing_kind"], "fd");
        assert_eq!(
            advanced_key(&first, &[]).expect("key a"),
            advanced_key(&second, &[]).expect("key b")
        );
    }
    #[test]
    fn profile_rejects_unstable_or_unsafe_values() {
        let mut profile = ConnectionProfile {
            id: "bench".into(),
            name: "台架".into(),
            adapter: ManagedAdapter {
                kind: "canable2".into(),
                serial: "abc".into(),
            },
            node: 1,
            mode: "fd".into(),
            expected_uid: None,
            autoconnect: false,
            default: true,
        };
        assert!(profile.validate().is_ok());
        profile.mode = "classic".into();
        assert!(profile.validate().is_err());
        profile.mode = "fd".into();
        profile.expected_uid = Some("123".into());
        assert!(profile.validate().is_err());
    }
    #[test]
    fn stored_profile_is_atomically_reloaded() {
        let root =
            std::env::temp_dir().join(format!("eha-tool-connections-{}", std::process::id()));
        let path = root.join("connections.json");
        let mut store = ConnectionStore::load(path.clone());
        store
            .save(ConnectionProfile {
                id: "bench".into(),
                name: "台架".into(),
                adapter: ManagedAdapter {
                    kind: "canable2".into(),
                    serial: "abc".into(),
                },
                node: 1,
                mode: "fd".into(),
                expected_uid: None,
                autoconnect: false,
                default: true,
            })
            .expect("save");
        assert_eq!(
            ConnectionStore::load(path)
                .profile("bench")
                .expect("reload")
                .node,
            1
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_writer_preserves_the_other_instances_saved_profile() {
        let root = std::env::temp_dir().join(format!(
            "eha-tool-connections-concurrent-{}",
            std::process::id()
        ));
        let path = root.join("connections.json");
        let mut first = ConnectionStore::load(path.clone());
        let mut second = ConnectionStore::load(path.clone());
        let profile = |id: &str| ConnectionProfile {
            id: id.into(),
            name: id.into(),
            adapter: ManagedAdapter {
                kind: "canable2".into(),
                serial: id.into(),
            },
            node: 1,
            mode: "fd".into(),
            expected_uid: None,
            autoconnect: false,
            default: false,
        };
        first.save(profile("first")).expect("first writer");
        assert!(
            second
                .save(profile("second"))
                .expect_err("reject stale writer")
                .contains("另一实例修改")
        );
        assert!(
            second.profiles.is_empty(),
            "failed save must not change memory"
        );
        let persisted = ConnectionStore::load(path);
        assert!(persisted.profile("first").is_ok());
        assert!(persisted.profile("second").is_err());
        first
            .delete("first")
            .expect("successful writer retains a current snapshot");
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn malformed_or_ambiguous_profiles_are_not_selected_or_overwritten() {
        let root = std::env::temp_dir().join(format!(
            "eha-tool-connections-invalid-{}",
            std::process::id()
        ));
        let path = root.join("connections.json");
        fs::create_dir_all(&root).expect("root");
        fs::write(&path, "{").expect("corrupt fixture");
        let mut store = ConnectionStore::load(path.clone());
        assert!(store.autoconnect_profile().is_none());
        assert!(
            store
                .save(ConnectionProfile {
                    id: "bench".into(),
                    name: "台架".into(),
                    adapter: ManagedAdapter {
                        kind: "canable2".into(),
                        serial: "abc".into()
                    },
                    node: 1,
                    mode: "fd".into(),
                    expected_uid: None,
                    autoconnect: false,
                    default: false
                })
                .is_err()
        );
        fs::write(&path, r#"{"version":1,"profiles":[{"id":"a","name":"a","adapter":{"kind":"canable2","serial":"1"},"node":1,"mode":"fd","expected_uid":"00112233445566778899aabb","autoconnect":true,"default":false},{"id":"b","name":"b","adapter":{"kind":"canable2","serial":"2"},"node":2,"mode":"fd","expected_uid":"00112233445566778899aabb","autoconnect":true,"default":false}]}"#).expect("ambiguous fixture");
        store = ConnectionStore::load(path);
        assert!(store.autoconnect_profile().is_none());
        let _ = fs::remove_dir_all(root);
    }
}
