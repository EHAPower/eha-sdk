// Copyright The eha-sdk Contributors

use core::fmt;

use crate::JsonError;

/// 原文数值直接量化为 binary32 时的失败原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloatParseError {
    /// 不是 Rust 浮点解析器接受的完整数值 token。
    InvalidSyntax,
    /// 原值为非有限值，或舍入后溢出。
    NonFinite,
    /// 原文尾数非零，但舍入后为零。
    Underflow,
}

/// 按绝对与相对误差预算比较两个有限 binary32 值。
///
/// 预算为 `absolute + relative * abs(expected)`；两个预算都必须有限且非负，候选值和
/// 期望值也必须有限。若差值或预算的中间计算溢出，返回 `false`，不把无法表示的预算
/// 当成无限大。绝对预算与被比较量同单位，相对预算无量纲。此函数只用于已明确
/// 误差预算的近似比较；离散编码、状态标识、方向和保护边界应使用严格比较。
pub fn within_tolerance(actual: f32, expected: f32, absolute: f32, relative: f32) -> bool {
    if !actual.is_finite()
        || !expected.is_finite()
        || !absolute.is_finite()
        || !relative.is_finite()
        || absolute < 0.0
        || relative < 0.0
    {
        return false;
    }
    let difference = (actual - expected).abs();
    let scaled = relative * expected.abs();
    let budget = absolute + scaled;
    difference.is_finite() && scaled.is_finite() && budget.is_finite() && difference <= budget
}

impl fmt::Display for FloatParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidSyntax => "数值格式无效",
            Self::NonFinite => "数值必须能表示为有限 binary32",
            Self::Underflow => "原文非零数值不能舍入为零",
        })
    }
}

/// 从原文 token 一次舍入为有限 binary32，拒绝原文非零下溢为零。
///
/// 使用 Rust 的完整浮点 token 语法；JSON 调用方仍须由 JSON 解码器检查自身语法。
/// 正负零等价，有限次正规数保留；不经过 binary64，也不检查产品范围。
pub fn parse_f32(token: &str) -> Result<f32, FloatParseError> {
    let value: f32 = token.parse().map_err(|_| FloatParseError::InvalidSyntax)?;
    if !value.is_finite() {
        return Err(FloatParseError::NonFinite);
    }
    // 只看尾数，不能把零尾数的非零指数误认为非零输入。
    let nonzero = token
        .bytes()
        .take_while(|byte| !matches!(byte, b'e' | b'E'))
        .any(|byte| matches!(byte, b'1'..=b'9'));
    if value == 0.0 && nonzero {
        return Err(FloatParseError::Underflow);
    }
    Ok(value)
}

/// 检查 JSON 中带小数点或指数的数值 token 是否发生非有限或非零归零。
///
/// 本函数只扫描原文数值；字符串（包括转义引号）不参与检查，不代替 JSON 语法与
/// typed 字段解码。整数 token 没有下溢可能，保持由字段的整数解码器校验，避免把整数
/// 精度无端限制为 binary32。调用方应先确认 JSON 格式和类型，再调用本函数。
pub fn validate_f32_tokens(json: &[u8]) -> Result<(), JsonError> {
    let mut at = 0;
    while at < json.len() {
        match json[at] {
            b'"' => {
                at += 1;
                while at < json.len() {
                    match json[at] {
                        b'\\' => at += 2,
                        b'"' => {
                            at += 1;
                            break;
                        }
                        _ => at += 1,
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = at;
                while at < json.len()
                    && matches!(json[at], b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E')
                {
                    at += 1;
                }
                let token = &json[start..at];
                if token.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
                    let token = core::str::from_utf8(token).expect("ASCII 数值 token");
                    match parse_f32(token) {
                        Ok(_) => {}
                        Err(FloatParseError::Underflow) => {
                            return Err(JsonError::NumberUnderflow { offset: start });
                        }
                        Err(FloatParseError::NonFinite) => return Err(JsonError::NonFiniteNumber),
                        // typed JSON 解码器负责语法，不在扫描器复制通用解析器。
                        Err(FloatParseError::InvalidSyntax) => {}
                    }
                }
            }
            _ => at += 1,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_quantizes_once_and_keeps_subnormal_and_signed_zero() {
        assert_eq!(
            parse_f32("1.000000059604644775390625000001")
                .expect("合法测试输入")
                .to_bits(),
            0x3f800001
        );
        assert_eq!(
            parse_f32("1.000000059604644775390624999999")
                .expect("合法测试输入")
                .to_bits(),
            0x3f800000
        );
        assert_eq!(
            parse_f32("1.401298464324817e-45")
                .expect("合法测试输入")
                .to_bits(),
            1
        );
        assert_eq!(
            parse_f32("-0e999").expect("合法测试输入").to_bits(),
            0x80000000
        );
        for token in ["1e-100", "-0.00001e-999"] {
            assert_eq!(parse_f32(token), Err(FloatParseError::Underflow));
        }
        for token in ["NaN", "inf", "1e999"] {
            assert_eq!(parse_f32(token), Err(FloatParseError::NonFinite));
        }
        assert_eq!(parse_f32("abc"), Err(FloatParseError::InvalidSyntax));
    }

    #[test]
    fn scan_skips_strings_and_leaves_integer_domain_to_typed_decoder() {
        assert_eq!(
            validate_f32_tokens(
                br#"{"escaped":"\\\"1e-999", "integer":18446744073709551615,"zero":0e999}"#
            ),
            Ok(())
        );
        assert_eq!(
            validate_f32_tokens(br#"{"gain":1e-999}"#),
            Err(JsonError::NumberUnderflow { offset: 8 })
        );
    }

    #[test]
    fn tolerance_uses_a_finite_absolute_and_relative_budget() {
        assert!(within_tolerance(100.005, 100.0, 0.001, 0.0001));
        assert!(!within_tolerance(100.02, 100.0, 0.001, 0.0001));
        assert!(within_tolerance(-0.0, 0.0, 0.0, 0.0));
        assert!(within_tolerance(f32::MAX, f32::MAX, 0.0, 0.0));
    }

    #[test]
    fn tolerance_rejects_nonfinite_negative_and_overflowed_budgets() {
        for (actual, expected, absolute, relative) in [
            (f32::NAN, 1.0, 0.0, 0.0),
            (1.0, f32::INFINITY, 0.0, 0.0),
            (1.0, 1.0, -0.001, 0.0),
            (0.0, f32::MAX, 0.0, 2.0),
            (f32::MAX, -f32::MAX, 0.0, 0.0),
        ] {
            assert!(!within_tolerance(actual, expected, absolute, relative));
        }
    }
}
