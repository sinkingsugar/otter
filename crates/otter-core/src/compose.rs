//! Compose: wire definitions in, shared compiled wires out (contract §4, §8).
//!
//! [`ComposeCtx`] records every dependency a compose reads, including failed
//! lookups. [`ComposeCache`] finds candidates by primary key (wire definition
//! and input type), then revalidates their recorded dependencies; it composes
//! only on a miss.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

mod evaluate;

use crate::args::{Args, decode};
use crate::diagnostic::{Diagnostic, PathStep, Phase, TypeRef};
use crate::error::{Error, Result};
use crate::flow::{CompiledFlow, NO_ORIGIN, OriginName, OriginStep};
use crate::function::{
  BlockArg, CallCompiled, CallTarget, CompiledFunction, FlowType, FunctionDef, KeepSlot,
};
use crate::reload::{FunctionKey, FunctionRegistry};
use crate::shard::{CompiledNode, Composed, ParamValue, ShardDef, ShardType};
use crate::shards::Operand;
use crate::signature::{Analysis, CallSite, Effects, Lifetime, NotInlined, NotVm, Occurrence};
use crate::stackless::Control;
use crate::types::Type;
use crate::var::Var;

/// A wire as the loader produces it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WireDef {
  pub name: String,
  pub looped: bool,
  pub flow: Vec<ShardDef>,
}

/// Where a variable lives: the instance's frame or the mesh's frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Binding {
  Local(usize),
  Mesh(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarInfo {
  pub binding: Binding,
  pub ty: Type,
  pub mutable: bool,
  /// Whether the variable is definitely assigned on every path that reaches
  /// the shard being composed. Mesh variables always are.
  pub initialized: bool,
}

/// One slot of a frame layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Slot {
  pub index: usize,
  pub ty: Type,
  pub mutable: bool,
}

/// Names, types and mutability of a frame's slots. Names exist only here,
/// at compose time; activation uses slot indices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameLayout {
  slots: Vec<(String, Slot)>,
  index: HashMap<String, usize>,
}

impl FrameLayout {
  pub fn lookup(&self, name: &str) -> Option<Slot> {
    self.index.get(name).map(|i| self.slots[*i].1)
  }

  pub(crate) fn declare(&mut self, name: &str, ty: Type, mutable: bool) -> Slot {
    let slot = Slot {
      index: self.slots.len(),
      ty,
      mutable,
    };
    self.index.insert(name.to_string(), slot.index);
    self.slots.push((name.to_string(), slot));
    slot
  }

  /// The type a slot was declared with.
  pub(crate) fn slot_type(&self, index: usize) -> Type {
    self.slots[index].1.ty
  }

  /// A slot no name reaches: a flattened composite keeps its input or a
  /// loop counter there (`ComposeCtx::declare_hidden`).
  pub(crate) fn declare_hidden(&mut self, ty: Type) -> Slot {
    let slot = Slot {
      index: self.slots.len(),
      ty,
      mutable: true,
    };
    self.slots.push((String::new(), slot));
    slot
  }

  /// The names visible now, for restoring at the end of a block.
  pub(crate) fn visible(&self) -> HashMap<String, usize> {
    self.index.clone()
  }

  /// Ends a block: names declared inside it stop being visible (their slots
  /// stay in the frame). Frontend temporaries (`%` names) stay visible,
  /// since lowering reads a hoisted value after the SubFlow computing it.
  pub(crate) fn restore_visible(&mut self, mut saved: HashMap<String, usize>) {
    for (name, index) in &self.index {
      if name.starts_with('%') {
        saved.insert(name.clone(), *index);
      }
    }
    self.index = saved;
  }

  /// Visible variable names, sorted (for suggestions).
  pub fn names(&self) -> Vec<String> {
    let mut names: Vec<String> = self
      .index
      .keys()
      .filter(|n| !n.starts_with('%'))
      .cloned()
      .collect();
    names.sort();
    names
  }

  pub fn len(&self) -> usize {
    self.slots.len()
  }

  pub fn is_empty(&self) -> bool {
    self.slots.is_empty()
  }
}

/// The most instructions a callee body may have to be inlined at its call
/// sites (`ComposeCtx::flatten`): a larger body stays a `VmCall`, which
/// costs one nested run per call but no code or frame slots per site.
const INLINE_BUDGET: usize = 24;

/// How many levels of inlined calls a body may contain and still be inlined
/// itself (`CompiledFunction::inline_depth`): a chain of small functions
/// is flattened this deep and called beyond it. Hosts inline deep, since
/// every inlined level is a call that costs nothing at run time; the
/// device stops at two, because each level of a chain holds a copy of the
/// code and dependency lists below it (a 24-deep chain compiled on ESP-IDF
/// peaked 90 KiB higher unbounded, past the C3's and the classic ESP32's
/// heap).
const INLINE_DEPTH: u8 = if cfg!(target_os = "espidf") { 2 } else { 8 };

/// An inlined call: the callee's body, and the site's blocks with their
/// parameters' names.
type InlinePlan<'a> = (&'a Arc<CompiledFunction>, Vec<(&'a CompiledFlow, &'a str)>);

/// Whether a call site is inlined (`ComposeCtx::flatten`): with the
/// callee's body and the site's blocks when it is, why not otherwise. One
/// decision, so what `check --json` reports for a site (`call_site`) is
/// what compose does.
fn inline_plan(c: &CallCompiled) -> std::result::Result<InlinePlan<'_>, NotInlined> {
  let CallTarget::Direct(body) = &c.target else {
    return Err(NotInlined::Recursive);
  };
  if c.stateful() {
    return Err(NotInlined::Stateful);
  }
  // The blocks written at the site, with their parameters' names, when
  // they can stand in for the callee's `Run`s: straight-line code, and a
  // block run more than once copied within the budget.
  let mut blocks = Vec::with_capacity(c.blocks.len());
  let names = body.def.params.iter().filter(|p| p.is_flow());
  for (block, param) in c.blocks.iter().zip(names) {
    let BlockArg::Flow(flow) = block else {
      return Err(NotInlined::Forwarded(param.name.as_str().into()));
    };
    if let Some(why) = not_vm_code(flow) {
      return Err(NotInlined::Block(param.name.as_str().into(), why));
    }
    blocks.push((flow, param.name.as_str()));
  }
  // A block is the site's own code; a second `Run` of it copies it.
  let mut runs = vec![0usize; blocks.len()];
  for node in &body.flow.pc_nodes {
    if let Some(Control::Run(run)) = body
      .flow
      .nodes
      .get(*node as usize)
      .and_then(|n| n.control())
    {
      runs[run.param as usize] += 1;
    }
  }
  let copies: usize = blocks
    .iter()
    .zip(&runs)
    .map(|((block, _), runs)| block.code.len() * runs.saturating_sub(1))
    .sum();
  let leaf = if blocks.is_empty() {
    body.vm_leaf
  } else {
    body.block_leaf
  };
  if !leaf {
    return Err(NotInlined::Body(
      not_vm_code(&body.flow).unwrap_or(NotVm::NotStraightLine),
    ));
  }
  if copies > INLINE_BUDGET {
    return Err(NotInlined::BlockCopies(copies as u32, INLINE_BUDGET as u32));
  }
  if !body.keeps.is_empty() {
    return Err(NotInlined::Keeps);
  }
  if !body.lazy_refs.is_empty() {
    return Err(NotInlined::CallsRecursive);
  }
  if body.flow.code.len() > INLINE_BUDGET {
    return Err(NotInlined::TooLong(
      body.flow.code.len() as u32,
      INLINE_BUDGET as u32,
    ));
  }
  if body.inline_depth >= INLINE_DEPTH {
    return Err(NotInlined::TooDeep(INLINE_DEPTH));
  }
  Ok((body, blocks))
}

/// Why a flow is not straight-line VM code: its first shard without a VM
/// form (a `Run` is the block it runs), or a call that is not inlined.
fn not_vm_code(flow: &CompiledFlow) -> Option<NotVm> {
  flow
    .code
    .iter()
    .zip(&flow.pc_nodes)
    .find_map(|(i, node)| match i.op {
      crate::inline::Op::Fallback => {
        let node = &flow.nodes[*node as usize];
        match node.control() {
          Some(Control::Run(_)) => None,
          Some(Control::Call(_)) => Some(NotVm::CallsNotInlined),
          _ => Some(NotVm::NoVmForm(node.name())),
        }
      }
      crate::inline::Op::VmCall => Some(NotVm::CallsNotInlined),
      _ => None,
    })
    .or_else(|| (!crate::inline::straight_line(&flow.code)).then_some(NotVm::NotStraightLine))
}

/// How a call site runs, for tooling (`Occurrence::call`).
fn call_site(c: &CallCompiled) -> CallSite {
  match inline_plan(c) {
    Ok(_) => CallSite {
      path: "inlined",
      reason: None,
    },
    Err(reason) => CallSite {
      path: match &c.target {
        CallTarget::Direct(body) if !c.stateful() && body.vm_only => "vm",
        _ => "framed",
      },
      reason: Some(reason),
    },
  }
}

/// A flow's code under construction: nodes, instructions and the node each
/// instruction stands for (`CompiledFlow::pc_nodes`).
struct Flat {
  nodes: Vec<Arc<dyn CompiledNode>>,
  code: Vec<crate::inline::Instruction>,
  pc_nodes: Vec<u32>,
  /// `Lowered::inlined` and `Lowered::released_slots`.
  inlined: Vec<(u32, u32)>,
  released_slots: Vec<u32>,
  /// `CompiledFlow::origins`, and the entry of the shard being added,
  /// when `record` (`ComposeCache::record_origins`).
  record: bool,
  origins: Vec<u32>,
  steps: Vec<(OriginStep, u32)>,
  current: u32,
}

impl Flat {
  /// The next instruction's index.
  fn here(&self) -> u32 {
    u32::try_from(self.code.len()).expect("flow code fits u32")
  }

  /// Starts the instructions of shard `index` of this flow.
  fn shard(&mut self, index: usize, name: impl FnOnce() -> OriginName) {
    if !self.record {
      return;
    }
    let name = name();
    self.current = self.step(
      OriginStep::Shard {
        index: u32::try_from(index).expect("shard index fits u32"),
        name,
      },
      NO_ORIGIN,
    );
  }

  /// A new origin entry, following `parent`.
  fn step(&mut self, step: OriginStep, parent: u32) -> u32 {
    self.steps.push((step, parent));
    u32::try_from(self.steps.len() - 1).expect("origin count fits u32")
  }

  /// A node of this flow with its one instruction.
  fn node(&mut self, node: Arc<dyn CompiledNode>, name: &'static str, output: Type) {
    if self.record {
      self.origins.push(self.current);
    }
    self
      .pc_nodes
      .push(u32::try_from(self.nodes.len()).expect("node count fits u32"));
    self
      .code
      .push(crate::inline::Instruction::new(node.inline(), name, output));
    self.nodes.push(node);
  }

