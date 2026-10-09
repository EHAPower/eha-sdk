// Copyright The eha-sdk Contributors
//! 将线上诊断值转换为面向使用者的简短说明。
//!
//! 这里不裁决设备是否健康，也不把历史诊断当作当前故障。调用方必须同时保留原始
//! 数值、`impact.current`、来源年龄及质量。ODrive 解码采用项目锁定的
//! `odrive==0.5.1.post0`（MKS 固件 0.0.0.1 适用）公开枚举；未定义位保持原样。

use crate::protocol::responses::{
    CanProfile, ConfigRecordState, ConfigView, ContactState, DesiredAxis, DetailUnit,
    DiagnosticDomain, DiagnosticObject, DriverState, FallbackReason, MaintenanceOperation,
    MaintenanceState, ModelState, NativeDomain, OperationPhase, PositiveForceChannel,
    ReferenceKind, RunIdentity, SourceQuality, StartupSource, TargetIngress, TargetMode,
    UnavailableSubject, UpdateRoute, ValueResult,
};

const ODRIVE_AXIS_ERROR_BITS: &[(u32, &str)] = &[
    (0x0000_0001, "状态无效"),
    (0x0000_0002, "直流母线欠压"),
    (0x0000_0004, "直流母线过压"),
    (0x0000_0008, "电流测量超时"),
    (0x0000_0010, "制动电阻已解除"),
    (0x0000_0020, "电机已解除"),
    (0x0000_0040, "电机故障"),
    (0x0000_0080, "无感估算器故障"),
    (0x0000_0100, "编码器故障"),
    (0x0000_0200, "控制器故障"),
    (0x0000_0400, "无感控制时使用位置控制"),
    (0x0000_0800, "watchdog 已到期"),
    (0x0000_1000, "最小端止挡被按下"),
    (0x0000_2000, "最大端止挡被按下"),
    (0x0000_4000, "急停请求"),
    (0x0002_0000, "未配置端止挡即回零"),
    (0x0004_0000, "过温"),
];

const ODRIVE_MOTOR_ERROR_BITS: &[(u32, &str)] = &[
    (0x0000_0001, "相电阻超范围"),
    (0x0000_0002, "相电感超范围"),
    (0x0000_0004, "ADC 故障"),
    (0x0000_0008, "驱动器故障"),
    (0x0000_0010, "控制截止期遗漏"),
    (0x0000_0020, "未实现的电机类型"),
    (0x0000_0040, "制动电流超范围"),
    (0x0000_0080, "调制幅值异常"),
    (0x0000_0100, "制动死区违例"),
    (0x0000_0200, "非预期定时器回调"),
    (0x0000_0400, "电流采样饱和"),
    (0x0000_1000, "电流限制违例"),
    (0x0000_2000, "制动占空比为 NaN"),
    (0x0000_4000, "直流母线回灌电流过大"),
    (0x0000_8000, "直流母线电流过大"),
];

const ODRIVE_SENSORLESS_ERROR_BITS: &[(u32, &str)] = &[(0x0000_0001, "增益不稳定")];

