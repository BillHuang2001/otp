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

//! The `tcp_inet` driver's **`{active, …}` delivery** suite — the C's `tcp_deliver`/`tcp_recv`
//! machine ([`super::active`]).
//!
//! Every case drives a **real** loopback connection through the runtime's own
//! `erlang:port_control/3` path ([`crate::PortRegistry::port_control`]), with a plain
//! [`std::net::TcpStream`] as the peer: the port is `open`ed, `bind`ed, `listen`ed and then
//! **transferred** to the accepted connection by the named `accept` verb
//! ([`super::TcpInetDriver::verb_accept`]), which is the only synchronous route to a connected
//! `tcp_inet` port in this crate.
//!
//! Each round is the runtime's own loop — [`crate::Poller::poll`], then
//! [`crate::PortRegistry::handle_events`], then [`crate::PortRegistry::take_active_inputs`] — and
//! every wait is a **condition** wait under a hang guard ([`GUARD`]), never a fixed budget tuned
//! for an idle host.
//!
//! The `erl`-oracle cases at the bottom run the same scenario against the system
//! `tcp_inet` driver (OTP 29.1.1 / erts-17.1) and pin its exact dump; they skip **loudly** when
//! `erl` is absent or exceeds its budget, never fail.
use super::opcode_tests::{erl_stdout, loopback_arg, open_arg, op};
use super::wire::ctl_ok;
use super::TcpInetDriver;
use crate::completion::{CompletionEvent, InetAsyncResult};
use crate::poller::Poller;
use crate::port::PortRegistry;
use crate::port_task::{ActiveInput, ActiveInputEvent, ReadMessage};
use beam_process::PortId;
use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// The `{active, …}` option code (`INET_LOPT_ACTIVE`, `inet_drv.c:897`).
const ACTIVE: u8 = 22;
/// The `{header, N}` option code (`INET_LOPT_HEADER`, `inet_drv.c:886`).
const HEADER: u8 = 21;
/// The `{packet, …}` option code (`INET_LOPT_PACKET`, `inet_drv.c:898`).
const PACKET: u8 = 23;
/// The `{mode, …}` option code (`INET_LOPT_MODE`, `inet_drv.c:899`).
const MODE: u8 = 24;
/// `INET_REQ_OPEN` (`inet_drv.c:11961`).
const OPEN: u32 = 1;
/// `INET_REQ_BIND` (`inet_drv.c:11409`).
const BIND: u32 = 6;
/// `INET_REQ_SETOPTS` (`inet_drv.c:11127`).
const SETOPTS: u32 = 7;
/// `INET_REQ_GETSTAT` (`inet_drv.c:11523`).
const GETSTAT: u32 = 11;
/// `INET_REQ_LISTEN` (`inet_drv.c:12042`).
const LISTEN: u32 = 27;
/// `TCP_REQ_RECV` (`inet_drv.c:12268`) — the asynchronous receive.
const TCP_REQ_RECV: u32 = 42;
/// `INET_MODE_BINARY` (`inet_drv.c:779`); `INET_MODE_LIST` is `0`.
const MODE_BINARY: i32 = 1;

/// How long a condition wait may take before the test is declared hung.
const GUARD: Duration = Duration::from_secs(5);
/// One poll round's wait: the readiness is normally already there, so this only bounds a miss.
const POLL_WAIT: Duration = Duration::from_millis(50);

/// One five-byte `[opt, val_be32]` `setopts` record ([`super::opt`]'s `RECORD`).
fn record(code: u8, value: i32) -> Vec<u8> {
    let mut bytes = vec![code];
    bytes.extend_from_slice(&value.to_be_bytes());
    bytes
}

/// `{active, true}` (`INET_ACTIVE`, `inet_drv.c:781`).
fn active_true() -> Vec<u8> {
    record(ACTIVE, 1)
}

/// `{active, false}` (`INET_PASSIVE`).
fn active_false() -> Vec<u8> {
    record(ACTIVE, 0)
}

/// `{active, once}` (`INET_ONCE`, `inet_drv.c:781`).
fn active_once() -> Vec<u8> {
    record(ACTIVE, 2)
}

/// `{active, N}` (`INET_MULTI`) plus the 16-bit counter that follows its record
/// (`inet_drv.c:6977`).
fn active_multi(n: i16) -> Vec<u8> {
    let mut bytes = record(ACTIVE, 3);
    bytes.extend_from_slice(&n.to_be_bytes());
    bytes
}

/// `{packet, N}`.
fn packet(n: i32) -> Vec<u8> {
    record(PACKET, n)
}

/// `{header, N}`.
fn header(n: i32) -> Vec<u8> {
    record(HEADER, n)
}

/// `{mode, …}`.
fn mode(m: i32) -> Vec<u8> {
    record(MODE, m)
}

/// Concatenates option records into one `setopts` payload.
fn records(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flatten().copied().collect()
}

/// A live in-crate `tcp_inet` port plus the poller the runtime drives it with.
struct Rig {
    registry: PortRegistry,
    poller: Poller,
    id: PortId,
}

