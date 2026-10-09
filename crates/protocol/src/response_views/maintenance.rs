// Copyright The eha-sdk Contributors

use super::common::{Sample, decoded_value_fields, sample_data};
use crate::responses;
use crate::validation::{f32_at, u16_at, u32_at, u64_at};

fn diagnostic_impact(bits: u8) -> responses::DiagnosticImpact {
    responses::DiagnosticImpact {
        blocks_adoption: bits & 1 != 0,
        limits_output: bits & 2 != 0,
        ends_target: bits & 4 != 0,
        limits_maintenance: bits & 8 != 0,
        affects_observation: bits & 16 != 0,
        unknown_side_effect: bits & 32 != 0,
        current: bits & 64 != 0,
        historical: bits & 128 != 0,
    }
}

fn diagnostic_evidence(bits: u8) -> responses::DiagnosticEvidence {
    responses::DiagnosticEvidence {
        not_started: bits & 1 != 0,
        send_submitted: bits & 2 != 0,
        matched_feedback: bits & 4 != 0,
        storage_replaced: bits & 8 != 0,
        actual_readback: bits & 16 != 0,
        missing_original_result: bits & 32 != 0,
    }
}

fn constraints(bits: u32) -> responses::Constraints {
    responses::Constraints {
        user_storage: bits & 1 != 0,
        driver_transition: bits & 2 != 0,
        application_handoff: bits & 4 != 0,
        result_slot: bits & 8 != 0,
    }
}

fn missing_evidence(bits: u32) -> responses::MissingEvidence {
    responses::MissingEvidence {
        call_result: bits & 1 != 0,
        device_feedback: bits & 2 != 0,
        actual_readback: bits & 4 != 0,
        same_device_comparison: bits & 8 != 0,
        update_route_identity: bits & 16 != 0,
        new_application_start: bits & 32 != 0,
    }
}

fn operation_evidence(bits: u8) -> responses::OperationEvidence {
    responses::OperationEvidence {
        not_started: bits & 1 != 0,
        storage_replace_succeeded: bits & 2 != 0,
        actual_storage_read: bits & 4 != 0,
        effect_may_have_occurred: bits & 8 != 0,
        original_result_missing: bits & 16 != 0,
        settled: bits & 32 != 0,
    }
}

/// `Diagnostics` 的借用视图。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Diagnostics<'a> {
    pub(super) payload: &'a [u8],
}
impl<'a> Diagnostics<'a> {
    /// 共同快照。
    pub fn sample(self) -> Sample<'a> {
        Sample {
            bytes: &self.payload[..32],
        }
    }
    /// 是否还有未逐项列出的诊断。
    pub fn overflow(self) -> bool {
        self.payload[34] != 0
    }
    /// 有界、借用的诊断迭代器；每项通过 [`DiagnosticEntry::fields`] 读取。
    pub fn entries(self) -> DiagnosticEntries<'a> {
        DiagnosticEntries {
            bytes: &self.payload[36..],
            at: 0,
        }
    }
}
/// `Diagnostics` 条目迭代器。
pub struct DiagnosticEntries<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Iterator for DiagnosticEntries<'a> {
    type Item = DiagnosticEntry<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.at == self.bytes.len() {
            None
        } else {
            let item = DiagnosticEntry {
                bytes: &self.bytes[self.at..self.at + 64],
            };
            self.at += 64;
            Some(item)
        }
    }
}
/// 单条详细诊断的字段读取视图。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiagnosticEntry<'a> {
    bytes: &'a [u8],
}
impl<'a> DiagnosticEntry<'a> {
    /// 这条诊断的全部公开字段及其具名类型化解释。
    pub fn fields(self) -> responses::DiagnosticFields {
        responses::DiagnosticFields {
            domain: responses::DiagnosticDomain::from_u8(self.bytes[0])
                .expect("payload was validated"),
            object: responses::DiagnosticObject::from_u8(self.bytes[1])
                .expect("payload was validated"),
            impact: diagnostic_impact(self.bytes[2]),
            phase: responses::OperationPhase::from_u8(self.bytes[3])
                .expect("payload was validated"),
            reason: u16_at(self.bytes, 4),
            next_actions: u16_at(self.bytes, 6),
            event_time_us: u64_at(self.bytes, 8),
            started_time_us: u64_at(self.bytes, 16),
            deadline_us: u64_at(self.bytes, 24),
            operation_id: u64_at(self.bytes, 32),
            native_code: u32_at(self.bytes, 40),
            native_domain: responses::NativeDomain::from_u8(self.bytes[44])
                .expect("payload was validated"),
            evidence: diagnostic_evidence(self.bytes[45]),
            detail_value: decoded_value_fields(f32_at(self.bytes, 48), self.bytes[52]),
            detail_unit: responses::DetailUnit::from_u8(self.bytes[53])
                .expect("payload was validated"),
            constraints: constraints(u16_at(self.bytes, 54).into()),
            occurrences: u32_at(self.bytes, 56),
            missing_evidence: missing_evidence(u32_at(self.bytes, 60)),
        }
    }
}

