//! `Db.Query` through Otter scripts: type mapping, parameters, `max-rows`,
//! refused writes, `FsAccess` confinement and read grants, cancellation,
//! and reading a WAL database while another connection writes it. The
//! fixtures are built here, with rusqlite.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use otter_core::fs_access::FsAccess;
use otter_core::service::Services;
use otter_core::{CancelToken, Catalog, Error, Outcome, Var};
use otter_lang::{CallOptions, MemoryFiles, Program, Source, Tool, ToolError, ToolInput};
use otter_sqlite::Connections;
use rusqlite::Connection;

fn catalog() -> Catalog {
  Catalog::new(&[
    otter_core::shards::CATALOG,
    otter_io::CATALOG,
    otter_sqlite::CATALOG,
  ])
  .unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
  fn new(test: &str) -> TempDir {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
      "otter-sqlite-{test}-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    TempDir(std::fs::canonicalize(path).unwrap())
  }

  fn path(&self) -> &Path {
    &self.0
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

/// A WAL database at `dir/name`: `notes` (every storage class, NULLs
/// included), an FTS5 table `docs`, and `big` with 5,000 rows. Returns the
/// connection that built it, still open, as a writer would be.
fn fixture(dir: &TempDir, name: &str) -> Connection {
  let conn = Connection::open(dir.path().join(name)).unwrap();
  let mode: String = conn
    .pragma_update_and_check(None, "journal_mode", "wal", |r| r.get(0))
    .unwrap();
  assert_eq!(mode, "wal");
  conn
    .execute_batch(
      "CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT, score REAL, data BLOB, extra TEXT);
       INSERT INTO notes VALUES (1, 'alpha', 1.5, x'00ff10', NULL);
       INSERT INTO notes VALUES (2, 'beta', NULL, NULL, 'x');
       INSERT INTO notes VALUES (3, 'gamma', -2.25, x'', 'y');
       CREATE VIRTUAL TABLE docs USING fts5(title, body);
       INSERT INTO docs VALUES ('Trip to Kyoto', 'We booked the ryokan near the station.');
       INSERT INTO docs VALUES ('Groceries', 'Milk, eggs and green tea.');
       INSERT INTO docs VALUES ('Kyoto again', 'Temples in the rain; more green tea.');
       CREATE TABLE big(n INTEGER);
       WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000)
       INSERT INTO big SELECT x FROM c;",
    )
    .unwrap();
  conn
}

/// Runs `text` with `@db` defined: the root wire's outcome.
fn run(text: &str, db: &str) -> Outcome {
  let defines = HashMap::from([("db".to_string(), db.to_string())]);
  let program = match Program::load(Source::new("t.otr", text), &catalog(), &defines) {
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

fn rows(value: &Var) -> &[Var] {
  value.as_seq().expect("a sequence of rows")
}

fn cell<'a>(row: &'a Var, key: &str) -> &'a Var {
  row.as_table().unwrap().get(key).unwrap()
}

/// A tool whose function runs `body` (which outputs a String) with
/// `services`, in `working_dir`: the result, or the failure message.
fn call_tool(
  body: &str,
  services: Services,
  working_dir: &str,
  timeout: Option<Duration>,
) -> Result<String, String> {
  let text = format!(
    r#"@fn(Main input: {{input: {{}} env: {{}} working-dir: String}} output: String params: {{}} {{
      {body}
    }})
    {{definition: {{name: "db" description: "queries" parameters: {{type: "object"}}}} use: "Main"}}"#
  );
  let tool = Tool::load_with(
    Source::new("tool.otr", text),
    &catalog(),
    &HashMap::new(),
    &MemoryFiles::default(),
  )
  .unwrap_or_else(|e| panic!("{e}"));
  let options = CallOptions {
    services,
    timeout,
    ..Default::default()
  };
  let input = ToolInput::new(Var::table(Vec::<(&str, Var)>::new())).with_working_dir(working_dir);
  match tool.call_with(&input, &options) {
    Ok(out) => Ok(out.text),
    Err(ToolError::Failed { message, .. }) => Err(message),
    Err(e) => Err(e.to_string()),
  }
}

