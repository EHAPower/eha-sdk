// Copyright The eha_controller Contributors

use super::*;

/// 维护操作类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MaintenanceOperation {
    /// 保存配置。
    SaveConfig = 1,
    /// 恢复出厂配置。
    RestoreFactory = 2,
    /// 应用复位。
    ResetApplication = 3,
    /// 切换更新入口。
    EnterUpdate = 4,
}

/// 维护结果状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MaintenanceState {
    /// 未开始。
    NotStarted = 0,
    /// 已开始。
    Started = 1,
    /// 进行中。
    InProgress = 2,
    /// 固件步骤完成。
    FirmwareStepComplete = 3,
    /// 已失败。
    Failed = 4,
    /// 等待到期。
    WaitingDeadline = 5,
    /// 结果未明。
    ResultUnknown = 6,
    /// 应用移交已开始。
    ApplicationHandoffStarted = 7,
}

/// 维护步骤证据位。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OperationEvidence {
    /// 副作用尚未开始。
    pub not_started: bool,
    /// 存储替换返回成功。
    pub storage_replace_succeeded: bool,
    /// 实际存储读取完成。
    pub actual_storage_read: bool,
    /// 副作用可能已发生。
    pub effect_may_have_occurred: bool,
    /// 原调用结果缺失。
    pub original_result_missing: bool,
    /// 本结果已结清。
    pub settled: bool,
}
impl OperationEvidence {
    fn bits(self) -> u8 {
        (self.not_started as u8)
            | ((self.storage_replace_succeeded as u8) << 1)
            | ((self.actual_storage_read as u8) << 2)
            | ((self.effect_may_have_occurred as u8) << 3)
            | ((self.original_result_missing as u8) << 4)
            | ((self.settled as u8) << 5)
    }
}

/// OperationResult 的完整具名字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationResultFields {
    /// 共同回复快照.
    pub sample: SampleData,
    /// 维护操作标识。
    pub operation_id: u64,
    /// 维护操作。
    pub operation: MaintenanceOperation,
    /// 当前结果状态。
    pub state: MaintenanceState,
    /// 所处执行阶段。
    pub phase: OperationPhase,
    /// 维护步骤证据。
    pub evidence: OperationEvidence,
    /// 合同原因码。
    pub reason: u16,
    /// 必要操作位图。
    pub next_actions: u16,
    /// 仍受约束资源。
    pub constraints: Constraints,
    /// 操作开始时间。
    pub started_time_us: u64,
    /// 原业务期限。
    pub deadline_us: u64,
    /// 结清时间。
    pub finished_time_us: u64,
    /// 涉及记录长度。
    pub content_length: u32,
    /// 涉及记录的 CRC-32C。
    pub content_crc32c: u32,
    /// 留存结果版本；非留存通知为零。
    pub revision: u32,
}

impl OperationResultFields {
    /// `OperationResult` 的完整线上长度。
    pub const fn encoded_len() -> usize {
        96
    }
}

/// 编码维护进展或结果。
pub fn encode_operation_result(
    fields: &OperationResultFields,
    output: &mut [u8],
) -> Result<usize, Error> {
    let p = payload(output, OperationResultFields::encoded_len() - 8)?;
    write_sample(p, fields.sample);
    put_u64(p, 32, fields.operation_id);
    p[40] = fields.operation as u8;
    p[41] = fields.state as u8;
    p[42] = fields.phase as u8;
    p[43] = fields.evidence.bits();
    put_u16(p, 44, fields.reason);
    put_u16(p, 46, fields.next_actions);
    put_u32(p, 48, fields.constraints.bits());
    put_u64(p, 52, fields.started_time_us);
    put_u64(p, 60, fields.deadline_us);
    put_u64(p, 68, fields.finished_time_us);
    put_u32(p, 76, fields.content_length);
    put_u32(p, 80, fields.content_crc32c);
    put_u32(p, 84, fields.revision);
    encode_in_place(
        MessageKind::OperationResult,
        OperationResultFields::encoded_len() - 8,
        output,
    )
}

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
    /// 维护结果。
    OperationResult = 5,
    /// 结果释放。
    ResultRelease = 6,
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
    /// 是否仍留存原维护结果。
    pub retained: bool,
    /// 关联维护操作标识；无关时零。
    pub operation_id: u64,
}

impl DataUnavailableFields {
    /// `DataUnavailable` 的完整线上长度。
    pub const fn encoded_len() -> usize {
        56
    }
}

/// 编码数据或结果不可取得的事实。
pub fn encode_data_unavailable(
    fields: &DataUnavailableFields,
    output: &mut [u8],
) -> Result<usize, Error> {
    let p = payload(output, DataUnavailableFields::encoded_len() - 8)?;
    write_sample(p, fields.sample);
    p[32] = fields.subject as u8;
    put_u16(p, 33, fields.reason);
    put_u16(p, 35, fields.next_actions);
    p[37] = fields.retained as u8;
    p[38..40].fill(0);
    put_u64(p, 40, fields.operation_id);
    encode_in_place(
        MessageKind::DataUnavailable,
        DataUnavailableFields::encoded_len() - 8,
        output,
    )
}
