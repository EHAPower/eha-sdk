// Copyright The eha_controller Contributors

use protocol::{Direction, validate};

use crate::guard::Guard;

use super::cobs::{CobsCursor, next_cobs_byte};
use super::{Lane, PACKET_LEN, Phase, PhaseError, SHORT_MESSAGE_LIMIT};

/// 发送本地状态错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendError {
    /// 公共消息未通过完整格式校验或方向不匹配。
    InvalidMessage,
    /// 原位消息缓冲不能容纳调用方声明的有效长度。
    MessageBufferTooSmall,
    /// 已校验消息应当使用另一个固定 lane。
    WrongLane,
    /// 目标 lane 已有未完成消息。
    LaneBusy(Lane),
    /// 轮转后的编号仍处于 sender guard。
    TransferProtected(Lane),
    /// 调用方缓冲不足一个 Bulk 包。
    PacketBufferTooSmall,
    /// 另一 lane 的 COBS 块尚未完成本地写入。
    StreamBusy,
    /// 该 lane 当前有尚未确认的本地端点 I/O。
    AwaitingCompletion,
    /// 本地 I/O 操作序号耗尽；不会复用旧完成令牌。
    TokenExhausted,
    /// 阶段序号耗尽；调用方须重建发送器。
    PhaseExhausted,
}

/// 一次端点包提交的阶段令牌。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TxToken {
    phase: Phase,
    serial: u64,
    lane: Lane,
}

/// 写入端点前由 [`Sender::prepare_packet`] 返回的本地包说明。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreparedPacket {
    /// 调用 [`Sender::accepted`]、[`Sender::completed`] 或 [`Sender::cancel`] 所需的令牌。
    pub token: TxToken,
    /// 已写入调用方输出缓冲的字节数，范围为 `1..=64`。
    pub length: usize,
}

/// 一个发送状态变更。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendEvent {
    /// 此令牌不属于当前阶段或不在预期状态，已忽略。
    Ignored,
    /// 端点已本地接纳该包；还须等待其写调用完成。
    PacketAccepted,
    /// 一个包的本地写调用已完成，消息尚有字节。
    PacketCommitted,
    /// 整条公共消息的最后一个 USB 包已本地提交完成。
    MessageCommitted { lane: Lane },
    /// 当前消息因取消或明确放弃而结束。
    Aborted { lane: Lane },
    /// 端点报告本地 I/O 失败；此消息已放弃，不能由旧完成通知推进新消息。
    Failed { lane: Lane },
    /// 已接纳 I/O 被取消或失败，发送器已进入新端点阶段。
    PhaseChanged {
        lane: Lane,
        phase: Phase,
        failed: bool,
    },
    /// 阶段序号耗尽，发送器已禁用，必须重建。
    PhaseExhausted,
}

