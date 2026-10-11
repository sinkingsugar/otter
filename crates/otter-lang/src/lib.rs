//! Otter: the Shards language frontend, source text to wire definitions.
//!
//! [`parser`] reads source into a syntax tree with spans (hand-written, with
//! recovery and messages in language terms); lowering turns it into
//! `WireDef`/`ShardDef` against a [`otter_core::Catalog`], recording where
//! each shard came from in a source map, so compose diagnostics (which carry
//! occurrence paths, never source positions) can be located.
//! docs/surface-syntax-review.md has the syntax decisions.

pub mod ast;
pub mod files;
pub mod lexer;
pub mod lower;
pub mod parser;
pub mod problem;
pub mod program;
pub mod session;
pub mod source;
pub mod watch;

pub use files::{Files, FsFiles, MemoryFiles};
pub use parser::parse;
pub use problem::{Problem, render};
pub use program::{CheckReport, Program, RunReport, check, check_with};
pub use session::{Finished, Session};
pub use source::{Source, Span};

pub use watch::{FileWatcher, WatchControl, WatchEvent};
