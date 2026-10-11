//! Shared signature and inferred compose metadata (golden path M3).
//! Native views borrow static descriptions; an owned `Signature<'static>`
//! can describe a script without leaking strings or parameter declarations.

use std::borrow::Cow;

use crate::describe::{Forms, InputDesc, OutputDesc, Params, Requirement, ShardDesc};
use crate::diagnostic::{PathStep, json_str};
use crate::{Type, Var};

/// Trusted effects of a native operation, or the union for a compiled body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effects {
  pub suspends: bool,
  pub io: bool,
  pub time: bool,
  pub random: bool,
  pub unknown: bool,
}

impl Effects {
  pub const NONE: Self = Self {
    suspends: false,
    io: false,
    time: false,
    random: false,
    unknown: false,
  };
  pub const UNKNOWN: Self = Self {
    unknown: true,
    ..Self::NONE
  };
  pub const IO: Self = Self {
    io: true,
    ..Self::NONE
  };
  pub const TIME: Self = Self {
    time: true,
    ..Self::NONE
  };
  pub const WAIT: Self = Self {
    suspends: true,
    time: true,
    ..Self::NONE
  };
  pub const IO_WAIT: Self = Self {
    suspends: true,
    io: true,
    time: true,
    ..Self::NONE
  };

  pub const fn union(self, other: Self) -> Self {
    Self {
      suspends: self.suspends || other.suspends,
      io: self.io || other.io,
      time: self.time || other.time,
      random: self.random || other.random,
      unknown: self.unknown || other.unknown,
    }
  }

  pub fn to_json(self) -> String {
    format!(
      "{{\"suspends\":{},\"io\":{},\"time\":{},\"random\":{},\"unknown\":{}}}",
      self.suspends, self.io, self.time, self.random, self.unknown
    )
  }
}

impl Default for Effects {
  fn default() -> Self {
    Self::UNKNOWN
  }
}

/// Persistent semantic state, not temporary state used while suspended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lifetime {
  Stateless,
  Stateful,
  #[default]
  Unknown,
}

impl Lifetime {
  pub fn name(self) -> &'static str {
    match self {
      Self::Stateless => "stateless",
      Self::Stateful => "stateful",
      Self::Unknown => "unknown",
    }
  }
  pub fn union(self, other: Self) -> Self {
    match (self, other) {
      (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
      (Self::Stateful, _) | (_, Self::Stateful) => Self::Stateful,
      _ => Self::Stateless,
    }
  }
}

#[derive(Clone, Debug)]
pub enum SignatureInput {
  Ignored,
  Type(Type),
}
#[derive(Clone, Debug)]
pub enum SignatureOutput {
  Type(Type),
  Passthrough,
  SameAsInput,
  Dynamic,
}
#[derive(Clone, Debug)]
pub enum ParameterRequirement {
  Required,
  Optional,
  Default(Var),
  Variadic,
}
#[derive(Clone, Debug)]
pub struct Parameter<'a> {
  pub name: Cow<'a, str>,
  pub help: Cow<'a, str>,
  pub forms: Forms,
  pub ty: Type,
  pub requirement: ParameterRequirement,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshAccess {
  pub name: String,
  pub ty: Type,
}
#[derive(Clone, Debug)]
pub struct DefinitionSource<'a> {
  pub file: Cow<'a, str>,
  pub line: u32,
  pub column: u32,
}
#[derive(Clone, Debug)]
pub struct Signature<'a> {
  pub name: Cow<'a, str>,
  pub revision: u32,
  pub input: SignatureInput,
  pub output: SignatureOutput,
  /// None means the host has not declared its parameter contract.
  pub params: Option<Vec<Parameter<'a>>>,
  pub lifetime: Lifetime,
  pub effects: Effects,
  /// Native requirements whose names depend on arguments are inferred by compose.
  pub uses: Vec<MeshAccess>,
  pub mutates: Vec<MeshAccess>,
  pub source: Option<DefinitionSource<'a>>,
  pub summary: Cow<'a, str>,
  pub help: Cow<'a, str>,
}

