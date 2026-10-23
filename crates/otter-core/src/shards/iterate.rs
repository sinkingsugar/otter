//! Iteration over a collection: `ForEach`, `Map`, `Filter` and `Reduce`
//! (docs/iteration-and-access.md §4).
//!
//! Each composes its body as a block with the names it binds (value first,
//! then the index or key; `Reduce`'s accumulator before them) declared at
//! its start, and the body's input is the element. Like `Repeat`, they are
//! never nodes of their own: compose lowers them to flat code in the parent
//! flow (`ComposeCtx::flatten`), a counted loop over the collection read
//! once before the first iteration. A body shard without a VM form stays a
//! node the engine activates where the run stops, so a body may suspend and
//! the loop resumes at the same element.

use std::sync::Arc;

use super::*;
use crate::compose::Binding;
use crate::var::Table;

/// Which iteration shard: what the loop builds, and how its messages name
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LoopKind {
  ForEach,
  Map,
  Filter,
  Reduce,
}

impl LoopKind {
  pub fn name(self) -> &'static str {
    match self {
      LoopKind::ForEach => "ForEach",
      LoopKind::Map => "Map",
      LoopKind::Filter => "Filter",
      LoopKind::Reduce => "Reduce",
    }
  }
}

/// What the input is, as compose knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
  /// A sequence type: its elements by index, through the VM's `Take`.
  Seq,
  /// A table type: its entries in key order.
  Table,
  /// `Any`, or a union holding a collection: checked at activation.
  Dynamic,
}

/// A composed iteration, lowered by `ComposeCtx::flatten`.
pub struct LoopCompiled {
  pub(crate) kind: LoopKind,
  pub(crate) source: Source,
  pub(crate) body: Arc<CompiledFlow>,
  /// The input's type (the collection's slot).
  pub(crate) input: Type,
  /// The element's type (the body's input).
  pub(crate) element: Type,
  /// The bindings of the names given: the value, the index or key, and
  /// `Reduce`'s accumulator.
  pub(crate) value: Option<Binding>,
  pub(crate) index: Option<Binding>,
  pub(crate) acc: Option<Binding>,
  /// `Reduce`'s accumulator type, and its initial value (`None`: the
  /// first element).
  pub(crate) acc_type: Type,
  pub(crate) init: Option<Operand>,
  /// The output's type (`Map`'s sequence or table).
  pub(crate) output: Type,
}

const NAME: Forms = Forms::VARIABLE;

macro_rules! name_decls {
  ($value:expr, $index:expr) => {
    [
      decl(
        "action",
        ACTION_HELP,
        Forms::FLOW,
        NONE_TYPES,
        Requirement::Required,
      ),
      decl("value", $value, NAME, NONE_TYPES, Requirement::Optional),
      decl("index", $index, NAME, NONE_TYPES, Requirement::Optional),
    ]
  };
}

const ACTION_HELP: &str = crate::shard_doc!(
  "The body, run once per element with the element as its input. It may run zero times, so variables it assigns are not definitely assigned after the loop."
);
const VALUE_HELP: &str = crate::shard_doc!(
  "The immutable name bound to each element (a table's value), in the body. Written first in the parentheses: `ForEach(x {...})`; `_` skips it."
);
const INDEX_HELP: &str = crate::shard_doc!(
  "The immutable name bound to each index (an Int from 0; a table's key, a String), in the body. Written after the value: `ForEach(x i {...})`."
);

pub static FOR_EACH_PARAMS: &[ParamDecl] = &name_decls!(VALUE_HELP, INDEX_HELP);
pub static MAP_PARAMS: &[ParamDecl] = &name_decls!(VALUE_HELP, INDEX_HELP);
pub static FILTER_PARAMS: &[ParamDecl] = &name_decls!(VALUE_HELP, INDEX_HELP);
pub static REDUCE_PARAMS: &[ParamDecl] = &[
  decl(
    "action",
    crate::shard_doc!(
      "The body, run once per element with the element as its input: its output is the next accumulator."
    ),
    Forms::FLOW,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "acc",
    crate::shard_doc!(
      "The immutable name bound to the accumulator, in the body. Written first: `Reduce(acc x {...} init: 0)`."
    ),
    NAME,
    NONE_TYPES,
    Requirement::Optional,
  ),
  decl("value", VALUE_HELP, NAME, NONE_TYPES, Requirement::Optional),
  decl("index", INDEX_HELP, NAME, NONE_TYPES, Requirement::Optional),
  decl(
    "init",
    crate::shard_doc!(
      "The accumulator's initial value (its type is the accumulator's): a literal, or a variable read when the loop starts. Without it, the first element starts it and the body runs from the second; an empty input then fails."
    ),
    Forms::LITERAL.or(Forms::VARIABLE),
    NONE_TYPES,
    Requirement::Optional,
  ),
];

