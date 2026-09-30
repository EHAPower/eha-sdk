// Copyright The eha_controller Contributors

//! 完整配置候选的离线校验，不访问设备或装配运行参数。

use core::fmt;

use config::Config;
use serde_json::Value;

/// 配置校验失败所在的阶段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationStage {
    /// 完整记录超过固件接受的长度。
    Length,
    /// 内嵌 schema 无法装配或候选不符合它。
    Schema,
    /// 候选不是有效 JSON。
    Json,
    /// 固件配置类型无法表示候选，或表示后的候选不再符合 schema。
    FirmwareRepresentation,
    /// 跨字段关系不成立。
    FieldRelationship,
    /// 派生控制参数无法表示。
    DerivedParameter,
    /// 名义心跳周期不满足联系年龄。
    HeartbeatTiming,
}

/// 完整配置候选的可归因校验错误。
///
/// `path` 是关联 JSON Pointer；没有单一字段时为 `None`。`message` 保留可直接呈现的
/// 中文诊断，`stage` 供调用方按错误来源处理。显示文本与旧工具的本地检查诊断一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationError {
    /// 失败的 JSON Pointer，若无单一字段则为 `None`。
    pub path: Option<String>,
    /// 可直接呈现的诊断文本。
    pub message: String,
    /// 失败发生的校验阶段。
    pub stage: ValidationStage,
}

impl fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigurationError {}

fn error(
    stage: ValidationStage,
    path: Option<impl Into<String>>,
    message: impl Into<String>,
) -> ConfigurationError {
    ConfigurationError {
        path: path.map(Into::into),
        message: message.into(),
        stage,
    }
}

/// 当前配置的标准 JSON Schema；桌面 SDK 从 `config` 取得，固件不引用此常量。
const SCHEMA_JSON: &str = config::SCHEMA_JSON;

/// 校验完整主机候选，返回固件实际可表示的配置；不补齐缺失参数、不访问设备。
///
/// 先按 schema 检查原始 JSON，再使用固件解码器确认表示，并对转换后的值执行同一
/// schema、标定和保护范围关系以及控制参数派生检查。浮点舍入为零、端点合并或派生转速
/// 无法表示时不会通过。此结果不证明设备适用性或目标固件匹配，也不在固件的保存或启动
/// 路径调用。
pub fn validate_json(json: &[u8]) -> Result<Config, ConfigurationError> {
    if json.len() > config::MAX_JSON_LEN {
        return Err(error(
            ValidationStage::Length,
            None::<String>,
            format!("配置超过 {} 字节", config::MAX_JSON_LEN),
        ));
    }
    let schema = schema()?;
    let validator = jsonschema::validator_for(&schema).map_err(|source| {
        error(
            ValidationStage::Schema,
            None::<String>,
            format!("内置 schema 无效：{source}"),
        )
    })?;
    let input: Value = serde_json::from_slice(json).map_err(|source| {
        error(
            ValidationStage::Json,
            None::<String>,
            format!("JSON 格式错误：{source}"),
        )
    })?;
    validator.validate(&input).map_err(|source| {
        let path = source.instance_path().to_string();
        error(
            ValidationStage::Schema,
            Some(path.clone()),
            format!("{path}：{source}"),
        )
    })?;
    let config = Config::from_json(json).map_err(|source| {
        error(
            ValidationStage::FirmwareRepresentation,
            None::<String>,
            format!("固件无法表示此配置：{source:?}"),
        )
    })?;
    let mut represented = input;
    represented["config"] = serde_json::to_value(config).map_err(|source| {
        error(
            ValidationStage::FirmwareRepresentation,
            Some("/config"),
            format!("固件配置表示失败：{source}"),
        )
    })?;
    validator.validate(&represented).map_err(|source| {
        let path = source.instance_path().to_string();
        error(
            ValidationStage::FirmwareRepresentation,
            Some(path.clone()),
            format!("固件数值转换后 {path}：{source}"),
        )
    })?;

    // 标准 minimum / maximum 不支持引用同一实例中的另一个数值字段。
    // 主机补充这些关系，不扩展 schema 词汇。
    for (channel, calibration) in [
        ("a", config.measurements.pressure.a),
        ("b", config.measurements.pressure.b),
    ] {
        if calibration.raw_min >= calibration.raw_max {
            let path = format!("/config/measurements/pressure/{channel}/raw_max");
            return Err(error(
                ValidationStage::FieldRelationship,
                Some(path.clone()),
                format!("{path}：必须大于同通道 raw_min"),
            ));
        }
        if calibration.min_mpa >= calibration.max_mpa {
            let path = format!("/config/measurements/pressure/{channel}/max_mpa");
            return Err(error(
                ValidationStage::FieldRelationship,
                Some(path.clone()),
                format!("{path}：必须大于同通道 min_mpa（固件表示后）"),
            ));
        }
    }
    validate_protection(&config)?;
    // 配置接受范围保持 binary32 的既有表示约束；检查未与电机软上限取小前的派生 rpm。
    // 只复用液压换算，不装配运行参数；计算结果保持 f64，缩窄值仅用于此边界检查。
    let derived_rpm = config::hydraulic_velocity_to_rpm(
        f64::from(config.protection.soft_velocity_max_mm_s),
        f64::from(config.control.hydraulics.effective_area_mm2),
        f64::from(config.control.hydraulics.pump_displacement_cm3_rev),
    )
    .map_err(|source| {
        error(
            ValidationStage::DerivedParameter,
            Some("/config/control"),
            format!("/config/control：派生控制参数无法表示：{source:?}"),
        )
    })?;
    let represented_rpm = derived_rpm as f32;
    if !represented_rpm.is_finite() || (derived_rpm != 0.0 && represented_rpm == 0.0) {
        return Err(error(
            ValidationStage::DerivedParameter,
            Some("/config/control"),
            "/config/control：派生控制参数无法表示：NotRepresentable",
        ));
    }
    // 只排除名义心跳周期已超过联系年龄的组合，不据此证明实际收发、抖动或链路容量。
    if u64::from(config.runtime.host_heartbeat_hz)
        * u64::from(config.runtime.host_contact_max_age_ms)
        < 1000
    {
        return Err(error(
            ValidationStage::HeartbeatTiming,
            Some("/config/runtime/host_heartbeat_hz"),
            "/config/runtime/host_heartbeat_hz：名义心跳周期不能超过 host_contact_max_age_ms",
        ));
    }
    Ok(config)
}