impl Rig {
    /// A bare (socket-less) port — the state the driver factory starts in.
    fn bare() -> Rig {
        let mut poller = Poller::new().expect("a poll set");
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(TcpInetDriver::new()))
            .expect("the port opens");
        Rig {
            registry,
            poller,
            id,
        }
    }

    /// One numeric control call, the way a live `beam_rt` forwards it.
    fn numbered(&mut self, code: u32, arg: &[u8]) -> Vec<u8> {
        self.registry
            .port_control(&mut self.poller, self.id, &op(code), arg)
            .expect("the control call answers")
    }

    /// One **named** control call ([`super::TcpInetDriver::control`]'s name match).
    fn named(&mut self, verb: &str, arg: &[u8]) -> Vec<u8> {
        self.registry
            .port_control(&mut self.poller, self.id, verb, arg)
            .expect("the named control call answers")
    }

    /// A `setopts` request over the given option records.
    fn setopts(&mut self, parts: &[Vec<u8>]) -> Vec<u8> {
        self.numbered(SETOPTS, &records(parts))
    }

    /// One round of the runtime's loop: poll, handle the readiness, drain the active channel.
    fn round(&mut self) -> Vec<ActiveInput> {
        let events = self.poller.poll(Some(POLL_WAIT)).expect("the poll runs");
        self.registry
            .handle_events(&mut self.poller, &events)
            .expect("the round runs");
        self.registry.take_active_inputs()
    }

    /// Rounds until `want` deliveries have been drained (or [`GUARD`] expires).
    fn until(&mut self, want: usize) -> Vec<ActiveInput> {
        let deadline = Instant::now() + GUARD;
        let mut got = Vec::new();
        while got.len() < want && Instant::now() < deadline {
            got.extend(self.round());
        }
        got
    }

    /// `rounds` further rounds, collected — for the "nothing more arrives" assertions.
    fn quiet(&mut self, rounds: usize) -> Vec<ActiveInput> {
        let mut got = Vec::new();
        for _ in 0..rounds {
            got.extend(self.round());
        }
        got
    }

    /// **One** readiness round, taken as soon as the poller reports an event for this port.
    ///
    /// Nothing is drained before it — the poll that observes the edge and the `handle_events`
    /// that runs the round are one step — so everything the round delivers had arrived by the
    /// time it ran. That is what makes a "two arrivals, one drain" assertion possible without a
    /// fixed sleep: the wait is the readiness **condition** under [`GUARD`].
    fn round_when_ready(&mut self) -> Vec<ActiveInput> {
        let deadline = Instant::now() + GUARD;
        loop {
            let events = self.poller.poll(Some(POLL_WAIT)).expect("the poll runs");
            if !events.is_empty() {
                self.registry
                    .handle_events(&mut self.poller, &events)
                    .expect("the round runs");
                return self.registry.take_active_inputs();
            }
            assert!(
                Instant::now() < deadline,
                "the socket never reported readiness"
            );
        }
    }

    /// How many bytes sit in the port's **input queue** (the passive channel).
    fn input_len(&mut self) -> usize {
        self.registry.get(self.id).expect("a live port").input_len()
    }

    /// Drains `len` bytes out of the port's input queue.
    fn take_input(&mut self, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let port = self.registry.get_mut(self.id).expect("a live port");
        let read = port.read_input(&mut buf);
        buf.truncate(read);
        buf
    }

    /// The driver's `recv_oct`/`send_oct` counters (`INET_REQ_GETSTAT` ops 9 and 10, which
    /// [`super::wire::getstat_reply`] renders as 9-byte `[op, u64_be]` records).
    fn octets(&mut self) -> (u64, u64) {
        let reply = self.numbered(GETSTAT, &[9, 10]);
        assert_eq!(reply.first(), Some(&super::INET_REP_OK), "GETSTAT accepted");
        assert_eq!(reply.len(), 19, "[1] ++ [9,u64] ++ [10,u64]");
        assert_eq!((reply[1], reply[10]), (9, 10), "the requested op codes");
        let recv = u64::from_be_bytes(reply[2..10].try_into().expect("eight octets"));
        let send = u64::from_be_bytes(reply[11..19].try_into().expect("eight octets"));
        (recv, send)
    }
}

/// A `tcp_inet` port **transferred** to an accepted loopback connection, plus the peer's end.
struct Link {
    port: Rig,
    peer: TcpStream,
}

