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

//! The `cowboy` serve half: spawn → wait for ready → load → peak RSS → stop.
//!
//! `cowboy_setup` gets a staged, built tree and the per-runtime argv; this module drives
//! it. `serve` spawns it, waits for the literal `CW_READY` stdout line (never a bare TCP
//! connect: a connect can succeed before the listener serves and it would dirty the
//! connection accounting), drains both streams on threads, polls peak RSS, drives the
//! in-process load generator, then kills/reaps the child and waits until the port is free
//! again.
//!
//! Two honesty rules shape the failure surface:
//!
//! * a serve failure is a **typed** `ServeError`, never a silent zero;
//! * the Rust runtime's `run` action kills the whole run at `RUN_DEADLINE_MS` (60 s,
//!   **not** overridable), so the load window is `clamp_load_duration`-clamped into a
//!   documented `RUN_SAFETY_MS` bound and the clamp is stated in a note — a row never
//!   silently rides into a `did not terminate within 60000 ms`.
//!
//! The stage/build/argv surface a row needs stays reachable through this module, because
//! `cowboy` drives a whole row through it: `stage_and_build`, `server_argv`,
//! `run_deadline_ms`, `LoadPlan`, `tail` and `OUTPUT_TAIL_CHARS` are re-exported from
//! `cowboy_setup`.

use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::http_load::{self, LoadConfig, LoadOutcome};
use crate::proc;

pub(super) use super::cowboy_setup::{
    run_deadline_ms, server_argv, stage_and_build, tail, LoadPlan, OUTPUT_TAIL_CHARS,
};
use super::cowboy_setup::{
    serve_command, MIN_LOAD_MS, PORT, READY_MARKER, RUN_DEADLINE_MS, TEARDOWN_ALLOWANCE_MS,
};

/// How long to wait for the harness' [`READY_MARKER`] on stdout.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the ready loop re-checks the transcript / the child.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long to wait for the port to be released after the server is killed.
const PORT_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the port is re-probed while waiting for its release.
const PORT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How often the peak-RSS poller samples the server process.
const PEAK_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Upper bound on each drained stream we retain (the run is not a log collector).
const TRANSCRIPT_CAP: usize = 256 * 1024;

/// Cowboy's default `max_keepalive`: it closes a keep-alive connection after this many
/// requests, which is what a long load run reports as a transport error.
pub(super) const KEEPALIVE_MAX_REQUESTS: u64 = 1000;

/// A successful serve window.
#[derive(Debug, Clone)]
pub(super) struct ServeOutcome {
    /// Spawn-to-`CW_READY` wall time in milliseconds (boot included).
    pub ready_ms: f64,
    /// The aggregated load result.
    pub load: LoadOutcome,
    /// The load window actually used (after any clamp).
    pub load_duration: Duration,
    /// The clamp note, when the load window had to be shortened.
    pub clamp_note: Option<String>,
    /// The server's peak RSS, when `/proc` was readable.
    pub peak_memory_bytes: Option<u64>,
    /// The drained stdout.
    pub stdout: String,
    /// The drained stderr.
    pub stderr: String,
    /// Whether either stream hit [`TRANSCRIPT_CAP`].
    pub transcript_truncated: bool,
    /// Whether the port was released again after the server was killed.
    pub port_released: bool,
    /// Whether a drain/poller thread panicked (their data is then incomplete).
    pub worker_panicked: bool,
}

/// A serve-window failure.
#[derive(Debug, Clone)]
pub(super) enum ServeError {
    /// Something else holds the port.
    PortBusy,
    /// The child could not be spawned.
    Spawn(String),
    /// The child exited before printing [`READY_MARKER`].
    ExitedEarly(String),
    /// The child never printed [`READY_MARKER`].
    NotReady(String),
    /// `try_wait` failed.
    WaitFailed(String),
    /// The load generator failed.
    Load(String),
    /// The boot left no usable load window inside the runtime's deadline.
    Deadline(String),
}

