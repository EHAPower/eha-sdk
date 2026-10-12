// Copyright The eha-sdk Contributors

use protocol::{
    Direction, Error, MAX_MESSAGE_LEN, Message, MessageKind, Prefix, Response, ValidatedMessage,
    crc32c, decode, encode, encode_in_place, probe_prefix, validate,
};

const H: [u8; 8] = [2, 6, 0, 0, 0x51, 0x9e, 0xb1, 0x26];
const P10: [u8; 12] = [2, 1, 4, 0, 0, 0, 0x20, 0x41, 0xc6, 0x2e, 0x3d, 0x08];

#[path = "protocol/requests.rs"]
mod requests;
#[path = "protocol/responses.rs"]
mod responses;
