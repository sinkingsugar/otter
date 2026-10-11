//! Types shared by the scheduler, the shard contract and the shard adapters.

use crate::compose::Binding;
use crate::error::Error;
use crate::var::Var;

pub type InstanceId = u64;

/// How an instance ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
  Completed(Var),
  Stopped,
  Failed(Error),
  Cancelled,
}

/// Context for `instantiate`.
pub struct InstanceCtx {
  pub instance: InstanceId,
}

impl InstanceCtx {
  pub(crate) fn cleanup_ctx(&self) -> CleanupCtx {
    CleanupCtx {
      instance: self.instance,
    }
  }
}

/// Context for `cleanup`.
pub struct CleanupCtx {
  pub instance: InstanceId,
}

/// Per-instance memory, for measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstanceMemory {
  /// Local frame plus shard states (inline sizes and boxes).
  pub state_bytes: usize,
}

/// Read and write access to the frames during activation.
pub trait Frames {
  fn get(&self, binding: Binding) -> Var;
  fn set(&mut self, binding: Binding, value: Var);
}

/// Operations available to a shard that cannot suspend ([`crate::shards::leaf`]).
pub trait LeafCtx: Frames {
  fn instance(&self) -> InstanceId;
  /// The instance's loop iteration: 0, then one more each time a looped
  /// wire starts again (or a Restart).
  fn iteration(&self) -> u64;
  /// The meter of a compose-time evaluation (docs/metaprogramming.md
  /// §2.1), `None` at run time. A shard on the compose-time list charges it
  /// for work that is not constant, and checks the value limit before it
  /// allocates.
  fn meter(&self) -> Option<&crate::compose_time::Meter> {
    None
  }
}

/// How a mesh treats instances waiting on an async operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WakeMode {
  /// Resume waiting instances every tick, so their futures are polled every
  /// tick whether or not anything changed.
  #[default]
  PollEveryTick,
  /// Resume a waiting instance only after its waker fired. Instances
  /// suspended for a tick (`Pause`) are still resumed every tick.
  OnNotify,
}

/// Per-instance wake flag behind the instance's [`std::task::Waker`]. A wake
/// after the instance has finished is harmless: the scheduler never resumes
/// a finished instance.
#[derive(Default)]
pub(crate) struct WakeFlag(std::sync::atomic::AtomicBool);

impl WakeFlag {
  /// Returns whether the waker fired since the last call, and clears it.
  pub(crate) fn take(&self) -> bool {
    self.0.swap(false, std::sync::atomic::Ordering::AcqRel)
  }
}

impl std::task::Wake for WakeFlag {
  fn wake(self: std::sync::Arc<Self>) {
    self.wake_by_ref();
  }

  fn wake_by_ref(self: &std::sync::Arc<Self>) {
    self.0.store(true, std::sync::atomic::Ordering::Release);
  }
}
