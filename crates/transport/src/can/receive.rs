// Copyright The eha_controller Contributors

use protocol::{Direction, MessageInfo, Prefix};

use crate::guard::Guard;

use super::{
    CompletedMessage, Error, Frame, Lane, Mode, ReceiveResult, RejectReason, RxBuffers,
    ValidatedCompletedMessage, ValidatedReceiveResult,
    types::{HEARTBEAT_CAPACITY, LARGE_CAPACITY, MAX_EXTENDED_ID, SHORT_CAPACITY},
    wire::{direction_bit, expected_payload, frame_length, lane_for, matches_payload},
};

#[derive(Clone, Copy)]
struct RxActive {
    transfer_id: u8,
    next_fragment: u16,
    length: usize,
    deadline_ms: u64,
}

struct RxLane<'a> {
    buffer: Option<&'a mut [u8]>,
    active: Option<RxActive>,
    completed: Option<MessageInfo>,
    guard: Guard,
}

impl<'a> RxLane<'a> {
    fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer: Some(buffer),
            active: None,
            completed: None,
            guard: Guard::new(),
        }
    }
}

/// CAN 接收器，拥有一个方向、节点、模式和通信世代内的三条有界组装状态。
///
/// `generation` 由 CAN 资源所有者在 Bus-Off、控制器复位、重新装配或设置变化时递增。每
/// 个输入帧都必须携带取得时所属世代；旧世代帧绝不续接或覆盖新世代组装。
pub struct Receiver<'a> {
    node: u8,
    direction: Direction,
    mode: Mode,
    generation: u32,
    heartbeat: RxLane<'a>,
    short: RxLane<'a>,
    large: RxLane<'a>,
}

/// 可在旧 CAN I/O 已隔离后移动到新接收器的有限重连状态。
///
/// 本状态不含调用方缓冲、部分组装字节或未交付完整消息，且不可复制。它仅保留新世代
/// 所需的节点、方向、模式与各 lane 的编号保护窗口。
pub struct RxReconnectState {
    node: u8,
    direction: Direction,
    mode: Mode,
    generation: u32,
    heartbeat: RxLaneReconnectState,
    short: RxLaneReconnectState,
    large: RxLaneReconnectState,
}

struct RxLaneReconnectState {
    guard: Guard,
}

impl<'a> Receiver<'a> {
    /// 以调用方提供的固定缓冲构造接收器。
    pub fn new(
        node: u8,
        direction: Direction,
        mode: Mode,
        generation: u32,
        buffers: RxBuffers<'a>,
    ) -> Result<Self, Error> {
        if node > 127 {
            return Err(Error::InvalidNode);
        }
        if buffers.heartbeat.len() < HEARTBEAT_CAPACITY
            || buffers.short.len() < SHORT_CAPACITY
            || buffers.large.len() < LARGE_CAPACITY
        {
            return Err(Error::BufferTooSmall);
        }
        Ok(Self {
            node,
            direction,
            mode,
            generation,
            heartbeat: RxLane::new(buffers.heartbeat),
            short: RxLane::new(buffers.short),
            large: RxLane::new(buffers.large),
        })
    }

