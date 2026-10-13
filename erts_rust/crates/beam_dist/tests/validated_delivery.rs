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

//! The per-link **validated** receive/dispatch layer, end-to-end over a real wire.
//!
//! [`Delivery::classify_validated`] (and its `LinkIdentity`) lives in
//! `src/delivery.rs`; this file proves the *per-link* entry points that feed it —
//! [`Connection::recv_delivery_checked`]/[`Connection::dispatch_inbound_checked`] and
//! [`ConnectionDriver::next_delivery_checked`]/[`ConnectionDriver::dispatch_inbound_checked`] —
//! behave as specified on messages that actually crossed a distribution link:
//!
//! * a `DOP_LINK` whose `from` is an external pid of the **peer** validates;
//! * the same message with a `from` naming a **third** node is rejected
//!   (`BeamError::BadArg`) by the checked path while the **unchanged**
//!   [`Connection::recv_delivery`]/[`ConnectionDriver::next_delivery`] still accept
//!   it (no regression);
//! * a `DOP_MONITOR_P` with a real external-pid `watcher` validates, while a
//!   small-int stand-in in the watcher slot is rejected.
//!
//! Everything here is hermetic: two endpoints of *this* crate over a real loopback
//! `TcpStream` ([`TcpListener`]/[`TcpTransport`]), or two [`ConnectionDriver`]s
//! shuttled by hand. No `erl`, no `epmd`.
//!
//! The terms are **real**: external pids/refs are materialized on a
//! `beam_alloc::Heap` (never small-int stand-ins) in the `from`/`watcher`/`reference`
//! positions the validator inspects, because the whole point of the checked path is
//! the term shape and the per-link node identity. Only `to`/`watched` — which the
//! validator does not inspect — carry stand-ins.

use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::term::{make_external_pid, make_external_ref};
use beam_common::BeamError;
use beam_dist::flags::Dop;
use beam_dist::{
    Connection, ConnectionDriver, Delivery, DeliveryOutcome, Dispatched, DriverState, InboundSink,
    TcpListener, TcpTransport,
};
use beam_term::prelude::{make_atom, make_small, Eterm};
use beam_term::AtomTable;

/// The shared cookie both endpoints authenticate with.
const COOKIE: &[u8] = b"validated-delivery-cookie";
/// The accepting node's full name (the distribution **server** role).
const ALPHA_NAME: &str = "alpha@localhost";
/// The accepting node's creation.
const ALPHA_CREATION: u32 = 1;
/// The connecting node's full name (the distribution **client** role).
const BETA_NAME: &str = "beta@localhost";
/// The connecting node's creation.
const BETA_CREATION: u32 = 2;
/// A third node that is **not** part of the link (a spoof source).
const GAMMA_NAME: &str = "gamma@localhost";

/// A finite read timeout, so a stalled or broken link fails instead of hanging.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

/// An external pid whose `node` slot is the atom named `node`.
///
/// The `node` name must match the link's peer/local name for the validator's
/// per-link check; that is exactly what these tests vary.
fn external_pid(heap: &mut Heap, atoms: &mut AtomTable, node: &str) -> Eterm {
    let node = make_atom(atoms.intern(node));
    make_external_pid(heap, node, 1, 0, 0).expect("materialize an external pid")
}

