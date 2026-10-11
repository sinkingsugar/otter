//! Iterative ownership, dispatch and teardown of child flows.
use std::any::Any;
use std::sync::Arc;

use super::arena::{Arena, Generational, Handle};
use crate::Var;
use crate::compose::CompiledWire;
use crate::diagnostic::PathStep;
use crate::error::{Error, Result};
use crate::flow::CompiledFlow;
use crate::function::{BlockArg, CallCompiled, CallTarget, CompiledFunction};
use crate::instance::{CleanupCtx, InstanceCtx};
use crate::reload::{FunctionKey, FunctionRegistry};
use crate::shard::{ActivationCtx, CompiledNode, ControlStep, CustomControl, Step};
use crate::shards::control::{
  Condition, ConditionsCompiled, IfCompiled, MatchCompiled, RunCompiled,
};
use crate::shards::{Predicated, RepeatCompiled};

/// Immutable builtin control descriptions. Children are borrowed only during
/// dispatch; frames retain their compiled owner, never pointers into it.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum Control<'a> {
  /// A function call: its invocation frame is built at entry (golden path
  /// §5), so it has no eager children.
  Call(&'a CallCompiled),
  When(&'a Predicated),
  While(&'a Predicated),
  Sub(&'a CompiledFlow),
  Once(&'a CompiledFlow),
  Repeat(&'a RepeatCompiled),
  If(&'a IfCompiled),
  Match(&'a MatchCompiled),
  Conditions(&'a ConditionsCompiled),
  /// Runs a block its function's call passed (docs/metaprogramming.md
  /// §3.2): its frame is built at entry, like a call's, for the block the
  /// running invocation received.
  Run(&'a RunCompiled),
  /// A composite written against the public interface
  /// ([`crate::shard::ControlShard`]): it says which child to enter and
  /// what to output; its progress lives in `Continuation::custom`. A thin
  /// reference (to the node's box), so `Control` stays two words.
  #[allow(clippy::borrowed_box)]
  Custom(&'a Box<dyn CustomControl>),
}

impl Control<'_> {
  fn len(&self) -> usize {
    match self {
      Self::Call(_) | Self::Run(_) => 0,
      Self::Custom(c) => c.flows().len(),
      Self::Sub(_) | Self::Once(_) => 1,
      // Lowered to flat code at compose: never a node, never framed.
      Self::When(_) | Self::While(_) | Self::Repeat(_) | Self::If(_) => 0,
      Self::Match(c) => c.flows.len(),
      Self::Conditions(c) => c.flows.len(),
    }
  }
}

#[derive(Clone)]
enum Code {
  Root(Arc<CompiledWire>),
  /// A composite's child flow by index (`u32`, so the tag fits beside it).
  Child(Arc<dyn CompiledNode>, u32),
  /// A function invocation: the frame owns the invocation's locals.
  Function(Arc<CompiledFunction>),
  /// A block a call passed for a flow parameter (the call's node and the
  /// block's index), run by `Run` inside the callee on the caller's scope.
  Block(Arc<dyn CompiledNode>, u32),
}
impl Code {
  fn flow(&self) -> &CompiledFlow {
    match self {
      Self::Root(w) => &w.flow,
      // Resolve from the retained owner without borrowing the frame arena.
      Self::Child(n, i) | Self::Block(n, i) => {
        child_flow(&n.control().expect("composite"), *i as usize)
      }
      Self::Function(f) => &f.flow,
    }
  }
}

/// Child flow `i` of a composite (the frame `Code::Child(_, i)` runs).
fn child_flow<'a>(control: &Control<'a>, i: usize) -> &'a CompiledFlow {
  match control {
    Control::Sub(c) | Control::Once(c) => c,
    Control::Match(c) => &c.flows[i],
    Control::Conditions(c) => &c.flows[i],
    // A call's children are its blocks (`Code::Block`); its invocation
    // frame holds its function.
    Control::Call(c) => match &c.blocks[i] {
      BlockArg::Flow(flow) => flow,
      BlockArg::Forward(_) => unreachable!("a forwarded block runs from the call that wrote it"),
    },
    Control::Run(_) => unreachable!("Run's frame runs a call's block"),
    Control::Custom(c) => &c.flows()[i],
    Control::When(_) | Control::While(_) | Control::Repeat(_) | Control::If(_) => {
      unreachable!("lowered to flat code at compose")
    }
  }
}

#[derive(Default)]
struct Continuation {
  children: Vec<Handle>,
  /// A stateless call whose kept frames hold native state to reset at exit
  /// (set at entry from the body entered, so the exit asks no frame).
  resets: bool,
  /// The node is a call (set at build): its entry may run inside this step.
  is_call: bool,
  /// For a stateless, non-recursive call site whose kept frame holds a
  /// straight-line body: the site, so the VM runs it on the kept frame's
  /// locals (`EngineCalls::callee`). Set when the frame path last entered
  /// the site, cleared when the children are replaced. The site lives in
  /// the flow this frame runs, which outlives the frame.
  vm: Option<std::ptr::NonNull<CallCompiled>>,
  phase: u32,
  once_done: bool,
  /// The body a call site selected at its last entry (golden path §11);
  /// `None` until a reload offered one.
  function: Option<Arc<CompiledFunction>>,
  revision: u64,
  /// A `Control::Custom` composite's progress (one word: boxed).
  custom: Option<Box<Custom>>,
}

/// A `Control::Custom` composite's progress: the shard's own state, and the
/// input of the child it asked to enter (taken when the child is entered).
struct Custom {
  state: Box<dyn Any>,
  input: Var,
}
impl Continuation {
  fn reset(&mut self) {
    self.phase = 0;
  }
}
enum State {
  Leaf(Box<dyn Any>),
  Control(Box<Continuation>),
}

/// The state of a leaf in a cached invocation frame between invocations:
/// cleaned up at exit, instantiated again at the next entry. A zero-sized
/// sentinel in the leaf's box (no allocation), so `State` stays two words.
struct Pending;

impl State {
  fn pending() -> State {
    State::Leaf(Box::new(Pending))
  }

  fn is_pending(&self) -> bool {
    matches!(self, State::Leaf(s) if s.is::<Pending>())
  }
}
/// A frame: 112 bytes, so the arena packs seven per kilobyte (the layout is
/// pinned in `tests/trampoline.rs`). Function-only data is boxed.
struct Frame {
  code: Code,
  /// `code.flow()`, resolved once: the flow lives inside the handle `code`
  /// holds, so its address is stable for the frame's lifetime (see `flow`).
  flow: *const CompiledFlow,
  states: Vec<State>,
  value: Var,
  parent: Option<Handle>,
  /// The function frame whose locals this frame's code addresses; `None`
  /// for the instance's own locals.
  scope: Option<Handle>,
  /// For a `Code::Function` frame: the invocation's locals and table.
  invocation: Option<Box<Invocation>>,
  pc: u32,
  call_depth: u32,
  /// The arena slot's generation (see `arena::Generational`).
  generation: std::num::NonZeroU32,
}

impl Generational for Frame {
  fn generation(&self) -> std::num::NonZeroU32 {
    self.generation
  }
  fn set_generation(&mut self, generation: std::num::NonZeroU32) {
    self.generation = generation;
  }
}

/// What a function frame owns beyond its code: the invocation's locals and
/// the table its recursive calls resolve through, pinned at the outermost
/// entry of the group.
struct Invocation {
  locals: Vec<Var>,
  table: Arc<FunctionRegistry>,
}

impl Frame {
  fn new(code: Code) -> Self {
    let flow: *const CompiledFlow = code.flow();
    Self {
      states: Vec::with_capacity(code.flow().nodes.len()),
      flow,
      code,
      value: Var::None,
      parent: None,
      scope: None,
      invocation: None,
      pc: 0,
      call_depth: 0,
      generation: std::num::NonZeroU32::MIN,
    }
  }

  /// The node the current instruction stands for.
  fn node(&self) -> usize {
    self.flow().node_at(self.pc())
  }

  fn pc(&self) -> usize {
    self.pc as usize
  }

  /// The frame's flow, without resolving it through the handle each step.
  /// The reference is not tied to the frame borrow, so a step can read the
  /// flow while writing the frame's other fields; it must not outlive the
  /// frame (every use is within one step or one cleanup of the frame).
  fn flow<'a>(&self) -> &'a CompiledFlow {
    // SAFETY: `self.flow` was taken from `self.code`, which the frame owns
    // for as long as it exists; the flow is inside that handle's allocation
    // (a wire's or function's field, or a child flow of a compiled node) and
    // never moves. The frame never changes `code` after construction, and
    // callers keep the frame alive while they hold the reference.
    unsafe { &*self.flow }
  }

  fn locals(&self) -> &[Var] {
    self.invocation.as_ref().map_or(&[], |i| &i.locals)
  }

  fn locals_mut(&mut self) -> &mut Vec<Var> {
    &mut self.invocation.as_mut().expect("function frame").locals
  }

  /// The exit of the invocation a function frame runs, returned or failed:
  /// the locals that do not persist are unset now, not at the next entry,
  /// so no input, argument or local value outlives the call (a collection
  /// the caller passed in is uniquely owned by the caller again).
  fn release_locals(&mut self) {
    let Code::Function(body) = &self.code else {
      unreachable!("an invocation's frame holds its function")
    };
    let invocation = self.invocation.as_mut().expect("function frame");
    if body.keeps.is_empty() {
      for local in invocation.locals.iter_mut() {
        *local = Var::None;
      }
    } else {
      for (slot, local) in invocation.locals.iter_mut().enumerate() {
        if !body.persistent(slot) {
          *local = Var::None;
        }
      }
    }
  }
}

/// The VM form of a call site with a kept frame, when it has one: a
/// stateless, non-recursive site whose body is straight-line code.
fn vm_site(
  call: &CallCompiled,
  body: &Arc<CompiledFunction>,
) -> Option<std::ptr::NonNull<CallCompiled>> {
  if !call.stateful() && body.vm_only && matches!(call.target, CallTarget::Direct(_)) {
    Some(std::ptr::NonNull::from(call))
  } else {
    None
  }
}

/// Binds a call's arguments and input into an invocation's locals: the
/// arguments are snapshots taken from the caller's scope (which `ctx`
/// still addresses at this point), immutable for the whole invocation.
fn bind_arguments(
  locals: &mut [Var],
  body: &CompiledFunction,
  call: &CallCompiled,
  input: Var,
  ctx: &ActivationCtx<'_>,
) {
  for (slot, op) in body.param_slots.iter().zip(&call.args) {
    locals[*slot] = op.get(ctx);
  }
  locals[body.input_slot] = input;
}

/// A unit of a flow for moving an instance between two compilations of
/// it (`Engine::rebase`): one of its own nodes, or the nodes an inlined
/// call brought (`start`, `len`).
#[derive(Clone, Copy)]
enum Unit {
  Own(usize),
  Inlined(usize, usize),
}

fn units(flow: &CompiledFlow) -> Vec<Unit> {
  let mut units = Vec::with_capacity(flow.nodes.len());
  let mut next = 0;
  for &(start, len) in flow.inlined() {
    let (start, len) = (start as usize, len as usize);
    units.extend((next..start).map(Unit::Own));
    units.push(Unit::Inlined(start, len));
    next = start + len;
  }
  units.extend((next..flow.nodes.len()).map(Unit::Own));
  units
}

/// The units of two compilations of a flow, paired in order, when an
/// instance can move from `old` to `new` keeping the state of every own
/// node: the same own nodes (by shard, control and call), with inlined
/// calls anywhere in between. A stateless call site may stand where the
/// other has an inlined call (an edit took the callee past the inlining
/// limits, or under them): its frames hold no state that persists.
fn pair_units(old: &CompiledFlow, new: &CompiledFlow) -> Option<Vec<(Unit, Unit)>> {
  fn stateless_call(node: &Arc<dyn CompiledNode>) -> bool {
    matches!(node.control(), Some(Control::Call(c)) if !c.stateful())
  }
  fn same_node(a: &Arc<dyn CompiledNode>, b: &Arc<dyn CompiledNode>) -> bool {
    if a.name() != b.name() {
      return false;
    }
    match (a.control(), b.control()) {
      (None, None) => true,
      (Some(Control::Call(x)), Some(Control::Call(y))) => {
        x.stateful() == y.stateful()
          && x.def().name == y.def().name
          && matches!(x.target, CallTarget::Lazy { .. })
            == matches!(y.target, CallTarget::Lazy { .. })
      }
      (Some(x), Some(y)) => {
        std::mem::discriminant(&x) == std::mem::discriminant(&y) && x.len() == y.len()
      }
      _ => false,
    }
  }
  let (a, b) = (units(old), units(new));
  if a.len() != b.len() {
    return None;
  }
  let pairs: Vec<(Unit, Unit)> = a.into_iter().zip(b).collect();
  pairs
    .iter()
    .all(|pair| match *pair {
      (Unit::Own(i), Unit::Own(j)) => same_node(&old.nodes[i], &new.nodes[j]),
      (Unit::Inlined(..), Unit::Inlined(..)) => true,
      (Unit::Own(i), Unit::Inlined(..)) => stateless_call(&old.nodes[i]),
      (Unit::Inlined(..), Unit::Own(j)) => stateless_call(&new.nodes[j]),
    })
    .then_some(pairs)
}

/// Whether a live instance of `old` can move onto `new` with its state
/// (`Engine::rebase`), through every composite's child flows.
pub(crate) fn rebasable(old: &CompiledFlow, new: &CompiledFlow) -> bool {
  let mut work = vec![(old, new)];
  while let Some((old, new)) = work.pop() {
    let Some(pairs) = pair_units(old, new) else {
      return false;
    };
    for (o, n) in pairs {
      if let (Unit::Own(i), Unit::Own(j)) = (o, n)
        && let (Some(x), Some(y)) = (old.nodes[i].control(), new.nodes[j].control())
        && !matches!(x, Control::Call(_) | Control::Run(_))
      {
        for k in 0..x.len() {
          work.push((child_flow(&x, k), child_flow(&y, k)));
        }
      }
    }
  }
  true
}

/// `flow`'s code ended other than at its end (a failure, `Stop`, `Return`
/// or `Restart`), skipping the `Clear`s after that point: the values of its
/// hidden slots end here, since the whole flow ends.
fn release_slots(flow: &CompiledFlow, locals: &mut [Var]) {
  for slot in flow.released_slots() {
    locals[*slot as usize] = Var::None;
  }
}

/// One level a failure passed through in a compose-time evaluation (kept
/// in its meter, `Meter::trace`).
pub(crate) struct Failed(Location);

enum Location {
  /// A frame's code and the instruction it failed at.
  Frame(Code, u32),
  /// A child flow (`.1`) a composite ran inside its own step, and the
  /// instruction it failed at.
  Leaf(Arc<dyn CompiledNode>, u32, u32),
}

#[cold]
fn no_body(name: &str, input: crate::types::Type) -> Error {
  Error::Activation(format!(
    "{name} has no compiled body for input {input} in this revision"
  ))
}

#[cold]
fn record(meter: &crate::compose_time::Meter, at: Location) {
  meter.trace.borrow_mut().push(Failed(at));
}

/// Where the evaluation metered by `meter` failed: the definition path from
/// its pipeline to the shard that failed, through the composites,
/// parameters and functions it ran in (inlined and flattened code
/// included). Empty when nothing failed.
pub(crate) fn failure_path(meter: &crate::compose_time::Meter) -> Vec<PathStep> {
  let prefix = |flow: &CompiledFlow| -> Vec<PathStep> {
    flow
      .origins
      .as_ref()
      .map_or_else(Vec::new, |o| o.prefix.iter().map(|s| s.path()).collect())
  };
  let mut path = Vec::new();
  let trace = meter.trace.borrow();
  // Each level's flow, and the path's length up to its failing shard.
  let mut levels: Vec<(&CompiledFlow, usize)> = Vec::new();
  for Failed(at) in trace.iter().rev() {
    let (flow, pc) = match at {
      Location::Frame(code, pc) => {
        match code {
          Code::Root(_) => {}
          Code::Child(..) => path.extend(prefix(code.flow())),
          Code::Function(f) => path.push(PathStep::Function(f.def.name.clone())),
          // A block is lexically where its call wrote it: back to the
          // level that ran that call, below the call's own shard.
          Code::Block(call, _) => {
            let wrote = |flow: &CompiledFlow| {
              flow
                .nodes
                .iter()
                .any(|n| std::ptr::addr_eq(Arc::as_ptr(n), Arc::as_ptr(call)))
            };
            if let Some(&(_, len)) = levels.iter().rev().find(|(flow, _)| wrote(flow)) {
              path.truncate(len);
            }
            path.extend(prefix(code.flow()));
          }
        }
        (code.flow(), *pc)
      }
      Location::Leaf(node, child, pc) => {
        let flow = child_flow(&node.control().expect("composite"), *child as usize);
        path.extend(prefix(flow));
        (flow, *pc)
      }
    };
    if let Some(origins) = &flow.origins {
      origins.path(pc as usize, &mut path);
    }
    levels.push((flow, path.len()));
  }
  path
}

/// What one frame step asks the loop to do next.
enum Next {
  Continue,
  Finish(Result<Step>),
}

/// An instance's flat frame tree and current innermost activation. Child
/// handles survive suspension; no ancestor is visited to find the active leaf.
pub(crate) struct Engine {
  frames: Arena<Frame>,
  root: Option<Handle>,
  current: Option<Handle>,
  /// The function frame whose locals `current` addresses (its `scope`),
  /// kept beside it so a step needs no lookup to know it.
  current_scope: Option<Handle>,
  /// The instance's own locals during an activation (`ctx.locals` as
  /// handed in), to point back at when the active scope leaves every
  /// invocation. Null outside an activation.
  own_locals: *mut [Var],
  pub dispatches: u64,
}

impl Engine {
  pub fn instantiate(wire: Arc<CompiledWire>, ctx: &mut InstanceCtx) -> Result<Self> {
    let mut engine = Self {
      frames: Arena::default(),
      root: None,
      current: None,
      current_scope: None,
      own_locals: std::ptr::slice_from_raw_parts_mut(std::ptr::null_mut(), 0),
      dispatches: 0,
    };
    let root = engine.build(Code::Root(wire), None, ctx)?;
    engine.root = Some(root);
    Ok(engine)
  }

  /// Depth-first initialization, preserving source order without Rust
  /// recursion. `scope` is the function frame whose locals the new frames
  /// address (a `Code::Function` root addresses its own).
  fn build(&mut self, code: Code, scope: Option<Handle>, ctx: &mut InstanceCtx) -> Result<Handle> {
    // The immutable graph gives the exact eagerly instantiated frame count.
    // Reserve once: Vec growth at a deep branch otherwise briefly needs both
    // the old and doubled arena, and wastes scarce device heap after growth.
    let mut count = 0;
    let mut pending = vec![code.clone()];
    while let Some(code) = pending.pop() {
      count += 1;
      for node in &code.flow().nodes {
        if let Some(control) = node.control() {
          for i in 0..control.len() {
            pending.push(Code::Child(node.clone(), i as u32));
          }
        }
      }
    }
    drop(pending);
    self.frames.reserve(count);
    let function = matches!(code, Code::Function(_));
    let root = self.frames.insert(Frame::new(code));
    let scope = if function { Some(root) } else { scope };
    self.frames.get_mut(root).expect("root").scope = scope;
    let mut work = vec![root];
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      while let Some(&handle) = work.last() {
        let frame = self.frames.get(handle).expect("frame");
        let index = frame.states.len();
        if index == frame.flow().nodes.len() {
          work.pop();
          continue;
        }
        let node = frame.flow().nodes[index].clone();
        let state = if let Some(control) = node.control() {
          let mut c = Continuation {
            children: Vec::with_capacity(control.len()),
            is_call: matches!(control, Control::Call(_)),
            ..Continuation::default()
          };
          if let Control::Custom(custom) = control {
            c.custom = Some(Box::new(Custom {
              state: custom.new_state(),
              input: Var::None,
            }));
          }
          for i in 0..control.len() {
            let mut child = Frame::new(Code::Child(node.clone(), i as u32));
            child.scope = scope;
            c.children.push(self.frames.insert(child));
          }
          work.extend(c.children.iter().rev().copied());
          State::Control(Box::new(c))
        } else {
          // Shared panic/rollback policy, including host instantiation panics.
          let mut cleanup = ctx.cleanup_ctx();
          let mut initialized = crate::lifecycle::instantiate_all(
            [node],
            |n| n.instantiate(ctx),
            |n, s| n.cleanup(s.as_mut(), &mut cleanup),
          )?;
          State::Leaf(initialized.pop().expect("one node").1)
        };
        self
          .frames
          .get_mut(handle)
          .expect("frame")
          .states
          .push(state);
      }
      Ok(())
    }))
    .unwrap_or_else(|p| {
      Err(Error::Activation(format!(
        "panic in instantiate: {}",
        crate::error::panic_message(&*p)
      )))
    });
    if let Err(err) = result {
      let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        self.cleanup_tree(root, &mut ctx.cleanup_ctx());
      }));
      return Err(match cleanup {
        Ok(()) => err,
        Err(p) => Error::Activation(format!(
          "{err}; and a cleanup panicked during rollback: {}",
          crate::error::panic_message(&*p)
        )),
      });
    }
    Ok(root)
  }

  /// No iteration is in flight: the last one ended and the next has not
  /// started (nothing suspended).
  pub fn between_iterations(&self) -> bool {
    self.current.is_none()
  }

  /// Moves the instance onto `wire`, a recompiled body of the wire it runs
  /// whose own nodes are those of the current body (`rebasable`: only
  /// inlined calls changed), between two iterations (no frame is active).
  /// Every own node keeps its state: leaves their native state, composites
  /// their progress flags, call sites their kept frames (components with
  /// their `Keep` slots, cached invocations). Only the nodes of inlined
  /// calls start fresh. All or nothing: if a fresh node fails to
  /// instantiate, the engine is left on the old body.
  pub fn rebase(&mut self, wire: Arc<CompiledWire>, ctx: &mut InstanceCtx) -> Result<()> {
    debug_assert!(self.current.is_none(), "rebased between iterations");
    let old_root = self.root.expect("live engine");
    let new_root = self.frames.insert(Frame::new(Code::Root(wire)));
    // States to move once every fresh one exists: (new frame, node, old
    // frame, node). The new slot holds a pending placeholder until then.
    let mut moves: Vec<(Handle, usize, Handle, usize)> = Vec::new();
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
      let mut work = vec![(new_root, old_root)];
      while let Some((nh, oh)) = work.pop() {
        let old_flow = self.frames.get(oh).expect("old frame").flow();
        let new_flow = self.frames.get(nh).expect("new frame").flow();
        let pairs = pair_units(old_flow, new_flow).ok_or_else(|| {
          Error::Activation("a swapped body changed outside its inlined calls".into())
        })?;
        for (old, new) in pairs {
          match (old, new) {
            (Unit::Own(i), Unit::Own(j)) => {
              let node = new_flow.nodes[j].clone();
              let state = match node.control() {
                Some(control) if !matches!(control, Control::Call(_) | Control::Run(_)) => {
                  let State::Control(old_c) = &self.frames.get(oh).expect("old frame").states[i]
                  else {
                    unreachable!("paired composites")
                  };
                  let (once_done, old_children) = (old_c.once_done, old_c.children.clone());
                  let mut c = Continuation {
                    children: Vec::with_capacity(control.len()),
                    once_done,
                    ..Continuation::default()
                  };
                  // Between iterations: no activation is in progress.
                  if let Control::Custom(custom) = control {
                    c.custom = Some(Box::new(Custom {
                      state: custom.new_state(),
                      input: Var::None,
                    }));
                  }
                  for (k, old_child) in old_children.into_iter().enumerate() {
                    let child = self
                      .frames
                      .insert(Frame::new(Code::Child(node.clone(), k as u32)));
                    c.children.push(child);
                    work.push((child, old_child));
                  }
                  State::Control(Box::new(c))
                }
                _ => {
                  moves.push((nh, j, oh, i));
                  State::pending()
                }
              };
              self
                .frames
                .get_mut(nh)
                .expect("new frame")
                .states
                .push(state);
            }
            (_, Unit::Inlined(start, len)) => {
              for node in &new_flow.nodes[start..start + len] {
                let state = State::Leaf(node.instantiate(ctx)?);
                self
                  .frames
                  .get_mut(nh)
                  .expect("new frame")
                  .states
                  .push(state);
              }
            }
            // A stateless call that is no longer inlined: a site never entered.
            (Unit::Inlined(..), Unit::Own(_)) => {
              let c = Continuation {
                is_call: true,
                ..Continuation::default()
              };
              let state = State::Control(Box::new(c));
              self
                .frames
                .get_mut(nh)
                .expect("new frame")
                .states
                .push(state);
            }
          }
        }
      }
      Ok(())
    }))
    .unwrap_or_else(|p| {
      Err(Error::Activation(format!(
        "panic in instantiate: {}",
        crate::error::panic_message(&*p)
      )))
    });
    if let Err(err) = built {
      let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        self.cleanup_tree(new_root, &mut ctx.cleanup_ctx());
      }));
      return Err(match cleanup {
        Ok(()) => err,
        Err(p) => Error::Activation(format!(
          "{err}; and a cleanup panicked during rollback: {}",
          crate::error::panic_message(&*p)
        )),
      });
    }
    for (nh, j, oh, i) in moves {
      let old_flow = self.frames.get(oh).expect("old frame").flow();
      let new_flow = self.frames.get(nh).expect("new frame").flow();
      let state = std::mem::replace(
        &mut self.frames.get_mut(oh).expect("old frame").states[i],
        State::pending(),
      );
      let state = match (
        state,
        old_flow.nodes[i].control(),
        new_flow.nodes[j].control(),
      ) {
        (State::Control(mut c), Some(Control::Call(old)), Some(Control::Call(new))) => {
          // The site keeps the body its frames run: selected at the next
          // entry against the installed revision, like any call site.
          if c.function.is_none()
            && let CallTarget::Direct(body) = &old.target
          {
            c.function = Some(body.clone());
          }
          // The VM form points at the site, which now lives in the new body.
          if c.vm.is_some() {
            c.vm = Some(std::ptr::NonNull::from(new));
          }
          State::Control(c)
        }
        (state, ..) => state,
      };
      self.frames.get_mut(nh).expect("new frame").states[j] = state;
    }
    self.root = Some(new_root);
    // What did not move: the old frames, the nodes of the old inlined
    // calls, and the frames of sites whose calls are inlined now.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      self.cleanup_tree(old_root, &mut ctx.cleanup_ctx());
    }))
    .map_err(|p| {
      Error::Activation(format!(
        "panic in cleanup: {}",
        crate::error::panic_message(&*p)
      ))
    })
  }

  /// Remove ownership first, then attempt every logical cleanup in reverse
  /// initialization order. A panicking cleanup cannot strand an owned frame.
  fn cleanup_tree(&mut self, root: Handle, ctx: &mut CleanupCtx) {
    enum Work {
      Frame(Handle),
      Node(Arc<dyn CompiledNode>, State),
    }
    let mut work = vec![Work::Frame(root)];
    let mut leaves = Vec::new();
    while let Some(item) = work.pop() {
      match item {
        Work::Frame(h) => {
          let frame = self.frames.remove(h).expect("live cleanup frame");
          for (node, state) in frame.flow().nodes.iter().cloned().zip(frame.states) {
            work.push(Work::Node(node, state));
          }
        }
        Work::Node(_, State::Leaf(state)) if state.is::<Pending>() => {}
        Work::Node(node, State::Leaf(state)) => leaves.push((node, state)),
        Work::Node(_, State::Control(c)) => work.extend(c.children.into_iter().map(Work::Frame)),
      }
    }
    crate::lifecycle::cleanup_each(leaves, |(node, mut state)| {
      node.cleanup(state.as_mut(), ctx)
    });
  }

  pub fn cleanup(&mut self, ctx: &mut CleanupCtx) {
    self.current = None;
    self.current_scope = None;
    if let Some(root) = self.root.take() {
      self.cleanup_tree(root, ctx);
    }
  }

  pub fn state_size(&self) -> usize {
    self.frames.capacity_bytes()
      + self
        .frames
        .values()
        .map(|f| {
          f.states.capacity() * std::mem::size_of::<State>()
            + f.invocation.as_ref().map_or(0, |i| {
              std::mem::size_of::<Invocation>() + i.locals.capacity() * std::mem::size_of::<Var>()
            })
            + f
              .states
              .iter()
              .enumerate()
              .map(|(i, s)| match s {
                State::Leaf(s) if s.is::<Pending>() => 0,
                State::Leaf(s) => f.flow().nodes[i].state_size(s.as_ref()),
                State::Control(c) => {
                  std::mem::size_of::<Continuation>()
                    + c.children.capacity() * std::mem::size_of::<Handle>()
                }
              })
              .sum::<usize>()
        })
        .sum::<usize>()
  }

  fn continuation(&mut self, h: Handle) -> &mut Continuation {
    let f = self.frames.get_mut(h).expect("frame");
    let node = f.node();
    match &mut f.states[node] {
      State::Control(c) => c,
      _ => unreachable!(),
    }
  }

  /// Entering a function call (golden path §3.3, §5, §11): the call site
  /// selects the newest accepted body once per reload, the arguments are
  /// read once, in the caller's frame, then the invocation frame is built
  /// (stateless: fresh each time) or reused (stateful: its `Keep` slots and
  /// native state survive, ordinary locals start fresh), and receives the
  /// parameters and `input`.
  fn prepare_function_call(
    &mut self,
    h: Handle,
    call: &CallCompiled,
    ctx: &mut ActivationCtx<'_>,
  ) -> Result<()> {
    let (depth, input, scope, revision, kept) = {
      let f = self.frames.get_mut(h).expect("frame");
      let input = if call.def().ignores_input() {
        Var::None
      } else {
        f.value.clone()
      };
      let node = f.node();
      let State::Control(c) = &mut f.states[node] else {
        unreachable!("a call site is a composite")
      };
      (
        f.call_depth as usize,
        input,
        f.scope,
        c.revision,
        c.children.first().copied(),
      )
    };
    if depth >= ctx.max_call_depth {
      use crate::diagnostic::{Diagnostic, PathStep, Phase};
      let name = &call.def().name;
      let mut d = Diagnostic::new(
        Phase::Activate,
        "activation-error",
        "recursion-limit",
        format!(
          "{name} exceeds max_call_depth {} at depth {depth}",
          ctx.max_call_depth
        ),
      );
      d.path.push(PathStep::Function(name.clone()));
      return Err(Error::Diagnostic(Box::new(d.shard(name))));
    }
    // The body, and the table it pins for recursive calls underneath it.
    let (body, table) = match &call.target {
      // A recursive call resolves through the table its group entered
      // with, so the group runs as a unit across reloads (§11).
      CallTarget::Lazy { key, def } => {
        let invocation = match scope {
          Some(s) => self.frames.get(s).expect("scope").invocation.as_ref(),
          None => None,
        };
        let table = match invocation {
          Some(i) => i.table.clone(),
          None => ctx.table().clone(),
        };
        let body = match table.get(key) {
          Some(body) => body.clone(),
          None => return Err(no_body(&def.name, key.input)),
        };
        (body, table)
      }
      CallTarget::Direct(direct) => {
        // Body selection: once per accepted reload, at an inactive boundary.
        // Between reloads a kept frame is entered without touching any
        // handle: its code is the selected body.
        if revision == ctx.reload_revision()
          && let Some(child) = kept
        {
          return self.enter_kept_frame(child, call, input, ctx);
        }
        let c = self.continuation(h);
        let mut body = match &c.function {
          Some(selected) => selected.clone(),
          None => direct.clone(),
        };
        if c.revision != ctx.reload_revision() {
          c.revision = ctx.reload_revision();
          if let Some(next) = ctx.function_body(&FunctionKey::of(&body))
            && !Arc::ptr_eq(&next, &body)
          {
            // The old frames stay owned by the site until the replacement
            // exists: a failed migration leaves the site as it was (its
            // old body still selected, retried at the next entry) and
            // nothing becomes unreachable for cleanup.
            let migrated = match (call.stateful(), c.children.first().copied()) {
              (true, Some(old_child)) => match self.migrate_component(old_child, &body, &next, ctx)
              {
                Ok(child) => Some(child),
                Err(err) => {
                  self.continuation(h).revision = 0;
                  return Err(err);
                }
              },
              _ => None,
            };
            let c = self.continuation(h);
            let old = std::mem::replace(&mut c.children, migrated.into_iter().collect());
            c.function = Some(next.clone());
            c.vm = None;
            for child in old {
              self.cleanup_tree(
                child,
                &mut CleanupCtx {
                  instance: ctx.instance(),
                },
              );
            }
            body = next;
          }
        }
        (body, ctx.table().clone())
      }
    };
    let body = &body;
    if let Some(child) = self.continuation(h).children.first().copied() {
      return self.enter_kept_frame(child, call, input, ctx);
    }
    let child = self.build(
      Code::Function(body.clone()),
      None,
      &mut InstanceCtx {
        instance: ctx.instance(),
      },
    )?;
    let frame = self.frames.get_mut(child).expect("new call frame");
    let mut locals = body.fresh_locals();
    bind_arguments(&mut locals, body, call, input, ctx);
    frame.invocation = Some(Box::new(Invocation {
      locals,
      table: table.clone(),
    }));
    let vm = vm_site(call, body);
    let c = self.continuation(h);
    c.children.push(child);
    c.resets = !call.stateful() && body.native_state;
    c.vm = vm;
    Ok(())
  }

  /// Enters a call site's kept frame (a stateful component, or a stateless
  /// site's cached invocation): resets its program counter, re-pins the
  /// current table (a recursive group below runs on the revision current
  /// now, not at the first call), binds the arguments and instantiates the
  /// leaves `reset_invocation` cleaned up. The locals that do not persist
  /// are already unset (`release_locals` at the last exit). No handle is
  /// cloned unless the table changed.
  fn enter_kept_frame(
    &mut self,
    child: Handle,
    call: &CallCompiled,
    input: Var,
    ctx: &ActivationCtx<'_>,
  ) -> Result<()> {
    let frame = self.frames.get_mut(child).expect("kept call frame");
    let Code::Function(body) = &frame.code else {
      unreachable!("a call's frame holds its function")
    };
    frame.pc = 0;
    let invocation = frame.invocation.as_mut().expect("function frame");
    if !Arc::ptr_eq(&invocation.table, ctx.table()) {
      invocation.table = ctx.table().clone();
    }
    bind_arguments(&mut invocation.locals, body, call, input, ctx);
    if !call.stateful()
      && body.native_state
      && let Err(err) = self.revive_invocation(child, ctx)
    {
      // Not entered: the arguments just bound end here too.
      self
        .frames
        .get_mut(child)
        .expect("kept call frame")
        .release_locals();
      return Err(err);
    }
    Ok(())
  }

  /// The block flow parameter `param` of the invocation `scope` runs: the
  /// frame of the call site that wrote it, the call's node there, the
  /// block's index and the scope it runs on (the caller's). A running
  /// invocation's frame has its caller as parent, stopped at the call; a
  /// forwarded block is looked up the same way from the caller's own
  /// invocation. Nothing is stored per call for this.
  fn resolve_block(&self, scope: Handle, param: u32) -> (Handle, usize, u32, Option<Handle>) {
    let (mut scope, mut param) = (scope, param);
    loop {
      let invocation = self.frames.get(scope).expect("running invocation");
      let site = invocation.parent.expect("a running invocation's caller");
      let caller = self.frames.get(site).expect("caller frame");
      let node = caller.node();
      let Some(Control::Call(call)) = caller.flow().nodes[node].control() else {
        unreachable!("an invocation's caller is stopped at its call")
      };
      match &call.blocks[param as usize] {
        BlockArg::Flow(_) => return (site, node, param, caller.scope),
        BlockArg::Forward(outer) => {
          scope = caller
            .scope
            .expect("a forwarded flow parameter is the calling function's");
          param = *outer;
        }
      }
    }
  }

  /// Entering `Run` (docs/metaprogramming.md §3.2): the frame running the
  /// block the invocation received for the flow parameter, on its caller's
  /// scope. The Run node keeps the last block's frame and enters it again
  /// when the same block runs next; native state inside it lives for one
  /// run (cleaned up at exit, instantiated again here), so a block starts
  /// fresh each time. Another block replaces the frame.
  ///
  /// A block of leaf code (`CompiledFlow::leaf`) needs no frame: at run
  /// time it runs right here, on its scope's locals, and the Run completes
  /// in this step (`Ok(true)`; no value travels back through the step).
  #[inline(never)]
  fn prepare_run(
    &mut self,
    h: Handle,
    node: usize,
    run: &RunCompiled,
    ctx: &mut ActivationCtx<'_>,
  ) -> Result<bool> {
    let frame = self.frames.get(h).expect("frame");
    let scope = frame.scope.expect("Run runs inside a function");
    let (site, call, block, block_scope) = self.resolve_block(scope, run.param);
    let owner = &self.frames.get(site).expect("caller frame").flow().nodes[call];
    let flow = child_flow(&owner.control().expect("a call"), block as usize);
    // A compose-time evaluation keeps the frame, which locates a failure.
    if flow.leaf && ctx.meter.is_none() {
      let input = if run.ignores_input {
        Var::None
      } else {
        frame.value.clone()
      };
      self.switch_scope(block_scope, ctx);
      let result = crate::inline::run(
        &flow.code,
        0,
        input,
        ctx.locals,
        ctx.mesh_frame,
        &crate::inline::NoCalls,
      );
      if result.is_err() {
        release_slots(flow, ctx.locals);
      }
      let scope = self.current_scope;
      self.switch_scope(scope, ctx);
      let (_, output) = result?;
      self.ran_block(h, node, run, output);
      return Ok(true);
    }
    let owner_ptr = Arc::as_ptr(owner);
    let c = self.continuation(h);
    if let Some(&child) = c.children.first() {
      let resets = c.resets;
      let frame = self.frames.get(child).expect("block frame");
      if let Code::Block(n, i) = &frame.code
        && std::ptr::addr_eq(Arc::as_ptr(n), owner_ptr)
        && *i == block
        && frame.scope == block_scope
      {
        if resets {
          self.revive_invocation(child, ctx)?;
        }
        return Ok(false);
      }
      self.continuation(h).children.clear();
      self.cleanup_tree(
        child,
        &mut CleanupCtx {
          instance: ctx.instance(),
        },
      );
    }
    let owner = self.frames.get(site).expect("caller frame").flow().nodes[call].clone();
    let child = self.build(
      Code::Block(owner, block),
      block_scope,
      &mut InstanceCtx {
        instance: ctx.instance(),
      },
    )?;
    let resets = self
      .frames
      .get(child)
      .expect("block frame")
      .flow()
      .analysis
      .lifetime
      != crate::signature::Lifetime::Stateless;
    let c = self.continuation(h);
    c.children.push(child);
    c.resets = resets;
    Ok(false)
  }

  /// A Run whose block ran in its prepare step (`prepare_run`): the Run
  /// completes with the block's output, or its input when it passes it on.
  fn ran_block(&mut self, h: Handle, node: usize, run: &RunCompiled, output: Var) {
    let f = self.frames.get_mut(h).expect("live frame");
    if let State::Control(c) = &mut f.states[node] {
      c.reset();
    }
    if run.discards_output {
      f.value = Var::None;
    } else if !run.passthrough {
      f.value = output;
    }
    f.pc += 1;
  }

  /// A stateful call site adopting a new body (golden path §11): the new
  /// frame is built, `Keep` slots that match by name and type carry their
  /// values over (and count as applied), everything else starts fresh. The
  /// old frame is cleaned up by the caller afterwards.
  fn migrate_component(
    &mut self,
    old_child: Handle,
    old: &CompiledFunction,
    new: &Arc<CompiledFunction>,
    ctx: &ActivationCtx<'_>,
  ) -> Result<Handle> {
    let plan = crate::reload::keep_plan(old, new);
    let child = self.build(
      Code::Function(new.clone()),
      None,
      &mut InstanceCtx {
        instance: ctx.instance(),
      },
    )?;
    let mut locals = new.fresh_locals();
    let old_locals = self.frames.get(old_child).expect("old call frame").locals();
    for (from, to) in &plan.moves {
      locals[*to] = old_locals[*from].clone();
    }
    let frame = self.frames.get_mut(child).expect("new call frame");
    frame.invocation = Some(Box::new(Invocation {
      locals,
      table: ctx.table().clone(),
    }));
    Ok(child)
  }

  /// The function bodies this engine holds frames for: running or cached
  /// invocations, and stateful call sites' components. A reload validates
  /// against these, not against the bodies the wire was composed with,
  /// since a site may have adopted newer ones (golden path §11).
  pub fn function_bodies(&self) -> impl Iterator<Item = &Arc<CompiledFunction>> {
    self.frames.values().filter_map(|f| match &f.code {
      Code::Function(body) => Some(body),
      _ => None,
    })
  }

  /// Every frame of the invocation rooted at `child`, parents first.
  fn invocation_frames(&self, child: Handle) -> Vec<Handle> {
    let mut all = Vec::new();
    let mut work = vec![child];
    while let Some(h) = work.pop() {
      all.push(h);
      for state in &self.frames.get(h).expect("invocation frame").states {
        if let State::Control(c) = state {
          work.extend(c.children.iter().copied());
        }
      }
    }
    all
  }

  /// A stateless invocation's exit (golden path §3.4): the frames stay for
  /// the next call, but native state inside them lives for the invocation,
  /// so every leaf that is not stateless is cleaned up now and instantiated
  /// again at the next entry.
  fn reset_invocation(&mut self, child: Handle, ctx: &mut CleanupCtx) {
    let mut leaves = Vec::new();
    for h in self.invocation_frames(child) {
      let frame = self.frames.get_mut(h).expect("invocation frame");
      let nodes = frame.flow().nodes.clone();
      for (node, state) in nodes.iter().zip(frame.states.iter_mut()) {
        if matches!(state, State::Leaf(_))
          && !state.is_pending()
          && node.lifetime() != crate::signature::Lifetime::Stateless
          && let State::Leaf(s) = std::mem::replace(state, State::pending())
        {
          leaves.push((node.clone(), s));
        }
      }
    }
    crate::lifecycle::cleanup_each(leaves, |(node, mut state)| {
      node.cleanup(state.as_mut(), ctx)
    });
  }

  /// The entry of a cached stateless invocation: instantiates the leaves
  /// `reset_invocation` cleaned up. All or nothing, like a first build: on
  /// an error (or a panic) the leaves this attempt instantiated are
  /// cleaned up and pending again, so the invocation that failed to start
  /// holds nothing and the next entry starts from the same point.
  fn revive_invocation(&mut self, child: Handle, ctx: &ActivationCtx<'_>) -> Result<()> {
    let mut ictx = InstanceCtx {
      instance: ctx.instance(),
    };
    let mut revived: Vec<(Handle, usize)> = Vec::new();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      for h in self.invocation_frames(child) {
        let frame = self.frames.get_mut(h).expect("invocation frame");
        let nodes = frame.flow().nodes.clone();
        for (i, (node, state)) in nodes.iter().zip(frame.states.iter_mut()).enumerate() {
          if state.is_pending() {
            *state = State::Leaf(node.instantiate(&mut ictx)?);
            revived.push((h, i));
          }
        }
      }
      Ok(())
    }));
    let result = match caught {
      Ok(result) => result,
      Err(p) => Err(Error::Activation(format!(
        "panic in instantiate: {}",
        crate::error::panic_message(&*p)
      ))),
    };
    if result.is_err() {
      // In reverse initialization order, every cleanup attempted.
      let mut leaves = Vec::new();
      for (h, i) in revived.into_iter().rev() {
        let frame = self.frames.get_mut(h).expect("invocation frame");
        let node = frame.flow().nodes[i].clone();
        if let State::Leaf(s) = std::mem::replace(&mut frame.states[i], State::pending()) {
          leaves.push((node, s));
        }
      }
      let mut cleanup = ictx.cleanup_ctx();
      let rollback = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::lifecycle::cleanup_each(leaves, |(node, mut state)| {
          node.cleanup(state.as_mut(), &mut cleanup)
        });
      }));
      if let (Err(err), Err(p)) = (&result, rollback) {
        return Err(Error::Activation(format!(
          "{err}; and a cleanup panicked during rollback: {}",
          crate::error::panic_message(&*p)
        )));
      }
    }
    result
  }

  pub fn activate(&mut self, ctx: &mut ActivationCtx<'_>, input: &Var) -> Result<Step> {
    if self.current.is_none() {
      let root = self.root.expect("live engine");
      self.frames.get_mut(root).expect("root").value = input.clone();
      self.current = Some(root);
      self.current_scope = None;
    }
    let mut completed = None;
    self.own_locals = &mut *ctx.locals;
    // Resuming inside an invocation: its locals are the ones addressed.
    let scope = self.current_scope;
    self.switch_scope(scope, ctx);
    // The runtime's steps carry no meter (a compose-time evaluation's
    // fuel and budgets), and only a locating evaluation's record where a
    // failure passed (`failure_path`): three instantiations, the runtime's
    // with none of that code or the registers it would take.
    let result = match ctx.meter {
      None => self.steps::<false, false>(ctx, &mut completed),
      Some(meter) if meter.locating => self.steps::<true, true>(ctx, &mut completed),
      Some(_) => self.steps::<true, false>(ctx, &mut completed),
    };
    // The activation ends with the instance's own locals addressed.
    self.switch_scope(None, ctx);
    self.own_locals = std::ptr::slice_from_raw_parts_mut(std::ptr::null_mut(), 0);
    result
  }

  /// Steps the current frame until the activation finishes. `METERED`: a
  /// compose-time evaluation, charged to its meter; `LOCATE`: one that
  /// also records where a failure passed. Out of line, one function per
  /// instantiation (the runtime's is `steps::<false, false>`).
  #[inline(never)]
  fn steps<const METERED: bool, const LOCATE: bool>(
    &mut self,
    ctx: &mut ActivationCtx<'_>,
    completed: &mut Option<Result<Step>>,
  ) -> Result<Step> {
    loop {
      let h = self.current.expect("active frame");
      if let Next::Finish(result) = self.step::<METERED, LOCATE>(h, ctx, completed) {
        return result;
      }
    }
  }

  /// Points `ctx.locals` at the locals of `scope`: a function frame's
  /// invocation buffer, or the instance's own for `None`.
  ///
  /// SAFETY contract: `ctx.locals` is handed in for the activation as the
  /// instance's own locals, which outlive it; an invocation's buffer is
  /// owned by a frame that is an ancestor of (or is) the current frame,
  /// and such a frame is never removed while it is current (frames are
  /// removed only below a completed call site, or when a site replaces its
  /// own children before entering them); an invocation's locals never
  /// resize. So the pointer stays valid for as long as it is addressed,
  /// and the next switch or the end of the activation replaces it.
  fn switch_scope(&mut self, scope: Option<Handle>, ctx: &mut ActivationCtx<'_>) {
    let locals: *mut [Var] = match scope {
      None => self.own_locals,
      Some(s) => self
        .frames
        .get_mut(s)
        .expect("scope")
        .locals_mut()
        .as_mut_slice(),
    };
    // SAFETY: see the contract above.
    ctx.locals = unsafe { &mut *locals };
  }

  /// Makes `child` the current frame with `input`, at call depth `depth`.
  #[inline(always)]
  fn enter_child<const METERED: bool>(
    &mut self,
    h: Handle,
    child: Handle,
    input: Var,
    depth: u32,
    ctx: &mut ActivationCtx<'_>,
  ) -> Next {
    let child_frame = self.frames.get_mut(child).expect("live child");
    debug_assert!(child_frame.parent.is_none());
    child_frame.parent = Some(h);
    child_frame.call_depth = depth;
    if METERED && let Some(meter) = ctx.meter {
      meter.note_depth(depth as usize);
    }
    child_frame.value = input;
    let scope = child_frame.scope;
    self.current = Some(child);
    if scope != self.current_scope {
      self.current_scope = scope;
      self.switch_scope(scope, ctx);
    }
    Next::Continue
  }

  /// Enters a child of a `Control::Custom` composite (node `node` of frame
  /// `h`) with the input its `resume` chose. Out of line: rare, and the
  /// step stays small without it.
  #[inline(never)]
  fn enter_custom(
    &mut self,
    h: Handle,
    node: usize,
    child: Handle,
    ctx: &mut ActivationCtx<'_>,
  ) -> Next {
    let f = self.frames.get_mut(h).expect("live frame");
    let depth = f.call_depth;
    let State::Control(c) = &mut f.states[node] else {
      unreachable!("a composite")
    };
    let custom = c.custom.as_mut().expect("a composite's state");
    let input = std::mem::replace(&mut custom.input, Var::None);
    self.enter_child::<true>(h, child, input, depth, ctx)
  }

  /// One step of the current frame: dispatches its current node, or
  /// delivers a child's completion to it. Inlined into the activation loop
  /// (its only caller): a call per step is measurable.
  #[inline(always)]
  fn step<const METERED: bool, const LOCATE: bool>(
    &mut self,
    h: Handle,
    ctx: &mut ActivationCtx<'_>,
    completed: &mut Option<Result<Step>>,
  ) -> Next {
    // The meter, read only through this: `None` in the runtime's
    // instantiation, so its paths fold away.
    let meter = if METERED { ctx.meter } else { None };
    // No registry lookup or compiled-handle cloning on the usual hot path:
    // one frame lookup per step. A call site asks to be prepared at phase 0
    // (`Request::Prepare`), which takes a step of its own; a call to a
    // straight-line body, and a `Repeat` of one, run inside the VM segment.
    let f = self.frames.get_mut(h).expect("live frame");
    let index = f.pc();
    let flow = f.flow();
    // A compose-time evaluation pays for every step that starts work (a
    // completion only delivers work already paid for); out of fuel, the
    // frame fails like a failed segment.
    let mut vm_error = match meter {
      Some(meter) if completed.is_none() => {
        if LOCATE {
          // A failure before the VM runs (out of fuel, a constructor)
          // is at this instruction; a run moves it along.
          meter.at(index);
        }
        meter.charge(1).err()
      }
      _ => None,
    };
    // The VM runs every instruction it can, leaf or composite, as one
    // segment; it stops before a node that activates through its shard, a
    // composite without a VM form, or a call site that is not ready, and
    // those take the paths below. A segment that made no progress (a call
    // site not ready) falls through to the composite path of that node.
    let vm = vm_error.is_none()
      && completed.is_none()
      && index < flow.code.len()
      && !matches!(flow.code[index].op, crate::inline::Op::Fallback);
    let f = if vm {
      let value = std::mem::replace(&mut f.value, Var::None);
      let depth = f.call_depth as usize;
      let calls = EngineCalls {
        frames: &mut self.frames,
        h,
        revision: ctx.reload_revision(),
        max_depth: ctx.max_call_depth,
        depth,
      };
      // One runner call per step, as before: a segment stops at a
      // constructor or after one, and the next step continues it.
      let result = if flow.code[index].is_constructor() {
        crate::inline::construct(&flow.code, index, value, ctx.locals, ctx.mesh_frame, meter)
      } else if let Some(meter) = meter {
        let calls = crate::inline::Metered {
          inner: calls,
          meter,
        };
        crate::inline::run(&flow.code, index, value, ctx.locals, ctx.mesh_frame, &calls)
      } else {
        crate::inline::run(&flow.code, index, value, ctx.locals, ctx.mesh_frame, &calls)
      };
      let f = self.frames.get_mut(h).expect("live frame");
      match result {
        Ok((pc, value)) => {
          f.value = value;
          if pc != index {
            f.pc = pc as u32;
            return Next::Continue;
          }
        }
        Err(err) => vm_error = Some(err),
      }
      f
    } else {
      f
    };
    // A completed stateless call: a recursive site releases its frames (a
    // group's depth varies per call); any other site keeps them for the
    // next call and resets what lives for one invocation (the invocation's
    // values were released as its frame completed).
    let mut release = Vec::new();
    let mut reset = None;
    // For a call being entered: whether the callee ignores its input (so the
    // enter path asks the node nothing).
    let mut call_entry: Option<bool> = None;
    // A failed segment fails the frame from the instruction it started at,
    // which may be flat control code standing for no node: decided before
    // any node lookup. Otherwise the instruction the VM stopped at stands
    // for a node, the one activated or dispatched here.
    let node = if vm_error.is_none() {
      flow.node_at(index)
    } else {
      usize::MAX
    };
    let result = if let Some(err) = vm_error.take() {
      // The meter holds the instruction the run failed at (`Meter::at`).
      if LOCATE {
        let meter = meter.expect("a located activation is metered");
        record(meter, Location::Frame(f.code.clone(), meter.pc() as u32));
      }
      Request::Complete(Err(err))
    } else if index == flow.code.len() {
      Request::Complete(Ok(Step::Next(std::mem::replace(&mut f.value, Var::None))))
    } else if let State::Control(c) = &mut f.states[node] {
      self.dispatches += 1;
      let control = flow.nodes[node].control().expect("composite");
      let stateless_call = match &control {
        Control::Call(call) if !call.stateful() => {
          Some(matches!(call.target, CallTarget::Lazy { .. }))
        }
        // Its block's frame stays, its native state reset.
        Control::Run(_) => Some(false),
        _ => None,
      };
      if let Control::Call(call) = &control {
        call_entry = Some(call.def().ignores_input());
      }
      let child_failed = LOCATE && matches!(completed, Some(Err(_)));
      let dispatched = dispatch::<METERED, LOCATE>(
        control,
        c,
        ctx,
        &f.value,
        completed.take(),
        (&flow.nodes, node),
      );
      let result = match dispatched {
        Ok(Dispatch::Enter(i)) => Request::Enter(c.children[i]),
        Ok(Dispatch::EnterCustom(i)) => Request::EnterCustom(c.children[i]),
        Ok(Dispatch::Complete(result)) => Request::Complete(result),
        Ok(Dispatch::Prepare) => Request::Prepare,
        Err(err) => Request::Complete(Err(err)),
      };
      if LOCATE {
        let meter = meter.expect("a located activation is metered");
        match &result {
          Request::Complete(Err(_)) => record(meter, Location::Frame(f.code.clone(), index as u32)),
          // The composite handled its child's failure.
          _ if child_failed => meter.trace.borrow_mut().clear(),
          _ => {}
        }
      }
      if let Request::Complete(_) = &result {
        c.reset();
        match stateless_call {
          Some(true) => release = std::mem::take(&mut c.children),
          Some(false) if c.resets => reset = c.children.first().copied(),
          _ => {}
        }
      }
      result
    } else {
      debug_assert!(completed.is_none());
      let State::Leaf(state) = &mut f.states[node] else {
        unreachable!()
      };
      let result = flow.nodes[node].activate(state.as_mut(), ctx, &f.value);
      if LOCATE && result.is_err() {
        let meter = meter.expect("a located activation is metered");
        record(meter, Location::Frame(f.code.clone(), index as u32));
      }
      Request::Complete(result)
    };
    let next = match result {
      Request::Prepare => {
        self.prepare_and_enter::<METERED, LOCATE>(h, node, flow, call_entry, index, ctx, completed)
      }
      Request::Enter(child) => {
        let input = if call_entry == Some(true) {
          Var::None
        } else {
          f.value.clone()
        };
        let depth = f.call_depth + u32::from(call_entry.is_some());
        self.enter_child::<METERED>(h, child, input, depth, ctx)
      }
      Request::EnterCustom(child) => self.enter_custom(h, node, child, ctx),
      Request::Complete(Ok(Step::Suspend)) => Next::Finish(Ok(Step::Suspend)),
      Request::Complete(Ok(Step::Next(value))) if index < flow.code.len() => {
        f.pc += 1;
        f.value = value;
        Next::Continue
      }
      Request::Complete(result) => {
        f.pc = 0;
        f.value = Var::None;
        let leaving_function = matches!(f.code, Code::Function(_));
        // An invocation's or a block's frame runs on another scope than
        // its parent (the caller's, or the callee's running the block).
        let leaving_scope = leaving_function || matches!(f.code, Code::Block(..));
        // Every exit of an invocation passes here, its frame in hand.
        if leaving_function {
          f.release_locals();
        } else if !matches!(result, Ok(Step::Next(_))) {
          // Ended other than at the end of its code (which ran its
          // `Clear`s): its hidden slots' values end here.
          release_slots(flow, ctx.locals);
        }
        let own_scope = f.scope;
        self.current = f.parent.take();
        match self.current {
          None => {
            self.current_scope = None;
            Next::Finish(result)
          }
          Some(parent) => {
            // A child shares its parent's scope unless it is an invocation
            // frame, whose parent is the caller, or a block's.
            if leaving_scope {
              let scope = self.frames.get(parent).expect("parent frame").scope;
              self.current_scope = scope;
              self.switch_scope(scope, ctx);
            } else {
              debug_assert_eq!(own_scope, self.current_scope);
            }
            *completed = Some(result);
            Next::Continue
          }
        }
      }
    };
    if !release.is_empty() || reset.is_some() {
      self.end_invocations(release, reset, ctx);
    }
    next
  }

  /// A call site or `Run` at phase 0 (`Request::Prepare`): prepares the
  /// invocation and enters it in this step, or fails the frame. Out of
  /// line: a site prepares once, and the step stays small without it.
  #[inline(never)]
  #[allow(clippy::too_many_arguments)]
  fn prepare_and_enter<const METERED: bool, const LOCATE: bool>(
    &mut self,
    h: Handle,
    node: usize,
    flow: &CompiledFlow,
    call_entry: Option<bool>,
    index: usize,
    ctx: &mut ActivationCtx<'_>,
    completed: &mut Option<Result<Step>>,
  ) -> Next {
    let meter = if METERED { ctx.meter } else { None };
    // The call site is read through the frame's flow, whose reference
    // is not tied to the frame borrow (the frame stays current).
    let prepared = match flow.nodes[node].control() {
      Some(Control::Call(call)) => self
        .prepare_function_call(h, call, ctx)
        .map(|()| (1, call_entry == Some(true))),
      Some(Control::Run(run)) => match self.prepare_run(h, node, run, ctx) {
        Ok(true) => return Next::Continue,
        Ok(false) => Ok((0, run.ignores_input)),
        Err(err) => Err(err),
      },
      _ => unreachable!("only a call or a Run asks to be prepared"),
    };
    match prepared {
      Ok((depth, ignores_input)) => {
        // Prepared: entered in this step, not the next (`dispatch`
        // would answer `enter(c, 0, 2)`).
        let f = self.frames.get_mut(h).expect("live frame");
        let State::Control(c) = &mut f.states[node] else {
          unreachable!("a call site is a composite")
        };
        c.phase = 2;
        let child = c.children[0];
        let input = if ignores_input {
          Var::None
        } else {
          f.value.clone()
        };
        let depth = f.call_depth + depth;
        self.enter_child::<METERED>(h, child, input, depth, ctx)
      }
      Err(err) => {
        let f = self.frames.get_mut(h).expect("live frame");
        if LOCATE {
          let meter = meter.expect("a located activation is metered");
          record(meter, Location::Frame(f.code.clone(), index as u32));
        }
        if let State::Control(c) = &mut f.states[node] {
          c.reset();
        }
        f.pc = 0;
        f.value = Var::None;
        self.current = f.parent.take();
        match self.current {
          None => {
            self.current_scope = None;
            Next::Finish(Err(err))
          }
          Some(parent) => {
            let scope = self.frames.get(parent).expect("parent frame").scope;
            self.current_scope = scope;
            self.switch_scope(scope, ctx);
            *completed = Some(Err(err));
            Next::Continue
          }
        }
      }
    }
  }

  /// The end of a step that completed a stateless call: its native state
  /// lives for the invocation (golden path §3.4), cleaned up on return,
  /// failure, Stop or Restart. Out of line, as most steps have none.
  #[inline(never)]
  fn end_invocations(
    &mut self,
    release: Vec<Handle>,
    reset: Option<Handle>,
    ctx: &mut ActivationCtx<'_>,
  ) {
    let mut cleanup = CleanupCtx {
      instance: ctx.instance(),
    };
    for child in release {
      self.cleanup_tree(child, &mut cleanup);
    }
    if let Some(child) = reset {
      self.reset_invocation(child, &mut cleanup);
    }
  }
}

