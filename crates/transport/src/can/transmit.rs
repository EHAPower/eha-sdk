// Copyright The eha-sdk Contributors

use protocol::Direction;

use crate::guard::Guard;

use super::{
    Error, Frame, Lane, Mode,
    types::{HEARTBEAT_CAPACITY, LARGE_CAPACITY, SHORT_CAPACITY},
    wire::{compose_id, expected_payload, lane_for},
};

/// 单帧的待本地提交项。
///
/// 调用方将 `frame` 交给实际 CAN 驱动；只有驱动明确本地接受该帧后，才以 `token` 调用
/// [`Transmitter::complete`] 推进到下一片。令牌不能代表对端接收或业务完成。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingFrame {
    /// 待交驱动的完整 CAN 元数据和数据。
    pub frame: Frame,
    /// 仅匹配这次本地提交的世代、通道、编号与分片令牌。
    pub token: TxToken,
}

/// 不透明的本地提交匹配令牌。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TxToken {
    generation: u32,
    lane: Lane,
    operation: u64,
    transfer_id: u8,
    fragment: u16,
}

/// 驱动对一次待提交帧的本地结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitResult {
    /// 驱动已本地接受此帧；这不证明 CAN ACK、对端接收或业务执行。
    Accepted,
    /// 驱动在本地拒绝或报告失败。
    Failed,
    /// 调用方确认本地提交未交给驱动，或相关 I/O 已经结束或隔离。
    ///
    /// 若不能确认驱动是否已接纳，不能仅因丢弃 future 就报告本项；调用方须先隔离旧 I/O，
    /// 再以严格递增世代调用 [`Transmitter::reconfigure`]。
    Cancelled,
}

/// 一次 [`Transmitter::complete`] 后可观察的本地发送事件。
///
/// `FrameAccepted` 只表示该帧被底层 I/O 接口接纳；`MessageCommitted` 表示最后一片也已获
/// 本地接纳；二者都不证明链路层 ACK、对端取得完整消息或业务执行。`Abandoned` 明确表示
/// 当前完整消息没有继续提交后续片段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitEvent {
    /// 一片已本地接纳，完整消息仍有后续片段。
    FrameAccepted {
        /// 所属隔离通道。
        lane: Lane,
    },
    /// 最后一片已本地接纳，整条消息的本地提交已完成。
    MessageCommitted {
        /// 所属隔离通道。
        lane: Lane,
    },
    /// 当前消息因失败或取消而放弃，后续片段不会再提交。
    Abandoned {
        /// 所属隔离通道。
        lane: Lane,
        /// 引起放弃的本地结果。
        result: SubmitResult,
    },
}

#[derive(Clone, Copy)]
struct TxActive {
    length: usize,
    operation: u64,
    transfer_id: u8,
    next_fragment: u16,
    awaiting: Option<u16>,
}

struct TxLane<'a> {
    buffer: &'a mut [u8],
    active: Option<TxActive>,
    next_transfer_id: u8,
    guard: Guard,
}

impl<'a> TxLane<'a> {
    fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer,
            active: None,
            next_transfer_id: 0,
            guard: Guard::new(),
        }
    }
}

/// 调用方提供的三条发送快照缓冲。
pub struct TxBuffers<'a> {
    /// 心跳通道快照缓冲。
    pub heartbeat: &'a mut [u8],
    /// 普通短消息通道快照缓冲。
    pub short: &'a mut [u8],
    /// 大消息通道快照缓冲。
    pub large: &'a mut [u8],
}

/// 可在旧 CAN I/O 已隔离后移动到新发送器的有限重连状态。
///
/// 本状态不含调用方缓冲或待发送消息，且不可复制；它只保留本应用运行内不可复用的
/// 世代、发送操作号、传输号与保护窗口。用 [`Transmitter::from_reconnect_state`] 消费它后，
/// 旧完成令牌和被放弃消息都不能推进新发送器。
pub struct TxReconnectState {
    node: u8,
    direction: Direction,
    mode: Mode,
    generation: u32,
    next_operation: u64,
    heartbeat: TxLaneReconnectState,
    short: TxLaneReconnectState,
    large: TxLaneReconnectState,
}

