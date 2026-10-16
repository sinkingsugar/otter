//! Hashes of a string's UTF-8 bytes or of a byte string, following 1.x's
//! `Hash.*` shards (`shards/modules/crypto/src/hash.rs`) where 1.x had the
//! algorithm: `Hash.Sha2-256`, `Hash.Blake2-128`, `Hash.Blake2-256`,
//! `Hash.XXH3-64`, `Hash.XXH3-128`; and `Hash.Sha1`, `Hash.Md5` and
//! `Hash.Blake3`, new in 2.0.
//!
//! Cryptographic digests are Bytes in the algorithm's standard byte order
//! (what `ToHex` prints is what `sha256sum`, `b3sum` and the like print).
//! XXH3 is not cryptographic: its 64-bit hash is an Int (the hash's bits,
//! so it may be negative; `ToHex` prints its digits, without `xxhsum`'s
//! leading zeros), and its
//! 128-bit hash is 16 bytes in the canonical, big-endian order `xxhsum`
//! prints. SHA-1 and MD5 are broken for security; they are here to match
//! checksums other systems publish.

use otter_core::compose::ComposeCtx;
use otter_core::describe::{InputDesc, OutputDesc, Params, ShardDesc, Targets, TypeName};
use otter_core::instance::{InstanceCtx, LeafCtx};
use otter_core::shard::Flow;
use otter_core::shards::leaf::{LeafShard, leaf_type};
use otter_core::{Args, Composed, Result, ShardType, Type, Var, shard_doc};
use sha2::Digest;

pub static SHA2_256: ShardType = leaf_type::<Hash<Sha2_256>>();
pub static SHA1: ShardType = leaf_type::<Hash<Sha1>>();
pub static MD5: ShardType = leaf_type::<Hash<Md5>>();
pub static BLAKE3: ShardType = leaf_type::<Hash<Blake3>>();
pub static BLAKE2_128: ShardType = leaf_type::<Hash<Blake2_128>>();
pub static BLAKE2_256: ShardType = leaf_type::<Hash<Blake2_256>>();
pub static XXH3_64: ShardType = leaf_type::<Hash<Xxh3_64>>();
pub static XXH3_128: ShardType = leaf_type::<Hash<Xxh3_128>>();

/// String or Bytes, as the core's encoders take them.
const HASHED: &[TypeName] = &[TypeName::Bytes, TypeName::String];

const fn desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  output: TypeName,
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(&[]),
    input: InputDesc::Types(HASHED),
    output: OutputDesc::Fixed(output),
    targets: Targets::All,
    aliases: &[],
    effects: otter_core::signature::Effects::NONE,
    lifetime: otter_core::signature::Lifetime::Stateless,
  }
}

/// One hash algorithm.
pub trait Algorithm: 'static {
  const DESC: ShardDesc;
  fn output() -> Type;
  fn hash(data: &[u8]) -> Var;
}

/// A shard outputting the hash of its input.
pub struct Hash<A>(std::marker::PhantomData<fn() -> A>);

impl<A: Algorithm> LeafShard for Hash<A> {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = A::DESC;

  /// The input type is checked from the description before compose runs.
  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: A::output(),
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, input: &Var) -> Result<Flow> {
    let data = match input {
      Var::String(s) => s.as_bytes(),
      Var::Bytes(b) => &b[..],
      other => unreachable!("{}: input checked at compose, got {other:?}", A::DESC.name),
    };
    Ok(Flow::Next(A::hash(data)))
  }
}

pub struct Sha2_256;
impl Algorithm for Sha2_256 {
  const DESC: ShardDesc = desc(
    "Hash.Sha2-256",
    shard_doc!("Outputs the SHA-256 digest of a string's UTF-8 bytes or of a byte string."),
    shard_doc!("32 bytes; `ToHex` prints them as `sha256sum` does."),
    TypeName::Bytes,
  );
  fn output() -> Type {
    Type::bytes()
  }
  fn hash(data: &[u8]) -> Var {
    Var::bytes(&sha2::Sha256::digest(data))
  }
}

pub struct Sha1;
impl Algorithm for Sha1 {
  const DESC: ShardDesc = desc(
    "Hash.Sha1",
    shard_doc!("Outputs the SHA-1 digest of a string's UTF-8 bytes or of a byte string."),
    shard_doc!(
      "20 bytes. SHA-1 is broken for security (collisions are practical): use it only to match checksums other systems publish, and `Hash.Sha2-256` or `Hash.Blake3` otherwise."
    ),
    TypeName::Bytes,
  );
  fn output() -> Type {
    Type::bytes()
  }
  fn hash(data: &[u8]) -> Var {
    Var::bytes(&sha1::Sha1::digest(data))
  }
}