  /// A control instruction standing for no node; returns its index (for
  /// `target`).
  fn op(&mut self, op: crate::inline::Op, output: Type) -> u32 {
    let at = self.here();
    if self.record {
      self.origins.push(self.current);
    }
    self.pc_nodes.push(crate::flow::NO_NODE);
    self.code.push(crate::inline::Instruction::new(
      Some(crate::inline::InlineOp(op)),
      "",
      output,
    ));
    at
  }

  /// Points the jump at `at` to the next instruction.
  fn target(&mut self, at: u32) {
    let here = self.here();
    self.code[at as usize].retarget(here);
  }

  /// Appends a child flow on this frame (a flattened composite's): its
  /// nodes, and its code with jump targets relocated and node references
  /// offset; the calls inlined into it stay inlined calls of this flow.
  fn append(&mut self, flow: &CompiledFlow) {
    let node_base = self.node_base();
    self.inlined.extend(
      flow
        .inlined()
        .iter()
        .map(|(start, len)| (start + node_base, *len)),
    );
    self
      .released_slots
      .extend(flow.released_slots().iter().copied());
    let prefix = flow.origins.as_ref().map_or(&[][..], |o| &o.prefix[..]);
    self.append_rebased(flow, 0, prefix);
  }

  /// Appends an inlined callee's body: like `append`, with every local slot
  /// the code addresses moved up by `slots` (the callee's frame lives at
  /// that offset in the caller's); all its nodes count as one inlined call.
  fn inline(&mut self, flow: &CompiledFlow, slots: usize, function: impl FnOnce() -> Arc<str>) {
    let len = u32::try_from(flow.nodes.len()).expect("node count fits u32");
    self.inlined.push((self.node_base(), len));
    let prefix = if self.record {
      vec![OriginStep::Function(function())]
    } else {
      Vec::new()
    };
    self.append_rebased(flow, slots, &prefix);
  }

  /// `inline` for a callee that runs blocks its call passed: each `Run`
  /// of the callee's code is replaced by its block's code, which addresses
  /// the caller's slots as composed at the call site; a `Run` without an
  /// output saves its input in `saved` and restores it after the block.
  /// The callee's own nodes and the blocks' all count as the inlined call.
  fn inline_blocks(
    &mut self,
    flow: &CompiledFlow,
    slots: usize,
    function: impl FnOnce() -> Arc<str>,
    blocks: &[(&CompiledFlow, &str)],
    saved: Option<Binding>,
  ) {
    let start = self.node_base();
    // The callee's nodes, without its `Run`s.
    let mut node_map = vec![crate::flow::NO_NODE; flow.nodes.len()];
    for (i, node) in flow.nodes.iter().enumerate() {
      if !matches!(node.control(), Some(Control::Run(_))) {
        node_map[i] = self.node_base();
        self.nodes.push(node.clone());
      }
    }
    // The callee's origins hang below the call through the function.
    let (root, steps) = if self.record {
      let root = self.step(OriginStep::Function(function()), self.current);
      let steps = u32::try_from(self.steps.len()).expect("origin count fits u32");
      if let Some(origins) = &flow.origins {
        let rebase = |at: u32| if at == NO_ORIGIN { root } else { at + steps };
        self.steps.extend(
          origins
            .steps
            .iter()
            .map(|(step, parent)| (step.clone(), rebase(*parent))),
        );
      }
      (root, steps)
    } else {
      (NO_ORIGIN, 0)
    };
    let mut pcs = Vec::with_capacity(flow.code.len() + 1);
    let mut jumps = Vec::new();
    for (pc, (instruction, node)) in flow.code.iter().zip(&flow.pc_nodes).enumerate() {
      pcs.push(self.here());
      let run = match *node {
        crate::flow::NO_NODE => None,
        node => match flow.nodes[node as usize].control() {
          Some(Control::Run(run)) => Some(run),
          _ => None,
        },
      };
      if let Some(run) = run {
        let (block, param) = blocks[run.param as usize];
        // Run passes its input on unless what follows replaces it anyway.
        let restored = run.passthrough
          && !flow
            .code
            .get(pc + 1)
            .is_some_and(crate::inline::Instruction::ignores_accumulator);
        let saved = restored.then(|| saved.expect("a hidden slot for the input"));
        if let Some(saved) = saved {
          self.op(crate::inline::Op::Set(saved), Type::any());
        }
        if run.ignores_input {
          self.op(crate::inline::Op::Const(Var::None), Type::none());
        }
        // Lexically the caller's: below the call, at its parameter.
        self
          .released_slots
          .extend(block.released_slots().iter().copied());
        self.append_rebased(block, 0, &[OriginStep::Param(param.to_string().into())]);
        if let Some(saved) = saved {
          // Restored, and ended at once (the accumulator takes the value it
          // reads): held until the callee ended, it kept a collection the
          // input shares from being uniquely owned, so the callee's next
          // push to it copied.
          self.op(crate::inline::Op::get(saved), Type::any());
          self.op(crate::inline::Op::Clear(saved), Type::any());
        } else if run.discards_output
          && !flow
            .code
            .get(pc + 1)
            .is_some_and(crate::inline::Instruction::ignores_accumulator)
        {
          self.op(crate::inline::Op::Const(Var::None), Type::none());
        }
        continue;
      }
      if self.record {
        let at = flow.origins.as_ref().map_or(NO_ORIGIN, |o| o.at[pc]);
        self
          .origins
          .push(if at == NO_ORIGIN { root } else { at + steps });
      }
      let mut instruction = instruction.clone();
      if slots != 0 {
        instruction.rebase_locals(slots);
      }
      if instruction.jump_target().is_some() {
        jumps.push(self.code.len());
      }
      self.code.push(instruction);
      self.pc_nodes.push(match *node {
        crate::flow::NO_NODE => crate::flow::NO_NODE,
        node => node_map[node as usize],
      });
    }
    pcs.push(self.here());
    for at in jumps {
      let target = self.code[at].jump_target().expect("a jump");
      self.code[at].retarget(pcs[target as usize]);
    }
    // Each restore ends the slot's value; a failure inside a block ends it
    // with the flow.
    if let Some(Binding::Local(index)) = saved {
      self
        .released_slots
        .push(u32::try_from(index).expect("slot fits u32"));
    }
    self.inlined.push((start, self.node_base() - start));
  }

  /// Ends the value of a hidden slot holding a value of type `ty` here
  /// (`Op::Clear`; the accumulator keeps it if it reads the slot), unless
  /// the value owns no heap storage.
  fn release(&mut self, slot: Binding, ty: Type) {
    if ty.is_scalar() {
      return;
    }
    let Binding::Local(index) = slot else {
      unreachable!("hidden slots are locals")
    };
    // The accumulator is not this slot's value in general: no output check.
    self.op(crate::inline::Op::Clear(slot), Type::any());
    self
      .released_slots
      .push(u32::try_from(index).expect("slot fits u32"));
  }

  fn node_base(&self) -> u32 {
    u32::try_from(self.nodes.len()).expect("node count fits u32")
  }

  /// Appends `flow`'s code; its origins hang below the current shard,
  /// through `prefix` (the parameter holding it, or the function inlined).
  fn append_rebased(&mut self, flow: &CompiledFlow, slots: usize, prefix: &[OriginStep]) {
    if self.record {
      let mut root = self.current;
      for step in prefix {
        root = self.step(step.clone(), root);
      }
      let steps = u32::try_from(self.steps.len()).expect("origin count fits u32");
      let rebase = |at: u32| if at == NO_ORIGIN { root } else { at + steps };
      match &flow.origins {
        Some(origins) => {
          self.steps.extend(
            origins
              .steps
              .iter()
              .map(|(step, parent)| (step.clone(), rebase(*parent))),
          );
          self.origins.extend(origins.at.iter().map(|at| rebase(*at)));
        }
        // Composed without origins: located at the shard that brought it.
        None => self.origins.extend(flow.code.iter().map(|_| root)),
      }
    }
    let base = self.here();
    let node_base = self.node_base();
    for (instruction, node) in flow.code.iter().zip(&flow.pc_nodes) {
      let mut instruction = instruction.clone();
      if slots != 0 {
        instruction.rebase_locals(slots);
      }
      instruction.relocate(base);
      self.code.push(instruction);
      self.pc_nodes.push(if *node == crate::flow::NO_NODE {
        crate::flow::NO_NODE
      } else {
        node + node_base
      });
    }
    self.nodes.extend(flow.nodes.iter().cloned());
  }
}

/// A dependency read during compose. Revalidated on every cache lookup.
#[derive(Clone, Debug, PartialEq)]
pub enum Dep {
  /// A mesh variable, or its absence (`found: None`).
  MeshVar { name: String, found: Option<Slot> },
  /// A wire definition referenced by name (`Spawn`), or its absence.
  Wire {
    name: String,
    def: Option<Arc<WireDef>>,
  },
  /// A function definition called by name, or its absence.
  Function {
    name: String,
    def: Option<Arc<FunctionDef>>,
  },
  /// A function whose body was inlined into this code (`ComposeCtx::flatten`),
  /// so an edit to it changes this body too: unlike `Function`, a body
  /// comparison counts it, and a wire holding it is swapped on reload. A
  /// function a compose-time evaluation reached is recorded the same way:
  /// its result is part of this code.
  Inlined {
    name: String,
    def: Option<Arc<FunctionDef>>,
  },
  /// What compose-time evaluations in this code used: valid only under
  /// limits that cover it (docs/metaprogramming.md §2.3).
  Evaluation(crate::compose_time::EvalUsage),
}

/// What compose may read besides parameters and input type.
pub struct ComposeEnv<'a> {
  pub mesh_layout: &'a FrameLayout,
  pub wires: &'a HashMap<String, WireDef>,
  pub functions: &'a HashMap<String, FunctionDef>,
  /// The budgets of compose-time evaluations.
  pub eval: crate::compose_time::EvalLimits,
}

impl Dep {
  pub(crate) fn still_valid(&self, env: &ComposeEnv<'_>) -> bool {
    match self {
      Dep::MeshVar { name, found } => env.mesh_layout.lookup(name) == *found,
      Dep::Wire { name, def } => env.wires.get(name) == def.as_deref(),
      Dep::Function { name, def } | Dep::Inlined { name, def } => {
        env.functions.get(name) == def.as_deref()
      }
      Dep::Evaluation(usage) => usage.fits(&env.eval),
    }
  }
}

/// What the flow being composed belongs to: a wire (which sees the whole
/// mesh) or a function body (which sees only what its signature declares,
/// golden path §3.2).
#[derive(Clone)]
pub(crate) enum Owner {
  Wire,
  Function(Arc<FunctionDef>),
  /// A pipeline evaluated at compose time (`#( ... )`): it sees only what
  /// it declares itself, no runtime value.
  Eval,
}

