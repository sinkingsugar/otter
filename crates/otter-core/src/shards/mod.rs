//! The prototype's shard subset: enough to express the 1.x benchmark's entity
//! wire and the nested suspension acceptance test (design doc §5).
//!
//! Shards that cannot suspend implement [`leaf::LeafShard`]; shards that wait
//! on async work implement [`async_shard::AsyncShard`]. Control-flow shards
//! (which suspend or run nested flows) keep their descriptions, compose
//! logic and compiled types here and in [`control`]; the engine runs them
//! ([`crate::stackless::shards`]).
//!
//! [`defs`] has helpers that build [`ShardDef`]s, standing in for the parser.

pub mod async_shard;
pub mod control;
pub mod data;
pub mod leaf;
pub mod math;
pub mod sim;
pub mod values;

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use crate::args::Args;
use crate::compose::{Binding, CompiledWire, ComposeCtx};
use crate::describe::{
  DefaultValue, Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets,
  TypeName,
};
use crate::diagnostic::{Diagnostic, Phase, TypeRef};
use crate::error::{Error, Result};
use crate::flow::CompiledFlow;
use crate::instance::{Frames, InstanceId};
use crate::shard::{Composed, ParamValue, ShardDef, ShardType};
use crate::stackless::shards as sl;
use crate::types::{Type, TypeDesc};
use crate::var::Var;
use leaf::leaf_type;

pub static CONST: ShardType = leaf_type::<leaf::Const>();
pub static VAR: ShardType = leaf_type::<leaf::VarDecl>();
/// `value = name`; not in the catalog.
pub static BIND: ShardType = leaf_type::<leaf::Bind>();
pub static KEEP: ShardType = leaf_type::<leaf::Keep>();
pub static UPDATE: ShardType = leaf_type::<leaf::Update>();
pub static GET: ShardType = leaf_type::<leaf::Get>();
pub static INC: ShardType = leaf_type::<leaf::Inc>();
pub static ADD: ShardType = leaf_type::<leaf::Add>();
pub static IS_LESS: ShardType = leaf_type::<leaf::IsLess>();
pub static IS_MORE_EQUAL: ShardType = leaf_type::<leaf::IsMoreEqual>();
pub static WHEN: ShardType = ShardType::new(WHEN_DESC).implemented_by::<sl::When>();
pub static IF: ShardType = ShardType::new(control::IF_DESC).implemented_by::<sl::If>();
pub static MATCH: ShardType = ShardType::new(control::MATCH_DESC).implemented_by::<sl::Match>();
pub static MAYBE: ShardType = ShardType::new(control::MAYBE_DESC).controlled_by::<control::Maybe>();
pub static ALL: ShardType = ShardType::new(control::ALL_DESC).implemented_by::<sl::All>();
pub static ANY: ShardType = ShardType::new(control::ANY_DESC).implemented_by::<sl::Any>();
pub static RUN: ShardType = ShardType::new(control::RUN_DESC).implemented_by::<sl::Run>();
pub static SUB: ShardType = ShardType::new(SUB_DESC).implemented_by::<sl::Sub>();
pub static ONCE: ShardType = ShardType::new(ONCE_DESC).implemented_by::<sl::Once>();
pub static REPEAT: ShardType = ShardType::new(REPEAT_DESC).implemented_by::<sl::Repeat>();
pub static WHILE: ShardType = ShardType::new(WHILE_DESC).implemented_by::<sl::While>();
pub static PAUSE: ShardType = ShardType::new(PAUSE_DESC).implemented_by::<sl::Pause>();
pub static SPAWN: ShardType = ShardType::new(SPAWN_DESC).implemented_by::<sl::Spawn>();
pub static PROBE: ShardType = leaf_type::<leaf::Probe>();
pub static REQUEST: ShardType = async_shard::async_type::<sim::Request>();
pub static RETURN: ShardType = leaf_type::<leaf::Return>();
/// The internal node behind a call to a script function
/// ([`ShardDef::call`]); not in the catalog, never written by name.
pub static CALL: ShardType = ShardType::new(CALL_DESC).implemented_by::<sl::Call>();

/// Every shard type in this crate, for building a [`crate::Catalog`].
pub static CATALOG: &[&ShardType] = &[
  &CONST,
  &VAR,
  &KEEP,
  &UPDATE,
  &GET,
  &INC,
  &ADD,
  &IS_LESS,
  &IS_MORE_EQUAL,
  &WHEN,
  &IF,
  &MATCH,
  &MAYBE,
  &ALL,
  &ANY,
  &RUN,
  &ONCE,
  &SUB,
  &data::TAKE,
  &data::PUSH,
  &data::SEQ_MAKE,
  &data::TABLE_MAKE,
  &data::STRING_FORMAT,
  &math::SUBTRACT,
  &math::MULTIPLY,
  &math::DIVIDE,
  &math::DEC,
  &math::ABS,
  &math::ROUND,
  &math::FLOOR,
  &math::CEIL,
  &math::LENGTH,
  &values::LOG,
  &values::STOP,
  &values::IS,
  &values::IS_NOT,
  &values::IS_MORE,
  &values::IS_LESS_EQUAL,
  &values::IS_ANY,
  &values::PARSE_INT,
  &values::COUNT,
  &values::NOT,
  &values::IS_NONE,
  &values::IS_NOT_NONE,
  &values::TIME_NOW,
  &values::TO_STRING,
  &values::TO_INT,
  &values::TO_FLOAT,
  &values::TO_HEX,
  &values::BYTES_TO_STRING,
  &values::STRING_TO_BYTES,
  &values::PARSE_FLOAT,
  &values::TO_FLOAT2,
  &values::TO_FLOAT3,
  &values::TO_FLOAT4,
  &values::EXPECT_INT,
  &values::EXPECT_FLOAT,
  &values::EXPECT_BOOL,
  &values::EXPECT_STRING,
  &values::EXPECT_SEQ,
  &values::EXPECT_TABLE,
  &values::EXPECT_FLOAT2,
  &values::EXPECT_FLOAT3,
  &values::EXPECT_FLOAT4,
  &REPEAT,
  &WHILE,
  &PAUSE,
  &SPAWN,
  &RETURN,
  &PROBE,
  &REQUEST,
];

// --- helpers ---

/// A parameter declaration (shorthand for the statics below).
const fn decl(
  name: &'static str,
  help: &'static str,
  forms: Forms,
  types: &'static [TypeName],
  requirement: Requirement,
) -> ParamDecl {
  ParamDecl {
    name,
    help,
    forms,
    types,
    requirement,
    ty: None,
  }
}

const OPERAND: Forms = Forms::LITERAL.or(Forms::VARIABLE);
const NONE_TYPES: &[TypeName] = &[];
const COMPARABLE: &[TypeName] = &[TypeName::Int, TypeName::Float];

