// Copyright The eha-sdk Contributors

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! EHA 配置的共享字段、JSON 编解码与静态换算。
//!
//! [`Config`] 在固定缓冲中编解码完整 JSON 记录。它不访问存储，不选择启动配置，不处理
//! JSONC 主机文件，不执行业务参数校验，也不决定维护许可和复位。

/// 当前配置记录的标准 JSON Schema；仅主机校验使用，固件不引用此常量。
#[cfg(feature = "schema")]
pub const SCHEMA_JSON: &str = include_str!("../schema.json");

mod control;
mod measurement;
mod numeric;
mod policy;

pub use control::{
    ControlConfig, HydraulicConfig, HydraulicConversionError, MotorDirection, PositionPidConfig,
    PositiveForcePressureChannel, PressurePidConfig, VelocityPidConfig, hydraulic_velocity_to_rpm,
};
pub use measurement::{
    MeasurementConfig, PositionConfig, PositionDirection, PressureCalibration, PressureConfig,
};
pub use policy::{ProtectionConfig, RuntimeConfig};

pub use numeric::{FloatParseError, parse_f32, validate_f32_tokens, within_tolerance};

use serde::{Deserialize, Serialize};

/// 当前字段版本的一份完整配置。
///
/// 字段按业务需要扩展，使用有界、自有数据，不借用读取缓冲，也不包含内部可变状态。
/// 反序列化拒绝未知或缺失参数，不执行业务校验、默认补全或迁移。持久 JSON 通过
/// [`Self::from_json`] 和 [`Self::write_json`] 读写，运行期使用方直接读取本类型
/// 或所属分组的只读引用。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 上层主控接入总线的启动参数。
    pub external_can: ExternalCanConfig,
    /// 传感器原始反馈的换算和处理参数。
    pub measurements: MeasurementConfig,
    /// 位置、速度、力与阻抗计算的固定参数。
    pub control: ControlConfig,
    /// 软硬限位和独立电机转速保护范围。
    pub protection: ProtectionConfig,
    /// 通信、测量和设备运行时效。
    pub runtime: RuntimeConfig,
}

/// 外部 CAN 参数，由目标装配转换为平台输入，供总线构造使用。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCanConfig {
    /// 外部 CAN 标识配置值；发送标识和接收匹配的编码与消息映射由通信合同规定。
    pub id: u32,
    /// 模式和位速率的完整组合；本次启动后保持固定。
    pub profile: ExternalCanProfile,
}

/// 外部 CAN 允许的模式与速率组合，与平台设计的通信配置对应。
///
/// 经典 CAN 使用单一位速率；FD 分别指定仲裁段和数据段速率。
/// 选择组合不证明总线负载、通信时效或其他节点与本配置匹配。
/// JSON 使用明确的字符串名称，枚举声明顺序不参与持久表示。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ExternalCanProfile {
    /// 经典 CAN，500 kbit/s。
    #[serde(rename = "classical_500k")]
    Classical500K,
    /// 经典 CAN，1 Mbit/s。
    #[serde(rename = "classical_1m")]
    Classical1M,
    /// CAN FD，仲裁段 500 kbit/s，数据段 2 Mbit/s。
    #[serde(rename = "fd_500k_2m")]
    Fd500K2M,
    /// CAN FD，仲裁段和数据段均为 500 kbit/s。
    #[serde(rename = "fd_500k_500k")]
    Fd500K500K,
    /// CAN FD，仲裁段 1 Mbit/s，数据段 2 Mbit/s。
    #[serde(rename = "fd_1m_2m")]
    Fd1M2M,
    /// CAN FD，仲裁段 1 Mbit/s，数据段 5 Mbit/s。
    #[serde(rename = "fd_1m_5m")]
    Fd1M5M,
    /// CAN FD，仲裁段 1 Mbit/s，数据段 8 Mbit/s。
    #[serde(rename = "fd_1m_8m")]
    Fd1M8M,
}

/// 一份持久 JSON 的最大字节数，也是用户区读取前缀和字符串解码缓冲的上限。
///
/// 此上限不代表物理 Flash 分区大小；超长候选必须在写入前被拒绝。
pub const MAX_JSON_LEN: usize = 16 * 1024;

/// 当前配置记录的格式版本，供调用方报告所支持的配置格式。
///
/// [`Config::from_json`] 只接受此版本，[`Config::write_json`] 使用此版本编码。
/// 开发期间字段扩展不自动升级版本；版本不证明目标固件或设备匹配。
pub const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document<T> {
    format_version: u32,
    config: T,
}

