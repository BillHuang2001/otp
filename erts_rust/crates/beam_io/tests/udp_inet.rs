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

//! Integration tests for the built-in `udp_inet` datagram socket driver
//! ([`beam_io::drivers::UdpInetDriver`]) through the **real port layer** — the Rust counterpart of
//! the legacy C driver's UDP half, the driver `gen_udp`/`inet_udp` sit on
//! (`erts/emulator/drivers/common/inet_drv.c`).
//!
//! The driver's own in-module suite (`../src/drivers/inet/udp/udp_tests.rs`) drives the driver
//! directly through a hand-built `DriverContext`. This suite goes through the seam a runtime
//! actually uses: `"udp_inet"` is resolved out of [`DriverRegistry::with_builtins`], opened as a
//! [`PortId`] through [`PortRegistry::open`], and every verb is issued over the port
//! ([`Port::control`] / [`PortRegistry::port_control`]) — so the registration, the verb dispatch,
//! the reactor round and the [`PortNotification`] channel are exercised together:
//!
//! * `a_datagram_from_a_second_socket_surfaces_through_the_reactor` — a `"udp_inet"` port is
//!   `open`ed and `bind`ed to an ephemeral loopback port, a **second real socket**
//!   (`std::net::UdpSocket`) sends it a datagram, and the reactor round (`poll` →
//!   `handle_events`) observes the socket's read readiness. The datagram then surfaces twice: as
//!   the source-carrying `recv` reply (`[1] ++ [INET_AF_INET, port_hi, port_lo, a, b, c, d]`, the
//!   eight-byte form) and, on the next port-task round, as the `PortNotification::Data` payload
//!   the runtime turns into `{Port, {data, Data}}`. The bare `[1]` (one byte) a drained socket
//!   answers pins the reply-length distinction between "a datagram was delivered" and "none was
//!   pending".
//! * `the_read_and_write_readiness_rounds_behave_for_a_datagram_socket` — the socket registers
//!   **read** readiness, a read-ready event schedules an `INPUT` task which the round runs, and a
//!   write-readiness event is handled but schedules nothing (the datagram driver arms only the
//!   read direction). The real write path is the `send` verb, which writes synchronously and
//!   leaves the output queue empty.
//! * `an_unknown_verb_is_a_bad_arg_and_the_reserved_close_op_closes_the_port` — the port layer's
//!   verb dispatch: an unknown verb is `BeamError::BadArg` (never a silently-accepted no-op), the
//!   reserved `"close"` op closes the port with an empty reply, and the close is reported
//!   **exactly once** as a `PortNotification::Closed`.
//! * `the_covered_control_replies_agree_with_the_erl_oracle` — the **compatibility gate** for the
//!   verbs this floor covers: the same control sequence is run against the *real* system
//!   `udp_inet` driver through `erl` (`open_port({spawn_driver, "udp_inet"}, [binary])` +
//!   `port_control`, as `prim_inet.erl:103` does) and the reply bytes are compared. Only the
//!   `bind` reply is shape-compared — the two runs bind *different* ephemeral ports.
//! * `the_recv_verb_is_the_documented_divergence_from_the_oracles_code_60` — the driver's one
//!   deliberate divergence (`../src/drivers/inet/udp.rs`, "The reception model"): the oracle's
//!   `PACKET_REQ_RECV` (code 60) is an **asynchronous** request whose immediate reply is the
//!   async-op id and whose result arrives later as an `{inet_async, …}` message, while this
//!   floor's `recv` reads **now** and answers the result (the source address, or the bare `[1]`).
//!   Only the oracle's *shape* is pinned; nothing is fabricated for the divergent bytes.
//!
//! Every oracle case **skips loudly** (a `SKIP:` banner and a `return`) when `erl` is not on
//! `PATH`, and never fails for a missing toolchain.
use std::net::UdpSocket;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::drivers::inet::{
    INET_AF_ANY, INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT, INET_PROTO_UDP,
    INET_REP_ERROR, INET_REP_OK, INET_TYPE_DGRAM, UDP_DRIVER_NAME,
};
use beam_io::prelude::*;
use beam_process::PortId;

/// Wall-clock budget for the `erl` oracle before the affected check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a helper waits for the kernel or the peer before failing the test.
const WAIT: Duration = Duration::from_secs(10);

