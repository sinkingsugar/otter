//! Host-driven execution with transactional source replacement.

use otter_core::hash::HashSet;
use std::sync::Arc;
use std::time::Duration;

use otter_core::diagnostic::{Diagnostic, PathStep};
use otter_core::{
  Catalog, CompiledWire, InstanceId, Mesh, Outcome, ReloadReport, ResetPolicy, Type, Var,
};

use crate::files::{Files, FsFiles};
use crate::{Program, Source};

/// An entry or spawned instance that finished, failed or was cancelled.
/// Returned once; a session does not accumulate a history of outcomes.
#[derive(Debug)]
pub struct Finished {
  pub wire: String,
  pub outcome: Outcome,
}

struct Entry {
  name: String,
  wire: Arc<CompiledWire>,
  id: InstanceId,
}

struct Execution {
  mesh: Mesh,
  entries: Vec<Entry>,
  failed: HashSet<InstanceId>,
  ticks: u64,
  iterations: Option<u64>,
  frame_interval: Option<Duration>,
}

/// A reloadable program on a host-owned event loop.
///
/// `reload` validates all wires and explicitly restarts the whole mesh.
/// `reload_preserving` retains compatible callers, locals and mesh values,
/// and call sites select edited function bodies at their next entry.
/// Rejected edits leave execution untouched. Host services live outside this
/// object and survive either mode. Each revision owns its compose cache.
///
/// Calls are synchronous and require exclusive access: reload between ticks.
/// Compilation can delay the host loop. Cancellation drops pending futures;
/// external work still requires cooperative cancellation by the host shard.
pub struct Session {
  active: Option<Execution>,
  reset_policy: ResetPolicy,
  /// Where `@include` and `@read` read from (the filesystem by default).
  files: Box<dyn Files>,
  /// The files the last reload read, accepted or not.
  read: Vec<String>,
  /// Every name the last reload looked at, found or not, with what it
  /// found there (`Session::dependencies`).
  dependencies: Vec<(String, Fingerprint)>,
}

/// What a file held when it was read: its length and a hash of its exact
/// bytes, `None` when it could not be read (missing). Compared, never
/// decoded, so any edit to a binary file is a change.
pub(crate) type Fingerprint = Option<(usize, u64)>;

pub(crate) fn fingerprint(bytes: Option<&[u8]>) -> Fingerprint {
  use std::hash::{Hash, Hasher};
  bytes.map(|bytes| {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    (bytes.len(), hasher.finish())
  })
}

impl Default for Session {
  fn default() -> Self {
    Self::new()
  }
}

impl Session {
  pub fn new() -> Self {
    Self {
      active: None,
      reset_policy: ResetPolicy::default(),
      files: Box::new(FsFiles::default()),
      read: Vec::new(),
      dependencies: Vec::new(),
    }
  }

  /// Where `@include` and `@read` find files: a filesystem with include
  /// paths, or files held in memory ([`crate::MemoryFiles`]).
  pub fn set_files(&mut self, files: impl Files + 'static) {
    self.files = Box::new(files);
  }

  /// The files the last reload included or read, by name, whether or not
  /// it was accepted: an edit to any of them is a new revision.
  pub fn files_read(&self) -> &[String] {
    &self.read
  }

  /// Every name the last reload looked at (a missing include, a file
  /// searched before the one found) with what the load found there: a
  /// watcher compares these with `observe_dependencies`.
  pub(crate) fn dependencies(&self) -> &[(String, Fingerprint)] {
    &self.dependencies
  }

  /// The same names, as they are now, through the session's files.
  pub(crate) fn observe_dependencies(&self) -> Vec<(String, Fingerprint)> {
    self
      .dependencies
      .iter()
      .map(|(name, _)| {
        (
          name.clone(),
          fingerprint(self.files.contents(name).as_deref()),
        )
      })
      .collect()
  }

  /// Loads a program through the session's files, recording what it read.
  fn load(
    &mut self,
    source: Source,
    catalog: &Catalog,
    defines: &std::collections::HashMap<String, String>,
  ) -> Result<Program, (Source, Vec<Diagnostic>)> {
    let recording = Recording {
      inner: &*self.files,
      read: std::cell::RefCell::new(Vec::new()),
      dependencies: std::cell::RefCell::new(Vec::new()),
    };
    let program = Program::load_with(source, catalog, defines, &recording);
    self.read = recording.read.into_inner();
    self.dependencies = recording.dependencies.into_inner();
    program
  }

  /// What a preserving reload does when a stateful function's state would
  /// reset (golden path §11): reject the edit (the embedding default) or
  /// apply the resets and report them (what `otter watch` does).
  pub fn set_reset_policy(&mut self, policy: ResetPolicy) {
    self.reset_policy = policy;
    if let Some(active) = &mut self.active {
      active.mesh.set_reset_policy(policy);
    }
  }

  /// What the last accepted preserving reload retained, reset and
  /// restarted, by name; `None` without an active revision.
  pub fn reload_report(&self) -> Option<&ReloadReport> {
    self.active.as_ref().map(|a| a.mesh.reload_report())
  }

