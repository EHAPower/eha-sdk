// Copyright The eha-sdk Contributors

use protocol::{
    Direction, Error, MAX_MESSAGE_LEN, Message, MessageKind, Prefix, Response, ValidatedMessage,
    crc32c, decode, encode, encode_in_place, probe_prefix, validate,
};

const H: [u8; 8] = [3, 6, 0, 0, 0xe9, 0x34, 0xf4, 0xfb];
const P10: [u8; 12] = [3, 1, 4, 0, 0, 0, 0x20, 0x41, 0xe1, 0x53, 0x01, 0x41];

#[path = "protocol/requests.rs"]
mod requests;
#[path = "protocol/responses.rs"]
mod responses;