impl ShardDesc {
  pub fn signature(&self) -> Signature<'_> {
    Signature {
      name: self.name.into(),
      revision: self.version,
      input: match self.input {
        InputDesc::Ignored => SignatureInput::Ignored,
        InputDesc::Any => SignatureInput::Type(Type::any()),
        InputDesc::Types(ts) => SignatureInput::Type(Type::union(
          ts.iter().map(|t| t.to_type()).collect::<Vec<_>>(),
        )),
        InputDesc::Typed(t) => SignatureInput::Type(t()),
      },
      output: match self.output {
        OutputDesc::Fixed(t) => SignatureOutput::Type(t.to_type()),
        OutputDesc::Passthrough => SignatureOutput::Passthrough,
        OutputDesc::SameAsInput => SignatureOutput::SameAsInput,
        OutputDesc::Dynamic(_) => SignatureOutput::Dynamic,
      },
      params: match self.params {
        Params::Undeclared => None,
        Params::Declared(ps) => Some(
          ps.iter()
            .map(|p| Parameter {
              name: p.name.into(),
              help: p.help.into(),
              forms: p.forms,
              ty: p.ty.map_or_else(
                || {
                  if p.types.is_empty() {
                    Type::any()
                  } else {
                    Type::union(p.types.iter().map(|t| t.to_type()).collect::<Vec<_>>())
                  }
                },
                |t| t(),
              ),
              requirement: match p.requirement {
                Requirement::Required => ParameterRequirement::Required,
                Requirement::Optional => ParameterRequirement::Optional,
                Requirement::Default(d) => ParameterRequirement::Default(d.to_var()),
                Requirement::Variadic => ParameterRequirement::Variadic,
              },
            })
            .collect(),
        ),
      },
      lifetime: self.lifetime,
      effects: self.effects,
      uses: Vec::new(),
      mutates: Vec::new(),
      source: None,
      summary: self.summary.into(),
      help: self.help.into(),
    }
  }
}

/// One successfully composed occurrence. Paths are semantic, never source locations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
  pub path: Vec<PathStep>,
  pub input: Type,
  pub output: Type,
  pub effects: Effects,
  pub lifetime: Lifetime,
  /// For a function call, how it runs: a call site that is not inlined
  /// costs a frame, and its blocks may too (`check --json`). Recorded only
  /// when asked (`Mesh::record_call_sites`), and stored apart from the
  /// occurrence (`Occurrences`): compiled flows keep every occurrence, and
  /// a field in each cost the classic ESP32 up to 7 KB of its lowest free
  /// heap.
  pub call: Option<Box<CallSite>>,
}

/// How a call site runs: `inlined` (the callee's code, and its blocks, in
/// place), `vm` (the callee's straight-line body in the VM, on a frame
/// the site keeps) or `framed` (the engine enters a frame for the callee,
/// and runs each block in one of its own unless it is leaf code), with why
/// it is not inlined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSite {
  pub path: &'static str,
  pub reason: Option<NotInlined>,
}

/// Why a call site is not inlined; printed in words (`Display`), kept as
/// data so recording it allocates no text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotInlined {
  Recursive,
  Stateful,
  /// The block for this parameter is the caller's own flow parameter.
  Forwarded(Box<str>),
  Block(Box<str>, NotVm),
  Body(NotVm),
  /// Instructions copied for blocks run more than once, and the budget.
  BlockCopies(u32, u32),
  Keeps,
  CallsRecursive,
  /// The body's instructions, and the budget.
  TooLong(u32, u32),
  TooDeep(u8),
}

/// Why a flow is not straight-line VM code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotVm {
  NoVmForm(&'static str),
  CallsNotInlined,
  NotStraightLine,
}

impl std::fmt::Display for NotVm {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      NotVm::NoVmForm(shard) => write!(f, "{shard} has no VM form"),
      NotVm::CallsNotInlined => f.write_str("it calls a function that is not inlined"),
      NotVm::NotStraightLine => f.write_str("it is not straight-line code"),
    }
  }
}

impl std::fmt::Display for NotInlined {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      NotInlined::Recursive => f.write_str("it is recursive"),
      NotInlined::Stateful => f.write_str("it is stateful (each site keeps an instance)"),
      NotInlined::Forwarded(param) => write!(
        f,
        "the block for {param} is passed on from the caller's own parameter"
      ),
      NotInlined::Block(param, why) => write!(f, "the block for {param}: {why}"),
      NotInlined::Body(why) => write!(f, "its body: {why}"),
      NotInlined::BlockCopies(copies, budget) => write!(
        f,
        "its blocks run more than once, {copies} instructions copied, over the budget of {budget}"
      ),
      NotInlined::Keeps => f.write_str("it keeps state"),
      NotInlined::CallsRecursive => f.write_str("it calls a recursive function"),
      NotInlined::TooLong(len, budget) => write!(
        f,
        "its body is {len} instructions, over the budget of {budget}"
      ),
      NotInlined::TooDeep(depth) => write!(f, "it inlines calls {depth} levels deep already"),
    }
  }
}