/// The C request numbers of the verbs this suite drives (`inet_drv.c:805`-`:847`) — the C's
/// `INET_REQ_*` and the UDP half's `PACKET_REQ_RECV` — plus the magic offset the Erlang side adds
/// (`prim_inet.erl:2969`, `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`).
const INET_REQ_OPEN: u32 = 1;
const INET_REQ_CLOSE: u32 = 2;
const INET_REQ_BIND: u32 = 6;
const INET_REQ_SETOPTS: u32 = 7;
const INET_REQ_GETOPTS: u32 = 8;
const PACKET_REQ_RECV: u32 = 60;
const CONTROL_MAGIC: u32 = 0x03f1_a300;

/// The syscall-backed option codes the datagram store serves (`inet_drv.c:874`-`:921`):
/// `SO_REUSEADDR` (`0`), `SO_BROADCAST` (`4`) and `IP_TTL` (`46`).
///
/// They are spelled out here rather than imported because the `inet` module's option vocabulary
/// is private to the crate (only a subset is re-exported).
const INET_OPT_REUSEADDR: u8 = 0;
const INET_OPT_BROADCAST: u8 = 4;
const INET_OPT_TTL: u8 = 46;

/// The datagram **driver-state** option codes (`{mode, …}` `24`, `{deliver, …}` `25`,
/// `{udp_read_packets, …}` `33`).
const INET_LOPT_MODE: u8 = 24;
const INET_LOPT_DELIVER: u8 = 25;
const INET_LOPT_UDP_READ_PACKETS: u8 = 33;

/// An option code no datagram store serves (the C's `default:` arm ⇒ `[0] ++ "einval"`).
const INET_OPT_UNSERVED: u8 = 200;

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// The packed `open` argument: `[AF_INET, SOCK_DGRAM, default protocol]`.
fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT]
}

/// The packed `INET_AF_LOOPBACK` argument naming `127.0.0.1:port` (the C's three-byte form).
fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_LOOPBACK, hi, lo]
}

/// The packed `INET_AF_INET` argument naming `127.0.0.1:port` (the C's seven-byte form).
fn inet_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_INET, hi, lo, 127, 0, 0, 1]
}

