// Copyright The eha-sdk Contributors

use super::{Lane, MAX_RAW_BLOCK_LEN};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CobsCursor {
    StartDelimiter,
    Segment {
        start: usize,
        length: usize,
        next_raw: usize,
        next_segment: bool,
        code_emitted: bool,
        data_emitted: usize,
    },
    EndDelimiter,
    Done,
}
#[cfg(test)]
pub(super) fn cobs_encode(raw: &[u8], output: &mut [u8]) -> Result<usize, ()> {
    if raw.is_empty() || output.is_empty() {
        return Err(());
    }
    let mut code_index = 0;
    let mut code = 1u8;
    let mut written = 1usize;
    for byte in raw {
        if *byte == 0 {
            output[code_index] = code;
            code_index = written;
            written += 1;
            if written > output.len() {
                return Err(());
            }
            code = 1;
        } else {
            if written == output.len() {
                return Err(());
            }
            output[written] = *byte;
            written += 1;
            code = code.wrapping_add(1);
            if code == 0xff {
                output[code_index] = code;
                code_index = written;
                written += 1;
                if written > output.len() {
                    return Err(());
                }
                code = 1;
            }
        }
    }
    output[code_index] = code;
    Ok(written)
}

pub(super) fn next_cobs_byte(
    cursor: &mut CobsCursor,
    lane: Lane,
    transfer: u8,
    fragment_index: u16,
    fragment: &[u8],
) -> Option<u8> {
    loop {
        match *cursor {
            CobsCursor::StartDelimiter => {
                *cursor = make_segment(0, lane, transfer, fragment_index, fragment);
                return Some(0);
            }
            CobsCursor::Segment {
                start,
                length,
                next_raw,
                next_segment,
                code_emitted: false,
                ..
            } => {
                *cursor = CobsCursor::Segment {
                    start,
                    length,
                    next_raw,
                    next_segment,
                    code_emitted: true,
                    data_emitted: 0,
                };
                return Some((length + 1) as u8);
            }
            CobsCursor::Segment {
                start,
                length,
                next_raw,
                next_segment,
                code_emitted: true,
                data_emitted,
            } => {
                if data_emitted < length {
                    let byte = virtual_raw_byte(
                        start + data_emitted,
                        lane,
                        transfer,
                        fragment_index,
                        fragment,
                    );
                    let emitted = data_emitted + 1;
                    *cursor = if emitted == length {
                        if next_segment {
                            make_segment(next_raw, lane, transfer, fragment_index, fragment)
                        } else {
                            CobsCursor::EndDelimiter
                        }
                    } else {
                        CobsCursor::Segment {
                            start,
                            length,
                            next_raw,
                            next_segment,
                            code_emitted: true,
                            data_emitted: emitted,
                        }
                    };
                    return Some(byte);
                }
                *cursor = if next_segment {
                    make_segment(next_raw, lane, transfer, fragment_index, fragment)
                } else {
                    CobsCursor::EndDelimiter
                };
            }
            CobsCursor::EndDelimiter => {
                *cursor = CobsCursor::Done;
                return Some(0);
            }
            CobsCursor::Done => return None,
        }
    }
}

fn make_segment(
    start: usize,
    lane: Lane,
    transfer: u8,
    fragment_index: u16,
    fragment: &[u8],
) -> CobsCursor {
    let raw_len = fragment.len() + 4;
    let mut length = 0;
    while start + length < raw_len && length < 254 {
        if virtual_raw_byte(start + length, lane, transfer, fragment_index, fragment) == 0 {
            break;
        }
        length += 1;
    }
    let has_remaining = start + length < raw_len;
    let next_segment = has_remaining;
    let next_raw = if has_remaining
        && virtual_raw_byte(start + length, lane, transfer, fragment_index, fragment) == 0
    {
        start + length + 1
    } else {
        start + length
    };
    CobsCursor::Segment {
        start,
        length,
        next_raw,
        next_segment,
        code_emitted: false,
        data_emitted: 0,
    }
}

fn virtual_raw_byte(
    at: usize,
    lane: Lane,
    transfer: u8,
    fragment_index: u16,
    fragment: &[u8],
) -> u8 {
    match at {
        0 => lane as u8,
        1 => transfer,
        2 => fragment_index as u8,
        3 => (fragment_index >> 8) as u8,
        _ => fragment[at - 4],
    }
}

#[cfg(test)]
pub(super) fn cobs_decode(encoded: &[u8], output: &mut [u8]) -> Result<usize, ()> {
    if encoded.is_empty() {
        return Err(());
    }
    let mut source = 0;
    let mut written = 0;
    while source < encoded.len() {
        let code = encoded[source];
        if code == 0 {
            return Err(());
        }
        source += 1;
        let count = usize::from(code - 1);
        if source + count > encoded.len() || written + count > output.len() {
            return Err(());
        }
        output[written..written + count].copy_from_slice(&encoded[source..source + count]);
        source += count;
        written += count;
        if code != 0xff && source < encoded.len() {
            if written == output.len() {
                return Err(());
            }
            output[written] = 0;
            written += 1;
        }
    }
    Ok(written)
}

pub(super) fn cobs_decode_in_place(bytes: &mut [u8], encoded_len: usize) -> Result<usize, ()> {
    if encoded_len == 0 || encoded_len > bytes.len() {
        return Err(());
    }
    let mut source = 0;
    let mut written = 0;
    while source < encoded_len {
        let code = bytes[source];
        if code == 0 {
            return Err(());
        }
        source += 1;
        let count = usize::from(code - 1);
        if source + count > encoded_len || written + count > MAX_RAW_BLOCK_LEN {
            return Err(());
        }
        // COBS encoded length is always greater than its decoded length at this
        // point, so this forward copy never overwrites unread source bytes.
        for offset in 0..count {
            bytes[written + offset] = bytes[source + offset];
        }
        source += count;
        written += count;
        if code != 0xff && source < encoded_len {
            if written == MAX_RAW_BLOCK_LEN {
                return Err(());
            }
            bytes[written] = 0;
            written += 1;
        }
    }
    Ok(written)
}