/// A compiled wire: shared, immutable, and free of per-instance data.
pub struct CompiledWire {
  pub name: String,
  pub looped: bool,
  pub input: Type,
  pub flow: CompiledFlow,
  /// Layout of each instance's local frame.
  pub locals: FrameLayout,
  /// Everything this compose read, for revalidation (e.g. by another mesh).
  pub deps: Vec<Dep>,
  pub(crate) definition: Arc<WireDef>,
  pub(crate) restart_deps: Vec<Dep>,
  /// The function bodies this wire's call sites (and their callees) were
  /// composed against, for reload admission.
  pub(crate) functions: FunctionRegistry,
  /// `Keep` slots: the locals that survive a root iteration (golden path
  /// §3.4); every other local starts the next iteration unset.
  pub keeps: Vec<crate::function::KeepSlot>,
}

impl CompiledWire {
  /// Whether a local slot survives between root iterations.
  pub fn persistent(&self, slot: usize) -> bool {
    self.keeps.iter().any(|k| k.slot == slot)
  }

  /// An instance's local frame at its start: every slot unset, except
  /// `Keep` slots at their initial values.
  pub fn fresh_locals(&self) -> Vec<Var> {
    let mut locals = vec![Var::None; self.locals.len()];
    for keep in &self.keeps {
      locals[keep.slot] = keep.initial.clone();
    }
    locals
  }
}

impl CompiledWire {
  /// A process signature with exact composed types and inferred mesh access.
  pub fn signature(&self) -> crate::signature::Signature<'_> {
    use crate::signature::{Lifetime, Signature, SignatureInput, SignatureOutput};
    Signature {
      name: self.name.as_str().into(),
      revision: 1,
      input: SignatureInput::Type(self.input),
      output: SignatureOutput::Type(self.flow.output),
      params: Some(Vec::new()),
      lifetime: Lifetime::Stateful,
      effects: self.flow.analysis.effects,
      uses: self.flow.analysis.uses.clone(),
      mutates: self.flow.analysis.mutates.clone(),
      source: None,
      summary: "".into(),
      help: "".into(),
    }
  }

  pub fn deps_valid(&self, env: &ComposeEnv<'_>) -> bool {
    self.deps.iter().all(|d| d.still_valid(env))
  }
}

/// A nested flow or referenced wire whose compose failed: recorded when the
/// error leaves it, so the step for the shard that composed it can name the
/// parameter holding it (diagnostic occurrence paths).
pub(crate) enum Child {
  /// A flow, by identity (the slice passed to `compose_flow`).
  Flow(*const ShardDef),
  Wire(String),
}

/// The context a shard's `compose` receives.
pub struct ComposeCtx<'a> {
  analysis: Analysis,
  input: Type,
  locals: FrameLayout,
  env: &'a ComposeEnv<'a>,
  cache: &'a mut ComposeCache,
  composing: &'a mut Vec<String>,
  deps: Vec<Dep>,
  restart_deps: Vec<Dep>,
  functions: FunctionRegistry,
  // Structural declaration origins for reload diagnostics, never source spans.
  diagnostic_path: Vec<PathStep>,
  local_paths: Vec<Vec<PathStep>>,
  current_args: Option<Arc<Args>>,
  /// Definite initialization of each local slot at the current compose point.
  initialized: Vec<bool>,
  /// The child flow or wire the last error came out of.
  failed_child: Option<Child>,
  /// How many flows enclose the one being composed, counting wires
  /// composed for `Spawn` and function bodies ([`MAX_FLOW_DEPTH`]).
  depth: usize,
  /// How many blocks (nested flows of shards) enclose the shard being
  /// composed within its wire body; 0 at a wire's top level.
  blocks: usize,
  owner: Owner,
  /// `Keep` slots declared so far (a stateful function's or a wire's
  /// persistent state).
  keeps: Vec<KeepSlot>,
  /// Names called lazily in this body or its callees (see
  /// `CompiledFunction::lazy_refs`).
  lazy_refs: Vec<String>,
  /// `CompiledFunction::inline_depth` of the body being composed.
  inline_depth: u8,
  /// How many blocks passed to a call (flow arguments) enclose the shard
  /// being composed: a `Return` there would leave the block
  /// (`control-in-flow`), and `Once` would not persist.
  flow_args: usize,
  /// The block being composed for a call's flow parameter, with the
  /// parameter's name: what a diagnostic path names for it (a call has no
  /// decoded `Args` to find it in).
  block_param: Option<(*const ShardDef, Arc<str>)>,
}

/// How deeply flows may nest, counting function bodies and wires composed
/// for `Spawn`. Compose recurses once per level: past this, it reports
/// `too-deep` instead of letting the stack overflow. Real scripts stay far
/// below. Activation does not recurse (golden path §6.3); its separate
/// limit is `Mesh::set_max_call_depth`. A level of function compose takes
/// about 6 KiB of native stack, so ESP-IDF's 128 KiB main task gets half the
/// limit, like its call depth.
pub const MAX_FLOW_DEPTH: usize = if cfg!(target_os = "espidf") { 24 } else { 48 };