/// Opens a `"udp_inet"` port through the **built-in registry** (the way the runtime resolves
/// `open_port({spawn_driver, "udp_inet"}, …)`), so the registration itself is exercised.
fn udp_port(poller: &mut Poller) -> (PortRegistry, PortId) {
    let registry = DriverRegistry::with_builtins();
    let mut ports = PortRegistry::new();
    let driver = registry
        .create("udp_inet")
        .expect("the built-in udp_inet driver is registered");
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

/// `open`s the datagram socket and `bind`s it to an ephemeral port on `INADDR_ANY`; returns the
/// port the kernel assigned (the `bind` reply's big-endian payload, as `prim_inet` decodes it).
fn open_and_bind_ephemeral(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> u16 {
    assert_eq!(
        control(ports, poller, id, "open", &open_arg()),
        vec![INET_REP_OK],
        "`open` answers [1]"
    );
    let bound = control(ports, poller, id, "bind", &[INET_AF_ANY, 0, 0]);
    assert_eq!(bound[0], INET_REP_OK, "`bind` succeeds");
    assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
    let port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(port, 0, "the kernel assigned a port");
    port
}

/// The `recv` verb with the empty argument this floor requires.
///
/// A datagram's source address cannot ride the byte-only port input queue, so reception is
/// **verb-driven**: `recv` reads one datagram and its reply carries the source. The reply length
/// distinguishes the outcomes — eight bytes (`[1] ++ src_address`) delivered a datagram, one byte
/// (the bare `[1]`) found none pending.
fn recv(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> Vec<u8> {
    control(ports, poller, id, "recv", b"")
}

/// Waits (bounded) for the `recv` verb to find a datagram, returning that reply.
fn recv_until_delivered(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = recv(poller, ports, id);
        if reply.len() != 1 {
            return reply;
        }
        assert_eq!(reply, vec![INET_REP_OK], "a drained socket answers the bare [1]");
        assert!(Instant::now() < deadline, "the datagram never arrived");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits (bounded) for a poller event, retrying a wait the kernel interrupted.
///
/// `mio` does not retry `EINTR` (see [`Poller::poll`]), and this suite deliberately does not
/// assume a quiet process.
fn poll_once(poller: &mut Poller) -> Vec<Event> {
    loop {
        match poller.poll(Some(Duration::from_millis(100))) {
            Ok(events) => return events,
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => panic!("poll failed: {err:?}"),
        }
    }
}

/// One reactor round for port `id`: `poll` for readiness, turn it into scheduled port tasks and
/// run them (`handle_events`), reporting whether the batch carried a read-ready event for `id`.
///
/// The readiness event for the port's key *is* the evidence that the socket is registered with
/// the poller: both drivers register the descriptor under the port number.
fn reactor_round(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> bool {
    let events = poll_once(poller);
    let read_ready = events
        .iter()
        .any(|event| event.key() == id.number() && event.ready().readable());
    ports
        .handle_events(poller, &events)
        .unwrap_or_else(|err| panic!("the ready events are dispatched: {err:?}"));
    read_ready
}

/// A plain port-task round, returning the notifications it reported — the channel the runtime
/// turns into `{Port, {data, Data}}` / port-exit messages.
fn harvest(poller: &mut Poller, ports: &mut PortRegistry) -> Vec<PortNotification> {
    ports
        .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the port-task round runs")
        .notifications
}

/// The C's `ctl_xerror(name)` reply: the `[INET_REP_ERROR]` tag followed by the lowercase name
/// (`inet_drv.c:2307`).
fn ctl_xerror(name: &str) -> Vec<u8> {
    let mut reply = vec![INET_REP_ERROR];
    reply.extend_from_slice(name.as_bytes());
    reply
}

// ---------------------------------------------------------------------------
// The port-layer round trip
// ---------------------------------------------------------------------------

/// A datagram from a **second real socket** reaches the port's owner: the reactor observes the
/// socket's read readiness, the source-carrying `recv` reply names the sender, and the payload
/// rides the `PortNotification::Data` channel.
///
/// The `udp_inet` driver's `ready_input` is a documented no-op (there is no channel for a
/// datagram's source address in the byte-only input queue / the two-variant
/// [`PortNotification`]), so the runtime issues the readiness-driven `recv` verb after the round —
/// the module's reception model. Nothing is pushed into the port here that the driver did not
/// read itself.
#[test]
fn a_datagram_from_a_second_socket_surfaces_through_the_reactor() {
    let mut poller = poller();
    let (mut ports, id) = udp_port(&mut poller);
    assert_eq!(
        ports.get(id).expect("live").driver_name(),
        UDP_DRIVER_NAME,
        "the port runs the built-in udp_inet driver"
    );
    let port = open_and_bind_ephemeral(&mut poller, &mut ports, id);

    // Nothing has been sent yet: the reply is the bare `[1]` — one byte, the length that marks
    // "no datagram was pending", as opposed to the eight-byte address-carrying form below.
    assert_eq!(
        recv(&mut poller, &mut ports, id),
        vec![INET_REP_OK],
        "a drained socket answers the bare [1]"
    );

    // A second, entirely independent socket sends the datagram.
    let sender = UdpSocket::bind("127.0.0.1:0").expect("a second real socket binds");
    let sender_port = sender.local_addr().expect("its address").port();
    sender
        .send_to(b"ping", ("127.0.0.1", port))
        .expect("the datagram is sent");

    // The reactor round: `poll` reports the registered socket's read readiness and
    // `handle_events` turns it into a scheduled-and-run `INPUT` task (`ready_input` is the
    // documented no-op), after which the runtime issues `recv`.
    let deadline = Instant::now() + WAIT;
    let mut observed_read_ready = false;
    let source = loop {
        observed_read_ready |= reactor_round(&mut poller, &mut ports, id);
        let reply = recv(&mut poller, &mut ports, id);
        if reply.len() != 1 {
            break reply;
        }
        assert_eq!(reply, vec![INET_REP_OK], "nothing pending is the bare [1]");
        assert!(Instant::now() < deadline, "the datagram never arrived");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        observed_read_ready,
        "the reactor observed the socket's read readiness"
    );

    // The reply carries the source: `[1] ++ [INET_AF_INET, port_hi, port_lo, a, b, c, d]`.
    assert_eq!(source.len(), 8, "a delivery reply carries the source: {source:?}");
    assert_eq!(source[0], INET_REP_OK, "the ok tag");
    assert_eq!(source[1], INET_AF_INET, "the packed source address tag");
    assert_eq!(
        u16::from_be_bytes([source[2], source[3]]),
        sender_port,
        "the source port is the sender's"
    );
    assert_eq!(&source[4..8], &[127, 0, 0, 1], "the sender is loopback");

    // The payload the `recv` read is now buffered in the port; the next port-task round reports
    // it as the `Data` notification the runtime forms `{Port, {data, Data}}` from.
    assert_eq!(
        harvest(&mut poller, &mut ports),
        vec![PortNotification::Data {
            port: id,
            read: ReadMessage::body_only(b"ping".to_vec()),
        }],
        "the datagram reaches the port's owner through the Data channel"
    );
    // ... and exactly once: a second round has nothing left to report.
    assert!(
        harvest(&mut poller, &mut ports).is_empty(),
        "a Data report happens once per byte"
    );

    ports.close(&mut poller, id).expect("closes the port");
}

/// The readiness round, in the datagram-appropriate form: the socket registers **read** readiness,
/// a read-ready event schedules and runs an `INPUT` task, a write-readiness event is handled but
/// schedules nothing, and the real write path (`send`) never leaves a remainder behind.
#[test]
fn the_read_and_write_readiness_rounds_behave_for_a_datagram_socket() {
    let mut poller = poller();
    let (mut ports, id) = udp_port(&mut poller);
    let port = open_and_bind_ephemeral(&mut poller, &mut ports, id);

    // A read-ready event naming the port schedules an INPUT task — which only happens for a
    // direction the driver armed with the poller (`UdpInetDriver::adopt`/`start` register
    // `Interest::READABLE`).
    assert!(
        ports.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ),
        "the readiness named a live port"
    );
    assert!(
        ports
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Input),
        "the read direction is armed, so the event scheduled an INPUT task"
    );

    // The round runs it. No datagram was sent, so the driver's no-op `ready_input` reads nothing
    // and the round reports no notification.
    let outcome = ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert!(outcome.ran >= 1, "the INPUT task ran");
    assert!(
        outcome.notifications.is_empty(),
        "nothing was read: {outcome:?}"
    );
    assert!(
        !ports
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Input),
        "the INPUT task was consumed"
    );

    // A write-readiness event is handled without panicking, but schedules nothing: the datagram
    // driver arms **only** the read direction (`write_pending` widens to `Interest::BOTH` only
    // when a *connected* socket's kernel buffer is full, which an unconnected datagram port never
    // reaches). A datagram write cannot stall the way a byte-stream write can, so no stall is
    // asserted here — the invariant the driver does provide is asserted instead.
    assert!(
        ports.schedule_readiness(
            id.number(),
            Readiness {
                writable: true,
                ..Readiness::default()
            }
        ),
        "the readiness named a live port"
    );
    assert!(
        !ports
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output),
        "a write-readiness event cannot schedule a direction the driver never armed"
    );

    // The real write path is the `send` verb: it calls `sendto(2)` and never queues, so the
    // kernel takes the datagram (or refuses it) immediately and the output queue stays empty.
    let mut arg = loopback_arg(port);
    arg.extend_from_slice(b"echo");
    assert_eq!(
        control(&mut ports, &mut poller, id, "send", &arg),
        vec![INET_REP_OK],
        "`send` answers [1]"
    );
    assert_eq!(
        ports.get(id).expect("live").output_len(),
        0,
        "a datagram send never leaves a remainder queued"
    );
    assert!(
        !ports
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output),
        "nothing was queued, so no OUTPUT task was scheduled"
    );

    // The loopback datagram is on the socket now: the read-now verb reports the port's **own**
    // address as the source, and the payload rides the Data channel.
    let source = recv_until_delivered(&mut poller, &mut ports, id);
    assert_eq!(source.len(), 8, "a delivery reply carries the source: {source:?}");
    assert_eq!(
        u16::from_be_bytes([source[2], source[3]]),
        port,
        "the source is the port's own address"
    );
    assert_eq!(&source[4..8], &[127, 0, 0, 1], "the loopback address");
    assert_eq!(
        harvest(&mut poller, &mut ports),
        vec![PortNotification::Data {
            port: id,
            read: ReadMessage::body_only(b"echo".to_vec()),
        }],
        "the echoed datagram reaches the owner"
    );

    // The C's seven-byte `INET_AF_INET` form is the other address encoding the `send` argument
    // accepts (`inet_set_address`); it routes identically.
    let mut arg = inet_arg(port);
    arg.extend_from_slice(b"full");
    assert_eq!(
        control(&mut ports, &mut poller, id, "send", &arg),
        vec![INET_REP_OK]
    );
    assert_eq!(ports.get(id).expect("live").output_len(), 0);
    let source = recv_until_delivered(&mut poller, &mut ports, id);
    assert_eq!(source.len(), 8);
    assert_eq!(
        harvest(&mut poller, &mut ports),
        vec![PortNotification::Data {
            port: id,
            read: ReadMessage::body_only(b"full".to_vec()),
        }]
    );

    ports.close(&mut poller, id).expect("closes the port");
}