/// Builds the link: `open` → `bind` → `listen` on an ephemeral loopback port, a peer connects,
/// and the named `accept` verb transfers the port to the accepted connection (the C's
/// `INET_REQ_ACCEPT` would create a *second* port; this floor's named verb is the synchronous
/// route, see [`super::TcpInetDriver::verb_accept`]).
fn link() -> Link {
    let mut port = Rig::bare();
    assert_eq!(port.numbered(OPEN, &open_arg()), ctl_ok(), "INET_REQ_OPEN");
    let bound = port.numbered(BIND, &loopback_arg(0));
    assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
    let bound_port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_eq!(port.numbered(LISTEN, &[0, 5]), ctl_ok(), "INET_REQ_LISTEN");

    let peer = TcpStream::connect(("127.0.0.1", bound_port)).expect("the peer connects");
    peer.set_nodelay(true).expect("nodelay is a socket option");

    // `connect(2)` completed, so the connection already sits in the accept queue; retry anyway so
    // a loaded host cannot turn a scheduling delay into a failure.
    let deadline = Instant::now() + GUARD;
    loop {
        let reply = port.named("accept", &[]);
        if reply.len() == 5 {
            assert_eq!(reply[0], super::INET_REP_OK, "the accept succeeded");
            break;
        }
        assert!(Instant::now() < deadline, "the listener never accepted the peer");
        std::thread::sleep(Duration::from_millis(5));
    }
    Link { port, peer }
}

/// Writes `bytes` on the peer's end — the bytes the port's socket then holds.
fn send(peer: &mut TcpStream, bytes: &[u8]) {
    peer.write_all(bytes).expect("the peer writes");
    peer.flush().expect("the peer flushes");
}

/// The `Data` payloads of the collected deliveries, in order.
fn payloads(inputs: &[ActiveInput]) -> Vec<ReadMessage> {
    inputs
        .iter()
        .map(|input| match &input.event {
            ActiveInputEvent::Data(read) => read.clone(),
            ActiveInputEvent::Passive => panic!("a passive event where data was expected"),
        })
        .collect()
}

/// How many of the collected deliveries are `Passive` events.
fn passive_count(inputs: &[ActiveInput]) -> usize {
    inputs
        .iter()
        .filter(|input| matches!(input.event, ActiveInputEvent::Passive))
        .count()
}

// -----------------------------------------------------------------------
// The delivery path
// -----------------------------------------------------------------------

/// `{active, true}`: one read burst is one `{tcp, S, Data}`, with an empty `{header, N}` split and
/// `recv_oct` advanced by the delivered bytes — under `{packet, raw}` that **is** the counted
/// length, since `packet_get_length` over a `TCP_PB_RAW` buffer answers the whole buffer
/// (`inet_input_count`, `inet_drv.c:13064`).
#[test]
fn active_true_delivers_one_data_message_and_counts_the_bytes() {
    let Link { mut port, mut peer } = link();
    assert_eq!(port.octets(), (0, 0), "a fresh port has counted nothing");

    assert_eq!(port.setopts(&[active_true()]), ctl_ok());
    send(&mut peer, b"abc");

    let delivered = port.until(1);
    assert_eq!(delivered.len(), 1, "exactly one `{{tcp, S, Data}}`");
    assert_eq!(delivered[0].port, port.id, "stamped with the port's own id");
    let read = &payloads(&delivered)[0];
    assert!(read.header.is_empty(), "no `{{header, N}}` option is set");
    assert_eq!(read.body.as_deref(), Some(&b"abc"[..]), "the whole burst");
    assert_eq!(port.octets().0, 3, "`recv_oct` counts the delivered bytes");
    assert_eq!(port.input_len(), 0, "an active port never fills the input queue");
}

/// `{packet, 2}`: the framing header is **stripped before delivery**
/// (`packet_get_body`, called from `tcp_reply_data`, `inet_drv.c:4285`), so the body is `abc`.
///
/// The counters credit the **whole frame** even though only the payload is delivered:
/// `tcp_deliver` charges `inet_input_count(INETP(desc), len)` (`:13064`) with `len` from
/// `tcp_remain(desc, &len)` (`:12967`/`:13052`), i.e. `packet_get_length`'s total packet length —
/// the two-byte header included. Measured on the oracle for one `<<0,3,"abc">>` frame under
/// `{packet,2},{active,true}`: `recv_max = 5`, `recv_oct = 5`.
#[test]
fn a_packet_framed_delivery_strips_the_framing_header() {
    let Link { mut port, mut peer } = link();
    assert_eq!(port.octets(), (0, 0), "a fresh port has counted nothing");

    assert_eq!(port.setopts(&[packet(2), active_true()]), ctl_ok());
    send(&mut peer, &[0, 3, b'a', b'b', b'c']);

    let delivered = port.until(1);
    assert_eq!(delivered.len(), 1, "one whole frame is one message");
    let read = &payloads(&delivered)[0];
    assert!(read.header.is_empty(), "the framing header is not a report header");
    assert_eq!(read.body.as_deref(), Some(&b"abc"[..]), "the 2-byte header is gone");
    assert_eq!(
        port.octets().0,
        5,
        "recv_oct counts the whole frame: three payload octets plus the two-byte header"
    );
    // `INET_STAT_RECV_MAX` (op 2) is the largest counted length, so it is 5 and not 3.
    let max = port.numbered(GETSTAT, &[2]);
    assert_eq!(max.len(), 6, "[1] ++ [2, u32_be]");
    assert_eq!(
        u32::from_be_bytes(max[2..6].try_into().expect("four octets")),
        5,
        "recv_max is the whole frame's length"
    );
}

