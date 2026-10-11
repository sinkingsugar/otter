//! Shards over spawned instances (docs/host-requirements.md H5): `Wait`,
//! `Peek` and `IsRunning` read a `Wire.Instance` handle `Spawn` returned;
//! `Stop(w)` (in `values`) ends one. `Wait` suspends, so it is a full shard
//! (`stackless::shards::Wait`); its description and compose live here.

use std::time::Duration;

use super::leaf::LeafShard;
use super::{Operand, decl, param_error};
use crate::args::Args;
use crate::compose::{Binding, ComposeCtx};
use crate::describe::{
  Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets, TypeName,
};
use crate::diagnostic::{Diagnostic, Phase};
use crate::error::{Error, Result};
use crate::instance::{InstanceCtx, LeafCtx, Outcome};
use crate::shard::{Composed, Flow};
use crate::types::Type;
use crate::var::Var;
use crate::wire_instance::{WIRE_INSTANCE, completion_of};

pub(crate) const HANDLE: &[TypeName] = &[TypeName::Object(&WIRE_INSTANCE)];

/// The handle parameter `Wait`, `Peek`, `IsRunning` and `Stop` share.
pub(crate) const fn handle_param(requirement: Requirement) -> ParamDecl {
  decl(
    "wire",
    crate::shard_doc!("A variable holding the instance, as `Spawn` returned it."),
    Forms::VARIABLE,
    HANDLE,
    requirement,
  )
}

/// The variable a handle parameter names.
pub(crate) fn compose_handle(
  args: &Args,
  shard: &str,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Binding> {
  match Operand::compose_arg(args, "wire", shard, ctx)? {
    (Operand::Bound(binding), _) => Ok(binding),
    (Operand::Const(_), _) => unreachable!("declared a variable"),
  }
}

/// What an ended instance gives `Wait` and `Peek`: its value; its error, if
/// it failed; a distinct error if it was stopped or cancelled.
pub(crate) fn ended(outcome: Outcome, wire: &str, shard: &str) -> Result<Var> {
  let (code, how) = match outcome {
    Outcome::Completed(value) => return Ok(value),
    Outcome::Failed(err) => return Err(err),
    Outcome::Stopped => ("wire-stopped", "was stopped"),
    Outcome::Cancelled => ("wire-cancelled", "was cancelled"),
  };
  Err(Error::Diagnostic(Box::new(
    Diagnostic::new(
      Phase::Activate,
      "activation-error",
      code,
      format!("{shard}: the instance of {wire} {how}"),
    )
    .shard(shard),
  )))
}

// --- Wait ---

pub static WAIT_PARAMS: &[ParamDecl] = &[
  handle_param(Requirement::Required),
  decl(
    "timeout",
    crate::shard_doc!(
      "The longest to wait, in seconds; past it, Wait fails. Must be finite, not negative and representable as a duration."
    ),
    Forms::LITERAL,
    &[TypeName::Float],
    Requirement::Optional,
  ),
];

pub const WAIT_DESC: ShardDesc = ShardDesc {
  name: "Wait",
  version: 1,
  summary: crate::shard_doc!("Waits for a spawned instance to end, and outputs its value."),
  help: crate::shard_doc!(
    "Suspends until the instance ends and its cleanup has run, then outputs the value it completed with. Fails with the instance's error if it failed, and with a distinct error if it was stopped (`Stop(w)`) or cancelled (its owner ended, or the host). An instance that already ended answers at once; any number of instances may wait on one. Ignores its input."
  ),
  params: Params::Declared(WAIT_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Fixed(TypeName::Any),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::IO,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct WaitCompiled {
  pub(crate) handle: Binding,
  pub(crate) timeout: Option<Duration>,
}

pub(crate) fn compose_wait(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Composed<WaitCompiled>> {
  let handle = compose_handle(args, "Wait", ctx)?;
  let timeout = match args.literal("timeout") {
    None => None,
    Some(Var::Float(secs)) => match Duration::try_from_secs_f64(*secs) {
      Ok(duration) => Some(duration),
      Err(_) => {
        return Err(param_error(
          args,
          "Wait",
          "timeout",
          "compose-error",
          "invalid-argument-value",
          format!(
            "timeout must be finite, not negative and representable as a duration, got {secs}"
          ),
        ));
      }
    },
    Some(_) => unreachable!("decoded Float"),
  };
  Ok(Composed {
    compiled: WaitCompiled { handle, timeout },
    output: Type::any(),
  })
}

// --- Peek ---

pub static PEEK_PARAMS: &[ParamDecl] = &[handle_param(Requirement::Required)];

pub const PEEK_DESC: ShardDesc = ShardDesc {
  name: "Peek",
  version: 1,
  summary: crate::shard_doc!("Outputs a spawned instance's value if it has ended, none if not."),
  help: crate::shard_doc!(
    "Never suspends. Outputs none while the instance runs, and the value it completed with once it has ended. Fails as Wait does if it failed, was stopped or was cancelled. Ignores its input."
  ),
  params: Params::Declared(PEEK_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Fixed(TypeName::Any),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::IO,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct Peek;

impl LeafShard for Peek {
  type Compiled = Binding;
  type State = ();
  const DESC: ShardDesc = PEEK_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    Ok(Composed {
      compiled: compose_handle(args, "Peek", ctx)?,
      output: Type::any(),
    })
  }

  fn instantiate(_: &Binding, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(handle: &Binding, _: &mut (), ctx: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    let value = ctx.get(*handle);
    let completion = completion_of(&value)?;
    match completion.outcome() {
      None => Ok(Flow::Next(Var::None)),
      Some(outcome) => ended(outcome, completion.wire(), "Peek").map(Flow::Next),
    }
  }
}

// --- IsRunning ---

pub static IS_RUNNING_PARAMS: &[ParamDecl] = &[handle_param(Requirement::Required)];

pub const IS_RUNNING_DESC: ShardDesc = ShardDesc {
  name: "IsRunning",
  version: 1,
  summary: crate::shard_doc!("Outputs whether a spawned instance has not ended yet."),
  help: crate::shard_doc!(
    "True from Spawn until the instance ends (an instance not started yet is running), false after, however it ended. Ignores its input."
  ),
  params: Params::Declared(IS_RUNNING_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Fixed(TypeName::Bool),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::IO,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct IsRunning;

impl LeafShard for IsRunning {
  type Compiled = Binding;
  type State = ();
  const DESC: ShardDesc = IS_RUNNING_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
    Ok(Composed {
      compiled: compose_handle(args, "IsRunning", ctx)?,
      output: Type::bool(),
    })
  }

  fn instantiate(_: &Binding, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(handle: &Binding, _: &mut (), ctx: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    let value = ctx.get(*handle);
    Ok(Flow::Next(Var::Bool(!completion_of(&value)?.is_finished())))
  }
}
