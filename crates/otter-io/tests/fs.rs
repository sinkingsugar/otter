//! The `FS.*` shards, through Shards scripts: reads and writes, probes,
//! listings, directories, copies and renames, the path helpers, the errors
//! scripts see, and the host's access policy (`FsAccess`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use otter_core::service::Services;
use otter_core::{Catalog, Error, Outcome, Var};
use otter_io::fs::FsAccess;
use otter_lang::{CallOptions, MemoryFiles, Program, Source, Tool, ToolError, ToolInput};

fn catalog() -> Catalog {
  Catalog::new(&[otter_core::shards::CATALOG, otter_io::CATALOG]).unwrap()
}

/// A fresh directory under the system's temporary directory, removed with
/// everything in it when dropped.
struct TempDir(PathBuf);

impl TempDir {
  fn new(test: &str) -> TempDir {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
      "otter-fs-{test}-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    // Canonical, so paths compare equal with what the shards resolve
    // (`/tmp` is a link on macOS).
    TempDir(std::fs::canonicalize(path).unwrap())
  }

  fn path(&self) -> &Path {
    &self.0
  }

  fn str(&self) -> &str {
    self.0.to_str().unwrap()
  }

  fn join(&self, rest: &str) -> String {
    self.0.join(rest).to_str().unwrap().to_string()
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.0);
  }
}

/// Runs `text` with `@dir` defined as `dir`: the root wire's outcome.
fn run(text: &str, dir: &TempDir) -> Outcome {
  let defines = HashMap::from([("dir".to_string(), dir.str().to_string())]);
  let program = match Program::load(Source::new("t.shs", text), &catalog(), &defines) {
    Ok(program) => program,
    Err((_, d)) => panic!("load failed: {d:?}"),
  };
  let report = program
    .run()
    .unwrap_or_else(|d| panic!("run failed: {d:?}"));
  report.outcomes.into_iter().next().unwrap().1.unwrap()
}

fn completed(outcome: Outcome) -> Var {
  match outcome {
    Outcome::Completed(value) => value,
    other => panic!("{other:?}"),
  }
}

fn failed(outcome: Outcome) -> String {
  match outcome {
    Outcome::Failed(Error::Activation(message)) => message,
    other => panic!("{other:?}"),
  }
}

fn field<'a>(value: &'a Var, key: &str) -> &'a Var {
  value
    .as_table()
    .and_then(|t| t.get(key))
    .unwrap_or_else(|| panic!("no {key} in {value}"))
}

fn strings(values: &[&str]) -> Var {
  Var::seq(values.iter().map(|s| Var::string(s)).collect())
}

#[test]
fn write_append_read_and_probe() {
  let dir = TempDir::new("rw");
  let result = completed(run(
    r#"
    [@dir "notes.txt"] | FS.Join = file
    file | FS.Write(contents: "hello") | FS.Write(contents: " world" append: true)
    file | FS.Read = text
    file | FS.Read(bytes: true) = raw
    [@dir "copy.bin"] | FS.Join | FS.Write(contents: raw) | FS.Read = copied
    file | FS.Write(contents: "replaced" overwrite: true) | FS.Read = replaced
    [@dir "missing"] | FS.Join = missing
    {
      text: text raw: raw copied: copied replaced: replaced
      exists: (file | FS.Exists) is-file: (file | FS.IsFile) is-dir: (file | FS.IsDirectory)
      dir-is-dir: (@dir | FS.IsDirectory) dir-is-file: (@dir | FS.IsFile)
      missing: (missing | FS.Exists) missing-file: (missing | FS.IsFile)
      through-file: ([file "x"] | FS.Join | FS.Exists)
    }
    "#,
    &dir,
  ));
  assert_eq!(field(&result, "text"), &Var::string("hello world"));
  assert_eq!(field(&result, "raw"), &Var::bytes(b"hello world"));
  assert_eq!(field(&result, "copied"), &Var::string("hello world"));
  assert_eq!(field(&result, "replaced"), &Var::string("replaced"));
  for (key, expected) in [
    ("exists", true),
    ("is-file", true),
    ("is-dir", false),
    ("dir-is-dir", true),
    ("dir-is-file", false),
    ("missing", false),
    ("missing-file", false),
    ("through-file", false),
  ] {
    assert_eq!(field(&result, key), &Var::Bool(expected), "{key}");
  }
  assert_eq!(
    std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
    "replaced"
  );
}