  /// Starts with a host-configured, idle mesh. Use `reload_preserving` to
  /// keep its declared mesh variables across source revisions. The mesh's
  /// reset policy is the session's.
  pub fn with_mesh(mesh: Mesh) -> Self {
    assert_eq!(mesh.running(), 0, "Session requires an idle mesh");
    Self {
      reset_policy: mesh.reset_policy(),
      active: Some(Execution {
        mesh,
        entries: Vec::new(),
        failed: HashSet::default(),
        ticks: 0,
        iterations: None,
        frame_interval: None,
      }),
      files: Box::new(FsFiles::default()),
      read: Vec::new(),
      dependencies: Vec::new(),
    }
  }

  /// Parses, lowers, composes every wire and schedules the entries on a fresh
  /// mesh. No candidate shard runs until a subsequent `tick`.
  ///
  /// On success returns the old execution's cancellation outcomes, including
  /// cleanup failures. Those failures do not roll back the replacement.
  /// Load/compose/scheduling errors return the rejected source and diagnostics
  /// and preserve the old execution. Runtime failures after commit are tick
  /// outcomes, not reload errors.
  pub fn reload(
    &mut self,
    source: Source,
    catalog: &Catalog,
    defines: &std::collections::HashMap<String, String>,
  ) -> Result<Vec<Finished>, (Source, Vec<Diagnostic>)> {
    let program = self.load(source, catalog, defines)?;
    let mut mesh = program.mesh();
    mesh.set_reset_policy(self.reset_policy);
    let entries = match compose_entries(&program, &mut mesh) {
      Ok(entries) => entries,
      Err(diagnostics) => return Err((program.source, diagnostics)),
    };
    let mut scheduled = Vec::new();
    for (name, wire) in entries {
      match mesh.spawn(&wire, Var::None) {
        Ok(id) => scheduled.push(Entry { name, wire, id }),
        Err(err) => {
          let diagnostic = program.diagnostic(PathStep::Wire(name), err);
          return Err((program.source, vec![diagnostic]));
        }
      }
    }
    let run = program.lowered.run.as_ref();
    let next = Execution {
      mesh,
      entries: scheduled,
      failed: HashSet::default(),
      ticks: 0,
      iterations: run.and_then(|r| r.iterations).map(|n| n as u64),
      frame_interval: run
        .and_then(|r| r.fps)
        .and_then(|fps| Duration::try_from_secs_f64(1.0 / fps).ok()),
    };
    let finished = self.stop();
    self.active = Some(next);
    Ok(finished)
  }

  /// Runs one scheduler tick without sleeping, and retires all finished
  /// records. At `@run`'s iteration limit, cancels remaining instances and
  /// reports their outcomes in this same call. Idle calls are no-ops.
  pub fn tick(&mut self) -> Vec<Finished> {
    let Some(active) = &mut self.active else {
      return Vec::new();
    };
    if active.mesh.running() == 0 {
      return Vec::new();
    }
    active.mesh.tick();
    active.ticks = active.ticks.saturating_add(1);
    if active.iterations.is_some_and(|n| active.ticks >= n) {
      active.mesh.cancel_all();
    }
    let entries: HashSet<_> = active.entries.iter().map(|e| e.id).collect();
    active
      .mesh
      .take_finished()
      .into_iter()
      .map(|(id, wire, outcome)| {
        if entries.contains(&id) && matches!(outcome, Outcome::Failed(_)) {
          active.failed.insert(id);
        }
        Finished { wire, outcome }
      })
      .collect()
  }

  /// Cancels all entries and children, reports their outcomes and releases
  /// the revision's mesh and compose cache. Safe to call repeatedly.
  /// Dropping the session also cancels work, but discards the outcomes.
  pub fn stop(&mut self) -> Vec<Finished> {
    let Some(mut active) = self.active.take() else {
      return Vec::new();
    };
    active.mesh.cancel_all();
    drain(&mut active.mesh)
  }

  pub fn running(&self) -> usize {
    self.active.as_ref().map_or(0, |a| a.mesh.running())
  }

  /// Ticks in the current revision; resets on a successful reload or stop.
  pub fn ticks(&self) -> u64 {
    self.active.as_ref().map_or(0, |a| a.ticks)
  }

  /// Suggested pacing from `@run(fps:)`. The host chooses when to call tick;
  /// `None` means no rate was requested. Reload can change this value.
  pub fn frame_interval(&self) -> Option<Duration> {
    self.active.as_ref().and_then(|a| a.frame_interval)
  }
}

fn drain(mesh: &mut Mesh) -> Vec<Finished> {
  mesh
    .take_finished()
    .into_iter()
    .map(|(_, wire, outcome)| Finished { wire, outcome })
    .collect()
}

