// Copyright The eha-sdk Contributors
//! USB 档案的独立存储和连接输入转换；不打开设备，不改变既有受管 CAN 档案格式。

use crate::{connections::ConnectionStore, session::ConnectionRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_PROFILES: usize = 128;
const MAX_STORE_BYTES: u64 = 1_048_576;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum UsbTransport {
    Usb,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct UsbProfile {
    pub transport: UsbTransport,
    pub id: String,
    pub name: String,
    pub serial: String,
    pub expected_uid: Option<String>,
}

impl UsbProfile {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err("USB 档案 id 必须为 1..=64 个字母、数字、_ 或 -".into());
        }
        if self.name.trim().is_empty() || self.name.len() > 256 {
            return Err("USB 档案名称必须非空且不超过 256 字节".into());
        }
        crate::owner_lock::ResourceKey::usb(&self.serial).map_err(|error| error.to_string())?;
        if self
            .expected_uid
            .as_deref()
            .is_some_and(|uid| !valid_uid(uid))
        {
            return Err("expected_uid 必须是 24 个十六进制字符的 EHA UID".into());
        }
        Ok(())
    }

    /// 仅转换选择；expected_uid 须由 owner 在首次 Identity 和以后 reconnect 准入时核对。
    pub fn request(&self) -> Result<ConnectionRequest, String> {
        self.validate()?;
        Ok(ConnectionRequest::Usb {
            serial: self.serial.clone(),
        })
    }

    pub fn identity_matches(&self, identity: &Value) -> Result<(), String> {
        self.validate()?;
        crate::connections::expected_uid_matches(self.expected_uid.as_deref(), identity)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedUsbProfiles {
    version: u8,
    profiles: Vec<UsbProfile>,
}

/// sidecar 与 connections.json 分离；损坏、旧快照或跨类型同 id 均不允许静默写入。
pub(super) struct UsbProfileStore {
    path: PathBuf,
    profiles: BTreeMap<String, UsbProfile>,
    load_error: Option<String>,
}

impl UsbProfileStore {
    #[cfg(not(test))]
    pub fn load_default() -> Self {
        Self::load(crate::connections::default_path().with_file_name("usb-connections.json"))
    }

    pub fn load(path: PathBuf) -> Self {
        let (profiles, load_error) = match read_profiles(&path) {
            Ok(profiles) => (profiles, None),
            Err(error) => (BTreeMap::new(), Some(error)),
        };
        Self {
            path,
            profiles,
            load_error,
        }
    }

    pub fn response(&self) -> Value {
        json!({"profiles":self.profiles.values().collect::<Vec<_>>(),"load_error":self.load_error})
    }

    pub fn profile_optional(&self, id: &str) -> Result<Option<UsbProfile>, String> {
        self.writable()?;
        Ok(self.profiles.get(id).cloned())
    }

    pub fn profile(&self, id: &str) -> Result<UsbProfile, String> {
        self.profile_optional(id)?
            .ok_or_else(|| format!("找不到 USB 连接档案：{id}"))
    }

    /// 合并列举和 CAN 保存也须检查此边界；两个文件不能按读取顺序解决同 id。
    pub fn check_namespace(&self, managed: &ConnectionStore) -> Result<(), String> {
        self.writable()?;
        for id in self.profiles.keys() {
            if managed.profile_optional(id)?.is_some() {
                return Err(format!("USB 与 CAN 连接档案 id 冲突：{id}"));
            }
        }
        Ok(())
    }

    pub fn save(
        &mut self,
        mut profile: UsbProfile,
        managed: &ConnectionStore,
    ) -> Result<UsbProfile, String> {
        self.writable()?;
        self.check_namespace(managed)?;
        if managed.profile_optional(&profile.id)?.is_some() {
            return Err(format!("USB 与 CAN 连接档案 id 冲突：{}", profile.id));
        }
        if let Some(old) = self.profiles.get(&profile.id) {
            match (&old.expected_uid, &profile.expected_uid) {
                (Some(old), Some(new)) if !old.eq_ignore_ascii_case(new) => {
                    return Err("同名 USB 档案已绑定另一 EHA UID；请使用新的 id".into());
                }
                (Some(old), None) => profile.expected_uid = Some(old.clone()),
                _ => {}
            }
        }
        profile.validate()?;
        if let Some(uid) = profile.expected_uid.as_mut() {
            uid.make_ascii_lowercase();
        }
        let mut candidate = self.profiles.clone();
        candidate.insert(profile.id.clone(), profile.clone());
        if candidate.len() > MAX_PROFILES {
            return Err("USB 连接档案超过 128 项上限".into());
        }
        self.persist(&candidate)?;
        self.profiles = candidate;
        Ok(profile)
    }

    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        self.writable()?;
        let mut candidate = self.profiles.clone();
        candidate
            .remove(id)
            .ok_or_else(|| format!("找不到 USB 连接档案：{id}"))?;
        self.persist(&candidate)?;
        self.profiles = candidate;
        Ok(())
    }

    pub fn bind_identity(&mut self, id: &str, uid: &str) -> Result<(), String> {
        self.writable()?;
        if !valid_uid(uid) {
            return Err("Identity 未提供有效 EHA UID，不能绑定 USB 档案".into());
        }
        let mut candidate = self.profiles.clone();
        let profile = candidate
            .get_mut(id)
            .ok_or_else(|| format!("找不到 USB 连接档案：{id}"))?;
        if profile
            .expected_uid
            .as_deref()
            .is_some_and(|old| !old.eq_ignore_ascii_case(uid))
        {
            return Err("USB 档案已绑定另一 EHA UID；拒绝静默覆盖".into());
        }
        profile.expected_uid = Some(uid.to_ascii_lowercase());
        self.persist(&candidate)?;
        self.profiles = candidate;
        Ok(())
    }

    fn writable(&self) -> Result<(), String> {
        self.load_error
            .as_ref()
            .map_or(Ok(()), |error| Err(format!("USB 连接档案未写入：{error}")))
    }

    fn persist(&self, profiles: &BTreeMap<String, UsbProfile>) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&SavedUsbProfiles {
            version: 1,
            profiles: profiles.values().cloned().collect(),
        })
        .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err("USB 连接档案文件超过 1 MiB 上限".into());
        }
        let parent = self.path.parent().ok_or("USB 档案路径没有父目录")?;
        fs::create_dir_all(parent).map_err(|error| format!("无法创建 USB 档案目录：{error}"))?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        unix_options(&mut options);
        let lock = options
            .open(self.path.with_extension("lock"))
            .map_err(|error| format!("无法打开 USB 档案锁：{error}"))?;
        if !lock
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("USB 档案锁必须是普通文件".into());
        }
        lock.try_lock()
            .map_err(|error| format!("另一实例正在修改 USB 档案：{error}"))?;
        if read_profiles(&self.path)? != self.profiles {
            return Err("USB 档案已被另一实例修改；请重新读取后再保存".into());
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| error.to_string())?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temporary = parent.join(format!(".usb-connections-{suffix}.tmp"));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        unix_options(&mut options);
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("无法创建 USB 临时档案：{error}"))?;
        let result = file.write_all(&bytes).and_then(|()| {
            drop(file);
            fs::rename(&temporary, &self.path)
        });
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(format!("无法原子写入 USB 档案：{error}"));
        }
        Ok(())
    }
}

