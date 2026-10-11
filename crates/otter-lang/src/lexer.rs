//! Tokens. Hand-written so errors can speak in language terms and point at
//! the right place (docs/surface-syntax-review.md §4-§5).
//!
//! Commas are whitespace. Comments are `//` and `/* */`; `;` is rejected
//! (it silently dropped code in 1.x) and skipped to the end of the line so
//! one mistake reports once.

use crate::problem::Problem;
use crate::source::Span;

/// The one assignment operator: `value = name` binds an immutable name.
/// Mutable variables use word forms (`Var`, `Update`, `Push`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
  /// `=`.
  Bind,
}

impl AssignOp {
  pub fn text(self) -> &'static str {
    "="
  }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
  /// A lowercase name: a variable, a wire, `none`, `true`, `false`.
  Ident(String),
  /// An uppercase name: a shard or a parameter name.
  Upper(String),
  /// `Type::Value`.
  Enum(String, String),
  Int(i64),
  Float(f64),
  /// A string literal, escapes decoded.
  Str(String),
  /// `f"..."`: the byte range of the content between the quotes.
  FStr(Span),
  Pipe,
  LParen,
  RParen,
  LBracket,
  RBracket,
  LBrace,
  RBrace,
  Colon,
  At,
  Hash,
  Dot,
  Assign(AssignOp),
  Eof,
}

