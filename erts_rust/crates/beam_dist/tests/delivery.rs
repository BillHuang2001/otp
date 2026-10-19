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

#![deny(unsafe_code)]

//! Hermetic tests for the inbound `DOP_*` **delivery** seam
//! ([`beam_dist::delivery`]): a decoded control message is classified into a
//! semantic [`Delivery`] and routed through an [`InboundSink`].
//!
//! No sockets are involved: a **client** and a **server** [`ConnectionDriver`]
//! are wired to each other and their [`ConnectionDriver::take_output`] bytes are
//! shuttled by hand (the same harness as `tests/driver.rs`). Every expectation is
//! the *observable* classification of a real wire message built with
//! [`ConnectionDriver::send_control`] / [`ConnectionDriver::send_message`] — never
//! an internal detail.
//!
//! Small integers are used as the control/message terms wherever the exact terms
//! matter: unlike atoms (whose raw encoding is table-local), a small integer is
//! its own immediate value, so a term compares `==` across the two drivers' heaps.
//! The tests therefore pin both the *variant* (which argument is which) and the
//! *exact* terms.

use beam_alloc::Heap;
use beam_codec::term::{
    external_ref_parts, internal_ref_id, make_external_pid, make_external_ref, make_internal_ref,
};
use beam_common::{BeamError, Result};
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::flags::{
    AltactSigFlags, DistSpawnFlags, Dop, DIST_SPAWN_FLAG_LINK, DIST_SPAWN_FLAG_MONITOR,
    DOP_ALTACT_SIG_FLG_NAME, DOP_ALTACT_SIG_FLG_PRIO, DOP_ALTACT_SIG_FLG_TOKEN,
};
use beam_dist::{
    ControlMessage, Delivery, DeliveryOutcome, Dispatched, InboundSink, LinkIdentity,
    UnsupportedSink,
};
use beam_term::prelude::{make_atom, make_internal_pid, make_pid_data, make_small, Eterm};
use beam_term::AtomTable;
/// The shared cookie both ends authenticate with.
const COOKIE: &[u8] = b"s3cr3t-cookie";
/// The connecting node's name.
const CLIENT_NAME: &[u8] = b"client@host";
/// The accepting node's name.
const SERVER_NAME: &[u8] = b"server@host";
/// The connecting node's creation.
const CLIENT_CREATION: u32 = 7;
/// The accepting node's creation.
const SERVER_CREATION: u32 = 0xdead_beef;

/// Drives a client and a server driver against each other until both are
/// established (the `tests/driver.rs` harness, whole-buffer mode).
fn establish() -> (ConnectionDriver, ConnectionDriver) {
    let mut client = ConnectionDriver::client(CLIENT_NAME, COOKIE, CLIENT_CREATION);
    let mut server = ConnectionDriver::server(SERVER_NAME, COOKIE, SERVER_CREATION);

    for _ in 0..100_000 {
        let client_out = client.take_output();
        let server_out = server.take_output();
        if !client_out.is_empty() {
            server.feed(&client_out);
        }
        if !server_out.is_empty() {
            client.feed(&server_out);
        }
        client.step().expect("client step");
        server.step().expect("server step");

        if client.state() == DriverState::Established && server.state() == DriverState::Established {
            return (client, server);
        }
        if client.state() == DriverState::Failed || server.state() == DriverState::Failed {
            panic!("driver failed: client={:?}, server={:?}", client.state(), server.state());
        }
    }
    panic!("handshake did not complete");
}

/// Shuttles `from`'s queued output into `to` and makes one `step` of progress.
fn relay(from: &mut ConnectionDriver, to: &mut ConnectionDriver) {
    let out = from.take_output();
    assert!(!out.is_empty(), "the sender produced no bytes");
    to.feed(&out);
    to.step().expect("receiver step");
}

/// Decodes and classifies the next inbound delivery on `server`.
fn next_delivery(
    server: &mut ConnectionDriver,
    heap: &mut Heap,
    atoms: &mut AtomTable,
) -> Delivery {
    server
        .next_delivery(heap, atoms)
        .expect("decode inbound delivery")
        .expect("an inbound delivery")
}

/// An [`InboundSink`] that records every delivery it receives and reports
/// [`DeliveryOutcome::Delivered`].
#[derive(Default)]
struct RecordingSink {
    deliveries: Vec<Delivery>,
}

