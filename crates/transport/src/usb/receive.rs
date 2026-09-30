// Copyright The eha_controller Contributors

use protocol::{Direction, Prefix, probe_prefix};

use crate::guard::Guard;

use super::cobs::cobs_decode_in_place;
use super::{
    BLOCK_TIMEOUT_MS, FeedResult, LONG_GUARD_MS, LONG_TIMEOUT_MS, Lane, MAX_COBS_LEN, Phase,
    PhaseError, PollResult, ReceiveEvent, ReceivedMessage, RejectReason, SHORT_GUARD_MS,
    SHORT_MESSAGE_LIMIT, ValidatedCompletedMessage, ValidatedFeedResult, ValidatedReceiveEvent,
};

#[derive(Clone, Copy, Debug)]
enum ReceiveState {
    Idle,
    Assembling {
        transfer: u8,
        length: usize,
        received: usize,
        expected_index: u16,
        deadline_ms: u64,
    },
    Delivered {
        length: usize,
    },
}

#[derive(Debug)]
struct ReceiveLane<'a> {
    buffer: Option<&'a mut [u8]>,
    state: ReceiveState,
    guard: Guard,
}

impl<'a> ReceiveLane<'a> {
    fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer: Some(buffer),
            state: ReceiveState::Idle,
            guard: Guard::new(),
        }
    }
}

/// USB OUT 字节流接收器。
///
/// 三个缓冲均由调用方分配和持有。本类型只在一条完整消息已经通过公共
/// 校验后短暂保留相应 lane 的借用；调用方同步处理或复制后可用
/// [`Self::discard_completed`] 复用原缓冲，需交给应用层时以
/// [`Self::replace_buffer`] 交换该缓冲，避免复制最大配置消息。
#[derive(Debug)]
pub struct Receiver<'a> {
    direction: Direction,
    pub(super) phase: Phase,
    phase_exhausted: bool,
    lanes: [ReceiveLane<'a>; 3],
    encoded: [u8; MAX_COBS_LEN],
    encoded_len: usize,
    block_state: BlockState,
}

