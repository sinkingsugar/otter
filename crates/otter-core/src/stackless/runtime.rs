//! The mesh: scheduling over the directly resumable frame runner. Each tick
//! resumes the active innermost leaf; a Suspend parks its frame.

use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::task::Waker;

use super::Engine;
use crate::compose::{CacheStats, CompiledWire, ComposeCache, ComposeEnv, FrameLayout, WireDef};
use crate::error::{Error, Result, panic_message};
use crate::function::{CompiledFunction, FunctionDef};
use crate::instance::{InstanceCtx, InstanceId, InstanceMemory, Outcome, WakeFlag, WakeMode};
use crate::reload::{FunctionKey, FunctionRegistry, ReloadReport, ResetPolicy, Revisions};
use crate::shard::{ActivationCtx, Step};
use crate::types::Type;
use crate::var::Var;

struct Instance {
  id: InstanceId,
  wire: Arc<CompiledWire>,
  input: Var,
  locals: Vec<Var>,
  /// `None` until instantiated (on the instance's first tick), and after
  /// cleanup.
  state: Option<Engine>,
  started: bool,
  memory: InstanceMemory,
  outcome: Option<Outcome>,
  /// Waiting on its waker (set by `ActivationCtx::set_waiting`).
  waiting: bool,
  wake: Arc<WakeFlag>,
  waker: Waker,
  /// The loop iteration ([`crate::instance::LeafCtx::iteration`]).
  iteration: u64,
  /// A body of the same wire accepted by a reload (a function inlined into
  /// it changed): the instance moves onto it at the end of the iteration
  /// in flight (`rebase`).
  next_body: Option<Arc<CompiledWire>>,
}

pub struct Mesh {
  /// The bodies call sites select at entry, after accepted reloads.
  revisions: Revisions,
  layout: FrameLayout,
  frame: Vec<Var>,
  spawn_queue: Vec<(Arc<CompiledWire>, Var)>,
  wires: HashMap<String, WireDef>,
  functions: HashMap<String, FunctionDef>,
  cache: ComposeCache,
  /// Every function body compiled wires were composed against, by key.
  prepared_functions: FunctionRegistry,
  /// `prepared_functions` as shared by running instances (recursive call
  /// sites resolve through the table pinned at their outermost entry).
  table: Arc<FunctionRegistry>,
  reset_policy: ResetPolicy,
  report: ReloadReport,
  instances: Vec<Instance>,
  next_id: InstanceId,
  wake_mode: WakeMode,
  max_call_depth: usize,
  eval_limits: crate::compose_time::EvalLimits,
}

impl Default for Mesh {
  fn default() -> Mesh {
    Mesh::new()
  }
}

impl Mesh {
  pub fn new() -> Mesh {
    Mesh::with_cache(ComposeCache::default())
  }

  pub fn with_cache(cache: ComposeCache) -> Mesh {
    Mesh {
      revisions: Revisions::default(),
      layout: FrameLayout::default(),
      frame: Vec::new(),
      spawn_queue: Vec::new(),
      wires: HashMap::new(),
      functions: HashMap::new(),
      cache,
      prepared_functions: HashMap::new(),
      table: Arc::new(HashMap::new()),
      reset_policy: ResetPolicy::default(),
      report: ReloadReport::default(),
      instances: Vec::new(),
      next_id: 0,
      wake_mode: WakeMode::default(),
      max_call_depth: if cfg!(target_os = "espidf") { 32 } else { 256 },
      eval_limits: crate::compose_time::EvalLimits::default(),
    }
  }

  /// Limit nested named invocations independently of the compose nesting limit.
  pub fn set_max_call_depth(&mut self, depth: usize) {
    self.max_call_depth = depth;
  }

  pub fn max_call_depth(&self) -> usize {
    self.max_call_depth
  }

  /// The budgets of compose-time evaluations (`#( ... )`) in what this mesh
  /// composes. A cached result computed under larger limits is not
  /// accepted under smaller ones.
  pub fn set_eval_limits(&mut self, limits: crate::compose_time::EvalLimits) {
    self.eval_limits = limits;
  }

