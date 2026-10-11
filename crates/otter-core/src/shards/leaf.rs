//! Shards that cannot suspend.
//!
//! "Leaf" means the shard never suspends, not that it does little work. An
//! I/O wrapper qualifies only if it returns without blocking; one that waits
//! on I/O is an async shard ([`crate::shards::async_shard`]).
//!
//! [`Leaf`] adapts a [`LeafShard`] to the shard contract ([`Shard`]): its
//! [`Flow`] result becomes a [`Step`] at the boundary, after the output
//! check.

use crate::args::Args;
use std::marker::PhantomData;

use super::*;
use crate::describe::ShardDesc;
use crate::instance::{CleanupCtx, InstanceCtx, LeafCtx};
use crate::shard::{ActivationCtx, Flow, Shard, Step};

/// A shard that cannot suspend. Prototype API.
pub trait LeafShard: 'static {
  type Compiled: Send + Sync + 'static;
  type State: 'static;

  /// The shard's description: identity, documentation and parameters. The
  /// shared decoder enforces the declared parameters.
  const DESC: ShardDesc;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Self::Compiled>>;

  fn instantiate(compiled: &Self::Compiled, ctx: &mut InstanceCtx) -> Result<Self::State>;

  /// Must not block. Can stop, restart or return, but not suspend.
  fn activate(
    compiled: &Self::Compiled,
    state: &mut Self::State,
    ctx: &mut impl LeafCtx,
    input: &Var,
  ) -> Result<Flow>;

  fn cleanup(_compiled: &Self::Compiled, _state: &mut Self::State, _ctx: &mut CleanupCtx) {}
}

/// Adapts a [`LeafShard`] to the shard contract.
pub struct Leaf<L>(PhantomData<fn() -> L>);

/// The shard type of a leaf shard.
pub const fn leaf_type<L: LeafShard>() -> ShardType {
  ShardType::new(L::DESC).implemented_by::<Leaf<L>>()
}

/// A leaf or async shard's compose output with the type compose declared
/// for it, so produced values can be checked against it
/// ([`check_output`]).
pub struct Checked<C> {
  pub(crate) inner: C,
  pub(crate) output: Type,
}

/// Whether produced values are checked against their compose output type:
/// always in debug builds, in release with the `output-checks` feature.
pub(crate) const OUTPUT_CHECKS: bool = cfg!(any(debug_assertions, feature = "output-checks"));

/// Fails when a shard produced a value its own compose output type does not
/// admit (a bug in the shard: it drifted from its declared type). Checked on
/// the value, without interning its type.
pub(crate) fn check_output(name: &str, output: Type, value: &Var) -> Result<()> {
  if !OUTPUT_CHECKS || output.admits(value) {
    return Ok(());
  }
  let mut text = value.to_string();
  if text.len() > 120 {
    let mut cut = 120;
    while !text.is_char_boundary(cut) {
      cut -= 1;
    }
    text.truncate(cut);
    text.push_str("...");
  }
  Err(Error::Activation(format!(
    "{name} produced {text}, which its declared output type {output} does not admit (a bug in the shard)"
  )))
}

pub(crate) fn checked<C>(composed: Composed<C>) -> Composed<Checked<C>> {
  Composed {
    output: composed.output,
    compiled: Checked {
      inner: composed.compiled,
      output: composed.output,
    },
  }
}

impl<L: LeafShard> Shard for Leaf<L> {
  type Compiled = Checked<L::Compiled>;
  type State = L::State;
  const NAME: &'static str = L::DESC.name;
  const VERSION: u32 = L::DESC.version;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Checked<L::Compiled>>> {
    L::compose(args, ctx).map(checked)
  }

  fn instantiate(c: &Checked<L::Compiled>, ctx: &mut InstanceCtx) -> Result<L::State> {
    L::instantiate(&c.inner, ctx)
  }

  fn inline(c: &Checked<L::Compiled>) -> Option<crate::inline::InlineOp> {
    crate::inline::leaf::<L>(&c.inner, c.output)
  }

  fn activate(
    c: &Checked<L::Compiled>,
    s: &mut L::State,
    ctx: &mut ActivationCtx<'_>,
    input: &Var,
  ) -> Result<Step> {
    Ok(match L::activate(&c.inner, s, ctx, input)? {
      Flow::Next(v) => {
        check_output(L::DESC.name, c.output, &v)?;
        Step::Next(v)
      }
      Flow::Stop => Step::Stop,
      Flow::Restart => Step::Restart,
      Flow::Return(v) => Step::Return(v),
    })
  }

  fn cleanup(c: &Checked<L::Compiled>, s: &mut L::State, ctx: &mut CleanupCtx) {
    L::cleanup(&c.inner, s, ctx)
  }
}

macro_rules! no_state {
  ($compiled:ty) => {
    type State = ();
    fn instantiate(_: &$compiled, _: &mut InstanceCtx) -> Result<()> {
      Ok(())
    }
  };
}

pub struct Const;

