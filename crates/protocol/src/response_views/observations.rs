// Copyright The eha_controller Contributors

use super::common::{Sample, decoded_value_fields, sample_data};
use crate::responses;
use crate::validation::{f32_at, u16_at, u32_at, u64_at};

fn telemetry_facts(bits: u16) -> responses::TelemetryFacts {
    responses::TelemetryFacts {
        output_allowed: bits & 1 != 0,
        reference_limited: bits & 2 != 0,
        candidate_limited: bits & 4 != 0,
        position_direction_inhibited: bits & 8 != 0,
        device_operation_pending: bits & 16 != 0,
        retained_result: bits & 32 != 0,
        unknown_effect: bits & 64 != 0,
    }
}

fn limit_flags(bits: u16) -> responses::LimitFlags {
    responses::LimitFlags {
        position_soft_lower: bits & (1 << 0) != 0,
        position_soft_upper: bits & (1 << 1) != 0,
        position_hard_lower: bits & (1 << 2) != 0,
        position_hard_upper: bits & (1 << 3) != 0,
        velocity_soft: bits & (1 << 4) != 0,
        velocity_hard: bits & (1 << 5) != 0,
        pressure_a_soft: bits & (1 << 6) != 0,
        pressure_a_hard: bits & (1 << 7) != 0,
        pressure_b_soft: bits & (1 << 8) != 0,
        pressure_b_hard: bits & (1 << 9) != 0,
        force_soft: bits & (1 << 10) != 0,
        force_hard: bits & (1 << 11) != 0,
        motor_output_soft: bits & (1 << 12) != 0,
        motor_output_hard: bits & (1 << 13) != 0,
    }
}

fn driver_state(bits: u8) -> responses::DriverState {
    responses::DriverState {
        has_status: bits & 1 != 0,
        qualified: bits & 2 != 0,
        stale: bits & 4 != 0,
        faulted: bits & 8 != 0,
    }
}

/// `Telemetry` 或 `Status` 的具名读取视图。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Telemetry<'a> {
    pub(super) payload: &'a [u8],
}
impl<'a> Telemetry<'a> {
    /// 共同快照。
    pub fn sample(self) -> Sample<'a> {
        Sample {
            bytes: &self.payload[..32],
        }
    }

    /// 所有公开遥测字段的具名类型化表示。
    pub fn fields(self) -> responses::TelemetryFields {
        responses::TelemetryFields {
            sample: sample_data(self.payload),
            decision_age_us: u32_at(self.payload, 32),
            target_mode: responses::TargetMode::from_u8(self.payload[36])
                .expect("payload was validated"),
            target_ingress: responses::TargetIngress::from_u8(self.payload[37])
                .expect("payload was validated"),
            facts: telemetry_facts(u16_at(self.payload, 38)),
            target_values: [
                f32_at(self.payload, 40),
                f32_at(self.payload, 44),
                f32_at(self.payload, 48),
            ],
            can_adoption_blockers: u32_at(self.payload, 52),
            usb_adoption_blockers: u32_at(self.payload, 56),
            output_blockers: u32_at(self.payload, 60),
            last_end_reason: u16_at(self.payload, 64),
            limits: limit_flags(u16_at(self.payload, 66)),
            main_values: [
                decoded_value_fields(f32_at(self.payload, 68), self.payload[88]),
                decoded_value_fields(f32_at(self.payload, 72), self.payload[89]),
                decoded_value_fields(f32_at(self.payload, 76), self.payload[90]),
                decoded_value_fields(f32_at(self.payload, 80), self.payload[91]),
                decoded_value_fields(f32_at(self.payload, 84), self.payload[92]),
            ],
            position_age_us: u32_at(self.payload, 96),
            velocity_age_us: u32_at(self.payload, 100),
            pressure_pair_age_us: u32_at(self.payload, 104),
            reference: decoded_value_fields(f32_at(self.payload, 108), self.payload[113]),
            reference_kind: responses::ReferenceKind::from_u8(self.payload[112])
                .expect("payload was validated"),
            candidate_rpm: decoded_value_fields(f32_at(self.payload, 116), self.payload[114]),
            last_submitted_rpm: decoded_value_fields(f32_at(self.payload, 120), self.payload[115]),
            submitted_age_us: u32_at(self.payload, 124),
            axis_state_raw: u32_at(self.payload, 128),
            axis_error_raw: u32_at(self.payload, 132),
            driver_age_us: u32_at(self.payload, 136),
            driver_state: driver_state(self.payload[140]),
            desired_axis: responses::DesiredAxis::from_u8(self.payload[141])
                .expect("payload was validated"),
            can_heartbeat_age_us: u32_at(self.payload, 142),
            usb_heartbeat_age_us: u32_at(self.payload, 146),
            can_contact: responses::ContactState::from_u8(self.payload[150])
                .expect("payload was validated"),
            usb_contact: responses::ContactState::from_u8(self.payload[151])
                .expect("payload was validated"),
        }
    }
}