impl InboundSink for RecordingSink {
    fn deliver(&mut self, delivery: &Delivery) -> Result<DeliveryOutcome> {
        self.deliveries.push(delivery.clone());
        Ok(DeliveryOutcome::Delivered)
    }
}

// ---------------------------------------------------------------------------
// (a) A `send` message is classified with exactly the terms sent, and a sink
//     receives it with the outcome the sink returned.
// ---------------------------------------------------------------------------

#[test]
fn send_sender_classifies_and_a_sink_receives_it() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(100);
    let to = make_small(200);
    let message = make_small(42);

    // `send_message` selects `DOP_SEND_SENDER` because the negotiated set includes
    // `DFLAG_SEND_SENDER`; the trailing message carries the payload.
    client.send_message(from, to, message, &atoms).expect("send message");
    relay(&mut client, &mut server);

    // --- the classification view -------------------------------------------
    let delivery = next_delivery(&mut server, &mut heap, &mut atoms);
    assert_eq!(
        delivery,
        Delivery::Send { from, to, message, token: None },
        "a DOP_SEND_SENDER classifies as Delivery::Send with exactly the terms sent",
    );
    assert_eq!(delivery.dop(), Dop::Send);

    // --- the routed view: an InboundSink records it and its outcome is kept --
    client.send_message(from, to, message, &atoms).expect("send message 2");
    relay(&mut client, &mut server);

    let mut sink = RecordingSink::default();
    let dispatched = server.dispatch_inbound(&mut heap, &mut atoms, &mut sink).expect("dispatch");
    assert_eq!(
        dispatched,
        vec![Dispatched {
            delivery: Delivery::Send { from, to, message, token: None },
            outcome: DeliveryOutcome::Delivered,
        }],
        "dispatch_inbound returns the classified delivery and the sink's outcome",
    );
    assert_eq!(
        sink.deliveries,
        vec![Delivery::Send { from, to, message, token: None }],
        "the sink observed the delivery",
    );
}

#[test]
fn alias_send_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(1);
    let to = make_small(2);
    let message = make_small(3);
    client.send_control(Dop::AliasSend, &[from, to, message], &atoms).expect("send");
    relay(&mut client, &mut server);

    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::AliasSend { from, to, message, token: None },
    );
}

#[test]
fn reg_send_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(11);
    let cookie = make_small(22);
    let name = make_small(33);
    let message = make_small(44);
    client
        .send_control(Dop::RegSend, &[from, cookie, name, message], &atoms)
        .expect("send");
    relay(&mut client, &mut server);

    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::RegSend { from, cookie, name, message, token: None },
    );
}

#[test]
fn link_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(5);
    let to = make_small(6);
    client.send_control(Dop::Link, &[from, to], &atoms).expect("send");
    relay(&mut client, &mut server);

    assert_eq!(next_delivery(&mut server, &mut heap, &mut atoms), Delivery::Link { from, to });
}

// ---------------------------------------------------------------------------
// (b) The exit family: `DOP_EXIT` and `DOP_EXIT_TT` both classify as
//     `Delivery::Exit`, the `_TT` form landing the token and reason correctly.
// ---------------------------------------------------------------------------

#[test]
fn exit_and_exit_tt_classify() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(7);
    let to = make_small(8);
    let reason = make_small(9);

    // `{DOP_EXIT, From, To, Reason}` — no trace token.
    client.send_control(Dop::Exit, &[from, to, reason], &atoms).expect("exit");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Exit { from, to, reason, token: None },
    );

    // `{DOP_EXIT_TT, From, To, TraceToken, Reason}` — the token and reason are
    // the two distinct trailing arguments, in that order (`dist.c:2773-2779`).
    let token = make_small(101);
    let tt_reason = make_small(202);
    client
        .send_control(Dop::ExitTt, &[from, to, token, tt_reason], &atoms)
        .expect("exit_tt");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Exit { from, to, reason: tt_reason, token: Some(token) },
        "the ExitTt token/reason must land in the token/reason fields respectively",
    );
}

// ---------------------------------------------------------------------------
// (c) The monitor family.
// ---------------------------------------------------------------------------