#[test]
fn rows_map_every_storage_class() {
  let dir = TempDir::new("types");
  let _writer = fixture(&dir, "app.db");
  let value = completed(run(
    r#"none | Db.Query("SELECT id, title, score, data, extra FROM notes ORDER BY id" @db)"#,
    &dir.join("app.db"),
  ));
  let all = rows(&value);
  assert_eq!(all.len(), 3);
  assert_eq!(cell(&all[0], "id"), &Var::Int(1));
  assert_eq!(cell(&all[0], "title"), &Var::string("alpha"));
  assert_eq!(cell(&all[0], "score"), &Var::Float(1.5));
  assert_eq!(cell(&all[0], "data"), &Var::bytes(&[0, 255, 16]));
  assert_eq!(cell(&all[0], "extra"), &Var::None);
  assert_eq!(cell(&all[1], "score"), &Var::None);
  assert_eq!(cell(&all[1], "data"), &Var::None);
  assert_eq!(cell(&all[2], "score"), &Var::Float(-2.25));
  assert_eq!(cell(&all[2], "data"), &Var::bytes(&[]));
  assert_eq!(all[0].as_table().unwrap().len(), 5);

  // No rows is an empty sequence; expressions are columns by their alias.
  let value = completed(run(
    r#"[] | Db.Query("SELECT count(*) AS n, 1 + 1.5 AS f FROM notes WHERE id > 100" @db)"#,
    &dir.join("app.db"),
  ));
  assert_eq!(cell(&rows(&value)[0], "n"), &Var::Int(0));
  assert_eq!(cell(&rows(&value)[0], "f"), &Var::Float(2.5));
  let value = completed(run(
    r#"none | Db.Query("SELECT * FROM notes WHERE id > 100" @db)"#,
    &dir.join("app.db"),
  ));
  assert!(rows(&value).is_empty());

  // Scripts read rows as tables.
  let value = completed(run(
    r#"none | Db.Query("SELECT title FROM notes ORDER BY id" @db) = rows
    rows.1.title | ExpectString"#,
    &dir.join("app.db"),
  ));
  assert_eq!(value, Var::string("beta"));

  // Two columns with one name are an error, not a silent loss.
  let message = failed(run(
    r#"none | Db.Query("SELECT id, id FROM notes" @db)"#,
    &dir.join("app.db"),
  ));
  assert!(message.contains("two columns are named `id`"), "{message}");
}

