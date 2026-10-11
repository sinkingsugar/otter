//! Compose-selected builtins and a borrowed accumulator for uninterrupted runs.
//!
//! The pointer accumulator never escapes `run`. It refers to owned scratch,
//! an immutable instruction constant, a frame slot, or numeric result storage.
//! Frames are exclusively borrowed and cannot resize or suspend during a run.
//! Each operation consumes its input before replacing storage and reanchors the
//! accumulator after a write. A run ends before any arbitrary shard activation,
//! returning owned output so nested flows, host writes and suspension see a snapshot.

use std::any::{Any, TypeId};

use crate::compose::Binding;
use crate::compose_time::Meter;
use crate::error::{Error, Result};
use crate::shards::Operand;
use crate::shards::data;
use crate::shards::leaf::{self, LeafShard};
use crate::shards::math::{self, BinOp};
use crate::shards::values;
use crate::types::{Shape, Type};
use crate::var::{Float4, Table, Var, assign, assign_copy, assign_copy_owning, copy, seq_mut};

/// Opaque internal optimization hook. Ordinary shards use the default `None`.
#[doc(hidden)]
pub struct InlineOp(pub(crate) Op);

// Explicit tags keep dispatch a direct byte load rather than decoding niches.
//
// Two tiers (docs/runtime-performance-overview.md, lesson 9): the
// variants up to `LoopTest` are the hot tier, which `run` dispatches through
// a jump table and runs in place when their operands are plain; every
// variant after it is the slow tier, which `run` hands to `slow_op` through
// one call. Keep this order: a new operation goes after `LoopTest`, so the
// hot tier's tags, its jump table and its range check never change
// (`size::hot_tags_are_fixed`).
#[derive(Clone)]
#[repr(u8)]
pub(crate) enum Op {
  Fallback,
  Const(Var),
  GetLocal(SlotOffset),
  GetMesh(SlotOffset),
  Set(Binding),
  Inc(Binding),
  ConstDrop(Var),
  GetLocalDrop(SlotOffset),
  GetMeshDrop(SlotOffset),
  SetDrop(Binding),
  IncDrop(Binding),
  Take(Operand),
  /// A literal key resolved on a fixed table (golden path §7.3).
  TakeSlot(usize),
  AddIntConst(i64),
  AddIntBound(Binding),
  AddFloatConst(f64),
  AddFloatBound(Binding),
  AddFloat4Const(Float4),
  AddFloat4Bound(Binding),
  /// Passes the value through (`Keep`: its slot was set at frame creation).
  Pass,
  /// Control flow of a composite lowered to flat code at compose (`Repeat`,
  /// `While`, `When`, `If`): an unconditional jump to an instruction index.
  Jump(u32),
  /// Jumps when the value is `false`; a non-Bool value is an error.
  JumpIfNot(u32),
  /// Jumps when the value is `true`; a non-Bool value is an error.
  JumpIf(u32),
  /// A loop counter in a hidden slot: at zero jumps to the given end,
  /// otherwise counts down and falls through into the body.
  LoopTest(Binding, u32),
  // The slow tier: `slow_op`, out of line.
  Push(Binding),
  SeqMake(Vec<Operand>),
  /// A struct table: its shape and one operand per slot, in key order.
  TableMake(Shape, Vec<Operand>),
  /// Any other arithmetic (vectors, mixed numbers, subtract/multiply/
  /// divide): the shared numeric rule without a node activation.
  Arith(BinOp, Operand),
  /// An ordered comparison of numbers.
  Compare(Cmp, Operand),
  /// The language's equality (`Is`, or `IsNot` when negated).
  Equal(bool, Operand),
  /// `Count`. A function looping over its input counts it, and without
  /// this its call could not be inlined.
  Count,
  /// A call to a straight-line body from a stateless, non-recursive site:
  /// runs the callee's code on its kept locals right here (`VmCalls`
  /// supplies them), without an engine step. When the site is not ready
  /// (first call, a reload) the run stops before it and the engine enters
  /// the frame.
  VmCall,
  /// Ends the value lifetime of a slot an inlined call wrote (its input,
  /// an argument, a local of the callee): the slot is unset, so the
  /// caller's next mutation of a collection it shared is not a copy. A
  /// value the accumulator still reads moves into it.
  Clear(Binding),
}

/// The tag of the last hot-tier operation: `run` sends every tag past it to
/// `slow_op` (its match's catch-all arm).
#[cfg(test)]
const LAST_HOT: u8 = 23;

#[cfg(test)]
impl Op {
  fn tag(&self) -> u8 {
    // SAFETY: `#[repr(u8)]` puts the tag in the first byte.
    unsafe { *(self as *const Op as *const u8) }
  }
}

#[cfg(test)]
mod size {
  use super::*;

  #[test]
  fn an_instruction_is_48_bytes_in_release_layout() {
    // `Instruction` adds the output check in debug builds; the op itself is
    // one value plus a tag. A slow-tier operation must fit too: a larger
    // one goes behind a box, not into every instruction.
    assert_eq!(std::mem::size_of::<super::Op>(), 48);
  }

  /// The hot tier's tags are what `run`'s jump table indexes: a new
  /// operation goes after `LoopTest` and leaves them as they are.
  #[test]
  fn hot_tags_are_fixed() {
    let b = Binding::Local(0);
    let o = SlotOffset::new(0);
    let hot = [
      Op::Fallback,
      Op::Const(Var::None),
      Op::GetLocal(o),
      Op::GetMesh(o),
      Op::Set(b),
      Op::Inc(b),
      Op::ConstDrop(Var::None),
      Op::GetLocalDrop(o),
      Op::GetMeshDrop(o),
      Op::SetDrop(b),
      Op::IncDrop(b),
      Op::Take(Operand::Const(Var::None)),
      Op::TakeSlot(0),
      Op::AddIntConst(0),
      Op::AddIntBound(b),
      Op::AddFloatConst(0.0),
      Op::AddFloatBound(b),
      Op::AddFloat4Const(Float4([0.0; 4])),
      Op::AddFloat4Bound(b),
      Op::Pass,
      Op::Jump(0),
      Op::JumpIfNot(0),
      Op::JumpIf(0),
      Op::LoopTest(b, 0),
    ];
    for (tag, op) in hot.iter().enumerate() {
      assert_eq!(op.tag() as usize, tag, "{}", op.name());
    }
    assert_eq!(LAST_HOT as usize, hot.len() - 1);
    assert!(Op::Push(b).tag() > LAST_HOT);
    assert!(Op::Clear(b).tag() > LAST_HOT);
  }
}

/// Whether a flow's code runs through the VM from start to end: no node
/// activates through its shard. A call site (`VmCall`) and the jumps of a
/// flattened composite count as code; a composite that keeps its frames,
/// and a leaf without a VM form, is a `Fallback`.
pub(crate) fn straight_line(code: &[Instruction]) -> bool {
  code.iter().all(|i| !matches!(i.op, Op::Fallback))
}

/// Whether a flow's code is one `run` call that needs nothing from the
/// engine: straight-line, without a call site (which asks the engine for
/// the callee's frame) and without a constructor (which `run` stops at).
pub(crate) fn leaf_code(code: &[Instruction]) -> bool {
  code
    .iter()
    .all(|i| !matches!(i.op, Op::Fallback | Op::VmCall) && !i.is_constructor())
}

/// What the engine lends a run for the call sites and child flows of the
/// frame being run. Copyable, so a nested run gets its own for the frame
/// it runs in.
pub(crate) trait VmCalls: Copy {
  /// Whether runs meter their work (a compose-time evaluation,
  /// [`Metered`]). A constant, so the runtime's instantiation of `run`
  /// carries no metering at all.
  const METERED: bool = false;
  /// The callee of the call site at instruction `site`, when it can run
  /// here: a stateless, non-recursive site with a kept frame, the current
  /// revision, a straight-line body whose own sites are ready too, and
  /// within the call depth. `None` sends the run back to the engine.
  fn enter(&self, site: usize) -> Option<VmCallee<Self>>;
  /// The same test without entering: readiness of the site and, through
  /// it, of everything its body would run.
  fn site_ready(&self, site: usize) -> bool;
  /// The call depth of the frame being run.
  fn depth(&self) -> usize {
    0
  }
  /// The meter, when `METERED`.
  fn meter(&self) -> Option<&Meter> {
    None
  }
  /// A metered run of a callee failed at instruction `pc` of its code:
  /// called for the callee's level before the failure leaves the caller's
  /// run, to locate it (`stackless::failure_path`). Runtime runs never call
  /// it.
  #[cold]
  fn failed(&self, _pc: usize) {}
  /// `failed`, from a [`Metered`] run: where its meter keeps the location.
  #[cold]
  fn failed_in(&self, _pc: usize, _meter: &Meter) {}
}

/// A run of a compose-time evaluation: `C`'s calls, with every instruction
/// charged to the meter, and the allocations and traversals of the VM's
/// own operations checked against its limits.
#[derive(Clone, Copy)]
pub(crate) struct Metered<'m, C> {
  pub inner: C,
  pub meter: &'m Meter,
}

impl<C: VmCalls> VmCalls for Metered<'_, C> {
  const METERED: bool = true;

  fn enter(&self, site: usize) -> Option<VmCallee<Self>> {
    let callee = self.inner.enter(site)?;
    self.meter.note_depth(callee.calls.depth());
    Some(VmCallee {
      code: callee.code,
      constructors: callee.constructors,
      locals: callee.locals,
      param_slots: callee.param_slots,
      input_slot: callee.input_slot,
      args: callee.args,
      ignores_input: callee.ignores_input,
      calls: Metered {
        inner: callee.calls,
        meter: self.meter,
      },
    })
  }

  fn site_ready(&self, site: usize) -> bool {
    self.inner.site_ready(site)
  }

  fn depth(&self) -> usize {
    self.inner.depth()
  }

  fn meter(&self) -> Option<&Meter> {
    Some(self.meter)
  }

  fn failed(&self, pc: usize) {
    self.inner.failed_in(pc, self.meter);
  }
}

/// A callee a run enters: its code, its kept locals (a buffer that outlives
/// the run and never resizes, see the engine's `switch_scope` contract)
/// and how to bind the arguments.
pub(crate) struct VmCallee<C> {
  pub code: &'static [Instruction],
  /// Whether the code holds a constructor (then it runs through
  /// `run_segment`, otherwise through `run` directly).
  pub constructors: bool,
  pub locals: *mut [Var],
  pub param_slots: &'static [usize],
  pub input_slot: usize,
  pub args: &'static [Operand],
  pub ignores_input: bool,
  pub calls: C,
}