/// A persistent occurrence tree: parents retain a child's relative paths once,
/// rather than copying all descendant paths into every compiled ancestor.
/// Iteration materializes full paths only for tooling that requests them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Occurrences(std::sync::Arc<Vec<OccurrenceEntry>>);
#[derive(Clone, Debug, PartialEq, Eq)]
enum OccurrenceEntry {
  Node(Node),
  /// How the call in the `Node` just before runs, when recorded.
  Call(Box<CallSite>),
  Child(Vec<PathStep>, Occurrences),
}
/// An occurrence as stored: `Occurrence` without its call site.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
  path: Vec<PathStep>,
  input: Type,
  output: Type,
  effects: Effects,
  lifetime: Lifetime,
}
impl Occurrences {
  pub fn is_empty(&self) -> bool {
    self.0.is_empty()
  }
  pub fn len(&self) -> usize {
    self.iter().count()
  }
  pub fn iter(&self) -> impl Iterator<Item = Occurrence> + '_ {
    let mut stack = vec![(self.0.iter(), 0)];
    let mut prefix = Vec::new();
    std::iter::from_fn(move || {
      loop {
        let (entries, _) = stack.last_mut()?;
        match entries.next() {
          Some(OccurrenceEntry::Node(node)) => {
            let mut path = node.path.clone();
            path.splice(0..0, prefix.iter().cloned());
            let call = match entries.as_slice().first() {
              Some(OccurrenceEntry::Call(call)) => {
                let call = call.clone();
                entries.next();
                Some(call)
              }
              _ => None,
            };
            return Some(Occurrence {
              path,
              input: node.input,
              output: node.output,
              effects: node.effects,
              lifetime: node.lifetime,
              call,
            });
          }
          Some(OccurrenceEntry::Call(_)) => unreachable!("read with its node"),
          Some(OccurrenceEntry::Child(path, child)) => {
            let restore = prefix.len();
            prefix.extend(path.iter().cloned());
            stack.push((child.0.iter(), restore));
          }
          None => {
            let (_, restore) = stack.pop().expect("nonempty");
            prefix.truncate(restore);
          }
        }
      }
    })
  }
  pub(crate) fn insert(&mut self, index: usize, occurrence: Occurrence) {
    let entries = std::sync::Arc::make_mut(&mut self.0);
    if let Some(call) = occurrence.call {
      entries.insert(index, OccurrenceEntry::Call(call));
    }
    entries.insert(
      index,
      OccurrenceEntry::Node(Node {
        path: occurrence.path,
        input: occurrence.input,
        output: occurrence.output,
        effects: occurrence.effects,
        lifetime: occurrence.lifetime,
      }),
    );
  }
  fn include(&mut self, child: &Self, prefix: &[PathStep]) {
    if !child.is_empty() {
      std::sync::Arc::make_mut(&mut self.0)
        .push(OccurrenceEntry::Child(prefix.to_vec(), child.clone()));
    }
  }
}

/// Immutable analysis shared with compiled code. A caller prefixes the relative
/// paths when incorporating a cached callee, preserving every use site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Analysis {
  pub effects: Effects,
  pub lifetime: Lifetime,
  pub uses: Vec<MeshAccess>,
  pub mutates: Vec<MeshAccess>,
  pub occurrences: Occurrences,
  /// The first native shard reached that a compose-time evaluation may not
  /// run (`compose_time::eligible`), by name.
  pub not_compose_time: Option<&'static str>,
  /// The path to the first shard reached that reads or assigns a mesh
  /// variable, relative like the occurrences.
  pub mesh_at: Option<Vec<PathStep>>,
}
impl Default for Analysis {
  fn default() -> Self {
    Self {
      effects: Effects::NONE,
      lifetime: Lifetime::Stateless,
      uses: Vec::new(),
      mutates: Vec::new(),
      occurrences: Occurrences::default(),
      not_compose_time: None,
      mesh_at: None,
    }
  }
}
impl Analysis {
  pub(crate) fn access(&mut self, name: &str, ty: Type, write: bool) {
    let target = if write {
      &mut self.mutates
    } else {
      &mut self.uses
    };
    if self.mesh_at.is_none() {
      self.mesh_at = Some(Vec::new());
    }
    let access = MeshAccess {
      name: name.into(),
      ty,
    };
    if !target.contains(&access) {
      target.push(access);
      target.sort_by(|a, b| a.name.cmp(&b.name));
    }
  }
  pub(crate) fn include(&mut self, child: &Self, prefix: &[PathStep]) {
    if self.mesh_at.is_none()
      && let Some(at) = &child.mesh_at
    {
      self.mesh_at = Some(prefix.iter().chain(at).cloned().collect());
    }
    self.effects = self.effects.union(child.effects);
    self.lifetime = self.lifetime.union(child.lifetime);
    for access in &child.uses {
      self.access(&access.name, access.ty, false);
    }
    for access in &child.mutates {
      self.access(&access.name, access.ty, true);
    }
    self.occurrences.include(&child.occurrences, prefix);
    if self.not_compose_time.is_none() {
      self.not_compose_time = child.not_compose_time;
    }
  }
  pub fn mesh_json(accesses: &[MeshAccess]) -> String {
    format!(
      "[{}]",
      accesses
        .iter()
        .map(|a| format!(
          "{{\"name\":{},\"type\":{}}}",
          json_str(&a.name),
          json_str(&a.ty.to_string())
        ))
        .collect::<Vec<_>>()
        .join(",")
    )
  }
}

