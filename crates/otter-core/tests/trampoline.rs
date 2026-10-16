//! Structural M4 gate: a pending leaf is resumed directly at every depth.
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use otter_core::args::Args;
use otter_core::compose::ComposeCtx;
use otter_core::instance::LeafCtx;
use otter_core::shards::async_shard::{AsyncShard, async_type};
use otter_core::shards::defs::*;
use otter_core::{
  Composed, Error, Mesh, Outcome, Result, ShardDef, ShardDesc, ShardType, Type, Var, WakeMode,
  WireDef,
};

#[derive(Default)]
struct Gate {
  ready: AtomicBool,
  fail: AtomicBool,
  starts: AtomicUsize,
  polls: AtomicUsize,
  drops: AtomicUsize,
  wakes: Mutex<Vec<Waker>>,
}
thread_local! { static GATE: RefCell<Option<Arc<Gate>>> = const { RefCell::new(None) }; }
struct Wait;
struct Pending {
  gate: Arc<Gate>,
  registered: bool,
}
impl Future for Pending {
  type Output = Result<Var>;
  fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
    self.gate.polls.fetch_add(1, Ordering::Relaxed);
    if !self.registered {
      self.gate.wakes.lock().unwrap().push(ctx.waker().clone());
      self.registered = true;
    }
    if self.gate.ready.load(Ordering::Relaxed) {
      Poll::Ready(if self.gate.fail.load(Ordering::Relaxed) {
        Err(Error::Activation("controlled failure".into()))
      } else {
        Ok(Var::Int(42))
      })
    } else {
      Poll::Pending
    }
  }
}
impl Drop for Pending {
  fn drop(&mut self) {
    self.gate.drops.fetch_add(1, Ordering::Relaxed);
  }
}
impl AsyncShard for Wait {
  type Compiled = ();
  type Op = Pending;
  const DESC: ShardDesc = ShardDesc::undocumented("Test.Wait", 1);
  fn compose(_: &Args, _: &mut ComposeCtx<'_>) -> Result<Composed<()>> {
    Ok(Composed {
      compiled: (),
      output: Type::int(),
    })
  }
  fn start(_: &(), _: &mut impl LeafCtx, _: &Var) -> Result<Pending> {
    let gate = GATE.with(|g| g.borrow().as_ref().unwrap().clone());
    gate.starts.fetch_add(1, Ordering::Relaxed);
    Ok(Pending {
      gate,
      registered: false,
    })
  }
}
static WAIT: ShardType = async_type::<Wait>();
fn gate() -> Arc<Gate> {
  let gate = Arc::new(Gate::default());
  GATE.with(|g| *g.borrow_mut() = Some(gate.clone()));
  gate
}
fn nested(depth: usize) -> Vec<ShardDef> {
  let mut flow = vec![ShardDef::new(&WAIT, vec![])];
  for i in 0..depth {
    flow = vec![match i % 4 {
      0 => if_(
        vec![konst(Var::Bool(true))],
        flow,
        Some(vec![konst(Var::Int(0))]),
      ),
      1 => repeat(flow, val(Var::Int(1))),
      2 => maybe(flow, Some(vec![konst(Var::Int(-1))])),
      _ => sub(flow),
    }];
  }
  flow
}
/// Like `nested`, with a stateless function call at every fourth level:
/// each invocation gets its own frame, and a pending leaf inside it must
/// still resume directly.
fn nested_with_functions(mesh: &mut Mesh, depth: usize) -> Vec<ShardDef> {
  let mut flow = vec![ShardDef::new(&WAIT, vec![])];
  for i in 0..depth {
    flow = vec![match i % 4 {
      0 => {
        let name = format!("Level{i}");
        mesh.add_function(otter_core::FunctionDef::new(&name, Type::int(), Type::int()).body(flow));
        call(&name, vec![])
      }
      1 => repeat(flow, val(Var::Int(1))),
      2 => maybe(flow, Some(vec![konst(Var::Int(-1))])),
      _ => sub(flow),
    }];
  }
  flow
}
fn wire(flow: Vec<ShardDef>, looped: bool) -> WireDef {
  WireDef {
    name: "root".into(),
    looped,
    flow,
  }
}

