// Copyright The eha-sdk Contributors
//! 原子、非阻塞地观察被动状态，与业务事件分离。
use super::*;

#[derive(Clone, Debug, Default)]
pub struct ClientObservation {
    pub connection: ConnectionStatus,
    pub heartbeat: HeartbeatStatus,
    pub transport: TransportStatus,
    pub adapter: AdapterStatus,
    pub telemetry: Option<Reply>,
    pub drained: TelemetryDrain,
}

impl Client {
    /// 配置被动缓冲，不等待 Inbox 锁。
    pub fn try_enable_telemetry_queue(&mut self, capacity: usize) -> Poll<Result<(), Error>> {
        if capacity == 0 {
            return Poll::Ready(Err(invalid("遥测队列容量必须大于零")));
        }
        let mut inbox = match self.backend.sink.0.0.try_lock() {
            Ok(inbox) => inbox,
            Err(std::sync::TryLockError::WouldBlock) => return Poll::Pending,
            Err(_) => return Poll::Ready(Err(invalid("遥测队列锁已中毒"))),
        };
        inbox.telemetry_queue = Some(backend::TelemetryQueue::new(capacity));
        Poll::Ready(Ok(()))
    }

    /// 尝试一次 Inbox 锁并复制被动事实，不消费业务事件。
    /// 最多取出 `max_telemetry.min(64)` 个排队帧；零则保持队列和丢弃计数不变。
    /// Pending 表示锁竞争，绝不表示断连。
    /// 运行代次过滤保留源时间戳，不接纳新的 Identity。
    pub fn try_observe(&self, max_telemetry: usize) -> Poll<Result<ClientObservation, Error>> {
        let mut inbox = match self.backend.sink.0.0.try_lock() {
            Ok(inbox) => inbox,
            Err(std::sync::TryLockError::WouldBlock) => return Poll::Pending,
            Err(_) => return Poll::Ready(Err(invalid("观察缓存锁已中毒"))),
        };
        let run = self
            .identity_verified
            .then(|| self.session.identity())
            .flatten()
            .map(|id| id.run_nonce);
        let (received, dropped) = if run.is_some() {
            inbox
                .telemetry_queue
                .as_mut()
                .map(|queue| queue.drain_limited(max_telemetry.min(64)))
                .unwrap_or_default()
        } else {
            (Vec::new(), 0)
        };
        let mut observation = ClientObservation {
            connection: ConnectionStatus {
                disconnected: inbox.disconnected.clone(),
                last_telemetry_received_at: inbox.telemetry.as_ref().map(|value| value.at),
            },
            heartbeat: inbox.heartbeat.clone(),
            transport: inbox.transport.clone(),
            adapter: inbox.adapter.clone(),
            telemetry: inbox.telemetry.clone().map(Reply::from),
            drained: TelemetryDrain {
                replies: received.into_iter().map(Reply::from).collect(),
                dropped,
            },
        };
        drop(inbox);
        let matches_run = |reply: &Reply| matches!(reply.response(), Ok(Response::Telemetry(value)) if Some(value.fields().sample.run_nonce) == run);
        observation.telemetry = observation.telemetry.filter(matches_run);
        observation.drained.replies.retain(matches_run);
        Poll::Ready(Ok(observation))
    }
}

#[cfg(test)]
#[allow(clippy::panic)] // Unexpected observation variants must fail the test.
mod tests {
    use super::*;

    #[test]
    fn observation_contention_yields_without_consuming_business_events() {
        let client = super::super::tests::verified_client([1; 16]);
        client.backend.sink.submitted(7);
        let guard = client
            .backend
            .sink
            .0
            .0
            .lock()
            .expect("lock observation inbox");
        assert!(client.try_observe(16).is_pending());
        drop(guard);
        assert!(matches!(client.try_observe(16), Poll::Ready(Ok(_))));
        assert!(matches!(
            client
                .backend
                .sink
                .0
                .0
                .lock()
                .expect("lock observation inbox")
                .events
                .front(),
            Some(Event::Submitted(7))
        ));
    }

    #[test]
    fn observation_bounds_drain_filters_run_and_preserves_zero_budget_overflow() {
        let mut client = super::super::tests::verified_client([1; 16]);
        client
            .enable_telemetry_queue(3)
            .expect("enable bounded telemetry queue");
        for (run, seq) in [([1; 16], 1), ([2; 16], 2), ([1; 16], 3), ([1; 16], 4)] {
            client
                .backend
                .sink
                .received(super::super::tests::telemetry(run, seq));
        }
        let Poll::Ready(Ok(zero)) = client.try_observe(0) else {
            panic!("ready")
        };
        assert_eq!(zero.drained.dropped, 0);
        assert!(zero.drained.replies.is_empty());
        let Poll::Ready(Ok(first)) = client.try_observe(1) else {
            panic!("ready")
        };
        assert_eq!(first.drained.dropped, 1);
        assert!(
            first.drained.replies.is_empty(),
            "one stale-run frame consumes the bounded budget"
        );
        let Poll::Ready(Ok(rest)) = client.try_observe(1) else {
            panic!("ready")
        };
        assert_eq!(rest.drained.replies.len(), 1);
        assert_eq!(rest.drained.dropped, 0);
        assert_eq!(
            rest.telemetry
                .expect("latest verified telemetry after drain")
                .received_at,
            zero.telemetry
                .expect("latest verified telemetry before drain")
                .received_at
        );
        let Poll::Ready(Ok(last)) = client.try_observe(64) else {
            panic!("ready")
        };
        assert_eq!(last.drained.replies.len(), 1);
    }
}
