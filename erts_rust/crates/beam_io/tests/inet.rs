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
//!
//! The peer-close family covers the other half of the driver's contract: a connection whose
//! peer has gone must stop behaving like a live one. `a_recv_after_the_peer_closes_answers_the_c_closed_shape`
//! pins the reply sequence (`closed` once per direction, `enotconn` after that, the oracle's
//! `R,R,S,S`), `a_reactor_observed_peer_close_releases_the_registration_and_reports_closed`
//! pins the reactor side (the dead socket stops producing readiness) and
//! `the_peer_close_replies_agree_with_the_erl_oracle` cross-checks the two reply bytes against
//! a real `gen_tcp` connection in the system Erlang.
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::drivers::inet::{
    INET_LOPT_ACTIVE, INET_LOPT_LINE_DELIM, INET_LOPT_PACKET, INET_LOPT_PACKET_SIZE,
    INET_LOPT_TCP_SEND_TIMEOUT, INET_LOPT_TCP_SEND_TIMEOUT_CLOSE,
};
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
const INET_REQ_SETOPTS: u32 = 7;
const INET_REQ_GETOPTS: u32 = 8;
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

/// The C's `ctl_xerror(name)` reply: the `[INET_REP_ERROR]` tag followed by the lowercase name
/// (`inet_drv.c:2307`).
fn ctl_xerror(name: &str) -> Vec<u8> {
    let mut reply = vec![INET_REP_ERROR];
    reply.extend_from_slice(name.as_bytes());
    reply
}

