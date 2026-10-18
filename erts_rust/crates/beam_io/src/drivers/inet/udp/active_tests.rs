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

//! The `udp_inet` **`{active, …}`** suite: a datagram that arrives on an armed datagram port is
//! **delivered to the port owner** on [`crate::port_task::ActiveInput`], and the
//! `{active, false | true | once | N}` state machine advances per delivery.
//!
//! Every case drives the real runtime path — a [`PortRegistry`] + [`Poller`] pair over a real
//! `udp_inet` socket, `erlang:port_control/3`'s decimal opcode route, and the registry's own
//! `handle_events` round — and injects its datagrams from **outside** the port with a plain
//! [`UdpSocket`], which is the only way to reach an `{active, …}` port with a source address the
//! test also knows. What is pinned here:
//!
//! * the delivery **shape** — the 7-byte packed source address as the read's `header`, the payload
//!   as its body (`packet_reply_binary_data` → `packet_binary_message`,
//!   `inet_drv.c:4335`/`:4110`), identical in `[list]` and `[binary]` mode because the channel is
//!   term-free;
//! * the per-delivery **accounting** (`inet_input_count`, `:15513`) and the `{active, …}` state
//!   machine `INET_CHECK_ACTIVE_TO_PASSIVE` (`:1552`-`:1560`): `{active, once}` flips
//!   **silently**, `{active, N}` flips **and** reports `{udp_passive, S}` after the datagram that
//!   spent the last credit, `{active, true}` never does;
//! * the passive floor — no `{active, …}` state, no delivery, and **nothing consumed** from the
//!   kernel (the datagram is still there for a passive `recv`);
//! * the two places a `Passive` can come from: a delivery
//!   ([`UdpInetDriver::active_delivery`]) and the `setopts` option walk itself
//!   (`inet_drv.c:6993`-`:6998`), which is where an `{active, N}` that reaches zero **without** a
//!   datagram is reported — and nowhere else (a UDP `setopts` never performs the inline read both
//!   `/* XXX: UDP sockets could also trigger immediate read here NIY */` comments admit the C does
//!   not have: `:7949`/`:7969`);
//! * the bounded burst (`packet_inet_input`'s `while (packet_count--)`, `:15378`): at most
//!   `read_packets` datagrams per readiness callback, the rest on the following rounds;
//! * every behavioural row **cross-checked against the live system `udp_inet`** through an `erl`
//!   probe, skipping **loudly** when `erl` is absent (`erl_stdout`).
use std::net::UdpSocket;
use std::time::{Duration, Instant};

use beam_process::PortId;

use super::super::opcode_tests::{erl_stdout, op};
use super::super::opt::{
    INET_ACTIVE, INET_LOPT_ACTIVE, INET_LOPT_MODE, INET_LOPT_UDP_READ_PACKETS, INET_MULTI, INET_ONCE,
    INET_PASSIVE,
};
use super::super::wire::{
    ctl_ok, ctl_xerror, INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT, INET_TYPE_DGRAM,
};
use super::UdpInetDriver;
use crate::poller::Poller;
use crate::port::PortRegistry;
use crate::port_task::{ActiveInput, ActiveInputEvent, ReadMessage};

