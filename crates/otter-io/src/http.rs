//! `Http.Get`, following 1.x (`shards/modules/http/src/lib.rs`): requests
//! run on the shared runtime ([`crate::runtime`]); the client is cached by
//! configuration and keeps pooled idle connections for at most 50 s; a
//! non-success status fails with the response body (truncated); the request
//! races a cancellation token.
//!
//! Differences from 1.x:
//! - the client cache is process-wide, keyed by configuration (1.x caches it
//!   per mesh; the prototype has no mesh services yet);
//! - reading the body also races the cancellation token. In 1.x only
//!   `send()` does, so cancelling midway through a body leaves the task
//!   reading until the body completes or the request times out.
//! - a subset of 1.x's parameters: `url`, `timeout`, `headers` (a table of
//!   String values), `bytes`, `full-response`, `retry` and `backoff`; no
//!   query input, proxy, invalid certificates or streaming yet. Constant
//!   headers are checked at compose: an invalid name or value is a compose
//!   error, not a failed request.
//! - `retry` retries only what can be transient: a transport error (no
//!   connection, a timeout, a body cut short) and, without `full-response`,
//!   the statuses 408, 429, 500, 502, 503 and 504. 1.x retried every
//!   failure, a 404 included, and ended with a bare "Request failed"; here
//!   the last attempt's error is reported with the attempt count. 1.x also
//!   said `full-response` disables retrying; here it still retries
//!   transport errors, and returns every status as a value.
//! - `backoff` may be a Float (1.x: whole seconds), so short waits are
//!   possible. The wait doubles after each attempt (`backoff`, 2x, 4x, ...)
//!   and races the cancellation token, so cancelling during it is prompt
//!   (1.x suspended the wire for the wait).
//! - with `full-response`, a header the response repeats is one entry, its
//!   values joined with `, ` (1.x kept the last); values that are not
//!   UTF-8 are converted lossily (1.x failed the request).
//!
//! **What cancellation does:** it stops local polling, and the request
//! future is dropped on the runtime, which closes the connection (verified by
//! the tests against a local server). Whatever the server already received
//! stays received.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use otter_core::compose::{Binding, ComposeCtx};
use otter_core::describe::{
  DefaultValue, Forms, InputDesc, OutputDesc, ParamDecl, Params, Requirement, ShardDesc, Targets,
  TypeName,
};
use otter_core::diagnostic::{Diagnostic, Phase, TypeRef};
use otter_core::instance::LeafCtx;
use otter_core::shards::async_shard::{AsyncShard, async_type};
use otter_core::{
  Arg, Args, Composed, Error, ParamValue, Result, ShardDef, ShardType, Type, Var, shard_doc,
};
use otter_core::{Shape, Table};
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::runtime::{CancellationToken, IoTask, spawn};

/// Client configuration, the cache key (as in 1.x: proxy and invalid-certs
/// settings).
#[derive(Clone, Default, PartialEq, Eq, Hash)]
struct ClientConfig {
  proxy_url: Option<String>,
  invalid_certs: bool,
}

fn client(config: &ClientConfig) -> std::result::Result<reqwest::Client, String> {
  static CLIENTS: OnceLock<Mutex<HashMap<ClientConfig, reqwest::Client>>> = OnceLock::new();
  let mut clients = CLIENTS
    .get_or_init(Default::default)
    .lock()
    .expect("client cache poisoned");
  if let Some(client) = clients.get(config) {
    return Ok(client.clone());
  }
  crate::runtime::install_tls_provider();
  // As in 1.x: keep pooled idle connections below common server/LB keep-alive
  // timeouts (nginx 75s, ALB 60s), so a connection the peer already dropped
  // is never reused.
  let mut builder = reqwest::Client::builder().pool_idle_timeout(Duration::from_secs(50));
  // Certificate options exist only with a TLS backend (see the features).
  #[cfg(any(feature = "rustls-ring", feature = "native-tls"))]
  {
    builder = builder.danger_accept_invalid_certs(config.invalid_certs);
  }
  if let Some(proxy_url) = &config.proxy_url {
    let proxy = reqwest::Proxy::all(proxy_url).map_err(|e| format!("Invalid proxy url: {e}"))?;
    builder = builder.proxy(proxy);
  }
  let client = builder
    .build()
    .map_err(|e| format!("Failed to create HTTP client: {e}"))?;
  clients.insert(config.clone(), client.clone());
  Ok(client)
}

