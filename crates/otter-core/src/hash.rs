//! Hash maps with a fixed hasher, for the runtime's own tables.
//!
//! The standard maps seed SipHash randomly per process, so the probes a
//! lookup takes, and the instructions it executes, vary from run to run. The
//! instruction gate (docs/performance-gates.md) needs the same binary on the
//! same input to execute the same instructions, so the runtime's tables use
//! SipHash with fixed keys: the same function at the same cost, with one
//! layout. Their keys are names, types and storage from the program being
//! composed and run, which can already make the runtime do as much work as
//! it likes, so a fixed seed exposes nothing new. Maps a host passes in
//! (`defines`) stay standard.

use std::hash::{BuildHasherDefault, DefaultHasher};

/// SipHash with fixed keys.
pub type FixedState = BuildHasherDefault<DefaultHasher>;
pub type HashMap<K, V> = std::collections::HashMap<K, V, FixedState>;
pub type HashSet<T> = std::collections::HashSet<T, FixedState>;