impl ComposeCtx<'_> {
  /// The input type of the shard being composed.
  pub fn input(&self) -> Type {
    self.input
  }

  /// Resolves a variable: the local frame first, then the mesh frame. Mesh
  /// lookups are recorded as dependencies, including absences. Inside a
  /// function body only mesh variables declared in `uses` or `mutates` are
  /// visible; there is no fallback from an unknown local to the mesh.
  pub fn var(&mut self, name: &str) -> Option<VarInfo> {
    if let Some(slot) = self.locals.lookup(name) {
      return Some(VarInfo {
        binding: Binding::Local(slot.index),
        ty: slot.ty,
        mutable: slot.mutable,
        initialized: self.initialized[slot.index],
      });
    }
    match &self.owner {
      Owner::Function(def) if !def.declares(name) => return None,
      Owner::Eval => return None,
      _ => {}
    }
    let found = self.env.mesh_layout.lookup(name);
    let dep = Dep::MeshVar {
      name: name.to_string(),
      found,
    };
    self.deps.push(dep.clone());
    self.restart_deps.push(dep);
    found.map(|slot| VarInfo {
      binding: Binding::Mesh(slot.index),
      ty: slot.ty,
      mutable: slot.mutable,
      initialized: true,
    })
  }

  /// Variable names visible here, locals and mesh variables, sorted. For
  /// suggestions; it records no dependencies.
  pub fn visible_names(&self) -> Vec<String> {
    let mut names = self.locals.names();
    for name in self.env.mesh_layout.names() {
      let declared = match &self.owner {
        Owner::Wire => true,
        Owner::Function(def) => def.declares(&name),
        Owner::Eval => false,
      };
      if declared && !names.contains(&name) {
        names.push(name);
      }
    }
    names.sort();
    names
  }

  /// The declared output type when composing a function body: what
  /// `Return` must produce. `None` in a wire.
  pub fn return_type(&self) -> Option<Type> {
    match &self.owner {
      Owner::Wire | Owner::Eval => None,
      Owner::Function(def) => Some(def.output),
    }
  }

  /// Whether `Keep` and `Once` may declare persistent state here: in a
  /// wire, or in a function declared `stateful: true`.
  pub fn allows_persistent_state(&self) -> bool {
    match &self.owner {
      // Composed, then refused as not compose-time with the other state.
      Owner::Wire | Owner::Eval => true,
      Owner::Function(def) => def.stateful,
    }
  }

  /// Whether the flow being composed is evaluated at compose time
  /// (`#( ... )`).
  pub fn evaluating(&self) -> bool {
    matches!(self.owner, Owner::Eval)
  }

  /// The flow parameter `name` of the function being composed: its index
  /// among the function's flow parameters, and its type.
  pub fn flow_param(&self, name: &str) -> Option<(usize, FlowType)> {
    match &self.owner {
      Owner::Function(def) => def.flow_param_index(name),
      Owner::Wire | Owner::Eval => None,
    }
  }

  /// Whether the shard being composed is inside a block passed to a call
  /// (docs/metaprogramming.md §3.1): it runs inside the callee.
  pub fn in_flow_argument(&self) -> bool {
    self.flow_args > 0
  }

  /// `flow-escapes`: a flow parameter used other than run or passed on.
  pub(crate) fn flow_escapes(&self, name: &str, shard: &str) -> Option<Error> {
    self.flow_param(name)?;
    Some(Error::Diagnostic(Box::new(
      compose_diagnostic(
        "flow-escapes",
        format!(
          "{name} is a flow parameter: it can only be run (`Run({name})`) or passed on as a flow argument to another call, not read, bound, assigned, returned or stored"
        ),
      )
      .shard(shard),
    )))
  }

  /// The function being composed, if any (for messages).
  pub fn function_name(&self) -> Option<&str> {
    match &self.owner {
      Owner::Wire | Owner::Eval => None,
      Owner::Function(def) => Some(&def.name),
    }
  }

  /// Whether the shard being composed is at its wire body's top level,
  /// outside every branch and loop.
  pub fn at_top_level(&self) -> bool {
    self.blocks == 0
  }

  /// Where a local was declared: the occurrence path of the declaring
  /// shard, from its wire.
  pub fn declaration_path(&self, binding: Binding) -> Option<Vec<PathStep>> {
    match binding {
      Binding::Local(i) => self.local_paths.get(i).cloned(),
      Binding::Mesh(_) => None,
    }
  }

  /// Resolves a variable that is about to be read. Fails if it is unknown, or
  /// not definitely initialized here (e.g. only assigned in a `When` body or
  /// a loop body that might not run).
  /// Errors are structured diagnostics (`unknown-variable`,
  /// `possibly-uninitialized`); callers that read a parameter add it with
  /// [`crate::Error::with_param`].
  pub fn read_var(&mut self, name: &str, shard: &str) -> Result<VarInfo> {
    let variable_error = |code: &'static str, message: String| {
      Err(crate::Error::Diagnostic(Box::new(
        crate::diagnostic::Diagnostic::new(
          crate::diagnostic::Phase::Compose,
          "compose-error",
          code,
          message,
        )
        .shard(shard),
      )))
    };
    if let Some(err) = self.flow_escapes(name, shard) {
      return Err(err);
    }
    match self.var(name) {
      None => {
        if let Owner::Function(def) = &self.owner
          && self.env.mesh_layout.lookup(name).is_some()
        {
          return variable_error(
            "undeclared-mesh-access",
            format!(
              "{name} is a mesh variable that {} does not declare; add `uses: [{name}]` to read it",
              def.name
            ),
          );
        }
        let near = crate::diagnostic::closest(name, self.visible_names(), 3);
        let message = if self.evaluating() {
          format!(
            "unknown variable {name}; `#( )` runs at compose time and cannot see runtime values (locals, parameters, mesh variables)"
          )
        } else {
          format!("unknown variable {name}")
        };
        variable_error("unknown-variable", message).map_err(|e: crate::Error| match e {
          crate::Error::Diagnostic(mut d) => {
            d.did_you_mean = near;
            crate::Error::Diagnostic(d)
          }
          other => other,
        })
      }
      Some(info) if !info.initialized => variable_error(
        "possibly-uninitialized",
        format!(
          "{name} may be uninitialized here (it is only assigned in a branch or loop body that might not run)"
        ),
      ),
      Some(info) => {
        if matches!(info.binding, Binding::Mesh(_)) {
          if let Owner::Function(def) = &self.owner
            && !def.may_read(name)
          {
            return variable_error(
              "undeclared-mesh-access",
              format!(
                "{name} is declared in `mutates` only; reading it needs `uses: [{name}]` as well"
              ),
            );
          }
          self.analysis.access(name, info.ty, false);
        }
        Ok(info)
      }
    }
  }

  /// Declares a new local variable in the wire's frame. The declaring shard
  /// assigns it, so it is initialized from this point on.
  pub fn declare_local(&mut self, name: &str, ty: Type, mutable: bool) -> VarInfo {
    let slot = self.locals.declare(name, ty, mutable);
    self.initialized.push(true);
    self.local_paths.push(self.diagnostic_path.clone());
    VarInfo {
      binding: Binding::Local(slot.index),
      ty,
      mutable,
      initialized: true,
    }
  }

  /// A frame slot without a name, for the flat code of a composite (its
  /// saved input, a loop counter). Cleared like any ordinary local.
  fn declare_hidden(&mut self, ty: Type) -> usize {
    let slot = self.locals.declare_hidden(ty);
    self.initialized.push(true);
    self.local_paths.push(self.diagnostic_path.clone());
    slot.index
  }

  /// Lowers a `Repeat`, `While`, `When` or `If` to flat code in the parent
  /// flow (its children's nodes and instructions, with jumps and hidden
  /// slots for its own control), and a call to a small stateless
  /// straight-line body to that body's code on hidden slots of the
  /// caller's frame (arguments bound by instructions, the input through
  /// the accumulator; the callee becomes a dependency of this body, see
  /// `Dep::Inlined`). An iteration or a branch is then a jump
  /// inside one VM run instead of a frame entered and completed through
  /// the engine, and the composite needs no frames of its own. Children
  /// that must suspend keep their nodes: the engine activates them where
  /// the run stops and continues the flat code after them. `false` for
  /// any other node, which stays a node of its own.
  fn flatten(&mut self, node: &Arc<dyn CompiledNode>, input: Type, flat: &mut Flat) -> bool {
    use crate::inline::Op;
    let Some(control) = node.control() else {
      return false;
    };
    match control {
      Control::When(p) | Control::While(p) => {
        let looping = matches!(control, Control::While(_));
        let saved = Binding::Local(self.declare_hidden(input));
        flat.op(Op::Set(saved), input);
        let top = flat.here();
        flat.append(&p.pred);
        let exit = flat.op(Op::JumpIfNot(0), Type::any());
        flat.op(Op::get(saved), input);
        flat.append(&p.body);
        if looping {
          flat.op(Op::get(saved), input);
          flat.op(Op::Jump(top), Type::any());
        }
        flat.target(exit);
        flat.op(Op::get(saved), input);
        flat.release(saved, input);
      }
      Control::Repeat(r) => {
        let saved = Binding::Local(self.declare_hidden(input));
        flat.op(Op::Set(saved), input);
        let counter = r.times.as_ref().map(|times| {
          let counter = Binding::Local(self.declare_hidden(Type::int()));
          match times {
            Operand::Const(v) => flat.op(Op::Const(v.clone()), Type::any()),
            Operand::Bound(b) => flat.op(Op::get(*b), Type::any()),
          };
          flat.op(Op::Set(counter), Type::any());
          counter
        });
        let top = flat.here();
        let mut exits = Vec::new();
        if let Some(counter) = counter {
          exits.push(flat.op(Op::LoopTest(counter, 0), Type::any()));
        }
        if let Some(until) = &r.until {
          flat.op(Op::get(saved), input);
          flat.append(until);
          exits.push(flat.op(Op::JumpIf(0), Type::any()));
        }
        flat.op(Op::get(saved), input);
        flat.append(&r.body);
        flat.op(Op::Jump(top), Type::any());
        for exit in exits {
          flat.target(exit);
        }
        flat.op(Op::get(saved), input);
        flat.release(saved, input);
      }
      Control::Call(c) => {
        let Ok((body, blocks)) = inline_plan(c) else {
          return false;
        };
        self.inline_depth = self.inline_depth.max(body.inline_depth + 1);
        let fdef = body.def.clone();
        let base = self.declare_hidden(Type::any());
        for _ in 1..body.locals.len() {
          self.declare_hidden(Type::any());
        }
        let input_slot = Binding::Local(base + body.input_slot);
        let reads_input = body
          .flow
          .code
          .iter()
          .any(|i| i.mentions_local(body.input_slot));
        // The input is saved when the body reads it by name, or when
        // binding the arguments clobbers the accumulator; otherwise it
        // flows straight into the body.
        if !fdef.ignores_input() && (reads_input || !c.args.is_empty()) {
          flat.op(Op::Set(input_slot), input);
        }
        for (slot, arg) in body.param_slots.iter().zip(&c.args) {
          match arg {
            Operand::Const(v) => flat.op(Op::Const(v.clone()), Type::any()),
            Operand::Bound(b) => flat.op(Op::get(*b), Type::any()),
          };
          flat.op(Op::Set(Binding::Local(base + slot)), Type::any());
        }
        if fdef.ignores_input() {
          flat.op(Op::Const(Var::None), Type::none());
        } else if !c.args.is_empty() {
          flat.op(Op::get(input_slot), input);
        }
        if blocks.is_empty() {
          flat.inline(&body.flow, base, || Arc::from(fdef.name.as_str()));
        } else {
          // A slot for the input of a `Run` that passes it on, unless
          // what follows each such `Run` replaces it anyway.
          let passthrough = body
            .flow
            .code
            .iter()
            .zip(&body.flow.pc_nodes)
            .enumerate()
            .any(|(pc, (_, node))| {
              matches!(
                body.flow.nodes.get(*node as usize).and_then(|n| n.control()),
                Some(Control::Run(run)) if run.passthrough
              ) && !body
                .flow
                .code
                .get(pc + 1)
                .is_some_and(crate::inline::Instruction::ignores_accumulator)
            });
          let saved = passthrough.then(|| Binding::Local(self.declare_hidden(Type::any())));
          flat.inline_blocks(
            &body.flow,
            base,
            || Arc::from(fdef.name.as_str()),
            &blocks,
            saved,
          );
        }
        // The call's values end with it: every slot it used that may hold
        // a heap value is cleared, unless the callee's own code already
        // ended it last (a call inlined into the callee, whose slots then
        // count here for a failure in the middle of it).
        let mut last_use: Vec<Option<bool>> = vec![None; body.locals.len()];
        if !fdef.ignores_input() && (reads_input || !c.args.is_empty()) {
          last_use[body.input_slot] = Some(false);
        }
        for slot in body.param_slots.iter().take(c.args.len()) {
          last_use[*slot] = Some(false);
        }
        for instruction in &body.flow.code {
          let clears = matches!(instruction.op, Op::Clear(_));
          instruction.visit_locals(&mut |slot| last_use[slot] = Some(clears));
        }
        for (slot, used) in last_use.iter().enumerate() {
          if *used == Some(false) {
            flat.release(Binding::Local(base + slot), body.locals.slot_type(slot));
          }
        }
        flat.released_slots.extend(
          body
            .flow
            .released_slots()
            .iter()
            .map(|slot| slot + u32::try_from(base).expect("slot fits u32")),
        );
        let dep = Dep::Inlined {
          name: fdef.name.clone(),
          def: Some(fdef),
        };
        // The callee's own dependencies already came with the call
        // (`compose_call` propagates them for cache validation); what a
        // reload must also see is which functions this code embeds: the
        // callee, and whatever it embedded in turn.
        self.deps.push(dep.clone());
        self.restart_deps.push(dep);
        self.restart_deps.extend(
          body
            .deps
            .iter()
            .filter(|d| matches!(d, Dep::Inlined { .. }))
            .cloned(),
        );
      }
      Control::If(i) => {
        let saved = Binding::Local(self.declare_hidden(input));
        flat.op(Op::Set(saved), input);
        flat.append(&i.flows[0]);
        let to_else = flat.op(Op::JumpIfNot(0), Type::any());
        flat.op(Op::get(saved), input);
        flat.append(&i.flows[1]);
        if i.passthrough {
          flat.op(Op::get(saved), input);
        }
        let to_end = flat.op(Op::Jump(0), Type::any());
        flat.target(to_else);
        flat.op(Op::get(saved), input);
        if let Some(els) = i.flows.get(2) {
          flat.append(els);
          if i.passthrough {
            flat.op(Op::get(saved), input);
          }
        }
        flat.target(to_end);
        flat.release(saved, input);
      }
      _ => return false,
    }
    true
  }

  /// Records that the shard being composed assigns this variable. Inside a
  /// function body a mesh variable may be assigned only when declared in
  /// `mutates` (`undeclared-mesh-access` otherwise; the caller names the
  /// shard with [`crate::Error::in_shard`]).
  pub fn mark_initialized(&mut self, binding: Binding) -> Result<()> {
    if let Binding::Mesh(i) = binding {
      let (name, slot) = &self.env.mesh_layout.slots[i];
      if let Owner::Function(def) = &self.owner
        && !def.may_write(name)
      {
        return Err(plain_error(
          "undeclared-mesh-access",
          format!(
            "{name} is a mesh variable that {} may not assign; add `mutates: [{name}]` to write it",
            def.name
          ),
        ));
      }
      self.analysis.access(name, slot.ty, true);
    }
    if let Binding::Local(i) = binding {
      self.initialized[i] = true;
    }
    Ok(())
  }

  /// Declares a `Keep` slot: a mutable local that a stateful owner retains
  /// between invocations (and across reloads, by name and type).
  pub fn declare_keep(&mut self, name: &str, initial: Var) -> VarInfo {
    let ty = initial.type_of();
    let info = self.declare_local(name, ty, true);
    if let Binding::Local(slot) = info.binding {
      self.keeps.push(KeepSlot {
        name: name.to_string(),
        slot,
        ty,
        initial,
      });
    }
    info
  }

  /// Like [`ComposeCtx::compose_flow`], for a flow that might not run (a
  /// branch, or a loop body that can run zero times). Variables it declares
  /// or assigns are not definitely initialized after it.
  pub fn compose_flow_conditional(
    &mut self,
    flow: &[ShardDef],
    input: Type,
  ) -> Result<CompiledFlow> {
    let before = self.initialized.clone();
    let result = self.compose_flow(flow, input);
    for (i, init) in self.initialized.iter_mut().enumerate() {
      *init = before.get(i).copied().unwrap_or(false);
    }
    result
  }

  /// Runs `f` as a region that might not run, as a whole: inside it,
  /// assignments are seen by what follows (Repeat's Until, then its
  /// Action); after it, nothing it assigned is definitely assigned.
  pub fn conditional_region<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
    let before = self.initialized.clone();
    let result = f(self);
    for (i, init) in self.initialized.iter_mut().enumerate() {
      *init = before.get(i).copied().unwrap_or(false);
    }
    result
  }

  /// The parameter holding a nested flow or wire (and the item, for a case
  /// or a variadic argument): the decoded arguments of the shard being
  /// composed, or the call whose block this is.
  fn held(&self, child: &Child) -> Option<(std::borrow::Cow<'static, str>, Option<usize>)> {
    if let Some((param, item)) = self.current_args.as_ref().and_then(|a| a.param_of(child)) {
      return Some((param.into(), item));
    }
    match (child, &self.block_param) {
      (Child::Flow(ptr), Some((block, name))) if std::ptr::eq(*ptr, *block) => {
        Some((name.to_string().into(), None))
      }
      _ => None,
    }
  }

  fn child_prefix(&self, child: &Child) -> Vec<PathStep> {
    let mut path = Vec::new();
    if let Some((param, item)) = self.held(child) {
      path.push(PathStep::Param(param.into()));
      if let Some(item) = item {
        path.push(PathStep::Item(item));
      }
    }
    if let Child::Wire(name) = child {
      path.push(PathStep::Wire(name.clone()));
    }
    path
  }

  /// Composes a nested flow (e.g. a `When` body) with the given input type,
  /// into the same local frame. The flow is a block: names it declares are
  /// not visible after it (golden-path.md §3.2).
  pub fn compose_flow(&mut self, flow: &[ShardDef], input: Type) -> Result<CompiledFlow> {
    let child = Child::Flow(flow.as_ptr());
    let visible = self.locals.visible();
    self.blocks += 1;
    let result = self.compose_flow_unscoped(flow, input);
    self.blocks -= 1;
    self.locals.restore_visible(visible);
    if let Ok(flow) = &result {
      let prefix = self.child_prefix(&child);
      self.analysis.include(&flow.analysis, &prefix);
    }
    result
  }

  /// Composes a flow whose declarations stay visible after it: a wire or
  /// function body.
  fn compose_flow_unscoped(&mut self, flow: &[ShardDef], input: Type) -> Result<CompiledFlow> {
    if self.depth >= MAX_FLOW_DEPTH {
      return Err(plain_error(
        "too-deep",
        format!(
          "flows nest more than {MAX_FLOW_DEPTH} levels deep (nested flows, function bodies and wires run through Spawn count)"
        ),
      ));
    }
    let path_len = self.diagnostic_path.len();
    let held = self.held(&Child::Flow(flow.as_ptr()));
    if let Some((param, item)) = &held {
      self
        .diagnostic_path
        .push(PathStep::Param(param.to_string()));
      if let Some(item) = item {
        self.diagnostic_path.push(PathStep::Item(*item));
      }
    }
    self.depth += 1;
    let result = self.compose_flow_at_depth(flow, input);
    self.depth -= 1;
    self.diagnostic_path.truncate(path_len);
    result.map(|mut compiled| {
      if let (Some((param, item)), Some(origins)) = (held, &mut compiled.origins) {
        let mut prefix = vec![OriginStep::Param(param)];
        prefix.extend(item.map(|i| OriginStep::Item(u32::try_from(i).expect("item fits u32"))));
        origins.prefix = prefix.into_boxed_slice();
      }
      compiled
    })
  }

  #[inline(never)]
  fn compose_flow_at_depth(&mut self, flow: &[ShardDef], input: Type) -> Result<CompiledFlow> {
    let saved = self.input;
    let mut analysis = Analysis::default();
    let mut flat = Flat {
      nodes: Vec::with_capacity(flow.len()),
      code: Vec::with_capacity(flow.len()),
      pc_nodes: Vec::with_capacity(flow.len()),
      inlined: Vec::new(),
      released_slots: Vec::new(),
      record: self.cache.record_origins,
      origins: Vec::new(),
      steps: Vec::new(),
      current: NO_ORIGIN,
    };
    let mut ty = input;
    // Once a shard never produces a value (`Stop`), the rest of the flow is
    // unreachable but still checked: the next shard gets a None input (it
    // receives nothing), later ones the types that really flow, and the
    // flow's output stays `Never`.
    let mut diverged = false;
    for (index, def) in flow.iter().enumerate() {
      // Arguments evaluated at compose time (`#( ... )`) become literals
      // first; the definition is copied only when it holds one.
      let evaluated;
      let def = if def
        .args
        .iter()
        .any(|a| matches!(a.value, ParamValue::Eval(_)))
      {
        evaluated = match self.evaluate_args(def) {
          Ok(def) => def,
          Err(err) => {
            self.input = saved;
            self.failed_child = Some(Child::Flow(flow.as_ptr()));
            return Err(err.prefix_path(PathStep::Shard {
              index,
              name: def.name().to_string(),
            }));
          }
        };
        &evaluated
      } else {
        def
      };
      self.input = if ty == Type::never() {
        Type::none()
      } else {
        ty
      };
      self.failed_child = None;
      self.cache.stats.shard_composes += 1;
      // Decode against the shard's declared parameters (the same
      // declarations its documentation is generated from), then compose.
      self.diagnostic_path.push(PathStep::Shard {
        index,
        name: def.name().into(),
      });
      let node_input = self.input;
      let parent_analysis = std::mem::replace(
        &mut self.analysis,
        Analysis {
          effects: def.ty.desc.effects,
          lifetime: def.ty.desc.lifetime,
          // A call is the body it reaches (included by `compose_call`).
          not_compose_time: (def.function.is_none() && !crate::compose_time::eligible(def.ty))
            .then(|| def.ty.name()),
          ..Analysis::default()
        },
      );
      let composed = match &def.function {
        Some(function) => self.compose_call(def, function),
        None => decode(&def.ty.desc, &def.args).and_then(|args| {
          check_input(def.ty, self.input)?;
          let args = Arc::new(args);
          let parent_args = self.current_args.replace(args.clone());
          let result = def.ty.compose_node(&args, self).map_err(|err| {
            // An error from a nested flow or wire: name the parameter
            // holding it.
            match self.failed_child.take().and_then(|c| args.param_of(&c)) {
              Some((param, item)) => {
                let err = match item {
                  Some(i) => err.prefix_path(PathStep::Item(i)),
                  None => err,
                };
                err.prefix_path(PathStep::Param(param.to_string()))
              }
              None => err,
            }
          });
          self.current_args = parent_args;
          result
        }),
      };
      self.diagnostic_path.pop();
      let mut node_analysis = std::mem::replace(&mut self.analysis, parent_analysis);
      let composed = match composed {
        Ok(c) => c,
        Err(err) => {
          self.input = saved;
          self.failed_child = Some(Child::Flow(flow.as_ptr()));
          let err = with_input_source(err, &flow[..index]);
          return Err(err.prefix_path(PathStep::Shard {
            index,
            name: def.name().to_string(),
          }));
        }
      };
      let call = match composed.compiled.control() {
        Some(Control::Call(c)) if self.cache.record_call_sites => Some(Box::new(call_site(c))),
        _ => None,
      };
      node_analysis.occurrences.insert(
        0,
        Occurrence {
          path: Vec::new(),
          input: node_input,
          output: composed.output,
          effects: node_analysis.effects,
          lifetime: node_analysis.lifetime,
          call,
        },
      );
      analysis.include(
        &node_analysis,
        &[PathStep::Shard {
          index,
          name: def.name().into(),
        }],
      );
      flat.shard(index, || match &def.function {
        Some(name) => OriginName::Call(name.clone()),
        None => OriginName::Shard(def.ty.name()),
      });
      if !self.flatten(&composed.compiled, node_input, &mut flat) {
        flat.node(composed.compiled, def.ty.name(), composed.output);
      }
      if composed.output == Type::never() {
        diverged = true;
      }
      ty = composed.output;
    }
    self.input = saved;
    let output = if diverged { Type::never() } else { ty };
    let Flat {
      nodes,
      mut code,
      pc_nodes,
      inlined,
      mut released_slots,
      record,
      origins,
      steps,
      ..
    } = flat;
    released_slots.sort_unstable();
    released_slots.dedup();
    crate::inline::lower_scratch_releases(&mut code);
    let leaf = crate::inline::leaf_code(&code);
    Ok(CompiledFlow {
      analysis,
      nodes,
      code,
      output,
      leaf,
      pc_nodes,
      lowered: (!inlined.is_empty() || !released_slots.is_empty()).then(|| {
        Box::new(crate::flow::Lowered {
          inlined,
          released_slots,
        })
      }),
      origins: record.then(|| {
        Box::new(crate::flow::Origins {
          at: origins.into_boxed_slice(),
          steps: steps.into_boxed_slice(),
          prefix: Box::default(),
        })
      }),
    })
  }

  fn wire_def(&mut self, name: &str) -> Option<Arc<WireDef>> {
    let def = self.env.wires.get(name).cloned().map(Arc::new);
    let dep = Dep::Wire {
      name: name.to_string(),
      def: def.clone(),
    };
    self.deps.push(dep.clone());
    self.restart_deps.push(dep);
    def
  }

  /// Compiles another wire on its own (`Spawn`), through the cache. Its
  /// dependencies become dependencies of the wire being composed.
  pub fn compose_wire(&mut self, name: &str, input: Type) -> Result<Arc<CompiledWire>> {
    let Some(def) = self.wire_def(name) else {
      return Err(wire_error("unknown-wire", format!("unknown wire: {name}")));
    };
    let wire = self
      .cache
      .get_or_compose(&def, input, self.env, self.composing, self.depth)
      .inspect_err(|err| {
        // Composing the wire failed inside it (its path starts at the
        // wire), as opposed to the reference itself failing.
        let inside = err
          .diagnostic()
          .is_some_and(|d| d.path.first() == Some(&PathStep::Wire(name.to_string())));
        if inside {
          self.failed_child = Some(Child::Wire(name.to_string()));
        }
      })?;
    self.analysis.include(
      &wire.flow.analysis,
      &self.child_prefix(&Child::Wire(name.into())),
    );
    self.deps.extend(wire.deps.iter().cloned());
    self.restart_deps.extend(wire.deps.iter().cloned());
    self
      .functions
      .extend(wire.functions.iter().map(|(k, v)| (k.clone(), v.clone())));
    Ok(wire)
  }

  /// Resolves a function definition by name, recording the dependency
  /// (including its absence) for cache validation only: a callee's edit
  /// never restarts its callers (golden path §11), reload admission decides.
  fn function_def(&mut self, name: &str) -> Option<Arc<FunctionDef>> {
    let def = self.env.functions.get(name).cloned().map(Arc::new);
    self.deps.push(Dep::Function {
      name: name.to_string(),
      def: def.clone(),
    });
    def
  }

  /// Composes a call site (golden path §3.3): validates the labels against
  /// the parameter list, resolves literal and variable arguments to
  /// operands read once at entry, composes the body once per input type
  /// (shared by every call site), and checks the caller's declared mesh
  /// access covers the callee's.
  /// Not inlined into the per-node loop: its temporaries would otherwise
  /// sit in every level's frame of the compose recursion (a nested body
  /// is composed from here), and the device's task stack is small.
  #[inline(never)]
  fn compose_call(
    &mut self,
    def: &ShardDef,
    name: &str,
  ) -> Result<Composed<Arc<dyn CompiledNode>>> {
    let Some(fdef) = self.function_def(name) else {
      let known: Vec<String> = self.env.functions.keys().cloned().collect();
      let mut d =
        compose_diagnostic("unknown-function", format!("unknown function {name}")).shard(name);
      d.did_you_mean = crate::diagnostic::closest(name, known, 3);
      return Err(Error::Diagnostic(Box::new(d)));
    };
    // A block starts fresh each time it runs (metaprogramming.md §3), so it
    // cannot own the instance a stateful callee keeps between calls, as it
    // cannot hold a `Once`.
    if fdef.stateful && self.in_flow_argument() {
      return Err(fn_error(
        name,
        "stateful-call-in-stateless",
        format!(
          "{name} is stateful, so each call site owns an instance of it, but a block passed to a call starts fresh each time it runs and cannot own one: call {name} outside the block, or make it stateless"
        ),
      ));
    }
    if fdef.stateful
      && let Owner::Function(caller) = &self.owner
      && !caller.stateful
    {
      return Err(fn_error(
        name,
        "stateful-call-in-stateless",
        format!(
          "{name} is stateful, so each call site owns an instance of it; {} is stateless and cannot: declare the caller `stateful: true`, or make {name} stateless",
          caller.name
        ),
      ));
    }
    if !fdef.ignores_input() && !fdef.input.accepts(self.input) {
      return Err(typed_error(
        "input-type-mismatch",
        "input-type-mismatch",
        format!("{name} needs {} input, got {}", fdef.input, self.input),
        name,
        None,
        self.input,
        vec![TypeRef::of(fdef.input)],
      ));
    }
    // Labels are validated against the parameter list first; then each
    // argument is a literal or a variable read once at entry, or, for a
    // flow parameter, a block composed here against this scope (or a flow
    // parameter of this function, passed on).
    enum Given {
      Value(Operand),
      Block(BlockArg),
    }
    let mut given: Vec<Option<Given>> = (0..fdef.params.len()).map(|_| None).collect();
    let mut seen_named = false;
    for (position, arg) in def.args.iter().enumerate() {
      let index = match &arg.name {
        Some(label) => {
          seen_named = true;
          match fdef.params.iter().position(|p| p.name == *label) {
            Some(index) => index,
            None => {
              let known: Vec<String> = fdef.params.iter().map(|p| p.name.clone()).collect();
              let mut d = compose_diagnostic(
                "unknown-argument",
                format!(
                  "{name} has no parameter {label} (parameters: {})",
                  known.join(", ")
                ),
              )
              .shard(name)
              .param(label, None);
              d.did_you_mean = crate::diagnostic::closest(label, known, 3);
              return Err(Error::Diagnostic(Box::new(d)));
            }
          }
        }
        None if seen_named => {
          return Err(fn_error(
            name,
            "positional-after-named",
            format!("{name}: positional argument {position} after a named one"),
          ));
        }
        None if position >= fdef.params.len() => {
          return Err(fn_error(
            name,
            "too-many-arguments",
            format!(
              "{name} takes at most {} arguments, got {}",
              fdef.params.len(),
              def.args.len()
            ),
          ));
        }
        None => position,
      };
      let param = &fdef.params[index];
      if given[index].is_some() {
        return Err(
          fn_error(
            name,
            "duplicate-argument",
            format!("{name}: {} given more than once", param.name),
          )
          .with_param(&param.name, index),
        );
      }
      if let Some(flow) = param.flow {
        let block = self.compose_block(name, &param.name, index, flow, &arg.value)?;
        given[index] = Some(Given::Block(block));
        continue;
      }
      let (operand, ty) = match &arg.value {
        ParamValue::Value(v) => (Operand::Const(v.clone().into_struct_tables()), v.type_of()),
        ParamValue::Var(var) => {
          let info = self
            .read_var(var, name)
            .map_err(|e| e.with_param(&param.name, index))?;
          (Operand::Bound(info.binding), info.ty)
        }
        other => {
          let form = match other {
            ParamValue::Wire(_) => "wire",
            ParamValue::Flow(_) => "flow",
            ParamValue::Cases(_) => "cases",
            _ => unreachable!(),
          };
          return Err(
            fn_error(
              name,
              "wrong-argument-form",
              format!(
                "{name}: {} takes a literal or a variable, got a {form}",
                param.name
              ),
            )
            .with_param(&param.name, index),
          );
        }
      };
      if !param.ty.accepts(ty) {
        let code = if matches!(arg.value, ParamValue::Value(_)) {
          "wrong-argument-type"
        } else {
          "wrong-variable-type"
        };
        return Err(typed_error(
          "compose-error",
          code,
          format!("{name}: {} must be {}, got {ty}", param.name, param.ty),
          name,
          Some((&param.name, index)),
          ty,
          vec![TypeRef::of(param.ty)],
        ));
      }
      given[index] = Some(Given::Value(operand));
    }
    let mut args = Vec::with_capacity(fdef.params.len());
    let mut blocks = Vec::new();
    for (index, (param, given)) in fdef.params.iter().zip(given).enumerate() {
      args.push(match (given, &param.default) {
        (Some(Given::Value(operand)), _) => operand,
        (Some(Given::Block(block)), _) => {
          blocks.push(block);
          continue;
        }
        (None, Some(default)) => Operand::Const(default.clone().into_struct_tables()),
        (None, None) => {
          return Err(
            fn_error(
              name,
              "missing-argument",
              format!("{name}: missing required parameter {}", param.name),
            )
            .with_param(&param.name, index),
          );
        }
      });
    }
    let input = if fdef.ignores_input() {
      Type::none()
    } else {
      self.input
    };
    // A compose-time evaluation that reaches a function whose body holds
    // it (directly or through other functions) would need that body first.
    if let Some(&floor) = self.cache.eval_floors.last()
      && self.composing[..floor].iter().any(|n| *n == fdef.name)
    {
      return Err(fn_error(
        name,
        "compose-time-cycle",
        format!(
          "{name} is evaluated at compose time from inside its own body (directly or through other functions), so its body would be needed to compose itself"
        ),
      ));
    }
    // A call to a function being composed (direct or mutual recursion,
    // golden path M7) composes against the declared signature: the call
    // site resolves the body at entry, and its effects are the recursive
    // group's, known after the group's first pass.
    if self.composing.iter().any(|n| *n == fdef.name) {
      if fdef.stateful {
        return Err(fn_error(
          name,
          "recursive-stateful",
          format!(
            "{name} is stateful and calls itself (directly or through other functions): a stateful function owns one instance per call site and cannot re-enter it"
          ),
        ));
      }
      let group = self.cache.recursive_reference(name);
      if let Owner::Function(caller) = &self.owner {
        for access in &group.uses {
          if !caller.may_read(&access.name) {
            return Err(fn_error(
              name,
              "undeclared-mesh-access",
              format!(
                "{name} reads mesh variable {}; {} must declare it in `uses: [{}]` to call it",
                access.name, caller.name, access.name
              ),
            ));
          }
        }
        for access in &group.mutates {
          if !caller.may_write(&access.name) {
            return Err(fn_error(
              name,
              "undeclared-mesh-access",
              format!(
                "{name} assigns mesh variable {}; {} must declare it in `mutates: [{}]` to call it",
                access.name, caller.name, access.name
              ),
            ));
          }
        }
      }
      self
        .analysis
        .include(&group, &[PathStep::Function(name.to_string())]);
      self.analysis.lifetime = Lifetime::Stateless;
      self.note_lazy_ref(name);
      return Ok(Composed {
        compiled: crate::shard::erase::<crate::stackless::shards::Call>(
          CallCompiled {
            target: CallTarget::Lazy {
              key: FunctionKey {
                name: name.to_string(),
                input,
              },
              def: fdef,
            },
            args,
            blocks,
          },
          Lifetime::Stateless,
        ),
        output: fdef_output(&self.env.functions[name]),
      });
    }
    let body = self
      .cache
      .get_or_compose_function(&fdef, input, self.env, self.composing, self.depth)
      .map_err(|err| self.explain_caller_local(err, name))?;
    // Mesh access is declared, never granted by inference (§3.1): a caller
    // function must declare what its callees reach.
    if let Owner::Function(caller) = &self.owner {
      for access in &body.flow.analysis.uses {
        if !caller.may_read(&access.name) {
          return Err(fn_error(
            name,
            "undeclared-mesh-access",
            format!(
              "{name} reads mesh variable {}; {} must declare it in `uses: [{}]` to call it",
              access.name, caller.name, access.name
            ),
          ));
        }
      }
      for access in &body.flow.analysis.mutates {
        if !caller.may_write(&access.name) {
          return Err(fn_error(
            name,
            "undeclared-mesh-access",
            format!(
              "{name} assigns mesh variable {}; {} must declare it in `mutates: [{}]` to call it",
              access.name, caller.name, access.name
            ),
          ));
        }
      }
    }
    self.deps.extend(body.deps.iter().cloned());
    self
      .functions
      .extend(body.functions.iter().map(|(k, v)| (k.clone(), v.clone())));
    self.functions.insert(FunctionKey::of(&body), body.clone());
    self
      .analysis
      .include(&body.flow.analysis, &[PathStep::Function(name.to_string())]);
    // The call site's lifetime is the function's: a stateless function
    // keeps nothing between invocations, whatever its body holds inside one.
    let lifetime = if fdef.stateful {
      Lifetime::Stateful
    } else {
      Lifetime::Stateless
    };
    self.analysis.lifetime = lifetime;
    // A callee that reaches a function still being composed is inside the
    // caller's recursive group: the edge resolves through the group's
    // pinned table too, so a reload never mixes revisions along a chain.
    let in_group = body
      .lazy_refs
      .iter()
      .any(|n| self.composing.iter().any(|c| c == n));
    let target = if in_group {
      for n in &body.lazy_refs {
        self.note_lazy_ref(n);
      }
      CallTarget::Lazy {
        key: FunctionKey::of(&body),
        def: fdef.clone(),
      }
    } else {
      CallTarget::Direct(body)
    };
    Ok(Composed {
      compiled: crate::shard::erase::<crate::stackless::shards::Call>(
        CallCompiled {
          target,
          args,
          blocks,
        },
        lifetime,
      ),
      output: fdef.output,
    })
  }

  /// The argument of flow parameter `param` (of type `ty`, at `index`) of
  /// a call to `function`: a block, composed here against the caller's
  /// scope with the parameter's input type (it may run any number of
  /// times, so what it assigns is not definitely assigned after the call),
  /// or a flow parameter of the function being composed, passed on.
  fn compose_block(
    &mut self,
    function: &str,
    param: &str,
    index: usize,
    ty: FlowType,
    value: &ParamValue,
  ) -> Result<BlockArg> {
    match value {
      ParamValue::Flow(defs) => {
        let args = self.current_args.take();
        let held = self.block_param.replace((defs.as_ptr(), Arc::from(param)));
        self.flow_args += 1;
        let result = self.compose_flow_conditional(defs, ty.input);
        self.flow_args -= 1;
        self.block_param = held;
        self.current_args = args;
        let flow = result.map_err(|err| err.prefix_path(PathStep::Param(param.to_string())))?;
        // `output: None`: whatever the block outputs is discarded.
        if let Some(output) = ty.output
          && output != Type::none()
          && !output.accepts(flow.output)
        {
          return Err(typed_error(
            "compose-error",
            "output-type-mismatch",
            format!(
              "{function}: the block for {param} outputs {}, but {param} is declared `{ty}`",
              flow.output
            ),
            function,
            Some((param, index)),
            flow.output,
            vec![TypeRef::of(output)],
          ));
        }
        Ok(BlockArg::Flow(flow))
      }
      ParamValue::Var(name) => {
        let Some((outer_index, outer)) = self.flow_param(name) else {
          return Err(
            fn_error(
              function,
              "wrong-argument-form",
              format!(
                "{function}: {param} takes a block `{{...}}` or a flow parameter of the calling function, got the variable {name}"
              ),
            )
            .with_param(param, index),
          );
        };
        // Every value Run passes must suit the block, and the block's
        // output must suit what Run declares.
        let fits = outer.input.accepts(ty.input)
          && ty
            .output
            .is_none_or(|u| outer.output.is_some_and(|o| u.accepts(o)));
        if !fits {
          return Err(
            fn_error(
              function,
              "wrong-argument-type",
              format!("{function}: {param} is declared `{ty}`, but {name} is `{outer}`"),
            )
            .with_param(param, index),
          );
        }
        Ok(BlockArg::Forward(
          u32::try_from(outer_index).expect("flow parameter index fits u32"),
        ))
      }
      _ => Err(
        fn_error(
          function,
          "wrong-argument-form",
          format!("{function}: {param} takes a block `{{...}}`"),
        )
        .with_param(param, index),
      ),
    }
  }

  /// An `unknown-variable` inside a callee that names one of the caller's
  /// locals: say how to pass it (golden path §3.2, test B).
  fn note_lazy_ref(&mut self, name: &str) {
    if !self.lazy_refs.iter().any(|n| n == name) {
      self.lazy_refs.push(name.to_string());
    }
  }

  fn explain_caller_local(&self, err: Error, function: &str) -> Error {
    let Error::Diagnostic(mut d) = err else {
      return err;
    };
    if d.code == "unknown-variable"
      && let Some(rest) = d.message.strip_prefix("unknown variable ")
    {
      let var: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ';')
        .collect();
      if let Some(slot) = self.locals.lookup(&var) {
        d.message.push_str(&format!(
          "; `{var}` is a local of the caller, and a function sees only its input and parameters: pass it as a parameter (`params: {{{var}: {}}}` in the declaration, `{function}({var}: {var})` at the call)",
          slot.ty
        ));
      }
    }
    Error::Diagnostic(d)
  }
}

