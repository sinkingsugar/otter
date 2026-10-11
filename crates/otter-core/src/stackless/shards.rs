//! Composite adapters expose immutable child code to the iterative runner.
use super::Control;
use crate::args::Args;
use crate::compose::{CompiledWire, ComposeCtx};
use crate::error::Result;
use crate::flow::CompiledFlow;
use crate::function::CallCompiled;
use crate::instance::InstanceCtx;
use crate::shard::{ActivationCtx, Composed, Shard, Step};
use crate::shards::control;
use crate::shards::*;
use crate::var::Var;
use std::sync::Arc;
use std::time::{Duration, Instant};
type Ctx<'a, 'b> = &'a mut ComposeCtx<'b>;
macro_rules! leaf_instantiate {
  ($compiled:ty) => {
    fn instantiate(_: &$compiled, _: &mut InstanceCtx) -> Result<()> {
      Ok(())
    }
  };
}
pub struct Spawn;

impl Shard for Spawn {
  type Compiled = Arc<CompiledWire>;
  type State = ();
  const NAME: &'static str = SPAWN_DESC.name;
  const VERSION: u32 = SPAWN_DESC.version;

  fn compose(args: &Args, ctx: Ctx) -> Result<Composed<Arc<CompiledWire>>> {
    compose_spawn(args, ctx)
  }
  leaf_instantiate!(Arc<CompiledWire>);

  fn activate(
    wire: &Arc<CompiledWire>,
    _: &mut (),
    ctx: &mut ActivationCtx<'_>,
    input: &Var,
  ) -> Result<Step> {
    ctx.spawn(wire.clone(), input.clone());
    Ok(Step::Next(input.clone()))
  }
}

// --- shards that suspend, or run nested flows: they keep a resume point ---

/// Suspends for one tick, or until at least `seconds` have passed. The resume
/// point is when it started waiting.
pub struct Pause;

/// A pause in progress: since when, for a timed pause; a plain yield reads
/// no clock.
pub struct Waiting {
  since: Option<Instant>,
}

impl Shard for Pause {
  type Compiled = Duration;
  type State = Option<Waiting>;
  const NAME: &'static str = PAUSE_DESC.name;
  const VERSION: u32 = PAUSE_DESC.version;

  fn compose(args: &Args, ctx: Ctx) -> Result<Composed<Duration>> {
    compose_pause(args, ctx)
  }

  fn instantiate(_: &Duration, _: &mut InstanceCtx) -> Result<Option<Waiting>> {
    Ok(None)
  }

  fn activate(
    duration: &Duration,
    waiting: &mut Option<Waiting>,
    _: &mut ActivationCtx<'_>,
    input: &Var,
  ) -> Result<Step> {
    match waiting {
      None => {
        *waiting = Some(Waiting {
          since: (!duration.is_zero()).then(Instant::now),
        });
        Ok(Step::Suspend)
      }
      Some(Waiting { since: Some(start) }) if start.elapsed() < *duration => Ok(Step::Suspend),
      Some(_) => {
        *waiting = None;
        Ok(Step::Next(input.clone()))
      }
    }
  }
}

/// A call site. Composed by `ComposeCtx::compose_call` (never through this
/// shard's own compose); the engine allocates the invocation frame at
/// entry and owns its continuation.
pub struct Call;

impl Shard for Call {
  type Compiled = CallCompiled;
  type State = ();
  const NAME: &'static str = CALL_DESC.name;
  const VERSION: u32 = CALL_DESC.version;

  fn compose(_: &Args, _: Ctx) -> Result<Composed<CallCompiled>> {
    Err(crate::Error::Diagnostic(Box::new(
      crate::diagnostic::Diagnostic::new(
        crate::diagnostic::Phase::Compose,
        "compose-error",
        "not-a-shard",
        "Call is the internal call node; call a function by its name (`ShardDef::call`)",
      )
      .shard("Call"),
    )))
  }
  leaf_instantiate!(CallCompiled);
  fn control(c: &CallCompiled) -> Option<Control<'_>> {
    Some(Control::Call(c))
  }

  /// A stateless, non-recursive call to a straight-line body is a VM
  /// instruction; the engine's run enters the kept frame's locals.
  fn inline(c: &CallCompiled) -> Option<crate::inline::InlineOp> {
    match &c.target {
      crate::function::CallTarget::Direct(body) if !c.stateful() && body.vm_only => {
        Some(crate::inline::InlineOp(crate::inline::Op::VmCall))
      }
      _ => None,
    }
  }
  fn activate(_: &CallCompiled, _: &mut (), _: &mut ActivationCtx<'_>, _: &Var) -> Result<Step> {
    unreachable!("calls are dispatched by the runner")
  }
}

macro_rules! composite {
  ($name:ident, $compiled:ty, $desc:expr, $compose:expr, $variant:ident) => {
    composite!(
      $name,
      $compiled,
      $desc,
      $compose,
      $variant,
      inline = |_| None
    );
  };
  ($name:ident, $compiled:ty, $desc:expr, $compose:expr, $variant:ident, inline = $inline:expr) => {
    pub struct $name;
    impl Shard for $name {
      type Compiled = $compiled;
      type State = ();
      const NAME: &'static str = $desc.name;
      const VERSION: u32 = $desc.version;
      fn compose(args: &Args, ctx: Ctx) -> Result<Composed<Self::Compiled>> {
        $compose(args, ctx)
      }
      fn instantiate(_: &Self::Compiled, _: &mut InstanceCtx) -> Result<()> {
        Ok(())
      }
      fn control(c: &Self::Compiled) -> Option<Control<'_>> {
        Some(Control::$variant(c))
      }
      fn inline(c: &Self::Compiled) -> Option<crate::inline::InlineOp> {
        let f: fn(&Self::Compiled) -> Option<crate::inline::InlineOp> = $inline;
        f(c)
      }
      fn activate(
        _: &Self::Compiled,
        _: &mut (),
        _: &mut ActivationCtx<'_>,
        _: &Var,
      ) -> Result<Step> {
        unreachable!("composites are dispatched by the runner")
      }
    }
  };
}
// `When`, `While` and `Repeat` are lowered to flat code by compose
// (`ComposeCtx::flatten`); their descriptions here compose the children.
composite!(When, Predicated, WHEN_DESC, compose_when, When);
composite!(While, Predicated, WHILE_DESC, compose_while, While);
composite!(Sub, CompiledFlow, SUB_DESC, compose_sub, Sub);
composite!(Once, CompiledFlow, ONCE_DESC, compose_once, Once);
composite!(Repeat, RepeatCompiled, REPEAT_DESC, compose_repeat, Repeat);
composite!(
  If,
  control::IfCompiled,
  control::IF_DESC,
  control::compose_if,
  If
);
composite!(
  Match,
  control::MatchCompiled,
  control::MATCH_DESC,
  control::compose_match,
  Match
);
composite!(
  Run,
  control::RunCompiled,
  control::RUN_DESC,
  control::compose_run,
  Run
);
composite!(
  All,
  control::ConditionsCompiled,
  control::ALL_DESC,
  |args, ctx| control::compose_conditions(args, ctx, "All", false),
  Conditions
);
composite!(
  Any,
  control::ConditionsCompiled,
  control::ANY_DESC,
  |args, ctx| control::compose_conditions(args, ctx, "Any", true),
  Conditions
);
