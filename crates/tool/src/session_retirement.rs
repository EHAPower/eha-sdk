// Copyright The eha-sdk Contributors

//! 单进程有界物理退役。连接前预留容量；退役与 Drop 都不在 owner 中 join。
//! Client 与 OwnerGuard 只有一个所有者，真实 disconnect 返回后才释放锁并发布 ClientState。

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex, OnceLock, TryLockError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    task::Poll,
    thread,
};

use crate::owner_lock::OwnerGuard;
use eha_sdk::host::{Client, ClientState};

const CAPACITY: usize = 64;
static WORKER: OnceLock<Arc<Shared>> = OnceLock::new();

struct Queues {
    jobs: VecDeque<Job>,
    // 故障时仍计入连接前预留额度，保留所有权；不是成功清理或无界 orphan。
    quarantined: Vec<Job>,
}
struct Shared {
    capacity: usize,
    reservations: AtomicUsize,
    healthy: AtomicBool,
    failure: Mutex<Option<String>>,
    queues: Mutex<Queues>,
    wake: Condvar,
}

/// 必须在创建 owner-managed Client 前取得；每个槽位最多持有一个 Client/job。
pub(super) struct RetirementSlot {
    shared: Arc<Shared>,
}
impl RetirementSlot {
    pub(super) fn reserve() -> Result<Self, String> {
        WORKER.get_or_init(Shared::start).reserve()
    }
}
impl Drop for RetirementSlot {
    fn drop(&mut self) {
        self.shared.reservations.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Job {
    // 防御性字段顺序：即使 worker unwinding，也先关 Client 再释放通路锁。
    client: Option<Client>,
    owner: Option<OwnerGuard>,
    slot: Option<RetirementSlot>,
    completed: SyncSender<ClientState>,
}

/// 错误并不表示物理退役完成。失败时保留此 handle，并继续禁止新的连接。
pub(super) struct Retirement {
    local: Option<Job>,
    completed: Receiver<ClientState>,
    shared: Arc<Shared>,
    finished: bool,
}
impl Retirement {
    pub(super) fn begin(client: Client, owner: Option<OwnerGuard>, slot: RetirementSlot) -> Self {
        let shared = slot.shared.clone();
        let (completed, received) = mpsc::sync_channel(1);
        let mut retirement = Self {
            local: Some(Job {
                client: Some(client),
                owner,
                slot: Some(slot),
                completed,
            }),
            completed: received,
            shared,
            finished: false,
        };
        // try_lock 不等待 worker；交接失败仍由当前 session handle 保有 Client/guard。
        retirement.try_handoff();
        retirement
    }

    fn try_handoff(&mut self) {
        if self.local.is_none() || !self.shared.healthy.load(Ordering::Acquire) {
            return;
        }
        let mut queues = match self.shared.queues.try_lock() {
            Ok(queues) => queues,
            Err(TryLockError::WouldBlock) => return,
            Err(TryLockError::Poisoned(_)) => {
                self.shared
                    .fail("retirement_worker_unavailable: 退役队列已损坏".into());
                return;
            }
        };
        if queues.jobs.len() >= self.shared.capacity {
            return;
        }
        if let Some(job) = self.local.take() {
            queues.jobs.push_back(job);
            self.shared.wake.notify_one();
        }
    }

    /// 只领取结果，不 join、不等待 I/O。Ready(Err) 时仍未完成，不能据此重连。
    pub(super) fn poll(&mut self) -> Poll<Result<ClientState, String>> {
        if self.finished {
            return Poll::Ready(Err("retirement_result_taken: 退役结果已领取".into()));
        }
        self.try_handoff();
        match self.completed.try_recv() {
            Ok(state) => {
                self.finished = true;
                Poll::Ready(Ok(state))
            }
            Err(TryRecvError::Empty) if self.shared.healthy.load(Ordering::Acquire) => {
                Poll::Pending
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                Poll::Ready(Err(self.shared.error()))
            }
        }
    }
}
impl Drop for Retirement {
    fn drop(&mut self) {
        let Some(job) = self.local.take() else {
            return;
        };
        // 连接前已预留该 job 的位置，所有存活槽位总数 <=capacity，故存放不扩张。
        // 此临界区不含 I/O/join；worker 故障或队列暂忙也不能在 owner 中析构 Client。
        let mut queues = self
            .shared
            .queues
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.shared.healthy.load(Ordering::Acquire) && queues.jobs.len() < self.shared.capacity {
            queues.jobs.push_back(job);
            self.shared.wake.notify_one();
        } else {
            queues.quarantined.push(job);
        }
    }
}

impl Shared {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            reservations: AtomicUsize::new(0),
            healthy: AtomicBool::new(true),
            failure: Mutex::new(None),
            queues: Mutex::new(Queues {
                jobs: VecDeque::with_capacity(capacity),
                quarantined: Vec::with_capacity(capacity),
            }),
            wake: Condvar::new(),
        })
    }
    fn start() -> Arc<Self> {
        Self::start_with_builder(
            thread::Builder::new().name("eha-tool-retirement".into()),
            CAPACITY,
        )
    }
    fn start_with_builder(builder: thread::Builder, capacity: usize) -> Arc<Self> {
        let shared = Self::new(capacity);
        let worker = shared.clone();
        if let Err(error) = builder.spawn(move || {
            let _liveness = Liveness(worker.clone());
            worker.run();
        }) {
            shared.fail(format!(
                "retirement_worker_unavailable: 无法启动退役线程：{error}"
            ));
        }
        shared
    }
    fn reserve(self: &Arc<Self>) -> Result<RetirementSlot, String> {
        if !self.healthy.load(Ordering::Acquire) {
            return Err(self.error());
        }
        self.reservations
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.capacity).then_some(count + 1)
            })
            .map_err(|_| "retirement_capacity_full: 物理退役容量已满，未打开新连接".to_owned())?;
        let slot = RetirementSlot {
            shared: self.clone(),
        };
        if !self.healthy.load(Ordering::Acquire) {
            return Err(self.error());
        }
        Ok(slot)
    }
    fn fail(&self, message: String) {
        self.healthy.store(false, Ordering::Release);
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if failure.is_none() {
            *failure = Some(message);
        }
        self.wake.notify_all();
    }
    fn error(&self) -> String {
        self.failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .unwrap_or_else(|| "retirement_worker_unavailable: 物理退役尚未完成".into())
    }
    fn run(&self) {
        loop {
            let job = {
                let mut queues = self
                    .queues
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                while queues.jobs.is_empty() && self.healthy.load(Ordering::Acquire) {
                    queues = self
                        .wake
                        .wait(queues)
                        .unwrap_or_else(|error| error.into_inner());
                }
                if !self.healthy.load(Ordering::Acquire) {
                    return;
                }
                queues.jobs.pop_front()
            };
            if let Some(job) = job {
                finish(job);
            }
        }
    }
}
struct Liveness(Arc<Shared>);
impl Drop for Liveness {
    fn drop(&mut self) {
        self.0
            .fail("retirement_worker_unavailable: 退役线程已退出，未完成通路仍隔离".into());
    }
}
fn finish(mut job: Job) {
    if let Some(client) = job.client.take() {
        let state = client.disconnect(); // 唯一允许真实 Backend Drop/join 的线程。
        drop(job.owner.take());
        drop(job.slot.take());
        // 每个 job 仅发布一次；接收者退出只丢弃状态，不引发 I/O，不发送 Stop。
        let _ = job.completed.try_send(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_lock::ResourceKey;
    use eha_sdk::host::{
        Wait,
        backend::{Backend, Command},
    };
    use std::{
        sync::Barrier,
        time::{Duration, Instant},
    };

    static NEXT_KEY: AtomicUsize = AtomicUsize::new(1);
    fn owner() -> (ResourceKey, OwnerGuard) {
        let serial = format!(
            "retirement-test-{}-{}",
            std::process::id(),
            NEXT_KEY.fetch_add(1, Ordering::Relaxed)
        );
        let key = ResourceKey::usb(&serial).expect("合成key");
        let guard = OwnerGuard::acquire(&key).expect("合成owner");
        (key, guard)
    }
    fn busy(key: &ResourceKey) -> bool {
        OwnerGuard::acquire(key)
            .err()
            .is_some_and(|error| error.code == "owner_busy")
    }
    fn backend() -> Client {
        Client::new(
            Backend::spawn("retirement-synthetic", |commands, _| {
                assert!(matches!(
                    commands.recv().expect("只收到Close"),
                    Command::Close
                ));
            })
            .expect("合成Backend"),
        )
    }
    fn blocked_backend() -> (Client, Receiver<()>, Arc<Barrier>, Arc<AtomicBool>) {
        let (closing, close_rx) = mpsc::sync_channel(1);
        let release = Arc::new(Barrier::new(2));
        let worker_release = release.clone();
        let done = Arc::new(AtomicBool::new(false));
        let worker_done = done.clone();
        let client = Client::new(
            Backend::spawn("retirement-blocked-synthetic", move |commands, _| {
                assert!(matches!(
                    commands.recv().expect("仅Close，无Stop/Heartbeat"),
                    Command::Close
                ));
                closing.send(()).expect("join开始");
                worker_release.wait();
                worker_done.store(true, Ordering::Release);
            })
            .expect("合成Backend"),
        );
        (client, close_rx, release, done)
    }
    fn complete(retirement: &mut Retirement) -> ClientState {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match retirement.poll() {
                Poll::Ready(result) => return result.expect("退役完成"),
                Poll::Pending => {
                    assert!(Instant::now() < deadline, "合成退役期限");
                    thread::yield_now();
                }
            }
        }
    }
    fn take_job(shared: &Shared) -> Job {
        shared
            .queues
            .lock()
            .expect("测试队列")
            .jobs
            .pop_front()
            .expect("队列job")
    }

    #[test]
    fn guard_is_held_while_joining_and_owner_poll_and_drop_do_not_join() {
        let slot = RetirementSlot::reserve().expect("预留槽");
        let (key, guard) = owner();
        let (client, closing, release, done) = blocked_backend();
        let start = Instant::now();
        let mut retiring = Retirement::begin(client, Some(guard), slot);
        assert!(start.elapsed() < Duration::from_millis(100));
        closing
            .recv_timeout(Duration::from_secs(1))
            .expect("worker开始真实join");
        assert!(matches!(retiring.poll(), Poll::Pending));
        assert!(busy(&key));
        assert!(!done.load(Ordering::Acquire));
        let start = Instant::now();
        drop(retiring);
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(busy(&key));
        release.wait();
        let deadline = Instant::now() + Duration::from_secs(2);
        while busy(&key) {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(done.load(Ordering::Acquire));
    }

    #[test]
    fn capacity_is_reserved_before_clients_and_recovers_only_after_real_cleanup() {
        let shared = Shared::new(1);
        let slot = shared.reserve().expect("唯一预留槽");
        assert!(
            shared
                .reserve()
                .err()
                .is_some_and(|error| error.starts_with("retirement_capacity_full"))
        );
        let mut retiring = Retirement::begin(backend(), None, slot);
        assert!(matches!(retiring.poll(), Poll::Pending));
        assert_eq!(shared.reservations.load(Ordering::Acquire), 1);
        assert!(shared.reserve().is_err());
        let job = take_job(&shared);
        thread::spawn(move || finish(job))
            .join()
            .expect("受控worker");
        let _ = complete(&mut retiring);
        drop(shared.reserve().expect("真实清理后可预留"));
    }

    #[test]
    fn busy_handoff_and_session_drop_preserve_job_without_owner_cleanup() {
        let shared = Shared::new(1);
        let slot = shared.reserve().expect("预留");
        let (key, guard) = owner();
        let queues = shared.queues.lock().expect("暂忙队列");
        let start = Instant::now();
        let mut retiring = Retirement::begin(backend(), Some(guard), slot);
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(retiring.local.is_some());
        assert!(matches!(retiring.poll(), Poll::Pending));
        assert!(busy(&key));
        drop(queues);
        drop(retiring);
        assert_eq!(shared.queues.lock().expect("queued").jobs.len(), 1);
        assert!(busy(&key));
        let job = take_job(&shared);
        thread::spawn(move || finish(job)).join().expect("受控回收");
        drop(OwnerGuard::acquire(&key).expect("接收者卸载后仍由worker真正释放"));
        assert_eq!(shared.reservations.load(Ordering::Acquire), 0);
    }

    #[test]
    fn worker_fault_preserves_local_client_guard_and_bounded_quarantine_on_drop() {
        let shared = Shared::new(1);
        let slot = shared.reserve().expect("预留");
        let (key, guard) = owner();
        shared.fail("retirement_worker_unavailable: 合成线程故障".into());
        let start = Instant::now();
        let mut retiring = Retirement::begin(backend(), Some(guard), slot);
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(retiring.local.is_some());
        assert!(matches!(retiring.poll(), Poll::Ready(Err(_))));
        assert!(busy(&key));
        assert!(shared.reserve().is_err());
        drop(retiring);
        let job = shared
            .queues
            .lock()
            .expect("隔离队列")
            .quarantined
            .pop()
            .expect("有界隔离job");
        assert!(busy(&key));
        assert_eq!(shared.reservations.load(Ordering::Acquire), 1);
        // 仅夹具显式清理隔离项；生产故障不创建恢复线程或自动重放。
        thread::spawn(move || finish(job)).join().expect("夹具回收");
        assert!(!busy(&key));
        assert_eq!(shared.reservations.load(Ordering::Acquire), 0);
    }

    #[test]
    fn actual_spawn_failure_refuses_slots_before_any_client_is_created() {
        let shared = Shared::start_with_builder(thread::Builder::new().stack_size(1usize << 62), 1);
        assert!(
            shared
                .reserve()
                .err()
                .is_some_and(|error| error.starts_with("retirement_worker_unavailable"))
        );
        assert_eq!(shared.reservations.load(Ordering::Acquire), 0);
        assert!(shared.queues.lock().expect("未打开Client").jobs.is_empty());
    }

    #[test]
    fn failure_after_handoff_keeps_queued_owner_and_projects_unfinished_error() {
        let shared = Shared::new(1);
        let (key, guard) = owner();
        let mut retiring =
            Retirement::begin(backend(), Some(guard), shared.reserve().expect("预留"));
        shared.fail("retirement_worker_unavailable: 合成worker退出".into());
        assert!(retiring.local.is_none());
        assert!(matches!(retiring.poll(), Poll::Ready(Err(_))));
        assert!(busy(&key));
        assert_eq!(shared.queues.lock().expect("故障队列").jobs.len(), 1);
        drop(retiring);
        let job = take_job(&shared);
        thread::spawn(move || finish(job)).join().expect("夹具回收");
        assert!(!busy(&key));
    }

    #[test]
    fn returned_client_state_preserves_local_submission_sequence() {
        let (seen_tx, seen_rx) = mpsc::sync_channel(2);
        let backend = Backend::spawn("retirement-query-sequence", move |commands, _| {
            while let Ok(command) = commands.recv() {
                match command {
                    Command::Send(request) | Command::CancellableSend(request, _) => {
                        seen_tx.send(request.id).expect("query id")
                    }
                    Command::Close => break,
                    other => assert!(matches!(other, Command::Close), "不发送其他动作"),
                }
            }
        })
        .expect("backend");
        let mut client = Client::new(backend);
        let wait = Wait {
            timeout: Duration::from_millis(2),
            ..Wait::default()
        };
        client
            .identify(None, &wait)
            .expect_err("合成无回复，只消费原查询编号");
        let first = seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("已编码第一查询");
        let shared = Shared::new(1);
        let mut retiring = Retirement::begin(client, None, shared.reserve().expect("slot"));
        let job = take_job(&shared);
        thread::spawn(move || finish(job)).join().expect("回收");
        let state = complete(&mut retiring);
        let (next_tx, next_rx) = mpsc::sync_channel(1);
        let backend = Backend::spawn("retirement-resumed-sequence", move |commands, _| {
            while let Ok(command) = commands.recv() {
                match command {
                    Command::Send(request) | Command::CancellableSend(request, _) => {
                        next_tx.send(request.id).expect("resumed id")
                    }
                    Command::Close => break,
                    other => assert!(matches!(other, Command::Close), "不自动恢复动作"),
                }
            }
        })
        .expect("resume backend");
        let mut resumed = Client::resume(backend, state);
        resumed
            .identify(None, &wait)
            .expect_err("明确重核身份，合成无回复");
        let next = next_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("第二查询");
        assert!(next > first);
        drop(resumed);
    }
}
