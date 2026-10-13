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

//! The full **inbound signal-family** surface of a two-node distribution session,
//! proven end-to-end over a real loopback socket for compatibility contract **A.5**.
//!
//! `tests/two_node_loopback.rs` already proves the *send* family
//! (`DOP_SEND*`/`DOP_REG_SEND`) and the monitor signals (`DOP_MONITOR_P`,
//! `DOP_MONITOR_P_EXIT`, `DOP_PAYLOAD_MONITOR_P_EXIT`) between two named in-process
//! endpoints. This file completes the picture: it drives every remaining control
//! opcode a live session uses — the *link* family (`DOP_LINK`, `DOP_UNLINK_ID`,
//! `DOP_UNLINK_ID_ACK`), the *exit* family (`DOP_EXIT`/`DOP_EXIT2` and their
//! `DFLAG_EXIT_PAYLOAD` forms), the *demonitor* and *group-leader* signals, the
//! *alias send* and *alternative-activation* signals, and the *distributed spawn*
//! pair (`DOP_SPAWN_REQUEST`/`DOP_SPAWN_REPLY`) — each asserted through the typed
//! [`Delivery`] variant and its fields, with the link / monitor / exit families
//! exercised **in both directions** (roles swapped) from the same pair of
//! established connections.
//!
//! The harness mirrors `tests/two_node_loopback.rs`: a real [`TcpListener`] on
//! `127.0.0.1:0` runs only the `'N'` handshake on a background thread, both live
//! [`Connection`]s are then owned by the test thread and driven sequentially, and
//! every wait is bounded by [`TEST_TIMEOUT`]. One test drives the pair through a spy
//! [`Transport`] so it can assert that a real `'D'` distribution header crossed the
//! socket in both directions.
//!
//! Small integers model pids, references and ids: the distribution *value* layer
//! treats those fields as opaque terms and, unlike a boxed term, an immediate
//! compares `==` across the two endpoints' heaps. A boxed field (an `MFA` tuple, a
//! list) is compared through its encoded image instead (see [`body_image`]).
//! `DOP_UNLINK` is deliberately **not** covered: the C never sends it and rejects it
//! outright (`dist.c:2277-2282`), so it carries no signal semantics to assert (it
//! classifies as [`Delivery::Other`]).

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::VERSION_MAGIC;
use beam_dist::dist_header::DIST_HEADER;
use beam_dist::flags::{
    Dop, DIST_SPAWN_FLAG_LINK, DIST_SPAWN_FLAG_MONITOR, DOP_ALTACT_SIG_FLG_EXIT,
    DOP_ALTACT_SIG_FLG_PRIO, DOP_ALTACT_SIG_FLG_TOKEN,
};
use beam_dist::transport::{Read, Transport, Write};
use beam_dist::{AltactSigFlags, Connection, Delivery, DistSpawnFlags, TcpListener, TcpTransport};
use beam_term::prelude::{make_small, Eterm, NIL};
use beam_term::AtomTable;

/// The shared cookie both endpoints authenticate with.
const COOKIE: &[u8] = b"two-node-session-cookie";
/// Node A — the accepting side (the distribution **server** role).
const ALPHA_NAME: &[u8] = b"alpha@localhost";
/// Node A's creation.
const ALPHA_CREATION: u32 = 1;
/// Node B — the connecting side (the distribution **client** role).
const BETA_NAME: &[u8] = b"beta@localhost";
/// Node B's creation.
const BETA_CREATION: u32 = 2;

/// A stand-in for node A's own pid.
const ALPHA_PID: i64 = 1001;
/// A stand-in for node B's own pid.
const BETA_PID: i64 = 2002;
/// A stand-in for a monitor reference.
const MONITOR_REF: i64 = 3003;
/// A stand-in for an unlink id (`DOP_UNLINK_ID`/`_ACK`).
const UNLINK_ID: i64 = 4004;
/// A stand-in for an exit reason term.
const EXIT_REASON: i64 = 5005;
/// A stand-in for a trace token (`_TT` / `DOP_ALTACT_SIG_FLG_TOKEN`).
const TRACE_TOKEN: i64 = 6006;
/// A stand-in for a group-leader pid.
const GROUP_LEADER_PID: i64 = 7007;
/// A stand-in for a process alias reference.
const ALIAS_REF: i64 = 8008;
/// A stand-in for a distributed-spawn reference.
const SPAWN_REF: i64 = 9009;
/// A stand-in for the pid a remote spawn returned.
const SPAWN_RESULT_PID: i64 = 10010;

/// A finite read timeout, so a stalled or broken link fails instead of hanging.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

