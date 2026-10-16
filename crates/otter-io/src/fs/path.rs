//! Path helpers that never touch the disk: `FS.Join`, `FS.Extension`,
//! `FS.Filename` and `FS.Parent`, following 1.x. Leaf shards with no
//! effects. They use the host platform's path syntax (separators, Windows
//! prefixes), so they sit with the filesystem shards rather than in the
//! core, whose shards mean the same on every target.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otter_core::compose::ComposeCtx;
use otter_core::describe::{
  DefaultValue, Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets,
  TypeName,
};
use otter_core::instance::{InstanceCtx, LeafCtx};
use otter_core::shards::leaf::{LeafShard, leaf_type};
use otter_core::signature::{Effects, Lifetime};
use otter_core::{Args, Composed, Error, Flow, Result, ShardType, Type, Var, shard_doc};

const STRING: &[TypeName] = &[TypeName::String];

const fn desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  params: &'static [ParamDecl],
  input: InputDesc,
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(params),
    input,
    output: OutputDesc::Fixed(TypeName::String),
    targets: Targets::All,
    aliases: &[],
    effects: Effects::NONE,
    lifetime: Lifetime::Stateless,
  }
}

fn text(path: &Path) -> Var {
  Var::from_string(Arc::from(path.to_string_lossy()))
}

/// The input path (the input type is checked at compose: a String).
fn input_path(input: &Var) -> &Path {
  Path::new(input.as_str().unwrap_or(""))
}

fn strings() -> Type {
  Type::seq(Type::string())
}

// --- FS.Join ---

pub struct Join;
impl LeafShard for Join {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = desc(
    "FS.Join",
    shard_doc!("Joins a sequence of path parts into one path."),
    shard_doc!(
      "Parts are joined with the platform's separator; an absolute part replaces everything before it (`[\"a\" \"/b\"]` is `/b`). An empty sequence is the empty path."
    ),
    &[],
    InputDesc::Typed(strings),
  );

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::string(),
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let mut path = PathBuf::new();
    for part in input.as_seq().unwrap_or(&[]) {
      let Some(part) = part.as_str() else {
        return Err(Error::Activation(format!(
          "FS.Join: every part must be a String, got {}",
          part.type_of()
        )));
      };
      path.push(part);
    }
    Ok(Flow::Next(text(&path)))
  }
}

// --- FS.Extension ---

pub struct Extension;
impl LeafShard for Extension {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = desc(
    "FS.Extension",
    shard_doc!("Outputs the extension of the input path's file name, with its dot (`.txt`)."),
    shard_doc!(
      "The text after the last dot of the file name, dot included; empty when there is none. A name starting with its only dot (`.profile`) has no extension."
    ),
    &[],
    InputDesc::Types(STRING),
  );

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::string(),
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let extension = match input_path(input).extension() {
      Some(ext) => format!(".{}", ext.to_string_lossy()),
      None => String::new(),
    };
    Ok(Flow::Next(Var::from_string(Arc::from(extension))))
  }
}

// --- FS.Filename ---

static FILENAME_PARAMS: &[ParamDecl] = &[ParamDecl::new(
  "no-extension",
  shard_doc!("Leave the extension out."),
  Forms::LITERAL,
  &[TypeName::Bool],
  Requirement::Default(DefaultValue::Bool(false)),
)];

pub struct Filename;
impl LeafShard for Filename {
  type Compiled = bool;
  type State = ();
  const DESC: ShardDesc = desc(
    "FS.Filename",
    shard_doc!("Outputs the last component of the input path: the file name."),
    shard_doc!(
      "Empty when the path ends in `..` or is a root. With `no-extension: true`, the name without its extension."
    ),
    FILENAME_PARAMS,
    InputDesc::Types(STRING),
  );

  fn compose(args: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<bool>> {
    Ok(Composed {
      compiled: args.bool("no-extension").expect("decoded no-extension"),
      output: Type::string(),
    })
  }

  fn instantiate(_: &bool, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(no_extension: &bool, _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let path = input_path(input);
    let name = if *no_extension {
      path.file_stem()
    } else {
      path.file_name()
    };
    Ok(Flow::Next(text(Path::new(name.unwrap_or_default()))))
  }
}

// --- FS.Parent ---

pub struct Parent;
impl LeafShard for Parent {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = desc(
    "FS.Parent",
    shard_doc!("Outputs the input path without its last component."),
    shard_doc!("Empty for a single relative component (`a.txt`) and for a root."),
    &[],
    InputDesc::Types(STRING),
  );

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::string(),
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let parent = input_path(input).parent().unwrap_or(Path::new(""));
    Ok(Flow::Next(text(parent)))
  }
}

pub static JOIN: ShardType = leaf_type::<Join>();
pub static EXTENSION: ShardType = leaf_type::<Extension>();
pub static FILENAME: ShardType = leaf_type::<Filename>();
pub static PARENT: ShardType = leaf_type::<Parent>();