/// The port layer's verb dispatch: an unknown verb is the crate's argument fault, the registry's
/// one reserved control name closes the port, and the close is reported exactly once.
#[test]
fn an_unknown_verb_is_a_bad_arg_and_the_reserved_close_op_closes_the_port() {
    let mut poller = poller();
    let (mut ports, id) = udp_port(&mut poller);
    open_and_bind_ephemeral(&mut poller, &mut ports, id);

    // An unknown verb is `BeamError::BadArg`, never a silently-accepted no-op.
    assert!(
        matches!(
            ports.port_control(&mut poller, id, "frobnicate", b""),
            Err(BeamError::BadArg)
        ),
        "an unknown verb is the crate's argument fault"
    );
    // The port survived it: the socket is still there and still answers a verb.
    assert_eq!(
        recv(&mut poller, &mut ports, id),
        vec![INET_REP_OK],
        "the refused verb did not disturb the port"
    );

    // A second `open` on the live port is refused rather than replacing the socket — the C's
    // `[0]exbadseq` is this floor's `[0]einval` (the same choice `TcpInetDriver::verb_open` makes).
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            id,
            "open",
            &[INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT]
        ),
        ctl_xerror("einval"),
        "a live port refuses a second socket"
    );
    // The explicit UDP protocol is accepted on a **fresh** port (`udp::verb_open` serves both
    // `INET_PROTO_DEFAULT` and `INET_PROTO_UDP`), so the refusal above is about the live socket,
    // not the protocol.
    let (mut other_ports, other) = udp_port(&mut poller);
    assert_eq!(
        control(
            &mut other_ports,
            &mut poller,
            other,
            "open",
            &[INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_UDP]
        ),
        vec![INET_REP_OK],
        "the explicit UDP protocol is accepted"
    );
    other_ports
        .close(&mut poller, other)
        .expect("closes the second port");

    // The registry reserves exactly one control name: `close` closes the port and answers with no
    // bytes (`PortControl::CLOSE`).
    assert_eq!(
        ports
            .port_control(&mut poller, id, PortControl::CLOSE, b"")
            .expect("the reserved close op closes the port")
            .len(),
        0,
        "the reserved close op answers with no bytes"
    );
    assert!(
        ports.get(id).is_none(),
        "the port is no longer live after the close"
    );

    // The close is reported exactly once, as a `Closed` notification.
    assert_eq!(
        harvest(&mut poller, &mut ports),
        vec![PortNotification::Closed { port: id }],
        "the close reaches the owner"
    );
    assert!(
        harvest(&mut poller, &mut ports).is_empty(),
        "a close is reported once"
    );
}