/// `INET_REQ_OPEN` (`inet_drv.c:14577`).
const INET_REQ_OPEN: u32 = 1;
/// `INET_REQ_BIND` (`:11409`).
const INET_REQ_BIND: u32 = 6;
/// `INET_REQ_SETOPTS` (`:11111`).
const INET_REQ_SETOPTS: u32 = 7;
/// `INET_REQ_GETOPTS` (`:11104`).
const INET_REQ_GETOPTS: u32 = 8;
/// `INET_REQ_GETSTAT` (`:11550`).
const INET_REQ_GETSTAT: u32 = 11;
/// The C's `INET_STAT_RECV_CNT` counter op (a 5-byte `[op, u32_be]` record).
const INET_STAT_RECV_CNT: u8 = 1;
/// The C's `INET_STAT_RECV_OCT` counter op (a 9-byte `[op, u64_be]` record).
const INET_STAT_RECV_OCT: u8 = 9;
/// `{mode, list}` (`INET_MODE_LIST`, `inet_drv.c:778`) — the driver's default.
const INET_MODE_LIST: i32 = 0;
/// `{mode, binary}` (`INET_MODE_BINARY`, `:779`).
const INET_MODE_BINARY: i32 = 1;
/// `INET_PACKET_POLL` (`:1072`) — the `{read_packets, N}` default, i.e. how many datagrams one
/// readiness callback may take.
const INET_PACKET_POLL: i32 = 5;
/// `INT16_MAX`: the `{active, N}` counter's ceiling (`:6975`).
const ACTIVE_COUNT_MAX: i32 = i16::MAX as i32;
/// The `{active, 2}` credit the multi cases arm with.
const ACTIVE_MULTI_CREDIT: i32 = 2;
/// One round's poll budget. Every wait in this suite is a *condition* under [`HANG_GUARD`], so
/// this only paces the rounds; a starved host cannot fail a case with it.
const ROUND_BUDGET: Duration = Duration::from_millis(100);
/// How many consecutive rounds without a single readiness event count as "the port has gone
/// quiet". More than one because a scheduler-run round may observe nothing yet (an edge already
/// consumed by the previous round) while more is still to come.
const QUIET_ROUNDS: usize = 2;
/// The hang guard every wait is bounded by: a broken driver fails the case instead of hanging it.
const HANG_GUARD: Duration = Duration::from_secs(60);
/// The most rounds [`Rig::drain_until_quiet`] will drive before concluding the port is looping.
const MAX_ROUNDS: usize = 200;

/// A live `udp_inet` port bound to an ephemeral loopback port, driven through the real runtime
/// path: [`PortRegistry::port_control`] for the control verbs, [`PortRegistry::handle_events`] for
/// a readiness round, [`PortRegistry::take_active_inputs`] for the deliveries.
struct Rig {
    registry: PortRegistry,
    poller: Poller,
    id: PortId,
    /// The loopback port the kernel bound (`INET_REQ_BIND`'s reply).
    bound: u16,
}

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
    /// How many deliveries each round produced.
    per_round: Vec<usize>,
    /// How many rounds observed a readiness event — the non-vacuity witness for a "nothing is
    /// delivered" assertion: the socket really was reported ready and the driver really ran.
    ready_rounds: usize,
}