  /// Records how each call site runs in the composed analysis
  /// (`Occurrence::call`), for tooling such as `check --json`. Off by
  /// default: it costs memory per call occurrence.
  pub fn record_call_sites(&mut self) {
    self.cache.record_call_sites = true;
  }

  pub fn eval_limits(&self) -> crate::compose_time::EvalLimits {
    self.eval_limits
  }

  pub fn set_wake_mode(&mut self, mode: WakeMode) {
    self.wake_mode = mode;
  }

  /// What a preserving reload does when a stateful function's state would
  /// reset (golden path §11). Rejecting is the embedding default; a
  /// candidate from `revision` inherits the policy.
  pub fn set_reset_policy(&mut self, policy: ResetPolicy) {
    self.reset_policy = policy;
  }

  pub fn reset_policy(&self) -> ResetPolicy {
    self.reset_policy
  }

  /// What the last accepted preserving reload retained, reset and
  /// restarted, by name.
  pub fn reload_report(&self) -> &ReloadReport {
    &self.report
  }

  /// Reserves space for a known batch of additional instances without the
  /// spare capacity of geometric growth. Does not create or activate instances.
  pub fn reserve_instances(&mut self, additional: usize) {
    self.instances.reserve_exact(additional);
  }

  pub fn add_wire(&mut self, def: WireDef) {
    self.wires.insert(def.name.clone(), def);
  }

  /// Declares a script function, callable from every wire and function of
  /// this mesh by name.
  pub fn add_function(&mut self, def: FunctionDef) {
    self.functions.insert(def.name.clone(), def);
  }

  /// Composes a function against its declared input type, as a call site
  /// would, without a caller: for checking definitions nothing calls yet.
  pub fn compile_function(&mut self, name: &str) -> Result<Arc<CompiledFunction>> {
    let def = self
      .functions
      .get(name)
      .cloned()
      .map(Arc::new)
      .ok_or_else(|| Error::Compose(format!("unknown function: {name}")))?;
    let env = ComposeEnv {
      mesh_layout: &self.layout,
      wires: &self.wires,
      functions: &self.functions,
      eval: self.eval_limits,
    };
    self
      .cache
      .get_or_compose_function(&def, def.input, &env, &mut Vec::new(), 0)
  }

  /// Declares a mesh variable, typed by its initial value.
  pub fn declare_var(&mut self, name: &str, value: Var, mutable: bool) {
    assert!(
      self.layout.lookup(name).is_none(),
      "mesh variable {name} already declared"
    );
    self.layout.declare(name, value.type_of(), mutable);
    self.frame.push(value);
  }

  pub fn get_var(&self, name: &str) -> Option<Var> {
    self
      .layout
      .lookup(name)
      .map(|slot| self.frame[slot.index].clone())
  }

  pub fn set_var(&mut self, name: &str, value: Var) {
    let slot = self.layout.lookup(name).expect("unknown mesh variable");
    assert!(
      slot.ty.admits(&value),
      "mesh variable {name} is {}; the value does not fit",
      slot.ty
    );
    self.frame[slot.index] = value;
  }

  pub fn compile(&mut self, name: &str, input: Type) -> Result<Arc<CompiledWire>> {
    let def = self
      .wires
      .get(name)
      .cloned()
      .ok_or_else(|| Error::Compose(format!("unknown wire: {name}")))?;
    let env = ComposeEnv {
      mesh_layout: &self.layout,
      wires: &self.wires,
      functions: &self.functions,
      eval: self.eval_limits,
    };
    let wire = self
      .cache
      .get_or_compose(&def, input, &env, &mut Vec::new(), 0)?;
    let mut added = false;
    for (key, body) in &wire.functions {
      if !self.prepared_functions.contains_key(key) {
        self.prepared_functions.insert(key.clone(), body.clone());
        added = true;
      }
    }
    if added {
      self.table = Arc::new(self.prepared_functions.clone());
    }
    Ok(wire)
  }

