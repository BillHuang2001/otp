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
//! The link table's suite: the routing entry over both halves (an outbound link and a link a
//! peer dialled into this runtime), the connected-node union, and the typed refusals.
//! A separate file so the fixtures over a real loopback socket pair do not grow
//! `src/dist/links.rs` (the crate's `port/tests.rs` / `runtime/scheduler/tests.rs` pattern).

use super::*;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;

use beam_alloc::Heap;
use beam_codec::term::{make_external_pid, make_external_ref};
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::{Delivery, StepOutcome, UnsupportedSink};
use beam_process::{ProcessId, RemotePid};
use beam_term::prelude::{make_atom, make_small, AtomTable};

use crate::dist::DIST_READ_SLICE;
use crate::dist_client::DistClient;

/// The cookie every loopback fixture below digests with.
const COOKIE: &[u8] = b"dist-links-unit-cookie";

/// The peer that plays the *connecting* role in an accepted-link fixture.
const PEER_NAME: &str = "peer@host.example";

/// This runtime's own node name in an accepted-link fixture.
const ACCEPTOR_NAME: &str = "acceptor@host.example";

/// A connected loopback pair, each end configured exactly like a real link
/// (`TCP_NODELAY` + the [`DIST_READ_SLICE`] read timeout) — the `dist` suite's fixture,
/// copied because its own helper is private to that module.
fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("the listener's address");
    let client = TcpStream::connect(addr).expect("connect the loopback listener");
    let (server, _) = listener.accept().expect("accept the loopback connection");
    for stream in [&client, &server] {
        stream.set_nodelay(true).expect("TCP_NODELAY");
        stream
            .set_read_timeout(Some(DIST_READ_SLICE))
            .expect("the link's read slice");
    }
    (server, client)
}

/// An **established** accepted link, returned with the peer's driver and socket.
///
/// The runtime side is a real [`DistConnection`] over one end of a loopback pair (the
/// production `ConnectionDriver::server`), the peer the connecting side of the same
/// `'N'` handshake — so the link is established by exactly the bytes a real peer sends,
/// with no `erl` and no `epmd`.
fn establishing_accepted() -> (DistConnection, ConnectionDriver, TcpStream) {
    establishing_accepted_with_peer_flags(beam_dist::preferred_flags())
}

/// [`establishing_accepted`] with an explicit **peer** (connecting-side) flag set, so a
/// fixture can withhold a non-mandatory capability — `DFLAG_ALIAS` in particular.
fn establishing_accepted_with_peer_flags(
    peer_flags: beam_dist::DistFlags,
) -> (DistConnection, ConnectionDriver, TcpStream) {
    let (server_stream, mut peer_stream) = socket_pair();
    let peer_addr = server_stream.peer_addr().expect("the peer's address");
    let mut connection = DistConnection::from_parts(
        ConnectionDriver::server(ACCEPTOR_NAME.as_bytes(), COOKIE, 1),
        server_stream,
        peer_addr,
    );
    let mut peer =
        ConnectionDriver::client_with_flags(PEER_NAME.as_bytes(), COOKIE, 1, peer_flags);
    for _ in 0..200 {
        connection.pump().expect("the accepted link's pump");
        read_into(&mut peer, &mut peer_stream);
        step_peer(&mut peer, &mut peer_stream);
        if connection.is_established() && matches!(peer.state(), DriverState::Established) {
            return (connection, peer, peer_stream);
        }
    }
    panic!("the loopback 'N' handshake never completed");
}

/// Binds an established accepted link's peer **node atom term**, exactly as
/// [`crate::dist::DistNode::bind_peer_nodes`] does from the scheduler pass: the peer's
/// `node@host` name is interned in the live table and the resulting term handed to the
/// link. Returns the bound term so a test can address the peer by it.
fn bind_peer(connection: &mut DistConnection, atoms: &mut AtomTable) -> Eterm {
    let peer = connection
        .peer_name()
        .expect("an established link knows its peer's name");
    let atom = make_atom(atoms.intern(&peer));
    connection.bind_node(atom);
    atom
}

