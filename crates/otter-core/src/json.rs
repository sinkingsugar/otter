//! JSON text to values and back, for `FromJson`, `ToJson` and hosts
//! (docs/host-requirements.md §3, the JSON mapping).
//!
//! The mapping:
//! - `null` is `none`, `true`/`false` are Bools, strings are Strings,
//!   arrays are sequences, objects are tables (string keys; tables keep
//!   keys sorted, so key order is not preserved; a repeated key keeps its
//!   last value).
//! - A number written without a fraction or exponent that fits in 64 bits
//!   is an Int; every other number is a Float. Writing keeps the
//!   distinction: an Int is written as an integer, a Float always with a
//!   fraction or exponent (`2.0`), so the text reads back as the same value.
//! - Float vectors are written as arrays of numbers. NaN and infinities,
//!   byte strings and objects have no JSON form and are an error (encode
//!   bytes with `ToBase64` first).
//!
//! Parsing nests at most [`MAX_DEPTH`] arrays and objects deep, so a hostile
//! document cannot exhaust the stack (the device has little of it).

use std::fmt::Write;
use std::sync::Arc;

use crate::var::{Table, Var};

/// The deepest nesting of arrays and objects [`parse`] accepts.
pub const MAX_DEPTH: usize = if cfg!(target_os = "espidf") { 32 } else { 128 };

/// Parses JSON text into a value. The error names the byte offset.
pub fn parse(text: &str) -> Result<Var, String> {
  let mut p = Parser {
    bytes: text.as_bytes(),
    at: 0,
  };
  p.space();
  let value = p.value(0)?;
  p.space();
  if p.at != p.bytes.len() {
    return Err(p.error("unexpected text after the value"));
  }
  Ok(value)
}

/// Writes a value as compact JSON.
pub fn write(value: &Var) -> Result<String, String> {
  let mut out = String::new();
  write_into(value, &mut out, None, 0)?;
  Ok(out)
}

/// Writes a value as JSON indented by `indent` spaces per level.
pub fn write_pretty(value: &Var, indent: usize) -> Result<String, String> {
  let mut out = String::new();
  write_into(value, &mut out, Some(indent), 0)?;
  Ok(out)
}

