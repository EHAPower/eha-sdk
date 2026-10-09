// Copyright The eha-sdk Contributors

//! USB Bulk 的 COBS 定界、分片和传输阶段处理。
//!
//! 本模块只恢复和发送公共应用消息的完整字节。端点 I/O、任务和业务
//! 交接由调用方拥有；线上规则唯一见 `docs/通信协议/USB通信绑定.md`。

/// 平台当前 Bulk 端点的最大包长。
pub const PACKET_LEN: usize = 64;
/// 含四字节块头的最大未编码 USB 块长度。
pub const MAX_RAW_BLOCK_LEN: usize = 260;
/// COBS 编码后的最大长度，不包括前后的零分隔符。
pub const MAX_COBS_LEN: usize = 262;
/// 含两个零分隔符的最大 USB 块长度。
pub const MAX_BLOCK_LEN: usize = 264;

const SHORT_MESSAGE_LIMIT: usize = 256;
const BLOCK_TIMEOUT_MS: u64 = 20;
const LONG_TIMEOUT_MS: u64 = 30_000;
const SHORT_GUARD_MS: u32 = 40;
const LONG_GUARD_MS: u32 = 60_000;

mod cobs;
mod receive;
mod send;
mod types;

pub use crate::validated::ValidatedCompletedMessage;
pub use receive::{Receiver, ReceiverReconnectState};
pub use send::{PreparedPacket, SendError, SendEvent, Sender, SenderReconnectState, TxToken};
pub use types::{
    FeedResult, Lane, Phase, PhaseError, PollResult, ReceiveEvent, ReceivedMessage, RejectReason,
    ValidatedFeedResult, ValidatedReceiveEvent,
};

#[cfg(test)]
mod tests;
