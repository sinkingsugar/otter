//! Composite adapters expose immutable child code to the iterative runner.
use super::Control;
use crate::args::Args;
use crate::compose::ComposeCtx;
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
  type Compiled = SpawnCompiled;
  type State = ();
  const NAME: &'static str = SPAWN_DESC.name;
  const VERSION: u32 = SPAWN_DESC.version;

  fn compose(args: &Args, ctx: Ctx) -> Result<Composed<SpawnCompiled>> {
    compose_spawn(args, ctx)
  }
  leaf_instantiate!(SpawnCompiled);

  fn activate(
    spawn: &SpawnCompiled,
    _: &mut (),
    ctx: &mut ActivationCtx<'_>,
    input: &Var,
  ) -> Result<Step> {
    let completion = ctx.spawn(spawn.wire.clone(), input.clone(), spawn.detach);
    Ok(Step::Next(crate::wire_instance::WireInstance::var(
      completion,
    )))
  }
}

/// Waits for a spawned instance to end. The resume point is its
/// registration with the instance's completion, removed when the wait ends
/// or its state is dropped (the waiting instance was cancelled).
pub struct Wait;

/// A wait in progress.
pub struct Registration {
  completion: Arc<crate::wire_instance::Completion>,
  key: u64,
  deadline: Option<Instant>,
}

impl Drop for Registration {
  fn drop(&mut self) {
    self.completion.unwait(self.key);
  }
}

impl Shard for Wait {
  type Compiled = instances::WaitCompiled;
  type State = Option<Registration>;
  const NAME: &'static str = instances::WAIT_DESC.name;
  const VERSION: u32 = instances::WAIT_DESC.version;

  fn compose(args: &Args, ctx: Ctx) -> Result<Composed<instances::WaitCompiled>> {
    instances::compose_wait(args, ctx)
  }

  fn instantiate(_: &instances::WaitCompiled, _: &mut InstanceCtx) -> Result<Option<Registration>> {
    Ok(None)
  }

  fn activate(
    wait: &instances::WaitCompiled,
    waiting: &mut Option<Registration>,
    ctx: &mut ActivationCtx<'_>,
    _: &Var,
  ) -> Result<Step> {
    let (outcome, completion) = match waiting {
      None => {
        let value = ctx.get(wait.handle);
        let completion = crate::wire_instance::completion_of(&value)?.clone();
        // A deadline past what the clock represents is no deadline.
        let deadline = wait.timeout.and_then(|t| Instant::now().checked_add(t));
        if Arc::ptr_eq(&completion, ctx.completion) {
          return Err(crate::Error::Activation(format!(
            "Wait: an instance of {} cannot wait for itself",
            completion.wire()
          )));
        }
        match completion.wait(ctx.waker()) {
          Err(outcome) => (outcome, completion),
          Ok(key) => {
            *waiting = Some(Registration {
              completion,
              key,
              deadline,
            });
            wait_until(ctx, deadline);
            return Ok(Step::Suspend);
          }
        }
      }
      Some(registration) => match registration.completion.outcome() {
        Some(outcome) => {
          let completion = registration.completion.clone();
          *waiting = None;
          (outcome, completion)
        }
        None if registration.deadline.is_some_and(|d| Instant::now() >= d) => {
          let wire = registration.completion.wire().to_string();
          *waiting = None;
          return Err(crate::Error::Diagnostic(Box::new(
            crate::diagnostic::Diagnostic::new(
              crate::diagnostic::Phase::Activate,
              "activation-error",
              "wait-timeout",
              format!(
                "Wait: the instance of {wire} did not end within {}s",
                wait.timeout.expect("a deadline").as_secs_f64()
              ),
            )
            .shard("Wait"),
          )));
        }
        None => {
          wait_until(ctx, registration.deadline);
          return Ok(Step::Suspend);
        }
      },
    };
    instances::ended(outcome, completion.wire(), "Wait").map(Step::Next)
  }
}

