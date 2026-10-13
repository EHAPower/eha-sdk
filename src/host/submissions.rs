// Copyright The eha-sdk Contributors

//! Pollable local maintenance submission on the same exclusive Client.

use super::*;
use crate::session::MutationMatch;

pub(super) struct PendingSubmission {
    matcher: Option<MutationMatch>,
    id: u64,
    deadline: Instant,
    wait: Wait,
    control_cancel: Option<backend::QueueCancellation>,
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
            control_cancel: None,
            matcher: Some(matcher),
            id,
            deadline,
            wait: wait.clone(),
        });
        Ok(())
    }

    /// Fixed control entries use the same exclusive local-submission slot.
    /// A successful completion proves only local I/O submission, never control completion.
    pub fn begin_stop_control(&mut self, wait: &Wait) -> Result<(), Error> {
        self.begin_control_submission(Message::Stop, wait)
    }
    pub fn begin_heartbeat(&mut self, wait: &Wait) -> Result<(), Error> {
        self.begin_control_submission(Message::Heartbeat, wait)
    }
    pub fn begin_position(&mut self, mm: f32, wait: &Wait) -> Result<(), Error> {
        self.begin_control_submission(Message::Position(mm), wait)
    }
    pub fn begin_velocity(&mut self, mm_s: f32, wait: &Wait) -> Result<(), Error> {
        self.begin_control_submission(Message::Velocity(mm_s), wait)
    }
    pub fn begin_force(&mut self, n: f32, wait: &Wait) -> Result<(), Error> {
        self.begin_control_submission(Message::Force(n), wait)
    }
    pub fn begin_impedance(
        &mut self,
        mm: f32,
        n_per_mm: f32,
        ns_per_mm: f32,
        wait: &Wait,
    ) -> Result<(), Error> {
        self.begin_control_submission(
            Message::Impedance {
                equilibrium_mm: mm,
                stiffness_n_per_mm: n_per_mm,
                damping_ns_per_mm: ns_per_mm,
            },
            wait,
        )
    }

    fn begin_control_submission(&mut self, message: Message<'_>, wait: &Wait) -> Result<(), Error> {
        self.require_idle()?;
        self.require_identity()?;
        let (id, deadline, control_cancel) = self.send_control(&message, wait)?;
        self.pending_submission = Some(PendingSubmission {
            control_cancel: Some(control_cancel),
            matcher: None,
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
                        && pending
                            .matcher
                            .is_some_and(|matcher| matcher.matches(&response))
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

    /// 取消/退役前仅领取当前提交已排队的终态，不等待 I/O，也不重新发送。
    /// 一次 try_lock、最多检查普通 FIFO 的 64 项；无关事件保持原序。
    /// Pending 仅表示锁忙；Ready(None) 表示未找到终态，两者均保留原槽位。
    /// 已存在的证据不受当前等待期限/取消标志影响；断连之前的本地提交
    /// 只证明该消息的本地 I/O 完成，绝不证明设备采用或维护执行。
    pub fn try_finish_submission(&mut self) -> Poll<Option<Result<LocalSubmission, Error>>> {
        let Some(pending) = self.pending_submission.as_ref() else {
            return Poll::Ready(Some(Err(invalid("没有等待中的维护提交"))));
        };
        let mut inbox = match self.backend.sink.0.0.try_lock() {
            Ok(inbox) => inbox,
            Err(std::sync::TryLockError::WouldBlock) => return Poll::Pending,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                self.pending_submission = None;
                return Poll::Ready(Some(Err(self.submission_poll_error(
                    Failure::Disconnected("I/O 状态锁失效".into()),
                    LocalStage::MayHaveBeenSubmitted,
                ))));
            }
        };
        let terminal = inbox
            .events
            .iter()
            .take(64)
            .enumerate()
            .find_map(|(index, event)| {
                let result = match event {
                    Event::Submitted(id) if *id == pending.id => Ok(LocalSubmission {
                        id: pending.id,
                        submitted_at: Instant::now(),
                        boundary: self.backend.boundary,
                    }),
                    Event::Failed(id, failure) if *id == pending.id => Err((
                        Failure::Transport {
                            detail: failure.detail.clone(),
                            raw: Vec::new(),
                        },
                        if failure.may_have_sent {
                            LocalStage::MayHaveBeenSubmitted
                        } else {
                            LocalStage::NotSubmitted
                        },
                    )),
                    Event::Message(received) => {
                        let Ok(Message::Response(response @ Response::DataUnavailable(value))) =
                            protocol::decode(&received.bytes, Direction::FirmwareToHost)
                        else {
                            return None;
                        };
                        if !pending
                            .matcher
                            .is_some_and(|matcher| matcher.matches(&response))
                        {
                            return None;
                        }
                        Err((
                            Failure::DataUnavailable(value.fields()),
                            LocalStage::MayHaveBeenSubmitted,
                        ))
                    }
                    Event::Disconnected(reason) => Err((
                        Failure::Disconnected(reason.clone()),
                        LocalStage::MayHaveBeenSubmitted,
                    )),
                    Event::TransportError { detail, raw } => Err((
                        Failure::Transport {
                            detail: detail.clone(),
                            raw: raw.clone(),
                        },
                        LocalStage::MayHaveBeenSubmitted,
                    )),
                    _ => return None,
                };
                Some((index, result))
            });
        let Some((index, result)) = terminal else {
            return Poll::Ready(None);
        };
        inbox.events.remove(index);
        drop(inbox);
        self.pending_submission = None;
        Poll::Ready(Some(result.map_err(|(failure, local)| {
            if matches!(failure, Failure::DataUnavailable(_)) {
                self.query_error(failure, local)
            } else {
                self.submission_poll_error(failure, local)
            }
        })))
    }

    /// End this local wait. Maintenance bytes are never retracted and retain their send deadline.
    /// A fixed control send can be cancelled only before driver handoff: then NotSubmitted is
    /// returned. Once taken, MayHaveBeenSubmitted remains unknown; neither path replays a send.
    pub fn cancel_submission(&mut self) -> Result<LocalStage, Error> {
        self.pending_submission
            .take()
            .map(|pending| {
                pending
                    .control_cancel
                    .map_or(LocalStage::MayHaveBeenSubmitted, |token| token.cancel())
            })
            .ok_or_else(|| invalid("没有等待中的维护提交"))
    }
}

#[cfg(test)]
#[allow(clippy::panic)] // Unexpected submission variants must fail the test.
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
    fn submission_evidence_precedes_disconnect_even_after_deadline_and_cancellation() {
        let (mut client, commands, sink) = client();
        let wait = Wait::default();
        client.begin_reset_application(&wait).expect("begin");
        let Command::Send(request) = commands.try_recv().expect("send") else {
            panic!("send expected");
        };
        for id in 100..162 {
            sink.submitted(id);
        }
        sink.submitted(request.id);
        sink.disconnected("reset transport closed");
        wait.cancellation.cancel();
        client
            .pending_submission
            .as_mut()
            .expect("pending")
            .deadline = Instant::now();
        let Poll::Ready(Some(Ok(local))) = client.try_finish_submission() else {
            panic!("queued local completion must survive disconnect");
        };
        assert_eq!(local.id, request.id);
        assert_eq!(local.boundary, client.backend.boundary);
        assert!(client.pending_submission.is_none());
        let inbox = sink.0.0.lock().expect("inbox");
        assert_eq!(inbox.events.len(), 63);
        assert!(matches!(inbox.events.front(), Some(Event::Submitted(100))));
        assert!(matches!(inbox.events.back(), Some(Event::Disconnected(_))));
        assert!(!request.cancelled.load(Ordering::Acquire));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn submission_evidence_disconnect_before_completion_stays_unknown() {
        let (mut client, commands, sink) = client();
        client
            .begin_reset_application(&Wait::default())
            .expect("begin");
        let Command::Send(request) = commands.try_recv().expect("send") else {
            panic!("send expected");
        };
        sink.disconnected("transport closed before receipt");
        sink.submitted(request.id);
        let Poll::Ready(Some(Err(error))) = client.try_finish_submission() else {
            panic!("first terminal must be retained");
        };
        assert!(matches!(error.failure, Failure::ResultUnknown(_)));
        assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
        assert!(client.pending_submission.is_none());
        assert!(
            matches!(sink.0.0.lock().expect("inbox").events.front(), Some(Event::Submitted(id)) if *id == request.id)
        );
    }

    #[test]
    fn submission_evidence_preserves_precise_failed_send_stage() {
        for may_have_sent in [false, true] {
            let (mut client, commands, sink) = client();
            client
                .begin_reset_application(&Wait::default())
                .expect("begin");
            let Command::Send(request) = commands.try_recv().expect("send") else {
                panic!("send expected");
            };
            sink.failed(request.id + 100, "another attempt", !may_have_sent);
            sink.failed(request.id, "actual attempt failed", may_have_sent);
            sink.disconnected("later disconnect");
            let Poll::Ready(Some(Err(error))) = client.try_finish_submission() else {
                panic!("original failed receipt expected");
            };
            if may_have_sent {
                assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
                assert!(matches!(error.failure, Failure::ResultUnknown(_)));
            } else {
                assert_eq!(error.local, LocalStage::NotSubmitted);
                assert!(matches!(error.failure, Failure::Transport { .. }));
            }
            assert_eq!(sink.0.0.lock().expect("inbox").events.len(), 2);
            assert!(!request.cancelled.load(Ordering::Acquire));
        }
    }

    #[test]
    fn submission_evidence_lock_busy_and_absent_terminal_preserve_original_slot() {
        let (mut client, commands, sink) = client();
        client
            .begin_reset_application(&Wait::default())
            .expect("begin");
        let Command::Send(request) = commands.try_recv().expect("send") else {
            panic!("send expected");
        };
        sink.submitted(request.id + 100);
        assert!(matches!(client.try_finish_submission(), Poll::Ready(None)));
        assert_eq!(
            client.pending_submission.as_ref().expect("pending").id,
            request.id
        );
        let inbox = sink.0.clone();
        let mut held = inbox.0.lock().expect("inbox");
        held.events.push_back(Event::Submitted(request.id));
        assert!(client.try_finish_submission().is_pending());
        assert_eq!(
            client.pending_submission.as_ref().expect("pending").id,
            request.id
        );
        drop(held);
        assert!(
            matches!(client.try_finish_submission(), Poll::Ready(Some(Ok(local))) if local.id == request.id)
        );
        assert_eq!(sink.0.0.lock().expect("inbox").events.len(), 1);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn submission_begin_does_not_wait_and_completes_only_its_own_local_event() {
        let (mut client, commands, sink) = client();
        client
            .begin_reset_application(&Wait::default())
            .expect("test invariant");
        let Command::Send(request) = commands.try_recv().expect("test invariant") else {
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
        client
            .begin_enter_update(&Wait::default())
            .expect("test invariant");
        let _ = commands.try_recv().expect("test invariant");
        for error in [
            client
                .begin_reset_application(&Wait::default())
                .expect_err("expected rejection"),
            client
                .begin_query(QueryKind::Status, &Wait::default())
                .expect_err("expected rejection"),
            client
                .stop_control(&Wait::default())
                .expect_err("expected rejection"),
            client
                .identify(None, &Wait::default())
                .expect_err("expected rejection"),
        ] {
            assert!(matches!(error.failure, Failure::Session(_)));
            assert_eq!(error.local, LocalStage::NotSubmitted);
        }
        assert!(client.identity_verified);
        assert!(commands.try_recv().is_err());
        client.cancel_submission().expect("test invariant");
        client
            .begin_query(QueryKind::Status, &Wait::default())
            .expect("test invariant");
        assert!(client.begin_reset_application(&Wait::default()).is_err());
        assert!(client.pending_query.is_some());
    }

    #[test]
    fn submission_cancel_ends_wait_without_retracting_or_replaying_the_send() {
        let (mut client, commands, sink) = client();
        client
            .begin_reset_application(&Wait::default())
            .expect("test invariant");
        let Command::Send(first) = commands.try_recv().expect("test invariant") else {
            panic!("send expected");
        };
        assert_eq!(
            client.cancel_submission().expect("test invariant"),
            LocalStage::MayHaveBeenSubmitted
        );
        assert!(!first.cancelled.load(Ordering::Acquire));
        client
            .begin_enter_update(&Wait::default())
            .expect("test invariant");
        let Command::Send(second) = commands.try_recv().expect("test invariant") else {
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
        client
            .begin_reset_application(&Wait::default())
            .expect("test invariant");
        let Command::Send(request) = commands.try_recv().expect("test invariant") else {
            panic!("send expected");
        };
        let inbox = client.backend.sink.0.clone();
        let held = inbox.0.lock().expect("test invariant");
        assert!(client.poll_submission().is_pending());
        client
            .pending_submission
            .as_mut()
            .expect("test invariant")
            .deadline = Instant::now();
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
        client
            .begin_reset_application(&Wait::default())
            .expect("test invariant");
        let Command::Send(request) = commands.try_recv().expect("test invariant") else {
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
            .expect_err("expected rejection");
        assert_eq!(error.local, LocalStage::NotSubmitted);
        assert!(commands.try_recv().is_err());
        assert!(client.pending_submission.is_none());
        client
            .begin_enter_update(&Wait::default())
            .expect("test invariant");
        let Command::Send(request) = commands.try_recv().expect("test invariant") else {
            panic!("send expected");
        };
        sink.failed(request.id, "possibly transmitted", true);
        let Poll::Ready(Err(error)) = client.poll_submission() else {
            panic!("unknown expected");
        };
        assert_eq!(error.local, LocalStage::MayHaveBeenSubmitted);
        assert!(matches!(error.failure, Failure::ResultUnknown(_)));
    }
    #[test]
    fn explicit_stop_after_cancelled_target_has_its_own_token_and_no_target_replay() {
        let (mut client, commands, sink) = client();
        client
            .begin_position(1.25, &Wait::default())
            .expect("test invariant");
        let Command::CancellableSend(target, _) = commands.try_recv().expect("test invariant")
        else {
            panic!("target expected");
        };
        assert!(client.begin_stop_control(&Wait::default()).is_err());
        assert_eq!(
            client.cancel_submission().expect("test invariant"),
            LocalStage::NotSubmitted
        );
        assert!(target.cancelled.load(Ordering::Acquire));
        client
            .begin_stop_control(&Wait::default())
            .expect("test invariant");
        let Command::CancellableSend(stop, _) = commands.try_recv().expect("test invariant") else {
            panic!("stop expected");
        };
        assert_ne!(stop.id, target.id);
        assert!(matches!(
            crate::protocol::decode(&stop.bytes, crate::protocol::Direction::HostToFirmware)
                .expect("test invariant"),
            Message::Stop
        ));
        sink.submitted(target.id);
        assert!(client.poll_submission().is_pending());
        sink.submitted(stop.id);
        assert!(matches!(client.poll_submission(), Poll::Ready(Ok(local)) if local.id == stop.id));
        assert!(commands.try_recv().is_err());
    }
}
