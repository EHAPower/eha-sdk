// Copyright The eha-sdk Contributors

//! Pollable local maintenance submission on the same exclusive Client.

use super::*;
use crate::session::MutationMatch;

pub(super) struct PendingSubmission {
    matcher: MutationMatch,
    id: u64,
    deadline: Instant,
    wait: Wait,
}

impl Client {
    /// Validate and enqueue one SaveAndRestart without waiting for local submission.
    /// Poll this same Client with [`Self::poll_submission`]; no device success is implied.
    pub fn begin_save_and_restart(&mut self, record: &[u8], wait: &Wait) -> Result<(), Error> {
        self.require_idle()?;
        crate::configuration::validate_json(record).map_err(|error| invalid(error.to_string()))?;
        self.begin_mutation_submission(MutationKind::SaveAndRestart, Some(record), wait)
    }

    /// Enqueue one ResetApplication without waiting for local submission.
    pub fn begin_reset_application(&mut self, wait: &Wait) -> Result<(), Error> {
        self.begin_mutation_submission(MutationKind::ResetApplication, None, wait)
    }

    /// Enqueue one EnterUpdate without waiting for local submission.
    pub fn begin_enter_update(&mut self, wait: &Wait) -> Result<(), Error> {
        self.begin_mutation_submission(MutationKind::EnterUpdate, None, wait)
    }

    fn begin_mutation_submission(
        &mut self,
        kind: MutationKind,
        record: Option<&[u8]>,
        wait: &Wait,
    ) -> Result<(), Error> {
        let (matcher, id, deadline, _) = self.prepare_mutation_submission(kind, record, wait)?;
        self.pending_submission = Some(PendingSubmission {
            matcher,
            id,
            deadline,
            wait: wait.clone(),
        });
        Ok(())
    }

    pub(super) fn prepare_mutation_submission(
        &mut self,
        kind: MutationKind,
        record: Option<&[u8]>,
        wait: &Wait,
    ) -> Result<(MutationMatch, u64, Instant, Arc<AtomicBool>), Error> {
        self.require_idle()?;
        self.require_identity()?;
        let matcher = self.session.next_mutation(kind).map_err(|error| {
            self.query_error(
                Failure::Session(format!("{error:?}")),
                LocalStage::NotSubmitted,
            )
        })?;
        let bytes = matcher.identity.to_bytes();
        let identity =
            ExpectedIdentity::new(&bytes).map_err(|error| invalid(format!("{error:?}")))?;
        let message = match kind {
            MutationKind::SaveAndRestart => Message::SaveAndRestart {
                query_id: matcher.query_id,
                identity,
                record: record.ok_or_else(|| invalid("缺少完整记录"))?,
            },
            MutationKind::ResetApplication => Message::ResetApplication {
                query_id: matcher.query_id,
                identity,
            },
            MutationKind::EnterUpdate => Message::EnterUpdate {
                query_id: matcher.query_id,
                identity,
            },
        };
        let (id, deadline, cancel) = self.send(&message, wait)?;
        Ok((matcher, id, deadline, cancel))
    }

