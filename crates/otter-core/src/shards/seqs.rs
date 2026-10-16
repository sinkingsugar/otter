//! Sequence shards (docs/host-requirements.md §3): the ones that change a
//! variable (`AppendTo`, `PrependTo`, `Extend`, `Clear`, `PopFront`,
//! `DropFront`, `Remove`) and the ones over a value (`Reverse`, `IndexOf`).
//!
//! A shard that changes a variable follows `Push`'s rules (`data.rs`): the
//! variable is declared, mutable and of a type that takes the change, all
//! checked at compose; at activation the value is taken out of its slot so
//! an unshared sequence changes in place instead of being copied. They are
//! leaf shards writing through [`crate::instance::Frames::set`], so the VM
//! needs no instruction of its own for them: a leaf call runs them. `Remove`
//! runs a predicate flow per element, so it is a composite
//! ([`crate::ControlShard`]), as `Maybe` is.

use std::sync::Arc;

use super::leaf::{LeafShard, leaf_type};
use super::values::{Pure, PureOp, comparable, fail, one_of, pure_desc, values_equal};
use super::*;
use crate::compose::VarInfo;
use crate::compose_time::Meter;
use crate::instance::{InstanceCtx, LeafCtx};
use crate::shard::Flow;
use crate::var::Table;

pub static APPEND_TO: ShardType = leaf_type::<XPend<false>>();
pub static PREPEND_TO: ShardType = leaf_type::<XPend<true>>();
pub static EXTEND: ShardType = leaf_type::<Extend>();
pub static CLEAR: ShardType = leaf_type::<Clear>();
pub static POP_FRONT: ShardType = leaf_type::<Front<true>>();
pub static DROP_FRONT: ShardType = leaf_type::<Front<false>>();
pub static REVERSE: ShardType = leaf_type::<Pure<ReverseOp>>();
pub static INDEX_OF: ShardType = leaf_type::<IndexOf>();
pub static REMOVE: ShardType = ShardType::new(REMOVE_DESC).controlled_by::<Remove>();

const SEQ: &[TypeName] = &[TypeName::Seq];
const VAR_SIZE: usize = std::mem::size_of::<Var>();

const fn desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  params: &'static [ParamDecl],
  input: InputDesc,
  output: OutputDesc,
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(params),
    input,
    output,
    targets: Targets::All,
    aliases: &[],
    effects: crate::signature::Effects::NONE,
    lifetime: crate::signature::Lifetime::Stateless,
  }
}

const fn variable_param(help: &'static str) -> ParamDecl {
  decl(
    "variable",
    help,
    Forms::VARIABLE,
    NONE_TYPES,
    Requirement::Required,
  )
}

/// A `variable-type-mismatch` error on the `variable` parameter.
fn variable_mismatch(args: &Args, shard: &str, message: String) -> Error {
  param_error(
    args,
    shard,
    "variable",
    "compose-error",
    "variable-type-mismatch",
    message,
  )
}

/// The variable a shard changes: declared and mutable (`assignable`), and
/// definitely assigned when the shard also reads it (`reads`).
fn target(args: &Args, ctx: &mut ComposeCtx<'_>, shard: &str, reads: bool) -> Result<VarInfo> {
  let info = super::assignable(args, ctx, shard)?;
  if reads {
    ctx
      .read_var(variable(args, "variable"), shard)
      .map_err(|e| e.with_param("variable", args.param_index("variable")))?;
  }
  Ok(info)
}

/// Records that the shard assigns the variable.
fn assigns(args: &Args, ctx: &mut ComposeCtx<'_>, shard: &str, info: &VarInfo) -> Result<()> {
  ctx.mark_initialized(info.binding).map_err(|e| {
    e.in_shard(shard)
      .with_param("variable", args.param_index("variable"))
  })
}

/// The sequence in a variable's slot, taken out so that it is unshared when
/// nothing else holds it (the slot holds none meanwhile). A value of
/// another kind is put back and is an error.
fn take_seq(ctx: &mut impl LeafCtx, b: Binding, shard: &str) -> Result<Arc<Vec<Var>>> {
  let current = ctx.get(b);
  ctx.set(b, Var::None);
  match current.into_seq() {
    Ok(items) => Ok(items),
    Err(other) => {
      ctx.set(b, other);
      Err(fail(shard, "the variable is not a sequence"))
    }
  }
}