impl Rig {
    /// Opens a `udp_inet` port, creates its datagram socket (`INET_REQ_OPEN`) and binds it to an
    /// ephemeral loopback port (`INET_REQ_BIND`).
    fn new() -> Rig {
        let mut poller = Poller::new().expect("a poll set");
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(UdpInetDriver::new()))
            .expect("the port opens");
        let mut rig = Rig {
            registry,
            poller,
            id,
            bound: 0,
        };
        assert_eq!(
            rig.control(op(INET_REQ_OPEN), &[
                INET_AF_INET,
                INET_TYPE_DGRAM,
                INET_PROTO_DEFAULT
            ]),
            ctl_ok(),
            "the datagram socket opens"
        );
        let reply = rig.control(op(INET_REQ_BIND), &[INET_AF_LOOPBACK, 0, 0]);
        assert_eq!(reply.first(), Some(&1), "the bind succeeds: {reply:?}");
        rig.bound = u16::from_be_bytes([reply[1], reply[2]]);
        assert_ne!(rig.bound, 0, "the kernel assigned a port");
        rig
    }

    /// One control call, panicking on a typed error.
    fn control(&mut self, command: String, arg: &[u8]) -> Vec<u8> {
        self.registry
            .port_control(&mut self.poller, self.id, &command, arg)
            .unwrap_or_else(|err| panic!("the `{command}` control call failed: {err:?}"))
    }

    /// One `INET_REQ_SETOPTS` carrying `records` (`[code, value_be32]*`).
    fn set_options(&mut self, records: &[(u8, i32)]) -> Vec<u8> {
        let mut arg = Vec::with_capacity(records.len() * 5);
        for (code, value) in records {
            arg.push(*code);
            arg.extend_from_slice(&value.to_be_bytes());
        }
        self.control(op(INET_REQ_SETOPTS), &arg)
    }

    /// `{active, value}` — the one option this whole suite turns on.
    fn set_active(&mut self, value: i32) -> Vec<u8> {
        self.set_options(&[(INET_LOPT_ACTIVE, value)])
    }

    /// `{active, value}`, asserting the walk accepted it.
    fn arm(&mut self, value: i32) {
        assert_eq!(self.set_active(value), ctl_ok(), "{{active,{value}}} is accepted");
    }

    /// The driver's stored `{active, …}` state, read back through `INET_REQ_GETOPTS` code 22: the
    /// `INET_LOPT_ACTIVE` value, plus the `{active, N}` counter when the mode is `N`.
    fn active_state(&mut self) -> (i32, Option<i16>) {
        let reply = self.control(op(INET_REQ_GETOPTS), &[INET_LOPT_ACTIVE]);
        assert_eq!(reply.first(), Some(&1), "the getopts reply is a success");
        assert_eq!(reply.get(1), Some(&INET_LOPT_ACTIVE), "one record: {reply:?}");
        let value = i32::from_be_bytes([reply[2], reply[3], reply[4], reply[5]]);
        let counter = (reply.len() >= 8)
            .then(|| i16::from_be_bytes([reply[6], reply[7]]));
        (value, counter)
    }

    /// The `INET_REQ_GETSTAT` receive counters as `(recv_cnt, recv_oct)`.
    fn recv_stats(&mut self) -> (u32, u64) {
        let reply = self.control(
            op(INET_REQ_GETSTAT),
            &[INET_STAT_RECV_OCT, INET_STAT_RECV_CNT],
        );
        assert_eq!(reply.len(), 15, "two records: {reply:?}");
        assert_eq!(reply[0], 1, "a success reply");
        assert_eq!(reply[1], INET_STAT_RECV_OCT);
        let oct = u64::from_be_bytes([
            reply[2], reply[3], reply[4], reply[5], reply[6], reply[7], reply[8], reply[9],
        ]);
        assert_eq!(reply[10], INET_STAT_RECV_CNT);
        let cnt = u32::from_be_bytes([reply[11], reply[12], reply[13], reply[14]]);
        (cnt, oct)
    }

    /// Runs exactly **one** readiness round: poll, hand the events to the registry (which
    /// schedules *and* runs the round's port tasks), then drain the deliveries.
    fn round(&mut self) -> Round {
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
    /// delivery — the condition wait every case here uses instead of a fixed sleep, under
    /// [`HANG_GUARD`].
    fn drain_until_quiet(&mut self) -> Drained {
        let guard = Instant::now() + HANG_GUARD;
        let mut drained = Drained {
            inputs: Vec::new(),
            per_round: Vec::new(),
            ready_rounds: 0,
        };
        let mut quiet = 0;
        while quiet < QUIET_ROUNDS {
            assert!(
                Instant::now() < guard,
                "the port produced deliveries but never went quiet: {:?}",
                drained.per_round
            );
            assert!(
                drained.per_round.len() < MAX_ROUNDS,
                "the port kept reporting readiness for {MAX_ROUNDS} rounds: {:?}",
                drained.per_round
            );
            let round = self.round();
            if round.events > 0 {
                quiet = 0;
                drained.ready_rounds += 1;
            } else {
                quiet += 1;
            }
            drained.per_round.push(round.inputs.len());
            drained.inputs.extend(round.inputs);
        }
        drained
    }

    /// Injects one datagram `payload` from a fresh external socket and returns that socket (whose
    /// `local_addr` is the delivery's expected source).
    fn inject(&self, payload: &[u8]) -> UdpSocket {
        let sender = UdpSocket::bind("127.0.0.1:0").expect("an external sender socket");
        sender
            .send_to(payload, ("127.0.0.1", self.bound))
            .expect("the datagram is injected");
        sender
    }

    /// A passive `recv` through the driver's named verb, draining the payload it pushed into the
    /// port's input queue. Returns the reply bytes and the payload.
    fn passive_recv(&mut self) -> (Vec<u8>, Vec<u8>) {
        let reply = self.control("recv".to_string(), &[]);
        let mut buf = vec![0u8; 64];
        let read = self
            .registry
            .get_mut(self.id)
            .expect("a live port")
            .read_input(&mut buf);
        buf.truncate(read);
        (reply, buf)
    }
}

