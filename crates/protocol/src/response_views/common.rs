// Copyright The eha-sdk Contributors

use crate::validation::{u32_at, u64_at};
use crate::{Error, responses};

/// 变异请求的期望控制器和运行实例。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedIdentity<'a> {
    pub(crate) bytes: &'a [u8],
}

impl<'a> ExpectedIdentity<'a> {
    /// 从精确28字节线上表示构造期望身份。
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() == 28 {
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
    /// 原始完整线上表示。
    pub fn as_bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// 将借用的线上表示复制为可长期保存的具名期望身份。
    pub fn fields(self) -> ExpectedIdentityFields {
        let mut uid = [0; 12];
        uid.copy_from_slice(&self.bytes[..12]);
        let mut run_nonce = [0; 16];
        run_nonce.copy_from_slice(&self.bytes[12..28]);
        ExpectedIdentityFields::new(uid, run_nonce)
    }
}

/// 可长期保存的具名期望身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedIdentityFields {
    /// 固定12字节控制器 UID。
    pub uid: [u8; 12],
    /// 固定16字节运行实例。
    pub run_nonce: [u8; 16],
}

impl ExpectedIdentityFields {
    /// 按具名字段构造一个自有期望身份。
    #[must_use]
    pub const fn new(uid: [u8; 12], run_nonce: [u8; 16]) -> Self {
        Self { uid, run_nonce }
    }

    /// 从精确28字节线上表示恢复一个自有期望身份。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let identity = ExpectedIdentity::new(bytes)?;
        let mut uid = [0; 12];
        uid.copy_from_slice(identity.uid());
        let mut run_nonce = [0; 16];
        run_nonce.copy_from_slice(identity.run_nonce());
        Ok(Self::new(uid, run_nonce))
    }

    /// 编码为公共协议使用的连续28字节表示。
    #[must_use]
    pub fn to_bytes(self) -> [u8; 28] {
        let mut bytes = [0; 28];
        bytes[..12].copy_from_slice(&self.uid);
        bytes[12..28].copy_from_slice(&self.run_nonce);
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