  /// A fresh compilation candidate retaining the mesh-variable schema and
  /// values, without instances or a compose cache.
  pub fn revision(&self) -> Self {
    let mut next = Self::new();
    next.layout = self.layout.clone();
    next.frame = self.frame.clone();
    next.wake_mode = self.wake_mode;
    next.max_call_depth = self.max_call_depth;
    next.eval_limits = self.eval_limits;
    next.reset_policy = self.reset_policy;
    next
  }

  pub fn can_retain(&self, wire: &CompiledWire) -> bool {
    self.retention(wire) != crate::reload::Retention::Restart
  }

  fn retention(&self, wire: &CompiledWire) -> crate::reload::Retention {
    crate::reload::retention(
      wire,
      &ComposeEnv {
        mesh_layout: &self.layout,
        wires: &self.wires,
        functions: &self.functions,
        eval: self.eval_limits,
      },
    )
  }

  /// Checks all live callers before committing a preserving reload: every
  /// retained instance's call sites must admit the candidate's bodies
  /// (golden path §11), and resets of persistent state are allowed only
  /// under the `Apply` policy. The candidate's report lists what an
  /// install will retain, reset and restart.
  pub fn validate_reload(&self, next: &mut Self, removed: &HashSet<InstanceId>) -> Result<()> {
    if self.layout != next.layout {
      return Err(Error::Compose(
        "preserving reload requires the same mesh-variable layout".into(),
      ));
    }
    // Detached children may have input specializations not reached by the
    // new entry graph. Prepare those too before selecting their bodies.
    for instance in &self.instances {
      if !removed.contains(&instance.id)
        && instance.outcome.is_none()
        && next.can_retain(&instance.wire)
      {
        next.compile(&instance.wire.name, instance.wire.input)?;
      }
    }
    let mut report = ReloadReport::default();
    let mut seen = HashSet::new();
    for instance in &self.instances {
      if removed.contains(&instance.id) || instance.outcome.is_some() {
        continue;
      }
      if !next.can_retain(&instance.wire) {
        report.restarted.push(instance.wire.name.clone());
        continue;
      }
      if instance.started
        && instance.wire.looped
        && next.retention(&instance.wire) == crate::reload::Retention::Swap
      {
        let candidate = next.compile(&instance.wire.name, instance.wire.input)?;
        if !super::engine::rebasable(&instance.wire.flow, &candidate.flow) {
          return Err(crate::Error::Diagnostic(Box::new(
            crate::diagnostic::Diagnostic::new(
              crate::diagnostic::Phase::Compose,
              "reload-incompatible",
              "reload-incompatible",
              format!(
                "cannot move the running {} onto its new body with its state: it changed outside the inlined calls; use a full restart (press r in watch)",
                instance.wire.name
              ),
            ),
          )));
        }
      }
      // Admission is against the bodies the wire's call sites were composed
      // with (their interface) and the bodies its sites actually hold now
      // (a site may have adopted a newer body under an earlier reload); the
      // state plan is for the live bodies, which are what migrates. An
      // instance not started yet holds what it was composed with.
      let composed: Vec<(&Arc<CompiledFunction>, bool)> = instance
        .wire
        .functions
        .values()
        .map(|b| (b, instance.state.is_none()))
        .collect();
      let live: Vec<(&Arc<CompiledFunction>, bool)> = instance
        .state
        .iter()
        .flat_map(|engine| engine.function_bodies())
        .map(|b| (b, true))
        .collect();
      let mut olds: Vec<(&Arc<CompiledFunction>, bool)> =
        composed.into_iter().chain(live).collect();
      // Functions whose own definition changed are admitted first: a body
      // that changed only because it inlined one of them fails for the
      // same reason, and the diagnostic should name the edited function.
      let edited = |body: &CompiledFunction| {
        next
          .prepared_functions
          .get(&FunctionKey::of(body))
          .is_some_and(|new| new.def != body.def)
      };
      olds.sort_by(|a, b| {
        edited(b.0)
          .cmp(&edited(a.0))
          .then(a.0.def.name.cmp(&b.0.def.name))
          .then(Arc::as_ptr(a.0).cmp(&Arc::as_ptr(b.0)))
          .then(b.1.cmp(&a.1))
      });
      olds.dedup_by(|a, b| Arc::ptr_eq(a.0, b.0));
      for (old, plans) in olds {
        let key = FunctionKey::of(old);
        let Some(new) = next.prepared_functions.get(&key) else {
          return Err(crate::Error::Diagnostic(Box::new(
            crate::diagnostic::Diagnostic::new(
              crate::diagnostic::Phase::Compose,
              "reload-incompatible",
              "reload-incompatible",
              format!(
                "cannot preserve callers of {}: it is no longer declared; use a full restart (press r in watch)",
                key.name
              ),
            )
            .shard(&key.name),
          )));
        };
        if crate::reload::same_body(old, new) {
          continue;
        }
        crate::reload::admit(old, new)?;
        if plans && old.def.stateful && seen.insert((key.clone(), Arc::as_ptr(old))) {
          let plan = crate::reload::keep_plan(old, new);
          report.retained.extend(plan.retained);
          report.reset.extend(plan.reset);
        }
      }
    }
    if self.reset_policy == ResetPolicy::Reject
      && let Some(first) = report.reset.first()
    {
      let _ = first;
      return Err(crate::Error::Diagnostic(Box::new(
        crate::diagnostic::Diagnostic::new(
          crate::diagnostic::Phase::Compose,
          "reload-incompatible",
          "reload-resets-state",
          format!(
            "the edit would reset persistent state ({}); the host rejects resets (apply them with ResetPolicy::Apply, as watch does), or use a full restart (press r in watch)",
            report.reset.join(", ")
          ),
        ),
      )));
    }
    report.restarted.sort();
    report.restarted.dedup();
    next.report = report;
    Ok(())
  }