/// Every `Data` delivery of a drain, in order.
fn data_of(inputs: &[ActiveInput]) -> Vec<&ReadMessage> {
    inputs
        .iter()
        .filter_map(|input| match &input.event {
            ActiveInputEvent::Data(read) => Some(read),
            ActiveInputEvent::Passive => None,
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

/// The `(address, port)` a packed-source-address header names: `[AF_INET, hi, lo, a, b, c, d]`
/// (`wire::pack_address`).
fn split_packed(header: &[u8]) -> ([u8; 4], u16) {
    assert_eq!(header.len(), 7, "the packed address is seven bytes: {header:?}");
    assert_eq!(header[0], INET_AF_INET, "the address family tag");
    let port = u16::from_be_bytes([header[1], header[2]]);
    (header[3..7].try_into().expect("four octets"), port)
}

// ---------------------------------------------------------------------------
// `{active, true}` — every datagram, delivered
// ---------------------------------------------------------------------------
#[test]
fn an_active_port_delivers_the_datagram_with_its_packed_source_address() {
    let mut rig = Rig::new();
    rig.arm(INET_ACTIVE);
    let sender = rig.inject(b"abc");
    let source = sender.local_addr().expect("the sender's address").port();
    let drained = rig.drain_until_quiet();
    assert!(drained.ready_rounds >= 1, "the socket was reported ready");
    assert_eq!(passive_count(&drained.inputs), 0, "{{active,true}} never reports a passive message");
    let data = data_of(&drained.inputs);
    assert_eq!(data.len(), 1, "exactly one delivery: {:?}", drained.per_round);
    assert_eq!(
        drained.inputs[0].port, rig.id,
        "the delivery names the delivering port"
    );
    let read = data[0];
    let (addr, port) = split_packed(&read.header);
    assert_eq!(addr, [127, 0, 0, 1], "the source address");
    assert_eq!(port, source, "the source port is the very socket that sent it");
    assert_eq!(read.body.as_deref(), Some(b"abc".as_slice()), "the payload");
    // `inet_input_count(desc, n)` counted the payload, once (`:15513`).
    assert_eq!(rig.recv_stats(), (1, 3), "one datagram of three octets");
}

// ---------------------------------------------------------------------------
// `{active, false}` — the passive floor
// ---------------------------------------------------------------------------
#[test]
fn a_passive_port_delivers_nothing_and_leaves_the_datagram_in_the_kernel() {
    let mut rig = Rig::new();
    assert_eq!(rig.active_state(), (INET_PASSIVE, None), "passive by default");
    rig.inject(b"abc");
    let drained = rig.drain_until_quiet();
    assert!(
        drained.ready_rounds >= 1,
        "the socket really was reported ready — the driver ran and chose not to deliver"
    );
    assert!(
        drained.inputs.is_empty(),
        "a passive port reports nothing on the active channel: {:?}",
        drained.per_round
    );
    // ... and the datagram was not consumed by the attempt: the passive verb still reads it.
    let (reply, payload) = rig.passive_recv();
    assert_eq!(reply.first(), Some(&1), "the recv succeeds: {reply:?}");
    assert_eq!(payload, b"abc", "the datagram was still in the kernel");
}

// ---------------------------------------------------------------------------
// `{active, once}` — one delivery, then a silent flip
// ---------------------------------------------------------------------------
#[test]
fn an_active_once_port_flips_silently_after_one_delivery() {
    let mut rig = Rig::new();
    rig.arm(INET_ONCE);
    rig.inject(b"one");
    let first = rig.drain_until_quiet();
    assert_eq!(data_of(&first.inputs).len(), 1, "the armed delivery");
    assert_eq!(passive_count(&first.inputs), 0, "{{active,once}} never reports a passive message");
    assert_eq!(
        rig.active_state(),
        (INET_PASSIVE, None),
        "the driver is passive after the delivery"
    );
    // A second datagram reaches a **passive** port: reported ready, nothing delivered, and
    // still no passive message.
    rig.inject(b"two");
    let second = rig.drain_until_quiet();
    assert!(second.ready_rounds >= 1, "the second datagram was reported ready");
    assert!(
        second.inputs.is_empty(),
        "the flipped port delivers nothing more (and no passive message): {:?}",
        second.per_round
    );
    // The undelivered datagram is still there for the passive verb.
    let (_, payload) = rig.passive_recv();
    assert_eq!(payload, b"two", "the second datagram waited in the kernel");
}

// ---------------------------------------------------------------------------
// `{active, N}` — the counter, and the one passive message
// ---------------------------------------------------------------------------
#[test]
fn an_active_n_port_delivers_n_datagrams_then_one_passive_message() {
    let mut rig = Rig::new();
    rig.arm(ACTIVE_MULTI_CREDIT);
    assert_eq!(
        rig.active_state(),
        (INET_MULTI, Some(2)),
        "the counter is the credit"
    );
    for index in 1..=3 {
        rig.inject(format!("d{index}").as_bytes());
    }
    let drained = rig.drain_until_quiet();
    let payloads: Vec<Vec<u8>> = data_of(&drained.inputs)
        .iter()
        .map(|read| read.body.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        payloads,
        vec![b"d1".to_vec(), b"d2".to_vec()],
        "exactly two deliveries — the third datagram stays in the kernel"
    );
    assert_eq!(passive_count(&drained.inputs), 1, "one {{udp_passive,S}}");
    // ... and the passive message comes **after** the datagram that spent the last credit.
    let passive_at = drained
        .inputs
        .iter()
        .position(|input| input.event == ActiveInputEvent::Passive)
        .expect("a passive delivery");
    assert_eq!(
        passive_at,
        drained.inputs.len() - 1,
        "the passive message is last: {:?}",
        drained.inputs
    );
    assert_eq!(
        rig.active_state(),
        (INET_PASSIVE, None),
        "the spent counter left the port passive"
    );
    // The third datagram is still readable.
    let (_, payload) = rig.passive_recv();
    assert_eq!(payload, b"d3", "the third datagram was never consumed");
    // Nothing more is delivered now that the port is passive.
    rig.inject(b"d4");
    let after = rig.drain_until_quiet();
    assert!(after.inputs.is_empty(), "a passive port delivers nothing");
}

/// `{active, 2}`'s value — the suite's constant for "two credits".
#[test]
fn the_active_n_counter_accumulates_and_is_reset_by_the_other_modes() {
    // `{active, 1}` twice: the C's `ac = active_count; ac += nval` (`:6975`) accumulates.
    let mut rig = Rig::new();
    rig.arm(1);
    assert_eq!(rig.active_state(), (INET_MULTI, Some(1)));
    rig.arm(1);
    assert_eq!(
        rig.active_state(),
        (INET_MULTI, Some(2)),
        "two single-credit records accumulate"
    );
    for index in 1..=3 {
        rig.inject(format!("a{index}").as_bytes());
    }
    let drained = rig.drain_until_quiet();
    assert_eq!(data_of(&drained.inputs).len(), 2, "two credits, two deliveries");
    assert_eq!(passive_count(&drained.inputs), 1, "then one passive message");
    // `{active, false}` zeroes the counter (and is not multi, so no counter is reported).
    let mut rig = Rig::new();
    rig.arm(7);
    rig.arm(INET_PASSIVE);
    assert_eq!(
        rig.active_state(),
        (INET_PASSIVE, None),
        "{{active,false}} zeroes the counter"
    );
    rig.arm(4);
    assert_eq!(
        rig.active_state(),
        (INET_MULTI, Some(4)),
        "a fresh multi arm after a passive one starts from its own credit"
    );
    // `{active, true}` zeroes it too (`:6999`).
    rig.arm(INET_ACTIVE);
    assert_eq!(
        rig.active_state(),
        (INET_ACTIVE, None),
        "{{active,true}} zeroes the counter"
    );
    // The counter's `INT16` ceiling (`:6975`): `ac > INT16_MAX` is refused with `[0]einval`.
    let mut rig = Rig::new();
    rig.arm(ACTIVE_COUNT_MAX);
    assert_eq!(
        rig.set_active(ACTIVE_COUNT_MAX),
        ctl_xerror("einval"),
        "the sum overflows the C's Sint16"
    );
    assert_eq!(
        rig.active_state(),
        (INET_MULTI, Some(i16::MAX)),
        "the refused record left the counter where it was (no rollback, `:6975`)"
    );
}

#[test]
fn an_active_n_reaching_zero_inside_setopts_reports_one_passive_message() {
    // No datagram is involved at all: `{active,1}` then `{active,-1}` sums to 0, which the option
    // walk turns into `active = INET_PASSIVE` + `packet_passive_message` (`:6993`-`:6998`).
    let mut rig = Rig::new();
    rig.arm(1);
    let before = rig.registry.take_active_inputs();
    assert!(before.is_empty(), "arming reports nothing");
    assert_eq!(rig.set_active(-1), ctl_ok(), "a negative credit is accepted");
    // The control call itself staged the message — no round, no poll, no datagram.
    let staged = rig.registry.take_active_inputs();
    assert_eq!(staged.len(), 1, "exactly one delivery from the setopts path");
    assert_eq!(staged[0].event, ActiveInputEvent::Passive);
    assert_eq!(staged[0].port, rig.id);
    assert_eq!(rig.active_state(), (INET_PASSIVE, None));
    // Draining is idempotent: the message is reported exactly once.
    rig.inject(b"x");
    let drained = rig.drain_until_quiet();
    assert_eq!(
        passive_count(&drained.inputs),
        0,
        "the setopts message was not re-reported"
    );
    assert!(drained.inputs.is_empty(), "the port is passive again");
}

// ---------------------------------------------------------------------------
// The bounded burst
// ---------------------------------------------------------------------------
#[test]
fn one_callback_delivers_at_most_read_packets_datagrams() {
    let mut rig = Rig::new();
    rig.arm(INET_ACTIVE);
    assert_eq!(
        rig.set_options(&[(INET_LOPT_UDP_READ_PACKETS, INET_PACKET_POLL)]),
        ctl_ok(),
        "{{read_packets,{INET_PACKET_POLL}}} is accepted"
    );
    let count = usize::try_from(INET_PACKET_POLL).expect("a positive count") + 3;
    for index in 0..count {
        rig.inject(format!("p{index}").as_bytes());
    }
    let started = Instant::now();
    let drained = rig.drain_until_quiet();
    assert!(
        drained
            .per_round
            .iter()
            .all(|per_round| *per_round <= usize::try_from(INET_PACKET_POLL).expect("a count")),
        "no round may exceed the read_packets allowance: {:?}",
        drained.per_round
    );
    assert_eq!(
        data_of(&drained.inputs).len(),
        count,
        "every datagram is delivered, across rounds: {:?}",
        drained.per_round
    );
    assert!(
        drained.per_round.len() < MAX_ROUNDS,
        "the burst is bounded and returns: {:?}",
        drained.per_round
    );
    assert!(
        started.elapsed() < HANG_GUARD,
        "the whole burst completed inside the guard"
    );
    // Order is preserved: the user-visible sequence is the injection sequence.
    let payloads: Vec<Vec<u8>> = data_of(&drained.inputs)
        .iter()
        .map(|read| read.body.clone().unwrap_or_default())
        .collect();
    let expected: Vec<Vec<u8>> = (0..count)
        .map(|index| format!("p{index}").into_bytes())
        .collect();
    assert_eq!(payloads, expected);
}

// ---------------------------------------------------------------------------
// Arming from passive
// ---------------------------------------------------------------------------
#[test]
fn a_datagram_buffered_while_passive_is_delivered_after_arming() {
    let mut rig = Rig::new();
    rig.inject(b"buffered");
    let passive = rig.drain_until_quiet();
    assert!(passive.ready_rounds >= 1, "the passive port saw the readiness");
    assert!(passive.inputs.is_empty(), "and delivered nothing");
    // The arming `setopts` only arms — UDP never performs the C's stream-only inline read
    // (`trigger_recv`, `:7945`, with both `NIY` comments at `:7949`/`:7969`), so nothing is
    // delivered *inside* the control call.
    rig.arm(INET_ACTIVE);
    assert!(
        rig.registry.take_active_inputs().is_empty(),
        "a UDP setopts never delivers inline"
    );
    let armed = rig.drain_until_quiet();
    assert!(armed.ready_rounds >= 1, "the re-arm re-reported the buffered datagram");
    let data = data_of(&armed.inputs);
    assert_eq!(data.len(), 1, "one delivery on the next readiness round");
    assert_eq!(data[0].body.as_deref(), Some(b"buffered".as_slice()));
}

// ---------------------------------------------------------------------------
// The channel is term-free: `[list]` and `[binary]` deliver the same read
// ---------------------------------------------------------------------------
#[test]
fn the_delivery_shape_is_the_same_in_list_and_binary_mode() {
    let mut reads = Vec::new();
    for mode in [INET_MODE_LIST, INET_MODE_BINARY] {
        let mut rig = Rig::new();
        assert_eq!(
            rig.set_options(&[(INET_LOPT_MODE, mode)]),
            ctl_ok(),
            "{{mode,{mode}}} is accepted"
        );
        rig.arm(INET_ACTIVE);
        let sender = rig.inject(b"abc");
        let source = sender.local_addr().expect("the sender's address").port();
        let drained = rig.drain_until_quiet();
        let data = data_of(&drained.inputs);
        assert_eq!(data.len(), 1, "{{mode,{mode}}} delivers one datagram");
        let read = data[0].clone();
        assert_eq!(
            split_packed(&read.header),
            ([127, 0, 0, 1], source),
            "the packed source address, mode {{mode}}"
        );
        assert_eq!(
            read.body.as_deref(),
            Some(b"abc".as_slice()),
            "the payload, mode {{mode}}"
        );
        reads.push(read);
    }
    assert_eq!(
        reads[0], reads[1],
        "the driver's read carries no term shape, so the two modes agree byte for byte"
    );
}

// ---------------------------------------------------------------------------
// The `erl` oracle
// ---------------------------------------------------------------------------
/// The `os:cmd` that injects `payload` into the datagram socket bound to `$PORT` — a datagram
/// from **outside** the VM, exactly like the [`UdpSocket`] the Rust cases use.
fn inject_cmd(port_var: &str, payload: &str) -> String {
    format!("os:cmd(\"printf {payload} > /dev/udp/127.0.0.1/\" ++ integer_to_list({port_var}))")
}

/// The oracle expression that opens a `gen_udp` socket in `mode` with `options`, injects
/// `payload`, and prints the delivery as
/// `{udp, {127,0,0,1}, is_integer(SourcePort), Payload}` — the ephemeral source port is printed as
/// a boolean so the two sides can be compared even though they send from different sockets.
fn shape_probe(mode: &str, options: &str, payload: &str) -> String {
    format!(
        "{{ok,S}} = gen_udp:open(0,[{mode},{options}]), \
         {{ok,P}} = inet:port(S), \
         {inject}, \
         M = receive X -> X after 2000 -> timeout end, \
         io:format(\"~p\", [{{element(1,M),element(3,M),is_integer(element(4,M)),element(5,M)}}])",
        inject = inject_cmd("P", payload),
    )
}

/// The same tuple this side can build from a [`ReadMessage`], so the oracle's rendering and this
/// floor's read are compared rather than merely both being asserted.
fn rendered_shape(read: &ReadMessage) -> String {
    let (addr, port) = split_packed(&read.header);
    let body = read.body.clone().unwrap_or_default();
    format!(
        "{{udp,{{{},{},{},{}}},{},<<\"{}\">>}}",
        addr[0],
        addr[1],
        addr[2],
        addr[3],
        port != 0,
        String::from_utf8_lossy(&body)
    )
}

#[test]
fn the_binary_delivery_shape_agrees_with_the_oracle() {
    let mut rig = Rig::new();
    assert_eq!(
        rig.set_options(&[(INET_LOPT_MODE, INET_MODE_BINARY)]),
        ctl_ok()
    );
    rig.arm(INET_ACTIVE);
    rig.inject(b"abc");
    let drained = rig.drain_until_quiet();
    let data = data_of(&drained.inputs);
    assert_eq!(data.len(), 1, "one delivery");
    let rendered = rendered_shape(data[0]);
    assert_eq!(
        rendered, "{udp,{127,0,0,1},true,<<\"abc\">>}",
        "the packed address + payload tuple"
    );
    let Some(oracle) = erl_stdout(&shape_probe("binary", "{active,true}", "abc")) else {
        return;
    };
    assert_eq!(
        oracle.trim(),
        rendered,
        "the oracle's {{udp,S,{{127,0,0,1}},SrcPort,Data}} agrees"
    );
}

#[test]
fn the_list_delivery_shape_agrees_with_the_oracle() {
    let mut rig = Rig::new();
    assert_eq!(rig.set_options(&[(INET_LOPT_MODE, INET_MODE_LIST)]), ctl_ok());
    rig.arm(INET_ACTIVE);
    rig.inject(b"abc");
    let drained = rig.drain_until_quiet();
    let data = data_of(&drained.inputs);
    assert_eq!(data.len(), 1, "one delivery");
    // The channel is term-free, so `[list]` mode's flat list is the runtime's rendering of this
    // very same `ReadMessage` — the payload bytes are what both modes carry.
    assert_eq!(data[0].body.as_deref(), Some(b"abc".as_slice()));
    let Some(oracle) = erl_stdout(&shape_probe("list", "{active,true}", "abc")) else {
        return;
    };
    assert_eq!(
        oracle.trim(),
        "{udp,{127,0,0,1},true,\"abc\"}",
        "the oracle's [list] delivery is the flat string"
    );
}

/// The oracle expression pinning `{active, once}`'s **silent** flip: one delivery, then the port is
/// passive — a second injection leaves the mailbox empty and no `{udp_passive, S}` is ever sent.
fn once_probe() -> String {
    format!(
        "{{ok,S}} = gen_udp:open(0,[binary,{{active,once}}]), \
         {{ok,P}} = inet:port(S), \
         {first}, \
         M = receive X -> X after 2000 -> timeout end, \
         {second}, \
         timer:sleep(300), \
         io:format(\"~p\", [{{element(5,M), \
                            element(2,process_info(self(),message_queue_len)), \
                            receive {{udp_passive,_}} -> passive after 0 -> none end}}])",
        first = inject_cmd("P", "one"),
        second = inject_cmd("P", "two"),
    )
}

/// The oracle expression pinning `{active, 2}`: two data messages and then exactly one
/// `{udp_passive, S}`, the third datagram left in the kernel.
fn multi_probe() -> String {
    format!(
        "{{ok,S}} = gen_udp:open(0,[binary,{{active,2}}]), \
         {{ok,P}} = inet:port(S), \
         [os:cmd(\"printf d\" ++ integer_to_list(I) ++ \" > /dev/udp/127.0.0.1/\" \
                    ++ integer_to_list(P)) || I <- [1,2,3]], \
         G = fun F(0, Acc) -> lists:reverse(Acc); \
                 F(N, Acc) -> F(N-1, [receive Z -> Z after 500 -> nothing end | Acc]) end, \
         io:format(\"~p\", [[case M of {{udp,_,_,_,D}} -> {{data,D}}; \
                                     {{udp_passive,_}} -> passive; \
                                     Other -> Other end || M <- G(3, [])]])"
    )
}

#[test]
fn the_once_and_multi_rows_agree_with_the_oracle() {
    let Some(once) = erl_stdout(&once_probe()) else {
        return;
    };
    assert_eq!(
        once.trim(),
        r#"{<<"one">>,0,none}"#,
        "the oracle flips silently after one delivery and never sends {{udp_passive,S}}"
    );
    let Some(multi) = erl_stdout(&multi_probe()) else {
        return;
    };
    assert_eq!(
        multi.trim(),
        r#"[{data,<<"d1">>},{data,<<"d2">>},passive]"#,
        "two deliveries then one passive message, the third datagram undelivered"
    );
}

/// The oracle expression pinning that a UDP `setopts` only **arms**: with a datagram already
/// buffered, the mailbox is empty immediately after `{active,false}` ⇒ `{active,true}` and holds
/// it one settle later.
fn arming_probe() -> String {
    format!(
        "{{ok,S}} = gen_udp:open(0,[binary,{{active,false}}]), \
         {{ok,P}} = inet:port(S), \
         {inject}, \
         timer:sleep(150), \
         ok = inet:setopts(S,[{{active,true}}]), \
         Before = element(2,process_info(self(),message_queue_len)), \
         timer:sleep(250), \
         After = length(element(2,process_info(self(),messages))), \
         io:format(\"~p\", [{{Before,After}}])",
        inject = inject_cmd("P", "buf"),
    )
}

/// The oracle expression pinning the setopts-path passive message: `{active,1}` then
/// `{active,-1}` sums the counter to zero with no datagram anywhere, and the C's option walk sends
/// `{udp_passive, S}` itself.
fn lands_on_zero_probe() -> String {
    "{ok,S} = gen_udp:open(0,[binary,{active,1}]), \
     ok = inet:setopts(S,[{active,-1}]), \
     io:format(\"~p\", [{receive Z -> element(1,Z) after 500 -> none end, \
                        inet:getopts(S,[active])}])"
        .to_string()
}

#[test]
fn the_arming_and_lands_on_zero_rows_agree_with_the_oracle() {
    let Some(arming) = erl_stdout(&arming_probe()) else {
        return;
    };
    assert_eq!(
        arming.trim(),
        "{0,1}",
        "a UDP setopts arms only: nothing inside the ctl, the buffered datagram one round later"
    );
    let Some(zeroed) = erl_stdout(&lands_on_zero_probe()) else {
        return;
    };
    assert_eq!(
        zeroed.trim(),
        "{udp_passive,{ok,[{active,false}]}}",
        "the walk itself sends {{udp_passive,S}} when the counter lands on zero"
    );
}
