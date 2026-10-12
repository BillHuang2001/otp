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

//! Integration tests for the OS-child [`SpawnDriver`] (`"spawn"`): real processes behind a
//! real port.
//!
//! Every case here starts a real `/bin/sh`/`echo`/`cat` child through the driver, hands it to
//! [`PortRegistry::open`] and drives the reactor exactly as the runtime will:
//! `Poller::poll` for readiness, then `PortRegistry::handle_events` (or
//! `schedule_events` + `execute_port_tasks`) and the port's queues. Every wait is bounded, so
//! a broken driver fails the test instead of hanging it.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::prelude::*;
use beam_process::PortId;

/// A `{spawn, Command}` spec (the shell form).
fn shell(command: &str) -> SpawnSpec {
    SpawnSpec::Spawn {
        command: command.to_string(),
        argv: None,
    }
}

/// A `{spawn, [Prog, Arg…]}` spec (the direct argv form).
fn argv(program: &str, args: &[&str]) -> SpawnSpec {
    let mut vector = vec![program.to_string()];
    vector.extend(args.iter().map(|arg| arg.to_string()));
    SpawnSpec::Spawn {
        command: vector.join(" "),
        argv: Some(vector),
    }
}

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// Polls the reactor, retrying when a signal interrupts the wait.
///
/// Starting and reaping children makes `SIGCHLD` deliveries certain, and the test harness
/// runs the cases below on parallel threads of one process, so an interrupted `epoll_wait`
/// (`EINTR`, which [`Poller::poll`] surfaces as `BeamError::Io`) is a normal outcome here
/// rather than a failure — the same reason the poll loop of a real runtime retries it.
fn poll_events(poller: &mut Poller, timeout: Duration) -> Vec<Event> {
    loop {
        match poller.poll(Some(timeout)) {
            Ok(events) => return events,
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => panic!("the poll succeeds: {err:?}"),
        }
    }
}

/// Polls and dispatches until `id` has received at least `want` input bytes, returning
/// everything it received (bounded by a deadline).
fn pump_input(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
    want: usize,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut received = Vec::new();
    while received.len() < want {
        assert!(
            Instant::now() < deadline,
            "the reactor never delivered {want} bytes from the child (got {})",
            received.len()
        );
        let events = poll_events(poller, Duration::from_millis(100));
        registry
            .handle_events(poller, &events)
            .expect("events dispatch");
        received.extend(registry.get_mut(id).expect("the port is live").drain_input());
    }
    received
}

/// Polls and runs port tasks, collecting the `{data, …}` bytes the *notification* seam
/// reports (the path the runtime turns into `{Port, {data, Data}}` messages) until `want`
/// bytes have arrived or the deadline passes.
fn pump_notifications(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
    want: usize,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut received = Vec::new();
    while received.len() < want {
        assert!(
            Instant::now() < deadline,
            "no port notification delivered {want} bytes (got {})",
            received.len()
        );
        let events = poll_events(poller, Duration::from_millis(100));
        registry.schedule_events(&events);
        let outcome = registry
            .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("port tasks run");
        for notification in outcome.notifications {
            if let PortNotification::Data { port, bytes } = notification {
                if port == id {
                    received.extend(bytes);
                }
            }
        }
    }
    received
}

/// Polls until `done` holds or the deadline passes, returning whether it held.
fn pump_until(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    mut done: impl FnMut(&PortRegistry, &Poller) -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if done(registry, poller) {
            return true;
        }
        let events = poll_events(poller, Duration::from_millis(100));
        registry
            .handle_events(poller, &events)
            .expect("events dispatch");
    }
    done(registry, poller)
}

/// Puts `fd` in non-blocking mode, so a bounded read loop over it cannot hang the test.
fn set_nonblocking(fd: std::os::unix::io::RawFd) {
    // SAFETY: `F_GETFL` reads the status flags of the descriptor the caller owns and takes no
    // pointer argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0, "F_GETFL succeeds");
    // SAFETY: the same owned descriptor; adding `O_NONBLOCK` to the current status flags is a
    // valid `F_SETFL` update that neither closes nor replaces the fd.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert!(rc >= 0, "F_SETFL succeeds");
}
#[test]
fn echo_stdout_is_read_back_through_the_port_queue() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let driver = SpawnDriver::new(&shell("echo hello"), &SpawnOptions::default())
        .expect("the shell starts");
    // The runtime reads these before handing the driver to the registry (`port_info/1,2`).
    assert_eq!(driver.name(), "spawn");
    assert!(driver.os_pid().is_some(), "the shell form starts a child");
    assert_eq!(driver.command(), "echo hello");
    assert_eq!(driver.stream(), SpawnStream::List);
    assert!(!driver.eof());

    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");
    assert_eq!(registry.get(id).expect("live").driver_name(), "spawn");

    assert_eq!(
        pump_input(&mut poller, &mut registry, id, 6),
        b"hello\n",
        "the child's stdout arrives through the port input queue"
    );

    registry.close(&mut poller, id).expect("the port closes");
    assert!(registry.is_empty());
    assert!(poller.is_empty(), "closing deregisters the port's descriptors");
}

