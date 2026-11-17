//! JavaScript の `JSON.stringify(JSON.parse(text))` と同じ書き方で JSON を書く。
//!
//! 移管前 (Node) が作った assets.json と同じバイト列にするため、serde_json の既定の書き方と違うところを合わせる。
//! - 数: 整数の値の小数 (`1.0`) は `1`。指数の書き方 (`1e-7`、`1e+21`) も JavaScript の `Number#toString` と同じ
//! - 連想配列の順: 配列の添字の形 (`"0"`、`"12"`) の鍵が、数の順で先。あとの鍵は入れた順
//!
//! 文字列の書き方 (`\"`、`\\`、`\n`、`\u00xx` の小文字) は serde_json と JavaScript で同じ。

use serde_json::{Number, Value};

/// JavaScript の数の文字列にする (有限の数だけ)
pub fn js_number(f: f64) -> String {
    if f == 0.0 {
        return "0".to_string();
    }
    let negative = f < 0.0;
    // 最短の桁 (往復できる最短) を、`1.2345e-7` の形で得る
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp has an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exp: i32 = exp.parse().expect("exponent is a number");
    let k = digits.len() as i32;
    // 値 = 0.<digits> × 10^n
    let n = exp + 1;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&e.abs().to_string());
    }
    out
}

/// JSON の数 (serde_json) を、JavaScript が書く形にする。2^53 を超える整数は、JavaScript と同じく double に丸める
pub fn number(n: &Number) -> String {
    const SAFE: u64 = 1 << 53;
    if let Some(i) = n.as_i64()
        && i.unsigned_abs() <= SAFE
    {
        return i.to_string();
    }
    if let Some(u) = n.as_u64()
        && u <= SAFE
    {
        return u.to_string();
    }
    js_number(n.as_f64().unwrap_or(0.0))
}

/// 配列の添字の形の鍵か (`"0"`、`"12"`。先頭の 0 や符号が付いたものは違う。2^32 - 2 まで)
fn array_index(key: &str) -> Option<u32> {
    let b = key.as_bytes();
    if b.is_empty() || b.len() > 10 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if b.len() > 1 && b[0] == b'0' {
        return None;
    }
    let v: u64 = key.parse().ok()?;
    (v < u64::from(u32::MAX)).then_some(v as u32)
}

fn string(s: &str, out: &mut Vec<u8>) {
    serde_json::to_writer(&mut *out, s).expect("a string serializes");
}

/// JavaScript の `JSON.stringify` と同じ書き方で、`out` に足す
pub fn write(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => out.extend_from_slice(number(n).as_bytes()),
        Value::String(s) => string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            out.push(b'{');
            let mut first = true;
            let mut put = |key: &str, item: &Value, out: &mut Vec<u8>| {
                if !first {
                    out.push(b',');
                }
                first = false;
                string(key, out);
                out.push(b':');
                write(item, out);
            };
            let mut indexed: Vec<(u32, &str, &Value)> = map
                .iter()
                .filter_map(|(k, v)| array_index(k).map(|i| (i, k.as_str(), v)))
                .collect();
            indexed.sort_by_key(|(i, _, _)| *i);
            for (_, k, v) in &indexed {
                put(k, v, out);
            }
            for (k, v) in map {
                if array_index(k).is_none() {
                    put(k, v, out);
                }
            }
            out.push(b'}');
        }
    }
}

pub fn to_vec(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(text: &str) -> String {
        let v: Value = serde_json::from_str(text).unwrap();
        String::from_utf8(to_vec(&v)).unwrap()
    }

    #[test]
    fn numbers_match_javascript() {
        for (f, want) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-2.5, "-2.5"),
            (0.8, "0.8"),
            (0.0625, "0.0625"),
            (100.0, "100"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (123456789012345680000.0, "123456789012345680000"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
        ] {
            assert_eq!(js_number(f), want, "{f}");
        }
    }

    #[test]
    fn json_is_rewritten_like_stringify() {
        assert_eq!(
            roundtrip(r#"{ "a": 1.0, "b": [2.50, 1e2, -0] }"#),
            r#"{"a":1,"b":[2.5,100,0]}"#
        );
        // 添字の形の鍵が先 (数の順)。あとは入れた順
        assert_eq!(
            roundtrip(r#"{"z":1,"10":2,"particle":3,"2":4,"01":5}"#),
            r#"{"2":4,"10":2,"z":1,"particle":3,"01":5}"#
        );
        // 同じ鍵が重なったら、最初の位置に後の値
        assert_eq!(roundtrip(r#"{"a":1,"b":2,"a":3}"#), r#"{"a":3,"b":2}"#);
        assert_eq!(roundtrip(r#""a\u0001\n\"é""#), r#""a\u0001\n\"é""#);
        assert_eq!(roundtrip("9007199254740993"), "9007199254740992");
    }
}