/// `{header, 2}`: the read's `header` is the `{header, N}` option (`desc->hsz`,
/// `inet_drv.c:6931`) split off the front of the payload, and the body is the remainder
/// (`tcp_message`, `:3979`).
#[test]
fn the_header_option_splits_the_payload() {
    let Link { mut port, mut peer } = link();

    assert_eq!(port.setopts(&[header(2), active_true()]), ctl_ok());
    send(&mut peer, b"abcdef");

    let delivered = port.until(1);
    assert_eq!(delivered.len(), 1);
    let read = &payloads(&delivered)[0];
    assert_eq!(read.header, b"ab".to_vec(), "the first hsz bytes");
    assert_eq!(read.body.as_deref(), Some(&b"cdef"[..]), "the remainder");
}

/// `{active, once}`: exactly one delivery, then the socket is passive **silently** — no
/// `{tcp_passive, S}` ever (the C's `INET_CHECK_ACTIVE_TO_PASSIVE`, `inet_drv.c:1552`).
#[test]
fn active_once_delivers_exactly_one_message_and_never_a_passive_one() {
    let Link { mut port, mut peer } = link();

    assert_eq!(port.setopts(&[active_once()]), ctl_ok());
    send(&mut peer, b"abc");
    let delivered = port.until(1);
    assert_eq!(delivered.len(), 1);
    assert_eq!(payloads(&delivered)[0].body.as_deref(), Some(&b"abc"[..]));

    // The grant is spent: a second write must not reach the active channel, and — unlike
    // `{active, N}` — nothing announces the flip.
    send(&mut peer, b"def");
    let extra = port.quiet(3);
    assert!(extra.is_empty(), "a spent grant delivers nothing: {extra:?}");
    assert_eq!(passive_count(&extra), 0, "`{{active, once}}` never sends a passive event");
}

/// `{active, N}`: N deliveries, then **one** `Passive` event, then silence
/// (`packet_passive_message`, `inet_drv.c:4215`, reached from `:1552`).
#[test]
fn active_n_delivers_n_messages_then_one_passive_event() {
    let Link { mut port, mut peer } = link();

    // Framed writes: two `{packet, 2}` frames are two messages even when both are already in the
    // socket buffer, so the count cannot depend on how the host coalesced the reads.
    assert_eq!(port.setopts(&[packet(2), active_multi(2)]), ctl_ok());
    send(&mut peer, &[0, 1, b'a']);
    send(&mut peer, &[0, 1, b'b']);

    let delivered = port.until(3);
    assert_eq!(delivered.len(), 3, "two data messages and one passive event");
    assert_eq!(payloads(&delivered[..2])[0].body.as_deref(), Some(&b"a"[..]));
    assert_eq!(payloads(&delivered[..2])[1].body.as_deref(), Some(&b"b"[..]));
    assert!(
        matches!(delivered[2].event, ActiveInputEvent::Passive),
        "the passive event follows the last data message"
    );

    send(&mut peer, &[0, 1, b'c']);
    let extra = port.quiet(3);
    assert!(extra.is_empty(), "the counter is spent: {extra:?}");
}

/// Two whole frames that are **already in the socket** when the readiness edge is reported are
/// both delivered by that one round, in arrival order, into a single drain.
///
/// This is `tcp_deliver`'s loop (`inet_drv.c:13042`-`:13127`, `while (len > 0)` at `:13061`): as
/// long as a complete packet sits in the driver's buffer it is delivered, and the loop only reads
/// again when the buffer falls short — so a callback serves **every** buffered frame, not one per
/// callback. `{active, true}` never
/// spends its grant, so the burst is the only thing bounding the round.
#[test]
fn two_frames_buffered_before_a_round_are_both_delivered_by_it_in_arrival_order() {
    let Link { mut port, mut peer } = link();

    assert_eq!(port.setopts(&[packet(2), active_true()]), ctl_ok());

    // Two whole frames in **one** write. On loopback the write's bytes are in this socket's
    // receive queue by the time `write_all` returns, so the first round that runs sees both.
    send(&mut peer, &[0, 1, b'a', 0, 1, b'b']);

    let delivered = port.round_when_ready();
    assert_eq!(
        delivered.len(),
        2,
        "one round delivers every buffered frame: {delivered:?}"
    );
    assert_eq!(passive_count(&delivered), 0, "`{{active,true}}` never spends its grant");
    assert!(
        delivered.iter().all(|input| input.port == port.id),
        "each delivery names the delivering port"
    );
    let reads = payloads(&delivered);
    assert_eq!(reads[0].body.as_deref(), Some(&b"a"[..]), "the first frame, first");
    assert_eq!(reads[1].body.as_deref(), Some(&b"b"[..]), "the second frame, second");
    assert!(
        reads.iter().all(|read| read.header.is_empty()),
        "a framing header is not a `{{header, N}}` report header"
    );
    assert_eq!(port.input_len(), 0, "an active port never fills the input queue");
}

