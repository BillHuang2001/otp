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

//! The **`{active, …}` input channel** driven end-to-end through a real [`PortRegistry`] port and
//! a real [`Poller`] round, with the datagram/stream traffic supplied by an **external**
//! `std::net` peer.
//!
//! # Why this suite exists
//!
//! The two drivers *produce* on the active-input channel in-crate (`src/drivers/inet/active.rs`
//! for `tcp_inet`, `src/drivers/inet/udp/active.rs` for `udp_inet`), and their own suites
//! (`src/drivers/inet/active_tests.rs`, `src/drivers/inet/udp/active_tests.rs`) drive the driver
//! bodies. What nothing pinned is the **level a runtime consumer actually runs**: port open through
//! [`PortRegistry::open_named`], the `prim_inet` numeric opcodes, one poll →
//! [`PortRegistry::handle_events`] → [`PortRegistry::execute_port_tasks`] round, and
//! [`PortRegistry::take_active_inputs`] as the drain. That is the path `beam_rt`'s main loop
//! takes, and it is the seam A.4 data-path gap #1 ("`{active, …}` input delivery") closes on.
//!
//! Every case therefore opens a **built-in** driver port (`"udp_inet"`/`"tcp_inet"`) out of
//! [`DriverRegistry::with_builtins`], arms it with `setopts {active, …}`, and lets a plain
//! `std::net::UdpSocket`/`std::net::TcpStream` — never a second Erlang VM, never an in-VM socket —
//! supply the bytes.
//!
//! # The channel shape this suite pins (frozen; it must not change)
//!
//! [`ActiveInput`] is `{ port: PortId, event: ActiveInputEvent }` with
//! [`ActiveInputEvent::Data(ReadMessage)`](ActiveInputEvent::Data) for one delivered unit and
//! [`ActiveInputEvent::Passive`] for a spent `{active, N}` counter. `ReadMessage { header, body }`
//! keeps the C's header/body split:
//!
//! * a **datagram** delivery's `header` is the seven-byte packed source address
//!   `[AF_INET, PortHi, PortLo, A, B, C, D]` and its `body` the payload
//!   (`packet_reply_binary_data` → `packet_binary_message`, `inet_drv.c:4335`/`:4110`);
//! * a **stream** delivery's `header` is the `{header, N}` split (`desc->hsz`, the C's
//!   `tcp_message`, `:3979`), which is empty for the default `{header, 0}`, and its `body` the read
//!   burst (or the single frame under `{packet, N}`, whose framing header is already stripped by
//!   `packet_get_body`).
//!
//! The event carries **no atom**: the runtime picks `{tcp_passive, S}` from `{udp_passive, S}` by
//! the driving port's name (this suite reads the name back from the port instead of guessing).
//!
//! # Determinism
//!
//! Nothing here is tuned for an idle host: every wait is a **condition** — pump one bounded
//! [`Poller::poll`] slice per iteration and retry until the expected delivery (or until the
//! "nothing is delivered" budget is spent) under a generous hang guard. A starved host makes a
//! case slower, never wrong. Assertions are on the **content** and the **set** of deliveries, never
//! on how many rounds they took.
use std::io::Write;
use std::net::{TcpStream, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::drivers::inet::{
    DRIVER_NAME, INET_ACTIVE, INET_AF_ANY, INET_AF_INET, INET_AF_LOOPBACK, INET_INFINITY,
    INET_LOPT_ACTIVE, INET_LOPT_PACKET, INET_MULTI, INET_ONCE, INET_PROTO_DEFAULT,
    INET_REP_ERROR, INET_REP_OK, INET_TYPE_DGRAM, INET_TYPE_STREAM, UDP_DRIVER_NAME,
};
use beam_io::prelude::*;
use beam_process::PortId;

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`): what
/// `erlang:port_control/3` adds to every opcode, and what the drivers subtract again.
const CONTROL_MAGIC: u32 = 0x03f1_a300;

/// The C request numbers this suite drives (`inet_drv.c:805`-`:847`).
const INET_REQ_OPEN: u32 = 1;
const INET_REQ_BIND: u32 = 6;
const INET_REQ_SETOPTS: u32 = 7;
const INET_REQ_LISTEN: u32 = 27;
/// `PACKET_REQ_RECV` (`inet_drv.c:14990`) — the datagram half's asynchronous receive.
const PACKET_REQ_RECV: u32 = 60;

/// The hang guard: how long a condition may stay unsatisfied before the case fails, instead of
/// hanging. Deliberately generous — it is a liveness bound, not a performance budget.
const WAIT: Duration = Duration::from_secs(10);

/// One reactor round's poll slice. Small, so a retry loop makes bounded progress and never
/// busy-spins; the delivery is normally already queued by the time the first round runs.
const POLL_SLICE: Duration = Duration::from_millis(20);

/// How long the "nothing more arrives" drains keep pumping the reactor for. Long enough to cover
/// several poll slices (so a delivery that was merely late would be caught), short enough that the
/// negative rows stay cheap.
const QUIET_BUDGET: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// The runtime's port plumbing
// ---------------------------------------------------------------------------

/// A live poll set, a port registry, and the built-in driver registry — the three things
/// `beam_rt`'s main loop holds.
struct Rig {
    poller: Poller,
    ports: PortRegistry,
    drivers: DriverRegistry,
    /// How many rounds this rig has run — only ever reported on `--nocapture`, never asserted.
    rounds: usize,
}

/// What one reactor round observed.
#[derive(Debug)]
struct Round {
    /// How many poller events the round was handed (a readiness edge, or nothing) — the
    /// non-vacuity witness that the reactor really ran.
    events: usize,
    /// Every `{active, …}` delivery the round produced, in delivery order.
    active: Vec<ActiveInput>,
    /// Every completion the round produced (the passive `{inet_async, …}` channel).
    completions: Vec<PortCompletion>,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            poller: Poller::new().expect("a poll set"),
            ports: PortRegistry::new(),
            drivers: DriverRegistry::with_builtins(),
            rounds: 0,
        }
    }

    /// Opens a port for the named **built-in** driver — the way the runtime resolves
    /// `open_port({spawn_driver, Name}, …)`.
    fn open(&mut self, name: &str) -> PortId {
        self.ports
            .open_named(&mut self.poller, &self.drivers, name)
            .unwrap_or_else(|err| panic!("the {name} port opens: {err:?}"))
    }

    /// One control call through the **numeric** opcode route (`Op + CONTROL_MAGIC`, the decimal
    /// command string `erlang:port_control/3` sends).
    fn op(&mut self, id: PortId, code: u32, arg: &[u8]) -> Vec<u8> {
        let op = (u64::from(code) + u64::from(CONTROL_MAGIC)).to_string();
        self.ports
            .port_control(&mut self.poller, id, &op, arg)
            .unwrap_or_else(|err| panic!("the control op {code} answered: {err:?}"))
    }

    /// One **named** control verb (the `prim_inet` verbs this floor also serves by name).
    fn named(&mut self, id: PortId, verb: &str, arg: &[u8]) -> Vec<u8> {
        self.ports
            .port_control(&mut self.poller, id, verb, arg)
            .unwrap_or_else(|err| panic!("the `{verb}` verb answered: {err:?}"))
    }

    /// Opens a `"udp_inet"` port, creates its datagram socket and binds it to an ephemeral
    /// loopback port; returns the port id and the port the kernel assigned.
    fn udp_bound(&mut self) -> (PortId, u16) {
        let id = self.open(UDP_DRIVER_NAME);
        assert_eq!(
            self.op(id, INET_REQ_OPEN, &open_dgram_arg()),
            ctl_ok(),
            "INET_REQ_OPEN(1) creates the datagram socket"
        );
        let bound = self.op(id, INET_REQ_BIND, &[INET_AF_ANY, 0, 0]);
        assert_eq!(bound.first(), Some(&INET_REP_OK), "the bind succeeds: {bound:?}");
        assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
        let port = u16::from_be_bytes([bound[1], bound[2]]);
        assert_ne!(port, 0, "the kernel assigned a port");
        (id, port)
    }

    /// `setopts {active, Value}` (`INET_LOPT_ACTIVE`), asserting the walk accepted it.
    fn arm(&mut self, id: PortId, value: i32) {
        let record = record(INET_LOPT_ACTIVE, value);
        assert_eq!(
            self.op(id, INET_REQ_SETOPTS, &record),
            ctl_ok(),
            "{{active,{value}}} is accepted"
        );
    }

    /// `setopts {active, N}` — the seven-byte `[opt, INET_MULTI_be32, counter_be16]` record
    /// `inet:setopts(S, [{active, N}])` puts on the wire (`inet_drv.c:6977`).
    fn arm_multi(&mut self, id: PortId, n: i16) {
        assert_eq!(
            self.op(id, INET_REQ_SETOPTS, &active_multi(n)),
            ctl_ok(),
            "{{active,{n}}} is accepted"
        );
    }

    /// Opens a `"tcp_inet"` port, binds it to an ephemeral loopback port, listens, lets an
    /// **external** `std::net::TcpStream` connect, and transfers the port to the accepted
    /// connection with the named `accept` verb (the reply carries the accepted descriptor as
    /// `[1] ++ fd_be32`). Returns the port id and the peer's end.
    fn tcp_accepted(&mut self) -> (PortId, TcpStream) {
        let id = self.open(DRIVER_NAME);
        assert_eq!(
            self.op(id, INET_REQ_OPEN, &open_stream_arg()),
            ctl_ok(),
            "INET_REQ_OPEN(1) creates the stream socket"
        );
        let bound = self.op(id, INET_REQ_BIND, &loopback_arg(0));
        assert_eq!(bound.first(), Some(&INET_REP_OK), "the bind succeeds: {bound:?}");
        assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
        let port = u16::from_be_bytes([bound[1], bound[2]]);
        assert_ne!(port, 0, "the kernel assigned a port");
        assert_eq!(
            self.op(id, INET_REQ_LISTEN, &[0, 5]),
            ctl_ok(),
            "INET_REQ_LISTEN(27)"
        );

        let peer = TcpStream::connect(("127.0.0.1", port)).expect("the external peer connects");
        peer.set_nodelay(true).expect("nodelay is a socket option");

        // `connect(2)` completed, so the connection already sits in the accept queue; retry
        // anyway, so a loaded host cannot turn a scheduling delay into a failure.
        let deadline = Instant::now() + WAIT;
        loop {
            let reply = self.named(id, "accept", b"");
            assert_eq!(
                reply.first(),
                Some(&INET_REP_OK),
                "the `accept` verb answers ok: {reply:?}"
            );
            if reply.len() == 5 {
                break;
            }
            assert_eq!(reply.len(), 1, "an `accept` reply is [1] or [1] ++ fd_be32");
            assert!(Instant::now() < deadline, "the listener never accepted the peer");
            thread::sleep(Duration::from_millis(5));
        }
        (id, peer)
    }

    /// Runs exactly **one** reactor round: poll, dispatch the readiness (`handle_events` schedules
    /// *and* runs the round's port tasks), run the port tasks again (the round's `drive_async`
    /// point) and drain both side channels.
    fn round(&mut self) -> Round {
        self.rounds += 1;
        let events = match self.poller.poll(Some(POLL_SLICE)) {
            Ok(events) => events,
            // A wait the kernel interrupted is not a failure (`mio` does not retry `EINTR`).
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => Vec::new(),
            Err(err) => panic!("poll failed: {err:?}"),
        };
        self.ports
            .handle_events(&mut self.poller, &events)
            .expect("the ready events are dispatched");
        self.ports
            .execute_port_tasks(&mut self.poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the port tasks run");
        Round {
            events: events.len(),
            active: self.ports.take_active_inputs(),
            completions: self.ports.take_completions(),
        }
    }

    /// Pumps rounds until at least `want` deliveries have been drained (the deliveries of every
    /// round are concatenated, in arrival order), under the hang guard.
    ///
    /// Every delivery on this channel is produced from a driver callback, so the rounds that
    /// carried one must also have carried a **readiness event** — the non-vacuity witness that the
    /// reactor, not the drain, produced the data.
    fn await_active(&mut self, want: usize) -> Vec<ActiveInput> {
        let deadline = Instant::now() + WAIT;
        let mut got = Vec::new();
        let mut events_seen = 0usize;
        while got.len() < want {
            assert!(
                Instant::now() < deadline,
                "only {} of {want} deliveries arrived: {got:?}",
                got.len()
            );
            let round = self.round();
            events_seen += round.events;
            got.extend(round.active);
        }
        assert!(
            events_seen > 0,
            "the reactor never reported readiness, so the delivery cannot be the driver's"
        );
        got
    }

    /// Pumps rounds for `budget`, asserting **nothing** is delivered in any of them — the
    /// negative half of a case, kept honest by the rounds really running.
    fn assert_quiet(&mut self, budget: Duration, what: &str) {
        let deadline = Instant::now() + budget;
        loop {
            let round = self.round();
            assert!(round.active.is_empty(), "{what}: {:?}", round.active);
            if Instant::now() >= deadline {
                return;
            }
        }
    }

    /// Pumps rounds until at least `want` completions have been drained, under the hang guard.
    fn await_completions(&mut self, want: usize) -> Vec<PortCompletion> {
        let deadline = Instant::now() + WAIT;
        let mut got = Vec::new();
        while got.len() < want {
            assert!(
                Instant::now() < deadline,
                "only {} of {want} completions arrived: {got:?}",
                got.len()
            );
            got.extend(self.round().completions);
        }
        got
    }

    /// Sends one datagram `payload` to the bound loopback `port` from a **fresh external socket**,
    /// returning that socket (its `local_addr` is the delivery's expected source address).
    fn inject(&self, port: u16, payload: &[u8]) -> UdpSocket {
        let sender = UdpSocket::bind("127.0.0.1:0").expect("an external sender socket");
        sender
            .send_to(payload, ("127.0.0.1", port))
            .expect("the datagram is injected");
        sender
    }
}

/// The packed `open` argument of a datagram socket: `[AF_INET, SOCK_DGRAM, default protocol]`.
fn open_dgram_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT]
}

/// The packed `open` argument of a stream socket: `[AF_INET, SOCK_STREAM, default protocol]`.
fn open_stream_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
}

/// The packed `INET_AF_LOOPBACK` argument naming `127.0.0.1:port` (the C's three-byte form).
fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_LOOPBACK, hi, lo]
}

/// One five-byte `[opt, value_be32]` `setopts` record.
fn record(code: u8, value: i32) -> Vec<u8> {
    let mut bytes = vec![code];
    bytes.extend_from_slice(&value.to_be_bytes());
    bytes
}

/// The **`{active, N}`** record: the option code, the value [`INET_MULTI`], then the trailing
/// big-endian 16-bit counter — `inet:setopts(S, [{active, N}])`'s wire form (`inet_drv.c:6977`).
fn active_multi(n: i16) -> Vec<u8> {
    let mut bytes = record(INET_LOPT_ACTIVE, INET_MULTI);
    bytes.extend_from_slice(&n.to_be_bytes());
    bytes
}

/// Concatenates option records into one `setopts` payload.
fn records(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flatten().copied().collect()
}

/// The C's `ctl_ok()` reply: `[INET_REP_OK]`.
fn ctl_ok() -> Vec<u8> {
    vec![INET_REP_OK]
}

/// The C's `ctl_xerror(name)` reply: the `[INET_REP_ERROR]` tag plus the lowercase name.
fn ctl_xerror(name: &str) -> Vec<u8> {
    let mut reply = vec![INET_REP_ERROR];
    reply.extend_from_slice(name.as_bytes());
    reply
}

/// The `Timeout(4), Length(4)` operand of `PACKET_REQ_RECV`(60) (`inet_drv.c:14990`).
fn recv_operand(timeout_ms: i32, length: u32) -> Vec<u8> {
    let mut arg = timeout_ms.to_be_bytes().to_vec();
    arg.extend_from_slice(&length.to_be_bytes());
    arg
}

/// The `(address, port)` a packed source-address header names:
/// `[AF_INET, port_hi, port_lo, a, b, c, d]` (`wire::pack_address`).
fn split_packed(header: &[u8]) -> ([u8; 4], u16) {
    assert_eq!(header.len(), 7, "the packed address is seven bytes: {header:?}");
    assert_eq!(header[0], INET_AF_INET, "the address family tag");
    let port = u16::from_be_bytes([header[1], header[2]]);
    let addr = [header[3], header[4], header[5], header[6]];
    (addr, port)
}

/// Every `Data` delivery of a drain, in order.
fn data_of(inputs: &[ActiveInput]) -> Vec<&ReadMessage> {
    inputs
        .iter()
        .filter_map(|input| match &input.event {
            ActiveInputEvent::Data(read) => Some(read),
            ActiveInputEvent::Passive | ActiveInputEvent::Error(_) => None,
        })
        .collect()
}

/// How many `Passive` deliveries a drain carried.
fn passive_count(inputs: &[ActiveInput]) -> usize {
    inputs
        .iter()
        .filter(|input| input.event == ActiveInputEvent::Passive)
        .count()
}

// ---------------------------------------------------------------------------
// 1. UDP `{active, true}` — every datagram, delivered, with its packed source address
// ---------------------------------------------------------------------------

/// An armed datagram port hands one arriving datagram to its owner through the reactor, with the
/// **sender's** packed address as the read header and the payload as the body — and nothing else.
///
/// The sender is a plain `std::net::UdpSocket` bound to `127.0.0.1:0`; the delivery's decoded
/// address and port are compared with that socket's own `local_addr()`.
#[test]
fn an_active_udp_port_delivers_an_external_datagram_with_the_packed_source_address() {
    let mut rig = Rig::new();
    let (id, bound) = rig.udp_bound();
    assert_eq!(
        rig.ports.get(id).expect("live").driver_name(),
        UDP_DRIVER_NAME,
        "the port runs the built-in udp_inet driver"
    );
    rig.arm(id, INET_ACTIVE);

    let sender = rig.inject(bound, b"ping");
    let sender_addr = sender.local_addr().expect("the sender's local address");

    let inputs = rig.await_active(1);
    assert_eq!(inputs.len(), 1, "exactly one delivery: {inputs:?}");
    assert_eq!(inputs[0].port, id, "the delivery names the delivering port");
    assert_eq!(passive_count(&inputs), 0, "{{active,true}} never reports a passive message");

    let data = data_of(&inputs);
    assert_eq!(data.len(), 1, "one data delivery, no passive event");
    let read = data[0];
    let (addr, port) = split_packed(&read.header);
    assert_eq!(port, sender_addr.port(), "the header names the sender's port");
    assert_eq!(addr, [127, 0, 0, 1], "the header names the sender's address");
    assert_eq!(
        read.body.as_deref(),
        Some(&b"ping"[..]),
        "the body is the datagram payload"
    );
    eprintln!("case 1: 1 delivery after {} round(s)", rig.rounds);
}

// ---------------------------------------------------------------------------
// 2. UDP `{active, false}` — nothing on the active channel, the passive path untouched
// ---------------------------------------------------------------------------

/// A **passive** datagram port reports nothing on the active channel even though the reactor sees
/// its socket readable, and the passive `PACKET_REQ_RECV`(60) route still returns the very
/// datagram the active channel declined — the regression pin that the passive path is untouched.
#[test]
fn a_passive_udp_port_reports_nothing_and_keeps_the_passive_receive_path() {
    let mut rig = Rig::new();
    let (id, bound) = rig.udp_bound();
    assert_eq!(
        rig.op(id, INET_REQ_SETOPTS, &record(INET_LOPT_ACTIVE, 0)),
        ctl_ok(),
        "{{active,false}} is accepted"
    );

    let sender = rig.inject(bound, b"passive");
    let sender_addr = sender.local_addr().expect("the sender's local address");
    rig.assert_quiet(QUIET_BUDGET, "a passive port never delivers on the active channel");

    // The passive floor: the queued asynchronous receive owns the socket and completes with the
    // source-carrying read, exactly as it did before the active channel existed.
    let reply = rig.op(id, PACKET_REQ_RECV, &recv_operand(INET_INFINITY, 0));
    assert_eq!(
        reply,
        vec![INET_REP_OK, 0, 0],
        "the reply is the async-op id, not the datagram"
    );
    let completions = rig.await_completions(1);
    assert_eq!(completions.len(), 1, "exactly one completion: {completions:?}");
    assert_eq!(completions[0].port, id, "the completion names its port");
    let (async_id, result) = match &completions[0].event {
        CompletionEvent::InetAsync { id, result } => (*id, result.clone()),
        other => panic!("an {{inet_async, …}} completion, not {other:?}"),
    };
    assert_eq!(async_id, 0, "the completion echoes the id its reply handed out");
    let read = match result {
        InetAsyncResult::Data(read) => read,
        other => panic!("a completed receive carries the datagram, not {other:?}"),
    };
    let (addr, port) = split_packed(&read.header);
    assert_eq!(port, sender_addr.port(), "the passive receive names the sender");
    assert_eq!(addr, [127, 0, 0, 1], "the sender is loopback");
    assert_eq!(
        read.body.as_deref(),
        Some(&b"passive"[..]),
        "the datagram the active channel declined is still delivered passively"
    );
    assert!(
        rig.ports.take_active_inputs().is_empty(),
        "the passive route never rides the active channel"
    );

    // The passive route's own guard ladder is untouched by the active channel: a short operand is
    // still the C's `[0]einval` (`inet_drv.c:14988`), whatever `{active, …}` says.
    let short = recv_operand(INET_INFINITY, 0)[..4].to_vec();
    assert_eq!(
        rig.op(id, PACKET_REQ_RECV, &short),
        ctl_xerror("einval"),
        "the operand must be the eight-byte Timeout(4), Length(4) record"
    );
    eprintln!(
        "case 2: quiet rounds + 1 passive completion, {} round(s)",
        rig.rounds
    );
}

// ---------------------------------------------------------------------------
// 3. UDP `{active, once}` — one delivery, then a silent flip
// ---------------------------------------------------------------------------

/// `{active, once}` grants exactly one delivery (`INET_CHECK_ACTIVE_TO_PASSIVE`,
/// `inet_drv.c:1552`) and flips to passive **silently** — no `{udp_passive, …}`, and a second
/// datagram injected afterwards produces nothing at all.
#[test]
fn an_active_once_udp_port_delivers_one_datagram_and_no_passive_event() {
    let mut rig = Rig::new();
    let (id, bound) = rig.udp_bound();
    rig.arm(id, INET_ONCE);

    let _first = rig.inject(bound, b"one");
    let inputs = rig.await_active(1);
    assert_eq!(inputs.len(), 1, "exactly one delivery: {inputs:?}");
    assert_eq!(inputs[0].port, id, "the delivery names the port");
    assert_eq!(passive_count(&inputs), 0, "{{active,once}} never reports a passive message");
    let read = data_of(&inputs)[0];
    assert_eq!(read.body.as_deref(), Some(&b"one"[..]), "the first datagram");

    // The second datagram finds the port passive: it stays in the kernel for a later passive read
    // and is never reported on the active channel.
    let _second = rig.inject(bound, b"two");
    rig.assert_quiet(QUIET_BUDGET, "a spent {{active,once}} grant delivers nothing more");
    eprintln!("case 3: 1 delivery then quiet, {} round(s)", rig.rounds);
}

// ---------------------------------------------------------------------------
// 4. UDP `{active, 2}` — two deliveries then exactly one passive event
// ---------------------------------------------------------------------------

/// `{active, 2}` decrements per delivery and, on exactly zero, flips to passive **and** reports one
/// `Passive` after the datagram that spent the last credit
/// (`packet_passive_message`, `inet_drv.c:4215`). A third datagram keeps waiting: the burst stops
/// the moment the mode flips (`:15566`).
#[test]
fn an_active_n_udp_port_delivers_n_datagrams_then_one_passive_event() {
    let mut rig = Rig::new();
    let (id, bound) = rig.udp_bound();
    rig.arm_multi(id, 2);

    let _a = rig.inject(bound, b"first");
    let _b = rig.inject(bound, b"second");
    let _c = rig.inject(bound, b"third");

    // Three events have to arrive: two data deliveries and the one passive message.
    let inputs = rig.await_active(3);
    assert_eq!(inputs.len(), 3, "two data deliveries and one passive: {inputs:?}");
    assert!(
        inputs.iter().all(|input| input.port == id),
        "every event names the delivering port: {inputs:?}"
    );
    let bodies = data_of(&inputs)
        .iter()
        .map(|read| read.body.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        bodies,
        vec![Some(b"first".to_vec()), Some(b"second".to_vec())],
        "the first two datagrams are the two credits, in arrival order"
    );
    assert_eq!(
        passive_count(&inputs),
        1,
        "exactly one passive message, after the credit that spent the counter"
    );
    assert_eq!(
        inputs[2].event,
        ActiveInputEvent::Passive,
        "the passive message follows the datagram that spent the last credit"
    );

    // Nothing more: the third datagram waits in the kernel for a passive read.
    rig.assert_quiet(QUIET_BUDGET, "the third datagram is not delivered after the flip");
    eprintln!("case 4: 3 events (2 data + 1 passive), {} round(s)", rig.rounds);
}

// ---------------------------------------------------------------------------
// 5. UDP passive → active re-arm delivers what was already buffered
// ---------------------------------------------------------------------------

/// Arming a **passive** port that already has a datagram buffered synthesizes the readiness edge
/// (`inet_set_opts`'s `sock_select(desc, FD_READ, desc->active > 0)`, `inet_drv.c:7944`): the
/// `setopts` re-registers the descriptor, and the next round delivers the datagram that arrived
/// while the port was passive.
#[test]
fn a_passive_to_active_setopts_delivers_the_already_buffered_datagram() {
    let mut rig = Rig::new();
    let (id, bound) = rig.udp_bound();

    let sender = rig.inject(bound, b"buffered");
    let sender_addr = sender.local_addr().expect("the sender's local address");
    rig.assert_quiet(QUIET_BUDGET, "the port is passive, so the datagram is not delivered");

    rig.arm(id, INET_ACTIVE);
    let inputs = rig.await_active(1);
    assert_eq!(inputs.len(), 1, "the buffered datagram is delivered once armed: {inputs:?}");
    assert_eq!(inputs[0].port, id, "the delivery names the port");
    let read = data_of(&inputs)[0];
    let (addr, port) = split_packed(&read.header);
    assert_eq!(port, sender_addr.port(), "the header names the sender's port");
    assert_eq!(addr, [127, 0, 0, 1], "the sender is loopback");
    assert_eq!(
        read.body.as_deref(),
        Some(&b"buffered"[..]),
        "the very datagram that arrived while the port was passive"
    );
    rig.assert_quiet(QUIET_BUDGET, "the delivery happens once, not once per round");
    eprintln!(
        "case 5: quiet rounds, then 1 delivery after arming, {} round(s)",
        rig.rounds
    );
}

// ---------------------------------------------------------------------------
// 6. TCP `{active, true}` — the peer's bytes arrive as one delivery with an empty header
// ---------------------------------------------------------------------------

/// An accepted `tcp_inet` port armed with `{active, true}` delivers the bytes an **external**
/// `std::net::TcpStream` writes as one `Data` whose `header` is empty (the `{header, 0}` default;
/// the C's `tcp_message`, `inet_drv.c:3979`) and whose `body` is the read burst.
#[test]
fn an_active_tcp_port_delivers_the_peer_bytes_with_an_empty_header() {
    let mut rig = Rig::new();
    let (id, mut peer) = rig.tcp_accepted();
    assert_eq!(
        rig.ports.get(id).expect("live").driver_name(),
        DRIVER_NAME,
        "the port runs the built-in tcp_inet driver"
    );
    rig.arm(id, INET_ACTIVE);

    peer.write_all(b"hello").expect("the peer writes");
    peer.flush().expect("the peer flushes");

    let inputs = rig.await_active(1);
    assert_eq!(inputs.len(), 1, "exactly one delivery: {inputs:?}");
    assert_eq!(inputs[0].port, id, "the delivery names the port");
    assert_eq!(passive_count(&inputs), 0, "{{active,true}} never reports a passive message");
    let read = data_of(&inputs)[0];
    assert!(
        read.header.is_empty(),
        "no {{header, N}} option is set, so the split is empty: {:?}",
        read.header
    );
    assert_eq!(
        read.body.as_deref(),
        Some(&b"hello"[..]),
        "the body is the peer's bytes"
    );
    eprintln!("case 6: 1 delivery after {} round(s)", rig.rounds);
}

/// A `{packet, 2}`-framed active stream port delivers the **stripped** body: the two-byte framing
/// header is consumed by `packet_get_body` (`inet_drv.c:4285`) and never reaches the owner.
#[test]
fn a_packet_framed_tcp_port_delivers_the_stripped_body() {
    let mut rig = Rig::new();
    let (id, mut peer) = rig.tcp_accepted();
    assert_eq!(
        rig.op(
            id,
            INET_REQ_SETOPTS,
            &records(&[record(INET_LOPT_PACKET, 2), record(INET_LOPT_ACTIVE, INET_ACTIVE)]),
        ),
        ctl_ok(),
        "{{packet,2}} and {{active,true}} are accepted"
    );

    peer.write_all(&[0, 3, b'a', b'b', b'c'])
        .expect("the peer writes a framed message");
    peer.flush().expect("the peer flushes");

    let inputs = rig.await_active(1);
    assert_eq!(inputs.len(), 1, "one frame is one delivery: {inputs:?}");
    let read = data_of(&inputs)[0];
    assert!(read.header.is_empty(), "the split is the {{header, 0}} default");
    assert_eq!(
        read.body.as_deref(),
        Some(&b"abc"[..]),
        "the {{{{packet,2}}}} framing header is stripped before delivery"
    );
    eprintln!("case 6b: 1 framed delivery after {} round(s)", rig.rounds);
}

// ---------------------------------------------------------------------------
// 7. Two armed ports, one round: port-number order, drained exactly once
// ---------------------------------------------------------------------------

/// The drain is deterministic across ports: one round that delivers on two armed datagram ports
/// reports them in **port-number order** ([`PortRegistry::take_active_inputs`] walks the live map
/// sorted by port number) and is destructive — a second call is empty.
#[test]
fn two_active_datagram_ports_are_reported_in_port_number_order_once() {
    let mut rig = Rig::new();
    let (first, first_bound) = rig.udp_bound();
    let (second, second_bound) = rig.udp_bound();
    assert!(
        first.number() < second.number(),
        "port numbers increase with allocation: {first:?} < {second:?}"
    );
    rig.arm(first, INET_ACTIVE);
    rig.arm(second, INET_ACTIVE);

    let _a = rig.inject(first_bound, b"to-first");
    let _b = rig.inject(second_bound, b"to-second");

    // Hold one **single** round's drain: poll until the reactor has reported every armed port
    // ready (both datagrams were queued before the first poll, so this is normally the first
    // one), dispatch that batch once, and read the channel once.
    let keys = [first.number(), second.number()];
    let events = poll_until_keys(&mut rig.poller, &keys);
    rig.ports
        .handle_events(&mut rig.poller, &events)
        .expect("the ready events are dispatched");
    rig.ports
        .execute_port_tasks(&mut rig.poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the port tasks run");
    let inputs = rig.ports.take_active_inputs();

    assert_eq!(inputs.len(), 2, "one delivery per armed port: {inputs:?}");
    assert_eq!(
        inputs.iter().map(|input| input.port).collect::<Vec<_>>(),
        vec![first, second],
        "the drain reports the ports in port-number order"
    );
    let bodies = data_of(&inputs)
        .iter()
        .map(|read| read.body.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        bodies,
        vec![Some(b"to-first".to_vec()), Some(b"to-second".to_vec())],
        "each delivery carries its own port's datagram"
    );
    assert!(
        rig.ports.take_active_inputs().is_empty(),
        "the drain is destructive: a second call reports nothing"
    );
    eprintln!("case 7: 2 deliveries in one drain after {} poll round(s)", rig.rounds + 1);
}

/// Polls until the reactor has reported an event for **every** one of `keys`, accumulating the
/// batches into one event list (de-duplicated per key) so a single `handle_events` dispatch sees
/// them all. Bounded by [`WAIT`].
fn poll_until_keys(poller: &mut Poller, keys: &[u64]) -> Vec<Event> {
    let deadline = Instant::now() + WAIT;
    let mut accumulated: Vec<Event> = Vec::new();
    while !keys.iter().all(|key| accumulated.iter().any(|event| event.key() == *key)) {
        assert!(
            Instant::now() < deadline,
            "the reactor never reported every port ready: want {keys:?}, saw {:?}",
            accumulated.iter().map(|event| event.key()).collect::<Vec<_>>()
        );
        let batch = match poller.poll(Some(POLL_SLICE)) {
            Ok(batch) => batch,
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => panic!("poll failed: {err:?}"),
        };
        for event in batch {
            if !accumulated.iter().any(|seen| seen.key() == event.key()) {
                accumulated.push(event);
            }
        }
    }
    accumulated
}

// ---------------------------------------------------------------------------
// 8. The channel is empty when it should be — passive, and after a close
// ---------------------------------------------------------------------------

/// The channel never invents a delivery: a fully **passive** port reports nothing however the
/// reactor is pumped, and closing an **armed** port leaves nothing behind — no stale delivery on
/// the close-harvest queue, no panic, and the port number is retired.
#[test]
fn the_active_channel_is_empty_for_a_passive_port_and_after_a_close() {
    let mut rig = Rig::new();

    // A fresh datagram port is passive by default: an arriving datagram is not delivered.
    let (passive, passive_bound) = rig.udp_bound();
    assert!(rig.ports.contains(passive), "the passive port is live");
    let _kept = rig.inject(passive_bound, b"unwanted");
    rig.assert_quiet(QUIET_BUDGET, "a default (passive) port delivers nothing");
    assert!(
        rig.ports.take_active_inputs().is_empty(),
        "a passive port leaves the channel empty"
    );

    // An armed port that really did deliver, then closes: the drain after the close is empty, and
    // so is every round after it.
    let (active, active_bound) = rig.udp_bound();
    rig.arm(active, INET_ACTIVE);
    let _sender = rig.inject(active_bound, b"delivered");
    let inputs = rig.await_active(1);
    assert_eq!(inputs.len(), 1, "the armed port delivered once: {inputs:?}");
    assert_eq!(inputs[0].port, active, "the delivery names the armed port");

    rig.ports
        .close(&mut rig.poller, active)
        .expect("the armed port closes");
    assert!(!rig.ports.contains(active), "the closed port is gone");
    assert!(
        rig.ports.take_active_inputs().is_empty(),
        "closing an armed port leaves no delivery behind"
    );
    rig.assert_quiet(QUIET_BUDGET, "a closed port never delivers again");
    assert!(
        rig.ports.take_active_inputs().is_empty(),
        "and the close-harvest queue stays empty"
    );
    eprintln!("case 8: quiet + 1 delivery + quiet after close, {} round(s)", rig.rounds);
}
