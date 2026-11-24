//! Python 语义的小工具：真值 / str() / float() / 截断 / URL 编码 / 数字格式化。

use serde_json::Value;

/// Python 真值：None / False / 0 / 0.0 / "" / [] / {} 为假。
pub(crate) fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(v)`：None->"None"、True/False、数字、字符串原样。
/// （列表/字典会得到 JSON 文本，与 Python 的 repr 略有出入——仅极端脏数据可见。）
pub(crate) fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => py_float_repr(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Python `str(v or default)`：假值取 default。
pub(crate) fn py_or_str(v: Option<&Value>, default: &str) -> String {
    match v {
        Some(x) if py_truthy(x) => py_str(x),
        _ => default.to_string(),
    }
}

/// Python `float(value)`，失败返回 None。
pub(crate) fn py_num_opt(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => py_float_str(s),
        _ => None,
    }
}

/// Python `_num(value, default)`：失败取 default。
pub(crate) fn py_num(v: Option<&Value>, default: f64) -> f64 {
    py_num_opt(v).unwrap_or(default)
}

/// Python `float(s)`（容忍前后空白与 inf/nan；不支持下划线数字串）。
pub(crate) fn py_float_str(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() || t.contains('_') {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Python `s[:n]`（按字符计）。
pub(crate) fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// JSON 字符串字面量（对齐 Python `json.dumps` 默认 ensure_ascii=True 的转义口径，
/// requests 的 `json=` 参数正是这么发的）。
pub(crate) fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) >= 0x7f => {
                let cp = c as u32;
                if cp > 0xffff {
                    // 增补平面：代理对转义（json.dumps 的口径）
                    let v = cp - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (v >> 10),
                        0xdc00 + (v & 0x3ff)
                    ));
                } else {
                    out.push_str(&format!("\\u{:04x}", cp));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// requests 风格 query 编码（quote_plus：空格 '+'，非安全字符 %XX UTF-8 大写）。
pub(crate) fn urlencode(params: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (idx, (k, v)) in params.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        out.push_str(&urlquote_plus(k));
        out.push('=');
        out.push_str(&urlquote_plus(v));
    }
    out
}

fn urlquote_plus(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 5292 -> "5.3 公里"；850 -> "850 米"；5000 -> "5 公里"。
pub(crate) fn fmt_distance(meters: i64) -> String {
    if meters < 1000 {
        return format!("{meters} 米");
    }
    let km = round_half_even(meters as f64 / 100.0) / 10.0; // 保留 1 位小数再去尾零
    let mut s = format!("{km:.1}");
    if s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    format!("{s} 公里")
}

/// 59 -> "不到 1 分钟"；90 -> "2 分钟"；3660 -> "1 小时 1 分钟"；7200 -> "2 小时"。
pub(crate) fn fmt_duration(seconds: i64) -> String {
    if seconds < 60 {
        return "不到 1 分钟".to_string();
    }
    let minutes = round_half_even(seconds as f64 / 60.0) as i64;
    if minutes < 60 {
        return format!("{minutes} 分钟");
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    if rest != 0 {
        format!("{hours} 小时 {rest} 分钟")
    } else {
        format!("{hours} 小时")
    }
}

/// Python `round()` 的银行家舍入（恰为 .5 时取偶数邻居）。
fn round_half_even(x: f64) -> f64 {
    let t = x.trunc();
    let diff = x - t;
    if diff.abs() != 0.5 {
        return x.round();
    }
    let mut r = t;
    if (t as i64) % 2 != 0 {
        r += if x > 0.0 { 1.0 } else { -1.0 };
    }
    r
}

/// Python `str(float)`：整值浮点带 ".0"。
fn py_float_repr(f: f64) -> String {
    if f.is_finite() && f == f.trunc() && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}