fn validate_protection(config: &Config) -> Result<(), ConfigurationError> {
    let p = &config.protection;
    if !(p.hard_position_min_mm <= p.soft_position_min_mm
        && p.soft_position_min_mm < p.soft_position_max_mm
        && p.soft_position_max_mm <= p.hard_position_max_mm)
    {
        return Err(error(
            ValidationStage::FieldRelationship,
            Some("/config/protection"),
            "/config/protection：position 必须满足 hard_min ≤ soft_min < soft_max ≤ hard_max（固件表示后）",
        ));
    }
    // 正幅值由 schema 检查，这里只核对表示后的软硬关系。
    for (quantity, soft_max, hard_max) in [
        (
            "velocity",
            p.soft_velocity_max_mm_s,
            p.hard_velocity_max_mm_s,
        ),
        ("pressure", p.soft_pressure_max_mpa, p.hard_pressure_max_mpa),
        ("force", p.soft_force_max_n, p.hard_force_max_n),
        (
            "motor_speed",
            p.soft_motor_speed_max_rpm,
            p.hard_motor_speed_max_rpm,
        ),
    ] {
        if soft_max > hard_max {
            return Err(error(
                ValidationStage::FieldRelationship,
                Some("/config/protection"),
                format!(
                    "/config/protection：{quantity} 必须满足 soft_max ≤ hard_max（固件表示后）"
                ),
            ));
        }
    }
    Ok(())
}

