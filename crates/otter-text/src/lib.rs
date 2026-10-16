//! Otter text shards that need third-party crates: regular expressions
//! ([`regex`], on the Rust `regex` engine), hashes ([`hash`]) and random
//! identifiers ([`id`]). All leaf shards, on every target the core's tests
//! run on except ESP-IDF (wasm included).
//!
//! The core's own text shards (`String.*`, encodings, JSON) are in
//! `otter_core::shards::text`; this crate keeps the dependencies out of the
//! core and out of builds that do not want them.

pub mod hash;
pub mod id;
pub mod regex;

/// Every shard type in this crate, for building a `otter_core::Catalog`.
pub static CATALOG: &[&otter_core::ShardType] = &[
  &regex::MATCH,
  &regex::SEARCH,
  &regex::REPLACE,
  &hash::SHA2_256,
  &hash::SHA1,
  &hash::MD5,
  &hash::BLAKE3,
  &hash::BLAKE2_128,
  &hash::BLAKE2_256,
  &hash::XXH3_64,
  &hash::XXH3_128,
  &id::UUID,
  &id::NANOID,
];
