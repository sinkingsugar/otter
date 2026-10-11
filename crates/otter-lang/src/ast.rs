//! The syntax tree. Every node keeps its span: lowering records them in the
//! source map, and diagnostics point back through it.

use crate::lexer::AssignOp;
use crate::source::Span;

#[derive(Clone, Debug, PartialEq)]
pub struct Spanned<T> {
  pub node: T,
  pub span: Span,
}

pub type Name = Spanned<String>;

#[derive(Clone, Debug, PartialEq)]
pub struct Program {
  pub statements: Vec<Statement>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
  /// `= x`: binds an immutable name.
  Assign {
    op: AssignOp,
    op_span: Span,
    var: Name,
  },
  Pipeline(Pipe),
}

/// Blocks joined by `|`. Inside `[...]`, table values and parameters, one
/// pipe is one element: `|` binds tighter than the space between elements.
#[derive(Clone, Debug, PartialEq)]
pub struct Pipe {
  pub blocks: Vec<Block>,
  pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
  pub kind: BlockKind,
  pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BlockKind {
  /// `Name` or `Name(...)`. A lowercase name followed by `(` is kept here
  /// too, so lowering can report it as a misspelled shard.
  Shard {
    name: Name,
    params: Option<Params>,
  },
  /// `@name` or `@name(...)`.
  Func {
    name: Name,
    params: Option<Params>,
  },
  Literal(Literal),
  /// `x`, `x.key`, `x.0.key`.
  Var {
    name: Name,
    path: Vec<Spanned<PathKey>>,
  },
  Seq(Vec<Pipe>),
  Table(Vec<(Spanned<TableKey>, Pipe)>),
  /// `{}`: an empty flow where a flow is expected, else an empty table.
  EmptyBraces,
  /// `{ ... }`: a nested flow.
  Flow(Vec<Statement>),
  /// `( ... )`.
  Expr(Vec<Statement>),
  /// `#( ... )`: evaluated while loading (not supported yet).
  Eval(Vec<Statement>),
  Enum(String, String),
  FString(Vec<FPart>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
  None,
  Bool(bool),
  Int(i64),
  Float(f64),
  String(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PathKey {
  Key(String),
  Index(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TableKey {
  Name(String),
  String(String),
  Int(i64),
  /// Any other value as a key (1.x allows them; 2.0 tables have string
  /// keys, and lowering rejects it).
  Value(Box<Block>),
}

impl TableKey {
  /// The key as a string (2.0 tables have string keys).
  pub fn text(&self) -> String {
    match self {
      TableKey::Name(s) | TableKey::String(s) => s.clone(),
      TableKey::Int(i) => i.to_string(),
      TableKey::Value(_) => "<value>".to_string(),
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub enum FPart {
  Text(String),
  Expr(Vec<Statement>, Span),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Params {
  pub items: Vec<Param>,
  pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
  pub name: Option<Name>,
  pub value: Pipe,
}