/// The body-form external term image of `term` (no `VERSION_MAGIC`).
///
/// `Eterm` pointers are heap-relative and atom indices are table-local, so a term
/// that crossed the wire is compared to the sent term through its encoded image
/// (the codec writes an atom's *name*, not its table index). This is the same
/// device `tests/two_node_loopback.rs` uses.
fn body_image(term: Eterm, atoms: &AtomTable) -> Vec<u8> {
    beam_codec::encode_term_body(term, atoms).expect("encode term body")
}

/// Builds a proper list of `elems` (in order) on `heap`.
fn list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut list = NIL;
    for &elem in elems.iter().rev() {
        list = heap.cons(elem, list).expect("cons");
    }
    list
}

/// The two live endpoints of one loopback link.
struct Pair<T: Transport> {
    /// Node A (`alpha@localhost`), the acceptor.
    alpha: Connection<T>,
    /// Node B (`beta@localhost`), the connector.
    beta: Connection<T>,
}

/// Both endpoints plus the per-endpoint heap and atom table used to build outgoing
/// terms and to decode incoming ones.
struct Ends<T: Transport> {
    alpha: Connection<T>,
    beta: Connection<T>,
    alpha_atoms: AtomTable,
    beta_atoms: AtomTable,
    alpha_heap: Heap,
    beta_heap: Heap,
}

impl<T: Transport> Ends<T> {
    /// Wraps an established pair with fresh heaps and atom tables.
    fn new(pair: Pair<T>) -> Ends<T> {
        Ends {
            alpha: pair.alpha,
            beta: pair.beta,
            alpha_atoms: AtomTable::new(),
            beta_atoms: AtomTable::new(),
            alpha_heap: Heap::new(1 << 16).expect("alpha heap"),
            beta_heap: Heap::new(1 << 16).expect("beta heap"),
        }
    }

    /// Sends one control message from α to β.
    fn alpha_send(&mut self, dop: Dop, terms: &[Eterm]) -> beam_common::Result<()> {
        self.alpha.send_control(dop, terms, &self.alpha_atoms)
    }

    /// Sends one control message from β to α.
    fn beta_send(&mut self, dop: Dop, terms: &[Eterm]) -> beam_common::Result<()> {
        self.beta.send_control(dop, terms, &self.beta_atoms)
    }

    /// Sends `t` from α as a control message whose terms are the small ints `ints`.
    ///
    /// A small integer is an immediate — it needs no heap and no atom table entry —
    /// so a stand-in pid/ref/id can be built directly here rather than on the
    /// sender's heap.
    fn alpha_send_ints(&mut self, dop: Dop, ints: &[i64]) -> beam_common::Result<()> {
        let terms: Vec<Eterm> = ints.iter().copied().map(make_small).collect();
        self.alpha_send(dop, &terms)
    }

    /// Sends `t` from β as a control message whose terms are the small ints `ints`.
    fn beta_send_ints(&mut self, dop: Dop, ints: &[i64]) -> beam_common::Result<()> {
        let terms: Vec<Eterm> = ints.iter().copied().map(make_small).collect();
        self.beta_send(dop, &terms)
    }

    /// Receives and classifies one inbound message at α.
    fn recv_at_alpha(&mut self) -> Delivery {
        self.alpha
            .recv_delivery(&mut self.alpha_heap, &mut self.alpha_atoms)
            .expect("α receives and classifies the signal")
    }

    /// Receives and classifies one inbound message at β.
    fn recv_at_beta(&mut self) -> Delivery {
        self.beta
            .recv_delivery(&mut self.beta_heap, &mut self.beta_atoms)
            .expect("β receives and classifies the signal")
    }
}

/// Binds a loopback listener, completes the `'N'` handshake on the acceptor thread
/// and returns both established endpoints.
///
/// Only the handshake runs on the background thread; it returns the live
/// [`Connection`] as soon as the link is established, so the test thread can drive
/// both endpoints afterwards.
fn connect_pair() -> Pair<TcpTransport> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let acceptor = thread::spawn(move || -> beam_common::Result<Connection<TcpTransport>> {
        let mut transport = listener.accept()?;
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        Connection::server(transport, ALPHA_NAME, COOKIE, ALPHA_CREATION)
    });

    let mut transport = TcpTransport::connect(&addr.to_string()).expect("connect to the peer");
    transport.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
    let beta = Connection::client_with_creation(transport, BETA_NAME, COOKIE, BETA_CREATION)
        .expect("connecting handshake");
    let alpha = acceptor
        .join()
        .expect("acceptor thread")
        .expect("accepting handshake");

    Pair { alpha, beta }
}