/// In a compose-time evaluation, charges a change that grows a sequence of
/// `len` elements to `grown` and moves or copies `moved` of them: a shared
/// sequence is copied whole first (`seq_mut`).
fn meter_change(meter: &Meter, current: &Var, grown: usize, moved: usize) -> Result<()> {
  // The slot and `current` each hold a reference.
  let shared = matches!(current, Var::Seq(items) if Arc::strong_count(items) > 2);
  let bytes = grown.saturating_mul(VAR_SIZE);
  if shared {
    meter.allocate(bytes)
  } else {
    meter.admit(bytes)?;
    meter.charge(moved.saturating_mul(VAR_SIZE) as u64)
  }
}

// --- AppendTo, PrependTo ---

static XPEND_PARAMS: &[ParamDecl] = &[variable_param(crate::shard_doc!(
  "The mutable variable to add the input to: a sequence whose element type accepts the input, or a String or byte string taking one of its kind."
))];

/// What `AppendTo` and `PrependTo` add to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
  Seq,
  String,
  Bytes,
}

pub struct XPendCompiled {
  binding: Binding,
  collection: Collection,
}

/// `AppendTo` (`FRONT` false) and `PrependTo`.
pub struct XPend<const FRONT: bool>;
impl<const FRONT: bool> XPend<FRONT> {
  const NAME: &'static str = if FRONT { "PrependTo" } else { "AppendTo" };
}

