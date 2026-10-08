// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! Dependency-free HTTP/1.1 load driver.
//!
//! The HTTP workloads must not depend on an external load generator (`wrk`,
//! `h2load`, `bombardier`) — none of them is assumed to be installed — so `bench`
//! drives its targets with this small, pure-`std` in-process client instead. It opens
//! a fixed number of TCP connections and, on each, issues keep-alive `GET` requests
//! for a bounded wall-clock duration, recording per-request latency and the aggregate
//! throughput.
//!
//! Design notes:
//!
//! * **No new dependencies / no `unsafe`.** Only [`std::net::TcpStream`], [`std::io`]
//!   and [`std::thread`].
//! * **Bounded memory.** Latency samples are capped at
//!   [`LoadConfig::max_latency_samples`]; the driver never grows its sample vector
//!   without bound on a long, fast run.
//! * **Honest accounting.** A connection reset, EOF or timeout is counted in
//!   [`LoadOutcome::errors`] and the connection is re-established; it is never counted
//!   as a successful request.
//! * **Keep-alive, one request in flight.** Each connection writes one request, reads
//!   the complete response, then repeats. A `Connection: close` response, or one
//!   without a `Content-Length`, is read to EOF and the connection is reopened on the
//!   next iteration.
//!
//! ```no_run
//! use bench::http_load::{drive, LoadConfig};
//! use std::net::SocketAddr;
//!
//! let cfg = LoadConfig::new(SocketAddr::from(([127, 0, 0, 1], 8080)), "/");
//! let outcome = drive(&cfg).unwrap();
//! println!("{} req in {} ms", outcome.requests, outcome.wall_ms);
//! ```

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

/// Per-socket read/write timeout: a stalled peer must not hang a driver thread forever.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait before re-attempting a connection that failed to open.
const RECONNECT_DELAY: Duration = Duration::from_millis(5);

/// How to drive the load: target, request shape, concurrency and budget.
#[derive(Debug, Clone)]
pub struct LoadConfig {
    /// The server address to connect to.
    pub addr: SocketAddr,
    /// The request path (e.g. `/`).
    pub path: String,
    /// The number of concurrent connections (and driver threads).
    pub connections: usize,
    /// How long each connection keeps issuing requests.
    pub duration: Duration,
    /// Upper bound on the number of stored latency samples (keeps memory bounded).
    pub max_latency_samples: usize,
}

impl LoadConfig {
    /// A configuration with sane defaults: 8 connections, a 2 s run and at most
    /// 20 000 stored latency samples.
    pub fn new(addr: SocketAddr, path: impl Into<String>) -> LoadConfig {
        LoadConfig {
            addr,
            path: path.into(),
            connections: 8,
            duration: Duration::from_secs(2),
            max_latency_samples: 20_000,
        }
    }
}

/// The aggregated result of a [`drive`] run.
#[derive(Debug, Clone, Default)]
pub struct LoadOutcome {
    /// Total number of fully-received responses.
    pub requests: u64,
    /// Total number of failed exchanges (connect/reset/EOF/timeout).
    pub errors: u64,
    /// Wall-clock duration of the whole run, in milliseconds.
    pub wall_ms: f64,
    /// Per-request latencies in milliseconds (capped at `max_latency_samples`).
    pub latencies_ms: Vec<f64>,
}

/// Drives `cfg.addr` with `cfg.connections` concurrent keep-alive connections for
/// `cfg.duration`, returning the aggregated [`LoadOutcome`].
///
/// A driver thread never panics in normal operation; if one somehow does, the panic is
/// surfaced as an [`io::Error`] rather than silently ignored.
pub fn drive(cfg: &LoadConfig) -> io::Result<LoadOutcome> {
    let started = Instant::now();
    let mut handles = Vec::with_capacity(cfg.connections);
    for _ in 0..cfg.connections {
        let cfg = cfg.clone();
        handles.push(thread::spawn(move || drive_connection(&cfg)));
    }

    let mut requests = 0u64;
    let mut errors = 0u64;
    let mut latencies = Vec::new();
    for handle in handles {
        let part = handle.join().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "http load driver thread panicked")
        })?;
        requests += part.requests;
        errors += part.errors;
        latencies.extend(part.latencies_ms);
    }

    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    // Bound the number of stored samples regardless of how fast the run was.
    if latencies.len() > cfg.max_latency_samples {
        latencies.truncate(cfg.max_latency_samples);
    }

    Ok(LoadOutcome {
        requests,
        errors,
        wall_ms,
        latencies_ms: latencies,
    })
}