// A child receives the parent's original input. Returning errors and signals
// through the same completion channel unwinds each continuation exactly once.
enum Request {
  Enter(Handle),
  EnterCustom(Handle),
  Complete(Result<Step>),
  /// A call site at phase 0: the step prepares the invocation (which
  /// needs the engine, not the continuation) and dispatches again.
  Prepare,
}
/// What a composite's dispatch asks of its step: `Request` with the child
/// by index.
enum Dispatch {
  Enter(usize),
  /// Enter a child of a `Control::Custom` composite, with the input it
  /// left in its state (`Custom::input`): no value travels through here,
  /// which keeps the step's own frame small.
  EnterCustom(usize),
  Complete(Result<Step>),
  Prepare,
}
fn next(value: Var) -> Result<Dispatch> {
  Ok(Dispatch::Complete(Ok(Step::Next(value))))
}
fn enter(c: &mut Continuation, child: usize, phase: u32) -> Result<Dispatch> {
  c.phase = phase;
  Ok(Dispatch::Enter(child))
}

/// Dispatches the composite, running every child flow it enters that is
/// leaf code (`CompiledFlow::leaf`) right here, on the current locals, and
/// dispatching its completion in the same step: a predicate, a `Sub`,
/// `Once` or `Maybe` body, a branch costs no frame. A child that must
/// suspend, holds a site, loop, branch or constructor, or a call (its own
/// scope) is entered as a frame.
#[inline(always)]
fn dispatch<const METERED: bool, const LOCATE: bool>(
  control: Control<'_>,
  c: &mut Continuation,
  ctx: &mut ActivationCtx<'_>,
  input: &Var,
  mut completion: Option<Result<Step>>,
  // The composite's node, read only to locate a failure (`LOCATE`).
  (nodes, node): (&[Arc<dyn CompiledNode>], usize),
) -> Result<Dispatch> {
  let meter = if METERED { ctx.meter } else { None };
  loop {
    let child_failed = LOCATE && matches!(completion, Some(Err(_)));
    let dispatched = dispatch_once(control, c, ctx, input, completion.take())?;
    if child_failed && !matches!(dispatched, Dispatch::Complete(Err(_))) {
      // The composite handled the failure of a child it ran here.
      let meter = meter.expect("a located activation is metered");
      meter.trace.borrow_mut().clear();
    }
    let Dispatch::Enter(i) = dispatched else {
      return Ok(dispatched);
    };
    if matches!(control, Control::Call(_) | Control::Run(_)) {
      return Ok(dispatched);
    }
    let flow = child_flow(&control, i);
    if !flow.leaf {
      return Ok(dispatched);
    }
    let result = match meter {
      Some(meter) if LOCATE => {
        let result = crate::inline::run(
          &flow.code,
          0,
          input.clone(),
          ctx.locals,
          ctx.mesh_frame,
          &crate::inline::Metered {
            inner: crate::inline::NoCalls,
            meter,
          },
        );
        if result.is_err() {
          record(
            meter,
            Location::Leaf(nodes[node].clone(), i as u32, meter.pc() as u32),
          );
        }
        result
      }
      None => crate::inline::run(
        &flow.code,
        0,
        input.clone(),
        ctx.locals,
        ctx.mesh_frame,
        &crate::inline::NoCalls,
      ),
      Some(meter) => crate::inline::run(
        &flow.code,
        0,
        input.clone(),
        ctx.locals,
        ctx.mesh_frame,
        &crate::inline::Metered {
          inner: crate::inline::NoCalls,
          meter,
        },
      ),
    };
    if result.is_err() {
      release_slots(flow, ctx.locals);
    }
    completion = Some(match result {
      Ok((_, value)) => Ok(Step::Next(value)),
      Err(err) => Err(err),
    });
  }
}
fn boolean(v: Var) -> Result<bool> {
  match v {
    Var::Bool(b) => Ok(b),
    _ => Err(Error::Activation("predicate did not output a Bool".into())),
  }
}

