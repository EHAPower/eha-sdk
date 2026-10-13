// Copyright The eha-sdk Contributors

//! Trial Startup/Status preflight shares the session's one owned read slot.
//! No target is submitted here. The service must poll between priority actions;
//! the legacy prepare_trial wrapper remains synchronous.

use std::task::Poll;

use super::{scheduler::ReadOwner, *};

pub(super) struct PendingTrial {
    request: TrialRequest,
    startup: Option<Value>,
}

impl ToolSession {
    pub(crate) fn begin_trial_preflight(
        &mut self,
        request: TrialRequest,
    ) -> Result<(), SessionError> {
        self.require_read_slot_idle()?;
        if self.pending_preflight.is_some() {
            return Err(invalid("已有尚未领取的试验预检"));
        }
        if self.trial.active.is_some() {
            return Err(invalid("已有进行中的试验"));
        }
        if self
            .trial
            .interrupted
            .as_ref()
            .is_some_and(|trial| !trial.unrecoverable)
        {
            return Err(invalid(
                "上次试验中断且结果未知；重连并核对同一运行实例后请显式 Stop",
            ));
        }
        validate_trial_request(&request)?;
        // Starting a new attempt invalidates old admission evidence, including when
        // this attempt is later cancelled. It must not expose an earlier prepared target.
        self.trial.prepared = None;
        self.trial.interrupted = None;
        self.begin_read_owned(
            Command::ConfigRead {
                view: "startup".into(),
            },
            ReadOwner::TrialPreflight,
        )?;
        self.pending_preflight = Some(PendingTrial {
            request,
            startup: None,
        });
        Ok(())
    }

    /// One SDK poll per call; a completed Startup starts Status and yields Pending.
    /// Stop/heartbeat cancels the shared read, so no prepared admission is produced.
    pub(crate) fn poll_trial_preflight(&mut self) -> Poll<Result<(), SessionError>> {
        let Some(mut pending) = self.pending_preflight.take() else {
            return Poll::Ready(Err(invalid("没有待领取的试验预检")));
        };
        let result = match self.poll_read_owned(ReadOwner::TrialPreflight) {
            Poll::Pending => {
                self.pending_preflight = Some(pending);
                return Poll::Pending;
            }
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Ready(Ok(result)) => result,
        };
        let Some(startup) = pending.startup.take() else {
            pending.startup = Some(result.data);
            if let Err(error) = self.begin_read_owned(Command::Status, ReadOwner::TrialPreflight) {
                return Poll::Ready(Err(error));
            }
            self.pending_preflight = Some(pending);
            return Poll::Pending;
        };
        let status = result.data;
        let request = pending.request;
        if let Err(error) = validate_trial_facts(&request, &startup, &status, self.request.as_ref())
        {
            return Poll::Ready(Err(error));
        }
        let Some(identity) = self
            .identity
            .as_ref()
            .and_then(|reply| reply_json(reply).ok())
        else {
            return Poll::Ready(Err(invalid("Identity 不可读")));
        };
        self.record_event(
            "trial_prepared",
            json!({"request": &request, "startup": &startup, "status": &status}),
        );
        self.trial.completed = None;
        self.trial.prepared = Some(PreparedTrial {
            request,
            status,
            startup,
            identity,
            connection: self.request.clone(),
        });
        Poll::Ready(Ok(()))
    }
}
