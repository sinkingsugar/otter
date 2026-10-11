//! Static shard descriptions (docs/shard-metadata-and-compose.md §3).
//!
//! One [`ShardDesc`] per shard, on its `ShardType`. It drives discovery,
//! documentation and argument decoding: the decoder ([`crate::args`]) and
//! the catalog ([`crate::catalog`]) read the same [`ParamDecl`]s, so the
//! documented argument contract is the one that is enforced.

use crate::types::{Type, TypeDesc};

/// Documentation prose in a shard description (summaries, help, parameter
/// help). With the `docs` feature (on by default) it is the given text;
/// without it, an empty string, so small builds carry no prose. Parameter
/// names, forms, types and defaults are the argument contract and are
/// always kept.
#[cfg(feature = "docs")]
#[macro_export]
macro_rules! shard_doc {
  ($text:expr) => {
    $text
  };
}

/// Documentation prose in a shard description; compiled out (empty)
/// without the `docs` feature.
#[cfg(not(feature = "docs"))]
#[macro_export]
macro_rules! shard_doc {
  ($text:expr) => {
    ""
  };
}
use crate::var::Var;

/// A value type, as written in descriptions. Symbolic and const-friendly;
/// internal [`Type`] handles are not portable documentation identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeName {
  None,
  Any,
  Bool,
  Int,
  Float,
  Float2,
  Float3,
  Float4,
  String,
  Bytes,
  /// Any sequence.
  Seq,
  /// Any table.
  Table,
}

impl TypeName {
  pub fn name(self) -> &'static str {
    match self {
      TypeName::None => "None",
      TypeName::Any => "Any",
      TypeName::Bool => "Bool",
      TypeName::Int => "Int",
      TypeName::Float => "Float",
      TypeName::Float2 => "Float2",
      TypeName::Float3 => "Float3",
      TypeName::Float4 => "Float4",
      TypeName::String => "String",
      TypeName::Bytes => "Bytes",
      TypeName::Seq => "Seq",
      TypeName::Table => "Table",
    }
  }

  /// The 1.x `SHType` value, for diagnostics compatible with `shards check
  /// --json` (`basic_type`).
  pub fn basic_type(self) -> i32 {
    match self {
      TypeName::None => 0,
      TypeName::Any => 1,
      TypeName::Bool => 3,
      TypeName::Int => 4,
      TypeName::Float => 10,
      TypeName::Float2 => 11,
      TypeName::Float3 => 12,
      TypeName::Float4 => 13,
      TypeName::Bytes => 51,
      TypeName::String => 52,
      TypeName::Seq => 56,
      TypeName::Table => 57,
    }
  }

  pub fn to_type(self) -> Type {
    match self {
      TypeName::None => Type::none(),
      TypeName::Any => Type::any(),
      TypeName::Bool => Type::bool(),
      TypeName::Int => Type::int(),
      TypeName::Float => Type::float(),
      TypeName::Float2 => Type::float2(),
      TypeName::Float3 => Type::float3(),
      TypeName::Float4 => Type::float4(),
      TypeName::String => Type::string(),
      TypeName::Bytes => Type::bytes(),
      TypeName::Seq => Type::seq(Type::any()),
      TypeName::Table => Type::any_table(),
    }
  }

  /// Whether a value of type `ty` is acceptable for this name
  /// ([`Type::accepts`]).
  pub fn matches(self, ty: Type) -> bool {
    self.to_type().accepts(ty)
  }

  /// The symbolic name of a type handle, if it has one.
  pub fn of(ty: Type) -> Option<TypeName> {
    Some(match ty.desc() {
      TypeDesc::None => TypeName::None,
      TypeDesc::Never => return None,
      TypeDesc::Any => TypeName::Any,
      TypeDesc::Bool => TypeName::Bool,
      TypeDesc::Int => TypeName::Int,
      TypeDesc::Float => TypeName::Float,
      TypeDesc::Float2 => TypeName::Float2,
      TypeDesc::Float3 => TypeName::Float3,
      TypeDesc::Float4 => TypeName::Float4,
      TypeDesc::String => TypeName::String,
      TypeDesc::Bytes => TypeName::Bytes,
      _ if ty == Type::seq(Type::any()) => TypeName::Seq,
      _ if ty == Type::any_table() => TypeName::Table,
      TypeDesc::Seq(_) | TypeDesc::Table(_) | TypeDesc::Union(_) => return None,
    })
  }
}

/// Which argument forms a parameter accepts. A string literal, a reference
/// to a String variable and a wire name are distinct forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forms(u8);