/// Out of line: the match is large and inlining it into the step costs
/// the step more than the call (measured on the entity workload).
#[inline(never)]
fn dispatch_once(
  control: Control<'_>,
  c: &mut Continuation,
  ctx: &mut ActivationCtx<'_>,
  input: &Var,
  completion: Option<Result<Step>>,
) -> Result<Dispatch> {
  if let Control::Custom(custom) = control {
    return dispatch_custom(custom.as_ref(), c, ctx, input, completion);
  }
  let value = match completion {
    None => None,
    Some(result) => {
      if matches!(control, Control::Once(_)) && result.is_ok() {
        c.once_done = true;
      }
      let step = result?;
      match step {
        Step::Next(v) => Some(v),
        // Return exits the nearest named invocation with its value.
        Step::Return(v) if matches!(control, Control::Call(_)) => return next(v),
        other => return Ok(Dispatch::Complete(Ok(other))),
      }
    }
  };
  match control {
    Control::Call(_) | Control::Match(_) if value.is_some() => next(value.unwrap()),
    Control::Call(_) if c.phase == 0 => {
      c.phase = 1;
      Ok(Dispatch::Prepare)
    }
    Control::Call(_) => enter(c, 0, 2),
    Control::Match(m) => enter(c, m.find(input, ctx.meter)?, 1),
    Control::Sub(_) => {
      if value.is_some() {
        next(input.clone())
      } else {
        enter(c, 0, 1)
      }
    }
    Control::Once(_) => {
      if c.once_done {
        next(input.clone())
      } else {
        enter(c, 0, 1)
      }
    }
    Control::When(_) | Control::While(_) | Control::If(_) | Control::Repeat(_) => {
      unreachable!("lowered to flat code at compose")
    }
    Control::Run(r) => match value {
      Some(value) => next(if r.passthrough {
        input.clone()
      } else if r.discards_output {
        Var::None
      } else {
        value
      }),
      None if c.phase == 0 => {
        c.phase = 1;
        Ok(Dispatch::Prepare)
      }
      None => enter(c, 0, 2),
    },
    Control::Custom(_) => unreachable!(),
    Control::Conditions(conditions) => {
      let mut index = c.phase as usize;
      if let Some(value) = value
        && boolean(value)? == conditions.stop_on
      {
        return next(Var::Bool(conditions.stop_on));
      }
      // phase is the next condition, saved before entering a predicate.
      while index < conditions.conditions.len() {
        let value = match conditions.conditions[index] {
          Condition::Const(b) => b,
          Condition::Bound(binding) => matches!(ctx.get(binding), Var::Bool(true)),
          Condition::Flow(i) => return enter(c, i, (index + 1) as u32),
        };
        if value == conditions.stop_on {
          return next(Var::Bool(conditions.stop_on));
        }
        index += 1;
      }
      next(Var::Bool(!conditions.stop_on))
    }
  }
}

