// Copyright The eha_controller Contributors

//! 测量换算与处理所需的启动参数。

use serde::{Deserialize, Serialize};

/// 位移、速度与压力处理共用的启动参数。
///
/// 本类型仅保存完整 JSON 中的参数，不判断标定端点、时间常数或方向是否适用于
/// 当前设备。测量处理库消费这些值并按其 API 的数值前提表达结果。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementConfig {
    /// BRT27 原始计数到产品位置及速度处理参数。
    pub position: PositionConfig,
    /// A/B 压力原始采样的标定与滤波参数。
    pub pressure: PressureConfig,
}

impl MeasurementConfig {
    /// 检查 JSON 数字能否用有限浮点表示，不检查标定与时间参数的业务范围。
    pub(crate) fn numbers_are_finite(&self) -> bool {
        self.position.mm_per_count.is_finite()
            && self.pressure.a.min_mpa.is_finite()
            && self.pressure.a.max_mpa.is_finite()
            && self.pressure.b.min_mpa.is_finite()
            && self.pressure.b.max_mpa.is_finite()
    }
}

/// BRT27 原始计数到产品位置以及速度处理的参数。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionConfig {
    /// 产品位置零点对应的 BRT27 原始计数，单位 count。
    pub reference_count: u32,
    /// 单个原始计数对应的位移，单位 mm/count。
    pub mm_per_count: f32,
    /// 原始计数变化与产品位置正方向的关系。
    pub direction: PositionDirection,
    /// 速度最小二乘窗口的最大时间跨度，单位毫秒。
    pub velocity_estimator_window_ms: u32,
    /// 速度一阶滤波的时间常数，单位毫秒。
    pub velocity_filter_time_constant_ms: u32,
    /// 相邻位置样本允许连续的最大取得时间间隔，单位毫秒。
    pub max_gap_ms: u32,
}

/// 原始 BRT27 计数与产品位置正方向的关系。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionDirection {
    /// 原始计数增大时，产品位置增大。
    Increasing,
    /// 原始计数减小时，产品位置增大。
    Decreasing,
}

/// 两路压力处理的参数。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PressureConfig {
    /// A 通道的原始计数和压力端点。
    pub a: PressureCalibration,
    /// B 通道的原始计数和压力端点。
    pub b: PressureCalibration,
    /// 每个压力通道一阶滤波的时间常数，单位毫秒。
    pub filter_time_constant_ms: u32,
    /// 相邻完整 A/B 样本对允许连续的最大取得时间间隔，单位毫秒。
    pub max_gap_ms: u32,
}

/// 一个压力通道的原始 ADC 计数和 MPa 标定端点。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PressureCalibration {
    /// 标定低端的原始 ADC 计数。
    pub raw_min: u16,
    /// 标定高端的原始 ADC 计数。
    pub raw_max: u16,
    /// `raw_min` 对应压力，单位 MPa。
    pub min_mpa: f32,
    /// `raw_max` 对应压力，单位 MPa。
    pub max_mpa: f32,
}
