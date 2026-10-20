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
//! In-module tests for the `spawn` driver's **teardown ordering**: closing a port must give the
//! child every byte the port queued before the close, which means the child's stdin is closed (its
//! end of file) *before* anything is killed, and the child is reaped only once it exited on its
//! own.
//!
//! The measured row these pin (`OTP 29.1.1`/`erts-17.1`): `erlang:port_command(P, Data)`
//! immediately followed by `erlang:port_close(P)` delivers **all** of `Data` to a plain `spawn`
//! port's child — a `tee` child received 500 000/500 000 bytes, while `port_close/1` returned
//! `true` with 454 656 bytes still queued. The port layer's half is
//! [`crate::Port::close`] (flush, then drain the output queue into the kernel pipe, then
//! `Driver::stop`); this module pins the driver's half — see [`super::SpawnDriver`]'s lifecycle
//! docs and `CHILD_EXIT_WAIT_MS`.
//!
//! Split out of `spawn.rs` only because that file is the driver source half; the tests are still
//! the driver's own in-module suite (`super` is the `spawn` module).
use super::{SpawnDriver, SpawnOptions, SpawnSpec};
use crate::driver::{Driver, DriverContext, PortArms};
use crate::poller::Poller;
use crate::port::{PortCommandOptions, PortCommandOutcome};
use crate::port_task::PortNotification;
use crate::queue::ByteQueue;
use crate::registry::PortRegistry;
use crate::DEFAULT_READINESS_RED_BUDGET;
use beam_common::BeamError;
use beam_process::PortId;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The generous outer **hang guard** for this suite's waits.
///
/// Every wait below polls the *condition* it is after — the port reported `Closed`, the child's
/// file holds the whole payload, the oracle's file reached its final size — so it ends as soon as
/// the condition holds, however heavily the host is loaded. This bound is deliberately **not** a
/// budget tuned for an idle host: it exists only so a genuinely broken driver fails a test in
/// bounded time instead of hanging the suite, and a healthy run returns long before it. It is the
/// same guard (and the same rationale) as the `spawn.rs` unit suite's.
const WAIT_HANG_GUARD: Duration = Duration::from_secs(60);

/// Wall-clock budget for the `erl` oracle before the affected cross-check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// The payload every case below queues in one `port_command/2`, in bytes.
///
/// Deliberately **larger than a pipe buffer** (Linux's default is 64 KiB): a payload that fits in
/// the pipe would be handed to the child by the write alone, so the teardown ordering would never
/// be exercised — the interesting state is "bytes the child has not read yet when the port closes".
const PAYLOAD_LEN: usize = 512 * 1024;

/// A deterministic payload of `len` bytes (a repeated 16-byte unit, so the oracle expression can
/// rebuild it with `binary:copy/2` instead of a megabyte of literals).
fn payload(len: usize) -> Vec<u8> {
    const UNIT: &[u8; 16] = b"0123456789abcdef";
    UNIT.iter().copied().cycle().take(len).collect()
}

/// A `{spawn, Command}` spec (the shell form).
fn shell(command: &str) -> SpawnSpec {
    SpawnSpec::Spawn {
        command: command.to_string(),
        argv: None,
    }
}

/// A private scratch directory under `$TMPDIR` for one case's file.
fn scratch(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "beam_io_spawn_teardown_{label}_{}_{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("the scratch directory is created");
    dir
}

/// One reactor round: wait for readiness, schedule what it implies and run the port tasks,
/// answering the round's notifications.
///
/// `poll` is retried on `EINTR`, which the `SIGCHLD` deliveries of the parallel cases in this
/// binary make a normal outcome.
fn round(poller: &mut Poller, registry: &mut PortRegistry) -> Vec<PortNotification> {
    let events = loop {
        match poller.poll(Some(Duration::from_millis(10))) {
            Ok(events) => break events,
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => panic!("the poll succeeds: {err:?}"),
        }
    };
    registry.schedule_events(&events);
    registry
        .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("port tasks run")
        .notifications
}