struct Parser<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl Parser<'_> {
  fn error(&self, what: &str) -> String {
    format!("invalid JSON at byte {}: {what}", self.at)
  }

  fn space(&mut self) {
    while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.at) {
      self.at += 1;
    }
  }

  fn literal(&mut self, word: &str, value: Var) -> Result<Var, String> {
    if self.bytes[self.at..].starts_with(word.as_bytes()) {
      self.at += word.len();
      Ok(value)
    } else {
      Err(self.error("unknown literal"))
    }
  }

  fn value(&mut self, depth: usize) -> Result<Var, String> {
    match self.bytes.get(self.at) {
      None => Err(self.error("unexpected end")),
      Some(b'n') => self.literal("null", Var::None),
      Some(b't') => self.literal("true", Var::Bool(true)),
      Some(b'f') => self.literal("false", Var::Bool(false)),
      Some(b'"') => Ok(Var::from_string(Arc::from(self.string()?))),
      Some(b'[') => {
        if depth >= MAX_DEPTH {
          return Err(self.error("nested too deeply"));
        }
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.bytes.get(self.at) == Some(&b']') {
          self.at += 1;
          return Ok(Var::seq(items));
        }
        loop {
          self.space();
          items.push(self.value(depth + 1)?);
          self.space();
          match self.bytes.get(self.at) {
            Some(b',') => self.at += 1,
            Some(b']') => {
              self.at += 1;
              return Ok(Var::seq(items));
            }
            _ => return Err(self.error("expected `,` or `]`")),
          }
        }
      }
      Some(b'{') => {
        if depth >= MAX_DEPTH {
          return Err(self.error("nested too deeply"));
        }
        self.at += 1;
        let mut table = Table::builder();
        self.space();
        if self.bytes.get(self.at) == Some(&b'}') {
          self.at += 1;
          return Ok(Var::from_table(table.build()));
        }
        loop {
          self.space();
          if self.bytes.get(self.at) != Some(&b'"') {
            return Err(self.error("expected a string key"));
          }
          let key = self.string()?;
          self.space();
          if self.bytes.get(self.at) != Some(&b':') {
            return Err(self.error("expected `:`"));
          }
          self.at += 1;
          self.space();
          let value = self.value(depth + 1)?;
          table.insert(key, value);
          self.space();
          match self.bytes.get(self.at) {
            Some(b',') => self.at += 1,
            Some(b'}') => {
              self.at += 1;
              return Ok(Var::from_table(table.build()));
            }
            _ => return Err(self.error("expected `,` or `}`")),
          }
        }
      }
      Some(b'-' | b'0'..=b'9') => self.number(),
      Some(_) => Err(self.error("unexpected character")),
    }
  }

  fn digits(&mut self) -> usize {
    let start = self.at;
    while let Some(b'0'..=b'9') = self.bytes.get(self.at) {
      self.at += 1;
    }
    self.at - start
  }

  fn number(&mut self) -> Result<Var, String> {
    let start = self.at;
    if self.bytes[self.at] == b'-' {
      self.at += 1;
    }
    let int_start = self.at;
    if self.digits() == 0 {
      return Err(self.error("expected digits"));
    }
    if self.at - int_start > 1 && self.bytes[int_start] == b'0' {
      return Err(self.error("leading zeros are not allowed"));
    }
    let mut float = false;
    if self.bytes.get(self.at) == Some(&b'.') {
      self.at += 1;
      float = true;
      if self.digits() == 0 {
        return Err(self.error("expected digits after `.`"));
      }
    }
    if let Some(b'e' | b'E') = self.bytes.get(self.at) {
      self.at += 1;
      float = true;
      if let Some(b'+' | b'-') = self.bytes.get(self.at) {
        self.at += 1;
      }
      if self.digits() == 0 {
        return Err(self.error("expected exponent digits"));
      }
    }
    // Only ASCII was consumed.
    let text = std::str::from_utf8(&self.bytes[start..self.at]).expect("ASCII");
    if !float && let Ok(i) = text.parse::<i64>() {
      return Ok(Var::Int(i));
    }
    match text.parse::<f64>() {
      // `1e400` would read as infinity, which has no JSON form to write back.
      Ok(f) if f.is_finite() => Ok(Var::Float(f)),
      Ok(_) => Err(self.error("number out of range")),
      Err(_) => Err(self.error("invalid number")),
    }
  }

  fn hex4(&mut self) -> Result<u32, String> {
    let Some(digits) = self.bytes.get(self.at..self.at + 4) else {
      return Err(self.error("expected four hex digits"));
    };
    // Digits only: `from_str_radix` alone would take a sign (`\u+041`).
    if !digits.iter().all(u8::is_ascii_hexdigit) {
      return Err(self.error("expected four hex digits"));
    }
    let code = digits
      .iter()
      .fold(0, |n, &d| n << 4 | (d as char).to_digit(16).unwrap_or(0));
    self.at += 4;
    Ok(code)
  }

  /// A string, the cursor on its opening quote.
  fn string(&mut self) -> Result<String, String> {
    self.at += 1;
    let mut out = String::new();
    loop {
      let run = self.at;
      while let Some(&b) = self.bytes.get(self.at) {
        if b == b'"' || b == b'\\' || b < 0x20 {
          break;
        }
        self.at += 1;
      }
      // The input is a `&str` and the run stops only at ASCII bytes, so it
      // is whole UTF-8.
      out.push_str(std::str::from_utf8(&self.bytes[run..self.at]).expect("UTF-8 run"));
      match self.bytes.get(self.at) {
        None => return Err(self.error("unterminated string")),
        Some(b'"') => {
          self.at += 1;
          return Ok(out);
        }
        Some(b'\\') => {
          self.at += 1;
          let Some(&escape) = self.bytes.get(self.at) else {
            return Err(self.error("unterminated escape"));
          };
          self.at += 1;
          match escape {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
              let first = self.hex4()?;
              let code = if (0xD800..0xDC00).contains(&first) {
                // A high surrogate: a low one must follow.
                if !self.bytes[self.at..].starts_with(b"\\u") {
                  return Err(self.error("unpaired surrogate"));
                }
                self.at += 2;
                let second = self.hex4()?;
                if !(0xDC00..0xE000).contains(&second) {
                  return Err(self.error("unpaired surrogate"));
                }
                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
              } else {
                first
              };
              match char::from_u32(code) {
                Some(c) => out.push(c),
                None => return Err(self.error("unpaired surrogate")),
              }
            }
            _ => return Err(self.error("unknown escape")),
          }
        }
        Some(_) => return Err(self.error("control character in a string")),
      }
    }
  }
}