// ---------------------------------------------------------------------------
// A socket spy, so one test can prove a real `'D'` distribution header crossed the
// wire rather than merely that a delivery classified.
// ---------------------------------------------------------------------------

/// One direction's worth of recorded transfers.
///
/// `framing::read_packet`/`write_packet` emit the length prefix and then the whole
/// body in one transition, so a frame contributes two recorded blocks; the body
/// block begins with the frame's first tag byte.
#[derive(Clone, Default)]
struct WireLog(Arc<Mutex<Vec<Vec<u8>>>>);

impl WireLog {
    fn new() -> WireLog {
        WireLog::default()
    }

    fn record(&self, bytes: &[u8]) {
        self.0.lock().expect("wire log").push(bytes.to_vec());
    }

    /// Whether any recorded block is a `'D'` distribution-header frame body
    /// (`VERSION_MAGIC + DIST_HEADER`), i.e. a real control message crossed the wire.
    fn saw_dist_header(&self) -> bool {
        self.0
            .lock()
            .expect("wire log")
            .iter()
            .any(|b| b.len() >= 2 && b[0] == VERSION_MAGIC && b[1] == DIST_HEADER)
    }
}

/// A [`TcpTransport`] that logs every byte read from and written to the socket.
struct SpyTransport {
    inner: TcpTransport,
    reads: WireLog,
    writes: WireLog,
}

impl Read for SpyTransport {
    fn read_exact(&mut self, buf: &mut [u8]) -> beam_common::Result<()> {
        self.inner.read_exact(buf)?;
        self.reads.record(buf);
        Ok(())
    }
}

impl Write for SpyTransport {
    fn write_all(&mut self, buf: &[u8]) -> beam_common::Result<()> {
        self.writes.record(buf);
        self.inner.write_all(buf)
    }

    fn flush(&mut self) -> beam_common::Result<()> {
        self.inner.flush()
    }
}

/// The two established spy endpoints plus the four wire logs they write to.
struct SpySession {
    ends: Ends<SpyTransport>,
    server_reads: WireLog,
    server_writes: WireLog,
    client_reads: WireLog,
    client_writes: WireLog,
}

/// The [`connect_pair`] harness with a spy [`Transport`] on each end, so a test can
/// inspect the bytes that really crossed the socket.
fn spy_pair() -> SpySession {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let server_reads = WireLog::new();
    let server_writes = WireLog::new();
    let client_reads = WireLog::new();
    let client_writes = WireLog::new();

    let sr = server_reads.clone();
    let sw = server_writes.clone();
    let acceptor = thread::spawn(move || -> beam_common::Result<Connection<SpyTransport>> {
        let mut transport = listener.accept()?;
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        Connection::server(
            SpyTransport {
                inner: transport,
                reads: sr,
                writes: sw,
            },
            ALPHA_NAME,
            COOKIE,
            ALPHA_CREATION,
        )
    });

    let mut inner = TcpTransport::connect(&addr.to_string()).expect("connect to the peer");
    inner.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
    let beta = Connection::client_with_creation(
        SpyTransport {
            inner,
            reads: client_reads.clone(),
            writes: client_writes.clone(),
        },
        BETA_NAME,
        COOKIE,
        BETA_CREATION,
    )
    .expect("connecting handshake");
    let alpha = acceptor
        .join()
        .expect("acceptor thread")
        .expect("accepting handshake");

    SpySession {
        ends: Ends::new(Pair { alpha, beta }),
        server_reads,
        server_writes,
        client_reads,
        client_writes,
    }
}

// ---------------------------------------------------------------------------
// (1) The link family: `DOP_LINK`, both directions.
// ---------------------------------------------------------------------------

#[test]
fn link_signals_cross_the_loopback_in_both_directions() {
    let mut ends = Ends::new(connect_pair());

    // α links with β (`{DOP_LINK, From, To}`)...
    ends.alpha_send_ints(Dop::Link, &[ALPHA_PID, BETA_PID])
        .expect("α sends DOP_LINK");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::Link);
    assert_eq!(
        got,
        Delivery::Link { from: make_small(ALPHA_PID), to: make_small(BETA_PID) },
        "β classifies α's link request as a link α holds on β",
    );

    // ...and β links with α (roles swapped).
    ends.beta_send_ints(Dop::Link, &[BETA_PID, ALPHA_PID])
        .expect("β sends DOP_LINK");
    let got = ends.recv_at_alpha();
    assert_eq!(got.dop(), Dop::Link);
    assert_eq!(
        got,
        Delivery::Link { from: make_small(BETA_PID), to: make_small(ALPHA_PID) },
        "α classifies β's link request as a link β holds on α",
    );
}

