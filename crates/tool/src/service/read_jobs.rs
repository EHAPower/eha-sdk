// Copyright The eha-sdk Contributors

//! WebUI 只读目录和适配器查询的有界后台工作。
//!
//! 这里的 worker 不持有 `ToolSession` 或任何 transport。记录下载仅接受本 worker
//! 最近一次列表实际返回的 run id，避免把浏览器提供的目录名直接用于本地文件访问。

use super::connection_profiles::{UsbProfile, UsbProfileStore};
use crate::connections::{ConnectionProfile, ConnectionStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread::{self, JoinHandle},
};

const MAX_RUNS: usize = 128;
const MAX_DIRECTORY_ENTRIES: usize = 1024;
const MAX_METADATA_BYTES: u64 = 65_536;
const MAX_LIST_METADATA_BYTES: u64 = 256 * 1024;
const MAX_DOWNLOAD_BYTES: usize = 65_536;
const RECORDING_FILES: [&str; 3] = ["metadata.json", "events.jsonl", "telemetry.csv"];

#[derive(Clone, Debug, Deserialize)]
pub(crate) enum Operation {
    Adapters,
    Recordings,
    Recording {
        run_id: String,
        file: String,
        offset: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct JobResult {
    pub(crate) job_id: String,
    pub(crate) state: JobState,
    pub(crate) data: Option<Value>,
    pub(crate) error: Option<String>,
}

struct ReadJob {
    job_id: String,
    operation: Operation,
}

pub(super) type ReadRunner =
    Box<dyn Fn(Operation, &Path, &mut BTreeSet<String>) -> Result<Value, String> + Send + 'static>;

enum ReadEvent {
    Running(String),
    Completed {
        job_id: String,
        result: Result<Value, String>,
    },
}

/// 串行执行会触发磁盘或 Python runtime 的只读操作；当前请求完成前拒绝下一请求。
pub(crate) struct ReadJobs {
    jobs: SyncSender<ReadJob>,
    events: Receiver<ReadEvent>,
    results: VecDeque<JobResult>,
    active_job: Option<String>,
    next_job_id: u64,
    busy: bool,
    _worker: JoinHandle<()>,
}

impl ReadJobs {
    pub(crate) fn new(runs_dir: PathBuf) -> Self {
        Self::with_runner(
            runs_dir,
            Box::new(|operation, runs_dir, listed_runs| {
                execute_read_operation(operation, runs_dir, listed_runs)
            }),
        )
    }

    #[cfg(test)]
    pub(super) fn with_test_runner(runs_dir: PathBuf, runner: ReadRunner) -> Self {
        Self::with_runner(runs_dir, runner)
    }

    fn with_runner(runs_dir: PathBuf, runner: ReadRunner) -> Self {
        let (jobs, receiver) = mpsc::sync_channel(1);
        let (sender, events) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("eha-tool-read-jobs".into())
            .spawn(move || run_read_jobs(receiver, sender, runs_dir, runner))
            .expect("启动只读工作线程");
        Self {
            jobs,
            events,
            results: VecDeque::new(),
            active_job: None,
            next_job_id: 1,
            busy: false,
            _worker: worker,
        }
    }

    pub(crate) fn start(&mut self, operation: Operation) -> Result<String, String> {
        self.reap();
        if self.busy {
            return Err("本机只读工作正在进行；请等待当前任务完成".into());
        }
        let job_id = self.allocate_job_id()?;
        match self.jobs.try_send(ReadJob {
            job_id: job_id.clone(),
            operation,
        }) {
            Ok(()) => {
                self.busy = true;
                self.active_job = Some(job_id.clone());
                self.results.push_back(JobResult {
                    job_id: job_id.clone(),
                    state: JobState::Queued,
                    data: None,
                    error: None,
                });
                while self.results.len() > 16 {
                    self.results.pop_front();
                }
                Ok(job_id)
            }
            Err(TrySendError::Full(_)) => Err("本机只读工作正在进行；请等待当前任务完成".into()),
            Err(TrySendError::Disconnected(_)) => Err("本机只读工作线程不可用".into()),
        }
    }

    pub(crate) fn reap(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(ReadEvent::Running(job_id)) => {
                    if let Some(result) = self.result_mut(&job_id) {
                        result.state = JobState::Running;
                    }
                }
                Ok(ReadEvent::Completed { job_id, result }) => {
                    self.busy = false;
                    self.active_job = None;
                    if let Some(job) = self.result_mut(&job_id) {
                        match result {
                            Ok(data) => {
                                job.state = JobState::Completed;
                                job.data = Some(data);
                                job.error = None;
                            }
                            Err(error) => {
                                job.state = JobState::Failed;
                                job.data = None;
                                job.error = Some(error);
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    if !self.busy {
                        return;
                    }
                    self.busy = false;
                    let Some(job_id) = self.active_job.take() else {
                        return;
                    };
                    if let Some(job) = self.result_mut(&job_id) {
                        job.state = JobState::Failed;
                        job.error = Some("本机只读工作线程已停止".into());
                    }
                    return;
                }
            }
        }
    }

    pub(crate) fn get(&self, job_id: &str) -> Option<&JobResult> {
        self.results.iter().find(|result| result.job_id == job_id)
    }

    pub(crate) fn results(&self) -> impl Iterator<Item = &JobResult> {
        self.results.iter()
    }

    fn allocate_job_id(&mut self) -> Result<String, String> {
        let next = self.next_job_id;
        self.next_job_id = self
            .next_job_id
            .checked_add(1)
            .ok_or_else(|| "本机只读任务序号已耗尽；请重启服务".to_owned())?;
        Ok(format!("read-{next}"))
    }

    fn result_mut(&mut self, job_id: &str) -> Option<&mut JobResult> {
        self.results
            .iter_mut()
            .find(|result| result.job_id == job_id)
    }
}

fn run_read_jobs(
    receiver: Receiver<ReadJob>,
    sender: mpsc::Sender<ReadEvent>,
    runs_dir: PathBuf,
    runner: ReadRunner,
) {
    let mut listed_runs = BTreeSet::new();
    while let Ok(ReadJob { job_id, operation }) = receiver.recv() {
        if sender.send(ReadEvent::Running(job_id.clone())).is_err() {
            return;
        }
        let result = runner(operation, &runs_dir, &mut listed_runs);
        if sender
            .send(ReadEvent::Completed { job_id, result })
            .is_err()
        {
            return;
        }
    }
}

fn execute_read_operation(
    operation: Operation,
    runs_dir: &Path,
    listed_runs: &mut BTreeSet<String>,
) -> Result<Value, String> {
    match operation {
        Operation::Adapters => list_adapters(),
        Operation::Recordings => list_recordings(runs_dir, listed_runs),
        Operation::Recording {
            run_id,
            file,
            offset,
        } => read_recording(runs_dir, listed_runs, &run_id, &file, offset),
    }
}

fn list_adapters() -> Result<Value, String> {
    let python = crate::runtime::python(None)?;
    let adapters = crate::connections::adapters(&python)?;
    let adapters = adapters
        .into_iter()
        .map(|adapter| {
            json!({
                "kind": adapter["kind"],
                "serial": adapter["serial"],
                "description": adapter["description"],
                "vid": adapter["vid"],
                "pid": adapter["pid"],
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"adapters": adapters}))
}

fn list_recordings(runs_dir: &Path, listed_runs: &mut BTreeSet<String>) -> Result<Value, String> {
    listed_runs.clear();
    let root = match fs::canonicalize(runs_dir) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"runs": [], "truncated": false}));
        }
        Err(error) => return Err(format!("无法确认记录目录：{error}")),
    };
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => return Err(format!("无法读取记录目录：{error}")),
    };

    let mut scanned = 0_usize;
    let mut runs = Vec::new();
    for entry in entries {
        if scanned == MAX_DIRECTORY_ENTRIES {
            break;
        }
        scanned += 1;
        let entry = entry.map_err(|error| format!("无法枚举记录目录：{error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("无法读取记录目录项：{error}"))?;
        if !file_type.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if !valid_run_id(&id) {
            continue;
        }
        runs.push(id);
    }
    runs.sort();
    let mut truncated = scanned == MAX_DIRECTORY_ENTRIES || runs.len() > MAX_RUNS;
    runs.truncate(MAX_RUNS);

    let mut metadata_budget = MAX_LIST_METADATA_BYTES;
    let mut response_runs = Vec::with_capacity(runs.len());
    for id in runs {
        listed_runs.insert(id.clone());
        let path = root.join(&id);
        let files = RECORDING_FILES
            .iter()
            .filter(|file| regular_file(&path.join(file)))
            .map(|file| Value::String((*file).into()))
            .collect::<Vec<_>>();
        let (metadata_json_text, metadata_truncated) =
            recording_metadata_json_text(&root, &id, &mut metadata_budget);
        truncated |= metadata_truncated;
        response_runs.push(json!({
            "id": id,
            "metadata_json_text": metadata_json_text,
            "files": files,
        }));
    }
    Ok(json!({"runs": response_runs, "truncated": truncated}))
}

fn read_recording(
    runs_dir: &Path,
    listed_runs: &BTreeSet<String>,
    run_id: &str,
    file: &str,
    offset: u64,
) -> Result<Value, String> {
    if !valid_run_id(run_id) || !listed_runs.contains(run_id) {
        return Err("记录尚未在本机列表中返回；请先重新列出记录".into());
    }
    if !RECORDING_FILES.contains(&file) {
        return Err("不支持读取该记录文件".into());
    }
    let root = fs::canonicalize(runs_dir).map_err(|error| format!("无法确认记录目录：{error}"))?;
    let canonical_run = fs::canonicalize(root.join(run_id))
        .map_err(|error| format!("无法确认记录目录：{error}"))?;
    if !canonical_run.starts_with(&root) || canonical_run == root {
        return Err("记录目录超出本机记录根目录".into());
    }
    let mut input = open_recording_file(&root, run_id, file)?;
    let length = input
        .metadata()
        .map_err(|error| format!("无法读取记录文件长度：{error}"))?
        .len();
    if offset > length {
        return Err("记录读取偏移超过文件长度".into());
    }
    input
        .seek(SeekFrom::Start(offset))
        .map_err(|error| format!("无法定位记录读取偏移：{error}"))?;
    let mut content = Vec::with_capacity(MAX_DOWNLOAD_BYTES);
    input
        .take(MAX_DOWNLOAD_BYTES as u64)
        .read_to_end(&mut content)
        .map_err(|error| format!("无法读取记录文件：{error}"))?;
    let next_offset = offset
        .checked_add(u64::try_from(content.len()).map_err(|_| "记录读取长度无效")?)
        .ok_or("记录读取偏移溢出")?;
    Ok(json!({
        "run_id": run_id,
        "file": file,
        "offset": offset.to_string(),
        "next_offset": next_offset.to_string(),
        "eof": next_offset >= length,
        "content_base64": base64(&content),
    }))
}

#[cfg(unix)]
fn open_recording_file(root: &Path, run_id: &str, file: &str) -> Result<fs::File, String> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};

    let root = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| format!("无法安全打开记录根目录：{error}"))?;
    let run = openat(
        &root,
        run_id,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| format!("无法安全打开记录目录：{error}"))?;
    let input = openat(
        &run,
        file,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| format!("无法安全打开记录文件：{error}"))?;
    let stat = fstat(&input).map_err(|error| format!("无法检查记录文件：{error}"))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err("记录文件不是普通文件".into());
    }
    Ok(fs::File::from(input))
}