/// Nesting depths exercised; the device composes at most 24 flow levels
/// (`MAX_FLOW_DEPTH` on ESP-IDF).
const DEPTHS: [usize; 4] = if cfg!(target_os = "espidf") {
  [1, 4, 16, 20]
} else {
  [1, 4, 16, 32]
};

/// The levels of `nested` that are frames of their own: `Maybe` and `Sub`.
/// `If` and `Repeat` are flat code in their parent's frame and dispatch
/// nothing.
fn framed(depth: usize) -> u64 {
  (0..depth).filter(|i| i % 4 >= 2).count() as u64
}

/// The levels of `nested_with_functions` that are frames of their own: the
/// call, `Maybe` and `Sub`.
fn framed_with_functions(depth: usize) -> u64 {
  (0..depth).filter(|i| i % 4 != 1).count() as u64
}

#[test]
fn pending_repolls_dispatch_zero_ancestors_at_every_depth() {
  for depth in DEPTHS {
    let gate = gate();
    let mut mesh = Mesh::new();
    mesh.add_wire(wire(nested(depth), false));
    let code = mesh.compile("root", Type::int()).unwrap();
    let id = mesh.spawn(&code, Var::Int(7)).unwrap();
    mesh.tick();
    let dispatches = mesh.composite_dispatches();
    assert!(dispatches >= framed(depth));
    let owners = Arc::strong_count(&gate);
    for _ in 0..100 {
      mesh.tick();
    }
    assert_eq!(mesh.composite_dispatches(), dispatches, "depth {depth}");
    assert_eq!(gate.starts.load(Ordering::Relaxed), 1);
    assert_eq!(gate.polls.load(Ordering::Relaxed), 101);
    assert_eq!(Arc::strong_count(&gate), owners);
    gate.ready.store(true, Ordering::Relaxed);
    mesh.tick();
    assert!(matches!(mesh.outcome(id), Some(Outcome::Completed(_))));
    assert_eq!(gate.drops.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&gate), owners - 1);
  }
}

#[test]
fn errors_reach_the_nearest_handler_and_loop_continuations_reset() {
  let gate = gate();
  gate.fail.store(true, Ordering::Relaxed);
  let mut mesh = Mesh::new();
  mesh.declare_var("caught", Var::Int(0), true);
  mesh.add_wire(wire(
    vec![maybe(
      vec![repeat(vec![ShardDef::new(&WAIT, vec![])], val(Var::Int(2)))],
      Some(vec![inc("caught")]),
    )],
    true,
  ));
  let code = mesh.compile("root", Type::none()).unwrap();
  let id = mesh.spawn(&code, Var::None).unwrap();
  for expected in 1..=20 {
    gate.ready.store(false, Ordering::Relaxed);
    mesh.tick();
    let dispatches = mesh.composite_dispatches();
    mesh.tick();
    assert_eq!(mesh.composite_dispatches(), dispatches);
    gate.ready.store(true, Ordering::Relaxed);
    mesh.tick();
    assert_eq!(mesh.get_var("caught"), Some(Var::Int(expected)));
    assert_eq!(gate.starts.load(Ordering::Relaxed), expected as usize);
    assert_eq!(gate.drops.load(Ordering::Relaxed), expected as usize);
  }
  mesh.cancel(id);
}

