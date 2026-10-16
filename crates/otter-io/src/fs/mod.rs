//! Filesystem shards, following 1.x (`shards/modules/fs`): `FS.Read`,
//! `FS.Write`, `FS.Exists`, `FS.IsFile`, `FS.IsDirectory`, `FS.Iterate`,
//! `FS.CreateDirectories`, `FS.Remove`, `FS.RemoveAll`, `FS.Copy` and
//! `FS.Rename`, and the path helpers in [`path`].
//!
//! Every shard that touches the disk is an async shard whose operation runs
//! on the shared runtime's blocking pool ([`crate::runtime::spawn_blocking`]),
//! so a slow disk or a network filesystem suspends only the waiting
//! instance. The input is the path. Which paths are allowed, and what a
//! relative path means, is the mesh's [`FsAccess`] service, read at
//! activation (unrestricted against the process's working directory when
//! the host inserts none). Failures are activation errors naming the shard,
//! the path as the script gave it, and the OS message.
//!
//! Cancellation: an operation that walks a tree (`FS.Iterate`,
//! `FS.RemoveAll`, copying a directory) checks the token between entries and
//! stops there; what it already changed stays changed. A single read, write
//! or rename cannot be interrupted and finishes on its own.

pub mod access;
pub mod path;

pub use access::FsAccess;

use std::fs;
use std::io::{self, ErrorKind, Write as _};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use otter_core::compose::ComposeCtx;
use otter_core::describe::{
  DefaultValue, Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets,
  TypeName,
};
use otter_core::instance::LeafCtx;
use otter_core::shards::Operand;
use otter_core::shards::async_shard::{AsyncShard, async_type};
use otter_core::signature::{Effects, Lifetime};
use otter_core::{Args, Composed, Error, Result, ShardType, Type, Var, shard_doc};

use crate::runtime::{CancellationToken, IoTask, spawn_blocking};

/// One filesystem operation on the path its shard receives as input.
/// [`Disk`] makes it an async shard running on the blocking pool.
pub trait DiskOp: 'static {
  type Compiled: Send + Sync + 'static;
  /// What `run` needs from the compiled arguments, owned (variables are
  /// read at activation, on the engine thread).
  type Owned: Send + 'static;
  const DESC: ShardDesc;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Self::Compiled>>;

  fn owned(compiled: &Self::Compiled, ctx: &impl LeafCtx) -> Result<Self::Owned>;

  /// Runs on the blocking pool. `path` is the input as the script gave it.
  fn run(
    access: &FsAccess,
    path: &str,
    owned: Self::Owned,
    cancel: &CancellationToken,
  ) -> std::result::Result<Var, String>;
}

/// Adapts a [`DiskOp`] to [`AsyncShard`].
pub struct Disk<O>(PhantomData<fn() -> O>);

impl<O: DiskOp> AsyncShard for Disk<O> {
  type Compiled = O::Compiled;
  type Op = IoTask;
  const DESC: ShardDesc = O::DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<O::Compiled>> {
    O::compose(args, ctx)
  }

  fn start(c: &O::Compiled, ctx: &mut impl LeafCtx, input: &Var) -> Result<IoTask> {
    // The input type is checked at compose (a String).
    let Var::String(path) = input else {
      return Err(Error::Activation(format!(
        "{}: the path must be a String",
        O::DESC.name
      )));
    };
    let path = Arc::clone(path);
    let owned = O::owned(c, ctx)?;
    // Owned by the operation, so releasing the instance cannot release it.
    let access = FsAccess::of(ctx.services());
    Ok(spawn_blocking(move |cancel| {
      O::run(&access, &path, owned, &cancel)
    }))
  }
}

const STRING: &[TypeName] = &[TypeName::String];
const BOOL: &[TypeName] = &[TypeName::Bool];
const TEXT_OR_BYTES: &[TypeName] = &[TypeName::String, TypeName::Bytes];
const OPERAND: Forms = Forms::LITERAL.or(Forms::VARIABLE);