impl Tok {
  /// How the token is named in messages.
  pub fn describe(&self) -> String {
    match self {
      Tok::Ident(n) | Tok::Upper(n) => format!("`{n}`"),
      Tok::Enum(t, v) => format!("`{t}::{v}`"),
      Tok::Int(i) => format!("`{i}`"),
      Tok::Float(f) => format!("`{f}`"),
      Tok::Str(_) => "a string".into(),
      Tok::FStr(_) => "an f-string".into(),
      Tok::Pipe => "`|`".into(),
      Tok::LParen => "`(`".into(),
      Tok::RParen => "`)`".into(),
      Tok::LBracket => "`[`".into(),
      Tok::RBracket => "`]`".into(),
      Tok::LBrace => "`{`".into(),
      Tok::RBrace => "`}`".into(),
      Tok::Colon => "`:`".into(),
      Tok::At => "`@`".into(),
      Tok::Hash => "`#`".into(),
      Tok::Dot => "`.`".into(),
      Tok::Assign(op) => format!("`{}`", op.text()),
      Tok::Eof => "the end of the source".into(),
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
  pub tok: Tok,
  pub span: Span,
  /// Whether whitespace or a comment separates it from the previous token.
  pub spaced: bool,
}

pub struct Lexer<'a> {
  text: &'a str,
  bytes: &'a [u8],
  pos: usize,
  end: usize,
  pub problems: Vec<Problem>,
}

fn is_ident_continue(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

impl<'a> Lexer<'a> {
  /// Lexes `text[start..end]`; spans stay relative to the whole text.
  pub fn new(text: &'a str, start: usize, end: usize) -> Lexer<'a> {
    Lexer {
      text,
      bytes: text.as_bytes(),
      pos: start,
      end,
      problems: Vec::new(),
    }
  }

  pub fn tokenize(mut self) -> (Vec<Token>, Vec<Problem>) {
    let mut tokens: Vec<Token> = Vec::new();
    loop {
      let spaced = self.skip_trivia();
      let start = self.pos;
      if start >= self.end {
        tokens.push(Token {
          tok: Tok::Eof,
          span: Span::new(self.end, self.end),
          spaced,
        });
        break;
      }
      let prev = tokens.last();
      // A path index right after an adjacent `.` (`s.0.a`): digits only,
      // so `0.a` is not read as a malformed float.
      let path_index =
        !spaced && prev.is_some_and(|t| t.tok == Tok::Dot) && self.bytes[start].is_ascii_digit();
      // A path key right after an adjacent `.` (`t.a.Key.b`): one name
      // segment, so an uppercase key does not take the dots after it.
      let path_key = !spaced
        && prev.is_some_and(|t| t.tok == Tok::Dot)
        && (self.bytes[start].is_ascii_alphabetic() || self.bytes[start] == b'_');
      // A `.` glued to a name or index is a path dot, even before digits.
      let after_name =
        !spaced && prev.is_some_and(|t| matches!(t.tok, Tok::Ident(_) | Tok::Int(_)));
      let tok = if path_index {
        self.path_index()
      } else if path_key {
        while self.peek(0).is_some_and(is_ident_continue) {
          self.pos += 1;
        }
        Some(Tok::Ident(self.text[start..self.pos].to_string()))
      } else if after_name && self.bytes[start] == b'.' {
        self.pos += 1;
        Some(Tok::Dot)
      } else {
        self.token(start)
      };
      if let Some(tok) = tok {
        tokens.push(Token {
          tok,
          span: Span::new(start, self.pos),
          spaced,
        });
      }
    }
    (tokens, self.problems)
  }

  fn peek(&self, ahead: usize) -> Option<u8> {
    let i = self.pos + ahead;
    (i < self.end).then(|| self.bytes[i])
  }

  fn error(&mut self, start: usize, code: &'static str, message: String) {
    self.problems.push(Problem::syntax(
      Span::new(start, self.pos.max(start + 1).min(self.end.max(start + 1))),
      code,
      message,
    ));
  }

  /// Skips whitespace, commas and comments. Returns whether anything was
  /// skipped.
  fn skip_trivia(&mut self) -> bool {
    let start = self.pos;
    while let Some(b) = self.peek(0) {
      match b {
        b' ' | b'\t' | b'\n' | b'\r' | b',' => self.pos += 1,
        b'/' if self.peek(1) == Some(b'/') => self.skip_line(),
        b'/' if self.peek(1) == Some(b'*') => {
          let open = self.pos;
          match self.text[self.pos + 2..self.end].find("*/") {
            Some(i) => self.pos += 2 + i + 2,
            None => {
              self.pos = self.end;
              self.problems.push(Problem::syntax(
                Span::new(open, open + 2),
                "unterminated-comment",
                "this `/*` comment is never closed with `*/`".into(),
              ));
            }
          }
        }
        b';' => {
          let at = self.pos;
          self.skip_line();
          self.problems.push(
            Problem::syntax(
              Span::new(at, at + 1),
              "semicolon",
              "`;` is not a comment or a separator in Shards; the rest of this line was ignored"
                .into(),
            )
            .help("write comments with `//`; statements need no separator"),
          );
        }
        _ => break,
      }
    }
    self.pos > start
  }

  fn skip_line(&mut self) {
    while let Some(b) = self.peek(0) {
      if b == b'\n' {
        break;
      }
      self.pos += 1;
    }
  }

  fn token(&mut self, start: usize) -> Option<Tok> {
    let b = self.bytes[start];
    let next = self.peek(1);
    let single = |lexer: &mut Lexer, tok| {
      lexer.pos += 1;
      Some(tok)
    };
    match b {
      b'|' => single(self, Tok::Pipe),
      b'(' => single(self, Tok::LParen),
      b')' => single(self, Tok::RParen),
      b'[' => single(self, Tok::LBracket),
      b']' => single(self, Tok::RBracket),
      b'{' => single(self, Tok::LBrace),
      b'}' => single(self, Tok::RBrace),
      b':' => single(self, Tok::Colon),
      b'@' => single(self, Tok::At),
      b'#' => single(self, Tok::Hash),
      b'=' => single(self, Tok::Assign(AssignOp::Bind)),
      b'>' => {
        // `>=`, `>` and `>>` were assignments; Shards 2 spells them as words.
        let (len, message) = match self.peek(1) {
          Some(b'=') => (
            2,
            "`>=` is removed: declare a mutable variable with `value | Var(name)`",
          ),
          Some(b'>') => (
            2,
            "`>>` is removed: append with `value | Push(name)` after declaring the sequence (`[] | Var(name)`)",
          ),
          _ => (
            1,
            "`>` is removed: assign an existing mutable variable with `value | Update(name)` (comparisons are shards: `IsMore`)",
          ),
        };
        self.pos += len;
        self.problems.push(Problem::syntax(
          Span::new(start, self.pos),
          "removed-operator",
          message.to_string(),
        ));
        None
      }
      b'.' if next.is_some_and(|n| n.is_ascii_digit()) => {
        self.pos += 1;
        while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
          self.pos += 1;
        }
        let digits = self.text[start + 1..self.pos].to_string();
        self.problems.push(
          Problem::syntax(
            Span::new(start, self.pos),
            "number-form",
            format!("a number cannot start with `.`: write 0.{digits}"),
          )
          .fix(format!("0.{digits}")),
        );
        None
      }
      b'.' => single(self, Tok::Dot),
      b'"' => self.string(start),
      b'f' if next == Some(b'"') => self.fstring(start),
      b'0'..=b'9' => self.number(start),
      b'-' | b'+' if next.is_some_and(|n| n.is_ascii_digit()) => self.number(start),
      b'~' if next.is_some_and(|n| n.is_ascii_digit()) => {
        self.pos += 1;
        self.error(
          start,
          "number-form",
          "`~` is not a number sign in Shards 2: write `-` for a negative number".into(),
        );
        None
      }
      b'a'..=b'z' => Some(self.ident(start)),
      b'_' if next.is_some_and(|n| n.is_ascii_lowercase()) => Some(self.ident(start)),
      b'$' if next.is_some_and(|n| n.is_ascii_digit()) => {
        // 1.x's implicit loop variables (`ForEach`, `Map`): models trained
        // on 1.x write them. Read as a name so the rest still parses.
        self.pos += 1;
        let tok = self.ident(start);
        self.problems.push(
          Problem::syntax(
            Span::new(start, self.pos),
            "implicit-loop-variable",
            format!(
              "`{}` is 1.x's implicit loop variable; Shards 2 has none",
              &self.text[start..self.pos]
            ),
          )
          .help(
            "in 2.0 the element is the block's input: bind it with `= item` if you need a name",
          ),
        );
        Some(tok)
      }
      b'$' if next.is_some_and(|n| n.is_ascii_alphanumeric()) => {
        self.pos += 1;
        Some(self.ident(start))
      }
      b'A'..=b'Z' => Some(self.upper(start)),
      b'_' if next.is_some_and(|n| n.is_ascii_uppercase()) => Some(self.upper(start)),
      _ => {
        let c = self.text[start..].chars().next().unwrap_or('?');
        self.pos += c.len_utf8();
        self.error(
          start,
          "unexpected-character",
          format!("unexpected character `{c}`"),
        );
        None
      }
    }
  }

  fn ident(&mut self, start: usize) -> Tok {
    self.pos = self.pos.max(start + 1);
    loop {
      match self.peek(0) {
        Some(b) if is_ident_continue(b) => self.pos += 1,
        // Namespaced names: `a/b`, but not a `//` comment.
        Some(b'/') if self.peek(1).is_some_and(|n| n.is_ascii_lowercase()) => self.pos += 1,
        _ => break,
      }
    }
    Tok::Ident(self.text[start..self.pos].to_string())
  }

  fn upper(&mut self, start: usize) -> Tok {
    self.pos = start + 1;
    loop {
      match self.peek(0) {
        Some(b) if is_ident_continue(b) || b == b'!' => self.pos += 1,
        Some(b'.') if self.peek(1).is_some_and(|n| n.is_ascii_alphanumeric()) => self.pos += 1,
        _ => break,
      }
    }
    let name = &self.text[start..self.pos];
    if self.peek(0) == Some(b':')
      && self.peek(1) == Some(b':')
      && !name.contains('.')
      && self
        .peek(2)
        .is_some_and(|n| n.is_ascii_alphanumeric() || n == b'_')
    {
      let ty = name.to_string();
      self.pos += 2;
      let value_start = self.pos;
      while self
        .peek(0)
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
      {
        self.pos += 1;
      }
      return Tok::Enum(ty, self.text[value_start..self.pos].to_string());
    }
    Tok::Upper(name.to_string())
  }

  fn path_index(&mut self) -> Option<Tok> {
    let start = self.pos;
    while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
      self.pos += 1;
    }
    match self.text[start..self.pos].parse::<i64>() {
      Ok(i) => Some(Tok::Int(i)),
      Err(_) => {
        self.error(start, "number-range", "index out of range".into());
        None
      }
    }
  }

  fn number(&mut self, start: usize) -> Option<Tok> {
    self.pos = start;
    let negative = self.bytes[start] == b'-';
    if matches!(self.bytes[start], b'-' | b'+') {
      self.pos += 1;
    }
    let digits_start = self.pos;
    if self.peek(0) == Some(b'0') && matches!(self.peek(1), Some(b'x' | b'X')) {
      self.pos += 2;
      let hex_start = self.pos;
      while self.peek(0).is_some_and(|b| b.is_ascii_hexdigit()) {
        self.pos += 1;
      }
      if self
        .peek(0)
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
      {
        while self.peek(0).is_some_and(is_ident_continue) {
          self.pos += 1;
        }
        let text = self.text[start..self.pos].to_string();
        self.error(
          start,
          "number-form",
          format!("`{text}` is not a hexadecimal number"),
        );
        return None;
      }
      let hex = &self.text[hex_start..self.pos];
      let value = u64::from_str_radix(hex, 16)
        .ok()
        .filter(|_| !hex.is_empty());
      return match value {
        // Hex literals are bit patterns (addresses): the Int keeps the bits.
        Some(v) => Some(Tok::Int(if negative {
          (v as i64).wrapping_neg()
        } else {
          v as i64
        })),
        None => {
          self.error(start, "number-form", "invalid hexadecimal number".into());
          None
        }
      };
    }
    while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
      self.pos += 1;
    }
    let mut float = false;
    if self.peek(0) == Some(b'.') {
      if self.peek(1).is_some_and(|b| b.is_ascii_digit()) {
        float = true;
        self.pos += 1;
        while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
          self.pos += 1;
        }
      } else if !self
        .peek(1)
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
      {
        // `1.` (a path like `x.1.a` is lexed elsewhere).
        let int = self.text[digits_start..self.pos].to_string();
        self.pos += 1;
        let sign = if negative { "-" } else { "" };
        self.problems.push(
          Problem::syntax(
            Span::new(start, self.pos),
            "number-form",
            format!("a number cannot end with `.`: write {sign}{int}.0"),
          )
          .fix(format!("{sign}{int}.0")),
        );
        return None;
      }
    }
    if matches!(self.peek(0), Some(b'e' | b'E')) {
      let sign = usize::from(matches!(self.peek(1), Some(b'+' | b'-')));
      if self.peek(1 + sign).is_some_and(|b| b.is_ascii_digit()) {
        float = true;
        self.pos += 1 + sign;
        while self.peek(0).is_some_and(|b| b.is_ascii_digit()) {
          self.pos += 1;
        }
      }
    }
    // Letters glued to a number (`1abc`, `2x`) are not a name and a number.
    if self
      .peek(0)
      .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
    {
      while self.peek(0).is_some_and(is_ident_continue) {
        self.pos += 1;
      }
      let text = self.text[start..self.pos].to_string();
      self.error(
        start,
        "number-form",
        format!("`{text}` is not a number or a name (names start with a letter)"),
      );
      return None;
    }
    let text = &self.text[start..self.pos];
    if float {
      match text.parse::<f64>() {
        Ok(f) if f.is_finite() => Some(Tok::Float(f)),
        _ => {
          self.error(
            start,
            "number-range",
            format!("`{text}` is out of range for a Float"),
          );
          None
        }
      }
    } else {
      match text.parse::<i64>() {
        Ok(i) => Some(Tok::Int(i)),
        Err(_) => {
          self.error(
            start,
            "number-range",
            format!("`{text}` is out of range for an Int (64-bit)"),
          );
          None
        }
      }
    }
  }

  fn string(&mut self, start: usize) -> Option<Tok> {
    if self.text[start..self.end].starts_with("\"\"\"") {
      let content = start + 3;
      return match self.text[content..self.end].find("\"\"\"") {
        Some(i) => {
          self.pos = content + i + 3;
          Some(Tok::Str(self.text[content..content + i].to_string()))
        }
        None => {
          self.pos = self.end;
          self.problems.push(Problem::syntax(
            Span::new(start, start + 3),
            "unterminated-string",
            "this `\"\"\"` string is never closed".into(),
          ));
          None
        }
      };
    }
    self.pos = start + 1;
    let decoded = self.string_body(start, self.pos, b'"')?;
    Some(Tok::Str(decoded))
  }

  /// Decodes a quoted body starting at `from`, up to the closing quote.
  fn string_body(&mut self, open: usize, from: usize, quote: u8) -> Option<String> {
    self.pos = from;
    let mut out = String::new();
    let mut ok = true;
    loop {
      let Some(b) = self.peek(0) else {
        self.problems.push(Problem::syntax(
          Span::new(open, open + 1),
          "unterminated-string",
          "this string is never closed with `\"`".into(),
        ));
        return None;
      };
      if b == quote {
        self.pos += 1;
        break;
      }
      if b == b'\\' {
        let esc_at = self.pos;
        let c = self.text[self.pos + 1..self.end].chars().next();
        self.pos += 1 + c.map_or(0, char::len_utf8);
        match c {
          Some(c) if escape(c).is_some() => out.extend(escape(c)),
          Some(other) => {
            ok = false;
            self.problems.push(
              Problem::syntax(
                Span::new(esc_at, self.pos),
                "string-escape",
                format!("unknown escape `\\{other}` in a string"),
              )
              .help(KNOWN_ESCAPES),
            );
          }
          None => {}
        }
        continue;
      }
      let c = self.text[self.pos..self.end].chars().next().unwrap_or('\0');
      out.push(c);
      self.pos += c.len_utf8();
    }
    ok.then_some(out)
  }

  fn fstring(&mut self, start: usize) -> Option<Tok> {
    let content = start + 2;
    self.pos = content;
    // Find the closing quote, skipping escapes and anything inside
    // `{...}` interpolations (which may contain strings).
    let mut depth = 0usize;
    loop {
      let Some(b) = self.peek(0) else {
        self.problems.push(Problem::syntax(
          Span::new(start, start + 2),
          "unterminated-string",
          "this f-string is never closed with `\"`".into(),
        ));
        return None;
      };
      match b {
        b'\\' => self.pos += 2,
        b'"' if depth == 0 => break,
        b'"' => {
          // A string inside an interpolation.
          let open = self.pos;
          self.string_body(open, open + 1, b'"')?;
        }
        b'{' if depth == 0 && self.peek(1) == Some(b'{') => self.pos += 2,
        b'}' if depth == 0 && self.peek(1) == Some(b'}') => self.pos += 2,
        b'{' => {
          depth += 1;
          self.pos += 1;
        }
        b'}' => {
          depth = depth.saturating_sub(1);
          self.pos += 1;
        }
        _ => self.pos += 1,
      }
    }
    let span = Span::new(content, self.pos);
    self.pos += 1;
    Some(Tok::FStr(span))
  }
}