/// A `Control::Custom` composite: its `resume` decides, with a child's
/// output or error; a `Stop`, `Restart` or `Return` passes through. Its
/// state goes back to its default whenever the node completes. Out of the
/// step's way: what it carries (the child's input, a leaf child run here)
/// would cost every other composite stack space in the step.
fn dispatch_custom(
  custom: &dyn CustomControl,
  c: &mut Continuation,
  ctx: &mut ActivationCtx<'_>,
  input: &Var,
  mut completion: Option<Result<Step>>,
) -> Result<Dispatch> {
  let held = c.custom.as_mut().expect("a composite's state");
  loop {
    let state = held.state.as_mut();
    let outcome = match completion.take() {
      None => None,
      Some(Ok(Step::Next(value))) => Some(Ok(value)),
      Some(Ok(signal)) => {
        custom.reset_state(state);
        return Ok(Dispatch::Complete(Ok(signal)));
      }
      Some(Err(err)) => Some(Err(err)),
    };
    match custom.resume(state, ctx, input, outcome) {
      Ok(ControlStep::Enter(child, child_input)) => {
        let Some(flow) = custom.flows().get(child) else {
          custom.reset_state(state);
          return Err(Error::Activation(format!(
            "a composite entered flow {child} of {}",
            custom.flows().len()
          )));
        };
        // Leaf code runs right here, at run time (a compose-time
        // evaluation never reaches a host composite).
        if !flow.leaf || ctx.meter.is_some() {
          held.input = child_input;
          return Ok(Dispatch::EnterCustom(child));
        }
        let result = crate::inline::run(
          &flow.code,
          0,
          child_input,
          ctx.locals,
          ctx.mesh_frame,
          &crate::inline::NoCalls,
        );
        if result.is_err() {
          release_slots(flow, ctx.locals);
        }
        completion = Some(result.map(|(_, value)| Step::Next(value)));
      }
      Ok(ControlStep::Complete(value)) => {
        custom.reset_state(state);
        return next(value);
      }
      Err(err) => {
        custom.reset_state(state);
        return Err(err);
      }
    }
  }
}

