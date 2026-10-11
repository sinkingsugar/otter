//! Reload policy for functions (golden path §11).
//!
//! A call site selects the newest accepted body at invocation entry; an
//! invocation in flight finishes on the body it started with. A candidate
//! body is admitted only if its input, output and parameters match what the
//! callers were composed with, and its effects and mesh access fit inside
//! theirs. A stateless callee may change its locals freely. A stateful
//! callee keeps `Keep` slots that match by name and type; the others reset,
//! and whether a reset is acceptable is the host's policy. Compiled
//! artifacts are never mutated by a reload: selection is scoped to a mesh.

use crate::hash::HashMap;
use std::cell::Cell;
use std::sync::Arc;

use crate::compose::{CompiledWire, ComposeEnv};
use crate::diagnostic::{Diagnostic, PathStep, Phase};
use crate::function::CompiledFunction;
use crate::{Error, Result, Type};

/// A compiled body's identity for selection: the function's name and the
/// input type it was composed for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FunctionKey {
  pub name: String,
  pub input: Type,
}

impl FunctionKey {
  pub fn of(body: &CompiledFunction) -> FunctionKey {
    FunctionKey {
      name: body.def.name.clone(),
      input: body.input,
    }
  }
}

/// The bodies a compiled wire or function was composed against, by key.
pub type FunctionRegistry = HashMap<FunctionKey, Arc<CompiledFunction>>;

/// A mesh's installed revision: the bodies call sites select at entry.
/// Keys are structural, so a call site consults the registry once per
/// accepted reload, when `revision` changes, rather than on every call.
pub struct Revisions {
  pub functions: FunctionRegistry,
  /// Accepted preserving reloads; 0 means nothing was ever installed.
  pub revision: u64,
  /// Registry lookups made at call entries, for tests and diagnostics.
  pub lookups: Cell<u64>,
  /// The mesh's services (`Mesh::services`). Kept here because every
  /// activation already holds this struct: reaching them costs the
  /// scheduler nothing per activation, where a pointer of their own cost
  /// two instructions each (instruction gate, 2026-10-11).
  pub(crate) services: crate::service::Services,
}

impl Default for Revisions {
  fn default() -> Self {
    Self {
      functions: HashMap::default(),
      revision: 0,
      lookups: Cell::new(0),
      services: Default::default(),
    }
  }
}

impl Revisions {
  pub fn select(&self, key: &FunctionKey) -> Option<Arc<CompiledFunction>> {
    self.lookups.set(self.lookups.get() + 1);
    self.functions.get(key).cloned()
  }

  pub fn install(&mut self, functions: FunctionRegistry) {
    self.functions = functions;
    self.revision += 1;
  }
}

/// What a host does with a preserving reload that would reset persistent
/// state (a `Keep` slot that changed type or disappeared, native state in
/// a stateful function): reject the candidate, or apply the resets and
/// report them. `otter watch` applies; an embedding host rejects by
/// default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResetPolicy {
  #[default]
  Reject,
  Apply,
}

/// What an accepted preserving reload did to state, by name
/// (`Function.slot` for `Keep` slots, the wire's name for restarted roots).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReloadReport {
  pub retained: Vec<String>,
  pub reset: Vec<String>,
  pub restarted: Vec<String>,
  /// Wires whose instances take the candidate's body because a function
  /// inlined into it changed: an iteration in flight finishes on the old
  /// body, the next one runs the new body with the instance's state (its
  /// `Keep` slots by name and type, its nodes' and components' state). A
  /// started run of a wire that does not loop finishes on the old body and
  /// is not listed.
  pub swapped: Vec<String>,
}

impl ReloadReport {
  pub fn is_empty(&self) -> bool {
    self.retained.is_empty()
      && self.reset.is_empty()
      && self.restarted.is_empty()
      && self.swapped.is_empty()
  }
}

/// What a preserving reload does with a live instance of `wire`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retention {
  /// The compiled body is still the program the candidate declares.
  Reuse,
  /// Only functions inlined into the body changed: the instance moves onto
  /// the candidate's body at its next iteration, with its state.
  Swap,
  /// The wire itself, or something it depends on at compose, changed.
  Restart,
}

pub(crate) fn retention(wire: &CompiledWire, env: &ComposeEnv<'_>) -> Retention {
  use crate::compose::Dep;
  if env.wires.get(&wire.name) != Some(wire.definition.as_ref()) {
    return Retention::Restart;
  }
  let mut retention = Retention::Reuse;
  for dep in &wire.restart_deps {
    if dep.still_valid(env) {
      continue;
    }
    match dep {
      Dep::Inlined { .. } => retention = Retention::Swap,
      // A framed callee (here through a spawned wire's call sites): its
      // call sites select the new body at entry, the code is unchanged.
      Dep::Function { .. } => {}
      Dep::MeshVar { .. } | Dep::Wire { .. } => return Retention::Restart,
      // Recorded in `deps` only: an evaluation's usage is deterministic, so
      // once it no longer fits the limits the candidate fails to compose
      // (`expansion-budget`) and no instance is retained or restarted.
      Dep::Evaluation(_) => {}
    }
  }
  retention
}