/// 可在旧 Bulk I/O 已隔离后移动到新接收器的有限重连状态。
///
/// 本状态不含输入块、组装字节、交付消息或调用方缓冲，且不可复制。它保留端点阶段和
/// 每条 lane 的传输号保护窗口，使旧阶段字节不能续接恢复后的接收器。
pub struct ReceiverReconnectState {
    direction: Direction,
    phase: Phase,
    guards: [Guard; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockState {
    Idle,
    Collecting { deadline_ms: u64 },
    Discarding,
}

impl<'a> Receiver<'a> {
    /// 创建一个固定三 lane 接收器。
    pub fn new(
        direction: Direction,
        heartbeat_buffer: &'a mut [u8],
        short_buffer: &'a mut [u8],
        long_buffer: &'a mut [u8],
    ) -> Self {
        Self {
            direction,
            phase: Phase(0),
            phase_exhausted: false,
            lanes: [
                ReceiveLane::new(heartbeat_buffer),
                ReceiveLane::new(short_buffer),
                ReceiveLane::new(long_buffer),
            ],
            encoded: [0; MAX_COBS_LEN],
            encoded_len: 0,
            block_state: BlockState::Idle,
        }
    }

    /// 当前端点阶段。
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// 在旧 Bulk I/O 已结束或隔离后，开始新阶段并导出可移动重连状态。
    ///
    /// 调用前不得持有由 [`Self::feed_validated`] 转交的 lane 缓冲；该前提与
    /// [`Self::phase_changed`] 相同。未闭合 COBS 块、部分组装和完整但尚未交付的消息均
    /// 被舍弃；旧阶段字节不能推进恢复后的接收器。
    pub fn into_reconnect_state(
        mut self,
        now_ms: u64,
    ) -> Result<ReceiverReconnectState, PhaseError> {
        self.phase_changed(now_ms)?;
        for lane in &mut self.lanes {
            lane.state = ReceiveState::Idle;
        }
        let Self {
            direction,
            phase,
            lanes,
            ..
        } = self;
        let [heartbeat, short, long] = lanes;
        let ReceiveLane {
            guard: heartbeat_guard,
            ..
        } = heartbeat;
        let ReceiveLane {
            guard: short_guard, ..
        } = short;
        let ReceiveLane {
            guard: long_guard, ..
        } = long;
        Ok(ReceiverReconnectState {
            direction,
            phase,
            guards: [heartbeat_guard, short_guard, long_guard],
        })
    }

    /// 以空的新组装缓冲恢复一个由 [`Self::into_reconnect_state`] 导出的接收器。
    pub fn from_reconnect_state(
        state: ReceiverReconnectState,
        heartbeat_buffer: &'a mut [u8],
        short_buffer: &'a mut [u8],
        long_buffer: &'a mut [u8],
    ) -> Self {
        let ReceiverReconnectState {
            direction,
            phase,
            guards,
        } = state;
        let mut receiver = Self::new(direction, heartbeat_buffer, short_buffer, long_buffer);
        receiver.phase = phase;
        for (lane, guard) in receiver.lanes.iter_mut().zip(guards) {
            lane.guard = guard;
        }
        receiver
    }

    /// 推进时间，清除到达绝对期限的未完成块和消息。
    pub fn poll(&mut self, now_ms: u64) -> PollResult {
        let mut result = PollResult::default();
        if let BlockState::Collecting { deadline_ms } = self.block_state
            && now_ms >= deadline_ms
        {
            self.encoded_len = 0;
            self.block_state = BlockState::Discarding;
            result.block_timed_out = true;
        }

        for (index, lane) in self.lanes.iter_mut().enumerate() {
            if let ReceiveState::Assembling {
                transfer,
                deadline_ms,
                ..
            } = lane.state
                && now_ms >= deadline_ms
            {
                lane.guard.protect(
                    transfer,
                    now_ms,
                    match index {
                        2 => LONG_GUARD_MS,
                        _ => SHORT_GUARD_MS,
                    },
                );
                lane.state = ReceiveState::Idle;
                result.lane_timed_out[index] = true;
            }
        }
        result
    }

    /// 处理一个 Bulk read 得到的实际字节增量。
    pub fn feed(&mut self, phase: Phase, input: &[u8], now_ms: u64) -> FeedResult {
        let result = self.feed_validated(phase, input, now_ms);
        let event = match result.event {
            ValidatedReceiveEvent::NeedMore => ReceiveEvent::NeedMore,
            ValidatedReceiveEvent::Ignored => ReceiveEvent::Ignored,
            ValidatedReceiveEvent::Rejected(reason) => ReceiveEvent::Rejected(reason),
            ValidatedReceiveEvent::TimedOut(result) => ReceiveEvent::TimedOut(result),
            ValidatedReceiveEvent::StalePhase => ReceiveEvent::StalePhase,
            ValidatedReceiveEvent::PhaseExhausted => ReceiveEvent::PhaseExhausted,
            ValidatedReceiveEvent::Complete { lane, message } => {
                let length = message.info().length;
                let buffer = message.into_buffer();
                let entry = &mut self.lanes[lane.index()];
                entry.buffer = Some(buffer);
                entry.state = ReceiveState::Delivered { length };
                ReceiveEvent::Complete(ReceivedMessage { lane, length })
            }
        };
        FeedResult {
            consumed: result.consumed,
            event,
        }
    }

    /// 处理一个 Bulk read，并在完整消息到达时转交绑定原缓冲的校验证明。
    ///
    /// `phase` 必须等于 [`Self::phase`]。一个调用最多处理到一项明确的块结果，因而调用
    /// 方可在交换 lane 缓冲后以未消费的输入继续调用本函数。
    pub fn feed_validated(
        &mut self,
        phase: Phase,
        input: &[u8],
        now_ms: u64,
    ) -> ValidatedFeedResult<'a> {
        if phase != self.phase {
            return ValidatedFeedResult {
                consumed: 0,
                event: ValidatedReceiveEvent::StalePhase,
            };
        }
        if self.phase_exhausted {
            return ValidatedFeedResult {
                consumed: 0,
                event: ValidatedReceiveEvent::PhaseExhausted,
            };
        }
        let timeouts = self.poll(now_ms);
        if timeouts != PollResult::default() {
            return ValidatedFeedResult {
                consumed: 0,
                event: ValidatedReceiveEvent::TimedOut(timeouts),
            };
        }

        for (index, byte) in input.iter().copied().enumerate() {
            match self.block_state {
                BlockState::Idle => {
                    if byte != 0 {
                        self.encoded[0] = byte;
                        self.encoded_len = 1;
                        self.block_state = BlockState::Collecting {
                            deadline_ms: now_ms.saturating_add(BLOCK_TIMEOUT_MS),
                        };
                    } else {
                        return ValidatedFeedResult {
                            consumed: index + 1,
                            event: ValidatedReceiveEvent::Ignored,
                        };
                    }
                }
                BlockState::Discarding => {
                    if byte == 0 {
                        self.encoded_len = 0;
                        self.block_state = BlockState::Idle;
                        return ValidatedFeedResult {
                            consumed: index + 1,
                            event: ValidatedReceiveEvent::Ignored,
                        };
                    }
                }
                BlockState::Collecting { .. } => {
                    if byte == 0 {
                        self.block_state = BlockState::Idle;
                        let event = self.finish_block(now_ms);
                        self.encoded_len = 0;
                        return ValidatedFeedResult {
                            consumed: index + 1,
                            event,
                        };
                    } else if self.encoded_len == MAX_COBS_LEN {
                        self.encoded_len = 0;
                        self.block_state = BlockState::Discarding;
                        return ValidatedFeedResult {
                            consumed: index + 1,
                            event: ValidatedReceiveEvent::Rejected(RejectReason::BlockTooLong),
                        };
                    } else {
                        self.encoded[self.encoded_len] = byte;
                        self.encoded_len += 1;
                    }
                }
            }
        }
        ValidatedFeedResult {
            consumed: input.len(),
            event: ValidatedReceiveEvent::NeedMore,
        }
    }

    /// 借用已交付消息的有效公共字节。收到 [`ReceivedMessage`] 后调用。
    pub fn message(&self, lane: Lane) -> Option<&[u8]> {
        let lane = &self.lanes[lane.index()];
        match lane.state {
            ReceiveState::Delivered { length } => Some(&lane.buffer.as_deref()?[..length]),
            ReceiveState::Idle | ReceiveState::Assembling { .. } => None,
        }
    }

    /// 舍弃一条已完成但不再需要移交的消息，保留其组装缓冲以继续接收。
    ///
    /// 返回 `true` 表示指定 lane 原先持有完整消息。它不改变传输编号保护、
    /// 当前端点阶段、COBS 组装状态或其他 lane。
    #[must_use]
    pub fn discard_completed(&mut self, lane: Lane) -> bool {
        let lane = &mut self.lanes[lane.index()];
        if !matches!(lane.state, ReceiveState::Delivered { .. }) || lane.buffer.is_none() {
            return false;
        }
        lane.state = ReceiveState::Idle;
        true
    }

    /// 用调用方的新缓冲替换已交付 lane 的缓冲，并返还原缓冲。
    ///
    /// 返回的切片仍含有 [`ReceivedMessage::length`] 指出的有效前缀。替换后该
    /// lane 可立即组装下一条消息；`transfer` 的 receiver guard 不受影响。
    pub fn replace_buffer(
        &mut self,
        lane: Lane,
        replacement: &'a mut [u8],
    ) -> Option<&'a mut [u8]> {
        let lane = &mut self.lanes[lane.index()];
        if !matches!(lane.state, ReceiveState::Delivered { .. }) {
            return None;
        }
        lane.state = ReceiveState::Idle;
        lane.buffer.replace(replacement)
    }

