//! Compose-time evaluation (`#( ... )`, docs/metaprogramming.md §2): an
//! argument holding [`ParamValue::Eval`] is composed with no input, checked
//! to be eligible, run on the engine in a temporary instance with no mesh
//! under a [`Meter`], and replaced by its value before the shard decodes its
//! arguments. Results are cached by the pipeline and what it read.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;
use crate::compose_time::{EvalLimits, EvalUsage, Meter, budget, text_size};
use crate::describe::{Params, Requirement};
use crate::instance::{CleanupCtx, InstanceCtx};
use crate::shard::{ActivationCtx, Step};
use crate::stackless::Engine;

impl ComposeCtx<'_> {
  /// `def` with every argument evaluated at compose time replaced by its
  /// value. An error inside an evaluation is located at it: the parameter
  /// holding it, then [`PathStep::Evaluation`], then the path inside.
  pub(super) fn evaluate_args(&mut self, def: &ShardDef) -> Result<ShardDef> {
    let mut args = Vec::with_capacity(def.args.len());
    for (position, arg) in def.args.iter().enumerate() {
      let ParamValue::Eval(flow) = &arg.value else {
        args.push(arg.clone());
        continue;
      };
      let value = self.evaluate(flow).map_err(|err| {
        let err = err.prefix_path(PathStep::Evaluation);
        match self.param_step(def, position) {
          Some((param, Some(item))) => err
            .prefix_path(PathStep::Item(item))
            .prefix_path(PathStep::Param(param)),
          Some((param, None)) => err.prefix_path(PathStep::Param(param)),
          None => err,
        }
      })?;
      args.push(crate::args::Arg {
        name: arg.name.clone(),
        value: ParamValue::Value(value),
      });
    }
    Ok(ShardDef {
      ty: def.ty,
      args,
      function: def.function.clone(),
    })
  }

  /// The parameter an argument of `def` fills, as occurrence paths name it
  /// (with the argument's index among a variadic parameter's), when known.
  fn param_step(&self, def: &ShardDef, position: usize) -> Option<(String, Option<usize>)> {
    let arg = &def.args[position];
    if let Some(name) = &arg.name {
      return Some((name.clone(), None));
    }
    if let Some(function) = &def.function {
      let fdef = self.env.functions.get(&**function)?;
      return fdef.params.get(position).map(|p| (p.name.clone(), None));
    }
    let Params::Declared(decls) = def.ty.desc.params else {
      return None;
    };
    let variadic = decls
      .iter()
      .position(|d| d.requirement == Requirement::Variadic);
    match variadic {
      Some(v) if position >= v => Some((decls[v].name.to_string(), Some(position - v))),
      _ => decls.get(position).map(|d| (d.name.to_string(), None)),
    }
  }

  /// The value of `flow`, evaluated at compose time, from the cache when a
  /// result is valid here and was computed within this context's limits.
  /// What the evaluation read becomes a dependency of this code (functions
  /// as `Dep::Inlined`: the result is part of this body), and what it used
  /// a `Dep::Evaluation`.
  pub(super) fn evaluate(&mut self, flow: &[ShardDef]) -> Result<Var> {
    let limits = self.env.eval;
    let key = {
      let mut hasher = DefaultHasher::new();
      flow.hash(&mut hasher);
      hasher.finish()
    };
    let cached = self.cache.evaluations.get(&key).and_then(|candidates| {
      candidates
        .iter()
        .find(|e| e.flow == flow && e.deps.iter().all(|d| d.still_valid(self.env)))
        .map(|e| (e.value.clone(), e.deps.clone(), e.usage))
    });
    if let Some((value, deps, usage)) = cached {
      if !usage.fits(&limits) {
        return Err(budget(format!(
          "a cached result of this evaluation used more than this compose allows ({usage:?} against {limits:?})"
        )));
      }
      self.cache.stats.evaluation_hits += 1;
      self.record_evaluation(&deps, usage);
      return Ok(value);
    }
    // Recursive-group bookkeeping belongs to the compose that started it;
    // the evaluation composes its functions on its own.
    self.cache.eval_floors.push(self.composing.len());
    let refs = std::mem::take(&mut self.cache.recursive_refs);
    let group = self.cache.group.take();
    let result = self.compose_and_run(flow, limits);
    self.cache.recursive_refs = refs;
    self.cache.group = group;
    self.cache.eval_floors.pop();
    let (value, deps, usage) = result?;
    self.cache.stats.evaluations += 1;
    self.record_evaluation(&deps, usage);
    self
      .cache
      .evaluations
      .entry(key)
      .or_default()
      .push(EvalEntry {
        flow: flow.to_vec(),
        value: value.clone(),
        deps,
        usage,
      });
    Ok(value)
  }

  fn record_evaluation(&mut self, deps: &[Dep], usage: EvalUsage) {
    for dep in deps {
      match dep {
        Dep::Function { name, def } | Dep::Inlined { name, def } => {
          let dep = Dep::Inlined {
            name: name.clone(),
            def: def.clone(),
          };
          self.deps.push(dep.clone());
          self.restart_deps.push(dep);
        }
        other => self.deps.push(other.clone()),
      }
    }
    self.deps.push(Dep::Evaluation(usage));
  }

  /// Composes `flow` as its own pipeline (input `None`, nothing visible
  /// but what it declares), checks it is eligible, and runs it.
  fn compose_and_run(
    &mut self,
    flow: &[ShardDef],
    limits: EvalLimits,
  ) -> Result<(Var, Vec<Dep>, EvalUsage)> {
    let (wire, deps) = pipeline(self.env, self.cache, self.composing, self.depth + 1, flow)?;
    let output = wire.flow.output;
    let meter = Meter::new(limits);
    let value = match run(wire, &meter) {
      Err(Error::Diagnostic(mut d))
        if d.code == "compose-time-error" && !self.cache.record_origins =>
      {
        locate(&mut d, self.locate(flow, limits));
        return Err(Error::Diagnostic(d));
      }
      result => result?,
    };
    let Some(bytes) = text_size(&value, limits.output_bytes) else {
      return Err(budget(format!(
        "the result is larger than the output limit of {} bytes",
        limits.output_bytes
      )));
    };
    if !output.admits(&value) {
      return Err(failure(format!(
        "the result {} is not the pipeline's output type {output}",
        value.type_of()
      )));
    }
    Ok((value, deps, meter.usage(bytes)))
  }

  /// Where a failed evaluation of `flow` failed: composed again in a fresh
  /// cache whose flows record their origins, and run again recording each
  /// level the failure passes (both cold: a failure only). An evaluation
  /// is deterministic, so it fails at the same shard; if it somehow does
  /// not, the path is empty and the failure stays located at the `#( )`.
  #[cold]
  fn locate(&self, flow: &[ShardDef], limits: EvalLimits) -> Vec<PathStep> {
    let mut cache = ComposeCache::locating();
    let mut composing = Vec::new();
    let Ok((wire, _)) = pipeline(self.env, &mut cache, &mut composing, self.depth + 1, flow) else {
      return Vec::new();
    };
    let meter = Meter::locating(limits);
    match run(wire, &meter) {
      Err(_) => crate::stackless::failure_path(&meter),
      Ok(_) => Vec::new(),
    }
  }
}