// ---------------------------------------------------------------------------
// The `erl` oracle cross-check (skips loudly when `erl` is absent)
// ---------------------------------------------------------------------------

/// Runs `expr` under the system `erl`, returning its **raw stdout bytes**, or `None` when `erl`
/// cannot be spawned or does not answer within [`ERL_TIMEOUT`] (the caller then skips loudly).
///
/// The bytes are handed over untouched: the dump is the driver's own reply bytes, and a
/// `String::from_utf8_lossy` would replace every byte above `127` with `U+FFFD` — which an
/// ephemeral port an oracle `bind` reply carries routinely are.
fn erl_stdout(expr: &str) -> Option<Vec<u8>> {
    let script = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out = Command::new("erl")
            .args(["-noshell", "-eval", &script])
            .output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(ERL_TIMEOUT) {
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle expression failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Some(out.stdout)
        }
        Ok(Err(_)) | Err(_) => None,
    }
}

/// The `erl` expression driving an oracle `udp_inet` port: open the driver port, run `calls`
/// (each `(request code, argument bytes)`) through `port_control`, and write every reply as
/// raw bytes prefixed by its big-endian length.
///
/// The replies are **length-framed** rather than `|`-separated: a reply's payload can contain any
/// byte — an ephemeral port, an errno name — so a one-byte separator would be ambiguous. The
/// length prefix cannot be.
///
/// `standard_io` is switched to `latin1` first: with the default (`unicode`) encoding,
/// `io:format("~s", …)` would UTF-8-encode any byte above `127`, so the dump would no longer be
/// the driver's raw reply bytes.
fn oracle_expression(calls: &[(u32, Vec<u8>)]) -> String {
    let mut calls_src = String::new();
    for (index, (code, arg)) in calls.iter().enumerate() {
        let arg_list = arg
            .iter()
            .map(|byte| byte.to_string())
            .collect::<Vec<_>>()
            .join(",");
        if index > 0 {
            calls_src.push_str(", ");
        }
        calls_src.push_str(&format!("F({code}, <<{arg_list}>>)"));
    }
    format!(
        "M = {CONTROL_MAGIC}, \
         P = open_port({{spawn_driver, \"udp_inet\"}}, [binary]), \
         F = fun(C, A) -> iolist_to_binary(erlang:port_control(P, C + M, A)) end, \
         R = [{calls_src}], \
         Out = iolist_to_binary([<<(byte_size(X)):32/big, X/binary>> || X <- R]), \
         io:setopts(standard_io, [{{encoding, latin1}}]), \
         io:format(\"~s\", [Out]), \
         erlang:port_close(P)"
    )
}

