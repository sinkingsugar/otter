//! A local HTTP/1.1 server with controlled responses, for the I/O tests.
//! Plain std threads, independent of the runtime under test. It records
//! what it received and how each connection ended, so tests can verify what
//! cancellation actually does on the wire.
//!
//! Routes:
//! - `/ok`: 200 with body `hello`;
//! - `/missing`: 404 with body `nope`;
//! - `/slow-headers`: waits until [`TestServer::release`] before sending
//!   anything, then 200 `late`;
//! - `/slow-body`: sends the headers and half of a 10-byte body (`hello`),
//!   then waits until released before sending the rest (`world`).
//! - `/echo-headers`: 200 with the request's `x-` and `user-agent` header
//!   lines as the body, names lowercased, sorted, one `name: value` per line.
//! - `/binary`: 200 with the 3-byte body `ff 00 01` (not UTF-8);
//! - `/twice`: 200 `hello` with the header `x-twice` sent twice (`a`, `b`);
//! - `/drop`: closes the connection without responding (a transport error);
//! - `/flaky`: like `/drop` for the first two requests, then 200 `recovered`;
//! - `/busy`: 503 `busy` for the first request, then 200 `recovered`;
//! - `/echo-body`: 200 with `<method> <content-type or -> <body>` (the body
//!   read by its `Content-Length`, as sent).
//!
//! [`TestServer::received`] counts the requests a path received.
//!
//! Events: `received <path>`, `partial-sent <path>`, `completed <path>`,
//! `client-closed <path>` (the client closed the connection while the server
//! was waiting to respond).

// Each test binary uses a part of it.
#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
  events: Mutex<Vec<String>>,
  released: AtomicBool,
}

impl State {
  fn record(&self, event: String) {
    self.events.lock().unwrap().push(event);
  }

  fn received(&self, path: &str) -> usize {
    let event = format!("received {path}");
    self
      .events
      .lock()
      .unwrap()
      .iter()
      .filter(|e| **e == event)
      .count()
  }
}

pub struct TestServer {
  addr: SocketAddr,
  state: Arc<State>,
}

impl TestServer {
  pub fn start() -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(State::default());
    let accept_state = state.clone();
    thread::spawn(move || {
      for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let state = accept_state.clone();
        thread::spawn(move || handle(stream, &state));
      }
    });
    TestServer { addr, state }
  }

  pub fn url(&self, path: &str) -> String {
    format!("http://{}{path}", self.addr)
  }

  /// Lets every waiting route finish its response.
  pub fn release(&self) {
    self.state.released.store(true, Ordering::SeqCst);
  }

  pub fn events(&self) -> Vec<String> {
    self.state.events.lock().unwrap().clone()
  }

  /// How many requests `path` received so far.
  pub fn received(&self, path: &str) -> usize {
    self.state.received(path)
  }

  /// Waits until `event` has been recorded, up to `timeout`.
  pub fn wait_for(&self, event: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
      if self.events().iter().any(|e| e == event) {
        return true;
      }
      thread::sleep(Duration::from_millis(2));
    }
    false
  }
}

/// A URL on a port with nothing listening (connection refused).
pub fn closed_port_url() -> String {
  let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
  let addr = listener.local_addr().unwrap();
  drop(listener);
  format!("http://{addr}/ok")
}

/// A request as the server read it.
struct Request {
  method: String,
  path: String,
  /// Header lines, as sent.
  headers: Vec<String>,
  /// The body, `Content-Length` bytes (empty without one).
  body: Vec<u8>,
}