/// 单一原因目录中的可读说明。未知值由调用方同时展示其原始十六进制值。
pub fn reason_label(reason: u16) -> &'static str {
    match reason {
        0x0000 => "无原因",
        0x0001 => "本入口未收到显式心跳",
        0x0002 => "本入口联系已过期",
        0x0003 => "位置数值不可用",
        0x0004 => "位置已过期",
        0x0005 => "线速度不可用",
        0x0006 => "完整压力对不可用",
        0x0007 => "压力对已过期",
        0x0008 => "所需估算力不可用",
        0x0009 => "驱动状态不可用",
        0x000a => "驱动状态已过期",
        0x000b => "驱动报告轴错误",
        0x000c => "轴状态不适用",
        0x000d => "等待本次进入证据",
        0x000e => "等待本次退出结清",
        0x000f => "已开始副作用未明",
        0x0010 => "维护资源占用",
        0x0011 => "操作输入无效",
        0x0012 => "目标超允许域",
        0x0013 => "控制计算失败",
        0x0014 => "硬保护条件",
        0x0015 => "会结束目标的软保护条件",
        0x0016 => "最终输出不合法",
        0x0017 => "所需资源不可用",
        0x0018 => "启动参数派生失败",
        0x0019 => "当前目标属于另一入口",
        0x001a => "没有当前目标",
        0x001b => "维护结果容量占用",
        0x001c => "无应用更新路由",
        0x001d => "运行实例来源不可取得",
        0x001e => "测量参考不适用",
        0x001f => "来源明确故障",
        0x0020 => "保留原因（协议要求阻止位为零）",
        0x0100 => "传输格式错误",
        0x0101 => "组装超时",
        0x0102 => "连接阶段改变",
        0x0103 => "总线不可用",
        0x0104 => "发送接口失败",
        0x0105 => "接收容量溢出",
        0x0106 => "控制周期遗漏",
        0x0200 => "配置为空",
        0x0201 => "配置残缺或不可解析",
        0x0202 => "配置格式版本不支持",
        0x0203 => "目标类型不能表示",
        0x0204 => "存储读取失败",
        0x0205 => "存储替换失败",
        0x0206 => "读回不一致",
        0x0300 => "结果未留存",
        0x0301 => "操作标识已使用",
        0x0302 => "设备不匹配",
        0x0303 => "应用会话不匹配",
        0x0304 => "结果尚未结清",
        0x0305 => "结果版本已变化",
        0x0306 => "结果已释放",
        0x0307 => "所需信息不可取得",
        0x0400 => "使用者主动停止",
        _ => "未知原因",
    }
}

/// 将阻止原因位图中置位的原因按位序转换为说明。
pub fn reason_bitmap_labels(blockers: u32) -> Vec<&'static str> {
    (0..u32::BITS)
        .filter(|bit| blockers & (1 << bit) != 0)
        .map(|bit| reason_label(bit as u16 + 1))
        .collect()
}

/// 设备给出的必要后续操作说明。该结果不授权工具自动执行任何操作。
pub fn next_actions_labels(actions: u16) -> Vec<&'static str> {
    const LABELS: [&str; 11] = [
        "重新查询事实",
        "维持或重建所选入口心跳",
        "满足条件后提交新目标",
        "使用者明确停止",
        "实际读取用户记录",
        "主机核对完整配置",
        "另行请求应用复位",
        "采用外部更新或恢复路径",
        "处理外部设备条件",
        "取得并释放已结清维护结果",
        "等待已开始操作的事实",
    ];
    LABELS
        .iter()
        .enumerate()
        .filter_map(|(bit, label)| (actions & (1 << bit) != 0).then_some(*label))
        .collect()
}

/// `next_actions` 中未在当前协议定义的位，调用方应以原始位图报告。
pub const fn unknown_next_action_bits(actions: u16) -> u16 {
    actions & !0x07ff
}

/// 当前目标模式。
pub const fn target_mode_label(mode: TargetMode) -> &'static str {
    match mode {
        TargetMode::None => "无当前目标",
        TargetMode::Position => "位置",
        TargetMode::Velocity => "速度",
        TargetMode::Force => "力",
        TargetMode::Impedance => "阻抗",
    }
}

/// 固件实际采用目标的入口。
pub const fn target_ingress_label(ingress: TargetIngress) -> &'static str {
    match ingress {
        TargetIngress::None => "无入口",
        TargetIngress::Can => "CAN",
        TargetIngress::Usb => "USB",
    }
}

/// ODrive 0.5.1 CANSimple 已锁定的原始轴状态；未知值不转换为其他状态。
pub const fn axis_state_label(state: u32) -> &'static str {
    match state {
        0 => "未定义",
        1 => "Idle",
        2 => "启动序列",
        3 => "完整校准序列",
        4 => "电机校准",
        5 => "无感控制",
        6 => "编码器索引搜索",
        7 => "编码器偏移校准",
        8 => "闭环控制",
        9 => "锁相旋转",
        10 => "编码器方向查找",
        11 => "回零",
        _ => "未知 ODrive 轴状态",
    }
}