/// Parses the oracle's `[len_be32][bytes]…` dump into one byte vector per reply.
fn parse_reply_dump(dump: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut replies = Vec::new();
    let mut rest = dump;
    while !rest.is_empty() {
        let header = rest.get(..4)?;
        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let body = rest.get(4..)?;
        if body.len() < len {
            return None;
        }
        replies.push(body[..len].to_vec());
        rest = &body[len..];
    }
    Some(replies)
}

/// The **compatibility gate** for the datagram control verbs: the same `open`/`bind`/`setopts`/
/// `getopts`/`close` sequence this floor is given is driven against the oracle's real `udp_inet`
/// port through `port_control`, and the reply bytes are compared.
///
/// Every reply must agree byte for byte except `bind`: the two runs bind **different** ephemeral
/// ports, so only that reply's shape and result tag are comparable.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_covered_control_replies_agree_with_the_erl_oracle() {
    let calls = vec![
        (
            INET_REQ_OPEN,
            vec![INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT],
        ),
        (INET_REQ_BIND, vec![INET_AF_ANY, 0, 0]),
        (
            INET_REQ_GETOPTS,
            vec![
                INET_LOPT_MODE,
                INET_LOPT_DELIVER,
                INET_LOPT_UDP_READ_PACKETS,
            ],
        ),
        (
            INET_REQ_SETOPTS,
            vec![INET_OPT_REUSEADDR, 0, 0, 0, 1],
        ),
        (INET_REQ_GETOPTS, vec![INET_OPT_REUSEADDR]),
        (INET_REQ_SETOPTS, vec![INET_OPT_BROADCAST, 0, 0, 0, 1]),
        (INET_REQ_GETOPTS, vec![INET_OPT_BROADCAST]),
        (INET_REQ_SETOPTS, vec![INET_OPT_TTL, 0, 0, 0, 42]),
        (INET_REQ_GETOPTS, vec![INET_OPT_TTL]),
        (INET_REQ_GETOPTS, vec![INET_OPT_UNSERVED]),
        (INET_REQ_SETOPTS, vec![INET_OPT_UNSERVED, 0, 0, 0, 1]),
        (INET_REQ_CLOSE, vec![]),
    ];
    let Some(dump) = erl_stdout(&oracle_expression(&calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet control-reply oracle check");
        return;
    };
    let Some(oracle) = parse_reply_dump(&dump) else {
        panic!("could not parse the oracle reply dump: {dump:?}");
    };
    assert_eq!(oracle.len(), calls.len(), "one reply per call: {oracle:?}");

    // The Rust side runs the identical sequence through the port layer.
    let mut poller = poller();
    let (mut ports, id) = udp_port(&mut poller);
    let rust = vec![
        control(&mut ports, &mut poller, id, "open", &open_arg()),
        control(&mut ports, &mut poller, id, "bind", &[INET_AF_ANY, 0, 0]),
        control(
            &mut ports,
            &mut poller,
            id,
            "getopts",
            &[
                INET_LOPT_MODE,
                INET_LOPT_DELIVER,
                INET_LOPT_UDP_READ_PACKETS,
            ],
        ),
        control(
            &mut ports,
            &mut poller,
            id,
            "setopts",
            &[INET_OPT_REUSEADDR, 0, 0, 0, 1],
        ),
        control(&mut ports, &mut poller, id, "getopts", &[INET_OPT_REUSEADDR]),
        control(
            &mut ports,
            &mut poller,
            id,
            "setopts",
            &[INET_OPT_BROADCAST, 0, 0, 0, 1],
        ),
        control(&mut ports, &mut poller, id, "getopts", &[INET_OPT_BROADCAST]),
        control(
            &mut ports,
            &mut poller,
            id,
            "setopts",
            &[INET_OPT_TTL, 0, 0, 0, 42],
        ),
        control(&mut ports, &mut poller, id, "getopts", &[INET_OPT_TTL]),
        control(&mut ports, &mut poller, id, "getopts", &[INET_OPT_UNSERVED]),
        control(
            &mut ports,
            &mut poller,
            id,
            "setopts",
            &[INET_OPT_UNSERVED, 0, 0, 0, 1],
        ),
        control(&mut ports, &mut poller, id, "close", b""),
    ];
    ports.close(&mut poller, id).expect("closes the port");

    eprintln!("oracle udp_inet replies: {oracle:?}");
    eprintln!("rust udp_inet replies:   {rust:?}");

    // Reply 1 (`bind`) is the one the two runs cannot share: the reply carries the ephemeral port
    // the kernel happened to assign.
    const BIND: usize = 1;
    for (index, (rust_reply, oracle_reply)) in rust.iter().zip(oracle.iter()).enumerate() {
        if index == BIND {
            assert_eq!(rust_reply.len(), oracle_reply.len(), "the `bind` reply length");
            assert_eq!(rust_reply[0], oracle_reply[0], "the `bind` result tag");
            assert_ne!(
                u16::from_be_bytes([rust_reply[1], rust_reply[2]]),
                0,
                "this floor reports the port the kernel assigned it"
            );
            assert_ne!(
                u16::from_be_bytes([oracle_reply[1], oracle_reply[2]]),
                0,
                "the oracle reports its own bound port"
            );
            continue;
        }
        assert_eq!(rust_reply, oracle_reply, "reply {index} disagrees");
    }

    // The oracle's own values, on the record: `open`/`setopts`/`close` are the C's `ctl_reply_ok`
    // = `[1]`, and the datagram driver-state defaults are reported in request order.
    assert_eq!(oracle[0], vec![INET_REP_OK], "INET_REQ_OPEN replies ok");
    assert_eq!(
        oracle[2],
        vec![
            INET_REP_OK,
            INET_LOPT_MODE,
            0,
            0,
            0,
            0, // `{mode, list}` = 0
            INET_LOPT_DELIVER,
            0,
            0,
            0,
            1, // `{deliver, term}` = 1
            INET_LOPT_UDP_READ_PACKETS,
            0,
            0,
            0,
            5, // `{udp_read_packets, 5}`
        ],
        "the datagram driver-state defaults"
    );
    // Each set/read-back pair echoes the value that was set, big-endian.
    assert_eq!(
        oracle[4],
        vec![INET_REP_OK, INET_OPT_REUSEADDR, 0, 0, 0, 1],
        "the `reuseaddr` read-back"
    );
    assert_eq!(
        oracle[6],
        vec![INET_REP_OK, INET_OPT_BROADCAST, 0, 0, 0, 1],
        "the `broadcast` read-back"
    );
    assert_eq!(
        oracle[8],
        vec![INET_REP_OK, INET_OPT_TTL, 0, 0, 0, 42],
        "the `ttl` read-back"
    );
    // An option code the datagram store does not serve answers the C's `[0] ++ "einval"` for
    // either verb — the unknown-code case this floor shares with the oracle.
    assert_eq!(
        oracle[9],
        ctl_xerror("einval"),
        "an unserved `getopts` code"
    );
    assert_eq!(
        oracle[10],
        ctl_xerror("einval"),
        "an unserved `setopts` code"
    );
    assert_eq!(oracle[11], vec![INET_REP_OK], "INET_REQ_CLOSE replies ok");
}

