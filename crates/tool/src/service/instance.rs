// Copyright The eha-sdk Contributors
//! 每用户私有发现记录；进程号仅供诊断，附着必须认证 describe 并核对实例。

use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    pub(crate) protocol: String,
    pub(crate) api_version: u8,
    pub(crate) tool_version: String,
    pub(crate) instance_id: String,
    pub(crate) pid: String,
    pub(crate) started_at_unix_ms: String,
    pub(crate) endpoint: String,
    pub(crate) token_file: PathBuf,
}

pub(super) fn default_path(port: u16) -> Result<PathBuf, String> {
    Ok(crate::owner_lock::user_data_directory()
        .map_err(|error| error.to_string())?
        .join("services-v2")
        .join(format!("port-{port}.json")))
}

#[cfg(unix)]
pub(crate) fn parent(path: &Path) -> Result<(File, &std::ffi::OsStr), String> {
    if !path.is_absolute()
        || path.components().any(|part| {
            !matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
    {
        return Err("服务私有文件必须使用无父目录跳转的绝对路径".into());
    }
    let name = path.file_name().ok_or("服务私有文件缺少文件名")?;
    let directory = crate::owner_lock::private_directory(path.parent().ok_or("缺少父目录")?)
        .map_err(|error| error.to_string())?;
    Ok((directory, name))
}

#[cfg(unix)]
pub(crate) fn open_private(
    parent: &File,
    name: &std::ffi::OsStr,
    create: bool,
) -> Result<File, String> {
    use rustix::fs::{Mode, OFlags, openat};
    let mut flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    if create {
        flags |= OFlags::CREATE;
    }
    let fd =
        openat(parent, name, flags, Mode::RUSR | Mode::WUSR).map_err(|error| error.to_string())?;
    validate_private(File::from(fd))
}

#[cfg(unix)]
pub(super) fn validate_private(file: File) -> Result<File, String> {
    use rustix::fs::{FileType, fstat};
    let stat = fstat(&file).map_err(|error| error.to_string())?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_nlink != 1
    {
        return Err("服务私有文件必须是当前 UID 的非硬链接普通文件，权限 0600".into());
    }
    Ok(file)
}

#[cfg(unix)]
pub(super) fn credential_file(
    parent: &File,
    name: &std::ffi::OsStr,
) -> Result<(File, bool), String> {
    use rustix::fs::{Mode, OFlags, openat};
    match openat(
        parent,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(fd) => {
            let file = File::from(fd);
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
            Ok((file, true))
        }
        Err(error) if error == rustix::io::Errno::EXIST => {
            open_private(parent, name, false).map(|file| (file, false))
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(unix)]
pub(super) struct Published {
    pub(super) path: PathBuf,
    directory: File,
    name: std::ffi::OsString,
    instance_id: String,
    _lock: File,
}

#[cfg(unix)]
impl Published {
    pub(super) fn publish(path: PathBuf, descriptor: &Descriptor) -> Result<Self, String> {
        use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
        let (directory, name) = parent(&path)?;
        let name = name.to_owned();
        let mut lock_name = name.clone();
        lock_name.push(".lock");
        let lock = open_private(&directory, &lock_name, true)?;
        lock.try_lock()
            .map_err(|error| format!("服务发现记录已被另一实例占用：{error}"))?;
        // 拒绝不安全旧记录，不能用原子替换绕过文件权限边界。
        match open_private(&directory, &name, false) {
            Ok(_) => {}
            Err(error) => {
                use rustix::fs::statat;
                match statat(&directory, &name, AtFlags::SYMLINK_NOFOLLOW) {
                    Err(errno) if errno == rustix::io::Errno::NOENT => {}
                    _ => return Err(error),
                }
            }
        }
        let temporary = format!(".instance-{}.tmp", super::integration::random_hex::<16>()?);
        let fd = openat(
            &directory,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| error.to_string())?;
        let result = (|| {
            let mut file = File::from(fd);
            let bytes = serde_json::to_vec(descriptor).map_err(|error| error.to_string())?;
            file.write_all(&bytes).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            renameat(&directory, temporary.as_str(), &directory, &name)
                .map_err(|error| error.to_string())?;
            directory.sync_all().map_err(|error| error.to_string())?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = unlinkat(&directory, temporary.as_str(), AtFlags::empty());
            return Err(error);
        }
        Ok(Self {
            path,
            directory,
            name,
            instance_id: descriptor.instance_id.clone(),
            _lock: lock,
        })
    }
}

#[cfg(unix)]
impl Drop for Published {
    fn drop(&mut self) {
        let result = (|| -> Result<(), String> {
            let mut file = open_private(&self.directory, &self.name, false)?;
            let mut bytes = Vec::new();
            std::io::Read::by_ref(&mut file)
                .take(65537)
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            let descriptor: Descriptor =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if descriptor.instance_id == self.instance_id {
                rustix::fs::unlinkat(&self.directory, &self.name, rustix::fs::AtFlags::empty())
                    .map_err(|error| error.to_string())?;
                self.directory
                    .sync_all()
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("warning: 服务发现记录清理失败：{error}");
        }
    }
}

#[cfg(not(unix))]
pub(super) struct Published {
    pub(super) path: PathBuf,
}
#[cfg(not(unix))]
impl Published {
    pub(super) fn publish(_path: PathBuf, _descriptor: &Descriptor) -> Result<Self, String> {
        Err("服务发现私有文件权限校验目前仅支持 Unix".into())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    pub(super) struct Fixture(pub(super) PathBuf);
    impl Fixture {
        pub(super) fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "eha-instance-{}",
                super::super::integration::random_hex::<8>().expect("random")
            ));
            fs::create_dir(&root).expect("root");
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("private root");
            Self(fs::canonicalize(root).expect("canonical root"))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn descriptor(instance: &str) -> Descriptor {
        Descriptor {
            protocol: "eha-tool.service.v2".into(),
            api_version: 2,
            tool_version: env!("EHA_TOOL_VERSION").into(),
            instance_id: instance.into(),
            pid: std::process::id().to_string(),
            started_at_unix_ms: "1".into(),
            endpoint: "http://127.0.0.1:12345".into(),
            token_file: PathBuf::from("/private/token"),
        }
    }

    #[test]
    fn stale_record_is_atomically_replaced_and_only_own_instance_is_removed() {
        let fixture = Fixture::new();
        let path = fixture.0.join("service.json");
        let old =
            Published::publish(path.clone(), &descriptor("stale-same-pid")).expect("old record");
        // 模拟异常死亡留下发现记录和仍存在的（可复用）PID。
        fs::write(
            &path,
            serde_json::to_vec(&descriptor("replacement-same-pid")).expect("descriptor"),
        )
        .expect("replace record");
        drop(old);
        let bytes = fs::read(&path).expect("new record survives old guard drop");
        assert_eq!(
            serde_json::from_slice::<Descriptor>(&bytes)
                .expect("descriptor")
                .instance_id,
            "replacement-same-pid"
        );
        let old_inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&path).expect("stat").ino()
        };
        let current =
            Published::publish(path.clone(), &descriptor("current")).expect("publish over stale");
        use std::os::unix::fs::MetadataExt;
        assert_ne!(fs::metadata(&path).expect("stat").ino(), old_inode);
        assert_eq!(
            fs::metadata(&path).expect("stat").permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            fs::metadata(&fixture.0)
                .expect("root stat")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert!(Published::publish(path.clone(), &descriptor("other")).is_err());
        assert!(fs::read_dir(&fixture.0).expect("list").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        drop(current);
        assert!(!path.exists());
    }

    #[test]
    fn unsafe_descriptor_token_directory_and_links_are_rejected() {
        let fixture = Fixture::new();
        let path = fixture.0.join("service.json");
        fs::write(&path, b"old").expect("unsafe descriptor");
        assert!(Published::publish(path.clone(), &descriptor("current")).is_err());
        fs::remove_file(&path).expect("remove");
        let target = fixture.0.join("target");
        fs::write(&target, b"old").expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("target mode");
        symlink(&target, &path).expect("symlink");
        assert!(Published::publish(path.clone(), &descriptor("current")).is_err());
        assert!(super::super::integration::IntegrationState::new(&path).is_err());
        fs::remove_file(&path).expect("remove link");
        fs::hard_link(&target, &path).expect("hard link");
        assert!(Published::publish(path.clone(), &descriptor("current")).is_err());
        assert!(super::super::integration::IntegrationState::new(&path).is_err());
        fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o755)).expect("unsafe parent");
        assert!(parent(&fixture.0.join("new")).is_err());
        assert!(parent(Path::new("relative.json")).is_err());
    }

    #[test]
    fn port_conflict_and_bad_credentials_preserve_existing_record() {
        let fixture = Fixture::new();
        let path = fixture.0.join("service.json");
        let guard = Published::publish(path.clone(), &descriptor("still-running")).expect("record");
        let before = fs::read(&path).expect("before");
        let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("listener");
        let port = server.server_addr().to_ip().expect("IP").port();
        let token = fixture.0.join("token");
        assert!(
            super::super::serve(port, None, None, path.clone(), Some(path.clone()), false).is_err()
        );
        assert_eq!(fs::read(&path).expect("record cannot become token"), before);
        assert!(
            super::super::serve(port, None, None, token.clone(), Some(path.clone()), false)
                .is_err()
        );
        assert_eq!(fs::read(&path).expect("after conflict"), before);
        fs::write(&token, b"invalid\n").expect("corrupt token");
        assert!(super::super::serve(port, None, None, token, Some(path.clone()), false).is_err());
        assert_eq!(fs::read(&path).expect("after bad token"), before);
        drop(guard);
    }

    #[test]
    fn standalone_http_shutdown_exits_loop_and_removes_discovery() {
        use std::{
            net::{TcpListener, TcpStream},
            sync::mpsc,
            thread,
            time::Duration,
        };
        let fixture = Fixture::new();
        let path = fixture.0.join("instance.json");
        let token = fixture.0.join("token");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        let (sender, finished) = mpsc::channel();
        let instance_path = path.clone();
        let token_path = token.clone();
        thread::spawn(move || {
            let result =
                super::super::serve(port, None, None, token_path, Some(instance_path), false);
            let _ = sender.send(result);
        });
        let descriptor = (0..100)
            .find_map(|_| match fs::read(&path) {
                Ok(bytes) => {
                    Some(serde_json::from_slice::<Descriptor>(&bytes).expect("atomic descriptor"))
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("published after startup");
        let secret = fs::read_to_string(&token).expect("credential");
        let request = |method: &str, route: &str, body: Option<String>| {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("timeout");
            let mut text = format!(
                "{method} {route} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n",
                secret.trim()
            );
            if let Some(body) = &body {
                text.push_str(&format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    body.len()
                ));
            }
            text.push_str("\r\n");
            if let Some(body) = body {
                text.push_str(&body);
            }
            stream.write_all(text.as_bytes()).expect("request");
            let mut response = String::new();
            stream.read_to_string(&mut response).expect("response");
            let (head, body) = response.split_once("\r\n\r\n").expect("HTTP");
            (
                head.split_whitespace()
                    .nth(1)
                    .expect("status")
                    .parse::<u16>()
                    .expect("status integer"),
                serde_json::from_str::<serde_json::Value>(body).expect("JSON"),
            )
        };
        let (status, describe) = request("GET", "/service/v2/describe", None);
        assert_eq!(status, 200);
        assert_eq!(describe["instance_id"], descriptor.instance_id);
        let (_, summary) = request("GET", "/service/v2/instance", None);
        assert_eq!(summary["service_state"], "running");
        assert!(summary.get("token_file").is_none());
        let body=serde_json::json!({"api_version":2,"instance_id":descriptor.instance_id,"confirmation":"shutdown_service_without_stopping_devices"}).to_string();
        let (status, response) = request("POST", "/service/v2/shutdown", Some(body));
        assert_eq!(status, 202);
        assert_eq!(response["device_stop_requested"], false);
        assert!(
            finished
                .recv_timeout(Duration::from_secs(1))
                .expect("bounded loop exit")
                .is_ok()
        );
        assert!(!path.exists());
        assert!(token.exists());
    }

    #[test]
    fn empty_existing_credentials_are_not_repaired() {
        let fixture = Fixture::new();
        let path = fixture.0.join("token");
        fs::write(&path, b"").expect("empty token");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        assert!(super::super::integration::IntegrationState::new(&path).is_err());
        assert_eq!(fs::read(&path).expect("preserved"), b"");
    }
}