/// 固件期望的轴状态，不等同于实际 ODrive 轴状态。
pub const fn desired_axis_label(axis: DesiredAxis) -> &'static str {
    match axis {
        DesiredAxis::None => "无可用期望",
        DesiredAxis::Idle => "Idle",
        DesiredAxis::ClosedLoop => "闭环控制",
    }
}

/// 主机入口联系状态。
pub const fn contact_state_label(state: ContactState) -> &'static str {
    match state {
        ContactState::Never => "从未收到有效心跳",
        ContactState::Active => "在允许年龄内",
        ContactState::Expired => "已过期",
    }
}

/// 当前参考物理量。
pub const fn reference_kind_label(kind: ReferenceKind) -> &'static str {
    match kind {
        ReferenceKind::None => "无参考",
        ReferenceKind::PositionMm => "位置（mm）",
        ReferenceKind::VelocityMmPerSecond => "速度（mm/s）",
        ReferenceKind::ForceN => "力（N）",
    }
}

/// 数值结果说明。
pub const fn value_result_label(result: ValueResult) -> &'static str {
    match result {
        ValueResult::Never => "从未有值",
        ValueResult::Available => "可表示值",
        ValueResult::CalculationFailed => "换算或计算失败",
        ValueResult::HistoryInsufficient => "历史不足",
        ValueResult::NotComputed => "本次未计算",
        ValueResult::OutputInhibited => "输出禁止",
        ValueResult::NotApplicable => "模型或参考不适用",
    }
}

/// 来源质量说明。
pub const fn value_quality_label(quality: SourceQuality) -> &'static str {
    match quality {
        SourceQuality::Unknown => "来源质量未知",
        SourceQuality::Qualified => "来源合格",
        SourceQuality::Faulted => "来源故障",
        SourceQuality::Discontinuous => "来源连续性中断",
    }
}

/// 驱动来源分类。它不把历史或过期状态描述成当前实际轴状态。
pub const fn driver_state_summary_label(state: DriverState) -> &'static str {
    if !state.has_status {
        "未取得 ODrive 状态"
    } else if state.stale {
        "ODrive 状态已过期"
    } else if state.faulted {
        "ODrive 报告故障"
    } else if !state.qualified {
        "ODrive 状态未获合格"
    } else {
        "ODrive 状态合格"
    }
}

/// 诊断所属域。
pub const fn diagnostic_domain_label(domain: DiagnosticDomain) -> &'static str {
    match domain {
        DiagnosticDomain::Control => "固件控制",
        DiagnosticDomain::Can => "外部 CAN",
        DiagnosticDomain::Usb => "USB",
        DiagnosticDomain::Driver => "ODrive 驱动",
        DiagnosticDomain::Position => "位置",
        DiagnosticDomain::Pressure => "压力",
        DiagnosticDomain::ConfigStore => "配置存储",
        DiagnosticDomain::Platform => "本地平台",
        DiagnosticDomain::Maintenance => "维护",
    }
}

/// 诊断关联对象。
pub const fn object_label(object: DiagnosticObject) -> &'static str {
    match object {
        DiagnosticObject::Overall => "整体",
        DiagnosticObject::Position => "位置",
        DiagnosticObject::Velocity => "线速度",
        DiagnosticObject::PressureA => "压力 A",
        DiagnosticObject::PressureB => "压力 B",
        DiagnosticObject::Force => "估算力",
        DiagnosticObject::MotorOutput => "电机输出",
        DiagnosticObject::DriverAxis => "ODrive 轴",
        DiagnosticObject::UserRecord => "用户记录",
        DiagnosticObject::Application => "应用",
        DiagnosticObject::UpdateRoute => "更新入口",
        DiagnosticObject::Can => "CAN",
        DiagnosticObject::Usb => "USB",
    }
}