#[test]
fn a_child_that_floods_stderr_does_not_block_the_port() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    // The child writes far more than a pipe buffer (200000 bytes >> the default 64 KiB) to
    // **stderr** and only afterwards writes its stdout. With `stderr_to_stdout` that stream is
    // a pipe the *driver* owns (the oracle hands the child its stdout pipe as stderr as well,
    // `stderrfd = ifd[1]`, `sys_drivers.c:500`), so an undrained stderr would fill, block the
    // child on its `write(2)`, and hang it before `echo done` ever runs; the flood is then
    // delivered back as port data, so the test can require **every** byte rather than merely
    // observing that the child did not hang.
    //
    // (The default — the child's stderr inherited from this process — is covered by
    // `the_childs_stderr_is_inherited_and_is_not_port_data_by_default`. It deliberately does
    // not flood: there is no driver-side pipe to fill, and a 200 KiB flood would only be
    // written to this test runner's own stderr.)
    const FLOOD: usize = 200000;
    let driver = SpawnDriver::with_stream_options(
        &shell("head -c 200000 /dev/zero 1>&2; echo done"),
        &SpawnOptions {
            stream: SpawnStream::Binary,
            ..SpawnOptions::default()
        },
        StreamOptions {
            stderr_to_stdout: true,
            eof: false,
        },
    )
    .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    let received = pump_input(&mut poller, &mut registry, id, FLOOD + 5);
    assert!(
        received.len() >= FLOOD + 5,
        "the whole flood arrives (got {} of {})",
        received.len(),
        FLOOD + 5
    );
    assert!(
        received[..FLOOD].iter().all(|byte| *byte == 0),
        "the flooded stderr bytes are the child's own, none were dropped"
    );
    assert_eq!(
        &received[FLOOD..FLOOD + 5],
        b"done\n",
        "the child reached its stdout write after the flood: it never blocked"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn stderr_to_stdout_merges_the_childs_stderr_into_the_port_data() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    // `echo O` writes stdout, `echo E 1>&2` writes stderr. With the `stderr_to_stdout` option
    // the child's descriptor 2 is an exact duplicate of its descriptor 1, so both land in the
    // port's input stream in true write order — the oracle's own behaviour
    // (`stderrfd = opts->redir_stderr ? ifd[1] : dup(2)`, `sys_drivers.c:500`), cross-checked
    // byte-for-byte below against the real `erl`.
    let options = SpawnOptions {
        args: vec!["-c".to_string(), "echo O; echo E 1>&2".to_string()],
        ..SpawnOptions::default()
    };
    let driver = SpawnDriver::with_stream_options(
        &SpawnSpec::SpawnExecutable {
            path: "/bin/sh".to_string(),
        },
        &options,
        StreamOptions {
            stderr_to_stdout: true,
            eof: false,
        },
    )
    .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    assert_eq!(
        pump_input(&mut poller, &mut registry, id, 4),
        b"O\nE\n",
        "stdout and stderr are one merged stream, in write order"
    );
    // The merged stream ends: nothing else is delivered, and the end-of-file deregisters it.
    assert!(pump_until(&mut poller, &mut registry, |_, poller| poller
        .is_empty()));

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn the_childs_stderr_is_inherited_and_is_not_port_data_by_default() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    // Without `stderr_to_stdout` the child's stderr is the emulator's (the oracle's `dup(2)`),
    // so nothing written to it is port data. The child writes to stderr *before* its stdout, so
    // a driver that merged the two (or that delivered stderr at all) would show it here. The
    // line is inherited by this test process's stderr, so it is worded to explain itself rather
    // than look like a stray failure.
    let driver = SpawnDriver::new(
        &shell("echo '[beam_io] child stderr inherited (expected, not port data)' 1>&2; echo O"),
        &SpawnOptions::default(),
    )
    .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    assert!(
        pump_until(&mut poller, &mut registry, |_, poller| poller.is_empty()),
        "the child's end-of-file deregisters its stdout"
    );
    assert_eq!(
        registry.get_mut(id).expect("live").drain_input(),
        b"O\n",
        "only stdout is port data; the child's stderr was inherited instead"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn a_child_that_writes_more_than_one_read_chunk_is_drained_in_full() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    // One `READ_CHUNK` is 64 KiB and the reactor is edge-triggered, so a `ready_input` that
    // read once per readiness would stall at 64 KiB and never deliver the tail (a further
    // edge would only arrive if more data were written). The write path must loop to `EAGAIN`.
    const PAYLOAD: usize = 256 * 1024;
    let driver = SpawnDriver::new(
        &shell("head -c 262144 /dev/zero; echo done"),
        &SpawnOptions {
            stream: SpawnStream::Binary,
            ..SpawnOptions::default()
        },
    )
    .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    let received = pump_input(&mut poller, &mut registry, id, PAYLOAD + 5);
    assert!(
        received.len() >= PAYLOAD + 5,
        "every byte arrives (got {} of {})",
        received.len(),
        PAYLOAD + 5
    );
    assert_eq!(
        &received[PAYLOAD..PAYLOAD + 5],
        b"done\n",
        "the tail of a multi-chunk write is delivered, not stranded"
    );
    assert!(
        received[..PAYLOAD].iter().all(|byte| *byte == 0),
        "the delivered bytes are the child's own"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn a_port_that_asked_for_eof_reports_it_once() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let driver = SpawnDriver::with_stream_options(
        &shell("echo hi"),
        &SpawnOptions::default(),
        StreamOptions {
            stderr_to_stdout: false,
            eof: true,
        },
    )
    .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    // Nothing has been read yet, so there is no end-of-file to report.
    assert!(
        !registry.take_eof(id).expect("the check succeeds"),
        "an end-of-file that has not happened yet is not reported"
    );
    assert!(
        pump_until(&mut poller, &mut registry, |_, poller| poller.is_empty()),
        "the child's end-of-file deregisters its stdout"
    );
    assert_eq!(
        registry.get_mut(id).expect("live").drain_input(),
        b"hi\n",
        "the data before the end of the stream still arrives"
    );
    assert!(
        registry.take_eof(id).expect("the check succeeds"),
        "the port's end-of-file is reported"
    );
    assert!(
        !registry.take_eof(id).expect("the check succeeds"),
        "and exactly once"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn a_port_without_the_eof_option_never_reports_one() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let driver = SpawnDriver::new(&shell("echo hi"), &SpawnOptions::default())
        .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    assert!(pump_until(&mut poller, &mut registry, |_, poller| poller
        .is_empty()));
    assert!(
        !registry.take_eof(id).expect("the check succeeds"),
        "without the `eof` option the end of the stream is not reported"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn an_unknown_port_has_no_eof() {
    let mut registry = PortRegistry::new();
    assert!(matches!(
        registry.take_eof(PortId::new(4242)),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn an_empty_name_is_rejected_as_badarg_like_the_oracle() {
    // `open_port({spawn,""},[])` and `open_port({spawn_executable,""},[])` are both
    // `error:badarg` (verified against erts-17.1) while `{spawn," "}` is accepted.
    assert!(matches!(
        SpawnDriver::new(&shell(""), &SpawnOptions::default())
            .map(|_| ())
            .err(),
        Some(BeamError::BadArg)
    ));
    assert!(matches!(
        SpawnDriver::new(
            &SpawnSpec::SpawnExecutable {
                path: String::new()
            },
            &SpawnOptions::default(),
        )
        .map(|_| ())
        .err(),
        Some(BeamError::BadArg)
    ));
}
#[test]
fn the_argv_form_runs_the_program_directly() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let driver = SpawnDriver::new(&argv("echo", &["hi"]), &SpawnOptions::default())
        .expect("echo starts");
    assert_eq!(driver.command(), "echo hi");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    assert_eq!(pump_input(&mut poller, &mut registry, id, 3), b"hi\n");
    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn spawn_executable_runs_the_path_with_the_option_args() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let options = SpawnOptions {
        args: vec!["-c".to_string(), "echo exec-ok".to_string()],
        ..SpawnOptions::default()
    };
    let driver = SpawnDriver::new(
        &SpawnSpec::SpawnExecutable {
            path: "/bin/sh".to_string(),
        },
        &options,
    )
    .expect("the shell starts");
    assert_eq!(driver.command(), "/bin/sh");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    assert_eq!(
        pump_input(&mut poller, &mut registry, id, 8),
        b"exec-ok\n"
    );
    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn cat_echoes_the_bytes_written_through_the_port_output_queue() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let driver = SpawnDriver::new(
        &argv("cat", &[]),
        &SpawnOptions {
            stream: SpawnStream::Binary,
            ..SpawnOptions::default()
        },
    )
    .expect("cat starts");
    assert_eq!(driver.stream(), SpawnStream::Binary);
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    // `port_command` queues the bytes and schedules the OUTPUT task that writes them to the
    // child's stdin; the echo comes back on the child's stdout.
    registry
        .port_command(&mut poller, id, b"hello\n", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    assert_eq!(
        pump_input(&mut poller, &mut registry, id, 6),
        b"hello\n",
        "cat echoes what the port wrote to its stdin"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn a_port_round_reports_the_child_output_as_a_data_notification() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let driver = SpawnDriver::new(&shell("echo notify"), &SpawnOptions::default())
        .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    assert_eq!(
        pump_notifications(&mut poller, &mut registry, id, 7),
        b"notify\n",
        "the notification seam carries the same bytes as the input queue"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn the_childs_eof_deregisters_its_stdout() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let driver =
        SpawnDriver::new(&shell("echo hi"), &SpawnOptions::default()).expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");
    assert_eq!(poller.len(), 1, "only the child's stdout is registered");

    assert_eq!(pump_input(&mut poller, &mut registry, id, 3), b"hi\n");
    assert!(
        pump_until(&mut poller, &mut registry, |_, poller| poller.is_empty()),
        "EOF deregisters a descriptor that can never become ready again"
    );
    assert!(registry
        .get_mut(id)
        .expect("live")
        .drain_input()
        .is_empty());

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn a_stalled_write_stays_queued_and_arms_write_readiness() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    // `sleep` never reads its stdin, so the pipe buffer fills and the write stalls.
    let driver = SpawnDriver::new(&shell("sleep 30"), &SpawnOptions::default())
        .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");
    assert_eq!(poller.len(), 1, "no write interest before anything is queued");

    let payload = vec![b'x'; 256 * 1024];
    let written = registry
        .output(&mut poller, id, &payload)
        .expect("a full pipe stalls, it does not fail");
    assert!(
        written < payload.len(),
        "the kernel buffer filled ({written} of {})",
        payload.len()
    );
    assert!(
        registry.get(id).expect("live").output_len() > 0,
        "an unwritten remainder stays queued instead of being dropped"
    );
    assert_eq!(
        poller.len(),
        2,
        "the stall arms write readiness on the child's stdin"
    );
    assert_eq!(
        registry
            .output(&mut poller, id, b"more")
            .expect("a still-full pipe is not a failure"),
        0,
        "the pipe stays full, so the queue keeps growing instead of writing"
    );
    assert!(
        registry.get(id).expect("live").output_len() > 0,
        "nothing was dropped by the failed write"
    );

    registry.close(&mut poller, id).expect("the port closes");
    assert!(poller.is_empty(), "the descriptors are deregistered");
}

#[test]
fn the_exit_status_of_a_reaped_child_is_taken_once() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let options = SpawnOptions {
        exit_status: true,
        ..SpawnOptions::default()
    };
    let driver = SpawnDriver::new(&shell("exit 7"), &options).expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    let mut status = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while status.is_none() && Instant::now() < deadline {
        let events = poll_events(&mut poller, Duration::from_millis(50));
        registry
            .handle_events(&mut poller, &events)
            .expect("events dispatch");
        status = registry.take_exit_status(id).expect("the wait succeeds");
    }
    assert_eq!(status, Some(7), "the reaped exit status is reported");
    assert_eq!(
        registry.take_exit_status(id).expect("the wait succeeds"),
        None,
        "the status is handed out exactly once"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn a_port_without_the_exit_status_option_never_reports_one() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let driver = SpawnDriver::new(&shell("true"), &SpawnOptions::default())
        .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    // `true` exits immediately; the status is only withheld because the port did not ask.
    assert!(pump_until(&mut poller, &mut registry, |_, poller| poller
        .is_empty()));
    assert_eq!(
        registry.take_exit_status(id).expect("the wait succeeds"),
        None
    );

    registry.close(&mut poller, id).expect("the port closes");
}

#[test]
fn an_unknown_port_has_no_exit_status() {
    let mut registry = PortRegistry::new();
    assert!(matches!(
        registry.take_exit_status(PortId::new(4242)),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn stop_kills_and_reaps_a_long_lived_child() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let mut driver = SpawnDriver::new(&shell("sleep 30"), &SpawnOptions::default())
        .expect("the shell starts");
    let pid = driver.os_pid().expect("the shell form starts a child");
    // The driver's own liveness probe reaps a dead child and answers `Some`, so `None` proves
    // the child is still running (a `sleep 30` cannot have finished).
    assert_eq!(
        driver.exited().expect("the liveness probe"),
        None,
        "the child is running"
    );
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    // `/proc/<pid>` exists while the process is alive *and* while it is an unreaped zombie,
    // so its disappearance is evidence of both the kill and the reap. The entry is not always
    // visible in the instant right after `fork` — a task can miss a `stat` for a moment while
    // procfs registers it — so wait a bounded moment for the baseline to appear instead of
    // reading the entry once and calling a transient miss "not running".
    let procfs = Path::new("/proc").exists();
    if procfs {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "the child is visible as a live process"
        );
    }

    registry
        .close(&mut poller, id)
        .expect("closing kills and reaps the child");
    assert!(poller.is_empty(), "the descriptors are deregistered");

    if procfs {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "the child is killed and reaped, not left running or as a zombie"
        );
    }
}

#[test]
fn an_adopted_fd_is_read_without_starting_a_child() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let mut fds = [0 as std::os::unix::io::RawFd; 2];
    // SAFETY: `fds` is a valid, writable 2-element array; on success `pipe(2)` fills both
    // slots with fresh descriptors owned by this test.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe(2) succeeded");

    let driver = SpawnDriver::new(&SpawnSpec::Fd { fd: fds[0] }, &SpawnOptions::default())
        .expect("the descriptor is adopted");
    assert_eq!(driver.os_pid(), None, "the fd form starts no child");
    assert_eq!(driver.command(), format!("fd:{}", fds[0]));

    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");
    let payload = b"adopted";
    // SAFETY: `fds[1]` is the write end of the pipe created above, and `payload` is a valid
    // slice of that many initialized bytes.
    let written = unsafe { libc::write(fds[1], payload.as_ptr().cast(), payload.len()) };
    assert_eq!(written, payload.len() as isize);

    assert_eq!(
        pump_input(&mut poller, &mut registry, id, payload.len()),
        payload
    );

    registry.close(&mut poller, id).expect("the port closes");
    assert!(poller.is_empty(), "the adopted descriptor is deregistered");
    // The descriptor belongs to the Erlang side: the driver never closed it, so the test
    // closes both ends.
    // SAFETY: both descriptors were created by `pipe(2)` above, are still open and are closed
    // exactly once here.
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
}

#[test]
fn an_adopted_fd_writes_through_the_port_output_path() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let mut fds = [0 as std::os::unix::io::RawFd; 2];
    // SAFETY: `fds` is a valid, writable 2-element array; on success `pipe(2)` fills both
    // slots with fresh descriptors owned by this test.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe(2) succeeded");
    // The peer end is read with a bounded loop below, so it must not block the test.
    set_nonblocking(fds[0]);

    // Adopt the **write** end: the single adopted descriptor is bidirectional, so the port
    // writes `port_command` bytes *into* it (`an_adopted_fd_is_read_without_starting_a_child`
    // adopts the read end of the very same construction).
    let driver = SpawnDriver::new(&SpawnSpec::Fd { fd: fds[1] }, &SpawnOptions::default())
        .expect("the descriptor is adopted");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    let payload = b"from-the-port";
    // `port_command` only queues the bytes and schedules the OUTPUT task; running the port
    // tasks is what actually writes them into the adopted descriptor.
    registry
        .port_command(&mut poller, id, payload, PortCommandOptions::EMPTY)
        .expect("the command is queued");
    let outcome = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the write round");
    assert!(
        outcome.ran >= 1,
        "the OUTPUT task the command scheduled ran"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        0,
        "every queued byte was written, none was dropped"
    );

    // The peer end of the pipe now holds exactly those bytes.
    let mut buf = [0u8; 64];
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.len() < payload.len() && Instant::now() < deadline {
        // SAFETY: `fds[0]` is the read end of the pipe created above and `buf` is a valid
        // writable array of `buf.len()` bytes.
        let n = unsafe { libc::read(fds[0], buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            got.extend_from_slice(&buf[..n as usize]);
            continue;
        }
        assert_ne!(
            n, 0,
            "the peer pipe hit end-of-file before {payload:?} arrived (got {got:?})"
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock,
            "an empty peer pipe is the only reason the read makes no progress"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(got, payload, "the peer reads what the port wrote");

    // Closing the port deregisters the adopted descriptor but never closes it.
    registry.close(&mut poller, id).expect("the port closes");
    assert!(poller.is_empty(), "the adopted descriptor is deregistered");
    // SAFETY: both descriptors were created by `pipe(2)` above, are still open and are closed
    // exactly once here.
    unsafe {
        libc::close(fds[0]);
        libc::close(fds[1]);
    }
}

#[test]
fn an_adopted_socket_is_bidirectional_on_one_descriptor() {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let mut poller = poller();
    let mut registry = PortRegistry::new();

    // One socketpair end is a single descriptor that is both readable and writable: the
    // clearest evidence that a `{fd, N}` port serves both directions through one fd.
    let (local, mut peer) = UnixStream::pair().expect("a socketpair");
    peer.set_nonblocking(true)
        .expect("the peer is non-blocking for the bounded reads below");
    let fd = local.as_raw_fd();
    let driver = SpawnDriver::new(&SpawnSpec::Fd { fd }, &SpawnOptions::default())
        .expect("the descriptor is adopted");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    // Write direction: the port's `port_command` bytes reach the peer.
    registry
        .port_command(&mut poller, id, b"out", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the write round");
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.len() < 3 && Instant::now() < deadline {
        let mut buf = [0u8; 8];
        match peer.read(&mut buf) {
            Ok(0) => panic!("the peer hit end-of-file before 3 bytes arrived"),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => panic!("the peer read fails: {err:?}"),
        }
    }
    assert_eq!(got, b"out", "the peer reads what the port wrote");

    // Read direction: the peer's bytes reach the port input queue through the same fd.
    peer.write_all(b"in").expect("the peer writes");
    assert_eq!(
        pump_input(&mut poller, &mut registry, id, 2),
        b"in",
        "the very same descriptor also delivers input to the port"
    );

    registry.close(&mut poller, id).expect("the port closes");
    assert!(poller.is_empty(), "the adopted descriptor is deregistered");
    // The adopted descriptor is the Erlang side's: the driver never closed it, so `local`
    // remains a live socket until the test drops it (a closed fd would fail `EPERM`/`EBADF`).
    local
        .set_nonblocking(true)
        .expect("the adopted descriptor is still open after port_close/1");
}
#[test]
fn a_failed_spawn_is_an_errno_atom_constructor_error_that_registers_nothing() {
    let poller = poller();
    let result = SpawnDriver::new(
        &SpawnSpec::SpawnExecutable {
            path: "/nonexistent/xyz-not-real".to_string(),
        },
        &SpawnOptions::default(),
    );
    // The oracle raises `error:enoent` for this path, so the constructor error carries that
    // atom rather than a generic I/O failure (`test a_failed_spawn_matches_the_oracle_errno_atom`).
    let error = result
        .map(|_| ())
        .expect_err("a program that cannot be started is a constructor error");
    assert_eq!(error.as_atom(), Some("enoent"));
    assert!(
        poller.is_empty(),
        "a failed spawn leaves nothing registered with the poller"
    );
}

#[test]
fn cd_and_env_are_honored_by_the_child() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let options = SpawnOptions {
        cd: Some("/".to_string()),
        env: vec![("SPAWN_TEST_VAR".to_string(), "spawn-ok".to_string())],
        ..SpawnOptions::default()
    };
    let driver = SpawnDriver::new(&shell("echo $SPAWN_TEST_VAR"), &options)
        .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    assert_eq!(
        pump_input(&mut poller, &mut registry, id, 9),
        b"spawn-ok\n",
        "the env option reaches the child"
    );
    registry.close(&mut poller, id).expect("the port closes");
}

// ---------------------------------------------------------------------------
// Oracle cross-check: the failed-spawn reason atom
// ---------------------------------------------------------------------------

/// Wall-clock budget for a single `erl` invocation before the oracle is declared unusable
/// and the check skips (mirrors `beam_term/tests/oracle.rs`).
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// The outcome of the oracle's `open_port({spawn_executable, Path}, [])`.
enum OracleOutcome {
    /// `erl` could not be run (absent from `PATH`, or silent past [`ERL_TIMEOUT`]).
    Unavailable,
    /// The oracle opened the port (it left the failed `exec` to the forked child).
    Opened,
    /// The oracle raised `error:Reason`.
    Raised(String),
}

/// Runs `<expr>, halt(0).` under the system `erl` with `BEAM_IO_ORACLE_PATH=path` in the
/// environment, returning its stdout.
///
/// Returns `None` — after a clear skip message — when `erl` cannot be spawned or does not
/// answer within [`ERL_TIMEOUT`], so `cargo test -p beam_io` stays green on a host without a
/// system Erlang. A *spawned* `erl` that exits non-zero is a bug in the embedded expression,
/// so that is asserted rather than skipped.
fn erl_stdout(expr: &str, path: &str) -> Option<String> {
    erl_run(expr, Some(path))
}

/// [`erl_stdout`] for an expression that needs no environment of its own.
fn erl_stdout_plain(expr: &str) -> Option<String> {
    erl_run(expr, None)
}

/// The shared body of the two helpers: runs a `-eval` snippet under the system `erl`, with
/// `BEAM_IO_ORACLE_PATH` set when `path` is given.
fn erl_run(expr: &str, path: Option<&str>) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = std::sync::mpsc::channel();
    let path = path.map(str::to_string);
    std::thread::spawn(move || {
        let mut command = std::process::Command::new("erl");
        command.arg("-noshell").arg("-eval").arg(&code);
        if let Some(path) = &path {
            command.env("BEAM_IO_ORACLE_PATH", path);
        }
        let out = command.output();
        // The receiver may have timed out and gone away; that is fine.
        let _ = tx.send(out);
    });
    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
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

/// Drives the real `erlang:open_port/2` on `path` and classifies the outcome.
///
/// The path travels through the environment (`BEAM_IO_ORACLE_PATH`), so it never has to be
/// escaped into the `-eval` source.
fn oracle_outcome(path: &str) -> OracleOutcome {
    let expr = r#"P = case os:getenv("BEAM_IO_ORACLE_PATH") of false -> ""; V -> V end, try erlang:open_port({spawn_executable, P}, []) of _Port -> io:format("ok~n") catch error:Reason -> io:format("~p~n", [Reason]) end"#;
    match erl_stdout(expr, path) {
        None => OracleOutcome::Unavailable,
        Some(out) => {
            let line = out.trim();
            if line == "ok" {
                OracleOutcome::Opened
            } else {
                OracleOutcome::Raised(line.to_string())
            }
        }
    }
}

/// The atom this crate's driver reports for a failed direct exec of `path`, or `None` when
/// the driver started it.
fn driver_spawn_reason(path: &str) -> Option<String> {
    SpawnDriver::new(
        &SpawnSpec::SpawnExecutable {
            path: path.to_string(),
        },
        &SpawnOptions::default(),
    )
    .map(|_| ())
    .err()
    .and_then(|error| error.as_atom().map(str::to_string))
}

#[test]
fn a_failed_spawn_matches_the_oracle_errno_atom() {
    // The oracle answers `erl_errno_id(errno)` for a program the `exec` rejects; this crate
    // must raise the same atom. The three cases live in a private scratch directory, so the
    // outcome does not depend on the host's `/etc` — and the no-permission file carries
    // **no** execute bit at all, so `access(X_OK)` fails for root and non-root alike.
    let dir = std::env::temp_dir().join(format!("beam_io_spawn_oracle_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory is writable");

    let missing = dir.join("this-path-does-not-exist");
    let regular = dir.join("a-regular-file");
    std::fs::write(&regular, b"x").expect("the regular file");
    let no_perm = dir.join("no-execute-bit");
    std::fs::write(&no_perm, b"#!/bin/sh\nexit 0\n").expect("the script");
    std::fs::set_permissions(&no_perm, std::fs::Permissions::from_mode(0o000))
        .expect("every permission bit is stripped");
    // A path component that is a file, not a directory (`ENOTDIR`).
    let not_dir = regular.join("below");

    let cases = [
        ("missing", &missing),
        ("no-permission", &no_perm),
        ("not-a-directory", &not_dir),
    ];

    for (label, path) in cases {
        let path = path.to_string_lossy().into_owned();
        let oracle = match oracle_outcome(&path) {
            OracleOutcome::Unavailable => {
                eprintln!("skipping failed-spawn oracle check: the system `erl` is unavailable");
                break;
            }
            OracleOutcome::Opened => None,
            OracleOutcome::Raised(reason) => Some(reason),
        };
        assert_eq!(
            driver_spawn_reason(&path),
            oracle,
            "the {label} reason matches the oracle for {path}"
        );
    }

    // The parent directory's write/search bits are enough to unlink the mode-0 file.
    let _ = std::fs::set_permissions(&no_perm, std::fs::Permissions::from_mode(0o600));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Oracle cross-check: a *deferred* `exec` failure
// ---------------------------------------------------------------------------

/// The oracle's observation of `open_port({spawn_executable, Path}, [binary, exit_status, eof])`.
enum OraclePort {
    /// The oracle opened the port, reporting these `{exit_status, N}` values and whether it
    /// also reported `eof`.
    Opened {
        /// Every `{exit_status, N}` value the port sent.
        statuses: Vec<i32>,
        /// Whether the port also sent `eof`.
        eof: bool,
    },
    /// The oracle raised `error:Reason` instead of opening the port.
    Raised(String),
}

/// Runs the oracle's `open_port/2` on `path` and collects what it reports, or `None` when the
/// system `erl` is unavailable.
///
/// The path travels through the environment (`BEAM_IO_ORACLE_PATH`), so it never has to be
/// escaped into the `-eval` source.
fn oracle_port(path: &str) -> Option<OraclePort> {
    let expr = r#"P = case os:getenv("BEAM_IO_ORACLE_PATH") of false -> ""; V -> V end,
                  try erlang:open_port({spawn_executable, P}, [binary, exit_status, eof]) of
                      Port ->
                          F = fun Loop(Acc) ->
                                  receive {Port, M} -> Loop([M|Acc]) after 400 -> lists:reverse(Acc) end
                              end,
                          Msgs = F([]),
                          io:format("ok ~w ~w~n",
                                    [[S || {exit_status, S} <- Msgs], lists:member(eof, Msgs)])
                  catch error:Reason -> io:format("error ~w~n", [Reason]) end"#;
    let out = erl_stdout(expr, path)?;
    let line = out.trim();
    match line.strip_prefix("ok ") {
        Some(rest) => {
            let (statuses, eof) = rest.split_once(' ').expect("the two reported terms");
            let statuses = statuses
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(|entry| entry.parse().expect("an integer exit status"))
                .collect();
            Some(OraclePort::Opened {
                statuses,
                eof: eof.trim() == "true",
            })
        }
        None => Some(OraclePort::Raised(
            line.strip_prefix("error ").unwrap_or(line).to_string(),
        )),
    }
}

#[test]
fn a_deferred_exec_failure_opens_the_port_and_reports_the_errno_as_the_exit_status() {
    // The reference checks `access(Path, X_OK)` in the **emulator** (`spawn_start`,
    // `sys_drivers.c:521`) and only then hands the name to its forked child, which `execve`s it
    // and, on failure, calls `_exit(errno)` (`erl_child_setup.c:361-369`). So a path the check
    // *accepts* but the `exec` rejects is not an `open_port/2` error: the port opens and the
    // child's exit status is the failing errno. Two such paths:
    //
    // * a **searchable directory** — `execve` fails `EACCES` (13);
    // * a file carrying its execute bit with no recognizable executable **format** —
    //   `execve` fails `ENOEXEC` (8).
    //
    // Both are exactly the cases the standard library's `Command::spawn` reports synchronously,
    // so this driver forks its own child for them. The expectation is taken live from the
    // oracle, so the test pins the *reason* the port opens rather than a hard-coded status.
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("beam_io_spawn_deferred_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory is writable");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
        .expect("the directory is searchable");
    let no_format = dir.join("no-executable-format");
    std::fs::write(&no_format, b"plain text, not an executable format\n").expect("the file");
    std::fs::set_permissions(&no_format, std::fs::Permissions::from_mode(0o755))
        .expect("the execute bit is set");

    let cases = [
        ("a searchable directory", dir.to_string_lossy().into_owned()),
        (
            "a file with no executable format",
            no_format.to_string_lossy().into_owned(),
        ),
    ];

    for (label, path) in cases {
        let (expected_status, expected_eof) = match oracle_port(&path) {
            Some(OraclePort::Opened { statuses, eof }) => {
                assert_eq!(
                    statuses.len(),
                    1,
                    "the oracle reports one exit status for {path}"
                );
                assert!(eof, "the oracle reports end of file for {path}");
                (statuses[0], eof)
            }
            Some(OraclePort::Raised(reason)) => panic!(
                "the oracle must open the port for {label} ({path}), but raised error:{reason}"
            ),
            None => {
                eprintln!("skipping deferred-exec oracle check: the system `erl` is unavailable");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let driver = SpawnDriver::with_stream_options(
            &SpawnSpec::SpawnExecutable { path: path.clone() },
            &SpawnOptions {
                exit_status: true,
                ..SpawnOptions::default()
            },
            StreamOptions {
                stderr_to_stdout: false,
                eof: true,
            },
        )
        .unwrap_or_else(|error| panic!("{label} ({path}) must open a port, not raise: {error:?}"));
        assert!(
            driver.os_pid().is_some(),
            "{label}: a real child process backs the port"
        );
        let id = registry
            .open(&mut poller, Box::new(driver))
            .expect("the port opens");
        assert_eq!(registry.get(id).expect("live").driver_name(), "spawn");

        // Take the exit status as the reactor reports the child's end of stream.
        let mut status = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while status.is_none() && Instant::now() < deadline {
            let events = poll_events(&mut poller, Duration::from_millis(50));
            registry
                .handle_events(&mut poller, &events)
                .expect("events dispatch");
            status = registry.take_exit_status(id).expect("the wait succeeds");
        }
        assert_eq!(
            status,
            Some(expected_status),
            "{label}: the failing errno is the child's exit status"
        );
        assert_eq!(
            registry.take_exit_status(id).expect("the wait succeeds"),
            None,
            "{label}: the status is reported exactly once"
        );

        // The child failed before it could write, so its stdout is an empty stream that reaches
        // end of file and is deregistered.
        assert!(
            pump_until(&mut poller, &mut registry, |_, poller| poller.is_empty()),
            "{label}: the child's empty stdout deregisters at end of file"
        );
        assert_eq!(
            registry.get_mut(id).expect("live").drain_input(),
            b"",
            "{label}: the child wrote nothing"
        );
        assert_eq!(
            registry.take_eof(id).expect("the check succeeds"),
            expected_eof,
            "{label}: the end of the stream is reported"
        );
        assert!(
            !registry.take_eof(id).expect("the check succeeds"),
            "{label}: and exactly once"
        );

        registry.close(&mut poller, id).expect("the port closes");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Oracle cross-check: the stderr stream option
// ---------------------------------------------------------------------------

/// The hex-encoded bytes the *oracle* delivers as port data for the given shell command, with
/// `stderr_to_stdout` either on or off, or `None` when `erl` is unavailable.
///
/// The oracle opens `{spawn_executable, "/bin/sh"}` with `{args, ["-c", Command]}` and
/// concatenates every `{Port, {data, Bin}}` chunk it receives before the port's stream ends
/// (the command and its flags are literals of this test, so nothing has to be escaped into the
/// `-eval` source).
fn oracle_port_data_hex(command: &str, stderr_to_stdout: bool) -> Option<String> {
    let extra = if stderr_to_stdout {
        "stderr_to_stdout, "
    } else {
        ""
    };
    let expr = format!(
        r#"try erlang:open_port({{spawn_executable, "/bin/sh"}},
                              [binary, {extra}{{args, ["-c", "{command}"]}}]) of
             P -> F = fun Loop(Acc) ->
                         receive {{P, {{data, B}}}} -> Loop(<<Acc/binary, B/binary>>)
                         after 300 -> Acc end end,
                  io:format("~s~n", [binary:encode_hex(F(<<>>), lowercase)])
           catch error:R -> io:format("error:~p~n", [R]) end"#
    );
    let out = erl_stdout_plain(&expr)?;
    Some(out.trim().to_string())
}

/// The hex encoding of `bytes`, the form the oracle cross-check compares in.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn the_stderr_stream_option_matches_the_oracle() {
    // The same child, run with and without `stderr_to_stdout`: the merged stream must contain
    // the stderr line and the default stream must not. Both expectations are taken live from
    // the oracle, so the test pins the *reason* the option exists rather than a hard-coded
    // value. Without the option the child's stderr is inherited by this test process, so that
    // one line is worded to explain itself rather than look like a stray failure.
    const COMMAND: &str = "echo O; echo '[spawn] stderr inherited (expected, not port data)' 1>&2";

    let options = SpawnOptions {
        args: vec!["-c".to_string(), COMMAND.to_string()],
        ..SpawnOptions::default()
    };
    let spec = SpawnSpec::SpawnExecutable {
        path: "/bin/sh".to_string(),
    };

    for stderr_to_stdout in [true, false] {
        let oracle = match oracle_port_data_hex(COMMAND, stderr_to_stdout) {
            Some(hex) => hex,
            None => {
                eprintln!("skipping stderr oracle check: the system `erl` is unavailable");
                return;
            }
        };
        assert!(!oracle.starts_with("error:"), "the oracle opened a port");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let driver = SpawnDriver::with_stream_options(
            &spec,
            &options,
            StreamOptions {
                stderr_to_stdout,
                eof: false,
            },
        )
        .expect("the shell starts");
        let id = registry
            .open(&mut poller, Box::new(driver))
            .expect("the port opens");

        // Collect everything up to (and including) the end of the stream, so a driver that
        // delivered *extra* bytes could not slip past the comparison.
        assert!(
            pump_until(&mut poller, &mut registry, |_, poller| poller.is_empty()),
            "the child's end-of-file deregisters its stdout"
        );
        let data = registry.get_mut(id).expect("live").drain_input();
        assert_eq!(
            hex(&data),
            oracle,
            "the port data matches the oracle with stderr_to_stdout={stderr_to_stdout}"
        );

        registry.close(&mut poller, id).expect("the port closes");
    }
}

// ---------------------------------------------------------------------------
// Oracle cross-check: `{line, N}` line framing (`open_port(…, [line])`)
// ---------------------------------------------------------------------------
//
// The C splits lines in the **port**, not in the driver: `open_port`'s bare `line` option
// sets `ERTS_PORT_SFLG_LINEBUF_IO` with a 512-byte line buffer (`erl_bif_port.c:986`) and
// `{line, N}` sets it to `N` (`:862-869`), after which the port wraps every frame it splits
// out of a read as `{eol | noeol, Data}` inside `{Port, {data, …}}`
// (`deliver_linebuf_message`, `io.c:3454`). The expectations below are the oracle's own
// answers, taken live from the system `erl` and skipped loudly when it is absent.

/// One `{Port, {data, …}}` message, normalized: `None` is a plain chunk (`[binary]`), and
/// `Some(eol)` a `{eol | noeol, Data}` line frame.
type DataMessage = (Option<bool>, Vec<u8>);

/// Renders messages in the canonical textual form both sides of the oracle comparison agree
/// on: one `eol:<hex>` / `noeol:<hex>` / `raw:<hex>` token per message, in arrival order.
fn render_messages(messages: &[DataMessage]) -> String {
    let mut out = String::new();
    for (marker, bytes) in messages {
        out.push_str(match marker {
            Some(true) => "eol:",
            Some(false) => "noeol:",
            None => "raw:",
        });
        out.push_str(&hex(bytes));
        out.push(' ');
    }
    out.trim_end().to_string()
}

/// Runs one port-task round and returns the `{data, …}` messages it reported for `id`.
///
/// The two channels are merged here: raw bytes ride [`PortNotification::Data`] (the round's
/// notifications), line frames ride the separate line channel
/// ([`PortRegistry::take_line_notifications`]). A port is in exactly one mode, so a given port
/// only ever produces one of the two.
fn message_round(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
) -> Vec<DataMessage> {
    let outcome = registry
        .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("port tasks run");
    let mut messages: Vec<DataMessage> = outcome
        .notifications
        .into_iter()
        .filter_map(|notification| match notification {
            PortNotification::Data { port, bytes } if port == id => Some((None, bytes)),
            _ => None,
        })
        .collect();
    messages.extend(
        registry
            .take_line_notifications()
            .into_iter()
            .filter(|frame| frame.port == id)
            .map(|frame| (Some(frame.eol), frame.bytes)),
    );
    messages
}

/// The `{data, …}` messages a real `/bin/sh -c <command>` port delivers, driven through the
/// reactor exactly as the runtime will.
///
/// `line_mode` selects the framing the runtime applies through
/// [`PortRegistry::set_line_mode`]: `None` leaves it off (the `[binary]` case), `Some(0)` turns
/// it on with no length limit (bare `[line]`) and `Some(n)` with an `n`-byte limit
/// (`[binary, {line, n}]`).
///
/// The port is opened with the `eof` stream option so the child's end of file is observable
/// through [`PortRegistry::take_eof`] — this crate's `driver_eof` point, and therefore where
/// the partial-line flush runs (`io.c:7211`). No oracle case needs the `eof` *message*, so the
/// reported flag itself is not part of the comparison.
fn port_messages(command: &str, line_mode: Option<usize>) -> Vec<DataMessage> {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let spec = SpawnSpec::SpawnExecutable {
        path: "/bin/sh".to_string(),
    };
    let options = SpawnOptions {
        args: vec!["-c".to_string(), command.to_string()],
        ..SpawnOptions::default()
    };
    let driver = SpawnDriver::with_stream_options(
        &spec,
        &options,
        StreamOptions {
            stderr_to_stdout: false,
            eof: true,
        },
    )
    .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");
    if let Some(max_len) = line_mode {
        registry
            .set_line_mode(id, Some(max_len))
            .expect("the port is live");
    }
    let mut messages = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !poller.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the child's stdout was never exhausted"
        );
        let events = poll_events(&mut poller, Duration::from_millis(100));
        registry.schedule_events(&events);
        messages.extend(message_round(&mut poller, &mut registry, id));
    }
    // The child's stdout is at end of file: the `driver_eof` flush releases any pending
    // partial line here — and only here, never on `close`.
    assert!(
        registry.take_eof(id).expect("the check succeeds"),
        "an `eof` port reports the end of the stream"
    );
    messages.extend(message_round(&mut poller, &mut registry, id));
    registry.close(&mut poller, id).expect("the port closes");
    messages
}

/// The messages the *oracle* delivers for `command` opened with the literal option list
/// `options` (`"[line]"`, `"[binary,{line,4}]"`, …), in the canonical [`render_messages`]
/// form, or `None` when `erl` is unavailable.
///
/// The port is opened **without** the `eof` option — the oracle flushes a partial line at
/// end-of-file whatever the option says — so only `{Port, {data, …}}` messages are collected.
fn oracle_messages(command: &str, options: &str) -> Option<String> {
    let expr = format!(
        r#"try erlang:open_port({{spawn_executable, "/bin/sh"}},
                              {options} ++ [{{args, ["-c", "{command}"]}}]) of
             P -> F = fun Loop(Acc) ->
                         receive
                             {{P, {{data, {{Tag, B}}}}}} ->
                                 Loop([Acc, atom_to_list(Tag), ":",
                                       binary:encode_hex(iolist_to_binary(B), lowercase), " "]);
                             {{P, {{data, B}}}} ->
                                 Loop([Acc, "raw:",
                                       binary:encode_hex(iolist_to_binary(B), lowercase), " "])
                         after 300 -> iolist_to_binary(Acc) end end,
                  io:format("~s~n", [F(<<>>)])
           catch error:R -> io:format("error:~p~n", [R]) end"#
    );
    let out = erl_stdout_plain(&expr)?;
    Some(out.trim().to_string())
}

/// Writes `payload` to a fresh scratch file and returns a `cat` command over it.
///
/// Driving the child with `cat <file>` keeps the exact bytes out of both the shell quoting
/// and the oracle's `-eval` source string (a `printf '\n'` would have to survive a round of
/// Rust, Erlang and shell escaping).
fn cat_payload(dir: &Path, name: &str, payload: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, payload).expect("the payload file");
    let path = path.to_string_lossy().into_owned();
    assert!(
        !path.contains(['"', '\\']),
        "the scratch path is free of quoting: {path}"
    );
    format!("cat {path}")
}

/// The scratch directory a line-framing case writes its payload into.
fn line_scratch(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("beam_io_line_{label}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory is writable");
    dir
}

#[test]
fn line_framing_matches_the_oracle() {
    // The pinned oracle table: three newline-terminated lines, a partial line flushed at end
    // of file, an over-long line split at the `{line, N}` limit, a blank line, CRLF
    // terminators and a lone carriage return. Every row is a real child behind a real
    // `{line, N}` port, compared byte-for-byte against the system `erl`.
    let dir = line_scratch("frames");
    let cases: &[(&str, &[u8], usize, &str)] = &[
        ("three lines", b"a\nb\nc\n", 0, "[line]"),
        ("partial at eof", b"a\nb", 0, "[line]"),
        ("limit split", b"abcdefgh\n", 4, "[binary,{line,4}]"),
        ("blank line", b"\n", 0, "[line]"),
        ("crlf", b"a\r\nb\r\n", 0, "[line]"),
        ("lone cr", b"a\rb\n", 0, "[line]"),
    ];
    for (index, (label, payload, max_len, options)) in cases.iter().enumerate() {
        let command = cat_payload(&dir, &format!("case-{index}"), payload);
        let oracle = match oracle_messages(&command, options) {
            Some(messages) => messages,
            None => {
                eprintln!("skipping line-framing oracle check: the system `erl` is unavailable");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        assert!(
            !oracle.starts_with("error:"),
            "{label}: the oracle opened the port: {oracle}"
        );
        assert_eq!(
            render_messages(&port_messages(&command, Some(*max_len))),
            oracle,
            "{label}: the port's line frames match the oracle"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn line_mode_off_delivers_the_plain_bytes() {
    // The mirror row: with line mode off (the runtime never calls `set_line_mode`) the bytes
    // cross as plain `{Port, {data, Bin}}` with no `{eol | noeol, _}` wrapper.
    let dir = line_scratch("plain");
    let payload: &[u8] = b"a\nb\n";
    let command = cat_payload(&dir, "plain", payload);
    let messages = port_messages(&command, None);
    assert_eq!(
        messages.iter().map(|(marker, _)| *marker).collect::<Vec<_>>(),
        vec![None],
        "no line wrapper is applied: {messages:?}"
    );
    let bytes: Vec<u8> = messages.iter().flat_map(|(_, bytes)| bytes.clone()).collect();
    assert_eq!(bytes, payload, "every byte crosses, unframed");
    match oracle_port_data_hex(&command, false) {
        Some(oracle) => assert_eq!(
            hex(&bytes),
            oracle,
            "the plain bytes match the oracle's `[binary]` stream"
        ),
        None => eprintln!("skipping line-mode-off oracle check: the system `erl` is unavailable"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn closing_a_line_port_does_not_flush_its_partial_line() {
    // A child that writes `a\nb` and then **stays alive** keeps its stdout open, so the port
    // never sees end of file. The oracle flushes a partial line only at `driver_eof`
    // (`io.c:7211`), never on `port_close/1`: closing must therefore deliver the `{eol,"a"}`
    // frame and no `{noeol,"b"}`.
    let dir = line_scratch("close");
    let command = format!("{}; sleep 30", cat_payload(&dir, "partial", b"a\nb"));
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let spec = SpawnSpec::SpawnExecutable {
        path: "/bin/sh".to_string(),
    };
    let options = SpawnOptions {
        args: vec!["-c".to_string(), command],
        ..SpawnOptions::default()
    };
    let driver = SpawnDriver::with_stream_options(
        &spec,
        &options,
        StreamOptions {
            stderr_to_stdout: false,
            eof: true,
        },
    )
    .expect("the shell starts");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");
    registry
        .set_line_mode(id, Some(0))
        .expect("the port is live");
    // Drive until the complete line has been framed.
    let mut messages = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while messages.is_empty() {
        assert!(Instant::now() < deadline, "the complete line never arrived");
        let events = poll_events(&mut poller, Duration::from_millis(100));
        registry.schedule_events(&events);
        messages.extend(message_round(&mut poller, &mut registry, id));
    }
    assert_eq!(
        messages,
        vec![(Some(true), b"a".to_vec())],
        "the partial line is held back"
    );
    // The child still holds its stdout open, so there is no end of file to flush from.
    assert!(
        !registry.take_eof(id).expect("the check succeeds"),
        "the child still holds its stdout open"
    );
    assert!(
        message_round(&mut poller, &mut registry, id).is_empty(),
        "nothing else is due while the stream stays open"
    );
    // Closing reports the close and **never** a flushed `{noeol,"b"}`.
    registry.close(&mut poller, id).expect("the port closes");
    assert!(
        message_round(&mut poller, &mut registry, id).is_empty(),
        "a close flushes nothing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