/// No engine: calls are never ready, child flows have no frame. For code
/// that needs none (`leaf_code`), and tests.
#[derive(Clone, Copy)]
pub(crate) struct NoCalls;

impl VmCalls for NoCalls {
  fn enter(&self, _: usize) -> Option<VmCallee<Self>> {
    None
  }
  fn site_ready(&self, _: usize) -> bool {
    false
  }
}

/// Whether every call site in `code` can run here now: checked before a
/// callee body runs, so a body never runs halfway and then needs the
/// engine (a reload, the one thing that changes readiness, happens between
/// ticks). A frame's own code needs no such check: the engine continues it
/// from the instruction the run stopped at.
pub(crate) fn vm_ready<C: VmCalls>(code: &[Instruction], calls: &C) -> bool {
  for (i, instruction) in code.iter().enumerate() {
    if matches!(instruction.op, Op::VmCall) && !calls.site_ready(i) {
      return false;
    }
  }
  true
}

/// Runs `code` from `from` as far as the VM goes: constructors through
/// `construct`, everything else through `run`, until the end of the code
/// or an instruction the VM cannot run (a `Fallback`, or a call site that
/// is not ready), which it stops before.
pub(crate) fn run_segment<C: VmCalls>(
  code: &[Instruction],
  from: usize,
  input: Var,
  locals: &mut [Var],
  mesh: &mut [Var],
  calls: &C,
) -> Result<(usize, Var)> {
  let (mut pc, mut value) = (from, input);
  loop {
    let before = pc;
    (pc, value) = if pc < code.len() && code[pc].is_constructor() {
      match construct(code, pc, value, locals, mesh, calls.meter()) {
        Ok(next) => next,
        Err(err) => {
          if let Some(meter) = calls.meter() {
            meter.at(pc);
          }
          return Err(err);
        }
      }
    } else {
      run(code, pc, value, locals, mesh, calls)?
    };
    if pc == code.len() || pc == before {
      return Ok((pc, value));
    }
  }
}

/// Which ordering a comparison instruction tests for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cmp {
  Less,
  LessEqual,
  More,
  MoreEqual,
}

impl Cmp {
  fn holds(self, o: std::cmp::Ordering) -> bool {
    match self {
      Cmp::Less => o.is_lt(),
      Cmp::LessEqual => o.is_le(),
      Cmp::More => o.is_gt(),
      Cmp::MoreEqual => o.is_ge(),
    }
  }
}

impl Op {
  /// The instruction's kind, for tests and diagnostics.
  pub(crate) fn name(&self) -> &'static str {
    match self {
      Op::Fallback => "fallback",
      Op::Const(_) => "const",
      Op::GetLocal(_) => "get-local",
      Op::GetMesh(_) => "get-mesh",
      Op::Set(_) => "set",
      Op::Inc(_) => "inc",
      Op::ConstDrop(_) => "const-drop",
      Op::GetLocalDrop(_) => "get-local-drop",
      Op::GetMeshDrop(_) => "get-mesh-drop",
      Op::SetDrop(_) => "set-drop",
      Op::IncDrop(_) => "inc-drop",
      Op::Take(_) => "take",
      Op::TakeSlot(_) => "take-slot",
      Op::Push(_) => "push",
      Op::SeqMake(_) => "seq-make",
      Op::TableMake(..) => "table-make",
      Op::AddIntConst(_) => "add-int-const",
      Op::AddIntBound(_) => "add-int-bound",
      Op::AddFloatConst(_) => "add-float-const",
      Op::AddFloatBound(_) => "add-float-bound",
      Op::AddFloat4Const(_) => "add-float4-const",
      Op::AddFloat4Bound(_) => "add-float4-bound",
      Op::Arith(..) => "arith",
      Op::Compare(..) => "compare",
      Op::Equal(..) => "equal",
      Op::Count => "count",
      Op::Pass => "pass",
      Op::VmCall => "vm-call",
      Op::Jump(_) => "jump",
      Op::JumpIfNot(_) => "jump-if-not",
      Op::JumpIf(_) => "jump-if",
      Op::LoopTest(..) => "loop-test",
      Op::Clear(_) => "clear",
    }
  }
}

/// Byte offset constructed only from a Var slot index. Its alignment is thus
/// guaranteed independently of the frame chosen by an instance.
#[derive(Clone, Copy)]
pub(crate) struct SlotOffset(usize);

impl SlotOffset {
  fn new(index: usize) -> Self {
    Self(
      index
        .checked_mul(size_of::<Var>())
        .expect("inline slot offset overflow"),
    )
  }

  fn index(self) -> usize {
    self.0 / size_of::<Var>()
  }
}

impl Op {
  pub(crate) fn get(binding: Binding) -> Self {
    match binding {
      Binding::Local(index) => Self::GetLocal(SlotOffset::new(index)),
      Binding::Mesh(index) => Self::GetMesh(SlotOffset::new(index)),
    }
  }
}

#[derive(Clone)]
pub(crate) struct Instruction {
  pub op: Op,
  #[cfg(any(debug_assertions, feature = "output-checks"))]
  check: (&'static str, Type),
}

impl Instruction {
  pub fn is_constructor(&self) -> bool {
    matches!(self.op, Op::SeqMake(_) | Op::TableMake(..))
  }

  pub fn new(op: Option<InlineOp>, _name: &'static str, _output: Type) -> Self {
    Self {
      op: op.map_or(Op::Fallback, |v| v.0),
      #[cfg(any(debug_assertions, feature = "output-checks"))]
      check: (_name, _output),
    }
  }

  /// Points a jump at `target` (compose, when the target is known).
  pub(crate) fn retarget(&mut self, target: u32) {
    match &mut self.op {
      Op::Jump(t) | Op::JumpIfNot(t) | Op::JumpIf(t) | Op::LoopTest(_, t) => *t = target,
      _ => unreachable!("only a jump is retargeted"),
    }
  }

  /// Whether the instruction replaces the accumulator without reading it:
  /// the value before it is dead there.
  pub(crate) fn ignores_accumulator(&self) -> bool {
    matches!(
      self.op,
      Op::Const(_)
        | Op::GetLocal(_)
        | Op::GetMesh(_)
        | Op::Inc(_)
        | Op::ConstDrop(_)
        | Op::GetLocalDrop(_)
        | Op::GetMeshDrop(_)
        | Op::IncDrop(_)
    )
  }

  /// A jump's target; `None` for any other instruction.
  pub(crate) fn jump_target(&self) -> Option<u32> {
    match self.op {
      Op::Jump(t) | Op::JumpIfNot(t) | Op::JumpIf(t) | Op::LoopTest(_, t) => Some(t),
      _ => None,
    }
  }

  /// Offsets a jump target by `base`: the instruction moved into a parent
  /// flow's code at that index.
  pub(crate) fn relocate(&mut self, base: u32) {
    if let Op::Jump(t) | Op::JumpIfNot(t) | Op::JumpIf(t) | Op::LoopTest(_, t) = &mut self.op {
      *t += base;
    }
  }

  /// Visits every local slot the instruction addresses (mesh slots are
  /// left alone): how an inlined callee's code is moved onto the caller's
  /// frame.
  fn for_each_local(&mut self, f: &mut dyn FnMut(&mut usize)) {
    fn binding(b: &mut Binding, f: &mut dyn FnMut(&mut usize)) {
      if let Binding::Local(i) = b {
        f(i);
      }
    }
    fn operand(o: &mut Operand, f: &mut dyn FnMut(&mut usize)) {
      if let Operand::Bound(b) = o {
        binding(b, f);
      }
    }
    match &mut self.op {
      Op::GetLocal(offset) | Op::GetLocalDrop(offset) => {
        let mut i = offset.index();
        f(&mut i);
        *offset = SlotOffset::new(i);
      }
      Op::Set(b)
      | Op::SetDrop(b)
      | Op::Inc(b)
      | Op::IncDrop(b)
      | Op::Push(b)
      | Op::AddIntBound(b)
      | Op::AddFloatBound(b)
      | Op::AddFloat4Bound(b)
      | Op::LoopTest(b, _)
      | Op::Clear(b) => binding(b, f),
      Op::Take(o) | Op::Arith(_, o) | Op::Compare(_, o) | Op::Equal(_, o) => operand(o, f),
      Op::SeqMake(ops) | Op::TableMake(_, ops) => {
        for o in ops {
          operand(o, f);
        }
      }
      Op::Fallback
      | Op::Const(_)
      | Op::ConstDrop(_)
      | Op::GetMesh(_)
      | Op::GetMeshDrop(_)
      | Op::TakeSlot(_)
      | Op::AddIntConst(_)
      | Op::AddFloatConst(_)
      | Op::AddFloat4Const(_)
      | Op::Pass
      | Op::Count
      | Op::VmCall
      | Op::Jump(_)
      | Op::JumpIfNot(_)
      | Op::JumpIf(_) => {}
    }
  }

  /// Moves every local slot the instruction addresses up by `base`.
  pub(crate) fn rebase_locals(&mut self, base: usize) {
    self.for_each_local(&mut |i| *i += base);
  }

  /// Calls `f` with every local slot the instruction addresses.
  pub(crate) fn visit_locals(&self, f: &mut dyn FnMut(usize)) {
    self.clone().for_each_local(&mut |i| f(*i));
  }