const fn desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  params: &'static [ParamDecl],
  output: OutputDesc,
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(params),
    input: InputDesc::Types(STRING),
    output,
    targets: Targets::NativeOnly,
    aliases: &[],
    effects: Effects::IO_WAIT,
    lifetime: Lifetime::Stateless,
  }
}

const fn flag(name: &'static str, help: &'static str) -> ParamDecl {
  ParamDecl::new(
    name,
    help,
    Forms::LITERAL,
    BOOL,
    Requirement::Default(DefaultValue::Bool(false)),
  )
}

/// `shard: path: message`, the form of every failure.
fn at(shard: &str, path: &str, error: impl std::fmt::Display) -> String {
  format!("{shard}: {path}: {error}")
}

fn cancelled(shard: &str) -> String {
  format!("{shard}: cancelled")
}

fn exists_error() -> io::Error {
  io::Error::new(ErrorKind::AlreadyExists, "already exists")
}

/// A path for output: `base` (as the script gave it) joined with `rest`.
fn display_path(base: &str, rest: &Path) -> String {
  if rest.as_os_str().is_empty() {
    return base.to_string();
  }
  Path::new(base).join(rest).to_string_lossy().into_owned()
}

/// The String or Bytes operand value as bytes.
fn contents_of(value: &Var) -> Option<&[u8]> {
  value
    .as_str()
    .map(str::as_bytes)
    .or_else(|| value.as_bytes())
}

// --- FS.Read ---

static READ_PARAMS: &[ParamDecl] = &[flag(
  "bytes",
  shard_doc!("Output the contents as Bytes instead of a String."),
)];

pub struct Read;
impl DiskOp for Read {
  type Compiled = bool;
  type Owned = bool;
  const DESC: ShardDesc = desc(
    "FS.Read",
    shard_doc!("Reads a whole file: its contents as a String, or as Bytes with `bytes: true`."),
    shard_doc!(
      "The input is the file's path. Fails when the file cannot be read, and, without `bytes: true`, when its contents are not valid UTF-8."
    ),
    READ_PARAMS,
    OutputDesc::Dynamic(shard_doc!("String, or Bytes with `bytes: true`")),
  );

