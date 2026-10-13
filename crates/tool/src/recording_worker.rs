// Copyright The eha-sdk Contributors

//! One bounded process-wide file writer. The session side never performs file I/O
//! or joins the worker. Dropping a recording only marks it abandoned; cleanup and
//! BufWriter destruction happen on the writer thread.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    task::Poll,
    time::{Duration, Instant},
};

use super::{DiskRecording, unix_time_ms};
use serde_json::{Value, json};

const QUEUE_CAPACITY: usize = 256;
const MAX_RECORDINGS: usize = 64;
const MAX_VALUE_BYTES: usize = 65_536;

#[derive(Clone)]
struct Writer {
    tx: SyncSender<Job>,
    active: Arc<AtomicUsize>,
}

#[derive(Clone)]
enum Status {
    Starting,
    Ready(Value),
    Failed(String),
    Finished(Value),
}

struct Progress {
    status: Status,
    flushed: u64,
}

struct State {
    progress: Mutex<Progress>,
    abandoned: AtomicBool,
    failed: AtomicBool,
    active: Arc<AtomicUsize>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Job {
    state: Arc<State>,
    action: Action,
}

enum Action {
    Start {
        root: std::path::PathBuf,
        metadata: Value,
    },
    Event(Value),
    Telemetry {
        value: Value,
        gap: bool,
        unix_ms: u128,
        elapsed_ms: u128,
    },
    Dropped(u64),
    Flush(u64),
    Finish,
    #[cfg(test)]
    FailEventWrites,
}

struct OpenRun {
    state: Arc<State>,
    disk: DiskRecording,
}

impl Writer {
    fn global() -> Result<Self, String> {
        static WRITER: OnceLock<Result<Writer, String>> = OnceLock::new();
        WRITER
            .get_or_init(|| Self::spawn(QUEUE_CAPACITY, || {}))
            .clone()
    }