  /// Installs a validated, unstarted candidate from `revision`. Compatible
  /// instances and mesh values remain; changed/removed instances are cancelled.
  /// Unchanged bodies keep their identity, so call sites see no new body.
  pub fn install_revision(&mut self, mut next: Self, removed: &HashSet<InstanceId>) {
    self
      .validate_reload(&mut next, removed)
      .expect("revision validated before commit");
    assert!(next.instances.is_empty(), "revision already has instances");
    for instance in &mut self.instances {
      if instance.outcome.is_none()
        && (removed.contains(&instance.id) || !next.can_retain(&instance.wire))
      {
        finish(instance, Outcome::Cancelled);
      }
    }
    let mut functions = std::mem::take(&mut next.prepared_functions);
    crate::reload::reuse_unchanged(&self.prepared_functions, &mut functions);
    crate::reload::reuse_unchanged(&self.revisions.functions, &mut functions);
    self.prepared_functions = functions.clone();
    self.table = Arc::new(functions.clone());
    self.revisions.install(functions);
    self.report = std::mem::take(&mut next.report);
    self.wires = std::mem::take(&mut next.wires);
    self.functions = std::mem::take(&mut next.functions);
    self.cache = std::mem::take(&mut next.cache);
    // A retained instance whose body inlined a changed function takes the
    // candidate's body (compiled by `validate_reload`): one not started yet
    // or between iterations now, a looped one in the middle of an
    // iteration at its end, with its state (golden path §11: an in-flight run finishes on its body). A
    // started run of a wire that does not loop finishes on the old body.
    for i in 0..self.instances.len() {
      let instance = &mut self.instances[i];
      if instance.outcome.is_some() || removed.contains(&instance.id) {
        continue;
      }
      // A body accepted by an earlier reload and not adopted yet is
      // superseded by this one (or by the old body, if it is valid again).
      instance.next_body = None;
      let instance = &self.instances[i];
      if self.retention(&instance.wire) != crate::reload::Retention::Swap
        || (instance.started && !instance.wire.looped)
      {
        continue;
      }
      let (name, input) = (instance.wire.name.clone(), instance.wire.input);
      let wire = self
        .compile(&name, input)
        .expect("swapped body compiled during validation");
      let instance = &mut self.instances[i];
      if !instance.started {
        instance.locals = carry_keeps(instance, &wire);
        instance.wire = wire;
      } else if instance
        .state
        .as_ref()
        .is_some_and(Engine::between_iterations)
      {
        if let Err(err) = rebase(instance, wire) {
          finish(instance, Outcome::Failed(err));
        }
      } else {
        instance.next_body = Some(wire);
      }
      self.report.swapped.push(name);
    }
  }