/// `Identity` 的字段读取视图。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Identity<'a> {
    pub(super) payload: &'a [u8],
}
impl<'a> Identity<'a> {
    /// 共同快照。
    pub fn sample(self) -> Sample<'a> {
        Sample {
            bytes: &self.payload[..32],
        }
    }

    /// 所有公开身份、适用性和有界文本字段的具名类型化表示。
    pub fn fields(self) -> responses::IdentityFields<'a> {
        let text = responses::IdentityText {
            version: self.string_at(0).expect("payload was validated"),
            source_revision: self.string_at(1).expect("payload was validated"),
            build_information: self.string_at(2).expect("payload was validated"),
            odrive_binding: self.string_at(3).expect("payload was validated"),
            odrive_observed_version: self.string_at(4).expect("payload was validated"),
            brt27_binding: self.string_at(5).expect("payload was validated"),
            brt27_observed_version: self.string_at(6).expect("payload was validated"),
            update_binding: self.string_at(7).expect("payload was validated"),
        };
        let mut uid = [0; 12];
        uid.copy_from_slice(&self.payload[32..44]);
        responses::IdentityFields {
            sample: sample_data(self.payload),
            uid,
            config_source: responses::StartupSource::from_u8(self.payload[44])
                .expect("payload was validated"),
            fallback_reason: responses::FallbackReason::from_u8(self.payload[45])
                .expect("payload was validated"),
            run_identity: responses::RunIdentity::from_u8(self.payload[46])
                .expect("payload was validated"),
            update_route: responses::UpdateRoute::from_u8(self.payload[47])
                .expect("payload was validated"),
            active_can_node: u32_at(self.payload, 48),
            active_can_profile: responses::CanProfile::from_u8(self.payload[52])
                .expect("payload was validated"),
            host_heartbeat_hz: u32_at(self.payload, 56),
            telemetry_hz: u32_at(self.payload, 60),
            host_contact_max_age_ms: u32_at(self.payload, 64),
            config_format_version: u32_at(self.payload, 68),
            build_evidence: responses::EvidenceState::from_u8(self.payload[72])
                .expect("payload was validated"),
            odrive_evidence: responses::EvidenceState::from_u8(self.payload[73])
                .expect("payload was validated"),
            brt27_evidence: responses::EvidenceState::from_u8(self.payload[74])
                .expect("payload was validated"),
            high_water_operation_id: u64_at(self.payload, 76),
            retained_operation_id: u64_at(self.payload, 84),
            text,
        }
    }
    fn string_at(self, index: usize) -> Option<&'a str> {
        let mut at = 92;
        for i in 0..8 {
            let n = usize::from(self.payload[at]);
            at += 1;
            if i == index {
                return core::str::from_utf8(&self.payload[at..at + n]).ok();
            }
            at += n;
        }
        None
    }
}

/// `Measurements` 的字段读取视图。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurements<'a> {
    pub(super) payload: &'a [u8],
}
impl<'a> Measurements<'a> {
    /// 共同快照。
    pub fn sample(self) -> Sample<'a> {
        Sample {
            bytes: &self.payload[..32],
        }
    }

    /// 所有详细测量字段的具名类型化表示。
    pub fn fields(self) -> responses::MeasurementFields {
        let value = |index: usize| {
            decoded_value_fields(
                f32_at(self.payload, 32 + index * 4),
                self.payload[64 + index],
            )
        };
        responses::MeasurementFields {
            sample: sample_data(self.payload),
            values: responses::MeasurementValues {
                position_mm: value(0),
                velocity_mm_s: value(1),
                raw_pressure_a_mpa: value(2),
                raw_pressure_b_mpa: value(3),
                filtered_pressure_a_mpa: value(4),
                filtered_pressure_b_mpa: value(5),
                main_force_n: value(6),
                protection_force_n: value(7),
            },
            position_age_us: u32_at(self.payload, 72),
            velocity_age_us: u32_at(self.payload, 76),
            pressure_pair_age_us: u32_at(self.payload, 80),
            position_reference_count: u32_at(self.payload, 84),
            effective_area_mm2: f32_at(self.payload, 88),
            positive_force_channel: responses::PositiveForceChannel::from_u8(self.payload[92])
                .expect("payload was validated"),
            reference_state: self.payload[93] != 0,
            model_state: responses::ModelState::from_u8(self.payload[94])
                .expect("payload was validated"),
            pressure_batch_sequence: u32_at(self.payload, 96),
        }
    }
}
