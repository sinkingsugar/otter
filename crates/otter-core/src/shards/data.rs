//! Shards over values: reading (`Take`), building (`Seq.Make`,
//! `Table.Make`, `String.Format`) and appending (`Push`). All are leaf
//! shards (one implementation for both schedulers). The frontend lowers
//! `t.key` / `s.0`, computed sequence and table elements and f-strings
//! onto them.

use std::sync::Arc;

use super::leaf::{LeafShard, leaf_type};
use super::*;
use crate::diagnostic::closest;
use crate::instance::{InstanceCtx, LeafCtx};
use crate::shard::Flow;
use crate::types::Shape;
use crate::var::Table;

pub static TAKE: ShardType = leaf_type::<Take>();
pub static PUSH: ShardType = leaf_type::<Push>();
pub static SEQ_MAKE: ShardType = leaf_type::<SeqMake>();
pub static TABLE_MAKE: ShardType = leaf_type::<TableMake>();
pub static STRING_FORMAT: ShardType = leaf_type::<StringFormat>();

const OPERAND: Forms = Forms::LITERAL.or(Forms::VARIABLE);

fn compose_error(shard: &str, kind: &'static str, code: &'static str, message: String) -> Error {
  Error::Diagnostic(Box::new(
    Diagnostic::new(Phase::Compose, kind, code, message).shard(shard),
  ))
}

// --- Take ---

pub static TAKE_PARAMS: &[ParamDecl] = &[decl(
  "key",
  crate::shard_doc!(
    "An Int index into a sequence or vector, or a String key into a table: a literal, or a variable read at activation."
  ),
  OPERAND,
  &[TypeName::Int, TypeName::String],
  Requirement::Required,
)];

