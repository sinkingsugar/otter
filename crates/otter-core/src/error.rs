use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
  /// Compose failed: a type mismatch, an unknown variable or wire, bad parameters.
  Compose(String),
  /// Instantiation or activation failed.
  Activation(String),
  /// The instance was cancelled while suspended. Shards must propagate this
  /// (with `?`) so suspended execution unwinds before cleanup (contract §5).
  Cancelled,
  /// A structured construct- or compose-time diagnostic (argument decoding,
  /// and shards that report structured errors).
  Diagnostic(Box<crate::diagnostic::Diagnostic>),
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Error::Compose(msg) => write!(f, "compose error: {msg}"),
      Error::Activation(msg) => write!(f, "activation error: {msg}"),
      Error::Cancelled => write!(f, "cancelled"),
      Error::Diagnostic(d) => write!(f, "{} error: {d}", d.phase.name()),
    }
  }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
  /// Adds the parameter a structured diagnostic is about, if it does not
  /// name one yet. Other errors are returned unchanged.
  pub fn with_param(self, name: &str, index: usize) -> Error {
    match self {
      Error::Diagnostic(mut d) if d.param.is_none() => {
        d.param = Some(name.to_string());
        d.param_index = Some(index as i32);
        Error::Diagnostic(d)
      }
      other => other,
    }
  }

  /// Names the shard a structured diagnostic is about, if it does not name
  /// one yet. Other errors are returned unchanged.
  pub fn in_shard(self, shard: &str) -> Error {
    match self {
      Error::Diagnostic(mut d) if d.shard.is_none() => {
        d.shard = Some(shard.to_string());
        Error::Diagnostic(d)
      }
      other => other,
    }
  }

  /// Prepends a step to a structured diagnostic's occurrence path (compose
  /// adds steps as the error leaves each flow and wire). Other errors are
  /// returned unchanged.
  pub(crate) fn prefix_path(self, step: crate::diagnostic::PathStep) -> Error {
    match self {
      Error::Diagnostic(mut d) => {
        d.path.insert(0, step);
        Error::Diagnostic(d)
      }
      other => other,
    }
  }

  /// The structured diagnostic, if this error carries one.
  pub fn diagnostic(&self) -> Option<&crate::diagnostic::Diagnostic> {
    match self {
      Error::Diagnostic(d) => Some(d),
      _ => None,
    }
  }
}

/// The message of a caught panic, for error reports.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
  if let Some(s) = payload.downcast_ref::<&str>() {
    s.to_string()
  } else if let Some(s) = payload.downcast_ref::<String>() {
    s.clone()
  } else {
    "non-string panic payload".to_string()
  }
}
