//! A compiled flow: a sequence of compiled nodes. Wires, and shards with
//! sub-flows (`Once`, `When`, `Do`...), hold these; the engine
//! (`stackless/engine.rs`) owns and runs their per-instance state.

use std::sync::Arc;

use crate::diagnostic::PathStep;
use crate::shard::CompiledNode;
use crate::types::Type;

/// A compiled flow: shared, immutable, and free of per-instance data.
pub struct CompiledFlow {
  pub(crate) nodes: Vec<Arc<dyn CompiledNode>>,
  pub(crate) code: Vec<crate::inline::Instruction>,
  pub output: Type,
  pub analysis: crate::signature::Analysis,
  /// The code runs through one `inline::run` call with no engine help: no
  /// node activates through its shard, no call site, no constructor
  /// (`inline::leaf_code`). A composite runs such a child flow inside its
  /// own step instead of entering a frame for it.
  pub(crate) leaf: bool,
  /// For every instruction, the node it stands for (an index into
  /// `nodes`): what the engine activates, or dispatches, when the VM stops
  /// at it. A composite lowered to flat code (`Repeat`, `While`, `When`,
  /// `If`) contributes its children's nodes and its own control
  /// instructions, which stand for no node (`NO_NODE`).
  pub(crate) pc_nodes: Vec<u32>,
  /// What lowering added beyond the flow's own nodes, when it added
  /// anything (most flows: `None`, one word).
  pub(crate) lowered: Option<Box<Lowered>>,
  /// Where each instruction comes from in the flow's definition, recorded
  /// only by a compose that locates a failure (`ComposeCache::locating`):
  /// `None` otherwise, so a flow costs nothing more for it.
  pub(crate) origins: Option<Box<Origins>>,
}

/// Where each instruction of a flow comes from in its definition, to
/// locate a runtime failure (`stackless::failure_path`): per instruction,
/// an entry in a tree of path steps. The instructions of one shard share its
/// entry, and what a flattened composite or an inlined call brought hangs
/// below the shard that brought it. Recorded only when a failure is being
/// located, and read only then.
#[derive(Default)]
pub(crate) struct Origins {
  /// Per instruction, its entry in `steps` (`NO_ORIGIN`: the flow itself).
  pub at: Box<[u32]>,
  /// A path step, and the entry it follows (`NO_ORIGIN`: the flow's root).
  pub steps: Box<[(OriginStep, u32)]>,
  /// The steps from the shard holding this flow to the flow (its parameter,
  /// and the item for a case or a variadic argument); empty for a wire or
  /// function body.
  pub prefix: Box<[OriginStep]>,
}

/// An `Origins` entry with no step: the flow itself.
pub(crate) const NO_ORIGIN: u32 = u32::MAX;

/// A step of an instruction's origin: a `PathStep` that keeps the names it
/// already shares (a shard type's, a call's function name) instead of
/// copying them.
#[derive(Clone, Debug)]
pub(crate) enum OriginStep {
  Shard {
    index: u32,
    name: OriginName,
  },
  /// A declared parameter's name (a shard's is static; a function's flow
  /// parameter is named by its definition).
  Param(std::borrow::Cow<'static, str>),
  Item(u32),
  Function(Arc<str>),
}

#[derive(Clone, Debug)]
pub(crate) enum OriginName {
  Shard(&'static str),
  Call(Arc<str>),
}

impl OriginStep {
  pub(crate) fn path(&self) -> PathStep {
    match self {
      OriginStep::Shard { index, name } => PathStep::Shard {
        index: *index as usize,
        name: match name {
          OriginName::Shard(name) => (*name).to_string(),
          OriginName::Call(name) => name.to_string(),
        },
      },
      OriginStep::Param(name) => PathStep::Param(name.to_string()),
      OriginStep::Item(item) => PathStep::Item(*item as usize),
      OriginStep::Function(name) => PathStep::Function(name.to_string()),
    }
  }
}

impl Origins {
  /// Appends the path of instruction `pc` within the flow to `out`: empty
  /// at the end of the code, or for an instruction of the flow itself.
  pub(crate) fn path(&self, pc: usize, out: &mut Vec<PathStep>) {
    let mut chain = Vec::new();
    let mut at = self.at.get(pc).copied().unwrap_or(NO_ORIGIN);
    while at != NO_ORIGIN {
      let (step, parent) = &self.steps[at as usize];
      chain.push(step);
      at = *parent;
    }
    out.extend(chain.into_iter().rev().map(OriginStep::path));
  }
}

/// The parts of a flow's code that compose lowering added (`ComposeCtx::flatten`).
#[derive(Default)]
pub(crate) struct Lowered {
  /// Every call inlined into this code, as the range of `nodes` its body
  /// brought (`start`, `len`; possibly empty), in order. The nodes outside
  /// them are the flow's own: a recompiled flow whose inlined bodies
  /// changed has the same own nodes, which is how a live instance moves
  /// onto it with its state (`Engine::rebase`).
  pub inlined: Vec<(u32, u32)>,
  /// The hidden slots this code writes that may hold a heap value: an
  /// inlined call's input, arguments and locals, a flattened composite's
  /// saved input. Each is cleared where its value ends (`Op::Clear`), and
  /// all of them when the code ends otherwise (a failure, a `Stop`).
  pub released_slots: Vec<u32>,
}

/// The `pc_nodes` entry of a control instruction: never activated.
pub(crate) const NO_NODE: u32 = u32::MAX;

impl CompiledFlow {
  /// `Lowered::inlined`.
  pub(crate) fn inlined(&self) -> &[(u32, u32)] {
    self.lowered.as_ref().map_or(&[], |l| &l.inlined)
  }

  /// `Lowered::released_slots`.
  pub(crate) fn released_slots(&self) -> &[u32] {
    self.lowered.as_ref().map_or(&[], |l| &l.released_slots)
  }

  /// Ends the code on `None`, releasing what it output: a function
  /// declared `output: None` whose body ends on a value, like a block for
  /// an `output: None` flow parameter. One instruction, standing for no
  /// node; the flow's own jumps to its end now reach it.
  pub(crate) fn discard_output(&mut self) {
    self.code.push(crate::inline::Instruction::new(
      Some(crate::inline::InlineOp(crate::inline::Op::ConstDrop(
        crate::var::Var::None,
      ))),
      "",
      crate::types::Type::none(),
    ));
    self.pc_nodes.push(NO_NODE);
    if let Some(origins) = &mut self.origins {
      let mut at = std::mem::take(&mut origins.at).into_vec();
      at.push(NO_ORIGIN);
      origins.at = at.into_boxed_slice();
    }
    self.output = crate::types::Type::none();
  }

  /// The node instruction `pc` stands for; `nodes.len()` at the end of
  /// the code. A control instruction stands for none and is never asked.
  #[inline]
  pub(crate) fn node_at(&self, pc: usize) -> usize {
    match self.pc_nodes.get(pc) {
      Some(&node) => {
        debug_assert!(node != NO_NODE, "a control instruction stopped a run");
        node as usize
      }
      None => self.nodes.len(),
    }
  }
}

impl CompiledFlow {
  /// The kind of instruction each top-level node compiled to (`"fallback"`
  /// for a node that runs through its shard). For tests that pin which
  /// builtin a shard lowers to; not a stable API.
  #[doc(hidden)]
  pub fn instruction_kinds(&self) -> Vec<&'static str> {
    self.code.iter().map(|i| i.op.name()).collect()
  }
}