fn write_string(s: &str, out: &mut String) {
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

fn write_float(f: f64, out: &mut String) -> Result<(), String> {
  if !f.is_finite() {
    return Err(format!("{f} has no JSON form"));
  }
  // `{:?}` is the shortest text that reads back as the same value, and
  // always has a fraction or an exponent (`2.0`, `1e-7`).
  let _ = write!(out, "{f:?}");
  Ok(())
}

fn newline(out: &mut String, indent: Option<usize>, depth: usize) {
  if let Some(n) = indent {
    out.push('\n');
    out.extend(std::iter::repeat_n(' ', n * depth));
  }
}

fn write_into(
  value: &Var,
  out: &mut String,
  indent: Option<usize>,
  depth: usize,
) -> Result<(), String> {
  if depth > MAX_DEPTH {
    return Err("the value nests too deeply for JSON".into());
  }
  match value {
    Var::None => out.push_str("null"),
    Var::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
    Var::Int(i) => {
      let _ = write!(out, "{i}");
    }
    Var::Float(f) => write_float(*f, out)?,
    Var::Float2(c) => write_floats(c.iter(), out)?,
    Var::Float3(c) => write_floats(c.iter(), out)?,
    Var::Float4(c) => write_floats(c.iter(), out)?,
    Var::String(s) => write_string(s, out),
    Var::Bytes(_) => {
      return Err("a byte string has no JSON form (encode it with ToBase64 first)".into());
    }
    Var::Object(_) => return Err(format!("{} has no JSON form", value.text())),
    Var::Seq(items) => {
      out.push('[');
      for (i, item) in items.iter().enumerate() {
        if i > 0 {
          out.push(',');
        }
        newline(out, indent, depth + 1);
        write_into(item, out, indent, depth + 1)?;
      }
      if !items.is_empty() {
        newline(out, indent, depth);
      }
      out.push(']');
    }
    Var::Table(table) => {
      out.push('{');
      for (i, (key, item)) in table.iter().enumerate() {
        if i > 0 {
          out.push(',');
        }
        newline(out, indent, depth + 1);
        write_string(key, out);
        out.push(':');
        if indent.is_some() {
          out.push(' ');
        }
        write_into(item, out, indent, depth + 1)?;
      }
      if !table.is_empty() {
        newline(out, indent, depth);
      }
      out.push('}');
    }
  }
  Ok(())
}

fn write_floats<'a>(items: impl Iterator<Item = &'a f32>, out: &mut String) -> Result<(), String> {
  out.push('[');
  for (i, x) in items.enumerate() {
    if i > 0 {
      out.push(',');
    }
    // An f32's own shortest text: widened to f64 it would print the
    // binary error (`0.10000000149011612`).
    if !x.is_finite() {
      return Err(format!("{x} has no JSON form"));
    }
    let _ = write!(out, "{x:?}");
  }
  out.push(']');
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reads_every_kind_of_value() {
    let v = parse(
      r#" {"a": [1, -2, 3.5, 1e3, true, false, null], "b": {"c": "d\n\u00e9\ud83d\ude00"}} "#,
    )
    .unwrap();
    let t = v.as_table().unwrap();
    let a = t.get("a").unwrap().as_seq().unwrap();
    assert_eq!(a[0], Var::Int(1));
    assert_eq!(a[1], Var::Int(-2));
    assert_eq!(a[2], Var::Float(3.5));
    assert_eq!(a[3], Var::Float(1000.0));
    assert_eq!(a[4], Var::Bool(true));
    assert_eq!(a[6], Var::None);
    let c = t.get("b").unwrap().as_table().unwrap().get("c").unwrap();
    assert_eq!(c.as_str(), Some("d\né😀"));
  }

  #[test]
  fn edge_cases_of_the_grammar() {
    assert_eq!(parse(" \t\r\n[ ] ").unwrap(), Var::seq(Vec::new()));
    assert_eq!(
      write(&parse("{\"a\":{},\"b\":[[]]}").unwrap()).unwrap(),
      "{\"a\":{},\"b\":[[]]}"
    );
    assert_eq!(
      parse("\"\\/\\b\\f\\u0000\"").unwrap().as_str(),
      Some("/\u{8}\u{c}\0")
    );
    assert_eq!(parse("-0").unwrap(), Var::Int(0));
    assert_eq!(parse("1E+2").unwrap(), Var::Float(100.0));
    assert_eq!(parse("-1.5e-3").unwrap(), Var::Float(-0.0015));
    // A repeated key keeps its last value; written back with sorted keys.
    assert_eq!(
      write(&parse("{\"b\":1,\"a\":2,\"b\":3}").unwrap()).unwrap(),
      "{\"a\":2,\"b\":3}"
    );
    // Control characters are escaped, other text is written as it is.
    assert_eq!(write(&Var::string("\u{1}é")).unwrap(), "\"\\u0001é\"");
    assert_eq!(write(&Var::Float(1e-7)).unwrap(), "1e-7");
  }

  #[test]
  fn integers_too_large_for_an_int_are_floats() {
    assert_eq!(parse("9223372036854775807").unwrap(), Var::Int(i64::MAX));
    assert_eq!(
      parse("9223372036854775808").unwrap(),
      Var::Float(9223372036854775808.0)
    );
  }

  #[test]
  fn writes_and_reads_back() {
    let v = parse(r#"{"x":[1,2.0,"q\"\\"],"y":null,"z":{"w":false}}"#).unwrap();
    let text = write(&v).unwrap();
    assert_eq!(text, r#"{"x":[1,2.0,"q\"\\"],"y":null,"z":{"w":false}}"#);
    assert_eq!(parse(&text).unwrap(), v);
    assert_eq!(
      write_pretty(&v, 2).unwrap(),
      "{\n  \"x\": [\n    1,\n    2.0,\n    \"q\\\"\\\\\"\n  ],\n  \"y\": null,\n  \"z\": {\n    \"w\": false\n  }\n}"
    );
  }

  #[test]
  fn rejects_malformed_text_and_values_without_a_form() {
    for bad in [
      "",
      "[1,]",
      "{\"a\" 1}",
      "01",
      "1.",
      "\"\\x\"",
      "nul",
      "[1] 2",
      "\"\\ud800\"",
      "\"a\nb\"",
      "1e400",
      "-",
      "[1 2]",
      "{\"a\":1,}",
      "\"\\u12\"",
    ] {
      assert!(parse(bad).is_err(), "{bad:?} parsed");
    }
    let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
    assert!(parse(&deep).unwrap_err().contains("nested too deeply"));
    assert!(write(&Var::Float(f64::NAN)).is_err());
    assert!(write(&Var::bytes(b"x")).is_err());
  }
}