/// JSON 的长度、版本与格式错误，不包含业务参数适用性判断。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonError {
    /// 用户区没有 JSON 内容。
    Empty,
    /// JSON 超过容量，或编码输出缓冲为空。
    InvalidLength,
    /// 数值参数不能表示为有限 f32；包括 null 和解析后溢出的指数。
    NonFiniteNumber,
    /// 原文非零数值舍入为零；偏移用于定位对应 JSON token。
    NumberUnderflow {
        /// 数值 token 在输入中的首字节偏移。
        offset: usize,
    },
    /// JSON 声明的版本不受当前配置类型支持。
    UnsupportedVersion {
        /// JSON 文档声明的版本。
        stored: u32,
        /// 当前配置类型接受的版本。
        expected: u32,
    },
    /// 编码失败，例如调用方的缓冲不足。
    Encode(serde_json_core::ser::Error),
    /// JSON 语法、字段、类型或完整性不符合要求。
    Decode(serde_json_core::de::Error),
}

impl Config {
    /// 完整解析持久 JSON 文档，返回不借用读取或字符串解码缓冲的配置值。
    ///
    /// 此方法不访问存储，不执行参数业务校验，不从出厂值补齐字段。
    pub fn from_json(json: &[u8]) -> Result<Self, JsonError> {
        if json.is_empty() {
            return Err(JsonError::Empty);
        }
        if json.len() > MAX_JSON_LEN {
            return Err(JsonError::InvalidLength);
        }
        let mut scratch = [0; MAX_JSON_LEN];
        // from_slice_escaped 在解析后检查整个输入已消费，只允许尾部 JSON 空白。
        let (document, _) =
            serde_json_core::from_slice_escaped::<Document<Self>>(json, &mut scratch)
                .map_err(JsonError::Decode)?;
        if document.format_version != FORMAT_VERSION {
            return Err(JsonError::UnsupportedVersion {
                stored: document.format_version,
                expected: FORMAT_VERSION,
            });
        }
        // serde-json-core 将 null 解作 NaN，并可把大指数解作 infinity；
        // 参数字段需要有限 JSON 数字，不能据此接受伪装成数值的完整记录。
        if !document.config.measurements.numbers_are_finite()
            || !document.config.control.numbers_are_finite()
            || !document.config.protection.numbers_are_finite()
        {
            return Err(JsonError::NonFiniteNumber);
        }
        validate_f32_tokens(json)?;
        Ok(document.config)
    }

    /// 编码完整配置及格式版本，返回可写入 Flash 的紧凑 JSON 前缀长度。
    ///
    /// 输出缓冲更大时仍遵守 [`MAX_JSON_LEN`]；失败时缓冲可能部分改变，不得保存它。
    /// 非有限数值返回 [`JsonError::NonFiniteNumber`]，不让编码器将其静默输出为 null。
    pub fn write_json(&self, output: &mut [u8]) -> Result<usize, JsonError> {
        let capacity = output.len().min(MAX_JSON_LEN);
        if capacity == 0 {
            return Err(JsonError::InvalidLength);
        }
        if !self.measurements.numbers_are_finite()
            || !self.control.numbers_are_finite()
            || !self.protection.numbers_are_finite()
        {
            return Err(JsonError::NonFiniteNumber);
        }
        let document = Document {
            format_version: FORMAT_VERSION,
            config: self,
        };
        serde_json_core::to_slice(&document, &mut output[..capacity]).map_err(JsonError::Encode)
    }
}

