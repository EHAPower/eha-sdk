// Copyright The eha_controller Contributors

use crate::protocol::VERSION;
use crate::validation::{
    f32_at, payload_len_is_valid, put_f32, put_u16, put_u32, u16_at, u32_at, validate_payload,
};
use crate::{
    Direction, Error, MAX_PAYLOAD_LEN, Message, MessageInfo, MessageKind, OperationKey, Prefix,
    Response,
};

mod sealed {
    pub trait Bytes {
        fn bytes(&self) -> &[u8];
    }

    impl Bytes for &[u8] {
        fn bytes(&self) -> &[u8] {
            self
        }
    }

    impl Bytes for &mut [u8] {
        fn bytes(&self) -> &[u8] {
            self
        }
    }
}

/// 与一段不可变完整字节绑定的公共格式校验证明。
///
/// 只能由 [`Self::validate`] 构造。它保留校验时的方向、摘要和借用的精确字节，因而
/// [`Self::decode`] 不会再次计算 CRC 或重做字段校验。
pub struct ValidatedMessage<B> {
    bytes: B,
    validated_len: usize,
    direction: Direction,
    info: MessageInfo,
}

impl<'a> ValidatedMessage<&'a [u8]> {
    /// 校验完整消息，并将成功结果与这段不可变字节绑定。
    pub fn validate(bytes: &'a [u8], direction: Direction) -> Result<Self, Error> {
        let info = validate(bytes, direction)?;
        Ok(Self {
            bytes,
            validated_len: info.length,
            direction,
            info,
        })
    }
}

impl<'a> ValidatedMessage<&'a mut [u8]> {
    /// 接管一块接收缓冲，校验其精确有效前缀，并保留整块缓冲以便随后归还。
    ///
    /// 失败时返还原缓冲所有权及错误。`length` 超出缓冲时返回
    /// [`Error::IncorrectMessageLength`]，不会读取缓冲外字节。
    pub fn validate_buffer(
        bytes: &'a mut [u8],
        length: usize,
        direction: Direction,
    ) -> Result<Self, (&'a mut [u8], Error)> {
        if length > bytes.len() {
            return Err((bytes, Error::IncorrectMessageLength));
        }
        let info = match validate(&bytes[..length], direction) {
            Ok(info) => info,
            Err(error) => return Err((bytes, error)),
        };
        Ok(Self {
            bytes,
            validated_len: length,
            direction,
            info,
        })
    }
}

impl<B: sealed::Bytes> ValidatedMessage<B> {
    /// 返回已经校验的完整消息字节。
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes.bytes()[..self.validated_len]
    }

    /// 返回校验时要求的应用方向。
    #[must_use]
    pub const fn direction(&self) -> Direction {
        self.direction
    }

    /// 返回与这段字节绑定的公共消息摘要。
    #[must_use]
    pub const fn info(&self) -> MessageInfo {
        self.info
    }

    /// 将已经校验的字节解码为具名消息，不重复完整格式校验。
    pub fn decode(&self) -> Result<Message<'_>, Error> {
        decode_validated(self.bytes(), self.info)
    }

    /// 取得校验过的原始字节所有权。
    ///
    /// 此操作消费证明；随后再次使用这些字节前，调用方必须重新校验。
    #[must_use]
    pub fn into_bytes(self) -> B {
        self.bytes
    }
}

/// 用唯一的头部规则探查完整长度。
pub fn probe_prefix(bytes: &[u8], direction: Direction) -> Result<Prefix, Error> {
    if bytes.len() < 4 {
        return Ok(Prefix::Incomplete);
    }
    if bytes[0] != VERSION {
        return Err(Error::UnsupportedVersion);
    }
    let kind = MessageKind::from_byte(bytes[1]).ok_or(Error::UnknownKind)?;
    if kind.direction() != direction {
        return Err(Error::WrongDirection);
    }
    let payload_length = usize::from(u16_at(bytes, 2));
    if payload_length > MAX_PAYLOAD_LEN || !payload_len_is_valid(kind, payload_length) {
        return Err(Error::InvalidPayloadLength);
    }
    Ok(Prefix::Complete {
        length: payload_length + 8,
        kind,
    })
}

/// 校验完整字节并返回摘要；不借用输入。
pub fn validate(bytes: &[u8], direction: Direction) -> Result<MessageInfo, Error> {
    let Prefix::Complete { length, kind } = probe_prefix(bytes, direction)? else {
        return Err(Error::IncorrectMessageLength);
    };
    if bytes.len() != length {
        return Err(Error::IncorrectMessageLength);
    }
    if crc32c(&bytes[..length - 4]) != u32_at(bytes, length - 4) {
        return Err(Error::CrcMismatch);
    }
    validate_payload(kind, &bytes[4..length - 4])?;
    Ok(MessageInfo { length, kind })
}

/// 完整解码。返回值借用 `bytes`，接收缓冲在它存活期间不可复用。
pub fn decode(bytes: &[u8], direction: Direction) -> Result<Message<'_>, Error> {
    let info = validate(bytes, direction)?;
    decode_validated(bytes, info)
}