#[cfg(not(unix))]
fn open_recording_file(root: &Path, run_id: &str, file: &str) -> Result<fs::File, String> {
    let run = root.join(run_id);
    if !fs::symlink_metadata(&run)
        .map_err(|error| format!("无法读取记录目录：{error}"))?
        .file_type()
        .is_dir()
    {
        return Err("记录目录不是普通目录".into());
    }
    let file = run.join(file);
    if !regular_file(&file) {
        return Err("记录文件不存在或不是普通文件".into());
    }
    fs::File::open(file).map_err(|error| format!("无法读取记录文件：{error}"))
}

fn recording_metadata_json_text(root: &Path, run_id: &str, budget: &mut u64) -> (Value, bool) {
    let Ok(file) = open_metadata_file(root, run_id) else {
        return (Value::Null, false);
    };
    let Ok(metadata) = file.metadata() else {
        return (Value::Null, false);
    };
    let len = metadata.len();
    if len > MAX_METADATA_BYTES {
        return (Value::Null, true);
    }
    if len > *budget {
        return (Value::Null, true);
    }
    let mut content = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    if file
        .take(MAX_METADATA_BYTES)
        .read_to_end(&mut content)
        .is_err()
    {
        return (Value::Null, false);
    }
    if serde_json::from_slice::<Value>(&content).is_err() {
        return (Value::Null, false);
    }
    let Ok(text) = String::from_utf8(content) else {
        return (Value::Null, false);
    };
    let used = u64::try_from(text.len()).unwrap_or(u64::MAX);
    if used > *budget {
        return (Value::Null, true);
    }
    *budget -= used;
    (Value::String(text), false)
}