fn schema() -> Result<Value, ConfigurationError> {
    serde_json::from_str(SCHEMA_JSON).map_err(|source| {
        error(
            ValidationStage::Schema,
            None::<String>,
            format!("内置 schema 格式错误：{source}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn candidate() -> Value {
        serde_json::from_slice(include_bytes!("../tests/fixtures/config.json"))
            .expect("合法的人工配置测试记录")
    }

    fn check(value: &Value) -> Result<Config, ConfigurationError> {
        validate_json(&serde_json::to_vec(value).expect("JSON candidate"))
    }

    #[test]
    fn host_rejects_schema_violation_that_firmware_can_parse() {
        let mut value = candidate();
        assert!(check(&value).is_ok());
        value["config"]["external_can"]["id"] = json!(128);
        let bytes = serde_json::to_vec(&value).expect("JSON candidate");
        assert!(Config::from_json(&bytes).is_ok());
        let error = validate_json(&bytes).expect_err("host owns the CAN node range");
        assert_eq!(error.path.as_deref(), Some("/config/external_can/id"));
        assert_eq!(error.stage, ValidationStage::Schema);
        assert!(error.to_string().contains("/config/external_can/id"));
    }

    #[test]
    fn checks_ranges_after_conversion_to_firmware_numbers() {
        let mut value = candidate();
        value["config"]["measurements"]["position"]["mm_per_count"] = json!(1e-50);
        let error = check(&value).expect_err("positive JSON value becomes zero in f32");
        assert!(error.to_string().contains("mm_per_count"));
    }

    #[test]
    fn checks_pressure_relations_on_each_channel_after_rounding() {
        for channel in ["a", "b"] {
            let mut value = candidate();
            value["config"]["measurements"]["pressure"][channel]["min_mpa"] = json!(-1.0);
            value["config"]["measurements"]["pressure"][channel]["max_mpa"] = json!(1.0);
            assert!(check(&value).is_ok(), "negative pressures remain valid");
            let raw_min = value["config"]["measurements"]["pressure"][channel]["raw_min"].clone();
            value["config"]["measurements"]["pressure"][channel]["raw_max"] = raw_min;
            let error = check(&value).expect_err("相等的原始端点必须由字段关系拒绝");
            let path = format!("/config/measurements/pressure/{channel}/raw_max");
            assert_eq!(error.path.as_deref(), Some(path.as_str()));
            assert!(error.to_string().contains("必须大于同通道 raw_min"));
            let mut value = candidate();
            value["config"]["measurements"]["pressure"][channel]["min_mpa"] = json!(1.00000001);
            value["config"]["measurements"]["pressure"][channel]["max_mpa"] = json!(1.00000002);
            let error = check(&value).expect_err("distinct JSON endpoints collapse to one f32");
            assert!(
                error
                    .to_string()
                    .contains(&format!("pressure/{channel}/max_mpa"))
            );
        }
    }

    #[test]
    fn checks_control_limits_after_hydraulic_conversion() {
        for (area, displacement) in [(1e30, 1e-30), (1e-30, 1e30)] {
            let mut value = candidate();
            value["config"]["control"]["hydraulics"]["effective_area_mm2"] = json!(area);
            value["config"]["control"]["hydraulics"]["pump_displacement_cm3_rev"] =
                json!(displacement);
            let error = check(&value).expect_err("derived rpm must remain representable");
            assert_eq!(error.stage, ValidationStage::DerivedParameter);
            assert!(error.to_string().contains("/config/control"));
            assert!(error.to_string().contains("NotRepresentable"));
        }
    }

    #[test]
    fn checks_protection_relations_after_firmware_rounding() {
        for (field, hard_max, quantity) in [
            ("soft_velocity_max_mm_s", 30.0, "velocity"),
            ("soft_pressure_max_mpa", 10.0, "pressure"),
        ] {
            let mut value = candidate();
            assert!(check(&value).is_ok());
            value["config"]["protection"][field] = json!(hard_max);
            assert!(check(&value).is_ok(), "{quantity} 软硬幅值可以相等");
            value["config"]["protection"][field] = json!(hard_max + 1.0);
            let bytes = serde_json::to_vec(&value).expect("完整候选");
            assert!(Config::from_json(&bytes).is_ok(), "固件仅解析记录");
            let error = check(&value).expect_err("软幅值不能超过自身硬幅值");
            assert!(error.to_string().contains(quantity), "{quantity}: {error}");
        }
        let mut value = candidate();
        value["config"]["protection"]["soft_position_min_mm"] = json!(1.00000001);
        value["config"]["protection"]["soft_position_max_mm"] = json!(1.00000002);
        let error = check(&value).expect_err("不同端点舍入后重合");
        assert!(error.to_string().contains("position"));
    }

    #[test]
    fn checks_heartbeat_period_against_contact_age_without_guessing_bus_capacity() {
        let mut value = candidate();
        value["config"]["runtime"]["host_heartbeat_hz"] = json!(1);
        value["config"]["runtime"]["host_contact_max_age_ms"] = json!(1000);
        assert!(check(&value).is_ok(), "等于联系年龄边界时仍有效");
        value["config"]["runtime"]["host_contact_max_age_ms"] = json!(999);
        let error = check(&value).expect_err("名义周期已超过联系年龄");
        assert_eq!(error.stage, ValidationStage::HeartbeatTiming);
        assert!(error.to_string().contains("host_heartbeat_hz"));
        assert!(error.to_string().contains("名义心跳周期"));
    }
}
