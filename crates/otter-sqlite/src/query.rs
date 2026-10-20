//! `Db.Query`: one read-only SQL statement against an existing database,
//! its rows as a sequence of tables.
//!
//! The statement is prepared when the shard runs, never at compose (compose
//! reads no file), on the shared runtime's blocking pool
//! (`otter_io::runtime::spawn_blocking`), so only the waiting instance
//! suspends. The database path resolves through the mesh's `FsAccess`, as
//! the `FS.*` shards' paths do, and must name an existing file; the
//! connection comes from the mesh's [`Connections`] service.
//!
//! Cancelling the instance calls `sqlite3_interrupt` on the connection
//! (a task on the shared runtime waits for the operation's cancellation
//! token), so a long query stops at SQLite's next check; the interrupted
//! connection is closed, not pooled. Waiting for a lock (the pool's busy
//! timeout) is not interrupted.

use std::path::Path;
use std::sync::{Arc, Mutex};

use otter_core::compose::ComposeCtx;
use otter_core::describe::{
  DefaultValue, Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets,
  TypeName,
};
use otter_core::diagnostic::{Diagnostic, Phase};
use otter_core::fs_access::FsAccess;
use otter_core::instance::LeafCtx;
use otter_core::shards::Operand;
use otter_core::shards::async_shard::{AsyncShard, async_type};
use otter_core::signature::{Effects, Lifetime};
use otter_core::{Args, Composed, Error, Result, ShardType, TableBuilder, Type, Var, shard_doc};
use otter_io::runtime::{CancellationToken, IoTask, runtime, spawn_blocking};
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{ErrorCode, Statement};

use crate::pool::Connections;

const NAME: &str = "Db.Query";
const STRING: &[TypeName] = &[TypeName::String];
const INT: &[TypeName] = &[TypeName::Int];
const OPERAND: Forms = Forms::LITERAL.or(Forms::VARIABLE);

/// The default `max-rows`.
pub const DEFAULT_MAX_ROWS: i64 = 1000;

static PARAMS: &[ParamDecl] = &[
  ParamDecl::new(
    "query",
    shard_doc!(
      "The SQL: one statement that only reads (a `SELECT`, a `WITH` query, an introspection pragma). Parameters are `?`, `?1`... (bound from a sequence input) or `:name`, `@name`, `$name` (bound from a table input)."
    ),
    OPERAND,
    STRING,
    Requirement::Required,
  ),
  ParamDecl::new(
    "database",
    shard_doc!(
      "The database file's path, resolved as the `FS.*` shards resolve theirs (the mesh's `FsAccess`). The file must exist; it is opened read-only."
    ),
    OPERAND,
    STRING,
    Requirement::Required,
  ),
  ParamDecl::new(
    "max-rows",
    shard_doc!(
      "The most rows the query may return; one more is an error (add a `LIMIT`, or raise this). At least 1."
    ),
    Forms::LITERAL,
    INT,
    Requirement::Default(DefaultValue::Int(DEFAULT_MAX_ROWS)),
  ),
];

/// What a row's values can be: SQLite's storage classes.
fn cell_type() -> Type {
  Type::union([
    Type::int(),
    Type::float(),
    Type::string(),
    Type::bytes(),
    Type::none(),
  ])
}

/// The output: a sequence of rows, each a table keyed by column name.
pub fn rows_type() -> Type {
  Type::seq(Type::table(Vec::<(&str, Type)>::new(), Some(cell_type())))
}

/// The input: the bound parameters, positional or named, or none.
fn params_type() -> Type {
  Type::union([Type::none(), Type::seq(Type::any()), Type::any_table()])
}

pub const QUERY_DESC: ShardDesc = ShardDesc {
  name: NAME,
  version: 1,
  summary: shard_doc!(
    "Runs a read-only SQL query on an existing SQLite database and outputs its rows."
  ),
  help: shard_doc!(
    "The input binds the query's parameters: a sequence for positional ones (`[42 \"x\"] | Db.Query(\"SELECT * FROM t WHERE id = ? AND name = ?\" \"app.db\")`), a table for named ones (`{id: 42} | Db.Query(\"SELECT * FROM t WHERE id = :id\" \"app.db\")`), `none` for a query without any. Values bind as Int, Float, String (text), Bytes (blob), Bool (1 or 0) and none (NULL); every parameter needs a value, and every value a parameter.\n\nThe output is one table per row, keyed by column name (two columns with the same name are an error: give one an alias with `AS`): INTEGER is an Int, REAL a Float, TEXT a String (invalid UTF-8 replaced by U+FFFD), BLOB Bytes, NULL none. More than `max-rows` rows is an error.\n\nOnly reading is allowed: the database is opened read-only, and a statement that would write, attach a database, open a transaction or set a pragma is refused before it runs. A WAL database may be read while another process writes it; each query sees the last commit before it started. FTS5 full-text tables work (`... WHERE t MATCH ?`). Cancelling the wire interrupts the query."
  ),
  params: Params::Declared(PARAMS),
  input: InputDesc::Typed(params_type),
  output: OutputDesc::Dynamic(shard_doc!(
    "[{column: Int | Float | String | Bytes | None}]: one table per row"
  )),
  targets: Targets::NativeOnly,
  aliases: &[],
  effects: Effects::IO_WAIT,
  lifetime: Lifetime::Stateless,
};