#[cfg(unix)]
fn open_metadata_file(root: &Path, run_id: &str) -> Result<fs::File, String> {
    open_recording_file(root, run_id, "metadata.json")
}

#[cfg(not(unix))]
fn open_metadata_file(root: &Path, run_id: &str) -> Result<fs::File, String> {
    let path = root.join(run_id).join("metadata.json");
    if !regular_file(&path) {
        return Err("metadata 不是普通文件".into());
    }
    fs::File::open(path).map_err(|error| error.to_string())
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn valid_run_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        result.push(TABLE[(first >> 2) as usize] as char);
        result.push(TABLE[((first & 0x03) << 4 | second >> 4) as usize] as char);
        result.push(if chunk.len() > 1 {
            TABLE[((second & 0x0f) << 2 | third >> 6) as usize] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            TABLE[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    result
}

#[derive(Clone, Debug)]
pub(super) enum ProfileOperation {
    SaveCan(ConnectionProfile),
    DeleteCan(String),
    SaveUsb(UsbProfile),
    DeleteUsb(String),
    BindUsb { id: String, uid: String },
}

struct ProfileJob {
    job_id: String,
    can_store: ConnectionStore,
    usb_store: UsbProfileStore,
    operation: ProfileOperation,
}

pub(super) type ProfileCompletion = (
    String,
    ConnectionStore,
    UsbProfileStore,
    Result<Value, String>,
);

/// 档案写入可等待文件锁，因此把唯一的 store 交给 worker，完成后显式归还 owner。
pub(super) struct ProfileJobs {
    jobs: SyncSender<ProfileJob>,
    completed: Receiver<ProfileCompletion>,
    next_job_id: u64,
    _worker: JoinHandle<()>,
}

impl ProfileJobs {
    pub(super) fn new() -> Self {
        let (jobs, receiver) = mpsc::sync_channel(1);
        let (sender, completed) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("eha-tool-profile-jobs".into())
            .spawn(move || run_profile_jobs(receiver, sender))
            .expect("启动连接档案工作线程");
        Self {
            jobs,
            completed,
            next_job_id: 1,
            _worker: worker,
        }
    }

    #[allow(clippy::result_large_err)] // 失败时必须把唯一两份 store 原样还给 owner。
    pub(super) fn start(
        &mut self,
        can_store: ConnectionStore,
        usb_store: UsbProfileStore,
        operation: ProfileOperation,
    ) -> Result<String, (String, ConnectionStore, UsbProfileStore)> {
        let job_id = match self.allocate_job_id() {
            Ok(job_id) => job_id,
            Err(error) => return Err((error, can_store, usb_store)),
        };
        match self.jobs.try_send(ProfileJob {
            job_id: job_id.clone(),
            can_store,
            usb_store,
            operation,
        }) {
            Ok(()) => Ok(job_id),
            Err(TrySendError::Full(job)) => Err((
                "连接档案写入正在进行；请等待当前任务完成".into(),
                job.can_store,
                job.usb_store,
            )),
            Err(TrySendError::Disconnected(job)) => Err((
                "连接档案工作线程不可用".into(),
                job.can_store,
                job.usb_store,
            )),
        }
    }

    pub(super) fn take_completed(&mut self) -> Option<ProfileCompletion> {
        self.completed.try_recv().ok()
    }

    fn allocate_job_id(&mut self) -> Result<String, String> {
        let next = self.next_job_id;
        self.next_job_id = self
            .next_job_id
            .checked_add(1)
            .ok_or_else(|| "连接档案任务序号已耗尽；请重启服务".to_owned())?;
        Ok(format!("profile-{next}"))
    }
}

fn run_profile_jobs(receiver: Receiver<ProfileJob>, sender: mpsc::Sender<ProfileCompletion>) {
    while let Ok(ProfileJob {
        job_id,
        mut can_store,
        mut usb_store,
        operation,
    }) = receiver.recv()
    {
        let result = match operation {
            ProfileOperation::SaveCan(profile) => match usb_store.profile_optional(&profile.id) {
                Ok(Some(_)) => Err(format!("USB 与 CAN 连接档案 id 冲突：{}", profile.id)),
                Ok(None) => can_store
                    .save(profile)
                    .map(|profile| json!({"profile": can_profile_response(profile)})),
                Err(error) => Err(error),
            },
            ProfileOperation::DeleteCan(id) => can_store
                .delete(&id)
                .map(|()| json!({"id": id, "transport": "can", "deleted": true})),
            ProfileOperation::SaveUsb(profile) => usb_store
                .save(profile, &can_store)
                .map(|profile| json!({"profile": usb_profile_response(profile)})),
            ProfileOperation::DeleteUsb(id) => usb_store
                .delete(&id)
                .map(|()| json!({"id": id, "transport": "usb", "deleted": true})),
            ProfileOperation::BindUsb { id, uid } => usb_store
                .bind_identity(&id, &uid)
                .and_then(|()| usb_store.profile(&id))
                .map(|profile| json!({"profile": usb_profile_response(profile)})),
        };
        if sender.send((job_id, can_store, usb_store, result)).is_err() {
            return;
        }
    }
}

fn can_profile_response(profile: ConnectionProfile) -> Value {
    json!({
        "transport": "can",
        "id": profile.id,
        "name": profile.name,
        "adapter": {"kind": profile.adapter.kind, "serial": profile.adapter.serial},
        "node": profile.node,
        "mode": profile.mode,
        "expected_uid": profile.expected_uid,
    })
}

fn usb_profile_response(profile: UsbProfile) -> Value {
    json!({
        "transport": "usb",
        "id": profile.id,
        "name": profile.name,
        "serial": profile.serial,
        "expected_uid": profile.expected_uid,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        JobState, Operation, ProfileJobs, ProfileOperation, ReadJobs, execute_read_operation,
    };
    use crate::connections::{ConnectionProfile, ConnectionStore, ManagedAdapter};
    use crate::service::connection_profiles::{UsbProfile, UsbProfileStore, UsbTransport};
    use serde_json::{Value, json};
    use std::{
        collections::BTreeSet,
        fs,
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temporary_root(label: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("eha-tool-read-jobs-{label}-{stamp}"))
    }

    fn reap_until(jobs: &mut ReadJobs, job_id: &str) {
        let mut completed = false;
        for _ in 0..10_000 {
            jobs.reap();
            if matches!(
                jobs.get(job_id).map(|job| job.state),
                Some(JobState::Completed)
            ) {
                completed = true;
                break;
            }
            thread::yield_now();
        }
        assert!(completed, "read job did not finish: {job_id}");
    }

    fn take_profile_completion(
        jobs: &mut ProfileJobs,
    ) -> (
        String,
        ConnectionStore,
        UsbProfileStore,
        Result<Value, String>,
    ) {
        let mut completion = None;
        for _ in 0..10_000 {
            if let Some(current) = jobs.take_completed() {
                completion = Some(current);
                break;
            }
            thread::yield_now();
        }
        assert!(completion.is_some(), "profile job did not finish");
        completion.expect("profile completion was asserted")
    }

    fn profile(uid: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: "fixture".into(),
            name: "fixture".into(),
            adapter: ManagedAdapter {
                kind: "canable2".into(),
                serial: "serial".into(),
            },
            node: 1,
            mode: "fd".into(),
            expected_uid: Some(uid.into()),
            autoconnect: false,
            default: false,
        }
    }

    fn usb_profile(id: &str) -> UsbProfile {
        UsbProfile {
            transport: UsbTransport::Usb,
            id: id.into(),
            name: id.into(),
            serial: format!("serial-{id}"),
            expected_uid: None,
        }
    }

    #[test]
    fn recordings_require_a_returned_whitelist_and_bound_download_chunks() {
        let root = temporary_root("recordings");
        let run = root.join("run_1");
        fs::create_dir_all(&run).expect("fixture directory");
        fs::write(run.join("metadata.json"), br#"{"label":"fixture"}"#).expect("metadata");
        fs::write(run.join("events.jsonl"), [0_u8, 1, 2]).expect("events");
        fs::write(run.join("telemetry.csv"), vec![b'x'; 65_537]).expect("telemetry");

        let mut listed = BTreeSet::new();
        assert!(
            execute_read_operation(
                Operation::Recording {
                    run_id: "run_1".into(),
                    file: "events.jsonl".into(),
                    offset: 0,
                },
                &root,
                &mut listed,
            )
            .is_err()
        );
        let listing =
            execute_read_operation(Operation::Recordings, &root, &mut listed).expect("listing");
        assert_eq!(
            listing["runs"][0]["metadata_json_text"],
            json!("{\"label\":\"fixture\"}")
        );
        assert_eq!(
            listing["runs"][0]["files"],
            json!(["metadata.json", "events.jsonl", "telemetry.csv"])
        );

        let download = execute_read_operation(
            Operation::Recording {
                run_id: "run_1".into(),
                file: "events.jsonl".into(),
                offset: 0,
            },
            &root,
            &mut listed,
        )
        .expect("whitelisted download");
        assert_eq!(download["content_base64"], "AAEC");
        assert_eq!(download["next_offset"], "3");
        assert!(download["eof"].as_bool().expect("boolean eof"));

        let chunk = execute_read_operation(
            Operation::Recording {
                run_id: "run_1".into(),
                file: "telemetry.csv".into(),
                offset: 0,
            },
            &root,
            &mut listed,
        )
        .expect("bounded chunk");
        assert_eq!(chunk["next_offset"], "65536");
        assert!(!chunk["eof"].as_bool().expect("boolean eof"));
        assert!(
            execute_read_operation(
                Operation::Recording {
                    run_id: "../run_1".into(),
                    file: "events.jsonl".into(),
                    offset: 0,
                },
                &root,
                &mut listed,
            )
            .is_err()
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn empty_recordings_listing_revokes_the_previous_whitelist() {
        let root = temporary_root("revoked-listing");
        let run = root.join("run_1");
        fs::create_dir_all(&run).expect("fixture directory");
        fs::write(run.join("events.jsonl"), b"first").expect("events");
        let mut listed = BTreeSet::new();
        execute_read_operation(Operation::Recordings, &root, &mut listed).expect("first listing");
        assert!(listed.contains("run_1"));

        fs::remove_dir_all(&root).expect("remove root");
        let empty = execute_read_operation(Operation::Recordings, &root, &mut listed)
            .expect("empty listing");
        assert_eq!(empty["runs"], json!([]));
        assert!(listed.is_empty());

        fs::create_dir_all(&run).expect("recreate directory");
        fs::write(run.join("events.jsonl"), b"second").expect("recreated events");
        assert!(
            execute_read_operation(
                Operation::Recording {
                    run_id: "run_1".into(),
                    file: "events.jsonl".into(),
                    offset: 0,
                },
                &root,
                &mut listed,
            )
            .is_err()
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[cfg(unix)]
    #[test]
    fn recordings_reject_run_and_fixed_file_symlinks() {
        use rustix::fs::{Mode, mkfifoat};
        use std::os::unix::fs::symlink;

        let root = temporary_root("symlinks");
        let run = root.join("run_1");
        fs::create_dir_all(&run).expect("fixture directory");
        let outside = root.join("outside");
        fs::write(&outside, b"outside").expect("outside file");
        symlink(&outside, run.join("events.jsonl")).expect("file symlink");
        let run_directory = fs::File::open(&run).expect("run directory");
        mkfifoat(&run_directory, "metadata.json", Mode::RUSR | Mode::WUSR).expect("metadata FIFO");
        mkfifoat(&run_directory, "telemetry.csv", Mode::RUSR | Mode::WUSR).expect("telemetry FIFO");
        symlink(&run, root.join("run_link")).expect("directory symlink");

        let mut listed = BTreeSet::new();
        let listing =
            execute_read_operation(Operation::Recordings, &root, &mut listed).expect("listing");
        assert_eq!(listing["runs"].as_array().expect("runs").len(), 1);
        assert_eq!(listing["runs"][0]["metadata_json_text"], Value::Null);
        assert_eq!(listing["runs"][0]["files"], json!([]));
        assert!(!listed.contains("run_link"));
        assert!(
            execute_read_operation(
                Operation::Recording {
                    run_id: "run_1".into(),
                    file: "events.jsonl".into(),
                    offset: 0,
                },
                &root,
                &mut listed,
            )
            .is_err()
        );
        assert!(
            execute_read_operation(
                Operation::Recording {
                    run_id: "run_1".into(),
                    file: "telemetry.csv".into(),
                    offset: 0,
                },
                &root,
                &mut listed,
            )
            .is_err()
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn read_jobs_reject_busy_work_without_blocking_and_retain_sixteen_results() {
        let root = temporary_root("worker");
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let first = Arc::new(AtomicBool::new(true));
        let runner_started = Arc::clone(&started);
        let runner_release = Arc::clone(&release);
        let runner_first = Arc::clone(&first);
        let mut jobs = ReadJobs::with_test_runner(
            root.clone(),
            Box::new(move |_, _, _| {
                if runner_first.swap(false, Ordering::SeqCst) {
                    runner_started.wait();
                    runner_release.wait();
                }
                Ok(json!({"fixture": true}))
            }),
        );
        let first_id = jobs.start(Operation::Adapters).expect("first job");
        assert!(
            jobs.start(Operation::Adapters).is_err(),
            "busy job is rejected"
        );
        started.wait();
        jobs.reap();
        assert_eq!(
            jobs.get(&first_id).expect("first state").state,
            JobState::Running
        );
        release.wait();
        reap_until(&mut jobs, &first_id);

        for expected in 2..=17 {
            let job_id = jobs.start(Operation::Adapters).expect("next job");
            assert_eq!(job_id, format!("read-{expected}"));
            reap_until(&mut jobs, &job_id);
        }
        assert_eq!(jobs.results().count(), 16);
        assert!(jobs.get(&first_id).is_none());
        drop(jobs);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn profile_worker_returns_the_store_and_rejects_uid_replacement() {
        let root = temporary_root("profiles");
        let mut jobs = ProfileJobs::new();
        let first = jobs.start(
            ConnectionStore::load(root.join("connections.json")),
            UsbProfileStore::load(root.join("usb-connections.json")),
            ProfileOperation::SaveCan(profile("00112233445566778899aabb")),
        );
        let first_id = match first {
            Ok(job_id) => job_id,
            Err((error, _can_store, _usb_store)) => {
                assert!(error.is_empty(), "start save: {error}");
                return;
            }
        };
        assert_eq!(first_id, "profile-1");
        let (_, can_store, usb_store, result) = take_profile_completion(&mut jobs);
        let result = result.expect("save result");
        assert_eq!(
            result["profile"]["expected_uid"],
            "00112233445566778899aabb"
        );
        assert_eq!(result["profile"]["transport"], "can");
        assert!(result["profile"].get("autoconnect").is_none());
        assert!(result["profile"].get("default").is_none());

        let second = jobs.start(
            can_store,
            usb_store,
            ProfileOperation::SaveCan(profile("ffeeddccbbaa998877665544")),
        );
        let second_id = match second {
            Ok(job_id) => job_id,
            Err((error, _can_store, _usb_store)) => {
                assert!(error.is_empty(), "start conflicting save: {error}");
                return;
            }
        };
        assert_eq!(second_id, "profile-2");
        let (_, can_store, usb_store, result) = take_profile_completion(&mut jobs);
        assert!(
            result
                .expect_err("different UID rejected")
                .contains("另一 EHA UID")
        );

        let third = jobs.start(
            can_store,
            usb_store,
            ProfileOperation::SaveUsb(usb_profile("usb-fixture")),
        );
        let third_id = match third {
            Ok(job_id) => job_id,
            Err((error, _can_store, _usb_store)) => {
                assert!(error.is_empty(), "start USB save: {error}");
                return;
            }
        };
        assert_eq!(third_id, "profile-3");
        let (_, can_store, usb_store, result) = take_profile_completion(&mut jobs);
        let result = result.expect("USB save result");
        assert_eq!(result["profile"]["transport"], "usb");

        let fourth = jobs.start(
            can_store,
            usb_store,
            ProfileOperation::SaveCan(ConnectionProfile {
                id: "usb-fixture".into(),
                ..profile("00112233445566778899aabb")
            }),
        );
        let fourth_id = match fourth {
            Ok(job_id) => job_id,
            Err((error, _can_store, _usb_store)) => {
                assert!(error.is_empty(), "start cross-transport save: {error}");
                return;
            }
        };
        assert_eq!(fourth_id, "profile-4");
        let (_, _can_store, _usb_store, result) = take_profile_completion(&mut jobs);
        assert!(
            result
                .expect_err("cross-transport id rejected")
                .contains("id 冲突")
        );
        drop(jobs);
        fs::remove_dir_all(root).expect("remove fixture");
    }
}
