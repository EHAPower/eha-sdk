// Copyright The eha-sdk Contributors

use protocol::MessageInfo;

use crate::validated::ValidatedCompletedMessage;

pub(super) const MAX_EXTENDED_ID: u32 = 0x1fff_ffff;
pub(super) const HEARTBEAT_CAPACITY: usize = 8;
pub(super) const SHORT_CAPACITY: usize = 256;
pub(super) const LARGE_CAPACITY: usize = protocol::MAX_MESSAGE_LEN;
pub(super) const SHORT_TIMEOUT_MS: u64 = 20;
pub(super) const LARGE_TIMEOUT_MS: u64 = 30_000;
pub(super) const SHORT_GUARD_MS: u32 = 40;
pub(super) const LARGE_GUARD_MS: u32 = 60_000;

/// CAN 承载的启动帧形态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// 经典 CAN 数据帧，每片最多 8 字节。
    Classic,
    /// CAN FD 数据帧，要求 FDF 和 BRS 均置位，每片最多 64 字节。
    Fd,
}

/// 组装隔离通道。
///
/// 此枚举只隔离未完成字节，不表达控制、停止或维护优先级。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Lane {
    /// 只承载 8 字节显式心跳。
    Heartbeat = 0,
    /// 承载除心跳外且不超过 256 字节的完整消息。
    Short = 1,
    /// 承载超过 256 字节的完整消息。
    Large = 2,
}

impl Lane {
    pub(super) const fn from_bits(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Heartbeat),
            1 => Some(Self::Short),
            2 => Some(Self::Large),
            _ => None,
        }
    }

    pub(super) const fn capacity(self) -> usize {
        match self {
            Self::Heartbeat => HEARTBEAT_CAPACITY,
            Self::Short => SHORT_CAPACITY,
            Self::Large => LARGE_CAPACITY,
        }
    }

    pub(super) const fn timeout_ms(self) -> u64 {
        match self {
            Self::Large => LARGE_TIMEOUT_MS,
            Self::Heartbeat | Self::Short => SHORT_TIMEOUT_MS,
        }
    }

    pub(super) const fn guard_ms(self) -> u32 {
        match self {
            Self::Large => LARGE_GUARD_MS,
            Self::Heartbeat | Self::Short => SHORT_GUARD_MS,
        }
    }
}

/// 真实 CAN 帧元数据和驱动实际交付的数据。
///
/// `data_len` 独立于 `dlc` 保留：接收时必须同时与 DLC 映射相符，不能根据 DLC 推定驱动
/// 实际给出了多少字节。`data_len` 后的 `data` 内容不属于帧，绑定不会读取它们。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame {
    /// 原始 CAN 标识符数值；扩展帧只允许 29 位。
    pub id: u32,
    /// 驱动报告的 XTD 位。
    pub extended: bool,
    /// 驱动报告的 RTR 位。
    pub rtr: bool,
    /// 驱动报告的 FDF 位。
    pub fdf: bool,
    /// 驱动报告的 BRS 位。
    pub brs: bool,
    /// 驱动报告的 ESI 位；本绑定接受两种值。
    pub esi: bool,
    /// 线上 4 bit DLC 代码，不是字节长度。
    pub dlc: u8,
    /// 驱动实际提供的数据字节数。
    pub data_len: u8,
    /// 驱动帧数据的固定后备存储。
    pub data: [u8; 64],
}

impl Frame {
    /// 返回驱动实际交付的数据；畸形 `data_len` 会截到后备存储上限，随后由绑定拒绝。
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data[..usize::from(self.data_len).min(self.data.len())]
    }
}

/// 调用方提供的三条接收组装缓冲。
///
/// 构造 [`Receiver`](super::Receiver) 时分别至少需要 8、256、16440 字节。缓冲不会复制到绑定内部。
pub struct RxBuffers<'a> {
    /// 心跳通道缓冲。
    pub heartbeat: &'a mut [u8],
    /// 普通短消息通道缓冲。
    pub short: &'a mut [u8],
    /// 大消息通道缓冲。
    pub large: &'a mut [u8],
}