const fn loop_desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  params: &'static [ParamDecl],
  output: OutputDesc,
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(params),
    input: InputDesc::Types(&[TypeName::Seq, TypeName::Table, TypeName::Any]),
    output,
    targets: Targets::All,
    aliases: &[],
    effects: crate::signature::Effects::NONE,
    lifetime: crate::signature::Lifetime::Stateless,
  }
}

pub const FOR_EACH_DESC: ShardDesc = loop_desc(
  "ForEach",
  crate::shard_doc!("Runs a flow once per element of a sequence, or per entry of a table."),
  crate::shard_doc!(
    "`rows | ForEach(row i { ... })`: the names come first, value then index (a table's value then key, entries in key order), both optional; the body's input is the element. The input is read once, before the first iteration: assigning the variable it came from inside the body does not change what is iterated. ForEach passes its input through. A Stop, Restart or Return in the body propagates; the body may suspend, and the loop resumes at the same element. The input is a sequence, a table, or Any or a union holding one (checked when the loop starts); a String is not iterable (`String.Split` or `String.CodePoints` first)."
  ),
  FOR_EACH_PARAMS,
  OutputDesc::Passthrough,
);

pub const MAP_DESC: ShardDesc = loop_desc(
  "Map",
  crate::shard_doc!("A sequence (or table) of a flow's outputs, one per element."),
  crate::shard_doc!(
    "`items | Map(x { x | Math.Multiply(2) })` or `rows | Map({.name})`: names as for ForEach. On a sequence the output is a sequence of the body's outputs; on a table, a table with the same keys and the body's outputs as values."
  ),
  MAP_PARAMS,
  OutputDesc::Dynamic(crate::shard_doc!(
    "a sequence (a table, for a table input) of the body's output type"
  )),
);

pub const FILTER_DESC: ShardDesc = loop_desc(
  "Filter",
  crate::shard_doc!("The elements (or table entries) for which a predicate outputs true."),
  crate::shard_doc!(
    "`items | Filter({IsMore(10)})` or `rows | Filter(r { r.age | IsMore(17) })`: names as for ForEach; the predicate receives the element and must output a Bool. The output has the input's type and keeps the order."
  ),
  FILTER_PARAMS,
  OutputDesc::Passthrough,
);

pub const REDUCE_DESC: ShardDesc = loop_desc(
  "Reduce",
  crate::shard_doc!("Folds a sequence (or a table's values) into one value."),
  crate::shard_doc!(
    "`items | Reduce(acc x { x | Add(acc) } init: 0)`: the accumulator's name comes first, then the value and the index (all optional); the body's input is the element and its output the next accumulator, which `init:`'s type must accept. Without `init:` the first element starts the accumulator. The output is the last accumulator."
  ),
  REDUCE_PARAMS,
  OutputDesc::Dynamic(crate::shard_doc!("the accumulator's type")),
);

fn loop_error(
  args: &Args,
  kind: LoopKind,
  param: &str,
  code: &'static str,
  message: String,
) -> Error {
  param_error(args, kind.name(), param, "compose-error", code, message)
}

/// The element and index types of an iterable input, and how it is read.
fn iterable(input: Type) -> Option<(Source, Type, Type)> {
  let table_element =
    |t: &crate::types::TableType| Type::union(t.keys.iter().map(|(_, ty)| *ty).chain(t.rest));
  match input.desc() {
    TypeDesc::Seq(element) => Some((Source::Seq, *element, Type::int())),
    TypeDesc::Table(t) => Some((Source::Table, table_element(t), Type::string())),
    TypeDesc::Any => Some((
      Source::Dynamic,
      Type::any(),
      Type::union([Type::int(), Type::string()]),
    )),
    TypeDesc::Union(members) => {
      let (mut elements, mut indices) = (Vec::new(), Vec::new());
      for member in members {
        match member.desc() {
          TypeDesc::Seq(element) => {
            elements.push(*element);
            indices.push(Type::int());
          }
          TypeDesc::Table(t) => {
            elements.push(table_element(t));
            indices.push(Type::string());
          }
          _ => {}
        }
      }
      (!elements.is_empty()).then(|| (Source::Dynamic, Type::union(elements), Type::union(indices)))
    }
    _ => None,
  }
}