/// `ConfigData` 的字段读取视图。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConfigData<'a> {
    pub(super) payload: &'a [u8],
}
impl<'a> ConfigData<'a> {
    /// 共同快照。
    pub fn sample(self) -> Sample<'a> {
        Sample {
            bytes: &self.payload[..32],
        }
    }
    /// 固定配置前缀的全部具名类型化字段。
    pub fn fields(self) -> responses::ConfigDataFields {
        responses::ConfigDataFields {
            sample: sample_data(self.payload),
            view: responses::ConfigView::from_u8(self.payload[32]).expect("payload was validated"),
            record_state: responses::ConfigRecordState::from_u8(self.payload[33])
                .expect("payload was validated"),
            startup_source: responses::StartupSource::from_u8(self.payload[34])
                .expect("payload was validated"),
            fallback_reason: responses::FallbackReason::from_u8(self.payload[35])
                .expect("payload was validated"),
            data_time_us: u64_at(self.payload, 40),
        }
    }
    /// 当前通信设置的具名投影。
    ///
    /// 仅当本视图是成功的 `Communication` 配置读取且 `data` 恰为合同规定的
    /// 20 字节时返回；其他配置记录继续通过 [`ConfigData::data`] 原样借用。
    pub fn communication_settings(self) -> Option<responses::CommunicationSettingsFields> {
        let fields = self.fields();
        let data = self.data();
        if fields.view != responses::ConfigView::Communication
            || fields.record_state != responses::ConfigRecordState::Complete
            || data.len() != 20
        {
            return None;
        }
        Some(responses::CommunicationSettingsFields {
            active_can_node: u32_at(data, 0),
            profile: responses::CanProfile::from_u8(data[4])?,
            host_heartbeat_hz: u32_at(data, 8),
            telemetry_hz: u32_at(data, 12),
            host_contact_max_age_ms: u32_at(data, 16),
        })
    }
    /// 借用的实际记录字节；不要求UTF-8或可解析JSON。
    pub fn data(self) -> &'a [u8] {
        &self.payload[48..]
    }
}

/// `OperationResult` 的字段读取视图。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OperationResult<'a> {
    pub(super) payload: &'a [u8],
}
impl<'a> OperationResult<'a> {
    /// 共同快照。
    pub fn sample(self) -> Sample<'a> {
        Sample {
            bytes: &self.payload[..32],
        }
    }
    /// 所有维护进展或结果字段的具名类型化表示。
    pub fn fields(self) -> responses::OperationResultFields {
        responses::OperationResultFields {
            sample: sample_data(self.payload),
            operation_id: u64_at(self.payload, 32),
            operation: responses::MaintenanceOperation::from_u8(self.payload[40])
                .expect("payload was validated"),
            state: responses::MaintenanceState::from_u8(self.payload[41])
                .expect("payload was validated"),
            phase: responses::OperationPhase::from_u8(self.payload[42])
                .expect("payload was validated"),
            evidence: operation_evidence(self.payload[43]),
            reason: u16_at(self.payload, 44),
            next_actions: u16_at(self.payload, 46),
            constraints: constraints(u32_at(self.payload, 48)),
            started_time_us: u64_at(self.payload, 52),
            deadline_us: u64_at(self.payload, 60),
            finished_time_us: u64_at(self.payload, 68),
            content_length: u32_at(self.payload, 76),
            content_crc32c: u32_at(self.payload, 80),
            revision: u32_at(self.payload, 84),
        }
    }
}

/// `DataUnavailable` 的字段读取视图。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DataUnavailable<'a> {
    pub(super) payload: &'a [u8],
}
impl<'a> DataUnavailable<'a> {
    /// 共同快照。
    pub fn sample(self) -> Sample<'a> {
        Sample {
            bytes: &self.payload[..32],
        }
    }
    /// 数据或维护结果不可取得事实的全部具名类型化字段。
    pub fn fields(self) -> responses::DataUnavailableFields {
        responses::DataUnavailableFields {
            sample: sample_data(self.payload),
            subject: responses::UnavailableSubject::from_u8(self.payload[32])
                .expect("payload was validated"),
            reason: u16_at(self.payload, 33),
            next_actions: u16_at(self.payload, 35),
            retained: self.payload[37] != 0,
            operation_id: u64_at(self.payload, 40),
        }
    }
}