impl ServeError {
    /// The short `failed(<reason>)` string recorded on the result.
    pub(super) fn reason(&self) -> String {
        match self {
            ServeError::PortBusy => "port-busy".to_string(),
            ServeError::Spawn(e) => format!("server-spawn-failed: {e}"),
            ServeError::ExitedEarly(t) => format!("server-exited-early: {t}"),
            ServeError::NotReady(t) => format!("server-not-ready: {t}"),
            ServeError::WaitFailed(e) => format!("server-wait-failed: {e}"),
            ServeError::Load(e) => format!("load-failed: {e}"),
            ServeError::Deadline(d) => format!("runtime-deadline-exceeded: {d}"),
        }
    }
}

/// Spawns one side, waits until it serves, drives it, and stops it again.
pub(super) fn serve(
    program: &Path,
    argv: &[String],
    plan: LoadPlan,
    deadline_ms: Option<f64>,
) -> Result<ServeOutcome, ServeError> {
    // Port hygiene: a stale listener would make the harness fail to bind, and a
    // `ranch_tcp:listen` `{error, eaddrinuse}` is indistinguishable from a boot failure
    // in the transcript. Refuse up front instead.
    if !port_is_free(PORT) {
        return Err(ServeError::PortBusy);
    }

    let mut cmd = serve_command(program, argv, PORT);
    let child = proc::spawn(&mut cmd).map_err(|e| ServeError::Spawn(e.to_string()))?;
    let mut server = RunningServer::attach(child);

    let ready_ms = match server.wait_ready(READY_TIMEOUT) {
        ReadyWait::Ready(ms) => ms,
        ReadyWait::Exited => {
            let stopped = server.stop();
            return Err(ServeError::ExitedEarly(tail(
                &stopped.transcript(),
                OUTPUT_TAIL_CHARS,
            )));
        }
        ReadyWait::Timeout => {
            let stopped = server.stop();
            return Err(ServeError::NotReady(tail(
                &stopped.transcript(),
                OUTPUT_TAIL_CHARS,
            )));
        }
        ReadyWait::WaitFailed(e) => {
            let stopped = server.stop();
            return Err(ServeError::WaitFailed(format!(
                "{e}: {}",
                tail(&stopped.transcript(), OUTPUT_TAIL_CHARS)
            )));
        }
    };

    let window = clamp_load_duration(plan.duration, ready_ms, deadline_ms)
        .map_err(ServeError::Deadline)?;

    let mut cfg = LoadConfig::new(SocketAddr::from(([127, 0, 0, 1], PORT)), "/");
    cfg.connections = plan.connections;
    cfg.duration = window.duration;
    cfg.max_latency_samples = plan.max_latency_samples;
    let load = http_load::drive(&cfg);

    let stopped = server.stop();
    let load = load.map_err(|e| ServeError::Load(e.to_string()))?;

    Ok(ServeOutcome {
        ready_ms,
        load,
        load_duration: window.duration,
        clamp_note: window.note,
        peak_memory_bytes: stopped.peak_memory_bytes,
        stdout: stopped.stdout,
        stderr: stopped.stderr,
        transcript_truncated: stopped.transcript_truncated,
        port_released: stopped.port_released,
        worker_panicked: stopped.worker_panicked,
    })
}

/// How the ready wait ended.
enum ReadyWait {
    /// The marker was seen, this many milliseconds after the spawn.
    Ready(f64),
    /// The child exited first.
    Exited,
    /// The timeout elapsed first.
    Timeout,
    /// `try_wait` itself failed.
    WaitFailed(String),
}

/// The load window after [`clamp_load_duration`].
#[derive(Debug, Clone, PartialEq)]
pub(super) struct LoadWindow {
    /// The duration to actually drive.
    pub duration: Duration,
    /// A note naming the clamp, when one was applied.
    pub note: Option<String>,
}

