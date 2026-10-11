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

//! A dense two-node distribution loopback for compatibility contract **A.5**.
//!
//! The other integration tests each exercise one seam: `tests/connection.rs` and
//! `tests/driver.rs` wire a client and a server of *this* crate together,
//! `tests/live_*.rs` talk to a real `erl` node through `epmd`. This file proves a
//! distinct property: **two independent `beam_dist` distribution endpoints, in the
//! SAME process, over a real loopback TCP link — no external `erl`, no `epmd`.**
//! Both endpoints run this crate's own handshake, control codec, atom cache,
//! fragmentation and delivery classification, so every byte that crosses is
//! produced and consumed by the code under test.
//!
//! The pattern is the one `tests/connection.rs` established: a real
//! [`TcpListener`] is bound on `127.0.0.1:0`; only the **handshake** runs on a
//! background thread (node A, the acceptor), and once it returns both live
//! [`Connection`]s are owned by the test thread, so all further traffic is driven
//! sequentially and every assertion is local. Every socket carries a finite read
//! timeout ([`TEST_TIMEOUT`]), so a wire bug surfaces as a failed assertion rather
//! than a hung suite.
//!
//! Small integers model pids and references throughout. The distribution *value*
//! layer treats those fields as opaque terms (it resolves neither a pid to a local
//! process nor a reference to a monitor — that is the [`InboundSink`]'s job), and
//! unlike a boxed term an immediate compares `==` across the two endpoints' heaps —
//! exactly the convention `tests/delivery.rs` uses.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::VERSION_MAGIC;
use beam_common::BeamError;
use beam_dist::dist_header::DIST_FRAG_HEADER;
use beam_dist::flags::Dop;
use beam_dist::fragment::DIST_FRAGMENT_SIZE;
use beam_dist::transport::{Read, Write};
use beam_dist::{
    Connection, ConnectionDriver, Delivery, DeliveryOutcome, Dispatched, DriverState, InboundSink,
    StepOutcome, TcpListener, TcpTransport,
};
use beam_term::prelude::{atom_val, is_atom, make_atom, make_small, Eterm, NIL};
use beam_term::AtomTable;

/// The shared cookie both endpoints authenticate with.
const COOKIE: &[u8] = b"two-node-loopback-cookie";
/// Node A — the accepting side (the distribution **server** role).
const ALPHA_NAME: &[u8] = b"alpha@localhost";
/// Node A's creation.
const ALPHA_CREATION: u32 = 1;
/// Node B — the connecting side (the distribution **client** role).
const BETA_NAME: &[u8] = b"beta@localhost";
/// Node B's creation.
const BETA_CREATION: u32 = 2;

/// A stand-in for node A's own pid (see the module docs on small-int stand-ins).
const ALPHA_PID: i64 = 1001;
/// A stand-in for node B's own pid.
const BETA_PID: i64 = 2002;
/// A stand-in for a fresh monitor reference.
const MONITOR_REF: i64 = 3003;
/// A stand-in for the monitor-down reason (`noproc` would be an atom here).
const DOWN_REASON: i64 = 4004;

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
/// device `tests/connection.rs` uses.
fn body_image(term: Eterm, atoms: &AtomTable) -> Vec<u8> {
    beam_codec::encode_term_body(term, atoms).expect("encode term body")
}

/// Builds a proper list of `len` small ints `start .. start + len` on `heap`.
fn big_list(heap: &mut Heap, len: i64, start: i64) -> Eterm {
    let mut list = NIL;
    for i in (0..len).rev() {
        list = heap.cons(make_small(start + i), list).expect("cons");
    }
    list
}

/// The two live endpoints of one loopback link, both owned by the test thread.
struct Pair {
    /// Node A (`alpha@localhost`), the acceptor.
    alpha: Connection<TcpTransport>,
    /// Node B (`beta@localhost`), the connector.
    beta: Connection<TcpTransport>,
}