// 独立合成记录，只用于公共 JSON 表示语义，不读取或复制产品出厂参数。
#[cfg(test)]
const TEST_JSON: &[u8] =
    br#"{"format_version":1,"config":{"external_can":{"id":1,"profile":"fd_1m_5m"},"measurements":{"position":{"reference_count":100,"mm_per_count":0.5,"direction":"increasing","velocity_estimator_window_ms":4,"velocity_filter_time_constant_ms":1,"max_gap_ms":2},"pressure":{"a":{"raw_min":100,"raw_max":200,"min_mpa":0.0,"max_mpa":12.0},"b":{"raw_min":50,"raw_max":200,"min_mpa":0.0,"max_mpa":12.0},"filter_time_constant_ms":3,"max_gap_ms":2}},"control":{"position":{"kp_rpm_per_mm":3.0,"ki_rpm_per_mm_s":0.0,"kd_rpm_s_per_mm":0.0},"velocity":{"kp_rpm_per_mm_s":2.0,"ki_rpm_per_mm":0.0,"kd_rpm_s2_per_mm":0.0},"pressure":{"kp_rpm_per_mpa":10.0,"ki_rpm_per_mpa_s":0.0,"kd_rpm_s_per_mpa":0.0},"hydraulics":{"effective_area_mm2":100.0,"pump_displacement_cm3_rev":0.6},"motor_direction":"negative","positive_force_pressure_channel":"b"},"protection":{"soft_position_min_mm":-1.0,"soft_position_max_mm":1.0,"hard_position_min_mm":-2.0,"hard_position_max_mm":2.0,"soft_velocity_max_mm_s":1.0,"hard_velocity_max_mm_s":2.0,"soft_pressure_max_mpa":1.0,"hard_pressure_max_mpa":2.0,"soft_force_max_n":1.0,"hard_force_max_n":2.0,"soft_motor_speed_max_rpm":1.0,"hard_motor_speed_max_rpm":2.0},"runtime":{"host_heartbeat_hz":1,"telemetry_hz":1,"host_contact_max_age_ms":1,"position_max_age_ms":1,"pressure_pair_max_age_ms":1,"driver_state_max_age_ms":1,"driver_operation_timeout_ms":1}}}"#;

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn rejects_nonzero_underflow_even_for_zero_allowed_gain() {
        let json = core::str::from_utf8(TEST_JSON)
            .expect("合法测试输入")
            .replace(r#""ki_rpm_per_mm_s":0.0"#, r#""ki_rpm_per_mm_s":1e-100"#);
        assert!(Config::from_json(json.as_bytes()).is_err());
    }

    #[test]
    fn parses_decimal_midpoint_directly_to_binary32() {
        let json = core::str::from_utf8(TEST_JSON)
            .expect("合法测试输入")
            .replace("0.5", "1.000000059604644775390625000001");
        let config = Config::from_json(json.as_bytes()).expect("合法测试输入");
        assert_eq!(
            config.measurements.position.mm_per_count.to_bits(),
            0x3f800001
        );
    }

    #[test]
    fn rejects_nonfinite_json_numbers_before_storage_or_encoding() {
        let template = core::str::from_utf8(TEST_JSON).expect("test JSON");
        // 依赖默认会把 null 变成 NaN、大指数变成 infinity，不能保存为数值参数。
        for number in ["null", "1e999"] {
            let json = template.replace("0.5", number);
            assert_eq!(
                Config::from_json(json.as_bytes()),
                Err(JsonError::NonFiniteNumber)
            );
        }
        let mut config = Config::from_json(TEST_JSON).expect("finite parameters");
        config.measurements.pressure.a.max_mpa = f32::INFINITY;
        let mut output = [0; MAX_JSON_LEN];
        assert_eq!(
            config.write_json(&mut output),
            Err(JsonError::NonFiniteNumber)
        );
        let mut config = Config::from_json(TEST_JSON).expect("finite control parameters");
        config.control.hydraulics.effective_area_mm2 = f32::NAN;
        assert_eq!(
            config.write_json(&mut output),
            Err(JsonError::NonFiniteNumber)
        );
        let mut config = Config::from_json(TEST_JSON).expect("有限保护参数");
        config.protection.hard_motor_speed_max_rpm = f32::INFINITY;
        assert_eq!(
            config.write_json(&mut output),
            Err(JsonError::NonFiniteNumber)
        );
        let control_infinity =
            template.replace(r#""kp_rpm_per_mm":3.0"#, r#""kp_rpm_per_mm":1e999"#);
        assert_eq!(
            Config::from_json(control_infinity.as_bytes()),
            Err(JsonError::NonFiniteNumber)
        );
        let protection_infinity =
            template.replace(r#""soft_force_max_n":1.0"#, r#""soft_force_max_n":1e999"#);
        assert_eq!(
            Config::from_json(protection_infinity.as_bytes()),
            Err(JsonError::NonFiniteNumber)
        );
        // 有限但保护关系非法的记录仍可完整加载和保存；记录层不做业务检查。
        let json = template.replace(
            r#""soft_position_min_mm":-1.0"#,
            r#""soft_position_min_mm":10.0"#,
        );
        assert!(Config::from_json(json.as_bytes()).is_ok());
    }

    #[test]
    fn rejects_unsupported_or_oversized_record() {
        let unsupported = core::str::from_utf8(TEST_JSON)
            .expect("test JSON")
            .replace(r#""format_version":1"#, r#""format_version":2"#);
        assert!(matches!(
            Config::from_json(unsupported.as_bytes()),
            Err(JsonError::UnsupportedVersion { .. })
        ));
        let mut oversized = [b' '; MAX_JSON_LEN + 1];
        oversized[..TEST_JSON.len()].copy_from_slice(TEST_JSON);
        assert_eq!(Config::from_json(&oversized), Err(JsonError::InvalidLength));
    }
}
