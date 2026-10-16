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
//! - only a subset of 1.x's parameters: `url`, `timeout` and `headers`
//!   (a table of String values). Constant headers are checked at compose:
//!   an invalid name or value is a compose error, not a failed request.
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
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::runtime::{IoTask, spawn};

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

/// `Http.Get(url [timeout] [headers])`: GETs `url` and outputs the body as a String.
/// Fails on a connection error, a timeout (default 10 s), or a non-success
/// status (with the body in the error).
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
];

pub const GET_DESC: ShardDesc = ShardDesc {
  name: "Http.Get",
  version: 1,
  summary: shard_doc!("GETs a URL and outputs the response body as a String."),
  help: shard_doc!(
    "Fails on a connection error, when the timeout expires, or on a non-success status (with the response body in the error, truncated to 1024 characters). Cancelling the wire stops local polling and releases the request; it cannot undo work the server already received."
  ),
  params: Params::Declared(GET_PARAMS),
  input: InputDesc::Ignored,
  output: OutputDesc::Fixed(TypeName::String),
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
    Ok(Composed {
      compiled: GetCompiled {
        url,
        timeout: Duration::from_secs(secs as u64),
        headers,
      },
      output: Type::string(),
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
    let request = client.get(url).timeout(c.timeout).headers(headers);

    Ok(spawn(move |cancel| async move {
      let response = tokio::select! {
        response = request.send() => response.map_err(|e| format!("Request failed {e:?}"))?,
        _ = cancel.cancelled() => return Err("Request cancelled".to_string()),
      };
      let status = response.status();
      let body = tokio::select! {
        body = response.text() => body.map_err(|e| format!("Failed to decode the response: {e}"))?,
        _ = cancel.cancelled() => return Err("Request cancelled".to_string()),
      };
      if !status.is_success() {
        let body = if body.len() > 1024 {
          format!("{}...", body.chars().take(1024).collect::<String>())
        } else {
          body
        };
        return Err(format!(
          "Request failed with status {status}, error: {body}"
        ));
      }
      Ok(Var::string(&body))
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
