//! Integration tests for the OS-child [`SpawnDriver`] (`"spawn"`): real processes behind a
//! real port.
//!
//! Every case here starts a real `/bin/sh`/`echo`/`cat` child through the driver, hands it to
//! [`PortRegistry::open`] and drives the reactor exactly as the runtime will:
//! `Poller::poll` for readiness, then `PortRegistry::handle_events` (or
//! `schedule_events` + `execute_port_tasks`) and the port's queues. Every wait is bounded, so
//! a broken driver fails the test instead of hanging it.

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
        let events = poller
            .poll(Some(Duration::from_millis(100)))
            .expect("poll succeeds");
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
        let events = poller
            .poll(Some(Duration::from_millis(100)))
            .expect("poll succeeds");
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
        let events = poller
            .poll(Some(Duration::from_millis(100)))
            .expect("poll succeeds");
        registry
            .handle_events(poller, &events)
            .expect("events dispatch");
    }
    done(registry, poller)
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
        let events = poller
            .poll(Some(Duration::from_millis(50)))
            .expect("poll succeeds");
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

    let driver = SpawnDriver::new(&shell("sleep 30"), &SpawnOptions::default())
        .expect("the shell starts");
    let pid = driver.os_pid().expect("the shell form starts a child");
    let id = registry
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");

    // `/proc/<pid>` exists while the process is alive *and* while it is an unreaped zombie,
    // so its disappearance is evidence of both the kill and the reap.
    let procfs = Path::new("/proc").exists();
    if procfs {
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "the child is running"
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
fn a_failed_spawn_is_a_constructor_error_that_registers_nothing() {
    let poller = poller();
    let result = SpawnDriver::new(
        &SpawnSpec::SpawnExecutable {
            path: "/nonexistent/xyz-not-real".to_string(),
        },
        &SpawnOptions::default(),
    );
    assert!(
        matches!(result.map(|_| ()), Err(BeamError::Io(_))),
        "a program that cannot be started is an I/O failure, not a panic"
    );
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