/// The engine's side of the VM's call sites (`inline::VmCalls`)
/// for one frame. Copyable: a nested run gets its own for the frame it
/// runs in (a callee's kept frame, a loop's child frame).
#[derive(Clone, Copy)]
struct EngineCalls {
  /// The arena, outliving every run it serves (the step holds the engine).
  frames: *mut Arena<Frame>,
  h: Handle,
  revision: u64,
  max_depth: usize,
  depth: usize,
}

impl EngineCalls {
  // A raw-pointer view by design (see the safety note); the lint wants
  // `&mut self`, which a copyable value lent to a run cannot offer.
  #[allow(clippy::mut_from_ref)]
  fn arena(&self) -> &mut Arena<Frame> {
    // SAFETY: the step that created this value holds `&mut Engine` for the
    // whole run; the run addresses frames only through these calls, one at
    // a time, and the returned borrows never outlive the run (callee code
    // and locals are handed out as raw or `'static` references with the
    // contract documented at `VmCallee`).
    unsafe { &mut *self.frames }
  }

  /// The kept callee frame of the call site at `site`, with the site
  /// itself: `None` unless the site is stateless, non-recursive, at phase
  /// 0, at the current revision, with a kept frame whose body is
  /// straight-line, and within the call depth.
  fn callee(
    &self,
    site: usize,
  ) -> Option<(
    &'static CallCompiled,
    Handle,
    &'static CompiledFunction,
    *mut [Var],
  )> {
    let frames = self.arena();
    let f = frames.get(self.h)?;
    let State::Control(c) = f.states.get(f.flow().node_at(site))? else {
      return None;
    };
    if !c.is_call || c.phase != 0 || c.revision != self.revision || self.depth >= self.max_depth {
      return None;
    }
    let call = c.vm?;
    let child = *c.children.first()?;
    let frame = frames.get_mut(child)?;
    let Code::Function(body) = &frame.code else {
      return None;
    };
    let body: *const CompiledFunction = Arc::as_ptr(body);
    let locals: *mut [Var] = frame.invocation.as_mut()?.locals.as_mut_slice();
    // SAFETY (lifetimes): the site lives in the flow of frame `h` and the
    // body in the kept frame, both outliving the run this serves (it ends
    // with the step; see `Continuation::vm`).
    let (call, body) = unsafe { (&*call.as_ptr(), &*body) };
    Some((call, child, body, locals))
  }
}