/// The url: a constant, or a String variable read at activation.
pub enum Url {
  Const(String),
  Bound(Binding),
}

/// Request headers: constant (checked at compose), or a table variable
/// read and checked at activation.
pub enum Headers {
  Const(HeaderMap),
  Bound(Binding),
}

pub struct GetCompiled {
  url: Url,
  timeout: Duration,
  headers: Headers,
  reply: Reply,
  retry: u32,
  backoff: Duration,
}

/// What a response becomes: the body as a String or as Bytes, alone or in
/// the full-response table (whose shape is kept to build it by slot).
#[derive(Clone, Copy)]
struct Reply {
  bytes: bool,
  full: Option<Shape>,
}

/// A headers table as a header map: every value must be a String, and
/// names and values valid in HTTP.
fn header_map(table: &Var) -> std::result::Result<HeaderMap, String> {
  let Some(table) = table.as_table() else {
    return Err(format!("headers must be a table, got {}", table.type_of()));
  };
  let mut map = HeaderMap::with_capacity(table.len());
  for (name, value) in table.iter() {
    let Some(text) = value.as_str() else {
      return Err(format!(
        "header {name} must be a String, got {}",
        value.type_of()
      ));
    };
    let header = HeaderName::from_bytes(name.as_bytes())
      .map_err(|_| format!("invalid header name {name:?}"))?;
    let value =
      HeaderValue::from_str(text).map_err(|_| format!("invalid value for header {name}"))?;
    map.append(header, value);
  }
  Ok(map)
}

/// The response headers as a table of Strings, a repeated header's values
/// joined with `, `.
fn header_table(headers: &HeaderMap) -> Var {
  Var::table(headers.keys().map(|name| {
    let mut value = String::new();
    for (i, part) in headers.get_all(name).iter().enumerate() {
      if i > 0 {
        value.push_str(", ");
      }
      value.push_str(&String::from_utf8_lossy(part.as_bytes()));
    }
    (name.as_str(), Var::string(&value))
  }))
}

