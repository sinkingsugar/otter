//! Where `@include` and `@read` find files. Loading reads them, once, before
//! anything composes: compose itself still reads nothing but its inputs,
//! and the program records every file it read (`Program::files`), which is
//! what a watcher follows.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// A file a script named.
pub struct File {
  /// How diagnostics and reloads name it.
  pub name: String,
  /// Its identity: a file included under two names is included once.
  pub key: String,
  pub bytes: Vec<u8>,
}

/// The files a program may include or read. A path is resolved against the
/// directory of the file that names it, then against the reader's own
/// search paths.
pub trait Files {
  /// The file `path` names, written in the file named `from`. Every name
  /// it looks at goes to `looked`, in order, found or not: a watcher
  /// observes them all, so creating a missing one (or one searched before
  /// the one found) is a change.
  fn resolve(&self, from: &str, path: &str, looked: &mut Vec<String>) -> Result<File, String>;

  /// The current contents of a file by the name `resolve` gave, `None`
  /// when it cannot be read: what a watcher compares with what a load
  /// read.
  fn contents(&self, name: &str) -> Option<Vec<u8>>;

  /// `resolve`, without the names looked at.
  fn read(&self, from: &str, path: &str) -> Result<File, String> {
    self.resolve(from, path, &mut Vec::new())
  }

  /// The identity of a file loaded by name (the program's first file).
  fn key(&self, name: &str) -> String {
    name.to_string()
  }
}

/// The filesystem, with include paths searched after the including file's
/// directory (the CLI's `-I`).
#[derive(Default)]
pub struct FsFiles {
  pub include_paths: Vec<PathBuf>,
}

impl Files for FsFiles {
  fn resolve(&self, from: &str, path: &str, looked: &mut Vec<String>) -> Result<File, String> {
    let path = Path::new(path);
    let mut candidates = Vec::new();
    if path.is_absolute() {
      candidates.push(path.to_path_buf());
    } else {
      candidates.push(directory(from).join(path));
      candidates.extend(self.include_paths.iter().map(|dir| dir.join(path)));
    }
    for candidate in &candidates {
      looked.push(candidate.to_string_lossy().into_owned());
      match std::fs::read(candidate) {
        Ok(bytes) => {
          let name = candidate.to_string_lossy().into_owned();
          return Ok(File {
            key: self.key(&name),
            name,
            bytes,
          });
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("{}: {err}", candidate.display())),
      }
    }
    let looked: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    Err(format!(
      "{} not found (looked for {})",
      path.display(),
      looked.join(", ")
    ))
  }

  fn contents(&self, name: &str) -> Option<Vec<u8>> {
    std::fs::read(name).ok()
  }

  fn key(&self, name: &str) -> String {
    match std::fs::canonicalize(name) {
      Ok(path) => path.to_string_lossy().into_owned(),
      Err(_) => name.to_string(),
    }
  }
}

/// Files held in memory, by name: for a host without a filesystem (a
/// device carries its scripts in flash), and for tests. Names are paths:
/// `lib/a.shs` includes `b.shs` as `lib/b.shs`.
#[derive(Default)]
pub struct MemoryFiles {
  files: HashMap<String, Vec<u8>>,
}

impl MemoryFiles {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn insert(&mut self, name: &str, bytes: impl Into<Vec<u8>>) -> &mut Self {
    self.files.insert(normalize(Path::new(name)), bytes.into());
    self
  }
}

impl Files for MemoryFiles {
  fn resolve(&self, from: &str, path: &str, looked: &mut Vec<String>) -> Result<File, String> {
    let name = normalize(&directory(from).join(path));
    looked.push(name.clone());
    match self.files.get(&name) {
      Some(bytes) => Ok(File {
        key: name.clone(),
        name,
        bytes: bytes.clone(),
      }),
      None => Err(format!("{name} not found")),
    }
  }

  fn contents(&self, name: &str) -> Option<Vec<u8>> {
    self.files.get(name).cloned()
  }

  fn key(&self, name: &str) -> String {
    normalize(Path::new(name))
  }
}

fn directory(file: &str) -> &Path {
  Path::new(file).parent().unwrap_or(Path::new(""))
}

/// A path without `.` and with `..` applied, written with `/`.
fn normalize(path: &Path) -> String {
  let mut parts: Vec<String> = Vec::new();
  for component in path.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir if parts.last().is_some_and(|p| p != "..") => {
        parts.pop();
      }
      other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
    }
  }
  parts.join("/")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn memory_files_resolve_against_the_including_files_directory() {
    let mut files = MemoryFiles::new();
    files.insert("lib/a.shs", "a").insert("lib/b.shs", "b");
    assert_eq!(files.read("lib/a.shs", "b.shs").unwrap().name, "lib/b.shs");
    assert_eq!(
      files.read("main.shs", "lib/./x/../a.shs").unwrap().key,
      "lib/a.shs"
    );
    assert!(files.read("main.shs", "b.shs").is_err());
  }
}