fn fdef_output(def: &FunctionDef) -> Type {
  def.output
}

fn compose_diagnostic(code: &'static str, message: String) -> Diagnostic {
  Diagnostic::new(Phase::Compose, "compose-error", code, message)
}

/// A structured compose error about function `name`.
///
/// Error constructors on the compose recursion path are cold and never
/// inlined: a `Diagnostic` is a few hundred bytes, and one stack slot per
/// error site in a function that recurses once per nesting level adds up
/// to the device's whole task stack.
#[cold]
#[inline(never)]
fn fn_error(name: &str, code: &'static str, message: String) -> Error {
  Error::Diagnostic(Box::new(compose_diagnostic(code, message).shard(name)))
}

/// A typed compose error (`input-type-mismatch`, `wrong-argument-type`,
/// `output-type-mismatch`): the actual type against the expected ones,
/// optionally at a parameter. Cold, see [`fn_error`].
#[cold]
#[inline(never)]
fn typed_error(
  kind: &'static str,
  code: &'static str,
  message: String,
  shard: &str,
  param: Option<(&str, usize)>,
  actual: Type,
  expected: Vec<TypeRef>,
) -> Error {
  let mut d = Diagnostic::new(Phase::Compose, kind, code, message).shard(shard);
  if let Some((name, index)) = param {
    d = d.param(name, Some(index));
  }
  Error::Diagnostic(Box::new(d.types(Some(TypeRef::of(actual)), expected)))
}