impl Session {
  /// Retains unchanged callers and their locals, Once state and suspended
  /// execution. Edited function bodies take effect at their next entry; an
  /// invocation already in flight finishes on its body. Compatible mesh
  /// variables remain in the same frame. Changes to a root's own definition
  /// or static dependencies restart that root; removed roots are cancelled.
  ///
  /// An incompatible function interface rejects the whole edit, and so does
  /// a reset of persistent state under the default policy. Use `reload` for
  /// an explicit full restart. Unchanged completed entries stay finished;
  /// failed entries retry after an accepted reload.
  pub fn reload_preserving(
    &mut self,
    source: Source,
    catalog: &Catalog,
    defines: &std::collections::HashMap<String, String>,
  ) -> Result<Vec<Finished>, (Source, Vec<Diagnostic>)> {
    if self.active.is_none() {
      return self.reload(source, catalog, defines);
    }
    let program = self.load(source, catalog, defines)?;
    let active = self.active.as_mut().expect("active mesh");
    let mut candidate = active.mesh.revision();
    program.declare_on(&mut candidate);
    let entries = match compose_entries(&program, &mut candidate) {
      Ok(entries) => entries,
      Err(diagnostics) => return Err((program.source, diagnostics)),
    };
    // Match scheduled occurrences, not only names (one wire may be scheduled
    // more than once). Finished entry IDs remain here after record retirement.
    let mut retained = HashSet::default();
    let mut plan = Vec::new();
    for (name, wire) in entries {
      let old = active.entries.iter().find(|entry| {
        entry.name == name
          && !retained.contains(&entry.id)
          && !active.failed.contains(&entry.id)
          && candidate.can_retain(&entry.wire)
      });
      let id = old.map(|entry| entry.id);
      if let Some(id) = id {
        retained.insert(id);
      }
      plan.push((name, wire, id));
    }
    let removed = active
      .entries
      .iter()
      .map(|e| e.id)
      .filter(|id| !retained.contains(id))
      .collect();
    if let Err(err) = active.mesh.validate_reload(&mut candidate, &removed) {
      let d = program.diagnostic(PathStep::Wire(String::new()), err);
      return Err((program.source, vec![d]));
    }
    active.mesh.install_revision(candidate, &removed);
    let finished = drain(&mut active.mesh);
    active.entries = plan
      .into_iter()
      .map(|(name, wire, id)| {
        let id = id.unwrap_or_else(|| {
          active
            .mesh
            .spawn(&wire, Var::None)
            .expect("validated candidate wire")
        });
        Entry { name, wire, id }
      })
      .collect();
    let run = program.lowered.run.as_ref();
    active.failed.clear();
    active.ticks = 0;
    active.iterations = run.and_then(|r| r.iterations).map(|n| n as u64);
    active.frame_interval = run
      .and_then(|r| r.fps)
      .and_then(|fps| Duration::try_from_secs_f64(1.0 / fps).ok());
    Ok(finished)
  }
}

fn compose_entries(
  program: &Program,
  mesh: &mut Mesh,
) -> Result<Vec<(String, Arc<CompiledWire>)>, Vec<Diagnostic>> {
  let mut entries = Vec::new();
  let mut diagnostics = Vec::new();
  for name in program.entries() {
    match mesh.compile(&name, Type::none()) {
      Ok(wire) => entries.push((name.clone(), wire)),
      Err(err) => diagnostics.push(program.diagnostic(PathStep::Wire(name), err)),
    }
  }
  for name in program.unreachable_roots() {
    if let Err(err) = mesh.compile(&name, Type::none()) {
      let d = program.diagnostic(PathStep::Wire(name), err);
      if !diagnostics.contains(&d) {
        diagnostics.push(d);
      }
    }
  }
  for def in &program.lowered.functions {
    if let Err(err) = mesh.compile_function(&def.name) {
      let d = program.diagnostic(PathStep::Function(def.name.clone()), err);
      if !diagnostics.contains(&d) {
        diagnostics.push(d);
      }
    }
  }
  if diagnostics.is_empty() {
    Ok(entries)
  } else {
    Err(diagnostics)
  }
}

/// A reader that remembers what it read (`Session::files_read`) and every
/// name it looked at, with what it found (`Session::dependencies`).
struct Recording<'a> {
  inner: &'a dyn Files,
  read: std::cell::RefCell<Vec<String>>,
  dependencies: std::cell::RefCell<Vec<(String, Fingerprint)>>,
}

impl Files for Recording<'_> {
  fn resolve(
    &self,
    from: &str,
    path: &str,
    looked: &mut Vec<String>,
  ) -> Result<crate::files::File, String> {
    let start = looked.len();
    let result = self.inner.resolve(from, path, looked);
    let mut dependencies = self.dependencies.borrow_mut();
    for name in &looked[start..] {
      if dependencies.iter().any(|(n, _)| n == name) {
        continue;
      }
      let found = match &result {
        Ok(file) if file.name == *name => fingerprint(Some(&file.bytes)),
        _ => None,
      };
      dependencies.push((name.clone(), found));
    }
    if let Ok(file) = &result {
      let mut read = self.read.borrow_mut();
      if !read.contains(&file.name) {
        read.push(file.name.clone());
      }
    }
    result
  }

  fn contents(&self, name: &str) -> Option<Vec<u8>> {
    self.inner.contents(name)
  }

  fn key(&self, name: &str) -> String {
    self.inner.key(name)
  }
}
