// Copyright The eha_controller Contributors

//! 控制计算所需的启动参数。

use serde::{Deserialize, Serialize};

/// 等面积液压速度换算无法产生有限 rpm 的原因。
///
/// 此错误只表达输入、几何参数或中间换算超出 `f64` 有限表示；不判断参数的静态正值
/// 前提、安装方向、保护范围或输出授权。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HydraulicConversionError {
    /// 输入、几何参数或中间换算结果为 NaN 或无穷。
    NonFinite,
}

/// 将产品线速度换算为不含安装方向的等效泵流量转速，单位 rpm。
///
/// 速度符号直接保留。调用方提供两腔共同的有效受压面积（mm²）和泵每转排量
/// （cm³/rev）；本函数不检查它们是否为正数，也不读取运行配置或设备状态。输入、参数或
/// 中间结果为 NaN 或无穷时返回 [`HydraulicConversionError::NonFinite`]；合法零输入返回
/// `Ok(0.0)`。
pub fn hydraulic_velocity_to_rpm(
    velocity_mm_s: f64,
    effective_area_mm2: f64,
    pump_displacement_cm3_rev: f64,
) -> Result<f64, HydraulicConversionError> {
    let finite = |value: f64| {
        value
            .is_finite()
            .then_some(value)
            .ok_or(HydraulicConversionError::NonFinite)
    };
    finite(velocity_mm_s)?;
    finite(effective_area_mm2)?;
    finite(pump_displacement_cm3_rev)?;
    let flow_mm3_s = finite(velocity_mm_s * effective_area_mm2)?;
    let flow_cm3_min = finite(flow_mm3_s * 0.06)?;
    finite(flow_cm3_min / pump_displacement_cm3_rev)
}

/// Position、Velocity、Force 与 Impedance 共用的固定控制参数。
///
/// 本类型保存增益、液压模型和安装方向；速度与力软范围由
/// [`crate::ProtectionConfig`] 提供。静态业务约束由主机 schema 校验；控制计算和
/// 运行决策按各自 API 处理实际输入、数值结果与最终输出授权。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    /// Position PID 的固定增益。
    pub position: PositionPidConfig,
    /// Velocity PID 的固定增益。
    pub velocity: VelocityPidConfig,
    /// Force 与 Impedance 共用的压差 PID 固定增益。
    pub pressure: PressurePidConfig,
    /// 等面积液压模型的几何与泵排量。
    pub hydraulics: HydraulicConfig,
    /// 产品位置正方向与电机轴转速符号的关系。
    pub motor_direction: MotorDirection,
    /// 哪个电气压力通道升压产生产品正向力。
    pub positive_force_pressure_channel: PositiveForcePressureChannel,
}

impl ControlConfig {
    /// 检查 JSON 数字能否用有限浮点表示，不检查静态业务范围或字段关系。
    pub(crate) fn numbers_are_finite(&self) -> bool {
        self.position.numbers_are_finite()
            && self.velocity.numbers_are_finite()
            && self.pressure.numbers_are_finite()
            && self.hydraulics.numbers_are_finite()
    }
}

/// Position PID 的固定增益。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionPidConfig {
    /// 位置比例增益，单位 rpm/mm。
    pub kp_rpm_per_mm: f32,
    /// 位置积分增益，单位 rpm/(mm·s)。
    pub ki_rpm_per_mm_s: f32,
    /// 测量速度阻尼增益，单位 rpm·s/mm。
    pub kd_rpm_s_per_mm: f32,
}

impl PositionPidConfig {
    fn numbers_are_finite(&self) -> bool {
        self.kp_rpm_per_mm.is_finite()
            && self.ki_rpm_per_mm_s.is_finite()
            && self.kd_rpm_s_per_mm.is_finite()
    }
}

/// Velocity PID 的固定增益。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VelocityPidConfig {
    /// 速度比例增益，单位 rpm/(mm/s)。
    pub kp_rpm_per_mm_s: f32,
    /// 速度积分增益，单位 rpm/mm。
    pub ki_rpm_per_mm: f32,
    /// 速度误差微分增益，单位 rpm·s²/mm。
    pub kd_rpm_s2_per_mm: f32,
}

impl VelocityPidConfig {
    fn numbers_are_finite(&self) -> bool {
        self.kp_rpm_per_mm_s.is_finite()
            && self.ki_rpm_per_mm.is_finite()
            && self.kd_rpm_s2_per_mm.is_finite()
    }
}

/// Force 与 Impedance 压差 PID 的固定增益。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PressurePidConfig {
    /// 压差比例增益，单位 rpm/MPa。
    pub kp_rpm_per_mpa: f32,
    /// 压差积分增益，单位 rpm/(MPa·s)。
    pub ki_rpm_per_mpa_s: f32,
    /// 压差误差微分增益，单位 rpm·s/MPa。
    pub kd_rpm_s_per_mpa: f32,
}

impl PressurePidConfig {
    fn numbers_are_finite(&self) -> bool {
        self.kp_rpm_per_mpa.is_finite()
            && self.ki_rpm_per_mpa_s.is_finite()
            && self.kd_rpm_s_per_mpa.is_finite()
    }
}

/// 等面积液压模型的固定参数。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HydraulicConfig {
    /// 两腔共同的有效受压面积，单位 mm²。
    pub effective_area_mm2: f32,
    /// 泵每转排量，单位 cm³/rev。
    pub pump_displacement_cm3_rev: f32,
}

impl HydraulicConfig {
    fn numbers_are_finite(&self) -> bool {
        self.effective_area_mm2.is_finite() && self.pump_displacement_cm3_rev.is_finite()
    }
}

/// 产品位置正方向与电机轴转速符号的关系。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotorDirection {
    /// 电机轴正转速使产品位置增大。
    Positive,
    /// 电机轴负转速使产品位置增大。
    Negative,
}

/// 哪个电气压力通道升压产生产品正向力。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositiveForcePressureChannel {
    /// A 通道升压产生产品正力。
    A,
    /// B 通道升压产生产品正力。
    B,
}

#[cfg(test)]
mod tests {
    use super::{HydraulicConversionError, hydraulic_velocity_to_rpm};

    #[test]
    fn hydraulic_velocity_conversion_preserves_sign_and_rejects_nonfinite_results() {
        assert_eq!(hydraulic_velocity_to_rpm(5.0, 100.0, 0.6), Ok(50.0));
        assert_eq!(hydraulic_velocity_to_rpm(-5.0, 100.0, 0.6), Ok(-50.0));

        let large = hydraulic_velocity_to_rpm(
            f64::from(f32::MAX),
            f64::from(f32::MAX),
            f64::from(f32::MIN_POSITIVE),
        )
        .expect("有限 f64 换算结果");
        assert!(large > f64::from(f32::MAX));
        assert_eq!(
            hydraulic_velocity_to_rpm(f64::MAX, 100.0, 0.6),
            Err(HydraulicConversionError::NonFinite)
        );
    }
}
