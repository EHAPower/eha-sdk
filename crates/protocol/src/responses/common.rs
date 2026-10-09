// Copyright The eha-sdk Contributors

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

    /// 将内部 f64 观测投影到 binary32 字段，保留来源质量和过期事实。
    ///
    /// 仅 `Available` 缩窄数值；非有限值、溢出或非零值缩窄为零改报
    /// `CalculationFailed` 并使用正零占位，其他不可用状态保持原分类并使用正零。
    /// 有限可表示值按 binary32 舍入，包括次正规数和正负零；本方法不检查产品范围。
    /// 失败只描述本次对外表示，不改变内部原值、计算历史、来源或保护判断。
    pub fn from_f64(value: f64, state: ValueState) -> Self {
        if state.result() != ValueResult::Available {
            return Self::new(0.0, state);
        }
        let narrowed = value as f32;
        if !narrowed.is_finite() || (value != 0.0 && narrowed == 0.0) {
            Self::new(
                0.0,
                ValueState::new(
                    ValueResult::CalculationFailed,
                    state.quality(),
                    state.is_stale(),
                ),
            )
        } else {
            Self::new(narrowed, state)
        }
    }
}
