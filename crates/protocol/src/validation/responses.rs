// Copyright The eha-sdk Contributors

use super::*;

pub(super) fn validate_sample(p: &[u8], require_query: bool) -> Result<(), Error> {
    if p.len() < 32 || u64_at(p, 24) == u64::MAX || (require_query && u32_at(p, 0) == 0) {
        Err(Error::InvalidField)
    } else {
        Ok(())
    }
}
pub(super) fn valid_state(s: u8) -> bool {
    (s & 0xc0) == 0 && (s & 7) <= 6
}
pub(super) fn finite_at(p: &[u8], offset: usize) -> Result<(), Error> {
    if f32_at(p, offset).is_finite() {
        Ok(())
    } else {
        Err(Error::InvalidField)
    }
}
pub(super) fn validate_value(p: &[u8], value: usize, state: u8) -> Result<(), Error> {
    if !valid_state(state) {
        return Err(Error::InvalidField);
    }
    if state & 7 == 1 {
        finite_at(p, value)
    } else if u32_at(p, value) == 0 {
        Ok(())
    } else {
        Err(Error::InvalidField)
    }
}
pub(super) fn validate_status(kind: MessageKind, p: &[u8]) -> Result<(), Error> {
    validate_sample(p, matches!(kind, MessageKind::Status))?;
    if matches!(kind, MessageKind::Telemetry) && u32_at(p, 0) != 0 {
        return Err(Error::InvalidField);
    }
    if p[93..96] != [0; 3]
        || p[36] > 4
        || p[37] > 2
        || u16_at(p, 38) & !0x007f != 0
        || u16_at(p, 66) & !0x3fff != 0
        || u32_at(p, 52) & 0x8000_0000 != 0
        || u32_at(p, 56) & 0x8000_0000 != 0
        || u32_at(p, 60) & 0x8000_0000 != 0
    {
        return Err(Error::InvalidField);
    }
    let mode = p[36];
    if (mode == 0) != (p[37] == 0) || (mode != 0 && p[37] == 0) {
        return Err(Error::InvalidField);
    }
    for off in [40, 44, 48, 68, 72, 76, 80, 84, 108, 116, 120] {
        finite_at(p, off)?;
    }
    for (value, state) in [
        (68, p[88]),
        (72, p[89]),
        (76, p[90]),
        (80, p[91]),
        (84, p[92]),
        (108, p[113]),
        (116, p[114]),
        (120, p[115]),
    ] {
        validate_value(p, value, state)?;
    }
    if (p[88] & 7 == 0 && u32_at(p, 96) != u32::MAX)
        || (p[89] & 7 == 0 && u32_at(p, 100) != u32::MAX)
        || ((p[90] & 7 == 0 || p[91] & 7 == 0 || p[92] & 7 == 0) && u32_at(p, 104) != u32::MAX)
    {
        return Err(Error::InvalidField);
    }
    if !valid_reason(u16_at(p, 64))
        || (p[114] & 7 == 0 && u32_at(p, 32) != u32::MAX)
        || (p[115] & 7 == 0 && u32_at(p, 124) != u32::MAX)
    {
        return Err(Error::InvalidField);
    }
    if (mode == 0 && p[40..52] != [0; 12])
        || (matches!(mode, 1..=3) && p[44..52] != [0; 8])
        || (u16_at(p, 38) & 1 != 0 && u32_at(p, 60) != 0)
    {
        return Err(Error::InvalidField);
    }
    if p[112] > 3
        || (p[112] == 0 && (u32_at(p, 108) != 0 || p[113] != 0))
        || p[140] & !0x0f != 0
        || p[141] > 2
        || p[150] > 2
        || p[151] > 2
        || p[140] & 0x0a == 0x0a
    {
        return Err(Error::InvalidField);
    }
    if p[140] & 1 == 0
        && (u32_at(p, 128) != 0 || u32_at(p, 132) != 0 || u32_at(p, 136) != u32::MAX || p[140] != 0)
    {
        return Err(Error::InvalidField);
    }
    if (p[150] == 0 && u32_at(p, 142) != u32::MAX)
        || (p[151] == 0 && u32_at(p, 146) != u32::MAX)
        || (p[150] != 0 && u32_at(p, 142) == u32::MAX)
        || (p[151] != 0 && u32_at(p, 146) == u32::MAX)
    {
        return Err(Error::InvalidField);
    }
    Ok(())
}
pub(super) fn validate_identity(p: &[u8]) -> Result<(), Error> {
    validate_sample(p, true)?;
    if p[44] > 2
        || p[45] > 6
        || p[46] > 1
        || p[47] > 2
        || p[52] > 6
        || p[53..56] != [0; 3]
        || p[72] > 1
        || p[73] > 2
        || p[74] > 2
        || p[75] != 0
        || u64_at(p, 84) > u64_at(p, 76)
    {
        return Err(Error::InvalidField);
    }
    if (p[46] == 0) != p[4..20].iter().all(|&x| x == 0) {
        return Err(Error::InvalidField);
    }
    let mut at = 92;
    let limits = [32usize, 40, 96, 64, 64, 64, 32, 28];
    for limit in limits {
        let n = usize::from(*p.get(at).ok_or(Error::InvalidField)?);
        at += 1;
        if n > limit
            || at + n > p.len()
            || core::str::from_utf8(&p[at..at + n]).is_err()
            || p[at..at + n].contains(&0)
            || p[at..at + n].starts_with(&[0xef, 0xbb, 0xbf])
        {
            return Err(Error::InvalidField);
        }
        at += n;
    }
    if at != p.len() {
        return Err(Error::InvalidField);
    }
    if (p[72] == 0
        && (self_string_nonempty(p, 0) || self_string_nonempty(p, 1) || self_string_nonempty(p, 2)))
        || (p[73] != 1 && self_string_nonempty(p, 4))
        || (p[74] != 1 && self_string_nonempty(p, 6))
        || (p[47] != 1 && self_string_nonempty(p, 7))
    {
        return Err(Error::InvalidField);
    }
    Ok(())
}
pub(super) fn validate_measurements(p: &[u8]) -> Result<(), Error> {
    validate_sample(p, true)?;
    for i in 0..8 {
        validate_value(p, 32 + i * 4, p[64 + i])?;
    }
    if p[92] > 2 || p[93] > 1 || p[94] > 2 || p[95] != 0 {
        return Err(Error::InvalidField);
    }
    finite_at(p, 88)?;
    if (p[64] & 7 == 0 && u32_at(p, 72) != u32::MAX)
        || (p[65] & 7 == 0 && u32_at(p, 76) != u32::MAX)
        || (((p[66] & 7 == 0)
            || (p[67] & 7 == 0)
            || (p[68] & 7 == 0)
            || (p[69] & 7 == 0)
            || (p[70] & 7 == 0)
            || (p[71] & 7 == 0))
            && u32_at(p, 80) != u32::MAX)
        || ((p[66] & 7 == 0)
            && (p[67] & 7 == 0)
            && (p[68] & 7 == 0)
            && (p[69] & 7 == 0)
            && (p[70] & 7 == 0)
            && (p[71] & 7 == 0)
            && u32_at(p, 96) != 0)
    {
        return Err(Error::InvalidField);
    }
    if p[93] == 0 && u32_at(p, 84) != 0
        || p[94] != 1 && (u32_at(p, 88) != 0 || p[92] != 0)
        || (p[94] == 1 && !(p[92] == 1 || p[92] == 2))
    {
        return Err(Error::InvalidField);
    }
    Ok(())
}
pub(super) fn validate_diagnostics(p: &[u8]) -> Result<(), Error> {
    validate_sample(p, true)?;
    let count = usize::from(u16_at(p, 32));
    if count > 16 || p[34] > 1 || p[35] != 0 || p.len() != 36 + count * 64 {
        return Err(Error::InvalidField);
    }
    let (entries, []) = p[36..].as_chunks::<64>() else {
        return Err(Error::InvalidField);
    };
    for e in entries {
        if e[0] > 8
            || e[1] > 12
            || e[3] > 11
            || e[46..48] != [0; 2]
            || e[53] > 7
            || u16_at(e, 54) & !0x000f != 0
            || e[45] & 0xc0 != 0
            || u16_at(e, 6) & !0x07ff != 0
            || !valid_reason(u16_at(e, 4))
            || u32_at(e, 60) & !0x3f != 0
        {
            return Err(Error::InvalidField);
        }
        validate_value(e, 48, e[52])?;
        let code = u32_at(e, 40);
        let domain = e[44];
        if (e[0] != 8 && u64_at(e, 32) != 0)
            || (e[3] == 0 && (u64_at(e, 16) != u64::MAX || u64_at(e, 24) != u64::MAX))
            || (domain == 0 && code != 0)
            || !valid_native_code(domain, code)
        {
            return Err(Error::InvalidField);
        }
    }
    Ok(())
}
pub(super) fn validate_config_data(p: &[u8]) -> Result<(), Error> {
    validate_sample(p, true)?;
    if p[32] > 3 || p[33] > 6 || p[34] > 2 || p[35] > 6 {
        return Err(Error::InvalidField);
    }
    let n = usize::try_from(u32_at(p, 36)).map_err(|_| Error::InvalidField)?;
    if n > 16_384 || p.len() != 48 + n {
        return Err(Error::InvalidField);
    }
    if (p[33] == 5 || p[33] == 6) && (n != 0 || u64_at(p, 40) != u64::MAX) {
        return Err(Error::InvalidField);
    }
    if p[33] <= 4 && u64_at(p, 40) == u64::MAX {
        return Err(Error::InvalidField);
    }
    if (p[32] == 0 || p[32] == 2) && p[33] != 0 && p[33] != 5 && p[33] != 6 {
        return Err(Error::InvalidField);
    }
    if p[32] == 1 && p[33] == 1 && n != 0 {
        return Err(Error::InvalidField);
    }
    if p[32] == 3
        && !((p[33] == 0 && n == 20 && p[52] <= 6 && p[53..56] == [0; 3])
            || ((p[33] == 5 || p[33] == 6) && n == 0))
    {
        return Err(Error::InvalidField);
    }
    Ok(())
}
pub(super) fn validate_operation_result(p: &[u8]) -> Result<(), Error> {
    validate_sample(p, u32_at(p, 0) != 0)?;
    if u64_at(p, 32) == 0
        || !(1..=4).contains(&p[40])
        || p[41] > 7
        || p[42] > 11
        || p[43] & 0xc0 != 0
        || u32_at(p, 48) & !0x000f != 0
    {
        return Err(Error::InvalidField);
    }
    let state = p[41];
    let evidence = p[43];
    let revision = u32_at(p, 84);
    if evidence & 1 != 0 && evidence & 0x0e != 0 {
        return Err(Error::InvalidField);
    }
    if p[40] >= 3 && evidence & 0x06 != 0 {
        return Err(Error::InvalidField);
    }
    let started = u64_at(p, 52);
    let deadline = u64_at(p, 60);
    let finished = u64_at(p, 68);
    let snapshot = u64_at(p, 24);
    let constraints = u32_at(p, 48);
    if started != u64::MAX && started > snapshot
        || finished != u64::MAX && finished > snapshot
        || started != u64::MAX && deadline != u64::MAX && deadline < started
        || started != u64::MAX && finished != u64::MAX && finished < started
    {
        return Err(Error::InvalidField);
    }
    if p[40] >= 3 && (u32_at(p, 76) != 0 || u32_at(p, 80) != 0)
        || p[40] <= 2 && u32_at(p, 76) > 16_384
        || (u32_at(p, 76) == 0 && u32_at(p, 80) != 0)
        || !valid_reason(u16_at(p, 44))
        || u16_at(p, 46) & !0x07ff != 0
    {
        return Err(Error::InvalidField);
    }
    if state == 0 {
        if !((p[42] == 1 || p[42] == 6)
            && evidence == 0x21
            && started == u64::MAX
            && deadline == u64::MAX
            && finished != u64::MAX
            && (revision == 0 && constraints == 0 || revision != 0 && constraints == 8))
        {
            return Err(Error::InvalidField);
        }
    } else if revision == 0 {
        return Err(Error::InvalidField);
    }
    let operation_phase = if p[40] <= 2 {
        matches!(p[42], 6 | 7 | 8 | 11)
    } else {
        matches!(p[42], 1 | 10 | 11)
    };
    if matches!(state, 1 | 2)
        && (!operation_phase
            || evidence & 0x20 != 0
            || started == u64::MAX
            || finished != u64::MAX
            || constraints & 8 == 0
            || (p[40] >= 3 && evidence & 0x06 != 0))
    {
        return Err(Error::InvalidField);
    }
    if state == 3
        && (!(p[40] <= 2)
            || !matches!(p[42], 7 | 8 | 11)
            || evidence & 0x22 != 0x22
            || evidence & 0x11 != 0
            || constraints != 8
            || started == u64::MAX
            || finished == u64::MAX)
    {
        return Err(Error::InvalidField);
    }
    if state == 4
        && (!operation_phase
            || u16_at(p, 44) == 0
            || started == u64::MAX
            || (evidence & 0x20 != 0 && (finished == u64::MAX || constraints != 8))
            || (evidence & 0x20 == 0 && (finished != u64::MAX || constraints & 8 == 0)))
    {
        return Err(Error::InvalidField);
    }
    if state == 5
        && (!operation_phase
            || u16_at(p, 44) == 0
            || evidence & 0x20 != 0
            || started == u64::MAX
            || deadline == u64::MAX
            || snapshot < deadline
            || finished != u64::MAX
            || constraints & 8 == 0)
    {
        return Err(Error::InvalidField);
    }
    if state == 6
        && (!operation_phase
            || u16_at(p, 44) == 0
            || evidence & 0x20 != 0
            || evidence & 0x18 == 0
            || started == u64::MAX
            || finished != u64::MAX
            || constraints & 8 == 0)
    {
        return Err(Error::InvalidField);
    }
    if state == 7
        && (!(p[40] >= 3)
            || p[42] != 10
            || evidence & 0x2f != 0x08
            || started == u64::MAX
            || finished != u64::MAX
            || constraints & 0x0c != 0x0c)
    {
        return Err(Error::InvalidField);
    }
    Ok(())
}
pub(super) fn self_string_nonempty(p: &[u8], wanted: usize) -> bool {
    let mut at = 92;
    for index in 0..8 {
        let n = usize::from(p[at]);
        if index == wanted {
            return n != 0;
        }
        at += 1 + n;
    }
    false
}
pub(super) fn validate_unavailable(p: &[u8]) -> Result<(), Error> {
    validate_sample(p, p[32] <= 4)?;
    if p[32] > 6
        || !valid_reason(u16_at(p, 33))
        || u16_at(p, 35) & !0x07ff != 0
        || p[37] > 1
        || p[38..40] != [0; 2]
        || (p[32] <= 4 && u64_at(p, 40) != 0)
    {
        Err(Error::InvalidField)
    } else {
        Ok(())
    }
}
pub(super) fn valid_reason(reason: u16) -> bool {
    matches!(reason,0..=31|0x0100..=0x0106|0x0200..=0x0206|0x0300..=0x0307|0x0400)
}
pub(super) fn valid_native_code(domain: u8, code: u32) -> bool {
    match domain {
        0 => code == 0,
        1 | 2 | 7 | 8 => true,
        3 => (1..=8).contains(&code),
        4 => (1..=2).contains(&code),
        5 => (1..=6).contains(&code),
        6 => (1..=7).contains(&code),
        _ => false,
    }
}