#[test]
fn parameters_bind_by_position_and_by_name() {
  let dir = TempDir::new("params");
  let _writer = fixture(&dir, "app.db");
  let db = dir.join("app.db");
  let value = completed(run(
    r#"[1 "gamma"] | Db.Query("SELECT id FROM notes WHERE id = ? OR title = ? ORDER BY id" @db)"#,
    &db,
  ));
  assert_eq!(rows(&value).len(), 2);
  let value = completed(run(
    r#"[2] | Db.Query("SELECT title FROM notes WHERE id = ?1 AND ?1 > 0" @db)"#,
    &db,
  ));
  assert_eq!(cell(&rows(&value)[0], "title"), &Var::string("beta"));
  let value = completed(run(
    r#"{min: 1.0 name: "alpha"} | Db.Query("SELECT id FROM notes WHERE score >= :min AND title = @name" @db)"#,
    &db,
  ));
  assert_eq!(cell(&rows(&value)[0], "id"), &Var::Int(1));
  // Bytes, Bool and none bind as blob, integer and NULL.
  let value = completed(run(
    r#"[("00ff10" | HexToBytes) true none] | Db.Query("SELECT ? = data AS same, ? AS t, ? IS NULL AS n FROM notes WHERE id = 1" @db)"#,
    &db,
  ));
  let row = &rows(&value)[0];
  assert_eq!(cell(row, "same"), &Var::Int(1));
  assert_eq!(cell(row, "t"), &Var::Int(1));
  assert_eq!(cell(row, "n"), &Var::Int(1));

  for (script, expected) in [
    (
      r#"[1 2] | Db.Query("SELECT ? AS a" @db)"#,
      "the query has 1 parameter(s), the input 2 value(s)",
    ),
    (
      r#"none | Db.Query("SELECT ? AS a" @db)"#,
      "give their values as input",
    ),
    (
      r#"{a: 1} | Db.Query("SELECT :b AS b" @db)"#,
      "no value for parameter :b",
    ),
    (
      r#"{a: 1 b: 2} | Db.Query("SELECT :b AS b" @db)"#,
      "does not use: a",
    ),
    (
      r#"{a: 1} | Db.Query("SELECT ? AS b" @db)"#,
      "give a sequence as input",
    ),
    (
      r#"[1] | Db.Query("SELECT :a AS a" @db)"#,
      "give a table as input",
    ),
    (
      r#"[[1]] | Db.Query("SELECT ? AS a" @db)"#,
      "a parameter must be an Int, Float, String, Bytes, Bool or none",
    ),
  ] {
    let message = failed(run(script, &db));
    assert!(message.contains(expected), "{script}: {message}");
    assert!(
      message.starts_with(&format!("Db.Query: {db}: ")),
      "{message}"
    );
  }

  // A String input is a compose error, located.
  let problems = otter_lang::check(
    Source::new("t.otr", r#""x" | Db.Query("SELECT 1" "a.db")"#),
    &catalog(),
    &HashMap::new(),
  )
  .diagnostics;
  assert!(
    problems.iter().any(|d| d.code == "input-type-mismatch"),
    "{problems:?}"
  );
}

#[test]
fn max_rows_bounds_the_output() {
  let dir = TempDir::new("max-rows");
  let _writer = fixture(&dir, "app.db");
  let db = dir.join("app.db");
  // The default is 1,000: 5,000 rows fail with a clear message.
  let message = failed(run(r#"none | Db.Query("SELECT n FROM big" @db)"#, &db));
  assert!(
    message.contains("more than 1000 rows (add a LIMIT, or raise max-rows)"),
    "{message}"
  );
  let value = completed(run(
    r#"none | Db.Query("SELECT n FROM big" @db max-rows: 5000)"#,
    &db,
  ));
  assert_eq!(rows(&value).len(), 5000);
  assert_eq!(cell(&rows(&value)[4999], "n"), &Var::Int(5000));
  let value = completed(run(
    r#"none | Db.Query("SELECT n FROM big LIMIT 3" @db max-rows: 3)"#,
    &db,
  ));
  assert_eq!(rows(&value).len(), 3);
  let message = failed(run(
    r#"none | Db.Query("SELECT n FROM big LIMIT 4" @db max-rows: 3)"#,
    &db,
  ));
  assert!(message.contains("more than 3 rows"), "{message}");
  // At least 1, checked at compose.
  let problems = otter_lang::check(
    Source::new("t.otr", r#"none | Db.Query("SELECT 1" "a.db" max-rows: 0)"#),
    &catalog(),
    &HashMap::new(),
  )
  .diagnostics;
  assert!(
    problems
      .iter()
      .any(|d| d.code == "invalid-argument" && d.message.contains("max-rows must be at least 1")),
    "{problems:?}"
  );
}

#[test]
fn fts5_matches() {
  let dir = TempDir::new("fts5");
  let _writer = fixture(&dir, "app.db");
  let value = completed(run(
    r#"["green tea"] | Db.Query("SELECT title, snippet(docs, 1, '[', ']', '...', 4) AS hit FROM docs WHERE docs MATCH ? ORDER BY rank" @db)"#,
    &dir.join("app.db"),
  ));
  let rows = rows(&value);
  assert_eq!(rows.len(), 2);
  let titles: Vec<&Var> = rows.iter().map(|r| cell(r, "title")).collect();
  assert!(titles.contains(&&Var::string("Groceries")), "{titles:?}");
  assert!(titles.contains(&&Var::string("Kyoto again")), "{titles:?}");
  assert!(
    cell(&rows[0], "hit")
      .as_str()
      .unwrap()
      .contains("[green] [tea]"),
    "{:?}",
    rows[0]
  );
  let value = completed(run(
    r#"{q: "kyoto"} | Db.Query("SELECT title FROM docs WHERE docs MATCH :q ORDER BY rowid" @db)"#,
    &dir.join("app.db"),
  ));
  assert_eq!(value.as_seq().unwrap().len(), 2);
}

#[test]
fn writes_and_other_changes_are_refused() {
  let dir = TempDir::new("refused");
  let writer = fixture(&dir, "app.db");
  let db = dir.join("app.db");
  let other = dir.join("other.db");
  Connection::open(&other)
    .unwrap()
    .execute_batch("CREATE TABLE t(x)")
    .unwrap();
  for sql in [
    "INSERT INTO notes(title) VALUES ('evil')",
    "UPDATE notes SET title = 'evil'",
    "DELETE FROM notes",
    "DROP TABLE notes",
    "CREATE TABLE evil(x)",
    "CREATE TEMP TABLE evil(x)",
    "INSERT INTO docs(docs) VALUES ('optimize')",
    &format!("ATTACH '{other}' AS o"),
    "DETACH main",
    "BEGIN",
    "COMMIT",
    "SAVEPOINT s",
    "PRAGMA journal_mode = DELETE",
    "PRAGMA user_version = 7",
    "PRAGMA query_only = 0",
    "PRAGMA writable_schema = 1",
    "PRAGMA wal_checkpoint",
    "VACUUM",
    &format!("VACUUM INTO '{}'", dir.join("copy.db")),
    "ANALYZE",
    "REINDEX",
  ] {
    let message = failed(run(&format!(r#"none | Db.Query("{sql}" @db)"#), &db));
    assert!(
      message.starts_with(&format!("Db.Query: {db}: refused: only reading is allowed")),
      "{sql}: {message}"
    );
  }
  let message = failed(run(r#"none | Db.Query("SELECT 1; SELECT 2" @db)"#, &db));
  assert!(
    message.contains("the query must be one statement"),
    "{message}"
  );
  assert!(!dir.path().join("copy.db").exists());
  let count: i64 = writer
    .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
    .unwrap();
  assert_eq!(count, 3);
  let version: i64 = writer
    .query_row("PRAGMA user_version", [], |r| r.get(0))
    .unwrap();
  assert_eq!(version, 0);

  // Introspection that only reads is allowed.
  let value = completed(run(
    r#"none | Db.Query("PRAGMA table_info(notes)" @db)"#,
    &db,
  ));
  assert_eq!(value.as_seq().unwrap().len(), 5);
  let value = completed(run(
    r#"none | Db.Query("SELECT name FROM pragma_table_info('notes') ORDER BY cid" @db)"#,
    &db,
  ));
  assert_eq!(cell(&rows(&value)[1], "name"), &Var::string("title"));
  let value = completed(run(r#"none | Db.Query("PRAGMA user_version" @db)"#, &db));
  assert_eq!(cell(&rows(&value)[0], "user_version"), &Var::Int(0));
  let value = completed(run(
    r#"none | Db.Query("SELECT name FROM sqlite_schema WHERE type = 'table' AND name = 'notes'" @db)"#,
    &db,
  ));
  assert_eq!(rows(&value).len(), 1);
}

#[test]
fn a_missing_database_is_not_created() {
  let dir = TempDir::new("missing");
  let message = failed(run(
    r#"none | Db.Query("SELECT 1" @db)"#,
    &dir.join("nope.db"),
  ));
  assert!(message.contains("No such file"), "{message}");
  assert!(!dir.path().join("nope.db").exists());
  let message = failed(run(r#"none | Db.Query("SELECT 1" @db)"#, &dir.join("")));
  assert!(message.contains("not a file"), "{message}");
}

#[test]
fn file_access_confines_databases_and_grants_reads() {
  let work = TempDir::new("work");
  let outside = TempDir::new("outside");
  let _w1 = fixture(&work, "inside.db");
  let _w2 = fixture(&outside, "chat.db");
  let _w3 = fixture(&outside, "secret.db");
  let count = r#"none | Db.Query("SELECT count(*) AS n FROM notes" DB) | ToJson"#;
  let query = |db: &str, services: Services| {
    call_tool(
      &count.replace("DB", &format!("\"{db}\"")),
      services,
      work.path().to_str().unwrap(),
      None,
    )
  };
  // A tool is confined to its working directory by default: relative paths
  // resolve there, and nothing outside it opens.
  assert_eq!(
    query("inside.db", Services::default()),
    Ok(r#"[{"n":3}]"#.into())
  );
  let message = query(&outside.join("chat.db"), Services::default()).unwrap_err();
  assert!(
    message.contains("outside the allowed directory"),
    "{message}"
  );
  let message = query("../x/../chat.db", Services::default()).unwrap_err();
  assert!(
    message.contains("outside the allowed directory"),
    "{message}"
  );

  // A read grant opens one file outside the root, and only that file.
  let granted = || {
    let mut services = Services::default();
    services.insert(
      FsAccess::within(work.path())
        .unwrap()
        .grant_read(outside.path().join("chat.db"))
        .unwrap(),
    );
    services
  };
  assert_eq!(
    query(&outside.join("chat.db"), granted()),
    Ok(r#"[{"n":3}]"#.into())
  );
  assert_eq!(query("inside.db", granted()), Ok(r#"[{"n":3}]"#.into()));
  let message = query(&outside.join("secret.db"), granted()).unwrap_err();
  assert!(
    message.contains("outside the allowed directory"),
    "{message}"
  );

  // A link inside the root that leads to a file outside is no way around.
  #[cfg(unix)]
  {
    std::os::unix::fs::symlink(
      outside.path().join("secret.db"),
      work.path().join("link.db"),
    )
    .unwrap();
    let message = query("link.db", granted()).unwrap_err();
    assert!(
      message.contains("outside the allowed directory"),
      "{message}"
    );
    // A link to the granted file reads it.
    std::os::unix::fs::symlink(
      outside.path().join("chat.db"),
      work.path().join("chat-link.db"),
    )
    .unwrap();
    assert_eq!(query("chat-link.db", granted()), Ok(r#"[{"n":3}]"#.into()));
  }

  // A grant reads only: writing through FS shards is still refused.
  let message = call_tool(
    &format!(
      r#""x" | FS.Write(contents: "x") = _x
      "{}" | FS.Write(contents: "x" overwrite: true)"#,
      outside.join("chat.db")
    ),
    granted(),
    work.path().to_str().unwrap(),
    None,
  )
  .unwrap_err();
  assert!(
    message.contains("outside the allowed directory"),
    "{message}"
  );
  assert!(outside.path().join("chat.db").metadata().unwrap().len() > 1);

  // Denied access with a grant reads the granted file and nothing else.
  let mut services = Services::default();
  services.insert(
    FsAccess::denied()
      .grant_read(outside.path().join("chat.db"))
      .unwrap(),
  );
  assert_eq!(
    query(&outside.join("chat.db"), services.clone()),
    Ok(r#"[{"n":3}]"#.into())
  );
  let message = query(&outside.join("secret.db"), services.clone()).unwrap_err();
  assert!(message.contains("file access is denied"), "{message}");
  let message = query("inside.db", services).unwrap_err();
  assert!(message.contains("file access is denied"), "{message}");

  // A directory grant covers what is under it.
  let mut services = Services::default();
  services.insert(
    FsAccess::within(work.path())
      .unwrap()
      .grant_read(outside.path())
      .unwrap(),
  );
  assert_eq!(
    query(&outside.join("secret.db"), services),
    Ok(r#"[{"n":3}]"#.into())
  );
}

#[test]
fn cancelling_a_long_query_interrupts_it() {
  let dir = TempDir::new("cancel");
  let _writer = fixture(&dir, "app.db");
  let pool = Arc::new(Connections::new());
  let mut services = Services::default();
  services.insert_arc(Arc::clone(&pool));
  // Never ends on its own: only the interrupt stops it.
  let endless = r#"none | Db.Query("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) AS n FROM c" "app.db") | ToJson"#;
  let started = Instant::now();
  let result = call_tool(
    endless,
    services.clone(),
    dir.path().to_str().unwrap(),
    Some(Duration::from_millis(300)),
  );
  assert!(result.unwrap_err().contains("timed out"));
  // The worker stops at SQLite's next check, and its connection is closed,
  // not pooled.
  let deadline = Instant::now() + Duration::from_secs(5);
  while pool.leased() > 0 && Instant::now() < deadline {
    std::thread::sleep(Duration::from_millis(5));
  }
  assert_eq!(
    pool.leased(),
    0,
    "the interrupted query still holds its connection"
  );
  assert_eq!(pool.idle(), 0, "an interrupted connection is not pooled");
  assert!(started.elapsed() < Duration::from_secs(5));

  // The pool works afterwards, and reuses connections of finished queries.
  let count = r#"none | Db.Query("SELECT count(*) AS n FROM notes" "app.db") | ToJson"#;
  for _ in 0..3 {
    assert_eq!(
      call_tool(count, services.clone(), dir.path().to_str().unwrap(), None),
      Ok(r#"[{"n":3}]"#.into())
    );
  }
  assert_eq!(pool.leased(), 0);
  assert_eq!(pool.idle(), 1);

  // Cancelling through a token works the same way.
  let tool = Tool::load_with(
    Source::new(
      "tool.otr",
      format!(
        r#"@fn(Main input: {{input: {{}} env: {{}} working-dir: String}} output: String params: {{}} {{ {endless} }})
        {{definition: {{name: "db" description: "q" parameters: {{type: "object"}}}} use: "Main"}}"#
      ),
    ),
    &catalog(),
    &HashMap::new(),
    &MemoryFiles::default(),
  )
  .unwrap();
  let token = CancelToken::new();
  let canceller = {
    let token = token.clone();
    std::thread::spawn(move || {
      std::thread::sleep(Duration::from_millis(200));
      token.cancel();
    })
  };
  let options = CallOptions {
    services,
    cancel: Some(token),
    ..Default::default()
  };
  let input = ToolInput::new(Var::table(Vec::<(&str, Var)>::new()))
    .with_working_dir(dir.path().to_str().unwrap());
  let result = tool.call_with(&input, &options);
  canceller.join().unwrap();
  assert!(
    matches!(result, Err(ToolError::Cancelled { .. })),
    "{result:?}"
  );
  let deadline = Instant::now() + Duration::from_secs(5);
  while pool.leased() > 0 && Instant::now() < deadline {
    std::thread::sleep(Duration::from_millis(5));
  }
  assert_eq!(pool.leased(), 0);
}

#[test]
fn reads_a_wal_database_while_another_connection_writes() {
  let dir = TempDir::new("wal");
  let writer = fixture(&dir, "app.db");
  let db = dir.join("app.db");
  let count = r#"none | Db.Query("SELECT count(*) AS n FROM notes" @db)"#;
  let n = |value: Var| cell(&rows(&value)[0], "n").clone();

  // An open write transaction: readers see the last commit, and are not
  // blocked by it.
  writer
    .execute_batch("BEGIN IMMEDIATE; INSERT INTO notes(title) VALUES ('pending');")
    .unwrap();
  assert_eq!(n(completed(run(count, &db))), Var::Int(3));
  writer.execute_batch("COMMIT").unwrap();
  assert_eq!(n(completed(run(count, &db))), Var::Int(4));

  // A writer committing in a loop while queries run.
  let stop = Arc::new(AtomicBool::new(false));
  let path = dir.path().join("app.db");
  let busy_writer = {
    let stop = Arc::clone(&stop);
    std::thread::spawn(move || {
      let conn = Connection::open(path).unwrap();
      conn.busy_timeout(Duration::from_secs(5)).unwrap();
      let mut written = 0;
      while !stop.load(Ordering::Relaxed) {
        conn
          .execute("INSERT INTO docs VALUES ('note', 'green tea again')", [])
          .unwrap();
        written += 1;
      }
      written
    })
  };
  let mut last = 0;
  for _ in 0..30 {
    let value = completed(run(
      r#"none | Db.Query("SELECT count(*) AS n FROM docs WHERE docs MATCH 'green'" @db)"#,
      &db,
    ));
    let Var::Int(seen) = n(value) else { panic!() };
    assert!(seen >= last, "counts only grow: {seen} after {last}");
    last = seen;
  }
  stop.store(true, Ordering::Relaxed);
  let written = busy_writer.join().unwrap();
  assert!(written > 0);
  let value = completed(run(
    r#"none | Db.Query("SELECT count(*) AS n FROM docs WHERE docs MATCH 'green'" @db)"#,
    &db,
  ));
  assert_eq!(n(value), Var::Int(2 + written));
}

#[test]
fn reads_a_wal_database_no_one_has_open() {
  let dir = TempDir::new("wal-closed");
  drop(fixture(&dir, "app.db"));
  let value = completed(run(
    r#"none | Db.Query("SELECT count(*) AS n FROM notes" @db)"#,
    &dir.join("app.db"),
  ));
  assert_eq!(cell(&rows(&value)[0], "n"), &Var::Int(3));
}

#[test]
fn query_and_database_may_be_variables() {
  let dir = TempDir::new("vars");
  let _writer = fixture(&dir, "app.db");
  let value = completed(run(
    r#"@db | ExpectString = path
    "SELECT title FROM notes WHERE id = ?" = sql
    [3] | Db.Query(sql path)"#,
    &dir.join("app.db"),
  ));
  assert_eq!(cell(&rows(&value)[0], "title"), &Var::string("gamma"));
}

#[test]
fn described_in_the_catalog() {
  let catalog = catalog();
  let json = catalog.describe_json("Db.Query").unwrap();
  assert!(json.contains("\"targets\":\"native-only\""), "{json}");
  assert!(json.contains("\"io\":true"), "{json}");
  assert!(json.contains("\"name\":\"query\",\"index\":0"), "{json}");
  assert!(json.contains("\"name\":\"database\",\"index\":1"), "{json}");
  assert!(json.contains("\"default\":1000"), "{json}");
}