#[test]
fn stale_wakes_do_not_resume_cancelled_or_replacement_instances() {
  let gate = gate();
  let mut mesh = Mesh::new();
  mesh.set_wake_mode(WakeMode::OnNotify);
  mesh.add_wire(wire(nested(16), false));
  let code = mesh.compile("root", Type::int()).unwrap();
  let old = mesh.spawn(&code, Var::Int(1)).unwrap();
  mesh.tick();
  let stale = gate.wakes.lock().unwrap()[0].clone();
  mesh.cancel(old);
  assert_eq!(mesh.take_outcome(old), Some(Outcome::Cancelled));
  let new = mesh.spawn(&code, Var::Int(2)).unwrap();
  mesh.tick();
  let polls = gate.polls.load(Ordering::Relaxed);
  stale.wake_by_ref();
  mesh.tick();
  assert_eq!(gate.polls.load(Ordering::Relaxed), polls);
  assert!(mesh.outcome(new).is_none());
  gate.ready.store(true, Ordering::Relaxed);
  gate.wakes.lock().unwrap()[1].wake_by_ref();
  mesh.tick();
  assert!(matches!(mesh.outcome(new), Some(Outcome::Completed(_))));
  assert_eq!(gate.drops.load(Ordering::Relaxed), 2);
}

#[test]
fn completing_nested_frames_dispatches_each_parent_once() {
  for depth in DEPTHS {
    let gate = gate();
    let mut mesh = Mesh::new();
    mesh.add_wire(wire(nested(depth), true));
    let code = mesh.compile("root", Type::int()).unwrap();
    let id = mesh.spawn(&code, Var::Int(7)).unwrap();
    mesh.tick();
    let before = mesh.composite_dispatches();
    gate.ready.store(true, Ordering::Relaxed);
    mesh.tick();
    assert_eq!(mesh.composite_dispatches() - before, framed(depth));
    mesh.cancel(id);
  }
}

/// `Mesh::wait` blocks the host thread until a tick can make progress:
/// it wakes for a waker fired on another thread, a due pause, its own time
/// limit or a cancellation, and returns at once when an instance is ready.
#[cfg(not(any(target_family = "wasm", target_os = "espidf")))]
mod wait {
  use super::*;
  use otter_core::CancelToken;
  use std::time::{Duration, Instant};

  /// Ticks and waits until `id` finishes; returns how many waits it took.
  fn run(mesh: &mut Mesh, id: otter_core::InstanceId) -> usize {
    let mut waits = 0;
    mesh.tick();
    while mesh.outcome(id).is_none() {
      mesh.wait(Some(Instant::now() + Duration::from_secs(10)), None);
      waits += 1;
      mesh.tick();
    }
    waits
  }