/// A plain compose error without a shard. Cold, see [`fn_error`].
#[cold]
#[inline(never)]
fn plain_error(code: &'static str, message: String) -> Error {
  Error::Diagnostic(Box::new(compose_diagnostic(code, message)))
}

/// The first occurrence in a body with the picked effect: the shard a
/// `not-pure` diagnostic names.
fn offending_shard(analysis: &Analysis, pick: fn(Effects) -> bool) -> String {
  analysis
    .occurrences
    .iter()
    .find(|o| pick(o.effects))
    .and_then(|o| match o.path.last() {
      Some(PathStep::Shard { name, .. }) => Some(name.clone()),
      _ => None,
    })
    .unwrap_or_else(|| "a shard".to_string())
}

/// Enforces a shard's declared input (`InputDesc::Types` or
/// `InputDesc::Typed`) before its compose runs, so no shard repeats the
/// check. `Any` and `Ignored` inputs are left to the shard.
fn check_input(ty: &ShardType, input: Type) -> Result<()> {
  use crate::describe::InputDesc;
  use crate::diagnostic::TypeRef;
  let (expected, refs) = match ty.desc.input {
    InputDesc::Types(types) => {
      if types.iter().any(|t| t.matches(input)) {
        return Ok(());
      }
      // "Int, Float or Float3".
      let names: Vec<&str> = types.iter().map(|t| t.name()).collect();
      let expected = match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => names.join(""),
      };
      (
        expected,
        types.iter().copied().map(TypeRef::named).collect(),
      )
    }
    InputDesc::Typed(full) => {
      let full = full();
      if full.accepts(input) {
        return Ok(());
      }
      (full.to_string(), vec![TypeRef::of(full)])
    }
    InputDesc::Any | InputDesc::Ignored => return Ok(()),
  };
  Err(typed_error(
    "input-type-mismatch",
    "input-type-mismatch",
    format!("{} needs {expected} input, got {input}", ty.name()),
    ty.name(),
    None,
    input,
    refs,
  ))
}