/// Binds a loopback listener, completes the `'N'` handshake on the acceptor thread
/// and returns both established endpoints.
///
/// Only the handshake runs on the background thread; it returns the live
/// [`Connection`] as soon as the link is established, so the test thread can drive
/// both endpoints afterwards.
fn connect_pair() -> Pair {
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

/// Drives two independent [`ConnectionDriver`]s against each other (the
/// `tests/driver.rs` byte shuttle) until both report `Established`.
///
/// The driver is byte-fed and transport-agnostic, so this needs no socket: the
/// two nodes' `take_output()` buffers are handed to each other by hand.
fn establish_drivers() -> (ConnectionDriver, ConnectionDriver) {
    let mut client = ConnectionDriver::client(BETA_NAME, COOKIE, BETA_CREATION);
    let mut server = ConnectionDriver::server(ALPHA_NAME, COOKIE, ALPHA_CREATION);

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
            panic!(
                "driver failed: client={:?}, server={:?}",
                client.state(),
                server.state()
            );
        }
    }
    panic!("the driver handshake did not complete");
}

/// Shuttles `from`'s queued output into `to` and makes one `step` of progress.
fn relay(from: &mut ConnectionDriver, to: &mut ConnectionDriver) {
    let out = from.take_output();
    assert!(!out.is_empty(), "the sender produced no bytes");
    to.feed(&out);
    to.step().expect("receiver step");
}

