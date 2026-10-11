//! Control flow added for real scripts: `If`, `Match`, `Maybe`, `All`,
//! `Any`. Descriptions, compose and compiled types live here; the engine
//! runs them (`stackless/engine.rs`, adapters in `stackless/shards.rs`).
//! Every compiled type keeps its nested flows in one list
//! ([`ControlFlows::flows`]) so instantiation, cleanup and size accounting
//! are one helper.

use super::*;

/// The nested flows of a control shard, in a fixed order.
pub trait ControlFlows {
  fn flows(&self) -> &[CompiledFlow];
}

fn compose_error(
  shard: &str,
  param: &str,
  index: usize,
  code: &'static str,
  message: String,
) -> Error {
  Error::Diagnostic(Box::new(
    Diagnostic::new(Phase::Compose, "compose-error", code, message)
      .shard(shard)
      .param(param, Some(index)),
  ))
}

/// A predicate flow, which must output Bool.
fn compose_predicate(
  ctx: &mut ComposeCtx<'_>,
  flow: &[ShardDef],
  input: Type,
  conditional: bool,
  shard: &str,
  param: &str,
  index: usize,
) -> Result<CompiledFlow> {
  let compiled = if conditional {
    ctx.compose_flow_conditional(flow, input)?
  } else {
    ctx.compose_flow(flow, input)?
  };
  if !Type::bool().accepts(compiled.output) {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "predicate-not-bool",
        format!("{param} must output Bool, got {}", compiled.output),
      )
      .shard(shard)
      .param(param, Some(index))
      .types(
        Some(TypeRef::of(compiled.output)),
        vec![TypeRef::named(TypeName::Bool)],
      ),
    )));
  }
  Ok(compiled)
}

const FLOW: Forms = Forms::FLOW;

// --- If ---

pub static IF_PARAMS: &[ParamDecl] = &[
  decl(
    "predicate",
    crate::shard_doc!("A flow that receives the input and outputs a Bool."),
    FLOW,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "then",
    crate::shard_doc!("The flow to run when the predicate is true."),
    FLOW,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "else",
    crate::shard_doc!("The flow to run when the predicate is false."),
    FLOW,
    NONE_TYPES,
    Requirement::Optional,
  ),
  decl(
    "passthrough",
    crate::shard_doc!("Output the input instead of the branch's output."),
    Forms::LITERAL,
    &[TypeName::Bool],
    Requirement::Default(DefaultValue::Bool(false)),
  ),
];