/// The per-thread tally (kept private so [`LoadOutcome::wall_ms`] stays a single,
/// whole-run figure).
#[derive(Debug, Default)]
struct ThreadOutcome {
    requests: u64,
    errors: u64,
    latencies_ms: Vec<f64>,
}

/// Runs one connection: open a socket, then issue keep-alive requests until the
/// deadline, reconnecting after any transport/protocol error or a closed connection.
fn drive_connection(cfg: &LoadConfig) -> ThreadOutcome {
    let deadline = Instant::now() + cfg.duration;
    let host = cfg.addr.to_string();
    let mut out = ThreadOutcome::default();
    let mut stream: Option<TcpStream> = None;

    while Instant::now() < deadline {
        if stream.is_none() {
            match connect(cfg.addr) {
                Ok(s) => stream = Some(s),
                Err(_) => {
                    out.errors += 1;
                    thread::sleep(RECONNECT_DELAY);
                    continue;
                }
            }
        }
        let Some(conn) = stream.as_mut() else {
            continue;
        };

        let start = Instant::now();
        match exchange(conn, &cfg.path, &host) {
            Ok(reusable) => {
                out.requests += 1;
                if out.latencies_ms.len() < cfg.max_latency_samples {
                    out.latencies_ms
                        .push(start.elapsed().as_secs_f64() * 1000.0);
                }
                if !reusable {
                    stream = None;
                }
            }
            Err(_) => {
                out.errors += 1;
                stream = None;
            }
        }
    }

    out
}

/// Opens a connection and applies the per-socket timeouts and `TCP_NODELAY`.
fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    // Latency measurement is meaningless under Nagle batching; failure to set it is not
    // fatal, so it is best-effort.
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Issues one `GET`, reads the complete response and reports whether the connection
/// may be reused (`false` after `Connection: close` or a body without `Content-Length`).
fn exchange(stream: &mut TcpStream, path: &str, host: &str) -> io::Result<bool> {
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut buf = Vec::new();
    let header_end = read_through_headers(stream, &mut buf)?;
    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();

    let content_length = parse_content_length(&header_text);
    let close = connection_close(&header_text);

    match content_length {
        Some(len) => read_exact_body(stream, &buf, header_end, len)?,
        None => read_to_eof(stream)?,
    }

    Ok(!close && content_length.is_some())
}