#[derive(Clone, Copy, Debug)]
enum SendLane {
    Idle,
    Queued {
        length: usize,
        transfer: u8,
        fragment_index: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PacketState {
    Offered {
        token: TxToken,
        length: usize,
        after: CobsCursor,
    },
    Accepted {
        token: TxToken,
        length: usize,
        after: CobsCursor,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stream {
    lane: Lane,
    cursor: CobsCursor,
    packet: Option<PacketState>,
}
/// USB IN 字节流发送器。
///
/// 调用方在 lane 空闲时从 [`Self::buffer_mut`] 借用对应缓冲并原位编码，随后以
/// [`Self::begin_buffer`] 冻结其有效前缀，直至消息提交、取消或阶段改变。每次
/// [`Self::prepare_packet`] 都复制至调用方提供的至多64字节 I/O 缓冲，
/// 因而等待端点不会长期借用整个发送器，也允许在 lane-2 块之间选择其他 lane。
#[derive(Debug)]
pub struct Sender<'a> {
    direction: Direction,
    pub(super) phase: Phase,
    phase_exhausted: bool,
    lanes: [SendLane; 3],
    buffers: [&'a mut [u8]; 3],
    next_transfer: [u8; 3],
    guards: [Guard; 3],
    stream: Option<Stream>,
    next_token: u64,
}

/// 可在旧 Bulk I/O 已隔离后移动到新发送器的有限重连状态。
///
/// 本状态不含消息缓冲、COBS 半包或待提交端点 I/O，且不可复制。它保留端点阶段、每条
/// lane 的下一个传输号和保护窗口，以及本地 I/O 令牌序号，避免旧完成通知或旧传输号在
/// 同一调用方运行内推进恢复后的发送器。
pub struct SenderReconnectState {
    direction: Direction,
    phase: Phase,
    next_transfer: [u8; 3],
    guards: [Guard; 3],
    next_token: u64,
}

impl<'a> Sender<'a> {
    /// 创建发送器；所有传输编号从零开始。
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
            lanes: [SendLane::Idle, SendLane::Idle, SendLane::Idle],
            buffers: [heartbeat_buffer, short_buffer, long_buffer],
            next_transfer: [0; 3],
            guards: [Guard::new(), Guard::new(), Guard::new()],
            stream: None,
            next_token: 0,
        }
    }

    /// 当前端点阶段。
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// 在旧 Bulk I/O 已结束或隔离后，开始新阶段并导出可移动重连状态。
    ///
    /// 所有冻结消息、COBS 半包和待提交端点 I/O 都会被放弃并进入相应保护期。该调用不
    /// 撤回已交给端点的字节；调用方必须先隔离旧 I/O，再调用本方法。
    pub fn into_reconnect_state(mut self, now_ms: u64) -> Result<SenderReconnectState, PhaseError> {
        self.phase_changed(now_ms)?;
        let Self {
            direction,
            phase,
            next_transfer,
            guards,
            next_token,
            ..
        } = self;
        Ok(SenderReconnectState {
            direction,
            phase,
            next_transfer,
            guards,
            next_token,
        })
    }

    /// 以新消息缓冲恢复一个由 [`Self::into_reconnect_state`] 导出的发送器。
    ///
    /// 状态被消费，不能同时恢复两个发送器。恢复后的发送器没有待发送消息或待完成 I/O。
    pub fn from_reconnect_state(
        state: SenderReconnectState,
        heartbeat_buffer: &'a mut [u8],
        short_buffer: &'a mut [u8],
        long_buffer: &'a mut [u8],
    ) -> Self {
        let SenderReconnectState {
            direction,
            phase,
            next_transfer,
            guards,
            next_token,
        } = state;
        let mut sender = Self::new(direction, heartbeat_buffer, short_buffer, long_buffer);
        sender.phase = phase;
        sender.next_transfer = next_transfer;
        sender.guards = guards;
        sender.next_token = next_token;
        sender
    }

    /// 在对应 lane 空闲时借用其可写发送缓冲。
    ///
    /// 调用方将完整公共消息原位编码到返回切片后，以 [`Self::begin_buffer`] 指定
    /// 有效长度；消息结束后同一缓冲会再次由本函数提供，无需销毁发送器。
    pub fn buffer_mut(&mut self, lane: Lane) -> Option<&mut [u8]> {
        if self.phase_exhausted
            || !matches!(self.lanes[lane.index()], SendLane::Idle)
            || self.stream.is_some_and(|stream| stream.lane == lane)
        {
            return None;
        }
        Some(&mut self.buffers[lane.index()])
    }

    /// 校验并冻结已原位写入 `lane` 缓冲的完整公共消息。
    pub fn begin_buffer(
        &mut self,
        lane: Lane,
        length: usize,
        now_ms: u64,
    ) -> Result<(), SendError> {
        if self.phase_exhausted {
            return Err(SendError::PhaseExhausted);
        }
        let index = lane.index();
        if !matches!(self.lanes[index], SendLane::Idle) {
            return Err(SendError::LaneBusy(lane));
        }
        if length > self.buffers[index].len() {
            return Err(SendError::MessageBufferTooSmall);
        }
        let info = validate(&self.buffers[index][..length], self.direction)
            .map_err(|_| SendError::InvalidMessage)?;
        let expected_lane = if info.kind.is_heartbeat() {
            Lane::Heartbeat
        } else if info.length <= SHORT_MESSAGE_LIMIT {
            Lane::Short
        } else {
            Lane::Long
        };
        if expected_lane != lane {
            return Err(SendError::WrongLane);
        }
        let transfer = self.next_transfer[index];
        if self.guards[index].is_protected(transfer, now_ms) {
            return Err(SendError::TransferProtected(lane));
        }
        self.next_transfer[index] = (transfer + 1) & 0x7f;
        self.lanes[index] = SendLane::Queued {
            length,
            transfer,
            fragment_index: 0,
        };
        Ok(())
    }

    /// 将当前 COBS 块的下一个不超过64字节的包写入 `out`。
    ///
    /// 端点接受该包后调用 [`Self::accepted`]；其写调用完成后调用
    /// [`Self::completed`]。在一个 COBS 块尚未完成时，只能继续该 lane。
    pub fn prepare_packet(
        &mut self,
        lane: Lane,
        out: &mut [u8],
    ) -> Result<Option<PreparedPacket>, SendError> {
        if self.phase_exhausted {
            return Err(SendError::PhaseExhausted);
        }
        if out.len() < PACKET_LEN {
            return Err(SendError::PacketBufferTooSmall);
        }
        if let Some(stream) = self.stream {
            if stream.lane != lane {
                return Err(SendError::StreamBusy);
            }
            if stream.packet.is_some() {
                return Err(SendError::AwaitingCompletion);
            }
        } else if matches!(self.lanes[lane.index()], SendLane::Idle) {
            return Ok(None);
        } else {
            self.start_block(lane)?;
        }

        let stream = self.stream.expect("stream was created for queued lane");
        let SendLane::Queued {
            length,
            transfer,
            fragment_index,
        } = self.lanes[lane.index()]
        else {
            return Ok(None);
        };
        let offset = usize::from(fragment_index) * SHORT_MESSAGE_LIMIT;
        let fragment_len = (length - offset).min(SHORT_MESSAGE_LIMIT);
        let fragment = &self.buffers[lane.index()][offset..offset + fragment_len];
        let mut after = stream.cursor;
        let mut packet_length = 0;
        while packet_length < PACKET_LEN {
            let Some(byte) = next_cobs_byte(&mut after, lane, transfer, fragment_index, fragment)
            else {
                break;
            };
            out[packet_length] = byte;
            packet_length += 1;
        }
        if packet_length == 0 {
            return Ok(None);
        }
        let next = self
            .next_token
            .checked_add(1)
            .ok_or(SendError::TokenExhausted)?;
        let token = TxToken {
            phase: self.phase,
            serial: self.next_token,
            lane,
        };
        self.next_token = next;
        self.stream.as_mut().expect("stream remains active").packet = Some(PacketState::Offered {
            token,
            length: packet_length,
            after,
        });
        Ok(Some(PreparedPacket {
            token,
            length: packet_length,
        }))
    }

    /// 记录端点已经本地接纳一个先前准备的包。
    ///
    /// 只能在驱动已给出实际本地接纳证据时调用，不能在创建 write future 或提交
    /// 候选缓冲时提前调用；它不表示主机已读取或业务已经完成。
    pub fn accepted(&mut self, token: TxToken) -> SendEvent {
        let Some(stream) = self.stream.as_mut() else {
            return SendEvent::Ignored;
        };
        if token.phase != self.phase || stream.lane != token.lane {
            return SendEvent::Ignored;
        }
        let Some(PacketState::Offered {
            token: expected,
            length,
            after,
        }) = stream.packet
        else {
            return SendEvent::Ignored;
        };
        if expected != token {
            return SendEvent::Ignored;
        }
        stream.packet = Some(PacketState::Accepted {
            token,
            length,
            after,
        });
        SendEvent::PacketAccepted
    }

    /// 记录端点的本地写调用完成。最后一个包完成才表示整条消息本地提交。
    pub fn completed(&mut self, token: TxToken, now_ms: u64) -> SendEvent {
        let Some(stream) = self.stream else {
            return SendEvent::Ignored;
        };
        if token.phase != self.phase || stream.lane != token.lane {
            return SendEvent::Ignored;
        }
        let Some(PacketState::Accepted {
            token: expected,
            length: _,
            after,
        }) = stream.packet
        else {
            return SendEvent::Ignored;
        };
        if expected != token {
            return SendEvent::Ignored;
        }
        let lane = stream.lane;
        let ends_block = after == CobsCursor::Done;
        if !ends_block {
            let stream = self.stream.as_mut().expect("stream checked above");
            stream.cursor = after;
            stream.packet = None;
            return SendEvent::PacketCommitted;
        }
        let (transfer, last_fragment) = match self.lanes[lane.index()] {
            SendLane::Queued {
                length,
                transfer,
                fragment_index,
            } => {
                let sent = usize::from(fragment_index) * SHORT_MESSAGE_LIMIT;
                (transfer, sent + SHORT_MESSAGE_LIMIT >= length)
            }
            SendLane::Idle => return SendEvent::Ignored,
        };
        self.stream = None;
        if last_fragment {
            self.guards[lane.index()].protect(transfer, now_ms, lane.guard_duration_ms());
            self.lanes[lane.index()] = SendLane::Idle;
            SendEvent::MessageCommitted { lane }
        } else {
            if let SendLane::Queued { fragment_index, .. } = &mut self.lanes[lane.index()] {
                *fragment_index += 1;
            }
            SendEvent::PacketCommitted
        }
    }

    /// 取消一个已经准备或已接纳、但尚未完成的包及其所属消息。
    ///
    /// 若包已被端点接纳，调用方必须先结束或隔离该 I/O；本函数随即开始新
    /// 传输阶段并丢弃全部未完成发送，避免旧字节与新 COBS 流拼接。
    /// 仅当调用方确定该包仍为 `Offered`、从未被端点接纳时，取消才保留当前
    /// 阶段。若 write future 被取消、超时或出错而无法确认端点是否接纳，调用方
    /// 必须先结束或隔离旧 I/O，再直接调用 [`Self::phase_changed`]。
    pub fn cancel(&mut self, token: TxToken, now_ms: u64) -> SendEvent {
        self.end_failed_or_cancelled(token, now_ms, false)
    }

    /// 报告端点对一个已准备或已接纳包的本地 I/O 失败，并放弃所属消息。
    ///
    /// 接纳后失败同样开始新阶段。库不能撤回已交给端点的字节；调用方负责先
    /// 结束或隔离旧 I/O，才可把新阶段字节交给端点。
    /// 若失败发生在仍为 `Offered` 的候选写入，调用方也只能在确知端点从未接纳
    /// 该包时使用本函数；其他不确定情况按 [`Self::phase_changed`] 处理。
    pub fn failed(&mut self, token: TxToken, now_ms: u64) -> SendEvent {
        self.end_failed_or_cancelled(token, now_ms, true)
    }

    fn end_failed_or_cancelled(&mut self, token: TxToken, now_ms: u64, failed: bool) -> SendEvent {
        let Some(stream) = self.stream else {
            return SendEvent::Ignored;
        };
        if token.phase != self.phase || stream.lane != token.lane {
            return SendEvent::Ignored;
        }
        let accepted = match stream.packet {
            Some(PacketState::Offered {
                token: expected, ..
            }) if expected == token => false,
            Some(PacketState::Accepted {
                token: expected, ..
            }) if expected == token => true,
            Some(PacketState::Offered { .. } | PacketState::Accepted { .. }) | None => {
                return SendEvent::Ignored;
            }
        };
        if accepted {
            return match self.phase_changed(now_ms) {
                Ok(phase) => SendEvent::PhaseChanged {
                    lane: stream.lane,
                    phase,
                    failed,
                },
                Err(PhaseError::Exhausted) => SendEvent::PhaseExhausted,
                // 此变体只由接收器的证明缓冲产生；若共享错误类型日后在发送侧可达，
                // 也必须停止使用当前阶段并由调用方重建，而不能在已接纳包后 panic。
                Err(PhaseError::BufferBusy) => SendEvent::PhaseExhausted,
            };
        } else {
            self.abandon_lane(stream.lane, now_ms);
        }
        if failed {
            SendEvent::Failed { lane: stream.lane }
        } else {
            SendEvent::Aborted { lane: stream.lane }
        }
    }

    /// 放弃尚未开始端点 I/O 的一条消息。
    pub fn abandon(&mut self, lane: Lane, now_ms: u64) -> Result<SendEvent, SendError> {
        if let Some(stream) = self.stream
            && stream.lane == lane
            && stream.packet.is_some()
        {
            return Err(SendError::AwaitingCompletion);
        }
        if matches!(self.lanes[lane.index()], SendLane::Idle) {
            return Ok(SendEvent::Ignored);
        }
        self.abandon_lane(lane, now_ms);
        Ok(SendEvent::Aborted { lane })
    }

    /// 开始新的端点传输阶段，放弃所有冻结消息并保留编号与 sender guard。
    ///
    /// 调用方先结束或隔离端点的旧 I/O。本函数只使旧完成令牌无法推进新状态，
    /// 不表示已经发送的旧字节从物理 Bulk 流消失。
    pub fn phase_changed(&mut self, now_ms: u64) -> Result<Phase, PhaseError> {
        for lane in [Lane::Heartbeat, Lane::Short, Lane::Long] {
            self.abandon_lane(lane, now_ms);
        }
        self.stream = None;
        let Some(next) = self.phase.0.checked_add(1) else {
            self.phase_exhausted = true;
            return Err(PhaseError::Exhausted);
        };
        self.phase = Phase(next);
        Ok(self.phase)
    }

    fn start_block(&mut self, lane: Lane) -> Result<(), SendError> {
        if !matches!(self.lanes[lane.index()], SendLane::Queued { .. }) {
            return Ok(());
        }
        self.stream = Some(Stream {
            lane,
            cursor: CobsCursor::StartDelimiter,
            packet: None,
        });
        Ok(())
    }

    fn abandon_lane(&mut self, lane: Lane, now_ms: u64) {
        let index = lane.index();
        if let SendLane::Queued { transfer, .. } = self.lanes[index] {
            self.guards[index].protect(transfer, now_ms, lane.guard_duration_ms());
            self.lanes[index] = SendLane::Idle;
        }
        if self.stream.is_some_and(|stream| stream.lane == lane) {
            self.stream = None;
        }
    }
}