pub(crate) fn compose_loop(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
  kind: LoopKind,
) -> Result<Composed<LoopCompiled>> {
  let input = ctx.input();
  let Some((source, element, index_type)) = iterable(input) else {
    let hint = if input == Type::string() {
      " (a String is not iterable: `String.Split` or `String.CodePoints` first)"
    } else {
      ""
    };
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "input-type-mismatch",
        "input-type-mismatch",
        format!(
          "{} needs a sequence or a table input, got {input}{hint}",
          kind.name()
        ),
      )
      .shard(kind.name())
      .types(
        Some(TypeRef::of(input)),
        vec![
          TypeRef::named(TypeName::Seq),
          TypeRef::named(TypeName::Table),
        ],
      ),
    )));
  };
  // Reduce's accumulator: init's type, or the element's.
  let init = match kind {
    LoopKind::Reduce => Operand::compose_optional_arg(args, "init", kind.name(), ctx)?,
    _ => None,
  };
  let acc_type = init.as_ref().map_or(element, |(_, ty)| *ty);
  // The names, checked as any declaration, then declared in the body.
  let mut names: Vec<(&str, Type, &str)> = Vec::new();
  if kind == LoopKind::Reduce
    && let Some(name) = args.variable("acc")
  {
    names.push((name, acc_type, "acc"));
  }
  if let Some(name) = args.variable("value") {
    names.push((name, element, "value"));
  }
  if let Some(name) = args.variable("index") {
    names.push((name, index_type, "index"));
  }
  for (i, (name, _, param)) in names.iter().enumerate() {
    check_declaration_of(args, ctx, kind.name(), param)?;
    if names[..i].iter().any(|(other, ..)| other == name) {
      return Err(loop_error(
        args,
        kind,
        param,
        "duplicate-binding",
        format!(
          "{name} names two things in this {}; pick another name",
          kind.name()
        ),
      ));
    }
  }
  let declared: Vec<(&str, Type)> = names.iter().map(|(n, t, _)| (*n, *t)).collect();
  let action = args.flow("action").expect("decoded action flow");
  let (body, bindings) =
    ctx.conditional_region(|ctx| ctx.compose_flow_declaring(action, element, &declared))?;
  let binding_of = |param: &str| {
    names
      .iter()
      .zip(&bindings)
      .find(|((_, _, p), _)| *p == param)
      .map(|(_, b)| *b)
  };
  let output = match kind {
    LoopKind::ForEach => input,
    LoopKind::Filter => {
      if !Type::bool().accepts(body.output) {
        return Err(loop_error(
          args,
          kind,
          "action",
          "predicate-not-bool",
          format!("Filter's predicate must output a Bool, got {}", body.output),
        ));
      }
      filtered(input)
    }
    LoopKind::Map => {
      let seq = Type::seq(body.output);
      let table = Type::table(Vec::<(Arc<str>, Type)>::new(), Some(body.output));
      match source {
        Source::Seq => seq,
        Source::Table => table,
        Source::Dynamic => {
          let (seqs, tables) = match input.desc() {
            TypeDesc::Union(members) => (
              members.iter().any(|m| matches!(m.desc(), TypeDesc::Seq(_))),
              members
                .iter()
                .any(|m| matches!(m.desc(), TypeDesc::Table(_))),
            ),
            _ => (true, true),
          };
          Type::union(
            [(seqs, seq), (tables, table)]
              .into_iter()
              .filter_map(|(present, ty)| present.then_some(ty)),
          )
        }
      }
    }
    LoopKind::Reduce => {
      if !acc_type.accepts(body.output) {
        return Err(loop_error(
          args,
          kind,
          "action",
          "wrong-output-type",
          format!(
            "Reduce's body outputs {}, which its accumulator ({acc_type}) does not accept",
            body.output
          ),
        ));
      }
      acc_type
    }
  };
  Ok(Composed {
    compiled: LoopCompiled {
      kind,
      source,
      value: binding_of("value"),
      index: binding_of("index"),
      acc: binding_of("acc"),
      body: Arc::new(body),
      input,
      element,
      acc_type,
      init: init.map(|(operand, _)| operand),
      output,
    },
    output,
  })
}

