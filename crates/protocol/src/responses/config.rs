// Copyright The eha-sdk Contributors

use super::*;

const CONFIG_PREFIX_LEN: usize = 48;
/// 读取的配置视图。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ConfigView {
    /// 配套出厂记录。
    Factory = 0,
    /// 实际用户记录。
    UserRecord = 1,
    /// 启动配置。
    Startup = 2,
    /// 当前通信设置。
    Communication = 3,
}

/// 配置记录状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ConfigRecordState {
    /// 完整可解析。
    Complete = 0,
    /// 记录为空。
    Empty = 1,
    /// 残缺或不可解析。
    Incomplete = 2,
    /// 格式版本不支持。
    FormatUnsupported = 3,
    /// 数值表示失败。
    NumericRepresentationFailed = 4,
    /// 读取失败。
    ReadFailed = 5,
    /// 所需视图不可取得。
    ViewUnavailable = 6,
}

/// ConfigData 固定前缀的具名字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigDataFields {
    /// 共同回复快照。
    pub sample: SampleData,
    /// 读取的配置视图。
    pub view: ConfigView,
    /// 实际记录状态。
    pub record_state: ConfigRecordState,
    /// 本次启动配置来源。
    pub startup_source: StartupSource,
    /// 启动回退原因。
    pub fallback_reason: FallbackReason,
    /// 实际读取或取得时间。
    pub data_time_us: u64,
}

/// `Communication` 配置视图中固定 20 字节通信设置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommunicationSettingsFields {
    /// 当前 CAN 节点标识。
    pub active_can_node: u32,
    /// 当前 CAN 仲裁和数据相位配置。
    pub profile: CanProfile,
    /// 主机心跳频率，单位 Hz。
    pub host_heartbeat_hz: u32,
    /// 固件遥测发布频率，单位 Hz。
    pub telemetry_hz: u32,
    /// 主机联系允许的最长年龄，单位毫秒。
    pub host_contact_max_age_ms: u32,
}

impl CommunicationSettingsFields {
    /// 包含公共头和 CRC 的完整 `ConfigData` 通信视图长度。
    pub const fn encoded_len() -> usize {
        76
    }
}

/// 直接写入最终发送缓冲的 ConfigData 构造器。
pub struct ConfigDataWriter<'a> {
    output: &'a mut [u8],
    payload_length: usize,
}

impl<'a> ConfigDataWriter<'a> {
    /// 计算指定原始记录长度需要的完整线上长度。
    pub fn required_len(record_length: usize) -> Result<usize, Error> {
        if record_length > 16_384 {
            return Err(Error::InvalidPayloadLength);
        }
        CONFIG_PREFIX_LEN
            .checked_add(record_length)
            .and_then(|payload_length| payload_length.checked_add(8))
            .ok_or(Error::InvalidPayloadLength)
    }

    /// 写入固定前缀并保留实际记录的可写窗口；不复制记录内容。
    pub fn start(
        fields: ConfigDataFields,
        record_length: usize,
        output: &'a mut [u8],
    ) -> Result<Self, Error> {
        let payload_length = Self::required_len(record_length)? - 8;
        let p = payload(output, payload_length)?;
        write_sample(p, fields.sample);
        p[32] = fields.view as u8;
        p[33] = fields.record_state as u8;
        p[34] = fields.startup_source as u8;
        p[35] = fields.fallback_reason as u8;
        put_u32(p, 36, record_length as u32);
        put_u64(p, 40, fields.data_time_us);
        Ok(Self {
            output,
            payload_length,
        })
    }

    /// 实际记录的最终发送窗口。
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.output[52..4 + self.payload_length]
    }

    /// 在同一缓冲写公共头和CRC并执行格式校验。
    pub fn finish(self) -> Result<usize, Error> {
        encode_in_place(MessageKind::ConfigData, self.payload_length, self.output)
    }
}

/// 编码当前通信设置的固定 `Communication` 配置视图。
pub fn encode_communication_settings(
    fields: ConfigDataFields,
    settings: CommunicationSettingsFields,
    output: &mut [u8],
) -> Result<usize, Error> {
    if fields.view != ConfigView::Communication
        || fields.record_state != ConfigRecordState::Complete
    {
        return Err(Error::InvalidField);
    }
    let mut writer = ConfigDataWriter::start(fields, 20, output)?;
    let data = writer.data_mut();
    put_u32(data, 0, settings.active_can_node);
    data[4] = settings.profile as u8;
    data[5..8].fill(0);
    put_u32(data, 8, settings.host_heartbeat_hz);
    put_u32(data, 12, settings.telemetry_hz);
    put_u32(data, 16, settings.host_contact_max_age_ms);
    writer.finish()
}