impl Signature<'_> {
  /// The same representation for native and owned script signatures.
  pub fn to_json(&self) -> String {
    let input = match self.input {
      SignatureInput::Ignored => "{\"kind\":\"ignored\"}".into(),
      SignatureInput::Type(t) => format!(
        "{{\"kind\":\"type\",\"type\":{}}}",
        json_str(&t.to_string())
      ),
    };
    let output = match self.output {
      SignatureOutput::Type(t) => format!(
        "{{\"kind\":\"type\",\"type\":{}}}",
        json_str(&t.to_string())
      ),
      SignatureOutput::Passthrough => "{\"kind\":\"passthrough\"}".into(),
      SignatureOutput::SameAsInput => "{\"kind\":\"same-as-input\"}".into(),
      SignatureOutput::Dynamic => "{\"kind\":\"dynamic\"}".into(),
    };
    let params = self.params.as_ref().map_or("null".into(), |ps| {
      format!(
        "[{}]",
        ps.iter()
          .map(|p| {
            let (requirement, default) = match &p.requirement {
              ParameterRequirement::Required => ("required", String::new()),
              ParameterRequirement::Optional => ("optional", String::new()),
              ParameterRequirement::Variadic => ("variadic", String::new()),
              ParameterRequirement::Default(v) => (
                "default",
                format!(
                  ",\"default\":{{\"type\":{},\"literal\":{}}}",
                  json_str(&v.type_of().to_string()),
                  json_str(&v.to_string())
                ),
              ),
            };
            format!(
              "{{\"name\":{},\"type\":{},\"forms\":[{}],\"requirement\":{}{default},\"help\":{}}}",
              json_str(&p.name),
              json_str(&p.ty.to_string()),
              p.forms
                .names()
                .iter()
                .map(|s| json_str(s))
                .collect::<Vec<_>>()
                .join(","),
              json_str(requirement),
              json_str(&p.help)
            )
          })
          .collect::<Vec<_>>()
          .join(",")
      )
    });
    let source = self.source.as_ref().map_or("null".into(), |s| {
      format!(
        "{{\"file\":{},\"line\":{},\"column\":{}}}",
        json_str(&s.file),
        s.line,
        s.column
      )
    });
    format!(
      "{{\"name\":{},\"revision\":{},\"input\":{input},\"output\":{output},\"params\":{params},\"lifetime\":{},\"effects\":{},\"uses\":{},\"mutates\":{},\"source\":{source},\"summary\":{},\"help\":{}}}",
      json_str(&self.name),
      self.revision,
      json_str(self.lifetime.name()),
      self.effects.to_json(),
      Analysis::mesh_json(&self.uses),
      Analysis::mesh_json(&self.mutates),
      json_str(&self.summary),
      json_str(&self.help)
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn shared_occurrences_keep_each_call_path_without_copying_descendants() {
    let mut leaf = Occurrences::default();
    leaf.insert(
      0,
      Occurrence {
        path: vec![PathStep::Item(99)],
        input: Type::any(),
        output: Type::any(),
        effects: Effects::NONE,
        lifetime: Lifetime::Stateless,
        call: None,
      },
    );
    let mut chain = leaf.clone();
    for depth in 0..32 {
      let mut parent = Occurrences::default();
      parent.include(&chain, &[PathStep::Item(depth)]);
      let OccurrenceEntry::Child(_, retained) = &parent.0[0] else {
        panic!("child")
      };
      assert!(std::sync::Arc::ptr_eq(&retained.0, &chain.0));
      chain = parent;
    }
    let mut callers = Occurrences::default();
    callers.include(&chain, &[PathStep::Wire("first".into())]);
    callers.include(&chain, &[PathStep::Wire("second".into())]);
    callers.include(&leaf, &[PathStep::Wire("sibling".into())]);
    let paths: Vec<_> = callers.iter().map(|o| o.path).collect();
    for (index, name) in ["first", "second"].into_iter().enumerate() {
      let mut expected = vec![PathStep::Wire(name.into())];
      expected.extend((0..32).rev().map(PathStep::Item));
      expected.push(PathStep::Item(99));
      assert_eq!(paths[index], expected);
    }
    assert_eq!(
      paths[2],
      vec![PathStep::Wire("sibling".into()), PathStep::Item(99)]
    );
    assert_eq!(callers.len(), 3);
    // Copy-on-write preserves reports already incorporated by a parent.
    let duplicate = leaf.iter().next().unwrap();
    leaf.insert(0, duplicate);
    assert_eq!(leaf.len(), 2);
    assert_eq!(callers.len(), 3);
  }
}
