//! 极简 XML plist 解析器（只吃 macOS `diskutil -plist` 的输出）。
//!
//! 为什么自己写：项目坚持零第三方依赖，而 plist 只需要 dict/array/string/integer
//! 这四种子集就够枚举磁盘了。完整 plist 规范（base64 data、date）也一并支持。

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub enum Value {
    Dict(Vec<(String, Value)>),
    Array(Vec<Value>),
    Str(String),
    Int(i64),
    Real(f64),
    Bool(bool),
    Data(Vec<u8>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Int(i) => Some(*i as u64),
            Value::Real(f) => Some(*f as u64),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Str(s) => match s.as_str() {
                "YES" | "true" | "1" => Some(true),
                "NO" | "false" | "0" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Self { s: s.as_bytes(), i: 0 }
    }

    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }

    fn peek_char(&self) -> u8 {
        if self.eof() {
            0
        } else {
            self.s[self.i]
        }
    }

    /// 跳过空白、注释、声明、DOCTYPE
    fn skip_noise(&mut self) {
        loop {
            while !self.eof() && self.peek_char().is_ascii_whitespace() {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"<!--") {
                if let Some(p) = find(self.s, self.i, b"-->") {
                    self.i = p + 3;
                    continue;
                }
                self.i = self.s.len();
                return;
            }
            if self.s[self.i..].starts_with(b"<?") {
                if let Some(p) = find(self.s, self.i, b"?>") {
                    self.i = p + 2;
                    continue;
                }
                self.i = self.s.len();
                return;
            }
            if self.s[self.i..].starts_with(b"<!DOCTYPE") {
                if let Some(p) = find(self.s, self.i, b">") {
                    self.i = p + 1;
                    continue;
                }
                self.i = self.s.len();
                return;
            }
            return;
        }
    }

    /// 读一个标签，返回 (是否闭合标签, 标签名, 是否自闭合 `<x/>`)
    fn read_tag(&mut self) -> Result<(bool, String, bool)> {
        self.skip_noise();
        if self.peek_char() != b'<' {
            return Err(Error::Format(format!(
                "plist: 期望标签，实际偏移 {}",
                self.i
            )));
        }
        self.i += 1;
        let closing = self.peek_char() == b'/';
        if closing {
            self.i += 1;
        }
        let start = self.i;
        while !self.eof() && !matches!(self.peek_char(), b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
        let name = String::from_utf8_lossy(&self.s[start..self.i]).to_string();
        // 跳到 '>' 之后，并记录是否自闭合
        let mut self_closed = false;
        while !self.eof() && self.peek_char() != b'>' {
            if self.peek_char() == b'/' {
                self_closed = true;
            }
            self.i += 1;
        }
        if !self.eof() {
            self.i += 1;
        }
        Ok((closing, name, self_closed))
    }

    /// 读标签内部的文本（含实体反转义）
    fn read_text(&mut self, tag: &str) -> Result<String> {
        let start = self.i;
        let end_marker = format!("</{tag}>");
        let pos = find(self.s, self.i, end_marker.as_bytes())
            .ok_or_else(|| Error::Format(format!("plist: <{tag}> 未闭合")))?;
        let raw = &self.s[start..pos];
        self.i = pos + end_marker.len();
        Ok(unescape(&String::from_utf8_lossy(raw)))
    }

    fn parse_value(&mut self) -> Result<Value> {
        self.skip_noise();
        if self.eof() {
            return Err(Error::Format("plist: 意外的文件结束".into()));
        }
        let (closing, name, self_closed) = self.read_tag()?;
        if closing {
            return Err(Error::Format(format!("plist: 意外的闭合标签 </{name}>")));
        }
        // 自闭合标签：<array/>、<dict/>、<string/>
        if self_closed {
            return Ok(match name.as_str() {
                "dict" => Value::Dict(Vec::new()),
                "array" => Value::Array(Vec::new()),
                "string" => Value::Str(String::new()),
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "integer" => Value::Int(0),
                "real" => Value::Real(0.0),
                "data" => Value::Data(Vec::new()),
                _ => Value::Str(String::new()),
            });
        }
        match name.as_str() {
            "dict" => {
                let mut kv = Vec::new();
                loop {
                    self.skip_noise();
                    if self.eof() {
                        return Err(Error::Format("plist: <dict> 未闭合".into()));
                    }
                    if self.s[self.i..].starts_with(b"</dict>") {
                        self.i += 7;
                        break;
                    }
                    let (c1, n1, _) = self.read_tag()?;
                    if c1 && n1 == "dict" {
                        break;
                    }
                    if n1 != "key" {
                        return Err(Error::Format(format!("plist: dict 内期望 <key>，遇到 <{n1}>")));
                    }
                    let k = self.read_text("key")?;
                    let v = self.parse_value()?;
                    kv.push((k, v));
                }
                Ok(Value::Dict(kv))
            }
            "array" => {
                let mut items = Vec::new();
                loop {
                    self.skip_noise();
                    if self.eof() {
                        return Err(Error::Format("plist: <array> 未闭合".into()));
                    }
                    if self.s[self.i..].starts_with(b"</array>") {
                        self.i += 8;
                        break;
                    }
                    items.push(self.parse_value()?);
                }
                Ok(Value::Array(items))
            }
            "string" => Ok(Value::Str(self.read_text("string")?)),
            "integer" => {
                let t = self.read_text("integer")?;
                t.trim()
                    .parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| Error::Format(format!("plist: 非法整数 {t:?}")))
            }
            "real" => {
                let t = self.read_text("real")?;
                t.trim()
                    .parse::<f64>()
                    .map(Value::Real)
                    .map_err(|_| Error::Format(format!("plist: 非法实数 {t:?}")))
            }
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "data" => {
                let t = self.read_text("data")?;
                let clean: String = t.chars().filter(|c| !c.is_whitespace()).collect();
                Ok(Value::Data(base64_decode(&clean)))
            }
            "date" => Ok(Value::Str(self.read_text("date")?)),
            other => {
                // 未知标签：尝试跳过内容
                let _ = self.read_text(other)?;
                Ok(Value::Str(String::new()))
            }
        }
    }
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'&' {
            if let Some(semi) = s[i..].find(';') {
                let ent = &s[i + 1..i + semi];
                let rep = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => {
                        if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                        } else if let Some(dec) = ent.strip_prefix('#') {
                            dec.parse::<u32>().ok().and_then(char::from_u32)
                        } else {
                            None
                        }
                    }
                };
                if let Some(c) = rep {
                    out.push(c);
                    i += semi + 1;
                    continue;
                }
            }
        }
        let ch_end = next_char_boundary(s, i);
        out.push_str(&s[i..ch_end]);
        i = ch_end;
    }
    out
}

fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j.min(s.len())
}

fn base64_decode(s: &str) -> Vec<u8> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    let mut k = 0;
    while k < 64 {
        lut[T[k] as usize] = k as u8;
        k += 1;
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = lut[c as usize];
        if v == 255 {
            continue;
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

pub fn parse(text: &str) -> Result<Value> {
    let mut p = Parser::new(text);
    p.skip_noise();
    // 跳过 <plist ...> 包装层
    if p.s[p.i..].starts_with(b"<plist") {
        let _ = p.read_tag()?;
    }
    let v = p.parse_value()?;
    Ok(v)
}
