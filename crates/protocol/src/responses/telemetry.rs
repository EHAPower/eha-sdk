// Copyright The eha_controller Contributors

use super::*;

/// 当前目标模式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TargetMode {
    /// 没有当前目标。
    None = 0,
    /// 绝对位置目标。
    Position = 1,
    /// 线速度目标。
    Velocity = 2,
    /// 推拉力目标。
    Force = 3,
    /// 阻抗目标。
    Impedance = 4,
}

/// 固件实际采用的外部入口。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TargetIngress {
    /// 无关联入口。
    None = 0,
    /// 外部 CAN。
    Can = 1,
    /// USB Type-C。
    Usb = 2,
}

/// `Telemetry`/`Status` 的运行事实位。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TelemetryFacts {
    /// 普通输出当前获准。
    pub output_allowed: bool,
    /// Velocity/Impedance 参考已经限幅。
    pub reference_limited: bool,
    /// 候选 rpm 已受正常算法幅值限制。
    pub candidate_limited: bool,
    /// 位置方向输出被抑制。
    pub position_direction_inhibited: bool,
    /// 存在未结清设备操作。
    pub device_operation_pending: bool,
    /// 存在留存维护结果。
    pub retained_result: bool,
    /// 存在未明影响。
    pub unknown_effect: bool,
}

impl TelemetryFacts {
    fn bits(self) -> u16 {
        (self.output_allowed as u16)
            | ((self.reference_limited as u16) << 1)
            | ((self.candidate_limited as u16) << 2)
            | ((self.position_direction_inhibited as u16) << 3)
            | ((self.device_operation_pending as u16) << 4)
            | ((self.retained_result as u16) << 5)
            | ((self.unknown_effect as u16) << 6)
    }
}

/// 当前有效限制位。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LimitFlags {
    /// 位置软下侧。
    pub position_soft_lower: bool,
    /// 位置软上侧。
    pub position_soft_upper: bool,
    /// 位置硬下侧。
    pub position_hard_lower: bool,
    /// 位置硬上侧。
    pub position_hard_upper: bool,
    /// 速度软限制。
    pub velocity_soft: bool,
    /// 速度硬限制。
    pub velocity_hard: bool,
    /// 压力 A 软限制。
    pub pressure_a_soft: bool,
    /// 压力 A 硬限制。
    pub pressure_a_hard: bool,
    /// 压力 B 软限制。
    pub pressure_b_soft: bool,
    /// 压力 B 硬限制。
    pub pressure_b_hard: bool,
    /// 力软限制。
    pub force_soft: bool,
    /// 力硬限制。
    pub force_hard: bool,
    /// 电机输出软限制。
    pub motor_output_soft: bool,
    /// 电机输出硬限制。
    pub motor_output_hard: bool,
}

impl LimitFlags {
    fn bits(self) -> u16 {
        let values = [
            self.position_soft_lower,
            self.position_soft_upper,
            self.position_hard_lower,
            self.position_hard_upper,
            self.velocity_soft,
            self.velocity_hard,
            self.pressure_a_soft,
            self.pressure_a_hard,
            self.pressure_b_soft,
            self.pressure_b_hard,
            self.force_soft,
            self.force_hard,
            self.motor_output_soft,
            self.motor_output_hard,
        ];
        let mut bits = 0;
        let mut index = 0;
        while index < values.len() {
            bits |= (values[index] as u16) << index;
            index += 1;
        }
        bits
    }
}

/// 用于 reference 的物理量。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ReferenceKind {
    /// 没有参考。
    None = 0,
    /// 位置，单位 mm。
    PositionMm = 1,
    /// 速度，单位 mm/s。
    VelocityMmPerSecond = 2,
    /// 力，单位 N。
    ForceN = 3,
}

/// 驱动来源归类位。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DriverState {
    /// 曾取得完整驱动状态。
    pub has_status: bool,
    /// 来源合格。
    pub qualified: bool,
    /// 来源已过期。
    pub stale: bool,
    /// 来源明确故障。
    pub faulted: bool,
}

impl DriverState {
    fn bits(self) -> u8 {
        (self.has_status as u8)
            | ((self.qualified as u8) << 1)
            | ((self.stale as u8) << 2)
            | ((self.faulted as u8) << 3)
    }
}

/// 固件的期望轴状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DesiredAxis {
    /// 无可用期望。
    None = 0,
    /// Idle。
    Idle = 1,
    /// ClosedLoop。
    ClosedLoop = 2,
}

/// 外部入口联系状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ContactState {
    /// 从未收到有效心跳。
    Never = 0,
    /// 在允许年龄内。
    Active = 1,
    /// 已过期。
    Expired = 2,
}