  fn compose(args: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<bool>> {
    let bytes = args.bool("bytes").expect("decoded bytes");
    Ok(Composed {
      compiled: bytes,
      output: if bytes { Type::bytes() } else { Type::string() },
    })
  }

  fn owned(bytes: &bool, _: &impl LeafCtx) -> Result<bool> {
    Ok(*bytes)
  }

  fn run(
    access: &FsAccess,
    path: &str,
    bytes: bool,
    _: &CancellationToken,
  ) -> std::result::Result<Var, String> {
    let name = Self::DESC.name;
    let resolved = access
      .resolve(Path::new(path), false)
      .map_err(|e| at(name, path, e))?;
    let data = fs::read(&resolved).map_err(|e| at(name, path, e))?;
    if bytes {
      return Ok(Var::from_bytes(Arc::from(data)));
    }
    let text = String::from_utf8(data)
      .map_err(|_| at(name, path, "not valid UTF-8 (read it with `bytes: true`)"))?;
    Ok(Var::from_string(Arc::from(text)))
  }
}

// --- FS.Write ---

static WRITE_PARAMS: &[ParamDecl] = &[
  ParamDecl::new(
    "contents",
    shard_doc!("What to write: a String or Bytes literal or variable."),
    OPERAND,
    TEXT_OR_BYTES,
    Requirement::Required,
  ),
  flag(
    "overwrite",
    shard_doc!(
      "Replace the file when it exists; without it (or `append`), an existing file is an error."
    ),
  ),
  flag(
    "append",
    shard_doc!("Add to the end of the file, creating it when it is missing."),
  ),
];

pub struct WriteCompiled {
  contents: Operand,
  overwrite: bool,
  append: bool,
}

pub struct Write;
impl DiskOp for Write {
  type Compiled = WriteCompiled;
  type Owned = (Var, bool, bool);
  const DESC: ShardDesc = desc(
    "FS.Write",
    shard_doc!("Writes `contents` to the file at the input path, and outputs the path."),
    shard_doc!(
      "Creates the file. An existing file is an error unless `overwrite: true` (replace it) or `append: true` (add to its end). The directory must exist: create it with `FS.CreateDirectories`."
    ),
    WRITE_PARAMS,
    OutputDesc::Passthrough,
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<WriteCompiled>> {
    let (contents, _) = Operand::compose_arg(args, "contents", Self::DESC.name, ctx)?;
    Ok(Composed {
      compiled: WriteCompiled {
        contents,
        overwrite: args.bool("overwrite").expect("decoded overwrite"),
        append: args.bool("append").expect("decoded append"),
      },
      output: Type::string(),
    })
  }

  fn owned(c: &WriteCompiled, ctx: &impl LeafCtx) -> Result<(Var, bool, bool)> {
    Ok((c.contents.get(ctx), c.overwrite, c.append))
  }

  fn run(
    access: &FsAccess,
    path: &str,
    (contents, overwrite, append): (Var, bool, bool),
    _: &CancellationToken,
  ) -> std::result::Result<Var, String> {
    let name = Self::DESC.name;
    let data = contents_of(&contents)
      .ok_or_else(|| format!("{name}: contents must be a String or Bytes"))?;
    let resolved = access
      .resolve(Path::new(path), true)
      .map_err(|e| at(name, path, e))?;
    let mut options = fs::OpenOptions::new();
    if append {
      options.append(true).create(true);
    } else if overwrite {
      options.write(true).create(true).truncate(true);
    } else {
      options.write(true).create_new(true);
    }
    let mut file = options.open(&resolved).map_err(|e| at(name, path, e))?;
    file.write_all(data).map_err(|e| at(name, path, e))?;
    Ok(Var::string(path))
  }
}

// --- FS.Exists, FS.IsFile, FS.IsDirectory ---

/// Whether `path` exists and its metadata (links followed) passes `test`.
/// A missing path, or one through a file, is `false`; other errors fail.
fn probe(
  name: &str,
  access: &FsAccess,
  path: &str,
  test: fn(&fs::Metadata) -> bool,
) -> std::result::Result<Var, String> {
  let missing = |e: &io::Error| matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory);
  let resolved = match access.resolve(Path::new(path), false) {
    Ok(resolved) => resolved,
    Err(e) if missing(&e) => return Ok(Var::Bool(false)),
    Err(e) => return Err(at(name, path, e)),
  };
  match fs::metadata(&resolved) {
    Ok(meta) => Ok(Var::Bool(test(&meta))),
    Err(e) if missing(&e) => Ok(Var::Bool(false)),
    Err(e) => Err(at(name, path, e)),
  }
}

macro_rules! probe_op {
  ($ty:ident, $name:literal, $summary:literal, $test:expr) => {
    pub struct $ty;
    impl DiskOp for $ty {
      type Compiled = ();
      type Owned = ();
      const DESC: ShardDesc = desc(
        $name,
        shard_doc!($summary),
        shard_doc!(
          "The input is the path; symbolic links are followed. A missing path is `false`; an error other than a missing path (no permission) fails."
        ),
        &[],
        OutputDesc::Fixed(TypeName::Bool),
      );

      fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
        Ok(Composed {
          compiled: (),
          output: Type::bool(),
        })
      }

      fn owned(_: &(), _: &impl LeafCtx) -> Result<()> {
        Ok(())
      }

      fn run(
        access: &FsAccess,
        path: &str,
        _: (),
        _: &CancellationToken,
      ) -> std::result::Result<Var, String> {
        probe(Self::DESC.name, access, path, $test)
      }
    }
  };
}

