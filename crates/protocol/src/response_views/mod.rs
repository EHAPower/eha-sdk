// Copyright The eha_controller Contributors

use crate::{Error, MessageKind};

mod common;
mod maintenance;
mod observations;

pub use common::{OperationKey, OperationKeyFields, Sample, SampleData};
pub use maintenance::{
    ConfigData, DataUnavailable, DiagnosticEntries, DiagnosticEntry, Diagnostics, OperationResult,
};
pub use observations::{Identity, Measurements, Telemetry};

/// 已校验的固件回复；各视图只借用接收缓冲。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Response<'a> {
    /// 周期遥测。
    Telemetry(Telemetry<'a>),
    /// 身份和适用性。
    Identity(Identity<'a>),
    /// 查询得到的当前状态。
    Status(Telemetry<'a>),
    /// 详细测量。
    Measurements(Measurements<'a>),
    /// 有界诊断条目序列。
    Diagnostics(Diagnostics<'a>),
    /// 实际配置字节或配置状态。
    ConfigData(ConfigData<'a>),
    /// 维护进展或结果。
    OperationResult(OperationResult<'a>),
    /// 数据或会话不可取得。
    DataUnavailable(DataUnavailable<'a>),
}

impl<'a> Response<'a> {
    pub(super) fn new(kind: MessageKind, payload: &'a [u8]) -> Result<Self, Error> {
        Ok(match kind {
            MessageKind::Telemetry => Self::Telemetry(Telemetry { payload }),
            MessageKind::Identity => Self::Identity(Identity { payload }),
            MessageKind::Status => Self::Status(Telemetry { payload }),
            MessageKind::Measurements => Self::Measurements(Measurements { payload }),
            MessageKind::Diagnostics => Self::Diagnostics(Diagnostics { payload }),
            MessageKind::ConfigData => Self::ConfigData(ConfigData { payload }),
            MessageKind::OperationResult => Self::OperationResult(OperationResult { payload }),
            MessageKind::DataUnavailable => Self::DataUnavailable(DataUnavailable { payload }),
            _ => return Err(Error::InvalidField),
        })
    }

    /// 此回复的公共类型。
    pub const fn kind(self) -> MessageKind {
        match self {
            Self::Telemetry(_) => MessageKind::Telemetry,
            Self::Identity(_) => MessageKind::Identity,
            Self::Status(_) => MessageKind::Status,
            Self::Measurements(_) => MessageKind::Measurements,
            Self::Diagnostics(_) => MessageKind::Diagnostics,
            Self::ConfigData(_) => MessageKind::ConfigData,
            Self::OperationResult(_) => MessageKind::OperationResult,
            Self::DataUnavailable(_) => MessageKind::DataUnavailable,
        }
    }

    pub(super) fn payload(self) -> &'a [u8] {
        match self {
            Self::Telemetry(x) | Self::Status(x) => x.payload,
            Self::Identity(x) => x.payload,
            Self::Measurements(x) => x.payload,
            Self::Diagnostics(x) => x.payload,
            Self::ConfigData(x) => x.payload,
            Self::OperationResult(x) => x.payload,
            Self::DataUnavailable(x) => x.payload,
        }
    }
}