/// A toolchain-free **outbound** pair: a [`DistClient`] that dialled a loopback listener
/// speaking the server side of the handshake, established and ready to route. The
/// `dist_client` suite's fixture, copied for the same reason as [`socket_pair`].
fn dialled_outbound() -> (DistClient, ConnectionDriver, TcpStream, AtomTable) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let addr = listener.local_addr().expect("read the port");
    let mut atoms = AtomTable::new();
    let our = make_atom(atoms.intern(ACCEPTOR_NAME));
    let client = DistClient::new(COOKIE.to_vec()).expect("client");
    client.set_identity(ACCEPTOR_NAME, our, 1, COOKIE);
    client
        .connect_addr(PEER_NAME, addr, &mut atoms)
        .expect("dial the loopback peer");
    let (mut stream, _) = listener.accept().expect("the client connected");
    stream
        .set_read_timeout(Some(DIST_READ_SLICE))
        .expect("read timeout");
    let mut peer = ConnectionDriver::server(PEER_NAME.as_bytes(), COOKIE, 1);
    for _ in 0..200 {
        client.pump(&mut atoms).expect("client pump");
        read_into(&mut peer, &mut stream);
        step_peer(&mut peer, &mut stream);
        if matches!(peer.state(), DriverState::Established) && client.is_connected(PEER_NAME) {
            return (client, peer, stream, atoms);
        }
    }
    panic!("the loopback handshake never completed");
}

/// Feeds the peer's driver whatever the link wrote.
fn read_into(peer: &mut ConnectionDriver, stream: &mut TcpStream) {
    let mut buf = [0u8; 8192];
    match stream.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => peer.feed(&buf[..n]),
        Err(err) if is_would_block(&err) => {}
        Err(err) => panic!("the peer's read: {err}"),
    }
}

/// Steps the peer's driver to quiescence and writes whatever it wants to say.
fn step_peer(peer: &mut ConnectionDriver, stream: &mut TcpStream) {
    loop {
        match peer.step() {
            Ok(StepOutcome::WouldBlock) => break,
            Ok(_) => {}
            Err(err) => panic!("the peer's step: {err}"),
        }
    }
    if peer.has_output() {
        let bytes = peer.take_output();
        stream.write_all(&bytes).expect("the peer's write");
        stream.flush().expect("the peer's flush");
    }
}

/// What the peer decoded from the bytes the link wrote (`DOP_*` classification only;
/// the per-link identity check is a different seam).
///
/// `heap` is the **caller's**: every term a `Delivery` carries is materialized there, so
/// it must outlive the assertions (a heap owned by this helper would be dropped on
/// return, leaving the returned terms naming recycled memory).
fn decoded(
    peer: &mut ConnectionDriver,
    stream: &mut TcpStream,
    atoms: &mut AtomTable,
    heap: &mut Heap,
) -> Vec<Delivery> {
    read_into(peer, stream);
    peer.step().expect("the peer's step");
    let mut sink = UnsupportedSink;
    peer.dispatch_inbound(heap, atoms, &mut sink)
        .expect("classify the control message the link wrote")
        .into_iter()
        .map(|record| record.delivery)
        .collect()
}

/// A read that timed out (or would have blocked) is "no data", never an error.
fn is_would_block(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// A message tuple `{hello, N}` on `heap`.
fn hello(heap: &mut Heap, atoms: &mut AtomTable, n: i64) -> Eterm {
    let tag = make_atom(atoms.intern("hello"));
    heap.tuple(&[tag, make_small(n)]).expect("the message tuple")
}

/// The routing entry finds a link this runtime **dialled**: a `DOP_REG_SEND` written
/// through it arrives at the peer as the same `{DOP_REG_SEND, From, Cookie, ToName} --
/// Msg` control message.
#[test]
fn the_routing_entry_finds_an_outbound_link() {
    let (client, mut peer, mut stream, mut atoms) = dialled_outbound();
    let mut heap = Heap::new(64).expect("the message heap");
    let msg = hello(&mut heap, &mut atoms, 42);
    client
        .send_reg_send(PEER_NAME, "echo", ProcessId::new(1, 1), msg, &mut atoms)
        .expect("route the DOP_REG_SEND over the outbound link");

    let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
    match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
        Some(Delivery::RegSend { name, message, .. }) => {
            assert_eq!(
                *name,
                make_atom(atoms.intern("echo")),
                "the routed control names the registered process"
            );
            let mut expected_heap = Heap::new(64).expect("the expected heap");
            let expected = hello(&mut expected_heap, &mut atoms, 42);
            assert!(
                beam_term::compare::eq(*message, expected, &atoms),
                "the routed message reaches the outbound peer intact"
            );
        }
        other => panic!("expected a DOP_REG_SEND on the outbound link, got {other:?}"),
    }
}

