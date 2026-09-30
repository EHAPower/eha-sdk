// Copyright The eha_controller Contributors

use crate::validation::{u32_at, u64_at};
use crate::{Error, responses};

/// 维护会话键的借用表示。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationKey<'a> {
    pub(crate) bytes: &'a [u8],
}

impl<'a> OperationKey<'a> {
    /// 从精确36字节线上表示构造键。
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() == 36 {
            Ok(Self { bytes })
        } else {
            Err(Error::InvalidField)
        }
    }
    /// 固定12字节控制器 UID。
    pub fn uid(self) -> &'a [u8] {
        &self.bytes[..12]
    }
    /// 固定16字节运行实例。
    pub fn run_nonce(self) -> &'a [u8] {
        &self.bytes[12..28]
    }
    /// 非零维护操作标识。
    pub fn operation_id(self) -> u64 {
        u64_at(self.bytes, 28)
    }
    /// 原始完整线上表示。
    pub fn as_bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// 将借用的线上表示复制为可长期保存的具名维护键。
    pub fn fields(self) -> OperationKeyFields {
        let mut uid = [0; 12];
        uid.copy_from_slice(&self.bytes[..12]);
        let mut run_nonce = [0; 16];
        run_nonce.copy_from_slice(&self.bytes[12..28]);
        OperationKeyFields::new(uid, run_nonce, self.operation_id())
    }
}

/// 可长期保存的维护会话键。
///
/// 该类型只拥有公共协议既有的 UID、运行实例和操作标识。需要发送时调用
/// [`OperationKeyFields::to_bytes`]，再以 [`OperationKey::new`] 借用该连续表示；线上
/// 字段顺序仍只在本模块维护。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationKeyFields {
    /// 固定12字节控制器 UID。
    pub uid: [u8; 12],
    /// 固定16字节运行实例。
    pub run_nonce: [u8; 16],
    /// 维护操作标识。
    pub operation_id: u64,
}

impl OperationKeyFields {
    /// 按具名字段构造一个自有维护键。
    #[must_use]
    pub const fn new(uid: [u8; 12], run_nonce: [u8; 16], operation_id: u64) -> Self {
        Self {
            uid,
            run_nonce,
            operation_id,
        }
    }

    /// 从精确36字节线上表示恢复一个自有维护键。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let key = OperationKey::new(bytes)?;
        let mut uid = [0; 12];
        uid.copy_from_slice(key.uid());
        let mut run_nonce = [0; 16];
        run_nonce.copy_from_slice(key.run_nonce());
        Ok(Self::new(uid, run_nonce, key.operation_id()))
    }

    /// 编码为公共协议使用的连续36字节表示。
    #[must_use]
    pub fn to_bytes(self) -> [u8; 36] {
        let mut bytes = [0; 36];
        bytes[..12].copy_from_slice(&self.uid);
        bytes[12..28].copy_from_slice(&self.run_nonce);
        bytes[28..].copy_from_slice(&self.operation_id.to_le_bytes());
        bytes
    }
}

/// 所有固件回复的共同快照字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sample<'a> {
    pub(super) bytes: &'a [u8],
}
impl<'a> Sample<'a> {
    /// 请求对应的查询标识，周期消息为零。
    pub fn query_id(self) -> u32 {
        u32_at(self.bytes, 0)
    }
    /// 本次应用运行实例。
    pub fn run_nonce(self) -> &'a [u8] {
        &self.bytes[4..20]
    }
    /// 发布副本序号。
    pub fn snapshot_sequence(self) -> u32 {
        u32_at(self.bytes, 20)
    }
    /// 应用启动后的单调微秒时间。
    pub fn snapshot_time_us(self) -> u64 {
        u64_at(self.bytes, 24)
    }
}

/// 形成固件回复时写入共同快照的自有小型字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SampleData {
    /// 对应查询标识；周期消息为零。
    pub query_id: u32,
    /// 当前应用运行实例。
    pub run_nonce: [u8; 16],
    /// 发布副本序号。
    pub snapshot_sequence: u32,
    /// 应用启动后的单调微秒时间，不能为全1。
    pub snapshot_time_us: u64,
}

pub(super) fn decoded_value_fields(value: f32, state: u8) -> responses::ValueFields {
    responses::ValueFields::new(
        value,
        responses::ValueState::from_bits(state).expect("payload was validated"),
    )
}

pub(super) fn sample_data(payload: &[u8]) -> SampleData {
    let mut run_nonce = [0; 16];
    run_nonce.copy_from_slice(&payload[4..20]);
    SampleData {
        query_id: u32_at(payload, 0),
        run_nonce,
        snapshot_sequence: u32_at(payload, 20),
        snapshot_time_us: u64_at(payload, 24),
    }
}
