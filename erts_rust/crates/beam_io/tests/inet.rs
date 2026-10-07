//! Integration tests for the built-in `tcp_inet` TCP socket driver
//! ([`beam_io::drivers::TcpInetDriver`]) — the Rust counterpart of the legacy C TCP
//! driver `gen_tcp`/`inet_tcp` sit on (`erts/emulator/drivers/common/inet_drv.c`).
//!
//! The suite drives **real loopback sockets** through the real reactor:
//!
//! * `a_tcp_port_round_trips_bytes_over_loopback` — a `"tcp_inet"` port created through the
//!   [`DriverRegistry`], `open`/`bind`/`listen` on `127.0.0.1:0`, a client connection
//!   `accept`ed, bytes read **peer → port input queue** through the poller (draining to
//!   `EAGAIN`, the backend is edge-triggered) and written **port → peer** through the
//!   `send` verb;
//! * `a_stalled_write_arms_write_readiness_and_an_unknown_verb_is_bad_arg` — a peer that
//!   never reads stalls the write, the remainder stays in the port output queue and write
//!   readiness is armed (a forged write-ready event then schedules an `OUTPUT` task, which
//!   only happens for a direction the driver actually armed); an unknown verb is the crate's
//!   `BeamError::BadArg`;
//! * `the_covered_control_replies_agree_with_the_erl_oracle` — the **compatibility gate** for
//!   the verbs this floor covers: the same control sequence is run against the *real* system
//!   `tcp_inet` driver through `erl` and the reply bytes are compared. The oracle's own reply
//!   encodings are the C's `ctl_reply`/`ctl_reply_ok`/`ctl_error` (`inet_drv.c:2260`-`:2311`)
//!   and its verb integers are the `INET_REQ_*`/`TCP_REQ_*` constants, so this pins the
//!   *shape* (`[1]`, `[1] ++ port_be16`, `[0] ++ errno_name`) against the implementation
//!   being replaced. If `erl` is absent the check **skips loudly**, never fails.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::prelude::*;
use beam_process::PortId;

/// Wall-clock budget for the `erl` oracle before the affected check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a helper waits for the kernel or the peer before failing the test.
const WAIT: Duration = Duration::from_secs(10);