/// A grant spent mid-burst **ends** the burst there (`if (! desc->inet.active)`, `inet_drv.c:13107`):
/// with `{active, 1}` and two frames already buffered, exactly one message is delivered, nothing
/// announces the flip (unlike `{active, N}`), and the second frame is still there — the next
/// passive `recv` takes it off the driver's buffer, so it was never consumed, only held.
#[test]
fn a_spent_grant_ends_the_burst_and_leaves_the_rest_for_the_passive_path() {
    let Link { mut port, mut peer } = link();

    assert_eq!(port.setopts(&[packet(2), active_multi(1)]), ctl_ok());
    send(&mut peer, &[0, 1, b'a', 0, 1, b'b']);

    let delivered = port.until(2);
    assert_eq!(delivered.len(), 2, "one data message and one passive event");
    assert_eq!(payloads(&delivered[..1])[0].body.as_deref(), Some(&b"a"[..]));
    assert!(
        matches!(delivered[1].event, ActiveInputEvent::Passive),
        "the credit was spent on the first frame and reported after it"
    );
    // The burst stopped with the second frame still buffered: no further delivery, no second
    // passive event.
    let extra = port.quiet(3);
    assert!(extra.is_empty(), "the burst ended at the flip: {extra:?}");

    // ... and the frame is not lost — the passive `recv` verb decodes it out of the driver's own
    // buffer (`pending_input`) and hands the payload to the port input queue.
    assert_eq!(port.named("recv", &[]), ctl_ok(), "the passive recv succeeds");
    assert_eq!(port.take_input(1), b"b".to_vec(), "the frame the burst left behind");
}

/// `{active, false}`: the bytes take the **passive** path into the port input queue, so the
/// active channel stays empty and the existing `recv` route is unchanged.
#[test]
fn a_passive_port_reports_nothing_on_the_active_channel() {
    let Link { mut port, mut peer } = link();

    assert_eq!(port.setopts(&[active_false()]), ctl_ok());
    send(&mut peer, b"abc");

    let deadline = Instant::now() + GUARD;
    while port.input_len() < 3 && Instant::now() < deadline {
        let delivered = port.round();
        assert!(delivered.is_empty(), "a passive port never uses the active channel");
    }
    assert_eq!(port.input_len(), 3, "the passive path still reads the bytes");
    assert_eq!(port.take_input(3), b"abc".to_vec(), "and the owner can recv them");
}

/// A queued `TCP_REQ_RECV` keeps the socket out of the active channel: the C refuses such an
/// operation on an active port (`active || len != 8 ⇒ [0]einval`, `inet_drv.c:12273`), so the state
/// below is only reachable when the option changes **after** the enqueue. There the queued
/// operation stays authoritative — its answer rides the completion channel and the active channel
/// stays silent (`TcpInetDriver::owns_readiness_for_active`).
///
/// This is a **deliberate divergence**: the C splits on `desc->inet.active` alone
/// (`tcp_reply_data`, `:4294`-`:4297`), so it sends the owner a `{tcp, S, Data}` and leaves the
/// operation pending. The measured oracle shape is pinned by the
/// [`the_oracle_lets_the_active_mode_win_over_a_queued_receive`] case below.
#[test]
fn a_queued_receive_owns_the_socket_instead_of_the_active_channel() {
    let Link { mut port, mut peer } = link();

    // `Timeout = INET_INFINITY` (`-1`), `Length = 0` ("whatever is available"), enqueued while the
    // port is still passive.
    let mut operand = (-1i32).to_be_bytes().to_vec();
    operand.extend_from_slice(&0u32.to_be_bytes());
    let reply = port.numbered(TCP_REQ_RECV, &operand);
    assert_eq!(reply.len(), 3, "`[1] ++ <<Id:16/big>>`: {reply:?}");
    assert_eq!(reply[0], super::INET_REP_OK, "the receive was accepted");

    // `{active, true}` after the enqueue: the option walk only arms read readiness (its
    // `trigger_recv` half is `{active, once}`/`{active, N}` only, `inet_drv.c:7950`-`:7958`).
    assert_eq!(port.setopts(&[active_true()]), ctl_ok());
    send(&mut peer, b"abc");

    let deadline = Instant::now() + GUARD;
    let mut completions = Vec::new();
    let mut active = Vec::new();
    while completions.is_empty() && Instant::now() < deadline {
        active.extend(port.round());
        completions = port.registry.take_completions();
    }
    assert_eq!(completions.len(), 1, "the queued receive completed: {completions:?}");
    assert_eq!(completions[0].port, port.id, "stamped with the port's own id");
    let CompletionEvent::InetAsync { id, result } = &completions[0].event else {
        panic!("an `{{inet_async, …}}` completion was expected: {:?}", completions[0]);
    };
    assert_eq!(*id, u16::from_be_bytes([reply[1], reply[2]]), "the id its reply handed out");
    match result {
        InetAsyncResult::Data(read) => {
            assert_eq!(
                read.body.as_deref(),
                Some(&b"abc"[..]),
                "the bytes went to the operation, not to the active channel"
            );
        }
        other => panic!("a data completion was expected: {other:?}"),
    }
    assert!(active.is_empty(), "the active channel stays silent: {active:?}");
    assert_eq!(port.input_len(), 0, "and the port input queue stays empty");
}

