//! M4 gate measurements: true pending re-polls and full completion/unwind.
//! 1000 instances; identical workloads on both schedulers. Mixed-flow throughput
//! is measured separately with bench_instances. See bench/trampoline/run.py.

use std::time::Instant;

use otter_core::args::Args;
use otter_core::compose::ComposeCtx;
use otter_core::instance::LeafCtx;
use otter_core::shards::async_shard::{AsyncShard, async_type};
use otter_core::shards::defs::*;
use otter_core::{Composed, FunctionDef, Result, ShardDef, ShardDesc, ShardType};
use otter_core::{Type, Var, WireDef};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

static READY: AtomicBool = AtomicBool::new(false);
struct Wait;
struct Pending(bool);
impl Future for Pending {
  type Output = Result<Var>;
  fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
    if self.0 && READY.load(Ordering::Relaxed) {
      Poll::Ready(Ok(Var::None))
    } else {
      self.0 = true;
      Poll::Pending
    }
  }
}
impl AsyncShard for Wait {
  type Compiled = ();
  type Op = Pending;
  const DESC: ShardDesc = ShardDesc::undocumented("Bench.Wait", 1);
  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::none(),
    })
  }
  fn start(_: &(), _: &mut impl LeafCtx, _: &Var) -> Result<Pending> {
    Ok(Pending(false))
  }
}
static WAIT: ShardType = async_type::<Wait>();

const INSTANCES: usize = 1000;
const WARMUP: u32 = 10;
const TICKS: u32 = 1000;

/// `depth` nested functions, each calling the next; the deepest holds the
/// leaf. Every level declares the mesh counter it reaches through its
/// callees (golden path §3.1).
fn functions(depth: usize, progress: bool) -> Vec<FunctionDef> {
  let level = |n: usize, body: Vec<ShardDef>| {
    FunctionDef::new(&format!("Level{n}"), Type::none(), Type::any())
      .uses(&["resumes"])
      .mutates(&["resumes"])
      .body(body)
  };
  let mut functions = vec![level(
    depth,
    if progress {
      // Retain the pre-M4 workload unchanged: each Pause completes on resume,
      // then While performs work and starts a new Pause at the bottom.
      vec![while_(
        vec![konst(Var::Bool(true))],
        vec![inc("resumes"), pause()],
      )]
    } else {
      vec![ShardDef::new(&WAIT, vec![]), inc("resumes")]
    },
  )];
  for n in 0..depth {
    functions.push(level(n, vec![call(&format!("Level{}", n + 1), vec![])]));
  }
  functions
}

fn wires() -> Vec<WireDef> {
  vec![WireDef {
    name: "root".into(),
    looped: true,
    flow: vec![call("Level0", vec![])],
  }]
}

macro_rules! run_depth {
  ($mesh:expr, $depth:expr, $progress:expr) => {{
    let mut mesh = $mesh;
    mesh.declare_var("resumes", Var::Int(0), true);
    for def in functions($depth, $progress) {
      mesh.add_function(def);
    }
    for def in wires() {
      mesh.add_wire(def);
    }
    let root = mesh.compile("root", Type::none()).expect("compile");
    for _ in 0..INSTANCES {
      mesh.spawn(&root, Var::None).expect("spawn");
    }
    READY.store(false, Ordering::Relaxed);
    for _ in 0..WARMUP {
      mesh.tick();
    }
    if $progress {
      let before = mesh.get_var("resumes").unwrap();
      let t = Instant::now();
      for _ in 0..TICKS {
        mesh.tick();
      }
      let ns = t.elapsed().as_secs_f64() * 1e9 / f64::from(TICKS);
      let Var::Int(before) = before else {
        unreachable!()
      };
      assert_eq!(
        mesh.get_var("resumes"),
        Some(Var::Int(before + INSTANCES as i64 * i64::from(TICKS)))
      );
      (ns, 0.0)
    } else {
      let t = Instant::now();
      for _ in 0..TICKS {
        mesh.tick();
      }
      let pending_ns = t.elapsed().as_secs_f64() * 1e9 / f64::from(TICKS);
      assert_eq!(mesh.get_var("resumes"), Some(Var::Int(0)));
      READY.store(true, Ordering::Relaxed);
      let mut completion = std::time::Duration::ZERO;
      for _ in 0..TICKS {
        let t = Instant::now();
        mesh.tick();
        completion += t.elapsed();
        // Entry/start is outside the completion sample. Each operation's first
        // poll always parks, even when ready, so the timed tick unwinds it fully.
        mesh.tick();
      }
      assert_eq!(
        mesh.get_var("resumes"),
        Some(Var::Int(INSTANCES as i64 * i64::from(TICKS)))
      );
      (
        pending_ns,
        completion.as_secs_f64() * 1e9 / f64::from(TICKS),
      )
    }
  }};
}

fn main() {
  let progress = std::env::args().any(|a| a == "--progress");
  // The `scheduler` field is kept in the output for the archived runners.
  let scheduler = "stackless";
  // Include the root/callee levels within the current 48-level compose limit.
  for depth in [1, 4, 16, 32] {
    let (pending_ns, completion_ns) = run_depth!(otter_core::Mesh::new(), depth, progress);
    let phases = if progress {
      vec![("progress", pending_ns)]
    } else {
      vec![("pending", pending_ns), ("completion", completion_ns)]
    };
    for (phase, tick_ns) in phases {
      println!(
        "scheduler={scheduler} phase={phase} depth={depth} instances={INSTANCES} ticks={TICKS} ns_per_instance_tick={:.2}",
        tick_ns / INSTANCES as f64
      );
    }
  }
}
