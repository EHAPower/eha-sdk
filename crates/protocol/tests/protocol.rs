// Copyright The eha_controller Contributors

use protocol::{
    Direction, Error, MAX_MESSAGE_LEN, Message, MessageKind, Prefix, Response, ValidatedMessage,
    crc32c, decode, encode, encode_in_place, probe_prefix, validate,
};

const H: [u8; 8] = [1, 6, 0, 0, 0x68, 0x17, 0x93, 0x44];
const P10: [u8; 12] = [1, 1, 4, 0, 0, 0, 0x20, 0x41, 0xaf, 0xa9, 0x79, 0xd3];

#[path = "protocol/requests.rs"]
mod requests;
#[path = "protocol/responses.rs"]
mod responses;