probe_op!(
  Exists,
  "FS.Exists",
  "Outputs whether a file or directory exists at the input path.",
  |_| true
);
probe_op!(
  IsFile,
  "FS.IsFile",
  "Outputs whether the input path exists and is not a directory.",
  |m| !m.is_dir()
);
probe_op!(
  IsDirectory,
  "FS.IsDirectory",
  "Outputs whether the input path is a directory.",
  |m| m.is_dir()
);

// --- FS.Iterate ---

static ITERATE_PARAMS: &[ParamDecl] = &[flag(
  "recursive",
  shard_doc!(
    "Include the contents of subdirectories too (symbolic links to directories are listed, not entered)."
  ),
)];

fn strings() -> Type {
  Type::seq(Type::string())
}

pub struct Iterate;
impl DiskOp for Iterate {
  type Compiled = bool;
  type Owned = bool;
  const DESC: ShardDesc = desc(
    "FS.Iterate",
    shard_doc!("Lists a directory: the paths of its files and subdirectories, sorted."),
    shard_doc!(
      "The input is the directory. Each output path is the input path joined with the entry's name (and its subdirectories, with `recursive: true`), so relative input gives relative output. Hidden entries are included. An entry that cannot be read fails the listing."
    ),
    ITERATE_PARAMS,
    OutputDesc::Dynamic(shard_doc!("[String]")),
  );

  fn compose(args: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<bool>> {
    Ok(Composed {
      compiled: args.bool("recursive").expect("decoded recursive"),
      output: strings(),
    })
  }

  fn owned(recursive: &bool, _: &impl LeafCtx) -> Result<bool> {
    Ok(*recursive)
  }

  fn run(
    access: &FsAccess,
    path: &str,
    recursive: bool,
    cancel: &CancellationToken,
  ) -> std::result::Result<Var, String> {
    let name = Self::DESC.name;
    let root = access
      .resolve(Path::new(path), false)
      .map_err(|e| at(name, path, e))?;
    let mut found = Vec::new();
    // Directories still to list: where they are, and their path below the
    // input.
    let mut pending = vec![(root, PathBuf::new())];
    while let Some((dir, below)) = pending.pop() {
      let failed = |e: io::Error| at(name, &display_path(path, &below), e);
      for entry in fs::read_dir(&dir).map_err(failed)? {
        if cancel.is_cancelled() {
          return Err(cancelled(name));
        }
        let entry = entry.map_err(failed)?;
        let entry_below = below.join(entry.file_name());
        // `file_type` does not follow links, so a link is never entered.
        if recursive && entry.file_type().map_err(failed)?.is_dir() {
          pending.push((entry.path(), entry_below.clone()));
        }
        found.push(display_path(path, &entry_below));
      }
    }
    found.sort_unstable();
    Ok(Var::seq(found.iter().map(|p| Var::string(p)).collect()))
  }
}

// --- FS.CreateDirectories ---

pub struct CreateDirectories;
impl DiskOp for CreateDirectories {
  type Compiled = ();
  type Owned = ();
  const DESC: ShardDesc = desc(
    "FS.CreateDirectories",
    shard_doc!(
      "Creates the directory at the input path and any missing parents, and outputs the path."
    ),
    shard_doc!("A directory that already exists is not an error; a file in its place is."),
    &[],
    OutputDesc::Passthrough,
  );

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::string(),
    })
  }

  fn owned(_: &(), _: &impl LeafCtx) -> Result<()> {
    Ok(())
  }

  fn run(
    access: &FsAccess,
    path: &str,
    _: (),
    _: &CancellationToken,
  ) -> std::result::Result<Var, String> {
    let name = Self::DESC.name;
    let resolved = access
      .resolve(Path::new(path), true)
      .map_err(|e| at(name, path, e))?;
    fs::create_dir_all(&resolved).map_err(|e| at(name, path, e))?;
    Ok(Var::string(path))
  }
}

// --- FS.Remove, FS.RemoveAll ---

