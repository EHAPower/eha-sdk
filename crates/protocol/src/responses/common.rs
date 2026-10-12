// Copyright The eha-sdk Contributors

use super::*;

/// 不可取得数据的主题。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum UnavailableSubject {
    /// 身份。
    Identity = 0,
    /// 当前状态。
    Status = 1,
    /// 测量。
    Measurements = 2,
    /// 诊断。
    Diagnostics = 3,
    /// 配置。
    Config = 4,
    /// 保存并重启请求。
    SaveAndRestart = 5,
    /// 应用复位请求。
    ResetApplication = 6,
    /// 更新入口请求。
    EnterUpdate = 7,
}

/// DataUnavailable 的完整具名字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DataUnavailableFields {
    /// 共同回复快照。
    pub sample: SampleData,
    /// 不可取得的主题。
    pub subject: UnavailableSubject,
    /// 合同原因码。
    pub reason: u16,
    /// 必要操作位图。
    pub next_actions: u16,
}

impl DataUnavailableFields {
    /// `DataUnavailable` 的完整线上长度。
    pub const fn encoded_len() -> usize {
        48
    }
}

/// 编码数据不可取得或本次变异请求被前置拒绝的事实。
pub fn encode_data_unavailable(
    fields: &DataUnavailableFields,
    output: &mut [u8],
) -> Result<usize, Error> {
    let p = payload(output, DataUnavailableFields::encoded_len() - 8)?;
    write_sample(p, fields.sample);
    p[32] = fields.subject as u8;
    put_u16(p, 33, fields.reason);
    put_u16(p, 35, fields.next_actions);
    p[37..40].fill(0);
    encode_in_place(
        MessageKind::DataUnavailable,
        DataUnavailableFields::encoded_len() - 8,
        output,
    )
}

/// 观测数值结果状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ValueResult {
    /// 从未取得数值。
    Never = 0,
    /// 数值可表示。
    Available = 1,
    /// 换算或计算失败。
    CalculationFailed = 2,
    /// 历史样本不足。
    HistoryInsufficient = 3,
    /// 本次没有计算。
    NotComputed = 4,
    /// 输出被禁止。
    OutputInhibited = 5,
    /// 模型或参考不适用。
    NotApplicable = 6,
}

/// 观测来源质量。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SourceQuality {
    /// 未知质量。
    Unknown = 0,
    /// 来源合格。
    Qualified = 1,
    /// 来源明确故障。
    Faulted = 2,
    /// 来源连续性中断。
    Discontinuous = 3,
}

/// `value_state` 的具名组成。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValueState {
    bits: u8,
}

impl ValueState {
    /// 由结果、来源质量和过期事实组成状态。
    pub const fn new(result: ValueResult, quality: SourceQuality, stale: bool) -> Self {
        Self {
            bits: result as u8 | ((quality as u8) << 3) | ((stale as u8) << 5),
        }
    }

    /// 数值结果的可用性分类。
    pub const fn result(self) -> ValueResult {
        match self.bits & 7 {
            0 => ValueResult::Never,
            1 => ValueResult::Available,
            2 => ValueResult::CalculationFailed,
            3 => ValueResult::HistoryInsufficient,
            4 => ValueResult::NotComputed,
            5 => ValueResult::OutputInhibited,
            6 => ValueResult::NotApplicable,
            _ => ValueResult::Never,
        }
    }

    /// 数值来源的质量分类。
    pub const fn quality(self) -> SourceQuality {
        match (self.bits >> 3) & 3 {
            0 => SourceQuality::Unknown,
            1 => SourceQuality::Qualified,
            2 => SourceQuality::Faulted,
            _ => SourceQuality::Discontinuous,
        }
    }

    /// 数值是否已经超过其适用时效。
    pub const fn is_stale(self) -> bool {
        self.bits & 0x20 != 0
    }

    pub(super) fn bits(self) -> u8 {
        self.bits
    }

    /// 从已校验的线上状态位构造状态；保留位或未分配结果返回 `None`。
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & 0xc0 == 0 && bits & 7 <= 6 {
            Some(Self { bits })
        } else {
            None
        }
    }
}

/// 一个数值及其公开可用性状态。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValueFields {
    /// binary32 数值；不可用状态下应为正零。
    pub value: f32,
    /// 数值状态。
    pub state: ValueState,
}

impl ValueFields {
    /// 构造一个数值字段。
    pub const fn new(value: f32, state: ValueState) -> Self {
        Self { value, state }
    }

    /// 将内部 f32 观测投影到 binary32 字段，保留来源质量和过期事实。
    ///
    /// 仅 `Available` 采用数值；非有限值改报
    /// `CalculationFailed` 并使用正零占位，其他不可用状态保持原分类并使用正零。
    /// 有限值保持原位模式，包括次正规数和正负零；本方法不检查产品范围。
    /// 失败只描述本次对外表示，不改变内部原值、计算历史、来源或保护判断。
    pub fn from_f32(value: f32, state: ValueState) -> Self {
        if state.result() != ValueResult::Available {
            return Self::new(0.0, state);
        }
        if !value.is_finite() {
            Self::new(
                0.0,
                ValueState::new(
                    ValueResult::CalculationFailed,
                    state.quality(),
                    state.is_stale(),
                ),
            )
        } else {
            Self::new(value, state)
        }
    }
}