/// `flow` composed as its own pipeline through `cache`, checked to be
/// eligible: the wire to run, and what composing it read.
fn pipeline(
  env: &ComposeEnv<'_>,
  cache: &mut ComposeCache,
  composing: &mut Vec<String>,
  depth: usize,
  flow: &[ShardDef],
) -> Result<(Arc<CompiledWire>, Vec<Dep>)> {
  // Boxed like every compose context: compose recurses per level.
  let mut ctx = Box::new(ComposeCtx {
    analysis: Analysis::default(),
    input: Type::none(),
    locals: FrameLayout::default(),
    env,
    cache,
    composing,
    deps: Vec::new(),
    restart_deps: Vec::new(),
    functions: HashMap::new(),
    diagnostic_path: Vec::new(),
    local_paths: Vec::new(),
    current_args: None,
    initialized: Vec::new(),
    failed_child: None,
    depth,
    blocks: 0,
    owner: Owner::Eval,
    keeps: Vec::new(),
    lazy_refs: Vec::new(),
    inline_depth: 0,
    flow_args: 0,
    block_param: None,
  });
  let compiled = ctx.compose_flow_unscoped(flow, Type::none())?;
  if let Some(err) = not_compose_time(&compiled.analysis) {
    return Err(err);
  }
  let ComposeCtx {
    locals,
    deps,
    functions,
    keeps,
    ..
  } = *ctx;
  let wire = Arc::new(CompiledWire {
    name: "#()".into(),
    looped: false,
    input: Type::none(),
    flow: compiled,
    locals,
    deps: Vec::new(),
    definition: Arc::new(WireDef {
      name: "#()".into(),
      looped: false,
      flow: Vec::new(),
    }),
    restart_deps: Vec::new(),
    functions,
    keeps,
  });
  Ok((wire, deps))
}

