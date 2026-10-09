// Copyright The eha-sdk Contributors

use crate::codec::payload_len;
use crate::validation::payload_len_is_valid;
use crate::{Error, MessageKind, OperationKey, Response};

/// 已解码的应用消息。固件回复通过具名的借用视图访问；接收缓冲在视图存活期间不可复用。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Message<'a> {
    /// 位置目标，单位 mm。
    Position(f32),
    /// 速度目标，单位 mm/s。
    Velocity(f32),
    /// 力目标，单位 N。
    Force(f32),
    /// 阻抗的平衡位置、刚度和阻尼。
    Impedance {
        /// 平衡位置，单位 mm。
        equilibrium_mm: f32,
        /// 刚度，单位 N/mm。
        stiffness_n_per_mm: f32,
        /// 阻尼，单位 N s/mm。
        damping_ns_per_mm: f32,
    },
    /// 无参数停止。
    Stop,
    /// 无参数显式心跳。
    Heartbeat,
    /// 查询标识及类别。
    Query {
        /// 非零查询标识。
        query_id: u32,
        /// 合同定义的查询类别。
        category: u8,
    },
    /// 配置视图读取。
    ReadConfig {
        /// 非零查询标识。
        query_id: u32,
        /// 合同定义的配置视图。
        view: u8,
    },
    /// 维护结果读取。
    ReadResult {
        /// 非零查询标识。
        query_id: u32,
        /// 被查询的维护键。
        key: OperationKey<'a>,
    },
    /// 借用的完整候选配置原字节。
    SaveConfig {
        /// 此请求的维护键。
        key: OperationKey<'a>,
        /// 借用的完整候选记录原字节。
        record: &'a [u8],
    },
    /// 无候选字节的维护操作。
    Maintenance {
        /// `RestoreFactory`、`ResetApplication` 或 `EnterUpdate`。
        kind: MessageKind,
        /// 此请求的维护键。
        key: OperationKey<'a>,
    },
    /// 已结清结果的释放请求。
    ReleaseResult {
        /// 此请求的维护键。
        key: OperationKey<'a>,
        /// 精确匹配的留存结果版本。
        revision: u32,
    },
    /// 具有字段访问器的固件回复。
    Response(Response<'a>),
}

impl<'a> Message<'a> {
    /// 此消息的线上类型。
    pub const fn kind(&self) -> MessageKind {
        match self {
            Self::Position(_) => MessageKind::Position,
            Self::Velocity(_) => MessageKind::Velocity,
            Self::Force(_) => MessageKind::Force,
            Self::Impedance { .. } => MessageKind::Impedance,
            Self::Stop => MessageKind::Stop,
            Self::Heartbeat => MessageKind::Heartbeat,
            Self::Query { .. } => MessageKind::Query,
            Self::ReadConfig { .. } => MessageKind::ReadConfig,
            Self::ReadResult { .. } => MessageKind::ReadResult,
            Self::SaveConfig { .. } => MessageKind::SaveConfig,
            Self::Maintenance { kind, .. } => *kind,
            Self::ReleaseResult { .. } => MessageKind::ReleaseResult,
            Self::Response(response) => response.kind(),
        }
    }

    /// 计算此消息的精确完整线上长度，不写入缓冲。
    pub fn encoded_len(&self) -> Result<usize, Error> {
        let payload_len = payload_len(self);
        if payload_len_is_valid(self.kind(), payload_len) {
            Ok(payload_len + 8)
        } else {
            Err(Error::InvalidPayloadLength)
        }
    }
}