/// Runs the `recv` verb until it answers something other than `ok` — i.e. until the read is
/// what observes the peer's end-of-file — and returns that reply.
fn recv_until_not_ok(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = control(ports, poller, id, "recv", b"");
        if reply != vec![INET_REP_OK] {
            return reply;
        }
        assert!(
            Instant::now() < deadline,
            "the peer's close was never observed"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Answers whether the `Vec<u8>` is the given reply, for the `assert_eq!` messages.
fn reply_name(reply: &[u8]) -> String {
    match reply.split_first() {
        Some((&INET_REP_ERROR, rest)) => String::from_utf8_lossy(rest).into_owned(),
        Some((&INET_REP_OK, _)) => "ok".to_string(),
        _ => format!("{reply:?}"),
    }
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
// The peer's close
// ---------------------------------------------------------------------------

/// **The connection follows the socket.** When the peer shuts its end down, the driver latches
/// the close: the `recv` that observes it answers the C's `[0] ++ "closed"`, every later one
/// `[0] ++ "enotconn"`, and `send` reports the close once too — the oracle's `R,R,S,S ->
/// closed, enotconn, closed, enotconn` sequence. No operation reads `0` forever or writes into
/// a socket the peer has left.
///
/// The fresh-connection happy path is asserted first, so the test also pins that a live
/// connection is unaffected by the new bookkeeping.
#[test]
fn a_recv_after_the_peer_closes_answers_the_c_closed_shape() {
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let port = listen_on_loopback(&mut poller, &mut ports, id);
    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    accept(&mut poller, &mut ports, id);

    // The happy path: the established connection still round-trips both ways.
    assert_eq!(
        control(&mut ports, &mut poller, id, "send", b"up"),
        vec![INET_REP_OK]
    );
    let mut echo = [0u8; 2];
    client.read_exact(&mut echo).expect("the peer reads");
    assert_eq!(&echo, b"up");
    client.write_all(b"ok").expect("the peer writes");
    assert_eq!(read_into_queue(&mut poller, &mut ports, id, 2), b"ok");

    // The peer goes away. The first `recv` that observes the end-of-file is the one that
    // reports `closed`; the one after that is the operational `enotconn`.
    client.shutdown(Shutdown::Both).expect("the peer shuts down");
    assert_eq!(
        recv_until_not_ok(&mut poller, &mut ports, id),
        ctl_xerror("closed")
    );
    assert_eq!(
        control(&mut ports, &mut poller, id, "recv", b""),
        ctl_xerror("enotconn")
    );

    // `send` keeps its **own** one-shot `closed` (the C's separate
    // `TCP_ADDF_DELAYED_CLOSE_SEND`), so a caller that only ever sends still learns about the
    // close — and never queues bytes into the dead socket.
    assert_eq!(
        control(&mut ports, &mut poller, id, "send", b"x"),
        ctl_xerror("closed")
    );
    assert_eq!(ports.get(id).expect("live").output_len(), 0);
    assert_eq!(
        control(&mut ports, &mut poller, id, "send", b"x"),
        ctl_xerror("enotconn")
    );
    ports.close(&mut poller, id).expect("closes the port");
}

/// The reactor's read-ready path latches the close too, and — like the built-in `FdDriver` at
/// end-of-file — releases the socket's poller registration, so a dead descriptor cannot keep
/// producing readiness (the vector for a busy loop). The next `recv` then reports the close.
#[test]
fn a_reactor_observed_peer_close_releases_the_registration_and_reports_closed() {
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let port = listen_on_loopback(&mut poller, &mut ports, id);
    let client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    accept(&mut poller, &mut ports, id);
    assert_eq!(poller.len(), 1, "the accepted socket is registered");

    // The peer goes away; the read-ready event makes `ready_input` read the end-of-file and
    // latch the close, which de-registers the dead socket.
    client.shutdown(Shutdown::Both).expect("the peer shuts down");
    let deadline = Instant::now() + WAIT;
    while !poller.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the peer's close was never observed by the reactor"
        );
        let events = match poller.poll(Some(Duration::from_millis(100))) {
            Ok(events) => events,
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => panic!("poll failed: {err:?}"),
        };
        ports
            .handle_events(&mut poller, &events)
            .expect("dispatch the ready events");
    }

    // The close was latched by the reactor, so the verbs report it without touching the socket.
    assert_eq!(
        control(&mut ports, &mut poller, id, "recv", b""),
        ctl_xerror("closed")
    );
    assert_eq!(
        control(&mut ports, &mut poller, id, "recv", b""),
        ctl_xerror("enotconn")
    );
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

/// The Erlang side of the peer-close cross-check: it opens a real `gen_tcp` pair over loopback,
/// closes the peer, reads the connection twice and prints the two reason atoms separated by
/// `|` (`closed|enotconn`).
///
/// A `gen_tcp` connection is used rather than a raw `open_port({spawn_driver, "tcp_inet"})` one
/// because the driver's `INET_REQ_ACCEPT` makes a *second* port for the accepted socket
/// (`inet_drv.c:12236`), which the Erlang side hides behind `gen_tcp` — the observable
/// `{error, closed}`/`{error, enotconn}` reasons are what this floor's `[0] ++ name` replies
/// encode.
fn oracle_peer_close_expression() -> String {
    "{ok, L} = gen_tcp:listen(0, [{ip, {127,0,0,1}}, binary, {active, false}]), \
     {ok, P} = inet:port(L), \
     {ok, C} = gen_tcp:connect({127,0,0,1}, P, [binary, {active, false}]), \
     {ok, S} = gen_tcp:accept(L), \
     gen_tcp:close(C), \
     R1 = gen_tcp:recv(S, 0), \
     R2 = gen_tcp:recv(S, 0), \
     N = fun({error, A}) -> atom_to_list(A); (ok) -> \"ok\" end, \
     io:format(\"~s|~s~n\", [N(R1), N(R2)])"
        .to_string()
}

/// The peer-close half of the compatibility gate: the reply this floor answers once the peer is
/// gone must be the reason the real system ERTS reports for the same sequence — `closed` first,
/// then `enotconn`.
#[test]
fn the_peer_close_replies_agree_with_the_erl_oracle() {
    let Some(stdout) = erl_stdout(&oracle_peer_close_expression()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(line) = stdout.lines().find(|line| line.contains('|')) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    let reasons: Vec<&str> = line.trim().split('|').collect();
    eprintln!("oracle peer-close reasons: {reasons:?}");
    assert_eq!(reasons, vec!["closed", "enotconn"], "the oracle's own sequence");

    // The Rust side runs the same sequence: peer closes, `recv` observes it.
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let port = listen_on_loopback(&mut poller, &mut ports, id);
    let client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    accept(&mut poller, &mut ports, id);
    client.shutdown(Shutdown::Both).expect("the peer shuts down");

    let first = recv_until_not_ok(&mut poller, &mut ports, id);
    let second = control(&mut ports, &mut poller, id, "recv", b"");
    eprintln!(
        "rust peer-close reasons: [{}, {}]",
        reply_name(&first),
        reply_name(&second)
    );

    // The reply **bytes** are the C's `ctl_xerror(reason)`, so they encode the oracle's own
    // reason atoms.
    assert_eq!(first, ctl_xerror(reasons[0]), "the first close reply");
    assert_eq!(second, ctl_xerror(reasons[1]), "the later close reply");
    ports.close(&mut poller, id).expect("closes the port");
}

/// The Erlang side of the socket-option cross-check: it opens a real `"tcp_inet"` port
/// (`open_port({spawn_driver, …})`, as `prim_inet.erl:103` does), `open`s/`bind`s/`listen`s it
/// and runs the same `setopts`/`getopts` sequence the Rust driver is given, printing each reply
/// as comma-separated bytes separated by `|`.
///
/// The sequence is the one the option vocabulary pins: set-then-read-back of `nodelay` (`10`),
/// `keepalive` (`1`), `recbuf` (`7`) and `sndbuf` (`6`); a four-code read-back; an unserved code
/// (`200`) for either verb; and the payload-framing edge cases (a partial `setopts` record, an
/// empty `getopts`/`setopts`).
fn oracle_options_expression() -> String {
    format!(
        "M = {CONTROL_MAGIC}, \
         P = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
         _ = F({INET_REQ_OPEN}, <<1,1,0>>), \
         _ = F({INET_REQ_BIND}, <<4,0,0>>), \
         _ = F({INET_REQ_LISTEN}, <<0,5>>), \
         R = [F({INET_REQ_SETOPTS}, <<10,0,0,0,1>>), \
              F({INET_REQ_GETOPTS}, <<10>>), \
              F({INET_REQ_SETOPTS}, <<1,0,0,0,1>>), \
              F({INET_REQ_GETOPTS}, <<1>>), \
              F({INET_REQ_SETOPTS}, <<7,0,0,16,0>>), \
              F({INET_REQ_GETOPTS}, <<7>>), \
              F({INET_REQ_SETOPTS}, <<6,0,0,16,0>>), \
              F({INET_REQ_GETOPTS}, <<6>>), \
              F({INET_REQ_GETOPTS}, <<1,10,6,7>>), \
              F({INET_REQ_GETOPTS}, <<200>>), \
              F({INET_REQ_SETOPTS}, <<200,0,0,0,1>>), \
              F({INET_REQ_SETOPTS}, <<7,0,0>>), \
              F({INET_REQ_GETOPTS}, <<>>), \
              F({INET_REQ_SETOPTS}, <<>>)], \
         S = fun(L) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- L])) end, \
         io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [S(L) || L <- R]))])"
    )
}