/// An `{active, N}` that lands on **zero inside the `setopts` walk** emits its passive event from
/// there, with no state gate and no data involved (`inet_drv.c:6995`-`:6998`).
#[test]
fn active_n_landing_on_zero_inside_setopts_emits_one_passive_event() {
    let Link { mut port, .. } = link();

    // `{active, 1}` arms a one-message grant; `{active, -1}` then drives the counter to 0.
    assert_eq!(port.setopts(&[active_multi(1)]), ctl_ok());
    assert_eq!(port.setopts(&[active_multi(-1)]), ctl_ok());

    // No poll round in between: the event came out of the control call itself.
    let drained = port.registry.take_active_inputs();
    assert_eq!(drained.len(), 1, "exactly one passive event: {drained:?}");
    assert!(matches!(drained[0].event, ActiveInputEvent::Passive));
    assert_eq!(drained[0].port, port.id);

    // The C emits it from the option walk, before any `(stream && connected)` gate, so a port
    // without a socket reports it too.
    let mut bare = Rig::bare();
    assert_eq!(bare.setopts(&[active_multi(1)]), ctl_ok());
    assert_eq!(bare.setopts(&[active_multi(-1)]), ctl_ok());
    let drained = bare.registry.take_active_inputs();
    assert_eq!(drained.len(), 1, "no state gate on the option-walk message");
    assert!(matches!(drained[0].event, ActiveInputEvent::Passive));
}

/// The C's `trigger_recv` (`inet_drv.c:7950`-`:7958`, consumed inline at `:11139`-`:11148`):
/// `{active, once}` on a socket that already holds a byte delivers **inside** the control call.
#[test]
fn active_once_delivers_inside_the_setopts_call() {
    let Link { mut port, mut peer } = link();
    send(&mut peer, b"Q");
    // Let the byte reach this socket's receive queue; nothing reads it yet (no round has run).
    std::thread::sleep(Duration::from_millis(50));

    assert_eq!(port.setopts(&[active_once()]), ctl_ok());

    // No poll round: the delivery must already be staged by the control call itself.
    let drained = port.registry.take_active_inputs();
    assert_eq!(drained.len(), 1, "the inline burst delivered: {drained:?}");
    let read = &payloads(&drained)[0];
    assert_eq!(read.body.as_deref(), Some(&b"Q"[..]));
}

/// The other half of the same tail: `{active, true}` answers `1` there, whose `tcp_deliver` only
/// replays a buffer this floor never pre-fills — so it **arms** and delivers nothing inside the
/// call, and the byte arrives on the next readiness round.
#[test]
fn active_true_arms_without_delivering_inside_the_setopts_call() {
    let Link { mut port, mut peer } = link();
    send(&mut peer, b"Q");
    std::thread::sleep(Duration::from_millis(50));

    assert_eq!(port.setopts(&[active_true()]), ctl_ok());
    let drained = port.registry.take_active_inputs();
    assert!(drained.is_empty(), "`{{active, true}}` delivers nothing inline: {drained:?}");

    let delivered = port.until(1);
    assert_eq!(delivered.len(), 1, "the re-arm produced a fresh readiness edge");
    assert_eq!(payloads(&delivered)[0].body.as_deref(), Some(&b"Q"[..]));
}

/// `{mode, binary}` vs `{mode, list}` is the **runtime's** rendering choice, not the driver's: the
/// identical [`ReadMessage`] is handed up under both.
#[test]
fn the_driver_hands_up_the_same_read_message_in_list_and_binary_mode() {
    fn delivered_under(mode_option: Vec<u8>) -> ReadMessage {
        let Link { mut port, mut peer } = link();
        assert_eq!(port.setopts(&[header(2), mode_option, active_true()]), ctl_ok());
        send(&mut peer, b"abcdef");
        let delivered = port.until(1);
        assert_eq!(delivered.len(), 1);
        payloads(&delivered).remove(0)
    }

    let binary = delivered_under(mode(MODE_BINARY));
    let list = delivered_under(mode(0));
    assert_eq!(binary, list, "the driver's split never depends on the mode");
    assert_eq!(binary.header, b"ab".to_vec());
    assert_eq!(binary.body.as_deref(), Some(&b"cdef"[..]));
}

/// A **listener** has no read to trigger: `{active, …}` on one is accepted, arms nothing and
/// delivers nothing (the C's tail is inside a `(stream && connected)` gate, `inet_drv.c:7945`),
/// and a peer's connection is still visible only through `accept`.
#[test]
fn a_listener_never_delivers_on_the_active_channel() {
    let mut port = Rig::bare();
    assert_eq!(port.numbered(OPEN, &open_arg()), ctl_ok());
    let bound = port.numbered(BIND, &loopback_arg(0));
    let bound_port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_eq!(port.numbered(LISTEN, &[0, 5]), ctl_ok());

    assert_eq!(port.setopts(&[active_true()]), ctl_ok(), "a listener accepts the option");
    let _peer = TcpStream::connect(("127.0.0.1", bound_port)).expect("the peer connects");

    let delivered = port.quiet(3);
    assert!(delivered.is_empty(), "a listener's input is `accept`, not a read: {delivered:?}");
}