/// The routing entry finds a link a peer **dialled into** this runtime — the regression
/// this table exists for: when a system `erl` dials `beam_rt`, the runtime's only link is
/// an accepted one, and an outbound `DOP_REG_SEND` must still reach it.
#[test]
fn the_routing_entry_finds_an_accepted_link() {
    let (mut connection, mut peer, mut stream) = establishing_accepted();
    let mut atoms = AtomTable::new();
    // The link's peer node term, bound exactly as the scheduler pass binds it — the
    // accepted half is an exact node-term match, so an unbound link is no route.
    let node = bind_peer(&mut connection, &mut atoms);
    assert_eq!(node, make_atom(atoms.intern(PEER_NAME)));
    let accepted = Rc::new(RefCell::new(vec![connection]));
    let mut links = DistLinks::new(Rc::clone(&accepted));
    let mut heap = Heap::new(64).expect("the message heap");
    let from = make_external_pid(
        &mut heap,
        make_atom(atoms.intern(PEER_NAME)),
        1,
        0,
        1,
    )
    .expect("the sender's external pid");
    let empty = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern("echo"));
    let msg = hello(&mut heap, &mut atoms, 7);
    links
        .route_control(node, Dop::RegSend, &[from, empty, to_name, msg], &atoms)
        .expect("the routing entry finds the accepted link");

    let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
    match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
        Some(Delivery::RegSend { name, message, .. }) => {
            assert_eq!(
                *name,
                make_atom(atoms.intern("echo")),
                "the routed control names the registered process"
            );
            let mut expected_heap = Heap::new(64).expect("the expected heap");
            let expected = hello(&mut expected_heap, &mut atoms, 7);
            assert!(
                beam_term::compare::eq(*message, expected, &atoms),
                "the routed message reaches the accepted peer intact"
            );
        }
        other => panic!("expected a DOP_REG_SEND on the accepted link, got {other:?}"),
    }
}

/// A node with no link in **either** half is the typed refusal, for the control entry
/// and the message entry alike — never a silent drop and never a fabricated success.
#[test]
fn a_node_with_no_link_is_a_typed_error() {
    let mut links = DistLinks::new(Rc::new(RefCell::new(Vec::new())));
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("absent@host.example"));

    let err = links
        .route_control(node, Dop::Link, &[make_small(1), make_small(2)], &atoms)
        .expect_err("no link on either half, so the control must be refused");
    assert!(
        err.to_string()
            .contains("no established connection to node absent@host.example"),
        "the control entry reports the typed refusal: {err}"
    );

    let err = links
        .route_message(node, make_small(1), make_small(2), make_small(3), &atoms)
        .expect_err("no link on either half, so the message must be refused");
    assert!(
        err.to_string()
            .contains("no established connection to node absent@host.example"),
        "the message entry reports the typed refusal: {err}"
    );
}

/// The runtime's [`DistSignalRouter`](beam_process::DistSignalRouter) reaches an accepted
/// link: a routed `DistSignal::Exit` is queued by the shared cell's table and written as a
/// `DOP_EXIT` the accepted peer decodes.
#[test]
fn the_router_reaches_an_accepted_link() {
    let (mut connection, mut peer, mut stream) = establishing_accepted();
    let mut atoms = AtomTable::new();
    let bound = bind_peer(&mut connection, &mut atoms);
    let accepted = Rc::new(RefCell::new(vec![connection]));
    let our = make_atom(atoms.intern(ACCEPTOR_NAME));
    let client = DistClient::new(COOKIE.to_vec()).expect("client");
    client.set_identity(ACCEPTOR_NAME, our, 1, COOKIE);
    client
        .share_links_with(Rc::clone(&accepted))
        .expect("install the node's accepted links");

    let mut heap = Heap::new(64).expect("the reason heap");
    let remote = RemotePid::from_eterm(
        make_external_pid(&mut heap, bound, 5, 0, 1).expect("the target's external pid"),
    )
    .expect("a remote target pid");
    let boom = make_atom(atoms.intern("boom"));
    let reason = heap
        .tuple(&[boom, make_small(17)])
        .expect("the exit reason");
    let mut router = client.router();
    router
        .route(beam_process::DistSignal::Exit {
            from: ProcessId::new(2, 0),
            to: remote,
            reason,
        })
        .expect("the router accepts a signal for an accepted link's node");
    client.pump(&mut atoms).expect("flush the queued exit");

    let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
    match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
        Some(Delivery::Exit { reason, .. }) => {
            let mut expected_heap = Heap::new(64).expect("the expected heap");
            let expected = expected_heap
                .tuple(&[boom, make_small(17)])
                .expect("the expected reason");
            assert!(
                beam_term::compare::eq(*reason, expected, &atoms),
                "the routed exit reason reaches the accepted peer intact"
            );
        }
        other => panic!("expected a DOP_EXIT on the accepted link, got {other:?}"),
    }
}

