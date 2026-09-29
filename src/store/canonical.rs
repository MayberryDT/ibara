//! Canonical JSON and request fingerprints, byte-for-byte compatible with the
//! TypeScript controller (`controller/src/journal.ts:118-128`, `:192-200`).
//!
//! Rules:
//! - `null`, booleans, numbers and strings are written as `JSON.stringify` writes them.
//!   Numbers use ECMAScript `Number.prototype.toString` on the IEEE double
//!   (`1` not `1.0`, `1e+21`, `-0` → `0`); strings escape only `"`, `\`, and
//!   U+0000–U+001F (short forms for `\b \f \n \r \t`, otherwise lowercase `\u00xx`).
//! - Arrays: elements joined by `,`, no whitespace.
//! - Objects: keys sorted by UTF-16 code units (JavaScript `Array.prototype.sort`).
//!   A JavaScript `undefined` value was emitted as `null`; Rust callers pass
//!   `Value::Null` for fields the TypeScript read as `undefined`.
//!
//! The key order of `serde_json::Map` is never relied on (the crate enables
//! `preserve_order`), keys are always sorted here.

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::cmp::Ordering;
use std::fmt::{self, Write};

type HmacSha256 = Hmac<Sha256>;

/// The canonical JSON text of `value` (`canonicalize`, journal.ts:118).
pub fn canonicalize(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out).expect("writing to a String cannot fail");
    out
}

/// Write the canonical JSON text of `value` into `out`.
pub fn write_canonical<W: Write>(value: &Value, out: &mut W) -> fmt::Result {
    match value {
        Value::Null => out.write_str("null"),
        Value::Bool(b) => out.write_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64() {
            Some(f) => write_js_number(f, out),
            None => out.write_str("null"),
        },
        Value::String(s) => write_js_string(s, out),
        Value::Array(items) => {
            out.write_char('[')?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.write_char(',')?;
                }
                write_canonical(item, out)?;
            }
            out.write_char(']')
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| utf16_cmp(a, b));
            out.write_char('{')?;
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.write_char(',')?;
                }
                write_js_string(key, out)?;
                out.write_char(':')?;
                write_canonical(&map[key.as_str()], out)?;
            }
            out.write_char('}')
        }
    }
}

/// Compare two strings the way JavaScript's default sort does: by UTF-16 code units.
pub fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `JSON.stringify(string)`.
pub fn write_js_string<W: Write>(s: &str, out: &mut W) -> fmt::Result {
    out.write_char('"')?;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        let escape: Option<&str> = match ch {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\u{08}' => Some("\\b"),
            '\u{0c}' => Some("\\f"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            c if (c as u32) < 0x20 => None,
            _ => continue,
        };
        out.write_str(&s[start..i])?;
        match escape {
            Some(e) => out.write_str(e)?,
            None => write!(out, "\\u{:04x}", ch as u32)?,
        }
        start = i + ch.len_utf8();
    }
    out.write_str(&s[start..])?;
    out.write_char('"')
}

/// ECMAScript `Number::toString(x)` for a double (ECMA-262 §6.1.6.1.20).
pub fn write_js_number<W: Write>(x: f64, out: &mut W) -> fmt::Result {
    if x.is_nan() || x.is_infinite() {
        // JSON.stringify(NaN/Infinity) is "null".
        return out.write_str("null");
    }
    if x == 0.0 {
        return out.write_char('0');
    }
    if x < 0.0 {
        out.write_char('-')?;
    }
    // Rust's `{:e}` without precision prints the shortest round-trip digits,
    // which is the digit string ECMAScript requires (k as small as possible).
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp.parse().expect("{:e} exponent is an integer");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp + 1;
    if k <= n && n <= 21 {
        out.write_str(&digits)?;
        for _ in 0..(n - k) {
            out.write_char('0')?;
        }
        Ok(())
    } else if 0 < n && n <= 21 {
        let (int, frac) = digits.split_at(n as usize);
        write!(out, "{int}.{frac}")
    } else if -6 < n && n <= 0 {
        out.write_str("0.")?;
        for _ in 0..(-n) {
            out.write_char('0')?;
        }
        out.write_str(&digits)
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let (first, rest) = digits.split_at(1);
        if rest.is_empty() {
            write!(out, "{first}e{sign}{}", (n - 1).abs())
        } else {
            write!(out, "{first}.{rest}e{sign}{}", (n - 1).abs())
        }
    }
}

