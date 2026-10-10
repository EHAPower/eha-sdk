// Copyright The eha-sdk Contributors

//! 控制计算所需的启动参数。

use serde::{Deserialize, Serialize};

/// 等面积液压速度换算失败的原因。
///
/// 只检查 binary32 运算域；静态正值前提、安装方向、保护范围和输出授权由调用方判断。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HydraulicConversionError {
    /// 输入、几何参数或中间换算结果为 NaN 或无穷。
    NonFinite,
    /// 非零换算在某一步舍入为零，不能作为有效派生参数。
    Underflow,
}

/// 将产品线速度换算为不含安装方向的等效泵流量转速，单位 rpm。
///
/// 固定依次计算速度乘面积、乘 `0.06`、除排量，每步使用 binary32 最近取偶。
/// 本函数只接受该运算顺序内每步有限且非零派生不归零的参数域，不借助更宽精度
/// 挽救中间溢出。它不检查几何参数是否为正数。有限参数且排量非零时，零速度返回
/// `Ok(0.0)`；其他非零输入派生归零返回 [`HydraulicConversionError::Underflow`]。
pub fn hydraulic_velocity_to_rpm(
    velocity_mm_s: f32,
    effective_area_mm2: f32,
    pump_displacement_cm3_rev: f32,
) -> Result<f32, HydraulicConversionError> {
    let finite = |value: f32| {
        value
            .is_finite()
            .then_some(value)
            .ok_or(HydraulicConversionError::NonFinite)
    };
    finite(velocity_mm_s)?;
    finite(effective_area_mm2)?;
    finite(pump_displacement_cm3_rev)?;
    if pump_displacement_cm3_rev == 0.0 {
        return Err(HydraulicConversionError::NonFinite);
    }
    if velocity_mm_s == 0.0 {
        return Ok(0.0);
    }
    let nonzero = |value: f32| {
        let value = finite(value)?;
        if velocity_mm_s != 0.0 && value == 0.0 {
            Err(HydraulicConversionError::Underflow)
        } else {
            Ok(value)
        }
    };
    let flow_mm3_s = nonzero(velocity_mm_s * effective_area_mm2)?;
    let flow_cm3_min = nonzero(flow_mm3_s * 0.06)?;
    nonzero(flow_cm3_min / pump_displacement_cm3_rev)
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
    use crate::within_tolerance;

    #[test]
    fn hydraulic_velocity_conversion_preserves_sign_and_rejects_nonfinite_results() {
        // 三步 binary32 舍入的绝对误差预算为 5e-6 rpm。
        let absolute_rpm = 5e-6;
        assert!(within_tolerance(
            hydraulic_velocity_to_rpm(5.0, 100.0, 0.6).expect("合法测试输入"),
            50.0,
            absolute_rpm,
            0.0
        ));
        assert!(within_tolerance(
            hydraulic_velocity_to_rpm(-5.0, 100.0, 0.6).expect("合法测试输入"),
            -50.0,
            absolute_rpm,
            0.0
        ));

        assert_eq!(hydraulic_velocity_to_rpm(0.0, 100.0, 0.6), Ok(0.0));
        assert_eq!(
            hydraulic_velocity_to_rpm(-0.0, 100.0, 0.6)
                .expect("合法零速度")
                .to_bits(),
            0
        );
        for (speed, area, displacement) in [
            (f32::MAX, f32::MAX, f32::MIN_POSITIVE),
            (f32::NAN, 100.0, 0.6),
            (0.0, 100.0, 0.0),
        ] {
            assert_eq!(
                hydraulic_velocity_to_rpm(speed, area, displacement),
                Err(HydraulicConversionError::NonFinite)
            );
        }
        for (speed, area, displacement) in [
            (f32::from_bits(1), 0.5, 1.0),
            (f32::from_bits(1), 1.0, 1.0),
            (1e-30, 1e-10, 1e30),
        ] {
            assert_eq!(
                hydraulic_velocity_to_rpm(speed, area, displacement),
                Err(HydraulicConversionError::Underflow)
            );
        }
    }
}