/// `Telemetry` 或 `Status` 的完整具名字段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TelemetryFields {
    /// 共同回复快照；Telemetry 的 `query_id` 为零，Status 非零。
    pub sample: SampleData,
    /// 运行决策副本年龄。
    pub decision_age_us: u32,
    /// 当前目标模式。
    pub target_mode: TargetMode,
    /// 当前目标实际采用入口。
    pub target_ingress: TargetIngress,
    /// 当前运行事实。
    pub facts: TelemetryFacts,
    /// 三个模式相关目标值。
    pub target_values: [f32; 3],
    /// CAN 新目标采用阻止原因位图。
    pub can_adoption_blockers: u32,
    /// USB 新目标采用阻止原因位图。
    pub usb_adoption_blockers: u32,
    /// 普通输出阻止原因位图。
    pub output_blockers: u32,
    /// 最后目标结束原因，零表示尚无结束。
    pub last_end_reason: u16,
    /// 当前限制和保护事实。
    pub limits: LimitFlags,
    /// 位置、速度、滤波压力 A/B、主要力。
    pub main_values: [ValueFields; 5],
    /// 位置来源年龄。
    pub position_age_us: u32,
    /// 线速度结果的位置来源年龄。
    pub velocity_age_us: u32,
    /// 完整压力对来源年龄。
    pub pressure_pair_age_us: u32,
    /// 实际使用参考。
    pub reference: ValueFields,
    /// 参考物理量。
    pub reference_kind: ReferenceKind,
    /// 本周期候选 rpm。
    pub candidate_rpm: ValueFields,
    /// 最后成功本地提交的 rpm。
    pub last_submitted_rpm: ValueFields,
    /// 最后本地提交的年龄。
    pub submitted_age_us: u32,
    /// ODrive 原始轴状态。
    pub axis_state_raw: u32,
    /// ODrive 原始轴错误。
    pub axis_error_raw: u32,
    /// 完整驱动状态来源年龄。
    pub driver_age_us: u32,
    /// 驱动来源状态。
    pub driver_state: DriverState,
    /// 固件期望轴状态。
    pub desired_axis: DesiredAxis,
    /// CAN 最后有效心跳年龄。
    pub can_heartbeat_age_us: u32,
    /// USB 最后有效心跳年龄。
    pub usb_heartbeat_age_us: u32,
    /// CAN 联系状态。
    pub can_contact: ContactState,
    /// USB 联系状态。
    pub usb_contact: ContactState,
}

impl TelemetryFields {
    /// `Telemetry` 或 `Status` 的完整线上长度。
    pub const fn encoded_len() -> usize {
        160
    }
}

/// 编码周期遥测。
pub fn encode_telemetry(fields: &TelemetryFields, output: &mut [u8]) -> Result<usize, Error> {
    encode_telemetry_kind(MessageKind::Telemetry, fields, output)
}

/// 编码查询得到的当前状态。
pub fn encode_status(fields: &TelemetryFields, output: &mut [u8]) -> Result<usize, Error> {
    encode_telemetry_kind(MessageKind::Status, fields, output)
}

fn encode_telemetry_kind(
    kind: MessageKind,
    fields: &TelemetryFields,
    output: &mut [u8],
) -> Result<usize, Error> {
    let p = payload(output, TelemetryFields::encoded_len() - 8)?;
    write_sample(p, fields.sample);
    put_u32(p, 32, fields.decision_age_us);
    p[36] = fields.target_mode as u8;
    p[37] = fields.target_ingress as u8;
    put_u16(p, 38, fields.facts.bits());
    for (index, value) in fields.target_values.iter().enumerate() {
        put_f32(p, 40 + 4 * index, *value);
    }
    put_u32(p, 52, fields.can_adoption_blockers);
    put_u32(p, 56, fields.usb_adoption_blockers);
    put_u32(p, 60, fields.output_blockers);
    put_u16(p, 64, fields.last_end_reason);
    put_u16(p, 66, fields.limits.bits());
    for (index, value) in fields.main_values.iter().enumerate() {
        put_value(p, 68 + 4 * index, 88 + index, *value);
    }
    p[93..96].fill(0);
    put_u32(p, 96, fields.position_age_us);
    put_u32(p, 100, fields.velocity_age_us);
    put_u32(p, 104, fields.pressure_pair_age_us);
    put_value(p, 108, 113, fields.reference);
    p[112] = fields.reference_kind as u8;
    put_value(p, 116, 114, fields.candidate_rpm);
    put_value(p, 120, 115, fields.last_submitted_rpm);
    put_u32(p, 124, fields.submitted_age_us);
    put_u32(p, 128, fields.axis_state_raw);
    put_u32(p, 132, fields.axis_error_raw);
    put_u32(p, 136, fields.driver_age_us);
    p[140] = fields.driver_state.bits();
    p[141] = fields.desired_axis as u8;
    put_u32(p, 142, fields.can_heartbeat_age_us);
    put_u32(p, 146, fields.usb_heartbeat_age_us);
    p[150] = fields.can_contact as u8;
    p[151] = fields.usb_contact as u8;
    encode_in_place(kind, TelemetryFields::encoded_len() - 8, output)
}