fn decode_validated(bytes: &[u8], info: MessageInfo) -> Result<Message<'_>, Error> {
    let p = &bytes[4..info.length - 4];
    Ok(match info.kind {
        MessageKind::Position => Message::Position(f32_at(p, 0)),
        MessageKind::Velocity => Message::Velocity(f32_at(p, 0)),
        MessageKind::Force => Message::Force(f32_at(p, 0)),
        MessageKind::Impedance => Message::Impedance {
            equilibrium_mm: f32_at(p, 0),
            stiffness_n_per_mm: f32_at(p, 4),
            damping_ns_per_mm: f32_at(p, 8),
        },
        MessageKind::Stop => Message::Stop,
        MessageKind::Heartbeat => Message::Heartbeat,
        MessageKind::Query => Message::Query {
            query_id: u32_at(p, 0),
            category: p[4],
        },
        MessageKind::ReadConfig => Message::ReadConfig {
            query_id: u32_at(p, 0),
            view: p[4],
        },
        MessageKind::ReadResult => Message::ReadResult {
            query_id: u32_at(p, 0),
            key: OperationKey { bytes: &p[4..40] },
        },
        MessageKind::SaveConfig => Message::SaveConfig {
            key: OperationKey { bytes: &p[..36] },
            record: &p[36..],
        },
        MessageKind::RestoreFactory | MessageKind::ResetApplication | MessageKind::EnterUpdate => {
            Message::Maintenance {
                kind: info.kind,
                key: OperationKey { bytes: p },
            }
        }
        MessageKind::ReleaseResult => Message::ReleaseResult {
            key: OperationKey { bytes: &p[..36] },
            revision: u32_at(p, 36),
        },
        kind => Message::Response(Response::new(kind, p)?),
    })
}

/// 编码已表示的消息到调用方拥有的有界缓冲并返回精确长度。
pub fn encode(message: Message<'_>, output: &mut [u8]) -> Result<usize, Error> {
    let kind = message.kind();
    let payload_len = payload_len(&message);
    let length = message.encoded_len()?;
    if output.len() < length {
        return Err(Error::BufferTooSmall);
    }
    output[0] = VERSION;
    output[1] = kind as u8;
    put_u16(output, 2, payload_len as u16);
    let p = &mut output[4..4 + payload_len];
    match message {
        Message::Position(v) | Message::Velocity(v) | Message::Force(v) => put_f32(p, 0, v),
        Message::Impedance {
            equilibrium_mm,
            stiffness_n_per_mm,
            damping_ns_per_mm,
        } => {
            put_f32(p, 0, equilibrium_mm);
            put_f32(p, 4, stiffness_n_per_mm);
            put_f32(p, 8, damping_ns_per_mm);
        }
        Message::Stop | Message::Heartbeat => {}
        Message::Query { query_id, category }
        | Message::ReadConfig {
            query_id,
            view: category,
        } => {
            put_u32(p, 0, query_id);
            p[4] = category;
        }
        Message::ReadResult { query_id, key } => {
            put_u32(p, 0, query_id);
            p[4..].copy_from_slice(key.as_bytes());
        }
        Message::SaveConfig { key, record } => {
            p[..36].copy_from_slice(key.as_bytes());
            p[36..].copy_from_slice(record);
        }
        Message::Maintenance { key, .. } => p.copy_from_slice(key.as_bytes()),
        Message::ReleaseResult { key, revision } => {
            p[..36].copy_from_slice(key.as_bytes());
            put_u32(p, 36, revision);
        }
        Message::Response(response) => p.copy_from_slice(response.payload()),
    }
    validate_payload(kind, &output[4..length - 4])?;
    put_u32(output, length - 4, crc32c(&output[..length - 4]));
    Ok(length)
}

pub(super) fn payload_len(message: &Message<'_>) -> usize {
    match message {
        Message::Position(_) | Message::Velocity(_) | Message::Force(_) => 4,
        Message::Impedance { .. } => 12,
        Message::Stop | Message::Heartbeat => 0,
        Message::Query { .. } | Message::ReadConfig { .. } => 5,
        Message::ReadResult { .. } => 40,
        Message::SaveConfig { record, .. } => 36 + record.len(),
        Message::Maintenance { .. } => 36,
        Message::ReleaseResult { .. } => 40,
        Message::Response(response) => response.payload().len(),
    }
}

/// 完成已在最终发送缓冲中写好的公共载荷。
///
/// 调用方先把精确 `payload_length` 字节写入 `buffer[4..4 + payload_length]`，再调用
/// 本函数。它在同一缓冲中写头和 CRC，并以完整格式校验收尾，因此实际读回配置可直接
/// 写入最终 TX 区，不需要同时借用一个16KiB载荷切片和整个输出缓冲，也不需要第二份副本。
pub fn encode_in_place(
    kind: MessageKind,
    payload_length: usize,
    buffer: &mut [u8],
) -> Result<usize, Error> {
    if payload_length > MAX_PAYLOAD_LEN || !payload_len_is_valid(kind, payload_length) {
        return Err(Error::InvalidPayloadLength);
    }
    let length = payload_length + 8;
    if buffer.len() < length {
        return Err(Error::BufferTooSmall);
    }
    buffer[0] = VERSION;
    buffer[1] = kind as u8;
    put_u16(buffer, 2, payload_length as u16);
    put_u32(buffer, length - 4, crc32c(&buffer[..length - 4]));
    validate(&buffer[..length], kind.direction()).map(|_| length)
}

/// CRC-32C (Castagnoli) of a byte slice.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
