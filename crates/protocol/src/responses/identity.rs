// Copyright The eha-sdk Contributors

use super::*;

/// 本次启动配置来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum StartupSource {
    /// 来源不可取得。
    Unavailable = 0,
    /// 用户记录。
    User = 1,
    /// 出厂记录。
    Factory = 2,
}

/// 启动回退原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FallbackReason {
    /// 没有回退。
    None = 0,
    /// 用户记录为空。
    UserEmpty = 1,
    /// 读取失败。
    ReadFailed = 2,
    /// 记录残缺或解析失败。
    ParseFailed = 3,
    /// 配置格式版本不支持。
    FormatUnsupported = 4,
    /// 数值表示失败。
    NumericRepresentationFailed = 5,
    /// 来源不可取得。
    SourceUnavailable = 6,
}

/// 运行实例随机来源状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RunIdentity {
    /// 随机运行实例不可取得。
    Unavailable = 0,
    /// 随机运行实例已取得。
    Random = 1,
}

/// 应用更新路由状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum UpdateRoute {
    /// 没有应用切换路由。
    Absent = 0,
    /// 存在适用路由。
    Present = 1,
    /// 无法核对路由。
    Unverifiable = 2,
}

/// 当前 CAN 配置组。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CanProfile {
    /// classic 500 kbit/s。
    Classical500k = 0,
    /// classic 1 Mbit/s。
    Classical1m = 1,
    /// FD arbitration 500 kbit/s、data 2 Mbit/s。
    Fd500k2m = 2,
    /// FD arbitration/data 均为500 kbit/s。
    Fd500k500k = 3,
    /// FD arbitration 1 Mbit/s、data 2 Mbit/s。
    Fd1m2m = 4,
    /// FD arbitration 1 Mbit/s、data 5 Mbit/s。
    Fd1m5m = 5,
    /// FD arbitration 1 Mbit/s、data 8 Mbit/s。
    Fd1m8m = 6,
}

/// 构建或设备版本证据状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EvidenceState {
    /// 没有取得证据。
    Unavailable = 0,
    /// 取得实际或构建元数据证据。
    Available = 1,
    /// 实际探测失败。
    Failed = 2,
}

/// Identity 的八个有界文本字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityText<'a> {
    /// 当前业务固件版本。
    pub version: &'a str,
    /// 完整来源修订。
    pub source_revision: &'a str,
    /// 构建或工作区信息。
    pub build_information: &'a str,
    /// ODrive 软件接入适用说明。
    pub odrive_binding: &'a str,
    /// 实际探测到的 ODrive 版本。
    pub odrive_observed_version: &'a str,
    /// BRT27 软件接入适用说明。
    pub brt27_binding: &'a str,
    /// 实际探测到的 BRT27 版本。
    pub brt27_observed_version: &'a str,
    /// 更新路由及身份核对说明。
    pub update_binding: &'a str,
}

/// Identity 的完整具名字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityFields<'a> {
    /// 共同回复快照。
    pub sample: SampleData,
    /// 固定控制器 UID。
    pub uid: [u8; 12],
    /// 本次启动的配置来源。
    pub config_source: StartupSource,
    /// 配置回退原因。
    pub fallback_reason: FallbackReason,
    /// 运行实例来源状态。
    pub run_identity: RunIdentity,
    /// 更新路由状态。
    pub update_route: UpdateRoute,
    /// 本次实际 CAN 节点号。
    pub active_can_node: u32,
    /// 本次实际 CAN 配置组。
    pub active_can_profile: CanProfile,
    /// 实际主机心跳频率。
    pub host_heartbeat_hz: u32,
    /// 实际遥测频率。
    pub telemetry_hz: u32,
    /// 实际主机联系最大年龄。
    pub host_contact_max_age_ms: u32,
    /// 配置所有者支持的格式版本。
    pub config_format_version: u32,
    /// 构建元数据证据。
    pub build_evidence: EvidenceState,
    /// ODrive 实际版本证据。
    pub odrive_evidence: EvidenceState,
    /// BRT27 实际版本证据。
    pub brt27_evidence: EvidenceState,
    /// 已处理维护标识上界。
    pub high_water_operation_id: u64,
    /// 当前留存维护标识。
    pub retained_operation_id: u64,
    /// 八个有界文本字段。
    pub text: IdentityText<'a>,
}

impl IdentityFields<'_> {
    /// 计算本身份回复的完整线上长度，供调用方预检发送缓冲。
    pub fn encoded_len(&self) -> Result<usize, Error> {
        let strings = [
            self.text.version,
            self.text.source_revision,
            self.text.build_information,
            self.text.odrive_binding,
            self.text.odrive_observed_version,
            self.text.brt27_binding,
            self.text.brt27_observed_version,
            self.text.update_binding,
        ];
        let limits = [32, 40, 96, 64, 64, 64, 32, 28];
        let mut payload_length = 92usize;
        for (text, limit) in strings.into_iter().zip(limits) {
            if text.len() > limit {
                return Err(Error::InvalidPayloadLength);
            }
            payload_length = payload_length
                .checked_add(1 + text.len())
                .ok_or(Error::InvalidPayloadLength)?;
        }
        let length = payload_length
            .checked_add(8)
            .ok_or(Error::InvalidPayloadLength)?;
        if payload_length > MAX_PAYLOAD_LEN || length > MAX_MESSAGE_LEN {
            Err(Error::InvalidPayloadLength)
        } else {
            Ok(length)
        }
    }
}

/// 编码身份和适用性回复。
pub fn encode_identity(fields: &IdentityFields<'_>, output: &mut [u8]) -> Result<usize, Error> {
    let strings = [
        fields.text.version,
        fields.text.source_revision,
        fields.text.build_information,
        fields.text.odrive_binding,
        fields.text.odrive_observed_version,
        fields.text.brt27_binding,
        fields.text.brt27_observed_version,
        fields.text.update_binding,
    ];
    let length = fields.encoded_len()?;
    let p = payload(output, length - 8)?;
    write_sample(p, fields.sample);
    p[32..44].copy_from_slice(&fields.uid);
    p[44] = fields.config_source as u8;
    p[45] = fields.fallback_reason as u8;
    p[46] = fields.run_identity as u8;
    p[47] = fields.update_route as u8;
    put_u32(p, 48, fields.active_can_node);
    p[52] = fields.active_can_profile as u8;
    p[53..56].fill(0);
    put_u32(p, 56, fields.host_heartbeat_hz);
    put_u32(p, 60, fields.telemetry_hz);
    put_u32(p, 64, fields.host_contact_max_age_ms);
    put_u32(p, 68, fields.config_format_version);
    p[72] = fields.build_evidence as u8;
    p[73] = fields.odrive_evidence as u8;
    p[74] = fields.brt27_evidence as u8;
    p[75] = 0;
    put_u64(p, 76, fields.high_water_operation_id);
    put_u64(p, 84, fields.retained_operation_id);
    let mut at = 92;
    for text in strings {
        p[at] = text.len() as u8;
        at += 1;
        p[at..at + text.len()].copy_from_slice(text.as_bytes());
        at += text.len();
    }
    encode_in_place(MessageKind::Identity, length - 8, output)
}