/// A structured compose error about one of a shard's parameters.
fn param_error(
  args: &Args,
  shard: &str,
  param: &str,
  kind: &'static str,
  code: &'static str,
  message: String,
) -> Error {
  Error::Diagnostic(Box::new(
    Diagnostic::new(Phase::Compose, kind, code, message)
      .shard(shard)
      .param(param, Some(args.param_index(param))),
  ))
}

/// The decoded variable name of a declared variable parameter.
fn variable<'a>(args: &'a Args, param: &str) -> &'a str {
  args.variable(param).expect("decoded variable parameter")
}

/// A constant, or a variable binding read at activation (contract §2).
#[derive(Clone, Debug)]
pub enum Operand {
  Const(Var),
  Bound(Binding),
}

impl Operand {
  /// Like [`Operand::compose_arg`], for a parameter declared
  /// `Requirement::Optional`: `None` when the script did not give it.
  pub fn compose_optional_arg(
    args: &Args,
    name: &str,
    shard: &str,
    ctx: &mut ComposeCtx<'_>,
  ) -> Result<Option<(Operand, Type)>> {
    match args.get(name) {
      None => Ok(None),
      Some(_) => Operand::compose_arg(args, name, shard, ctx).map(Some),
    }
  }

  /// Composes a declared literal-or-variable parameter (already decoded):
  /// the literal, or the variable's binding (an unknown or possibly
  /// uninitialized variable is a structured error naming `shard` and the
  /// parameter). Returns the operand and its type, for the shard to check.
  /// The parameter must be required or have a default; for an optional one
  /// use [`Operand::compose_optional_arg`].
  pub fn compose_arg(
    args: &Args,
    name: &str,
    shard: &str,
    ctx: &mut ComposeCtx<'_>,
  ) -> Result<(Operand, Type)> {
    match args.get(name).expect("decoded required parameter") {
      ParamValue::Value(v) => Ok((Operand::Const(v.clone()), v.type_of())),
      ParamValue::Var(var) => {
        let info = ctx
          .read_var(var, shard)
          .map_err(|e| e.with_param(name, args.param_index(name)))?;
        // The variable's type is checked against the declaration (its full
        // type, or its type list), so shards need not repeat it.
        if let Some(decl) = args.decl(name)
          && !decl.accepts(info.ty)
        {
          return Err(Error::Diagnostic(Box::new(
            Diagnostic::new(
              Phase::Compose,
              "compose-error",
              "wrong-variable-type",
              format!(
                "{name} must be {}, but {var} is {}",
                decl.expected(),
                info.ty
              ),
            )
            .shard(shard)
            .param(name, Some(args.param_index(name)))
            .types(Some(TypeRef::of(info.ty)), crate::args::expected_refs(decl)),
          )));
        }
        Ok((Operand::Bound(info.binding), info.ty))
      }
      other => unreachable!("decoder accepted {other:?} for {shard}.{name}"),
    }
  }

  /// The value at activation: the literal, or the variable's current value.
  pub fn get(&self, frames: &impl Frames) -> Var {
    match self {
      Operand::Const(v) => v.clone(),
      Operand::Bound(b) => frames.get(*b),
    }
  }
}

// --- descriptions, shared compose logic and compiled types ---
//
// Each shard has one description: its implementation, the argument decoder
// and the catalog read it. Forms, literal types and
// defaults are enforced by the decoder; compose checks what depends on
// context (bindings, input types, flow outputs, values).

pub static CONST_PARAMS: &[ParamDecl] = &[decl(
  "value",
  crate::shard_doc!("The constant value: any literal."),
  Forms::LITERAL,
  NONE_TYPES,
  Requirement::Required,
)];

