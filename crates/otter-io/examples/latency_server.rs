//! A fixed-latency HTTP server for the HTTP concurrency benchmark
//! (`bench/http-concurrency/`). Shared by the 1.x and 2.0 clients so both
//! talk to the same server.
//!
//! - `GET /work`: waits `--latency-ms` (default 50), then responds 200 with a
//!   64-byte body. Every connection is served concurrently.
//! - `GET /stats`: `peak=<max concurrent /work requests> completed=<n>`.
//! - `GET /reset`: zeroes the counters.
//!
//! Every response uses `Connection: close`, so connection reuse cannot
//! favor either client.
//!
//! Usage: cargo run --release -p otter-io --example latency_server -- [port] [latency-ms]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct Stats {
  in_flight: AtomicU64,
  peak: AtomicU64,
  completed: AtomicU64,
}

async fn read_path(stream: &mut TcpStream) -> Option<String> {
  let mut data = Vec::new();
  let mut buf = [0u8; 1024];
  while !data.windows(4).any(|w| w == b"\r\n\r\n") {
    let n = stream.read(&mut buf).await.ok()?;
    if n == 0 {
      return None;
    }
    data.extend_from_slice(&buf[..n]);
  }
  let text = String::from_utf8_lossy(&data);
  text.split_whitespace().nth(1).map(str::to_string)
}

async fn respond(stream: &mut TcpStream, body: &str) {
  let response = format!(
    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
    body.len()
  );
  let _ = stream.write_all(response.as_bytes()).await;
  let _ = stream.shutdown().await;
}

async fn handle(mut stream: TcpStream, stats: Arc<Stats>, latency: Duration) {
  let Some(path) = read_path(&mut stream).await else {
    return;
  };
  match path.as_str() {
    "/work" => {
      let now = stats.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
      stats.peak.fetch_max(now, Ordering::SeqCst);
      tokio::time::sleep(latency).await;
      respond(&mut stream, &"x".repeat(64)).await;
      stats.in_flight.fetch_sub(1, Ordering::SeqCst);
      stats.completed.fetch_add(1, Ordering::SeqCst);
    }
    "/stats" => {
      let body = format!(
        "peak={} completed={}",
        stats.peak.load(Ordering::SeqCst),
        stats.completed.load(Ordering::SeqCst)
      );
      respond(&mut stream, &body).await;
    }
    "/reset" => {
      stats.peak.store(0, Ordering::SeqCst);
      stats.completed.store(0, Ordering::SeqCst);
      respond(&mut stream, "ok").await;
    }
    _ => respond(&mut stream, "").await,
  }
}

#[tokio::main]
async fn main() {
  let mut args = std::env::args().skip(1);
  let port: u16 = args
    .next()
    .map(|a| a.parse().expect("port"))
    .unwrap_or(18090);
  let latency_ms: u64 = args
    .next()
    .map(|a| a.parse().expect("latency-ms"))
    .unwrap_or(50);
  let latency = Duration::from_millis(latency_ms);
  let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
  eprintln!("latency_server on 127.0.0.1:{port}, latency {latency_ms} ms");
  let stats = Arc::new(Stats::default());
  loop {
    let Ok((stream, _)) = listener.accept().await else {
      continue;
    };
    tokio::spawn(handle(stream, stats.clone(), latency));
  }
}