pub struct QueryCompiled {
  query: Operand,
  database: Operand,
  max_rows: usize,
}

pub struct Query;

impl AsyncShard for Query {
  type Compiled = QueryCompiled;
  type Op = IoTask;
  const DESC: ShardDesc = QUERY_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<QueryCompiled>> {
    let (query, _) = Operand::compose_arg(args, "query", NAME, ctx)?;
    let (database, _) = Operand::compose_arg(args, "database", NAME, ctx)?;
    let max_rows = args.int("max-rows").expect("decoded max-rows");
    if max_rows < 1 {
      return Err(Error::Diagnostic(Box::new(
        Diagnostic::new(
          Phase::Compose,
          "compose-error",
          "invalid-argument",
          format!("max-rows must be at least 1, got {max_rows}"),
        )
        .shard(NAME)
        .param("max-rows", Some(args.param_index("max-rows"))),
      )));
    }
    Ok(Composed {
      compiled: QueryCompiled {
        query,
        database,
        max_rows: usize::try_from(max_rows).unwrap_or(usize::MAX),
      },
      output: rows_type(),
    })
  }

  fn start(c: &QueryCompiled, ctx: &mut impl LeafCtx, input: &Var) -> Result<IoTask> {
    let query = c.query.get(ctx);
    let database = c.database.get(ctx);
    let (Some(_), Some(_)) = (query.as_str(), database.as_str()) else {
      return Err(Error::Activation(format!(
        "{NAME}: query and database must be Strings"
      )));
    };
    let params = input.clone();
    let max_rows = c.max_rows;
    // Owned by the operation, so releasing the instance cannot release them.
    let access = FsAccess::of(ctx.services());
    let pool = Connections::of(ctx.services());
    Ok(spawn_blocking(move |cancel| {
      let sql = query.as_str().expect("checked above");
      let database = database.as_str().expect("checked above");
      run(&pool, &access, database, sql, &params, max_rows, &cancel)
        .map_err(|message| format!("{NAME}: {database}: {message}"))
    }))
  }
}

/// Runs on the blocking pool.
fn run(
  pool: &Arc<Connections>,
  access: &FsAccess,
  database: &str,
  sql: &str,
  params: &Var,
  max_rows: usize,
  cancel: &CancellationToken,
) -> std::result::Result<Var, String> {
  let resolved = access
    .resolve(Path::new(database), false)
    .map_err(|e| e.to_string())?;
  match std::fs::metadata(&resolved) {
    Ok(m) if m.is_file() => {}
    Ok(_) => return Err("not a file".into()),
    Err(e) => return Err(e.to_string()),
  }
  if cancel.is_cancelled() {
    return Err("cancelled".into());
  }
  let mut lease = pool.lease(&resolved).map_err(message)?;

  // The cancellation bridge: when the token fires while the query runs, the
  // connection is interrupted. The worker takes the handle back when it is
  // done; finding it gone means the interrupt was (or is being) sent, and
  // the connection is closed rather than pooled, so a late interrupt can
  // never reach another query.
  let handle = Arc::new(Mutex::new(Some(lease.conn().get_interrupt_handle())));
  {
    let handle = Arc::clone(&handle);
    let cancel = cancel.clone();
    // Ends when the token is cancelled, which dropping the operation does.
    runtime().spawn(async move {
      cancel.cancelled().await;
      let taken = handle.lock().unwrap().take();
      if let Some(h) = taken {
        h.interrupt();
      }
    });
  }
  let result = query(lease.conn(), sql, params, max_rows, cancel);
  let interrupted = handle.lock().unwrap().take().is_none();
  if interrupted || cancel.is_cancelled() {
    lease.discard();
    return Err("cancelled".into());
  }
  if let Err(Failure::Sqlite(_)) = &result {
    // The statement failed inside SQLite; start the next query afresh.
    lease.discard();
  }
  result.map_err(|f| match f {
    Failure::Sqlite(e) => message(e),
    Failure::Script(m) => m,
  })
}

enum Failure {
  Sqlite(rusqlite::Error),
  Script(String),
}

impl From<rusqlite::Error> for Failure {
  fn from(e: rusqlite::Error) -> Failure {
    Failure::Sqlite(e)
  }
}

fn message(e: rusqlite::Error) -> String {
  match &e {
    rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::AuthorizationForStatementDenied => {
      "refused: only reading is allowed (no writes, ATTACH, transactions or pragmas that set a value)"
        .into()
    }
    rusqlite::Error::MultipleStatement => "the query must be one statement".into(),
    _ => e.to_string(),
  }
}