pub const TAKE_DESC: ShardDesc = ShardDesc {
  name: "Take",
  version: 1,
  summary: crate::shard_doc!("Outputs one element of a sequence, table or vector."),
  help: crate::shard_doc!(
    "On a fixed table, a literal key must be one of its keys (a compose error lists them) and the output has that key's type. On an open table, a missing key outputs none. An index out of range is an activation error. `t.key` and `s.0` are Take."
  ),
  params: Params::Declared(TAKE_PARAMS),
  input: InputDesc::Types(&[
    TypeName::Seq,
    TypeName::Table,
    TypeName::Float2,
    TypeName::Float3,
    TypeName::Float4,
  ]),
  output: OutputDesc::Dynamic(crate::shard_doc!("the element's type")),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct Take;

/// What a `Take` does at activation: look a key up, or read the slot a
/// literal key resolved to on a fixed table (golden path §7.3).
#[derive(Clone, Debug)]
pub enum TakeCode {
  Key(Operand),
  Slot(usize),
}

impl LeafShard for Take {
  type Compiled = TakeCode;
  type State = ();
  const DESC: ShardDesc = TAKE_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<TakeCode>> {
    let (key, key_ty) = Operand::compose_arg(args, "key", "Take", ctx)?;
    let input = ctx.input();
    let literal = match &key {
      Operand::Const(v) => Some(v.clone()),
      Operand::Bound(_) => None,
    };
    let mut code = TakeCode::Key(key);
    let key_error = |expected: TypeName| {
      Err(
        compose_error(
          "Take",
          "compose-error",
          "wrong-key-type",
          format!(
            "Take on {input} needs a {} key, got {key_ty}",
            expected.name()
          ),
        )
        .with_param("key", 0),
      )
    };
    let output = match input.desc() {
      TypeDesc::Seq(element) => {
        if key_ty != Type::int() {
          return key_error(TypeName::Int);
        }
        *element
      }
      TypeDesc::Float2 | TypeDesc::Float3 | TypeDesc::Float4 => {
        if key_ty != Type::int() {
          return key_error(TypeName::Int);
        }
        Type::float()
      }
      TypeDesc::Table(table) => {
        if key_ty != Type::string() {
          return key_error(TypeName::String);
        }
        match &literal {
          Some(Var::String(k)) => {
            match table.keys.binary_search_by(|(name, _)| (**name).cmp(&***k)) {
              Ok(index) => {
                if table.is_fixed() {
                  code = TakeCode::Slot(index);
                }
                table.keys[index].1
              }
              Err(_) => match table.rest {
                // An open table may not have the key at runtime.
                Some(rest) => Type::union([rest, Type::none()]),
                None => {
                  let keys: Vec<String> = table.keys.iter().map(|(n, _)| n.to_string()).collect();
                  let mut d = Diagnostic::new(
                    Phase::Compose,
                    "compose-error",
                    "unknown-key",
                    format!("{input} has no key `{}` (keys: {})", &***k, keys.join(", ")),
                  )
                  .shard("Take")
                  .param("key", Some(0));
                  d.did_you_mean = closest(k, keys, 3);
                  return Err(Error::Diagnostic(Box::new(d)));
                }
              },
            }
          }
          // A key read at activation: any value type, or none if missing.
          _ => Type::union(
            table
              .keys
              .iter()
              .map(|(_, t)| *t)
              .chain(table.rest)
              .chain([Type::none()]),
          ),
        }
      }
      _ => {
        return Err(Error::Diagnostic(Box::new(
          Diagnostic::new(
            Phase::Compose,
            "input-type-mismatch",
            "input-type-mismatch",
            format!("Take needs a sequence, table or vector input, got {input}"),
          )
          .shard("Take")
          .types(
            Some(TypeRef::of(input)),
            [TypeName::Seq, TypeName::Table, TypeName::Float3]
              .into_iter()
              .map(TypeRef::named)
              .collect(),
          ),
        )));
      }
    };
    Ok(Composed {
      compiled: code,
      output,
    })
  }

  fn instantiate(_: &TakeCode, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(code: &TakeCode, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    match code {
      TakeCode::Key(key) => {
        let key = key.get(ctx);
        take_value(input, &key).map(Flow::Next)
      }
      TakeCode::Slot(index) => take_slot(input, *index).map(Flow::Next),
    }
  }
}

/// The value at `index` in key order of a table admitted by a fixed type.
#[inline]
pub(crate) fn take_slot(input: &Var, index: usize) -> Result<Var> {
  match input {
    Var::Table(table) => match table.slot(index) {
      Some(value) => Ok(value.clone()),
      None => Err(Error::Activation(format!(
        "Take: the table has {} keys, not the {} its type declares",
        table.len(),
        index + 1
      ))),
    },
    _ => Err(Error::Activation("Take: input type mismatch".into())),
  }
}

/// The VM's `Take` inlines the sequence-by-index and table-by-key reads
/// and calls this for everything else.
pub(crate) fn take_value(input: &Var, key: &Var) -> Result<Var> {
  let value = match input {
    Var::Seq(items) => items[take_index(key, items.len())?].clone(),
    Var::Float2(v) => Var::Float(f64::from(v[take_index(key, 2)?])),
    Var::Float3(v) => Var::Float(f64::from(v[take_index(key, 3)?])),
    Var::Float4(v) => Var::Float(f64::from(v[take_index(key, 4)?])),
    Var::Table(entries) => match &key {
      Var::String(k) => entries.get(k).cloned().unwrap_or(Var::None),
      _ => return Err(Error::Activation("Take: the key must be a String".into())),
    },
    _ => return Err(Error::Activation("Take: input type mismatch".into())),
  };
  Ok(value)
}

/// `key` as an index into something of length `len`.
#[inline(always)]
fn take_index(key: &Var, len: usize) -> Result<usize> {
  match key {
    Var::Int(i) if *i >= 0 && (*i as usize) < len => Ok(*i as usize),
    Var::Int(i) => Err(Error::Activation(format!(
      "Take: index {i} is out of range (length {len})"
    ))),
    _ => Err(Error::Activation("Take: the key must be an Int".into())),
  }
}

// --- Push ---

pub static PUSH_PARAMS: &[ParamDecl] = &[decl(
  "variable",
  crate::shard_doc!("The existing mutable sequence variable to append to."),
  Forms::VARIABLE,
  NONE_TYPES,
  Requirement::Required,
)];

pub const PUSH_DESC: ShardDesc = ShardDesc {
  name: "Push",
  version: 1,
  summary: crate::shard_doc!("Appends the input to a sequence variable."),
  help: crate::shard_doc!(
    "`variable` must be a declared mutable sequence whose element type accepts the input: declare it first, for example `[] | Var(xs)`. Push neither declares nor clears it. Passes its input through."
  ),
  params: Params::Declared(PUSH_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct Push;

impl LeafShard for Push {
  type Compiled = Binding;
  type State = ();
  const DESC: ShardDesc = PUSH_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    let name = variable(args, "variable");
    let input = ctx.input();
    let info = super::assignable(args, ctx, "Push")?;
    ctx.read_var(name, "Push")?;
    let problem = match info.ty.desc() {
      TypeDesc::Seq(e) if e.accepts(input) => None,
      TypeDesc::Seq(_) => Some(format!("{name} is {}, cannot push {input}", info.ty)),
      _ => Some(format!("{name} is {}, not a sequence", info.ty)),
    };
    if let Some(message) = problem {
      return Err(param_error(
        args,
        "Push",
        "variable",
        "compose-error",
        "variable-type-mismatch",
        message,
      ));
    }
    ctx.mark_initialized(info.binding).map_err(|e| {
      e.in_shard(PUSH_DESC.name)
        .with_param("variable", args.param_index("variable"))
    })?;
    Ok(Composed {
      compiled: info.binding,
      output: input,
    })
  }

  fn instantiate(_: &Binding, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    // Take the value out of its slot so the sequence is unshared and grows
    // in place instead of being copied.
    let current = ctx.get(*b);
    if let Some(meter) = ctx.meter() {
      meter.nest(input)?;
      // Before taking it out of its slot: the slot's own reference counts.
      let shared = matches!(&current, Var::Seq(items) if Arc::strong_count(items) > 2);
      let bytes = match &current {
        Var::Seq(items) => (items.len() + 1) * std::mem::size_of::<Var>(),
        _ => 0,
      };
      if shared {
        meter.allocate(bytes)?;
      } else {
        meter.admit(bytes)?;
        meter.charge(std::mem::size_of::<Var>() as u64)?;
      }
    }
    ctx.set(*b, Var::None);
    let seq = match current.into_seq() {
      Ok(mut items) => {
        crate::var::seq_mut(&mut items).push(input.clone());
        Var::from_seq(items)
      }
      Err(other) => {
        ctx.set(*b, other);
        return Err(Error::Activation(
          "Push: the variable is not a sequence".into(),
        ));
      }
    };
    ctx.set(*b, seq);
    Ok(Flow::Next(input.clone()))
  }
}

// --- Seq.Make and Table.Make ---

pub static SEQ_MAKE_PARAMS: &[ParamDecl] = &[decl(
  "items",
  crate::shard_doc!("The elements: literals, or variables read at activation."),
  OPERAND,
  &[],
  Requirement::Variadic,
)];

pub const SEQ_MAKE_DESC: ShardDesc = ShardDesc {
  name: "Seq.Make",
  version: 1,
  summary: crate::shard_doc!("Builds a sequence from literals and variables."),
  help: crate::shard_doc!(
    "Ignores its input. The element type is the union of the items' types. The frontend lowers sequence literals with computed elements to Seq.Make."
  ),
  params: Params::Declared(SEQ_MAKE_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Dynamic(crate::shard_doc!("a sequence of the items' types")),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// Composes variadic literal-or-variable operands.
fn compose_operands(
  args: &Args,
  param: &str,
  shard: &str,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Vec<(Operand, Type)>> {
  let index = args.param_index(param);
  args
    .variadic(param)
    .iter()
    .map(|value| match value {
      ParamValue::Value(v) => Ok((Operand::Const(v.clone()), v.type_of())),
      ParamValue::Var(name) => {
        let info = ctx
          .read_var(name, shard)
          .map_err(|e| e.with_param(param, index))?;
        Ok((Operand::Bound(info.binding), info.ty))
      }
      other => unreachable!("decoder accepted {other:?} for {shard}.{param}"),
    })
    .collect()
}

pub struct SeqMake;

impl LeafShard for SeqMake {
  type Compiled = Vec<Operand>;
  type State = ();
  const DESC: ShardDesc = SEQ_MAKE_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Vec<Operand>>> {
    let items = compose_operands(args, "items", "Seq.Make", ctx)?;
    let element = if items.is_empty() {
      Type::any()
    } else {
      Type::union(items.iter().map(|(_, t)| *t))
    };
    Ok(Composed {
      compiled: items.into_iter().map(|(o, _)| o).collect(),
      output: Type::seq(element),
    })
  }

  fn instantiate(_: &Vec<Operand>, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(items: &Vec<Operand>, _: &mut (), ctx: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    if let Some(meter) = ctx.meter() {
      meter.allocate(items.len() * std::mem::size_of::<Var>())?;
      for item in items {
        meter.nest(&item.get(ctx))?;
      }
    }
    let mut values = Vec::with_capacity(items.len());
    for item in items {
      values.push(item.get(ctx));
    }
    Ok(Flow::Next(Var::from_seq(Arc::new(values))))
  }
}

pub static TABLE_MAKE_PARAMS: &[ParamDecl] = &[
  decl(
    "keys",
    crate::shard_doc!("The keys, as a literal sequence of distinct strings."),
    Forms::LITERAL,
    &[TypeName::Seq],
    Requirement::Required,
  ),
  decl(
    "values",
    crate::shard_doc!("One value per key, in order: literals, or variables read at activation."),
    OPERAND,
    &[],
    Requirement::Variadic,
  ),
];

pub const TABLE_MAKE_DESC: ShardDesc = ShardDesc {
  name: "Table.Make",
  version: 1,
  summary: crate::shard_doc!("Builds a fixed table from keys and values."),
  help: crate::shard_doc!(
    "Ignores its input. The output is a fixed table with exactly these keys. The frontend lowers table literals with computed values to Table.Make."
  ),
  params: Params::Declared(TABLE_MAKE_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Dynamic(crate::shard_doc!(
    "a fixed table of the keys and the values' types"
  )),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct TableMake;

/// A struct table under construction: its shape and one operand per key,
/// in key order.
#[derive(Clone, Debug)]
pub struct TableCode {
  pub shape: Shape,
  pub values: Vec<Operand>,
}

impl LeafShard for TableMake {
  type Compiled = TableCode;
  type State = ();
  const DESC: ShardDesc = TABLE_MAKE_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<TableCode>> {
    let bad_keys = || {
      Err(param_error(
        args,
        "Table.Make",
        "keys",
        "compose-error",
        "invalid-keys",
        "Keys must be a sequence of distinct strings".into(),
      ))
    };
    let keys: Vec<Arc<str>> = match args.literal("keys") {
      Some(Var::Seq(items)) => {
        let mut keys = Vec::with_capacity(items.len());
        for item in items.iter() {
          match item {
            Var::String(s) if !keys.contains(&**s) => keys.push(Arc::clone(s)),
            _ => return bad_keys(),
          }
        }
        keys
      }
      _ => return bad_keys(),
    };
    let values = compose_operands(args, "values", "Table.Make", ctx)?;
    if values.len() != keys.len() {
      return Err(param_error(
        args,
        "Table.Make",
        "values",
        "compose-error",
        "value-count",
        format!("{} keys but {} values", keys.len(), values.len()),
      ));
    }
    let mut entries: Vec<(Arc<str>, (Operand, Type))> = keys.into_iter().zip(values).collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let shape = Shape::new(entries.iter().map(|(k, _)| k.clone()));
    let output = Type::fixed_table_of(shape, entries.iter().map(|(_, (_, t))| *t));
    Ok(Composed {
      compiled: TableCode {
        shape,
        values: entries.into_iter().map(|(_, (o, _))| o).collect(),
      },
      output,
    })
  }

  fn instantiate(_: &TableCode, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(code: &TableCode, _: &mut (), ctx: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    if let Some(meter) = ctx.meter() {
      meter.allocate(code.values.len() * std::mem::size_of::<Var>())?;
      for value in &code.values {
        meter.nest(&value.get(ctx))?;
      }
    }
    // One allocation, filled in place.
    let mut slots = Arc::<[Var]>::new_uninit_slice(code.values.len());
    let uninit = Arc::get_mut(&mut slots).expect("just allocated");
    for (slot, value) in uninit.iter_mut().zip(&code.values) {
      slot.write(value.get(ctx));
    }
    // SAFETY: `slots` has one entry per value, each written above.
    let slots = unsafe { slots.assume_init() };
    Ok(Flow::Next(Var::from_table(Table::with_slots(
      code.shape, slots,
    ))))
  }
}

// --- String.Format ---

pub const STRING_FORMAT_DESC: ShardDesc = ShardDesc {
  name: "String.Format",
  version: 1,
  summary: crate::shard_doc!("Joins a sequence's elements into one string."),
  help: crate::shard_doc!(
    "Elements print as text: strings as they are, whole floats without `.0`, other floats exact (`12`, `2.5`, `[1 2]`, `{a: 1}`, `none`). f-strings lower to Seq.Make followed by String.Format."
  ),
  params: Params::Declared(&[]),
  input: InputDesc::Types(&[TypeName::Seq]),
  output: OutputDesc::Fixed(TypeName::String),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct StringFormat;

impl LeafShard for StringFormat {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = STRING_FORMAT_DESC;

  fn compose(_: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    let input = ctx.input();
    if !matches!(input.desc(), TypeDesc::Seq(_)) {
      return Err(Error::Diagnostic(Box::new(
        Diagnostic::new(
          Phase::Compose,
          "input-type-mismatch",
          "input-type-mismatch",
          format!("String.Format needs a sequence input, got {input}"),
        )
        .shard("String.Format")
        .types(
          Some(TypeRef::of(input)),
          vec![TypeRef::named(TypeName::Seq)],
        ),
      )));
    }
    Ok(Composed {
      compiled: (),
      output: Type::string(),
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(_: &(), _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    if let Some(meter) = ctx.meter() {
      // The text is at most the input's size as text.
      let bytes = meter.traverse(input)?;
      meter.allocate(bytes)?;
    }
    let Var::Seq(items) = input else {
      return Err(Error::Activation(
        "String.Format: input is not a sequence".into(),
      ));
    };
    let mut out = String::new();
    for item in items.iter() {
      out.push_str(&item.text());
    }
    Ok(Flow::Next(Var::from_string(Arc::from(out))))
  }
}
