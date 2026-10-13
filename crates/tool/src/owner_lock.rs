// Copyright The eha-sdk Contributors

//! eha-tool 实例之间的物理通路 owner；不提供设备安全或外部 CAN 发送者排他保证。
//! 调用方必须在打开设备前持锁，并在整个通路生命周期内持有 guard。
//! CAN 多节点须由同一网络 owner 共享此 guard；第二次 acquire 即使在同进程也会竞争。
//! 不识别不同 interface/channel 的外部别名；已知别名须由调用方统一使用同一个 key。

use std::{
    env, fmt,
    fs::File,
    path::{Path, PathBuf},
};

/// 完整稳定身份编码；不含档案 id、CAN 节点、mode、python 或动态串口路径。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResourceKey(String);

impl ResourceKey {
    pub(crate) fn usb(serial: &str) -> Result<Self, OwnerLockError> {
        Self::encode(1, &[serial])
    }

    /// physical_channel 是适配器内稳定通路标识，单通路适配器可固定为 "0"。
    pub(crate) fn managed_can(
        serial: &str,
        physical_channel: &str,
    ) -> Result<Self, OwnerLockError> {
        Self::encode(2, &[serial, physical_channel])
    }

    pub(crate) fn advanced_can(interface: &str, channel: &str) -> Result<Self, OwnerLockError> {
        let interface = interface.trim().to_ascii_lowercase();
        let channel = channel.trim();
        // 已存在的绝对路径解析 symlink；其余 channel 由后端解释，不能猜测厂商别名。
        let canonical = if Path::new(channel).is_absolute() {
            Some(
                std::fs::canonicalize(channel)
                    .map_err(|error| OwnerLockError::io("无法规范化 CAN channel", error))?,
            )
        } else {
            None
        };
        let channel = match &canonical {
            Some(path) => path
                .to_str()
                .ok_or_else(|| OwnerLockError::invalid("CAN channel 必须是 UTF-8"))?,
            None => channel,
        };
        Self::encode(3, &[&interface, channel])
    }

    fn encode(kind: u8, fields: &[&str]) -> Result<Self, OwnerLockError> {
        if fields
            .iter()
            .any(|field| field.trim().is_empty() || field.as_bytes().contains(&0))
            || fields.iter().map(|field| field.len()).sum::<usize>() > 1024
        {
            return Err(OwnerLockError::invalid(
                "资源身份不能为空、含 NUL 或超过 1024 字节；禁止截断序列号",
            ));
        }
        // 类型与长度前缀避免字段边界碰撞；十六进制避免路径注入，分段避免 NAME_MAX。
        let mut encoded = format!("{kind:02x}");
        for field in fields {
            use std::fmt::Write;
            let _ = write!(encoded, "{:08x}", field.len());
            for byte in field.as_bytes() {
                let _ = write!(encoded, "{byte:02x}");
            }
        }
        Ok(Self(encoded))
    }

    fn components(&self) -> Vec<String> {
        self.0
            .as_bytes()
            .chunks(120)
            .map(|chunk| format!("k-{}", String::from_utf8_lossy(chunk)))
            .collect()
    }
}

#[derive(Debug)]
pub(crate) struct OwnerLockError {
    pub(crate) code: &'static str,
    message: String,
}

impl OwnerLockError {
    fn invalid(message: &str) -> Self {
        Self {
            code: "owner_lock_invalid",
            message: message.into(),
        }
    }
    #[cfg(unix)]
    fn unsafe_path(message: &str) -> Self {
        Self {
            code: "owner_lock_unsafe",
            message: message.into(),
        }
    }
    fn io(context: &str, error: impl fmt::Display) -> Self {
        Self {
            code: "owner_lock_io",
            message: format!("{context}：{error}"),
        }
    }
}

impl fmt::Display for OwnerLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for OwnerLockError {}

/// 关闭句柄（包括进程死亡）释放内核锁；绝不删除文件，避免 inode 锁分裂。
#[derive(Debug)]
pub(crate) struct OwnerGuard {
    _file: File,
}

impl OwnerGuard {
    pub(crate) fn acquire(key: &ResourceKey) -> Result<Self, OwnerLockError> {
        Self::acquire_in(&default_directory()?, key)
    }