#[test]
fn errors_name_the_shard_the_path_and_the_os_reason() {
  let dir = TempDir::new("errors");
  let cases = [
    // A missing file.
    (
      r#"[@dir "nope.txt"] | FS.Join | FS.Read"#,
      format!("FS.Read: {}: ", dir.join("nope.txt")),
      "os error",
    ),
    // Writing into a directory that does not exist.
    (
      r#"[@dir "no-dir" "x.txt"] | FS.Join | FS.Write(contents: "x")"#,
      format!("FS.Write: {}: ", dir.join("no-dir/x.txt")),
      "os error",
    ),
    // Writing over a file without `overwrite`.
    (
      r#"[@dir "a.txt"] | FS.Join | FS.Write(contents: "1") | FS.Write(contents: "2")"#,
      format!("FS.Write: {}: ", dir.join("a.txt")),
      "os error",
    ),
    // Text that is not UTF-8.
    (
      r#"[@dir "b.bin"] | FS.Join | FS.Write(contents: @read("bad.bin" bytes: true)) | FS.Read"#,
      format!("FS.Read: {}: ", dir.join("b.bin")),
      "not valid UTF-8",
    ),
    // Listing a file.
    (
      r#"[@dir "c.txt"] | FS.Join | FS.Write(contents: "c") | FS.Iterate"#,
      format!("FS.Iterate: {}: ", dir.join("c.txt")),
      "os error",
    ),
    // Removing a directory that is not empty.
    (
      r#"[@dir "full"] | FS.Join | FS.CreateDirectories = full
      [full "f"] | FS.Join | FS.Write(contents: "f")
      full | FS.Remove"#,
      format!("FS.Remove: {}: ", dir.join("full")),
      "os error",
    ),
    // Renaming onto an existing path.
    (
      r#"[@dir "d1"] | FS.Join | FS.Write(contents: "1") = d1
      [@dir "d2"] | FS.Join | FS.Write(contents: "2") = d2
      d1 | FS.Rename(new-name: d2)"#,
      format!("FS.Rename: {}: ", dir.join("d2")),
      "already exists",
    ),
    // Copying a missing file.
    (
      r#"[@dir "gone"] | FS.Join | FS.Copy(destination: "unused")"#,
      format!("FS.Copy: {}: ", dir.join("gone")),
      "os error",
    ),
    // Copying into a missing directory names the destination.
    (
      r#"[@dir "src.txt"] | FS.Join | FS.Write(contents: "s")
      | FS.Copy(destination: "/nonexistent-otter-dir/x.txt")"#,
      "FS.Copy: /nonexistent-otter-dir/x.txt: ".to_string(),
      "os error",
    ),
    // Copying onto an existing file without `overwrite`.
    (
      r#"[@dir "e1"] | FS.Join | FS.Write(contents: "1") = e1
      [@dir "e2"] | FS.Join | FS.Write(contents: "2") = e2
      e1 | FS.Copy(destination: e2)"#,
      format!("FS.Copy: {}: ", dir.join("e2")),
      "already exists",
    ),
    // Copying a directory into itself.
    (
      r#"[@dir "self"] | FS.Join | FS.CreateDirectories = here
      [here "inner"] | FS.Join = inner
      here | FS.Copy(destination: inner)"#,
      format!("FS.Copy: {}: ", dir.join("self/inner")),
      "inside the directory being copied",
    ),
  ];
  std::fs::write(dir.path().join("bad.bin"), [0xff, 0xfe]).unwrap();
  for (script, prefix, reason) in cases {
    // `@read` is relative to the script, which is in the directory.
    let source = Source::new(dir.join("t.shs"), script);
    let defines = HashMap::from([("dir".to_string(), dir.str().to_string())]);
    let program = Program::load(source, &catalog(), &defines)
      .unwrap_or_else(|(_, d)| panic!("load failed: {d:?}"));
    let report = program.run().unwrap();
    let message = failed(report.outcomes.into_iter().next().unwrap().1.unwrap());
    assert!(
      message.starts_with(&prefix) && message.contains(reason),
      "{script}: {message}"
    );
  }
}