/// 诊断所处的实际执行阶段。
pub const fn phase_label(phase: OperationPhase) -> &'static str {
    match phase {
        OperationPhase::None => "无操作",
        OperationPhase::ConditionCheck => "条件检查",
        OperationPhase::WaitingEntry => "等待进入",
        OperationPhase::SendCalled => "已调用发送",
        OperationPhase::WaitingFeedback => "等待新反馈",
        OperationPhase::WaitingExit => "等待退出",
        OperationPhase::FormatParse => "格式解析",
        OperationPhase::StorageReplacement => "存储替换",
        OperationPhase::ActualReadback => "实际读回",
        OperationPhase::ResultDelivery => "结果交付",
        OperationPhase::ApplicationHandoff => "应用移交",
        OperationPhase::ResourceRecovery => "资源恢复",
    }
}

/// 原生错误码的来源域。
pub const fn native_domain_label(domain: NativeDomain) -> &'static str {
    match domain {
        NativeDomain::None => "无原生码",
        NativeDomain::OdriveAxisError => "ODrive 轴错误位",
        NativeDomain::OdriveAxisState => "ODrive 轴状态",
        NativeDomain::Can => "CAN 公开错误",
        NativeDomain::Usb => "USB 公开错误",
        NativeDomain::RecordParse => "配置记录解析",
        NativeDomain::Flash => "Flash 公开错误",
        NativeDomain::OdriveSensorlessError => "ODrive 无感估算器错误位",
        NativeDomain::OdriveMotorError => "ODrive 电机错误位",
    }
}

/// 维护结果状态。
pub const fn operation_state_label(state: MaintenanceState) -> &'static str {
    match state {
        MaintenanceState::NotStarted => "未开始",
        MaintenanceState::Started => "已开始",
        MaintenanceState::InProgress => "进行中",
        MaintenanceState::FirmwareStepComplete => "固件步骤完成",
        MaintenanceState::Failed => "已失败",
        MaintenanceState::WaitingDeadline => "等待到期",
        MaintenanceState::ResultUnknown => "结果未明",
        MaintenanceState::ApplicationHandoffStarted => "应用移交已开始",
    }
}

/// 启动配置来源。
pub const fn startup_source_label(source: StartupSource) -> &'static str {
    match source {
        StartupSource::Unavailable => "来源不可取得",
        StartupSource::User => "用户记录",
        StartupSource::Factory => "出厂记录",
    }
}

/// 启动回退原因。
pub const fn fallback_reason_label(reason: FallbackReason) -> &'static str {
    match reason {
        FallbackReason::None => "无回退",
        FallbackReason::UserEmpty => "用户记录为空",
        FallbackReason::ReadFailed => "读取失败",
        FallbackReason::ParseFailed => "记录残缺或解析失败",
        FallbackReason::FormatUnsupported => "配置格式版本不支持",
        FallbackReason::NumericRepresentationFailed => "数值表示失败",
        FallbackReason::SourceUnavailable => "来源不可取得",
    }
}

/// 配置视图。
pub const fn config_view_label(view: ConfigView) -> &'static str {
    match view {
        ConfigView::Factory => "出厂记录",
        ConfigView::UserRecord => "用户记录",
        ConfigView::Startup => "启动配置",
        ConfigView::Communication => "当前通信设置",
    }
}

/// 配置记录状态。
pub const fn config_record_state_label(state: ConfigRecordState) -> &'static str {
    match state {
        ConfigRecordState::Complete => "完整可解析",
        ConfigRecordState::Empty => "记录为空",
        ConfigRecordState::Incomplete => "残缺或不可解析",
        ConfigRecordState::FormatUnsupported => "格式版本不支持",
        ConfigRecordState::NumericRepresentationFailed => "数值表示失败",
        ConfigRecordState::ReadFailed => "读取失败",
        ConfigRecordState::ViewUnavailable => "所需视图不可取得",
    }
}

