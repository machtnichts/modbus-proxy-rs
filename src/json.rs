//! Minimal JSON parser and serialiser.
//!
//! Hand-written so the proxy needs no crate for something this small. Supports
//! the subset any config uses: objects, arrays, strings (with \u escapes),
//! numbers, booleans and null. Tolerates `//` comments? No - strict JSON, so a
//! broken config fails loudly instead of being silently misread.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        let bytes = text.as_bytes();
        let mut p = Parser { b: bytes, i: 0 };
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        if p.i != bytes.len() {
            return Err(format!("trailing data at byte {}", p.i));
        }
        Ok(v)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.get(key),
            _ => None,
        }
    }

    /// Nested lookup: `get_path(&["poll", "ranges"])`.
    pub fn get_path(&self, path: &[&str]) -> Option<&Json> {
        let mut cur = self;
        for key in path {
            cur = cur.get(key)?;
        }
        Some(cur)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            // accept numeric strings: configs edited by hand often quote numbers
            Json::Str(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            // mirrors Python truthiness for the config values we read
            Json::Num(n) => Some(*n != 0.0),
            Json::Str(s) => match s.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" | "on" => Some(true),
                "false" | "no" | "0" | "off" | "" => Some(false),
                _ => None,
            },
            Json::Null => Some(false),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Compact serialisation. Enough for the status endpoints, and it keeps the
    /// serialiser to one screenful of code.
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    let _ = write!(out, "{}", *n as i64);
                } else {
                    let _ = write!(out, "{}", n);
                }
            }
            Json::Str(s) => write_json_string(out, s),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Json::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        let c = *self.b.get(self.i).ok_or("unexpected end of input")?;
        match c {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Json::Str(self.string()?)),
            b't' => self.literal("true", Json::Bool(true)),
            b'f' => self.literal("false", Json::Bool(false)),
            b'n' => self.literal("null", Json::Null),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str, val: Json) -> Result<Json, String> {
        if self.b.len() >= self.i + word.len()
            && &self.b[self.i..self.i + word.len()] == word.as_bytes()
        {
            self.i += word.len();
            Ok(val)
        } else {
            Err(format!("invalid literal at byte {}", self.i))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.i += 1; // {
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Json::Obj(map));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(format!("expected ':' at byte {}", self.i));
            }
            self.i += 1;
            let val = self.value()?;
            map.insert(key, val);
            self.skip_ws();
            match self.b.get(self.i) {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(map));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.i)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.i += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.b.get(self.i) {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.i)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&b'"') {
            return Err(format!("expected string at byte {}", self.i));
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = *self.b.get(self.i).ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("unterminated escape")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self
                                .b
                                .get(self.i..self.i + 4)
                                .ok_or("truncated \\u escape")?;
                            let s = std::str::from_utf8(hex).map_err(|_| "bad hex")?;
                            let cp = u32::from_str_radix(s, 16).map_err(|_| "bad hex")?;
                            self.i += 4;
                            // surrogate pair
                            if (0xD800..0xDC00).contains(&cp)
                                && self.b.get(self.i) == Some(&b'\\')
                                && self.b.get(self.i + 1) == Some(&b'u')
                            {
                                let hex2 = self
                                    .b
                                    .get(self.i + 2..self.i + 6)
                                    .ok_or("truncated surrogate")?;
                                let s2 = std::str::from_utf8(hex2).map_err(|_| "bad hex")?;
                                let lo = u32::from_str_radix(s2, 16).map_err(|_| "bad hex")?;
                                self.i += 6;
                                let combined = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                out.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                            } else {
                                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            }
                        }
                        other => return Err(format!("bad escape \\{}", other as char)),
                    }
                }
                c if c < 0x20 => return Err("control character in string".into()),
                c if c < 0x80 => out.push(c as char),
                _ => {
                    // multi-byte UTF-8: re-decode from the string start
                    self.i -= 1;
                    let rest =
                        std::str::from_utf8(&self.b[self.i..]).map_err(|_| "invalid utf-8")?;
                    let ch = rest.chars().next().ok_or("bad utf-8")?;
                    self.i += ch.len_utf8();
                    out.push(ch);
                }
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while let Some(c) = self.b.get(self.i) {
            match c {
                b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-' => self.i += 1,
                _ => break,
            }
        }
        if start == self.i {
            return Err(format!("invalid value at byte {}", self.i));
        }
        let raw = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "bad number")?;
        raw.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid number '{}'", raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_config() {
        let v = Json::parse(
            r#"{"listen":{"host":"0.0.0.0","port":1503},
                "poll":{"ranges":[{"name":"meter","address":40188,"count":107,
                                   "expect_header":[203,105],"sf_offsets":[15,17]}],
                        "interval_active":5.0,"max_registers_per_read":125},
                "logging":{"level":"INFO","file":null},
                "policy":{"allow_writes":false}}"#,
        )
        .expect("valid json");

        assert_eq!(
            v.get_path(&["listen", "port"]).unwrap().as_f64(),
            Some(1503.0)
        );
        assert_eq!(v.get_path(&["logging", "file"]).unwrap().is_null(), true);
        assert_eq!(
            v.get_path(&["policy", "allow_writes"]).unwrap().as_bool(),
            Some(false)
        );
        let ranges = v.get_path(&["poll", "ranges"]).unwrap().as_array().unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].get("name").unwrap().as_str(), Some("meter"));
        assert_eq!(
            ranges[0].get("expect_header").unwrap().as_array().unwrap()[0].as_f64(),
            Some(203.0)
        );
    }

    #[test]
    fn round_trips_strings() {
        let j = Json::parse(r#"{"a":"x\"y\nz","b":-3,"c":true,"d":null}"#).unwrap();
        let s = j.to_string();
        let back = Json::parse(&s).unwrap();
        assert_eq!(j, back);
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(Json::parse(r#"{"a":1} junk"#).is_err());
        assert!(Json::parse("{").is_err());
        assert!(Json::parse(r#"{"a":}"#).is_err());
    }

    #[test]
    fn integer_numbers_serialise_without_decimal_point() {
        assert_eq!(Json::Num(1503.0).to_string(), "1503");
        assert_eq!(Json::Num(2.5).to_string(), "2.5");
    }
}