    /// Consume at most 16 events, using only try_lock; no sleep or device reply wait.
    /// Submitted proves only the existing local I/O boundary. IDs and mutation rejection
    /// matchers remain specific to this attempt, so late events cannot finish a later attempt.
    pub fn poll_submission(&mut self) -> Poll<Result<LocalSubmission, Error>> {
        let Some(pending) = self.pending_submission.take() else {
            return Poll::Ready(Err(invalid("没有等待中的维护提交")));
        };
        for _ in 0..16 {
            let event = self.try_pending_event(&pending.wait, pending.deadline, Instant::now());
            match event {
                Ok(Some(Event::Submitted(got))) if got == pending.id => {
                    return Poll::Ready(Ok(LocalSubmission {
                        id: pending.id,
                        submitted_at: Instant::now(),
                        boundary: self.backend.boundary,
                    }));
                }
                Ok(Some(Event::Failed(got, failure))) if got == pending.id => {
                    let local = if failure.may_have_sent {
                        LocalStage::MayHaveBeenSubmitted
                    } else {
                        LocalStage::NotSubmitted
                    };
                    return Poll::Ready(Err(self.submission_poll_error(
                        Failure::Transport {
                            detail: failure.detail,
                            raw: Vec::new(),
                        },
                        local,
                    )));
                }
                Ok(Some(Event::Message(received))) => {
                    let reply: Reply = received.into();
                    if let Ok(response @ Response::DataUnavailable(value)) = reply.response()
                        && pending.matcher.matches(&response)
                    {
                        return Poll::Ready(Err(self.query_error(
                            Failure::DataUnavailable(value.fields()),
                            LocalStage::MayHaveBeenSubmitted,
                        )));
                    }
                }
                Ok(Some(Event::Disconnected(reason))) => {
                    return Poll::Ready(Err(self.submission_poll_error(
                        Failure::Disconnected(reason),
                        LocalStage::MayHaveBeenSubmitted,
                    )));
                }
                Ok(Some(Event::TransportError { detail, raw })) => {
                    return Poll::Ready(Err(self.submission_poll_error(
                        Failure::Transport { detail, raw },
                        LocalStage::MayHaveBeenSubmitted,
                    )));
                }
                Err(failure) => {
                    return Poll::Ready(Err(
                        self.submission_poll_error(failure, LocalStage::MayHaveBeenSubmitted)
                    ));
                }
                Ok(None) => break,
                Ok(_) => {}
            }
        }
        self.pending_submission = Some(pending);
        Poll::Pending
    }

    fn submission_poll_error(&self, failure: Failure, local: LocalStage) -> Error {
        let failure = if local != LocalStage::NotSubmitted {
            Failure::ResultUnknown(format!("{failure:?}"))
        } else {
            failure
        };
        self.query_error(failure, local)
    }

