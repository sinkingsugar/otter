//! Compose-time evaluation (`#( ... )`, docs/metaprogramming.md §2): its
//! budgets, the meter that enforces them while the engine runs, and the
//! native shards an evaluation may reach.
//!
//! An evaluation runs on the real engine in a temporary instance with no
//! mesh (`ComposeCtx::evaluate`). Purity does not imply termination, so it
//! is metered: fuel for the work done (one unit per engine dispatch and per
//! VM instruction, one per byte a native shard allocates, one per
//! [`TRAVERSAL_BYTES`] a shard reads through), a depth limit, a limit on
//! each value's size, checked before it is allocated or traversed, and a
//! limit on the result's size. Since every allocation is charged by its
//! size, everything an evaluation allocates together is bounded by its fuel.
//! Runtime code never meters: the VM's metered form is a separate
//! instantiation ([`crate::inline::VmCalls::METERED`]) and the engine checks
//! one `Option` per step.

use std::cell::Cell;

use crate::diagnostic::{Diagnostic, Phase};
use crate::error::{Error, Result};
use crate::shard::ShardType;
use crate::var::Var;

/// The budgets of one compose-time evaluation. Host-configurable
/// (`Mesh::set_eval_limits`); the defaults are small enough for the device,
/// which composes on device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvalLimits {
  /// Engine dispatches and VM instructions, plus the work native shards
  /// charge (see the module documentation).
  pub fuel: u64,
  /// Nested function calls, like the mesh's `max_call_depth` at run time.
  pub depth: usize,
  /// The largest value an evaluation may allocate (by the bytes
  /// allocated) or traverse (by its size as text, [`text_size`]).
  pub value_bytes: usize,
  /// The largest result, by its size as text.
  pub output_bytes: usize,
}

impl Default for EvalLimits {
  fn default() -> EvalLimits {
    if cfg!(target_os = "espidf") {
      // An allocation costs a unit per byte, so the fuel also bounds the
      // heap an evaluation can take. The classic ESP32's heap low-water
      // over the acceptance suites is 9.7 KB (CI at f7d3b25, 2026-10-09,
      // set by the frontend suite's deep function chain), so an
      // evaluation keeps below 8 KB.
      EvalLimits {
        fuel: 8_000,
        depth: 32,
        value_bytes: 4 * 1024,
        output_bytes: 2 * 1024,
      }
    } else {
      EvalLimits {
        fuel: 10_000_000,
        depth: 256,
        value_bytes: 1024 * 1024,
        output_bytes: 64 * 1024,
      }
    }
  }
}

/// What one evaluation used, recorded with its cached result: a cache hit
/// is accepted only by a compose context whose limits cover it, so a host
/// with smaller limits never accepts a result it could not have computed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EvalUsage {
  pub fuel: u64,
  pub depth: usize,
  pub value_bytes: usize,
  pub output_bytes: usize,
}

impl EvalUsage {
  pub fn fits(&self, limits: &EvalLimits) -> bool {
    self.fuel <= limits.fuel
      && self.depth <= limits.depth
      && self.value_bytes <= limits.value_bytes
      && self.output_bytes <= limits.output_bytes
  }
}

/// Bytes a shard reads through per unit of fuel (comparing, printing).
pub const TRAVERSAL_BYTES: usize = 16;

/// How deeply a value built during an evaluation may nest: what reads,
/// prints or drops a value recurses once per level, so a loop that wraps a
/// value in itself must stop long before the stack does.
pub const VALUE_DEPTH: usize = 64;

/// The meter of one running evaluation. Shards reach it through
/// [`crate::instance::LeafCtx::meter`], which is `None` at run time.
pub struct Meter {
  limits: EvalLimits,
  fuel: Cell<u64>,
  depth: Cell<usize>,
  largest: Cell<usize>,
  /// The evaluation runs again to locate its failure: the activation
  /// records `trace` (`Engine::steps::<true>`).
  pub(crate) locating: bool,
  /// The instruction the innermost metered VM run is at (`at`), read when
  /// a run fails.
  pc: Cell<usize>,
  /// Where the failure being propagated passed, innermost first
  /// (`stackless::failure_path`): each frame that fails adds itself as it
  /// completes, and a composite that handles the failure clears it.
  pub(crate) trace: std::cell::RefCell<Vec<crate::stackless::Failed>>,
}

impl Meter {
  pub(crate) fn new(limits: EvalLimits) -> Meter {
    Meter {
      limits,
      fuel: Cell::new(0),
      depth: Cell::new(0),
      largest: Cell::new(0),
      locating: false,
      pc: Cell::new(0),
      trace: Default::default(),
    }
  }

