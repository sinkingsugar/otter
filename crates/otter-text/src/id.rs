//! Random identifiers, following 1.x (`shards/modules/core/src/uuid.rs`):
//! `UUID` (version 4) and `NanoID`. Both draw from the operating system's
//! generator (`getrandom`; WASI's `random_get` on wasm) at activation, and
//! declare the `random` effect. Differences from 1.x are listed in
//! docs/current-state.md ("Deviations from 1.x").

use otter_core::compose::ComposeCtx;
use otter_core::describe::{
  DefaultValue, Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets,
  TypeName,
};
use otter_core::diagnostic::{Diagnostic, Phase};
use otter_core::instance::{InstanceCtx, LeafCtx};
use otter_core::shard::Flow;
use otter_core::shards::leaf::{LeafShard, leaf_type};
use otter_core::signature::{Effects, Lifetime};
use otter_core::{Args, Composed, Error, Result, ShardType, Type, Var, shard_doc};

pub static UUID: ShardType = leaf_type::<Uuid>();
pub static NANOID: ShardType = leaf_type::<NanoId>();

const RANDOM: Effects = Effects {
  random: true,
  ..Effects::NONE
};

/// The longest NanoID `size` allows.
pub const MAX_NANOID_SIZE: i64 = 1024;

const fn desc(
  name: &'static str,
  summary: &'static str,
  help: &'static str,
  params: &'static [ParamDecl],
) -> ShardDesc {
  ShardDesc {
    name,
    version: 1,
    summary,
    help,
    params: Params::Declared(params),
    input: InputDesc::Ignored,
    output: OutputDesc::Fixed(TypeName::String),
    targets: Targets::All,
    aliases: &[],
    effects: RANDOM,
    lifetime: Lifetime::Stateless,
  }
}

fn random(buf: &mut [u8], shard: &str) -> Result<()> {
  getrandom::fill(buf)
    .map_err(|e| Error::Activation(format!("{shard}: no random bytes from the system: {e}")))
}

// --- UUID ---

pub struct Uuid;
impl LeafShard for Uuid {
  type Compiled = ();
  type State = ();
  const DESC: ShardDesc = desc(
    "UUID",
    shard_doc!("Outputs a new random UUID (version 4) as a String."),
    shard_doc!(
      "In the standard form: 36 characters, lowercase hex in groups of 8, 4, 4, 4 and 12 separated by `-` (`7c9e6679-7425-40de-944b-e07fc1f90ae7`). 122 of its bits are random, from the system's generator."
    ),
    &[],
  );

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::string(),
    })
  }

  fn instantiate(_: &(), _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(_: &(), _: &mut (), _: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    let mut bytes = [0u8; 16];
    random(&mut bytes, Self::DESC.name)?;
    let uuid = uuid::Builder::from_random_bytes(bytes).into_uuid();
    let mut text = [0u8; uuid::fmt::Hyphenated::LENGTH];
    Ok(Flow::Next(Var::string(
      uuid.hyphenated().encode_lower(&mut text),
    )))
  }
}

// --- NanoID ---

/// NanoID's default alphabet: 64 URL-safe characters, so each takes 6 bits
/// of a random byte with no bias.
const ALPHABET: &[u8; 64] = b"useandom-26T198340PX75pxJACKVERYMINDBUSHWOLF_GQZbfghjklqvwyzrict";

static NANOID_PARAMS: &[ParamDecl] = &[ParamDecl::new(
  "size",
  shard_doc!("How many characters the ID has: 1 to 1024 (21, the default, has 126 random bits)."),
  Forms::LITERAL,
  &[TypeName::Int],
  Requirement::Default(DefaultValue::Int(21)),
)];

pub struct NanoId;
impl LeafShard for NanoId {
  type Compiled = usize;
  type State = ();
  const DESC: ShardDesc = desc(
    "NanoID",
    shard_doc!("Outputs a new random NanoID: a short, URL-safe random String."),
    shard_doc!(
      "Each character is one of `A-Z`, `a-z`, `0-9`, `_` and `-`, uniformly random from the system's generator."
    ),
    NANOID_PARAMS,
  );

  fn compose(args: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<usize>> {
    let size = args.int("size").expect("decoded size");
    if !(1..=MAX_NANOID_SIZE).contains(&size) {
      return Err(Error::Diagnostic(Box::new(
        Diagnostic::new(
          Phase::Compose,
          "compose-error",
          "invalid-argument-value",
          format!("size must be from 1 to {MAX_NANOID_SIZE}, got {size}"),
        )
        .shard(Self::DESC.name)
        .param("size", Some(args.param_index("size"))),
      )));
    }
    Ok(Composed {
      compiled: size as usize,
      output: Type::string(),
    })
  }

  fn instantiate(_: &usize, _: &mut InstanceCtx) -> Result<()> {
    Ok(())
  }

  fn activate(size: &usize, _: &mut (), _: &mut impl LeafCtx, _: &Var) -> Result<Flow> {
    let mut bytes = vec![0u8; *size];
    random(&mut bytes, Self::DESC.name)?;
    for b in &mut bytes {
      *b = ALPHABET[usize::from(*b & 63)];
    }
    let text = std::str::from_utf8(&bytes).expect("the alphabet is ASCII");
    Ok(Flow::Next(Var::string(text)))
  }
}