/// 维护操作类型。
pub const fn maintenance_operation_label(operation: MaintenanceOperation) -> &'static str {
    match operation {
        MaintenanceOperation::SaveConfig => "保存配置",
        MaintenanceOperation::RestoreFactory => "恢复出厂配置",
        MaintenanceOperation::ResetApplication => "应用复位",
        MaintenanceOperation::EnterUpdate => "切换更新入口",
    }
}

/// 诊断细节值的单位。
pub const fn detail_unit_label(unit: DetailUnit) -> &'static str {
    match unit {
        DetailUnit::None => "无单位",
        DetailUnit::Mm => "mm",
        DetailUnit::MmPerSecond => "mm/s",
        DetailUnit::Mpa => "MPa",
        DetailUnit::N => "N",
        DetailUnit::Rpm => "rpm",
        DetailUnit::TurnsPerSecond => "turn/s",
        DetailUnit::Bytes => "字节",
    }
}

/// `DataUnavailable` 所述不可取得的对象。
pub const fn unavailable_subject_label(subject: UnavailableSubject) -> &'static str {
    match subject {
        UnavailableSubject::Identity => "身份",
        UnavailableSubject::Status => "当前状态",
        UnavailableSubject::Measurements => "测量",
        UnavailableSubject::Diagnostics => "诊断",
        UnavailableSubject::Config => "配置",
        UnavailableSubject::OperationResult => "维护结果",
        UnavailableSubject::ResultRelease => "结果释放",
    }
}

/// 估算力正方向对应的电气压力通道。
pub const fn positive_force_channel_label(channel: PositiveForceChannel) -> &'static str {
    match channel {
        PositiveForceChannel::Unavailable => "不可取得",
        PositiveForceChannel::ElectricalA => "电气 A 通道",
        PositiveForceChannel::ElectricalB => "电气 B 通道",
    }
}

/// 等面积模型状态。
pub const fn model_state_label(state: ModelState) -> &'static str {
    match state {
        ModelState::Unavailable => "模型不可取得",
        ModelState::Available => "模型可用",
        ModelState::NotApplicable => "模型不适用",
    }
}

/// 运行实例来源状态。
pub const fn run_identity_label(state: RunIdentity) -> &'static str {
    match state {
        RunIdentity::Unavailable => "随机运行实例不可取得",
        RunIdentity::Random => "随机运行实例已取得",
    }
}

/// 更新路由状态。
pub const fn update_route_label(route: UpdateRoute) -> &'static str {
    match route {
        UpdateRoute::Absent => "无应用切换路由",
        UpdateRoute::Present => "存在适用路由",
        UpdateRoute::Unverifiable => "无法核对更新路由",
    }
}

/// 当前 CAN 配置组。
pub const fn can_profile_label(profile: CanProfile) -> &'static str {
    match profile {
        CanProfile::Classical500k => "经典 CAN 500 kbit/s",
        CanProfile::Classical1m => "经典 CAN 1 Mbit/s",
        CanProfile::Fd500k2m => "CAN FD 500 kbit/s / 2 Mbit/s",
        CanProfile::Fd500k500k => "CAN FD 500 kbit/s / 500 kbit/s",
        CanProfile::Fd1m2m => "CAN FD 1 Mbit/s / 2 Mbit/s",
        CanProfile::Fd1m5m => "CAN FD 1 Mbit/s / 5 Mbit/s",
        CanProfile::Fd1m8m => "CAN FD 1 Mbit/s / 8 Mbit/s",
    }
}