struct MacWriter<'a>(&'a mut HmacSha256);

impl Write for MacWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.update(s.as_bytes());
        Ok(())
    }
}

/// `hex(HMAC-SHA256(secret, UTF-8(canonicalize(value))))` (journal.ts:192-194).
/// The canonical text is streamed into the MAC, never materialised.
pub fn fingerprint(secret: &[u8], value: &Value) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    write_canonical(value, &mut MacWriter(&mut mac)).expect("MAC writer cannot fail");
    hex_encode(&mac.finalize().into_bytes())
}

/// `fingerprintsEqual` (journal.ts:196-200): both hex-decoded as Node's `Buffer.from(x, 'hex')`
/// does, equal non-zero length, constant-time comparison.
pub fn fingerprints_equal(left: &str, right: &str) -> bool {
    let a = node_hex_decode(left);
    let b = node_hex_decode(right);
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Lowercase hex, as Node's `digest('hex')` and `toString('hex')`.
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

/// Node's `Buffer.from(text, 'hex')`: decodes byte pairs until the first pair
/// that is not valid hex, and ignores a trailing odd character.
pub fn node_hex_decode(text: &str) -> Vec<u8> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        match (nibble(pair[0]), nibble(pair[1])) {
            (Some(h), Some(l)) => out.push((h << 4) | l),
            _ => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("testdata/canonical_vectors.json");

    fn vectors() -> (Vec<u8>, Vec<Value>) {
        let doc: Value = serde_json::from_str(VECTORS).unwrap();
        let secret = node_hex_decode(doc["secret_hex"].as_str().unwrap());
        (secret, doc["vectors"].as_array().unwrap().clone())
    }

    // Failure cases, written first:
    // - number formatting differs from ECMAScript (exponent forms, -0, integers beyond 2^53)
    // - string escaping differs from JSON.stringify (controls, U+2028, non-ASCII, emoji)
    // - key order is code-point or insertion order rather than UTF-16 order
    // - undefined-valued fields are dropped instead of written as null
    // - the HMAC differs even when the canonical text agrees

    #[test]
    fn canonical_text_matches_node_vectors() {
        let (_, vectors) = vectors();
        assert!(vectors.len() > 50);
        for v in &vectors {
            let text = v["text"].as_str().unwrap();
            let value: Value = serde_json::from_str(text).unwrap();
            assert_eq!(canonicalize(&value), v["canonical"].as_str().unwrap(), "input {text}");
        }
    }

    #[test]
    fn fingerprints_match_node_vectors() {
        let (secret, vectors) = vectors();
        for v in &vectors {
            let value: Value = serde_json::from_str(v["text"].as_str().unwrap()).unwrap();
            assert_eq!(fingerprint(&secret, &value), v["fingerprint"].as_str().unwrap(), "input {}", v["text"]);
        }
    }

    #[test]
    fn key_order_is_utf16_not_code_point_or_insertion() {
        // U+FF61 sorts after U+1F600 by code point but before it in UTF-16
        // (0xD83D < 0xFF61 is false: surrogate 0xD83D is smaller), so the emoji comes first.
        let value = serde_json::json!({ "\u{ff61}": 1, "😀": 2, "a": 3 });
        assert_eq!(canonicalize(&value), "{\"a\":3,\"😀\":2,\"｡\":1}");
    }

    #[test]
    fn fingerprint_comparison_follows_node_hex_rules() {
        assert!(fingerprints_equal("abcd", "ABCD"));
        assert!(!fingerprints_equal("", ""));
        assert!(!fingerprints_equal("zz", "zz"));
        assert!(!fingerprints_equal("abcd", "abce"));
        assert!(!fingerprints_equal("abcd", "ab"));
    }
}