impl LeafShard for Const {
  type Compiled = Var;
  no_state!(Var);
  const DESC: ShardDesc = CONST_DESC;

  fn compose(args: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<Var>> {
    compose_const(args)
  }

  fn activate(value: &Var, _: &mut (), _: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    Ok(Flow::Next(value.clone()))
  }
}

/// `Var`: declares a mutable local holding the input.
pub struct VarDecl;

impl LeafShard for VarDecl {
  type Compiled = Binding;
  no_state!(Binding);
  const DESC: ShardDesc = VAR_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    compose_var(args, ctx)
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    ctx.set(*b, input.clone());
    Ok(Flow::Next(input.clone()))
  }
}

/// `= name`: declares an immutable local holding the input.
pub struct Bind;

impl LeafShard for Bind {
  type Compiled = Binding;
  no_state!(Binding);
  const DESC: ShardDesc = BIND_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    compose_bind(args, ctx)
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    ctx.set(*b, input.clone());
    Ok(Flow::Next(input.clone()))
  }
}

/// `Keep`: persistent state, initialized the first time the instance
/// reaches it.
pub struct Keep;

impl LeafShard for Keep {
  type Compiled = ();
  no_state!(());
  const DESC: ShardDesc = KEEP_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    compose_keep(args, ctx)
  }

  /// The slot already holds its value (set when the frame was created, or
  /// carried over by a reload): nothing to do but pass the input on.
  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Ok(Flow::Next(input.clone()))
  }
}

/// Assigns the input to an existing mutable variable.
pub struct Update;

impl LeafShard for Update {
  type Compiled = Binding;
  no_state!(Binding);
  const DESC: ShardDesc = UPDATE_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    compose_update(args, ctx)
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    ctx.set(*b, input.clone());
    Ok(Flow::Next(input.clone()))
  }
}

pub struct Get;

impl LeafShard for Get {
  type Compiled = Binding;
  no_state!(Binding);
  const DESC: ShardDesc = GET_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    compose_get(args, ctx)
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    Ok(Flow::Next(ctx.get(*b)))
  }
}

/// Increments an Int variable and outputs the new value.
pub struct Inc;

impl LeafShard for Inc {
  type Compiled = Binding;
  no_state!(Binding);
  const DESC: ShardDesc = INC_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    compose_inc(args, ctx)
  }

  fn activate(b: &Binding, _: &mut (), ctx: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    Ok(Flow::Next(activate_inc(*b, ctx)?))
  }
}

pub struct Add;

impl LeafShard for Add {
  type Compiled = Operand;
  no_state!(Operand);
  const DESC: ShardDesc = ADD_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Operand>> {
    math::compose_binary(args, ctx, ADD_DESC.name, math::BinOp::Add)
  }

  fn activate(op: &Operand, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Ok(Flow::Next(math::arith(
      math::BinOp::Add,
      ADD_DESC.name,
      input,
      &op.get(ctx),
    )?))
  }
}

pub struct IsLess;

impl LeafShard for IsLess {
  type Compiled = Operand;
  no_state!(Operand);
  const DESC: ShardDesc = IS_LESS_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Operand>> {
    compose_compare(args, ctx, Self::DESC.name)
  }

  fn activate(op: &Operand, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Ok(Flow::Next(Var::Bool(compare(input, op.get(ctx))?.is_lt())))
  }
}

pub struct IsMoreEqual;

impl LeafShard for IsMoreEqual {
  type Compiled = Operand;
  no_state!(Operand);
  const DESC: ShardDesc = IS_MORE_EQUAL_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Operand>> {
    compose_compare(args, ctx, Self::DESC.name)
  }

  fn activate(op: &Operand, _: &mut (), ctx: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Ok(Flow::Next(Var::Bool(compare(input, op.get(ctx))?.is_ge())))
  }
}

/// Ends the enclosing function or wire with the input.
pub struct Return;

impl LeafShard for Return {
  type Compiled = ();
  no_state!(());
  const DESC: ShardDesc = RETURN_DESC;

  fn compose(_: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    compose_return(ctx)
  }

  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    Ok(Flow::Return(input.clone()))
  }
}

/// Test instrumentation, see [`ProbeMode`].
pub struct Probe;

impl LeafShard for Probe {
  type Compiled = ProbeCompiled;
  type State = InstanceId;
  const DESC: ShardDesc = PROBE_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<ProbeCompiled>> {
    compose_probe(args, ctx)
  }

  fn instantiate(c: &ProbeCompiled, ctx: &mut InstanceCtx) -> Result<InstanceId> {
    probe_instantiate(c, ctx.instance)
  }

  fn activate(
    c: &ProbeCompiled,
    instance: &mut InstanceId,
    _: &mut impl LeafCtx,
    input: &Var,
  ) -> Result<Flow> {
    probe_activate(c, *instance);
    Ok(Flow::Next(input.clone()))
  }

  fn cleanup(c: &ProbeCompiled, instance: &mut InstanceId, _: &mut CleanupCtx) {
    probe_cleanup(c, *instance);
  }
}
