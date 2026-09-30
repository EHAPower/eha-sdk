// Copyright The eha_controller Contributors

//! 外部 CAN 的帧表示、分片、组装和本地提交边界。
//!
//! 本模块只保存公共应用消息的不可变字节并调用 `protocol` 作前缀和完整格式校验。它不
//! 持有 CAN 外设，也不把本地提交当成对端接收或业务执行。`Receiver` 的三个缓冲由调用方
//! 供给；完成的通道可短暂通过 [`Receiver::message`] 借用，或用
//! [`Receiver::take_completed`] 取走，再用 [`Receiver::replace_buffer`] 放回可复用缓冲。

mod receive;
mod transmit;
mod types;
mod wire;

pub use crate::validated::ValidatedCompletedMessage;
pub use receive::{Receiver, RxReconnectState};
pub use transmit::{
    PendingFrame, SubmitEvent, SubmitResult, Transmitter, TxBuffers, TxReconnectState, TxToken,
};
pub use types::{
    CompletedMessage, Error, Frame, Lane, Mode, ReceiveResult, RejectReason, RxBuffers,
    ValidatedReceiveResult,
};
