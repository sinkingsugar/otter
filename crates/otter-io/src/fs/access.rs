//! What the `FS.*` shards may touch, and what a relative path means: the
//! [`FsAccess`] service (docs/embedding.md §3, "Files while running").
//!
//! A host inserts one into a mesh's services (`mesh.services_mut()`,
//! `Session::set_services`, a tool call's `CallOptions::services`). Without
//! one, the shards use [`FsAccess::unrestricted`]: any path, relative paths
//! against the process's working directory.
//!
//! [`FsAccess::within`] confines every operation to one directory, the
//! working directory of the scripts it serves. The check runs at activation
//! on the blocking pool, never in compose: the path is made absolute against
//! the root, `.` and `..` are removed lexically, the longest existing prefix
//! is resolved through the filesystem (symbolic links included), and the
//! result must lie under the root. A path whose last component is a
//! symbolic link must also lead under the root. The operation then runs on
//! the resolved path, so what was checked is what is touched. What remains
//! is the race every path check has: a directory swapped for a link between
//! the check and the operation. A host that runs untrusted scripts next to
//! untrusted writers of the same tree needs an OS sandbox as well.

use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use otter_core::service::Services;

/// Which paths the `FS.*` shards may use, and how relative paths resolve.
/// A service: the host inserts it; shards read it at activation.
#[derive(Clone, Debug)]
pub struct FsAccess {
  scope: Scope,
  read_only: bool,
}

#[derive(Clone, Debug)]
enum Scope {
  Unrestricted,
  /// Canonical: relative paths resolve against it, and only paths under it
  /// are allowed.
  Within(PathBuf),
  Denied,
}

impl FsAccess {
  /// Any path; relative paths resolve against the process's working
  /// directory. What the shards use when the host inserts no `FsAccess`.
  pub fn unrestricted() -> FsAccess {
    FsAccess {
      scope: Scope::Unrestricted,
      read_only: false,
    }
  }

  /// Only paths under `root` (an existing directory, resolved here);
  /// relative paths resolve against it.
  pub fn within(root: impl AsRef<Path>) -> io::Result<FsAccess> {
    let root = std::fs::canonicalize(root)?;
    if !root.is_dir() {
      return Err(io::Error::new(
        ErrorKind::NotADirectory,
        format!("{} is not a directory", root.display()),
      ));
    }
    Ok(FsAccess {
      scope: Scope::Within(root),
      read_only: false,
    })
  }

  /// No file access: every `FS.*` shard that touches the disk fails.
  pub fn denied() -> FsAccess {
    FsAccess {
      scope: Scope::Denied,
      read_only: false,
    }
  }

  /// The same access, without writing: writing, creating, removing, copying
  /// to and renaming fail.
  pub fn read_only(mut self) -> FsAccess {
    self.read_only = true;
    self
  }

  /// The directory paths are confined to, for [`FsAccess::within`].
  pub fn root(&self) -> Option<&Path> {
    match &self.scope {
      Scope::Within(root) => Some(root),
      _ => None,
    }
  }

  pub fn is_read_only(&self) -> bool {
    self.read_only
  }

  /// The path an operation on `path` uses, or why it may not (an error of
  /// kind `PermissionDenied`, or the OS error met while resolving).
  /// `write`: the operation changes the filesystem at `path`. Touches the
  /// filesystem for a confined access: call it off the engine thread.
  pub fn resolve(&self, path: &Path, write: bool) -> io::Result<PathBuf> {
    if write && self.read_only {
      return Err(denied("writing is not allowed"));
    }
    match &self.scope {
      Scope::Unrestricted => Ok(path.to_path_buf()),
      Scope::Denied => Err(denied("file access is denied")),
      Scope::Within(root) => {
        let resolved = resolve_within(root, path)?;
        // The root holds everything allowed: removing or replacing it
        // (`"." | FS.RemoveAll`) would take the whole scope.
        if write && resolved == *root {
          return Err(denied("the allowed directory itself cannot be changed"));
        }
        Ok(resolved)
      }
    }
  }

  /// The mesh's access, or the unrestricted default.
  pub(crate) fn of(services: &Services) -> Arc<FsAccess> {
    static DEFAULT: OnceLock<Arc<FsAccess>> = OnceLock::new();
    services.get_arc::<FsAccess>().unwrap_or_else(|| {
      DEFAULT
        .get_or_init(|| Arc::new(FsAccess::unrestricted()))
        .clone()
    })
  }
}

fn denied(message: &str) -> io::Error {
  io::Error::new(ErrorKind::PermissionDenied, message.to_string())
}

fn resolve_within(root: &Path, path: &Path) -> io::Result<PathBuf> {
  // An absolute `path` replaces the root here.
  let normal = normalize(&root.join(path));
  let (parent, name) = match (normal.parent(), normal.file_name()) {
    (Some(parent), Some(name)) => (parent, Some(name)),
    _ => (normal.as_path(), None),
  };
  let mut resolved = canonical_prefix(parent)?;
  if let Some(name) = name {
    resolved.push(name);
  }
  if !resolved.starts_with(root) {
    return Err(denied("outside the allowed directory"));
  }
  // A last component that is a symbolic link must lead inside too, since
  // most operations follow it.
  if std::fs::symlink_metadata(&resolved).is_ok_and(|m| m.file_type().is_symlink()) {
    let target = std::fs::read_link(&resolved)?;
    let from = resolved.parent().unwrap_or(root);
    // Where the OS will go: resolved through the filesystem, never
    // lexically, since `..` after a link in the target leaves that
    // link's directory (`s/../x` with `s` pointing outside). What does
    // not exist yet resolves through its existing prefix; a missing
    // component followed by `..` is an error, and the OS reports a loop.
    let lands = canonical_prefix(&from.join(target))?;
    if !lands.starts_with(root) {
      return Err(denied(
        "a symbolic link leads outside the allowed directory",
      ));
    }
  }
  Ok(resolved)
}

/// `path` without `.` components, and with each `..` removing the component
/// before it (never the root).
pub(crate) fn normalize(path: &Path) -> PathBuf {
  let mut out = PathBuf::new();
  for component in path.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        if matches!(out.components().next_back(), Some(Component::Normal(_))) {
          out.pop();
        } else if !out.has_root() {
          out.push("..");
        }
      }
      other => out.push(other),
    }
  }
  out
}

/// `path` (absolute, normalized) with its longest existing prefix resolved
/// through the filesystem and the missing rest appended.
pub(crate) fn canonical_prefix(path: &Path) -> io::Result<PathBuf> {
  let mut missing = Vec::new();
  let mut current = path;
  loop {
    match std::fs::canonicalize(current) {
      Ok(mut resolved) => {
        resolved.extend(missing.iter().rev());
        return Ok(resolved);
      }
      Err(e) if e.kind() == ErrorKind::NotFound => match (current.parent(), current.file_name()) {
        (Some(parent), Some(name)) => {
          missing.push(name);
          current = parent;
        }
        _ => return Err(e),
      },
      Err(e) => return Err(e),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::normalize;
  use std::path::Path;

  #[test]
  fn normalize_removes_dots_lexically() {
    for (path, normal) in [
      ("/a/./b/../c", "/a/c"),
      ("/a/../../b", "/b"),
      ("/..", "/"),
      ("a/../../b", "../b"),
      ("./a//b/", "a/b"),
    ] {
      assert_eq!(normalize(Path::new(path)), Path::new(normal), "{path}");
    }
  }
}