/// Reads from `stream` into `buf` until the end of the header block (`\r\n\r\n`),
/// returning the index just past that terminator.
fn read_through_headers(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<usize> {
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(idx) = find_header_end(buf) {
            return Ok(idx);
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before HTTP headers completed",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Index just past the first `\r\n\r\n` in `buf`, if present.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Ensures exactly `need` body bytes are consumed: those already buffered after the
/// header terminator (`buf[body_start..]`) plus whatever is still on the socket.
fn read_exact_body(
    stream: &mut TcpStream,
    buf: &[u8],
    body_start: usize,
    need: usize,
) -> io::Result<()> {
    let have = buf.len().saturating_sub(body_start);
    if have >= need {
        return Ok(());
    }
    let mut remaining = need - have;
    let mut chunk = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        let n = stream.read(&mut chunk[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before the response body completed",
            ));
        }
        remaining -= n;
    }
    Ok(())
}

/// Drains the socket until the peer closes it (used for `Connection: close` and
/// length-less bodies).
fn read_to_eof(stream: &mut TcpStream) -> io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Parses the `Content-Length` header (case-insensitive) from a raw header block.
///
/// Returns `None` when the header is absent or its value does not parse as a number.
pub fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Whether the header block requests the connection be closed — i.e. a case-insensitive
/// `Connection:` header whose value (a comma-separated token list) contains `close`.
pub fn connection_close(headers: &str) -> bool {
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("connection") {
            return value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("close"));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parse_content_length_is_case_insensitive_and_trims() {
        assert_eq!(
            parse_content_length("HTTP/1.1 200 OK\r\ncontent-length: 42\r\n"),
            Some(42)
        );
        assert_eq!(
            parse_content_length("HTTP/1.1 200 OK\r\nContent-Length:   7 \r\n"),
            Some(7)
        );
        assert_eq!(
            parse_content_length("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n"),
            None
        );
        assert_eq!(parse_content_length("HTTP/1.1 200 OK\r\n\r\n"), None);
        assert_eq!(
            parse_content_length("HTTP/1.1 200 OK\r\nContent-Length: not-a-number\r\n"),
            None
        );
    }

    #[test]
    fn connection_close_detects_close_but_not_keep_alive() {
        assert!(connection_close("HTTP/1.1 200 OK\r\nConnection: close\r\n"));
        assert!(connection_close("HTTP/1.1 200 OK\r\nconnection: CLOSE\r\n"));
        assert!(connection_close(
            "HTTP/1.1 200 OK\r\nConnection: keep-alive, close\r\n"
        ));
        assert!(!connection_close(
            "HTTP/1.1 200 OK\r\nConnection: keep-alive\r\n"
        ));
        assert!(!connection_close("HTTP/1.1 200 OK\r\n\r\n"));
    }

    #[test]
    fn find_header_end_locates_the_terminator() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(find_header_end(b"no terminator here"), None);
        assert_eq!(find_header_end(b"\r\n\r\n"), Some(4));
    }

    #[test]
    fn drive_reports_requests_and_latencies_against_a_canned_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        thread::spawn(move || serve_canned(listener));

        let mut cfg = LoadConfig::new(addr, "/");
        cfg.connections = 2;
        cfg.duration = Duration::from_millis(300);
        let outcome = drive(&cfg).expect("drive");

        assert!(
            outcome.requests > 0,
            "expected at least one response, got {outcome:?}"
        );
        assert!(!outcome.latencies_ms.is_empty());
        assert!(
            outcome.wall_ms >= 250.0,
            "wall should cover the run window, got {}",
            outcome.wall_ms
        );
    }

    #[test]
    fn drive_caps_the_number_of_stored_latency_samples() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        thread::spawn(move || serve_canned(listener));

        let mut cfg = LoadConfig::new(addr, "/");
        cfg.connections = 4;
        cfg.duration = Duration::from_millis(300);
        cfg.max_latency_samples = 5;
        let outcome = drive(&cfg).expect("drive");

        assert!(outcome.requests > 0);
        assert!(
            outcome.latencies_ms.len() <= 5,
            "cap not honoured: {} samples",
            outcome.latencies_ms.len()
        );
    }

    #[test]
    fn drive_against_a_dead_port_records_errors_and_no_requests() {
        // Bind then drop the listener so the port is (almost certainly) closed.
        let addr = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("local addr")
        };
        let mut cfg = LoadConfig::new(addr, "/");
        cfg.connections = 1;
        cfg.duration = Duration::from_millis(100);
        let outcome = drive(&cfg).expect("drive");
        assert_eq!(outcome.requests, 0);
        assert!(outcome.errors > 0);
    }

    /// A minimal keep-alive HTTP/1.1 server that answers every request with a fixed body.
    /// It leaks its accept thread by design (the test process exits when the test ends).
    fn serve_canned(listener: TcpListener) {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                let mut acc: Vec<u8> = Vec::new();
                loop {
                    // Consume one request (up to its header terminator).
                    while !acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut chunk) {
                            Ok(0) => return,
                            Ok(n) => acc.extend_from_slice(&chunk[..n]),
                            Err(_) => return,
                        }
                    }
                    acc.clear();
                    let body = "Hello world!";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if s.write_all(resp.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    }
}