    #[cfg(unix)]
    fn acquire_in(directory: &Path, key: &ResourceKey) -> Result<Self, OwnerLockError> {
        use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
        let mut parent = private_directory(directory)?;
        let components = key.components();
        for component in &components[..components.len() - 1] {
            parent = child_directory(&parent, component.as_ref(), true)?;
        }
        let name = format!("{}.lock", components[components.len() - 1]);
        let fd = openat(
            &parent,
            name.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| path_error("无法打开物理通路锁", error))?;
        let stat = fstat(&fd).map_err(|error| OwnerLockError::io("无法检查物理通路锁", error))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o7777 != 0o600
            || stat.st_nlink != 1
        {
            return Err(OwnerLockError::unsafe_path(
                "锁文件必须是当前 UID 的非硬链接普通文件，权限 0600",
            ));
        }
        let file = File::from(fd);
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => OwnerLockError {
                code: "owner_busy",
                message: "另一 owner 已占用该物理通路".into(),
            },
            std::fs::TryLockError::Error(error) => OwnerLockError::io("无法锁定物理通路", error),
        })?;
        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    fn acquire_in(_directory: &Path, _key: &ResourceKey) -> Result<Self, OwnerLockError> {
        Err(OwnerLockError {
            code: "owner_lock_unsupported",
            message: "当前平台尚未实现私有目录与 owner 锁安全边界".into(),
        })
    }
}

fn default_directory() -> Result<PathBuf, OwnerLockError> {
    Ok(user_data_directory()?.join("owners-v1"))
}

pub(crate) fn user_data_directory() -> Result<PathBuf, OwnerLockError> {
    let base = if let Some(path) = env::var_os("XDG_DATA_HOME") {
        PathBuf::from(path)
    } else {
        let home = env::var_os("HOME")
            .ok_or_else(|| OwnerLockError::invalid("缺少 HOME 和 XDG_DATA_HOME"))?;
        #[cfg(target_os = "macos")]
        let path = PathBuf::from(home)
            .join("Library")
            .join("Application Support");
        #[cfg(not(target_os = "macos"))]
        let path = PathBuf::from(home).join(".local").join("share");
        path
    };
    if !base.is_absolute() {
        return Err(OwnerLockError::invalid("用户数据目录必须是绝对路径"));
    }
    Ok(base.join("eha-tool"))
}

#[cfg(unix)]
fn path_error(context: &str, error: rustix::io::Errno) -> OwnerLockError {
    if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR {
        OwnerLockError::unsafe_path("owner 路径包含符号链接或非目录组件")
    } else {
        OwnerLockError::io(context, error)
    }
}

#[cfg(unix)]
pub(crate) fn private_directory(path: &Path) -> Result<File, OwnerLockError> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(OwnerLockError::invalid("owner 目录必须是绝对路径"));
    }
    let components: Vec<_> = path.components().collect();
    let mut directory =
        File::open("/").map_err(|error| OwnerLockError::io("无法打开根目录", error))?;
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = child_directory(&directory, name, index + 1 == components.len())?
            }
            _ => return Err(OwnerLockError::invalid("owner 目录不能包含父目录跳转")),
        }
    }
    if components.len() <= 1 {
        return Err(OwnerLockError::invalid("owner 目录不能是根目录"));
    }
    Ok(directory)
}

