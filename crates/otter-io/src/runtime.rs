//! The shared Tokio runtime and the tasks shards run on it.
//!
//! Follows 1.x (`TOKIO_RUNTIME` and `run_future` in `shards/modules/http`):
//! one multi-threaded runtime (4 workers) shared by all I/O shards. A shard
//! spawns its work here and gets an [`IoTask`]. The `AsyncShard` adapter
//! polls that task from the mesh thread with the instance's waker, so the
//! mesh never blocks, on either scheduler.
//!
//! **Cancellation.** Dropping an [`IoTask`] (which the adapter does when the
//! instance is cancelled, or the shard's state is cleaned up) cancels the
//! task's [`CancellationToken`], like 1.x's `run_future` cancel callback and
//! `cleanup`. The task must race its awaits against the token, and drops its
//! in-flight work when it fires. Note that dropping a Tokio `JoinHandle`
//! alone would only detach the task, not stop it. Cancellation cannot undo
//! work the remote side already received.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use otter_core::{Error, Var};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
pub use tokio_util::sync::CancellationToken;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Whether the shared runtime has been created (reading shard descriptions
/// must not create it).
pub fn runtime_started() -> bool {
  RUNTIME.get().is_some()
}

/// As in 1.x: installs the ring provider once, before any TLS client is
/// built. Clients are built when a request starts, which can be before the
/// runtime exists, so building one calls this too.
pub(crate) fn install_tls_provider() {
  #[cfg(feature = "rustls-ring")]
  {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
      // A host that installed a provider of its own keeps it.
      let _ = rustls::crypto::ring::default_provider().install_default();
    });
  }
}

/// The shared runtime, created on first use.
pub fn runtime() -> &'static Runtime {
  RUNTIME.get_or_init(|| {
    install_tls_provider();
    tokio::runtime::Builder::new_multi_thread()
      .worker_threads(4)
      .enable_all()
      .build()
      .expect("Failed to create Tokio runtime")
  })
}

thread_local! {
  static POLLS: Cell<u64> = const { Cell::new(0) };
}

/// Number of times I/O tasks were polled on this thread (the mesh thread),
/// for measurements.
pub fn polls_on_this_thread() -> u64 {
  POLLS.with(Cell::get)
}

/// A task running on the shared runtime. Polled from the mesh thread; see
/// the module docs for cancellation.
pub struct IoTask {
  handle: JoinHandle<Result<Var, String>>,
  cancel: CancellationToken,
}

/// Spawns `work` on the shared runtime. `work` receives the task's
/// cancellation token and must race its awaits against it.
pub fn spawn<F, Fut>(work: F) -> IoTask
where
  F: FnOnce(CancellationToken) -> Fut,
  Fut: Future<Output = Result<Var, String>> + Send + 'static,
{
  let cancel = CancellationToken::new();
  // Lines the task logs go where the spawning thread's go (a capture).
  let sink = otter_core::log::current();
  let task = work(cancel.clone());
  let handle = runtime().spawn(WithSink {
    sink,
    task: Box::pin(task),
  });
  IoTask { handle, cancel }
}

/// Polls a task with a log sink installed, on whichever runtime thread
/// polls it. The task is boxed so polling it needs no pin projection.
struct WithSink<F> {
  sink: Option<otter_core::log::Sink>,
  task: Pin<Box<F>>,
}

impl<F: Future> Future for WithSink<F> {
  type Output = F::Output;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
    let sink = self.sink.clone();
    otter_core::log::with_sink(sink, || self.task.as_mut().poll(cx))
  }
}

/// Runs blocking work (a long scan, a call into a blocking library, paced
/// input) on the runtime's blocking thread pool, so the mesh never blocks
/// and only the waiting instance is suspended. Blocking code cannot be
/// interrupted from outside: `work` receives the task's cancellation token
/// and should check it at safe points (or wire it to the library's own
/// cancel hook), and must not leave external state half-changed when it
/// stops early. Dropping the [`IoTask`] (instance cancelled or cleaned up)
/// cancels the token; the thread finishes on its own.
pub fn spawn_blocking<F>(work: F) -> IoTask
where
  F: FnOnce(CancellationToken) -> Result<Var, String> + Send + 'static,
{
  let cancel = CancellationToken::new();
  let token = cancel.clone();
  // Lines the work logs go where the spawning thread's go (a capture).
  let sink = otter_core::log::current();
  let handle = runtime().spawn_blocking(move || otter_core::log::with_sink(sink, || work(token)));
  IoTask { handle, cancel }
}

impl Future for IoTask {
  type Output = Result<Var, Error>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    POLLS.with(|p| p.set(p.get() + 1));
    match Pin::new(&mut self.handle).poll(cx) {
      Poll::Pending => Poll::Pending,
      Poll::Ready(Ok(Ok(value))) => Poll::Ready(Ok(value)),
      Poll::Ready(Ok(Err(message))) => Poll::Ready(Err(Error::Activation(message))),
      Poll::Ready(Err(join_error)) => Poll::Ready(Err(Error::Activation(format!(
        "I/O task failed: {join_error}"
      )))),
    }
  }
}

impl Drop for IoTask {
  fn drop(&mut self) {
    // Stops the task's work (it races its awaits against this token). The
    // task then finishes on its own; dropping the handle detaches it.
    self.cancel.cancel();
  }
}