/// Adds where the input came from to a mismatch on the failing shard's own
/// input (not to errors from inside its nested flows, which already have
/// path steps). `before` are the shards before it in the same flow; those
/// that pass their input through are skipped to reach the producer.
fn with_input_source(err: crate::Error, before: &[ShardDef]) -> crate::Error {
  use crate::describe::OutputDesc;
  let crate::Error::Diagnostic(mut d) = err else {
    return err;
  };
  let about_input = matches!(d.kind, "input-type-mismatch") || d.code == "variable-type-mismatch";
  if !about_input || !d.path.is_empty() || d.input_from.is_some() {
    return crate::Error::Diagnostic(d);
  }
  let mut via = Vec::new();
  let mut origin = None;
  for (index, def) in before.iter().enumerate().rev() {
    if def.function.is_none() && matches!(def.ty.desc.output, OutputDesc::Passthrough) {
      via.push((index, def.name().to_string()));
    } else {
      origin = Some((index, def.name().to_string()));
      break;
    }
  }
  via.reverse();
  let source = crate::diagnostic::InputSource { origin, via };
  d.message = format!("{} ({})", d.message, source.describe());
  d.input_from = Some(source);
  crate::Error::Diagnostic(d)
}

/// A structured error about a referenced wire; the referencing shard adds
/// itself and its parameter.
fn wire_error(code: &'static str, message: String) -> crate::Error {
  crate::Error::Diagnostic(Box::new(crate::diagnostic::Diagnostic::new(
    crate::diagnostic::Phase::Compose,
    "compose-error",
    code,
    message,
  )))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
  /// Lookups that reused a cached compiled wire.
  pub hits: u64,
  /// Lookups that had to compose.
  pub misses: u64,
  /// Wires actually composed (equals `misses` unless compose failed).
  pub wire_composes: u64,
  /// Individual shard compose calls.
  pub shard_composes: u64,
  /// Function bodies actually composed (one per definition and input type
  /// while its dependencies hold).
  pub function_composes: u64,
  /// Compose-time evaluations run (`#( ... )`).
  pub evaluations: u64,
  /// Compose-time evaluations answered from the cache.
  pub evaluation_hits: u64,
}

struct Entry {
  def: WireDef,
  input: Type,
  compiled: Arc<CompiledWire>,
}

struct FunctionEntry {
  def: Arc<FunctionDef>,
  input: Type,
  compiled: Arc<CompiledFunction>,
}

/// A compose-time evaluation's result, by the pipeline evaluated: what it
/// read (revalidated like any compose) and what it used (checked against
/// the requesting context's limits).
struct EvalEntry {
  flow: Vec<ShardDef>,
  value: Var,
  deps: Vec<Dep>,
  usage: crate::compose_time::EvalUsage,
}

pub type HashFn = fn(&WireDef, Type) -> u64;

fn default_hash(def: &WireDef, input: Type) -> u64 {
  let mut hasher = DefaultHasher::new();
  def.hash(&mut hasher);
  input.hash(&mut hasher);
  hasher.finish()
}

/// A recursive group being composed (golden path M7): its root is the
/// function whose body was referenced while composing; the group's effects
/// and mesh access are the root's first-pass analysis, applied to every
/// recursive call site in the second pass.
struct RecursiveGroup {
  root: String,
  analysis: Analysis,
}

/// Compose cache with two-step lookup (contract §4).
pub struct ComposeCache {
  entries: HashMap<u64, Vec<Entry>>,
  functions: HashMap<u64, Vec<FunctionEntry>>,
  hash_fn: HashFn,
  pub stats: CacheStats,
  /// Functions referenced while being composed, in the current pass.
  recursive_refs: Vec<String>,
  group: Option<RecursiveGroup>,
  evaluations: HashMap<u64, Vec<EvalEntry>>,
  /// For each compose-time evaluation in progress, innermost last: how
  /// many names `composing` held when it started. A function among those
  /// holds the evaluation in its body (`compose-time-cycle`).
  eval_floors: Vec<usize>,
  /// Flows composed through this cache record where each instruction comes
  /// from (`CompiledFlow::origins`): a cache made to locate a failure.
  pub(crate) record_origins: bool,
  /// Call occurrences record how each call site runs (`Occurrence::call`),
  /// for `check --json`: off unless a check asks, so a device's compiled
  /// flows carry none of it (`Mesh::record_call_sites`).
  pub record_call_sites: bool,
}

impl Default for ComposeCache {
  fn default() -> ComposeCache {
    ComposeCache::with_hash_fn(default_hash)
  }
}

impl ComposeCache {
  /// A cache with a custom primary-key hash (tests force collisions with it).
  pub fn with_hash_fn(hash_fn: HashFn) -> ComposeCache {
    ComposeCache {
      entries: HashMap::new(),
      functions: HashMap::new(),
      hash_fn,
      stats: CacheStats::default(),
      recursive_refs: Vec::new(),
      group: None,
      evaluations: HashMap::new(),
      eval_floors: Vec::new(),
      record_origins: false,
      record_call_sites: false,
    }
  }