/// The socket-option half of the compatibility gate: the **same** `setopts`/`getopts` sequence
/// run against the real system `tcp_inet` driver must answer the same reply bytes this floor
/// answers — including the two buffer read-backs, where the kernel's own value (Linux doubles a
/// `SO_RCVBUF`/`SO_SNDBUF` request, so a 4096 request reads back as 8192) is what both sides
/// report. Nothing is hard-coded: the comparison is byte-for-byte against the oracle's dump, and
/// the doubling is asserted separately so the *measured* value is on the record.
#[test]
fn the_socket_option_replies_agree_with_the_erl_oracle() {
    let Some(stdout) = erl_stdout(&oracle_options_expression()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), 14, "fourteen replies: {oracle:?}");

    // The Rust side runs the identical sequence against this floor.
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    listen_on_loopback(&mut poller, &mut ports, id);
    let rust: Vec<Vec<u8>> = vec![
        control(&mut ports, &mut poller, id, "setopts", &[TCP_OPT_NODELAY, 0, 0, 0, 1]),
        control(&mut ports, &mut poller, id, "getopts", &[TCP_OPT_NODELAY]),
        control(&mut ports, &mut poller, id, "setopts", &[INET_OPT_KEEPALIVE, 0, 0, 0, 1]),
        control(&mut ports, &mut poller, id, "getopts", &[INET_OPT_KEEPALIVE]),
        control(&mut ports, &mut poller, id, "setopts", &[INET_OPT_RCVBUF, 0, 0, 16, 0]),
        control(&mut ports, &mut poller, id, "getopts", &[INET_OPT_RCVBUF]),
        control(&mut ports, &mut poller, id, "setopts", &[INET_OPT_SNDBUF, 0, 0, 16, 0]),
        control(&mut ports, &mut poller, id, "getopts", &[INET_OPT_SNDBUF]),
        control(
            &mut ports,
            &mut poller,
            id,
            "getopts",
            &[INET_OPT_KEEPALIVE, TCP_OPT_NODELAY, INET_OPT_SNDBUF, INET_OPT_RCVBUF],
        ),
        control(&mut ports, &mut poller, id, "getopts", &[200]),
        control(&mut ports, &mut poller, id, "setopts", &[200, 0, 0, 0, 1]),
        control(&mut ports, &mut poller, id, "setopts", &[INET_OPT_RCVBUF, 0, 0]),
        control(&mut ports, &mut poller, id, "getopts", &[]),
        control(&mut ports, &mut poller, id, "setopts", &[]),
    ];
    ports.close(&mut poller, id).expect("closes the port");

    eprintln!("oracle option replies: {oracle:?}");
    eprintln!("rust option replies:   {rust:?}");

    // Every reply must match byte for byte.
    assert_eq!(rust.len(), oracle.len());
    for (index, (rust_reply, oracle_reply)) in rust.iter().zip(oracle.iter()).enumerate() {
        assert_eq!(rust_reply, oracle_reply, "reply {index} disagrees");
    }

    // The oracle's own buffer read-backs, on the record: both are **twice** the 4096 the
    // sequence requested (the kernel's `SO_RCVBUF`/`SO_SNDBUF` doubling), which is exactly what
    // this floor reports because it answers the kernel's `getsockopt(2)` value.
    let read_back = |reply: &[u8]| u32::from_be_bytes([reply[2], reply[3], reply[4], reply[5]]);
    assert_eq!(
        read_back(&oracle[5]),
        2 * 4096,
        "the oracle doubles a SO_RCVBUF request (8192 for 4096)"
    );
    assert_eq!(
        read_back(&oracle[7]),
        2 * 4096,
        "the oracle doubles a SO_SNDBUF request (8192 for 4096)"
    );
    // The oracle's unserved-code and framing replies, spelled out.
    assert_eq!(oracle[9], ctl_xerror("einval"), "getopts of an unserved code");
    assert_eq!(oracle[10], ctl_xerror("einval"), "setopts of an unserved code");
    assert_eq!(oracle[11], vec![INET_REP_OK], "a partial setopts record is ignored");
    assert_eq!(oracle[12], vec![INET_REP_OK], "an empty getopts payload");
    assert_eq!(oracle[13], vec![INET_REP_OK], "an empty setopts payload");
}