fn incompatible(name: &str, reason: String) -> Error {
  let mut d = Diagnostic::new(
    Phase::Compose,
    "reload-incompatible",
    "reload-incompatible",
    format!("cannot preserve callers of {name}: {reason}; use a full restart (press r in watch)"),
  )
  .shard(name);
  d.path = vec![PathStep::Function(name.to_string())];
  Error::Diagnostic(Box::new(d))
}

/// Whether `new` may replace `old` under the callers composed against
/// `old`: same interface, and no effect or mesh access the callers were not
/// admitted with.
pub(crate) fn admit(old: &CompiledFunction, new: &CompiledFunction) -> Result<()> {
  let name = &old.def.name;
  if old.def.output != new.def.output {
    return Err(incompatible(
      name,
      format!(
        "output type changed from {} to {}",
        old.def.output, new.def.output
      ),
    ));
  }
  if old.def.params != new.def.params {
    let reason = match old
      .def
      .params
      .iter()
      .zip(&new.def.params)
      .find(|(a, b)| a != b)
    {
      Some((a, b)) if a.name != b.name => {
        format!("parameter `{}` became `{}`", a.name, b.name)
      }
      Some((a, b)) if a.ty != b.ty => {
        format!(
          "parameter `{}` changed type from {} to {}",
          a.name, a.ty, b.ty
        )
      }
      Some((a, _)) => format!("parameter `{}` changed its default", a.name),
      None if old.def.params.len() < new.def.params.len() => format!(
        "new parameter `{}`",
        new.def.params[old.def.params.len()].name
      ),
      None => format!(
        "parameter `{}` was removed",
        old.def.params[new.def.params.len()].name
      ),
    };
    return Err(incompatible(name, reason));
  }
  if old.def.stateful != new.def.stateful {
    return Err(incompatible(
      name,
      format!(
        "it became {}",
        if new.def.stateful {
          "stateful"
        } else {
          "stateless"
        }
      ),
    ));
  }
  let (before, after) = (old.flow.analysis.effects, new.flow.analysis.effects);
  for (label, was, is) in [
    ("suspends", before.suspends, after.suspends),
    ("io", before.io, after.io),
    ("time", before.time, after.time),
    ("random", before.random, after.random),
    ("unknown", before.unknown, after.unknown),
  ] {
    if is && !was {
      return Err(incompatible(
        name,
        format!("it gained the effect `{label}`, which its callers were not admitted with"),
      ));
    }
  }
  for (kind, before, after) in [
    ("reads", &old.flow.analysis.uses, &new.flow.analysis.uses),
    (
      "assigns",
      &old.flow.analysis.mutates,
      &new.flow.analysis.mutates,
    ),
  ] {
    if let Some(access) = after.iter().find(|a| !before.contains(a)) {
      return Err(incompatible(
        name,
        format!(
          "it now {kind} mesh variable {}, which its callers were not admitted with",
          access.name
        ),
      ));
    }
  }
  Ok(())
}

/// How a stateful body's `Keep` slots carry over: `(old slot, new slot)`
/// for each retained slot, by name and type; the others start at the new
/// body's initial values.
pub(crate) struct KeepPlan {
  pub moves: Vec<(usize, usize)>,
  pub retained: Vec<String>,
  pub reset: Vec<String>,
}

pub(crate) fn keep_plan(old: &CompiledFunction, new: &CompiledFunction) -> KeepPlan {
  let mut plan = KeepPlan {
    moves: Vec::new(),
    retained: Vec::new(),
    reset: Vec::new(),
  };
  let name = &old.def.name;
  for keep in &old.keeps {
    match new
      .keeps
      .iter()
      .find(|k| k.name == keep.name && k.ty == keep.ty)
    {
      Some(target) => {
        plan.moves.push((keep.slot, target.slot));
        plan.retained.push(format!("{name}.{}", keep.name));
      }
      None => plan.reset.push(format!("{name}.{}", keep.name)),
    }
  }
  // Native shard state inside the body (and nested components) declares no
  // compatibility contract yet, so it restarts with the body; say so when
  // the old body held any beyond its Keep slots.
  let keeps_only = old.flow.analysis.occurrences.iter().all(|o| {
    o.lifetime != crate::signature::Lifetime::Stateful
      || matches!(o.path.last(), Some(PathStep::Shard { name, .. }) if name == "Keep")
  });
  if !keeps_only {
    plan.reset.push(format!("{name} (native state)"));
  }
  plan
}

/// Whether a candidate body is the same program as the body callers hold:
/// the same definition, and the same dependencies except for callee
/// definitions. A changed callee is reached through selection at entry, so
/// the caller's own body (and its state) need not change for it.
pub(crate) fn same_body(old: &CompiledFunction, new: &CompiledFunction) -> bool {
  use crate::compose::Dep;
  let static_deps = |body: &CompiledFunction| -> Vec<Dep> {
    body
      .deps
      .iter()
      .filter(|d| !matches!(d, Dep::Function { .. }))
      .cloned()
      .collect()
  };
  old.def == new.def && static_deps(old) == static_deps(new)
}

/// Reuses the previous body where a candidate recomposed the same program,
/// so call sites see no new body and keep their state.
pub(crate) fn reuse_unchanged(old: &FunctionRegistry, next: &mut FunctionRegistry) {
  for (key, new) in next {
    if let Some(previous) = old.get(key)
      && same_body(previous, new)
    {
      *new = previous.clone();
    }
  }
}