fn query(
  conn: &rusqlite::Connection,
  sql: &str,
  params: &Var,
  max_rows: usize,
  cancel: &CancellationToken,
) -> std::result::Result<Var, Failure> {
  let mut stmt = conn.prepare(sql)?;
  if !stmt.readonly() {
    return Err(Failure::Script(
      "refused: only reading is allowed, and this statement writes".into(),
    ));
  }
  bind(&mut stmt, params).map_err(Failure::Script)?;

  let names: Vec<Arc<str>> = stmt.column_names().into_iter().map(Arc::from).collect();
  // Rows are built in key order, so each insert appends.
  let mut order: Vec<usize> = (0..names.len()).collect();
  order.sort_by(|a, b| names[*a].cmp(&names[*b]));
  if let Some(w) = order.windows(2).find(|w| names[w[0]] == names[w[1]]) {
    return Err(Failure::Script(format!(
      "two columns are named `{}`: give one an alias with AS",
      names[w[0]]
    )));
  }

  let mut rows = stmt.raw_query();
  let mut out = Vec::new();
  while let Some(row) = rows.next()? {
    if out.len() == max_rows {
      return Err(Failure::Script(format!(
        "more than {max_rows} rows (add a LIMIT, or raise max-rows)"
      )));
    }
    if cancel.is_cancelled() {
      return Err(Failure::Script("cancelled".into()));
    }
    let mut table = TableBuilder::new();
    for &i in &order {
      table.insert(Arc::clone(&names[i]), cell(row.get_ref(i)?));
    }
    out.push(Var::from_table(table.build()));
  }
  Ok(Var::seq(out))
}

fn cell(value: ValueRef<'_>) -> Var {
  match value {
    ValueRef::Null => Var::None,
    ValueRef::Integer(i) => Var::Int(i),
    ValueRef::Real(f) => Var::Float(f),
    ValueRef::Text(t) => Var::from_string(Arc::from(String::from_utf8_lossy(t))),
    ValueRef::Blob(b) => Var::bytes(b),
  }
}

fn value_ref(value: &Var) -> std::result::Result<ValueRef<'_>, String> {
  Ok(match value {
    Var::None => ValueRef::Null,
    Var::Bool(b) => ValueRef::Integer(i64::from(*b)),
    Var::Int(i) => ValueRef::Integer(*i),
    Var::Float(f) => ValueRef::Real(*f),
    Var::String(s) => ValueRef::Text(s.as_bytes()),
    Var::Bytes(b) => ValueRef::Blob(b),
    other => {
      return Err(format!(
        "a parameter must be an Int, Float, String, Bytes, Bool or none, got {}",
        other.type_of()
      ));
    }
  })
}

fn bind(stmt: &mut Statement<'_>, params: &Var) -> std::result::Result<(), String> {
  let count = stmt.parameter_count();
  let bind_one = |stmt: &mut Statement<'_>, index: usize, value: &Var| {
    let value = value_ref(value)?;
    stmt
      .raw_bind_parameter(index, ToSqlOutput::Borrowed(value))
      .map_err(|e| e.to_string())
  };
  match params {
    Var::None if count == 0 => Ok(()),
    Var::None => Err(format!(
      "the query has {count} parameter(s): give their values as input (a sequence or a table)"
    )),
    Var::Seq(values) => {
      for index in 1..=count {
        if let Some(name) = stmt.parameter_name(index)
          && !name.starts_with('?')
        {
          return Err(format!(
            "parameter {name} is named: give a table as input, not a sequence"
          ));
        }
      }
      if values.len() != count {
        return Err(format!(
          "the query has {count} parameter(s), the input {} value(s)",
          values.len()
        ));
      }
      for (i, value) in values.iter().enumerate() {
        bind_one(stmt, i + 1, value)?;
      }
      Ok(())
    }
    Var::Table(table) => {
      let mut used = 0;
      for index in 1..=count {
        let name = match stmt.parameter_name(index) {
          Some(name) if !name.starts_with('?') => name.to_string(),
          _ => {
            return Err(
              "the query has positional parameters (`?`): give a sequence as input, not a table"
                .into(),
            );
          }
        };
        let key = &name[1..];
        let value = table
          .get(key)
          .ok_or_else(|| format!("no value for parameter {name} (the input has no key `{key}`)"))?;
        used += 1;
        bind_one(stmt, index, value)?;
      }
      if used != table.len() {
        let unused: Vec<&str> = table
          .keys()
          .filter(|k| !(1..=count).any(|i| stmt.parameter_name(i).is_some_and(|n| &n[1..] == *k)))
          .collect();
        return Err(format!(
          "the input has key(s) the query does not use: {}",
          unused.join(", ")
        ));
      }
      Ok(())
    }
    other => Err(format!(
      "the parameters must be a sequence, a table or none, got {}",
      other.type_of()
    )),
  }
}

pub static QUERY: ShardType = async_type::<Query>();