  /// How many times call sites consulted the reload registry. A call site
  /// checks it once per accepted preserving reload, not on every call.
  pub fn reload_lookups(&self) -> u64 {
    self.revisions.lookups.get()
  }

  /// Composite entries and completion handlers dispatched by live instances.
  /// Pending leaf re-polls must leave this unchanged, regardless of nesting.
  pub fn composite_dispatches(&self) -> u64 {
    self
      .instances
      .iter()
      .filter_map(|i| i.state.as_ref())
      .map(|s| s.dispatches)
      .sum()
  }

  pub fn cache_stats(&self) -> CacheStats {
    self.cache.stats
  }

  pub fn spawn(&mut self, wire: &Arc<CompiledWire>, input: Var) -> Result<InstanceId> {
    let env = ComposeEnv {
      mesh_layout: &self.layout,
      wires: &self.wires,
      functions: &self.functions,
      eval: self.eval_limits,
    };
    if !wire.deps_valid(&env) {
      return Err(Error::Compose(format!(
        "wire {} was compiled for a different mesh layout",
        wire.name
      )));
    }
    // Checked on the value: a union or open-table input accepts any value it
    // admits, and nothing is interned.
    if !wire.input.admits(&input) {
      return Err(Error::Compose(format!(
        "wire {} expects {} input, got {}",
        wire.name,
        wire.input,
        input.type_of()
      )));
    }
    Ok(self.start(wire.clone(), input))
  }

  fn start(&mut self, wire: Arc<CompiledWire>, input: Var) -> InstanceId {
    let id = self.next_id;
    self.next_id += 1;
    let wake = Arc::new(WakeFlag::default());
    self.instances.push(Instance {
      waiting: false,
      waker: Waker::from(wake.clone()),
      wake,
      iteration: 0,
      id,
      locals: wire.fresh_locals(),
      wire,
      input,
      state: None,
      started: false,
      memory: InstanceMemory::default(),
      outcome: None,
      next_body: None,
    });
    id
  }

  /// Runs one tick: activates every running instance once, in schedule order,
  /// then starts instances spawned during the tick. Returns the number of
  /// instances still running.
  pub fn tick(&mut self) -> usize {
    let notify_only = self.wake_mode == WakeMode::OnNotify;
    let Mesh {
      instances,
      frame,
      spawn_queue,
      revisions,
      table,
      max_call_depth,
      ..
    } = self;
    for instance in instances.iter_mut() {
      if instance.outcome.is_some() {
        continue;
      }
      let woken = instance.wake.take();
      if notify_only && instance.waiting && !woken {
        continue;
      }
      instance.waiting = false;
      step(
        instance,
        frame,
        spawn_queue,
        revisions,
        table,
        *max_call_depth,
      );
    }
    for (wire, input) in std::mem::take(&mut self.spawn_queue) {
      self.start(wire, input);
    }
    self.running()
  }

  /// Cancels a running instance: its state is cleaned up exactly once, and it
  /// is never activated again. Nothing needs unwinding: a suspended stackless
  /// instance is only data.
  pub fn cancel(&mut self, id: InstanceId) {
    if let Some(instance) = self
      .instances
      .iter_mut()
      .find(|i| i.id == id && i.outcome.is_none())
    {
      finish(instance, Outcome::Cancelled);
    }
  }

  /// Cancels all running instances, including spawned children, in one pass.
  /// Outcomes (including cleanup failures) remain available to `take_finished`.
  pub fn cancel_all(&mut self) {
    for instance in &mut self.instances {
      if instance.outcome.is_none() {
        finish(instance, Outcome::Cancelled);
      }
    }
  }