// ---------------------------------------------------------------------------
// (2) The OTP-26+ link protocol: `DOP_UNLINK_ID` / `DOP_UNLINK_ID_ACK`, both ways.
// ---------------------------------------------------------------------------

#[test]
fn unlink_id_and_ack_cross_the_loopback_in_both_directions() {
    let mut ends = Ends::new(connect_pair());

    // β → α: `{DOP_UNLINK_ID, Id, From, To}`.
    ends.beta_send_ints(Dop::UnlinkId, &[UNLINK_ID, BETA_PID, ALPHA_PID])
        .expect("β sends DOP_UNLINK_ID");
    let got = ends.recv_at_alpha();
    assert_eq!(got.dop(), Dop::UnlinkId);
    assert_eq!(
        got,
        Delivery::UnlinkId {
            id: make_small(UNLINK_ID),
            from: make_small(BETA_PID),
            to: make_small(ALPHA_PID),
        },
        "the unlink id, the remote sender and the local target all crossed intact",
    );

    // α → β: the acknowledgement `{DOP_UNLINK_ID_ACK, Id, From, To}`.
    ends.alpha_send_ints(Dop::UnlinkIdAck, &[UNLINK_ID, ALPHA_PID, BETA_PID])
        .expect("α sends DOP_UNLINK_ID_ACK");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::UnlinkIdAck);
    assert_eq!(
        got,
        Delivery::UnlinkIdAck {
            id: make_small(UNLINK_ID),
            from: make_small(ALPHA_PID),
            to: make_small(BETA_PID),
        },
        "the acknowledgement echoes the same unlink id",
    );

    // The reverse directions, so neither opcode is proven one-way only.
    ends.alpha_send_ints(Dop::UnlinkId, &[UNLINK_ID + 1, ALPHA_PID, BETA_PID])
        .expect("α sends DOP_UNLINK_ID");
    assert_eq!(
        ends.recv_at_beta(),
        Delivery::UnlinkId {
            id: make_small(UNLINK_ID + 1),
            from: make_small(ALPHA_PID),
            to: make_small(BETA_PID),
        },
    );
    ends.beta_send_ints(Dop::UnlinkIdAck, &[UNLINK_ID + 1, BETA_PID, ALPHA_PID])
        .expect("β sends DOP_UNLINK_ID_ACK");
    assert_eq!(
        ends.recv_at_alpha(),
        Delivery::UnlinkIdAck {
            id: make_small(UNLINK_ID + 1),
            from: make_small(BETA_PID),
            to: make_small(ALPHA_PID),
        },
    );
}

// ---------------------------------------------------------------------------
// (3) The monitor family's cancel signal: `DOP_DEMONITOR_P`, both directions.
// ---------------------------------------------------------------------------

#[test]
fn demonitor_signals_cross_the_loopback_in_both_directions() {
    let mut ends = Ends::new(connect_pair());

    // α cancels the monitor it holds on β (`{DOP_DEMONITOR_P, Remote, Local, Ref}`).
    ends.alpha_send_ints(Dop::DemonitorP, &[ALPHA_PID, BETA_PID, MONITOR_REF])
        .expect("α sends DOP_DEMONITOR_P");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::DemonitorP);
    assert_eq!(
        got,
        Delivery::Demonitor {
            watcher: make_small(ALPHA_PID),
            watched: make_small(BETA_PID),
            reference: make_small(MONITOR_REF),
        },
        "β classifies α's demonitor as the cancellation of α's monitor",
    );

    // β cancels the monitor it holds on α (roles swapped).
    ends.beta_send_ints(Dop::DemonitorP, &[BETA_PID, ALPHA_PID, MONITOR_REF])
        .expect("β sends DOP_DEMONITOR_P");
    let got = ends.recv_at_alpha();
    assert_eq!(got.dop(), Dop::DemonitorP);
    assert_eq!(
        got,
        Delivery::Demonitor {
            watcher: make_small(BETA_PID),
            watched: make_small(ALPHA_PID),
            reference: make_small(MONITOR_REF),
        },
        "α classifies β's demonitor as the cancellation of β's monitor",
    );
}

// ---------------------------------------------------------------------------
// (4) The exit family: `DOP_EXIT` / `DOP_EXIT2` (inline reason), both directions.
// ---------------------------------------------------------------------------

