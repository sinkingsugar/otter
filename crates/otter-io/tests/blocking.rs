//! Blocking work on the shared runtime's blocking pool, through one
//! `AsyncShard`: it completes, it does not block the mesh, and cancelling
//! the instance cancels the work's token.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use otter_core::Mesh;
use otter_core::args::Args;
use otter_core::compose::ComposeCtx;
use otter_core::describe::{InputDesc, OutputDesc, Params, ShardDesc, Targets, TypeName};
use otter_core::instance::LeafCtx;
use otter_core::shards::async_shard::{AsyncShard, async_type};
use otter_core::{Composed, Result, ShardType, Type, Var};
use otter_io::IoTask;
use otter_io::runtime::spawn_blocking;

/// Per test slot (so parallel tests cannot see each other's signals):
/// whether the blocking task started, and whether it saw its cancellation
/// token fire.
static STARTED: [AtomicBool; 2] = [const { AtomicBool::new(false) }; 2];
static SAW_CANCEL: [AtomicBool; 2] = [const { AtomicBool::new(false) }; 2];
static RELEASE: [AtomicBool; 2] = [const { AtomicBool::new(false) }; 2];

/// Positive slots block until cancelled; negative slots wait for the test
/// to release them after tick returns. No wall-clock scheduling assumption.
struct Block;

const BLOCK_DESC: ShardDesc = ShardDesc {
  name: "Test.Block",
  version: 1,
  summary: "",
  help: "",
  params: Params::Declared(&[]),
  input: InputDesc::Types(&[TypeName::Int]),
  output: OutputDesc::Fixed(TypeName::Int),
  targets: Targets::NativeOnly,
  aliases: &[],
  effects: otter_core::signature::Effects::UNKNOWN,
  lifetime: otter_core::signature::Lifetime::Unknown,
};

impl AsyncShard for Block {
  type Compiled = ();
  type Op = IoTask;
  const DESC: ShardDesc = BLOCK_DESC;

  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::int(),
    })
  }

  fn start(_: &(), _: &mut impl LeafCtx, input: &Var) -> Result<IoTask> {
    let Var::Int(input) = input else {
      unreachable!()
    };
    let slot = input.unsigned_abs() as usize;
    let cancel = *input > 0;
    Ok(spawn_blocking(move |token| {
      if cancel {
        STARTED[slot].store(true, Ordering::SeqCst);
        while !token.is_cancelled() {
          std::thread::sleep(Duration::from_millis(1));
        }
        SAW_CANCEL[slot].store(true, Ordering::SeqCst);
        Err("cancelled".into())
      } else {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !RELEASE[slot].load(Ordering::SeqCst) {
          if token.is_cancelled() || Instant::now() >= deadline {
            return Err("test did not release blocking operation".into());
          }
          std::thread::sleep(Duration::from_millis(1));
        }
        // Logged on a blocking-pool thread.
        otter_core::log::emit("scanned".into());
        Ok(Var::Int(42))
      }
    }))
  }
}

static BLOCK: ShardType = async_type::<Block>();

use otter_core::{Outcome, ShardDef, WireDef};

fn wire(input: i64) -> WireDef {
  WireDef {
    name: "w".into(),
    looped: false,
    flow: vec![
      otter_core::shards::defs::konst(Var::Int(input)),
      ShardDef::new(&BLOCK, vec![]),
    ],
  }
}

#[test]
fn blocking_work_completes_without_blocking_the_mesh() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(-1));
  let w = mesh.compile("w", Type::none()).unwrap();
  let id = mesh.spawn(&w, Var::None).unwrap();
  let ((), lines) = otter_core::log::capture(|| {
    let start = Instant::now();
    mesh.tick();
    // Completion cannot happen until the ticking thread releases the
    // worker. A busy CI host may deschedule this thread for any duration.
    assert!(mesh.outcome(id).is_none(), "tick waited for blocking work");
    RELEASE[1].store(true, Ordering::SeqCst);
    while mesh.outcome(id).is_none() {
      assert!(start.elapsed() < Duration::from_secs(5), "timed out");
      mesh.tick();
      std::thread::sleep(Duration::from_millis(1));
    }
  });
  assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(42))));
  // The pool thread's line reached the capture of the ticking thread.
  assert_eq!(lines, ["scanned"]);
}

#[test]
fn cancelling_the_instance_cancels_the_blocking_work() {
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(1));
  let w = mesh.compile("w", Type::none()).unwrap();
  let id = mesh.spawn(&w, Var::None).unwrap();
  let start = Instant::now();
  while !STARTED[1].load(Ordering::SeqCst) {
    assert!(start.elapsed() < Duration::from_secs(5), "never started");
    mesh.tick();
    std::thread::sleep(Duration::from_millis(1));
  }
  mesh.cancel(id);
  assert_eq!(mesh.outcome(id), Some(&Outcome::Cancelled));
  while !SAW_CANCEL[1].load(Ordering::SeqCst) {
    assert!(
      start.elapsed() < Duration::from_secs(5),
      "token never fired"
    );
    std::thread::sleep(Duration::from_millis(1));
  }
}
