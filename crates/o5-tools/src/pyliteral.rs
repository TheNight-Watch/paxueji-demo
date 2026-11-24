//! Python 字面量解析（`ast.literal_eval` 的可用子集）：单/双/三引号字符串
//! （r/u 前缀、常用转义）、十/十六/八/二进制数字、True/False/None、
//! dict/list/tuple、空白与 `#` 注释。只解字面量，绝不执行代码。

use serde_json::{Map, Number, Value};

pub(crate) fn parse_literal(s: &str) -> Result<Value, String> {
    let mut p = Parser { chars: s.chars().collect(), pos: 0 };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err("多余内容".to_string());
    }
    Ok(v)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// 空白与 `#` 注释（Python 括号内允许隐式换行 + 注释）。
    fn skip_ws(&mut self) {
        loop {
            while self.peek().is_some_and(|c| c.is_whitespace()) {
                self.pos += 1;
            }
            if self.peek() == Some('#') {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.parse_dict(),
            Some('[') => self.parse_list(),
            Some('(') => self.parse_tuple(),
            Some('\'') | Some('"') => Ok(Value::String(self.parse_string(false)?)),
            Some(c) if c.is_ascii_digit() || c == '+' || c == '-' || c == '.' => {
                self.parse_number()
            }
            Some(_) => self.parse_name_value(),
            None => Err("意外的输入结束".to_string()),
        }
    }

    /// True/False/None，或带前缀的字符串字面量（r/u 可，b/f 不支持）。
    fn parse_name_value(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        let word: String = self.chars[start..self.pos].iter().collect();
        match word.as_str() {
            "True" => Ok(Value::Bool(true)),
            "False" => Ok(Value::Bool(false)),
            "None" => Ok(Value::Null),
            _ => {
                if word.is_empty()
                    || word.chars().count() > 2
                    || !word.chars().all(|c| matches!(c.to_ascii_lowercase(), 'r' | 'u'))
                {
                    return Err(format!("不支持的字面量: {word}"));
                }
                let raw = word.to_ascii_lowercase().contains('r');
                match self.peek() {
                    Some('\'') | Some('"') => Ok(Value::String(self.parse_string(raw)?)),
                    _ => Err(format!("不支持的字面量: {word}")),
                }
            }
        }
    }

    /// 字典键：只接受（可带 r/u 前缀的）字符串。
    fn parse_key(&mut self) -> Result<String, String> {
        self.skip_ws();
        match self.peek() {
            Some('\'') | Some('"') => self.parse_string(false),
            Some(c) if c.is_alphabetic() || c == '_' => match self.parse_name_value()? {
                Value::String(s) => Ok(s),
                _ => Err("字典键必须是字符串".to_string()),
            },
            _ => Err("字典键必须是字符串".to_string()),
        }
    }

    fn parse_dict(&mut self) -> Result<Value, String> {
        self.bump(); // '{'
        let mut map = Map::new();
        loop {
            self.skip_ws();
            if self.peek() == Some('}') {
                self.bump();
                break;
            }
            let key = self.parse_key()?;
            self.skip_ws();
            if self.bump() != Some(':') {
                return Err("期望 ':'".to_string());
            }
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue, // 允许尾逗号
                Some('}') => break,
                _ => return Err("期望 ',' 或 '}'".to_string()),
            }
        }
        Ok(Value::Object(map))
    }

    fn parse_list(&mut self) -> Result<Value, String> {
        self.bump(); // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(']') {
                self.bump();
                break;
            }
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => break,
                _ => return Err("期望 ',' 或 ']'".to_string()),
            }
        }
        Ok(Value::Array(items))
    }

    /// tuple -> 数组；`(x)` 是加括号的标量本身（不是容器）；`()` 空数组。
    fn parse_tuple(&mut self) -> Result<Value, String> {
        self.bump(); // '('
        self.skip_ws();
        if self.peek() == Some(')') {
            self.bump();
            return Ok(Value::Array(Vec::new()));
        }
        let first = self.parse_value()?;
        self.skip_ws();
        match self.peek() {
            Some(',') => {
                self.bump();
                let mut items = vec![first];
                loop {
                    self.skip_ws();
                    if self.peek() == Some(')') {
                        self.bump();
                        break;
                    }
                    items.push(self.parse_value()?);
                    self.skip_ws();
                    match self.bump() {
                        Some(',') => continue,
                        Some(')') => break,
                        _ => return Err("期望 ',' 或 ')'".to_string()),
                    }
                }
                Ok(Value::Array(items))
            }
            Some(')') => {
                self.bump();
                Ok(first)
            }
            _ => Err("期望 ',' 或 ')'".to_string()),
        }
    }

    fn parse_string(&mut self, raw: bool) -> Result<String, String> {
        let Some(quote) = self.bump() else {
            return Err("字符串未开始".to_string());
        };
        let mut triple = false;
        if self.peek() == Some(quote) {
            self.bump();
            if self.peek() == Some(quote) {
                self.bump();
                triple = true;
            } else {
                return Ok(String::new()); // 空串 ''
            }
        }
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err("字符串未终止".to_string());
            };
            if c == '\\' {
                let Some(e) = self.bump() else {
                    return Err("字符串未终止".to_string());
                };
                if raw {
                    out.push('\\');
                    out.push(e);
                    continue;
                }
                match e {
                    '\n' => {} // 行 continuation
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'a' => out.push('\u{07}'),
                    'b' => out.push('\u{08}'),
                    'f' => out.push('\u{0c}'),
                    'v' => out.push('\u{0b}'),
                    'x' => {
                        let code = self.take_hex(2)?;
                        out.push(char::from_u32(code).ok_or("非法 \\x 转义")?);
                    }
                    'u' => {
                        let code = self.take_hex(4)?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    'U' => {
                        let code = self.take_hex(8)?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                    }
                    '0'..='7' => {
                        let mut v = e.to_digit(8).unwrap_or(0);
                        for _ in 0..2 {
                            match self.peek().and_then(|c| c.to_digit(8)) {
                                Some(d) => {
                                    self.bump();
                                    v = v * 8 + d;
                                }
                                None => break,
                            }
                        }
                        out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                    }
                    other => {
                        out.push('\\'); // 未知转义保留反斜杠（Python 行为）
                        out.push(other);
                    }
                }
            } else if c == quote {
                if triple {
                    if self.peek() == Some(quote) && self.chars.get(self.pos + 1) == Some(&quote) {
                        self.bump();
                        self.bump();
                        break;
                    }
                    out.push(quote);
                } else {
                    break;
                }
            } else if c == '\n' && !triple {
                return Err("字符串中的裸换行".to_string());
            } else {
                out.push(c);
            }
        }
        Ok(out)
    }

    fn take_hex(&mut self, n: usize) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..n {
            let Some(c) = self.peek() else {
                return Err("转义数字不足".to_string());
            };
            let Some(d) = c.to_digit(16) else {
                return Err("转义数字不足".to_string());
            };
            self.bump();
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let mut sign = String::new();
        if matches!(self.peek(), Some('+') | Some('-')) {
            if let Some(c) = self.bump() {
                sign.push(c);
            }
        }
        // 0x / 0o / 0b
        if self.peek() == Some('0') {
            let base = match self.chars.get(self.pos + 1).map(|c| c.to_ascii_lowercase()) {
                Some('x') => Some(16),
                Some('o') => Some(8),
                Some('b') => Some(2),
                _ => None,
            };
            if let Some(base) = base {
                self.pos += 2;
                let digits = self.take_radix_digits(base)?;
                if digits.is_empty() {
                    return Err("进制整数缺少数字".to_string());
                }
                let mag = i64::from_str_radix(&digits, base)
                    .map_err(|_| "整数非法/溢出".to_string())?;
                let v = if sign == "-" { -mag } else { mag };
                return Ok(Value::Number(v.into()));
            }
        }
        let int_digits = self.take_dec_digits()?;
        let mut text = int_digits.clone();
        let mut is_float = false;
        if self.peek() == Some('.') {
            self.pos += 1;
            is_float = true;
            let frac = self.take_dec_digits()?;
            if int_digits.is_empty() && frac.is_empty() {
                return Err("非法数字".to_string());
            }
            text.push('.');
            text.push_str(&frac);
        } else if int_digits.is_empty() {
            return Err("非法数字".to_string());
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            let save = self.pos;
            self.pos += 1;
            let mut exp = String::from("e");
            if matches!(self.peek(), Some('+') | Some('-')) {
                if let Some(c) = self.bump() {
                    exp.push(c);
                }
            }
            let exp_digits = self.take_dec_digits()?;
            if exp_digits.is_empty() {
                self.pos = save; // 不是合法指数，交给上层报"多余内容"
            } else {
                is_float = true;
                text.push_str(&exp);
                text.push_str(&exp_digits);
            }
        }
        if is_float {
            let v: f64 = format!("{sign}{text}").parse().map_err(|_| "非法浮点数".to_string())?;
            if !v.is_finite() {
                return Err("非有限浮点数不支持".to_string());
            }
            let n = Number::from_f64(v).ok_or_else(|| "非法浮点数".to_string())?;
            Ok(Value::Number(n))
        } else {
            // 十进制整数字面量不许前导零（"0" 本身除外）
            if int_digits.len() > 1 && int_digits.starts_with('0') {
                return Err("十进制整数字面量不许前导零".to_string());
            }
            let v: i64 = format!("{sign}{int_digits}")
                .parse()
                .map_err(|_| "整数溢出".to_string())?;
            Ok(Value::Number(v.into()))
        }
    }

    fn take_digits(&mut self, pred: impl Fn(char) -> bool) -> Result<String, String> {
        let mut raw = String::new();
        while let Some(c) = self.peek() {
            if c == '_' || pred(c) {
                raw.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if raw.starts_with('_') || raw.ends_with('_') || raw.contains("__") {
            return Err("数字下划线位置非法".to_string());
        }
        Ok(raw.chars().filter(|c| *c != '_').collect())
    }

    fn take_dec_digits(&mut self) -> Result<String, String> {
        self.take_digits(|c| c.is_ascii_digit())
    }

    fn take_radix_digits(&mut self, base: u32) -> Result<String, String> {
        self.take_digits(|c| c.is_digit(base))
    }
}