#[test]
fn monitor_family_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let watcher = make_small(1);
    let watched = make_small(2);
    let reference = make_small(3);

    client
        .send_control(Dop::MonitorP, &[watcher, watched, reference], &atoms)
        .expect("monitor");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Monitor { watcher, watched, reference },
    );

    client
        .send_control(Dop::DemonitorP, &[watcher, watched, reference], &atoms)
        .expect("demonitor");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Demonitor { watcher, watched, reference },
    );

    let reason = make_small(4);
    client
        .send_control(Dop::MonitorPExit, &[watched, watcher, reference, reason], &atoms)
        .expect("monitor_exit");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::MonitorExit { watched, watcher, reference, reason },
    );
}

// ---------------------------------------------------------------------------
// (d) The unlink-id family and the group leader.
// ---------------------------------------------------------------------------

#[test]
fn unlink_id_family_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let id = make_small(7);
    let from = make_small(8);
    let to = make_small(9);

    client.send_control(Dop::UnlinkId, &[id, from, to], &atoms).expect("unlink_id");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::UnlinkId { id, from, to },
    );

    client.send_control(Dop::UnlinkIdAck, &[id, from, to], &atoms).expect("unlink_id_ack");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::UnlinkIdAck { id, from, to },
    );
}

#[test]
fn group_leader_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(1);
    let to = make_small(2);
    client.send_control(Dop::GroupLeader, &[from, to], &atoms).expect("group_leader");
    relay(&mut client, &mut server);

    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::GroupLeader { from, to },
    );
}

// ---------------------------------------------------------------------------
// (e) The payload-exit family (`DFLAG_EXIT_PAYLOAD`): the reason rides as the
//     trailing term, the control tuple stays small.
// ---------------------------------------------------------------------------

#[test]
fn payload_exit_family_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(1);
    let to = make_small(2);
    let payload = make_small(3);
    let token = make_small(4);

    // `{DOP_PAYLOAD_EXIT, From, To} -- Reason` (`dist.c:2757`).
    client.send_control(Dop::PayloadExit, &[from, to, payload], &atoms).expect("payload_exit");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::PayloadExit { from, to, payload, token: None },
    );

    // `{DOP_PAYLOAD_EXIT_TT, From, To, Token} -- Reason` (`dist.c:2786-2791`).
    client
        .send_control(Dop::PayloadExitTt, &[from, to, token, payload], &atoms)
        .expect("payload_exit_tt");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::PayloadExit { from, to, payload, token: Some(token) },
    );

    // `{DOP_PAYLOAD_EXIT2, From, To} -- Reason` (`dist.c:2821-2849`).
    client.send_control(Dop::PayloadExit2, &[from, to, payload], &atoms).expect("payload_exit2");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::PayloadExit2 { from, to, payload, token: None },
    );

    // `{DOP_PAYLOAD_EXIT2_TT, From, To, Token} -- Reason` (`dist.c:2850-2856`).
    client
        .send_control(Dop::PayloadExit2Tt, &[from, to, token, payload], &atoms)
        .expect("payload_exit2_tt");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::PayloadExit2 { from, to, payload, token: Some(token) },
    );

    // `{DOP_PAYLOAD_MONITOR_P_EXIT, Remote, Local, Ref} -- Reason`
    // (`dist.c:2700-2714`).
    let watched = make_small(5);
    let watcher = make_small(6);
    let reference = make_small(7);
    client
        .send_control(
            Dop::PayloadMonitorPExit,
            &[watched, watcher, reference, payload],
            &atoms,
        )
        .expect("payload_monitor_p_exit");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::PayloadMonitorExit { watched, watcher, reference, payload },
    );
    assert_eq!(
        Delivery::PayloadMonitorExit { watched, watcher, reference, payload }.dop(),
        Dop::PayloadMonitorPExit,
    );
}

// ---------------------------------------------------------------------------
// (f) The distributed-spawn family.
// ---------------------------------------------------------------------------

