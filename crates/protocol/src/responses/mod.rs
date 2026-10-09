// Copyright The eha-sdk Contributors

//! 固件回复的具名字段与最终发送缓冲编码。
//!
//! 本模块只表达公共协议已经定义的线上字段。它不判断控制、维护或配置业务是否
//! 允许；每个编码函数在写入具名字段后调用 crate 根部唯一的完整格式校验。

use crate::{Error, MAX_MESSAGE_LEN, MAX_PAYLOAD_LEN, MessageKind, SampleData, encode_in_place};

mod helpers;
use helpers::*;
mod common;
pub use common::*;
mod telemetry;
pub use telemetry::*;
mod identity;
pub use identity::*;
mod measurements;
pub use measurements::*;
mod diagnostics;
pub use diagnostics::*;
mod config;
pub use config::*;
mod operation;
pub use operation::*;
mod enums;