/// A routed signal for a node **no link reaches** is the typed refusal even when
/// another link is present — and it is refused **by the router**, not accepted and
/// silently dropped by the later flush (contract A.5: a signal for a node with no link
/// is a typed error, never a silent drop).
///
/// The fixture holds one **established accepted** link bound to peer
/// `peer@host.example`, and addresses `other@host.example`: an accepted half that only
/// knew "some link exists" would have answered `Ok` here, queued the control, and let
/// `flush_pending` drop it with nothing but a recorded reason to show for it.
#[test]
fn a_routed_signal_for_an_unlinked_node_is_a_typed_error_even_with_another_link_present() {
    let (mut connection, _peer, _stream) = establishing_accepted();
    let mut atoms = AtomTable::new();
    let bound = bind_peer(&mut connection, &mut atoms);
    assert_eq!(
        bound,
        make_atom(atoms.intern(PEER_NAME)),
        "the accepted link is bound to its own peer"
    );
    let accepted = Rc::new(RefCell::new(vec![connection]));
    let our = make_atom(atoms.intern(ACCEPTOR_NAME));
    let client = DistClient::new(COOKIE.to_vec()).expect("client");
    client.set_identity(ACCEPTOR_NAME, our, 1, COOKIE);
    client
        .share_links_with(Rc::clone(&accepted))
        .expect("install the node's accepted links");

    let mut heap = Heap::new(64).expect("the reason heap");
    let other = make_atom(atoms.intern("other@host.example"));
    let remote =
        RemotePid::from_eterm(make_external_pid(&mut heap, other, 5, 0, 1).expect("the pid"))
            .expect("a remote target pid");
    let reason = make_atom(atoms.intern("normal"));
    let mut router = client.router();
    let err = router
        .route(beam_process::DistSignal::Exit {
            from: ProcessId::new(2, 0),
            to: remote,
            reason,
        })
        .expect_err("no link reaches other@host.example, so the signal must be refused");
    assert!(
        err.to_string()
            .contains("no established connection to node"),
        "the router reports the typed refusal: {err}"
    );
    assert_eq!(
        client.pump(&mut atoms).expect("pump the client"),
        0,
        "nothing was queued for the peer"
    );
    assert!(
        client.last_error().is_none(),
        "the refusal happened at the router, not as a silent flush drop: {:?}",
        client.last_error()
    );
}

/// An alias send reaches an accepted peer as `{DOP_ALIAS_SEND, From, To} -- Msg`
/// (`dist.c:2563`): the destination node is read out of the alias **reference** term
/// itself, so the routing entry needs no separate node argument.
///
/// The link's peer advertised `DFLAG_ALIAS` (it is in OTP 29's default mask), so the
/// gate passes and the opcode is written; the peer decodes the very reference term the
/// caller held (compared structurally, `beam_term::compare::eq`) and the message.
#[test]
fn an_alias_send_writes_a_dop_alias_send() {
    let (mut connection, mut peer, mut stream) = establishing_accepted();
    let mut atoms = AtomTable::new();
    let node = bind_peer(&mut connection, &mut atoms);
    let accepted = Rc::new(RefCell::new(vec![connection]));
    let mut links = DistLinks::new(Rc::clone(&accepted));
    let mut heap = Heap::new(64).expect("the alias heap");
    let to = make_external_ref(&mut heap, node, &[7, 11], 5).expect("the alias reference");
    let from = make_external_pid(&mut heap, make_atom(atoms.intern(ACCEPTOR_NAME)), 9, 0, 1)
        .expect("the sender's external pid");
    let msg = hello(&mut heap, &mut atoms, 3);
    links
        .route_alias(to, from, msg, &atoms)
        .expect("the routing entry writes the DOP_ALIAS_SEND");

    let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
    match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
        Some(Delivery::AliasSend {
            to: got_to,
            message,
            ..
        }) => {
            assert!(
                beam_term::compare::eq(*got_to, to, &atoms),
                "the alias reference reaches the peer intact (same node/ids/creation)"
            );
            let mut expected_heap = Heap::new(64).expect("the expected heap");
            let expected = hello(&mut expected_heap, &mut atoms, 3);
            assert!(
                beam_term::compare::eq(*message, expected, &atoms),
                "the routed message reaches the accepted peer intact"
            );
        }
        other => panic!("expected a DOP_ALIAS_SEND, got {other:?}"),
    }
}