  /// A fresh cache whose flows record their origins, to compose a failed
  /// evaluation again and locate the failure (`compose/evaluate.rs`).
  pub(crate) fn locating() -> ComposeCache {
    ComposeCache {
      record_origins: true,
      ..ComposeCache::default()
    }
  }

  pub(crate) fn get_or_compose(
    &mut self,
    def: &WireDef,
    input: Type,
    env: &ComposeEnv<'_>,
    composing: &mut Vec<String>,
    depth: usize,
  ) -> Result<Arc<CompiledWire>> {
    let key = (self.hash_fn)(def, input);
    // Step 1: candidates by primary key (the hash only indexes; equality
    // decides). Step 2: revalidate each candidate's recorded dependencies.
    if let Some(candidates) = self.entries.get(&key) {
      for entry in candidates {
        if entry.def == *def && entry.input == input && entry.compiled.deps_valid(env) {
          self.stats.hits += 1;
          return Ok(entry.compiled.clone());
        }
      }
    }
    self.stats.misses += 1;

    if composing.contains(&def.name) {
      return Err(wire_error(
        "recursive-wire",
        format!("recursive wire reference: {}", def.name),
      ));
    }
    composing.push(def.name.clone());
    let result = {
      // Boxed for the same reason as the function compose context below.
      let mut ctx = Box::new(ComposeCtx {
        analysis: Analysis::default(),
        input,
        locals: FrameLayout::default(),
        env,
        cache: self,
        composing,
        deps: Vec::new(),
        restart_deps: Vec::new(),
        functions: HashMap::new(),
        diagnostic_path: vec![PathStep::Wire(def.name.clone())],
        local_paths: Vec::new(),
        current_args: None,
        initialized: Vec::new(),
        failed_child: None,
        depth,
        blocks: 0,
        owner: Owner::Wire,
        keeps: Vec::new(),
        lazy_refs: Vec::new(),
        inline_depth: 0,
        flow_args: 0,
        block_param: None,
      });
      ctx
        .compose_flow_unscoped(&def.flow, input)
        .map(|flow| {
          (
            flow,
            ctx.locals,
            ctx.deps,
            ctx.restart_deps,
            ctx.functions,
            ctx.keeps,
          )
        })
        .map_err(|err| err.prefix_path(PathStep::Wire(def.name.clone())))
    };
    composing.pop();
    let (flow, locals, deps, restart_deps, functions, keeps) = result?;

    self.stats.wire_composes += 1;
    let compiled = Arc::new(CompiledWire {
      name: def.name.clone(),
      looped: def.looped,
      input,
      flow,
      locals,
      deps,
      definition: Arc::new(def.clone()),
      restart_deps,
      functions,
      keeps,
    });
    self.entries.entry(key).or_default().push(Entry {
      def: def.clone(),
      input,
      compiled: compiled.clone(),
    });
    Ok(compiled)
  }

  /// A function body composed against its declared signature, once per
  /// input type (golden path §4): the frame holds the parameters, then
  /// `input`, then the body's locals. Runtime argument values never enter
  /// the key; the definition, the input type and the recorded dependencies
  /// do.
  pub(crate) fn get_or_compose_function(
    &mut self,
    def: &Arc<FunctionDef>,
    input: Type,
    env: &ComposeEnv<'_>,
    composing: &mut Vec<String>,
    depth: usize,
  ) -> Result<Arc<CompiledFunction>> {
    let key = {
      let mut hasher = DefaultHasher::new();
      def.hash(&mut hasher);
      input.hash(&mut hasher);
      hasher.finish()
    };
    if let Some(candidates) = self.functions.get(&key) {
      for entry in candidates {
        if entry.def == *def
          && entry.input == input
          && entry.compiled.deps.iter().all(|d| d.still_valid(env))
        {
          self.stats.hits += 1;
          return Ok(entry.compiled.clone());
        }
      }
    }
    self.stats.misses += 1;
    // A recursive reference while composing is handled by the call site
    // (`compose_call`); reaching here with the name in `composing` cannot
    // happen.
    debug_assert!(!composing.iter().any(|n| n == &def.name));
    composing.push(def.name.clone());
    let result = {
      // Boxed: compose recurses once per nested body, and the device's
      // task stack is small (the context is 400 bytes and lives for the
      // whole recursion below it).
      let mut ctx = Box::new(ComposeCtx {
        analysis: Analysis::default(),
        input,
        locals: FrameLayout::default(),
        env,
        cache: self,
        composing,
        deps: Vec::new(),
        restart_deps: Vec::new(),
        functions: HashMap::new(),
        diagnostic_path: vec![PathStep::Function(def.name.clone())],
        local_paths: Vec::new(),
        current_args: None,
        initialized: Vec::new(),
        failed_child: None,
        depth,
        blocks: 0,
        owner: Owner::Function(def.clone()),
        keeps: Vec::new(),
        lazy_refs: Vec::new(),
        inline_depth: 0,
        flow_args: 0,
        block_param: None,
      });
      let slot_of = |info: VarInfo| match info.binding {
        Binding::Local(i) => i,
        Binding::Mesh(_) => unreachable!("declared locally"),
      };
      let param_slots: Vec<usize> = def
        .params
        .iter()
        .filter(|p| !p.is_flow())
        .map(|p| slot_of(ctx.declare_local(&p.name, p.ty, false)))
        .collect();
      let input_slot = slot_of(ctx.declare_local("input", input, false));
      ctx
        .compose_flow_unscoped(&def.body, input)
        .and_then(|mut flow| {
          if def.output.accepts(flow.output) {
            Ok(flow)
          } else if def.output == Type::none() {
            // `output: None` discards the body's output, as it does a
            // block's (the authoring eval's most common compose error).
            flow.discard_output();
            Ok(flow)
          } else {
            Err(typed_error(
              "compose-error",
              "output-type-mismatch",
              format!(
                "{} declares output {} but its body outputs {}",
                def.name, def.output, flow.output
              ),
              &def.name,
              None,
              flow.output,
              vec![TypeRef::of(def.output)],
            ))
          }
        })
        .map(|flow| {
          (
            flow,
            ctx.locals,
            ctx.deps,
            ctx.keeps,
            ctx.functions,
            input_slot,
            param_slots,
            ctx.lazy_refs,
            ctx.inline_depth,
          )
        })
        .map_err(|err| err.prefix_path(PathStep::Function(def.name.clone())))
    };
    composing.pop();
    let (flow, locals, deps, keeps, functions, input_slot, param_slots, lazy_refs, inline_depth) =
      result?;
    if def.pure {
      let analysis = &flow.analysis;
      let effect = [
        (
          analysis.effects.suspends,
          "suspends",
          (|e: Effects| e.suspends) as fn(Effects) -> bool,
        ),
        (analysis.effects.io, "io", |e| e.io),
        (analysis.effects.time, "time", |e| e.time),
        (analysis.effects.random, "random", |e| e.random),
        (analysis.effects.unknown, "unknown", |e| e.unknown),
      ]
      .into_iter()
      .find(|(set, _, _)| *set);
      let reason = if def.stateful {
        Some("it is declared stateful".to_string())
      } else if let Some((_, label, pick)) = effect {
        Some(format!(
          "{} has the effect `{label}`",
          offending_shard(analysis, pick)
        ))
      } else if let Some(access) = analysis.uses.first() {
        Some(format!("it reads mesh variable {}", access.name))
      } else {
        analysis
          .mutates
          .first()
          .map(|access| format!("it assigns mesh variable {}", access.name))
      };
      if let Some(reason) = reason {
        return Err(
          fn_error(
            &def.name,
            "not-pure",
            format!("{} is declared pure, but {reason}", def.name),
          )
          .prefix_path(PathStep::Function(def.name.clone())),
        );
      }
    }
    self.stats.function_composes += 1;
    // The root of a recursive group: its first pass found the group's
    // effects; compose it again with every recursive call site carrying
    // them (a fixpoint after one more pass, since the union is finite).
    let referenced = self.recursive_refs.iter().any(|n| *n == def.name);
    if referenced && self.group.as_ref().is_none_or(|g| g.root != def.name) {
      let outer = self.group.replace(RecursiveGroup {
        root: def.name.clone(),
        analysis: flow.analysis.clone(),
      });
      self.recursive_refs.retain(|n| *n != def.name);
      let result = self.get_or_compose_function(def, input, env, composing, depth);
      self.group = outer;
      return result;
    }
    let native_state =
      flow.analysis.lifetime != Lifetime::Stateless || functions.values().any(|f| f.native_state);
    // A body that runs blocks needs its caller's frame for them: never a
    // VM call (it may be inlined with its blocks in place, `block_leaf`).
    let vm_only =
      !def.stateful && !def.has_flow_params() && crate::inline::straight_line(&flow.code);
    let vm_leaf = vm_only
      && !flow
        .code
        .iter()
        .any(|i| matches!(i.op, crate::inline::Op::VmCall));
    let block_leaf = !def.stateful
      && def.has_flow_params()
      && flow
        .code
        .iter()
        .zip(&flow.pc_nodes)
        .all(|(i, node)| match i.op {
          crate::inline::Op::Fallback => {
            matches!(flow.nodes[*node as usize].control(), Some(Control::Run(_)))
          }
          crate::inline::Op::VmCall => false,
          _ => true,
        });
    let compiled = Arc::new(CompiledFunction {
      def: def.clone(),
      input,
      native_state,
      vm_only,
      vm_leaf,
      block_leaf,
      lazy_refs,
      inline_depth,
      flow,
      locals,
      input_slot,
      param_slots,
      keeps,
      deps,
      functions,
    });
    // Bodies composed during a group's first pass carry provisional
    // effects at their recursive call sites: keep them out of the cache.
    if self.recursive_refs.is_empty() || self.group.is_some() {
      self.functions.entry(key).or_default().push(FunctionEntry {
        def: def.clone(),
        input,
        compiled: compiled.clone(),
      });
    }
    if self.group.as_ref().is_some_and(|g| g.root == def.name) {
      self.group = None;
      self.recursive_refs.retain(|n| *n != def.name);
    }
    Ok(compiled)
  }

  /// A call to a function being composed: records the reference and gives
  /// the analysis its call site carries (the group's, once known).
  pub(crate) fn recursive_reference(&mut self, name: &str) -> Analysis {
    if !self.recursive_refs.iter().any(|n| n == name) {
      self.recursive_refs.push(name.to_string());
    }
    match &self.group {
      Some(group) => Analysis {
        effects: group.analysis.effects,
        lifetime: Lifetime::Stateless,
        uses: group.analysis.uses.clone(),
        mutates: group.analysis.mutates.clone(),
        occurrences: Default::default(),
        not_compose_time: group.analysis.not_compose_time,
        // The group's own body locates its accesses.
        mesh_at: None,
      },
      None => Analysis::default(),
    }
  }
}
