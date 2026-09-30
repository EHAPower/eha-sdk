// Copyright The eha_controller Contributors

//! 运行保护范围和时效的完整启动参数。

use serde::{Deserialize, Serialize};

/// 运行决策使用的软硬保护范围。
///
/// 位置独立保存上下限；速度、各腔压力、力和电机转速用正幅值表示零点对称范围。
/// 本类型不判断边界关系、物理适用性或命中后的控制后果；主机静态检查和运行决策
/// 分别拥有这些职责。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectionConfig {
    /// 位置软下限，单位 mm。
    pub soft_position_min_mm: f32,
    /// 位置软上限，单位 mm。
    pub soft_position_max_mm: f32,
    /// 位置硬下限，单位 mm。
    pub hard_position_min_mm: f32,
    /// 位置硬上限，单位 mm。
    pub hard_position_max_mm: f32,
    /// 活塞杆线速度软幅值上限，单位 mm/s。
    pub soft_velocity_max_mm_s: f32,
    /// 活塞杆线速度硬幅值上限，单位 mm/s。
    pub hard_velocity_max_mm_s: f32,
    /// A/B 两腔共用的压力软幅值上限，单位 MPa；各通道分别判断。
    pub soft_pressure_max_mpa: f32,
    /// A/B 两腔共用的压力硬幅值上限，单位 MPa；各通道分别判断。
    pub hard_pressure_max_mpa: f32,
    /// 估算力软幅值上限，单位 N。
    pub soft_force_max_n: f32,
    /// 估算力硬幅值上限，单位 N。
    pub hard_force_max_n: f32,
    /// 电机轴转速软幅值上限，单位 rpm。
    pub soft_motor_speed_max_rpm: f32,
    /// 电机轴转速硬幅值上限，单位 rpm。
    pub hard_motor_speed_max_rpm: f32,
}

impl ProtectionConfig {
    /// 检查边界能否用有限浮点表示，不检查范围关系或正负语义。
    pub(crate) fn numbers_are_finite(&self) -> bool {
        [
            self.soft_position_min_mm,
            self.soft_position_max_mm,
            self.hard_position_min_mm,
            self.hard_position_max_mm,
            self.soft_velocity_max_mm_s,
            self.hard_velocity_max_mm_s,
            self.soft_pressure_max_mpa,
            self.hard_pressure_max_mpa,
            self.soft_force_max_n,
            self.hard_force_max_n,
            self.soft_motor_speed_max_rpm,
            self.hard_motor_speed_max_rpm,
        ]
        .iter()
        .all(|value| value.is_finite())
    }
}

/// 与运行决策有关的通信、测量和设备时效。
///
/// 本类型只提供频率和时间数值，接收时刻与运行状态由消费者维护。位置和压力的
/// 允许年龄独立于测量处理的 `max_gap_ms`。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    /// 主机在每个入口发送心跳的频率，单位 Hz。
    pub host_heartbeat_hz: u32,
    /// 对外遥测发布频率，单位 Hz。
    pub telemetry_hz: u32,
    /// 每个入口最近有效主机心跳允许的最大年龄，单位 ms。
    pub host_contact_max_age_ms: u32,
    /// 位置及由其导出的速度允许的最大年龄，单位 ms。
    pub position_max_age_ms: u32,
    /// 同次完整 A/B 压力对允许的最大年龄，单位 ms。
    pub pressure_pair_max_age_ms: u32,
    /// 设备状态允许的最大年龄，单位 ms。
    pub driver_state_max_age_ms: u32,
    /// 一次设备操作等待匹配结果的最大期限，单位 ms。
    pub driver_operation_timeout_ms: u32,
}