  /// Returns the outcome of a finished instance and retires its record. A
  /// long-running mesh should take outcomes it no longer needs, so records
  /// of finished instances do not accumulate. Returns `None` for an unknown
  /// or still running instance.
  pub fn take_outcome(&mut self, id: InstanceId) -> Option<Outcome> {
    let index = self
      .instances
      .iter()
      .position(|i| i.id == id && i.outcome.is_some())?;
    self.instances.remove(index).outcome
  }

  pub fn outcome(&self, id: InstanceId) -> Option<&Outcome> {
    self
      .instances
      .iter()
      .find(|i| i.id == id)
      .and_then(|i| i.outcome.as_ref())
  }

  /// An instance's local frame, for tests of iteration freshness; not a
  /// stable API.
  #[doc(hidden)]
  pub fn instance_locals(&self, id: InstanceId) -> Option<&[Var]> {
    self
      .instances
      .iter()
      .find(|i| i.id == id)
      .map(|i| i.locals.as_slice())
  }

  pub fn memory(&self, id: InstanceId) -> Option<InstanceMemory> {
    self.instances.iter().find(|i| i.id == id).map(|i| i.memory)
  }

  /// Removes the records of every finished instance in one pass, returning
  /// each one's id, wire name and outcome. The cost is linear in the
  /// records; a host that needs some outcomes later keeps them itself.
  pub fn take_finished(&mut self) -> Vec<(InstanceId, String, Outcome)> {
    let mut finished = Vec::new();
    // Compacts in place, keeping the order of the records left.
    let mut kept = 0;
    for at in 0..self.instances.len() {
      match self.instances[at].outcome.take() {
        Some(outcome) => {
          let i = &self.instances[at];
          finished.push((i.id, i.wire.name.clone(), outcome));
        }
        None => {
          self.instances.swap(kept, at);
          kept += 1;
        }
      }
    }
    self.instances.truncate(kept);
    finished
  }

  /// The name of the wire an instance runs, while its record exists.
  pub fn instance_wire(&self, id: InstanceId) -> Option<&str> {
    self
      .instances
      .iter()
      .find(|i| i.id == id)
      .map(|i| i.wire.name.as_str())
  }

  pub fn instance_ids(&self) -> Vec<InstanceId> {
    self.instances.iter().map(|i| i.id).collect()
  }

  pub fn running(&self) -> usize {
    self
      .instances
      .iter()
      .filter(|i| i.outcome.is_none())
      .count()
  }

  pub fn run(&mut self, max_ticks: usize) -> usize {
    let mut ticks = 0;
    while ticks < max_ticks && self.tick() > 0 {
      ticks += 1;
    }
    ticks
  }
}

impl Drop for Mesh {
  fn drop(&mut self) {
    self.cancel_all();
  }
}

