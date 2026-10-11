//! A simulated async service and the `Request` shard that uses it: the
//! realistic async example for both schedulers.
//!
//! The service behaves like real async I/O: a request completes or fails
//! after a latency, its future registers the instance's waker, and the
//! service wakes it on completion. It runs on a simulated clock that the
//! caller advances (`advance`), so tests are deterministic.
//!
//! **Cancellation semantics** are explicit per request:
//! - abortable: dropping the pending future aborts the external request;
//! - detached: dropping the pending future leaves the request running; it
//!   completes later and fires the (now stale) waker, which must be
//!   harmless.
//!
//! The service is thread-local test infrastructure, like `Probe`'s events.

use crate::args::Args;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use super::async_shard::AsyncShard;
use super::*;
use crate::describe::{
  Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets, TypeName,
};
use crate::instance::LeafCtx;

/// Where a simulated request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestState {
  Pending,
  Completed,
  Failed,
  /// The future was dropped while pending, and the request was aborted.
  Aborted,
  /// The future was dropped while pending; the request keeps running.
  Detached,
  /// A detached request that completed later (its waker fired, stale).
  DetachedCompleted,
}

struct ServiceRequest {
  due: u64,
  fail: bool,
  abortable: bool,
  instance: InstanceId,
  state: RequestState,
  waker: Option<Waker>,
  polls: u64,
}

#[derive(Default)]
struct Service {
  now: u64,
  requests: Vec<ServiceRequest>,
}

thread_local! {
  static SERVICE: RefCell<Service> = RefCell::new(Service::default());
}

/// Resets the simulated service (clock and requests) on this thread.
pub fn reset() {
  SERVICE.with(|s| *s.borrow_mut() = Service::default());
}

/// Advances the simulated clock by one step, completing due requests and
/// waking their wakers (including stale ones of detached requests).
pub fn advance() {
  let wakers: Vec<Waker> = SERVICE.with(|s| {
    let mut s = s.borrow_mut();
    s.now += 1;
    let now = s.now;
    let mut wakers = Vec::new();
    for r in &mut s.requests {
      if r.due > now {
        continue;
      }
      match r.state {
        RequestState::Pending => {
          r.state = if r.fail {
            RequestState::Failed
          } else {
            RequestState::Completed
          }
        }
        RequestState::Detached => r.state = RequestState::DetachedCompleted,
        _ => continue,
      }
      wakers.extend(r.waker.take());
    }
    wakers
  });
  // Wake outside the borrow, as a real reactor would.
  for waker in wakers {
    waker.wake();
  }
}

/// The state of every request, in submission order.
pub fn request_states() -> Vec<RequestState> {
  SERVICE.with(|s| s.borrow().requests.iter().map(|r| r.state).collect())
}

/// Total number of times request futures were polled.
pub fn total_polls() -> u64 {
  SERVICE.with(|s| s.borrow().requests.iter().map(|r| r.polls).sum())
}

/// The future of one simulated request. Owns only its request id.
pub struct RequestOp {
  id: usize,
}

impl Future for RequestOp {
  type Output = Result<Var>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<Var>> {
    SERVICE.with(|s| {
      let mut s = s.borrow_mut();
      let r = &mut s.requests[self.id];
      r.polls += 1;
      match r.state {
        RequestState::Completed => Poll::Ready(Ok(Var::Int(self.id as i64))),
        RequestState::Failed => Poll::Ready(Err(Error::Activation(format!(
          "request {} failed",
          self.id
        )))),
        RequestState::Pending => {
          if !r.waker.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
            r.waker = Some(cx.waker().clone());
          }
          Poll::Pending
        }
        other => Poll::Ready(Err(Error::Activation(format!(
          "request {} polled in state {other:?}",
          self.id
        )))),
      }
    })
  }
}

impl Drop for RequestOp {
  fn drop(&mut self) {
    let aborted = SERVICE.with(|s| {
      let mut s = s.borrow_mut();
      let r = &mut s.requests[self.id];
      if r.state != RequestState::Pending {
        return None;
      }
      if r.abortable {
        r.state = RequestState::Aborted;
        r.waker = None;
        Some(r.instance)
      } else {
        // Keeps running; its waker stays registered and will fire late.
        r.state = RequestState::Detached;
        None
      }
    });
    if let Some(instance) = aborted {
      record("request", instance, ProbeEventKind::Aborted);
    }
  }
}

pub struct RequestCompiled {
  delay: u64,
  fail: bool,
  abortable: bool,
}

/// `Request(delay fail abortable)`: a simulated request that completes after
/// `delay` clock steps (failing if `fail`), and outputs its request id.
/// Dropping it while pending aborts it if `abortable`, else detaches it.
pub struct Request;

pub static REQUEST_PARAMS: &[ParamDecl] = &[
  ParamDecl {
    name: "delay",
    help: crate::shard_doc!(
      "Simulated clock steps until the request completes; must not be negative."
    ),
    forms: Forms::LITERAL,
    types: &[TypeName::Int],
    requirement: Requirement::Required,
    ty: None,
  },
  ParamDecl {
    name: "fail",
    help: crate::shard_doc!("Whether the request fails instead of completing."),
    forms: Forms::LITERAL,
    types: &[TypeName::Bool],
    requirement: Requirement::Required,
    ty: None,
  },
  ParamDecl {
    name: "abortable",
    help: crate::shard_doc!(
      "Whether dropping the pending future aborts the request (true) or detaches it (false)."
    ),
    forms: Forms::LITERAL,
    types: &[TypeName::Bool],
    requirement: Requirement::Required,
    ty: None,
  },
];

pub const REQUEST_DESC: ShardDesc = ShardDesc {
  name: "Request",
  version: 1,
  summary: crate::shard_doc!("Test instrumentation: a request to a simulated async service."),
  help: crate::shard_doc!(
    "Completes after `delay` steps of the simulated clock and outputs the request id, or fails if `fail`. Ignores its input."
  ),
  params: Params::Declared(REQUEST_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Fixed(TypeName::Int),
  targets: Targets::All,
  aliases: &[],
  effects: crate::signature::Effects::IO_WAIT,
  lifetime: crate::signature::Lifetime::Stateless,
};

impl AsyncShard for Request {
  type Compiled = RequestCompiled;
  type Op = RequestOp;
  const DESC: ShardDesc = REQUEST_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<RequestCompiled>> {
    let delay = args.int("delay").expect("decoded Delay");
    if delay < 0 {
      return Err(Error::Diagnostic(Box::new(
        crate::diagnostic::Diagnostic::new(
          crate::diagnostic::Phase::Compose,
          "compose-error",
          "invalid-argument-value",
          format!("Delay must not be negative, got {delay}"),
        )
        .shard("Request")
        .param("delay", Some(args.param_index("delay"))),
      )));
    }
    let _ = ctx.input();
    Ok(Composed {
      compiled: RequestCompiled {
        delay: delay as u64,
        fail: args.bool("fail").expect("decoded Fail"),
        abortable: args.bool("abortable").expect("decoded Abortable"),
      },
      output: Type::int(),
    })
  }

  fn start(c: &RequestCompiled, ctx: &mut impl LeafCtx, _: &Var) -> Result<RequestOp> {
    let instance = ctx.instance();
    let id = SERVICE.with(|s| {
      let mut s = s.borrow_mut();
      let due = s.now + c.delay;
      s.requests.push(ServiceRequest {
        due,
        fail: c.fail,
        abortable: c.abortable,
        instance,
        state: RequestState::Pending,
        waker: None,
        polls: 0,
      });
      s.requests.len() - 1
    });
    Ok(RequestOp { id })
  }
}