/// The C request numbers of the verbs this floor covers (`inet_drv.c:805`-`:847`), and the
/// magic offset the Erlang side adds (`prim_inet.erl:2969`,
/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`).
const INET_REQ_OPEN: u32 = 1;
const INET_REQ_CLOSE: u32 = 2;
const INET_REQ_BIND: u32 = 6;
const INET_REQ_LISTEN: u32 = 27;
const TCP_REQ_RECV: u32 = 42;
const TCP_REQ_SHUTDOWN: u32 = 44;
const CONTROL_MAGIC: u32 = 0x03f1_a300;

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// The packed `open` argument (`[AF_INET, SOCK_STREAM, default protocol]`).
fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
}

/// The packed `bind`/`connect` argument naming `127.0.0.1:port`.
fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_LOOPBACK, hi, lo]
}

/// Opens a `"tcp_inet"` port through the **built-in registry** (the way the runtime resolves
/// `open_port({spawn_driver, "tcp_inet"}, …)`), so the registration itself is exercised.
fn tcp_port(poller: &mut Poller) -> (PortRegistry, PortId) {
    let registry = DriverRegistry::with_builtins();
    let mut ports = PortRegistry::new();
    let driver = registry
        .create("tcp_inet")
        .expect("the built-in tcp_inet driver is registered");
    let id = ports.open(poller, driver).expect("the port opens");
    (ports, id)
}

/// Runs one control verb on port `id`, panicking on an error of the Rust kind.
fn control(
    ports: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    command: &str,
    arg: &[u8],
) -> Vec<u8> {
    ports
        .get_mut(id)
        .expect("the port is live")
        .control(poller, command, arg)
        .unwrap_or_else(|err| panic!("the `{command}` verb failed: {err:?}"))
}

/// Creates the socket, binds it to an ephemeral loopback port and listens; returns the port
/// the kernel assigned (the `bind` reply's big-endian payload, as `prim_inet` decodes it).
fn listen_on_loopback(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> u16 {
    assert_eq!(control(ports, poller, id, "open", &open_arg()), vec![INET_REP_OK]);
    let bound = control(ports, poller, id, "bind", &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK);
    assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
    let port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(port, 0, "the kernel assigned a port");
    assert_eq!(control(ports, poller, id, "listen", &[0, 5]), vec![INET_REP_OK]);
    port
}

/// Accepts the connection the kernel queued for the listening port, retrying until it
/// arrives; returns the descriptor the reply carried.
fn accept(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> i32 {
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = control(ports, poller, id, "accept", b"");
        assert_eq!(reply[0], INET_REP_OK);
        if reply.len() == 5 {
            return i32::from_be_bytes([reply[1], reply[2], reply[3], reply[4]]);
        }
        assert_eq!(reply.len(), 1, "an `accept` reply is [1] or [1] ++ fd_be32");
        assert!(Instant::now() < deadline, "no connection was accepted");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Drives the reactor (retrying a poll the kernel interrupted) until port `id`'s input queue
/// holds at least `want` bytes, returning everything it read.
fn read_into_queue(
    poller: &mut Poller,
    ports: &mut PortRegistry,
    id: PortId,
    want: usize,
) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    while got.len() < want {
        assert!(Instant::now() < deadline, "no input arrived");
        let events = match poller.poll(Some(Duration::from_millis(100))) {
            Ok(events) => events,
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => panic!("poll failed: {err:?}"),
        };
        ports
            .handle_events(poller, &events)
            .expect("dispatch the ready events");
        got.extend(ports.get_mut(id).expect("live").drain_input());
    }
    got
}

#[test]
fn a_tcp_port_round_trips_bytes_over_loopback() {
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    assert_eq!(ports.get(id).expect("live").driver_name(), "tcp_inet");
    let port = listen_on_loopback(&mut poller, &mut ports, id);

    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("the client connects");
    let accepted_fd = accept(&mut poller, &mut ports, id);
    assert!(accepted_fd >= 0, "the reply carried a real descriptor");

    // Peer -> port: the reactor reads the bytes into the port's input queue.
    client.write_all(b"ping").expect("the client writes");
    assert_eq!(
        read_into_queue(&mut poller, &mut ports, id, 4),
        b"ping",
        "the connection's bytes reach the port input queue"
    );

    // Port -> peer: the `send` verb drains the socket.
    assert_eq!(
        control(&mut ports, &mut poller, id, "send", b"pong"),
        vec![INET_REP_OK]
    );
    assert_eq!(ports.get(id).expect("live").output_len(), 0);
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).expect("the client reads");
    assert_eq!(&reply, b"pong");

    // `shutdown` then `close`, and a second `close` is idempotent.
    assert_eq!(
        control(&mut ports, &mut poller, id, "shutdown", &[TCP_SHUT_WR]),
        vec![INET_REP_OK]
    );
    assert_eq!(
        control(&mut ports, &mut poller, id, "close", b""),
        vec![INET_REP_OK]
    );
    assert_eq!(
        control(&mut ports, &mut poller, id, "close", b""),
        vec![INET_REP_OK]
    );
    ports.close(&mut poller, id).expect("closes the port");
}

#[test]
fn a_stalled_write_arms_write_readiness_and_an_unknown_verb_is_bad_arg() {
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let port = listen_on_loopback(&mut poller, &mut ports, id);
    // The peer connects but never reads, so the kernel's buffers fill and stay full.
    let _client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    accept(&mut poller, &mut ports, id);

    // A small write is taken by the kernel straight away.
    assert_eq!(
        control(&mut ports, &mut poller, id, "send", b"tiny"),
        vec![INET_REP_OK]
    );
    assert_eq!(ports.get(id).expect("live").output_len(), 0);

    // Keep offering bytes until the kernel stops taking them (the poller's backend is
    // edge-triggered, so `EAGAIN` must leave the remainder queued and arm write readiness).
    let chunk = vec![b'x'; 256 * 1024];
    let deadline = Instant::now() + WAIT;
    while ports.get(id).expect("live").output_len() == 0 {
        assert!(Instant::now() < deadline, "the kernel accepted every byte");
        assert_eq!(
            control(&mut ports, &mut poller, id, "send", &chunk),
            vec![INET_REP_OK]
        );
    }

    // The evidence: a write-ready event now schedules an OUTPUT task, which only happens
    // for a direction the driver actually armed with the poller (`PortArms`).
    assert!(ports.schedule_readiness(
        id.number(),
        Readiness {
            writable: true,
            ..Readiness::default()
        }
    ));
    assert!(ports
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Output));

    // The port-task round runs that OUTPUT task without panicking on the full socket
    // buffer (it re-arms and stays queued).
    let outcome = ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert!(outcome.ran >= 1, "the OUTPUT task ran");
    assert!(ports.get(id).expect("live").output_len() > 0);

    // An unknown verb is the crate's argument fault, not a silently-accepted no-op.
    assert!(matches!(
        ports
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "frobnicate", b""),
        Err(BeamError::BadArg)
    ));
    ports.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// The oracle cross-check (skips loudly when `erl` is absent)
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout.
///
/// `None` — after printing a clear skip message — when `erl` cannot be spawned or does not
/// answer within [`ERL_TIMEOUT`]. A spawned `erl` that exits non-zero is a bug in the
/// embedded expression, so that is asserted rather than skipped.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({err}); is Erlang on PATH?");
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

/// The Erlang side of the cross-check: it opens a **real** `"tcp_inet"` port with
/// `open_port({spawn_driver, …})` (exactly as `prim_inet.erl:103` does) and runs the same
/// verb sequence the Rust driver is given, printing each reply as comma-separated bytes
/// separated by `|`.
fn oracle_expression() -> String {
    format!(
        "M = {CONTROL_MAGIC}, \
         P = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
         R = [F({INET_REQ_OPEN}, <<1,1,0>>), \
              F({INET_REQ_BIND}, <<4,0,0>>), \
              F({INET_REQ_LISTEN}, <<0,5>>), \
              F({TCP_REQ_SHUTDOWN}, <<1>>), \
              F({TCP_REQ_RECV}, <<0,0,0,0,0,0,0,0>>), \
              F({INET_REQ_CLOSE}, <<>>)], \
         S = fun(L) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- L])) end, \
         io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [S(L) || L <- R]))])"
    )
}

/// Parses the oracle's `a,b,c|d,e|…` reply dump into one byte vector per reply.
fn parse_reply_dump(stdout: &str) -> Option<Vec<Vec<u8>>> {
    let line = stdout.lines().find(|line| line.contains('|'))?;
    let mut replies = Vec::new();
    for field in line.trim().split('|') {
        let mut bytes = Vec::new();
        for part in field.split(',') {
            if part.is_empty() {
                // An empty reply is a legal answer (`close` answering nothing would be one).
                continue;
            }
            bytes.push(part.trim().parse::<u8>().ok()?);
        }
        replies.push(bytes);
    }
    Some(replies)
}

/// The compatibility gate for the covered verbs: the **same** control sequence run against
/// the real system `tcp_inet` driver must answer the same reply bytes this floor answers.
#[test]
fn the_covered_control_replies_agree_with_the_erl_oracle() {
    let Some(stdout) = erl_stdout(&oracle_expression()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), 6, "six replies: {oracle:?}");

    // The Rust side runs the identical sequence against this floor.
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let open = control(&mut ports, &mut poller, id, "open", &open_arg());
    let bind = control(&mut ports, &mut poller, id, "bind", &loopback_arg(0));
    let listen = control(&mut ports, &mut poller, id, "listen", &[0, 5]);
    let shutdown = control(&mut ports, &mut poller, id, "shutdown", &[TCP_SHUT_WR]);
    let recv = control(&mut ports, &mut poller, id, "recv", &[0u8; 8]);
    let close = control(&mut ports, &mut poller, id, "close", b"");
    ports.close(&mut poller, id).expect("closes the port");

    // The oracle's own encoding of the sequence, for the failure messages.
    eprintln!("oracle replies: {oracle:?}");
    eprintln!(
        "rust replies:   [open: {open:?}, bind: {bind:?}, listen: {listen:?}, \
         shutdown: {shutdown:?}, recv: {recv:?}, close: {close:?}]"
    );

    // `open`, `listen`, `close` are the C's `ctl_reply_ok` = `[INET_REP_OK]`.
    assert_eq!(open, oracle[0], "INET_REQ_OPEN replies ok");
    assert_eq!(open, vec![INET_REP_OK]);
    assert_eq!(listen, oracle[2], "INET_REQ_LISTEN replies ok");
    assert_eq!(listen, vec![INET_REP_OK]);
    assert_eq!(close, oracle[5], "INET_REQ_CLOSE replies ok");
    assert_eq!(close, vec![INET_REP_OK]);

    // `bind` is `ctl_reply(INET_REP_OK, port_be16)`: the shape and the byte order must agree
    // (the two runs bind different ephemeral ports, so only the shape is comparable).
    assert_eq!(bind[0], oracle[1][0], "`bind` answers the ok tag");
    assert_eq!(oracle[1].len(), 3, "the oracle replies [1, port_hi, port_lo]");
    assert_eq!(bind.len(), oracle[1].len(), "the same reply length");
    assert_ne!(
        u16::from_be_bytes([bind[1], bind[2]]),
        0,
        "the kernel-assigned port is reported big-endian"
    );

    // Not connected: the C's `ctl_error(ENOTCONN)` = `[INET_REP_ERROR] ++ "enotconn"`.
    let enotconn = {
        let mut reply = vec![INET_REP_ERROR];
        reply.extend_from_slice(b"enotconn");
        reply
    };
    assert_eq!(
        shutdown, oracle[3],
        "TCP_REQ_SHUTDOWN on an unconnected port agrees"
    );
    assert_eq!(shutdown, enotconn);
    assert_eq!(recv, oracle[4], "TCP_REQ_RECV on an unconnected port agrees");
    assert_eq!(recv, enotconn);
}