#[test]
fn exit_and_exit2_signals_cross_the_loopback_in_both_directions() {
    let mut ends = Ends::new(connect_pair());

    // DOP_EXIT α → β (a trappable exit signal).
    ends.alpha_send_ints(Dop::Exit, &[ALPHA_PID, BETA_PID, EXIT_REASON])
        .expect("α sends DOP_EXIT");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::Exit);
    assert_eq!(
        got,
        Delivery::Exit {
            from: make_small(ALPHA_PID),
            to: make_small(BETA_PID),
            reason: make_small(EXIT_REASON),
            token: None,
        },
        "DOP_EXIT (not _TT) carries no trace token",
    );

    // DOP_EXIT β → α.
    ends.beta_send_ints(Dop::Exit, &[BETA_PID, ALPHA_PID, EXIT_REASON])
        .expect("β sends DOP_EXIT");
    assert_eq!(
        ends.recv_at_alpha(),
        Delivery::Exit {
            from: make_small(BETA_PID),
            to: make_small(ALPHA_PID),
            reason: make_small(EXIT_REASON),
            token: None,
        },
    );

    // DOP_EXIT2 α → β (an untrappable `exit/2` signal).
    ends.alpha_send_ints(Dop::Exit2, &[ALPHA_PID, BETA_PID, EXIT_REASON])
        .expect("α sends DOP_EXIT2");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::Exit2);
    assert_eq!(
        got,
        Delivery::Exit2 {
            from: make_small(ALPHA_PID),
            to: make_small(BETA_PID),
            reason: make_small(EXIT_REASON),
            token: None,
        },
        "DOP_EXIT2 classifies as the untrappable `exit/2` form",
    );

    // DOP_EXIT2 β → α.
    ends.beta_send_ints(Dop::Exit2, &[BETA_PID, ALPHA_PID, EXIT_REASON])
        .expect("β sends DOP_EXIT2");
    assert_eq!(
        ends.recv_at_alpha(),
        Delivery::Exit2 {
            from: make_small(BETA_PID),
            to: make_small(ALPHA_PID),
            reason: make_small(EXIT_REASON),
            token: None,
        },
    );
}

// ---------------------------------------------------------------------------
// (5) The `DFLAG_EXIT_PAYLOAD` forms: `DOP_PAYLOAD_EXIT` / `DOP_PAYLOAD_EXIT2`,
// where the reason rides as the trailing term after the control tuple.
// ---------------------------------------------------------------------------

#[test]
fn payload_exit_signals_cross_the_loopback_both_ways() {
    let mut ends = Ends::new(connect_pair());

    // `{DOP_PAYLOAD_EXIT, From, To} -- Reason` (α → β).
    ends.alpha_send_ints(Dop::PayloadExit, &[ALPHA_PID, BETA_PID, EXIT_REASON])
        .expect("α sends DOP_PAYLOAD_EXIT");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::PayloadExit);
    assert_eq!(
        got,
        Delivery::PayloadExit {
            from: make_small(ALPHA_PID),
            to: make_small(BETA_PID),
            payload: make_small(EXIT_REASON),
            token: None,
        },
        "the exit reason rode as the trailing payload term, not inline",
    );

    // `{DOP_PAYLOAD_EXIT2, From, To} -- Reason` (β → α, the untrappable form).
    ends.beta_send_ints(Dop::PayloadExit2, &[BETA_PID, ALPHA_PID, EXIT_REASON])
        .expect("β sends DOP_PAYLOAD_EXIT2");
    let got = ends.recv_at_alpha();
    assert_eq!(got.dop(), Dop::PayloadExit2);
    assert_eq!(
        got,
        Delivery::PayloadExit2 {
            from: make_small(BETA_PID),
            to: make_small(ALPHA_PID),
            payload: make_small(EXIT_REASON),
            token: None,
        },
        "the untrappable payload-exit carries its reason as the trailing term",
    );
}

// ---------------------------------------------------------------------------
// (6) The group-leader signal: `DOP_GROUP_LEADER`, both ways.
// ---------------------------------------------------------------------------

#[test]
fn group_leader_signals_cross_the_loopback_both_ways() {
    let mut ends = Ends::new(connect_pair());

    // `{DOP_GROUP_LEADER, From, To}` — α's process becomes β's group leader.
    ends.alpha_send_ints(Dop::GroupLeader, &[GROUP_LEADER_PID, BETA_PID])
        .expect("α sends DOP_GROUP_LEADER");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::GroupLeader);
    assert_eq!(
        got,
        Delivery::GroupLeader {
            from: make_small(GROUP_LEADER_PID),
            to: make_small(BETA_PID),
        },
        "β re-parents to the remote group leader α named",
    );

    // Roles swapped: β's process becomes α's group leader.
    ends.beta_send_ints(Dop::GroupLeader, &[GROUP_LEADER_PID, ALPHA_PID])
        .expect("β sends DOP_GROUP_LEADER");
    assert_eq!(
        ends.recv_at_alpha(),
        Delivery::GroupLeader {
            from: make_small(GROUP_LEADER_PID),
            to: make_small(ALPHA_PID),
        },
    );
}