#[test]
fn directories_listings_copies_and_renames() {
  let dir = TempDir::new("dirs");
  let result = completed(run(
    r#"
    [@dir "tree" "sub" "deeper"] | FS.Join | FS.CreateDirectories | FS.CreateDirectories
    [@dir "tree"] | FS.Join = tree
    [tree "a.txt"] | FS.Join | FS.Write(contents: "a")
    [tree "sub" "b.txt"] | FS.Join | FS.Write(contents: "b")
    [tree "sub" "deeper" ".hidden"] | FS.Join | FS.Write(contents: "h")
    tree | FS.Iterate = flat
    tree | FS.Iterate(recursive: true) = deep

    [@dir "copy"] | FS.Join = copy
    tree | FS.Copy(destination: copy) = copied-from
    copy | FS.Iterate(recursive: true) | Count = copy-count
    [copy "sub" "b.txt"] | FS.Join | FS.Read = copied-b
    [copy "a.txt"] | FS.Join = copy-a
    copy-a | FS.Write(contents: "changed" overwrite: true)
    [tree "a.txt"] | FS.Join | FS.Copy(destination: copy-a overwrite: true)
    copy-a | FS.Read = recopied

    [@dir "moved"] | FS.Join = moved
    copy | FS.Rename(new-name: moved) = renamed
    copy | FS.Exists = copy-left
    [moved "a.txt"] | FS.Join = moved-a
    moved-a | FS.Remove = removed-file
    moved-a | FS.Remove = removed-again
    moved | FS.RemoveAll = removed-all
    moved | FS.RemoveAll = removed-none
    moved | FS.Exists = moved-left
    {
      flat: flat deep: deep copied-from: copied-from copy-count: copy-count
      copied-b: copied-b recopied: recopied renamed: renamed copy-left: copy-left
      removed-file: removed-file removed-again: removed-again
      removed-all: removed-all removed-none: removed-none moved-left: moved-left
    }
    "#,
    &dir,
  ));
  let tree = dir.join("tree");
  let expect = [
    (
      "flat",
      strings(&[&format!("{tree}/a.txt"), &format!("{tree}/sub")]),
    ),
    (
      "deep",
      strings(&[
        &format!("{tree}/a.txt"),
        &format!("{tree}/sub"),
        &format!("{tree}/sub/b.txt"),
        &format!("{tree}/sub/deeper"),
        &format!("{tree}/sub/deeper/.hidden"),
      ]),
    ),
    ("copied-from", Var::string(&tree)),
    ("copy-count", Var::Int(5)),
    ("copied-b", Var::string("b")),
    ("recopied", Var::string("a")),
    ("renamed", Var::string(&dir.join("moved"))),
    ("copy-left", Var::Bool(false)),
    ("removed-file", Var::Bool(true)),
    ("removed-again", Var::Bool(false)),
    // sub, sub/b.txt, sub/deeper, sub/deeper/.hidden and moved itself.
    ("removed-all", Var::Int(5)),
    ("removed-none", Var::Int(0)),
    ("moved-left", Var::Bool(false)),
  ];
  for (key, value) in expect {
    assert_eq!(field(&result, key), &value, "{key}");
  }
  // The source tree is untouched.
  assert_eq!(
    std::fs::read_to_string(dir.path().join("tree/sub/deeper/.hidden")).unwrap(),
    "h"
  );
}

