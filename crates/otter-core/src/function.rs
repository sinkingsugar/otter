//! Script functions (golden path §3 to §5): a definition owns shared,
//! immutable compiled code; a call site holds argument code and a reference
//! to the compiled body; an invocation owns a private frame.
//!
//! A definition is composed once per input type (`ComposeCache`), against
//! its declared signature: parameters are immutable locals of the body's
//! frame, `input` is the reserved entry binding, and mesh variables are
//! visible only when declared in `uses` or `mutates`. The engine
//! (`stackless/engine.rs`) allocates a stateless invocation's frame at entry
//! and frees it at exit; a stateful call site keeps its frame, and only its
//! `Keep` slots and native state survive between invocations.

use std::sync::Arc;

use crate::compose::{Dep, FrameLayout};
use crate::describe::Forms;
use crate::flow::CompiledFlow;
use crate::shard::ShardDef;
use crate::shards::Operand;
use crate::signature::{
  Lifetime, MeshAccess, Parameter, ParameterRequirement, Signature, SignatureInput, SignatureOutput,
};
use crate::types::Type;
use crate::var::Var;

/// One declared parameter: a literal default makes it optional.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FunctionParam {
  pub name: String,
  /// A value parameter's type (`Any` for a flow parameter).
  pub ty: Type,
  pub default: Option<Var>,
  /// A flow parameter (docs/metaprogramming.md §3): the call passes a
  /// block, which the body runs with `Run`. It is not a local of the body.
  pub flow: Option<FlowType>,
}

impl FunctionParam {
  pub fn is_flow(&self) -> bool {
    self.flow.is_some()
  }
}

/// The type of a flow parameter: `Flow(input: T output: U)`. The block
/// composes with input `T` at the call site; with an output, `Run`
/// outputs the block's (which `U` must accept), without one `Run` passes
/// its input through, like `SubFlow`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FlowType {
  pub input: Type,
  pub output: Option<Type>,
}

impl std::fmt::Display for FlowType {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match (self.input == Type::any(), self.output) {
      (true, None) => write!(f, "Flow"),
      (false, None) => write!(f, "Flow(input: {})", self.input),
      (_, Some(output)) => write!(f, "Flow(input: {} output: {output})", self.input),
    }
  }
}

/// A function as the loader produces it. Immutable; part of the compose
/// cache key of every caller (through a recorded dependency).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FunctionDef {
  pub name: String,
  pub input: Type,
  pub output: Type,
  pub params: Vec<FunctionParam>,
  /// One persistent instance per call site; `Keep` and `Once` allowed.
  pub stateful: bool,
  /// A checked contract: stateless, no mesh access, no effects.
  pub pure: bool,
  /// Mesh variables the body may read, by name.
  pub uses: Vec<String>,
  /// Mesh variables the body may write, by name (reading needs `uses` too).
  pub mutates: Vec<String>,
  pub body: Vec<ShardDef>,
}

impl FunctionDef {
  /// A stateless function with no parameters and an empty body.
  pub fn new(name: &str, input: Type, output: Type) -> FunctionDef {
    FunctionDef {
      name: name.to_string(),
      input,
      output,
      params: Vec::new(),
      stateful: false,
      pure: false,
      uses: Vec::new(),
      mutates: Vec::new(),
      body: Vec::new(),
    }
  }

  pub fn param(mut self, name: &str, ty: Type) -> FunctionDef {
    self.params.push(FunctionParam {
      name: name.to_string(),
      ty,
      default: None,
      flow: None,
    });
    self
  }

  /// A parameter with a literal default (its type is the literal's).
  pub fn param_default(mut self, name: &str, default: Var) -> FunctionDef {
    self.params.push(FunctionParam {
      name: name.to_string(),
      ty: default.type_of(),
      default: Some(default),
      flow: None,
    });
    self
  }

  /// A flow parameter: the call passes a block, the body runs it with
  /// `Run(name)`.
  pub fn flow_param(mut self, name: &str, ty: FlowType) -> FunctionDef {
    self.params.push(FunctionParam {
      name: name.to_string(),
      ty: Type::any(),
      default: None,
      flow: Some(ty),
    });
    self
  }