// ---------------------------------------------------------------------------
// The driver-state option codes
// ---------------------------------------------------------------------------

/// The Erlang side of the **driver-state option** cross-check: it opens a real `"tcp_inet"` port
/// (`open_port({spawn_driver, …})`, as `prim_inet.erl:103` does), `open`s/`bind`s/`listen`s it and
/// runs one default read-back plus one `set`/read-back pair per settable **driver-state** code —
/// `{packet, …}` (`23`), `{packet_size, …}` (`32`), `{send_timeout_close, …}` (`35`) and
/// `{send_timeout, …}` (`30`) — and a `getopts` of the **set-only** `{line_delim, …}` (`40`),
/// printing each reply as comma-separated bytes separated by `|`.
///
/// `{active, …}` (`22`) is read back in the opening multi-code `getopts` (its default is
/// `active,false`, so no trailing two-byte `N` is appended); `send_timeout`/`send_timeout_close`
/// are gated on a stream socket, so they only report after the `open`.
fn oracle_driver_state_expression() -> String {
    format!(
        "M = {CONTROL_MAGIC}, \
         P = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
         _ = F({INET_REQ_OPEN}, <<1,1,0>>), \
         _ = F({INET_REQ_BIND}, <<4,0,0>>), \
         _ = F({INET_REQ_LISTEN}, <<0,5>>), \
         R = [F({INET_REQ_GETOPTS}, <<{INET_LOPT_ACTIVE},{INET_LOPT_PACKET},\
                                     {INET_LOPT_PACKET_SIZE}>>), \
              F({INET_REQ_SETOPTS}, <<{INET_LOPT_PACKET},0,0,0,2>>), \
              F({INET_REQ_GETOPTS}, <<{INET_LOPT_PACKET}>>), \
              F({INET_REQ_SETOPTS}, <<{INET_LOPT_PACKET_SIZE},0,0,4,0>>), \
              F({INET_REQ_GETOPTS}, <<{INET_LOPT_PACKET_SIZE}>>), \
              F({INET_REQ_SETOPTS}, <<{INET_LOPT_TCP_SEND_TIMEOUT_CLOSE},0,0,0,1>>), \
              F({INET_REQ_GETOPTS}, <<{INET_LOPT_TCP_SEND_TIMEOUT_CLOSE}>>), \
              F({INET_REQ_SETOPTS}, <<{INET_LOPT_TCP_SEND_TIMEOUT},0,0,19,136>>), \
              F({INET_REQ_GETOPTS}, <<{INET_LOPT_TCP_SEND_TIMEOUT}>>), \
              F({INET_REQ_GETOPTS}, <<{INET_LOPT_LINE_DELIM}>>)], \
         S = fun(L) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- L])) end, \
         io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [S(L) || L <- R]))])"
    )
}

