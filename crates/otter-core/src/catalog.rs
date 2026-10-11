//! The shard catalog (docs/shard-metadata-and-compose.md §3, §6): the
//! shards linked into a runtime, assembled from explicit per-crate lists,
//! with a compact index, full descriptions and search, as JSON.
//!
//! Reading the catalog only reads static descriptions: it never composes,
//! instantiates, starts a reactor or does I/O.

use crate::describe::{DefaultValue, InputDesc, OutputDesc, Params, Requirement, TypeName};
use crate::diagnostic::json_str;
use crate::object::ObjectType;
use crate::shard::ShardType;

/// The JSON schema identifier of catalog documents.
pub const SCHEMA: &str = "otter-catalog/1";

pub struct Catalog {
  shards: Vec<&'static ShardType>,
  /// Every name a shard answers to (canonical names and aliases), sorted
  /// for binary search: looking a name up does not get slower as the
  /// catalog grows.
  index: Vec<(&'static str, &'static ShardType)>,
  /// Object types scripts can name in signatures (docs/host-requirements.md H1).
  objects: Vec<&'static ObjectType>,
}

impl Catalog {
  /// Builds a catalog from per-crate lists of shard types, e.g.
  /// `Catalog::new(&[otter_core::shards::CATALOG, otter_io::CATALOG])`.
  /// Fails on a duplicate shard name, or a parameter whose name is not a
  /// lowercase label (golden-path.md D3: uppercase is a shard).
  pub fn new(lists: &[&[&'static ShardType]]) -> Result<Catalog, String> {
    let mut shards: Vec<&'static ShardType> = Vec::new();
    let names = |s: &ShardType| {
      std::iter::once(s.name())
        .chain(s.desc.aliases.iter().copied())
        .collect::<Vec<_>>()
    };
    for ty in lists.iter().flat_map(|list| list.iter()) {
      for name in names(ty) {
        if shards.iter().any(|s| names(s).contains(&name)) {
          return Err(format!("duplicate shard name in catalog: {name}"));
        }
      }
      if let crate::describe::Params::Declared(params) = ty.desc.params
        && let Some(p) = params.iter().find(|p| !is_label(p.name))
      {
        return Err(format!(
          "{}: parameter `{}` must be a lowercase label (`{}`)",
          ty.name(),
          p.name,
          p.name.to_lowercase()
        ));
      }
      shards.push(ty);
    }
    shards.sort_by_key(|s| s.name());
    let mut index: Vec<_> = shards
      .iter()
      .flat_map(|&s| {
        std::iter::once(s.name())
          .chain(s.desc.aliases.iter().copied())
          .map(move |n| (n, s))
      })
      .collect();
    index.sort_by_key(|&(name, _)| name);
    // The core's own object types (`Wire.Instance`), whatever the lists.
    Ok(Catalog {
      shards,
      index,
      objects: crate::wire_instance::OBJECTS.to_vec(),
    })
  }

  /// Adds per-crate lists of object types, e.g.
  /// `catalog.with_objects(&[otter_llm::OBJECTS])`. Fails on a duplicate
  /// name, or a name that is not `Module.Name` (dot-separated segments,
  /// each an uppercase letter then letters, digits, `_` or `-`):
  /// object types are namespaced by the module that defines them, and never
  /// shadow a builtin type.
  pub fn with_objects(mut self, lists: &[&[&'static ObjectType]]) -> Result<Catalog, String> {
    for ty in lists.iter().flat_map(|list| list.iter()) {
      // What the language reads as one name: segments joined by dots, each
      // an uppercase letter then letters, digits, `_` or `-` (otter-lang's
      // lexer), at least two segments.
      let segment = |part: &str| {
        part.starts_with(|c: char| c.is_ascii_uppercase())
          && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
      };
      let namespaced = ty.name.split('.').all(segment) && ty.name.contains('.');
      if !namespaced {
        return Err(format!(
          "object type `{}` must be named `Module.Name`",
          ty.name
        ));
      }
      if self.objects.iter().any(|o| o.name == ty.name) {
        return Err(format!("duplicate object type in catalog: {}", ty.name));
      }
      self.objects.push(ty);
    }
    self.objects.sort_by_key(|o| o.name);
    Ok(self)
  }

  /// The object type with this name.
  pub fn object_type(&self, name: &str) -> Option<&'static ObjectType> {
    self.objects.iter().copied().find(|o| o.name == name)
  }

  pub fn objects(&self) -> &[&'static ObjectType] {
    &self.objects
  }

  /// The shard with this name or alias.
  pub fn get(&self, name: &str) -> Option<&'static ShardType> {
    self
      .index
      .binary_search_by(|&(n, _)| n.cmp(name))
      .ok()
      .map(|at| self.index[at].1)
  }

  /// Every name the catalog resolves: canonical names and aliases.
  pub fn names(&self) -> Vec<&'static str> {
    self
      .shards
      .iter()
      .flat_map(|s| std::iter::once(s.name()).chain(s.desc.aliases.iter().copied()))
      .collect()
  }

  pub fn shards(&self) -> &[&'static ShardType] {
    &self.shards
  }

  /// Shards whose name or summary contains `query` (case-insensitive).
  pub fn search(&self, query: &str) -> Vec<&'static ShardType> {
    let query = query.to_lowercase();
    self
      .shards
      .iter()
      .copied()
      .filter(|s| {
        s.name().to_lowercase().contains(&query)
          || s
            .desc
            .aliases
            .iter()
            .any(|a| a.to_lowercase().contains(&query))
          || s.desc.summary.to_lowercase().contains(&query)
      })
      .collect()
  }

