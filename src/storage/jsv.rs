//! JavaScript value semantics the TypeScript storage relied on implicitly:
//! truthiness (`a || b`), `String(x)`, `Number(x)`, `parseInt`, UTF-16 string
//! lengths and `slice`, and the few regular expressions, written out by hand so
//! the port needs no regex engine.

use serde_json::{Map, Value};

/// JavaScript truthiness of an optional JSON value (`undefined` is `None`).
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `String(v)` for a present value.
pub fn to_js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|x| if x.is_null() { String::new() } else { to_js_string(x) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `String(obj[key] || fallback)`.
pub fn str_or(obj: &Value, key: &str, fallback: &str) -> String {
    match obj.get(key) {
        v @ Some(inner) if truthy(v) => to_js_string(inner),
        _ => fallback.to_string(),
    }
}

/// `Number(v)` (`undefined` is NaN).
pub fn to_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => string_to_number(s),
        Some(Value::Array(items)) => match items.as_slice() {
            [] => 0.0,
            [one] => string_to_number(&to_js_string(one)),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// `Number(obj[key] || fallback)`.
pub fn num_or(obj: &Value, key: &str, fallback: f64) -> f64 {
    let v = obj.get(key);
    if truthy(v) { to_number(v) } else { fallback }
}

fn string_to_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).map(|n| n as f64).unwrap_or(f64::NAN);
    }
    if t.chars().any(|c| !(c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))) {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Math.min(hi, Math.max(lo, x))` with NaN meaning "use the default".
pub fn clamp_or(x: f64, lo: f64, hi: f64, default: f64) -> f64 {
    if x.is_nan() { default } else { x.clamp(lo, hi) }
}

/// ECMAScript `Number::toString` for the values storage produces (integers and
/// ordinary decimals).
pub fn number_string(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if f == f.trunc() && f.abs() < 1e21 {
        format!("{}", f as i128)
    } else {
        format!("{f}")
    }
}

/// `Number.parseInt(s, 10) || 0` for a continuation cursor, never negative.
pub fn parse_int_or_zero(s: &str) -> usize {
    let t = s.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    if neg || end == 0 {
        return 0;
    }
    digits[..end].parse::<usize>().unwrap_or(usize::MAX)
}

/// `Number.isSafeInteger(v)` on a JSON value, returning the integer.
pub fn safe_integer(v: Option<&Value>) -> Option<i64> {
    const MAX: f64 = 9_007_199_254_740_991.0;
    let f = v?.as_f64()?;
    (f == f.trunc() && f.abs() <= MAX).then_some(f as i64)
}

/// Strict equality `v === n` between a JSON value and a number.
pub fn strict_eq_num(v: Option<&Value>, n: f64) -> bool {
    matches!(v, Some(Value::Number(x)) if x.as_f64() == Some(n))
}

/// JavaScript `string.length` (UTF-16 code units).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `text.slice(0, max)` in UTF-16 code units, never splitting a surrogate pair.
pub fn clip(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &text[..i];
        }
    }
    text
}

/// JavaScript default `Array.prototype.sort` order for strings (UTF-16 code units).
pub fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `/^[A-Za-z0-9_.:-]+$/` (procedures.ts:6).
pub fn id_re(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// `/^[a-z][a-z0-9_-]{0,63}$/`: principals, operator ids and file-root ids.
pub fn principal_re(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
}

/// `/^[a-f0-9]{64}$/`.
pub fn sha256_re(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// REL_RE (procedures.ts:7):
/// `^(?!\/)(?!.*(?:^|\/)\.\.(?:\/|$))(?!.*\\)(?!.*[\u0000-\u001f]).+$`.
/// `.` excludes line terminators, so U+2028/U+2029 also fail.
pub fn rel_re(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.split('/').any(|part| part == "..")
        && !s.contains('\\')
        && !s.chars().any(|c| (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}')
}

/// Strict padded base64 (`/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/`).
pub fn strict_base64_re(s: &str) -> bool {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(4) {
        return false;
    }
    let alpha = |c: &u8| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/';
    let pad = b.iter().rev().take_while(|c| **c == b'=').count();
    pad <= 2 && b[..b.len() - pad].iter().all(alpha)
}

/// Node's `Buffer.from(s, 'base64')`: accepts both alphabets, skips characters
/// outside them, stops at the first `=`, and tolerates missing padding.
pub fn lenient_base64(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// FORBIDDEN_REF (procedures.ts:17), case-insensitive:
/// `\b(?:el|element|frame|obs|observation)[_-]?ref\b|\bel_[A-Za-z0-9]+|\bframe_[A-Za-z0-9]+`.
pub fn forbidden_ref(s: &str) -> bool {
    let b = s.to_ascii_lowercase().into_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    for i in 0..b.len() {
        if i > 0 && word(b[i - 1]) {
            continue;
        }
        let rest = &b[i..];
        for prefix in [&b"el"[..], b"element", b"frame", b"obs", b"observation"] {
            let Some(after) = rest.strip_prefix(prefix) else { continue };
            let seps: &[&[u8]] = &[b"", b"_", b"-"];
            for sep in seps {
                if let Some(tail) = after.strip_prefix(*sep).and_then(|t| t.strip_prefix(&b"ref"[..]))
                    && tail.first().is_none_or(|c| !word(*c))
                {
                    return true;
                }
            }
        }
        for prefix in [&b"el_"[..], b"frame_"] {
            if let Some(tail) = rest.strip_prefix(prefix)
                && tail.first().is_some_and(u8::is_ascii_alphanumeric)
            {
                return true;
            }
        }
    }
    false
}

/// POSIX `path.normalize` (Node), used to require normalized absolute paths.
pub fn posix_normalize(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let absolute = p.starts_with('/');
    let trailing = p.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = parts.join("/");
    if out.is_empty() && !absolute {
        out.push('.');
    }
    if trailing && !out.is_empty() {
        out.push('/');
    }
    if absolute { format!("/{out}") } else { out }
}

/// POSIX `path.basename` (Node) without an extension argument.
pub fn basename(p: &str) -> &str {
    let trimmed = p.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// `{...base, ...over}`: existing keys keep their position, new keys append.
pub fn spread(base: &Map<String, Value>, over: &Map<String, Value>) -> Map<String, Value> {
    let mut out = base.clone();
    for (k, v) in over {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Lenient `Date.parse` for ISO-8601 strings: date-only (UTC) or date-time with
/// `Z` or a numeric offset. Offset-less date-times (JavaScript local time) and
/// non-ISO forms are refused.
pub fn parse_iso_millis(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() == 10 {
        return crate::ids::millis_from_iso(&format!("{s}T00:00:00.000Z"));
    }
    let (body, offset_ms) = if let Some(body) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        (body, 0i64)
    } else {
        let tail = s.get(s.len().checked_sub(6)?..)?;
        let sign = match tail.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let (h, m) = tail[1..].split_once(':')?;
        let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
        (&s[..s.len() - 6], sign * (h * 60 + m) * 60_000)
    };
    let (date, time) = body.split_once('T').or_else(|| body.split_once('t'))?;
    let time = if time.len() == 5 { format!("{time}:00") } else { time.to_string() };
    let (hms, frac) = time.split_once('.').unwrap_or((&time, "0"));
    if date.len() != 10 || hms.len() != 8 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms = crate::ids::millis_from_iso(&format!("{date}T{hms}.{frac}Z"))?;
    Some(ms - offset_ms)
}
