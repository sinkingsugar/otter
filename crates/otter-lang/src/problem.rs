//! Frontend problems: a span plus what a structured diagnostic needs.
//! Converted to [`otter_core::Diagnostic`] (with file, line and column)
//! once the source is known.

use otter_core::diagnostic::{Diagnostic, Phase};

use crate::source::{Source, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProblemKind {
  /// Reading the text (`parse` phase, kind `syntax`).
  Syntax,
  /// Resolving names and lowering (`construct` phase).
  Construct,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
  pub kind: ProblemKind,
  pub span: Span,
  /// The 1.x kind for construct problems (`unknown-shard`, `generic`).
  pub kind_name: &'static str,
  pub code: &'static str,
  pub message: String,
  /// A hint in words.
  pub help: Option<String>,
  /// A literal replacement for the span, when one is known.
  pub fix: Option<String>,
  pub did_you_mean: Vec<String>,
  pub shard: Option<String>,
  pub param: Option<String>,
}

impl Problem {
  pub fn syntax(span: Span, code: &'static str, message: String) -> Problem {
    Problem {
      kind: ProblemKind::Syntax,
      span,
      kind_name: "syntax",
      code,
      message,
      help: None,
      fix: None,
      did_you_mean: Vec::new(),
      shard: None,
      param: None,
    }
  }

  pub fn construct(
    span: Span,
    kind_name: &'static str,
    code: &'static str,
    message: String,
  ) -> Problem {
    Problem {
      kind: ProblemKind::Construct,
      kind_name,
      ..Problem::syntax(span, code, message)
    }
  }

  pub fn help(mut self, help: impl Into<String>) -> Problem {
    self.help = Some(help.into());
    self
  }

  pub fn fix(mut self, fix: impl Into<String>) -> Problem {
    self.fix = Some(fix.into());
    self
  }

  pub fn did_you_mean(mut self, names: Vec<String>) -> Problem {
    self.did_you_mean = names;
    self
  }

  pub fn shard(mut self, shard: &str) -> Problem {
    self.shard = Some(shard.to_string());
    self
  }

  pub fn param(mut self, param: &str) -> Problem {
    self.param = Some(param.to_string());
    self
  }

  /// The structured diagnostic, located in `source`. The help and fix are
  /// appended to the message (the 1.x schema has no field for them).
  pub fn to_diagnostic(&self, source: &Source) -> Diagnostic {
    let phase = match self.kind {
      ProblemKind::Syntax => Phase::Parse,
      ProblemKind::Construct => Phase::Construct,
    };
    let mut message = self.message.clone();
    if let Some(help) = &self.help {
      message.push_str("; ");
      message.push_str(help);
    }
    if let Some(fix) = &self.fix
      && !message.contains(fix.as_str())
    {
      message.push_str(&format!(" (fix: `{fix}`)"));
    }
    let mut d = Diagnostic::new(phase, self.kind_name, self.code, message);
    d.shard = self.shard.clone();
    d.param = self.param.clone();
    d.did_you_mean = self.did_you_mean.clone();
    locate(&mut d, source, self.span);
    d
  }
}

/// Sets a diagnostic's file, line and column from a span.
pub fn locate(d: &mut Diagnostic, source: &Source, span: Span) {
  let (line, column) = source.line_col(span.start);
  d.file = Some(source.file_name(span.start).to_string());
  d.line = Some(line);
  d.column = Some(column);
}

/// A diagnostic as human text: `file:line:column: error: message [code]`,
/// the source line with a caret, and suggestions.
pub fn render(d: &Diagnostic, source: &Source) -> String {
  let mut out = String::new();
  let file = d.file.as_deref().unwrap_or(&source.name);
  match (d.line, d.column) {
    (Some(line), Some(column)) => out.push_str(&format!("{file}:{line}:{column}: ")),
    _ => out.push_str(&format!("{file}: ")),
  }
  out.push_str(&format!(
    "{} error: {} [{}]\n",
    d.phase.name(),
    d.message,
    d.code
  ));
  if let (Some(line), Some(column)) = (d.line, d.column)
    && let Some(text) = source.line(file, line)
  {
    let gutter = line.to_string();
    out.push_str(&format!("  {gutter} | {text}\n"));
    let pad: String = text
      .chars()
      .take(column as usize - 1)
      .map(|c| if c == '\t' { '\t' } else { ' ' })
      .collect();
    out.push_str(&format!("  {} | {pad}^\n", " ".repeat(gutter.len())));
  }
  if !d.did_you_mean.is_empty() {
    out.push_str(&format!("  did you mean: {}\n", d.did_you_mean.join(", ")));
  }
  if let Some(r) = &d.related {
    match (r.line, r.column) {
      (Some(line), Some(column)) => out.push_str(&format!(
        "  note: {} at {file}:{line}:{column}\n",
        r.message
      )),
      _ => out.push_str(&format!("  note: {}\n", r.message)),
    }
  }
  if !d.path.is_empty() {
    out.push_str(&format!("  at: {}\n", d.path_string()));
  }
  out
}

pub use otter_core::diagnostic::closest;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn closest_names_are_case_insensitive_and_bounded() {
    let all = ["Log", "Loop", "Set", "Math.Add"].map(String::from);
    assert_eq!(closest("Logg", all.clone(), 3), ["Log", "Loop"]);
    assert_eq!(closest("log", all.clone(), 3), ["Log", "Loop"]);
    assert!(closest("Zzzzzz", all, 3).is_empty());
  }
}