#[test]
fn path_helpers_need_no_disk() {
  let dir = TempDir::new("paths");
  let result = completed(run(
    r#"
    ["a" "b" "c.txt"] | FS.Join = join
    ["a" "/b" "c"] | FS.Join = join-absolute
    "dir/archive.tar.gz" | FS.Extension = ext
    "dir/README" | FS.Extension = no-ext
    "dir/.profile" | FS.Extension = dotfile
    "dir/archive.tar.gz" | FS.Filename = name
    "dir/archive.tar.gz" | FS.Filename(no-extension: true) = stem
    "dir/sub/file.txt" | FS.Parent = parent
    "file.txt" | FS.Parent = no-parent
    {
      join: join join-absolute: join-absolute ext: ext no-ext: no-ext dotfile: dotfile
      name: name stem: stem parent: parent no-parent: no-parent
    }
    "#,
    &dir,
  ));
  for (key, value) in [
    ("join", "a/b/c.txt"),
    ("join-absolute", "/b/c"),
    ("ext", ".gz"),
    ("no-ext", ""),
    ("dotfile", ""),
    ("name", "archive.tar.gz"),
    ("stem", "archive.tar"),
    ("parent", "dir/sub"),
    ("no-parent", ""),
  ] {
    assert_eq!(field(&result, key), &Var::string(value), "{key}");
  }
}

/// A tool whose function runs `body` (which outputs a String), called with
/// `access` as the mesh's file access: the result, or the failure message.
fn call_tool(body: &str, access: Option<FsAccess>) -> Result<String, String> {
  let text = format!(
    r#"@fn(Main input: {{input: {{}} env: {{}} working-dir: String}} output: String params: {{}} {{
      {body}
    }})
    {{definition: {{name: "fs" description: "files" parameters: {{type: "object"}}}} use: "Main"}}"#
  );
  let tool = Tool::load_with(
    Source::new("tool.shs", text),
    &catalog(),
    &HashMap::new(),
    &MemoryFiles::default(),
  )
  .unwrap_or_else(|e| panic!("{e}"));
  let mut services = Services::default();
  if let Some(access) = access {
    services.insert(access);
  }
  let options = CallOptions {
    services,
    ..Default::default()
  };
  let input = ToolInput::new(Var::table(Vec::<(&str, Var)>::new()));
  match tool.call_with(&input, &options) {
    Ok(out) => Ok(out.text),
    Err(ToolError::Failed { message, .. }) => Err(message),
    Err(e) => panic!("{e}"),
  }
}