    /// 在旧 CAN I/O 已停止或隔离后，丢弃未交付消息并导出可移动重连状态。
    ///
    /// 调用前不得持有由 [`Self::take_completed`] 转交的缓冲；该前提与
    /// [`Self::reconfigure`] 相同。所有部分组装按放弃处理并进入保护期，完整但尚未交付
    /// 的消息会被舍弃。旧世代帧不能推进恢复后的接收器。
    pub fn into_reconnect_state(mut self, now_ms: u64) -> Result<RxReconnectState, Error> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(Error::InvalidGeneration)?;
        self.reconfigure(self.node, self.mode, generation, now_ms)?;
        self.heartbeat.completed = None;
        self.short.completed = None;
        self.large.completed = None;
        let Self {
            node,
            direction,
            mode,
            generation,
            heartbeat,
            short,
            large,
        } = self;
        let RxLane {
            guard: heartbeat_guard,
            ..
        } = heartbeat;
        let RxLane {
            guard: short_guard, ..
        } = short;
        let RxLane {
            guard: large_guard, ..
        } = large;
        Ok(RxReconnectState {
            node,
            direction,
            mode,
            generation,
            heartbeat: RxLaneReconnectState {
                guard: heartbeat_guard,
            },
            short: RxLaneReconnectState { guard: short_guard },
            large: RxLaneReconnectState { guard: large_guard },
        })
    }

    /// 以空的新组装缓冲恢复一个由 [`Self::into_reconnect_state`] 导出的接收器。
    pub fn from_reconnect_state(
        state: RxReconnectState,
        buffers: RxBuffers<'a>,
    ) -> Result<Self, Error> {
        let RxReconnectState {
            node,
            direction,
            mode,
            generation,
            heartbeat,
            short,
            large,
        } = state;
        let mut receiver = Self::new(node, direction, mode, generation, buffers)?;
        receiver.heartbeat.guard = heartbeat.guard;
        receiver.short.guard = short.guard;
        receiver.large.guard = large.guard;
        Ok(receiver)
    }

    /// 推进三个绝对组装期限；返回本次废弃的组装数。
    ///
    /// 调用方即使没有新帧也必须调用它。达到期限的后续片段不能复活原组。
    pub fn poll(&mut self, now_ms: u64) -> usize {
        let mut expired = 0;
        for lane in [Lane::Heartbeat, Lane::Short, Lane::Large] {
            let state = self.lane_mut(lane);
            if let Some(active) = state.active
                && now_ms >= active.deadline_ms
            {
                state.active = None;
                state
                    .guard
                    .protect(active.transfer_id, now_ms, lane.guard_ms());
                expired += 1;
            }
        }
        expired
    }

    /// 清理旧世代的部分组装并安装新的节点、模式和世代。
    ///
    /// 未取走的完整消息不会被静默覆盖；调用方须先取走或显式舍弃其缓冲。所有部分组装按
    /// 放弃处理并进入相应接收保护期。调用方须先隔离旧 CAN I/O；本调用只清理软件字节，
    /// 不能撤回已经进入控制器或总线的旧帧。`generation` 必须严格递增且不得在本次应用
    /// 运行内回绕复用。
    pub fn reconfigure(
        &mut self,
        node: u8,
        mode: Mode,
        generation: u32,
        now_ms: u64,
    ) -> Result<(), Error> {
        if node > 127 {
            return Err(Error::InvalidNode);
        }
        if generation <= self.generation {
            return Err(Error::InvalidGeneration);
        }
        if self.heartbeat.buffer.is_none()
            || self.short.buffer.is_none()
            || self.large.buffer.is_none()
        {
            return Err(Error::LaneBusy);
        }
        for lane in [Lane::Heartbeat, Lane::Short, Lane::Large] {
            self.abandon_active(lane, now_ms);
        }
        self.node = node;
        self.mode = mode;
        self.generation = generation;
        Ok(())
    }

    /// 接收一帧实际 CAN 元数据；`frame_generation` 必须是接收调用开始时的通信世代。
    pub fn receive(&mut self, frame: &Frame, now_ms: u64, frame_generation: u32) -> ReceiveResult {
        match self.receive_validated(frame, now_ms, frame_generation) {
            ValidatedReceiveResult::Ignored => ReceiveResult::Ignored,
            ValidatedReceiveResult::Incomplete { lane } => ReceiveResult::Incomplete { lane },
            ValidatedReceiveResult::Rejected { reason } => ReceiveResult::Rejected { reason },
            ValidatedReceiveResult::Complete { lane, message } => {
                let info = message.info();
                let buffer = message.into_buffer();
                let state = self.lane_mut(lane);
                state.buffer = Some(buffer);
                state.completed = Some(info);
                ReceiveResult::Complete { lane, info }
            }
        }
    }

    /// 接收一帧并在完整消息到达时转交绑定原缓冲的校验证明。
    ///
    /// 证明存活期间该 lane 没有可复用缓冲，因而不能在保留原始消息的异步处理期间被
    /// 覆盖、重新配置或舍弃。处理完后调用方须将
    /// [`ValidatedCompletedMessage::into_buffer`] 的结果交给 [`Self::replace_buffer`]。
    pub fn receive_validated(
        &mut self,
        frame: &Frame,
        now_ms: u64,
        frame_generation: u32,
    ) -> ValidatedReceiveResult<'a> {
        self.poll(now_ms);
        if frame_generation != self.generation {
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::StaleGeneration,
            };
        }
        let (lane, transfer_id, fragment) = match self.match_id(frame) {
            Ok(Some(parts)) => parts,
            Ok(None) => return ValidatedReceiveResult::Ignored,
            Err(reason) => return ValidatedReceiveResult::Rejected { reason },
        };
        if self.lane_ref(lane).completed.is_some() || self.lane_ref(lane).buffer.is_none() {
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::CompletedNotTaken,
            };
        }
        let data_len = match frame_length(frame, self.mode) {
            Ok(length) => length,
            Err(reason) => {
                if self.matches_active(lane, transfer_id) {
                    self.abandon_active(lane, now_ms);
                }
                return ValidatedReceiveResult::Rejected { reason };
            }
        };
        if fragment == 0 {
            return self.start_fragment(lane, transfer_id, frame.data(), data_len, now_ms);
        }
        self.continue_fragment(lane, transfer_id, fragment, frame.data(), data_len, now_ms)
    }

    /// 短暂借用某条已完成消息的完整字节。
    ///
    /// 该借用结束前 Rust 不允许再次可变推进同一个接收器；需跨等待持有时请改用
    /// [`Self::take_completed`]，它只使该通道暂时不可接收。
    #[must_use]
    pub fn message(&self, lane: Lane) -> Option<&[u8]> {
        let state = self.lane_ref(lane);
        let info = state.completed?;
        Some(&state.buffer.as_ref()?[..info.length])
    }

    /// 取走已完成消息及原组装缓冲，使该通道等待 [`Self::replace_buffer`]。
    pub fn take_completed(&mut self, lane: Lane) -> Option<CompletedMessage<'a>> {
        let state = self.lane_mut(lane);
        let info = state.completed.take()?;
        let buffer = state.buffer.take()?;
        Some(CompletedMessage { buffer, info })
    }

    /// 给一个已被取走的通道安装新的或归还的组装缓冲。
    pub fn replace_buffer(&mut self, lane: Lane, buffer: &'a mut [u8]) -> Result<(), Error> {
        if buffer.len() < lane.capacity() {
            return Err(Error::BufferTooSmall);
        }
        let state = self.lane_mut(lane);
        if state.buffer.is_some() || state.active.is_some() || state.completed.is_some() {
            return Err(Error::LaneBusy);
        }
        state.buffer = Some(buffer);
        Ok(())
    }

    /// 明确舍弃一条已完成但尚未交付的消息，保留其缓冲以继续接收。
    pub fn discard_completed(&mut self, lane: Lane) -> bool {
        self.lane_mut(lane).completed.take().is_some()
    }

    fn match_id(&self, frame: &Frame) -> Result<Option<(Lane, u8, u16)>, RejectReason> {
        if !frame.extended {
            return Ok(None);
        }
        if frame.id > MAX_EXTENDED_ID {
            return Err(RejectReason::InvalidIdentifier);
        }
        let node = ((frame.id >> 22) & 0x7f) as u8;
        let direction = (frame.id >> 21) & 1;
        let lane_bits = ((frame.id >> 19) & 3) as u8;
        if node != self.node || direction != direction_bit(self.direction) {
            return Ok(None);
        }
        let Some(lane) = Lane::from_bits(lane_bits) else {
            return Ok(None);
        };
        Ok(Some((
            lane,
            ((frame.id >> 12) & 0x7f) as u8,
            (frame.id & 0x0fff) as u16,
        )))
    }

    fn start_fragment(
        &mut self,
        lane: Lane,
        transfer_id: u8,
        data: &[u8],
        data_len: usize,
        now_ms: u64,
    ) -> ValidatedReceiveResult<'a> {
        if self.matches_active(lane, transfer_id) {
            self.abandon_active(lane, now_ms);
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::InvalidFragment,
            };
        }
        if self.lane_mut(lane).guard.is_protected(transfer_id, now_ms) {
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::TransferProtected,
            };
        }
        if data_len < 4 {
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::InvalidPrefix,
            };
        }
        let (length, kind) = match protocol::probe_prefix(&data[..data_len], self.direction) {
            Ok(Prefix::Complete { length, kind }) => (length, kind),
            Ok(Prefix::Incomplete) | Err(_) => {
                return ValidatedReceiveResult::Rejected {
                    reason: RejectReason::InvalidPrefix,
                };
            }
        };
        if lane_for(kind.is_heartbeat(), length) != lane {
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::WrongLane,
            };
        }
        let expected = expected_payload(self.mode, length, 0);
        if !matches_payload(data, data_len, expected) {
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::InvalidFragment,
            };
        }
        let logical = expected.logical;
        let deadline_ms = now_ms.saturating_add(lane.timeout_ms());
        // 先完整确认新首片，才废弃另一编号的活动组。
        if self.lane_ref(lane).active.is_some() {
            self.abandon_active(lane, now_ms);
        }
        {
            let state = self.lane_mut(lane);
            let Some(buffer) = state.buffer.as_deref_mut() else {
                return ValidatedReceiveResult::Rejected {
                    reason: RejectReason::CompletedNotTaken,
                };
            };
            buffer[..logical].copy_from_slice(&data[..logical]);
            state.active = Some(RxActive {
                transfer_id,
                next_fragment: 1,
                length,
                deadline_ms,
            });
        }
        if logical == length {
            self.finish(lane, now_ms)
        } else {
            ValidatedReceiveResult::Incomplete { lane }
        }
    }

    fn continue_fragment(
        &mut self,
        lane: Lane,
        transfer_id: u8,
        fragment: u16,
        data: &[u8],
        data_len: usize,
        now_ms: u64,
    ) -> ValidatedReceiveResult<'a> {
        let Some(active) = self.lane_ref(lane).active else {
            return ValidatedReceiveResult::Ignored;
        };
        if active.transfer_id != transfer_id {
            return ValidatedReceiveResult::Ignored;
        }
        if fragment != active.next_fragment {
            self.abandon_active(lane, now_ms);
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::InvalidFragment,
            };
        }
        let expected = expected_payload(self.mode, active.length, fragment);
        if !matches_payload(data, data_len, expected) {
            self.abandon_active(lane, now_ms);
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::InvalidFragment,
            };
        }
        let offset = usize::from(fragment) * self.mode.quantum();
        if self.lane_ref(lane).buffer.is_none() {
            self.abandon_active(lane, now_ms);
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::CompletedNotTaken,
            };
        }
        {
            let state = self.lane_mut(lane);
            if let Some(buffer) = state.buffer.as_deref_mut() {
                buffer[offset..offset + expected.logical]
                    .copy_from_slice(&data[..expected.logical]);
            }
            if let Some(current) = state.active.as_mut() {
                current.next_fragment += 1;
            }
        }
        if offset + expected.logical == active.length {
            self.finish(lane, now_ms)
        } else {
            ValidatedReceiveResult::Incomplete { lane }
        }
    }

    fn finish(&mut self, lane: Lane, now_ms: u64) -> ValidatedReceiveResult<'a> {
        let direction = self.direction;
        let state = self.lane_mut(lane);
        let Some(active) = state.active.take() else {
            return ValidatedReceiveResult::Ignored;
        };
        let Some(buffer) = state.buffer.take() else {
            state
                .guard
                .protect(active.transfer_id, now_ms, lane.guard_ms());
            return ValidatedReceiveResult::Rejected {
                reason: RejectReason::CompletedNotTaken,
            };
        };
        state
            .guard
            .protect(active.transfer_id, now_ms, lane.guard_ms());
        match ValidatedCompletedMessage::validate(buffer, active.length, direction) {
            Ok(message) => ValidatedReceiveResult::Complete { lane, message },
            Err((buffer, _)) => {
                state.buffer = Some(buffer);
                ValidatedReceiveResult::Rejected {
                    reason: RejectReason::InvalidMessage,
                }
            }
        }
    }

    fn matches_active(&self, lane: Lane, transfer_id: u8) -> bool {
        self.lane_ref(lane)
            .active
            .is_some_and(|active| active.transfer_id == transfer_id)
    }

    fn abandon_active(&mut self, lane: Lane, now_ms: u64) {
        let state = self.lane_mut(lane);
        if let Some(active) = state.active.take() {
            state
                .guard
                .protect(active.transfer_id, now_ms, lane.guard_ms());
        }
    }

    fn lane_ref(&self, lane: Lane) -> &RxLane<'a> {
        match lane {
            Lane::Heartbeat => &self.heartbeat,
            Lane::Short => &self.short,
            Lane::Large => &self.large,
        }
    }

    fn lane_mut(&mut self, lane: Lane) -> &mut RxLane<'a> {
        match lane {
            Lane::Heartbeat => &mut self.heartbeat,
            Lane::Short => &mut self.short,
            Lane::Large => &mut self.large,
        }
    }
}