  /// The flow parameter `name`: its index among the flow parameters and
  /// its type.
  pub fn flow_param_index(&self, name: &str) -> Option<(usize, FlowType)> {
    self
      .params
      .iter()
      .filter_map(|p| Some((p.name.as_str(), p.flow?)))
      .enumerate()
      .find(|(_, (n, _))| *n == name)
      .map(|(i, (_, ty))| (i, ty))
  }

  /// Whether the function takes a block.
  pub fn has_flow_params(&self) -> bool {
    self.params.iter().any(FunctionParam::is_flow)
  }

  pub fn stateful(mut self) -> FunctionDef {
    self.stateful = true;
    self
  }

  pub fn pure(mut self) -> FunctionDef {
    self.pure = true;
    self
  }

  pub fn uses(mut self, names: &[&str]) -> FunctionDef {
    self.uses.extend(names.iter().map(|n| n.to_string()));
    self
  }

  pub fn mutates(mut self, names: &[&str]) -> FunctionDef {
    self.mutates.extend(names.iter().map(|n| n.to_string()));
    self
  }

  pub fn body(mut self, flow: Vec<ShardDef>) -> FunctionDef {
    self.body = flow;
    self
  }

  /// Whether the body may read mesh variable `name`.
  pub fn may_read(&self, name: &str) -> bool {
    self.uses.iter().any(|n| n == name)
  }

  /// Whether the body may write mesh variable `name`.
  pub fn may_write(&self, name: &str) -> bool {
    self.mutates.iter().any(|n| n == name)
  }

  /// Whether `name` is visible in the body as a mesh variable.
  pub fn declares(&self, name: &str) -> bool {
    self.may_read(name) || self.may_write(name)
  }

  /// A function declared with `input: None` ignores its input (like a shard
  /// with an ignored input), so it can start a statement anywhere; its
  /// `input` binding is `none`.
  pub fn ignores_input(&self) -> bool {
    self.input == Type::none()
  }

  /// The declared signature, before compose (effects and mesh types are
  /// filled in by [`CompiledFunction::signature`]).
  pub fn signature(&self) -> Signature<'static> {
    Signature {
      name: self.name.clone().into(),
      revision: 1,
      input: SignatureInput::Type(self.input),
      output: SignatureOutput::Type(self.output),
      params: Some(
        self
          .params
          .iter()
          .map(|p| Parameter {
            name: p.name.clone().into(),
            help: match p.flow {
              Some(flow) => flow.to_string().into(),
              None => "".into(),
            },
            forms: if p.is_flow() {
              Forms::FLOW
            } else {
              Forms::LITERAL.or(Forms::VARIABLE)
            },
            ty: p.ty,
            requirement: match &p.default {
              Some(v) => ParameterRequirement::Default(v.clone()),
              None => ParameterRequirement::Required,
            },
          })
          .collect(),
      ),
      lifetime: if self.stateful {
        Lifetime::Stateful
      } else {
        Lifetime::Stateless
      },
      effects: crate::signature::Effects::NONE,
      uses: Vec::new(),
      mutates: Vec::new(),
      source: None,
      summary: "".into(),
      help: "".into(),
    }
  }
}

/// A `Keep` slot of a stateful function: retained across invocations, and
/// matched by name and type across reloads (golden path §11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepSlot {
  pub name: String,
  pub slot: usize,
  pub ty: Type,
  /// The declared initial value: the slot holds it from the frame's
  /// creation (or from a reload that did not carry a value over), so the
  /// `Keep` node itself is a pass-through.
  pub initial: Var,
}

