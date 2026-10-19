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

//! The **zero-length datagram** pin: an empty UDP datagram is a delivery, not a silence.
//!
//! A `recvfrom(2)` that returns `0` received a *real* datagram with an empty payload, so the C
//! delivers it on every channel it has, and none of them consults a byte count:
//!
//! * the active-mode term path — `packet_reply_binary_data` (`inet_drv.c:4335`) →
//!   `packet_binary_message` (`:4110`), which `LOAD_BINARY`s the payload of size
//!   `len - addrlen`, i.e. `<<>>` — measured: `{udp,S,{127,0,0,1},SrcPort,<<>>}`;
//! * the raw port path (`{deliver,port}`) — `inet_port_binary_data` (`:2765`) with `hsz == 0`
//!   answers `driver_output` over the packed address + payload, i.e. the 7 source-address bytes
//!   and no tail — measured: `{Port,{data,<<1,PortHi,PortLo,127,0,0,1>>}}`;
//! * the passive async path — `inet_async_binary_data` behind the same
//!   `packet_reply_binary_data` passive arm, i.e. `{inet_async, Port, Id, {ok, Addr ++ <<>>}}`.
//!
//! On this floor the first two term-free channels are [`ActiveInputEvent::Data`] and
//! [`InetAsyncResult::Data`], both carrying the packed source address as the read's **header** and
//! the payload as its body — so a zero-length datagram must arrive as
//! `ReadMessage { header: <7 bytes>, body: Some(vec![]) }`: a **present, empty** body, never
//! `body: None` (that is a header-only reply, a different message) and never nothing at all.
//!
//! Every case drives the real runtime path (a [`PortRegistry`](crate::PortRegistry) + [`Poller`]
//! pair over a real `udp_inet` socket, `erlang:port_control/3`'s decimal opcode route, the
//! registry's own `handle_events` round) and injects its datagram from **outside** the port with a
//! plain [`std::net::UdpSocket`], so the asserted delivery is the driver's own.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use super::super::opcode_tests::{erl_stdout, op, Rig};
use super::super::opt::{INET_ACTIVE, INET_LOPT_ACTIVE};
use super::super::wire::{
    ctl_ok, pack_address, INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT, INET_TYPE_DGRAM,
};
use super::UdpInetDriver;
use crate::completion::{CompletionEvent, InetAsyncResult};
use crate::port_task::{ActiveInput, ActiveInputEvent, ReadMessage};

/// `INET_REQ_OPEN` (`inet_drv.c:14577`).
const INET_REQ_OPEN: u32 = 1;
/// `INET_REQ_BIND` (`:11409`).
const INET_REQ_BIND: u32 = 6;
/// `INET_REQ_SETOPTS` (`:11111`).
const INET_REQ_SETOPTS: u32 = 7;
/// `PACKET_REQ_RECV` (`:14968`) — the datagram driver's asynchronous receive.
const PACKET_REQ_RECV: u32 = 60;
/// One round's poll budget. Every wait here is a *condition* under [`HANG_GUARD`], so this only
/// paces the rounds; a starved host cannot fail a case with it.
const ROUND_BUDGET: Duration = Duration::from_millis(100);
/// How many consecutive rounds without a readiness event count as "the port has gone quiet".
const QUIET_ROUNDS: usize = 2;
/// The hang guard every wait is bounded by: a broken driver fails the case instead of hanging it.
const HANG_GUARD: Duration = Duration::from_secs(60);
/// The most rounds a wait will drive before concluding the port is looping.
const MAX_ROUNDS: usize = 200;

/// What one readiness round observed.
struct Round {
    /// How many poller events the round was handed (a readiness edge, or nothing).
    events: usize,
    /// Every `{active, …}` delivery the round produced.
    inputs: Vec<ActiveInput>,
}

/// What a burst of rounds collected.
struct Drained {
    /// Every delivery, in delivery order.
    inputs: Vec<ActiveInput>,
    /// How many rounds observed a readiness event — the non-vacuity witness for a "nothing is
    /// delivered" assertion: the socket really was reported ready and the driver really ran.
    ready_rounds: usize,
}