pub struct Remove;
impl DiskOp for Remove {
  type Compiled = ();
  type Owned = ();
  const DESC: ShardDesc = desc(
    "FS.Remove",
    shard_doc!(
      "Removes the file or empty directory at the input path; outputs whether there was one."
    ),
    shard_doc!(
      "A missing path outputs `false`. A directory that is not empty is an error (use `FS.RemoveAll`). A symbolic link is removed, not what it points to."
    ),
    &[],
    OutputDesc::Fixed(TypeName::Bool),
  );

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::bool(),
    })
  }

  fn owned(_: &(), _: &impl LeafCtx) -> Result<()> {
    Ok(())
  }

  fn run(
    access: &FsAccess,
    path: &str,
    _: (),
    _: &CancellationToken,
  ) -> std::result::Result<Var, String> {
    let name = Self::DESC.name;
    let resolved = access
      .resolve(Path::new(path), true)
      .map_err(|e| at(name, path, e))?;
    let meta = match fs::symlink_metadata(&resolved) {
      Ok(meta) => meta,
      Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Var::Bool(false)),
      Err(e) => return Err(at(name, path, e)),
    };
    if meta.is_dir() {
      fs::remove_dir(&resolved)
    } else {
      fs::remove_file(&resolved)
    }
    .map_err(|e| at(name, path, e))?;
    Ok(Var::Bool(true))
  }
}

/// Removes `dir` and everything below it, links not followed; counts what
/// it removed.
fn remove_tree(dir: &Path, cancel: &CancellationToken) -> io::Result<i64> {
  let mut removed = 0;
  for entry in fs::read_dir(dir)? {
    if cancel.is_cancelled() {
      return Err(io::Error::new(ErrorKind::Interrupted, "cancelled"));
    }
    let entry = entry?;
    if entry.file_type()?.is_dir() {
      removed += remove_tree(&entry.path(), cancel)?;
    } else {
      fs::remove_file(entry.path())?;
      removed += 1;
    }
  }
  fs::remove_dir(dir)?;
  Ok(removed + 1)
}

pub struct RemoveAll;
impl DiskOp for RemoveAll {
  type Compiled = ();
  type Owned = ();
  const DESC: ShardDesc = desc(
    "FS.RemoveAll",
    shard_doc!(
      "Removes the file or directory at the input path with everything in it; outputs how many entries were removed."
    ),
    shard_doc!(
      "A missing path outputs 0. Symbolic links are removed, never followed. Cancelling stops between entries; what was removed stays removed."
    ),
    &[],
    OutputDesc::Fixed(TypeName::Int),
  );

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::int(),
    })
  }

  fn owned(_: &(), _: &impl LeafCtx) -> Result<()> {
    Ok(())
  }

  fn run(
    access: &FsAccess,
    path: &str,
    _: (),
    cancel: &CancellationToken,
  ) -> std::result::Result<Var, String> {
    let name = Self::DESC.name;
    let resolved = access
      .resolve(Path::new(path), true)
      .map_err(|e| at(name, path, e))?;
    let removed = match fs::symlink_metadata(&resolved) {
      Err(e) if e.kind() == ErrorKind::NotFound => Ok(0),
      Err(e) => Err(e),
      Ok(meta) if meta.is_dir() => remove_tree(&resolved, cancel),
      Ok(_) => fs::remove_file(&resolved).map(|()| 1),
    }
    .map_err(|e| at(name, path, e))?;
    Ok(Var::Int(removed))
  }
}

// --- FS.Copy ---

static COPY_PARAMS: &[ParamDecl] = &[
  ParamDecl::new(
    "destination",
    shard_doc!(
      "Where to copy to: a String literal or variable, the new file's or directory's path."
    ),
    OPERAND,
    STRING,
    Requirement::Required,
  ),
  flag(
    "overwrite",
    shard_doc!(
      "Replace existing files at the destination; without it, an existing destination is an error."
    ),
  ),
];

pub struct CopyCompiled {
  destination: Operand,
  overwrite: bool,
}

