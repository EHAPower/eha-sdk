// Copyright The eha_controller Contributors

use super::*;

/// 正力方向对应的电气压力通道。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PositiveForceChannel {
    /// 当前不可取得。
    Unavailable = 0,
    /// 电气 A 通道。
    ElectricalA = 1,
    /// 电气 B 通道。
    ElectricalB = 2,
}

/// 等面积模型状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ModelState {
    /// 模型不可取得。
    Unavailable = 0,
    /// 本次模型可用。
    Available = 1,
    /// 模型不适用。
    NotApplicable = 2,
}

/// 详细测量的八个具名数值。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasurementValues {
    /// 位置，mm。
    pub position_mm: ValueFields,
    /// 线速度，mm/s。
    pub velocity_mm_s: ValueFields,
    /// 原换算压力 A，MPa。
    pub raw_pressure_a_mpa: ValueFields,
    /// 原换算压力 B，MPa。
    pub raw_pressure_b_mpa: ValueFields,
    /// 滤波压力 A，MPa。
    pub filtered_pressure_a_mpa: ValueFields,
    /// 滤波压力 B，MPa。
    pub filtered_pressure_b_mpa: ValueFields,
    /// 基于滤波压力对的主要估算力，N。
    pub main_force_n: ValueFields,
    /// 基于原换算压力对的保护估算力，N。
    pub protection_force_n: ValueFields,
}

/// 详细测量的完整具名字段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasurementFields {
    /// 共同回复快照。
    pub sample: SampleData,
    /// 八个具名观测值。
    pub values: MeasurementValues,
    /// 位置来源年龄。
    pub position_age_us: u32,
    /// 速度来源年龄。
    pub velocity_age_us: u32,
    /// 完整压力批次年龄。
    pub pressure_pair_age_us: u32,
    /// 位置参考计数。
    pub position_reference_count: u32,
    /// 有效面积，平方毫米。
    pub effective_area_mm2: f32,
    /// 正力方向的电气通道。
    pub positive_force_channel: PositiveForceChannel,
    /// 本次位置参考是否可用。
    pub reference_state: bool,
    /// 等面积模型状态。
    pub model_state: ModelState,
    /// 完整压力采集批次序号。
    pub pressure_batch_sequence: u32,
}

impl MeasurementFields {
    /// `Measurements` 的完整线上长度。
    pub const fn encoded_len() -> usize {
        108
    }
}

/// 编码详细测量回复。
pub fn encode_measurements(fields: &MeasurementFields, output: &mut [u8]) -> Result<usize, Error> {
    let p = payload(output, MeasurementFields::encoded_len() - 8)?;
    write_sample(p, fields.sample);
    let values = [
        fields.values.position_mm,
        fields.values.velocity_mm_s,
        fields.values.raw_pressure_a_mpa,
        fields.values.raw_pressure_b_mpa,
        fields.values.filtered_pressure_a_mpa,
        fields.values.filtered_pressure_b_mpa,
        fields.values.main_force_n,
        fields.values.protection_force_n,
    ];
    for (index, value) in values.iter().enumerate() {
        put_value(p, 32 + index * 4, 64 + index, *value);
    }
    put_u32(p, 72, fields.position_age_us);
    put_u32(p, 76, fields.velocity_age_us);
    put_u32(p, 80, fields.pressure_pair_age_us);
    put_u32(p, 84, fields.position_reference_count);
    put_f32(p, 88, fields.effective_area_mm2);
    p[92] = fields.positive_force_channel as u8;
    p[93] = fields.reference_state as u8;
    p[94] = fields.model_state as u8;
    p[95] = 0;
    put_u32(p, 96, fields.pressure_batch_sequence);
    encode_in_place(
        MessageKind::Measurements,
        MeasurementFields::encoded_len() - 8,
        output,
    )
}