/// A bound `udp_inet` port over the registry's own rig (see
/// [`Rig`](super::super::opcode_tests::Rig)), plus the loopback port the kernel chose.
fn bound_port() -> (Rig, u16) {
    let mut rig = Rig::new(Box::new(UdpInetDriver::new()));
    assert_eq!(
        rig.control(&op(INET_REQ_OPEN), &[
            INET_AF_INET,
            INET_TYPE_DGRAM,
            INET_PROTO_DEFAULT
        ]),
        ctl_ok(),
        "the datagram socket opens"
    );
    let reply = rig.control(&op(INET_REQ_BIND), &[INET_AF_LOOPBACK, 0, 0]);
    assert_eq!(reply.first(), Some(&1), "the bind succeeds: {reply:?}");
    let bound = u16::from_be_bytes([reply[1], reply[2]]);
    assert_ne!(bound, 0, "the kernel assigned a port");
    (rig, bound)
}

/// Arms `{active, true}` — the one option the active-channel case turns on.
fn arm_active(rig: &mut Rig) {
    let mut record = vec![INET_LOPT_ACTIVE];
    record.extend_from_slice(&INET_ACTIVE.to_be_bytes());
    assert_eq!(
        rig.control(&op(INET_REQ_SETOPTS), &record),
        ctl_ok(),
        "{{active,true}} is accepted"
    );
}

impl Rig {
    /// Runs exactly **one** readiness round: poll, hand the events to the registry (which schedules
    /// *and* runs the round's port tasks), then drain the `{active, …}` deliveries.
    fn active_round(&mut self) -> Round {
        let events = self
            .poller
            .poll(Some(ROUND_BUDGET))
            .expect("the poll set is usable");
        self.registry
            .handle_events(&mut self.poller, &events)
            .expect("the round runs");
        Round {
            events: events.len(),
            inputs: self.registry.take_active_inputs(),
        }
    }

    /// Drives rounds until the port has been quiet for [`QUIET_ROUNDS`] rounds, collecting every
    /// delivery — the condition wait every case here uses instead of a fixed sleep.
    fn drain_active_until_quiet(&mut self) -> Drained {
        let guard = Instant::now() + HANG_GUARD;
        let mut drained = Drained {
            inputs: Vec::new(),
            ready_rounds: 0,
        };
        let mut quiet = 0;
        let mut rounds = 0;
        while quiet < QUIET_ROUNDS {
            assert!(Instant::now() < guard, "the port never went quiet");
            assert!(rounds < MAX_ROUNDS, "the port kept reporting readiness");
            rounds += 1;
            let round = self.active_round();
            if round.events > 0 {
                quiet = 0;
                drained.ready_rounds += 1;
            } else {
                quiet += 1;
            }
            drained.inputs.extend(round.inputs);
        }
        drained
    }