struct TxLaneReconnectState {
    next_transfer_id: u8,
    guard: Guard,
}

/// CAN 发送器，冻结调用方提供的通道缓冲，按通道独立推进本地帧提交。
///
/// 调用方通过 [`Self::buffer_mut`] 原位编码，并以 [`Self::start_buffer`] 冻结精确完整字节。
/// 三个通道各有独立状态，发送大消息不会持续借用整个发送器，也不会阻塞其他通道取得下一帧。
pub struct Transmitter<'a> {
    node: u8,
    direction: Direction,
    mode: Mode,
    generation: u32,
    next_operation: u64,
    heartbeat: TxLane<'a>,
    short: TxLane<'a>,
    large: TxLane<'a>,
}

impl<'a> Transmitter<'a> {
    /// 构造一个方向固定的 CAN 发送器。
    pub fn new(
        node: u8,
        direction: Direction,
        mode: Mode,
        generation: u32,
        buffers: TxBuffers<'a>,
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
            next_operation: 1,
            heartbeat: TxLane::new(buffers.heartbeat),
            short: TxLane::new(buffers.short),
            large: TxLane::new(buffers.large),
        })
    }

    /// 在旧 CAN I/O 已停止或隔离后，放弃未交付消息并导出可移动重连状态。
    ///
    /// 该调用严格递增通信世代，因此所有旧 [`TxToken`] 都被拒绝。它不撤回已交给控制器
    /// 或总线的帧；调用方必须先完成旧 I/O 的隔离，再调用本方法。
    pub fn into_reconnect_state(mut self, now_ms: u64) -> Result<TxReconnectState, Error> {
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(Error::InvalidGeneration)?;
        self.reconfigure(self.node, self.mode, generation, now_ms)?;
        let Self {
            node,
            direction,
            mode,
            generation,
            next_operation,
            heartbeat,
            short,
            large,
        } = self;
        let TxLane {
            next_transfer_id: heartbeat_next_transfer_id,
            guard: heartbeat_guard,
            ..
        } = heartbeat;
        let TxLane {
            next_transfer_id: short_next_transfer_id,
            guard: short_guard,
            ..
        } = short;
        let TxLane {
            next_transfer_id: large_next_transfer_id,
            guard: large_guard,
            ..
        } = large;
        Ok(TxReconnectState {
            node,
            direction,
            mode,
            generation,
            next_operation,
            heartbeat: TxLaneReconnectState {
                next_transfer_id: heartbeat_next_transfer_id,
                guard: heartbeat_guard,
            },
            short: TxLaneReconnectState {
                next_transfer_id: short_next_transfer_id,
                guard: short_guard,
            },
            large: TxLaneReconnectState {
                next_transfer_id: large_next_transfer_id,
                guard: large_guard,
            },
        })
    }

    /// 以新缓冲恢复一个由 [`Self::into_reconnect_state`] 导出的发送器。
    ///
    /// 状态会被消费，不能同时恢复两个发送器；所有新缓冲必须满足 [`Self::new`] 的容量要求。
    pub fn from_reconnect_state(
        state: TxReconnectState,
        buffers: TxBuffers<'a>,
    ) -> Result<Self, Error> {
        let TxReconnectState {
            node,
            direction,
            mode,
            generation,
            next_operation,
            heartbeat,
            short,
            large,
        } = state;
        let mut transmitter = Self::new(node, direction, mode, generation, buffers)?;
        transmitter.next_operation = next_operation;
        transmitter.heartbeat.next_transfer_id = heartbeat.next_transfer_id;
        transmitter.heartbeat.guard = heartbeat.guard;
        transmitter.short.next_transfer_id = short.next_transfer_id;
        transmitter.short.guard = short.guard;
        transmitter.large.next_transfer_id = large.next_transfer_id;
        transmitter.large.guard = large.guard;
        Ok(transmitter)
    }

    /// 短暂借用空闲通道的完整快照缓冲，供调用方原位编码。
    ///
    /// 在 [`Self::start_buffer`]、提交完成或 [`Self::abandon`] 前，此通道不能再次借用。返回
    /// 的借用结束后其他通道仍可单独推进。
    pub fn buffer_mut(&mut self, lane: Lane) -> Result<&mut [u8], Error> {
        let state = self.tx_lane_mut(lane);
        if state.active.is_some() {
            return Err(Error::SendBusy);
        }
        Ok(state.buffer)
    }

    /// 校验并开始发送已经写入指定通道缓冲的完整公共消息。
    ///
    /// `length` 必须是精确公共消息长度；此调用从缓冲中冻结该范围，直到最后一片本地提交或
    /// 放弃。成功只表示绑定建立了待发分片；必须继续调用 [`Self::next_frame`] 和
    /// [`Self::complete`] 才会出现本地提交结果。
    pub fn start_buffer(&mut self, lane: Lane, length: usize, now_ms: u64) -> Result<(), Error> {
        let direction = self.direction;
        {
            let state = self.tx_lane_mut(lane);
            if state.active.is_some() {
                return Err(Error::SendBusy);
            }
            if length > state.buffer.len() {
                return Err(Error::BufferTooSmall);
            }
            let info = protocol::validate(&state.buffer[..length], direction)
                .map_err(|_| Error::InvalidMessage)?;
            if lane_for(info.kind.is_heartbeat(), info.length) != lane {
                return Err(Error::InvalidMessage);
            }
            let id = state.next_transfer_id;
            if state.guard.is_protected(id, now_ms) {
                return Err(Error::TransferProtected);
            }
        }
        let operation = self.next_operation;
        self.next_operation = self
            .next_operation
            .checked_add(1)
            .ok_or(Error::OperationExhausted)?;
        let state = self.tx_lane_mut(lane);
        let id = state.next_transfer_id;
        state.next_transfer_id = state.next_transfer_id.wrapping_add(1) & 0x7f;
        state.active = Some(TxActive {
            length,
            operation,
            transfer_id: id,
            next_fragment: 0,
            awaiting: None,
        });
        Ok(())
    }

    /// 显式复制完整消息到其所属通道缓冲后开始发送。
    ///
    /// 大配置会产生一份绑定发送快照；若调用方已经在该通道缓冲中编码，请优先使用
    /// [`Self::buffer_mut`] 与 [`Self::start_buffer`] 避免这次复制。
    pub fn copy_and_start(&mut self, message: &[u8], now_ms: u64) -> Result<Lane, Error> {
        let info =
            protocol::validate(message, self.direction).map_err(|_| Error::InvalidMessage)?;
        let lane = lane_for(info.kind.is_heartbeat(), info.length);
        self.buffer_mut(lane)?[..message.len()].copy_from_slice(message);
        self.start_buffer(lane, message.len(), now_ms)?;
        Ok(lane)
    }

    /// 提供一条尚未交驱动的分片；调用方可暂停而不调用本方法。
    pub fn next_frame(&mut self, lane: Lane) -> Option<PendingFrame> {
        let node = self.node;
        let direction = self.direction;
        let mode = self.mode;
        let generation = self.generation;
        let state = self.tx_lane_mut(lane);
        let active = state.active?;
        if active.awaiting.is_some() {
            return None;
        }
        let fragment = active.next_fragment;
        let expected = expected_payload(mode, active.length, fragment);
        let offset = usize::from(fragment) * mode.quantum();
        let mut data = [0; 64];
        data[..expected.logical].copy_from_slice(&state.buffer[offset..offset + expected.logical]);
        if let Some(active) = state.active.as_mut() {
            active.awaiting = Some(fragment);
        }
        Some(PendingFrame {
            frame: Frame {
                id: compose_id(node, direction, lane, active.transfer_id, fragment),
                extended: true,
                rtr: false,
                fdf: matches!(mode, Mode::Fd),
                brs: matches!(mode, Mode::Fd),
                esi: false,
                dlc: expected.dlc,
                data_len: expected.data_len as u8,
                data,
            },
            token: TxToken {
                generation,
                lane,
                operation: active.operation,
                transfer_id: active.transfer_id,
                fragment,
            },
        })
    }

    /// 处理该令牌对应帧的本地提交结果。
    ///
    /// 失败、取消、重复完成或旧世代令牌都不能推进新消息。失败或取消会放弃整个未完成消息，
    /// 并从当前时刻开始其编号保护期。传入 [`SubmitResult::Cancelled`] 前，调用方必须已经确认
    /// 该帧未交给驱动，或已结束或隔离其 I/O；接纳状态未知时，先隔离 I/O 并用严格新世代
    /// [`Self::reconfigure`]，不能把丢弃 future 当成底层取消成功。
    pub fn complete(
        &mut self,
        token: TxToken,
        result: SubmitResult,
        now_ms: u64,
    ) -> Result<SubmitEvent, Error> {
        if token.generation != self.generation {
            return Err(Error::StaleCompletion);
        }
        let mode = self.mode;
        let state = self.tx_lane_mut(token.lane);
        let Some(active) = state.active else {
            return Err(Error::StaleCompletion);
        };
        if active.operation != token.operation
            || active.transfer_id != token.transfer_id
            || active.awaiting != Some(token.fragment)
        {
            return Err(Error::StaleCompletion);
        }
        match result {
            SubmitResult::Accepted => {
                let is_last = expected_payload(mode, active.length, token.fragment).is_last;
                let Some(active) = state.active.as_mut() else {
                    return Err(Error::StaleCompletion);
                };
                active.awaiting = None;
                active.next_fragment += 1;
                if is_last {
                    let id = active.transfer_id;
                    state.active = None;
                    state.guard.protect(id, now_ms, token.lane.guard_ms());
                    Ok(SubmitEvent::MessageCommitted { lane: token.lane })
                } else {
                    Ok(SubmitEvent::FrameAccepted { lane: token.lane })
                }
            }
            SubmitResult::Failed | SubmitResult::Cancelled => {
                self.abandon(token.lane, now_ms);
                Ok(SubmitEvent::Abandoned {
                    lane: token.lane,
                    result,
                })
            }
        }
    }

    /// 放弃一条未完成发送。
    ///
    /// 只能在调用方尚未把待发帧交给驱动，或驱动已经明确结束、取消或隔离该 I/O 时调用。
    /// 若驱动调用可能仍在等待或其结果未知，调用方必须先隔离旧 I/O，并通过严格递增世代的
    /// [`Self::reconfigure`] 使迟到完成失效。已被驱动接受的片段不能由本模块撤回。
    pub fn abandon(&mut self, lane: Lane, now_ms: u64) {
        let state = self.tx_lane_mut(lane);
        if let Some(active) = state.active.take() {
            state
                .guard
                .protect(active.transfer_id, now_ms, lane.guard_ms());
        }
    }

    /// 在 Bus-Off、控制器复位、重新装配或设置改变后放弃所有未完成分片并更新参数。
    ///
    /// 调用方须先停止、复位或隔离旧 I/O；本模块只清理软件状态，不证明控制器队列已清空。
    /// `generation` 必须严格递增且不得在本次应用运行内回绕复用。发送编号和重用保护历史
    /// 保留在本应用运行内；旧提交令牌因世代不符被拒绝。
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
        for lane in [Lane::Heartbeat, Lane::Short, Lane::Large] {
            self.abandon(lane, now_ms);
        }
        self.node = node;
        self.mode = mode;
        self.generation = generation;
        Ok(())
    }

    fn tx_lane_mut(&mut self, lane: Lane) -> &mut TxLane<'a> {
        match lane {
            Lane::Heartbeat => &mut self.heartbeat,
            Lane::Short => &mut self.short,
            Lane::Large => &mut self.large,
        }
    }
}
