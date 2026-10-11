//! The scheduler: an iterative, directly resumable frame runner (golden
//! path §5, §6). Composite requests enter child frames; pending leaves resume
//! without redispatching ancestors. State ownership and cleanup are flat and
//! iterative. The shard contract it runs is in [`crate::shard`].

mod arena;
mod engine;
pub mod runtime;
pub mod shards;

pub use engine::Control;
pub(crate) use engine::{Engine, Failed, failure_path};
pub use runtime::Mesh;