/// A link whose peer did **not** offer `DFLAG_ALIAS` refuses the alias send with the
/// typed `did not negotiate DFLAG_ALIAS` — never a silent drop, never a bare `BadArg`.
///
/// OTP 29 advertises `DFLAG_ALIAS` in its default **and** addable masks but does not
/// make it mandatory, so a peer may establish a link without it: the fixture's peer
/// offers only the mandatory set, which still completes the `'N'` handshake.
#[test]
fn an_alias_send_without_the_alias_flag_is_a_typed_error() {
    let (mut connection, mut peer, mut stream) =
        establishing_accepted_with_peer_flags(beam_dist::DistFlags::from_bits(
            beam_dist::flags::DFLAG_DIST_MANDATORY,
        ));
    assert!(
        !negotiated_alias(connection.negotiated()),
        "the fixture's link did not negotiate DFLAG_ALIAS"
    );
    let mut atoms = AtomTable::new();
    let node = bind_peer(&mut connection, &mut atoms);
    let accepted = Rc::new(RefCell::new(vec![connection]));
    let mut links = DistLinks::new(Rc::clone(&accepted));
    let mut heap = Heap::new(64).expect("the alias heap");
    let to = make_external_ref(&mut heap, node, &[1, 2], 1).expect("the alias reference");
    let from = make_external_pid(&mut heap, make_atom(atoms.intern(ACCEPTOR_NAME)), 2, 0, 1)
        .expect("the sender's external pid");
    let msg = hello(&mut heap, &mut atoms, 5);

    let err = links
        .route_alias(to, from, msg, &atoms)
        .expect_err("a link without DFLAG_ALIAS must refuse an alias send");
    assert!(
        err.to_string().contains("DFLAG_ALIAS"),
        "the refusal names the missing capability: {err}"
    );
    assert!(
        err.to_string()
            .contains("the link to peer@host.example did not negotiate DFLAG_ALIAS"),
        "the refusal names the link's node: {err}"
    );
    let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
    assert!(
        decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).is_empty(),
        "nothing was written to the peer"
    );
}

/// The **message** entry — not only the control entry — reaches a link a peer **dialled into**
/// this runtime: `route_message` writes the `DOP_SEND`/`DOP_SEND_SENDER` frame on the accepted
/// link and the peer decodes the very destination and message the caller held.
///
/// This is the plain-message half of A.5's routing union: a system `erl` that dialled `beam_rt`
/// holds an accepted link only, and `erlang:send/2` to a process there must reach the wire.
#[test]
fn the_message_entry_reaches_an_accepted_link() {
    let (mut connection, mut peer, mut stream) = establishing_accepted();
    let mut atoms = AtomTable::new();
    let node = bind_peer(&mut connection, &mut atoms);
    let accepted = Rc::new(RefCell::new(vec![connection]));
    let mut links = DistLinks::new(Rc::clone(&accepted));
    assert!(
        links.outbound.is_empty(),
        "the fixture holds an accepted link only, so nothing but the accepted half can route"
    );
    let mut heap = Heap::new(64).expect("the message heap");
    let from = make_external_pid(&mut heap, make_atom(atoms.intern(ACCEPTOR_NAME)), 1, 0, 1)
        .expect("the sender's external pid");
    let to = make_external_pid(&mut heap, node, 9, 0, 1).expect("the target's external pid");
    let msg = hello(&mut heap, &mut atoms, 11);
    links
        .route_message(node, from, to, msg, &atoms)
        .expect("the message entry finds the accepted link");

    let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
    match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
        Some(Delivery::Send {
            to: got_to,
            message,
            ..
        }) => {
            assert!(
                beam_term::compare::eq(*got_to, to, &atoms),
                "the routed destination reaches the accepted peer intact"
            );
            let mut expected_heap = Heap::new(64).expect("the expected heap");
            let expected = hello(&mut expected_heap, &mut atoms, 11);
            assert!(
                beam_term::compare::eq(*message, expected, &atoms),
                "the routed message reaches the accepted peer intact"
            );
        }
        other => panic!("expected a DOP_SEND frame on the accepted link, got {other:?}"),
    }
}