  #[test]
  fn it_sleeps_until_a_waker_fires_on_another_thread() {
    for mode in [WakeMode::PollEveryTick, WakeMode::OnNotify] {
      let gate = gate();
      let mut mesh = Mesh::new();
      mesh.set_wake_mode(mode);
      mesh.add_wire(wire(vec![ShardDef::new(&WAIT, vec![])], false));
      let code = mesh.compile("root", Type::none()).unwrap();
      let id = mesh.spawn(&code, Var::None).unwrap();
      let waker = {
        let gate = gate.clone();
        std::thread::spawn(move || {
          std::thread::sleep(Duration::from_millis(100));
          gate.ready.store(true, Ordering::Relaxed);
          for waker in gate.wakes.lock().unwrap().iter() {
            waker.wake_by_ref();
          }
        })
      };
      let started = Instant::now();
      let waits = run(&mut mesh, id);
      waker.join().unwrap();
      assert!(started.elapsed() >= Duration::from_millis(100), "{mode:?}");
      // Woken by the waker, not by `run`'s 10 s limit.
      assert!(started.elapsed() < Duration::from_secs(5), "{mode:?}");
      // Woken once (a spurious wake may add one), not polled.
      assert!(waits <= 2, "{mode:?}: {waits} waits");
      assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(42))));
    }
  }

  #[test]
  fn it_sleeps_until_a_timed_pause_is_due() {
    let mut mesh = Mesh::new();
    mesh.add_wire(wire(vec![pause_secs(0.1), konst(Var::Int(1))], false));
    let code = mesh.compile("root", Type::none()).unwrap();
    let id = mesh.spawn(&code, Var::None).unwrap();
    let started = Instant::now();
    let waits = run(&mut mesh, id);
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(waits <= 2, "{waits} waits");
    assert_eq!(mesh.outcome(id), Some(&Outcome::Completed(Var::Int(1))));
  }

  #[test]
  fn it_stops_at_its_limit_or_a_cancellation() {
    let _gate = gate();
    let mut mesh = Mesh::new();
    mesh.add_wire(wire(vec![ShardDef::new(&WAIT, vec![])], false));
    let code = mesh.compile("root", Type::none()).unwrap();
    mesh.spawn(&code, Var::None).unwrap();
    mesh.tick();
    let started = Instant::now();
    mesh.wait(Some(started + Duration::from_millis(50)), None);
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert!(started.elapsed() < Duration::from_secs(5));

    let token = CancelToken::new();
    let canceller = {
      let token = token.clone();
      std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        token.cancel();
      })
    };
    let started = Instant::now();
    mesh.wait(Some(started + Duration::from_secs(10)), Some(&token));
    canceller.join().unwrap();
    assert!(token.is_cancelled());
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert!(started.elapsed() < Duration::from_secs(5));
    // Already cancelled: no wait.
    let started = Instant::now();
    mesh.wait(None, Some(&token));
    assert!(started.elapsed() < Duration::from_secs(1));
  }

  #[test]
  fn it_returns_at_once_when_a_tick_has_work() {
    let mut mesh = Mesh::new();
    // Nothing running.
    let started = Instant::now();
    mesh.wait(None, None);
    // Not started yet, then yielded (`Pause` with no time).
    mesh.add_wire(wire(vec![pause(), pause(), konst(Var::Int(1))], false));
    let code = mesh.compile("root", Type::none()).unwrap();
    let id = mesh.spawn(&code, Var::None).unwrap();
    mesh.wait(None, None);
    assert_eq!(run(&mut mesh, id), 2);
    assert!(started.elapsed() < Duration::from_secs(1));
  }

  #[test]
  fn a_pause_the_clock_cannot_reach_never_wakes() {
    let mut mesh = Mesh::new();
    mesh.add_wire(wire(vec![pause_secs(1e18)], false));
    let code = mesh.compile("root", Type::none()).unwrap();
    let id = mesh.spawn(&code, Var::None).unwrap();
    mesh.tick();
    let started = Instant::now();
    mesh.wait(Some(started + Duration::from_millis(50)), None);
    assert!(started.elapsed() >= Duration::from_millis(50));
    mesh.tick();
    assert!(mesh.outcome(id).is_none());
  }
}

#[cfg(not(target_os = "espidf"))]
mod allocations {
  use std::alloc::{GlobalAlloc, Layout, System};
  use std::cell::Cell;
  thread_local! { static LIVE: Cell<isize> = const { Cell::new(0) }; }
  thread_local! { static COUNT: Cell<usize> = const { Cell::new(0) }; }
  struct Count;
  // SAFETY: forwards each allocation unchanged to System. The counter is
  // thread-local and allocation-free; test instances never migrate threads.
  unsafe impl GlobalAlloc for Count {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
      let ptr = unsafe { System.alloc(layout) };
      if !ptr.is_null() {
        let _ = LIVE.try_with(|n| n.set(n.get() + layout.size() as isize));
        let _ = COUNT.try_with(|n| n.set(n.get() + 1));
      }
      ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
      let _ = LIVE.try_with(|n| n.set(n.get() - layout.size() as isize));
      unsafe {
        System.dealloc(ptr, layout);
      }
    }
  }
  #[global_allocator]
  static ALLOC: Count = Count;
  pub fn live() -> isize {
    LIVE.with(Cell::get)
  }
  pub fn count() -> usize {
    COUNT.with(Cell::get)
  }
}

