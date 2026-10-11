//! Structured diagnostics (docs/shard-metadata-and-compose.md §6).
//!
//! Field names and meanings follow 1.x `shards check --json`
//! (`shards/lang/src/check.rs`): `phase`, `severity`, `kind`, `message`,
//! `file`/`line`/`column`, `shard`, `actual`/`expected` (`{name,
//! basic_type}`), `param_index`, `did_you_mean` (filled by the frontend for
//! unknown names). `candidates` is not computed yet; as in 1.x, empty lists
//! are omitted. New information is
//! in new fields: `code` (a fine-grained stable code) and `param` (the
//! parameter name), and `path` (where in the wire definitions the error
//! occurred, see [`PathStep`]). Source location is absent for wires built in
//! Rust; a frontend maps `path` to its source spans.

use std::fmt;

use crate::describe::TypeName;
use crate::types::{Type, TypeDesc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
  /// Reading source text (frontend syntax errors).
  Parse,
  /// Building the wire: name resolution and argument decoding.
  Construct,
  Compose,
  Activate,
}

impl Phase {
  pub fn name(self) -> &'static str {
    match self {
      Phase::Parse => "parse",
      Phase::Construct => "construct",
      Phase::Compose => "compose",
      Phase::Activate => "activate",
    }
  }
}

/// One step of a diagnostic's occurrence path, from the composed wire down
/// to the shard that failed. `Wire` starts a wire definition (the root, a
/// `Do` sub-wire, a spawned wire); `Shard` indexes the current flow; `Param`
/// enters a nested flow or wire reference of the preceding shard. Indices
/// refer to wire definitions, never to source text, so the same definition
/// at two call sites has the same steps below its `Wire`, and a frontend
/// maps the steps to spans through its own side table.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathStep {
  Wire(String),
  /// Enters a function body at a call site (the definition, not the site).
  Function(String),
  Shard {
    index: usize,
    name: String,
  },
  Param(String),
  /// The case (`Match`) or variadic argument (`All`) holding the flow.
  Item(usize),
  /// Enters a pipeline evaluated at compose time (`#( ... )`, the value of
  /// the preceding parameter): the steps after it are inside the pipeline.
  Evaluation,
}

/// Where a shard's input came from, for type mismatches on it: the shard
/// that produced the value (or the enclosing flow's own input), and the
/// shards between that passed it through unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSource {
  /// `(index, shard name)` in the same flow; `None`: the flow's own input.
  pub origin: Option<(usize, String)>,
  /// Shards between the origin and the failing shard that pass their input
  /// through, in flow order.
  pub via: Vec<(usize, String)>,
}

impl InputSource {
  /// A sentence for the human message.
  pub fn describe(&self) -> String {
    let origin = match &self.origin {
      Some((index, name)) => format!("the input comes from {index}:{name}"),
      None => "the input is the flow's own input".to_string(),
    };
    if self.via.is_empty() {
      return origin;
    }
    let via: Vec<String> = self.via.iter().map(|(i, n)| format!("{i}:{n}")).collect();
    let verb = if self.via.len() == 1 {
      "passes its"
    } else {
      "pass their"
    };
    format!(
      "{origin}, through {}, which {verb} input through unchanged",
      via.join(", ")
    )
  }
}

/// A type as reported in a diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeRef {
  /// Canonical type string (e.g. `String`, `[Int]`).
  pub name: String,
  /// The 1.x `SHType` value, -1 if there is none.
  pub basic_type: i32,
}

impl TypeRef {
  pub fn of(ty: Type) -> TypeRef {
    TypeRef {
      name: ty.to_string(),
      basic_type: basic_type(ty),
    }
  }

  pub fn named(t: TypeName) -> TypeRef {
    TypeRef {
      name: t.name().to_string(),
      basic_type: t.basic_type(),
    }
  }
}

