//! Shards that wait on an async operation. Prototype API.
//!
//! An [`AsyncShard`] starts one operation (a future) per activation, from
//! owned inputs. [`Async`] adapts it to the shard contract: the future is
//! created once, kept in the shard's state as the resume point, and polled
//! again on every resume until it completes. While it is pending, the
//! instance waits on its waker (see [`crate::WakeMode`]).
//!
//! Cancellation drops the pending future, before the shard's state (and any
//! resource an earlier shard holds) is released. Dropping a future cancels
//! its local execution only; whether the external operation is aborted,
//! detached or already committed is up to the future's `Drop`, and each
//! async shard must document which.

use crate::args::Args;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::leaf::{Checked, check_output, checked};
use super::*;
use crate::describe::ShardDesc;
use crate::instance::{CleanupCtx, InstanceCtx, LeafCtx};
use crate::shard::{ActivationCtx, Shard, Step};

/// A shard that waits on an async operation. Prototype API.
pub trait AsyncShard: 'static {
  type Compiled: Send + Sync + 'static;
  /// One operation. Need not be `Send`: the prototype is single-threaded.
  type Op: Future<Output = Result<Var>> + 'static;

  /// The shard's description: identity, documentation and parameters. The
  /// shared decoder enforces the declared parameters.
  const DESC: ShardDesc;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Self::Compiled>>;

  /// Starts one operation. Called once per activation that starts it; the
  /// returned future must own its inputs and resource handles (no borrows of
  /// frames or of `ctx`).
  fn start(compiled: &Self::Compiled, ctx: &mut impl LeafCtx, input: &Var) -> Result<Self::Op>;
}

/// State of an [`Async`] shard: the pending operation, if one is running.
pub struct AsyncState<Op> {
  op: Option<Pin<Box<Op>>>,
}

/// Adapts an [`AsyncShard`] to the shard contract.
pub struct Async<A>(PhantomData<fn() -> A>);

/// The shard type of an async shard.
pub const fn async_type<A: AsyncShard>() -> ShardType {
  ShardType::new(A::DESC).implemented_by::<Async<A>>()
}

fn ensure_started<A: AsyncShard>(
  c: &A::Compiled,
  state: &mut AsyncState<A::Op>,
  ctx: &mut impl LeafCtx,
  input: &Var,
) -> Result<()> {
  if state.op.is_none() {
    state.op = Some(Box::pin(A::start(c, ctx, input)?));
  }
  Ok(())
}

impl<A: AsyncShard> Shard for Async<A> {
  type Compiled = Checked<A::Compiled>;
  type State = AsyncState<A::Op>;
  const NAME: &'static str = A::DESC.name;
  const VERSION: u32 = A::DESC.version;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Checked<A::Compiled>>> {
    A::compose(args, ctx).map(checked)
  }

  fn instantiate(_: &Checked<A::Compiled>, _: &mut InstanceCtx) -> Result<AsyncState<A::Op>> {
    Ok(AsyncState { op: None })
  }

  /// The pending future is the resume point.
  fn activate(
    c: &Checked<A::Compiled>,
    state: &mut AsyncState<A::Op>,
    ctx: &mut ActivationCtx<'_>,
    input: &Var,
  ) -> Result<Step> {
    ensure_started::<A>(&c.inner, state, ctx, input)?;
    let op = state.op.as_mut().expect("operation started");
    match op.as_mut().poll(&mut Context::from_waker(ctx.waker())) {
      Poll::Ready(result) => {
        state.op = None;
        let value = result?;
        check_output(A::DESC.name, c.output, &value)?;
        Ok(Step::Next(value))
      }
      Poll::Pending => {
        ctx.set_waiting();
        Ok(Step::Suspend)
      }
    }
  }

  fn cleanup(_: &Checked<A::Compiled>, state: &mut AsyncState<A::Op>, _: &mut CleanupCtx) {
    state.op = None;
  }
}
