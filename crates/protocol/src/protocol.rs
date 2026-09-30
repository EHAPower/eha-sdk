// Copyright The eha_controller Contributors

/// 完整公共消息的最大字节数。
pub const MAX_MESSAGE_LEN: usize = 16_440;
/// 公共载荷的最大字节数。
pub const MAX_PAYLOAD_LEN: usize = MAX_MESSAGE_LEN - 8;
pub(super) const VERSION: u8 = 1;

/// 公共消息所在的应用方向。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// 主机发送给固件。
    HostToFirmware,
    /// 固件发送给主机。
    FirmwareToHost,
}

macro_rules! message_kind_enum {
    ($(#[$meta:meta] $variant:ident = $value:expr,)+) => {
        /// 已定义的公共消息类型。
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        #[repr(u8)]
        pub enum MessageKind {
            $(#[$meta] $variant = $value,)+
        }

        impl MessageKind {
            /// 从线上类型字节取得已定义类型。
            pub const fn from_byte(value: u8) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

message_kind_enum! {
    /// 绝对位置目标。
    Position = 0x01,
    /// 线速度目标。
    Velocity = 0x02,
    /// 力目标。
    Force = 0x03,
    /// 阻抗目标。
    Impedance = 0x04,
    /// 显式停止。
    Stop = 0x05,
    /// 显式主机心跳。
    Heartbeat = 0x06,
    /// 查询。
    Query = 0x10,
    /// 读取配置。
    ReadConfig = 0x11,
    /// 读取维护结果。
    ReadResult = 0x12,
    /// 保存完整配置。
    SaveConfig = 0x20,
    /// 恢复出厂配置。
    RestoreFactory = 0x21,
    /// 请求应用复位。
    ResetApplication = 0x22,
    /// 请求更新入口。
    EnterUpdate = 0x23,
    /// 释放已结清结果。
    ReleaseResult = 0x24,
    /// 周期遥测。
    Telemetry = 0x80,
    /// 身份及适用性。
    Identity = 0x90,
    /// 当前状态查询回复。
    Status = 0x91,
    /// 详细测量。
    Measurements = 0x92,
    /// 详细诊断。
    Diagnostics = 0x93,
    /// 完整配置数据。
    ConfigData = 0x94,
    /// 维护进展或结果。
    OperationResult = 0x95,
    /// 数据或结果不可取得的事实。
    DataUnavailable = 0x96,
}

impl MessageKind {
    /// 返回此类型是否是唯一可刷新入口联系的显式心跳。
    pub const fn is_heartbeat(self) -> bool {
        matches!(self, Self::Heartbeat)
    }

    pub(super) const fn direction(self) -> Direction {
        match self {
            Self::Position
            | Self::Velocity
            | Self::Force
            | Self::Impedance
            | Self::Stop
            | Self::Heartbeat
            | Self::Query
            | Self::ReadConfig
            | Self::ReadResult
            | Self::SaveConfig
            | Self::RestoreFactory
            | Self::ResetApplication
            | Self::EnterUpdate
            | Self::ReleaseResult => Direction::HostToFirmware,
            _ => Direction::FirmwareToHost,
        }
    }
}

/// 前缀探查结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Prefix {
    /// 尚未收齐4字节公共头。
    Incomplete,
    /// 头合法且可确定完整消息长度；载荷和 CRC 尚未验证。
    Complete {
        /// 完整消息精确长度。
        length: usize,
        /// 已识别的消息类型。
        kind: MessageKind,
    },
}

/// 已经通过完整公共格式校验的摘要。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageInfo {
    /// 完整消息精确长度。
    pub length: usize,
    /// 消息类型。
    pub kind: MessageKind,
}

/// 公共格式错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// 版本不是当前公共版本。
    UnsupportedVersion,
    /// 类型未分配。
    UnknownKind,
    /// 已定义类型不允许此接收方向。
    WrongDirection,
    /// 类型的载荷长度不符合合同。
    InvalidPayloadLength,
    /// 输入不是该公共头声明的精确完整长度。
    IncorrectMessageLength,
    /// CRC-32C 不匹配。
    CrcMismatch,
    /// 某字段、判别值、保留位、字符串或数值表示非法。
    InvalidField,
    /// 输出缓冲不足。
    BufferTooSmall,
}
