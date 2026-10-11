//! Otter I/O shards. Native only for now: browser integration (fetch
//! instead of sockets, the browser event loop instead of Tokio) is separate
//! work.
//!
//! [`runtime`] owns the integration with the async runtime, outside any
//! shard: shards spawn their I/O there and poll the returned [`IoTask`] from
//! the mesh thread, through the shared `AsyncShard` adapter, so they work on
//! both schedulers and never block.

pub mod http;
pub mod runtime;

pub use runtime::{CancellationToken, IoTask};

/// Every shard type in this crate, for building a `otter_core::Catalog`.
pub static CATALOG: &[&otter_core::ShardType] = &[&http::GET];