/// Copies the directory `from` to `to`, recursively.
fn copy_tree(
  access: &FsAccess,
  from: &Path,
  to: &Path,
  overwrite: bool,
  cancel: &CancellationToken,
) -> std::result::Result<(), (PathBuf, io::Error)> {
  match fs::create_dir(to) {
    Ok(()) => {}
    Err(e) if e.kind() == ErrorKind::AlreadyExists && overwrite && to.is_dir() => {}
    Err(e) => return Err((to.to_path_buf(), e)),
  }
  for entry in fs::read_dir(from).map_err(|e| (from.to_path_buf(), e))? {
    if cancel.is_cancelled() {
      return Err((
        from.to_path_buf(),
        io::Error::new(ErrorKind::Interrupted, "cancelled"),
      ));
    }
    let entry = entry.map_err(|e| (from.to_path_buf(), e))?;
    let source = entry.path();
    let target = to.join(entry.file_name());
    let kind = entry.file_type().map_err(|e| (source.clone(), e))?;
    if kind.is_dir() {
      copy_tree(access, &source, &target, overwrite, cancel)?;
      continue;
    }
    if kind.is_symlink() {
      // Copied as what it points to, which must be allowed and a file.
      access
        .resolve(&source, false)
        .map_err(|e| (source.clone(), e))?;
      if source.is_dir() {
        return Err((
          source,
          io::Error::other("a symbolic link to a directory is not copied"),
        ));
      }
    }
    copy_file(&source, &target, overwrite)?;
  }
  Ok(())
}

fn copy_file(
  from: &Path,
  to: &Path,
  overwrite: bool,
) -> std::result::Result<(), (PathBuf, io::Error)> {
  if !overwrite && fs::symlink_metadata(to).is_ok() {
    return Err((to.to_path_buf(), exists_error()));
  }
  // Opened first, so a failure to read is reported at the source and any
  // other at the destination.
  fs::File::open(from).map_err(|e| (from.to_path_buf(), e))?;
  fs::copy(from, to).map_err(|e| (to.to_path_buf(), e))?;
  Ok(())
}

