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
//! * `recv` (the crate-local verb, `../src/drivers/inet/udp.rs`) reads a datagram **now** and
//!   reports its source on the same reply: `[1] ++ src_address` with a datagram pending, the bare
//!   `[1]` when none is.
//! * `the_code_60_recv_is_an_async_request_completed_by_a_later_round` — the C's
//!   `PACKET_REQ_RECV` (code 60): the reply is the 16-bit async-op id **and nothing else**, and the
//!   datagram (source address first, payload after) crosses the completion channel at a later
//!   round. The guard ladder (`[0]exbadport`, `[0]einval`, `[0]ealready`), the id accounting, the
//!   exactly-once delivery and the `{error, closed}` teardown are pinned against the real driver.
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

/// One control op through the **numeric** opcode route — the form `erlang:port_control/3` sends
/// (`Op + ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`), which is where the drivers decode the C request
/// numbers.
fn control_code(
    ports: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    code: u32,
    arg: &[u8],
) -> Vec<u8> {
    let op = (u64::from(code) + u64::from(CONTROL_MAGIC)).to_string();
    control(ports, poller, id, &op, arg)
}

/// The `Timeout(4), Length(4)` operand of `PACKET_REQ_RECV`(60) (`inet_drv.c:14990`).
fn recv_operand(timeout_ms: i32, length: u32) -> Vec<u8> {
    let mut arg = timeout_ms.to_be_bytes().to_vec();
    arg.extend_from_slice(&length.to_be_bytes());
    arg
}

/// The C's `INET_INFINITY` (`inet_drv.c:1043`): the 4-byte timeout meaning "no deadline".
const INET_INFINITY: i32 = -1;