/// An external reference whose `node` slot is the atom named `node`.
fn external_ref(heap: &mut Heap, atoms: &mut AtomTable, node: &str) -> Eterm {
    let node = make_atom(atoms.intern(node));
    make_external_ref(heap, node, &[1, 2, 3], 0).expect("materialize an external ref")
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
/// both endpoints afterwards (the `tests/two_node_loopback.rs` pattern).
fn connect_pair() -> Pair {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let acceptor = thread::spawn(move || -> beam_common::Result<Connection<TcpTransport>> {
        let mut transport = listener.accept()?;
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        Connection::server(transport, ALPHA_NAME.as_bytes(), COOKIE, ALPHA_CREATION)
    });

    let mut transport = TcpTransport::connect(&addr.to_string()).expect("connect to the peer");
    transport.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
    let beta = Connection::client_with_creation(
        transport,
        BETA_NAME.as_bytes(),
        COOKIE,
        BETA_CREATION,
    )
    .expect("connecting handshake");
    let alpha = acceptor.join().expect("acceptor thread").expect("accepting handshake");

    Pair { alpha, beta }
}

/// Drives two independent [`ConnectionDriver`]s against each other (the
/// `tests/driver.rs` byte shuttle) until both report `Established`.
///
/// Returns `(alpha, beta)`: `alpha` is the accepting **server** driver (its
/// `peer_name` is `beta@localhost`), `beta` the connecting **client** driver.
fn establish_drivers() -> (ConnectionDriver, ConnectionDriver) {
    let mut beta = ConnectionDriver::client(BETA_NAME.as_bytes(), COOKIE, BETA_CREATION);
    let mut alpha = ConnectionDriver::server(ALPHA_NAME.as_bytes(), COOKIE, ALPHA_CREATION);

    for _ in 0..100_000 {
        let beta_out = beta.take_output();
        let alpha_out = alpha.take_output();
        if !beta_out.is_empty() {
            alpha.feed(&beta_out);
        }
        if !alpha_out.is_empty() {
            beta.feed(&alpha_out);
        }
        beta.step().expect("client step");
        alpha.step().expect("server step");

        if alpha.state() == DriverState::Established && beta.state() == DriverState::Established {
            return (alpha, beta);
        }
        if alpha.state() == DriverState::Failed || beta.state() == DriverState::Failed {
            panic!("driver failed: alpha={:?}, beta={:?}", alpha.state(), beta.state());
        }
    }
    panic!("the driver handshake did not complete");
}

/// An [`InboundSink`] that records every delivery it is handed and reports
/// [`DeliveryOutcome::Delivered`], so a dispatch test can assert routing happened.
#[derive(Default)]
struct RecordingSink {
    /// The deliveries routed to this sink, in arrival order.
    delivered: Vec<Delivery>,
}

impl InboundSink for RecordingSink {
    fn deliver(&mut self, delivery: &Delivery) -> beam_common::Result<DeliveryOutcome> {
        self.delivered.push(delivery.clone());
        Ok(DeliveryOutcome::Delivered)
    }
}

// ---------------------------------------------------------------------------
// (a) The blocking `Connection` checked path.
// ---------------------------------------------------------------------------

#[test]
fn the_checked_path_accepts_a_link_from_the_peer() {
    let mut pair = connect_pair();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let from = external_pid(&mut beta_heap, &mut beta_atoms, BETA_NAME);
    let to = external_pid(&mut beta_heap, &mut beta_atoms, ALPHA_NAME);
    pair.beta.send_link(from, to, &beta_atoms).expect("send DOP_LINK");

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");
    let delivery = pair
        .alpha
        .recv_delivery_checked(&mut alpha_heap, &mut alpha_atoms)
        .expect("a link from the peer node validates");
    assert!(matches!(delivery, Delivery::Link { .. }), "got {delivery:?}");
}

#[test]
fn the_checked_path_rejects_a_link_from_a_third_node_but_the_unchecked_path_accepts_it() {
    let mut pair = connect_pair();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    // `from` names a THIRD node — nothing on this link.
    let from = external_pid(&mut beta_heap, &mut beta_atoms, GAMMA_NAME);
    let to = external_pid(&mut beta_heap, &mut beta_atoms, ALPHA_NAME);

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");

    // (1) The unchecked, semantic view classifies it (shape only, no `dep` check).
    pair.beta.send_link(from, to, &beta_atoms).expect("send spoofed DOP_LINK");
    let unchecked = pair
        .alpha
        .recv_delivery(&mut alpha_heap, &mut alpha_atoms)
        .expect("the unchecked path accepts any well-shaped link");
    assert!(matches!(unchecked, Delivery::Link { .. }), "got {unchecked:?}");

    // (2) The checked path applies the C's `dep != external_pid_dist_entry(from)`
    //     (`dist.c:2237`) and rejects the very same control message.
    pair.beta.send_link(from, to, &beta_atoms).expect("send spoofed DOP_LINK again");
    let err = pair
        .alpha
        .recv_delivery_checked(&mut alpha_heap, &mut alpha_atoms)
        .expect_err("a link from a third node is invalid");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
}

#[test]
fn the_checked_path_accepts_a_monitor_with_a_real_external_pid_watcher() {
    let mut pair = connect_pair();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let watcher = external_pid(&mut beta_heap, &mut beta_atoms, BETA_NAME);
    let watched = make_atom(beta_atoms.intern("registered_process"));
    let reference = external_ref(&mut beta_heap, &mut beta_atoms, BETA_NAME);

    pair.beta
        .send_monitor_p(watcher, watched, reference, &beta_atoms)
        .expect("send DOP_MONITOR_P");

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");
    let delivery = pair
        .alpha
        .recv_delivery_checked(&mut alpha_heap, &mut alpha_atoms)
        .expect("a monitor with a peer pid + a ref validates");
    assert!(matches!(delivery, Delivery::Monitor { .. }), "got {delivery:?}");
}

#[test]
fn the_checked_path_rejects_a_monitor_with_a_non_pid_watcher() {
    let mut pair = connect_pair();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let watched = make_atom(beta_atoms.intern("registered_process"));
    let reference = external_ref(&mut beta_heap, &mut beta_atoms, BETA_NAME);

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");

    // (1) A small-int stand-in in the watcher slot is shape-accepted by the
    //     unchecked path (`classify` does not inspect the term kind).
    pair.beta
        .send_monitor_p(make_small(7), watched, reference, &beta_atoms)
        .expect("send a stand-in monitor");
    let unchecked = pair
        .alpha
        .recv_delivery(&mut alpha_heap, &mut alpha_atoms)
        .expect("the unchecked path accepts any well-shaped monitor");
    assert!(matches!(unchecked, Delivery::Monitor { .. }), "got {unchecked:?}");

    // (2) `is_not_external_pid(watcher)` (`dist.c:2356`) rejects the same message.
    pair.beta
        .send_monitor_p(make_small(7), watched, reference, &beta_atoms)
        .expect("send the stand-in monitor again");
    let err = pair
        .alpha
        .recv_delivery_checked(&mut alpha_heap, &mut alpha_atoms)
        .expect_err("a non-pid watcher is invalid");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
}

#[test]
fn dispatch_inbound_checked_routes_a_validated_link_through_the_sink() {
    let mut pair = connect_pair();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let from = external_pid(&mut beta_heap, &mut beta_atoms, BETA_NAME);
    let to = external_pid(&mut beta_heap, &mut beta_atoms, ALPHA_NAME);
    pair.beta.send_link(from, to, &beta_atoms).expect("send DOP_LINK");

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");
    let mut sink = RecordingSink::default();
    let dispatched = pair
        .alpha
        .dispatch_inbound_checked(&mut alpha_heap, &mut alpha_atoms, &mut sink)
        .expect("a validated link dispatches");

    assert_eq!(dispatched.outcome, DeliveryOutcome::Delivered);
    assert!(matches!(dispatched.delivery, Delivery::Link { .. }));
    assert_eq!(sink.delivered.len(), 1, "the sink saw the link");
}

// ---------------------------------------------------------------------------
// (b) The non-blocking `ConnectionDriver` checked path (a hermetic byte shuttle).
// ---------------------------------------------------------------------------

#[test]
fn the_driver_checked_path_accepts_a_peer_link() {
    let (mut alpha, mut beta) = establish_drivers();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let from = external_pid(&mut beta_heap, &mut beta_atoms, BETA_NAME);
    let to = external_pid(&mut beta_heap, &mut beta_atoms, ALPHA_NAME);
    beta.send_control(Dop::Link, &[from, to], &beta_atoms).expect("send DOP_LINK");

    alpha.feed(&beta.take_output());
    alpha.step().expect("server step");

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");
    let delivery = alpha
        .next_delivery_checked(&mut alpha_heap, &mut alpha_atoms)
        .expect("decode")
        .expect("a message was queued");
    assert!(matches!(delivery, Delivery::Link { .. }), "got {delivery:?}");

    // Nothing else is pending: the checked form reports `None` like the unchecked one.
    assert!(alpha
        .next_delivery_checked(&mut alpha_heap, &mut alpha_atoms)
        .expect("decode")
        .is_none());
}

/// The strongest "same bytes" form: the *identical* framed bytes are fed twice, so
/// the two queued messages differ in no byte at all — the unchecked path accepts the
/// first copy and the checked path rejects the second.
#[test]
fn the_driver_checked_path_rejects_a_spoofed_link_on_byte_identical_input() {
    let (mut alpha, mut beta) = establish_drivers();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let from = external_pid(&mut beta_heap, &mut beta_atoms, GAMMA_NAME);
    let to = external_pid(&mut beta_heap, &mut beta_atoms, ALPHA_NAME);
    beta.send_control(Dop::Link, &[from, to], &beta_atoms).expect("send");

    let bytes = beta.take_output();
    assert!(!bytes.is_empty(), "the driver wrote a frame");
    // Two byte-identical copies queued on the receiving driver.
    alpha.feed(&bytes);
    alpha.feed(&bytes);
    alpha.step().expect("server step");

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");

    // (1) The unchecked, semantic view accepts the first copy.
    let unchecked = alpha
        .next_delivery(&mut alpha_heap, &mut alpha_atoms)
        .expect("decode")
        .expect("a message was queued");
    assert!(matches!(unchecked, Delivery::Link { .. }), "got {unchecked:?}");

    // (2) The checked path rejects the second, byte-identical copy.
    let err = alpha
        .next_delivery_checked(&mut alpha_heap, &mut alpha_atoms)
        .expect_err("a link from a third node is invalid");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
}

#[test]
fn the_driver_dispatch_inbound_checked_routes_every_validated_message() {
    let (mut alpha, mut beta) = establish_drivers();

    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(1 << 16).expect("client heap");
    let from = external_pid(&mut beta_heap, &mut beta_atoms, BETA_NAME);
    let to = external_pid(&mut beta_heap, &mut beta_atoms, ALPHA_NAME);
    beta.send_control(Dop::Link, &[from, to], &beta_atoms).expect("send 1");
    beta.send_control(Dop::Link, &[from, to], &beta_atoms).expect("send 2");

    alpha.feed(&beta.take_output());
    alpha.step().expect("server step");

    let mut alpha_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(1 << 16).expect("server heap");
    let mut sink = RecordingSink::default();
    let dispatched = alpha
        .dispatch_inbound_checked(&mut alpha_heap, &mut alpha_atoms, &mut sink)
        .expect("validated dispatch drains both links");

    assert_eq!(dispatched.len(), 2, "both queued links dispatched");
    assert!(dispatched.iter().all(|d: &Dispatched| matches!(d.delivery, Delivery::Link { .. })));
    assert!(dispatched.iter().all(|d| d.outcome == DeliveryOutcome::Delivered));
    assert_eq!(sink.delivered.len(), 2);
}