/// A second location that explains a diagnostic, such as where a name was
/// first declared. Compose sets `path`; the frontend adds line and column.
#[derive(Clone, Debug, PartialEq)]
pub struct Related {
  pub message: String,
  /// Occurrence path of the related shard, starting at its wire.
  pub path: Vec<PathStep>,
  pub line: Option<u32>,
  pub column: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
  pub phase: Phase,
  /// The 1.x classification: `input-type-mismatch`, `compose-error`,
  /// `unknown-shard`, `syntax` or `generic`.
  pub kind: &'static str,
  /// A finer stable code, e.g. `missing-argument`, `wrong-argument-form`.
  pub code: &'static str,
  pub message: String,
  pub shard: Option<String>,
  pub param: Option<String>,
  pub param_index: Option<i32>,
  pub actual: Option<TypeRef>,
  pub expected: Vec<TypeRef>,
  pub file: Option<String>,
  pub line: Option<u32>,
  pub column: Option<u32>,
  /// Root first; empty when the error did not come from composing a wire.
  pub path: Vec<PathStep>,
  /// For a mismatch on a shard's input: where that input came from.
  pub input_from: Option<InputSource>,
  /// Close names for a misspelled one (1.x field), best first.
  pub did_you_mean: Vec<String>,
  pub related: Option<Related>,
}

impl Diagnostic {
  pub fn new(
    phase: Phase,
    kind: &'static str,
    code: &'static str,
    message: impl Into<String>,
  ) -> Diagnostic {
    Diagnostic {
      phase,
      kind,
      code,
      message: message.into(),
      shard: None,
      param: None,
      param_index: None,
      actual: None,
      expected: Vec::new(),
      file: None,
      line: None,
      column: None,
      path: Vec::new(),
      input_from: None,
      did_you_mean: Vec::new(),
      related: None,
    }
  }

  /// Adds a related location by occurrence path.
  pub fn related(mut self, message: impl Into<String>, path: Vec<PathStep>) -> Diagnostic {
    self.related = Some(Related {
      message: message.into(),
      path,
      line: None,
      column: None,
    });
    self
  }

  /// The path as text, e.g. `main/2:When/action/0:Add`.
  pub fn path_string(&self) -> String {
    let steps: Vec<String> = self
      .path
      .iter()
      .map(|step| match step {
        PathStep::Wire(name) => name.clone(),
        PathStep::Function(name) => format!("{name}()"),
        PathStep::Shard { index, name } => format!("{index}:{name}"),
        PathStep::Param(name) => name.clone(),
        PathStep::Item(index) => format!("#{index}"),
        PathStep::Evaluation => "#()".to_string(),
      })
      .collect();
    steps.join("/")
  }

  pub fn shard(mut self, shard: &str) -> Diagnostic {
    self.shard = Some(shard.to_string());
    self
  }

  pub fn param(mut self, name: &str, index: Option<usize>) -> Diagnostic {
    self.param = Some(name.to_string());
    self.param_index = index.map(|i| i as i32);
    self
  }

  pub fn types(mut self, actual: Option<TypeRef>, expected: Vec<TypeRef>) -> Diagnostic {
    self.actual = actual;
    self.expected = expected;
    self
  }

  /// Renders the diagnostic as a JSON object with 1.x field names.
  pub fn to_json(&self) -> String {
    let mut fields = vec![
      format!("\"phase\":{}", json_str(self.phase.name())),
      "\"severity\":\"error\"".to_string(),
      format!("\"kind\":{}", json_str(self.kind)),
      format!("\"code\":{}", json_str(self.code)),
      format!("\"message\":{}", json_str(&self.message)),
    ];
    if let Some(file) = &self.file {
      fields.push(format!("\"file\":{}", json_str(file)));
    }
    if let Some(line) = self.line {
      fields.push(format!("\"line\":{line}"));
    }
    if let Some(column) = self.column {
      fields.push(format!("\"column\":{column}"));
    }
    if let Some(shard) = &self.shard {
      fields.push(format!("\"shard\":{}", json_str(shard)));
    }
    if let Some(actual) = &self.actual {
      fields.push(format!("\"actual\":{}", type_json(actual)));
    }
    if !self.expected.is_empty() {
      let expected: Vec<String> = self.expected.iter().map(type_json).collect();
      fields.push(format!("\"expected\":[{}]", expected.join(",")));
    }
    if let Some(index) = self.param_index {
      fields.push(format!("\"param_index\":{index}"));
    }
    if let Some(param) = &self.param {
      fields.push(format!("\"param\":{}", json_str(param)));
    }
    if !self.did_you_mean.is_empty() {
      let names: Vec<String> = self.did_you_mean.iter().map(|n| json_str(n)).collect();
      fields.push(format!("\"did_you_mean\":[{}]", names.join(",")));
    }
    if let Some(r) = &self.related {
      let mut related = vec![format!("\"message\":{}", json_str(&r.message))];
      if let (Some(line), Some(column)) = (r.line, r.column) {
        related.push(format!("\"line\":{line},\"column\":{column}"));
      }
      fields.push(format!("\"related\":{{{}}}", related.join(",")));
    }
    if let Some(source) = &self.input_from {
      let step = |(index, name): &(usize, String)| {
        format!("{{\"shard\":{index},\"name\":{}}}", json_str(name))
      };
      let origin = match &source.origin {
        Some(o) => format!("\"origin\":{}", step(o)),
        None => "\"origin\":\"flow-input\"".to_string(),
      };
      let via: Vec<String> = source.via.iter().map(step).collect();
      fields.push(format!(
        "\"input_from\":{{{origin},\"via\":[{}]}}",
        via.join(",")
      ));
    }
    if !self.path.is_empty() {
      fields.push(format!("\"path\":{}", path_json(&self.path)));
    }
    format!("{{{}}}", fields.join(","))
  }
}