/// What `Filter` outputs on `input`: a sequence keeps its type; a table
/// becomes an open table of its values' type (a fixed table may lose
/// keys).
fn filtered(input: Type) -> Type {
  let one = |ty: Type| match ty.desc() {
    TypeDesc::Table(t) if t.rest.is_none() => Type::table(
      Vec::<(Arc<str>, Type)>::new(),
      Some(Type::union(t.keys.iter().map(|(_, ty)| *ty))),
    ),
    _ => ty,
  };
  match input.desc() {
    TypeDesc::Union(members) => Type::union(members.iter().map(|m| one(*m))),
    _ => one(input),
  }
}

/// How many elements or entries `value` has: the loop's length, checked
/// once when it starts (a `Dynamic` or table source).
pub(crate) fn iter_len(value: &Var, kind: LoopKind) -> Result<i64> {
  match value {
    Var::Seq(items) => Ok(items.len() as i64),
    Var::Table(table) => Ok(table.len() as i64),
    other => Err(not_iterable(other, kind)),
  }
}

#[cold]
fn not_iterable(value: &Var, kind: LoopKind) -> Error {
  Error::Activation(format!(
    "{} needs a sequence or a table, got {}",
    kind.name(),
    super::data::kind_name(value)
  ))
}

/// Element `index` of a sequence, or the value of entry `index` of a table.
/// Out of range only for `Reduce`'s first element of an empty input.
pub(crate) fn element(value: &Var, index: &Var, kind: LoopKind) -> Result<Var> {
  let Var::Int(i) = index else {
    return Err(Error::Activation(format!(
      "{}: the index is not an Int",
      kind.name()
    )));
  };
  let i = *i as usize;
  let found = match value {
    Var::Seq(items) => items.get(i),
    Var::Table(table) => table.slot(i),
    other => return Err(not_iterable(other, kind)),
  };
  match found {
    Some(v) => Ok(v.clone()),
    None => Err(Error::Activation(format!(
      "{}: the input is empty and there is no `init:` to start the accumulator",
      kind.name()
    ))),
  }
}

/// The index name's value: the index itself for a sequence, the key of
/// entry `index` for a table.
pub(crate) fn entry_key(value: &Var, index: &Var) -> Result<Var> {
  match (value, index) {
    (Var::Table(table), Var::Int(i)) => match table.key_at(*i as usize) {
      Some(key) => Ok(Var::from_string(Arc::clone(key))),
      None => Err(Error::Activation("the table has no entry here".into())),
    },
    (_, Var::Int(i)) => Ok(Var::Int(*i)),
    _ => Err(Error::Activation("the index is not an Int".into())),
  }
}

/// What a `Map` or `Filter` over a table (or a `Dynamic` input) outputs,
/// from what its loop collected: `Map`, the body's outputs (a table with
/// the input's keys, for a table); `Filter`, the kept indices (the elements
/// or entries at them).
pub(crate) fn rebuild(input: &Var, collected: &Var, filter: bool) -> Result<Var> {
  let Var::Seq(items) = collected else {
    return Err(Error::Activation(
      "Map: the outputs are not a sequence".into(),
    ));
  };
  let index = |v: &Var| match v {
    Var::Int(i) => *i as usize,
    _ => usize::MAX,
  };
  Ok(match (input, filter) {
    (Var::Seq(_), false) => collected.clone(),
    (Var::Seq(source), true) => Var::seq(
      items
        .iter()
        .filter_map(|i| source.get(index(i)).cloned())
        .collect(),
    ),
    (Var::Table(table), false) => match table.shape() {
      Some(shape) if shape.len() == items.len() => {
        Var::from_table(Table::with_shape(shape, items.iter().cloned()))
      }
      _ => {
        let mut builder = Table::builder();
        for (i, value) in items.iter().enumerate() {
          if let Some(key) = table.key_at(i) {
            builder.insert(Arc::clone(key), value.clone());
          }
        }
        Var::from_table(builder.build())
      }
    },
    (Var::Table(table), true) => {
      let mut builder = Table::builder();
      for i in items.iter() {
        let i = index(i);
        if let (Some(key), Some(value)) = (table.key_at(i), table.slot(i)) {
          builder.insert(Arc::clone(key), value.clone());
        }
      }
      Var::from_table(builder.build())
    }
    (other, _) => return Err(not_iterable(other, LoopKind::Map)),
  })
}