/// A stateless call site keeps its invocation frames: after the first
/// call, repeated calls of a body with a suspension, a nested flow and a
/// callee allocate nothing and visit no ancestor on the re-poll.
#[cfg(not(target_os = "espidf"))]
#[test]
fn repeated_stateless_calls_reuse_their_frames_without_allocating() {
  let mut mesh = Mesh::new();
  mesh.declare_var("total", Var::Int(0), true);
  mesh.add_function(
    otter_core::FunctionDef::new("Leaf", Type::int(), Type::int())
      .uses(&["total"])
      .mutates(&["total"])
      .body(vec![pause(), inc("total")]),
  );
  mesh.add_function(
    otter_core::FunctionDef::new("Think", Type::int(), Type::int())
      .uses(&["total"])
      .mutates(&["total"])
      .body(vec![
        add(val(Var::Int(1))),
        declare("n"),
        when(
          vec![is_less(val(Var::Int(100)))],
          vec![call("Leaf", vec![])],
        ),
        get("n"),
      ]),
  );
  mesh.add_wire(wire(vec![call("Think", vec![])], true));
  let code = mesh.compile("root", Type::int()).unwrap();
  let id = mesh.spawn(&code, Var::Int(7)).unwrap();
  // Warm up: the first call builds the frames; one full cycle is two ticks.
  for _ in 0..4 {
    mesh.tick();
  }
  let (live, count) = (allocations::live(), allocations::count());
  let state = mesh.memory(id).unwrap().state_bytes;
  let dispatches = mesh.composite_dispatches();
  mesh.tick();
  mesh.tick();
  let per_cycle = mesh.composite_dispatches() - dispatches;
  for _ in 0..999 {
    mesh.tick();
    mesh.tick();
    assert_eq!(allocations::live(), live);
    assert_eq!(allocations::count(), count);
  }
  assert_eq!(mesh.memory(id).unwrap().state_bytes, state);
  // The same composite steps every cycle: Think and Leaf each take a
  // preparation step, an entry and a completion, When an entry and a
  // completion; the pause re-polls without visiting any of them.
  assert_eq!(mesh.composite_dispatches() - dispatches, 1000 * per_cycle);
  assert!(per_cycle <= 9, "{per_cycle} composite steps per cycle");
  assert_eq!(mesh.get_var("total"), Some(Var::Int(1002)));
  mesh.cancel(id);
}

#[cfg(not(target_os = "espidf"))]
#[test]
fn repeated_suspend_complete_cycles_do_not_grow_live_allocations_or_owners() {
  let gate = gate();
  let mut mesh = Mesh::new();
  mesh.add_wire(wire(nested(32), true));
  let code = mesh.compile("root", Type::int()).unwrap();
  let id = mesh.spawn(&code, Var::Int(7)).unwrap();
  mesh.tick();
  let live = allocations::live();
  let owners = Arc::strong_count(&gate);
  for _ in 0..1000 {
    for _ in 0..3 {
      mesh.tick();
    }
    gate.ready.store(true, Ordering::Relaxed);
    mesh.tick();
    gate.wakes.lock().unwrap().clear();
    gate.ready.store(false, Ordering::Relaxed);
    mesh.tick();
    assert_eq!(allocations::live(), live);
    assert_eq!(Arc::strong_count(&gate), owners);
  }
  mesh.cancel(id);
}