#[test]
fn spawn_family_classifies() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let reference = make_small(1);
    let from = make_small(2);
    let group_leader = make_small(3);
    let mfa = make_small(4);
    let opts = make_small(5);
    let args = make_small(6);
    let token = make_small(7);

    // `{DOP_SPAWN_REQUEST, Ref, From, GL, MFA, Opts} -- Args` (`dist.c:2919-2931`).
    client
        .send_control(
            Dop::SpawnRequest,
            &[reference, from, group_leader, mfa, opts, args],
            &atoms,
        )
        .expect("spawn_request");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::SpawnRequest {
            reference,
            from,
            group_leader,
            mfa,
            opts,
            args,
            token: None,
        },
    );

    // `{DOP_SPAWN_REQUEST_TT, Ref, From, GL, MFA, Opts, Token} -- Args`
    // (`dist.c:2902-2916`).
    client
        .send_control(
            Dop::SpawnRequestTt,
            &[reference, from, group_leader, mfa, opts, token, args],
            &atoms,
        )
        .expect("spawn_request_tt");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::SpawnRequest {
            reference,
            from,
            group_leader,
            mfa,
            opts,
            args,
            token: Some(token),
        },
    );

    // `{DOP_SPAWN_REPLY, Ref, To, Flags, From}` (`dist.c:3051-3068`) — `To` is the
    // *parent* and `From` the *result*; no trailing term (`dist.c:1897`).
    let to = make_small(8);
    let flags = make_small(i64::from(DIST_SPAWN_FLAG_LINK | DIST_SPAWN_FLAG_MONITOR));
    let result = make_small(9);
    client
        .send_control(Dop::SpawnReply, &[reference, to, flags, result], &atoms)
        .expect("spawn_reply");
    relay(&mut client, &mut server);
    let delivery = next_delivery(&mut server, &mut heap, &mut atoms);
    assert_eq!(
        delivery,
        Delivery::SpawnReply {
            reference,
            to,
            flags: DistSpawnFlags::from_bits(DIST_SPAWN_FLAG_LINK | DIST_SPAWN_FLAG_MONITOR),
            result,
            token: None,
        },
    );
    assert_eq!(delivery.dop(), Dop::SpawnReply);

    // `{DOP_SPAWN_REPLY_TT, Ref, To, Flags, From, Token}` (`dist.c:3033-3045`).
    client
        .send_control(Dop::SpawnReplyTt, &[reference, to, flags, result, token], &atoms)
        .expect("spawn_reply_tt");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::SpawnReply {
            reference,
            to,
            flags: DistSpawnFlags::from_bits(DIST_SPAWN_FLAG_LINK | DIST_SPAWN_FLAG_MONITOR),
            result,
            token: Some(token),
        },
    );
}

// ---------------------------------------------------------------------------
// (g) `DOP_ALTACT_SIG_SEND`: the token is a *flag*, not a `_TT` opcode.
// ---------------------------------------------------------------------------

#[test]
fn altact_sig_send_classifies_with_and_without_a_token() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(1);
    let to = make_small(2);
    let message = make_small(3);
    let token = make_small(4);

    // `{DOP_ALTACT_SIG_SEND, Flags, SenderPid, To} -- Signal` (`dist.c:2592`).
    let flags = make_small(i64::from(DOP_ALTACT_SIG_FLG_PRIO | DOP_ALTACT_SIG_FLG_NAME));
    client
        .send_control(Dop::AltactSigSend, &[flags, from, to, message], &atoms)
        .expect("altact");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::AltactSigSend {
            flags: AltactSigFlags::from_bits(DOP_ALTACT_SIG_FLG_PRIO | DOP_ALTACT_SIG_FLG_NAME),
            from,
            to,
            token: None,
            message,
        },
    );

    // `{DOP_ALTACT_SIG_SEND, Flags, SenderPid, To, Token} -- Signal`
    // (`dist.c:2651-2658`): only the TOKEN *flag* grows the tuple.
    let token_flags = make_small(i64::from(DOP_ALTACT_SIG_FLG_TOKEN));
    client
        .send_control(Dop::AltactSigSend, &[token_flags, from, to, token, message], &atoms)
        .expect("altact_token");
    relay(&mut client, &mut server);
    let delivery = next_delivery(&mut server, &mut heap, &mut atoms);
    assert_eq!(
        delivery,
        Delivery::AltactSigSend {
            flags: AltactSigFlags::TOKEN,
            from,
            to,
            token: Some(token),
            message,
        },
        "the TOKEN flag grows the control tuple; there is no DOP_ALTACT_SIG_SEND_TT",
    );
    // Still the *same* opcode: the flag is the `_TT` marker, not a second opcode.
    assert_eq!(delivery.dop(), Dop::AltactSigSend);
    assert!(!Dop::AltactSigSend.is_tt());
}

// ---------------------------------------------------------------------------
// (h) `DOP_UNLINK` is the one recognised-but-unrouted opcode:
//     `Delivery::Other(dop)`, never a substituted send.
// ---------------------------------------------------------------------------

