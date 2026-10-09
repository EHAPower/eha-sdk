// Copyright The eha-sdk Contributors

use super::*;

/// 诊断所属域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DiagnosticDomain {
    /// 控制。
    Control = 0,
    /// 外部 CAN。
    Can = 1,
    /// USB。
    Usb = 2,
    /// 驱动。
    Driver = 3,
    /// 位置。
    Position = 4,
    /// 压力。
    Pressure = 5,
    /// 配置存储。
    ConfigStore = 6,
    /// 本地平台。
    Platform = 7,
    /// 维护。
    Maintenance = 8,
}

/// 诊断对象。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DiagnosticObject {
    /// 整体。
    Overall = 0,
    /// 位置。
    Position = 1,
    /// 线速度。
    Velocity = 2,
    /// 压力 A。
    PressureA = 3,
    /// 压力 B。
    PressureB = 4,
    /// 估算力。
    Force = 5,
    /// 电机输出。
    MotorOutput = 6,
    /// 驱动轴。
    DriverAxis = 7,
    /// 用户记录。
    UserRecord = 8,
    /// 应用。
    Application = 9,
    /// 更新入口。
    UpdateRoute = 10,
    /// CAN。
    Can = 11,
    /// USB。
    Usb = 12,
}

/// 诊断所处执行阶段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum OperationPhase {
    /// 无操作。
    None = 0,
    /// 条件检查。
    ConditionCheck = 1,
    /// 等待进入。
    WaitingEntry = 2,
    /// 已调用发送。
    SendCalled = 3,
    /// 等待新反馈。
    WaitingFeedback = 4,
    /// 等待退出。
    WaitingExit = 5,
    /// 格式解析。
    FormatParse = 6,
    /// 存储替换。
    StorageReplacement = 7,
    /// 实际读回。
    ActualReadback = 8,
    /// 结果交付。
    ResultDelivery = 9,
    /// 应用移交。
    ApplicationHandoff = 10,
    /// 资源恢复。
    ResourceRecovery = 11,
}

/// 诊断影响位。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticImpact {
    /// 阻止采用。
    pub blocks_adoption: bool,
    /// 限制输出。
    pub limits_output: bool,
    /// 结束目标。
    pub ends_target: bool,
    /// 限制维护。
    pub limits_maintenance: bool,
    /// 影响观测。
    pub affects_observation: bool,
    /// 存在未知副作用。
    pub unknown_side_effect: bool,
    /// 当前仍有效。
    pub current: bool,
    /// 历史上下文。
    pub historical: bool,
}
impl DiagnosticImpact {
    fn bits(self) -> u8 {
        (self.blocks_adoption as u8)
            | ((self.limits_output as u8) << 1)
            | ((self.ends_target as u8) << 2)
            | ((self.limits_maintenance as u8) << 3)
            | ((self.affects_observation as u8) << 4)
            | ((self.unknown_side_effect as u8) << 5)
            | ((self.current as u8) << 6)
            | ((self.historical as u8) << 7)
    }
}

/// 诊断原生码的来源域。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NativeDomain {
    /// 无原生码。
    None = 0,
    /// ODrive 轴错误位。
    OdriveAxisError = 1,
    /// ODrive 轴状态。
    OdriveAxisState = 2,
    /// CAN 公开错误。
    Can = 3,
    /// USB 公开错误。
    Usb = 4,
    /// 记录解析。
    RecordParse = 5,
    /// Flash 公开错误。
    Flash = 6,
    /// ODrive 无感估算器错误位；来自启动恢复的实际 CAN 查询。
    OdriveSensorlessError = 7,
    /// ODrive 电机错误位；来自启动恢复的实际 CAN 查询。
    OdriveMotorError = 8,
}

/// 诊断证据位。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticEvidence {
    /// 副作用尚未开始。
    pub not_started: bool,
    /// 发送接口本地提交。
    pub send_submitted: bool,
    /// 已取得相符新设备反馈。
    pub matched_feedback: bool,
    /// 存储替换返回。
    pub storage_replaced: bool,
    /// 实际存储读回。
    pub actual_readback: bool,
    /// 缺原结果证据。
    pub missing_original_result: bool,
}
impl DiagnosticEvidence {
    fn bits(self) -> u8 {
        (self.not_started as u8)
            | ((self.send_submitted as u8) << 1)
            | ((self.matched_feedback as u8) << 2)
            | ((self.storage_replaced as u8) << 3)
            | ((self.actual_readback as u8) << 4)
            | ((self.missing_original_result as u8) << 5)
    }
}

/// 诊断细节值单位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DetailUnit {
    /// 无单位。
    None = 0,
    /// 毫米。
    Mm = 1,
    /// 毫米每秒。
    MmPerSecond = 2,
    /// 兆帕。
    Mpa = 3,
    /// 牛顿。
    N = 4,
    /// 转每分。
    Rpm = 5,
    /// 转每秒。
    TurnsPerSecond = 6,
    /// 字节数。
    Bytes = 7,
}