#[test]
fn call_depth_limit_is_checked_before_entering_the_named_body() {
  let gate = gate();
  let mut mesh = Mesh::new();
  mesh.set_max_call_depth(2);
  mesh.add_function(
    otter_core::FunctionDef::new("Deep", Type::int(), Type::int())
      .body(vec![ShardDef::new(&WAIT, vec![])]),
  );
  mesh.add_function(
    otter_core::FunctionDef::new("Middle", Type::int(), Type::int())
      .body(vec![call("Deep", vec![])]),
  );
  mesh.add_function(
    otter_core::FunctionDef::new("Outer", Type::int(), Type::int())
      .body(vec![call("Middle", vec![])]),
  );
  mesh.add_wire(wire(vec![call("Outer", vec![])], false));
  let code = mesh.compile("root", Type::int()).unwrap();
  let id = mesh.spawn(&code, Var::Int(7)).unwrap();
  mesh.tick();
  let Some(Outcome::Failed(Error::Diagnostic(d))) = mesh.outcome(id) else {
    panic!("expected limit error")
  };
  assert_eq!(d.code, "recursion-limit");
  assert_eq!(gate.starts.load(Ordering::Relaxed), 0);
  assert_eq!(mesh.revision().max_call_depth(), 2);
  mesh.set_max_call_depth(3);
  let id = mesh.spawn(&code, Var::Int(7)).unwrap();
  mesh.tick();
  assert_eq!(gate.starts.load(Ordering::Relaxed), 1);
  mesh.cancel(id);
}

#[test]
fn function_frames_in_the_nesting_resume_pending_leaves_directly() {
  for depth in DEPTHS {
    let gate = gate();
    let mut mesh = Mesh::new();
    let flow = nested_with_functions(&mut mesh, depth);
    mesh.add_wire(wire(flow, true));
    let code = mesh.compile("root", Type::int()).unwrap();
    let id = mesh.spawn(&code, Var::Int(7)).unwrap();
    mesh.tick();
    let dispatches = mesh.composite_dispatches();
    assert!(dispatches >= framed_with_functions(depth));
    let owners = Arc::strong_count(&gate);
    for _ in 0..100 {
      mesh.tick();
    }
    assert_eq!(mesh.composite_dispatches(), dispatches, "depth {depth}");
    assert_eq!(gate.starts.load(Ordering::Relaxed), 1);
    assert_eq!(gate.polls.load(Ordering::Relaxed), 101);
    assert_eq!(Arc::strong_count(&gate), owners);
    // Completing through every level, invocation frames included, costs
    // one dispatch per framed parent (a Repeat level is flat code), and the
    // next iteration enters fresh frames.
    gate.ready.store(true, Ordering::Relaxed);
    mesh.tick();
    assert_eq!(
      mesh.composite_dispatches() - dispatches,
      framed_with_functions(depth)
    );
    assert_eq!(gate.drops.load(Ordering::Relaxed), 1);
    assert_eq!(gate.starts.load(Ordering::Relaxed), 1);
    gate.ready.store(false, Ordering::Relaxed);
    mesh.tick();
    assert_eq!(gate.starts.load(Ordering::Relaxed), 2);
    mesh.cancel(id);
    assert_eq!(mesh.take_outcome(id), Some(Outcome::Cancelled));
    assert_eq!(gate.drops.load(Ordering::Relaxed), 2);
  }
}

#[cfg(not(target_os = "espidf"))]
#[test]
fn invocation_frames_are_released_on_every_exit() {
  let gate = gate();
  let mut mesh = Mesh::new();
  let flow = nested_with_functions(&mut mesh, 32);
  mesh.add_wire(wire(flow, true));
  let code = mesh.compile("root", Type::int()).unwrap();
  let id = mesh.spawn(&code, Var::Int(7)).unwrap();
  let cycle = |mesh: &mut Mesh| {
    for _ in 0..3 {
      mesh.tick();
    }
    gate.ready.store(true, Ordering::Relaxed);
    mesh.tick();
    gate.wakes.lock().unwrap().clear();
    gate.ready.store(false, Ordering::Relaxed);
    mesh.tick();
  };
  mesh.tick();
  // The first exit populates the arena's free list; measure after it.
  cycle(&mut mesh);
  let live = allocations::live();
  let owners = Arc::strong_count(&gate);
  for _ in 0..1000 {
    cycle(&mut mesh);
    assert_eq!(allocations::live(), live);
    assert_eq!(Arc::strong_count(&gate), owners);
  }
  mesh.cancel(id);
}