/// Shortens `requested` so the whole run stays inside `deadline_ms`.
///
/// `deadline_ms` is the runtime's own deadline minus our safety margin
/// ([`RUN_SAFETY_MS`](super::cowboy_setup::RUN_SAFETY_MS)) — `None` means the runtime
/// imposes none (the oracle is killed by us). The boot time (`ready_ms`) and a
/// [`TEARDOWN_ALLOWANCE_MS`] reservation are subtracted from it; a window shorter than
/// [`MIN_LOAD_MS`] is **not** clamped but refused, so the row fails loudly instead of
/// publishing a meaningless figure.
pub(super) fn clamp_load_duration(
    requested: Duration,
    ready_ms: f64,
    deadline_ms: Option<f64>,
) -> Result<LoadWindow, String> {
    let Some(deadline) = deadline_ms else {
        return Ok(LoadWindow {
            duration: requested,
            note: None,
        });
    };
    let requested_ms = requested.as_secs_f64() * 1000.0;
    let budget = deadline - ready_ms - TEARDOWN_ALLOWANCE_MS;
    if requested_ms <= budget {
        return Ok(LoadWindow {
            duration: requested,
            note: None,
        });
    }
    if budget < MIN_LOAD_MS {
        return Err(format!(
            "the server took {ready_ms:.0} ms to serve, which leaves {budget:.0} ms of the \
             {deadline:.0} ms safety bound inside this runtime's {RUN_DEADLINE_MS:.0} ms `run` \
             deadline — less than the {MIN_LOAD_MS:.0} ms minimum load window"
        ));
    }
    let note = format!(
        "load window clamped from {requested_ms:.0} ms to {budget:.0} ms: the boot took \
         {ready_ms:.0} ms and this runtime's `run` action kills the whole run (boot + load + \
         teardown) at {RUN_DEADLINE_MS:.0} ms, so the row keeps it under a {deadline:.0} ms \
         safety bound less a {TEARDOWN_ALLOWANCE_MS:.0} ms teardown allowance — the figure is a \
         shorter window, so compare it with that stated"
    );
    Ok(LoadWindow {
        duration: Duration::from_millis(budget as u64),
        note: Some(note),
    })
}

/// A child that is (or is about to be) serving, with its drains and peak poller running.
struct RunningServer {
    child: Child,
    quiesce: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    poller: Option<thread::JoinHandle<()>>,
    out: Arc<Mutex<Transcript>>,
    err: Arc<Mutex<Transcript>>,
    out_drain: Option<thread::JoinHandle<()>>,
    err_drain: Option<thread::JoinHandle<()>>,
}

impl RunningServer {
    /// Takes over a freshly spawned child: drains both streams and starts the poller.
    fn attach(mut child: Child) -> RunningServer {
        let pid = child.id();
        let out = Arc::new(Mutex::new(Transcript::default()));
        let err = Arc::new(Mutex::new(Transcript::default()));
        let out_drain = child
            .stdout
            .take()
            .map(|stream| drain_stream(stream, Arc::clone(&out)));
        let err_drain = child
            .stderr
            .take()
            .map(|stream| drain_stream(stream, Arc::clone(&err)));

        let quiesce = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let poller = {
            let quiesce = Arc::clone(&quiesce);
            let peak = Arc::clone(&peak);
            thread::spawn(move || {
                while !quiesce.load(Ordering::Relaxed) {
                    if let Some(bytes) = proc::read_vmhwm(pid) {
                        peak.fetch_max(bytes, Ordering::Relaxed);
                    }
                    thread::sleep(PEAK_POLL_INTERVAL);
                }
            })
        };

        RunningServer {
            child,
            quiesce,
            peak,
            poller: Some(poller),
            out,
            err,
            out_drain,
            err_drain,
        }
    }

    /// Waits for [`READY_MARKER`] on stdout, the child's exit, or the timeout.
    fn wait_ready(&mut self, timeout: Duration) -> ReadyWait {
        let start = Instant::now();
        loop {
            if transcript_contains(&self.out, READY_MARKER) {
                return ReadyWait::Ready(start.elapsed().as_secs_f64() * 1000.0);
            }
            match self.child.try_wait() {
                Ok(Some(_)) => return ReadyWait::Exited,
                Ok(None) => {}
                Err(e) => return ReadyWait::WaitFailed(e.to_string()),
            }
            if start.elapsed() >= timeout {
                return ReadyWait::Timeout;
            }
            thread::sleep(READY_POLL_INTERVAL);
        }
    }