// ---------------------------------------------------------------------------
// (7) The alias-send signal: `DOP_ALIAS_SEND`, both ways.
// ---------------------------------------------------------------------------

#[test]
fn alias_sends_cross_the_loopback_both_ways() {
    let mut ends = Ends::new(connect_pair());

    // `{DOP_ALIAS_SEND, From, To} -- Message` where `To` is an alias reference.
    ends.alpha_send_ints(Dop::AliasSend, &[ALPHA_PID, ALIAS_REF, 7])
        .expect("α sends DOP_ALIAS_SEND");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::AliasSend);
    assert_eq!(
        got,
        Delivery::AliasSend {
            from: make_small(ALPHA_PID),
            to: make_small(ALIAS_REF),
            message: make_small(7),
            token: None,
        },
        "an alias-addressed send keeps its alias target and its message",
    );

    // β → α.
    ends.beta_send_ints(Dop::AliasSend, &[BETA_PID, ALIAS_REF, 8])
        .expect("β sends DOP_ALIAS_SEND");
    assert_eq!(
        ends.recv_at_alpha(),
        Delivery::AliasSend {
            from: make_small(BETA_PID),
            to: make_small(ALIAS_REF),
            message: make_small(8),
            token: None,
        },
    );
}

// ---------------------------------------------------------------------------
// (8) The distributed-spawn pair: `DOP_SPAWN_REQUEST` (with its trailing argument
// list) answered by `DOP_SPAWN_REPLY` (with the spawn-flag word).
// ---------------------------------------------------------------------------