/// The driver's one deliberate divergence, measured against the oracle: the C's `recv` is an
/// **asynchronous** request and this floor's is a **read-now** verb.
///
/// The C's `PACKET_REQ_RECV` (code `60`) takes a `Timeout(4), Length(4)` payload, enqueues an
/// async operation on the descriptor's `INET_MAX_ASYNC = 1` queue and answers the **async-op id**
/// (`[1] ++ async_id_be16`, three bytes); the datagram itself arrives later as an
/// `{inet_async, Port, Ref, Result}` message, which a single passively-driven port cannot observe
/// here. This floor has no driver message path, so `recv` reads one datagram now and answers its
/// result.
///
/// Only the oracle's **shape** is pinned — no expected value is fabricated for bytes the two
/// designs do not share.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_recv_verb_is_the_documented_divergence_from_the_oracles_code_60() {
    let calls = vec![
        (
            INET_REQ_OPEN,
            vec![INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT],
        ),
        (INET_REQ_BIND, vec![INET_AF_ANY, 0, 0]),
        (PACKET_REQ_RECV, vec![0u8; 8]),
    ];
    let Some(dump) = erl_stdout(&oracle_expression(&calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet recv-divergence oracle check");
        return;
    };
    let Some(oracle) = parse_reply_dump(&dump) else {
        panic!("could not parse the oracle reply dump: {dump:?}");
    };
    assert_eq!(oracle.len(), calls.len(), "one reply per call: {oracle:?}");

    // Measured, not assumed: the oracle accepts the code-60 request and answers the async-op id.
    // (The module doc predicts `[0]einval` there; that holds only for the *empty* argument, which
    // the C rejects without enqueuing anything.)
    eprintln!("oracle code-60 reply: {:?}", oracle[2]);
    assert_eq!(oracle[2][0], INET_REP_OK, "the oracle's async request is queued");
    assert_eq!(
        oracle[2].len(),
        3,
        "the oracle answers the async-op id, not the datagram: {:?}",
        oracle[2]
    );

    // This floor's `recv` reads now. A drained socket answers the bare `[1]` — one byte, and a
    // different shape from the oracle's async-op id.
    let mut poller = poller();
    let (mut ports, id) = udp_port(&mut poller);
    let port = open_and_bind_ephemeral(&mut poller, &mut ports, id);
    let empty = recv(&mut poller, &mut ports, id);
    assert_eq!(empty, vec![INET_REP_OK], "a drained socket answers [1]");
    assert_ne!(empty, oracle[2], "the same call answers a different shape");

    // With a datagram pending the reply is the result itself — the eight-byte source-carrying
    // form — which is what the C delivers as the *later* async message instead.
    let sender = UdpSocket::bind("127.0.0.1:0").expect("a second real socket binds");
    sender
        .send_to(b"x", ("127.0.0.1", port))
        .expect("the datagram is sent");
    let delivered = recv_until_delivered(&mut poller, &mut ports, id);
    assert_eq!(
        delivered.len(),
        8,
        "the read-now reply carries the source address: {delivered:?}"
    );
    assert_ne!(
        delivered, oracle[2],
        "the oracle's async-op id is never this floor's reply"
    );
    assert_eq!(
        harvest(&mut poller, &mut ports),
        vec![PortNotification::Data {
            port: id,
            read: ReadMessage::body_only(b"x".to_vec()),
        }]
    );

    ports.close(&mut poller, id).expect("closes the port");
}
