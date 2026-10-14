// Copyright The eha-sdk Contributors

//! 本机受管 CANable2 连接档案。
//!
//! 档案只保存稳定 USB 序列号，绝不保存会在拔插后变化的 `/dev/cu.*` 路径。每次
//! `ManagedCanFactory::open` 都重新经 pyserial 解析该序列号，再把完整、显式的
//! python-can 配置交给 SDK；这里不实现 SLCAN 协议，也不触碰用户的 `can.conf`。

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use eha_sdk::can::{CanChannel, CanChannelFactory, python::PythonCanOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    Arc::new(ManagedCanFactory { serial, python })
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