/// One activation of one instance.
fn step(
  instance: &mut Instance,
  frame: &mut Vec<Var>,
  spawn_queue: &mut Vec<(Arc<CompiledWire>, Var)>,
  revisions: &Revisions,
  table: &Arc<FunctionRegistry>,
  max_call_depth: usize,
) {
  if !instance.started {
    instance.started = true;
    let mut ictx = InstanceCtx {
      instance: instance.id,
    };
    let wire = &instance.wire;
    // Flow instantiation already turns panics into errors; this is a last
    // line of defense so a panic never escapes `tick`.
    let instantiated = match catch_unwind(AssertUnwindSafe(|| {
      Engine::instantiate(wire.clone(), &mut ictx)
    })) {
      Ok(result) => result,
      Err(payload) => Err(Error::Activation(format!(
        "panic in instantiate: {}",
        panic_message(&*payload)
      ))),
    };
    match instantiated {
      Ok(state) => {
        instance.memory = InstanceMemory {
          state_bytes: state.state_size()
            + instance.locals.len() * size_of::<Var>()
            + size_of::<Vec<Var>>(),
        };
        instance.state = Some(state);
      }
      Err(err) => {
        finish(instance, Outcome::Failed(err));
        return;
      }
    }
  }

  // The instance is the panic boundary.
  let result = {
    let Instance {
      id,
      input,
      locals,
      state,
      waiting,
      waker,
      iteration,
      ..
    } = instance;
    let state = state.as_mut().expect("running instance without state");
    catch_unwind(AssertUnwindSafe(|| {
      let mut ctx = ActivationCtx {
        instance: *id,
        locals: locals.as_mut_slice(),
        revisions,
        table,
        mesh_frame: frame,
        spawn_queue,
        waiting,
        waker,
        iteration: *iteration,
        max_call_depth,
        meter: None,
      };
      state.activate(&mut ctx, input)
    }))
  };

  let outcome = match result {
    Ok(Ok(Step::Suspend)) => return,
    Ok(Ok(Step::Next(value) | Step::Return(value))) if !instance.wire.looped => {
      Outcome::Completed(value)
    }
    // A loop iteration, a Return at the root or a Restart keeps the
    // instance and its state; the next iteration starts with fresh locals
    // except the `Keep` slots (golden path §3.4).
    Ok(Ok(Step::Next(_) | Step::Return(_) | Step::Restart)) => {
      instance.iteration += 1;
      for (slot, value) in instance.locals.iter_mut().enumerate() {
        if !instance.wire.persistent(slot) {
          *value = Var::None;
        }
      }
      if instance.next_body.is_some() {
        let wire = instance.next_body.take().expect("checked");
        if let Err(err) = rebase(instance, wire) {
          finish(instance, Outcome::Failed(err));
        }
      }
      return;
    }
    Ok(Ok(Step::Stop)) => Outcome::Stopped,
    Ok(Err(err)) => Outcome::Failed(err),
    Err(payload) => Outcome::Failed(Error::Activation(format!(
      "panic in activation: {}",
      panic_message(&*payload)
    ))),
  };
  finish(instance, outcome);
}

/// Moves a looped instance onto a new body of its wire between two
/// iterations (`Engine::rebase`): its frames keep their state, its locals
/// are laid out for the new body with `Keep` slots carried over.
fn rebase(instance: &mut Instance, wire: Arc<CompiledWire>) -> Result<()> {
  let mut ictx = InstanceCtx {
    instance: instance.id,
  };
  let state = instance.state.as_mut().expect("started instance");
  state.rebase(wire.clone(), &mut ictx)?;
  instance.memory = InstanceMemory {
    state_bytes: state.state_size() + wire.locals.len() * size_of::<Var>() + size_of::<Vec<Var>>(),
  };
  instance.locals = carry_keeps(instance, &wire);
  instance.wire = wire;
  Ok(())
}

/// The locals of `wire`'s body for an instance moving onto it: fresh, with
/// the instance's `Keep` slots carried over by name and type.
fn carry_keeps(instance: &Instance, wire: &CompiledWire) -> Vec<Var> {
  let mut locals = wire.fresh_locals();
  for keep in &wire.keeps {
    if let Some(old) = instance
      .wire
      .keeps
      .iter()
      .find(|k| k.name == keep.name && k.ty == keep.ty)
      && let Some(value) = instance.locals.get(old.slot)
    {
      locals[keep.slot] = value.clone();
    }
  }
  locals
}

/// Ends an instance: cleans up its state exactly once (if it was
/// instantiated) and records the outcome.
fn finish(instance: &mut Instance, mut outcome: Outcome) {
  if let Some(mut state) = instance.state.take() {
    let mut ctx = InstanceCtx {
      instance: instance.id,
    }
    .cleanup_ctx();
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| state.cleanup(&mut ctx))) {
      outcome = Outcome::Failed(Error::Activation(format!(
        "panic in cleanup: {}",
        panic_message(&*payload)
      )));
    }
  }
  // Release execution data on every terminal path: only the outcome stays
  // (until `take_outcome`).
  instance.locals = Vec::new();
  instance.input = Var::None;
  instance.next_body = None;
  instance.outcome = Some(outcome);
}
