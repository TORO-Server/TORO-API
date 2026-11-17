//! JavaScript と同じ結果にするための小さな道具。
//!
//! 移管前の実装 (Node) の挙動をそのまま再現するためのもの: `Number()` の変換、`decodeURIComponent`、
//! `Date#toUTCString`、`JSON.stringify` の数の書き方、UTF-16 での文字数。

use serde_json::Value;

/// JavaScript の `Number(text)`。数にならなければ NaN
pub fn number(text: &str) -> f64 {
    // 前後の空白 (JavaScript の WhiteSpace と LineTerminator) は無視される
    let t = text.trim_matches(|c: char| {
        c.is_whitespace() || c == '\u{feff}' // BOM も空白として扱われる
    });
    if t.is_empty() {
        return 0.0;
    }
    let radix_digits = |digits: &str, radix: u32| -> f64 {
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return f64::NAN;
        }
        digits.chars().fold(0.0, |acc, c| {
            acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
        })
    };
    let lower = t.get(..2).map(str::to_ascii_lowercase);
    match lower.as_deref() {
        Some("0x") => return radix_digits(&t[2..], 16),
        Some("0o") => return radix_digits(&t[2..], 8),
        Some("0b") => return radix_digits(&t[2..], 2),
        _ => {}
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    if unsigned == "Infinity" {
        return if t.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    // Rust の f64 の読み取りは "inf" "nan" "infinity" も通すので、10 進の形だけを先に確かめる
    if !is_decimal_literal(unsigned) {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// 符号なしの 10 進数 (`12`、`1.5`、`.5`、`5.`、`1e3`、`1.5E-3`)
fn is_decimal_literal(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < bytes.len() && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    let int = digits(&mut i);
    let mut frac = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        frac = digits(&mut i);
    }
    if int + frac == 0 {
        return false;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == bytes.len()
}

/// `Number.isSafeInteger(n)` のあとの整数。安全な整数でなければ None
pub fn safe_integer(n: f64) -> Option<i64> {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    (n.is_finite() && n.fract() == 0.0 && n.abs() <= MAX_SAFE).then_some(n as i64)
}

/// JavaScript の `Number(value)` (JSON の値から)。undefined は NaN
pub fn value_number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => number(s),
        Some(Value::Array(_) | Value::Object(_)) => f64::NAN,
    }
}

/// `a !== b` の否定 (JSON の値どうし。数は値で比べる)
pub fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// JavaScript の `decodeURIComponent`。壊れた `%` や、UTF-8 として不正な並びなら None
pub fn decode_uri_component(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// JavaScript の `decodeURI`。`decodeURIComponent` と違い、予約文字 (`;/?:@&=+$,#`) になる `%XX` はそのまま残す。
/// 壊れた `%` や、UTF-8 として不正な並びなら None
pub fn decode_uri(input: &str) -> Option<String> {
    const RESERVED: &[u8] = b";/?:@&=+$,#";
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            let byte = (hi * 16 + lo) as u8;
            if byte < 0x80 && RESERVED.contains(&byte) {
                out.extend_from_slice(&bytes[i..i + 3]);
            } else {
                out.push(byte);
            }
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// UTF-16 の長さ (JavaScript の `string.length`)
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `new Date(ms).toUTCString()` (例: `Thu, 23 Jul 2026 11:20:52 GMT`)
pub fn utc_string(ms: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // days_from_civil の逆 (Howard Hinnant の方法)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let weekday = (days + 4).rem_euclid(7); // 1970-01-01 は木曜
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[weekday as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `JSON.stringify(value)`。数の書き方を JavaScript に合わせる (`64.0` は `64`)。
/// 辞書の順序は、読んだときのまま (serde_json の preserve_order)
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_text(n)),
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

fn number_text(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f == 0.0 {
        return "0".into(); // -0 も 0 と書く
    }
    let abs = f.abs();
    if (1e-6..1e21).contains(&abs) {
        // Rust は 10 進で最短の表記 (指数なし)。整数なら小数点も付かない
        return format!("{f}");
    }
    // 指数表記: JavaScript は `1e+21`、`1.5e-7`
    let text = format!("{f:e}");
    match text.split_once('e') {
        Some((mantissa, exp)) if !exp.starts_with('-') => format!("{mantissa}e+{exp}"),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn number_follows_javascript() {
        assert_eq!(number(""), 0.0);
        assert_eq!(number("  12 "), 12.0);
        assert_eq!(number("0x10"), 16.0);
        assert_eq!(number("0b101"), 5.0);
        assert_eq!(number("0o17"), 15.0);
        assert_eq!(number("1e3"), 1000.0);
        assert_eq!(number("-5"), -5.0);
        assert_eq!(number(".5"), 0.5);
        assert_eq!(number("5."), 5.0);
        assert!(number("1_0").is_nan());
        assert!(number("abc").is_nan());
        assert!(number("inf").is_nan());
        assert!(number("nan").is_nan());
        assert!(number("1e").is_nan());
        assert!(number("0x").is_nan());
        assert!(number("-0x10").is_nan());
        assert_eq!(number("Infinity"), f64::INFINITY);
        assert_eq!(number("-Infinity"), f64::NEG_INFINITY);
    }

    #[test]
    fn safe_integers() {
        assert_eq!(safe_integer(5.0), Some(5));
        assert_eq!(safe_integer(-3.0), Some(-3));
        assert_eq!(safe_integer(1.5), None);
        assert_eq!(safe_integer(f64::NAN), None);
        assert_eq!(safe_integer(f64::INFINITY), None);
        assert_eq!(
            safe_integer(9_007_199_254_740_991.0),
            Some(9_007_199_254_740_991)
        );
        assert_eq!(safe_integer(9_007_199_254_740_992.0), None);
    }

    #[test]
    fn decode_uri_component_is_strict() {
        assert_eq!(decode_uri_component("a%20b").as_deref(), Some("a b"));
        assert_eq!(decode_uri_component("%E3%81%82").as_deref(), Some("あ"));
        assert_eq!(decode_uri_component("a+b").as_deref(), Some("a+b"));
        assert_eq!(decode_uri_component("%"), None);
        assert_eq!(decode_uri_component("%zz"), None);
        assert_eq!(decode_uri_component("%E3%81"), None);
        assert_eq!(decode_uri_component("%FF"), None);
    }

    #[test]
    fn utc_string_matches_javascript() {
        assert_eq!(utc_string(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // golden の Last-Modified
        assert_eq!(
            utc_string(1_784_805_652_212),
            "Thu, 23 Jul 2026 11:20:52 GMT"
        );
        assert_eq!(utc_string(951_782_400_000), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(utc_string(-1000), "Wed, 31 Dec 1969 23:59:59 GMT");
    }

    #[test]
    fn stringify_matches_javascript_numbers() {
        let v: Value = serde_json::from_str(
            r#"{"a":64.0,"b":1.5,"c":-2,"d":1e21,"e":1.5e-7,"f":0.000001,"g":[true,null,"x\n"]}"#,
        )
        .unwrap();
        assert_eq!(
            stringify(&v),
            r#"{"a":64,"b":1.5,"c":-2,"d":1e+21,"e":1.5e-7,"f":0.000001,"g":[true,null,"x\n"]}"#
        );
        assert_eq!(stringify(&json!({"z": 1, "a": 2})), r#"{"z":1,"a":2}"#);
    }

    #[test]
    fn same_value_compares_numbers_by_value() {
        assert!(same_value(&json!(1), &json!(1.0)));
        assert!(!same_value(&json!(1), &json!(2)));
        assert!(same_value(&Value::Null, &Value::Null));
        assert!(!same_value(&Value::Null, &json!(0)));
    }
}
