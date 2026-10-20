//! Otter SQLite shards: read-only queries of existing databases, the first
//! cut of the sqlite module (docs/host-requirements.md §4).
//!
//! [`query`] holds `Db.Query`, which runs one read-only statement on the
//! shared runtime's blocking pool and outputs its rows as tables;
//! [`pool`] holds the [`Connections`] service that pools read-only
//! connections per database. Database paths go through the mesh's
//! `FsAccess` like the `FS.*` shards' (read grants included,
//! `FsAccess::grant_read`). SQLite is bundled (rusqlite 0.40, FTS5
//! included). Native only: desktop and iOS; not on wasm or ESP-IDF.
//!
//! Writes, transactions, `Db.RawQuery`, cr-sqlite and sqlite-vec are not
//! ported yet.

pub mod pool;
pub mod query;

pub use pool::Connections;

/// Every shard type in this crate, for building a `otter_core::Catalog`.
pub static CATALOG: &[&otter_core::ShardType] = &[&query::QUERY];