    fn spawn(
        capacity: usize,
        mut before_job: impl FnMut() + Send + 'static,
    ) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel::<Job>(capacity);
        let active = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("eha-recording-writer".into())
            .spawn(move || {
                let mut runs: BTreeMap<usize, OpenRun> = BTreeMap::new();
                loop {
                    // A full queue cannot prevent abandoned recordings being reclaimed.
                    runs.retain(|_, run| {
                        !run.state.abandoned.load(Ordering::SeqCst)
                            && !run.state.failed.load(Ordering::SeqCst)
                    });
                    let job = match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    before_job();
                    let key = Arc::as_ptr(&job.state) as usize;
                    if job.state.abandoned.load(Ordering::SeqCst)
                        || job.state.failed.load(Ordering::SeqCst)
                    {
                        runs.remove(&key);
                        continue;
                    }
                    let result: Result<Option<Status>, String> = (|| {
                        match job.action {
                            Action::Start { root, metadata } => {
                                let mut disk = DiskRecording::start(&root, metadata)?;
                                disk.flush()?;
                                let snapshot = disk.snapshot();
                                runs.insert(
                                    key,
                                    OpenRun {
                                        state: job.state.clone(),
                                        disk,
                                    },
                                );
                                Ok(Some(Status::Ready(snapshot)))
                            }
                            Action::Finish => {
                                let run = runs
                                    .remove(&key)
                                    .ok_or("recording writer has no active run")?;
                                // Failure can be latched by the producer while a previous disk
                                // call was blocked. Such a run must never acquire completion metadata.
                                if job.state.failed.load(Ordering::SeqCst) {
                                    return Err("recording failed before completion".into());
                                }
                                run.disk.finish().map(|value| Some(Status::Finished(value)))
                            }
                            action => {
                                let run = runs
                                    .get_mut(&key)
                                    .ok_or("recording writer has no active run")?;
                                match action {
                                    Action::Event(value) => run.disk.event_value(value)?,
                                    Action::Telemetry {
                                        value,
                                        gap,
                                        unix_ms,
                                        elapsed_ms,
                                    } => run.disk.telemetry_at(&value, gap, unix_ms, elapsed_ms)?,
                                    Action::Dropped(count) => run.disk.dropped(count)?,
                                    Action::Flush(sequence) => {
                                        run.disk.flush()?;
                                        let mut progress = job
                                            .state
                                            .progress
                                            .lock()
                                            .map_err(|_| "recording progress poisoned")?;
                                        progress.flushed = sequence;
                                        progress.status = Status::Ready(run.disk.snapshot());
                                    }
                                    #[cfg(test)]
                                    Action::FailEventWrites => run
                                        .disk
                                        .fail_event_writes()
                                        .map_err(|error| error.to_string())?,
                                    Action::Start { .. } | Action::Finish => {
                                        return Err("invalid recording writer dispatch".into());
                                    }
                                }
                                Ok(None)
                            }
                        }
                    })();
                    match result {
                        Ok(Some(status)) => {
                            if let Ok(mut progress) = job.state.progress.lock() {
                                progress.status = status;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            job.state.failed.store(true, Ordering::SeqCst);
                            if let Ok(mut progress) = job.state.progress.lock() {
                                progress.status = Status::Failed(error);
                            }
                            runs.remove(&key);
                        }
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self { tx, active })
    }
}

pub(crate) struct Recording {
    writer: Writer,
    state: Arc<State>,
    cached: Value,
    failure: Option<String>,
    sequence: u64,
    flushed: u64,
    flush_requested: Option<u64>,
    finishing: bool,
    started_at: Instant,
    #[cfg(test)]
    flush_probe: Arc<AtomicUsize>,
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.state.abandoned.store(true, Ordering::SeqCst);
    }
}

impl Recording {
    #[cfg(test)]
    pub(crate) fn start(root: &Path, metadata: Value) -> Result<Self, String> {
        let mut recording = Self::begin(root, metadata)?;
        loop {
            match recording.poll_started() {
                Poll::Ready(result) => {
                    result?;
                    return Ok(recording);
                }
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_event_writes(&mut self) -> std::io::Result<()> {
        // Synchronize the injected I/O failure, rather than race the worker against
        // an arbitrary number of subsequent owner ticks.
        self.enqueue(Action::FailEventWrites)
            .map_err(std::io::Error::other)?;
        self.event("test_write_failure", json!({}))
            .map_err(std::io::Error::other)?;
        let barrier = self.barrier().map_err(std::io::Error::other)?;
        loop {
            match self.poll_barrier(barrier) {
                Poll::Ready(Err(_)) => return Ok(()),
                Poll::Ready(Ok(())) => {
                    return Err(std::io::Error::other(
                        "injected write unexpectedly succeeded",
                    ));
                }
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    pub(crate) fn begin(root: &Path, metadata: Value) -> Result<Self, String> {
        Self::begin_with_writer(root, metadata, Writer::global()?)
    }

    fn begin_with_writer(root: &Path, metadata: Value, writer: Writer) -> Result<Self, String> {
        validate_value(&metadata)?;
        writer
            .active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < MAX_RECORDINGS).then_some(count + 1)
            })
            .map_err(|_| "recording writer active-run capacity exhausted")?;
        let state = Arc::new(State {
            progress: Mutex::new(Progress {
                status: Status::Starting,
                flushed: 0,
            }),
            abandoned: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            active: writer.active.clone(),
        });
        let mut recording = Self {
            writer,
            state,
            cached: json!({"active":false,"pending":true}),
            failure: None,
            sequence: 0,
            flushed: 0,
            flush_requested: None,
            finishing: false,
            started_at: Instant::now(),
            #[cfg(test)]
            flush_probe: Arc::new(AtomicUsize::new(0)),
        };
        recording.enqueue(Action::Start {
            root: root.into(),
            metadata,
        })?;
        Ok(recording)
    }

    #[cfg(test)]
    pub(crate) fn poll_started(&mut self) -> Poll<Result<Value, String>> {
        match self.progress() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok((Status::Starting, _))) => Poll::Pending,
            Poll::Ready(Ok((Status::Ready(value), _))) => Poll::Ready(Ok(value)),
            Poll::Ready(Ok((Status::Finished(_), _))) => {
                Poll::Ready(Err("recording already finished".into()))
            }
            Poll::Ready(Ok((Status::Failed(error), _))) => Poll::Ready(Err(error)),
        }
    }

    pub(crate) fn event(&mut self, kind: &str, data: Value) -> Result<(), String> {
        self.enqueue(Action::Event(json!({"event":kind,"unix_time_ms":unix_time_ms(),"elapsed_ms":self.started_at.elapsed().as_millis(),"data":data})))
    }

    pub(crate) fn telemetry(&mut self, value: &Value, gap: bool) -> Result<(), String> {
        self.enqueue(Action::Telemetry {
            value: value.clone(),
            gap,
            unix_ms: unix_time_ms(),
            elapsed_ms: self.started_at.elapsed().as_millis(),
        })
    }

    pub(crate) fn dropped(&mut self, count: u64) -> Result<(), String> {
        if count == 0 {
            return Ok(());
        }
        self.enqueue(Action::Dropped(count))
    }

    #[cfg(test)]
    pub(crate) fn flush_probe(&self) -> Arc<AtomicUsize> {
        self.flush_probe.clone()
    }

    pub(crate) fn flush(&mut self) -> Result<(), String> {
        #[cfg(test)]
        self.flush_probe.fetch_add(1, Ordering::SeqCst);
        self.barrier().map(|_| ())
    }

    pub(crate) fn barrier(&mut self) -> Result<u64, String> {
        self.check_failure()?;
        let sequence = self.sequence;
        if self.flush_requested != Some(sequence) {
            self.enqueue_control(Action::Flush(sequence))?;
            self.flush_requested = Some(sequence);
        }
        Ok(sequence)
    }

    pub(crate) fn poll_barrier(&mut self, sequence: u64) -> Poll<Result<(), String>> {
        match self.progress() {
            Poll::Ready(Ok((Status::Ready(_), flushed))) if flushed >= sequence => {
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) | Poll::Ready(Ok((Status::Failed(error), _))) => {
                Poll::Ready(Err(error))
            }
            _ => Poll::Pending,
        }
    }

    pub(crate) fn begin_finish(&mut self) -> Result<(), String> {
        if self.finishing {
            return Ok(());
        }
        self.enqueue_control(Action::Finish)?;
        self.finishing = true;
        Ok(())
    }

    pub(crate) fn poll_finished(&mut self) -> Poll<Result<Value, String>> {
        match self.progress() {
            Poll::Ready(Ok((Status::Finished(value), _))) => Poll::Ready(Ok(value)),
            Poll::Ready(Err(error)) | Poll::Ready(Ok((Status::Failed(error), _))) => {
                Poll::Ready(Err(error))
            }
            _ => Poll::Pending,
        }
    }

    pub(crate) fn snapshot(&self) -> Value {
        let mut value = self.cached.clone();
        value["pending_writes"] = json!(self.sequence > self.flushed);
        value
    }

    fn progress(&mut self) -> Poll<Result<(Status, u64), String>> {
        if let Some(error) = &self.failure {
            return Poll::Ready(Err(error.clone()));
        }
        let progress = match self.state.progress.try_lock() {
            Ok(progress) => progress,
            Err(std::sync::TryLockError::WouldBlock) => return Poll::Pending,
            Err(_) => return Poll::Ready(Err("recording progress poisoned".into())),
        };
        let status = progress.status.clone();
        // A successful Finish flushes every previously queued write. No new
        // writes are accepted once finishing, so it acknowledges our sequence.
        let flushed = if matches!(status, Status::Finished(_)) {
            self.sequence
        } else {
            progress.flushed
        };
        drop(progress);
        self.flushed = flushed;
        match &status {
            Status::Ready(value) | Status::Finished(value) => self.cached = value.clone(),
            Status::Failed(error) => self.failure = Some(error.clone()),
            Status::Starting => {}
        }
        Poll::Ready(Ok((status, flushed)))
    }

    fn check_failure(&mut self) -> Result<(), String> {
        match self.progress() {
            Poll::Ready(Err(error)) | Poll::Ready(Ok((Status::Failed(error), _))) => Err(error),
            _ if self.finishing => Err("recording is finishing".into()),
            _ => Ok(()),
        }
    }

    fn enqueue(&mut self, action: Action) -> Result<(), String> {
        let validation = match &action {
            Action::Start { metadata, .. } => validate_value(metadata),
            Action::Event(value) | Action::Telemetry { value, .. } => validate_value(value),
            _ => Ok(()),
        };
        if let Err(error) = validation {
            return Err(self.fail(error));
        }
        let next = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| self.fail("recording sequence exhausted".into()))?;
        self.enqueue_control(action)?;
        self.sequence = next;
        Ok(())
    }

    fn enqueue_control(&mut self, action: Action) -> Result<(), String> {
        self.check_failure()?;
        match self.writer.tx.try_send(Job {
            state: self.state.clone(),
            action,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                Err(self.fail("recording writer queue full; evidence is partial".into()))
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(self.fail("recording writer disconnected; evidence is partial".into()))
            }
        }
    }

    fn fail(&mut self, error: String) -> String {
        self.state.failed.store(true, Ordering::SeqCst);
        self.failure = Some(error.clone());
        error
    }
}

fn validate_value(value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_VALUE_BYTES {
        Err("recording item exceeds bounded writer capacity".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "eha-writer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ))
    }

    fn wait<T>(mut poll: impl FnMut() -> Poll<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Poll::Ready(value) = poll() {
                return value;
            }
            assert!(Instant::now() < deadline, "test worker made progress");
            std::thread::yield_now();
        }
    }

    fn trial() -> super::super::super::TrialRequest {
        use super::super::super::{Command, TrialEnvelope, TrialRequest};
        TrialRequest {
            command: Command::Position { mm: 1.0 },
            envelope: TrialEnvelope {
                position_min_mm: -10.0,
                position_max_mm: 10.0,
                velocity_abs_max_mm_s: 2.0,
                force_abs_max_n: 10.0,
                stiffness_max_n_per_mm: 10.0,
                damping_max_ns_per_mm: 10.0,
                duration_max_s: 5.0,
            },
            duration_s: Some(5.0),
            reach: None,
        }
    }

    #[test]
    fn stalled_initial_audit_never_submits_target_and_explicit_stop_still_progresses() {
        use super::super::super::{
            Command,
            trial_tests::{FixtureBehavior, session_fixture},
        };
        let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        session.prepare_trial(&trial()).expect("preflight");
        let root = root();
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = stall.clone();
        let (entered, entered_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let writer = Writer::spawn(16, move || {
            if stalled.swap(false, Ordering::SeqCst) {
                entered.send(()).expect("audit stalled");
                release_rx.recv().expect("release audit");
            }
        })
        .expect("worker");
        let mut recording =
            Recording::begin_with_writer(&root, json!({}), writer).expect("recording");
        wait(|| recording.poll_started()).expect("metadata written");
        session.recording = Some(recording);
        stall.store(true, Ordering::SeqCst);
        session.begin_trial_start(trial()).expect("audit queued");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("audit stalled");
        assert!(session.poll_trial_start().is_pending());
        assert_eq!(probe.observed.lock().expect("observed").targets, 0);
        session
            .execute(Command::Stop)
            .expect("Stop does not wait for disk");
        assert!(
            matches!(session.poll_trial_start(), Poll::Ready(Err(error)) if &*error.kind == "cancelled" && error.local_stage.as_deref() == Some("not_submitted"))
        );
        assert!(session.trial.prepared.is_none());
        {
            let observed = probe.observed.lock().expect("observed");
            assert_eq!(observed.targets, 0);
            assert_eq!(observed.stops, 1);
        }
        release.send(()).expect("release");
        session.stop_recording().expect("recording finish");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn stalled_position_audit_does_not_flush_target_and_trial_stop_remains_available() {
        use super::super::super::trial_tests::{FixtureBehavior, session_fixture};
        let (mut session, probe) = session_fixture(1, [7; 12], FixtureBehavior::Ready);
        session.prepare_trial(&trial()).expect("preflight");
        let root = root();
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = stall.clone();
        let (entered, entered_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let writer = Writer::spawn(32, move || {
            if stalled.swap(false, Ordering::SeqCst) {
                entered.send(()).expect("position audit stalled");
                release_rx.recv().expect("release audit");
            }
        })
        .expect("worker");
        let mut recording =
            Recording::begin_with_writer(&root, json!({}), writer).expect("recording");
        wait(|| recording.poll_started()).expect("metadata written");
        session.recording = Some(recording);
        session
            .start_prepared_trial(trial())
            .expect("initial audited target");
        stall.store(true, Ordering::SeqCst);
        session.update_trial_position(2.0).expect("new position");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer stalled");
        session
            .trial
            .active
            .as_mut()
            .expect("trial")
            .last_position_submit = None;
        session.tick();
        session.tick();
        assert_eq!(
            probe.observed.lock().expect("observed").positions,
            vec![1.0]
        );
        session.stop_trial().expect("Stop is not behind audit");
        {
            let observed = probe.observed.lock().expect("observed");
            assert_eq!(observed.positions, vec![1.0]);
            assert_eq!(observed.stops, 1);
        }
        release.send(()).expect("release");
        session.stop_recording().expect("recording finish");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn stalled_start_and_full_queue_fail_without_waiting_or_completion() {
        let root = root();
        let (entered, entered_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let mut first = true;
        let writer = Writer::spawn(1, move || {
            if first {
                first = false;
                entered.send(()).expect("announce stall");
                release_rx.recv().expect("release stall");
            }
        })
        .expect("worker");
        let mut recording = Recording::begin_with_writer(&root, json!({}), writer.clone())
            .expect("begin without disk wait");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker stalled");
        assert!(recording.poll_started().is_pending());
        recording
            .event("queued", json!({}))
            .expect("one reserved queue item");
        assert!(
            recording
                .event("overflow", json!({}))
                .expect_err("full queue")
                .contains("queue full")
        );
        assert!(matches!(recording.poll_started(), Poll::Ready(Err(_))));
        assert!(recording.begin_finish().is_err());
        drop(recording); // Must not join the stalled writer.
        release.send(()).expect("release");
        wait(|| {
            if writer.active.load(Ordering::SeqCst) == 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        assert!(
            !root.exists(),
            "failed start cannot write completion metadata"
        );
    }

    #[test]
    fn barrier_is_acknowledged_after_real_flush_and_preserves_record_format() {
        let root = root();
        let mut recording = Recording::begin_with_writer(
            &root,
            json!({"device_uid":"a"}),
            Writer::spawn(8, || {}).expect("worker"),
        )
        .expect("begin");
        let started = wait(|| recording.poll_started()).expect("started");
        let event =
            json!({"event":"observed","unix_time_ms":7,"elapsed_ms":3,"data":{"value":2.5}});
        recording.enqueue(Action::Event(event)).expect("event");
        recording.telemetry(&json!({"sample":{"snapshot_time_us":7,"snapshot_sequence":3},"main_values":[{"value":2.5,"result":1,"quality":1,"stale":false}]}), false).expect("telemetry");
        let barrier = recording.barrier().expect("barrier");
        wait(|| recording.poll_barrier(barrier)).expect("flushed");
        assert_eq!(recording.snapshot()["pending_writes"], false);
        let directory = std::path::PathBuf::from(started["directory"].as_str().expect("directory"));
        let events =
            std::fs::read_to_string(directory.join("events.jsonl")).expect("flushed events");
        assert!(events.contains("\"unix_time_ms\":7"));
        let csv =
            std::fs::read_to_string(directory.join("telemetry.csv")).expect("flushed telemetry");
        assert!(csv.contains("position_mm,position_result,position_quality,position_stale"));
        assert!(csv.contains("\"2.5\",\"1\",\"1\",\"false\""));
        // Finish must acknowledge writes queued after the last explicit barrier.
        recording.event("after_barrier", json!({})).expect("event");
        assert_eq!(recording.snapshot()["pending_writes"], true);
        recording.begin_finish().expect("finish queued");
        let completed = wait(|| recording.poll_finished()).expect("finished");
        assert_eq!(completed["active"], false);
        assert_eq!(recording.snapshot()["pending_writes"], false);
        let metadata: Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("metadata.json")).expect("metadata"),
        )
        .expect("JSON");
        assert_eq!(metadata["completion"], completed);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn slow_flush_keeps_audit_pending_while_owner_can_continue() {
        let root = root();
        let (entered, entered_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let mut count = 0;
        let writer = Writer::spawn(4, move || {
            count += 1;
            if count == 3 {
                entered.send(()).expect("announce flush stall");
                release_rx.recv().expect("release flush");
            }
        })
        .expect("worker");
        let mut recording = Recording::begin_with_writer(&root, json!({}), writer).expect("begin");
        wait(|| recording.poll_started()).expect("started");
        recording
            .event("trial_command_input", json!({}))
            .expect("audit");
        let barrier = recording.barrier().expect("barrier");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("flush stalled");
        assert!(recording.poll_barrier(barrier).is_pending());
        recording
            .event("owner_can_continue", json!({}))
            .expect("nonblocking enqueue");
        assert!(recording.poll_barrier(barrier).is_pending());
        release.send(()).expect("release");
        wait(|| recording.poll_barrier(barrier)).expect("audit ack");
        recording.begin_finish().expect("finish");
        wait(|| recording.poll_finished()).expect("completed");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn disk_failure_latches_and_cannot_be_reported_as_completed() {
        let root = root();
        let mut recording = Recording::begin_with_writer(
            &root,
            json!({}),
            Writer::spawn(8, || {}).expect("worker"),
        )
        .expect("begin");
        let started = wait(|| recording.poll_started()).expect("started");
        recording
            .enqueue_control(Action::FailEventWrites)
            .expect("inject readonly file");
        recording
            .event("cannot_write", json!({}))
            .expect("enqueue before failure observed");
        let barrier = recording.barrier().expect("queue flush");
        assert!(wait(|| recording.poll_barrier(barrier)).is_err());
        assert!(recording.begin_finish().is_err());
        let directory = std::path::PathBuf::from(started["directory"].as_str().expect("directory"));
        let metadata: Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("metadata.json")).expect("metadata"),
        )
        .expect("JSON");
        assert!(metadata.get("completion").is_none());
        drop(recording);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