// ---------------------------------------------------------------------------
// A socket spy, so a test can prove a large message really was fragmented rather
// than merely round-tripping (the `tests/connection.rs` `WireLog` pattern).
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

    /// Whether any recorded block is a `'E'` fragment-header frame body
    /// (`VERSION_MAGIC + DIST_FRAG_HEADER`), i.e. a real fragment crossed the wire.
    fn saw_fragment_header(&self) -> bool {
        self.0.lock().expect("wire log").iter().any(|b| {
            b.len() >= 2 && b[0] == VERSION_MAGIC && b[1] == DIST_FRAG_HEADER
        })
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

/// An [`InboundSink`] that records every delivery it receives (a stand-in for the
/// process-table/mailbox routing `beam_rt` supplies) and reports `Delivered`.
#[derive(Clone, Default)]
struct RecordingSink {
    deliveries: Arc<Mutex<Vec<Delivery>>>,
}

impl RecordingSink {
    /// A snapshot of everything routed through the sink so far.
    fn recorded(&self) -> Vec<Delivery> {
        self.deliveries.lock().expect("recording sink").clone()
    }
}

impl InboundSink for RecordingSink {
    fn deliver(&mut self, delivery: &Delivery) -> beam_common::Result<DeliveryOutcome> {
        self.deliveries
            .lock()
            .expect("recording sink")
            .push(delivery.clone());
        Ok(DeliveryOutcome::Delivered)
    }
}

// ---------------------------------------------------------------------------
// (1) Two named nodes handshake and exchange a name-addressed send both ways.
// ---------------------------------------------------------------------------

#[test]
fn two_named_nodes_handshake_and_exchange_a_name_addressed_send_both_ways() {
    let Pair {
        mut alpha,
        mut beta,
    } = connect_pair();

    // Both ends learned the other's name and creation from the `'N'` handshake.
    assert_eq!(beta.peer_name(), ALPHA_NAME);
    assert_eq!(beta.peer_creation(), ALPHA_CREATION);
    assert_eq!(alpha.peer_name(), BETA_NAME);
    assert_eq!(alpha.peer_creation(), BETA_CREATION);

    let mut alpha_atoms = AtomTable::new();
    let mut beta_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("alpha heap");
    let mut beta_heap = Heap::new(1 << 16).expect("beta heap");

    // β → α: a name-addressed send to α's registered name `echo`.
    let payload = beta_heap
        .tuple(&[make_atom(beta_atoms.intern("hello")), make_small(42)])
        .expect("payload tuple");
    beta.send_reg_send(
        make_small(BETA_PID),
        make_atom(beta_atoms.intern("")),
        make_atom(beta_atoms.intern("echo")),
        payload,
        &beta_atoms,
    )
    .expect("send DOP_REG_SEND");

    let delivery = alpha
        .recv_delivery(&mut alpha_heap, &mut alpha_atoms)
        .expect("α receives the name-addressed send");
    match delivery {
        Delivery::RegSend {
            from,
            cookie,
            name,
            message,
            token,
        } => {
            assert_eq!(from, make_small(BETA_PID), "the sender pid crossed intact");
            assert!(token.is_none(), "DOP_REG_SEND (not _TT) carries no trace token");
            assert!(is_atom(cookie), "the cookie is an atom");
            assert_eq!(
                alpha_atoms.name(atom_val(cookie)),
                Some(""),
                "the cookie is the empty atom",
            );
            assert!(is_atom(name), "the registered name is an atom");
            assert_eq!(
                alpha_atoms.name(atom_val(name)),
                Some("echo"),
                "the message was addressed to α's registered name `echo`",
            );
            assert_eq!(
                body_image(message, &alpha_atoms),
                body_image(payload, &beta_atoms),
                "the tuple payload crossed byte-identically",
            );
        }
        other => panic!("expected a name-addressed Delivery::RegSend, got {other:?}"),
    }

    // α replies to β's (external) pid with a send addressed to that pid.
    let pong = alpha_heap
        .tuple(&[make_atom(alpha_atoms.intern("pong")), make_small(99)])
        .expect("reply tuple");
    alpha
        .send_message(make_small(ALPHA_PID), make_small(BETA_PID), pong, &alpha_atoms)
        .expect("α replies to β's pid");

    let reply = beta
        .recv_delivery(&mut beta_heap, &mut beta_atoms)
        .expect("β receives the pid-addressed reply");
    match reply {
        Delivery::Send {
            from,
            to,
            message,
            token,
        } => {
            assert_eq!(from, make_small(ALPHA_PID), "the reply is from α's pid");
            assert_eq!(to, make_small(BETA_PID), "the reply is addressed to β's pid");
            assert!(token.is_none());
            assert_eq!(
                body_image(message, &beta_atoms),
                body_image(pong, &alpha_atoms),
                "the reply payload crossed byte-identically",
            );
        }
        other => panic!("expected a pid-addressed Delivery::Send reply, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (2) A tick is answered by both endpoints, transparently.
// ---------------------------------------------------------------------------

#[test]
fn a_tick_is_answered_by_both_rust_endpoints() {
    let Pair {
        mut alpha,
        mut beta,
    } = connect_pair();

    let mut alpha_atoms = AtomTable::new();
    let mut beta_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(4096).expect("alpha heap");
    let mut beta_heap = Heap::new(4096).expect("beta heap");

    let (from, to) = (make_small(1), make_small(2));

    // Each endpoint sends a tick and then a message. Reading the inbound stream,
    // `recv` answers the tick (writing a tick back) and then delivers the message.
    alpha.send_tick().expect("α sends a tick");
    alpha
        .send_control(Dop::Link, &[from, to], &alpha_atoms)
        .expect("α sends a message");
    beta.send_tick().expect("β sends a tick");
    beta
        .send_control(Dop::Link, &[to, from], &beta_atoms)
        .expect("β sends a message");

    // β answers α's tick, then decodes α's message.
    let got = beta
        .recv(&mut beta_heap, &mut beta_atoms)
        .expect("β reads past its own tick");
    assert_eq!(got.dop, Dop::Link);
    assert_eq!(got.terms, vec![from, to]);
    assert_eq!(beta.ticks_answered(), 1, "β answered α's tick");

    // α answers β's tick (its own answer to α's tick is still queued and unread,
    // so it does not disturb the count), then decodes β's message.
    let got = alpha
        .recv(&mut alpha_heap, &mut alpha_atoms)
        .expect("α reads past its own tick");
    assert_eq!(got.dop, Dop::Link);
    assert_eq!(got.terms, vec![to, from]);
    assert_eq!(alpha.ticks_answered(), 1, "α answered β's tick");
}

// ---------------------------------------------------------------------------
// (3) A monitor round trip between the two endpoints.
// ---------------------------------------------------------------------------

#[test]
fn a_monitor_round_trip_between_two_rust_endpoints() {
    let Pair {
        mut alpha,
        mut beta,
    } = connect_pair();

    let mut alpha_atoms = AtomTable::new();
    let mut beta_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(4096).expect("alpha heap");
    let mut beta_heap = Heap::new(4096).expect("beta heap");

    // α asks to monitor β, identified by a fresh reference term.
    alpha
        .send_monitor_p(
            make_small(ALPHA_PID),
            make_small(BETA_PID),
            make_small(MONITOR_REF),
            &alpha_atoms,
        )
        .expect("α sends DOP_MONITOR_P");
    let got = beta
        .recv_delivery(&mut beta_heap, &mut beta_atoms)
        .expect("β receives the monitor request");
    assert_eq!(
        got,
        Delivery::Monitor {
            watcher: make_small(ALPHA_PID),
            watched: make_small(BETA_PID),
            reference: make_small(MONITOR_REF),
        },
        "DOP_MONITOR_P classifies as a monitor the remote α holds on the local β",
    );

    // β answers with the classic (inline) monitor-exit form.
    beta.send_control(
        Dop::MonitorPExit,
        &[
            make_small(BETA_PID),
            make_small(ALPHA_PID),
            make_small(MONITOR_REF),
            make_small(DOWN_REASON),
        ],
        &beta_atoms,
    )
    .expect("β sends DOP_MONITOR_P_EXIT");
    let got = alpha
        .recv_delivery(&mut alpha_heap, &mut alpha_atoms)
        .expect("α receives the monitor exit");
    assert_eq!(
        got,
        Delivery::MonitorExit {
            watched: make_small(BETA_PID),
            watcher: make_small(ALPHA_PID),
            reference: make_small(MONITOR_REF),
            reason: make_small(DOWN_REASON),
        },
        "DOP_MONITOR_P_EXIT classifies as the monitored β's down signal",
    );

    // The payload form of the same signal: the reason rides as the trailing term
    // (`DFLAG_EXIT_PAYLOAD`), so the control tuple stays small.
    alpha
        .send_monitor_p(
            make_small(ALPHA_PID),
            make_small(BETA_PID),
            make_small(MONITOR_REF),
            &alpha_atoms,
        )
        .expect("α sends a second DOP_MONITOR_P");
    let got = beta
        .recv_delivery(&mut beta_heap, &mut beta_atoms)
        .expect("β receives the second monitor request");
    assert_eq!(got.dop(), Dop::MonitorP);

    beta.send_control(
        Dop::PayloadMonitorPExit,
        &[
            make_small(BETA_PID),
            make_small(ALPHA_PID),
            make_small(MONITOR_REF),
            make_small(DOWN_REASON),
        ],
        &beta_atoms,
    )
    .expect("β sends DOP_PAYLOAD_MONITOR_P_EXIT");
    let got = alpha
        .recv_delivery(&mut alpha_heap, &mut alpha_atoms)
        .expect("α receives the payload monitor exit");
    assert_eq!(
        got,
        Delivery::PayloadMonitorExit {
            watched: make_small(BETA_PID),
            watcher: make_small(ALPHA_PID),
            reference: make_small(MONITOR_REF),
            payload: make_small(DOWN_REASON),
        },
        "DOP_PAYLOAD_MONITOR_P_EXIT carries the reason as the trailing payload term",
    );
}

// ---------------------------------------------------------------------------
// (4) A message larger than a fragment round-trips, `'E'`-framed on the wire.
// ---------------------------------------------------------------------------

#[test]
fn a_message_larger_than_a_fragment_round_trips_between_two_rust_endpoints_both_ways() {
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
    let mut beta = Connection::client_with_creation(
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
    let mut alpha = acceptor
        .join()
        .expect("acceptor thread")
        .expect("accepting handshake");

    let mut alpha_atoms = AtomTable::new();
    let mut beta_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 17).expect("alpha heap");
    let mut beta_heap = Heap::new(1 << 17).expect("beta heap");
    let (from, to) = (make_small(1), make_small(2));

    // Each list element is an `INTEGER_EXT` (5 bytes), so 14_000 elements encode
    // to ~70 KB — comfortably past `DIST_FRAGMENT_SIZE` (64 KiB).
    let big = big_list(&mut beta_heap, 14_000, 1_000);
    let image = body_image(big, &beta_atoms);
    assert!(
        image.len() > DIST_FRAGMENT_SIZE,
        "the payload must exceed one fragment, got {} bytes",
        image.len()
    );

    // β → α.
    beta.send_message(from, to, big, &beta_atoms)
        .expect("β sends an oversized message");
    let got = alpha
        .recv(&mut alpha_heap, &mut alpha_atoms)
        .expect("α reassembles the oversized message");
    assert_eq!(got.dop, Dop::SendSender);
    assert_eq!(body_image(got.terms[2], &alpha_atoms), image);

    // α → β (the endpoints fragment independently in each direction).
    let back = big_list(&mut alpha_heap, 14_000, 1_000);
    let back_image = body_image(back, &alpha_atoms);
    alpha
        .send_message(to, from, back, &alpha_atoms)
        .expect("α sends an oversized reply");
    let got = beta
        .recv(&mut beta_heap, &mut beta_atoms)
        .expect("β reassembles the oversized reply");
    assert_eq!(got.dop, Dop::SendSender);
    assert_eq!(got.terms[0], to);
    assert_eq!(got.terms[1], from);
    assert_eq!(body_image(got.terms[2], &beta_atoms), back_image);

    // Both directions really carried `'E'` fragment-header frames.
    assert!(
        client_writes.saw_fragment_header(),
        "β fragmented its oversized message"
    );
    assert!(
        server_reads.saw_fragment_header(),
        "α received `'E'` fragments"
    );
    assert!(
        server_writes.saw_fragment_header(),
        "α fragmented its oversized reply"
    );
    assert!(
        client_reads.saw_fragment_header(),
        "β received `'E'` fragments"
    );
}

// ---------------------------------------------------------------------------
// (5) The classification → routing seam: an InboundSink routes a send.
// ---------------------------------------------------------------------------

#[test]
fn an_inbound_sink_routes_a_send_into_a_local_mailbox() {
    let Pair {
        mut alpha,
        mut beta,
    } = connect_pair();

    let beta_atoms = AtomTable::new();
    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(4096).expect("alpha heap");

    let (from, to, message) = (make_small(BETA_PID), make_small(ALPHA_PID), make_small(7));
    beta.send_message(from, to, message, &beta_atoms)
        .expect("β sends a message to α");

    // α classifies the message and routes it through a test-local sink — the seam
    // `beam_rt` will supply over its process table/mailbox.
    let mut sink = RecordingSink::default();
    let dispatched = alpha
        .dispatch_inbound(&mut alpha_heap, &mut alpha_atoms, &mut sink)
        .expect("α dispatches the inbound message");
    assert_eq!(
        dispatched,
        Dispatched {
            delivery: Delivery::Send {
                from,
                to,
                message,
                token: None,
            },
            outcome: DeliveryOutcome::Delivered,
        },
        "the delivery was classified and the sink reported it Delivered (not Unsupported)",
    );
    assert_eq!(
        sink.recorded(),
        vec![Delivery::Send {
            from,
            to,
            message,
            token: None,
        }],
        "the sink actually observed the routed delivery",
    );
}

// ---------------------------------------------------------------------------
// (6) The non-blocking pair: two ConnectionDrivers handshake and exchange.
// ---------------------------------------------------------------------------

#[test]
fn a_nonblocking_two_node_pair_handshakes_and_exchanges_a_message() {
    let (mut client, mut server) = establish_drivers();

    // The handshake completed and each side learned the other's name.
    assert_eq!(client.state(), DriverState::Established);
    assert_eq!(server.state(), DriverState::Established);
    assert_eq!(client.peer_name(), Some(ALPHA_NAME));
    assert_eq!(server.peer_name(), Some(BETA_NAME));
    assert_eq!(client.peer_creation(), ALPHA_CREATION);
    assert_eq!(server.peer_creation(), BETA_CREATION);

    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(4096).expect("heap");

    // β (client) → α (server).
    client
        .send_message(make_small(BETA_PID), make_small(ALPHA_PID), make_small(11), &atoms)
        .expect("client sends a message");
    relay(&mut client, &mut server);
    let msg = server
        .next_message(&mut heap, &mut atoms)
        .expect("decode the client's message")
        .expect("a message");
    assert_eq!(msg.dop, Dop::SendSender);
    assert_eq!(
        msg.terms,
        vec![make_small(BETA_PID), make_small(ALPHA_PID), make_small(11)]
    );

    // α (server) → β (client).
    server
        .send_message(make_small(ALPHA_PID), make_small(BETA_PID), make_small(22), &atoms)
        .expect("server sends a message");
    relay(&mut server, &mut client);
    let msg = client
        .next_message(&mut heap, &mut atoms)
        .expect("decode the server's message")
        .expect("a message");
    assert_eq!(msg.dop, Dop::SendSender);
    assert_eq!(
        msg.terms,
        vec![make_small(ALPHA_PID), make_small(BETA_PID), make_small(22)]
    );
}

// ---------------------------------------------------------------------------
// (7) Dropping one endpoint is observed as a closed link by the peer.
// ---------------------------------------------------------------------------

#[test]
fn dropping_one_endpoint_is_observed_as_a_closed_link_by_the_peer() {
    let Pair { alpha, mut beta } = connect_pair();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(4096).expect("beta heap");

    // Close α's socket by dropping its connection.
    drop(alpha);

    // β's next read sees end-of-stream: a typed `BadArg` (a short read), never a
    // hang and never a panic. The finite read timeout bounds a regression.
    //
    // Honest finding: at THIS layer peer death is transport EOF. The
    // `net_ticktime` timeout and the resulting `{nodedown, Node}` decision belong
    // to `net_kernel` on the Erlang side (the emulator only reports the link down)
    // — this crate ships no tick timer and takes no `nodedown` decision.
    let err = beta
        .recv(&mut beta_heap, &mut beta_atoms)
        .expect_err("a closed peer must not yield a message");
    assert!(
        matches!(err, BeamError::BadArg),
        "expected an EOF short read, got {err:?}"
    );

    // The non-blocking driver reports the same event as a terminal `Closed` state.
    let (mut client, _server) = establish_drivers();
    client.feed_eof(); // the runtime observed `read() == 0` on the peer's socket
    assert_eq!(client.step().expect("step at EOF"), StepOutcome::Closed);
    assert_eq!(client.state(), DriverState::Closed);
    assert!(client.is_eof());
    // A closed link stays closed: `step` keeps reporting it, never resumes.
    assert_eq!(client.step().expect("step after close"), StepOutcome::Closed);
}

// ---------------------------------------------------------------------------
// (8) Node up/down are not v6 control opcodes.
// ---------------------------------------------------------------------------

#[test]
fn node_up_and_down_are_not_v6_control_opcodes() {
    // `enum dop` (`erts/emulator/beam/dist.h:142-183`) assigns exactly 30 v6
    // control opcodes. There is no `DOP_NODEUP`/`DOP_NODEDOWN` in v6 — node up and
    // node down are LOCAL emulator events (`send_nodes_mon_msgs`, `dist.c:6931`)
    // delivered to a watcher as `{nodeup, Node}`/`{nodedown, Node}` messages, not
    // as distribution control opcodes. So every byte such an opcode would have to
    // occupy must stay unassigned. (`0` and `5` head the low gaps; `5` is the
    // ancient `DOP_NODE_LINK`/`DOP_n`, whose slot was left free `dist.h:147`.)
    for byte in [0u8, 5, 9, 10, 11, 14, 15, 17] {
        assert_eq!(
            Dop::from_u8(byte),
            None,
            "byte {byte} is not a v6 control opcode"
        );
    }
    for byte in 38u8..=u8::MAX {
        assert_eq!(
            Dop::from_u8(byte),
            None,
            "byte {byte} is not a v6 control opcode"
        );
    }

    // `DOP_UNLINK` (4) *is* a v6 opcode, just one the C never sends.
    assert_eq!(Dop::from_u8(4), Some(Dop::Unlink));

    // Exactly 30 bytes are v6 opcodes — no hidden node-up/down byte among them.
    let decodable = (0u8..=u8::MAX)
        .filter(|&byte| Dop::from_u8(byte).is_some())
        .count();
    assert_eq!(decodable, 30, "v6 has exactly 30 control opcodes");
}