impl<const FRONT: bool> LeafShard for XPend<FRONT> {
  type Compiled = XPendCompiled;
  type State = ();
  const DESC: ShardDesc = desc(
    Self::NAME,
    if FRONT {
      crate::shard_doc!(
        "Puts the input at the start of a sequence, string or byte string variable."
      )
    } else {
      crate::shard_doc!("Puts the input at the end of a sequence, string or byte string variable.")
    },
    crate::shard_doc!(
      "A sequence gets the input as a new element (`AppendTo` on a sequence is `Push`); a String gets the input's text and a byte string the input's bytes. The variable must be declared and mutable, with a type that takes the input. Passes its input through."
    ),
    XPEND_PARAMS,
    InputDesc::Any,
    OutputDesc::Passthrough,
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<XPendCompiled>> {
    let name = variable(args, "variable");
    let input = ctx.input();
    let info = target(args, ctx, Self::NAME, true)?;
    let collection = match info.ty.desc() {
      TypeDesc::Seq(e) if e.accepts(input) => Collection::Seq,
      TypeDesc::String if Type::string().accepts(input) => Collection::String,
      TypeDesc::Bytes if Type::bytes().accepts(input) => Collection::Bytes,
      TypeDesc::Seq(_) | TypeDesc::String | TypeDesc::Bytes => {
        return Err(variable_mismatch(
          args,
          Self::NAME,
          format!("{name} is {}, cannot add {input}", info.ty),
        ));
      }
      _ => {
        return Err(variable_mismatch(
          args,
          Self::NAME,
          format!(
            "{name} is {}, not a sequence, string or byte string",
            info.ty
          ),
        ));
      }
    };
    assigns(args, ctx, Self::NAME, &info)?;
    Ok(Composed {
      compiled: XPendCompiled {
        binding: info.binding,
        collection,
      },
      output: input,
    })
  }

  fn instantiate(_: &XPendCompiled, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(c: &XPendCompiled, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let b = c.binding;
    match c.collection {
      // Push's own activation: one rule for growing a sequence.
      Collection::Seq if !FRONT => {
        return <data::Push as LeafShard>::activate(&b, &mut (), ctx, input);
      }
      Collection::Seq => {
        if let Some(meter) = ctx.meter() {
          meter.nest(input)?;
          let current = ctx.get(b);
          let len = current.as_seq().map_or(0, <[Var]>::len);
          meter_change(meter, &current, len + 1, len + 1)?;
        }
        let mut items = take_seq(ctx, b, Self::NAME)?;
        crate::var::seq_mut(&mut items).insert(0, input.clone());
        ctx.set(b, Var::from_seq(items));
      }
      Collection::String => {
        let current = ctx.get(b);
        let (Some(have), Some(add)) = (current.as_str(), input.as_str()) else {
          return Err(fail(
            Self::NAME,
            "the variable or the input is not a string",
          ));
        };
        if let Some(meter) = ctx.meter() {
          meter.allocate(have.len() + add.len())?;
        }
        let (first, second) = if FRONT { (add, have) } else { (have, add) };
        let mut text = String::with_capacity(have.len() + add.len());
        text.push_str(first);
        text.push_str(second);
        ctx.set(b, Var::from_string(Arc::from(text)));
      }
      Collection::Bytes => {
        let current = ctx.get(b);
        let (Some(have), Some(add)) = (current.as_bytes(), input.as_bytes()) else {
          return Err(fail(
            Self::NAME,
            "the variable or the input is not a byte string",
          ));
        };
        if let Some(meter) = ctx.meter() {
          meter.allocate(have.len() + add.len())?;
        }
        let (first, second) = if FRONT { (add, have) } else { (have, add) };
        let mut bytes = Vec::with_capacity(have.len() + add.len());
        bytes.extend_from_slice(first);
        bytes.extend_from_slice(second);
        ctx.set(b, Var::from_bytes(Arc::from(bytes)));
      }
    }
    Ok(Flow::Next(input.clone()))
  }
}

// --- Extend ---

static EXTEND_PARAMS: &[ParamDecl] = &[variable_param(crate::shard_doc!(
  "The mutable sequence variable to add the input's elements to; its element type must accept theirs."
))];

pub struct Extend;
impl LeafShard for Extend {
  type Compiled = Binding;
  type State = ();
  const DESC: ShardDesc = desc(
    "Extend",
    crate::shard_doc!("Appends the elements of the input sequence to a sequence variable."),
    crate::shard_doc!(
      "`[3 4] | Extend(xs)` makes `[1 2]` `[1 2 3 4]`. The variable must be a declared mutable sequence whose element type accepts the input's. Passes its input through."
    ),
    EXTEND_PARAMS,
    InputDesc::Types(SEQ),
    OutputDesc::Passthrough,
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    let name = variable(args, "variable");
    let input = ctx.input();
    let TypeDesc::Seq(added) = input.desc() else {
      return Err(super::values::mismatch("Extend", input, SEQ));
    };
    let info = target(args, ctx, "Extend", true)?;
    match info.ty.desc() {
      TypeDesc::Seq(e) if e.accepts(*added) => {}
      TypeDesc::Seq(_) => {
        return Err(variable_mismatch(
          args,
          "Extend",
          format!("{name} is {}, cannot add the elements of {input}", info.ty),
        ));
      }
      _ => {
        return Err(variable_mismatch(
          args,
          "Extend",
          format!("{name} is {}, not a sequence", info.ty),
        ));
      }
    }
    assigns(args, ctx, "Extend", &info)?;
    Ok(Composed {
      compiled: info.binding,
      output: input,
    })
  }

  fn instantiate(_: &Binding, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let added = input.as_seq().unwrap_or(&[]);
    if let Some(meter) = ctx.meter() {
      meter.nest(input)?;
      let current = ctx.get(*b);
      let len = current.as_seq().map_or(0, <[Var]>::len);
      meter_change(meter, &current, len + added.len(), added.len())?;
    }
    let mut items = take_seq(ctx, *b, "Extend")?;
    crate::var::seq_mut(&mut items).extend_from_slice(added);
    ctx.set(*b, Var::from_seq(items));
    Ok(Flow::Next(input.clone()))
  }
}

// --- Clear ---

static CLEAR_PARAMS: &[ParamDecl] = &[variable_param(crate::shard_doc!(
  "The mutable variable to empty: a sequence, or a table whose type lists no keys (`{...}`, `{...: Int}`)."
))];

pub struct Clear;
impl LeafShard for Clear {
  type Compiled = Binding;
  type State = ();
  const DESC: ShardDesc = desc(
    "Clear",
    crate::shard_doc!("Empties a sequence or table variable."),
    crate::shard_doc!(
      "A sequence no other value shares keeps its storage for what is added next. A table whose type lists keys cannot be emptied (it would lose them): a compose error. Passes its input through."
    ),
    CLEAR_PARAMS,
    InputDesc::Any,
    OutputDesc::Passthrough,
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    let name = variable(args, "variable");
    let info = target(args, ctx, "Clear", false)?;
    match info.ty.desc() {
      TypeDesc::Seq(_) => {}
      TypeDesc::Table(t) if t.keys.is_empty() && t.rest.is_some() => {}
      TypeDesc::Table(_) => {
        return Err(variable_mismatch(
          args,
          "Clear",
          format!(
            "{name} is {}, whose keys an empty table would not have; declare it with an open type (`ToAnyTable`)",
            info.ty
          ),
        ));
      }
      _ => {
        return Err(variable_mismatch(
          args,
          "Clear",
          format!("{name} is {}, not a sequence or table", info.ty),
        ));
      }
    }
    assigns(args, ctx, "Clear", &info)?;
    Ok(Composed {
      compiled: info.binding,
      output: ctx.input(),
    })
  }

  fn instantiate(_: &Binding, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let current = ctx.get(*b);
    ctx.set(*b, Var::None);
    let empty = match current {
      Var::Table(_) => Var::from_table(Table::new()),
      other => match other.into_seq() {
        Ok(mut items) => match Arc::get_mut(&mut items) {
          Some(vec) => {
            vec.clear();
            Var::from_seq(items)
          }
          None => Var::seq(Vec::new()),
        },
        Err(other) => {
          ctx.set(*b, other);
          return Err(fail("Clear", "the variable is not a sequence or table"));
        }
      },
    };
    ctx.set(*b, empty);
    Ok(Flow::Next(input.clone()))
  }
}

// --- PopFront, DropFront ---

static FRONT_PARAMS: &[ParamDecl] = &[variable_param(crate::shard_doc!(
  "The mutable sequence variable to remove the first element of."
))];

/// `PopFront` (`POP` true: outputs the element, and an empty sequence is
/// an error) and `DropFront` (passes its input through; nothing to drop is
/// no error).
pub struct Front<const POP: bool>;
impl<const POP: bool> Front<POP> {
  const NAME: &'static str = if POP { "PopFront" } else { "DropFront" };
}

impl<const POP: bool> LeafShard for Front<POP> {
  type Compiled = Binding;
  type State = ();
  const DESC: ShardDesc = desc(
    Self::NAME,
    if POP {
      crate::shard_doc!("Removes the first element of a sequence variable and outputs it.")
    } else {
      crate::shard_doc!("Removes the first element of a sequence variable, if it has one.")
    },
    if POP {
      crate::shard_doc!(
        "Ignores its input. The output has the sequence's element type. An empty sequence is an activation error."
      )
    } else {
      crate::shard_doc!("An empty sequence stays empty. Passes its input through.")
    },
    FRONT_PARAMS,
    if POP {
      InputDesc::Ignored
    } else {
      InputDesc::Any
    },
    if POP {
      OutputDesc::Dynamic(crate::shard_doc!("the sequence's element type"))
    } else {
      OutputDesc::Passthrough
    },
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    let name = variable(args, "variable");
    let info = target(args, ctx, Self::NAME, true)?;
    let TypeDesc::Seq(element) = info.ty.desc() else {
      return Err(variable_mismatch(
        args,
        Self::NAME,
        format!("{name} is {}, not a sequence", info.ty),
      ));
    };
    assigns(args, ctx, Self::NAME, &info)?;
    Ok(Composed {
      compiled: info.binding,
      output: if POP { *element } else { ctx.input() },
    })
  }

  fn instantiate(_: &Binding, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    if let Some(meter) = ctx.meter() {
      let current = ctx.get(*b);
      let len = current.as_seq().map_or(0, <[Var]>::len);
      meter_change(meter, &current, len, len)?;
    }
    let mut items = take_seq(ctx, *b, Self::NAME)?;
    if items.is_empty() {
      ctx.set(*b, Var::from_seq(items));
      return if POP {
        Err(fail(Self::NAME, "the sequence is empty"))
      } else {
        Ok(Flow::Next(input.clone()))
      };
    }
    let first = crate::var::seq_mut(&mut items).remove(0);
    ctx.set(*b, Var::from_seq(items));
    Ok(Flow::Next(if POP { first } else { input.clone() }))
  }
}

// --- Reverse ---

const REVERSED: &[TypeName] = &[TypeName::Seq, TypeName::String, TypeName::Bytes];

pub struct ReverseOp;
impl PureOp for ReverseOp {
  const DESC: ShardDesc = pure_desc(
    "Reverse",
    crate::shard_doc!("Reverses a sequence, string or byte string."),
    crate::shard_doc!(
      "A string is reversed by characters (Unicode scalar values), so it stays valid text; combining marks then precede the letter they followed."
    ),
    InputDesc::Types(REVERSED),
    OutputDesc::SameAsInput,
  );
  fn output(input: Type) -> std::result::Result<Type, &'static [TypeName]> {
    one_of(input, REVERSED, input)
  }
  fn meter(input: &Var, meter: &Meter) -> Result<()> {
    meter.allocate(match input {
      Var::Seq(items) => items.len().saturating_mul(VAR_SIZE),
      Var::String(s) => s.len(),
      Var::Bytes(b) => b.len(),
      _ => 0,
    })
  }
  fn apply(input: &Var) -> Result<Var> {
    Ok(match input {
      Var::Seq(items) => Var::seq(items.iter().rev().cloned().collect()),
      Var::String(s) => Var::from_string(Arc::from(s.chars().rev().collect::<String>())),
      Var::Bytes(b) => Var::from_bytes(b.iter().rev().copied().collect()),
      _ => return Err(fail("Reverse", "input type mismatch")),
    })
  }
}

// --- IndexOf ---

static INDEX_OF_PARAMS: &[ParamDecl] = &[
  decl(
    "item",
    crate::shard_doc!(
      "What to look for: an element, or a sequence of elements to find in a row. A literal, or a variable read at activation."
    ),
    OPERAND,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "all",
    crate::shard_doc!("Output every index where it is found, in order, instead of the first."),
    Forms::LITERAL,
    &[TypeName::Bool],
    Requirement::Default(DefaultValue::Bool(false)),
  ),
];

pub struct IndexOfCompiled {
  item: Operand,
  /// `item` is a run of elements to find in a row, not one element.
  run: bool,
  all: bool,
}

pub struct IndexOf;
impl LeafShard for IndexOf {
  type Compiled = IndexOfCompiled;
  type State = ();
  const DESC: ShardDesc = desc(
    "IndexOf",
    crate::shard_doc!("Outputs where an element, or a run of elements, is in a sequence."),
    crate::shard_doc!(
      "Elements compare as in `Is`. The output is the first index, or -1 when it is not there; with `all`, the sequence of every index (empty when none). When the item's type can be an element's, it is looked for as one element; otherwise a sequence item is looked for as a run of elements in a row (`[1 1]` in `[0 1 1]` is at 1). An empty run is an activation error."
    ),
    INDEX_OF_PARAMS,
    InputDesc::Types(SEQ),
    OutputDesc::Dynamic(crate::shard_doc!(
      "an Int, or a sequence of Ints with `all`"
    )),
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<IndexOfCompiled>> {
    let input = ctx.input();
    let element = match input.desc() {
      TypeDesc::Seq(e) => *e,
      _ => return Err(super::values::mismatch("IndexOf", input, SEQ)),
    };
    let (item, ty) = Operand::compose_arg(args, "item", "IndexOf", ctx)?;
    let run = if comparable(element, ty) {
      false
    } else if matches!(ty.desc(), TypeDesc::Seq(i) if comparable(element, *i)) {
      true
    } else {
      return Err(param_error(
        args,
        "IndexOf",
        "item",
        "input-type-mismatch",
        "input-type-mismatch",
        format!("{ty} can never be an element of {input}, nor a run of them"),
      ));
    };
    if run && matches!(&item, Operand::Const(Var::Seq(items)) if items.is_empty()) {
      return Err(param_error(
        args,
        "IndexOf",
        "item",
        "compose-error",
        "invalid-argument-value",
        "the run of elements to find is empty".into(),
      ));
    }
    let all = args.bool("all").unwrap_or(false);
    Ok(Composed {
      compiled: IndexOfCompiled { item, run, all },
      output: if all {
        Type::seq(Type::int())
      } else {
        Type::int()
      },
    })
  }

  fn instantiate(_: &IndexOfCompiled, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(
    c: &IndexOfCompiled,
    _: &mut (),
    ctx: &mut impl LeafCtx,
    input: &Var,
  ) -> Result<Flow> {
    let item = c.item.get(ctx);
    if let Some(meter) = ctx.meter() {
      meter.traverse(input)?;
      meter.traverse(&item)?;
    }
    let items = input.as_seq().unwrap_or(&[]);
    let run: &[Var] = if c.run {
      let run = item.as_seq().unwrap_or(&[]);
      if run.is_empty() {
        return Err(fail("IndexOf", "the run of elements to find is empty"));
      }
      run
    } else {
      std::slice::from_ref(&item)
    };
    let found = |i: &usize| items[*i..].iter().zip(run).all(|(a, b)| values_equal(a, b));
    let candidates = 0..(items.len() + 1).saturating_sub(run.len());
    Ok(Flow::Next(if c.all {
      if let Some(meter) = ctx.meter() {
        meter.allocate(items.len().saturating_mul(VAR_SIZE))?;
      }
      Var::seq(
        candidates
          .filter(found)
          .map(|i| Var::Int(i as i64))
          .collect(),
      )
    } else {
      Var::Int(candidates.into_iter().find(found).map_or(-1, |i| i as i64))
    }))
  }
}

// --- Remove ---

static REMOVE_PARAMS: &[ParamDecl] = &[
  variable_param(crate::shard_doc!(
    "The mutable sequence variable to remove elements from."
  )),
  decl(
    "predicate",
    crate::shard_doc!(
      "A flow that receives each element and outputs a Bool: true removes the element."
    ),
    Forms::FLOW,
    NONE_TYPES,
    Requirement::Required,
  ),
];

pub const REMOVE_DESC: ShardDesc = ShardDesc {
  name: "Remove",
  version: 1,
  summary: crate::shard_doc!(
    "Removes the elements of a sequence variable that a predicate flow selects."
  ),
  help: crate::shard_doc!(
    "Runs `predicate` on each element in order, then assigns the variable the elements it did not select, in their order, and outputs them. Ignores its input. The predicate sees the sequence as it was when Remove started; what it assigns to the variable itself is replaced at the end. An error in the predicate fails Remove and leaves the variable unchanged."
  ),
  params: Params::Declared(REMOVE_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Dynamic(crate::shard_doc!("the variable's sequence type")),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct RemoveCompiled {
  binding: Binding,
  /// `[predicate]`.
  flows: Vec<CompiledFlow>,
}

/// One activation's progress: the sequence being filtered, the next
/// element's index, and the elements kept so far.
#[derive(Default)]
pub struct RemoveState {
  items: Option<Arc<Vec<Var>>>,
  next: usize,
  kept: Vec<Var>,
}

pub struct Remove;

impl crate::ControlShard for Remove {
  type Compiled = RemoveCompiled;
  type State = RemoveState;
  const NAME: &'static str = REMOVE_DESC.name;
  const VERSION: u32 = REMOVE_DESC.version;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<RemoveCompiled>> {
    let name = variable(args, "variable");
    let info = target(args, ctx, "Remove", true)?;
    let TypeDesc::Seq(element) = info.ty.desc() else {
      return Err(variable_mismatch(
        args,
        "Remove",
        format!("{name} is {}, not a sequence", info.ty),
      ));
    };
    // Runs once per element, so possibly never.
    let predicate = control::compose_predicate(
      ctx,
      args.flow("predicate").expect("decoded predicate"),
      *element,
      true,
      "Remove",
      "predicate",
      args.param_index("predicate"),
    )?;
    assigns(args, ctx, "Remove", &info)?;
    Ok(Composed {
      compiled: RemoveCompiled {
        binding: info.binding,
        flows: vec![predicate],
      },
      output: info.ty,
    })
  }

  fn flows(compiled: &RemoveCompiled) -> &[CompiledFlow] {
    &compiled.flows
  }

  fn resume(
    c: &RemoveCompiled,
    state: &mut RemoveState,
    ctx: &mut crate::ActivationCtx<'_>,
    _: &Var,
    completion: Option<Result<Var>>,
  ) -> Result<crate::ControlStep> {
    use crate::ControlStep::{Complete, Enter};
    match completion {
      None => {
        let Ok(items) = ctx.get(c.binding).into_seq() else {
          return Err(fail("Remove", "the variable is not a sequence"));
        };
        *state = RemoveState {
          items: Some(items),
          next: 0,
          kept: Vec::new(),
        };
      }
      Some(result) => {
        let selected = matches!(result?, Var::Bool(true));
        if !selected {
          let items = state.items.as_ref().expect("started");
          state.kept.push(items[state.next - 1].clone());
        }
      }
    }
    let items = state.items.as_ref().expect("started");
    if let Some(item) = items.get(state.next) {
      state.next += 1;
      return Ok(Enter(0, item.clone()));
    }
    let out = Var::seq(std::mem::take(&mut state.kept));
    ctx.set(c.binding, out.clone());
    *state = RemoveState::default();
    Ok(Complete(out))
  }
}