#[test]
fn a_host_confines_scripts_to_a_directory() {
  let work = TempDir::new("within");
  let other = TempDir::new("outside");
  std::fs::write(other.path().join("secret.txt"), "secret").unwrap();
  let within = || Some(FsAccess::within(work.path()).unwrap());

  // Relative paths resolve against the root, wherever the process runs.
  assert_eq!(
    call_tool(
      r#""notes/a.txt" | FS.Parent | FS.CreateDirectories
      "notes/a.txt" | FS.Write(contents: "inside") | FS.Read"#,
      within()
    ),
    Ok("inside".to_string())
  );
  assert_eq!(
    std::fs::read_to_string(work.path().join("notes/a.txt")).unwrap(),
    "inside"
  );
  // `..` that stays inside is fine; a listing gives paths as written.
  assert_eq!(
    call_tool(
      r#""notes/../notes/./a.txt" | FS.Read = text
      "notes" | FS.Iterate | String.Join(separator: ",") = names
      [text names] | String.Join(separator: " ")"#,
      within()
    ),
    Ok("inside notes/a.txt".to_string())
  );

  // Leaving the root, by `..`, by an absolute path or through a link, is
  // denied, for reads, probes and writes alike.
  std::fs::write(work.path().join("a.txt"), "a").unwrap();
  let other_name = other.path().file_name().unwrap().to_str().unwrap();
  let secret = other.join("secret.txt");
  let escapes = [
    format!(r#""../{other_name}/secret.txt" | FS.Read"#),
    format!(r#""{secret}" | FS.Read"#),
    format!(r#""{secret}" | FS.Exists | ToString"#),
    format!(r#""{}" | FS.Write(contents: "x")"#, other.join("new.txt")),
    format!(
      r#""a.txt" | FS.Copy(destination: "{}")"#,
      other.join("copy.txt")
    ),
  ];
  for script in &escapes {
    let message = call_tool(script, within()).unwrap_err();
    assert!(
      message.ends_with("outside the allowed directory"),
      "{script}: {message}"
    );
  }
  #[cfg(unix)]
  {
    std::os::unix::fs::symlink(other.path(), work.path().join("link")).unwrap();
    std::os::unix::fs::symlink(&secret, work.path().join("secret-link")).unwrap();
    for script in [
      r#""link/secret.txt" | FS.Read"#,
      r#""link" | FS.Iterate | String.Join(separator: ",")"#,
      r#""secret-link" | FS.Read"#,
    ] {
      let message = call_tool(script, within()).unwrap_err();
      assert!(
        message.ends_with("outside the allowed directory"),
        "{script}: {message}"
      );
    }
  }
  assert!(!other.path().join("new.txt").exists());
  assert!(!other.path().join("copy.txt").exists());

  // Read-only: reads work, changes fail.
  let read_only = || Some(FsAccess::within(work.path()).unwrap().read_only());
  assert_eq!(
    call_tool(r#""a.txt" | FS.Read"#, read_only()),
    Ok("a".to_string())
  );
  for script in [
    r#""b.txt" | FS.Write(contents: "b")"#,
    r#""a.txt" | FS.Rename(new-name: "c.txt")"#,
    r#""dir" | FS.CreateDirectories"#,
  ] {
    let message = call_tool(script, read_only()).unwrap_err();
    assert!(
      message.ends_with("writing is not allowed"),
      "{script}: {message}"
    );
  }
  assert!(work.path().join("a.txt").exists());

  // Denied: nothing touches the disk.
  let message = call_tool(r#""a.txt" | FS.Read"#, Some(FsAccess::denied())).unwrap_err();
  assert_eq!(message, "FS.Read: a.txt: file access is denied");
  // Without a policy, any path works.
  assert_eq!(
    call_tool(&format!(r#""{secret}" | FS.Read"#), None),
    Ok("secret".to_string())
  );
}

/// Review findings: links already in the tree, a link through another link
/// and `..`, and the root itself.
#[cfg(unix)]
#[test]
fn links_in_the_tree_and_the_root_itself_stay_confined() {
  use std::os::unix::fs::symlink;
  let work = TempDir::new("confined");
  let other = TempDir::new("victim");
  let within = || Some(FsAccess::within(work.path()).unwrap());
  let victim = other.path().join("victim.txt");
  std::fs::write(&victim, "untouched").unwrap();

  // A copy over a destination tree holding a link out writes nothing there.
  std::fs::create_dir_all(work.path().join("src")).unwrap();
  std::fs::write(work.path().join("src/f"), "payload").unwrap();
  std::fs::create_dir_all(work.path().join("dst")).unwrap();
  symlink(&victim, work.path().join("dst/f")).unwrap();
  let message = call_tool(
    r#""src" | FS.Copy(destination: "dst" overwrite: true)"#,
    within(),
  )
  .unwrap_err();
  assert!(
    message.ends_with("outside the allowed directory"),
    "{message}"
  );
  assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
  // Nor into a destination directory that is itself a link out.
  symlink(other.path(), work.path().join("dst2")).unwrap();
  assert!(
    call_tool(
      r#""src" | FS.Copy(destination: "dst2" overwrite: true)"#,
      within()
    )
    .is_err()
  );
  assert!(!other.path().join("f").exists());

  // `s/../secret` where `s` leads outside: the OS resolves `..` after `s`.
  std::fs::create_dir_all(other.path().join("a/b")).unwrap();
  std::fs::write(other.path().join("a/secret"), "SECRET").unwrap();
  symlink(other.path().join("a/b"), work.path().join("s")).unwrap();
  symlink("s/../secret", work.path().join("L")).unwrap();
  let message = call_tool(r#""L" | FS.Read"#, within()).unwrap_err();
  assert!(
    message.ends_with("outside the allowed directory"),
    "{message}"
  );

  // The root itself is not changed, however it is named.
  for path in [".", "", "x/.."] {
    let message =
      call_tool(&format!(r#""{path}" | FS.RemoveAll | ToString"#), within()).unwrap_err();
    assert!(message.ends_with("cannot be changed"), "{path}: {message}");
  }
  assert!(work.path().join("src/f").exists());
}