  /// Whether the instruction addresses local `slot`.
  pub(crate) fn mentions_local(&self, slot: usize) -> bool {
    let mut found = false;
    self.visit_locals(&mut |i| found |= i == slot);
    found
  }
}

/// Specialize cleanup at compose time. A segment enters with owned input;
/// Take/Push/generic vector arithmetic may introduce owned scratch again.
/// Once released, subsequent reanchoring instructions need no cleanup branch.
/// A jump target may be reached with owned scratch from any of its
/// predecessors, so the analysis restarts there.
pub(crate) fn lower_scratch_releases(code: &mut [Instruction]) {
  let mut targets = vec![false; code.len() + 1];
  for instruction in code.iter() {
    match instruction.op {
      Op::Jump(t) | Op::JumpIfNot(t) | Op::JumpIf(t) | Op::LoopTest(_, t) => {
        if let Some(target) = targets.get_mut(t as usize) {
          *target = true;
        }
      }
      _ => {}
    }
  }
  let mut may_own = true;
  for (pc, instruction) in code.iter_mut().enumerate() {
    if targets[pc] {
      may_own = true;
    }
    match &mut instruction.op {
      Op::Const(_) | Op::GetLocal(_) | Op::GetMesh(_) | Op::Set(_) | Op::Inc(_) if may_own => {
        instruction.op = match std::mem::replace(&mut instruction.op, Op::Fallback) {
          Op::Const(v) => Op::ConstDrop(v),
          Op::GetLocal(offset) => Op::GetLocalDrop(offset),
          Op::GetMesh(offset) => Op::GetMeshDrop(offset),
          Op::Set(b) => Op::SetDrop(b),
          Op::Inc(b) => Op::IncDrop(b),
          _ => unreachable!(),
        };
        may_own = false;
      }
      Op::ConstDrop(_)
      | Op::GetLocalDrop(_)
      | Op::GetMeshDrop(_)
      | Op::SetDrop(_)
      | Op::IncDrop(_) => may_own = false,
      Op::Fallback
      | Op::SeqMake(_)
      | Op::TableMake(..)
      | Op::Take(_)
      | Op::TakeSlot(_)
      | Op::Push(_)
      | Op::VmCall
      | Op::AddFloat4Const(_)
      | Op::AddFloat4Bound(_)
      | Op::Clear(_) => may_own = true,
      Op::Const(_)
      | Op::GetLocal(_)
      | Op::GetMesh(_)
      | Op::Set(_)
      | Op::Inc(_)
      | Op::AddIntConst(_)
      | Op::AddIntBound(_)
      | Op::AddFloatConst(_)
      | Op::AddFloatBound(_)
      // Results are numbers or booleans, written to the numeric slot.
      | Op::Arith(..)
      | Op::Compare(..)
      | Op::Equal(..)
      | Op::Count
      | Op::Pass
      | Op::Jump(_)
      | Op::JumpIfNot(_)
      | Op::JumpIf(_)
      | Op::LoopTest(..) => {}
    }
  }
}

/// Select by Rust implementation identity, never by a user-controlled name.
/// Only operations that need no activation state participate. Normal
/// lifecycle stays in nodes.
pub(crate) fn leaf<L: LeafShard>(c: &L::Compiled, output: Type) -> Option<InlineOp> {
  let id = TypeId::of::<L>();
  let c = c as &dyn Any;
  let op = if id == TypeId::of::<leaf::Const>() {
    Op::Const(c.downcast_ref::<Var>()?.clone())
  } else if id == TypeId::of::<leaf::Get>() {
    Op::get(*c.downcast_ref::<Binding>()?)
  } else if id == TypeId::of::<leaf::VarDecl>()
    || id == TypeId::of::<leaf::Bind>()
    || id == TypeId::of::<leaf::Update>()
  {
    Op::Set(*c.downcast_ref::<Binding>()?)
  } else if id == TypeId::of::<leaf::Inc>() {
    Op::Inc(*c.downcast_ref::<Binding>()?)
  } else if id == TypeId::of::<leaf::Keep>() {
    Op::Pass
  } else if id == TypeId::of::<data::Take>() {
    match c.downcast_ref::<data::TakeCode>()? {
      data::TakeCode::Key(key) => Op::Take(key.clone()),
      data::TakeCode::Slot(index) => Op::TakeSlot(*index),
    }
  } else if id == TypeId::of::<data::SeqMake>() {
    Op::SeqMake(c.downcast_ref::<Vec<Operand>>()?.clone())
  } else if id == TypeId::of::<data::TableMake>() {
    let code = c.downcast_ref::<data::TableCode>()?;
    Op::TableMake(code.shape, code.values.clone())
  } else if id == TypeId::of::<data::Push>() {
    Op::Push(*c.downcast_ref::<Binding>()?)
  } else if id == TypeId::of::<leaf::Add>() {
    match (output, c.downcast_ref::<Operand>()?) {
      (ty, Operand::Const(Var::Int(v))) if ty == Type::int() => Op::AddIntConst(*v),
      (ty, Operand::Bound(b)) if ty == Type::int() => Op::AddIntBound(*b),
      (ty, Operand::Const(Var::Float(v))) if ty == Type::float() => Op::AddFloatConst(*v),
      (ty, Operand::Bound(b)) if ty == Type::float() => Op::AddFloatBound(*b),
      (ty, Operand::Const(Var::Float4(v))) if ty == Type::float4() => Op::AddFloat4Const(*v),
      (ty, Operand::Bound(b)) if ty == Type::float4() => Op::AddFloat4Bound(*b),
      (_, operand) => Op::Arith(BinOp::Add, operand.clone()),
    }
  } else if id == TypeId::of::<math::Binary<math::SubOp>>() {
    Op::Arith(BinOp::Subtract, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<math::Binary<math::MulOp>>() {
    Op::Arith(BinOp::Multiply, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<math::Binary<math::DivOp>>() {
    Op::Arith(BinOp::Divide, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<leaf::IsLess>() {
    Op::Compare(Cmp::Less, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<leaf::IsMoreEqual>() {
    Op::Compare(Cmp::MoreEqual, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<values::Ordered<values::IsMoreSpec>>() {
    Op::Compare(Cmp::More, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<values::Ordered<values::IsLessEqualSpec>>() {
    Op::Compare(Cmp::LessEqual, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<values::Equality<values::IsSpec>>() {
    Op::Equal(false, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<values::Equality<values::IsNotSpec>>() {
    Op::Equal(true, c.downcast_ref::<Operand>()?.clone())
  } else if id == TypeId::of::<values::Pure<values::CountOp>>() {
    Op::Count
  } else {
    return None;
  };
  Some(InlineOp(op))
}

/// `Op::Count`, charged to the meter in a compose-time evaluation as the
/// shard is. Out of line: in place, even only for sequences, its arm moved
/// `run`'s blocks and cost the constructor cases 5 to 14 percent
/// (2026-10-10); it runs once per loop, not per element.
#[inline(never)]
fn count(value: &Var, meter: Option<&crate::compose_time::Meter>) -> Result<i64> {
  use values::PureOp;
  if let Var::Seq(items) = value {
    return Ok(items.len() as i64);
  }
  if let Some(meter) = meter {
    values::CountOp::meter(value, meter)?;
  }
  match values::CountOp::apply(value)? {
    Var::Int(n) => Ok(n),
    _ => unreachable!("Count outputs an Int"),
  }
}

#[cold]
fn invalid() -> Error {
  Error::Activation("inline builtin: operand type mismatch".into())
}

#[cold]
fn overflow() -> Error {
  Error::Activation("Math.Add: integer overflow".into())
}

// Raw frame bases are derived once from the exclusive borrows. In particular,
// do not recreate a mutable slice reference while an accumulator points into
// it. Bounds checks precede every offset; the resulting pointer is only used
// within `run` while both original slices remain exclusively borrowed.
#[derive(Clone, Copy)]
struct Frames {
  /// The locals' and the mesh's frames, indexed by `LOCALS` and `MESH`:
  /// a binding's slot is one indexed load, not a choice between two
  /// (which the two architectures compiled differently, each paying an
  /// instruction per slot in one placement of this struct or the other).
  base: [*mut Var; 2],
  len: [usize; 2],
  bytes: [usize; 2],
}

const LOCALS: usize = 0;
const MESH_FRAME: usize = 1;

impl Frames {
  fn new(locals: &mut [Var], mesh: &mut [Var]) -> Frames {
    Frames {
      base: [locals.as_mut_ptr(), mesh.as_mut_ptr()],
      len: [locals.len(), mesh.len()],
      bytes: [std::mem::size_of_val(locals), std::mem::size_of_val(mesh)],
    }
  }

  #[inline(always)]
  fn slot_offset<const MESH: bool>(&self, offset: SlotOffset) -> *mut Var {
    let frame = if MESH { MESH_FRAME } else { LOCALS };
    assert!(
      offset.0 < self.bytes[frame],
      "inline binding outside its frame"
    );
    // SAFETY: SlotOffset is an aligned whole-Var offset constructed with
    // checked multiplication. Frame byte lengths are multiples of Var size,
    // so offset < bytes also proves the entire Var fits. Bases come from the
    // original exclusive borrows; no new mutable reference is constructed.
    unsafe { self.base[frame].cast::<u8>().add(offset.0).cast::<Var>() }
  }

  #[inline(always)]
  fn slot(&self, b: Binding) -> *mut Var {
    let (frame, index) = match b {
      Binding::Local(i) => (LOCALS, i),
      Binding::Mesh(i) => (MESH_FRAME, i),
    };
    assert!(index < self.len[frame], "inline binding outside its frame");
    // SAFETY: checked against the length of the exclusively borrowed frame.
    unsafe { self.base[frame].add(index) }
  }
}

/// Runs until a fallback node or the end, returning an owned snapshot. There
/// are no callbacks, suspension points, frame resizes or escaping references.
/// Code must be lowered and entry must be at a segment boundary (flow start or
/// immediately after a fallback/constructor), as enforced by both flow loops.
///
/// Two tiers (2026-10-11; docs/runtime-performance-overview.md, lesson 9).
/// This loop runs the hot tier's operations (the `Op` variants up to
/// `LoopTest`) in place, and only their fast case: plain operands and a
/// plain scratch, no error. Everything else (the slow tier's operations,
/// a value owning storage, any failure) goes through the one call to
/// `slow_op`, which implements every operation in full. So nothing in this
/// loop depends on which kinds of value own storage or on how many
/// operations exist: the compiler allocates registers and lays out blocks
/// for the whole loop at once, and only the hot tier is in it.
pub(crate) fn run<C: VmCalls>(
  code: &[Instruction],
  mut index: usize,
  input: Var,
  locals: &mut [Var],
  mesh: &mut [Var],
  calls: &C,
) -> Result<(usize, Var)> {
  // The hot tier reads its own copy of the frame descriptor: through the
  // tier, every slot address was computed by selecting an address and
  // loading from it (one more instruction per slot).
  let frames = Frames::new(locals, mesh);
  let mut tier = Tier {
    frames,
    // Own the incoming output so replacing it releases captured values
    // before later frame mutations (see `Tier::scratch`).
    scratch: input,
    numeric: std::mem::MaybeUninit::uninit(),
    error: None,
  };
  // Every access to the tier's storage, here and in `slow_op`, goes through
  // this pointer, so the accumulator's pointers into it stay valid.
  let t: *mut Tier = &raw mut tier;
  // SAFETY: `t` is valid for the whole run (see `Tier`).
  let scratch: *mut Var = unsafe { &raw mut (*t).scratch };
  let numeric: *mut Var = unsafe { (&raw mut (*t).numeric).cast::<Var>() };
  let mut value: *const Var = scratch;
  'run: while let Some(instruction) = code.get(index) {
    if C::METERED {
      // The meter keeps the instruction being run, so a failure can be
      // located (`Meter::at`). Nothing else in this loop knows about
      // locating: an extra parameter or a closure around it (to read
      // `index` after a failure) cost the VM suite's push, take and float4
      // cases 7 to 16 percent at runtime (3ceac16).
      let meter = calls.meter().expect("metered");
      meter.at(index);
      meter.charge(1)?;
    }
    // SAFETY: the hot arms keep `Tier`'s accumulator invariant; each reads
    // its operands before it writes, and writes only plain values over
    // plain ones (nothing to drop).
    unsafe {
      let frames = &(*t).frames;
      'slow: {
        match &instruction.op {
          // The run ends before these: the engine activates a fallback,
          // `run_segment` builds a constructor's value. Directly, not
          // through `slow_op`: a constructor ends every segment that builds
          // a value.
          Op::Fallback | Op::SeqMake(_) | Op::TableMake(..) => break 'run,
          Op::Const(v) => value = v,
          Op::GetLocal(offset) => value = frames.slot_offset::<false>(*offset),
          Op::GetMesh(offset) => value = frames.slot_offset::<true>(*offset),
          Op::Set(b) => {
            let target = frames.slot(*b);
            if !std::ptr::eq(value, target) {
              if (*target).is_plain() && (*value).is_plain() {
                std::ptr::write(target, std::ptr::read(value));
              } else {
                copy_into(target, value);
              }
            }
            value = target;
          }
          Op::Inc(b) => {
            let target = frames.slot(*b);
            let Var::Int(n) = &mut *target else {
              break 'slow;
            };
            let Some(m) = n.checked_add(1) else {
              break 'slow;
            };
            *n = m;
            value = target;
          }
          // The Drop variants release an owned scratch as they reanchor:
          // plain, it needs nothing; owned (a value a constructor or a
          // Take left), through `release_scratch`.
          // Each releases the scratch last, after its own checks: the
          // accumulator may be the scratch until it reanchors, and a
          // `break 'slow` redoes the whole operation.
          Op::ConstDrop(v) => {
            value = v;
            if !(*scratch).is_plain() {
              release_scratch(scratch);
            }
          }
          Op::GetLocalDrop(offset) => {
            value = frames.slot_offset::<false>(*offset);
            if !(*scratch).is_plain() {
              release_scratch(scratch);
            }
          }
          Op::GetMeshDrop(offset) => {
            value = frames.slot_offset::<true>(*offset);
            if !(*scratch).is_plain() {
              release_scratch(scratch);
            }
          }
          Op::SetDrop(b) => {
            let target = frames.slot(*b);
            if !std::ptr::eq(value, target) {
              if (*target).is_plain() && (*value).is_plain() {
                std::ptr::write(target, std::ptr::read(value));
              } else {
                copy_into(target, value);
              }
            }
            value = target;
            if !(*scratch).is_plain() {
              release_scratch(scratch);
            }
          }
          Op::IncDrop(b) => {
            let target = frames.slot(*b);
            let Var::Int(n) = &mut *target else {
              break 'slow;
            };
            let Some(m) = n.checked_add(1) else {
              break 'slow;
            };
            *n = m;
            value = target;
            if !(*scratch).is_plain() {
              release_scratch(scratch);
            }
          }
          // A plain element by index, a plain struct slot: read, then
          // written over a plain scratch.
          Op::Take(key) => {
            let key = match key {
              Operand::Const(v) => v,
              Operand::Bound(b) => &*frames.slot(*b),
            };
            let item = match (&*value, key) {
              (Var::Seq(items), Var::Int(i)) if *i >= 0 && (*i as usize) < items.len() => {
                &items[*i as usize]
              }
              (Var::Table(_), Var::String(_)) => {
                take_key_into(value, key, scratch);
                value = scratch;
                index += 1;
                continue 'run;
              }
              _ => break 'slow,
            };
            if !item.is_plain() {
              break 'slow;
            }
            // Copied out first: the read ends here, even when the
            // collection is the scratch itself (a value just built).
            let item = std::ptr::read(item);
            if (*scratch).is_plain() {
              std::ptr::write(scratch, item);
            } else {
              replace_scratch(scratch, item);
            }
            value = scratch;
          }
          Op::TakeSlot(slot) => {
            let item = match &*value {
              Var::Table(entries) => match entries.slot(*slot) {
                Some(item) => item,
                None => break 'slow,
              },
              _ => break 'slow,
            };
            if !item.is_plain() {
              break 'slow;
            }
            // Copied out first: the read ends here, even when the
            // collection is the scratch itself (a value just built).
            let item = std::ptr::read(item);
            if (*scratch).is_plain() {
              std::ptr::write(scratch, item);
            } else {
              replace_scratch(scratch, item);
            }
            value = scratch;
          }
          Op::AddIntConst(rhs) => {
            let Var::Int(lhs) = &*value else {
              break 'slow;
            };
            let Some(result) = lhs.checked_add(*rhs) else {
              break 'slow;
            };
            std::ptr::write(numeric, Var::Int(result));
            value = numeric;
          }
          Op::AddIntBound(b) => {
            let (Var::Int(lhs), Var::Int(rhs)) = (&*value, &*frames.slot(*b)) else {
              break 'slow;
            };
            let Some(result) = lhs.checked_add(*rhs) else {
              break 'slow;
            };
            std::ptr::write(numeric, Var::Int(result));
            value = numeric;
          }
          Op::AddFloatConst(rhs) => {
            let lhs = match &*value {
              Var::Float(v) => *v,
              Var::Int(v) => *v as f64,
              _ => break 'slow,
            };
            std::ptr::write(numeric, Var::Float(lhs + rhs));
            value = numeric;
          }
          Op::AddFloatBound(b) => {
            let number = |v: &Var| match v {
              Var::Float(v) => Some(*v),
              Var::Int(v) => Some(*v as f64),
              _ => None,
            };
            let (Some(lhs), Some(rhs)) = (number(&*value), number(&*frames.slot(*b))) else {
              break 'slow;
            };
            std::ptr::write(numeric, Var::Float(lhs + rhs));
            value = numeric;
          }
          Op::AddFloat4Const(rhs) => {
            let Var::Float4(lhs) = &*value else {
              break 'slow;
            };
            let result = [
              lhs[0] + rhs[0],
              lhs[1] + rhs[1],
              lhs[2] + rhs[2],
              lhs[3] + rhs[3],
            ];
            std::ptr::write(numeric, Var::Float4(Float4(result)));
            value = numeric;
          }
          Op::AddFloat4Bound(b) => {
            let (Var::Float4(lhs), Var::Float4(rhs)) = (&*value, &*frames.slot(*b)) else {
              break 'slow;
            };
            let result = [
              lhs[0] + rhs[0],
              lhs[1] + rhs[1],
              lhs[2] + rhs[2],
              lhs[3] + rhs[3],
            ];
            std::ptr::write(numeric, Var::Float4(Float4(result)));
            value = numeric;
          }
          Op::Pass => {}
          // Jumps leave the value where it is and skip the output check.
          Op::Jump(target) => {
            index = *target as usize;
            continue 'run;
          }
          Op::JumpIfNot(target) => match &*value {
            Var::Bool(false) => {
              index = *target as usize;
              continue 'run;
            }
            Var::Bool(true) => {}
            _ => break 'slow,
          },
          Op::JumpIf(target) => match &*value {
            Var::Bool(true) => {
              index = *target as usize;
              continue 'run;
            }
            Var::Bool(false) => {}
            _ => break 'slow,
          },
          Op::LoopTest(counter, end) => match &mut *frames.slot(*counter) {
            Var::Int(n) if *n > 0 => *n -= 1,
            Var::Int(_) => {
              index = *end as usize;
              continue 'run;
            }
            _ => break 'slow,
          },
          // Every slow-tier variant by name rather than a catch-all, so
          // the jump table covers every tag and needs no range check
          // first (one instruction on every operation). A new operation
          // adds its name here: a table entry, no code.
          Op::Push(b) if !C::METERED => {
            let target = frames.slot(*b);
            // A plain value onto another sequence that is uniquely owned
            // and has room: a reference check and a write. Growing, copy
            // on write, a sequence pushing itself and errors in
            // `push_into`.
            if !std::ptr::eq(value, target)
              && (*value).is_plain()
              && let Var::Seq(items) = &mut *target
              && let Some(items) = std::sync::Arc::get_mut(items)
              && items.len() < items.capacity()
            {
              // Room checked above: a write and the length, without
              // `Vec::push`'s growth path in this loop.
              let len = items.len();
              std::ptr::write(items.as_mut_ptr().add(len), std::ptr::read(value));
              items.set_len(len + 1);
            } else {
              value = match push_into(target, value, scratch) {
                Ok(v) => v,
                Err(e) => return Err(*e),
              };
            }
          }
          // Typed arithmetic and numeric comparison on plain operands
          // (`arith_into`, `compare_into`), as `math::arith` and
          // `shards::compare` compute them; mixed shapes, overflow,
          // division by zero and NaN comparisons go to the slow tier.
          Op::Arith(op, rhs) => {
            let rhs = match rhs {
              Operand::Const(v) => v,
              Operand::Bound(b) => &*frames.slot(*b),
            };
            if !arith_into(*op, &*value, rhs, numeric) {
              break 'slow;
            }
            value = numeric;
          }
          Op::Compare(cmp, rhs) => {
            let rhs = match rhs {
              Operand::Const(v) => v,
              Operand::Bound(b) => &*frames.slot(*b),
            };
            if !compare_into(*cmp, &*value, rhs, numeric) {
              break 'slow;
            }
            value = numeric;
          }
          Op::Push(_) | Op::Equal(..) | Op::Count | Op::VmCall | Op::Clear(_) => break 'slow,
        }
        #[cfg(any(debug_assertions, feature = "output-checks"))]
        leaf::check_output(instruction.check.0, instruction.check.1, &*value)?;
        index += 1;
        continue 'run;
      }
      // The one call into the slow tier.
      let step = slow_op(t, calls, instruction, index, value);
      value = step.value;
      match step.next {
        STOP => break 'run,
        FAIL => {
          return Err(
            (*t)
              .error
              .take()
              .expect("a failing operation sets the error"),
          );
        }
        next => {
          #[cfg(any(debug_assertions, feature = "output-checks"))]
          leaf::check_output(instruction.check.0, instruction.check.1, &*value)?;
          index = next;
        }
      }
    }
  }
  // SAFETY: the accumulator is still live; the output is owned and leaves
  // the tier, which drops what remains.
  let output = unsafe {
    if std::ptr::eq(value, scratch) {
      std::mem::replace(&mut *scratch, Var::None)
    } else {
      copy(&*value)
    }
  };
  Ok((index, output))
}

/// `run`'s storage, at one address for the whole run. The accumulator
/// (`value` in `run` and `slow_op`) points at a constant, a checked frame
/// slot, `scratch` or `numeric`; each operation reads through it before it
/// writes and then reanchors it. Both tiers reach this storage only through
/// the raw pointer `run` takes once, never through a reference to the whole
/// tier, so the accumulator's pointers into it stay valid (a shared and a
/// mutable reference to the scratch in one call is undefined behavior,
/// review 2026-10-11-dcee86c-claude-0b393d#F1).
struct Tier {
  frames: Frames,
  /// Owned output. Compose-selected Drop variants release it when
  /// reanchoring to external storage; Take/Push/generic arithmetic replace
  /// it with their output. Typed numeric arithmetic can leave only a
  /// resource-free numeric value in scratch: its input must be numeric, and
  /// no obsolete owning value survives the other reanchoring operations.
  /// This avoids a cleanup branch per Add.
  scratch: Var,
  /// Numeric results own no heap storage: written without running `Var`'s
  /// drop. Only Int/Float/Float4/Bool values are written here; generic
  /// results use `scratch`. Never read until written.
  numeric: std::mem::MaybeUninit<Var>,
  /// The failure of the operation `slow_op` returned `FAIL` for.
  error: Option<Error>,
}

/// What `slow_op` returns to `run`: the next instruction (always the one
/// after it), `STOP` (the VM cannot run it: the run ends before it) or
/// `FAIL` (`Tier::error`), and the accumulator. Two words, returned in
/// registers.
struct Step {
  next: usize,
  value: *const Var,
}

const STOP: usize = usize::MAX - 1;
const FAIL: usize = usize::MAX;

/// The slow tier: every operation in full, the hot tier's included (their
/// cases the hot arms leave: a value owning storage, a failure). Not a
/// whole interpreter on its own: it never takes a jump. A jump reaches it
/// only when its condition is not a Bool (or a loop counter not an Int), to
/// fail; the hot tier takes every valid jump. One
/// function behind one call site in `run`, so `run`'s code does not depend
/// on what is here. Marked cold so the compiler keeps `run`'s registers for
/// the hot path and spills around this call only.
///
/// SAFETY: `t` is `run`'s tier pointer and `value` its live accumulator.
#[cold]
#[inline(never)]
unsafe fn slow_op<C: VmCalls>(
  t: *mut Tier,
  calls: &C,
  instruction: &Instruction,
  index: usize,
  mut value: *const Var,
) -> Step {
  // SAFETY: see above: every access goes through `t`; each operation reads
  // through the accumulator before it writes and reanchors.
  unsafe {
    let scratch: *mut Var = &raw mut (*t).scratch;
    let numeric: *mut Var = (&raw mut (*t).numeric).cast::<Var>();
    let frames = &(*t).frames;
    macro_rules! tri {
      ($e:expr) => {
        match $e {
          Ok(v) => v,
          Err(e) => {
            (*t).error = Some(e);
            return Step { next: FAIL, value };
          }
        }
      };
    }
    // The result, owned by the scratch; the accumulator reads it there.
    macro_rules! own {
      ($new:expr) => {{
        let new = $new;
        assign(&mut *scratch, new);
        value = scratch;
      }};
    }
    macro_rules! number {
      ($new:expr) => {{
        std::ptr::write(numeric, $new);
        value = numeric;
      }};
    }
    let inc = |b: &Binding| -> Result<*mut Var> {
      let target = frames.slot(*b);
      let Var::Int(n) = &mut *target else {
        return Err(Error::Activation("Inc: variable is not an Int".into()));
      };
      *n = n
        .checked_add(1)
        .ok_or_else(|| Error::Activation("Inc: integer overflow".into()))?;
      Ok(target)
    };
    match &instruction.op {
      Op::Fallback | Op::SeqMake(_) | Op::TableMake(..) => return Step { next: STOP, value },
      Op::Const(v) => value = v,
      Op::GetLocal(offset) => value = frames.slot_offset::<false>(*offset),
      Op::GetMesh(offset) => value = frames.slot_offset::<true>(*offset),
      Op::Set(b) => {
        let target = frames.slot(*b);
        if !std::ptr::eq(value, target) {
          assign_copy(&mut *target, &*value);
        }
        value = target;
      }
      Op::Inc(b) => value = tri!(inc(b)),
      Op::ConstDrop(v) => {
        value = v;
        assign(&mut *scratch, Var::None);
      }
      Op::GetLocalDrop(offset) => {
        value = frames.slot_offset::<false>(*offset);
        assign(&mut *scratch, Var::None);
      }
      Op::GetMeshDrop(offset) => {
        value = frames.slot_offset::<true>(*offset);
        assign(&mut *scratch, Var::None);
      }
      Op::SetDrop(b) => {
        let target = frames.slot(*b);
        if !std::ptr::eq(value, target) {
          assign_copy(&mut *target, &*value);
        }
        value = target;
        assign(&mut *scratch, Var::None);
      }
      Op::IncDrop(b) => {
        value = tri!(inc(b));
        assign(&mut *scratch, Var::None);
      }
      Op::Take(key) => {
        let key = match key {
          Operand::Const(v) => v,
          Operand::Bound(b) => &*frames.slot(*b),
        };
        let new = match (&*value, key) {
          (Var::Seq(items), Var::Int(i)) if *i >= 0 && (*i as usize) < items.len() => {
            copy(&items[*i as usize])
          }
          (Var::Table(entries), Var::String(k)) => entries.get(k).map(copy).unwrap_or(Var::None),
          _ => tri!(data::take_value(&*value, key)),
        };
        own!(new);
      }
      Op::TakeSlot(slot) => {
        let found = match &*value {
          Var::Table(entries) => entries.slot(*slot),
          _ => None,
        };
        let new = match found {
          Some(v) => copy(v),
          None => tri!(data::take_slot(&*value, *slot)),
        };
        own!(new);
      }
      Op::Push(b) => {
        // The accumulator never points into a sequence's buffer (a Take
        // copies its element out), so only an input that is the
        // destination itself (a sequence pushing itself) needs a snapshot
        // before the slot is mutated.
        let target = frames.slot(*b);
        if C::METERED {
          tri!(meter_push(
            calls.meter().expect("metered"),
            &*target,
            &*value
          ));
        }
        let pushed = if std::ptr::eq(value, target) {
          own!(copy(&*value));
          copy(&*scratch)
        } else {
          copy(&*value)
        };
        match &mut *target {
          Var::Seq(items) => seq_mut(items).push(pushed),
          _ => {
            tri!(Err(Error::Activation(
              "Push: the variable is not a sequence".into()
            )))
          }
        }
      }
      Op::AddIntConst(rhs) => {
        let Var::Int(lhs) = &*value else {
          tri!(Err(invalid()))
        };
        number!(Var::Int(tri!(lhs.checked_add(*rhs).ok_or_else(overflow))));
      }
      Op::AddIntBound(b) => {
        let (Var::Int(lhs), Var::Int(rhs)) = (&*value, &*frames.slot(*b)) else {
          tri!(Err(invalid()))
        };
        number!(Var::Int(tri!(lhs.checked_add(*rhs).ok_or_else(overflow))));
      }
      Op::AddFloatConst(rhs) => {
        let lhs = match &*value {
          Var::Float(v) => *v,
          Var::Int(v) => *v as f64,
          _ => tri!(Err(invalid())),
        };
        number!(Var::Float(lhs + rhs));
      }
      Op::AddFloatBound(b) => {
        let number = |v: &Var| match v {
          Var::Float(v) => Ok(*v),
          Var::Int(v) => Ok(*v as f64),
          _ => Err(invalid()),
        };
        let result = tri!(number(&*value)) + tri!(number(&*frames.slot(*b)));
        number!(Var::Float(result));
      }
      Op::AddFloat4Const(rhs) => own!(tri!(add_generic(&*value, &Var::Float4(*rhs)))),
      Op::AddFloat4Bound(b) => own!(tri!(add_generic(&*value, &*frames.slot(*b)))),
      Op::Pass => {}
      // Jumps reach here only to fail: the hot tier takes them otherwise.
      Op::Jump(_) => unreachable!("an unconditional jump is always taken in run"),
      Op::JumpIfNot(_) | Op::JumpIf(_) => tri!(Err(not_a_bool())),
      Op::LoopTest(..) => tri!(Err(Error::Activation("Repeat: times is not an Int".into()))),
      Op::Arith(op, rhs) => {
        let rhs = match rhs {
          Operand::Const(v) => v,
          Operand::Bound(b) => &*frames.slot(*b),
        };
        number!(tri!(math::arith(*op, arith_name(*op), &*value, rhs)));
      }
      Op::Compare(cmp, rhs) => {
        let rhs = match rhs {
          Operand::Const(v) => v,
          Operand::Bound(b) => &*frames.slot(*b),
        };
        let ordering = tri!(crate::shards::compare(&*value, rhs.clone()));
        number!(Var::Bool(cmp.holds(ordering)));
      }
      Op::Equal(negate, rhs) => {
        let rhs = match rhs {
          Operand::Const(v) => v,
          Operand::Bound(b) => &*frames.slot(*b),
        };
        if C::METERED {
          let meter = calls.meter().expect("metered");
          tri!(meter.traverse(&*value));
          tri!(meter.traverse(rhs));
        }
        number!(Var::Bool(values::values_equal(&*value, rhs) != *negate));
      }
      Op::Count => number!(Var::Int(tri!(count(&*value, calls.meter())))),
      Op::Clear(b) => {
        if let Some(moved) = clear_slot(frames.slot(*b), value) {
          own!(moved);
        }
      }
      Op::VmCall => match tri!(vm_call_op(calls, index, value, frames)) {
        Some(output) => own!(output),
        None => return Step { next: STOP, value },
      },
    }
    Step {
      next: index + 1,
      value,
    }
  }
}

/// The meter's side of a `Push` onto `target` (the sequence before the
/// push): the grown buffer within the value limit, and one element's bytes
/// charged, or the whole copy when the sequence is shared (it is copied
/// before it grows). Shared by the VM and the `Push` shard.
pub(crate) fn meter_push(meter: &Meter, target: &Var, pushed: &Var) -> Result<()> {
  let Var::Seq(items) = target else {
    return Ok(());
  };
  meter.nest(pushed)?;
  let bytes = (items.len() + 1) * size_of::<Var>();
  if std::sync::Arc::strong_count(items) == 1 {
    meter.admit(bytes)?;
    meter.charge(size_of::<Var>() as u64)
  } else {
    meter.allocate(bytes)
  }
}

/// The shard name an arithmetic error reports.
fn arith_name(op: BinOp) -> &'static str {
  match op {
    BinOp::Add => "Math.Add",
    BinOp::Subtract => "Math.Subtract",
    BinOp::Multiply => "Math.Multiply",
    BinOp::Divide => "Math.Divide",
  }
}

/// `Set`'s copy of a value owning storage, from `run`'s hot tier: a small
/// function of its own rather than the slow tier's, whose prologue and second
/// dispatch cost a heap copy about 60 instructions (`Set` of a string runs
/// every iteration in ordinary scripts).
///
/// SAFETY: `target` is a checked frame slot and `value` the live
/// accumulator, not `target` itself; it does not point into `target`'s
/// storage (a Take copies its element out).
#[inline(never)]
unsafe fn copy_into(target: *mut Var, value: *const Var) {
  // SAFETY: see above. `run` sends here only when one of them owns
  // storage, so the plain test is not repeated.
  unsafe { assign_copy_owning(&mut *target, &*value) }
}

/// `Push`, from `run`'s hot tier: a small function of its own, as
/// `copy_into` (through the slow tier it cost a push about 50 instructions);
/// a metered run pushes through the slow tier, which charges the meter.
/// Returns where the accumulator reads afterwards. The accumulator never
/// points into a sequence's buffer (a Take copies its element out), so only
/// an input that is the destination itself (a sequence pushing itself)
/// needs a snapshot, in the scratch, before the slot is mutated.
///
/// SAFETY: `target` is a checked frame slot, `value` the live accumulator
/// and `scratch` the run's scratch, all reached through `run`'s tier
/// pointer; the read through `value` ends before the scratch is written.
#[inline(never)]
unsafe fn push_into(
  target: *mut Var,
  mut value: *const Var,
  scratch: *mut Var,
) -> std::result::Result<*const Var, Box<Error>> {
  // SAFETY: see above.
  unsafe {
    let pushed = if std::ptr::eq(value, target) {
      let new = copy(&*value);
      assign(&mut *scratch, new);
      value = scratch;
      copy(&*scratch)
    } else {
      copy(&*value)
    };
    match &mut *target {
      Var::Seq(items) => seq_mut(items).push(pushed),
      _ => {
        return Err(Box::new(Error::Activation(
          "Push: the variable is not a sequence".into(),
        )));
      }
    }
    Ok(value)
  }
}

/// Replaces an owned scratch with a plain value, releasing it: a `Take` of
/// a plain value out of a collection the scratch holds (one a constructor
/// just built). A small function of its own, as `copy_into`; through the
/// slow tier this cost a constructed table's read about 55 instructions.
///
/// SAFETY: `scratch` is the run's scratch, reached through its tier
/// pointer, and nothing reads it during the call.
#[inline(never)]
unsafe fn replace_scratch(scratch: *mut Var, value: Var) {
  // SAFETY: see above.
  unsafe { assign(&mut *scratch, value) }
}

/// `Arith` on two numbers or two vectors of the same size, written to
/// `numeric`, as `math::arith` computes them (the same `BinOp` methods);
/// `false` for anything else, which the slow tier handles (mixed shapes,
/// integer overflow, division by zero). A small function of its own: in
/// `run`, its per-operation and per-component dispatch grew the loop to 926
/// instructions and slowed every plain operation; through the slow tier, the
/// entity tick's `Float3` add paid that tier's prologue and second dispatch.
///
/// SAFETY: `numeric` is the run's numeric slot, reached through its tier
/// pointer; neither operand is it while it is written (both are read first).
#[inline(never)]
unsafe fn arith_into(op: BinOp, value: &Var, rhs: &Var, numeric: *mut Var) -> bool {
  let result = match (value, rhs) {
    (Var::Int(a), Var::Int(b)) => match op.int(*a, *b) {
      Some(r) => Var::Int(r),
      None => return false,
    },
    (Var::Float(a), Var::Float(b)) => Var::Float(op.f64(*a, *b)),
    (Var::Float2(a), Var::Float2(b)) => Var::float2(op.f32(a[0], b[0]), op.f32(a[1], b[1])),
    (Var::Float3(a), Var::Float3(b)) => {
      Var::float3(op.f32(a[0], b[0]), op.f32(a[1], b[1]), op.f32(a[2], b[2]))
    }
    (Var::Float4(a), Var::Float4(b)) => Var::float4(
      op.f32(a[0], b[0]),
      op.f32(a[1], b[1]),
      op.f32(a[2], b[2]),
      op.f32(a[3], b[3]),
    ),
    _ => return false,
  };
  // SAFETY: see above; a numeric result owns nothing.
  unsafe { std::ptr::write(numeric, result) };
  true
}

/// `Compare` of two Ints or two Floats, written to `numeric` as
/// `shards::compare` orders them; `false` for anything else (mixed numbers,
/// NaN, other kinds), which the slow tier handles. Out of line as
/// `arith_into`: in `run` it slowed the plain operations.
///
/// SAFETY: as `arith_into`.
#[inline(never)]
unsafe fn compare_into(cmp: Cmp, value: &Var, rhs: &Var, numeric: *mut Var) -> bool {
  let ordering = match (value, rhs) {
    (Var::Int(a), Var::Int(b)) => a.cmp(b),
    (Var::Float(a), Var::Float(b)) => match a.partial_cmp(b) {
      Some(o) => o,
      None => return false,
    },
    _ => return false,
  };
  // SAFETY: see above; a Bool owns nothing.
  unsafe { std::ptr::write(numeric, Var::Bool(cmp.holds(ordering))) };
  true
}

/// Releases an owned scratch for a Drop variant (`ConstDrop`, `SetDrop`,
/// ...) about to reanchor: what a constructor or an owned result left there.
/// Cold and out of line: through the slow tier this cost a constructor's
/// loop about 90 instructions an iteration on aarch64.
///
/// SAFETY: `scratch` is the run's scratch, reached through its tier
/// pointer, and the accumulator does not read it (the variant reanchors).
#[cold]
#[inline(never)]
unsafe fn release_scratch(scratch: *mut Var) {
  // SAFETY: see above.
  unsafe { assign(&mut *scratch, Var::None) }
}

/// `Take` of a table's entry by a string key, into the scratch (`none`
/// for a missing key): a small function of its own, as `copy_into`; through
/// the slow tier this cost a read by a variable key about 45 instructions on
/// aarch64. The table and the key are read before the scratch is written
/// (the table may be the scratch itself: raw pointers, review
/// 2026-10-11-dcee86c-claude-0b393d#F1).
///
/// SAFETY: `value` is the live accumulator, a table; `key` a string that is
/// a constant or a frame slot, never the scratch; `scratch` the run's
/// scratch, reached through its tier pointer.
#[inline(never)]
unsafe fn take_key_into(value: *const Var, key: &Var, scratch: *mut Var) {
  // SAFETY: see above; the reference ends with this statement.
  let new = match (unsafe { &*value }, key) {
    (Var::Table(entries), Var::String(k)) => entries.get(k).map(copy).unwrap_or(Var::None),
    _ => unreachable!("run checked a table and a string key"),
  };
  // SAFETY: no reference into the scratch is live.
  unsafe { assign(&mut *scratch, new) }
}

/// Out of line so `run`'s own frame stays small: a VM call's state would
/// otherwise be set up in every segment's prologue.
#[inline(never)]
unsafe fn vm_call_op<C: VmCalls>(
  calls: &C,
  index: usize,
  value: *const Var,
  frames: &Frames,
) -> Result<Option<Var>> {
  unsafe {
    let Some(callee) = calls.enter(index) else {
      return Ok(None);
    };
    // SAFETY: the callee's locals are a kept frame's buffer, distinct
    // from this frame's, alive and unmoved for the run (the engine's
    // contract for invocation locals); nothing else addresses it
    // while the callee runs.
    let callee_locals: &mut [Var] = &mut *callee.locals;
    let input = if callee.ignores_input {
      Var::None
    } else {
      (*value).clone()
    };
    for (slot, op) in callee.param_slots.iter().zip(callee.args) {
      callee_locals[*slot] = match op {
        Operand::Const(v) => v.clone(),
        Operand::Bound(b) => (*frames.slot(*b)).clone(),
      };
    }
    callee_locals[callee.input_slot] = input.clone();
    // SAFETY: the mesh frame through the same pointer this run uses.
    let mesh = std::slice::from_raw_parts_mut(frames.base[MESH_FRAME], frames.len[MESH_FRAME]);
    let result = if callee.constructors {
      run_segment(callee.code, 0, input, callee_locals, mesh, &callee.calls)
    } else {
      run(callee.code, 0, input, callee_locals, mesh, &callee.calls)
    };
    if C::METERED && result.is_err() {
      // The callee's level, then this run's own instruction again.
      let meter = calls.meter().expect("metered");
      callee.calls.failed(meter.pc());
      meter.at(index);
    }
    // The invocation's values end with it, returned or failed: the kept
    // locals hold nothing between calls (the body is stateless), so a
    // collection passed in is uniquely owned again by the caller.
    for local in callee_locals.iter_mut() {
      *local = Var::None;
    }
    Ok(Some(result?.1))
  }
}

/// `Op::Clear`, out of line and cold so the interpreter loop's layout does
/// not depend on it (inline, its arm cost unrelated loops 0.25 ns per
/// iteration through block order and register choice): unsets `target`,
/// returning its value when the accumulator (`value`, only compared) reads
/// it, for the caller to move into its scratch.
///
/// SAFETY: `target` is a checked frame slot, valid for writes.
#[cold]
#[inline(never)]
unsafe fn clear_slot(target: *mut Var, value: *const Var) -> Option<Var> {
  unsafe {
    if std::ptr::eq(value, target) {
      Some(std::mem::replace(&mut *target, Var::None))
    } else {
      *target = Var::None;
      None
    }
  }
}

/// A constructor operand's value.
#[inline(always)]
fn read(operand: &Operand, locals: &[Var], mesh: &[Var]) -> Var {
  match operand {
    Operand::Const(v) => v.clone(),
    Operand::Bound(Binding::Local(i)) => locals[*i].clone(),
    Operand::Bound(Binding::Mesh(i)) => mesh[*i].clone(),
  }
}

#[cold]
fn not_a_bool() -> Error {
  Error::Activation("predicate did not output a Bool".into())
}

#[cold]
fn add_generic(a: &Var, b: &Var) -> Result<Var> {
  crate::shards::math::arith(crate::shards::math::BinOp::Add, "Math.Add", a, b)
}

/// Constructors consume their input. Reuse only its uniquely owned allocation,
/// never a value retained in shard state. All operands are read-only frame or
/// compiled values, so clearing unique input storage cannot change an operand.
/// The returned value owns the output; nothing stays behind after consumption.
pub(crate) fn construct(
  code: &[Instruction],
  mut index: usize,
  mut value: Var,
  locals: &[Var],
  mesh: &[Var],
  meter: Option<&Meter>,
) -> Result<(usize, Var)> {
  use std::sync::Arc;
  while let Some(instruction) = code.get(index) {
    match &instruction.op {
      Op::SeqMake(_) => {
        // Consume the old output. A uniquely owned sequence is rebuilt in
        // place, handle and buffer included; a shared one belongs to a
        // saved snapshot, so it is left alone and a new one is built.
        let mut output: Option<Arc<Vec<Var>>> = std::mem::take(&mut value).into_seq().ok();
        while let Some(instruction) = code.get(index) {
          let Op::SeqMake(items) = &instruction.op else {
            break;
          };
          if let Some(meter) = meter {
            meter.charge(1)?;
            meter.allocate(items.len() * size_of::<Var>())?;
            for operand in items {
              meter.nest(&read(operand, locals, mesh))?;
            }
          }
          match output.as_mut().and_then(Arc::get_mut) {
            // Same length: overwrite the slots (one drop and one write
            // each, no length bookkeeping).
            Some(buffer) if buffer.len() == items.len() => {
              for (slot, operand) in buffer.iter_mut().zip(items) {
                *slot = read(operand, locals, mesh);
              }
            }
            Some(buffer) => {
              buffer.clear();
              buffer.reserve(items.len());
              for operand in items {
                buffer.push(read(operand, locals, mesh));
              }
            }
            None => {
              let mut buffer = Vec::with_capacity(items.len());
              for operand in items {
                buffer.push(read(operand, locals, mesh));
              }
              output = Some(Arc::new(buffer));
            }
          }
          // Checking builds materialize each intermediate output; release
          // keeps the owned buffer until the constructor segment ends.
          #[cfg(any(debug_assertions, feature = "output-checks"))]
          leaf::check_output(
            instruction.check.0,
            instruction.check.1,
            &Var::from_seq(output.clone().expect("built above")),
          )?;
          index += 1;
        }
        value = Var::from_seq(output.expect("a constructor ran"));
      }
      Op::TableMake(..) => {
        // Consume the old output: a struct table of the same shape that
        // nobody else holds is overwritten in place; anything else is left
        // to its owners and a new table is built. There are no callbacks
        // or frame writes between constructors.
        let mut output = std::mem::take(&mut value).into_table().ok();
        while let Some(instruction) = code.get(index) {
          let Op::TableMake(shape, operands) = &instruction.op else {
            break;
          };
          if let Some(meter) = meter {
            meter.charge(1)?;
            meter.allocate(operands.len() * size_of::<Var>())?;
            for operand in operands {
              meter.nest(&read(operand, locals, mesh))?;
            }
          }
          let reused = match output.as_mut() {
            Some(table) => table.unique_slots(*shape),
            None => None,
          };
          match reused {
            Some(slots) => {
              for (slot, operand) in slots.iter_mut().zip(operands) {
                *slot = read(operand, locals, mesh);
              }
            }
            None => {
              // One allocation, filled in place.
              let mut slots = Arc::<[Var]>::new_uninit_slice(operands.len());
              let uninit = Arc::get_mut(&mut slots).expect("just allocated");
              for (slot, operand) in uninit.iter_mut().zip(operands) {
                slot.write(read(operand, locals, mesh));
              }
              // SAFETY: `slots` has one entry per operand, each written
              // above.
              let slots = unsafe { slots.assume_init() };
              output = Some(Table::with_slots(*shape, slots));
            }
          }
          #[cfg(any(debug_assertions, feature = "output-checks"))]
          leaf::check_output(
            instruction.check.0,
            instruction.check.1,
            &Var::from_table(output.clone().expect("built above")),
          )?;
          index += 1;
        }
        value = Var::from_table(output.expect("a constructor ran"));
      }
      _ => break,
    }
  }
  Ok((index, value))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// An out-of-line `Take` or `TakeSlot` whose accumulator is the run's
  /// scratch (the input, or a previous result): the read must end before
  /// scratch is written. Native runs passed with the aliasing; these fail
  /// under Miri there (reviews 2026-10-11-dcee86c-claude-0b393d#F1 and
  /// 2026-10-11-dcee86c-codex-5d92a7).
  #[test]
  fn out_of_line_takes_read_the_scratch_before_writing_it() {
    let table = |entries: Vec<(&str, Var)>| Var::from_table(entries.into_iter().collect::<Table>());
    // A key lookup on the input, then on its result.
    let code = instructions([
      Op::Take(Operand::Const(Var::string("a"))),
      Op::Take(Operand::Const(Var::string("b"))),
    ]);
    let input = table(vec![("a", table(vec![("b", Var::Int(5))]))]);
    let (_, out) = run(&code, 0, input, &mut [], &mut [], &NoCalls).unwrap();
    assert_eq!(out, Var::Int(5));
    // A valid read of a plain vector, and the same metered.
    let code = instructions([Op::Take(Operand::Const(Var::Int(0)))]);
    let float4 = || Var::Float4(Float4([1.0; 4]));
    let (_, out) = run(&code, 0, float4(), &mut [], &mut [], &NoCalls).unwrap();
    assert_eq!(out, Var::Float(1.0));
    let meter = Meter::new(Default::default());
    let metered = Metered {
      inner: NoCalls,
      meter: &meter,
    };
    let (_, out) = run(&code, 0, float4(), &mut [], &mut [], &metered).unwrap();
    assert_eq!(out, Var::Float(1.0));
    // Errors: an index out of range, a slot on a value that is no table.
    let code = instructions([Op::Take(Operand::Const(Var::Int(-1)))]);
    let seq = Var::from_seq(std::sync::Arc::new(vec![Var::Int(1)]));
    assert!(run(&code, 0, seq, &mut [], &mut [], &NoCalls).is_err());
    let code = instructions([Op::TakeSlot(0)]);
    assert!(run(&code, 0, Var::Int(1), &mut [], &mut [], &NoCalls).is_err());
    assert!(run(&code, 0, Var::None, &mut [], &mut [], &NoCalls).is_err());
  }

  fn instructions(ops: impl IntoIterator<Item = Op>) -> Vec<Instruction> {
    let mut code: Vec<_> = ops
      .into_iter()
      .map(|op| Instruction::new(Some(InlineOp(op)), "test", Type::any()))
      .collect();
    lower_scratch_releases(&mut code);
    code
  }

  #[test]
  fn constructor_reuses_unique_sequence_storage_but_preserves_shared_input() {
    use std::sync::Arc;
    let mut buffer = Vec::with_capacity(8);
    buffer.push(Var::Int(1));
    let allocation = buffer.as_ptr();
    let code = instructions([
      Op::SeqMake(vec![Operand::Const(Var::Int(2))]),
      Op::SeqMake(vec![
        Operand::Const(Var::Int(3)),
        Operand::Const(Var::Int(4)),
      ]),
    ]);
    let (index, output) =
      construct(&code, 0, Var::from_seq(Arc::new(buffer)), &[], &[], None).unwrap();
    assert_eq!(index, 2);
    let Var::Seq(items) = &output else {
      panic!("expected sequence")
    };
    assert_eq!(items.as_ptr(), allocation);
    assert_eq!(items.as_slice(), &[Var::Int(3), Var::Int(4)]);
    let snapshot = output;
    let code = instructions([Op::SeqMake(vec![Operand::Const(Var::Int(5))])]);
    let (_, output) = construct(&code, 0, snapshot.clone(), &[], &[], None).unwrap();
    assert_eq!(
      snapshot,
      Var::from_seq(Arc::new(vec![Var::Int(3), Var::Int(4)]))
    );
    assert_eq!(output, Var::from_seq(Arc::new(vec![Var::Int(5)])));
  }

  #[test]
  fn obsolete_input_and_take_scratch_are_released_before_push() {
    use std::sync::Arc;
    for through_take in [false, true] {
      for replacement in [Op::Const(Var::Int(1)), Op::get(Binding::Local(1))] {
        let mut locals = vec![Var::from_seq(Arc::new(vec![Var::Int(7)])), Var::Int(1)];
        let Var::Seq(items) = &locals[0] else {
          unreachable!()
        };
        let allocation = Arc::as_ptr(items);
        let captured = Var::from_seq(Arc::new(vec![locals[0].clone()]));
        let (input, mut ops) = if through_take {
          (
            Var::from_seq(Arc::new(vec![captured])),
            vec![Op::Take(Operand::Const(Var::Int(0)))],
          )
        } else {
          (captured, vec![])
        };
        ops.extend([replacement, Op::Push(Binding::Local(0))]);
        let (_, output) =
          run(&instructions(ops), 0, input, &mut locals, &mut [], &NoCalls).unwrap();
        assert_eq!(output, Var::Int(1));
        let Var::Seq(items) = &locals[0] else {
          unreachable!()
        };
        assert_eq!(Arc::as_ptr(items), allocation);
        assert_eq!(items.as_slice(), &[Var::Int(7), Var::Int(1)]);
      }
    }
  }

  #[test]
  fn specialized_get_checks_the_selected_frame() {
    // A longer other frame must not make an invalid offset pass its check.
    for binding in [Binding::Local(2), Binding::Mesh(3)] {
      let code = instructions([Op::get(binding)]);
      let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut locals = vec![
          Var::Int(1);
          if matches!(binding, Binding::Local(_)) {
            2
          } else {
            4
          }
        ];
        let mut mesh = vec![Var::Int(2); 3];
        run(&code, 0, Var::None, &mut locals, &mut mesh, &NoCalls)
      }));
      assert!(result.is_err(), "out-of-frame offset must be rejected");
    }
    let result = std::panic::catch_unwind(|| SlotOffset::new(usize::MAX));
    assert!(result.is_err(), "slot multiplication must not wrap");
  }

  #[test]
  fn owned_input_is_released_after_a_generic_boundary() {
    use std::sync::Arc;
    let mut locals = vec![Var::from_seq(Arc::new(vec![Var::Int(7)]))];
    let Var::Seq(items) = &locals[0] else {
      unreachable!()
    };
    let allocation = Arc::as_ptr(items);
    let code = instructions([
      Op::Const(Var::None),
      Op::Fallback,
      Op::Const(Var::Int(1)),
      Op::Push(Binding::Local(0)),
    ]);
    let (next, _) = run(&code, 0, Var::None, &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!(next, 1);
    // The fallback can return a fresh owner even though the preceding segment
    // had already cleared scratch. Its successor must release that new owner.
    let input = Var::from_seq(Arc::new(vec![locals[0].clone()]));
    let (_, output) = run(&code, next + 1, input, &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!(output, Var::Int(1));
    let Var::Seq(items) = &locals[0] else {
      unreachable!()
    };
    assert_eq!(Arc::as_ptr(items), allocation);
    assert_eq!(items.as_slice(), &[Var::Int(7), Var::Int(1)]);
  }

  #[test]
  fn owned_input_survives_passthrough_and_self_push() {
    use std::sync::Arc;
    let input = Var::from_seq(Arc::new(vec![Var::Int(7)]));
    let code = instructions([Op::Fallback]);
    let (_, output) = run(&code, 0, input.clone(), &mut [], &mut [], &NoCalls).unwrap();
    assert_eq!(output, input);
    let mut locals = vec![input.clone()];
    let code = instructions([Op::Push(Binding::Local(0))]);
    let (_, output) = run(&code, 0, input.clone(), &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!(output, input);
    assert_eq!(locals[0], Var::from_seq(Arc::new(vec![Var::Int(7), input])));
  }

  #[test]
  fn borrowed_slots_and_scratch_match_owned_execution() {
    // Differential execution exercises self-assignment, both frame kinds,
    // scratch reuse, and replacing reference-counted values with numbers.
    let initial = vec![Var::string("held"), Var::Int(3), Var::Int(4)];
    let mut locals = initial.clone();
    let mut mesh = initial.clone();
    let mut expected_locals = initial.clone();
    let mut expected_mesh = initial;
    let mut expected = Var::None;
    let mut seed = 42u64;
    for _ in 0..40 {
      let input = expected.clone();
      let mut code = Vec::new();
      for _ in 0..20 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let index = (seed >> 32) as usize % 3;
        let binding = if seed & 1 == 0 {
          Binding::Local(index)
        } else {
          Binding::Mesh(index)
        };
        let frame = if seed & 1 == 0 {
          &mut expected_locals
        } else {
          &mut expected_mesh
        };
        let op = match (seed >> 8) % 5 {
          0 => {
            expected = Var::Int(7);
            Op::Const(expected.clone())
          }
          1 => {
            expected = frame[index].clone();
            Op::get(binding)
          }
          2 => {
            frame[index] = expected.clone();
            Op::Set(binding)
          }
          3 if matches!(frame[index], Var::Int(_)) => {
            let Var::Int(n) = &mut frame[index] else {
              unreachable!()
            };
            *n += 1;
            expected = frame[index].clone();
            Op::Inc(binding)
          }
          _ if matches!(expected, Var::Int(_)) => {
            expected = add_generic(&expected, &Var::Int(2)).unwrap();
            Op::AddIntConst(2)
          }
          _ => {
            expected = Var::string("replacement");
            Op::Const(expected.clone())
          }
        };
        code.push(op);
      }
      let code = instructions(code);
      let (index, actual) = run(&code, 0, input, &mut locals, &mut mesh, &NoCalls).unwrap();
      assert_eq!(index, code.len());
      assert_eq!(actual, expected);
      assert_eq!(locals, expected_locals);
      assert_eq!(mesh, expected_mesh);
    }
  }

  #[test]
  fn snapshots_survive_frame_and_code_replacement() {
    let mut locals = vec![Var::string("original")];
    let code = instructions([
      Op::get(Binding::Local(0)),
      Op::Set(Binding::Local(0)),
      Op::Fallback,
    ]);
    let (index, output) = run(&code, 0, Var::None, &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!(index, 2);
    locals[0] = Var::string("changed");
    drop(code);
    assert_eq!(output, Var::string("original"));
  }

  #[test]
  fn take_then_replace_owner_and_push_self_preserve_snapshots() {
    use std::sync::Arc;
    let old = Var::from_seq(Arc::new(vec![Var::string("element")]));
    let mut locals = vec![old.clone()];
    let code = instructions([
      Op::get(Binding::Local(0)),
      Op::Take(Operand::Const(Var::Int(0))),
      Op::Set(Binding::Local(0)),
    ]);
    let (_, output) = run(&code, 0, Var::None, &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!(output, Var::string("element"));
    assert_eq!(locals[0], output);
    locals[0] = old.clone();
    let code = instructions([
      Op::get(Binding::Local(0)),
      Op::Push(Binding::Local(0)),
      Op::Push(Binding::Local(0)),
    ]);
    let (_, output) = run(&code, 0, Var::None, &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!(output, old);
    assert_eq!(
      locals[0],
      Var::from_seq(Arc::new(vec![Var::string("element"), old.clone(), old]))
    );
  }

  #[test]
  fn math_matches_generic_path_and_reports_overflow() {
    for (lhs, rhs, op) in [
      (Var::Int(2), Var::Int(3), Op::AddIntBound(Binding::Local(0))),
      (
        Var::Int(2),
        Var::Float(0.5),
        Op::AddFloatBound(Binding::Local(0)),
      ),
      (
        Var::Float4(Float4([1.0, -0.0, f32::MIN_POSITIVE, f32::MAX])),
        Var::Float4(Float4([2.0, -0.0, -f32::MIN_POSITIVE, f32::MAX])),
        Op::AddFloat4Bound(Binding::Local(0)),
      ),
      (
        Var::Int(2),
        Var::Float4(Float4([1.0; 4])),
        Op::AddFloat4Bound(Binding::Local(0)),
      ),
      (
        Var::Float4(Float4([1.0; 4])),
        Var::Int(2),
        Op::AddFloat4Bound(Binding::Local(0)),
      ),
    ] {
      let expected = add_generic(&lhs, &rhs).unwrap();
      let (_, actual) = run(&instructions([op]), 0, lhs, &mut [rhs], &mut [], &NoCalls).unwrap();
      assert_eq!(actual, expected);
    }
    let error = run(
      &instructions([Op::AddIntConst(1)]),
      0,
      Var::Int(i64::MAX),
      &mut [],
      &mut [],
      &NoCalls,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), overflow().to_string());
  }

  #[test]
  fn clear_unsets_a_slot_and_moves_a_value_the_accumulator_reads() {
    use std::sync::Arc;
    let items = Arc::new(vec![Var::Int(7)]);
    let mut locals = vec![Var::from_seq(items.clone()), Var::from_seq(items.clone())];
    // The accumulator reads slot 0 when it is cleared: the value moves into
    // it (no copy, no dangling read); slot 1 is not read and is dropped.
    let code = instructions([
      Op::get(Binding::Local(0)),
      Op::Clear(Binding::Local(0)),
      Op::Clear(Binding::Local(1)),
    ]);
    let (pc, out) = run(&code, 0, Var::None, &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!(pc, 3);
    assert_eq!(locals, [Var::None, Var::None]);
    let Var::Seq(read) = &out else {
      panic!("the read value is the output")
    };
    assert!(Arc::ptr_eq(read, &items));
    drop(out);
    assert_eq!(
      Arc::strong_count(&items),
      1,
      "nothing else holds the sequence"
    );
  }

  #[test]
  fn jumps_branch_loop_and_stop_at_a_site_that_is_not_ready() {
    // A flattened `Repeat` of three over `Inc(local 0)`, the input kept in
    // a hidden slot (local 1) and the counter in another (local 2).
    let mut locals = vec![Var::Int(0), Var::None, Var::None];
    let code = instructions([
      Op::Const(Var::Int(9)),
      Op::Set(Binding::Local(1)),
      Op::Const(Var::Int(3)),
      Op::Set(Binding::Local(2)),
      Op::LoopTest(Binding::Local(2), 8),
      Op::get(Binding::Local(1)),
      Op::Inc(Binding::Local(0)),
      Op::Jump(4),
      Op::get(Binding::Local(1)),
    ]);
    let (pc, out) = run(&code, 0, Var::None, &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!(
      (pc, out),
      (9, Var::Int(9)),
      "the loop's input is its output"
    );
    assert_eq!(locals[0], Var::Int(3));
    // Conditional jumps consume a Bool; anything else is an error.
    for (op, taken) in [(Op::JumpIfNot(3), false), (Op::JumpIf(3), true)] {
      for flag in [true, false] {
        let code = instructions([
          Op::Const(Var::Bool(flag)),
          op.clone(),
          Op::Const(Var::Int(1)),
          Op::Const(Var::Int(2)),
        ]);
        let (_, out) = run(&code, 0, Var::None, &mut [], &mut [], &NoCalls).unwrap();
        let jumped = flag == taken;
        assert_eq!(out, Var::Int(2), "both paths end at the last const");
        let code = instructions([
          Op::Const(Var::Bool(flag)),
          op.clone(),
          Op::Const(Var::Int(1)),
        ]);
        let (_, out) = run(&code, 0, Var::None, &mut [], &mut [], &NoCalls).unwrap();
        assert_eq!(out, if jumped { Var::Bool(flag) } else { Var::Int(1) });
      }
      let code = instructions([Op::Const(Var::Int(1)), op.clone(), Op::Const(Var::Int(2))]);
      let err = run(&code, 0, Var::None, &mut [], &mut [], &NoCalls).unwrap_err();
      assert!(err.to_string().contains("Bool"), "{err}");
    }
    // A counter that is not an Int is an error.
    let mut locals = vec![Var::None];
    let code = instructions([Op::LoopTest(Binding::Local(0), 1)]);
    let err = run(&code, 0, Var::None, &mut locals, &mut [], &NoCalls).unwrap_err();
    assert!(err.to_string().contains("times"), "{err}");
    // A call site that is not ready stops the run before it, mid-loop; the
    // engine takes over at that instruction.
    let mut locals = vec![Var::Int(2)];
    let code = instructions([
      Op::Const(Var::Int(1)),
      Op::LoopTest(Binding::Local(0), 4),
      Op::VmCall,
      Op::Jump(1),
      Op::Const(Var::Int(0)),
    ]);
    let (pc, out) = run(&code, 0, Var::None, &mut locals, &mut [], &NoCalls).unwrap();
    assert_eq!((pc, out), (2, Var::Int(1)));
    assert_eq!(
      locals[0],
      Var::Int(1),
      "the counter already counted this iteration"
    );
    // Scratch releases restart at a jump target.
    let mut code = instructions([
      Op::Const(Var::Int(1)),
      Op::Jump(2),
      Op::Const(Var::Int(2)),
      Op::Const(Var::Int(3)),
    ]);
    lower_scratch_releases(&mut code);
    assert!(matches!(code[0].op, Op::ConstDrop(_)));
    assert!(
      matches!(code[2].op, Op::ConstDrop(_)),
      "a target may receive owned scratch"
    );
    assert!(matches!(code[3].op, Op::Const(_)));
  }
}
