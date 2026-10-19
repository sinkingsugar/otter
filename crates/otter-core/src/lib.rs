//! Otter: the runtime core.
//!
//! The design is in `docs/shards-2-compose-split.md`, the shard contract in
//! `docs/prototype-shard-contract.md` and the current plan in
//! `docs/golden-path.md`. Compose output ([`Shard::Compiled`]) is shared and
//! immutable; each instance owns only its [`Shard::State`] and its local
//! frame. [`Mesh`] schedules instances on an iterative, directly resumable
//! engine (`stackless`), the same on native, wasm and ESP-IDF.

pub mod args;
pub mod bench;
pub mod catalog;
pub mod compose;
pub mod compose_time;
pub mod describe;
pub mod diagnostic;
pub mod error;
pub mod flow;
pub mod fs_access;
pub mod function;
pub mod hash;
#[doc(hidden)]
pub mod inline;
pub mod instance;
pub mod json;
mod lifecycle;
pub mod log;
pub mod object;
pub mod reload;
pub mod service;
pub mod shard;
pub mod shards;
pub mod signature;
pub mod stackless;
pub mod types;
pub mod var;
mod wait;
pub mod wire_instance;

pub use args::{Arg, Args};
pub use catalog::Catalog;
pub use compose::{CacheStats, CompiledWire, ComposeCache, ComposeCtx, WireDef};
pub use compose_time::{Budget, EvalLimits, EvalUsage, RunBudget};
pub use describe::ShardDesc;
pub use diagnostic::Diagnostic;
pub use error::{Error, Result};
pub use function::{FlowType, FunctionDef, FunctionParam};
pub use instance::{InstanceId, InstanceMemory, Outcome, WakeMode};
pub use reload::{ReloadReport, ResetPolicy};
pub use shard::{
  ActivationCtx, Composed, ControlShard, ControlStep, Flow, ParamValue, Shard, ShardDef, ShardType,
  Step,
};
pub use stackless::Mesh;
pub use types::{Shape, Type, TypeDesc};
pub use var::{Float2, Float3, Float4, Keys, Table, TableBuilder, Values, Var};
pub use wait::CancelToken;