/// A compiled function body: shared by every call site and invocation,
/// never cloned into callers.
pub struct CompiledFunction {
  pub def: Arc<FunctionDef>,
  /// The input type the body was composed for (`None` when the function
  /// ignores its input).
  pub input: Type,
  pub flow: CompiledFlow,
  /// The invocation frame: parameters, `input`, then the body's locals
  /// (`Keep` slots among them, listed in `keeps`).
  pub locals: FrameLayout,
  pub input_slot: usize,
  /// One slot per declared value parameter, in declaration order (flow
  /// parameters have none: their blocks run on the caller's locals).
  pub param_slots: Vec<usize>,
  pub keeps: Vec<KeepSlot>,
  /// Everything the body's compose read, for revalidation by callers.
  pub deps: Vec<Dep>,
  /// The bodies this one calls, directly or through callees, as composed.
  pub functions: crate::reload::FunctionRegistry,
  /// Whether the body (or a callee's) holds a node that is not stateless:
  /// a cached invocation frame then cleans such leaves up at exit and
  /// instantiates them again at entry (golden path §3.4).
  pub(crate) native_state: bool,
  /// The body is straight-line VM code (no node that runs through its
  /// shard): a stateless call site runs it inside the VM on the kept
  /// locals, without entering a frame.
  pub(crate) vm_only: bool,
  /// A straight-line body with no call site or loop of its own: nothing
  /// to check for readiness at entry.
  pub(crate) vm_leaf: bool,
  /// A body with flow parameters that is straight-line except for the
  /// `Run`s of its blocks: a site passing straight-line blocks inlines it
  /// with the blocks in place of the `Run`s (docs/metaprogramming.md §3.2).
  pub(crate) block_leaf: bool,
  /// Functions this body (or a callee's) calls lazily, by name: members of
  /// a recursive group not closed when the body was composed. A caller
  /// still composing one of them is inside that group, and calls this
  /// body lazily too, so the group is pinned as a unit across reloads.
  pub(crate) lazy_refs: Vec<String>,
  /// Levels of inlined calls inside the body: 0 when none, else one more
  /// than the deepest inlined callee's. A chain of small functions is
  /// inlined `compose::INLINE_DEPTH` levels deep (eight on hosts, two on
  /// the device), so a level holds a bounded copy of the code below it.
  pub(crate) inline_depth: u8,
}

impl CompiledFunction {
  /// A fresh frame: every slot unset.
  /// A fresh frame: every slot unset, except `Keep` slots at their
  /// initial values.
  pub(crate) fn fresh_locals(&self) -> Vec<Var> {
    let mut locals = vec![Var::None; self.locals.len()];
    for keep in &self.keeps {
      locals[keep.slot] = keep.initial.clone();
    }
    locals
  }

  /// Whether a slot survives between invocations of a stateful function.
  pub(crate) fn persistent(&self, slot: usize) -> bool {
    self.keeps.iter().any(|k| k.slot == slot)
  }

  /// The signature with inferred effects and mesh access.
  pub fn signature(&self) -> Signature<'static> {
    let mut signature = self.def.signature();
    signature.effects = self.flow.analysis.effects;
    signature.uses = self
      .def
      .uses
      .iter()
      .filter_map(|name| access_type(&self.flow.analysis.uses, name))
      .collect();
    signature.mutates = self
      .def
      .mutates
      .iter()
      .filter_map(|name| access_type(&self.flow.analysis.mutates, name))
      .collect();
    signature
  }
}

fn access_type(accesses: &[MeshAccess], name: &str) -> Option<MeshAccess> {
  accesses.iter().find(|a| a.name == name).cloned()
}

/// What a call site runs: the body it was composed against, or, inside a
/// recursive group (golden path M7), the function's identity, resolved
/// through the table the outermost invocation entered with, so a compiled
/// body never holds a cyclic `Arc` and a group is pinned as a unit.
pub enum CallTarget {
  Direct(Arc<CompiledFunction>),
  Lazy {
    key: crate::reload::FunctionKey,
    def: Arc<FunctionDef>,
  },
}

/// A call site: the target, the argument operands (one per declared value
/// parameter) and the blocks (one per flow parameter), in declaration order.
pub struct CallCompiled {
  pub target: CallTarget,
  pub args: Vec<Operand>,
  pub blocks: Vec<BlockArg>,
}

/// What a call passes for a flow parameter (docs/metaprogramming.md §3.2).
pub enum BlockArg {
  /// A block written at the call site, composed against the caller's
  /// scope: it runs on the caller's locals.
  Flow(CompiledFlow),
  /// A flow parameter of the calling function, passed on: the block its
  /// own caller passed (an index among the caller's flow parameters).
  Forward(u32),
}

impl CallCompiled {
  pub fn def(&self) -> &FunctionDef {
    match &self.target {
      CallTarget::Direct(body) => &body.def,
      CallTarget::Lazy { def, .. } => def,
    }
  }

  pub fn stateful(&self) -> bool {
    self.def().stateful
  }
}