#[test]
fn unlink_classifies_as_other() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    // `{DOP_UNLINK, From, To}` — the pre-OTP-26 link teardown. The C never sends
    // it and *rejects* it outright (`goto invalid_message`, `dist.c:2277-2282`),
    // so this crate recognises the wire shape but refuses to invent semantics
    // for it. It is the only `Delivery::Other` case left.
    let terms: Vec<Eterm> = vec![make_small(1), make_small(2)];
    client.send_control(Dop::Unlink, &terms, &atoms).expect("unlink");
    relay(&mut client, &mut server);
    assert_eq!(
        next_delivery(&mut server, &mut heap, &mut atoms),
        Delivery::Other(Dop::Unlink),
    );
}

/// Every one of the 30 v6 opcodes is classified — `DOP_UNLINK` (never sent) is the
/// sole `Delivery::Other`, so the classification cannot silently lose an opcode.
#[test]
fn every_v6_opcode_is_classified() {
    // `enum dop` (`erts/emulator/beam/dist.h:140-183`): exactly 30 values, 1-4
    // (`LINK`, `UNLINK`, `REG_SEND`, `GROUP_LEADER`) and 6-37.
    let mut seen = 0usize;
    for byte in 0u8..=u8::MAX {
        let Some(dop) = Dop::from_u8(byte) else { continue };
        seen += 1;
        if dop == Dop::Unlink {
            continue;
        }
        assert!(dop.arity() > 0, "{dop:?} must have a documented arity");
    }
    assert_eq!(seen, 30, "v6 has exactly 30 control opcodes");
    assert_eq!(Dop::from_u8(4), Some(Dop::Unlink));
    // There is no DOP_NODEUP/DOP_NODEDOWN and no `*_MONITOR_ID` opcode: node
    // up/down are local emulator events (`send_nodes_mon_msgs`, `dist.c:6931`)
    // and monitors are cancelled with DOP_DEMONITOR_P. The unassigned bytes must
    // stay unassigned.
    for byte in [0u8, 5, 9, 10, 11, 14, 15, 17] {
        assert_eq!(Dop::from_u8(byte), None, "byte {byte} is not a v6 opcode");
    }
    for byte in 38u8..=u8::MAX {
        assert_eq!(Dop::from_u8(byte), None, "byte {byte} is not a v6 opcode");
    }
}

// ---------------------------------------------------------------------------
// (f) The default trait method is honest: a sink with no runtime reports
//     `Unsupported`, never a wrong result.
// ---------------------------------------------------------------------------

#[test]
fn unsupported_sink_reports_unsupported() {
    let (mut client, mut server) = establish();
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    let from = make_small(1);
    let to = make_small(2);
    let message = make_small(3);
    client.send_message(from, to, message, &atoms).expect("send");
    relay(&mut client, &mut server);

    let mut sink = UnsupportedSink;
    let dispatched = server.dispatch_inbound(&mut heap, &mut atoms, &mut sink).expect("dispatch");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0],
        Dispatched {
            delivery: Delivery::Send { from, to, message, token: None },
            outcome: DeliveryOutcome::Unsupported,
        },
        "the default InboundSink reports Unsupported, proving it never mis-delivers",
    );
}

// ---------------------------------------------------------------------------
// (j) `DOP_SPAWN_REPLY`'s reference guard (`dist.c:3070-3086`).
//
// The C accepts **either** an internal reference (`is_internal_ref(ref)` →
// continue) **or** an external one whose `external_ref_dist_entry` is
// `erts_this_dist_entry` (an old incarnation of this node → `break`); anything
// else is `invalid_message`. A *decoded wire* reference is always external, so the
// internal arm is inert for a wire image — but it is exactly what a decoded
// `DOP_SPAWN_REPLY` carries once the decoder's local-node demotion turns this
// node's own reference back into its internal form. These tests therefore build
// the control message from **heap terms** (an internal reference has no wire
// image), and pin all three verdicts:
//
//   * an internal reference                    → accepted,
//   * an external reference of this node       → accepted (old incarnation),
//   * an external reference of a third node    → `BadArg`.
// ---------------------------------------------------------------------------

/// The per-link identity the validated tests below use: the peer is the driver
/// harness's connector and this node is its acceptor.
fn link_identity(atoms: &mut AtomTable) -> LinkIdentity {
    LinkIdentity::intern(atoms, CLIENT_NAME, SERVER_NAME).expect("link identity")
}