/// The **named** `"setopts"` verb runs the very same body as the numeric route: the replies are
/// identical and the inline burst fires on both.
#[test]
fn the_named_and_numeric_setopts_routes_agree_and_both_deliver() {
    // Replies first, over a matrix that includes the C's whole-reply `[0]einval`.
    let mut port = Rig::bare();
    for payload in [
        records(&[active_true()]),
        records(&[packet(2), header(4)]),
        records(&[record(0x8f, 1)]), // an unserved code: the C's `default: return -1`
        records(&[active_multi(1)]),
    ] {
        assert_eq!(
            port.numbered(SETOPTS, &payload),
            port.named("setopts", &payload),
            "the two wire forms answer identically for {payload:?}"
        );
    }

    // Then the inline burst, through the named route only.
    let Link { mut port, mut peer } = link();
    send(&mut peer, b"Q");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(port.named("setopts", &records(&[active_once()])), ctl_ok());
    let drained = port.registry.take_active_inputs();
    assert_eq!(drained.len(), 1, "the named route triggers the inline read too");
    assert_eq!(payloads(&drained)[0].body.as_deref(), Some(&b"Q"[..]));
}

// -----------------------------------------------------------------------
// The system-`erl` oracle (OTP 29.1.1 / erts-17.1)
//
// Each expression drives a real `gen_tcp:listen`/`accept` pair and prints one dump line; the
// assertion is the exact line the system driver produced. The Rust-side equivalent of every one
// is pinned by the production tests above, so a change on either side shows up as a failure.
// -----------------------------------------------------------------------

/// `{packet, 2}` ⇒ `{tcp, S, <<"abc">>}`: the framing header is stripped before delivery.
#[test]
fn the_oracle_strips_the_framing_header_before_delivery() {
    let expr = concat!(
        "{ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{ip,{127,0,0,1}}]), ",
        "{ok,P} = inet:port(L), ",
        "{ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{nodelay,true}]), ",
        "{ok,S} = gen_tcp:accept(L), ",
        "ok = inet:setopts(S,[{packet,2},{active,true}]), ",
        "ok = gen_tcp:send(C, <<0,3,\"abc\">>), ",
        "D = receive {tcp,_,B} -> B after 2000 -> timeout end, ",
        "io:format(\"D=~p~n\",[D])"
    );
    let Some(out) = erl_stdout(expr) else { return };
    assert_eq!(out.trim(), "D=<<\"abc\">>", "the oracle's `{{packet,2}}` delivery");
}

/// `{packet, raw}, {header, 2}` on `abcdef`: `[97,98|<<"cdef">>]` in `[binary]` mode and
/// `"abcdef"` in `[list]` mode — the same payload, the runtime's rendering.
#[test]
fn the_oracle_reports_the_header_split_in_both_modes() {
    let expr = concat!(
        "{ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{ip,{127,0,0,1}}]), ",
        "{ok,P} = inet:port(L), ",
        "{ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{nodelay,true}]), ",
        "{ok,S} = gen_tcp:accept(L), ",
        "ok = inet:setopts(S,[{packet,raw},{header,2},{mode,binary},{active,true}]), ",
        "ok = gen_tcp:send(C,\"abcdef\"), ",
        "{tcp,_,D1} = receive M1 -> M1 after 2000 -> timeout end, ",
        "ok = inet:setopts(S,[{mode,list}]), ",
        "ok = gen_tcp:send(C,\"abcdef\"), ",
        "{tcp,_,D2} = receive M2 -> M2 after 2000 -> timeout end, ",
        "io:format(\"B=~p|L=~p~n\",[D1,D2])"
    );
    let Some(out) = erl_stdout(expr) else { return };
    assert_eq!(
        out.trim(),
        "B=[97,98|<<\"cdef\">>]|L=\"abcdef\"",
        "the oracle's `{{header,2}}` split, in both modes"
    );
}

/// `{active, once}` ⇒ one message, `message_queue_len` 0 after it and **no** `{tcp_passive, S}`.
#[test]
fn the_oracle_spends_an_active_once_grant_silently() {
    let expr = concat!(
        "{ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{ip,{127,0,0,1}}]), ",
        "{ok,P} = inet:port(L), ",
        "{ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{nodelay,true}]), ",
        "{ok,S} = gen_tcp:accept(L), ",
        "ok = inet:setopts(S,[{packet,raw},{active,once}]), ",
        "ok = gen_tcp:send(C,\"abc\"), ",
        "{tcp,_,D} = receive M -> M after 2000 -> timeout end, ",
        "ok = gen_tcp:send(C,\"def\"), ",
        "Extra = receive M2 -> M2 after 300 -> none end, ",
        "{message_queue_len,N} = process_info(self(), message_queue_len), ",
        "io:format(\"D=~p|Extra=~p|N=~p~n\",[D,Extra,N])"
    );
    let Some(out) = erl_stdout(expr) else { return };
    assert_eq!(
        out.trim(),
        "D=<<\"abc\">>|Extra=none|N=0",
        "one delivery, no passive message, an empty queue"
    );
}