    /// Runs rounds until a completion is staged (or [`HANG_GUARD`] expires), returning every
    /// `{inet_async, …}` event the drain produced — the seam `beam_rt` renders.
    fn drain_async_completions(&mut self) -> Vec<(u16, InetAsyncResult)> {
        let guard = Instant::now() + HANG_GUARD;
        loop {
            assert!(Instant::now() < guard, "no completion was ever produced");
            self.active_round();
            let staged: Vec<(u16, InetAsyncResult)> = self
                .registry
                .take_completions()
                .into_iter()
                .filter_map(|completion| match completion.event {
                    CompletionEvent::InetAsync { id, result } => Some((id, result)),
                    CompletionEvent::InetReply { .. } => None,
                })
                .collect();
            if !staged.is_empty() {
                return staged;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
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

/// The `(address, port)` a packed-source-address header names: `[AF_INET, hi, lo, a, b, c, d]`
/// (`wire::pack_address`).
fn split_packed(header: &[u8]) -> ([u8; 4], u16) {
    assert_eq!(header.len(), 7, "the packed address is seven bytes: {header:?}");
    assert_eq!(header[0], INET_AF_INET, "the address family tag");
    let port = u16::from_be_bytes([header[1], header[2]]);
    (header[3..7].try_into().expect("four octets"), port)
}

/// Injects one **zero-length** datagram from a fresh external socket and returns that socket (whose
/// `local_addr` is the delivery's expected source).
///
/// `send_to(&[])` really does put a zero-length datagram on the wire — a `write(2)` of zero bytes
/// would send nothing, which is why this goes through a socket rather than `/dev/udp`.
fn inject_empty(destination: u16) -> UdpSocket {
    let sender = UdpSocket::bind("127.0.0.1:0").expect("an external sender socket");
    let sent = sender
        .send_to(&[], ("127.0.0.1", destination))
        .expect("the empty datagram is injected");
    assert_eq!(sent, 0, "a zero-length datagram carries no payload bytes");
    sender
}

// ---------------------------------------------------------------------------
// The active channel
// ---------------------------------------------------------------------------
/// **An empty datagram is delivered on the `{active, …}` channel**, with the packed source address
/// as the read's header and a **present, empty** body.
///
/// This is the C's `packet_reply_binary_data` → `packet_binary_message` pair: a `recvfrom(2)` that
/// returned `0` is a datagram, so the message is built and sent (`inet_drv.c:4335`, `:4110`). A
/// producer that gated the delivery on a non-empty payload would leave the owner waiting for a
/// message the oracle sent.
#[test]
fn an_empty_datagram_is_delivered_with_a_present_empty_body() {
    let (mut rig, bound) = bound_port();
    arm_active(&mut rig);
    let sender = inject_empty(bound);
    let source = sender.local_addr().expect("the sender's address").port();

    let drained = rig.drain_active_until_quiet();
    assert!(
        drained.ready_rounds >= 1,
        "the socket really was reported ready — the driver ran"
    );
    let data = data_of(&drained.inputs);
    assert_eq!(
        data.len(),
        1,
        "exactly one delivery for one empty datagram: {:?}",
        drained.inputs
    );
    let read = data[0];
    assert_eq!(
        read.header,
        pack_address(source, [127, 0, 0, 1]),
        "the 7-byte packed source address is the header: {read:?}"
    );
    assert_eq!(
        split_packed(&read.header),
        ([127, 0, 0, 1], source),
        "the header names the very socket that sent it"
    );
    assert_eq!(
        read.body,
        Some(Vec::new()),
        "the body is present and empty — `<<>>`, never `None`: {read:?}"
    );
    assert!(
        read.has_body(),
        "the binary tail exists even though it is empty: {read:?}"
    );
    assert!(
        !read.is_empty(),
        "the read carries its seven header bytes: {read:?}"
    );
    assert_eq!(read.len(), 7, "header only, no payload bytes");
    assert_eq!(
        read.flattened(),
        pack_address(source, [127, 0, 0, 1]),
        "`[list]` mode's flat list is the packed address alone"
    );
}

/// A second empty datagram is a second delivery: nothing coalesces or disappears because the
/// payloads are empty.
#[test]
fn every_empty_datagram_is_its_own_delivery() {
    let (mut rig, bound) = bound_port();
    arm_active(&mut rig);
    let first = inject_empty(bound);
    let second = inject_empty(bound);
    let drained = rig.drain_active_until_quiet();
    let data = data_of(&drained.inputs);
    assert_eq!(
        data.len(),
        2,
        "two datagrams, two deliveries: {:?}",
        drained.inputs
    );
    let sources: Vec<u16> = data
        .iter()
        .map(|read| split_packed(&read.header).1)
        .collect();
    let mut expected = vec![
        first.local_addr().expect("an address").port(),
        second.local_addr().expect("an address").port(),
    ];
    expected.sort_unstable();
    let mut got = sources.clone();
    got.sort_unstable();
    assert_eq!(got, expected, "each delivery names its own sender");
    for read in &data {
        assert_eq!(read.body, Some(Vec::new()), "both bodies are empty: {read:?}");
    }
}

// ---------------------------------------------------------------------------
// The passive async channel
// ---------------------------------------------------------------------------
/// **An empty datagram completes a queued `PACKET_REQ_RECV`**, as `{ok, Addr ++ <<>>}` — the C's
/// passive arm of `packet_reply_binary_data` (`inet_drv.c:4341`) reaching `inet_async_binary_data`.
#[test]
fn an_empty_datagram_completes_a_queued_passive_receive() {
    let (mut rig, bound) = bound_port();
    // `Timeout(4), Length(4)`; `-1` is `INET_INFINITY` and `Length` is ignored for a datagram
    // socket (`:14992`).
    let mut operand = (-1i32).to_be_bytes().to_vec();
    operand.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        rig.control(&op(PACKET_REQ_RECV), &operand),
        vec![1, 0, 0],
        "the receive is queued and its id answered"
    );
    let sender = inject_empty(bound);
    let source = sender.local_addr().expect("the sender's address").port();

    let completions = rig.drain_async_completions();
    assert_eq!(completions.len(), 1, "exactly one completion: {completions:?}");
    let (id, result) = completions.into_iter().next().expect("one completion");
    assert_eq!(id, 0, "the operation's own id");
    let InetAsyncResult::Data(read) = result else {
        panic!("a data completion, not {result:?}");
    };
    assert_eq!(
        read.header,
        pack_address(source, [127, 0, 0, 1]),
        "the packed source address is the header"
    );
    assert_eq!(
        read.body,
        Some(Vec::new()),
        "the payload is present and empty: {read:?}"
    );
    assert!(read.has_body(), "the binary tail exists: {read:?}");
    assert_eq!(read.len(), 7, "address only, no payload bytes");
}

// ---------------------------------------------------------------------------
// The live cross-check against the system `udp_inet`
// ---------------------------------------------------------------------------

/// The oracle expression for the **default `{deliver,term}`** mode: a `gen_udp` socket armed
/// `{active,true}`, one zero-length datagram sent from a *second* socket, and the delivery printed
/// as `{udp, {127,0,0,1}, is_integer(SrcPort), Payload}` — the ephemeral source port as a boolean
/// so the two sides are comparable even though they send from different sockets.
///
/// The sender is a `gen_udp` socket rather than `printf > /dev/udp/…`: a zero-length `write(2)`
/// puts **nothing** on the wire, so an empty datagram cannot be produced that way. It is still a
/// socket distinct from the receiver's, which is what the row depends on.
fn term_mode_probe() -> String {
    "{ok,S} = gen_udp:open(0,[binary,{active,true}]), \
     {ok,P} = inet:port(S), \
     {ok,S2} = gen_udp:open(0,[binary]), \
     ok = gen_udp:send(S2, {127,0,0,1}, P, <<>>), \
     M = receive X -> X after 2000 -> timeout end, \
     io:format(\"~w\", [{element(1,M),element(3,M),is_integer(element(4,M)),element(5,M)}])"
        .to_string()
}

/// The same probe in the C's **`{deliver,port}`** mode, normalised to
/// `{data, byte_size(Data), binary_part(Data,3,4)}` so the ephemeral source port cannot make the
/// row uncomparable.
///
/// This is the shape `inet_port_binary_data` (`inet_drv.c:2765`) produces with `hsz == 0`: the
/// packed source address in front of the payload through `{Port, {data, …}}`. This floor does not
/// deliver `{deliver,port}` ports at all (the documented gap: `PortNotification` carries the bytes
/// but the mode is left undelivered rather than handed a `{udp, …}` the owner never asked for), so
/// the row is pinned as the oracle's — and its seven bytes are exactly the header the
/// `{deliver,term}` channel above carries.
fn port_mode_probe() -> String {
    "{ok,S} = gen_udp:open(0,[binary,{active,true},{deliver,port}]), \
     {ok,P} = inet:port(S), \
     {ok,S2} = gen_udp:open(0,[binary]), \
     ok = gen_udp:send(S2, {127,0,0,1}, P, <<>>), \
     M = receive X -> X after 2000 -> timeout end, \
     D = element(2,M), \
     io:format(\"~w\", [{element(1,D),byte_size(element(2,D)),binary_part(element(2,D),3,4)}])"
        .to_string()
}

/// The tuple this side builds from the delivered [`ReadMessage`], rendered exactly as
/// [`term_mode_probe`] prints it.
fn rendered_term_mode(read: &ReadMessage) -> String {
    let (address, port) = split_packed(&read.header);
    let body = read.body.clone().expect("a present body, empty or not");
    let bytes: Vec<String> = body.iter().map(u8::to_string).collect();
    format!(
        "{{udp,{{{},{},{},{}}},{},<<{}>>}}",
        address[0],
        address[1],
        address[2],
        address[3],
        port != 0,
        bytes.join(",")
    )
}

/// **The oracle row.** The delivered `{udp, S, {127,0,0,1}, SrcPort, <<>>}` of an empty datagram
/// agrees term for term with what this floor's delivery renders, and the `{deliver,port}` row
/// carries the same seven packed address bytes.
#[test]
fn the_empty_datagram_rows_agree_with_the_erl_oracle() {
    let (mut rig, bound) = bound_port();
    arm_active(&mut rig);
    inject_empty(bound);
    let drained = rig.drain_active_until_quiet();
    let data = data_of(&drained.inputs);
    assert_eq!(data.len(), 1, "one delivery: {:?}", drained.inputs);
    let rendered = rendered_term_mode(data[0]);
    assert_eq!(
        rendered, "{udp,{127,0,0,1},true,<<>>}",
        "the packed address, a source port, and an empty binary payload"
    );

    let Some(oracle) = erl_stdout(&term_mode_probe()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    assert_eq!(
        oracle.trim(),
        rendered,
        "the oracle's {{udp,S,{{127,0,0,1}},SrcPort,<<>>}} agrees"
    );

    let Some(row) = erl_stdout(&port_mode_probe()) else {
        return;
    };
    assert_eq!(
        row.trim(),
        "{data,7,<<127,0,0,1>>}",
        "the `{{deliver,port}}` row is the seven packed address bytes and no tail"
    );
}