pub const CONST_DESC: ShardDesc = ShardDesc {
  name: "Const",
  version: 1,
  summary: crate::shard_doc!("Outputs a constant value."),
  help: crate::shard_doc!("Ignores its input and outputs `value` on every activation."),
  params: Params::Declared(CONST_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Dynamic(crate::shard_doc!("the type of Value")),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_const(args: &Args) -> Result<Composed<Var>> {
  let value = args.literal("value").expect("decoded Value").clone();
  Ok(Composed {
    output: value.type_of(),
    compiled: value,
  })
}

/// Checks that a declaration may introduce `name` here: not the reserved
/// `input`, and not a name already visible (no shadowing, golden-path.md
/// §3.2). Frontend temporaries (`%` names) are exempt.
pub(crate) fn check_declaration(args: &Args, ctx: &mut ComposeCtx<'_>, shard: &str) -> Result<()> {
  let name = variable(args, "variable");
  if name.starts_with('%') {
    return Ok(());
  }
  if ctx.flow_param(name).is_some() {
    return Err(param_error(
      args,
      shard,
      "variable",
      "compose-error",
      "duplicate-binding",
      format!("{name} is already a flow parameter of this function; pick another name"),
    ));
  }
  if name == "input" {
    return Err(param_error(
      args,
      shard,
      "variable",
      "compose-error",
      "reserved-name",
      "`input` is reserved: it names the entry value of the wire or function; pick another name"
        .into(),
    ));
  }
  if let Some(info) = ctx.var(name) {
    let (message, help) = match info.binding {
      Binding::Mesh(_) => (
        format!("{name} is already a mesh variable"),
        format!("assign it with `Update({name})`, or pick another name"),
      ),
      Binding::Local(_) if info.mutable => (
        format!("{name} is already declared"),
        format!("assign it with `Update({name})`, or pick another name"),
      ),
      Binding::Local(_) => (
        format!("{name} is already declared (immutable)"),
        "pick another name".to_string(),
      ),
    };
    let mut err = param_error(
      args,
      shard,
      "variable",
      "compose-error",
      "duplicate-binding",
      format!("{message}; {help}"),
    );
    if let (Error::Diagnostic(d), Some(path)) = (&mut err, ctx.declaration_path(info.binding)) {
      **d = (**d)
        .clone()
        .related(format!("{name} is declared here"), path);
    }
    return Err(err);
  }
  Ok(())
}

/// An error about the variable a shard writes: unknown (with suggestions),
/// immutable, or of the wrong type.
pub(crate) fn assignable(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
  shard: &str,
) -> Result<crate::compose::VarInfo> {
  let name = variable(args, "variable");
  if let Some(err) = ctx.flow_escapes(name, shard) {
    return Err(err);
  }
  let Some(info) = ctx.var(name) else {
    let mut err = param_error(
      args,
      shard,
      "variable",
      "compose-error",
      "unknown-variable",
      format!("unknown variable {name}; declare it first with `value | Var({name})`"),
    );
    if let Error::Diagnostic(d) = &mut err {
      d.did_you_mean = crate::diagnostic::closest(name, ctx.visible_names(), 3);
    }
    return Err(err);
  };
  if !info.mutable {
    let mut err = param_error(
      args,
      shard,
      "variable",
      "compose-error",
      "immutable-binding",
      format!(
        "{name} is immutable (declared with `= {name}`); declare it with `Var({name})` to change it"
      ),
    );
    if let (Error::Diagnostic(d), Some(path)) = (&mut err, ctx.declaration_path(info.binding)) {
      **d = (**d)
        .clone()
        .related(format!("{name} is declared here"), path);
    }
    return Err(err);
  }
  Ok(info)
}

pub static VAR_PARAMS: &[ParamDecl] = &[decl(
  "variable",
  crate::shard_doc!("The mutable variable to declare; no visible variable may have its name."),
  Forms::VARIABLE,
  NONE_TYPES,
  Requirement::Required,
)];

pub const VAR_DESC: ShardDesc = ShardDesc {
  name: "Var",
  version: 1,
  summary: crate::shard_doc!("Declares a mutable variable holding the input."),
  help: crate::shard_doc!(
    "Declares a mutable variable with the input's type, visible after it in the same block and the blocks inside it. Assign it later with Update. Declaring a name that is already visible is an error (no shadowing). Passes its input through: `0 | Var(n)`."
  ),
  params: Params::Declared(VAR_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// `Var`: declares a mutable local holding the input.
pub(crate) fn compose_var(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
  check_declaration(args, ctx, "Var")?;
  let input = ctx.input();
  Ok(Composed {
    compiled: ctx
      .declare_local(variable(args, "variable"), input, true)
      .binding,
    output: input,
  })
}

pub static BIND_PARAMS: &[ParamDecl] = &[decl(
  "variable",
  crate::shard_doc!("The immutable name to bind."),
  Forms::VARIABLE,
  NONE_TYPES,
  Requirement::Required,
)];

/// `value = name`. Not in the catalog: scripts write the `=` form.
pub const BIND_DESC: ShardDesc = ShardDesc {
  name: "Bind",
  version: 1,
  summary: crate::shard_doc!("Binds an immutable name to the input (`value = name`)."),
  help: crate::shard_doc!(
    "Declares an immutable variable with the input's type. It cannot be assigned again; running the same binding again (in a loop) gives it the new value. Passes its input through."
  ),
  params: Params::Declared(BIND_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// `= name`: declares an immutable local holding the input.
pub(crate) fn compose_bind(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
  check_declaration(args, ctx, "Bind")?;
  let input = ctx.input();
  // `%` names are frontend temporaries (source cannot name them): each
  // occurrence declares a fresh slot.
  Ok(Composed {
    compiled: ctx
      .declare_local(variable(args, "variable"), input, false)
      .binding,
    output: input,
  })
}

pub static KEEP_PARAMS: &[ParamDecl] = &[
  decl(
    "variable",
    crate::shard_doc!("The persistent mutable variable to declare."),
    Forms::VARIABLE,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "value",
    crate::shard_doc!("Its initial value: a literal, applied once per instance."),
    Forms::LITERAL,
    NONE_TYPES,
    Requirement::Required,
  ),
];

pub const KEEP_DESC: ShardDesc = ShardDesc {
  name: "Keep",
  version: 1,
  summary: crate::shard_doc!(
    "Declares persistent state: a mutable variable kept across iterations."
  ),
  help: crate::shard_doc!(
    "`Keep(n 0)` declares the mutable variable `n` with the type of its literal initial value. The value is applied once, the first time the instance reaches it; later iterations of a looped wire keep what was assigned. Only allowed at a wire's top level, outside branches and loops. Passes its input through."
  ),
  params: Params::Declared(KEEP_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateful,
};

/// `Keep`: declares the persistent slot, whose initial value the frame
/// holds from its creation (golden path §3.4); the node passes its input
/// through.
pub(crate) fn compose_keep(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
  if !ctx.allows_persistent_state() {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "keep-in-stateless",
        format!(
          "Keep declares persistent state, but {} is stateless; add `stateful: true` to its declaration, or hold the value in a local (`value | Var({})`)",
          ctx.function_name().unwrap_or("this function"),
          variable(args, "variable")
        ),
      )
      .shard("Keep"),
    )));
  }
  if !ctx.at_top_level() {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "keep-not-top-level",
        "Keep declares state at the top level of a wire or function; it cannot be inside a branch or loop"
          .to_string(),
      )
      .shard("Keep"),
    )));
  }
  check_declaration(args, ctx, "Keep")?;
  let value = args.literal("value").expect("decoded value").clone();
  ctx.declare_keep(variable(args, "variable"), value);
  Ok(Composed {
    compiled: (),
    output: ctx.input(),
  })
}

pub static UPDATE_PARAMS: &[ParamDecl] = &[decl(
  "variable",
  crate::shard_doc!("The existing mutable variable to assign."),
  Forms::VARIABLE,
  NONE_TYPES,
  Requirement::Required,
)];