/// The character a `\x` escape stands for (1.x's set).
pub(crate) fn escape(c: char) -> Option<char> {
  Some(match c {
    'n' => '\n',
    't' => '\t',
    'r' => '\r',
    '0' => '\0',
    'b' => '\u{8}',
    'f' => '\u{c}',
    'v' => '\u{b}',
    '\\' => '\\',
    '"' => '"',
    '\'' => '\'',
    _ => return None,
  })
}

pub(crate) const KNOWN_ESCAPES: &str = "known escapes: \\n \\t \\r \\0 \\b \\f \\v \\\\ \\\" \\'";

/// Lexes a whole text.
pub fn lex(text: &str) -> (Vec<Token>, Vec<Problem>) {
  Lexer::new(text, 0, text.len()).tokenize()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn toks(text: &str) -> Vec<Tok> {
    let (tokens, problems) = lex(text);
    assert!(problems.is_empty(), "{problems:?}");
    tokens.into_iter().map(|t| t.tok).collect()
  }

  fn first_problem(text: &str) -> Problem {
    lex(text).1.into_iter().next().expect("a problem")
  }

  #[test]
  fn names_numbers_and_punctuation() {
    assert_eq!(
      toks("x-1 | Math.Add(-2, 0x10) ns/var 2.5e-3 1e3 Type::Val"),
      [
        Tok::Ident("x-1".into()),
        Tok::Pipe,
        Tok::Upper("Math.Add".into()),
        Tok::LParen,
        Tok::Int(-2),
        Tok::Int(16),
        Tok::RParen,
        Tok::Ident("ns/var".into()),
        Tok::Float(2.5e-3),
        Tok::Float(1e3),
        Tok::Enum("Type".into(), "Val".into()),
        Tok::Eof
      ]
    );
    assert_eq!(toks("="), [Tok::Assign(AssignOp::Bind), Tok::Eof]);
  }

  /// The old assignment operators are syntax errors that say what to write.
  #[test]
  fn removed_operators_name_their_word_forms() {
    for (text, word) in [
      ("1 >= n", "Var("),
      ("1 > n", "Update("),
      ("1 >> n", "Push("),
    ] {
      let (_, problems) = lex(text);
      assert_eq!(problems.len(), 1, "{text}");
      assert_eq!(problems[0].code, "removed-operator");
      assert!(
        problems[0].message.contains(word),
        "{}",
        problems[0].message
      );
      assert_eq!(problems[0].span, Span::new(2, text.len() - 2));
    }
  }

  #[test]
  fn paths_mix_keys_and_indices() {
    assert_eq!(
      toks("s.0.a t.a.1"),
      [
        Tok::Ident("s".into()),
        Tok::Dot,
        Tok::Int(0),
        Tok::Dot,
        Tok::Ident("a".into()),
        Tok::Ident("t".into()),
        Tok::Dot,
        Tok::Ident("a".into()),
        Tok::Dot,
        Tok::Int(1),
        Tok::Eof
      ]
    );
  }

  #[test]
  fn strings_and_comments() {
    assert_eq!(
      toks("\"a\\n\\\"b\" // c\n/* d */ \"\"\"raw \\n\"\"\""),
      [
        Tok::Str("a\n\"b".into()),
        Tok::Str("raw \\n".into()),
        Tok::Eof
      ]
    );
    let t = toks("f\"x {y | F(\"}\")} {{z}}\"");
    assert!(matches!(t[0], Tok::FStr(_)) && t[1] == Tok::Eof, "{t:?}");
  }

  #[test]
  fn traps_are_rejected_with_fixes() {
    let p = first_problem("1 | Log(\"a\"); 2 | Log(\"b\")");
    assert_eq!(p.code, "semicolon");
    assert_eq!(p.span, Span::new(12, 13));
    assert_eq!(first_problem("1. | Log").fix.as_deref(), Some("1.0"));
    assert_eq!(first_problem(".5").fix.as_deref(), Some("0.5"));
    assert_eq!(first_problem("~5").code, "number-form");
    assert_eq!(first_problem("2x").code, "number-form");
    assert_eq!(first_problem("\"a\\qb\"").code, "string-escape");
    assert_eq!(first_problem("\"open").code, "unterminated-string");
    assert_eq!(first_problem("99999999999999999999").code, "number-range");
  }
}