pub struct Md5;
impl Algorithm for Md5 {
  const DESC: ShardDesc = desc(
    "Hash.Md5",
    shard_doc!("Outputs the MD5 digest of a string's UTF-8 bytes or of a byte string."),
    shard_doc!(
      "16 bytes. MD5 is broken for security: use it only to match checksums other systems publish, and `Hash.Sha2-256` or `Hash.Blake3` otherwise."
    ),
    TypeName::Bytes,
  );
  fn output() -> Type {
    Type::bytes()
  }
  fn hash(data: &[u8]) -> Var {
    Var::bytes(&md5::Md5::digest(data))
  }
}

pub struct Blake3;
impl Algorithm for Blake3 {
  const DESC: ShardDesc = desc(
    "Hash.Blake3",
    shard_doc!("Outputs the BLAKE3 hash of a string's UTF-8 bytes or of a byte string."),
    shard_doc!(
      "32 bytes (BLAKE3's default length), unkeyed; `ToHex` prints them as `b3sum` does. Cryptographic, and faster than SHA-256."
    ),
    TypeName::Bytes,
  );
  fn output() -> Type {
    Type::bytes()
  }
  fn hash(data: &[u8]) -> Var {
    Var::bytes(blake3::hash(data).as_bytes())
  }
}

pub struct Blake2_128;
impl Algorithm for Blake2_128 {
  const DESC: ShardDesc = desc(
    "Hash.Blake2-128",
    shard_doc!("Outputs the 128-bit BLAKE2b hash of a string's UTF-8 bytes or of a byte string."),
    shard_doc!(
      "16 bytes: BLAKE2b with a 16-byte output length (`b2sum -l 128`), unkeyed. A compact cryptographic hash, as 1.x's."
    ),
    TypeName::Bytes,
  );
  fn output() -> Type {
    Type::bytes()
  }
  fn hash(data: &[u8]) -> Var {
    Var::bytes(&blake2::Blake2b::<blake2::digest::consts::U16>::digest(
      data,
    ))
  }
}

pub struct Blake2_256;
impl Algorithm for Blake2_256 {
  const DESC: ShardDesc = desc(
    "Hash.Blake2-256",
    shard_doc!("Outputs the 256-bit BLAKE2b hash of a string's UTF-8 bytes or of a byte string."),
    shard_doc!("32 bytes: BLAKE2b with a 32-byte output length (`b2sum -l 256`), unkeyed."),
    TypeName::Bytes,
  );
  fn output() -> Type {
    Type::bytes()
  }
  fn hash(data: &[u8]) -> Var {
    Var::bytes(&blake2::Blake2b::<blake2::digest::consts::U32>::digest(
      data,
    ))
  }
}

pub struct Xxh3_64;
impl Algorithm for Xxh3_64 {
  const DESC: ShardDesc = desc(
    "Hash.XXH3-64",
    shard_doc!(
      "Outputs the 64-bit XXH3 hash of a string's UTF-8 bytes or of a byte string, as an Int."
    ),
    shard_doc!(
      "Seed 0. The Int holds the hash's 64 bits, so it may be negative; `ToHex` prints its hex digits (with a `0x` prefix and without `xxhsum`'s leading zeros). Fast and well distributed, not cryptographic: for keys and checksums, not for anything an adversary chooses."
    ),
    TypeName::Int,
  );
  fn output() -> Type {
    Type::int()
  }
  fn hash(data: &[u8]) -> Var {
    Var::Int(xxhash_rust::xxh3::xxh3_64(data) as i64)
  }
}

pub struct Xxh3_128;
impl Algorithm for Xxh3_128 {
  const DESC: ShardDesc = desc(
    "Hash.XXH3-128",
    shard_doc!("Outputs the 128-bit XXH3 hash of a string's UTF-8 bytes or of a byte string."),
    shard_doc!(
      "Seed 0. 16 bytes in XXH3's canonical (big-endian) order, so `ToHex` prints it as `xxhsum -H2` does. Not cryptographic."
    ),
    TypeName::Bytes,
  );
  fn output() -> Type {
    Type::bytes()
  }
  fn hash(data: &[u8]) -> Var {
    Var::bytes(&xxhash_rust::xxh3::xxh3_128(data).to_be_bytes())
  }
}