/// Runs rounds until `id`'s [`PortNotification::Closed`] report arrives, answering whether it did
/// within [`WAIT_HANG_GUARD`].
///
/// The close of a port whose output does not fit in the pipe **parks** it in
/// [`crate::PortState::Closing`] (the C's `ERTS_PORT_SFLG_CLOSING`), so the rounds are what finish
/// the drain — and, once the queue is empty, what runs the driver's teardown and reports the close.
fn pump_until_closed(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> bool {
    let deadline = Instant::now() + WAIT_HANG_GUARD;
    loop {
        let notifications = round(poller, registry);
        if notifications.iter().any(
            |notification| matches!(notification, PortNotification::Closed { port } if *port == id),
        ) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

/// A `spawn` driver plus the four port-side buffers a [`DriverContext`] needs, so the driver's
/// teardown can be driven without a [`crate::Port`] (the `ram_file` suite's `Harness` shape).
struct Harness {
    poller: Poller,
    input: ByteQueue,
    output: ByteQueue,
    arms: PortArms,
}

impl Harness {
    fn new() -> Harness {
        Harness {
            poller: Poller::new().expect("a poll set"),
            input: ByteQueue::new(),
            output: ByteQueue::new(),
            arms: PortArms::default(),
        }
    }

    /// A context over this harness's buffers, exactly as a port builds one.
    fn ctx(&mut self) -> DriverContext<'_> {
        DriverContext::new(
            PortId::new(1),
            &mut self.poller,
            &mut self.input,
            &mut self.output,
            &mut self.arms,
        )
    }
}

/// Queues `bytes` on the port's output side and drives the driver's write path until the queue is
/// empty, under [`WAIT_HANG_GUARD`].
///
/// A stalling write (`EAGAIN`, a full pipe) leaves the remainder queued and is retried after a
/// short sleep: the child drains the pipe at its own pace, and readiness would normally re-drive
/// the write, which this harness does not need to wait for.
fn write_all(driver: &mut SpawnDriver, harness: &mut Harness, bytes: &[u8]) {
    harness.output.push(bytes);
    let deadline = Instant::now() + WAIT_HANG_GUARD;
    while !harness.output.is_empty() {
        let mut ctx = harness.ctx();
        driver.output(&mut ctx).expect("the write succeeds");
        drop(ctx);
        if harness.output.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the child drains the pipe so the whole payload is written"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn stop_gives_the_child_its_eof_before_anything_is_killed() {
    // The defect this pins: `stop` used to `kill(2)` the child while the bytes the port had just
    // drained were still in the pipe, so the child never read them, its file stayed empty and its
    // status was `128 + 9` instead of its own `0`.
    let dir = scratch("eof");
    let file = dir.join("payload");
    let mut harness = Harness::new();
    let options = SpawnOptions {
        exit_status: true,
        ..SpawnOptions::default()
    };
    let mut driver = SpawnDriver::new(&shell(&format!("cat > {}", file.display())), &options)
        .expect("the shell starts");
    {
        // Exactly the port's sequence: `start`, then the close's `flush`, then the drained
        // output, then `stop` (see `Port::close`).
        let mut ctx = harness.ctx();
        driver.start(&mut ctx).expect("the stdout registration");
        driver.flush(&mut ctx).expect("the flush succeeds");
    }

    let payload = payload(PAYLOAD_LEN);
    write_all(&mut driver, &mut harness, &payload);
    assert!(
        harness.output.is_empty(),
        "the whole payload reached the kernel pipe"
    );

    {
        let mut ctx = harness.ctx();
        driver.stop(&mut ctx).expect("the stop succeeds");
    }

    // (e) The status is the child's own: `0` means it ran to completion, `128 + 9` would mean the
    // teardown killed it with `SIGKILL`.
    assert_eq!(
        driver.take_exit_status().expect("the wait succeeds"),
        Some(0),
        "the child exits on its own after its end of file, it is not killed"
    );
    assert_eq!(
        std::fs::read(&file).expect("the child's file"),
        payload,
        "every byte queued before the close reaches the child"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn closing_the_port_delivers_every_queued_byte_to_the_child() {
    // The same row through the whole port layer: `port_command/2` then `port_close/1`, with the
    // payload larger than a pipe buffer so the close parks the port in `Closing` and the rounds
    // that follow finish the drain — after which the driver's teardown must not throw the tail
    // away.
    let dir = scratch("close");
    let file = dir.join("payload");
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let options = SpawnOptions {
        exit_status: true,
        ..SpawnOptions::default()
    };
    let driver = SpawnDriver::new(&shell(&format!("cat > {}", file.display())), &options)
        .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    let payload = payload(PAYLOAD_LEN);
    assert_eq!(
        registry
            .port_command_with(&mut poller, id, &payload, PortCommandOptions::new())
            .expect("the command is accepted"),
        PortCommandOutcome::Done,
        "an idle port takes the whole command"
    );
    registry.close(&mut poller, id).expect("the port closes");
    assert!(
        pump_until_closed(&mut poller, &mut registry, id),
        "the parked close finishes and is reported"
    );

    assert_eq!(
        std::fs::read(&file).expect("the child's file"),
        payload,
        "the child received every byte of the command the close followed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_child_that_never_reads_stdin_does_not_hang_the_close() {
    // The fallback half of the fix: a child that never reads its stdin cannot exit on its own, so
    // the bounded wait in `stop` expires and the abrupt kill-and-reap keeps the close finite. The
    // single-threaded port pump must never be parked for as long as such a child lives (the C
    // would block in its own teardown path; this floor cannot).
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    // `exec` so the child really is the `sleep` that gets killed (no shell wrapper left behind).
    let driver = SpawnDriver::new(&shell("exec sleep 30"), &SpawnOptions::default())
        .expect("the child starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");
    let payload = payload(PAYLOAD_LEN);
    assert_eq!(
        registry
            .port_command_with(&mut poller, id, &payload, PortCommandOptions::new())
            .expect("a full pipe stalls, it does not fail"),
        PortCommandOutcome::Done,
        "an idle port takes the command and stalls in the write"
    );

    let started = Instant::now();
    registry.close(&mut poller, id).expect("the port closes");
    assert!(
        pump_until_closed(&mut poller, &mut registry, id),
        "the close completes and is reported"
    );
    // Far below the child's own 30 s: the fallback kill fired instead of the close waiting for a
    // child that will never read what it was sent.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the bounded wait gives up instead of parking the port pump ({:?})",
        started.elapsed()
    );
}

// -----------------------------------------------------------------------
// Live cross-check against the system `erl`
// -----------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after
/// printing a clear skip message — when `erl` cannot be spawned or does not answer within
/// [`ERL_TIMEOUT`]. A spawned `erl` that exits non-zero is a bug in the expression, so it fails the
/// test.
///
/// A local copy of the helper the `ram_file` suite keeps: a `#[cfg(test)]` module's private helpers
/// are not reachable across those boundaries, and the crate's convention is to skip **loudly**
/// rather than fail when the oracle is unavailable.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(
            Command::new("erl")
                .arg("-noshell")
                .arg("-eval")
                .arg(&code)
                .output(),
        );
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("SKIP: skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!(
                "SKIP: skipping oracle check: could not spawn `erl` ({err}); is Erlang on PATH?"
            );
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle expression failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Reads `path` until it holds at least `len` bytes, under [`WAIT_HANG_GUARD`], and answers
/// whatever it read.
///
/// The wait is the oracle's asynchronous drain: `port_close/1` returns while bytes are still
/// queued, so the child finishes writing its file some time after the close returned.
fn wait_for_file(path: &Path, len: usize) -> Vec<u8> {
    let deadline = Instant::now() + WAIT_HANG_GUARD;
    loop {
        let bytes = std::fs::read(path).unwrap_or_default();
        if bytes.len() >= len || Instant::now() >= deadline {
            return bytes;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_oracle_delivers_every_byte_of_a_closed_ports_command_too() {
    // The measured row, both sides of the same question: `port_command(P, Data)` followed
    // immediately by `port_close(P)` must leave the child's file holding **all** of `Data`. The
    // oracle's close is asynchronous with respect to its return (it answers `true` while bytes are
    // still queued), so its file is waited for rather than read once.
    //
    // The child is a `tee`, not a `cat > file`, and that is the whole point of the shape: a child
    // whose stdout is redirected closes the port's **read** stream, and the C treats `read(2) == 0`
    // as `driver_failure_eof` (`erts/emulator/sys/unix/sys_drivers.c:1235-1252`), which terminates
    // the port and with it drops the queued `ioq` — measured: the child of a `cat > file` port gets
    // exactly one pipe buffer (65 536 of 524 288 bytes) under **both** runtimes. A `tee` keeps
    // writing to the port's stdout, so the read stream stays open and the drain the close started
    // really is what decides the outcome. This is the shape the row was measured in.
    let len = PAYLOAD_LEN;
    let payload = payload(len);

    // The Rust side: the identical command and close through the port layer.
    let rust_dir = scratch("oracle_rust");
    let rust_file = rust_dir.join("payload");
    {
        let mut poller = Poller::new().expect("a poll set");
        let mut registry = PortRegistry::new();
        let driver = SpawnDriver::new(
            &shell(&format!("tee {}", rust_file.display())),
            &SpawnOptions::default(),
        )
        .expect("the shell starts");
        let id = registry
            .open(&mut poller, Box::new(driver))
            .expect("the port opens");
        assert_eq!(
            registry
                .port_command_with(&mut poller, id, &payload, PortCommandOptions::new())
                .expect("the command is accepted"),
            PortCommandOutcome::Done
        );
        registry.close(&mut poller, id).expect("the port closes");
        assert!(
            pump_until_closed(&mut poller, &mut registry, id),
            "the close finishes and is reported"
        );
    }
    let rust_bytes = wait_for_file(&rust_file, len);

    // The oracle side: the same two calls on a plain `spawn` port.
    let oracle_dir = scratch("oracle_erl");
    let oracle_file = oracle_dir.join("payload");
    let expr = format!(
        "Data = binary:copy(<<\"0123456789abcdef\">>, {}), \
         P = open_port({{spawn, \"tee {}\"}}, [binary]), \
         true = port_command(P, Data), \
         true = port_close(P), \
         timer:sleep(800)",
        len / 16,
        oracle_file.display()
    );
    let Some(_stdout) = erl_stdout(&expr) else {
        let _ = std::fs::remove_dir_all(&rust_dir);
        let _ = std::fs::remove_dir_all(&oracle_dir);
        return;
    };
    let oracle_bytes = wait_for_file(&oracle_file, len);

    assert_eq!(
        rust_bytes.len(),
        len,
        "the child of the rust port received every byte ({rust_bytes:?} bytes)"
    );
    assert_eq!(
        oracle_bytes.len(),
        len,
        "the oracle's child received every byte ({oracle_bytes:?} bytes)"
    );
    assert_eq!(
        rust_bytes, oracle_bytes,
        "the bytes the child received are identical under both runtimes"
    );
    assert_eq!(payload.len(), len, "the payload is the one both sides sent");

    let _ = std::fs::remove_dir_all(&rust_dir);
    let _ = std::fs::remove_dir_all(&oracle_dir);
}