#[test]
fn spawn_request_and_reply_cross_the_loopback() {
    let mut ends = Ends::new(connect_pair());

    // The MFA tuple / options list / argument list are boxed terms, so they are built
    // on the SENDER's heap and compared through their encoded image.
    let mfa = ends
        .alpha_heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("mfa tuple");
    let opts = list(&mut ends.alpha_heap, &[make_small(5)]);
    let args = list(&mut ends.alpha_heap, &[make_small(6), make_small(7)]);
    let mfa_image = body_image(mfa, &ends.alpha_atoms);
    let opts_image = body_image(opts, &ends.alpha_atoms);
    let args_image = body_image(args, &ends.alpha_atoms);

    // `{DOP_SPAWN_REQUEST, Ref, From, GL, MFA, Opts} -- Args`.
    ends.alpha_send(
        Dop::SpawnRequest,
        &[
            make_small(SPAWN_REF),
            make_small(ALPHA_PID),
            make_small(GROUP_LEADER_PID),
            mfa,
            opts,
            args,
        ],
    )
    .expect("α sends DOP_SPAWN_REQUEST");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::SpawnRequest);
    match got {
        Delivery::SpawnRequest {
            reference,
            from,
            group_leader,
            mfa: got_mfa,
            opts: got_opts,
            args: got_args,
            token,
        } => {
            assert_eq!(reference, make_small(SPAWN_REF));
            assert_eq!(from, make_small(ALPHA_PID));
            assert_eq!(group_leader, make_small(GROUP_LEADER_PID));
            assert!(token.is_none(), "DOP_SPAWN_REQUEST (not _TT) carries no token");
            assert_eq!(body_image(got_mfa, &ends.beta_atoms), mfa_image);
            assert_eq!(body_image(got_opts, &ends.beta_atoms), opts_image);
            assert_eq!(
                body_image(got_args, &ends.beta_atoms),
                args_image,
                "the spawn argument list crossed as the trailing term",
            );
        }
        other => panic!("expected a Delivery::SpawnRequest, got {other:?}"),
    }

    // The reply with an EMPTY flag word: `{DOP_SPAWN_REPLY, Ref, To, Flags, From}`.
    ends.beta_send_ints(
        Dop::SpawnReply,
        &[
            SPAWN_REF,
            ALPHA_PID,
            i64::from(DistSpawnFlags::EMPTY.bits()),
            SPAWN_RESULT_PID,
        ],
    )
    .expect("β sends DOP_SPAWN_REPLY with no flags");
    let got = ends.recv_at_alpha();
    assert_eq!(got.dop(), Dop::SpawnReply);
    match got {
        Delivery::SpawnReply {
            reference,
            to,
            flags,
            result,
            token,
        } => {
            assert_eq!(reference, make_small(SPAWN_REF), "the spawn reference echoed");
            assert_eq!(to, make_small(ALPHA_PID), "the reply is addressed to the parent");
            assert_eq!(result, make_small(SPAWN_RESULT_PID), "the spawned pid came back");
            assert!(token.is_none());
            assert_eq!(flags, DistSpawnFlags::EMPTY);
            assert!(!flags.contains(DistSpawnFlags::LINK));
            assert!(!flags.contains(DistSpawnFlags::MONITOR));
        }
        other => panic!("expected a Delivery::SpawnReply, got {other:?}"),
    }

    // The reply with a real `DIST_SPAWN_FLAG_*` bit: link + monitor (3).
    ends.beta_send_ints(
        Dop::SpawnReply,
        &[
            SPAWN_REF,
            ALPHA_PID,
            i64::from(DIST_SPAWN_FLAG_LINK | DIST_SPAWN_FLAG_MONITOR),
            SPAWN_RESULT_PID,
        ],
    )
    .expect("β sends DOP_SPAWN_REPLY with link+monitor");
    let got = ends.recv_at_alpha();
    assert_eq!(got.dop(), Dop::SpawnReply);
    match got {
        Delivery::SpawnReply { flags, .. } => {
            assert!(
                flags.contains(DistSpawnFlags::LINK),
                "the ERTS_DIST_SPAWN_FLAG_LINK bit crossed the wire",
            );
            assert!(
                flags.contains(DistSpawnFlags::MONITOR),
                "the ERTS_DIST_SPAWN_FLAG_MONITOR bit crossed the wire",
            );
            assert_eq!(flags.bits(), DIST_SPAWN_FLAG_LINK | DIST_SPAWN_FLAG_MONITOR);
        }
        other => panic!("expected a Delivery::SpawnReply, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (9) The alternative-activation signal: token-less, token-carrying, and the
// EXIT-flagged form (where the trailing term is a reason, not a message).
// ---------------------------------------------------------------------------

#[test]
fn altact_sig_sends_with_and_without_a_token_cross_the_loopback() {
    let mut ends = Ends::new(connect_pair());

    // Token-less form: `{DOP_ALTACT_SIG_SEND, Flags, SenderPid, To} -- Message`.
    ends.alpha_send_ints(Dop::AltactSigSend, &[0, ALPHA_PID, BETA_PID, 42])
        .expect("α sends a token-less DOP_ALTACT_SIG_SEND");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::AltactSigSend);
    match got {
        Delivery::AltactSigSend {
            flags,
            from,
            to,
            token,
            message,
        } => {
            assert_eq!(flags, AltactSigFlags::EMPTY);
            assert_eq!(from, make_small(ALPHA_PID));
            assert_eq!(to, make_small(BETA_PID));
            assert!(token.is_none(), "the token-less form carries no token");
            assert_eq!(message, make_small(42));
        }
        other => panic!("expected a Delivery::AltactSigSend, got {other:?}"),
    }

    // Token-carrying form: the control tuple grows one element
    // (`ERTS_DOP_ALTACT_SIG_FLG_TOKEN`, `dist.c:2651-2658`).
    ends.beta_send_ints(
        Dop::AltactSigSend,
        &[
            i64::from(DOP_ALTACT_SIG_FLG_TOKEN | DOP_ALTACT_SIG_FLG_PRIO),
            BETA_PID,
            ALPHA_PID,
            TRACE_TOKEN,
            43,
        ],
    )
    .expect("β sends a token-carrying DOP_ALTACT_SIG_SEND");
    let got = ends.recv_at_alpha();
    assert_eq!(got.dop(), Dop::AltactSigSend);
    match got {
        Delivery::AltactSigSend {
            flags,
            from,
            to,
            token,
            message,
        } => {
            assert!(flags.contains(AltactSigFlags::TOKEN));
            assert!(flags.contains(AltactSigFlags::PRIO));
            assert_eq!(from, make_small(BETA_PID));
            assert_eq!(to, make_small(ALPHA_PID));
            assert_eq!(
                token,
                Some(make_small(TRACE_TOKEN)),
                "the TOKEN-flagged form locates the token element",
            );
            assert_eq!(message, make_small(43), "the message stays after the token");
        }
        other => panic!("expected a Delivery::AltactSigSend, got {other:?}"),
    }

    // EXIT-flagged form: the trailing term is an exit reason, not a message.
    ends.alpha_send_ints(
        Dop::AltactSigSend,
        &[i64::from(DOP_ALTACT_SIG_FLG_EXIT), ALPHA_PID, BETA_PID, EXIT_REASON],
    )
    .expect("α sends an EXIT-flagged DOP_ALTACT_SIG_SEND");
    match ends.recv_at_beta() {
        Delivery::AltactSigSend { flags, message, .. } => {
            assert!(flags.contains(AltactSigFlags::EXIT));
            assert_eq!(
                message,
                make_small(EXIT_REASON),
                "an EXIT-flagged altact carries the reason as the trailing term",
            );
        }
        other => panic!("expected a Delivery::AltactSigSend, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (10) The trace-token (`_TT`) forms populate the token field.
// ---------------------------------------------------------------------------

#[test]
fn trace_token_signals_populate_the_token_field() {
    let mut ends = Ends::new(connect_pair());

    // `{DOP_EXIT_TT, From, To, TraceToken, Reason}` — an inline reason.
    ends.alpha_send_ints(Dop::ExitTt, &[ALPHA_PID, BETA_PID, TRACE_TOKEN, EXIT_REASON])
        .expect("α sends DOP_EXIT_TT");
    let got = ends.recv_at_beta();
    // `Delivery` carries no separate `_TT` variant, so `dop()` reports the base
    // opcode; the *token* is what distinguishes a `_TT` message in this view.
    assert_eq!(got.dop(), Dop::Exit);
    match got {
        Delivery::Exit {
            from,
            to,
            reason,
            token,
        } => {
            assert_eq!(
                token,
                Some(make_small(TRACE_TOKEN)),
                "the trace token is populated, not swallowed",
            );
            assert_eq!(from, make_small(ALPHA_PID));
            assert_eq!(to, make_small(BETA_PID));
            assert_eq!(reason, make_small(EXIT_REASON));
        }
        other => panic!("expected a `_TT` Delivery::Exit, got {other:?}"),
    }

    // `{DOP_PAYLOAD_EXIT_TT, From, To, TraceToken} -- Reason` — a payload reason.
    ends.beta_send_ints(
        Dop::PayloadExitTt,
        &[BETA_PID, ALPHA_PID, TRACE_TOKEN, EXIT_REASON],
    )
    .expect("β sends DOP_PAYLOAD_EXIT_TT");
    let got = ends.recv_at_alpha();
    assert_eq!(got.dop(), Dop::PayloadExit);
    match got {
        Delivery::PayloadExit {
            from,
            to,
            payload,
            token,
        } => {
            assert_eq!(token, Some(make_small(TRACE_TOKEN)));
            assert_eq!(from, make_small(BETA_PID));
            assert_eq!(to, make_small(ALPHA_PID));
            assert_eq!(payload, make_small(EXIT_REASON));
        }
        other => panic!("expected a `_TT` Delivery::PayloadExit, got {other:?}"),
    }

    // `{DOP_SEND_TT, From, To, TraceToken} -- Message` — the send family, too.
    ends.alpha_send_ints(Dop::SendTt, &[ALPHA_PID, BETA_PID, TRACE_TOKEN, 9])
        .expect("α sends DOP_SEND_TT");
    let got = ends.recv_at_beta();
    assert_eq!(got.dop(), Dop::Send);
    match got {
        Delivery::Send {
            from,
            to,
            message,
            token,
        } => {
            assert_eq!(token, Some(make_small(TRACE_TOKEN)));
            assert_eq!(from, make_small(ALPHA_PID));
            assert_eq!(to, make_small(BETA_PID));
            assert_eq!(message, make_small(9));
        }
        other => panic!("expected a `_TT` Delivery::Send, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (11) The wire is real: a `'D'` distribution header crosses in both directions.
// ---------------------------------------------------------------------------

#[test]
fn the_loopback_wire_carries_a_real_distribution_header() {
    let SpySession {
        mut ends,
        server_reads,
        server_writes,
        client_reads,
        client_writes,
    } = spy_pair();

    // One signal each way, so both directions put a real frame on the wire.
    ends.alpha_send_ints(Dop::Link, &[ALPHA_PID, BETA_PID])
        .expect("α sends DOP_LINK");
    assert_eq!(ends.recv_at_beta().dop(), Dop::Link);
    ends.beta_send_ints(Dop::Link, &[BETA_PID, ALPHA_PID])
        .expect("β sends DOP_LINK");
    assert_eq!(ends.recv_at_alpha().dop(), Dop::Link);

    // Both endpoints framed their control message with the `'D'` distribution header,
    // and both peers read one off the socket — so the deliveries above really came
    // from bytes this crate's framing produced and consumed, not from a shortcut.
    assert!(
        client_writes.saw_dist_header(),
        "β wrote a 'D' distribution header onto the socket",
    );
    assert!(
        server_reads.saw_dist_header(),
        "α read β's 'D' distribution header off the socket",
    );
    assert!(
        server_writes.saw_dist_header(),
        "α wrote a 'D' distribution header onto the socket",
    );
    assert!(
        client_reads.saw_dist_header(),
        "β read α's 'D' distribution header off the socket",
    );
}