impl Request {
  /// The value of the header `name` (case-insensitive), if sent.
  fn header(&self, name: &str) -> Option<&str> {
    self.headers.iter().find_map(|line| {
      let (n, value) = line.split_once(':')?;
      n.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
  }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
  stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
  let mut data = Vec::new();
  let mut buf = [0u8; 1024];
  let end = loop {
    if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
      break at + 4;
    }
    let n = stream.read(&mut buf).ok()?;
    if n == 0 {
      return None;
    }
    data.extend_from_slice(&buf[..n]);
  };
  let text = String::from_utf8_lossy(&data[..end]).into_owned();
  let mut lines = text.split("\r\n");
  let mut request_line = lines.next()?.split_whitespace();
  let method = request_line.next()?.to_string();
  let path = request_line.next()?.to_string();
  let headers = lines
    .take_while(|l| !l.is_empty())
    .map(str::to_string)
    .collect();
  let mut request = Request {
    method,
    path,
    headers,
    body: data[end..].to_vec(),
  };
  let length: usize = request
    .header("content-length")
    .map_or(Some(0), |v| v.parse().ok())?;
  while request.body.len() < length {
    let n = stream.read(&mut buf).ok()?;
    if n == 0 {
      return None;
    }
    request.body.extend_from_slice(&buf[..n]);
  }
  request.body.truncate(length);
  Some(request)
}

/// Waits until released (true) or until the client closes the connection
/// (false). Gives up after 10 s (true), so a broken test cannot hang.
fn wait_release_or_close(stream: &mut TcpStream, state: &State) -> bool {
  stream
    .set_read_timeout(Some(Duration::from_millis(5)))
    .unwrap();
  let start = Instant::now();
  let mut buf = [0u8; 64];
  loop {
    if state.released.load(Ordering::SeqCst) || start.elapsed() > Duration::from_secs(10) {
      return true;
    }
    match stream.read(&mut buf) {
      Ok(0) => return false,
      Ok(_) => {}
      Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
      Err(_) => return false,
    }
  }
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
  respond_with(stream, status, "", body.as_bytes())
}

/// A response with extra header lines (each ending in `\r\n`).
fn respond_with(
  stream: &mut TcpStream,
  status: &str,
  headers: &str,
  body: &[u8],
) -> std::io::Result<()> {
  write!(
    stream,
    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
    body.len()
  )?;
  stream.write_all(body)?;
  stream.flush()
}

fn handle(mut stream: TcpStream, state: &State) {
  let Some(request) = read_request(&mut stream) else {
    return;
  };
  let path = request.path.clone();
  let headers = &request.headers;
  state.record(format!("received {path}"));
  let done = match path.as_str() {
    "/ok" => respond(&mut stream, "200 OK", "hello").is_ok(),
    "/missing" => respond(&mut stream, "404 Not Found", "nope").is_ok(),
    "/binary" => respond_with(&mut stream, "200 OK", "", &[0xff, 0x00, 0x01]).is_ok(),
    "/twice" => respond_with(
      &mut stream,
      "200 OK",
      "X-Twice: a\r\nX-Twice: b\r\n",
      b"hello",
    )
    .is_ok(),
    "/drop" => return,
    "/flaky" => {
      if state.received(&path) <= 2 {
        return;
      }
      respond(&mut stream, "200 OK", "recovered").is_ok()
    }
    "/busy" => {
      if state.received(&path) <= 1 {
        respond(&mut stream, "503 Service Unavailable", "busy").is_ok()
      } else {
        respond(&mut stream, "200 OK", "recovered").is_ok()
      }
    }
    "/echo-headers" => {
      let mut echoed: Vec<String> = headers
        .iter()
        .filter_map(|line| {
          let (name, value) = line.split_once(':')?;
          let name = name.trim().to_ascii_lowercase();
          (name.starts_with("x-") || name == "user-agent")
            .then(|| format!("{name}: {}", value.trim()))
        })
        .collect();
      echoed.sort();
      respond(&mut stream, "200 OK", &echoed.join("\n")).is_ok()
    }
    "/echo-body" => {
      let mut echoed = format!(
        "{} {} ",
        request.method,
        request.header("content-type").unwrap_or("-")
      )
      .into_bytes();
      echoed.extend_from_slice(&request.body);
      respond_with(&mut stream, "200 OK", "", &echoed).is_ok()
    }
    "/slow-headers" => {
      if !wait_release_or_close(&mut stream, state) {
        state.record(format!("client-closed {path}"));
        return;
      }
      respond(&mut stream, "200 OK", "late").is_ok()
    }
    "/slow-body" => {
      let head = "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nhello";
      if stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.flush())
        .is_err()
      {
        return;
      }
      state.record(format!("partial-sent {path}"));
      if !wait_release_or_close(&mut stream, state) {
        state.record(format!("client-closed {path}"));
        return;
      }
      stream
        .write_all(b"world")
        .and_then(|_| stream.flush())
        .is_ok()
    }
    _ => respond(&mut stream, "404 Not Found", "").is_ok(),
  };
  if done {
    state.record(format!("completed {path}"));
  }
}