/// The driver-state half of the compatibility gate for the `getopts`/`setopts` vocabulary: the
/// **same** sequence over the driver-state codes (`22` `23` `30` `32` `35` `40`) run against the
/// real system `tcp_inet` driver must answer the same reply bytes this floor answers — the
/// `active`/`packet`/`packet_size` defaults in request order, each set/read-back pair, and the
/// set-only `line_delim` read-back, whose unserved code replaces the **whole** reply with
/// `[0] ++ "einval"`. Nothing is hard-coded for the settable values: the comparison is
/// byte-for-byte against the oracle's dump, and the defaults and the set-only error are asserted
/// separately so the measured behaviour is on the record.
///
/// If `erl` is absent the check **skips loudly**, never fails.
#[test]
fn the_driver_state_option_replies_agree_with_the_erl_oracle() {
    let Some(stdout) = erl_stdout(&oracle_driver_state_expression()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), 10, "ten replies: {oracle:?}");

    // The Rust side runs the identical sequence against this floor.
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    listen_on_loopback(&mut poller, &mut ports, id);
    let rust: Vec<Vec<u8>> = vec![
        control(
            &mut ports,
            &mut poller,
            id,
            "getopts",
            &[INET_LOPT_ACTIVE, INET_LOPT_PACKET, INET_LOPT_PACKET_SIZE],
        ),
        control(&mut ports, &mut poller, id, "setopts", &[INET_LOPT_PACKET, 0, 0, 0, 2]),
        control(&mut ports, &mut poller, id, "getopts", &[INET_LOPT_PACKET]),
        control(
            &mut ports,
            &mut poller,
            id,
            "setopts",
            &[INET_LOPT_PACKET_SIZE, 0, 0, 4, 0],
        ),
        control(&mut ports, &mut poller, id, "getopts", &[INET_LOPT_PACKET_SIZE]),
        control(
            &mut ports,
            &mut poller,
            id,
            "setopts",
            &[INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 0, 0, 0, 1],
        ),
        control(
            &mut ports,
            &mut poller,
            id,
            "getopts",
            &[INET_LOPT_TCP_SEND_TIMEOUT_CLOSE],
        ),
        control(
            &mut ports,
            &mut poller,
            id,
            "setopts",
            &[INET_LOPT_TCP_SEND_TIMEOUT, 0, 0, 19, 136],
        ),
        control(&mut ports, &mut poller, id, "getopts", &[INET_LOPT_TCP_SEND_TIMEOUT]),
        control(&mut ports, &mut poller, id, "getopts", &[INET_LOPT_LINE_DELIM]),
    ];
    ports.close(&mut poller, id).expect("closes the port");

    eprintln!("oracle driver-state replies: {oracle:?}");
    eprintln!("rust driver-state replies:   {rust:?}");

    // Every reply must match byte for byte.
    assert_eq!(rust.len(), oracle.len(), "one reply per step");
    for (index, (rust_reply, oracle_reply)) in rust.iter().zip(oracle.iter()).enumerate() {
        assert_eq!(rust_reply, oracle_reply, "reply {index} disagrees");
    }

    // The oracle's own values, on the record: the `active`/`packet`/`packet_size` defaults are the
    // C descriptor's (`active,false`, `raw`, `0`), reported in request order.
    assert_eq!(
        oracle[0],
        vec![
            1, INET_LOPT_ACTIVE, 0, 0, 0, 0, // `{active, false}` — no trailing `N`
            INET_LOPT_PACKET, 0, 0, 0, 0, // `{packet, raw}`
            INET_LOPT_PACKET_SIZE, 0, 0, 0, 0, // `{packet_size, 0}`
        ],
        "the driver-state defaults"
    );
    // Each set/read-back pair echoes the value that was set, big-endian.
    assert_eq!(oracle[2], vec![1, INET_LOPT_PACKET, 0, 0, 0, 2], "the `packet` read-back");
    assert_eq!(
        oracle[4],
        vec![1, INET_LOPT_PACKET_SIZE, 0, 0, 4, 0],
        "the `packet_size` read-back"
    );
    assert_eq!(
        oracle[6],
        vec![1, INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 0, 0, 0, 1],
        "the `send_timeout_close` read-back"
    );
    assert_eq!(
        oracle[8],
        vec![1, INET_LOPT_TCP_SEND_TIMEOUT, 0, 0, 19, 136],
        "the `send_timeout` read-back"
    );
    // `line_delim` is set-only: a `getopts` naming it is the C's `default: RETURN_ERROR()`, which
    // replaces the whole reply rather than contributing a record.
    assert_eq!(
        oracle[9],
        ctl_xerror("einval"),
        "getopts of the set-only line_delim"
    );
}