    /// End only the local waiting slot. It does not retract, cancel or replay the
    /// maintenance bytes already queued to I/O, whose original send deadline remains.
    /// MayHaveBeenSubmitted must stay unknown in the caller's operation ledger.
    pub fn cancel_submission(&mut self) -> Result<LocalStage, Error> {
        self.pending_submission
            .take()
            .map(|_| LocalStage::MayHaveBeenSubmitted)
            .ok_or_else(|| invalid("没有等待中的维护提交"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> (
        Client,
        std::sync::mpsc::Receiver<Command>,
        backend::EventSink,
    ) {
        let mut client = super::super::tests::verified_client([1; 16]);
        let (backend, commands, sink) = Backend::endpoint();
        client.backend = backend;
        (client, commands, sink)
    }

    #[test]
    fn submission_begin_does_not_wait_and_completes_only_its_own_local_event() {
        let (mut client, commands, sink) = client();
        client.begin_reset_application(&Wait::default()).unwrap();
        let Command::Send(request) = commands.try_recv().unwrap() else {
            panic!("send expected");
        };
        assert!(client.poll_submission().is_pending());
        sink.submitted(request.id + 100);
        assert!(client.poll_submission().is_pending());
        sink.submitted(request.id);
        let Poll::Ready(Ok(local)) = client.poll_submission() else {
            panic!("local submission expected");
        };
        assert_eq!(local.id, request.id);
        assert_eq!(local.boundary, client.backend.boundary);
        assert!(client.pending_submission.is_none());
    }

    #[test]
    fn submission_is_mutually_exclusive_with_queries_and_other_business_calls() {
        let (mut client, commands, _) = client();
        client.begin_enter_update(&Wait::default()).unwrap();
        let _ = commands.try_recv().unwrap();
        for error in [
            client
                .begin_reset_application(&Wait::default())
                .unwrap_err(),
            client
                .begin_query(QueryKind::Status, &Wait::default())
                .unwrap_err(),
            client.stop_control(&Wait::default()).unwrap_err(),
            client.identify(None, &Wait::default()).unwrap_err(),
        ] {
            assert!(matches!(error.failure, Failure::Session(_)));
            assert_eq!(error.local, LocalStage::NotSubmitted);
        }
        assert!(client.identity_verified);
        assert!(commands.try_recv().is_err());
        client.cancel_submission().unwrap();
        client
            .begin_query(QueryKind::Status, &Wait::default())
            .unwrap();
        assert!(client.begin_reset_application(&Wait::default()).is_err());
        assert!(client.pending_query.is_some());
    }

    #[test]
    fn submission_cancel_ends_wait_without_retracting_or_replaying_the_send() {
        let (mut client, commands, sink) = client();
        client.begin_reset_application(&Wait::default()).unwrap();
        let Command::Send(first) = commands.try_recv().unwrap() else {
            panic!("send expected");
        };
        assert_eq!(
            client.cancel_submission().unwrap(),
            LocalStage::MayHaveBeenSubmitted
        );
        assert!(!first.cancelled.load(Ordering::Acquire));
        client.begin_enter_update(&Wait::default()).unwrap();
        let Command::Send(second) = commands.try_recv().unwrap() else {
            panic!("send expected");
        };
        assert_ne!(first.id, second.id);
        sink.submitted(first.id);
        assert!(client.poll_submission().is_pending());
        sink.submitted(second.id);
        assert!(
            matches!(client.poll_submission(), Poll::Ready(Ok(local)) if local.id == second.id)
        );
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn submission_timeout_and_lock_contention_are_bounded_and_preserve_unknown() {
        let (mut client, commands, _) = client();
        client.begin_reset_application(&Wait::default()).unwrap();
        let Command::Send(request) = commands.try_recv().unwrap() else {
            panic!("send expected");
        };
        let inbox = client.backend.sink.0.clone();
        let held = inbox.0.lock().unwrap();
        assert!(client.poll_submission().is_pending());
        client.pending_submission.as_mut().unwrap().deadline = Instant::now();
        let Poll::Ready(Err(error)) = client.poll_submission() else {
            panic!("timeout expected");
        };
        assert!(matches!(error.failure, Failure::ResultUnknown(_)));
        assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
        assert!(error.observed_transport.is_none());
        assert!(!request.cancelled.load(Ordering::Acquire));
        drop(held);
    }

    #[test]
    fn submission_poll_budget_yields_and_definite_presend_failure_is_not_unknown() {
        let (mut client, commands, sink) = client();
        client.begin_reset_application(&Wait::default()).unwrap();
        let Command::Send(request) = commands.try_recv().unwrap() else {
            panic!("send expected");
        };
        for id in 100..120 {
            sink.submitted(id);
        }
        sink.failed(request.id, "rejected before send", false);
        assert!(client.poll_submission().is_pending());
        let Poll::Ready(Err(error)) = client.poll_submission() else {
            panic!("presend failure expected");
        };
        assert_eq!(error.local, LocalStage::NotSubmitted);
        assert!(matches!(error.failure, Failure::Transport { .. }));
    }
    #[test]
    fn invalid_save_never_enqueues_and_possible_send_failure_remains_unknown() {
        let (mut client, commands, sink) = client();
        let error = client
            .begin_save_and_restart(b"{}", &Wait::default())
            .unwrap_err();
        assert_eq!(error.local, LocalStage::NotSubmitted);
        assert!(commands.try_recv().is_err());
        assert!(client.pending_submission.is_none());
        client.begin_enter_update(&Wait::default()).unwrap();
        let Command::Send(request) = commands.try_recv().unwrap() else {
            panic!("send expected");
        };
        sink.failed(request.id, "possibly transmitted", true);
        let Poll::Ready(Err(error)) = client.poll_submission() else {
            panic!("unknown expected");
        };
        assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
        assert!(matches!(error.failure, Failure::ResultUnknown(_)));
    }
}