impl fmt::Display for Diagnostic {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match &self.shard {
      Some(shard) => write!(f, "{}: {}", shard, self.message),
      None => write!(f, "{}", self.message),
    }
  }
}

/// The 1.x `SHType` value of a type, mapped from the full type description
/// (not from the smaller [`TypeName`] used in shard descriptions).
fn basic_type(ty: Type) -> i32 {
  match ty.desc() {
    TypeDesc::None => 0,
    TypeDesc::Never => -1,
    TypeDesc::Any => 1,
    TypeDesc::Bool => 3,
    TypeDesc::Int => 4,
    TypeDesc::Float => 10,
    TypeDesc::Float2 => 11,
    TypeDesc::Float3 => 12,
    TypeDesc::Float4 => 13,
    TypeDesc::Bytes => 51,
    TypeDesc::String => 52,
    TypeDesc::Seq(_) => 56,
    TypeDesc::Table(_) => 57,
    // A union has no single 1.x type; `name` carries its printed form.
    TypeDesc::Union(_) => -1,
  }
}

fn type_json(t: &TypeRef) -> String {
  format!(
    "{{\"name\":{},\"basic_type\":{}}}",
    json_str(&t.name),
    t.basic_type
  )
}

/// Up to `max` names from `all` closest to `name` (case-insensitive edit
/// distance, at most a third of the length plus one), best first.
pub fn closest(name: &str, all: impl IntoIterator<Item = String>, max: usize) -> Vec<String> {
  let lower = name.to_lowercase();
  let limit = lower.chars().count() / 3 + 1;
  let mut scored: Vec<(usize, String)> = all
    .into_iter()
    .filter_map(|candidate| {
      let d = edit_distance(&lower, &candidate.to_lowercase());
      (d <= limit).then_some((d, candidate))
    })
    .collect();
  scored.sort();
  scored.into_iter().take(max).map(|(_, n)| n).collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
  let b: Vec<char> = b.chars().collect();
  let mut prev: Vec<usize> = (0..=b.len()).collect();
  for (i, ca) in a.chars().enumerate() {
    let mut cur = vec![i + 1];
    for (j, cb) in b.iter().enumerate() {
      let cost = usize::from(ca != *cb);
      cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
    }
    prev = cur;
  }
  prev[b.len()]
}

/// A JSON string literal.
pub fn json_str(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 2);
  out.push('"');
  for c in s.chars() {
    match c {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
      c => out.push(c),
    }
  }
  out.push('"');
  out
}

/// JSON for a semantic occurrence path, shared by diagnostics and analysis.
pub fn path_json(path: &[PathStep]) -> String {
  let steps: Vec<String> = path
    .iter()
    .map(|step| match step {
      PathStep::Wire(name) => format!("{{\"wire\":{}}}", json_str(name)),
      PathStep::Function(name) => format!("{{\"function\":{}}}", json_str(name)),
      PathStep::Shard { index, name } => {
        format!("{{\"shard\":{index},\"name\":{}}}", json_str(name))
      }
      PathStep::Param(name) => format!("{{\"param\":{}}}", json_str(name)),
      PathStep::Item(index) => format!("{{\"item\":{index}}}"),
      PathStep::Evaluation => "{\"evaluation\":true}".to_string(),
    })
    .collect();
  format!("[{}]", steps.join(","))
}