impl Forms {
  pub const LITERAL: Forms = Forms(1);
  pub const VARIABLE: Forms = Forms(2);
  pub const WIRE: Forms = Forms(4);
  pub const FLOW: Forms = Forms(8);
  /// A sequence of value-flow pairs: `[value {flow} value {flow}]`.
  pub const CASES: Forms = Forms(16);

  pub const fn or(self, other: Forms) -> Forms {
    Forms(self.0 | other.0)
  }

  pub fn contains(self, other: Forms) -> bool {
    self.0 & other.0 == other.0
  }

  pub fn names(self) -> Vec<&'static str> {
    [
      (Forms::LITERAL, "literal"),
      (Forms::VARIABLE, "variable"),
      (Forms::WIRE, "wire"),
      (Forms::FLOW, "flow"),
      (Forms::CASES, "cases"),
    ]
    .into_iter()
    .filter(|(f, _)| self.contains(*f))
    .map(|(_, n)| n)
    .collect()
  }
}

/// A default value, as written in descriptions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DefaultValue {
  None,
  Bool(bool),
  Int(i64),
  Float(f64),
  Str(&'static str),
}

impl DefaultValue {
  pub fn to_var(self) -> Var {
    match self {
      DefaultValue::None => Var::None,
      DefaultValue::Bool(b) => Var::Bool(b),
      DefaultValue::Int(i) => Var::Int(i),
      DefaultValue::Float(f) => Var::Float(f),
      DefaultValue::Str(s) => Var::string(s),
    }
  }
}

/// Whether a parameter must be given. `Optional` (absent when omitted) is
/// distinct from `Default(DefaultValue::None)`. `Variadic` takes every
/// remaining positional argument (zero or more); only the last parameter
/// can be variadic, and it cannot be given by name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Requirement {
  Required,
  Optional,
  Default(DefaultValue),
  Variadic,
}

/// One parameter, in positional order.
#[derive(Clone, Copy, Debug)]
pub struct ParamDecl {
  pub name: &'static str,
  pub help: &'static str,
  pub forms: Forms,
  /// Accepted value types for literals (and the documented types for
  /// variable references, whose binding compose checks). Empty for wire
  /// and flow parameters.
  pub types: &'static [TypeName],
  pub requirement: Requirement,
  /// The full type, when `types` cannot say it (`[Int]`, `Float4 | None`,
  /// a table with given keys). When set, it is what literals (in the
  /// decoder) and variables (in `Operand::compose_arg`) are checked
  /// against, and what the catalog documents; `types` then only gives the
  /// 1.x type codes. A function, so declarations stay `const`.
  pub ty: Option<fn() -> Type>,
}

impl ParamDecl {
  /// A declaration without a full type; add one with [`ParamDecl::typed`].
  pub const fn new(
    name: &'static str,
    help: &'static str,
    forms: Forms,
    types: &'static [TypeName],
    requirement: Requirement,
  ) -> ParamDecl {
    ParamDecl {
      name,
      help,
      forms,
      types,
      requirement,
      ty: None,
    }
  }

  /// A declaration with a full type and no `TypeName` list: the catalog
  /// derives the 1.x type code from the full type.
  pub const fn new_typed(
    name: &'static str,
    help: &'static str,
    forms: Forms,
    requirement: Requirement,
    ty: fn() -> Type,
  ) -> ParamDecl {
    ParamDecl {
      name,
      help,
      forms,
      types: &[],
      requirement,
      ty: Some(ty),
    }
  }

  /// Sets the full type literals and variables are checked against. The
  /// `TypeName` list is then not used for checks; it can stay empty.
  pub const fn typed(self, ty: fn() -> Type) -> ParamDecl {
    ParamDecl {
      ty: Some(ty),
      ..self
    }
  }

  /// Whether a value of type `actual` is acceptable: the full type when
  /// declared, else one of `types` (any type when `types` is empty).
  pub fn accepts(&self, actual: Type) -> bool {
    match self.ty {
      Some(ty) => ty().accepts(actual),
      None => self.types.is_empty() || self.types.iter().any(|t| t.matches(actual)),
    }
  }

  /// The accepted types, for messages: the full type, or `types`.
  pub fn expected(&self) -> String {
    match self.ty {
      Some(ty) => ty().to_string(),
      None => self
        .types
        .iter()
        .map(|t| t.name())
        .collect::<Vec<_>>()
        .join(" or "),
    }
  }
}