/// Locates a `compose-time-error` at the shard `path` leads to, inside the
/// pipeline (`PathStep::Evaluation` and the site are prefixed as it
/// propagates).
fn locate(d: &mut Diagnostic, path: Vec<PathStep>) {
  if let Some(PathStep::Shard { name, .. }) = path
    .iter()
    .rev()
    .find(|s| matches!(s, PathStep::Shard { .. }))
  {
    d.shard = Some(name.clone());
  }
  d.path = path;
}

/// Runs the composed pipeline to its end on the engine: one temporary
/// instance with no mesh, metered, cleaned up whatever happens.
fn run(wire: Arc<CompiledWire>, meter: &Meter) -> Result<Var> {
  let instance = crate::InstanceId::MAX;
  let mut ictx = InstanceCtx { instance };
  let mut locals = wire.fresh_locals();
  let table = Arc::new(wire.functions.clone());
  let mut engine = Engine::instantiate(wire, &mut ictx).map_err(|e| failure(e.to_string()))?;
  let revisions = crate::reload::Revisions::default();
  let (mut mesh_frame, mut spawn_queue, mut waiting) = (Vec::new(), Vec::new(), false);
  let result = catch_unwind(AssertUnwindSafe(|| {
    let mut ctx = ActivationCtx {
      instance,
      locals: &mut locals,
      revisions: &revisions,
      table: &table,
      mesh_frame: &mut mesh_frame,
      spawn_queue: &mut spawn_queue,
      waiting: &mut waiting,
      waker: std::task::Waker::noop(),
      iteration: 0,
      max_call_depth: meter.limits().depth,
      meter: Some(meter),
    };
    engine.activate(&mut ctx, &Var::None)
  }));

  let cleanup = catch_unwind(AssertUnwindSafe(|| {
    engine.cleanup(&mut CleanupCtx { instance });
  }));
  let result = match result {
    Ok(result) => result,
    Err(payload) => {
      return Err(failure(format!(
        "a shard panicked: {}",
        crate::error::panic_message(&*payload)
      )));
    }
  };
  if let Err(payload) = cleanup {
    return Err(failure(format!(
      "a cleanup panicked: {}",
      crate::error::panic_message(&*payload)
    )));
  }
  match result {
    Ok(Step::Next(value)) => Ok(value),
    // `Return` and `Stop` are refused at compose, and nothing eligible
    // suspends or restarts.
    Ok(other) => Err(failure(format!("the pipeline ended with {other:?}"))),
    Err(Error::Diagnostic(d)) if d.code == "expansion-budget" => Err(Error::Diagnostic(d)),
    Err(Error::Diagnostic(d)) if d.code == "recursion-limit" => Err(budget(format!(
      "calls nest deeper than the depth limit of {} ({})",
      meter.limits().depth,
      d.message
    ))),
    Err(err) => Err(failure(match err {
      Error::Activation(message) | Error::Compose(message) => message,
      Error::Diagnostic(d) => d.to_string(),
      Error::Cancelled => "cancelled".into(),
    })),
  }
}

/// A runtime failure inside the evaluation (`compose-time-error`).
#[cold]
fn failure(message: String) -> Error {
  Error::Diagnostic(Box::new(compose_diagnostic(
    "compose-time-error",
    format!("the evaluation failed: {message}"),
  )))
}

