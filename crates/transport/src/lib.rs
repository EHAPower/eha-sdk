// Copyright The eha-sdk Contributors

#![no_std]
#![doc = include_str!("../README.md")]

pub mod can;
pub mod usb;

mod guard;

mod validated {
    use protocol::{Direction, Message, MessageInfo, ValidatedMessage};

    /// 转交原组装缓冲的已校验公共消息。
    ///
    /// 该类型只能由 CAN 或 USB 接收器的 validated 路径在完整消息通过校验时构造。它持有
    /// 原缓冲，因而字节、方向和摘要不能与其他消息拼接；调用 [`Self::decode`] 不重复完整
    /// CRC 或字段校验。
    pub struct ValidatedCompletedMessage<'a> {
        message: ValidatedMessage<&'a mut [u8]>,
    }

    impl<'a> ValidatedCompletedMessage<'a> {
        pub(crate) fn validate(
            buffer: &'a mut [u8],
            length: usize,
            direction: Direction,
        ) -> Result<Self, (&'a mut [u8], protocol::Error)> {
            match ValidatedMessage::validate_buffer(buffer, length, direction) {
                Ok(message) => Ok(Self { message }),
                Err((buffer, error)) => Err((buffer, error)),
            }
        }

        /// 返回已校验的完整原始字节。
        #[must_use]
        pub fn bytes(&self) -> &[u8] {
            self.message.bytes()
        }

        /// 返回已校验公共消息的摘要。
        #[must_use]
        pub const fn info(&self) -> MessageInfo {
            self.message.info()
        }

        /// 解码已校验消息，不重复完整格式校验。
        pub fn decode(&self) -> Result<Message<'_>, protocol::Error> {
            self.message.decode()
        }

        /// 取回原组装缓冲。
        ///
        /// 此操作消费校验证明；再次使用字节前必须经相应绑定重新完成与校验。
        #[must_use]
        pub fn into_buffer(self) -> &'a mut [u8] {
            self.message.into_bytes()
        }
    }
}