    /// 归还由 [`Self::feed_validated`] 完成事件转交的原组装缓冲。
    ///
    /// 证明存活期间对应 lane 没有内部缓冲；调用方消费证明并取回原缓冲后，才可用本方法
    /// 使它重新参与组装。
    #[must_use]
    pub fn restore_buffer(&mut self, lane: Lane, buffer: &'a mut [u8]) -> bool {
        let entry = &mut self.lanes[lane.index()];
        if !matches!(entry.state, ReceiveState::Idle) || entry.buffer.is_some() {
            return false;
        }
        entry.buffer = Some(buffer);
        true
    }

    /// 开始新的端点传输阶段，放弃未完成输入但不重置编号或保护期。
    pub fn phase_changed(&mut self, now_ms: u64) -> Result<Phase, PhaseError> {
        if self.lanes.iter().any(|lane| lane.buffer.is_none()) {
            return Err(PhaseError::BufferBusy);
        }
        self.encoded_len = 0;
        self.block_state = BlockState::Idle;
        for (index, lane) in self.lanes.iter_mut().enumerate() {
            if let ReceiveState::Assembling { transfer, .. } = lane.state {
                lane.guard.protect(
                    transfer,
                    now_ms,
                    if index == 2 {
                        LONG_GUARD_MS
                    } else {
                        SHORT_GUARD_MS
                    },
                );
                lane.state = ReceiveState::Idle;
            }
        }
        let Some(next) = self.phase.0.checked_add(1) else {
            self.phase_exhausted = true;
            return Err(PhaseError::Exhausted);
        };
        self.phase = Phase(next);
        Ok(self.phase)
    }