    /// Kills and reaps the child, joins every worker, and waits for the port to be free.
    fn stop(mut self) -> StoppedServer {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.quiesce.store(true, Ordering::Relaxed);

        // A panicking drain/poller would silently lose output, so it is reported rather
        // than joined-and-forgotten.
        let mut worker_panicked = false;
        if let Some(handle) = self.poller.take() {
            worker_panicked |= handle.join().is_err();
        }
        if let Some(handle) = self.out_drain.take() {
            worker_panicked |= handle.join().is_err();
        }
        if let Some(handle) = self.err_drain.take() {
            worker_panicked |= handle.join().is_err();
        }

        let peak = match self.peak.load(Ordering::Relaxed) {
            0 => None,
            bytes => Some(bytes),
        };
        let (stdout, out_truncated) = take_transcript(&self.out);
        let (stderr, err_truncated) = take_transcript(&self.err);

        StoppedServer {
            peak_memory_bytes: peak,
            stdout,
            stderr,
            transcript_truncated: out_truncated || err_truncated,
            port_released: wait_until_port_free(PORT, PORT_RELEASE_TIMEOUT),
            worker_panicked,
        }
    }
}

/// A server that has been killed, reaped and had its port released.
struct StoppedServer {
    peak_memory_bytes: Option<u64>,
    stdout: String,
    stderr: String,
    transcript_truncated: bool,
    port_released: bool,
    worker_panicked: bool,
}

impl StoppedServer {
    /// Both transcripts, for a bounded failure note.
    fn transcript(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// A bounded capture of one child stream.
#[derive(Debug, Default)]
struct Transcript {
    text: String,
    truncated: bool,
}

impl Transcript {
    /// Appends `chunk`, keeping at most [`TRANSCRIPT_CAP`] bytes.
    fn push(&mut self, chunk: &str) {
        if self.text.len() >= TRANSCRIPT_CAP {
            self.truncated = true;
            return;
        }
        let room = TRANSCRIPT_CAP - self.text.len();
        if chunk.len() > room {
            // Never split a char: back off to the last boundary inside `room`.
            let mut end = room;
            while end > 0 && !chunk.is_char_boundary(end) {
                end -= 1;
            }
            self.text.push_str(&chunk[..end]);
            self.truncated = true;
        } else {
            self.text.push_str(chunk);
        }
    }
}

/// Locks a transcript, recovering the data if a writer panicked (poison is not
/// corruption: the `String` is still consistent).
fn lock(transcript: &Arc<Mutex<Transcript>>) -> MutexGuard<'_, Transcript> {
    transcript.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether the transcript already contains `needle`.
fn transcript_contains(transcript: &Arc<Mutex<Transcript>>, needle: &str) -> bool {
    lock(transcript).text.contains(needle)
}

/// Moves the transcript's text out, together with its truncation flag.
fn take_transcript(transcript: &Arc<Mutex<Transcript>>) -> (String, bool) {
    let mut guard = lock(transcript);
    (std::mem::take(&mut guard.text), guard.truncated)
}

/// Reads `reader` to EOF on its own thread, appending everything to `sink`.
fn drain_stream<R: Read + Send + 'static>(
    mut reader: R,
    sink: Arc<Mutex<Transcript>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => lock(&sink).push(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break, // the child is gone; nothing more can arrive
            }
        }
    })
}

/// Whether `127.0.0.1:<port>` can be bound right now (i.e. nothing is listening).
pub(super) fn port_is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Waits (bounded) until `port` can be bound again.
fn wait_until_port_free(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if port_is_free(port) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(PORT_POLL_INTERVAL);
    }
}

/// The first `max_chars` characters of `text` (trimmed), for a bounded note.
pub(super) fn head(text: &str, max_chars: usize) -> String {
    text.trim().chars().take(max_chars).collect()
}

/// How many `=ERROR REPORT====` blocks the drained stdout contains.
///
/// The Rust runtime writes crash reports to **stdout**, so this is the count of
/// processes that died while serving — a number that must be visible on the row, since a
/// server that keeps answering while its connection processes crash is not a healthy
/// server. A truncated transcript makes it a lower bound (see the run note).
pub(super) fn count_crash_reports(transcript: &str) -> usize {
    transcript.matches("=ERROR REPORT====").count()
}