pub struct Copy;
impl DiskOp for Copy {
  type Compiled = CopyCompiled;
  type Owned = (Arc<str>, bool);
  const DESC: ShardDesc = desc(
    "FS.Copy",
    shard_doc!(
      "Copies the file or directory at the input path to `destination`, and outputs the input path."
    ),
    shard_doc!(
      "A directory is copied with everything in it. An existing destination is an error unless `overwrite: true`, which replaces files and copies into an existing directory. A symbolic link inside a copied directory is copied as the file it points to; one pointing to a directory is an error. Cancelling stops between files; what was copied stays."
    ),
    COPY_PARAMS,
    OutputDesc::Passthrough,
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<CopyCompiled>> {
    let (destination, _) = Operand::compose_arg(args, "destination", Self::DESC.name, ctx)?;
    Ok(Composed {
      compiled: CopyCompiled {
        destination,
        overwrite: args.bool("overwrite").expect("decoded overwrite"),
      },
      output: Type::string(),
    })
  }

  fn owned(c: &CopyCompiled, ctx: &impl LeafCtx) -> Result<(Arc<str>, bool)> {
    Ok((string_operand(&c.destination, ctx), c.overwrite))
  }

  fn run(
    access: &FsAccess,
    path: &str,
    (destination, overwrite): (Arc<str>, bool),
    cancel: &CancellationToken,
  ) -> std::result::Result<Var, String> {
    let name = Self::DESC.name;
    let source = access
      .resolve(Path::new(path), false)
      .map_err(|e| at(name, path, e))?;
    let target = access
      .resolve(Path::new(&*destination), true)
      .map_err(|e| at(name, &destination, e))?;
    let meta = fs::metadata(&source).map_err(|e| at(name, path, e))?;
    let copied = if meta.is_dir() {
      let inside = std::path::absolute(&target)
        .and_then(|t| access::canonical_prefix(&access::normalize(&t)))
        .and_then(|t| Ok(t.starts_with(fs::canonicalize(&source)?)))
        .map_err(|e| at(name, &destination, e))?;
      if inside {
        return Err(at(name, &destination, "inside the directory being copied"));
      }
      copy_tree(access, &source, &target, overwrite, cancel)
    } else {
      copy_file(&source, &target, overwrite)
    };
    copied.map_err(|(at_path, e)| {
      // Name the path the script gave where the failure is one of its own.
      let shown = if at_path == target {
        destination.to_string()
      } else if at_path == source {
        path.to_string()
      } else {
        at_path.to_string_lossy().into_owned()
      };
      at(name, &shown, e)
    })?;
    Ok(Var::string(path))
  }
}

/// A String operand's value (declared String, so a variable was checked at
/// compose).
fn string_operand(op: &Operand, ctx: &impl LeafCtx) -> Arc<str> {
  match &op.get(ctx) {
    Var::String(s) => Arc::clone(s),
    other => Arc::from(other.to_string()),
  }
}

// --- FS.Rename ---

static RENAME_PARAMS: &[ParamDecl] = &[ParamDecl::new(
  "new-name",
  shard_doc!("The new path (not only a name): a String literal or variable."),
  OPERAND,
  STRING,
  Requirement::Required,
)];

pub struct Rename;
impl DiskOp for Rename {
  type Compiled = Operand;
  type Owned = Arc<str>;
  const DESC: ShardDesc = desc(
    "FS.Rename",
    shard_doc!(
      "Moves the file or directory at the input path to `new-name`, and outputs the new path."
    ),
    shard_doc!(
      "An existing `new-name` is an error. Moving across filesystems fails with the OS error: copy and remove instead."
    ),
    RENAME_PARAMS,
    OutputDesc::Fixed(TypeName::String),
  );

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<Operand>> {
    let (new_name, _) = Operand::compose_arg(args, "new-name", Self::DESC.name, ctx)?;
    Ok(Composed {
      compiled: new_name,
      output: Type::string(),
    })
  }

  fn owned(new_name: &Operand, ctx: &impl LeafCtx) -> Result<Arc<str>> {
    Ok(string_operand(new_name, ctx))
  }

  fn run(
    access: &FsAccess,
    path: &str,
    new_name: Arc<str>,
    _: &CancellationToken,
  ) -> std::result::Result<Var, String> {
    let name = Self::DESC.name;
    let from = access
      .resolve(Path::new(path), true)
      .map_err(|e| at(name, path, e))?;
    let to = access
      .resolve(Path::new(&*new_name), true)
      .map_err(|e| at(name, &new_name, e))?;
    fs::symlink_metadata(&from).map_err(|e| at(name, path, e))?;
    if fs::symlink_metadata(&to).is_ok() {
      return Err(at(name, &new_name, exists_error()));
    }
    fs::rename(&from, &to).map_err(|e| at(name, path, e))?;
    Ok(Var::from_string(new_name))
  }
}

pub static READ: ShardType = async_type::<Disk<Read>>();
pub static WRITE: ShardType = async_type::<Disk<Write>>();
pub static EXISTS: ShardType = async_type::<Disk<Exists>>();
pub static IS_FILE: ShardType = async_type::<Disk<IsFile>>();
pub static IS_DIRECTORY: ShardType = async_type::<Disk<IsDirectory>>();
pub static ITERATE: ShardType = async_type::<Disk<Iterate>>();
pub static CREATE_DIRECTORIES: ShardType = async_type::<Disk<CreateDirectories>>();
pub static REMOVE: ShardType = async_type::<Disk<Remove>>();
pub static REMOVE_ALL: ShardType = async_type::<Disk<RemoveAll>>();
pub static COPY: ShardType = async_type::<Disk<Copy>>();
pub static RENAME: ShardType = async_type::<Disk<Rename>>();
