// Copyright The eha-sdk Contributors

//! CAN 网络构造的单 worker。
//!
//! 构造网络可能等待 Python runtime、适配器发现或通道打开；这些工作不能占用服务
//! owner。worker 只移交新建的网络，不持有 Client、session 或后续连接器。

use crate::connections::ConnectionProfile;
use eha_sdk::can::CanNetwork;
use std::{
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread::{self, JoinHandle},
};

#[derive(Clone, Debug)]
pub(super) enum Operation {
    Advanced {
        channel: String,
        mode: String,
        python: Option<String>,
    },
    Managed(ConnectionProfile),
}

/// 完成前只由 worker 持有网络；owner 取走后才可附加 session。
pub(super) struct NetworkPrepared {
    pub(super) network: CanNetwork,
    pub(super) channel: String,
    pub(super) mode: String,
    pub(super) python: Option<String>,
}

pub(super) type NetworkResult = Result<NetworkPrepared, String>;
pub(super) type NetworkRunner = Box<dyn Fn(Operation) -> NetworkResult + Send + 'static>;

struct Job {
    operation: Operation,
    retired: Option<NetworkPrepared>,
}

/// 一项构造任务和一项待领取结果均有固定容量；owner 负责在提交前维护 busy 门禁。
pub(super) struct NetworkJobs {
    jobs: SyncSender<Job>,
    completed: Receiver<NetworkResult>,
    _worker: JoinHandle<()>,
}

impl NetworkJobs {
    pub(super) fn new() -> Self {
        Self::with_runner(Box::new(prepare_network))
    }

    #[cfg(test)]
    pub(super) fn with_test_runner(runner: NetworkRunner) -> Self {
        Self::with_runner(runner)
    }

    fn with_runner(runner: NetworkRunner) -> Self {
        let (jobs, receiver) = mpsc::sync_channel(1);
        let (completed_sender, completed) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("eha-tool-network-jobs".into())
            .spawn(move || run_network_jobs(receiver, completed_sender, runner))
            .expect("启动 CAN 网络构造线程");
        Self {
            jobs,
            completed,
            _worker: worker,
        }
    }

    /// 不排队等待；无法提交时把旧网络交还给 owner，避免隐式丢弃唯一所有权。
    ///
    /// Worker 在运行构造器前释放 `retired`，所以重连不会让 owner 阻塞等待旧网络销毁。
    #[allow(
        clippy::result_large_err,
        reason = "提交失败时必须将唯一持有的旧网络交还给 owner。"
    )]
    pub(super) fn start_replacing(
        &self,
        operation: Operation,
        retired: Option<NetworkPrepared>,
    ) -> Result<(), (String, Option<NetworkPrepared>)> {
        match self.jobs.try_send(Job { operation, retired }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job)) => Err((
                "CAN 网络构造正在进行；请等待当前任务完成".into(),
                job.retired,
            )),
            Err(TrySendError::Disconnected(job)) => {
                Err(("CAN 网络构造线程不可用".into(), job.retired))
            }
        }
    }

    #[cfg(test)]
    pub(super) fn start(&self, operation: Operation) -> Result<(), String> {
        self.start_replacing(operation, None)
            .map_err(|(error, _)| error)
    }

    pub(super) fn take_completed(&self) -> Option<NetworkResult> {
        match self.completed.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            // The owner clears its pending marker after consuming this result, so a
            // disconnected worker cannot leave the CAN lifecycle busy forever.
            Err(TryRecvError::Disconnected) => Some(Err("CAN 网络构造线程已停止".into())),
        }
    }
}

fn run_network_jobs(
    receiver: Receiver<Job>,
    completed: SyncSender<NetworkResult>,
    runner: NetworkRunner,
) {
    while let Ok(Job { operation, retired }) = receiver.recv() {
        drop(retired);
        if completed.send(runner(operation)).is_err() {
            return;
        }
    }
}

fn prepare_network(operation: Operation) -> NetworkResult {
    match operation {
        Operation::Advanced {
            channel,
            mode,
            python,
        } => {
            let resolved_python = crate::runtime::python(python.as_deref())?;
            let factory =
                crate::connections::advanced_factory(channel.clone(), Some(resolved_python))?;
            let parsed = crate::session::parse_can_mode(&mode).map_err(|error| error.message)?;
            let network = CanNetwork::new(factory, parsed)?;
            Ok(NetworkPrepared {
                network,
                channel,
                mode,
                python,
            })
        }
        Operation::Managed(profile) => {
            let python = crate::runtime::python(None)?;
            let channel = format!("managed:{}", profile.adapter.serial);
            let mode = profile.mode.clone();
            let parsed = crate::session::parse_can_mode(&mode).map_err(|error| error.message)?;
            let factory =
                crate::connections::managed_factory(profile.adapter.serial, python.clone());
            let network = CanNetwork::new(factory, parsed)?;
            Ok(NetworkPrepared {
                network,
                channel,
                mode,
                python: Some(python),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NetworkJobs, Operation};
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    #[test]
    fn pending_network_construction_does_not_block_completion_polling() {
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let runner_started = Arc::clone(&started);
        let runner_release = Arc::clone(&release);
        let jobs = NetworkJobs::with_test_runner(Box::new(move |_| {
            runner_started.wait();
            runner_release.wait();
            Err("fixture failure".into())
        }));
        jobs.start(Operation::Advanced {
            channel: "fixture".into(),
            mode: "fd".into(),
            python: None,
        })
        .expect("submit slow construction");
        started.wait();
        assert!(jobs.take_completed().is_none());
        release.wait();

        let mut completed = None;
        for _ in 0..10_000 {
            if let Some(result) = jobs.take_completed() {
                completed = Some(result);
                break;
            }
            thread::yield_now();
        }
        assert!(completed.is_some(), "network worker did not complete");
        let result = completed.expect("completion was asserted");
        assert!(
            result.is_err(),
            "fixture runner unexpectedly prepared a network"
        );
        let Err(error) = result else {
            return;
        };
        assert_eq!(error, "fixture failure");
    }
}
