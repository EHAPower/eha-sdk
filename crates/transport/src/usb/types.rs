// Copyright The eha-sdk Contributors

use super::{LONG_GUARD_MS, SHORT_GUARD_MS};

use crate::validated::ValidatedCompletedMessage;

/// USB 绑定的三个固定组装 lane。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Lane {
    /// 仅承载显式主机心跳的短消息。
    Heartbeat = 0,
    /// 承载非心跳、长度不超过 256 字节的消息。
    Short = 1,
    /// 承载长度超过 256 字节的消息。
    Long = 2,
}

impl Lane {
    pub(super) const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Heartbeat),
            1 => Some(Self::Short),
            2 => Some(Self::Long),
            _ => None,
        }
    }

    pub(super) const fn index(self) -> usize {
        self as usize
    }

    pub(super) const fn guard_duration_ms(self) -> u32 {
        match self {
            Self::Long => LONG_GUARD_MS,
            Self::Heartbeat | Self::Short => SHORT_GUARD_MS,
        }
    }
}

/// 端点阶段的本地标识。
///
/// 调用方在端点禁用、重配、总线复位、断连或恢复后调用
/// [`Receiver::phase_changed`](super::Receiver::phase_changed) 和
/// [`Sender::phase_changed`](super::Sender::phase_changed) 取得新值。旧阶段
/// 的输入或 I/O 完成通知不能推进新阶段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Phase(pub(super) u64);

/// 端点阶段序号无法继续增长。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseError {
    /// 旧阶段标识绝不复用；调用方须重建绑定对象。
    Exhausted,
    /// 已校验消息仍持有某条 lane 的原组装缓冲；调用方须先归还缓冲。
    BufferBusy,
}

/// 一次 [`Receiver::feed`](super::Receiver::feed) 的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedResult {
    /// 已从传入切片处理的字节数。消息完成时，剩余字节保留给下一次调用。
    pub consumed: usize,
    /// 本次输入的明确传输层结果。
    pub event: ReceiveEvent,
}

/// 接收器对本次输入作出的传输层判断。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiveEvent {
    /// 仍在等待当前 COBS 块或 lane-2 消息的后续字节。
    NeedMore,
    /// 输入不属于一个活动传输，例如空 COBS 块、受保护旧编号或孤立后续片。
    Ignored,
    /// 块或片段与绑定规则不符，未形成公共消息。
    Rejected(RejectReason),
    /// 一条完整公共消息已通过格式校验。
    Complete(ReceivedMessage),
    /// 无新字节时，或处理字节前，发现一个绝对期限已经到达。
    TimedOut(PollResult),
    /// 字节来自旧端点阶段，未被消费。
    StalePhase,
    /// 阶段序号耗尽后，接收器拒绝所有新字节直到重建。
    PhaseExhausted,
}

/// 被拒绝的传输层原因。它不表示业务拒绝或设备执行结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    /// COBS 编码不合法。
    Cobs,
    /// raw 块缺少有效块头或片段。
    RawBlock,
    /// lane 或首片与公共消息类别、长度不匹配。
    Lane,
    /// 分片索引、长度或顺序不匹配。
    Fragment,
    /// 公共前缀不足或不合法。
    Prefix,
    /// 完整公共消息未通过格式或 CRC32C 校验。
    Message,
    /// 调用方为相应 lane 提供的缓冲不足。
    BufferTooSmall,
    /// 结束零前 COBS 块超过最大长度。
    BlockTooLong,
}

/// [`Receiver::poll`](super::Receiver::poll) 的绝对期限清理结果。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PollResult {
    /// 一个未闭合 COBS 块超过20 ms。
    pub block_timed_out: bool,
    /// 各 lane 是否有未完成消息达到其20 ms或30 s期限。
    pub lane_timed_out: [bool; 3],
}

/// 已完整组装并通过公共协议校验的消息位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceivedMessage {
    /// 保存该消息的 lane。
    pub lane: Lane,
    /// 公共消息的精确长度。
    pub length: usize,
}

/// 一次 [`Receiver::feed_validated`](super::Receiver::feed_validated) 的结果。
pub struct ValidatedFeedResult<'a> {
    /// 已从传入切片处理的字节数。消息完成时，剩余字节保留给下一次调用。
    pub consumed: usize,
    /// 本次输入的明确传输层结果。
    pub event: ValidatedReceiveEvent<'a>,
}

/// 接收器对本次输入作出的传输层判断；完成时转交原 lane 缓冲。
pub enum ValidatedReceiveEvent<'a> {
    /// 仍在等待当前 COBS 块或 lane-2 消息的后续字节。
    NeedMore,
    /// 输入不属于一个活动传输，例如空 COBS 块、受保护旧编号或孤立后续片。
    Ignored,
    /// 块或片段与绑定规则不符，未形成公共消息。
    Rejected(RejectReason),
    /// 一条完整公共消息已通过格式校验，原缓冲随证明转交。
    Complete {
        /// 保存原始消息的 lane。
        lane: Lane,
        /// 与原始字节、方向和摘要绑定的校验证明。
        message: ValidatedCompletedMessage<'a>,
    },
    /// 无新字节时，或处理字节前，发现一个绝对期限已经到达。
    TimedOut(PollResult),
    /// 字节来自旧端点阶段，未被消费。
    StalePhase,
    /// 阶段序号耗尽后，接收器拒绝所有新字节直到重建。
    PhaseExhausted,
}