/// 接收构造或运行期操作错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// 节点号无法表示为 7 位。
    InvalidNode,
    /// 调用方缓冲不足以承载指定通道的最大完整消息。
    BufferTooSmall,
    /// 试图在仍有未完成、未取走消息或未归还校验证明缓冲的通道替换、重配。
    LaneBusy,
    /// 正在发送的通道又开始另一条完整消息。
    SendBusy,
    /// 下一个 7 位传输编号仍处于重用保护期。
    TransferProtected,
    /// 本地提交操作序号已经耗尽，不能再生成可区分旧完成的令牌。
    OperationExhausted,
    /// 新通信世代没有严格递增，可能与迟到 I/O 混淆。
    InvalidGeneration,
    /// 完成通知不属于当前世代、通道、编号或分片。
    StaleCompletion,
    /// 待发送字节未通过公共完整格式校验。
    InvalidMessage,
}

/// 一帧相关输入未能满足 CAN 绑定合同的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    /// 标识符不能表示为 29 位扩展 ID。
    InvalidIdentifier,
    /// 非数据帧或 Classic/FD 帧形态与启动配置不符。
    InvalidForm,
    /// DLC 非法，或驱动实际长度与 DLC 映射不一致。
    InvalidDlc,
    /// 首片不足四字节，或完整公共头非法。
    InvalidPrefix,
    /// 首片选择了不承载该公共消息的通道。
    WrongLane,
    /// 分片长度、零填充、编号或序号不符合合同。
    InvalidFragment,
    /// 同一编号仍在接收重用保护期内。
    TransferProtected,
    /// 已完成消息尚未被调用方取走或替换缓冲。
    CompletedNotTaken,
    /// 调用方交付的是旧通信世代的帧。
    StaleGeneration,
    /// 完整字节未通过公共 CRC 或字段校验。
    InvalidMessage,
}

/// 接收一帧后的分类结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiveResult {
    /// 节点、方向、保留通道或无活动组的非首片不属于本绑定。
    Ignored,
    /// 已接收相关片段，但尚未收齐完整公共消息。
    Incomplete {
        /// 正在组装的隔离通道。
        lane: Lane,
    },
    /// 已收齐并通过完整公共格式校验；字节由该通道持有。
    Complete {
        /// 完成消息所在通道。
        lane: Lane,
        /// 公共格式校验后的摘要。
        info: MessageInfo,
    },
    /// 相关帧非法，或旧世代帧被拒绝；不会交付半条消息。
    Rejected {
        /// 拒绝原因。
        reason: RejectReason,
    },
}

/// 接收一帧后的分类结果；完成变体转交已校验字节的唯一借用。
///
/// 使用它的调用方在处理完成后以 [`ValidatedCompletedMessage::into_buffer`] 取回原缓冲，
/// 再调用 [`Receiver::replace_buffer`](super::Receiver::replace_buffer) 归还该 lane。
pub enum ValidatedReceiveResult<'a> {
    /// 节点、方向、保留通道或无活动组的非首片不属于本绑定。
    Ignored,
    /// 已接收相关片段，但尚未收齐完整公共消息。
    Incomplete {
        /// 正在组装的隔离通道。
        lane: Lane,
    },
    /// 已收齐并校验；原组装缓冲随证明转交给调用方。
    Complete {
        /// 完成消息所在通道。
        lane: Lane,
        /// 与原始字节、方向和摘要绑定的校验证明。
        message: ValidatedCompletedMessage<'a>,
    },
    /// 相关帧非法，或旧世代帧被拒绝；不会交付半条消息。
    Rejected {
        /// 拒绝原因。
        reason: RejectReason,
    },
}

/// 从接收器取走的一条完整消息及其原组装缓冲。
///
/// 调用方可先通过 [`Self::as_bytes`] 交给应用层；处理完后用 [`Self::into_buffer`] 取回
/// 缓冲，并以 [`Receiver::replace_buffer`](super::Receiver::replace_buffer) 交还该通道或替换为另一个等容量缓冲。
pub struct CompletedMessage<'a> {
    pub(super) buffer: &'a mut [u8],
    pub(super) info: MessageInfo,
}

impl<'a> CompletedMessage<'a> {
    /// 通过公共格式校验的完整消息字节。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer[..self.info.length]
    }

    /// 公共消息摘要。
    #[must_use]
    pub const fn info(&self) -> MessageInfo {
        self.info
    }

    /// 取得原组装缓冲以便转交或复用。
    #[must_use]
    pub fn into_buffer(self) -> &'a mut [u8] {
        self.buffer
    }
}

impl Mode {
    pub(super) const fn quantum(self) -> usize {
        match self {
            Self::Classic => 8,
            Self::Fd => 64,
        }
    }
}