  /// A meter for running a failed evaluation again to locate the failure.
  pub(crate) fn locating(limits: EvalLimits) -> Meter {
    Meter {
      locating: true,
      ..Meter::new(limits)
    }
  }

  /// The VM is at instruction `pc` (metered runs only).
  #[inline(always)]
  pub(crate) fn at(&self, pc: usize) {
    self.pc.set(pc);
  }

  /// The instruction the VM was last at.
  pub(crate) fn pc(&self) -> usize {
    self.pc.get()
  }

  pub(crate) fn limits(&self) -> &EvalLimits {
    &self.limits
  }

  /// Spends `units` of fuel; past the budget, every further charge fails
  /// too, so nothing that catches the error can keep the evaluation going.
  pub fn charge(&self, units: u64) -> Result<()> {
    let used = self.fuel.get().saturating_add(units);
    self.fuel.set(used);
    if used > self.limits.fuel {
      return Err(budget(format!(
        "the evaluation ran out of fuel (limit {})",
        self.limits.fuel
      )));
    }
    Ok(())
  }

  /// `bytes` are about to be allocated for a value: they must be within the
  /// value limit, and the allocation is charged a unit per byte.
  pub fn allocate(&self, bytes: usize) -> Result<()> {
    self.admit(bytes)?;
    self.charge(bytes as u64)
  }

  /// A value grows to `bytes` in place (amortized, nothing copied): only
  /// the value limit applies. The caller charges the work it does.
  pub fn admit(&self, bytes: usize) -> Result<()> {
    if bytes > self.limits.value_bytes {
      return Err(budget(format!(
        "a value would grow past the value limit of {} bytes",
        self.limits.value_bytes
      )));
    }
    self.largest.set(self.largest.get().max(bytes));
    Ok(())
  }

  /// A value a shard is about to read through (compare, print): measured
  /// with an early exit, it must be within the value limit, and the read is
  /// charged by its size. Returns the size.
  pub fn traverse(&self, value: &Var) -> Result<usize> {
    let Some(bytes) = text_size(value, self.limits.value_bytes) else {
      return Err(budget(format!(
        "a value to compare or print is larger than the value limit of {} bytes",
        self.limits.value_bytes
      )));
    };
    self.charge((bytes / TRAVERSAL_BYTES + 1) as u64)?;
    Ok(bytes)
  }

  /// `value` is about to become an element of a new or grown sequence or
  /// table: it must nest less than [`VALUE_DEPTH`] levels. Measured with an
  /// early exit at that depth, and charged a unit per element read.
  pub fn nest(&self, value: &Var) -> Result<()> {
    // A widely shared value can hold far more elements than memory: the
    // walk stops when it has read as many as the fuel left pays for.
    let left = self.limits.fuel.saturating_sub(self.fuel.get());
    let mut nodes = 0u64;
    let mut walk = Walk::new(value);
    while let Some((v, level)) = walk.next() {
      nodes += 1;
      if nodes > left {
        return self.charge(nodes);
      }
      if level >= VALUE_DEPTH {
        self.charge(nodes)?;
        return Err(budget(format!(
          "a value would nest deeper than {VALUE_DEPTH} levels"
        )));
      }
      walk.enter(v);
    }
    self.charge(nodes)
  }

  /// Records a call depth reached.
  pub(crate) fn note_depth(&self, depth: usize) {
    self.depth.set(self.depth.get().max(depth));
  }

  pub(crate) fn usage(&self, output_bytes: usize) -> EvalUsage {
    EvalUsage {
      fuel: self.fuel.get(),
      depth: self.depth.get(),
      value_bytes: self.largest.get(),
      output_bytes,
    }
  }
}

/// An `expansion-budget` diagnostic (located by the caller at the `#( )`).
#[cold]
pub(crate) fn budget(message: String) -> Error {
  Error::Diagnostic(Box::new(Diagnostic::new(
    Phase::Compose,
    "compose-error",
    "expansion-budget",
    message,
  )))
}