impl crate::inline::VmCalls for EngineCalls {
  fn enter(&self, site: usize) -> Option<crate::inline::VmCallee<Self>> {
    let (call, child, body, locals) = self.callee(site)?;
    let calls = EngineCalls {
      frames: self.frames,
      h: child,
      revision: self.revision,
      max_depth: self.max_depth,
      depth: self.depth + 1,
    };
    if !body.vm_leaf && !crate::inline::vm_ready(&body.flow.code, &calls) {
      return None;
    }
    Some(crate::inline::VmCallee {
      code: &body.flow.code,
      constructors: body
        .flow
        .code
        .iter()
        .any(crate::inline::Instruction::is_constructor),
      locals,
      param_slots: &body.param_slots,
      input_slot: body.input_slot,
      args: &call.args,
      ignores_input: call.def().ignores_input(),
      calls,
    })
  }

  fn depth(&self) -> usize {
    self.depth
  }

  fn failed_in(&self, pc: usize, meter: &crate::compose_time::Meter) {
    let code = self.arena().get(self.h).expect("live frame").code.clone();
    record(meter, Location::Frame(code, pc as u32));
  }

  fn site_ready(&self, site: usize) -> bool {
    let Some((_, child, body, _)) = self.callee(site) else {
      return false;
    };
    if body.vm_leaf {
      return true;
    }
    let calls = EngineCalls {
      frames: self.frames,
      h: child,
      revision: self.revision,
      max_depth: self.max_depth,
      depth: self.depth + 1,
    };
    crate::inline::vm_ready(&body.flow.code, &calls)
  }
}

#[cfg(test)]
mod layout {
  #[test]
  fn a_frame_is_128_bytes_on_64_bit_targets() {
    if cfg!(target_pointer_width = "64") {
      assert_eq!(std::mem::size_of::<super::Frame>(), 128);
      assert_eq!(std::mem::size_of::<Option<super::Frame>>(), 128);
      assert_eq!(std::mem::size_of::<super::State>(), 16);
      // 56 before the state of host composites (`custom`, one word).
      assert_eq!(std::mem::size_of::<super::Continuation>(), 64);
    }
  }
}