/// The runtime's connected-node view is the **union** of both halves: a peer that dialled into
/// this runtime is a connected node, even though nothing was ever dialled out.
///
/// The fixtures here are the whole chain the runtime delegates over:
/// [`DistLinks::established_names`] (the one union rule) and, on that same shared table,
/// [`DistClient::all_connected_nodes`] / [`DistClient::is_connected_any_half`] — so
/// `Runtime::dist_connected_nodes` and `Runtime::dist_is_connected` (one-line delegations) cover
/// a peer that opened the link, while [`DistClient::connected_nodes`] (outbound-only)
/// legitimately stays empty.
#[test]
fn the_connected_node_view_includes_a_peer_that_dialled_in() {
    let (mut connection, _peer, _stream) = establishing_accepted();
    let mut atoms = AtomTable::new();
    let bound = bind_peer(&mut connection, &mut atoms);
    assert_eq!(
        bound,
        make_atom(atoms.intern(PEER_NAME)),
        "the accepted link is bound to its own peer"
    );
    let accepted = Rc::new(RefCell::new(vec![connection]));
    let links = DistLinks::new(Rc::clone(&accepted));
    assert_eq!(
        links.established_names(),
        vec![PEER_NAME.to_string()],
        "the union's one rule reports the accepted link's peer"
    );

    let our = make_atom(atoms.intern(ACCEPTOR_NAME));
    let client = DistClient::new(COOKIE.to_vec()).expect("client");
    client.set_identity(ACCEPTOR_NAME, our, 1, COOKIE);
    client
        .share_links_with(Rc::clone(&accepted))
        .expect("install the node's accepted links");

    assert_eq!(
        client.all_connected_nodes(),
        vec![PEER_NAME.to_string()],
        "the connected-node view is the union, so the peer that dialled in is reported"
    );
    assert!(
        client.is_connected_any_half(PEER_NAME),
        "the peer that dialled in answers `connected` on the union view"
    );
    assert!(
        client.connected_nodes().is_empty(),
        "the outbound-only view is unchanged: this runtime dialled nobody"
    );
    assert!(
        !client.is_connected(PEER_NAME),
        "the outbound-only predicate is unchanged: no link to that peer was dialled"
    );
}

/// A peer name that matches **no** link is absent from the union view — never a fabricated
/// membership, on an accepted-only table or an empty one.
///
/// The fixture holds one established accepted link to `peer@host.example` and asks about
/// `other@host.example`: a union that answered "some link exists" would report a connected node
/// this runtime cannot reach.
#[test]
fn an_unlinked_peer_is_absent_from_the_connected_node_view() {
    let other = "other@host.example";
    let (mut connection, _peer, _stream) = establishing_accepted();
    let mut atoms = AtomTable::new();
    let bound = bind_peer(&mut connection, &mut atoms);
    assert_ne!(
        bound,
        make_atom(atoms.intern(other)),
        "the accepted link is bound to its own peer, never to the absent one"
    );
    let accepted = Rc::new(RefCell::new(vec![connection]));
    let client = DistClient::new(COOKIE.to_vec()).expect("client");
    client
        .share_links_with(Rc::clone(&accepted))
        .expect("install the node's accepted links");

    assert!(
        !client.is_connected_any_half(other),
        "no link reaches {other}, so it is not a connected node"
    );
    assert_eq!(
        client.all_connected_nodes(),
        vec![PEER_NAME.to_string()],
        "the union reports exactly the one peer a link reaches"
    );

    let empty = DistClient::new(COOKIE.to_vec()).expect("client");
    empty
        .share_links_with(Rc::new(RefCell::new(Vec::new())))
        .expect("install an empty accepted table");
    assert!(
        empty.all_connected_nodes().is_empty(),
        "a runtime with no link at all has no connected node"
    );
    assert!(
        !empty.is_connected_any_half(PEER_NAME),
        "a runtime with no link at all is connected to nobody"
    );
}