/// `{active, 2}` ⇒ two messages, then `{tcp_passive, S}`, then nothing.
#[test]
fn the_oracle_delivers_n_messages_then_one_passive_message() {
    let expr = concat!(
        "{ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{ip,{127,0,0,1}}]), ",
        "{ok,P} = inet:port(L), ",
        "{ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{nodelay,true}]), ",
        "{ok,S} = gen_tcp:accept(L), ",
        "ok = inet:setopts(S,[{packet,2},{active,2}]), ",
        "Get = fun() -> receive {tcp,_,B} -> {tcp,B}; {tcp_passive,_} -> passive ",
        "after 500 -> none end end, ",
        "ok = gen_tcp:send(C, <<0,1,97>>), ",
        "M1 = Get(), ",
        "ok = gen_tcp:send(C, <<0,1,98>>), ",
        "M2 = Get(), ",
        "M3 = Get(), ",
        "ok = gen_tcp:send(C, <<0,1,99>>), ",
        "M4 = Get(), ",
        "io:format(\"~p~n\",[[M1,M2,M3,M4]])"
    );
    let Some(out) = erl_stdout(expr) else { return };
    assert_eq!(
        out.trim(),
        "[{tcp,<<\"a\">>},{tcp,<<\"b\">>},passive,none]",
        "two deliveries, one passive event, then silence"
    );
}

/// The passive → active transitions, measured together: `{active, once}` with a byte already
/// waiting delivers **inside** the `setopts` ctl (`message_queue_len` 1 immediately after it),
/// while `{active, true}` only arms (0 immediately, the byte arrives on the next readiness
/// event).
#[test]
fn the_oracle_delivers_a_waiting_byte_inside_the_setopts_call() {
    let once = concat!(
        "{ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{ip,{127,0,0,1}}]), ",
        "{ok,P} = inet:port(L), ",
        "{ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{nodelay,true}]), ",
        "{ok,S} = gen_tcp:accept(L), ",
        "ok = gen_tcp:send(C,\"Q\"), ",
        "timer:sleep(50), ",
        "ok = inet:setopts(S,[{active,once}]), ",
        "{message_queue_len,N} = process_info(self(), message_queue_len), ",
        "D = receive {tcp,_,B} -> B after 1000 -> timeout end, ",
        "io:format(\"N=~p|D=~p~n\",[N,D])"
    );
    let Some(out) = erl_stdout(once) else { return };
    assert_eq!(out.trim(), "N=1|D=<<\"Q\">>", "`{{active,once}}` delivers inline");

    let armed = concat!(
        "{ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{ip,{127,0,0,1}}]), ",
        "{ok,P} = inet:port(L), ",
        "{ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{nodelay,true}]), ",
        "{ok,S} = gen_tcp:accept(L), ",
        "ok = gen_tcp:send(C,\"Q\"), ",
        "timer:sleep(50), ",
        "ok = inet:setopts(S,[{active,true}]), ",
        "{message_queue_len,N} = process_info(self(), message_queue_len), ",
        "D = receive {tcp,_,B} -> B after 2000 -> timeout end, ",
        "io:format(\"N=~p|D=~p~n\",[N,D])"
    );
    let Some(out) = erl_stdout(armed) else { return };
    assert_eq!(out.trim(), "N=0|D=<<\"Q\">>", "`{{active,true}}` only arms");
}

/// The oracle's counterpart of
/// [`a_queued_receive_owns_the_socket_instead_of_the_active_channel`], and the evidence for the
/// **deliberate divergence** recorded there: the C's `tcp_reply_data` splits on
/// `desc->inet.active` alone (`:4294`-`:4297`), so a `TCP_REQ_RECV` queued *before* the port went
/// active is not consulted — the owner gets the `{tcp, S, Data}` and the waiting
/// `gen_tcp:recv/3` runs into its own `{{error,timeout}}`.
#[test]
fn the_oracle_lets_the_active_mode_win_over_a_queued_receive() {
    let expr = concat!(
        "{ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},{ip,{127,0,0,1}}]), ",
        "{ok,P} = inet:port(L), ",
        "{ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{nodelay,true}]), ",
        "{ok,S} = gen_tcp:accept(L), ",
        "Self = self(), ",
        "spawn(fun() -> Self ! {recv, gen_tcp:recv(S, 0, 1000)} end), ",
        "timer:sleep(100), ",
        "ok = inet:setopts(S, [{active,true}]), ",
        "ok = gen_tcp:send(C, \"abc\"), ",
        "M = receive {tcp,_,B} -> {tcp,B}; Other -> {other,Other} after 2000 -> none end, ",
        "R = receive Y -> Y after 1500 -> none end, ",
        "io:format(\"M=~p|R=~p~n\",[M,R])"
    );
    let Some(out) = erl_stdout(expr) else { return };
    assert_eq!(
        out.trim(),
        "M={tcp,<<\"abc\">>}|R={recv,{error,timeout}}",
        "the C delivers actively and leaves the queued receive pending"
    );
}
