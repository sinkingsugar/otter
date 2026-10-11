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
//!
//! Events: `received <path>`, `partial-sent <path>`, `completed <path>`,
//! `client-closed <path>` (the client closed the connection while the server
//! was waiting to respond).

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

fn read_request_path(stream: &mut TcpStream) -> Option<String> {
  stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
  let mut data = Vec::new();
  let mut buf = [0u8; 1024];
  while !data.windows(4).any(|w| w == b"\r\n\r\n") {
    let n = stream.read(&mut buf).ok()?;
    if n == 0 {
      return None;
    }
    data.extend_from_slice(&buf[..n]);
  }
  let text = String::from_utf8_lossy(&data);
  text.split_whitespace().nth(1).map(str::to_string)
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
  write!(
    stream,
    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
    body.len()
  )?;
  stream.flush()
}

fn handle(mut stream: TcpStream, state: &State) {
  let Some(path) = read_request_path(&mut stream) else {
    return;
  };
  state.record(format!("received {path}"));
  let done = match path.as_str() {
    "/ok" => respond(&mut stream, "200 OK", "hello").is_ok(),
    "/missing" => respond(&mut stream, "404 Not Found", "nope").is_ok(),
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