/// A bounded sample of the first `=ERROR REPORT====` block, if there is one.
///
/// The sample keeps the **head** of the block, because the actionable part of an Erlang
/// crash report is its headline (`exception error: <reason>`) — the stack below it is
/// long, repetitive and useless when truncated from the front.
pub(super) fn crash_report_sample(transcript: &str, max_chars: usize) -> Option<String> {
    const MARKER: &str = "=ERROR REPORT====";
    let start = transcript.find(MARKER)?;
    let rest = &transcript[start..];
    // Cut at the *next* report, so the sample is exactly one block.
    let block = match rest[MARKER.len()..].find(MARKER) {
        Some(offset) => &rest[..MARKER.len() + offset],
        None => rest,
    };
    Some(head(block, max_chars))
}

/// The keep-alive-close explanation for a run.
///
/// Cowboy's default `max_keepalive` closes a keep-alive connection after
/// [`KEEPALIVE_MAX_REQUESTS`] requests. A close the server announces with
/// `Connection: close` is a **completed** request for the driver (it reconnects without
/// counting anything); a close that races the next request shows up as one transport
/// error. A long run therefore reports either zero errors or a handful that are closes —
/// never failed requests — and this note states which, derived from the measured counts.
pub(super) fn keepalive_close_note(requests: u64, errors: u64) -> String {
    let expected_closes = requests / KEEPALIVE_MAX_REQUESTS;
    if errors == 0 {
        format!(
            "no transport errors across {requests} request(s): every response was complete, \
             including the reconnects after Cowboy's default max_keepalive closed a keep-alive \
             connection ({KEEPALIVE_MAX_REQUESTS} requests per connection ⇒ ~{expected_closes} \
             close(s) for this request count, each announced with `Connection: close` and \
             therefore a completed request)"
        )
    } else {
        format!(
            "{errors} transport error(s) across {requests} request(s): Cowboy's default \
             max_keepalive closes each keep-alive connection after {KEEPALIVE_MAX_REQUESTS} \
             requests, and a close that races the driver's next request is counted as one \
             error — ~{expected_closes} such close(s) are expected for this request count, so a \
             few errors are reconnects, not failed requests (a count far above that IS a real \
             failure)"
        )
    }
}