#[cfg(unix)]
fn child_directory(
    parent: &File,
    name: &std::ffi::OsStr,
    private: bool,
) -> Result<File, OwnerLockError> {
    use rustix::fs::{Mode, OFlags, fstat, mkdirat, openat};
    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::EXIST => {}
        Err(error) => return Err(OwnerLockError::io("无法创建 owner 目录", error)),
    }
    let fd = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| path_error("无法打开 owner 目录", error))?;
    let stat = fstat(&fd).map_err(|error| OwnerLockError::io("无法检查 owner 目录", error))?;
    let uid = rustix::process::geteuid().as_raw();
    let mode = stat.st_mode & 0o7777;
    let unsafe_parent = (stat.st_uid != uid && stat.st_uid != 0)
        || (mode & 0o022 != 0 && !(stat.st_uid == 0 && mode & 0o1000 != 0));
    if unsafe_parent || (private && (stat.st_uid != uid || mode != 0o700)) {
        return Err(OwnerLockError::unsafe_path(
            "owner 目录须属于当前 UID、权限 0700；祖先目录不可由其他用户修改",
        ));
    }
    Ok(File::from(fd))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "eha-owner-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("测试目录");
            Self(fs::canonicalize(path).expect("测试目录实路径"))
        }
        fn locks(&self) -> PathBuf {
            self.0.join("owners")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn second_open_is_busy_and_drop_releases() {
        let fixture = Fixture::new();
        let key = ResourceKey::usb("FULL-SERIAL-001").expect("资源 key");
        let first = OwnerGuard::acquire_in(&fixture.locks(), &key).expect("首次持锁");
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks(), &key)
                .expect_err("第二次必须竞争")
                .code,
            "owner_busy"
        );
        drop(first);
        let _next = OwnerGuard::acquire_in(&fixture.locks(), &key).expect("释放后持锁");
    }
    #[test]
    fn keys_separate_identities_without_path_injection() {
        let fixture = Fixture::new();
        let keys = [
            ResourceKey::usb("../FULL/serial").expect("完整 serial"),
            ResourceKey::usb("../FULL/serial-more").expect("不同 serial"),
            ResourceKey::managed_can("ab", "c").expect("受管 key"),
            ResourceKey::managed_can("a", "bc").expect("受管 key"),
        ];
        let guards: Vec<_> = keys
            .iter()
            .map(|key| {
                OwnerGuard::acquire_in(&fixture.locks(), key).expect("不同物理身份可各自持锁")
            })
            .collect();
        assert_eq!(guards.len(), 4);
        assert!(!fixture.0.join("FULL").exists());
        assert!(ResourceKey::usb(" ").is_err());
        assert!(ResourceKey::usb("serial\0tail").is_err());
        assert!(ResourceKey::usb(&"x".repeat(1025)).is_err());
        let long_key = ResourceKey::usb(&"x".repeat(255)).expect("完整长序列号");
        let _long_guard =
            OwnerGuard::acquire_in(&fixture.locks(), &long_key).expect("长 key 分段打开");
    }

    #[test]
    fn managed_owner_covers_all_nodes_modes_and_runtime_paths() {
        let fixture = Fixture::new();
        let key = ResourceKey::managed_can("ADAPTER-FULL-SERIAL", "0").expect("网络 key");
        let _owner = OwnerGuard::acquire_in(&fixture.locks(), &key).expect("网络 owner");
        // 节点、模式、python 与重枚举串口路径无须也不能传入此构造器。
        for _node in [1, 127] {
            let other = ResourceKey::managed_can("ADAPTER-FULL-SERIAL", "0").expect("相同物理通路");
            assert_eq!(
                OwnerGuard::acquire_in(&fixture.locks(), &other)
                    .expect_err("网络已持锁")
                    .code,
                "owner_busy"
            );
        }
        let other_channel = ResourceKey::managed_can("ADAPTER-FULL-SERIAL", "1").expect("独立端口");
        let _other =
            OwnerGuard::acquire_in(&fixture.locks(), &other_channel).expect("不同物理端口");
    }

    #[test]
    fn advanced_channel_normalizes_interface_and_existing_path_aliases() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let device = fixture.0.join("port");
        let alias = fixture.0.join("alias");
        fs::write(&device, "fixture").expect("路径 fixture");
        symlink(&device, &alias).expect("CAN 路径别名");
        let key =
            ResourceKey::advanced_can(" SLCAN ", device.to_str().expect("路径")).expect("高级 key");
        let _guard = OwnerGuard::acquire_in(&fixture.locks(), &key).expect("高级 owner");
        let other =
            ResourceKey::advanced_can("slcan", alias.to_str().expect("路径")).expect("别名 key");
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks(), &other)
                .expect_err("同一实路径竞争")
                .code,
            "owner_busy"
        );
        let plain = ResourceKey::advanced_can(" SocketCAN ", " can0 ").expect("文本 channel");
        assert_eq!(
            plain,
            ResourceKey::advanced_can("socketcan", "can0").expect("规范文本")
        );
    }

    fn lock_path(fixture: &Fixture, key: &ResourceKey) -> PathBuf {
        let components = key.components();
        let mut path = fixture.locks();
        for component in &components[..components.len() - 1] {
            path.push(component);
        }
        path.join(format!("{}.lock", components[components.len() - 1]))
    }

    #[test]
    fn secure_permissions_and_stale_file_reuse_preserve_inode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let fixture = Fixture::new();
        let key = ResourceKey::usb("serial").expect("key");
        let guard = OwnerGuard::acquire_in(&fixture.locks(), &key).expect("owner");
        let path = lock_path(&fixture, &key);
        let inode = fs::metadata(&path).expect("锁 metadata").ino();
        assert_eq!(
            fs::metadata(fixture.locks())
                .expect("目录 metadata")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path)
                .expect("锁 metadata")
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        drop(guard);
        let _guard = OwnerGuard::acquire_in(&fixture.locks(), &key).expect("陈旧文件正常复用");
        assert_eq!(fs::metadata(path).expect("锁 metadata").ino(), inode);
    }

    #[test]
    fn rejects_symlink_directories_and_lock_files() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let key = ResourceKey::usb("serial").expect("key");
        let target = fixture.0.join("target");
        fs::create_dir(&target).expect("目标目录");
        symlink(&target, fixture.locks()).expect("目录 symlink");
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks(), &key)
                .expect_err("拒绝 symlink 目录")
                .code,
            "owner_lock_unsafe"
        );
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks().join("nested"), &key)
                .expect_err("拒绝祖先 symlink")
                .code,
            "owner_lock_unsafe"
        );
        fs::remove_file(fixture.locks()).expect("移除 fixture symlink");
        drop(OwnerGuard::acquire_in(&fixture.locks(), &key).expect("建立锁文件"));
        let lock = lock_path(&fixture, &key);
        fs::remove_file(&lock).expect("移除 fixture 文件");
        let sentinel = fixture.0.join("sentinel");
        fs::write(&sentinel, "untouched").expect("sentinel");
        symlink(&sentinel, lock).expect("文件 symlink");
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks(), &key)
                .expect_err("拒绝 symlink 文件")
                .code,
            "owner_lock_unsafe"
        );
        assert_eq!(
            fs::read_to_string(sentinel).expect("sentinel 内容"),
            "untouched"
        );
    }

    #[test]
    fn rejects_public_permissions_hardlinks_and_parent_traversal() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let key = ResourceKey::usb("serial").expect("key");
        drop(OwnerGuard::acquire_in(&fixture.locks(), &key).expect("建立锁"));
        let path = lock_path(&fixture, &key);
        fs::set_permissions(fixture.locks(), fs::Permissions::from_mode(0o755))
            .expect("fixture 权限");
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks(), &key)
                .expect_err("拒绝公共目录")
                .code,
            "owner_lock_unsafe"
        );
        fs::set_permissions(fixture.locks(), fs::Permissions::from_mode(0o700))
            .expect("fixture 权限");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("fixture 权限");
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks(), &key)
                .expect_err("拒绝公共文件")
                .code,
            "owner_lock_unsafe"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("fixture 权限");
        fs::hard_link(path, fixture.0.join("hardlink")).expect("fixture 硬链接");
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks(), &key)
                .expect_err("拒绝硬链接锁")
                .code,
            "owner_lock_unsafe"
        );
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks().join("..").join("escape"), &key)
                .expect_err("拒绝跳转")
                .code,
            "owner_lock_invalid"
        );
    }

    #[test]
    fn rejects_foreign_uid_and_writable_ancestor() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let key = ResourceKey::usb("serial").expect("key");
        drop(OwnerGuard::acquire_in(&fixture.locks(), &key).expect("建立锁"));
        if rustix::process::geteuid().is_root() {
            let foreign = rustix::process::Uid::from_raw_unchecked(65534);
            let lock = File::open(lock_path(&fixture, &key)).expect("锁文件");
            rustix::fs::fchown(&lock, Some(foreign), None).expect("fixture 文件 UID");
            assert_eq!(
                OwnerGuard::acquire_in(&fixture.locks(), &key)
                    .expect_err("拒绝异 UID 文件")
                    .code,
                "owner_lock_unsafe"
            );
            rustix::fs::fchown(&lock, Some(rustix::process::geteuid()), None)
                .expect("恢复 fixture 文件 UID");
            let directory = File::open(fixture.locks()).expect("owner 目录");
            rustix::fs::fchown(&directory, Some(foreign), None).expect("fixture 目录 UID");
            assert_eq!(
                OwnerGuard::acquire_in(&fixture.locks(), &key)
                    .expect_err("拒绝异 UID 目录")
                    .code,
                "owner_lock_unsafe"
            );
            rustix::fs::fchown(&directory, Some(rustix::process::geteuid()), None)
                .expect("恢复 fixture 目录 UID");
        }
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o777))
            .expect("fixture 祖先权限");
        assert_eq!(
            OwnerGuard::acquire_in(&fixture.locks(), &key)
                .expect_err("拒绝可写祖先")
                .code,
            "owner_lock_unsafe"
        );
    }

    #[test]
    fn parallel_directory_creation_has_one_owner() {
        use std::sync::{Arc, Barrier};
        let fixture = Fixture::new();
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                let directory = fixture.locks();
                std::thread::spawn(move || {
                    let key = ResourceKey::usb("parallel").expect("key");
                    barrier.wait();
                    OwnerGuard::acquire_in(&directory, &key)
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("线程完成"))
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .all(|error| error.code == "owner_busy")
        );
    }

    #[test]
    #[ignore = "仅由跨进程测试子进程执行"]
    fn owner_child() {
        use std::io::{Read, Write};
        let directory = std::env::var_os("EHA_OWNER_TEST_DIRECTORY").expect("子进程目录");
        let key = ResourceKey::usb("process-shared").expect("key");
        match OwnerGuard::acquire_in(Path::new(&directory), &key) {
            Ok(_guard) => {
                assert!(std::env::var_os("EHA_OWNER_TEST_BUSY").is_none());
                println!("owner_ready");
                std::io::stdout().flush().expect("ready 输出");
                let mut buffer = String::new();
                std::io::stdin()
                    .read_to_string(&mut buffer)
                    .expect("等待父进程释放");
            }
            Err(error) => {
                assert!(std::env::var_os("EHA_OWNER_TEST_BUSY").is_some());
                assert_eq!(error.code, "owner_busy");
            }
        }
    }

    fn child_command(directory: &Path) -> std::process::Command {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("测试执行文件"));
        let test_name = format!(
            "{}::owner_child",
            module_path!().split_once("::").expect("测试模块路径").1
        );
        command
            .args(["--exact", &test_name, "--ignored", "--nocapture"])
            .env("EHA_OWNER_TEST_DIRECTORY", directory);
        command
    }

    #[test]
    fn processes_compete_and_exit_or_death_releases_owner() {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        let fixture = Fixture::new();
        let directory = fixture.locks();
        let key = ResourceKey::usb("process-shared").expect("key");
        let guard = OwnerGuard::acquire_in(&directory, &key).expect("父 owner");
        assert!(
            child_command(&directory)
                .env("EHA_OWNER_TEST_BUSY", "1")
                .stdout(Stdio::null())
                .status()
                .expect("竞争子进程")
                .success()
        );
        drop(guard);
        for killed in [false, true] {
            let mut child = child_command(&directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("子 owner");
            let stdout = child.stdout.take().expect("子 stdout");
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                assert!(
                    reader.read_line(&mut line).expect("ready 读取") > 0,
                    "子进程在持锁前退出"
                );
                if line.contains("owner_ready") {
                    break;
                }
                line.clear();
            }
            assert_eq!(
                OwnerGuard::acquire_in(&directory, &key)
                    .expect_err("子进程持锁")
                    .code,
                "owner_busy"
            );
            if killed {
                child.kill().expect("终止子进程");
            }
            drop(child.stdin.take());
            let status = child.wait().expect("子进程结束");
            if !killed {
                assert!(status.success());
            }
            let _guard = OwnerGuard::acquire_in(&directory, &key).expect("退出/死亡后释放");
        }
    }
}
