// Copyright The eha_controller Contributors

//! 完整公共应用消息的 `no_std` 编码、解码和格式校验。
//!
//! 线上字节、方向和字段约束由《公共应用通信协议》唯一维护。本库不决定控制
//! 准入、维护许可、配置保存、结果槽或设备执行。`Message` 借用输入；特别是
//! `SaveConfig` 和 `ConfigData` 的记录不会复制到枚举或临时大数组中。

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod codec;
mod message;
mod protocol;
mod response_views;
mod validation;

/// 固件回复的具名字段和最终发送缓冲编码。
pub mod responses;

pub use codec::{
    ValidatedMessage, crc32c, decode, encode, encode_in_place, probe_prefix, validate,
};
pub use message::Message;
pub use protocol::{
    Direction, Error, MAX_MESSAGE_LEN, MAX_PAYLOAD_LEN, MessageInfo, MessageKind, Prefix,
};
pub use response_views::{
    ConfigData, DataUnavailable, DiagnosticEntries, DiagnosticEntry, Diagnostics, Identity,
    Measurements, OperationKey, OperationKeyFields, OperationResult, Response, Sample, SampleData,
    Telemetry,
};