/// Statuses worth another attempt: the server may answer differently soon.
fn transient(status: StatusCode) -> bool {
  matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

/// Why an attempt failed.
enum Failure {
  /// Worth another attempt, if any are left.
  Transient(String),
  Final(String),
  Cancelled,
}

/// One request, raced against the cancellation token.
async fn attempt(
  client: &reqwest::Client,
  request: reqwest::Request,
  reply: Reply,
  cancel: &CancellationToken,
) -> std::result::Result<Var, Failure> {
  let response = tokio::select! {
    response = client.execute(request) => {
      response.map_err(|e| Failure::Transient(format!("Request failed {e:?}")))?
    }
    _ = cancel.cancelled() => return Err(Failure::Cancelled),
  };
  let status = response.status();
  let headers = reply.full.map(|_| header_table(response.headers()));
  let body = async {
    if reply.bytes {
      response.bytes().await.map(|b| Var::bytes(&b))
    } else {
      response.text().await.map(|t| Var::string(&t))
    }
  };
  let body = tokio::select! {
    body = body => {
      body.map_err(|e| Failure::Transient(format!("Failed to decode the response: {e}")))?
    }
    _ = cancel.cancelled() => return Err(Failure::Cancelled),
  };
  if let (Some(shape), Some(headers)) = (reply.full, headers) {
    // In the shape's (sorted) key order: body, headers, status.
    let status = Var::Int(i64::from(status.as_u16()));
    return Ok(Var::from_table(Table::with_shape(
      shape,
      [body, headers, status],
    )));
  }
  if status.is_success() {
    return Ok(body);
  }
  let text = match &body {
    Var::String(text) => text.to_string(),
    other => String::from_utf8_lossy(other.as_bytes().unwrap_or_default()).into_owned(),
  };
  let text = if text.len() > 1024 {
    format!("{}...", text.chars().take(1024).collect::<String>())
  } else {
    text
  };
  let message = format!("Request failed with status {status}, error: {text}");
  Err(if transient(status) {
    Failure::Transient(message)
  } else {
    Failure::Final(message)
  })
}

/// `Http.Get(url [timeout] [headers] [bytes] [full-response] [retry]
/// [backoff])`: GETs `url` and outputs the body as a String (or Bytes), or
/// the whole response as a table. Fails on a connection error, a timeout
/// (default 10 s), or, without `full-response`, a non-success status (with
/// the body in the error); `retry` repeats transient failures.
pub struct Get;

/// `Http.Get`'s parameters. The decoder enforces these declarations and the
/// catalog documents them.
pub static GET_PARAMS: &[ParamDecl] = &[
  ParamDecl {
    name: "url",
    help: shard_doc!(
      "The URL to request: a String literal, or a String variable read at activation."
    ),
    forms: Forms::LITERAL.or(Forms::VARIABLE),
    types: &[TypeName::String],
    requirement: Requirement::Required,
    ty: None,
  },
  ParamDecl {
    name: "timeout",
    help: shard_doc!(
      "Request timeout in seconds; must be positive. Timeouts from variables are not supported yet."
    ),
    forms: Forms::LITERAL,
    types: &[TypeName::Int],
    requirement: Requirement::Default(DefaultValue::Int(10)),
    ty: None,
  },
  ParamDecl {
    name: "headers",
    help: shard_doc!(
      "Request headers, a table of String values by header name (`{user-agent: \"...\"}`): a literal, or a table variable read at activation."
    ),
    forms: Forms::LITERAL.or(Forms::VARIABLE),
    types: &[TypeName::Table],
    requirement: Requirement::Optional,
    ty: None,
  },
  ParamDecl {
    name: "bytes",
    help: shard_doc!("Output the body as Bytes instead of a String."),
    forms: Forms::LITERAL,
    types: &[TypeName::Bool],
    requirement: Requirement::Default(DefaultValue::Bool(false)),
    ty: None,
  },
  ParamDecl {
    name: "full-response",
    help: shard_doc!(
      "Output `{status: Int headers: {name: String} body: String}` (the body Bytes with `bytes`), and treat every status as a response, not an error."
    ),
    forms: Forms::LITERAL,
    types: &[TypeName::Bool],
    requirement: Requirement::Default(DefaultValue::Bool(false)),
    ty: None,
  },
  ParamDecl {
    name: "retry",
    help: shard_doc!(
      "How many times to repeat a request that failed transiently: no connection, a timeout, a body cut short, or (without `full-response`) the status 408, 429, 500, 502, 503 or 504."
    ),
    forms: Forms::LITERAL,
    types: &[TypeName::Int],
    requirement: Requirement::Default(DefaultValue::Int(0)),
    ty: None,
  },
  ParamDecl {
    name: "backoff",
    help: shard_doc!(
      "Seconds to wait before the first retry; each later wait doubles. Must not be negative."
    ),
    forms: Forms::LITERAL,
    types: &[TypeName::Int, TypeName::Float],
    requirement: Requirement::Default(DefaultValue::Int(1)),
    ty: None,
  },
];

pub const GET_DESC: ShardDesc = ShardDesc {
  name: "Http.Get",
  version: 1,
  summary: shard_doc!(
    "GETs a URL and outputs the response body as a String, as Bytes, or the whole response as a table."
  ),
  help: shard_doc!(
    "Fails on a connection error, when the timeout expires (per attempt), or, without `full-response`, on a non-success status (with the response body in the error, truncated to 1024 characters). With `retry`, transient failures are repeated after a doubling wait, and the last one is reported. Cancelling the wire stops local polling, a wait between attempts included, and releases the request; it cannot undo work the server already received."
  ),
  params: Params::Declared(GET_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Dynamic(shard_doc!(
    "String; Bytes with `bytes: true`; with `full-response: true`, `{status: Int headers: {name: String} body: String}` (the body Bytes with `bytes: true`)"
  )),
  targets: Targets::NativeOnly,
  aliases: &[],
  effects: otter_core::signature::Effects::IO_WAIT,
  lifetime: otter_core::signature::Lifetime::Stateless,
};

fn compose_error(code: &'static str, message: String, param: &str, index: usize) -> Error {
  Error::Diagnostic(Box::new(
    Diagnostic::new(Phase::Compose, "compose-error", code, message)
      .shard(GET_DESC.name)
      .param(param, Some(index)),
  ))
}

impl AsyncShard for Get {
  type Compiled = GetCompiled;
  type Op = IoTask;
  const DESC: ShardDesc = GET_DESC;

  fn compose(args: &Args, ctx: &mut ComposeCtx<'_>) -> Result<Composed<GetCompiled>> {
    // Names, positions, the default, forms and literal types were checked
    // by the shared decoder; compose resolves the binding and checks values.
    let url = match args.get("url").expect("decoded required URL") {
      ParamValue::Value(Var::String(url)) => Url::Const(url.to_string()),
      ParamValue::Var(name) => {
        let info = ctx
          .read_var(name, GET_DESC.name)
          .map_err(|e| e.with_param("url", args.param_index("url")))?;
        if info.ty != Type::string() {
          return Err(Error::Diagnostic(Box::new(
            Diagnostic::new(
              Phase::Compose,
              "compose-error",
              "wrong-variable-type",
              format!("URL variable {name} must be a String, got {}", info.ty),
            )
            .shard(GET_DESC.name)
            .param("url", Some(args.param_index("url")))
            .types(
              Some(TypeRef::of(info.ty)),
              vec![TypeRef::named(TypeName::String)],
            ),
          )));
        }
        Url::Bound(info.binding)
      }
      other => unreachable!("decoder accepted {other:?} for Http.Get URL"),
    };
    let secs = args.int("timeout").expect("decoded Timeout");
    if secs <= 0 {
      return Err(compose_error(
        "invalid-argument-value",
        format!("Timeout must be positive, got {secs}"),
        "timeout",
        args.param_index("timeout"),
      ));
    }
    let headers = match args.get("headers") {
      None => Headers::Const(HeaderMap::new()),
      Some(ParamValue::Value(table)) => Headers::Const(header_map(table).map_err(|message| {
        compose_error(
          "invalid-argument-value",
          message,
          "headers",
          args.param_index("headers"),
        )
      })?),
      Some(ParamValue::Var(name)) => {
        let info = ctx
          .read_var(name, GET_DESC.name)
          .map_err(|e| e.with_param("headers", args.param_index("headers")))?;
        // Every value must be able to hold a String; the values themselves
        // are checked at activation.
        let strings = info.ty.as_table().is_some_and(|t| {
          t.keys
            .iter()
            .map(|(_, ty)| *ty)
            .chain(t.rest)
            .all(|ty| ty.accepts(Type::string()))
        });
        if !strings {
          return Err(compose_error(
            "wrong-variable-type",
            format!(
              "headers variable {name} must be a table of Strings, got {}",
              info.ty
            ),
            "headers",
            args.param_index("headers"),
          ));
        }
        Headers::Bound(info.binding)
      }
      Some(other) => unreachable!("decoder accepted {other:?} for Http.Get headers"),
    };
    let bytes = args.bool("bytes").expect("decoded bytes");
    let body = if bytes { Type::bytes() } else { Type::string() };
    let (output, full) = if args.bool("full-response").expect("decoded full-response") {
      let headers = Type::table(Vec::<(&str, Type)>::new(), Some(Type::string()));
      let output = Type::fixed_table([
        ("status", Type::int()),
        ("headers", headers),
        ("body", body),
      ]);
      let shape = output
        .as_table()
        .and_then(|t| t.shape)
        .expect("a fixed table has a shape");
      debug_assert_eq!(
        shape.keys().iter().map(|k| &**k).collect::<Vec<_>>(),
        ["body", "headers", "status"]
      );
      (output, Some(shape))
    } else {
      (body, None)
    };
    let retry = args.int("retry").expect("decoded retry");
    let retry = u32::try_from(retry).map_err(|_| {
      compose_error(
        "invalid-argument-value",
        format!("retry must be between 0 and {}, got {retry}", u32::MAX),
        "retry",
        args.param_index("retry"),
      )
    })?;
    let backoff = match args.literal("backoff").expect("decoded backoff") {
      Var::Int(secs) => *secs as f64,
      Var::Float(secs) => *secs,
      other => unreachable!("decoder accepted {other:?} for Http.Get backoff"),
    };
    let backoff = Duration::try_from_secs_f64(backoff).map_err(|_| {
      compose_error(
        "invalid-argument-value",
        format!("backoff must be a non-negative number of seconds, got {backoff}"),
        "backoff",
        args.param_index("backoff"),
      )
    })?;
    Ok(Composed {
      compiled: GetCompiled {
        url,
        timeout: Duration::from_secs(secs as u64),
        headers,
        reply: Reply { bytes, full },
        retry,
        backoff,
      },
      output,
    })
  }

  fn start(c: &GetCompiled, ctx: &mut impl LeafCtx, _input: &Var) -> Result<IoTask> {
    // Owned inputs only: nothing borrowed from the frames goes into the task.
    let url = match &c.url {
      Url::Const(url) => url.clone(),
      Url::Bound(binding) => match &ctx.get(*binding) {
        Var::String(url) => url.to_string(),
        other => {
          return Err(Error::Activation(format!(
            "Http.Get: URL is not a String: {other:?}"
          )));
        }
      },
    };
    let client = client(&ClientConfig::default()).map_err(Error::Activation)?;
    let headers = match &c.headers {
      Headers::Const(map) => map.clone(),
      Headers::Bound(binding) => {
        header_map(&ctx.get(*binding)).map_err(|e| Error::Activation(format!("Http.Get: {e}")))?
      }
    };
    let request = client
      .get(url)
      .timeout(c.timeout)
      .headers(headers)
      .build()
      .map_err(|e| Error::Activation(format!("Http.Get: invalid request: {e}")))?;
    let (reply, retry, backoff) = (c.reply, c.retry, c.backoff);

    Ok(spawn(move |cancel| async move {
      let mut request = Some(request);
      let mut attempts = 0u32;
      loop {
        // A GET has no body, so it can always be repeated.
        let this = if attempts < retry {
          request.as_ref().and_then(reqwest::Request::try_clone)
        } else {
          request.take()
        }
        .expect("a GET request can be repeated");
        let message = match attempt(&client, this, reply, &cancel).await {
          Ok(value) => return Ok(value),
          Err(Failure::Cancelled) => return Err("Request cancelled".to_string()),
          Err(Failure::Final(message)) => return Err(message),
          Err(Failure::Transient(message)) => message,
        };
        if attempts == retry {
          return Err(if retry == 0 {
            message
          } else {
            format!("{message} (after {} attempts)", retry + 1)
          });
        }
        let wait = backoff.saturating_mul(1 << attempts.min(31));
        tokio::select! {
          _ = tokio::time::sleep(wait) => {}
          _ = cancel.cancelled() => return Err("Request cancelled".to_string()),
        }
        attempts += 1;
      }
    }))
  }
}

pub static GET: ShardType = async_type::<Get>();

/// `Http.Get` with a constant URL.
pub fn get(url: &str) -> ShardDef {
  ShardDef::new(&GET, vec![ParamValue::Value(Var::string(url))])
}

/// `Http.Get` with any arguments (positional and/or named).
pub fn get_args(args: Vec<Arg>) -> ShardDef {
  ShardDef::with_args(&GET, args)
}

/// `Http.Get` with a constant URL and a timeout in seconds.
pub fn get_with_timeout(url: &str, secs: i64) -> ShardDef {
  ShardDef::new(
    &GET,
    vec![
      ParamValue::Value(Var::string(url)),
      ParamValue::Value(Var::Int(secs)),
    ],
  )
}