pub const UPDATE_DESC: ShardDesc = ShardDesc {
  name: "Update",
  version: 1,
  summary: crate::shard_doc!("Assigns the input to an existing mutable variable."),
  help: crate::shard_doc!(
    "`variable` must be declared (with Var, Keep or as a mesh variable), be mutable and have the input's type. Passes its input through."
  ),
  params: Params::Declared(UPDATE_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// `Update`: assigns the input to an existing mutable variable.
pub(crate) fn compose_update(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
  let name = variable(args, "variable");
  let input = ctx.input();
  let info = assignable(args, ctx, "Update")?;
  if !info.ty.accepts(input) {
    return Err(param_error(
      args,
      "Update",
      "variable",
      "compose-error",
      "variable-type-mismatch",
      format!("{name} is {}, cannot assign {input}", info.ty),
    ));
  }
  ctx.mark_initialized(info.binding).map_err(|e| {
    e.in_shard("Update")
      .with_param("variable", args.param_index("variable"))
  })?;
  Ok(Composed {
    compiled: info.binding,
    output: input,
  })
}

pub static GET_PARAMS: &[ParamDecl] = &[decl(
  "variable",
  crate::shard_doc!("The variable to read; it must be definitely assigned here."),
  Forms::VARIABLE,
  NONE_TYPES,
  Requirement::Required,
)];

pub const GET_DESC: ShardDesc = ShardDesc {
  name: "Get",
  version: 1,
  summary: crate::shard_doc!("Outputs a variable's value."),
  help: crate::shard_doc!(
    "Ignores its input. Reading a variable that is only assigned in a branch or loop body that might not run is a compose error."
  ),
  params: Params::Declared(GET_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Dynamic(crate::shard_doc!("the variable's type")),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_get(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
  let name = variable(args, "variable");
  let info = ctx
    .read_var(name, "Get")
    .map_err(|e| e.with_param("variable", args.param_index("variable")))?;
  Ok(Composed {
    compiled: info.binding,
    output: info.ty,
  })
}

pub static INC_PARAMS: &[ParamDecl] = &[decl(
  "variable",
  crate::shard_doc!("The mutable Int variable to increment."),
  Forms::VARIABLE,
  NONE_TYPES,
  Requirement::Required,
)];

pub const INC_DESC: ShardDesc = ShardDesc {
  name: "Math.Inc",
  version: 1,
  summary: crate::shard_doc!("Increments an Int variable and outputs the new value."),
  help: crate::shard_doc!(
    "Ignores its input. `variable` must be a mutable Int; overflow is an activation error."
  ),
  params: Params::Declared(INC_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Fixed(TypeName::Int),
  targets: Targets::All,
  aliases: &["Inc"],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// `Math.Inc`: increments an Int variable and outputs the new value.
pub(crate) fn compose_inc(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Binding>> {
  compose_counter(args, ctx, INC_DESC.name)
}

/// `Math.Inc` and `Math.Dec`: Variable must be a mutable Int.
pub(crate) fn compose_counter(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
  shard: &'static str,
) -> Result<Composed<Binding>> {
  let name = variable(args, "variable");
  let info = ctx
    .read_var(name, shard)
    .map_err(|e| e.with_param("variable", args.param_index("variable")))?;
  if !info.mutable || info.ty != Type::int() {
    return Err(param_error(
      args,
      shard,
      "variable",
      "compose-error",
      "wrong-variable-type",
      format!(
        "{name} must be a mutable Int, got {}{}",
        if info.mutable { "" } else { "immutable " },
        info.ty
      ),
    ));
  }
  ctx.mark_initialized(info.binding).map_err(|e| {
    e.in_shard(shard)
      .with_param("variable", args.param_index("variable"))
  })?;
  Ok(Composed {
    compiled: info.binding,
    output: Type::int(),
  })
}

pub(crate) fn activate_inc(binding: Binding, frames: &mut impl Frames) -> Result<Var> {
  let Var::Int(v) = frames.get(binding) else {
    return Err(Error::Activation("Inc: variable is not an Int".into()));
  };
  let Some(next) = v.checked_add(1) else {
    return Err(Error::Activation("Inc: integer overflow".into()));
  };
  frames.set(binding, Var::Int(next));
  Ok(Var::Int(next))
}

/// `Add`'s parameters. The decoder enforces these declarations and the
/// catalog documents them.
pub static ADD_PARAMS: &[ParamDecl] = &[ParamDecl {
  name: "operand",
  help: crate::shard_doc!("The value to add: a literal, or a variable read at activation."),
  forms: Forms::LITERAL.or(Forms::VARIABLE),
  types: NUMERIC,
  requirement: Requirement::Required,
  ty: None,
}];

const NUMERIC: &[TypeName] = math::ARITHMETIC;

pub const ADD_DESC: ShardDesc = ShardDesc {
  name: "Math.Add",
  version: 1,
  summary: crate::shard_doc!("Adds the operand to the input."),
  help: crate::shard_doc!(
    "Int, Float and the float vectors mix: Int with Float gives a Float, a vector with a number applies to each component, and vectors of one size work per component. Int with Int stays Int; overflow is an activation error."
  ),
  params: Params::Declared(ADD_PARAMS),
  input: InputDesc::Types(NUMERIC),
  output: OutputDesc::Dynamic(crate::shard_doc!(
    "the mixed type of the input and the operand"
  )),
  targets: Targets::All,
  aliases: &["Add"],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub static IS_LESS_PARAMS: &[ParamDecl] = &[decl(
  "operand",
  crate::shard_doc!(
    "The value to compare the input with: a literal, or a variable read at activation."
  ),
  OPERAND,
  COMPARABLE,
  Requirement::Required,
)];

pub const IS_LESS_DESC: ShardDesc = ShardDesc {
  name: "IsLess",
  version: 1,
  summary: crate::shard_doc!("Outputs whether the input is less than the operand."),
  help: crate::shard_doc!(
    "The input and the operand are Ints or Floats, mixed freely (compared by value). Comparing NaN is an activation error."
  ),
  params: Params::Declared(IS_LESS_PARAMS),
  input: InputDesc::Types(COMPARABLE),
  output: OutputDesc::Fixed(TypeName::Bool),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub static IS_MORE_EQUAL_PARAMS: &[ParamDecl] = &[decl(
  "operand",
  crate::shard_doc!(
    "The value to compare the input with: a literal, or a variable read at activation."
  ),
  OPERAND,
  COMPARABLE,
  Requirement::Required,
)];

pub const IS_MORE_EQUAL_DESC: ShardDesc = ShardDesc {
  name: "IsMoreEqual",
  version: 1,
  summary: crate::shard_doc!("Outputs whether the input is greater than or equal to the operand."),
  help: crate::shard_doc!(
    "The input and the operand are Ints or Floats, mixed freely (compared by value). Comparing NaN is an activation error."
  ),
  params: Params::Declared(IS_MORE_EQUAL_PARAMS),
  input: InputDesc::Types(COMPARABLE),
  output: OutputDesc::Fixed(TypeName::Bool),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_compare(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
  shard: &str,
) -> Result<Composed<Operand>> {
  let (operand, ty) = Operand::compose_arg(args, "operand", shard, ctx)?;
  let input = ctx.input();
  // Int and Float compare with each other by value.
  let number = |t: Type| matches!(t.desc(), TypeDesc::Int | TypeDesc::Float);
  if !number(input) || !number(ty) {
    let expected = COMPARABLE.iter().copied().map(TypeRef::named).collect();
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "input-type-mismatch",
        "input-type-mismatch",
        format!("cannot compare {input} with {ty}"),
      )
      .shard(shard)
      .param("operand", Some(args.param_index("operand")))
      .types(Some(TypeRef::of(input)), expected),
    )));
  }
  Ok(Composed {
    compiled: operand,
    output: Type::bool(),
  })
}

/// Compares an Int with a Float exactly (no rounding of the Int to f64,
/// which would make 2^53 + 1 equal to 2^53). `None` for NaN.
pub(crate) fn cmp_int_float(i: i64, f: f64) -> Option<std::cmp::Ordering> {
  use std::cmp::Ordering;
  // 2^63 is exactly representable; every Float at or beyond it is outside
  // the Int range.
  const LIMIT: f64 = 9_223_372_036_854_775_808.0;
  if f.is_nan() {
    return None;
  }
  if f >= LIMIT {
    return Some(Ordering::Less);
  }
  if f < -LIMIT {
    return Some(Ordering::Greater);
  }
  let whole = f.trunc();
  // In range, so the conversion is exact.
  Some(match i.cmp(&(whole as i64)) {
    Ordering::Equal if f > whole => Ordering::Less,
    Ordering::Equal if f < whole => Ordering::Greater,
    ordering => ordering,
  })
}

#[cold]
fn nan_compare() -> Error {
  Error::Activation("cannot compare NaN".into())
}

pub(crate) fn compare(input: &Var, operand: Var) -> Result<std::cmp::Ordering> {
  match (input, operand) {
    (Var::Int(a), Var::Int(b)) => Ok(a.cmp(&b)),
    (Var::Float(a), Var::Float(b)) => match a.partial_cmp(&b) {
      Some(ordering) => Ok(ordering),
      None => Err(nan_compare()),
    },
    (Var::Int(a), Var::Float(b)) => match cmp_int_float(*a, b) {
      Some(ordering) => Ok(ordering),
      None => Err(nan_compare()),
    },
    (Var::Float(a), Var::Int(b)) => match cmp_int_float(b, *a) {
      Some(ordering) => Ok(ordering.reverse()),
      None => Err(nan_compare()),
    },
    _ => Err(Error::Activation("comparison type mismatch".into())),
  }
}

/// Compiled form of `When` and `While`: a predicate flow and a body flow.
pub struct Predicated {
  /// Lowered into the parent flow's code at compose (`ComposeCtx::flatten`).
  pub(crate) pred: Arc<CompiledFlow>,
  pub(crate) body: Arc<CompiledFlow>,
}

/// Composes a predicate flow (which must output Bool) and a body flow that
/// might not run. Shared by `When` and `While`.
fn compose_predicate_and_body(
  pred: &[ShardDef],
  body: &[ShardDef],
  ctx: &mut ComposeCtx<'_>,
  shard: &str,
) -> Result<Composed<Predicated>> {
  let input = ctx.input();
  let pred = ctx.compose_flow(pred, input)?;
  if !Type::bool().accepts(pred.output) {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "predicate-not-bool",
        format!("predicate must output Bool, got {}", pred.output),
      )
      .shard(shard)
      .param("predicate", Some(0))
      .types(
        Some(TypeRef::of(pred.output)),
        vec![TypeRef::named(TypeName::Bool)],
      ),
    )));
  }
  // The Action flow might not run (When) or might run zero times (While).
  let body = ctx.compose_flow_conditional(body, input)?;
  Ok(Composed {
    compiled: Predicated {
      pred: Arc::new(pred),
      body: Arc::new(body),
    },
    output: input,
  })
}

pub static WHILE_PARAMS: &[ParamDecl] = &[
  decl(
    "predicate",
    crate::shard_doc!(
      "A flow that receives the input and must output a Bool; evaluated before each iteration."
    ),
    Forms::FLOW,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "action",
    crate::shard_doc!(
      "The flow to run while the predicate is true. It can run zero times, so variables it assigns are not definitely assigned after While."
    ),
    Forms::FLOW,
    NONE_TYPES,
    Requirement::Required,
  ),
];

pub const WHILE_DESC: ShardDesc = ShardDesc {
  name: "While",
  version: 1,
  summary: crate::shard_doc!("Runs an `action` flow while a predicate flow outputs true."),
  help: crate::shard_doc!(
    "Both flows receive While's input. On normal completion While passes its input through. A Stop, Restart or Return inside either flow propagates. The `action` flow may suspend; the loop resumes where it left off."
  ),
  params: Params::Declared(WHILE_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_while(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Predicated>> {
  let pred = args.flow("predicate").expect("decoded Predicate flow");
  let body = args.flow("action").expect("decoded Action flow");
  compose_predicate_and_body(pred, body, ctx, "While")
}

/// `When`'s parameters, shared by its implementation, the decoder and the
/// catalog.
pub static WHEN_PARAMS: &[ParamDecl] = &[
  ParamDecl {
    name: "predicate",
    help: crate::shard_doc!("A flow that receives the input and must output a Bool."),
    forms: Forms::FLOW,
    types: &[],
    requirement: Requirement::Required,
    ty: None,
  },
  ParamDecl {
    name: "action",
    help: crate::shard_doc!(
      "The flow to run when the predicate is true. Variables it assigns are not definitely assigned after When."
    ),
    forms: Forms::FLOW,
    types: &[],
    requirement: Requirement::Required,
    ty: None,
  },
];

pub const WHEN_DESC: ShardDesc = ShardDesc {
  name: "When",
  version: 1,
  summary: crate::shard_doc!("Runs an `action` flow if a predicate flow outputs true."),
  help: crate::shard_doc!(
    "Both flows receive When's input. On normal completion When passes its input through, whether or not the `action` flow ran. A Stop, Restart or Return inside either flow propagates."
  ),
  params: Params::Declared(WHEN_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_when(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Predicated>> {
  // The decoder guarantees both are present flows.
  let pred = args.flow("predicate").expect("decoded Predicate flow");
  let body = args.flow("action").expect("decoded Action flow");
  compose_predicate_and_body(pred, body, ctx, "When")
}

pub static ONCE_PARAMS: &[ParamDecl] = &[decl(
  "action",
  crate::shard_doc!(
    "The flow to run on the first activation. Variables it assigns are definitely assigned after Once."
  ),
  Forms::FLOW,
  NONE_TYPES,
  Requirement::Required,
)];

pub const ONCE_DESC: ShardDesc = ShardDesc {
  name: "Once",
  version: 1,
  summary: crate::shard_doc!("Runs an `action` flow on the first activation only."),
  help: crate::shard_doc!(
    "The `action` flow receives Once's input. Once passes its input through. If the `action` flow suspends during the first run, it resumes there; it never runs again after completing. If it fails (an error caught by Maybe), it runs again on the next activation. A Stop, Restart or Return inside the `action` flow propagates."
  ),
  params: Params::Declared(ONCE_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateful,
};

pub(crate) fn compose_once(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Composed<CompiledFlow>> {
  if ctx.in_flow_argument() {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "once-in-stateless",
        "Once remembers that it ran, but a block passed to a call starts fresh each time it runs; put the Once around the call",
      )
      .shard("Once"),
    )));
  }
  if !ctx.allows_persistent_state() {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "once-in-stateless",
        format!(
          "Once remembers that it ran, but {} is stateless and starts fresh on every invocation; add `stateful: true` to its declaration",
          ctx.function_name().unwrap_or("this function")
        ),
      )
      .shard("Once"),
    )));
  }
  let input = ctx.input();
  Ok(Composed {
    compiled: ctx.compose_flow(args.flow("action").expect("decoded Action flow"), input)?,
    output: input,
  })
}

pub static SUBFLOW_PARAMS: &[ParamDecl] = &[decl(
  "action",
  crate::shard_doc!(
    "The flow to run on every activation. Variables it assigns are definitely assigned after SubFlow."
  ),
  Forms::FLOW,
  NONE_TYPES,
  Requirement::Required,
)];

pub const SUB_DESC: ShardDesc = ShardDesc {
  name: "SubFlow",
  version: 1,
  summary: crate::shard_doc!("Runs an `action` flow and passes its input through."),
  help: crate::shard_doc!(
    "The `action` flow receives SubFlow's input; its output is discarded (1.x `_SubFlow`; `Sub` is 1.x's alias for Math.Subtract). A Stop, Restart or Return inside the `action` flow propagates; a suspension resumes inside it. The frontend computes parameter values and computed elements in a SubFlow before the shard that uses them."
  ),
  params: Params::Declared(SUBFLOW_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

/// `SubFlow`: the flow always runs, in the caller's frame.
pub(crate) fn compose_sub(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<CompiledFlow>> {
  let input = ctx.input();
  Ok(Composed {
    compiled: ctx.compose_flow(args.flow("action").expect("decoded Action flow"), input)?,
    output: input,
  })
}

pub struct RepeatCompiled {
  /// Lowered into the parent flow's code at compose (`ComposeCtx::flatten`).
  pub(crate) body: Arc<CompiledFlow>,
  /// `None`: no limit (Forever, or Until alone).
  pub(crate) times: Option<Operand>,
  /// Checked before each iteration; true stops the repeat.
  pub(crate) until: Option<Arc<CompiledFlow>>,
}

pub static REPEAT_PARAMS: &[ParamDecl] = &[
  decl(
    "action",
    crate::shard_doc!(
      "The flow to repeat. It can run zero times, so variables it assigns are not definitely assigned after Repeat."
    ),
    Forms::FLOW,
    NONE_TYPES,
    Requirement::Required,
  ),
  decl(
    "times",
    crate::shard_doc!(
      "How many times to run the `action` flow at most: an Int literal, or an Int variable read when the repeat starts. Negative counts run it zero times."
    ),
    OPERAND,
    &[TypeName::Int],
    Requirement::Optional,
  ),
  decl(
    "forever",
    crate::shard_doc!("Repeat without a limit (`times` is ignored); `until` can still end it."),
    Forms::LITERAL,
    &[TypeName::Bool],
    Requirement::Default(DefaultValue::Bool(false)),
  ),
  decl(
    "until",
    crate::shard_doc!(
      "A flow that receives the input and outputs a Bool, checked before each iteration: true ends the repeat."
    ),
    Forms::FLOW,
    NONE_TYPES,
    Requirement::Optional,
  ),
];

pub const REPEAT_DESC: ShardDesc = ShardDesc {
  name: "Repeat",
  version: 1,
  summary: crate::shard_doc!(
    "Runs an `action` flow a number of times, until a condition, or forever."
  ),
  help: crate::shard_doc!(
    "Needs `times`, `until` or `forever`. Before each iteration, `until` (if given) is checked and ends the repeat when true; `times` limits the iterations unless `forever`. The `action` and `until` flows receive Repeat's input. Repeat passes its input through. A Stop, Restart or Return inside a flow propagates. The flows may suspend; the repeat resumes where it left off."
  ),
  params: Params::Declared(REPEAT_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_repeat(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Composed<RepeatCompiled>> {
  let input = ctx.input();
  // Until runs before each iteration, then action: one region that might
  // not run (Times 0), composed in run order, so a variable Until assigns is
  // usable in Action but not after the Repeat.
  let (until, body) = ctx.conditional_region(|ctx| {
    let until = match args.flow("until") {
      Some(flow) => {
        let until = ctx.compose_flow(flow, input)?;
        if !Type::bool().accepts(until.output) {
          return Err(param_error(
            args,
            "Repeat",
            "until",
            "compose-error",
            "predicate-not-bool",
            format!("Until must output Bool, got {}", until.output),
          ));
        }
        Some(until)
      }
      None => None,
    };
    let body = ctx.compose_flow(args.flow("action").expect("decoded Action flow"), input)?;
    Ok((until, body))
  })?;
  let forever = args.bool("forever").unwrap_or(false);
  let times = match args.get("times") {
    Some(_) if !forever => {
      let (times, ty) = Operand::compose_arg(args, "times", "Repeat", ctx)?;
      // A literal is checked by the decoder; a variable's binding here.
      if ty != Type::int() {
        return Err(param_error(
          args,
          "Repeat",
          "times",
          "compose-error",
          "wrong-variable-type",
          format!("Times must be an Int, got {ty}"),
        ));
      }
      Some(times)
    }
    _ => None,
  };
  if times.is_none() && until.is_none() && !forever {
    return Err(param_error(
      args,
      "Repeat",
      "times",
      "compose-error",
      "missing-argument",
      "Repeat needs Times, Until or Forever".into(),
    ));
  }
  Ok(Composed {
    compiled: RepeatCompiled {
      body: Arc::new(body),
      times,
      until: until.map(Arc::new),
    },
    output: input,
  })
}

/// Attributes an error from resolving a `wire` argument to the calling shard
/// and its `wire` parameter, but only when the error is about the reference
/// itself. Diagnostics from shards inside the called wire keep their owner.
fn wire_ref_error(e: Error, args: &Args, shard: &str) -> Error {
  match e.diagnostic() {
    Some(d) if matches!(d.code, "unknown-wire" | "recursive-wire") => e
      .in_shard(shard)
      .with_param("wire", args.param_index("wire")),
    _ => e,
  }
}

pub static PAUSE_PARAMS: &[ParamDecl] = &[decl(
  "seconds",
  crate::shard_doc!(
    "The minimum time to wait, in seconds; 0 waits for the next tick. Must be finite, not negative and representable as a duration."
  ),
  Forms::LITERAL,
  &[TypeName::Float],
  Requirement::Default(DefaultValue::Float(0.0)),
)];

pub const PAUSE_DESC: ShardDesc = ShardDesc {
  name: "Pause",
  version: 1,
  summary: crate::shard_doc!("Suspends until a later tick."),
  help: crate::shard_doc!(
    "Always suspends at least once, then resumes on the first tick at least `seconds` after it started waiting. Passes its input through."
  ),
  params: Params::Declared(PAUSE_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::WAIT,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_pause(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Duration>> {
  let Some(Var::Float(secs)) = args.literal("seconds") else {
    unreachable!("decoded Seconds")
  };
  // Rejects negative, NaN, infinite and too large values.
  let Ok(duration) = Duration::try_from_secs_f64(*secs) else {
    return Err(param_error(
      args,
      "Pause",
      "seconds",
      "compose-error",
      "invalid-argument-value",
      format!("Seconds must be finite, not negative and representable as a duration, got {secs}"),
    ));
  };
  Ok(Composed {
    compiled: duration,
    output: ctx.input(),
  })
}

pub static SPAWN_PARAMS: &[ParamDecl] = &[decl(
  "wire",
  crate::shard_doc!("The wire to start a new instance of."),
  Forms::WIRE,
  NONE_TYPES,
  Requirement::Required,
)];

pub const SPAWN_DESC: ShardDesc = ShardDesc {
  name: "Spawn",
  version: 1,
  summary: crate::shard_doc!("Starts a new instance of a wire."),
  help: crate::shard_doc!(
    "The new instance receives Spawn's input and starts on the next tick, with its own variables. The wire is compiled once, through the compose cache, and every spawned instance shares the compiled wire. Passes its input through."
  ),
  params: Params::Declared(SPAWN_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::IO,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_spawn(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Composed<Arc<CompiledWire>>> {
  let name = args.wire("wire").expect("decoded Wire");
  let input = ctx.input();
  Ok(Composed {
    compiled: ctx
      .compose_wire(name, input)
      .map_err(|e| wire_ref_error(e, args, "Spawn"))?,
    output: input,
  })
}

pub const RETURN_DESC: ShardDesc = ShardDesc {
  name: "Return",
  version: 1,
  summary: crate::shard_doc!("Ends the enclosing function or wire with the input as its output."),
  help: crate::shard_doc!(
    "Inside a function, the input becomes the function's output and must fit its declared `output` type. At the top level of a wire, it ends the iteration (a looped wire starts again; a non-looped one completes with the value). Nothing after it in the flow runs."
  ),
  params: Params::Declared(&[]),
  input: InputDesc::Any,
  output: OutputDesc::Dynamic(crate::shard_doc!("nothing: the flow ends here")),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

pub(crate) fn compose_return(ctx: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
  let input = ctx.input();
  if ctx.evaluating() {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "not-compose-time",
        "Return cannot end a `#( )` evaluation: its value is what the pipeline outputs",
      )
      .shard("Return"),
    )));
  }
  if ctx.in_flow_argument() {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "control-in-flow",
        "Return cannot leave a block passed to a call: the block runs inside the called function, which it would have to end too; return from a function the block calls, or end the block with a value",
      )
      .shard("Return"),
    )));
  }
  if let Some(expected) = ctx.return_type()
    && !expected.accepts(input)
  {
    return Err(Error::Diagnostic(Box::new(
      Diagnostic::new(
        Phase::Compose,
        "compose-error",
        "return-type-mismatch",
        format!(
          "Return gives {input}, but {} declares output {expected}",
          ctx.function_name().unwrap_or("the function")
        ),
      )
      .shard("Return")
      .types(Some(TypeRef::of(input)), vec![TypeRef::of(expected)]),
    )));
  }
  Ok(Composed {
    compiled: (),
    output: Type::never(),
  })
}

/// The call node's description: its parameters are the called function's,
/// and compose checks them against that signature rather than this one.
pub const CALL_DESC: ShardDesc = ShardDesc {
  name: "Call",
  version: 1,
  summary: crate::shard_doc!("Calls a script function (written as the function's name)."),
  help: crate::shard_doc!(
    "Internal: a call site is written `Name(param: value ...)` and lowers to this node with the function's name attached."
  ),
  params: Params::Undeclared,
  input: InputDesc::Any,
  output: OutputDesc::Dynamic(crate::shard_doc!("the function's declared output")),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::NONE,
  lifetime: crate::signature::Lifetime::Stateless,
};

// --- test instrumentation ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeEventKind {
  Instantiate,
  Activate,
  Cleanup,
  /// A simulated request was aborted because its future was dropped.
  Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeEvent {
  pub tag: String,
  pub instance: InstanceId,
  pub kind: ProbeEventKind,
}

thread_local! {
  static PROBE_EVENTS: RefCell<Vec<ProbeEvent>> = const { RefCell::new(Vec::new()) };
}

/// Returns and clears the probe events recorded on this thread.
pub fn take_probe_events() -> Vec<ProbeEvent> {
  PROBE_EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

pub(crate) fn record(tag: &str, instance: InstanceId, kind: ProbeEventKind) {
  PROBE_EVENTS.with(|events| {
    events.borrow_mut().push(ProbeEvent {
      tag: tag.to_string(),
      instance,
      kind,
    })
  });
}

/// Failure modes for `Probe`, selected by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeMode {
  Normal,
  /// `"fail-instantiate"`: instantiate returns an error.
  FailInstantiate,
  /// `"panic-instantiate"`: instantiate panics.
  PanicInstantiate,
  /// `"panic-activate"`: activate panics.
  PanicActivate,
  /// `"panic-cleanup"`: cleanup panics (after recording its event).
  PanicCleanup,
}

pub struct ProbeCompiled {
  pub(crate) tag: Arc<str>,
  pub(crate) mode: ProbeMode,
}

pub static PROBE_PARAMS: &[ParamDecl] = &[
  decl(
    "tag",
    crate::shard_doc!("The tag recorded with each event."),
    Forms::LITERAL,
    &[TypeName::String],
    Requirement::Required,
  ),
  decl(
    "mode",
    crate::shard_doc!(
      "A failure mode: fail-instantiate, panic-instantiate, panic-activate or panic-cleanup. Absent: normal."
    ),
    Forms::LITERAL,
    &[TypeName::String],
    Requirement::Optional,
  ),
];

pub const PROBE_DESC: ShardDesc = ShardDesc {
  name: "Probe",
  version: 1,
  summary: crate::shard_doc!("Test instrumentation: records its lifecycle events."),
  help: crate::shard_doc!(
    "Records instantiate, activate and cleanup events for tests, and can fail in a chosen way. Passes its input through."
  ),
  params: Params::Declared(PROBE_PARAMS),
  input: InputDesc::Any,
  output: OutputDesc::Passthrough,
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::IO,
  // Its lifecycle is what tests observe, so it is stateful: a cached
  // invocation frame instantiates it per invocation.
  lifetime: crate::signature::Lifetime::Stateful,
};

/// `Probe`: records its instantiate, activate and cleanup events (for tests).
pub(crate) fn compose_probe(
  args: &Args,
  ctx: &mut ComposeCtx<'_>,
) -> Result<Composed<ProbeCompiled>> {
  let tag: Arc<str> = Arc::from(args.string("tag").expect("decoded Tag"));
  let mode = match args.string("mode") {
    None => ProbeMode::Normal,
    Some("fail-instantiate") => ProbeMode::FailInstantiate,
    Some("panic-instantiate") => ProbeMode::PanicInstantiate,
    Some("panic-activate") => ProbeMode::PanicActivate,
    Some("panic-cleanup") => ProbeMode::PanicCleanup,
    Some(other) => {
      return Err(param_error(
        args,
        "Probe",
        "mode",
        "compose-error",
        "invalid-argument-value",
        format!("unknown mode {other}"),
      ));
    }
  };
  Ok(Composed {
    compiled: ProbeCompiled { tag, mode },
    output: ctx.input(),
  })
}

pub(crate) fn probe_instantiate(c: &ProbeCompiled, instance: InstanceId) -> Result<InstanceId> {
  match c.mode {
    ProbeMode::FailInstantiate => {
      return Err(Error::Activation(format!(
        "probe {} failed to instantiate",
        c.tag
      )));
    }
    ProbeMode::PanicInstantiate => panic!("probe {} panicked in instantiate", c.tag),
    _ => {}
  }
  record(&c.tag, instance, ProbeEventKind::Instantiate);
  Ok(instance)
}

pub(crate) fn probe_activate(c: &ProbeCompiled, instance: InstanceId) {
  record(&c.tag, instance, ProbeEventKind::Activate);
  if c.mode == ProbeMode::PanicActivate {
    panic!("probe {} panicked in activate", c.tag);
  }
}

pub(crate) fn probe_cleanup(c: &ProbeCompiled, instance: InstanceId) {
  record(&c.tag, instance, ProbeEventKind::Cleanup);
  if c.mode == ProbeMode::PanicCleanup {
    panic!("probe {} panicked in cleanup", c.tag);
  }
}

/// Helpers that build shard definitions, standing in for the parser.
pub mod defs {
  use super::*;
  use crate::args::Arg;

  pub fn val(v: Var) -> ParamValue {
    ParamValue::Value(v)
  }
  pub fn var(name: &str) -> ParamValue {
    ParamValue::Var(name.to_string())
  }

  pub fn konst(v: Var) -> ShardDef {
    ShardDef::new(&CONST, vec![val(v)])
  }
  /// `#( flow )`: the value of `flow`, evaluated at compose time.
  pub fn evaluated(flow: Vec<ShardDef>) -> ShardDef {
    ShardDef::new(&CONST, vec![ParamValue::Eval(flow)])
  }
  /// `= name`.
  pub fn bind(name: &str) -> ShardDef {
    ShardDef::new(&BIND, vec![var(name)])
  }
  /// `Var(name)`.
  pub fn declare(name: &str) -> ShardDef {
    ShardDef::new(&VAR, vec![var(name)])
  }
  /// `Keep(name value)`.
  pub fn keep(name: &str, value: Var) -> ShardDef {
    ShardDef::new(&KEEP, vec![var(name), val(value)])
  }
  pub fn update(name: &str) -> ShardDef {
    ShardDef::new(&UPDATE, vec![var(name)])
  }
  pub fn get(name: &str) -> ShardDef {
    ShardDef::new(&GET, vec![var(name)])
  }
  pub fn inc(name: &str) -> ShardDef {
    ShardDef::new(&INC, vec![var(name)])
  }
  pub fn add(operand: ParamValue) -> ShardDef {
    ShardDef::new(&ADD, vec![operand])
  }
  pub fn is_less(operand: ParamValue) -> ShardDef {
    ShardDef::new(&IS_LESS, vec![operand])
  }
  pub fn is_more_equal(operand: ParamValue) -> ShardDef {
    ShardDef::new(&IS_MORE_EQUAL, vec![operand])
  }
  pub fn when(pred: Vec<ShardDef>, body: Vec<ShardDef>) -> ShardDef {
    ShardDef::new(&WHEN, vec![ParamValue::Flow(pred), ParamValue::Flow(body)])
  }
  pub fn while_(pred: Vec<ShardDef>, body: Vec<ShardDef>) -> ShardDef {
    ShardDef::new(&WHILE, vec![ParamValue::Flow(pred), ParamValue::Flow(body)])
  }
  pub fn if_(pred: Vec<ShardDef>, then: Vec<ShardDef>, els: Option<Vec<ShardDef>>) -> ShardDef {
    let mut params = vec![ParamValue::Flow(pred), ParamValue::Flow(then)];
    params.extend(els.map(ParamValue::Flow));
    ShardDef::new(&IF, params)
  }
  pub fn match_(cases: Vec<(Var, Vec<ShardDef>)>, default: Vec<ShardDef>) -> ShardDef {
    ShardDef::new(
      &MATCH,
      vec![ParamValue::Cases(cases), ParamValue::Flow(default)],
    )
  }
  pub fn maybe(action: Vec<ShardDef>, els: Option<Vec<ShardDef>>) -> ShardDef {
    let mut args = vec![Arg::pos(ParamValue::Flow(action))];
    args.extend(els.map(|e| Arg::pos(ParamValue::Flow(e))));
    args.push(Arg::named("silent", val(Var::Bool(true))));
    ShardDef::with_args(&MAYBE, args)
  }
  pub fn all(conditions: Vec<ParamValue>) -> ShardDef {
    ShardDef::new(&ALL, conditions)
  }
  pub fn repeat_until(body: Vec<ShardDef>, until: Vec<ShardDef>) -> ShardDef {
    ShardDef::with_args(
      &REPEAT,
      vec![
        Arg::pos(ParamValue::Flow(body)),
        Arg::named("until", ParamValue::Flow(until)),
      ],
    )
  }
  pub fn stop() -> ShardDef {
    ShardDef::new(&super::values::STOP, vec![])
  }
  /// `Log` without a prefix.
  pub fn log() -> ShardDef {
    ShardDef::new(&super::values::LOG, vec![])
  }
  /// `Return`.
  pub fn return_() -> ShardDef {
    ShardDef::new(&RETURN, vec![])
  }
  /// A call to the script function `name`.
  pub fn call(name: &str, args: Vec<Arg>) -> ShardDef {
    ShardDef::call(name, args)
  }
  pub fn sub(body: Vec<ShardDef>) -> ShardDef {
    ShardDef::new(&SUB, vec![ParamValue::Flow(body)])
  }
  pub fn take(key: ParamValue) -> ShardDef {
    ShardDef::new(&super::data::TAKE, vec![key])
  }
  pub fn push(name: &str) -> ShardDef {
    ShardDef::new(&super::data::PUSH, vec![var(name)])
  }
  pub fn once(body: Vec<ShardDef>) -> ShardDef {
    ShardDef::new(&ONCE, vec![ParamValue::Flow(body)])
  }
  pub fn repeat(body: Vec<ShardDef>, times: ParamValue) -> ShardDef {
    ShardDef::new(&REPEAT, vec![ParamValue::Flow(body), times])
  }
  pub fn pause() -> ShardDef {
    ShardDef::new(&PAUSE, vec![])
  }
  pub fn pause_secs(secs: f64) -> ShardDef {
    ShardDef::new(&PAUSE, vec![val(Var::Float(secs))])
  }
  pub fn spawn(wire: &str) -> ShardDef {
    ShardDef::new(&SPAWN, vec![ParamValue::Wire(wire.to_string())])
  }
  pub fn probe(tag: &str) -> ShardDef {
    ShardDef::new(&PROBE, vec![val(Var::string(tag))])
  }
  /// A simulated async request, see [`sim::Request`].
  pub fn request(delay: i64, fail: bool, abortable: bool) -> ShardDef {
    ShardDef::new(
      &REQUEST,
      vec![
        val(Var::Int(delay)),
        val(Var::Bool(fail)),
        val(Var::Bool(abortable)),
      ],
    )
  }
  /// A probe with a failure mode: `"fail-instantiate"`, `"panic-instantiate"`,
  /// `"panic-activate"` or `"panic-cleanup"`.
  pub fn probe_mode(tag: &str, mode: &str) -> ShardDef {
    ShardDef::new(&PROBE, vec![val(Var::string(tag)), val(Var::string(mode))])
  }
}