/// Why a composed pipeline may not run at compose time, located at the
/// first occurrence responsible: an effect, persistent state, mesh access,
/// or a native shard not on the compose-time list.
fn not_compose_time(analysis: &Analysis) -> Option<Error> {
  type Pick = fn(Effects) -> bool;
  let effects: [(&str, Pick); 5] = [
    ("suspends", |e| e.suspends),
    ("io", |e| e.io),
    ("time", |e| e.time),
    ("random", |e| e.random),
    ("unknown", |e| e.unknown),
  ];
  // The innermost occurrence responsible: a composite or a call carries
  // what its children do, so the first one found is followed down to the
  // shard inside it that does it.
  let first = |pick: &dyn Fn(&Occurrence) -> bool| {
    let found: Vec<Occurrence> = analysis.occurrences.iter().filter(|o| pick(o)).collect();
    let inner = |o: &Occurrence| {
      !found
        .iter()
        .any(|d| d.path.len() > o.path.len() && d.path.starts_with(&o.path))
    };
    found.iter().find(|o| inner(o)).cloned()
  };
  let (reason, path) =
    if let Some((label, pick)) = effects.iter().find(|(_, pick)| pick(analysis.effects)) {
      (
        format!("it has the effect `{label}`"),
        first(&|o| pick(o.effects)).map(|o| o.path),
      )
    } else if analysis.lifetime != Lifetime::Stateless {
      (
        "it holds persistent state".to_string(),
        first(&|o| o.lifetime != Lifetime::Stateless).map(|o| o.path),
      )
    } else if let Some(access) = analysis.uses.first().or(analysis.mutates.first()) {
      (
        format!("it reaches mesh variable {}", access.name),
        analysis.mesh_at.clone(),
      )
    } else if let Some(name) = &analysis.not_compose_time {
      (
        "it is not on the list of shards compose-time evaluation may run".to_string(),
        first(&|o| matches!(o.path.last(), Some(PathStep::Shard { name: n, .. }) if **n == **name))
          .map(|o| o.path),
      )
    } else {
      return None;
    };
  let path = path.unwrap_or_default();
  let shard = match path.last() {
    Some(PathStep::Shard { name, .. }) => name.clone(),
    _ => "the pipeline".to_string(),
  };
  let through: Vec<&str> = path
    .iter()
    .filter_map(|s| match s {
      PathStep::Function(name) => Some(name.as_str()),
      _ => None,
    })
    .collect();
  let through = if through.is_empty() {
    String::new()
  } else {
    format!(" (reached through {})", through.join(", "))
  };
  let mut d = compose_diagnostic(
    "not-compose-time",
    format!("`#( )` cannot run {shard} at compose time{through}: {reason}"),
  )
  .shard(&shard);
  d.path = path;
  Some(Error::Diagnostic(Box::new(d)))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::shards::defs::*;

  #[test]
  fn a_failure_handled_on_the_way_leaves_no_trace_in_the_location() {
    // `Maybe` handles a failure in a child run inside its own step (leaf
    // code), then one in a child frame (a shard without a VM form); the
    // failure that ends the run is located at its own shard.
    let seq = || konst(Var::from_seq(Arc::new(vec![Var::Int(1), Var::Int(2)])));
    let flow = vec![
      maybe(
        vec![seq(), take(val(Var::Int(5)))],
        Some(vec![konst(Var::Int(0))]),
      ),
      maybe(
        vec![log(), seq(), take(val(Var::Int(5)))],
        Some(vec![konst(Var::Int(0))]),
      ),
      seq(),
      take(val(Var::Int(9))),
    ];
    // Composed as a failed evaluation is located: its flows record origins.
    let mut mesh = crate::Mesh::with_cache(ComposeCache::locating());
    mesh.add_wire(WireDef {
      name: "w".into(),
      looped: false,
      flow,
    });
    let wire = mesh.compile("w", Type::none()).expect("composes");
    let meter = Meter::locating(EvalLimits::default());
    let err = run(wire, &meter).expect_err("fails");
    assert_eq!(
      err.diagnostic().expect("a diagnostic").code,
      "compose-time-error"
    );
    assert_eq!(
      crate::stackless::failure_path(&meter),
      [PathStep::Shard {
        index: 3,
        name: "Take".into()
      }]
    );
  }
}