/// One runtime round of the completion channel's consumers: poll for readiness, dispatch it, run
/// the port tasks (the round's `drive_async` point) and drain the completions.
fn round(poller: &mut Poller, ports: &mut PortRegistry) -> Vec<PortCompletion> {
    let events = poll_once(poller);
    ports
        .handle_events(poller, &events)
        .expect("the ready events are dispatched");
    ports
        .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the port tasks run");
    ports.take_completions()
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

/// The code-60 oracle: the async receive ladder and one completed receive, measured on a real raw
/// `udp_inet` port.
///
/// The port binds an ephemeral port (`INET_AF_ANY`, so the kernel picks one and the `bind` reply
/// names it); a **second** raw `udp_inet` port (socket-less) supplies the `[0]exbadport` row, and a
/// real `gen_udp` socket sends the datagram. Printed, in order: the socket-less row, the queued
/// receive's reply, the `ealready` refusal, the short-operand `einval` refusal, the completed
/// receive's `{Id, Bytes}` (the bytes flattened, as the port's `[binary]` mode delivers them), the
/// sender's port (so the address prefix can be checked), the next request's id, and the teardown
/// pair.
const CODE_60_EXPRESSION: &str = "M = 16#03f1a300, \
 P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
 F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
 _ = F(1, <<1,2,0>>), \
 BP = case F(6, <<3,0,0>>) of [1, BH, BL] -> BH * 256 + BL; _ -> exit(no_bind) end, \
 Q = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
 G = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(Q, C + M, A))) end, \
 Bare = G(60, <<-1:32,0:32>>), \
 Id1 = F(60, <<-1:32,0:32>>), \
 Busy = F(60, <<-1:32,0:32>>), \
 Short = F(60, <<-1:32>>), \
 {ok, S} = gen_udp:open(0, [binary, {ip,{127,0,0,1}}]), \
 {ok, SPort} = inet:port(S), \
 ok = gen_udp:send(S, {127,0,0,1}, BP, \"x\"), \
 Recv = receive {inet_async, _, IdA, {ok, D}} -> {IdA, binary_to_list(iolist_to_binary(D))}; \
                Other -> {other, Other} after 5000 -> none end, \
 Next = F(60, <<-1:32,0:32>>), \
 port_close(P), \
 Wait = fun W() -> receive \
     {inet_async, _, IdT, {error, R}} -> {IdT, atom_to_list(R)}; \
     {'EXIT', _, _} -> W(); \
     T -> {other, T} after 1000 -> none end end, \
 Teardown = Wait(), \
 S2 = fun(Ls) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- Ls])) end, \
 io:format(\"bare=~s~n\", [S2(Bare)]), \
 io:format(\"id1=~s~n\", [S2(Id1)]), \
 io:format(\"busy=~s~n\", [S2(Busy)]), \
 io:format(\"short=~s~n\", [S2(Short)]), \
 io:format(\"next=~s~n\", [S2(Next)]), \
 io:format(\"recv=~w sport=~w~n\", [Recv, SPort]), \
 io:format(\"teardown=~p~n\", [Teardown])";

/// The value of a `key=value` field on an oracle output line.
fn code_60_field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    let start = stdout.find(key)? + key.len();
    let rest = &stdout[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// The code-60 request is the C's **asynchronous** receive: the reply is the async-op id and
/// nothing else, and the datagram crosses the completion channel at a later round.
///
/// The rows, each the real driver's:
///
/// 1. a queued receive answers `[1] ++ <<Id:16/big>>` — `[1,0,0]` — and stages **no** completion;
/// 2. a second concurrent receive is `[0] ++ "ealready"` (the single `INET_MAX_ASYNC` slot), and
///    the refused enqueue still consumes an id, because `NEW_ASYNC_ID` is taken *before* the
///    capacity check (`inet_drv.c:1549`, `:15003`);
/// 3. a short operand (`len != 8`) is `[0] ++ "einval"` and a socket-less port `[0] ++
///    "exbadport"` (the C's `!IS_OPEN` arm, `:14980`, which runs before the operand check);
/// 4. the datagram completes **at `execute_port_tasks`** — no readiness event is needed, the bytes
///    are already queued in the socket — exactly **once**, as `InetAsyncResult::Data`, whose
///    `read.flattened()` is the oracle's flat list-mode `[1, PortHi, PortLo, A, B, C, D, …data]`;
/// 5. closing a port with a receive pending answers it `{error, closed}` exactly once
///    (`async_error_am_all`, `:2597`-`:2608`).
///
/// Rows 1–5's control replies and the id accounting are byte-compared against the real driver when
/// `erl` is on `PATH` (the ephemeral ports differ between the two runs, so the address *prefix* is
/// shape-compared and checked against each side's own sender); without the oracle the Rust rows
/// still run, and the check **skips loudly**.
#[test]
fn the_code_60_recv_is_an_async_request_completed_by_a_later_round() {
    // The oracle's rows first: on an unavailable oracle this is the loud skip.
    let oracle = match erl_stdout(CODE_60_EXPRESSION) {
        Some(dump) => {
            // The probe's output is ASCII (`key=value` lines of decimal byte lists), so a lossy
            // decode is exact here.
            let stdout = String::from_utf8_lossy(&dump).into_owned();
            eprintln!("oracle code-60 report: {}", stdout.trim());
            Some(stdout)
        }
        None => {
            eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet code-60 oracle cross-check");
            None
        }
    };
    // -- the socket-less row ------------------------------------------------
    let mut poller = poller();
    let (mut bare_ports, bare) = udp_port(&mut poller);
    let bare_reply = control_code(
        &mut bare_ports,
        &mut poller,
        bare,
        PACKET_REQ_RECV,
        &recv_operand(INET_INFINITY, 0),
    );
    assert_eq!(
        bare_reply,
        ctl_xerror("exbadport"),
        "a socket-less port refuses the request before looking at the operand"
    );
    if let Some(stdout) = &oracle {
        assert_eq!(
            code_60_field(stdout, "bare="),
            Some(
                bare_reply
                    .iter()
                    .map(|byte| byte.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
                    .as_str()
            ),
            "the socket-less row agrees with the oracle"
        );
    }
    // -- the request, the refused second request, the short operand --------
    let (mut ports, id) = udp_port(&mut poller);
    let port = open_and_bind_ephemeral(&mut poller, &mut ports, id);
    let first = control_code(
        &mut ports,
        &mut poller,
        id,
        PACKET_REQ_RECV,
        &recv_operand(INET_INFINITY, 0),
    );
    assert_eq!(first, vec![INET_REP_OK, 0, 0], "the reply is the id, not the datagram");
    assert!(
        ports.take_completions().is_empty(),
        "a queued receive completes nothing inline"
    );
    let second = control_code(
        &mut ports,
        &mut poller,
        id,
        PACKET_REQ_RECV,
        &recv_operand(INET_INFINITY, 0),
    );
    assert_eq!(
        second,
        ctl_xerror("ealready"),
        "the single async slot refuses a second concurrent receive"
    );
    let short = control_code(&mut ports, &mut poller, id, PACKET_REQ_RECV, &recv_operand(INET_INFINITY, 0)[..4]);
    assert_eq!(
        short,
        ctl_xerror("einval"),
        "the operand must be the eight-byte Timeout(4), Length(4) record"
    );
    if let Some(stdout) = &oracle {
        let row = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| byte.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(code_60_field(stdout, "id1="), Some(row(&first).as_str()), "the id reply");
        assert_eq!(code_60_field(stdout, "busy="), Some(row(&second).as_str()), "the ealready row");
        assert_eq!(code_60_field(stdout, "short="), Some(row(&short).as_str()), "the einval row");
    }
    // -- the datagram completes at the port-task drive point ---------------
    let sender = UdpSocket::bind("127.0.0.1:0").expect("a second real socket binds");
    let sender_port = sender
        .local_addr()
        .expect("the sender's address")
        .port();
    sender
        .send_to(b"x", ("127.0.0.1", port))
        .expect("the datagram is sent");
    let deadline = Instant::now() + WAIT;
    let mut completions = Vec::new();
    let mut rounds = 0u32;
    while completions.is_empty() {
        assert!(Instant::now() < deadline, "the queued receive never completed");
        // Deliberately **no** readiness dispatch: `execute_port_tasks` is the round's async drive
        // point, and the datagram is already queued in the socket.
        ports
            .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the port tasks run");
        completions.extend(ports.take_completions());
        rounds += 1;
        if rounds > 1000 {
            thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(completions.len(), 1, "exactly one completion: {completions:?}");
    let completion = &completions[0];
    assert_eq!(completion.port, id, "the completion names the port that queued it");
    let (async_id, result) = match &completion.event {
        CompletionEvent::InetAsync { id, result } => (*id, result.clone()),
        other => panic!("an {{inet_async, …}} completion, not {other:?}"),
    };
    assert_eq!(async_id, 0, "the completion echoes the id its reply handed out");
    let read = match result {
        InetAsyncResult::Data(read) => read,
        other => panic!("a completed receive carries the datagram, not {other:?}"),
    };
    // The address is the read's *header* and the datagram its body, so the flat list a `[list]`
    // port would build is `[1, PortHi, PortLo, A, B, C, D] ++ payload`.
    let [sender_hi, sender_lo] = sender_port.to_be_bytes();
    let expected = vec![INET_AF_INET, sender_hi, sender_lo, 127, 0, 0, 1, b'x'];
    assert_eq!(
        read.header,
        expected[..7].to_vec(),
        "the header is the packed source address"
    );
    assert_eq!(read.body, Some(b"x".to_vec()), "the body is the datagram payload");
    assert_eq!(
        read.flattened(),
        expected,
        "the flattened read is the oracle's flat list-mode shape"
    );
    // Exactly once: a further round reports nothing, and the datagram is not re-delivered.
    assert!(round(&mut poller, &mut ports).is_empty(), "a completion is never reported twice");
    // -- the id accounting, and the next request ---------------------------
    let next = control_code(
        &mut ports,
        &mut poller,
        id,
        PACKET_REQ_RECV,
        &recv_operand(INET_INFINITY, 0),
    );
    assert_eq!(
        next,
        vec![INET_REP_OK, 0, 2],
        "the refused enqueue consumed id 1, so the next request is id 2"
    );
    if let Some(stdout) = &oracle {
        // The oracle's own completed receive: its id, and the same flattened shape with its own
        // sender's port.
        let recv_field = code_60_field(stdout, "recv=").expect("the oracle's receive row");
        let sport = code_60_field(stdout, "sport=")
            .and_then(|value| value.parse::<u16>().ok())
            .expect("the oracle's sender port");
        let [oracle_hi, oracle_lo] = sport.to_be_bytes();
        let expected_oracle = format!(
            "{{0,[1,{oracle_hi},{oracle_lo},127,0,0,1,120]}}"
        );
        assert_eq!(
            recv_field, expected_oracle,
            "the oracle's completed receive is {{Id, [1, PortHi, PortLo, A, B, C, D, …data]}}"
        );
        let row = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| byte.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(
            code_60_field(stdout, "next="),
            Some(row(&next).as_str()),
            "the id accounting agrees with the oracle"
        );
    }
    // -- the teardown answers the pending receive `{error, closed}`, once --
    ports.close(&mut poller, id).expect("closes the port");
    let teardown = ports.take_completions();
    assert_eq!(teardown.len(), 1, "the pending receive is answered once: {teardown:?}");
    assert_eq!(teardown[0].port, id, "the dead port's completion still names it");
    assert_eq!(
        teardown[0].event,
        CompletionEvent::InetAsync {
            id: 2,
            result: InetAsyncResult::Error(InetReason::Closed),
        },
        "the pending receive is completed {{error, closed}}"
    );
    assert_eq!(
        InetReason::Closed.atom_name(),
        "closed",
        "the reason renders as the oracle's atom"
    );
    assert!(
        ports.take_completions().is_empty(),
        "the teardown completion is never re-reported"
    );
    if let Some(stdout) = &oracle {
        assert_eq!(
            code_60_field(stdout, "teardown="),
            Some("{2,\"closed\"}"),
            "the oracle answers its pending receive {{error, closed}} with the same id"
        );
    }
}