/// A value's size as text (an upper bound of [`Var::text`]'s length, which
/// prints strings as they are), or `None` past `cap`. Iterative, and it
/// stops as soon as the size passes `cap`, so a deeply nested or widely
/// shared value costs at most `cap` steps to measure.
pub fn text_size(value: &Var, cap: usize) -> Option<usize> {
  let mut size = 0usize;
  let mut walk = Walk::new(value);
  while let Some((v, _)) = walk.next() {
    size += match v {
      Var::None | Var::Bool(_) => 5,
      Var::Int(_) => 20,
      // The longest shortest form of an f64, exponent included.
      Var::Float(_) => 32,
      Var::Float2(_) => 8 + 2 * 33,
      Var::Float3(_) => 8 + 3 * 33,
      Var::Float4(_) => 8 + 4 * 33,
      Var::String(s) => s.len(),
      // `@bytes(`, two digits a byte, `)`.
      Var::Bytes(b) => 8 + 2 * b.len(),
      Var::Seq(items) => 2 + items.len(),
      Var::Table(table) => {
        let mut keys = 2;
        for k in table.keys() {
          keys += k.len() + 3;
          if size + keys > cap {
            return None;
          }
        }
        keys
      }
    };
    if size > cap {
      return None;
    }
    walk.enter(v);
  }
  Some(size)
}

/// A depth-first walk over a value that keeps one iterator per level open,
/// so its memory grows with the nesting, not with the number of elements
/// waiting to be read.
struct Walk<'a> {
  next: Option<&'a Var>,
  open: Vec<Children<'a>>,
}

