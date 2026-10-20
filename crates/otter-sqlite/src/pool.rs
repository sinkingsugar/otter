//! The [`Connections`] service: read-only connections pooled per database.
//!
//! A host inserts one into a mesh's services (`mesh.services_mut()`,
//! `Session::set_services`, a tool call's `CallOptions::services`) to size
//! the pool or to share it between meshes; without one, `Db.Query` uses a
//! process-wide default ([`Connections::of`]). A query leases a connection
//! for its whole run, on the blocking pool, and gives it back when it ends,
//! so concurrent queries never share one. Connections are keyed by the
//! database's resolved path.
//!
//! Every connection is read-only three times over: opened with
//! `SQLITE_OPEN_READ_ONLY` (no `SQLITE_OPEN_URI`, so a name is always a file
//! name), `PRAGMA query_only` set, and an authorizer that admits only what a
//! reading `SELECT` needs (reads, functions, recursive CTEs and the
//! introspection pragmas listed in [`READ_PRAGMAS`] and
//! [`READ_PRAGMAS_WITH_ARGUMENT`]): no transaction or savepoint (which could
//! hold a read snapshot on a pooled connection), no `ATTACH` (which would
//! open a file the host's `FsAccess` never checked; the attached-database
//! limit is 0 as well), no pragma that sets a value. `Db.Query` checks
//! `sqlite3_stmt_readonly` before running a statement on top.
//!
//! A connection that was interrupted, failed in a way that may leave it
//! changed, or ended outside autocommit mode is closed instead of pooled.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use otter_core::service::Services;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags};

/// Pragmas a query may read, without a value (`PRAGMA user_version`).
pub const READ_PRAGMAS: &[&str] = &[
  "application_id",
  "collation_list",
  "compile_options",
  "data_version",
  "database_list",
  "encoding",
  "freelist_count",
  "function_list",
  "journal_mode",
  "module_list",
  "page_count",
  "page_size",
  "pragma_list",
  "schema_version",
  "user_version",
];

/// Pragmas a query may read with an argument naming what to describe
/// (`PRAGMA table_info(messages)`, `SELECT * FROM pragma_table_info('t')`).
pub const READ_PRAGMAS_WITH_ARGUMENT: &[&str] = &[
  "foreign_key_list",
  "index_info",
  "index_list",
  "index_xinfo",
  "table_info",
  "table_list",
  "table_xinfo",
];

/// Read-only SQLite connections, pooled per database path. A service.
pub struct Connections {
  idle: Mutex<HashMap<PathBuf, Vec<Connection>>>,
  max_idle: usize,
  busy_timeout: Duration,
  leased: AtomicUsize,
}

impl Default for Connections {
  fn default() -> Connections {
    Connections::new()
  }
}

impl Connections {
  /// A pool keeping up to 4 idle connections per database, waiting up to 5
  /// seconds for a lock (a reader of a WAL database rarely waits; one of a
  /// rollback-journal database waits while a writer commits).
  pub fn new() -> Connections {
    Connections {
      idle: Mutex::new(HashMap::new()),
      max_idle: 4,
      busy_timeout: Duration::from_secs(5),
      leased: AtomicUsize::new(0),
    }
  }

  /// At most `n` idle connections kept per database (0: none pooled).
  pub fn with_max_idle(mut self, n: usize) -> Connections {
    self.max_idle = n;
    self
  }

  /// How long a query waits for a database another connection locked
  /// before failing with "database is locked". Cancellation does not
  /// shorten this wait.
  pub fn with_busy_timeout(mut self, timeout: Duration) -> Connections {
    self.busy_timeout = timeout;
    self
  }

  /// Connections leased to queries now.
  pub fn leased(&self) -> usize {
    self.leased.load(Ordering::SeqCst)
  }

  /// Idle connections kept, over every database.
  pub fn idle(&self) -> usize {
    self.idle.lock().unwrap().values().map(Vec::len).sum()
  }

  /// Closes every idle connection (leased ones close when they come back
  /// only if the pool is full).
  pub fn clear(&self) {
    self.idle.lock().unwrap().clear();
  }

  /// The mesh's pool, or the process-wide default.
  pub fn of(services: &Services) -> Arc<Connections> {
    static DEFAULT: OnceLock<Arc<Connections>> = OnceLock::new();
    services
      .get_arc::<Connections>()
      .unwrap_or_else(|| DEFAULT.get_or_init(|| Arc::new(Connections::new())).clone())
  }

  /// A connection to the database at `path` (resolved, existing), idle or
  /// newly opened. Call it off the engine thread: opening reads the file.
  pub fn lease(self: &Arc<Self>, path: &Path) -> rusqlite::Result<Lease> {
    let pooled = self.idle.lock().unwrap().get_mut(path).and_then(Vec::pop);
    let conn = match pooled {
      Some(conn) => conn,
      None => self.open(path)?,
    };
    self.leased.fetch_add(1, Ordering::SeqCst);
    Ok(Lease {
      pool: Arc::clone(self),
      path: path.to_path_buf(),
      conn: Some(conn),
      reusable: true,
    })
  }

  fn open(&self, path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
      path,
      OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(self.busy_timeout)?;
    conn.pragma_update(None, "query_only", true)?;
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    conn.authorizer(Some(authorize))?;
    Ok(conn)
  }
}

/// Admits what reading needs, and nothing else.
fn authorize(ctx: AuthContext<'_>) -> Authorization {
  match ctx.action {
    AuthAction::Select
    | AuthAction::Read { .. }
    | AuthAction::Function { .. }
    | AuthAction::Recursive => Authorization::Allow,
    AuthAction::Pragma {
      pragma_name,
      pragma_value,
    } => {
      let name = pragma_name.to_ascii_lowercase();
      let allowed = match pragma_value {
        None => {
          READ_PRAGMAS.contains(&name.as_str())
            || READ_PRAGMAS_WITH_ARGUMENT.contains(&name.as_str())
        }
        Some(_) => READ_PRAGMAS_WITH_ARGUMENT.contains(&name.as_str()),
      };
      if allowed {
        Authorization::Allow
      } else {
        Authorization::Deny
      }
    }
    _ => Authorization::Deny,
  }
}

/// A connection leased from [`Connections`]: back to the pool when dropped,
/// unless [`Lease::discard`]ed or left outside autocommit mode.
pub struct Lease {
  pool: Arc<Connections>,
  path: PathBuf,
  conn: Option<Connection>,
  reusable: bool,
}

impl Lease {
  pub fn conn(&self) -> &Connection {
    self.conn.as_ref().expect("a lease holds its connection")
  }

  /// Closes the connection instead of pooling it.
  pub fn discard(&mut self) {
    self.reusable = false;
  }
}

impl Drop for Lease {
  fn drop(&mut self) {
    if let Some(conn) = self.conn.take()
      && self.reusable
      && conn.is_autocommit()
    {
      let mut idle = self.pool.idle.lock().unwrap();
      let list = idle.entry(std::mem::take(&mut self.path)).or_default();
      if list.len() < self.pool.max_idle {
        list.push(conn);
      }
    }
    self.pool.leased.fetch_sub(1, Ordering::SeqCst);
  }
}