  /// The compact index: name, summary and targets of every shard.
  pub fn index_json(&self) -> String {
    let entries: Vec<String> = self.shards.iter().map(|s| summary_json(s)).collect();
    format!(
      "{{\"schema\":{},\"shards\":[{}]}}",
      json_str(SCHEMA),
      entries.join(",")
    )
  }

  /// The full description of one shard, or `None` if it is not in the catalog.
  pub fn describe_json(&self, name: &str) -> Option<String> {
    self.get(name).map(describe_json)
  }
}

fn strings_json(items: &[&str]) -> String {
  let items: Vec<String> = items.iter().map(|s| json_str(s)).collect();
  format!("[{}]", items.join(","))
}

fn type_json(t: TypeName) -> String {
  format!(
    "{{\"name\":{},\"basic_type\":{}}}",
    json_str(t.name()),
    t.basic_type()
  )
}

fn types_json(types: &[TypeName]) -> String {
  let items: Vec<String> = types.iter().copied().map(type_json).collect();
  format!("[{}]", items.join(","))
}

/// `,"aliases":[...]` when the shard has aliases, else nothing.
fn aliases_json(s: &ShardType) -> String {
  if s.desc.aliases.is_empty() {
    String::new()
  } else {
    format!(",\"aliases\":{}", strings_json(s.desc.aliases))
  }
}

fn summary_json(s: &ShardType) -> String {
  format!(
    "{{\"name\":{}{},\"summary\":{},\"documented\":{},\"targets\":{},\"effects\":{},\"lifetime\":{}}}",
    json_str(s.name()),
    aliases_json(s),
    json_str(s.desc.summary),
    s.desc.is_documented(),
    json_str(s.desc.targets.name()),
    s.desc.effects.to_json(),
    json_str(s.desc.lifetime.name()),
  )
}

fn default_json(d: DefaultValue) -> String {
  match d {
    DefaultValue::None => "null".into(),
    DefaultValue::Bool(b) => b.to_string(),
    DefaultValue::Int(i) => i.to_string(),
    DefaultValue::Float(f) => f.to_string(),
    DefaultValue::Str(s) => json_str(s),
  }
}

fn describe_json(s: &ShardType) -> String {
  let d = &s.desc;
  let signature = d.signature();
  let input = match d.input {
    InputDesc::Any => "{\"kind\":\"any\"}".to_string(),
    InputDesc::Ignored => "{\"kind\":\"ignored\"}".to_string(),
    InputDesc::Types(types) => format!("{{\"kind\":\"types\",\"types\":{}}}", types_json(types)),
    InputDesc::Typed(ty) => format!(
      "{{\"kind\":\"type\",\"type\":{}}}",
      json_str(&ty().to_string())
    ),
  };
  let output = match d.output {
    OutputDesc::Fixed(t) => format!("{{\"kind\":\"fixed\",\"type\":{}}}", type_json(t)),
    OutputDesc::Passthrough => "{\"kind\":\"passthrough\"}".to_string(),
    OutputDesc::SameAsInput => "{\"kind\":\"same-as-input\"}".to_string(),
    OutputDesc::Dynamic(text) => format!(
      "{{\"kind\":\"dynamic\",\"description\":{}}}",
      json_str(text)
    ),
  };
  // The parameter list is rendered from the same declarations the argument
  // decoder enforces.
  let params = match d.params {
    Params::Undeclared => "null".to_string(),
    Params::Declared(decls) => {
      let items: Vec<String> = decls
        .iter()
        .enumerate()
        .map(|(index, p)| {
          let mut fields = vec![
            format!("\"name\":{}", json_str(p.name)),
            format!("\"index\":{index}"),
            format!("\"help\":{}", json_str(p.help)),
            format!("\"forms\":{}", strings_json(&p.forms.names())),
            // A typed parameter without a type list documents the type
            // code derived from its full type.
            match (p.types, p.ty) {
              ([], Some(ty)) => {
                let t = crate::diagnostic::TypeRef::of(ty());
                format!(
                  "\"types\":[{{\"name\":{},\"basic_type\":{}}}]",
                  json_str(&t.name),
                  t.basic_type
                )
              }
              _ => format!("\"types\":{}", types_json(p.types)),
            },
            format!("\"required\":{}", p.requirement == Requirement::Required),
          ];
          if p.requirement == Requirement::Variadic {
            fields.push("\"variadic\":true".to_string());
          }
          // The full type, when declared beyond the type list.
          if let Some(ty) = p.ty {
            fields.push(format!("\"type\":{}", json_str(&ty().to_string())));
          }
          if let Requirement::Default(default) = p.requirement {
            fields.push(format!("\"default\":{}", default_json(default)));
          }
          format!("{{{}}}", fields.join(","))
        })
        .collect();
      format!("[{}]", items.join(","))
    }
  };
  format!(
    "{{\"schema\":{},\"name\":{}{},\"version\":{},\"summary\":{},\"help\":{},\"documented\":{},\"targets\":{},\"input\":{input},\"output\":{output},\"params\":{params},\"effects\":{},\"lifetime\":{},\"uses\":[],\"mutates\":[],\"signature\":{}}}",
    json_str(SCHEMA),
    json_str(d.name),
    aliases_json(s),
    d.version,
    json_str(d.summary),
    json_str(d.help),
    d.is_documented(),
    json_str(d.targets.name()),
    signature.effects.to_json(),
    json_str(signature.lifetime.name()),
    signature.to_json(),
  )
}

/// A parameter label: lowercase letters, digits and `-`, starting with a
/// letter, as variable names are written.
fn is_label(name: &str) -> bool {
  name.starts_with(|c: char| c.is_ascii_lowercase())
    && name
      .chars()
      .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}