/// `/proc/loadavg`, or `None` when unreadable — the host is shared, so this is recorded
/// around the run and the ratio (never the absolute wall time) is the reading.
pub(super) fn loadavg() -> Option<String> {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workloads::cowboy_setup::RUN_SAFETY_MS;

    #[test]
    fn a_sys_side_is_never_clamped_and_a_rust_side_clamps_under_the_deadline() {
        let requested = Duration::from_secs(2);

        // No deadline ⇒ unchanged (the oracle is killed by us).
        let open = clamp_load_duration(requested, 100.0, None).expect("no deadline");
        assert_eq!(open.duration, requested);
        assert_eq!(open.note, None);

        // A comfortable boot inside the bound ⇒ unchanged.
        let roomy = clamp_load_duration(requested, 100.0, Some(RUN_SAFETY_MS)).expect("roomy");
        assert_eq!(roomy.duration, requested);
        assert_eq!(roomy.note, None);

        // A boot that eats most of the bound ⇒ clamped, with the clamp stated.
        let tight = clamp_load_duration(requested, 46_000.0, Some(RUN_SAFETY_MS))
            .expect("still fits a minimum window");
        assert_eq!(
            tight.duration,
            Duration::from_millis((RUN_SAFETY_MS - 46_000.0 - TEARDOWN_ALLOWANCE_MS) as u64)
        );
        let note = tight.note.expect("a clamp must be stated");
        assert!(note.contains("clamped"), "{note}");
        assert!(note.contains(&format!("{RUN_DEADLINE_MS:.0} ms")), "{note}");

        // A boot that leaves less than the minimum window ⇒ refused, never a fake figure.
        let err = clamp_load_duration(requested, 49_000.0, Some(RUN_SAFETY_MS))
            .expect_err("no usable window left");
        assert!(err.contains("minimum load window"), "{err}");
    }

    #[test]
    fn the_keepalive_note_states_the_measured_maths_and_invents_no_metric() {
        let quiet = keepalive_close_note(900, 0);
        assert!(quiet.contains("900 request(s)"), "{quiet}");
        assert!(quiet.contains("no transport errors"), "{quiet}");
        assert!(quiet.contains("~0 close(s)"), "{quiet}");

        let closes = keepalive_close_note(2_500, 3);
        assert!(closes.contains("3 transport error(s)"), "{closes}");
        assert!(closes.contains("2500 request(s)"), "{closes}");
        assert!(closes.contains("1000"), "{closes}");
        assert!(closes.contains("~2 such close(s)"), "{closes}");
        assert!(closes.contains("not failed requests"), "{closes}");
    }

    #[test]
    fn the_crash_report_counter_counts_blocks_and_samples_one_of_them() {
        let transcript = "booting\n=ERROR REPORT==== 1 ===\nreason one\n\
                          =ERROR REPORT==== 2 ===\nreason two\nstill serving\n";
        assert_eq!(count_crash_reports(transcript), 2);
        assert_eq!(count_crash_reports("all calm"), 0);

        let sample = crash_report_sample(transcript, 400).expect("a sample");
        assert!(sample.starts_with("=ERROR REPORT===="), "{sample}");
        assert!(sample.contains("reason one"), "{sample}");
        assert!(!sample.contains("reason two"), "one block only: {sample}");
        assert_eq!(crash_report_sample("all calm", 400), None);

        let long = format!("=ERROR REPORT===={}", "x".repeat(1000));
        let bounded = crash_report_sample(&long, 30).expect("sample");
        assert_eq!(bounded.chars().count(), 30);
        assert!(
            bounded.starts_with("=ERROR REPORT===="),
            "the headline must survive the bound: {bounded}"
        );
        // The head, not the tail: an exception reason sits right under the header.
        let reason = format!(
            "=ERROR REPORT==== 1 ===\nexception error: badarg\n{}",
            "stack".repeat(500)
        );
        let sample = crash_report_sample(&reason, 60).expect("sample");
        assert!(sample.contains("exception error: badarg"), "{sample}");
    }

    #[test]
    fn port_hygiene_sees_a_free_port_and_a_busy_one() {
        // Bind an ephemeral port, then ask about it: while held it is not free.
        let held = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
        let port = held.local_addr().expect("addr").port();
        assert!(!port_is_free(port), "a bound port must read as busy");
        assert!(!wait_until_port_free(port, Duration::from_millis(100)));
        drop(held);
        assert!(
            wait_until_port_free(port, Duration::from_secs(5)),
            "the port must read as free once released"
        );
    }

    #[test]
    fn the_transcript_never_grows_past_its_cap_and_never_splits_a_char() {
        let mut t = Transcript::default();
        t.push(&"a".repeat(TRANSCRIPT_CAP - 1));
        assert!(!t.truncated);
        t.push("é€xy"); // 2-byte + 3-byte + 1 + 1: only 1 byte of room, no boundary
        assert!(t.truncated);
        assert_eq!(t.text.len(), TRANSCRIPT_CAP - 1, "nothing half-encoded stuck");
        assert!(t.text.is_char_boundary(t.text.len()));
        t.push("more");
        assert_eq!(
            t.text.len(),
            TRANSCRIPT_CAP,
            "the cap itself is the hard limit"
        );
        t.push("even more");
        assert_eq!(t.text.len(), TRANSCRIPT_CAP, "cap holds");
    }

    #[test]
    fn loadavg_is_readable_or_absent_never_a_guess() {
        if let Some(v) = loadavg() {
            assert!(v.split_whitespace().count() >= 3, "{v}");
        }
    }

    #[test]
    fn a_serve_error_renders_a_typed_reason() {
        assert_eq!(ServeError::PortBusy.reason(), "port-busy");
        assert!(ServeError::ExitedEarly("boom".into())
            .reason()
            .starts_with("server-exited-early: boom"));
        assert!(ServeError::Deadline("too slow".into())
            .reason()
            .starts_with("runtime-deadline-exceeded: too slow"));
    }
}
