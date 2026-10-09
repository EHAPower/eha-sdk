// Copyright The eha-sdk Contributors
//! EHA 客户端：公共会话核心与可选择的 CAN / USB 桌面接入。
//! 控制提交只报告本地 I/O；关闭和异常不发送停止、复位或重放目标。
#![cfg_attr(not(feature = "desktop"), no_std)]
#![forbid(unsafe_code)]

pub use {config, protocol, transport};
#[cfg(feature = "desktop")]
pub mod can;
#[cfg(feature = "desktop")]
pub mod configuration;
#[cfg(feature = "desktop")]
pub mod diagnostics;
#[cfg(feature = "desktop")]
pub mod host;
#[cfg(feature = "desktop")]
pub mod odrive;
pub mod session;
#[cfg(feature = "desktop")]
pub mod usb;