    fn finish_block(&mut self, now_ms: u64) -> ValidatedReceiveEvent<'a> {
        let raw_len = match cobs_decode_in_place(&mut self.encoded, self.encoded_len) {
            Ok(length) => length,
            Err(()) => return ValidatedReceiveEvent::Rejected(RejectReason::Cobs),
        };
        if raw_len <= 4 {
            return ValidatedReceiveEvent::Rejected(RejectReason::RawBlock);
        }
        let Some(lane) = Lane::from_raw(self.encoded[0]) else {
            return ValidatedReceiveEvent::Rejected(RejectReason::Lane);
        };
        let transfer = self.encoded[1];
        if transfer > 127 {
            return ValidatedReceiveEvent::Rejected(RejectReason::RawBlock);
        }
        let fragment_index = u16::from_le_bytes([self.encoded[2], self.encoded[3]]);
        let fragment_len = raw_len - 4;
        let mut fragment = [0; SHORT_MESSAGE_LIMIT];
        fragment[..fragment_len].copy_from_slice(&self.encoded[4..raw_len]);
        self.accept_fragment(
            lane,
            transfer,
            fragment_index,
            &fragment[..fragment_len],
            now_ms,
        )
    }

    fn accept_fragment(
        &mut self,
        lane: Lane,
        transfer: u8,
        fragment_index: u16,
        fragment: &[u8],
        now_ms: u64,
    ) -> ValidatedReceiveEvent<'a> {
        let lane_index = lane.index();
        let state = &self.lanes[lane_index].state;
        match state {
            ReceiveState::Idle => {
                if fragment_index != 0
                    || self.lanes[lane_index].guard.is_protected(transfer, now_ms)
                {
                    return ValidatedReceiveEvent::Ignored;
                }
                let length = match first_fragment_length(lane, fragment, self.direction) {
                    Ok(length) => length,
                    Err(reason) => return ValidatedReceiveEvent::Rejected(reason),
                };
                self.start_fragment(lane, transfer, length, fragment, now_ms)
            }
            ReceiveState::Delivered { .. } => ValidatedReceiveEvent::Ignored,
            ReceiveState::Assembling {
                transfer: current,
                expected_index,
                ..
            } if *current == transfer => {
                if fragment_index != *expected_index {
                    self.abandon_receive(lane, now_ms);
                    return ValidatedReceiveEvent::Rejected(RejectReason::Fragment);
                }
                self.append_fragment(lane, transfer, fragment_index, fragment, now_ms)
            }
            ReceiveState::Assembling { .. } => {
                if self.lanes[lane_index].guard.is_protected(transfer, now_ms)
                    || fragment_index != 0
                {
                    return ValidatedReceiveEvent::Ignored;
                }
                let length = match first_fragment_length(lane, fragment, self.direction) {
                    Ok(length) => length,
                    Err(reason) => return ValidatedReceiveEvent::Rejected(reason),
                };
                self.abandon_receive(lane, now_ms);
                self.start_fragment(lane, transfer, length, fragment, now_ms)
            }
        }
    }

    fn start_fragment(
        &mut self,
        lane: Lane,
        transfer: u8,
        length: usize,
        fragment: &[u8],
        now_ms: u64,
    ) -> ValidatedReceiveEvent<'a> {
        let direction = self.direction;
        let entry = &mut self.lanes[lane.index()];
        let Some(buffer) = entry.buffer.as_deref_mut() else {
            return ValidatedReceiveEvent::Ignored;
        };
        if buffer.len() < length {
            entry
                .guard
                .protect(transfer, now_ms, lane.guard_duration_ms());
            return ValidatedReceiveEvent::Rejected(RejectReason::BufferTooSmall);
        }
        match lane {
            Lane::Heartbeat | Lane::Short => {
                buffer[..length].copy_from_slice(fragment);
                entry
                    .guard
                    .protect(transfer, now_ms, lane.guard_duration_ms());
                let buffer = entry.buffer.take().expect("buffer checked above");
                match ValidatedCompletedMessage::validate(buffer, length, direction) {
                    Ok(message) => ValidatedReceiveEvent::Complete { lane, message },
                    Err((buffer, _)) => {
                        entry.buffer = Some(buffer);
                        ValidatedReceiveEvent::Rejected(RejectReason::Message)
                    }
                }
            }
            Lane::Long => {
                buffer[..SHORT_MESSAGE_LIMIT].copy_from_slice(fragment);
                entry.state = ReceiveState::Assembling {
                    transfer,
                    length,
                    received: SHORT_MESSAGE_LIMIT,
                    expected_index: 1,
                    deadline_ms: now_ms.saturating_add(LONG_TIMEOUT_MS),
                };
                ValidatedReceiveEvent::NeedMore
            }
        }
    }

    fn append_fragment(
        &mut self,
        lane: Lane,
        transfer: u8,
        fragment_index: u16,
        fragment: &[u8],
        now_ms: u64,
    ) -> ValidatedReceiveEvent<'a> {
        let direction = self.direction;
        debug_assert_eq!(lane, Lane::Long);
        let (length, received, expected_index, deadline_ms) = match self.lanes[lane.index()].state {
            ReceiveState::Assembling {
                length,
                received,
                expected_index,
                deadline_ms,
                ..
            } => (length, received, expected_index, deadline_ms),
            ReceiveState::Idle | ReceiveState::Delivered { .. } => {
                return ValidatedReceiveEvent::Ignored;
            }
        };
        if fragment_index != expected_index {
            self.abandon_receive(lane, now_ms);
            return ValidatedReceiveEvent::Rejected(RejectReason::Fragment);
        }
        let remaining = length - received;
        let expected_len = remaining.min(SHORT_MESSAGE_LIMIT);
        if fragment.len() != expected_len {
            self.abandon_receive(lane, now_ms);
            return ValidatedReceiveEvent::Rejected(RejectReason::Fragment);
        }
        let entry = &mut self.lanes[lane.index()];
        let Some(buffer) = entry.buffer.as_deref_mut() else {
            return ValidatedReceiveEvent::Ignored;
        };
        buffer[received..received + fragment.len()].copy_from_slice(fragment);
        let completed = received + fragment.len() == length;
        if !completed {
            entry.state = ReceiveState::Assembling {
                transfer,
                length,
                received: received + fragment.len(),
                expected_index: expected_index + 1,
                deadline_ms,
            };
            return ValidatedReceiveEvent::NeedMore;
        }
        entry
            .guard
            .protect(transfer, now_ms, lane.guard_duration_ms());
        let buffer = entry.buffer.take().expect("buffer checked above");
        match ValidatedCompletedMessage::validate(buffer, length, direction) {
            Ok(message) => {
                entry.state = ReceiveState::Idle;
                ValidatedReceiveEvent::Complete { lane, message }
            }
            Err((buffer, _)) => {
                entry.buffer = Some(buffer);
                entry.state = ReceiveState::Idle;
                ValidatedReceiveEvent::Rejected(RejectReason::Message)
            }
        }
    }

    fn abandon_receive(&mut self, lane: Lane, now_ms: u64) {
        let entry = &mut self.lanes[lane.index()];
        if let ReceiveState::Assembling { transfer, .. } = entry.state {
            entry
                .guard
                .protect(transfer, now_ms, lane.guard_duration_ms());
            entry.state = ReceiveState::Idle;
        }
    }
}
fn first_fragment_length(
    lane: Lane,
    fragment: &[u8],
    direction: Direction,
) -> Result<usize, RejectReason> {
    let prefix = probe_prefix(fragment, direction).map_err(|_| RejectReason::Prefix)?;
    let Prefix::Complete { length, kind } = prefix else {
        return Err(RejectReason::Prefix);
    };
    let expected_len = match lane {
        Lane::Heartbeat if kind.is_heartbeat() && length <= SHORT_MESSAGE_LIMIT => length,
        Lane::Short if !kind.is_heartbeat() && length <= SHORT_MESSAGE_LIMIT => length,
        Lane::Long if length > SHORT_MESSAGE_LIMIT => SHORT_MESSAGE_LIMIT,
        Lane::Heartbeat | Lane::Short | Lane::Long => return Err(RejectReason::Lane),
    };
    if fragment.len() != expected_len {
        return Err(RejectReason::Fragment);
    }
    Ok(length)
}