/// A shard's parameters.
#[derive(Clone, Copy, Debug)]
pub enum Params {
  /// Declared: decoded by the shared decoder (names, positions, defaults,
  /// forms and literal types).
  Declared(&'static [ParamDecl]),
  /// Not yet described: positional arguments are passed through to the
  /// shard's compose unchanged. Named arguments are rejected.
  Undeclared,
}

/// The input a shard accepts (a broad constraint; compose decides exactly).
#[derive(Clone, Copy, Debug)]
pub enum InputDesc {
  Any,
  /// The input is not used.
  Ignored,
  Types(&'static [TypeName]),
  /// A full type, when a `TypeName` list cannot say it (a table with given
  /// keys, `[Int]`, a union), like `ParamDecl::typed`. A function, so
  /// descriptions stay `const`.
  Typed(fn() -> Type),
}

/// A shard's output. Outputs that depend on compose are not presented as
/// an exact static type.
#[derive(Clone, Copy, Debug)]
pub enum OutputDesc {
  Fixed(TypeName),
  /// The input value passes through unchanged.
  Passthrough,
  /// A new value of the same type as the input.
  SameAsInput,
  /// Decided by compose; the text explains how.
  Dynamic(&'static str),
}

/// Where a shard can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Targets {
  All,
  NativeOnly,
}

impl Targets {
  pub fn name(self) -> &'static str {
    match self {
      Targets::All => "all",
      Targets::NativeOnly => "native-only",
    }
  }
}

/// The static description of one shard kind.
#[derive(Clone, Copy, Debug)]
pub struct ShardDesc {
  /// Trusted native effect contract; omission through `undocumented` is unknown.
  pub effects: crate::signature::Effects,
  pub lifetime: crate::signature::Lifetime,
  pub name: &'static str,
  /// Bump when compose output changes for the same inputs (not for help
  /// edits). The compose cache is process-local, see the design doc §4.
  pub version: u32,
  pub summary: &'static str,
  pub help: &'static str,
  pub params: Params,
  pub input: InputDesc,
  pub output: OutputDesc,
  pub targets: Targets,
  /// Other names that resolve to this shard (1.x short forms such as
  /// `Add` for `Math.Add`). Diagnostics use the canonical name.
  pub aliases: &'static [&'static str],
}

impl ShardDesc {
  /// A shard not yet described: name and version only, positional
  /// arguments passed through.
  pub const fn undocumented(name: &'static str, version: u32) -> ShardDesc {
    ShardDesc {
      effects: crate::signature::Effects::UNKNOWN,
      lifetime: crate::signature::Lifetime::Unknown,
      name,
      version,
      summary: "",
      help: "",
      params: Params::Undeclared,
      input: InputDesc::Any,
      output: OutputDesc::Dynamic("not described yet"),
      targets: Targets::All,
      aliases: &[],
    }
  }

  pub fn is_documented(&self) -> bool {
    matches!(self.params, Params::Declared(_))
  }
}

impl DefaultValue {
  /// Whether this default is a value the declared types accept.
  const fn conforms(self, types: &[TypeName]) -> bool {
    if types.is_empty() {
      return true;
    }
    let mut i = 0;
    while i < types.len() {
      let ok = matches!(
        (self, types[i]),
        (_, TypeName::Any)
          | (DefaultValue::None, TypeName::None)
          | (DefaultValue::Bool(_), TypeName::Bool)
          | (DefaultValue::Int(_), TypeName::Int)
          | (DefaultValue::Float(_), TypeName::Float)
          | (DefaultValue::Str(_), TypeName::String)
      );
      if ok {
        return true;
      }
      i += 1;
    }
    false
  }
}

/// Checks parameter declarations: names are unique, and every default is a
/// literal the parameter itself accepts (so a default can never bypass the
/// declared contract), and only the last parameter is variadic. Used at compile time by `ShardType::new`, and by the
/// decoder for descriptions used directly. Returns the offending
/// parameter's index.
pub const fn check_params(decls: &[ParamDecl]) -> Result<(), usize> {
  let mut i = 0;
  while i < decls.len() {
    let mut j = i + 1;
    while j < decls.len() {
      if str_eq(decls[i].name, decls[j].name) {
        return Err(j);
      }
      j += 1;
    }
    if let Requirement::Default(default) = decls[i].requirement
      && (decls[i].forms.0 & Forms::LITERAL.0 == 0 || !default.conforms(decls[i].types))
    {
      return Err(i);
    }
    if matches!(decls[i].requirement, Requirement::Variadic) && i + 1 != decls.len() {
      return Err(i);
    }
    i += 1;
  }
  Ok(())
}

/// `const` string equality, for compile-time checks.
pub(crate) const fn str_eq(a: &str, b: &str) -> bool {
  let (a, b) = (a.as_bytes(), b.as_bytes());
  if a.len() != b.len() {
    return false;
  }
  let mut i = 0;
  while i < a.len() {
    if a[i] != b[i] {
      return false;
    }
    i += 1;
  }
  true
}