/// 仍受约束的资源位。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Constraints {
    /// 用户存储。
    pub user_storage: bool,
    /// 驱动进入或退出。
    pub driver_transition: bool,
    /// 应用移交。
    pub application_handoff: bool,
    /// 结果留存容量。
    pub result_slot: bool,
}
impl Constraints {
    pub(super) fn bits(self) -> u32 {
        (self.user_storage as u32)
            | ((self.driver_transition as u32) << 1)
            | ((self.application_handoff as u32) << 2)
            | ((self.result_slot as u32) << 3)
    }
}

/// 尚缺失的证据位。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MissingEvidence {
    /// 调用结果。
    pub call_result: bool,
    /// 设备反馈。
    pub device_feedback: bool,
    /// 实际读回。
    pub actual_readback: bool,
    /// 同设备内容核对。
    pub same_device_comparison: bool,
    /// 更新入口身份。
    pub update_route_identity: bool,
    /// 新应用启动。
    pub new_application_start: bool,
}
impl MissingEvidence {
    fn bits(self) -> u32 {
        (self.call_result as u32)
            | ((self.device_feedback as u32) << 1)
            | ((self.actual_readback as u32) << 2)
            | ((self.same_device_comparison as u32) << 3)
            | ((self.update_route_identity as u32) << 4)
            | ((self.new_application_start as u32) << 5)
    }
}

/// 单条详细诊断的具名字段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiagnosticFields {
    /// 所属域。
    pub domain: DiagnosticDomain,
    /// 关联对象。
    pub object: DiagnosticObject,
    /// 当前影响。
    pub impact: DiagnosticImpact,
    /// 执行阶段。
    pub phase: OperationPhase,
    /// 合同原因码。
    pub reason: u16,
    /// 必要操作位图。
    pub next_actions: u16,
    /// 来源事件时间。
    pub event_time_us: u64,
    /// 原操作开始时间。
    pub started_time_us: u64,
    /// 原业务期限。
    pub deadline_us: u64,
    /// 维护操作标识；非维护诊断为零。
    pub operation_id: u64,
    /// 已取得的原始错误码。
    pub native_code: u32,
    /// 原始错误码域。
    pub native_domain: NativeDomain,
    /// 证据。
    pub evidence: DiagnosticEvidence,
    /// 发生时相关量。
    pub detail_value: ValueFields,
    /// 细节值单位。
    pub detail_unit: DetailUnit,
    /// 仍受约束资源。
    pub constraints: Constraints,
    /// 同类发生次数。
    pub occurrences: u32,
    /// 尚缺证据。
    pub missing_evidence: MissingEvidence,
}

/// 诊断回复的完整具名字段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiagnosticsFields<'a> {
    /// 共同回复快照。
    pub sample: SampleData,
    /// 是否还有未逐项列出的诊断。
    pub overflow: bool,
    /// 最多16条有序诊断。
    pub entries: &'a [DiagnosticFields],
}

impl DiagnosticsFields<'_> {
    /// 计算本诊断回复的完整线上长度，供调用方预检发送缓冲。
    pub fn encoded_len(&self) -> Result<usize, Error> {
        if self.entries.len() > 16 {
            return Err(Error::InvalidPayloadLength);
        }
        let payload_length = 36usize
            .checked_add(
                self.entries
                    .len()
                    .checked_mul(64)
                    .ok_or(Error::InvalidPayloadLength)?,
            )
            .ok_or(Error::InvalidPayloadLength)?;
        Ok(payload_length + 8)
    }
}

/// 编码有界详细诊断回复。
pub fn encode_diagnostics(
    fields: &DiagnosticsFields<'_>,
    output: &mut [u8],
) -> Result<usize, Error> {
    let length = fields.encoded_len()?;
    let p = payload(output, length - 8)?;
    write_sample(p, fields.sample);
    put_u16(p, 32, fields.entries.len() as u16);
    p[34] = fields.overflow as u8;
    p[35] = 0;
    for (index, entry) in fields.entries.iter().enumerate() {
        write_diagnostic(&mut p[36 + index * 64..100 + index * 64], entry);
    }
    encode_in_place(MessageKind::Diagnostics, length - 8, output)
}

fn write_diagnostic(out: &mut [u8], field: &DiagnosticFields) {
    out[0] = field.domain as u8;
    out[1] = field.object as u8;
    out[2] = field.impact.bits();
    out[3] = field.phase as u8;
    put_u16(out, 4, field.reason);
    put_u16(out, 6, field.next_actions);
    put_u64(out, 8, field.event_time_us);
    put_u64(out, 16, field.started_time_us);
    put_u64(out, 24, field.deadline_us);
    put_u64(out, 32, field.operation_id);
    put_u32(out, 40, field.native_code);
    out[44] = field.native_domain as u8;
    out[45] = field.evidence.bits();
    out[46..48].fill(0);
    put_value(out, 48, 52, field.detail_value);
    out[53] = field.detail_unit as u8;
    put_u16(out, 54, field.constraints.bits() as u16);
    put_u32(out, 56, field.occurrences);
    put_u32(out, 60, field.missing_evidence.bits());
}