/// Tells the scheduler what a suspended `Wait` waits for: the end, whose
/// waker it registered, and its deadline if it has one (then it is still
/// activated every tick, and a host blocked in `Mesh::wait` sleeps until
/// the deadline or the waker, whichever comes first).
fn wait_until(ctx: &mut ActivationCtx<'_>, deadline: Option<Instant>) {
  match deadline {
    Some(at) => ctx.resume_at(at),
    None => ctx.set_waiting(),
  }
}

// --- shards that suspend, or run nested flows: they keep a resume point ---

/// `Pause()`: suspends for one tick. The resume point is whether it has.
/// A plain yield, so its activation never touches the context: `Pause` with
/// a time composes to [`TimedPause`] instead (`compose_pause_node`).
pub struct Pause;

impl Shard for Pause {
  type Compiled = ();
  type State = bool;
  const NAME: &'static str = PAUSE_DESC.name;
  const VERSION: u32 = PAUSE_DESC.version;

  fn compose(args: &Args, ctx: Ctx) -> Result<Composed<()>> {
    compose_pause(args, ctx).map(|c| Composed {
      compiled: (),
      output: c.output,
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<bool> {
    Ok(false)
  }

  fn activate(_: &(), paused: &mut bool, _: &mut ActivationCtx<'_>, input: &Var) -> Result<Step> {
    if *paused {
      *paused = false;
      Ok(Step::Next(input.clone()))
    } else {
      *paused = true;
      Ok(Step::Suspend)
    }
  }
}

/// `Pause(seconds)`: suspends until at least `seconds` have passed, at
/// least once. The resume point is when it started waiting. Each suspension
/// tells the scheduler when it is due (`ActivationCtx::resume_at`).
pub struct TimedPause;

impl Shard for TimedPause {
  type Compiled = Duration;
  type State = Option<Instant>;
  const NAME: &'static str = PAUSE_DESC.name;
  const VERSION: u32 = PAUSE_DESC.version;

  fn compose(args: &Args, ctx: Ctx) -> Result<Composed<Duration>> {
    compose_pause(args, ctx)
  }

  fn instantiate(_: &Duration, _: &mut InstanceCtx) -> Result<Option<Instant>> {
    Ok(None)
  }

  fn activate(
    duration: &Duration,
    since: &mut Option<Instant>,
    ctx: &mut ActivationCtx<'_>,
    input: &Var,
  ) -> Result<Step> {
    match *since {
      None => {
        let start = Instant::now();
        *since = Some(start);
        wake_after(ctx, start, *duration);
        Ok(Step::Suspend)
      }
      Some(start) if start.elapsed() < *duration => {
        wake_after(ctx, start, *duration);
        Ok(Step::Suspend)
      }
      Some(_) => {
        *since = None;
        Ok(Step::Next(input.clone()))
      }
    }
  }
}

/// Tells the scheduler when a timed pause is due. A pause too long for the
/// clock to reach never is: nothing wakes it, like a waker that never fires.
fn wake_after(ctx: &mut ActivationCtx<'_>, start: Instant, duration: Duration) {
  match start.checked_add(duration) {
    Some(at) => ctx.resume_at(at),
    None => ctx.set_waiting(),
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
  ForEach,
  iterate::LoopCompiled,
  iterate::FOR_EACH_DESC,
  |args, ctx| iterate::compose_loop(args, ctx, iterate::LoopKind::ForEach),
  Loop
);
composite!(
  Map,
  iterate::LoopCompiled,
  iterate::MAP_DESC,
  |args, ctx| iterate::compose_loop(args, ctx, iterate::LoopKind::Map),
  Loop
);
composite!(
  Filter,
  iterate::LoopCompiled,
  iterate::FILTER_DESC,
  |args, ctx| iterate::compose_loop(args, ctx, iterate::LoopKind::Filter),
  Loop
);
composite!(
  Reduce,
  iterate::LoopCompiled,
  iterate::REDUCE_DESC,
  |args, ctx| iterate::compose_loop(args, ctx, iterate::LoopKind::Reduce),
  Loop
);
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