pub const IF_DESC: ShardDesc = ShardDesc {
  name: "If",
  version: 1,
  summary: crate::shard_doc!("Runs `then` or `else` depending on a predicate."),
  help: crate::shard_doc!(
    "All three flows receive If's input. The output is the branch's output (a union when the branches differ); without `else`, a false predicate outputs the input. With `passthrough`, the input. A Stop, Restart or Return inside a flow propagates."
  ),
  params: Params::Declared(IF_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Dynamic(crate::shard_doc!(
    "the branch's output, or the input with Passthrough"
  )),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// `flows`: `[predicate, then]` or `[predicate, then, else]`.
pub struct IfCompiled {
  pub(crate) flows: Vec<CompiledFlow>,
  pub(crate) passthrough: bool,
}

impl ControlFlows for IfCompiled {
  fn flows(&self) -> &[CompiledFlow] {
    &self.flows
  }
}

pub(crate) fn compose_if(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<IfCompiled>> {
  let input = ctx.input();
  let pred = compose_predicate(
    ctx,
    args.flow("predicate").expect("decoded"),
    input,
    false,
    "If",
    "predicate",
    0,
  )?;
  let then = ctx.compose_flow_conditional(args.flow("then").expect("decoded"), input)?;
  let passthrough = args.bool("passthrough").unwrap_or(false);
  let els = match args.flow("else") {
    Some(flow) => Some(ctx.compose_flow_conditional(flow, input)?),
    None => None,
  };
  let output = if passthrough {
    input
  } else {
    Type::union([then.output, els.as_ref().map_or(input, |e| e.output)])
  };
  let mut flows = vec![pred, then];
  flows.extend(els);
  Ok(Composed {
    compiled: IfCompiled { flows, passthrough },
    output,
  })
}

// --- Match ---

pub static MATCH_PARAMS: &[ParamDecl] = &[
  decl(
    "cases",
    crate::shard_doc!("Value-flow pairs, `[value {flow} value {flow}]`."),
    Forms::CASES,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "default",
    crate::shard_doc!(
      "The flow to run when no case matches. Required unless the cases cover every value of the input type; `default: {}` outputs the input."
    ),
    FLOW,
    NONE_TYPES,
    Requirement::Optional,
  ),
];

pub const MATCH_DESC: ShardDesc = ShardDesc {
  name: "Match",
  version: 1,
  summary: crate::shard_doc!("Runs the flow of the first case equal to the input."),
  help: crate::shard_doc!(
    "Cases are tried in order and compare as in Is; `none` matches only none. The matched flow receives the input, and Match outputs that flow's output (an empty flow `{}` outputs its input). Match must be exhaustive: either the cases cover every value of the input type (true and false for a Bool, none for None, all of them for a union of those), or `default:` runs when nothing matched."
  ),
  params: Params::Declared(MATCH_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Dynamic(crate::shard_doc!(
    "the union of the case and default outputs"
  )),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub struct MatchCompiled {
  /// One value per case flow.
  pub(crate) values: Vec<Var>,
  /// The case flows, then the default flow if there is one.
  pub(crate) flows: Vec<CompiledFlow>,
}

impl ControlFlows for MatchCompiled {
  fn flows(&self) -> &[CompiledFlow] {
    &self.flows
  }
}

impl MatchCompiled {
  /// The flow to run for `input`: the first equal case, else the default.
  /// Compose proved one of them exists for every input of its type.
  pub(crate) fn find(
    &self,
    input: &Var,
    meter: Option<&crate::compose_time::Meter>,
  ) -> Result<usize> {
    let found = match meter {
      None => {
        let mut found = None;
        for (i, v) in self.values.iter().enumerate() {
          if super::values::values_equal(v, input) {
            found = Some(i);
            break;
          }
        }
        found
      }
      Some(meter) => self.find_metered(input, meter)?,
    };
    match found {
      Some(i) => Ok(i),
      // The default case, when there is one.
      None if self.flows.len() > self.values.len() => Ok(self.values.len()),
      None => Err(no_case(input)),
    }
  }

  /// [`Self::find`] in a compose-time evaluation: each case compared is
  /// charged by the size of the smaller side, which bounds the comparison.
  fn find_metered(&self, input: &Var, meter: &crate::compose_time::Meter) -> Result<Option<usize>> {
    let size = meter.traverse(input)?;
    for (i, v) in self.values.iter().enumerate() {
      let compared = crate::compose_time::text_size(v, size).unwrap_or(size);
      meter.charge((compared / crate::compose_time::TRAVERSAL_BYTES + 1) as u64)?;
      if super::values::values_equal(v, input) {
        return Ok(Some(i));
      }
    }
    Ok(None)
  }
}

/// The values a type has, when there are finitely many and Match can prove
/// coverage of them: Bool, None and unions of those.
fn finite_values(ty: Type) -> Option<Vec<Var>> {
  match ty.desc() {
    TypeDesc::Bool => Some(vec![Var::Bool(false), Var::Bool(true)]),
    TypeDesc::None => Some(vec![Var::None]),
    TypeDesc::Union(members) => {
      let mut all = Vec::new();
      for m in members.iter() {
        all.extend(finite_values(*m)?);
      }
      Some(all)
    }
    _ => None,
  }
}

pub(crate) fn compose_match(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Composed<MatchCompiled>> {
  let input = ctx.input();
  let cases = args.cases("cases").expect("decoded Cases");
  let mut values = Vec::with_capacity(cases.len());
  let mut flows = Vec::with_capacity(cases.len() + 1);
  for (value, flow) in cases {
    let ty = value.type_of();
    if !super::values::comparable(input, ty) {
      return Err(compose_error(
        "Match",
        "cases",
        0,
        "unmatchable-case",
        format!("the case {value} ({ty}) can never match a {input} input"),
      ));
    }
    values.push(value.clone());
    flows.push(ctx.compose_flow_conditional(flow, input)?);
  }
  match args.flow("default") {
    Some(flow) => flows.push(ctx.compose_flow_conditional(flow, input)?),
    None => {
      let missing: Option<Vec<Var>> = finite_values(input).map(|all| {
        all
          .into_iter()
          .filter(|v| !values.iter().any(|c| super::values::values_equal(c, v)))
          .collect()
      });
      let message = match missing {
        Some(missing) if missing.is_empty() => None,
        Some(missing) => {
          let names: Vec<String> = missing.iter().map(|v| v.to_string()).collect();
          Some(format!(
            "Match does not cover {} of its {input} input: add those cases, or `default: {{...}}`",
            names.join(" and ")
          ))
        }
        None => Some(format!(
          "Match on {input} cannot list every value: add `default: {{...}}` (`default: {{}}` outputs the input)"
        )),
      };
      if let Some(message) = message {
        return Err(Error::Diagnostic(Box::new(
          Diagnostic::new(
            Phase::Compose,
            "compose-error",
            "non-exhaustive-match",
            message,
          )
          .shard("Match"),
        )));
      }
    }
  }
  let output = Type::union(flows.iter().map(|f| f.output).collect::<Vec<_>>());
  Ok(Composed {
    compiled: MatchCompiled { values, flows },
    output,
  })
}

// --- Maybe ---

pub static MAYBE_PARAMS: &[ParamDecl] = &[
  decl(
    "action",
    crate::shard_doc!("The flow to try."),
    FLOW,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "else",
    crate::shard_doc!("The flow to run, with Maybe's input, if `action` fails."),
    FLOW,
    NONE_TYPES,
    Requirement::Optional,
  ),
  decl(
    "silent",
    crate::shard_doc!("Do not log the error that `action` failed with."),
    Forms::LITERAL,
    &[TypeName::Bool],
    Requirement::Default(DefaultValue::Bool(false)),
  ),
];

pub const MAYBE_DESC: ShardDesc = ShardDesc {
  name: "Maybe",
  version: 1,
  summary: crate::shard_doc!("Runs `action`; if it fails, runs `else` instead."),
  help: crate::shard_doc!(
    "With `else`: on success the output is `action`'s, on an activation error (logged unless `silent`) it is `else`'s. Without `else`, Maybe passes its input through either way, as in 1.x: `action` runs for its effects. Cancellation is not caught. Effects of the part of `action` that ran stay."
  ),
  params: Params::Declared(MAYBE_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Dynamic(crate::shard_doc!(
    "Action's or Else's output; the input without Else"
  )),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::IO,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// `flows`: `[action]` or `[action, else]`.
pub struct MaybeCompiled {
  flows: Vec<CompiledFlow>,
  silent: bool,
}

impl ControlFlows for MaybeCompiled {
  fn flows(&self) -> &[CompiledFlow] {
    &self.flows
  }
}

pub(crate) fn compose_maybe(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Composed<MaybeCompiled>> {
  let input = ctx.input();
  // Action may stop partway, so what it assigns is not definitely assigned.
  let action = ctx.compose_flow_conditional(args.flow("action").expect("decoded"), input)?;
  let els = match args.flow("else") {
    Some(flow) => Some(ctx.compose_flow_conditional(flow, input)?),
    None => None,
  };
  // Without Else, the input passes through (1.x).
  let output = match &els {
    Some(e) => Type::union([action.output, e.output]),
    None => input,
  };
  let mut flows = vec![action];
  flows.extend(els);
  Ok(Composed {
    compiled: MaybeCompiled {
      flows,
      silent: args.bool("silent").unwrap_or(false),
    },
    output,
  })
}

/// What Maybe does with an error from action: `None` to propagate it.
fn maybe_caught(silent: bool, err: Error) -> std::result::Result<(), Error> {
  match err {
    Error::Cancelled => Err(Error::Cancelled),
    other => {
      if !silent {
        crate::log::emit(format!("Maybe: {other}"));
      }
      Ok(())
    }
  }
}

/// `Maybe`, written against the public composite interface
/// ([`crate::ControlShard`]), as a host crate would write one.
pub struct Maybe;

/// Which of Maybe's flows is running.
#[derive(Default)]
pub enum MaybeState {
  #[default]
  Start,
  Action,
  Else,
}

impl crate::ControlShard for Maybe {
  type Compiled = MaybeCompiled;
  type State = MaybeState;
  const NAME: &'static str = MAYBE_DESC.name;
  const VERSION: u32 = MAYBE_DESC.version;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<MaybeCompiled>> {
    compose_maybe(args, ctx)
  }

  fn flows(compiled: &MaybeCompiled) -> &[CompiledFlow] {
    &compiled.flows
  }

  fn resume(
    compiled: &MaybeCompiled,
    state: &mut MaybeState,
    _: &mut crate::ActivationCtx<'_>,
    input: &Var,
    completion: Option<Result<Var>>,
  ) -> Result<crate::ControlStep> {
    use crate::ControlStep::{Complete, Enter};
    match (&*state, completion) {
      (_, None) => {
        *state = MaybeState::Action;
        Ok(Enter(0, input.clone()))
      }
      // Without Else, the input passes through (1.x).
      (MaybeState::Action, Some(Ok(value))) => Ok(Complete(if compiled.flows.len() < 2 {
        input.clone()
      } else {
        value
      })),
      (MaybeState::Action, Some(Err(err))) => {
        maybe_caught(compiled.silent, err)?;
        if compiled.flows.len() > 1 {
          *state = MaybeState::Else;
          Ok(Enter(1, input.clone()))
        } else {
          Ok(Complete(input.clone()))
        }
      }
      (MaybeState::Else, Some(result)) => result.map(Complete),
      (MaybeState::Start, Some(_)) => unreachable!("a completion before entering"),
    }
  }
}

// --- All, Any ---

pub static CONDITIONS_PARAMS: &[ParamDecl] = &[decl(
  "conditions",
  crate::shard_doc!(
    "One or more conditions, checked left to right: Bool literals, Bool variables, or flows that receive the input and output a Bool."
  ),
  Forms::LITERAL.or(Forms::VARIABLE).or(Forms::FLOW),
  &[TypeName::Bool],
  Requirement::Variadic,
)];

pub const ALL_DESC: ShardDesc = ShardDesc {
  name: "All",
  version: 1,
  summary: crate::shard_doc!("Outputs whether every condition is true."),
  help: crate::shard_doc!(
    "Stops at the first false condition (later flows do not run). `If(All(far can-point) ...)` replaces 1.x's `{far | And | can-point}`."
  ),
  params: Params::Declared(CONDITIONS_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Fixed(TypeName::Bool),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub const ANY_DESC: ShardDesc = ShardDesc {
  name: "Any",
  version: 1,
  summary: crate::shard_doc!("Outputs whether at least one condition is true."),
  help: crate::shard_doc!(
    "Stops at the first true condition (later flows do not run). Replaces 1.x's `Or` in conditions."
  ),
  params: Params::Declared(CONDITIONS_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Fixed(TypeName::Bool),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

#[derive(Clone, Copy)]
pub(crate) enum Condition {
  Const(bool),
  Bound(Binding),
  /// An index into the compiled flows.
  Flow(usize),
}

pub struct ConditionsCompiled {
  pub(crate) conditions: Vec<Condition>,
  pub(crate) flows: Vec<CompiledFlow>,
  /// `All` stops at the first false, `Any` at the first true.
  pub(crate) stop_on: bool,
}

impl ControlFlows for ConditionsCompiled {
  fn flows(&self) -> &[CompiledFlow] {
    &self.flows
  }
}

pub(crate) fn compose_conditions(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
  shard: &'static str,
  stop_on: bool,
) -> Result<Composed<ConditionsCompiled>> {
  let input = ctx.input();
  let items = args.variadic("conditions");
  if items.is_empty() {
    return Err(compose_error(
      shard,
      "conditions",
      0,
      "missing-argument",
      format!("{shard} needs at least one condition"),
    ));
  }
  let mut conditions = Vec::with_capacity(items.len());
  let mut flows = Vec::new();
  for (i, item) in items.iter().enumerate() {
    conditions.push(match item {
      ParamValue::Value(Var::Bool(b)) => Condition::Const(*b),
      ParamValue::Var(name) => {
        let info = ctx
          .read_var(name, shard)
          .map_err(|e| e.with_param("conditions", 0))?;
        if info.ty != Type::bool() {
          return Err(compose_error(
            shard,
            "conditions",
            0,
            "wrong-variable-type",
            format!("condition {name} must be a Bool, got {}", info.ty),
          ));
        }
        Condition::Bound(info.binding)
      }
      ParamValue::Flow(flow) => {
        // Only the first condition always runs.
        flows.push(compose_predicate(
          ctx,
          flow,
          input,
          i > 0,
          shard,
          "conditions",
          0,
        )?);
        Condition::Flow(flows.len() - 1)
      }
      other => unreachable!("decoder accepted {other:?} for {shard}.Conditions"),
    });
  }
  Ok(Composed {
    compiled: ConditionsCompiled {
      conditions,
      flows,
      stop_on,
    },
    output: Type::bool(),
  })
}

// --- Repeat: Until and Forever (compose and compiled type in mod.rs) ---

#[cold]
fn no_case(input: &Var) -> Error {
  Error::Activation(format!("Match: no case matches {input}"))
}

// --- Run ---

pub static RUN_PARAMS: &[ParamDecl] = &[decl(
  "action",
  crate::shard_doc!("The flow parameter of the enclosing function to run, by name."),
  Forms::VARIABLE,
  NONE_TYPES,
  Requirement::Required,
)];

pub const RUN_DESC: ShardDesc = ShardDesc {
  name: "Run",
  version: 1,
  summary: crate::shard_doc!("Runs the block a caller passed for a flow parameter."),
  help: crate::shard_doc!(
    "Inside a function with a flow parameter (`params: {action: Flow(input: Int output: Int)}`), `Run(action)` runs the block the call passed, on the caller's variables, with Run's input (which the parameter's `input` type must accept; with `input: None` the block ignores its input and receives none, whatever Run's input is). With an `output` type Run outputs the block's output (with `output: None`, the block's output is discarded and Run outputs none); without one it passes its input through. The block may suspend; a failure inside it fails Run. A flow parameter can only be run or passed on as a flow argument to another call."
  ),
  params: Params::Declared(RUN_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Dynamic(crate::shard_doc!(
    "the block's output, or the input when the parameter declares no output"
  )),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// `Run(action)`: the flow parameter, by its index among the enclosing
/// function's flow parameters. The engine resolves it through the running
/// invocation to the block its call passed.
pub struct RunCompiled {
  pub(crate) param: u32,
  /// No declared output: Run outputs its input.
  pub(crate) passthrough: bool,
  /// Declared `output: None`: the block's output is discarded and Run
  /// outputs none.
  pub(crate) discards_output: bool,
  /// Declared `input: None`: the block ignores its input and gets none.
  pub(crate) ignores_input: bool,
}

pub(crate) fn compose_run(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<RunCompiled>> {
  let name = variable(args, "action");
  let Some((index, ty)) = ctx.flow_param(name) else {
    let message = match ctx.function_name() {
      Some(function) => format!(
        "{name} is not a flow parameter of {function}; Run runs a block passed for a parameter declared `{name}: Flow` (`Flow(input: T output: U)`)"
      ),
      None => format!(
        "{name} is not a flow parameter: Run runs a block passed to a function, inside that function's body"
      ),
    };
    return Err(param_error(
      args,
      "Run",
      "action",
      "compose-error",
      "not-a-flow",
      message,
    ));
  };
  let input = ctx.input();
  let ignores_input = ty.input == Type::none();
  if !ignores_input && !ty.input.accepts(input) {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "input-type-mismatch",
        "input-type-mismatch",
        format!("Run({name}) needs {} input, got {input}", ty.input),
      )
      .shard("Run")
      .types(Some(TypeRef::of(input)), vec![TypeRef::of(ty.input)]),
    )));
  }
  Ok(Composed {
    compiled: RunCompiled {
      param: u32::try_from(index).expect("flow parameter index fits u32"),
      passthrough: ty.output.is_none(),
      discards_output: ty.output == Some(Type::none()),
      ignores_input,
    },
    output: ty.output.unwrap_or(input),
  })
}