fn read_profiles(path: &Path) -> Result<BTreeMap<String, UsbProfile>, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    unix_options(&mut options);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("无法读取 USB 档案：{error}")),
    };
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("USB 档案必须是普通文件".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_STORE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err("USB 档案文件超过 1 MiB 上限".into());
    }
    let saved: SavedUsbProfiles =
        serde_json::from_slice(&bytes).map_err(|error| format!("USB 档案格式无效：{error}"))?;
    if saved.version != 1 || saved.profiles.len() > MAX_PROFILES {
        return Err("USB 档案版本或数量无效".into());
    }
    let mut profiles = BTreeMap::new();
    for profile in saved.profiles {
        profile.validate()?;
        if profiles.insert(profile.id.clone(), profile).is_some() {
            return Err("USB 档案包含重复 id".into());
        }
    }
    Ok(profiles)
}

fn unix_options(options: &mut fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    #[cfg(not(unix))]
    let _ = options;
}

/// 仅沿用命名 python-can context；实际配置冻结与 owner 锁仍由 advanced_factory 处理。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdvancedCanInput {
    pub channel: String,
    pub mode: String,
    pub python: Option<String>,
}

impl AdvancedCanInput {
    pub fn requests(&self, nodes: &[u8]) -> Result<Vec<ConnectionRequest>, String> {
        validate_text(&self.channel, 1024, "python-can 配置 context")?;
        crate::session::parse_can_mode(&self.mode).map_err(|error| error.message.to_string())?;
        if let Some(python) = &self.python {
            validate_text(python, 4096, "Python 路径")?;
        }
        if nodes.is_empty() || nodes.len() > 16 {
            return Err("必须明确选择 1..=16 个 CAN 节点".into());
        }
        let mut seen = [false; 128];
        for &node in nodes {
            if node > 127 || std::mem::replace(&mut seen[node as usize], true) {
                return Err("CAN 节点必须在 0..=127 且不得重复".into());
            }
        }
        Ok(nodes
            .iter()
            .map(|&node| ConnectionRequest::Can {
                channel: self.channel.clone(),
                node,
                mode: self.mode.clone(),
                python: self.python.clone(),
            })
            .collect())
    }
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
fn validate_text(value: &str, max_bytes: usize, name: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > max_bytes || value.contains('\0') {
        Err(format!(
            "{name}必须非空、不含 NUL 且不超过 {max_bytes} 字节"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::{ConnectionProfile, ManagedAdapter};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "eha-usb-profiles-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("valid profile fixture");
            Self { root }
        }
        fn usb(&self) -> UsbProfileStore {
            UsbProfileStore::load(self.root.join("usb-connections.json"))
        }
        fn can(&self) -> ConnectionStore {
            ConnectionStore::load(self.root.join("connections.json"))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn profile() -> UsbProfile {
        UsbProfile {
            transport: UsbTransport::Usb,
            id: "usb_test".into(),
            name: "USB 测试".into(),
            serial: "complete-stable-serial".into(),
            expected_uid: None,
        }
    }
    fn can_profile(id: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: id.into(),
            name: "旧 CAN".into(),
            adapter: ManagedAdapter {
                kind: "canable2".into(),
                serial: "can-stable-serial".into(),
            },
            node: 1,
            mode: "fd".into(),
            expected_uid: None,
            autoconnect: false,
            default: false,
        }
    }

    #[test]
    fn usb_roundtrip_conversion_delete_preserve_can_file() {
        let fixture = Fixture::new();
        let mut can = fixture.can();
        can.save(can_profile("can_test"))
            .expect("valid profile fixture");
        let original =
            fs::read(fixture.root.join("connections.json")).expect("valid profile fixture");
        let mut usb = fixture.usb();
        usb.save(profile(), &can).expect("valid profile fixture");
        let saved = fixture
            .usb()
            .profile("usb_test")
            .expect("valid profile fixture");
        assert_eq!(
            saved.request().expect("valid profile fixture"),
            ConnectionRequest::Usb {
                serial: "complete-stable-serial".into()
            }
        );
        assert_eq!(usb.response()["profiles"][0]["transport"], "usb");
        usb.delete("usb_test").expect("valid profile fixture");
        assert!(
            fixture
                .usb()
                .profile_optional("usb_test")
                .expect("valid profile fixture")
                .is_none()
        );
        assert_eq!(
            fs::read(fixture.root.join("connections.json")).expect("valid profile fixture"),
            original
        );
    }

    #[test]
    fn uid_binding_cannot_be_cleared_or_replaced_and_matches_case_insensitively() {
        let fixture = Fixture::new();
        let can = fixture.can();
        let mut usb = fixture.usb();
        usb.save(profile(), &can).expect("valid profile fixture");
        usb.bind_identity("usb_test", "ABCDEF0123456789ABCDEF01")
            .expect("valid profile fixture");
        let saved = usb.save(profile(), &can).expect("valid profile fixture");
        assert_eq!(
            saved.expected_uid.as_deref(),
            Some("abcdef0123456789abcdef01")
        );
        assert!(
            saved
                .identity_matches(&json!({"uid":"ABCDEF0123456789ABCDEF01"}))
                .is_ok()
        );
        assert!(
            saved
                .identity_matches(&json!({"uid":"000000000000000000000000"}))
                .is_err()
        );
        assert!(saved.identity_matches(&json!({})).is_err());
        let mut changed = profile();
        changed.expected_uid = Some("000000000000000000000000".into());
        assert!(usb.save(changed, &can).is_err());
        assert!(
            usb.bind_identity("usb_test", "000000000000000000000000")
                .is_err()
        );
    }

    #[test]
    fn stale_snapshot_and_corrupt_store_never_overwrite() {
        let fixture = Fixture::new();
        let can = fixture.can();
        let mut first = fixture.usb();
        let mut stale = fixture.usb();
        first.save(profile(), &can).expect("valid profile fixture");
        let mut different = profile();
        different.id = "other".into();
        assert!(stale.save(different, &can).is_err());
        let path = fixture.root.join("usb-connections.json");
        fs::write(&path, "{broken").expect("valid profile fixture");
        let mut corrupt = fixture.usb();
        assert!(corrupt.save(profile(), &can).is_err());
        assert!(corrupt.delete("usb_test").is_err());
        assert_eq!(
            fs::read_to_string(path).expect("valid profile fixture"),
            "{broken"
        );
    }

    #[test]
    fn namespace_collision_is_explicit_in_both_creation_orders() {
        let fixture = Fixture::new();
        let mut can = fixture.can();
        let mut usb = fixture.usb();
        can.save(can_profile("usb_test"))
            .expect("valid profile fixture");
        assert!(usb.save(profile(), &can).is_err());
        can.delete("usb_test").expect("valid profile fixture");
        usb.save(profile(), &can).expect("valid profile fixture");
        can.save(can_profile("usb_test"))
            .expect("valid profile fixture");
        assert!(usb.check_namespace(&can).is_err());
    }

    #[test]
    fn strict_profile_fields_and_duplicate_ids_are_rejected() {
        let fixture = Fixture::new();
        let mut value = serde_json::to_value(profile()).expect("valid profile fixture");
        value["autoconnect"] = json!(true);
        assert!(serde_json::from_value::<UsbProfile>(value).is_err());
        let duplicate = r#"{"transport":"usb","id":"a","id":"b","name":"USB","serial":"serial","expected_uid":null}"#;
        assert!(serde_json::from_str::<UsbProfile>(duplicate).is_err());
        let mut invalid = profile();
        invalid.serial = " ".into();
        assert!(invalid.validate().is_err());
        invalid = profile();
        invalid.expected_uid = Some("wrong".into());
        assert!(invalid.validate().is_err());
        invalid = profile();
        invalid.id = "../path".into();
        assert!(invalid.validate().is_err());
        let saved = SavedUsbProfiles {
            version: 1,
            profiles: vec![profile(), profile()],
        };
        fs::write(
            fixture.root.join("usb-connections.json"),
            serde_json::to_vec(&saved).expect("valid profile fixture"),
        )
        .expect("valid profile fixture");
        assert!(fixture.usb().response()["load_error"].is_string());
    }

    #[test]
    fn file_and_list_limits_reject_incomplete_loads() {
        let fixture = Fixture::new();
        let path = fixture.root.join("usb-connections.json");
        fs::write(&path, vec![b' '; (MAX_STORE_BYTES + 1) as usize])
            .expect("valid profile fixture");
        assert!(fixture.usb().response()["load_error"].is_string());
        let profiles = (0..=MAX_PROFILES)
            .map(|index| {
                let mut item = profile();
                item.id = format!("usb_{index}");
                item
            })
            .collect();
        fs::write(
            path,
            serde_json::to_vec(&SavedUsbProfiles {
                version: 1,
                profiles,
            })
            .expect("valid profile fixture"),
        )
        .expect("valid profile fixture");
        assert!(fixture.usb().response()["load_error"].is_string());
    }

    #[test]
    fn advanced_conversion_preserves_context_python_and_classic_or_fd() {
        for mode in ["classic", "fd"] {
            let input: AdvancedCanInput = serde_json::from_value(
                json!({"channel":"named-config","mode":mode,"python":"/custom/python"}),
            )
            .expect("valid profile fixture");
            let requests = input.requests(&[0, 127]).expect("valid profile fixture");
            assert_eq!(
                requests[1],
                ConnectionRequest::Can {
                    channel: "named-config".into(),
                    node: 127,
                    mode: mode.into(),
                    python: Some("/custom/python".into())
                }
            );
            assert!(input.requests(&[]).is_err());
            assert!(input.requests(&[1, 1]).is_err());
            assert!(input.requests(&[128]).is_err());
            assert!(input.requests(&(0..17).collect::<Vec<_>>()).is_err());
        }
        let input: AdvancedCanInput =
            serde_json::from_value(json!({"channel":"named-config","mode":"fd","python":null}))
                .expect("valid profile fixture");
        assert!(input.requests(&[1]).is_ok());
        assert!(
            serde_json::from_value::<AdvancedCanInput>(
                json!({"channel":"named-config","mode":"fd","config":{}})
            )
            .is_err()
        );
        let invalid = AdvancedCanInput {
            mode: "unknown".into(),
            ..input
        };
        assert!(invalid.requests(&[1]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_store_and_lock_are_rejected_without_changing_target() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let can = fixture.can();
        let outside = fixture.root.join("unrelated");
        fs::write(&outside, "preserve").expect("valid profile fixture");
        let path = fixture.root.join("usb-connections.json");
        symlink(&outside, &path).expect("valid profile fixture");
        assert!(fixture.usb().response()["load_error"].is_string());
        fs::remove_file(&path).expect("valid profile fixture");
        symlink(&outside, path.with_extension("lock")).expect("valid profile fixture");
        assert!(fixture.usb().save(profile(), &can).is_err());
        assert_eq!(
            fs::read_to_string(outside).expect("valid profile fixture"),
            "preserve"
        );
    }
}