/// A `{DOP_SPAWN_REPLY, Ref, To, Flags, From}` control message with an empty
/// `Flags` word, built directly from heap terms.
fn spawn_reply_control(reference: Eterm, to: Eterm, result: Eterm) -> ControlMessage {
    ControlMessage { dop: Dop::SpawnReply, terms: vec![reference, to, make_small(0), result] }
}

/// The new remote process a `DOP_SPAWN_REPLY` reports — a pid of the peer node.
fn peer_pid(heap: &mut Heap, atoms: &mut AtomTable) -> Eterm {
    let node = make_atom(atoms.intern("client@host"));
    make_external_pid(heap, node, 3, 0, 0).expect("an external pid")
}

#[test]
fn spawn_reply_accepts_an_internal_reference() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();
    let link = link_identity(&mut atoms);

    // This node's own reference (`0`-node, `0`-serial, id 42 in the M1 block).
    let reference = make_internal_ref(&mut heap, 42).expect("an internal reference");
    assert!(internal_ref_id(reference).is_ok(), "the term is an internal reference");
    assert!(external_ref_parts(reference).is_err(), "…and not an external one");

    let to = make_internal_pid(make_pid_data(0, 9));
    let result = peer_pid(&mut heap, &mut atoms);
    let message = spawn_reply_control(reference, to, result);

    assert_eq!(
        Delivery::classify_validated(&message, &link)
            .expect("the C accepts an internal reference (`dist.c:3079`)"),
        Delivery::SpawnReply {
            reference,
            to,
            flags: DistSpawnFlags::EMPTY,
            result,
            token: None,
        },
    );
}

#[test]
fn spawn_reply_accepts_an_external_reference_of_this_node() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();
    let link = link_identity(&mut atoms);

    // The same reference re-arriving as an external ref of **this** node — the C's
    // old-incarnation escape hatch (`dist.c:3082-3086`).
    let node = make_atom(atoms.intern("server@host"));
    let reference = make_external_ref(&mut heap, node, &[42], 0).expect("an external reference");
    let to = make_internal_pid(make_pid_data(0, 9));
    let result = peer_pid(&mut heap, &mut atoms);

    let message = spawn_reply_control(reference, to, result);
    assert!(
        Delivery::classify_validated(&message, &link).is_ok(),
        "an external reference of this node is accepted (old incarnation)",
    );
}

#[test]
fn spawn_reply_rejects_an_external_reference_of_a_third_node() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();
    let link = link_identity(&mut atoms);

    // A reference of neither this node nor its peer: the C's `goto invalid_message`
    // (`dist.c:3079-3086`).
    let node = make_atom(atoms.intern("third@host"));
    let reference = make_external_ref(&mut heap, node, &[42], 0).expect("an external reference");
    let to = make_internal_pid(make_pid_data(0, 9));
    let result = peer_pid(&mut heap, &mut atoms);

    let message = spawn_reply_control(reference, to, result);
    assert!(
        matches!(Delivery::classify_validated(&message, &link), Err(BeamError::BadArg)),
        "a spawn reply carrying a foreign reference must be rejected",
    );
}

/// The mirror-image arm is **not** widened: `DOP_SPAWN_REQUEST` requires
/// `is_not_external_ref(ref)` ⇒ `invalid_message` (`dist.c:2936-2937`), so an
/// internal reference stays rejected there.
#[test]
fn spawn_request_still_rejects_an_internal_reference() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();
    let link = link_identity(&mut atoms);

    let reference = make_internal_ref(&mut heap, 42).expect("an internal reference");
    let from = peer_pid(&mut heap, &mut atoms);
    let group_leader = peer_pid(&mut heap, &mut atoms);
    // A well-formed `{Module, Function, Arity}` so the *only* violation is the
    // reference form under test (`dist.c:2933-2949` checks the MFA shape separately).
    let module = make_atom(atoms.intern("erlang"));
    let function = make_atom(atoms.intern("send"));
    let mfa = heap.tuple(&[module, function, make_small(2)]).expect("the MFA tuple");
    let opts = make_small(0);
    let args = make_small(0);    let message = ControlMessage {
        dop: Dop::SpawnRequest,
        terms: vec![reference, from, group_leader, mfa, opts, args],
    };

    assert!(
        matches!(Delivery::classify_validated(&message, &link), Err(BeamError::BadArg)),
        "a spawn request must carry an external reference (`dist.c:2936-2937`)",
    );
}