enum Children<'a> {
  Seq(std::slice::Iter<'a, Var>),
  Table(crate::var::Iter<'a>),
}

impl<'a> Walk<'a> {
  fn new(value: &'a Var) -> Walk<'a> {
    Walk {
      next: Some(value),
      open: Vec::new(),
    }
  }

  /// The next value and its level (the root's is 1).
  fn next(&mut self) -> Option<(&'a Var, usize)> {
    loop {
      if let Some(v) = self.next.take() {
        return Some((v, self.open.len() + 1));
      }
      let item = match self.open.last_mut()? {
        Children::Seq(items) => items.next(),
        Children::Table(entries) => entries.next().map(|(_, v)| v),
      };
      match item {
        Some(v) => self.next = Some(v),
        None => {
          self.open.pop();
        }
      }
    }
  }

  /// Visits the elements of `value`, the value just returned, next.
  fn enter(&mut self, value: &'a Var) {
    match value {
      Var::Seq(items) => self.open.push(Children::Seq(items.iter())),
      Var::Table(table) => self.open.push(Children::Table(table.iter())),
      _ => {}
    }
  }
}

/// The native shards a compose-time evaluation may reach, with the version
/// each was audited at: their work is constant, or charged to the meter in
/// proportion (per byte allocated or traversed), and they check the value
/// limit before allocating. A shard type is eligible only by identity and
/// at the listed version, so a new version is excluded until it is audited
/// again; host shards are never eligible. Shards with effects or persistent
/// state are absent (`Keep`, `Once`, `Log`, `Maybe`, which logs what it
/// catches, `Time.Now`, `Pause`, `Spawn`), as are `Stop` and `Return`,
/// which have no value to give.
static SAFE: &[(&ShardType, u32)] = {
  use crate::shards::{data, math, values, *};
  &[
    // Constant work.
    (&CONST, 1),
    (&VAR, 1),
    (&BIND, 1),
    (&UPDATE, 1),
    (&GET, 1),
    (&INC, 1),
    (&ADD, 1),
    (&math::SUBTRACT, 1),
    (&math::MULTIPLY, 1),
    (&math::DIVIDE, 1),
    (&math::DEC, 1),
    (&math::ABS, 1),
    (&math::ROUND, 1),
    (&math::FLOOR, 1),
    (&math::CEIL, 1),
    (&math::LENGTH, 1),
    (&IS_LESS, 1),
    (&IS_MORE_EQUAL, 1),
    (&values::IS_MORE, 1),
    (&values::IS_LESS_EQUAL, 1),
    (&values::NOT, 1),
    (&values::IS_NONE, 1),
    (&values::IS_NOT_NONE, 1),
    (&values::TO_FLOAT2, 1),
    (&values::TO_FLOAT3, 1),
    (&values::TO_FLOAT4, 1),
    (&data::TAKE, 1),
    // Composites: the engine and the VM charge their dispatches; `Match`
    // charges each case it compares by size.
    (&WHEN, 1),
    (&IF, 1),
    (&MATCH, 1),
    (&ALL, 1),
    (&ANY, 1),
    (&SUB, 1),
    (&REPEAT, 1),
    (&WHILE, 1),
    // Runs the block its call passed, which composes (and is checked)
    // at the call site like any other code there.
    (&RUN, 1),
    (&CALL, 1),
    // Charged by size: allocation, traversal, or a string's bytes.
    (&data::PUSH, 1),
    (&data::SEQ_MAKE, 1),
    (&data::TABLE_MAKE, 1),
    (&data::STRING_FORMAT, 1),
    (&values::IS, 1),
    (&values::IS_NOT, 1),
    (&values::IS_ANY, 1),
    (&values::COUNT, 1),
    (&values::TO_STRING, 1),
    (&values::TO_INT, 1),
    (&values::TO_FLOAT, 1),
    (&values::TO_HEX, 1),
    (&values::BYTES_TO_STRING, 1),
    (&values::STRING_TO_BYTES, 1),
    (&values::PARSE_INT, 1),
    (&values::PARSE_FLOAT, 1),
    (&values::EXPECT_INT, 1),
    (&values::EXPECT_FLOAT, 1),
    (&values::EXPECT_BOOL, 1),
    (&values::EXPECT_STRING, 1),
    (&values::EXPECT_SEQ, 1),
    (&values::EXPECT_TABLE, 1),
    (&values::EXPECT_FLOAT2, 1),
    (&values::EXPECT_FLOAT3, 1),
    (&values::EXPECT_FLOAT4, 1),
  ]
};

/// Whether a compose-time evaluation may run this shard type. Asked for
/// every node compose builds, so the list is searched by address.
pub fn eligible(ty: &ShardType) -> bool {
  static SORTED: std::sync::OnceLock<Vec<(usize, u32)>> = std::sync::OnceLock::new();
  let sorted = SORTED.get_or_init(|| {
    let mut sorted: Vec<(usize, u32)> = SAFE
      .iter()
      .map(|(ty, version)| (std::ptr::from_ref(*ty) as usize, *version))
      .collect();
    sorted.sort_unstable();
    sorted
  });
  let address = std::ptr::from_ref(ty) as usize;
  sorted
    .binary_search_by_key(&address, |(a, _)| *a)
    .is_ok_and(|i| sorted[i].1 == ty.desc.version)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  #[test]
  fn text_size_bounds_text_and_stops_at_the_cap() {
    let values = [
      Var::None,
      Var::Bool(false),
      Var::Int(i64::MIN),
      Var::Float(-1.7976931348623157e308),
      Var::Float(1.2345678901234567e-300),
      Var::Float(f64::NAN),
      Var::float4(f32::MIN, f32::MIN_POSITIVE, -0.0, 1.0e-45),
      Var::string("héllo"),
      Var::from_seq(Arc::new(vec![Var::Int(1), Var::string("a b")])),
      Var::table([("key", Var::Float(0.1)), ("k", Var::None)]),
    ];
    for v in &values {
      let size = text_size(v, usize::MAX).unwrap();
      assert!(v.text().len() <= size, "{v}: {} > {size}", v.text().len());
    }
    // A widely shared value: 2^40 elements as text, measured in a few steps.
    let mut shared = Var::Int(1);
    for _ in 0..40 {
      shared = Var::from_seq(Arc::new(vec![shared.clone(), shared]));
    }
    assert_eq!(text_size(&shared, 1000), None);
  }

  #[test]
  fn walks_hold_one_iterator_per_level() {
    // Three levels of 1000 elements: a walk that queued the waiting
    // elements would hold thousands at once.
    let row = Var::from_seq(Arc::new(vec![Var::Int(1); 1000]));
    let table = Var::table([("row", row.clone())]);
    let value = Var::from_seq(Arc::new(vec![table; 1000]));
    let mut walk = Walk::new(&value);
    let (mut nodes, mut widest) = (0, 0);
    while let Some((v, level)) = walk.next() {
      nodes += 1;
      widest = widest.max(walk.open.len());
      assert!(level <= 4);
      walk.enter(v);
    }
    assert_eq!(nodes, 1 + 1000 * (2 + 1000));
    assert_eq!(widest, 3);
    // Nesting is measured the same way, and charged a unit per element.
    let meter = Meter::new(EvalLimits {
      fuel: 10_000_000,
      ..EvalLimits::default()
    });
    meter.nest(&value).unwrap();
    assert_eq!(meter.usage(0).fuel, nodes);
  }

  #[test]
  fn the_safe_list_names_catalog_shards_at_their_versions() {
    for (ty, version) in SAFE {
      assert_eq!(ty.desc.version, *version, "{} was not audited", ty.name());
    }
    assert!(!eligible(&crate::shards::KEEP));
    assert!(!eligible(&crate::shards::values::LOG));
    assert!(eligible(&crate::shards::CONST));
  }
}