/// 在来源域内解释原生码；未知 ODrive 位保留在描述和原始数值中。
pub fn native_code_description(domain: NativeDomain, code: u32) -> String {
    let description = match domain {
        NativeDomain::None => "无原生码",
        NativeDomain::OdriveAxisError => {
            return odrive_bit_description("ODrive 轴错误", code, ODRIVE_AXIS_ERROR_BITS);
        }
        NativeDomain::OdriveAxisState => {
            return format!(
                "ODrive 轴状态：{}（原始 0x{code:08X}）",
                axis_state_label(code)
            );
        }
        NativeDomain::Can => match code {
            1 => "CAN 位填充错误",
            2 => "CAN 格式错误",
            3 => "CAN 链路层 ACK 错误",
            4 => "CAN 位 1 错误",
            5 => "CAN 位 0 错误",
            6 => "CAN CRC 错误",
            7 => "CAN BusOff",
            8 => "CAN 其他公开错误",
            _ => "未知 CAN 公开错误",
        },
        NativeDomain::Usb => match code {
            1 => "USB 已禁用",
            2 => "USB 缓冲区溢出",
            _ => "未知 USB 公开错误",
        },
        NativeDomain::RecordParse => match code {
            1 => "配置记录为空",
            2 => "配置记录容量错误",
            3 => "配置记录版本错误",
            4 => "配置记录解码错误",
            5 => "配置记录包含非有限数",
            6 => "配置记录编码失败",
            _ => "未知配置记录解析错误",
        },
        NativeDomain::Flash => match code {
            1 => "Flash 未对齐",
            2 => "Flash 并行度错误",
            3 => "Flash 顺序错误",
            4 => "Flash 写保护",
            5 => "Flash 编程错误",
            6 => "Flash 操作错误",
            7 => "Flash 其他公开错误",
            _ => "未知 Flash 公开错误",
        },
        NativeDomain::OdriveSensorlessError => {
            return odrive_bit_description(
                "ODrive 无感估算器错误",
                code,
                ODRIVE_SENSORLESS_ERROR_BITS,
            );
        }
        NativeDomain::OdriveMotorError => {
            return odrive_bit_description("ODrive 电机错误", code, ODRIVE_MOTOR_ERROR_BITS);
        }
    };
    format!("{description}（原始 0x{code:08X}）")
}

fn odrive_bit_description(name: &str, code: u32, known_bits: &[(u32, &str)]) -> String {
    if code == 0 {
        return format!("{name}：无错误位（原始 0x{code:08X}）");
    }

    let known_mask = known_bits.iter().fold(0, |mask, (bit, _)| mask | bit);
    let mut labels: Vec<_> = known_bits
        .iter()
        .filter_map(|(bit, label)| (code & bit != 0).then_some(*label))
        .collect();
    let unknown = code & !known_mask;
    if unknown != 0 {
        labels.push("未知位");
    }
    format!(
        "{name}：{}（原始 0x{code:08X}{}）",
        labels.join("、"),
        if unknown != 0 {
            format!("；未知位 0x{unknown:08X}")
        } else {
            String::new()
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_native_code_uses_its_source_domain() {
        assert_eq!(
            native_code_description(NativeDomain::Can, 1),
            "CAN 位填充错误（原始 0x00000001）"
        );
        assert_eq!(
            native_code_description(NativeDomain::Usb, 1),
            "USB 已禁用（原始 0x00000001）"
        );
    }

    #[test]
    fn unknown_odrive_error_bits_remain_visible() {
        let description = native_code_description(NativeDomain::OdriveAxisError, 0x8000_0000);
        assert!(description.contains("未知位"));
        assert!(description.contains("0x80000000"));
    }

    #[test]
    fn combined_action_and_blocker_bits_keep_each_meaning() {
        assert_eq!(
            next_actions_labels((1 << 0) | (1 << 8)),
            vec!["重新查询事实", "处理外部设备条件"]
        );
        assert_eq!(
            reason_bitmap_labels((1 << 0) | (1 << 10)),
            vec!["本入口未收到显式心跳", "驱动报告轴错误"]
        );
        let errors = native_code_description(NativeDomain::OdriveAxisError, 0x0000_0802);
        assert!(errors.contains("直流母线欠压"));
        assert!(errors.contains("watchdog 已到期"));
    }
}
