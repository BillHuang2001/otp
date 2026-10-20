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

//! The client half's colocated suite: the `'N'` handshake against a server driver, node
//! visibility, the outbound message and routed-signal path, and the `epmd` lookup.
//!
//! Hermetic — a loopback socket and a server-side `ConnectionDriver`, no `erl` and no real
//! `epmd` (the fixture style of `src/dist/links/tests.rs`). It is a child module of the client
//! half, so `use super::*;` reaches the façade and `super::<file>` the sibling helpers.

use super::*;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;

use beam_alloc::Heap as TestHeap;
use beam_codec::term::{external_pid_parts, external_ref_parts, make_external_pid, make_external_ref};
use beam_common::BeamError;
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::flags::DFLAG_PUBLISHED;
use beam_dist::{
    preferred_flags, publish_flag, Delivery, DistFlags, Dop, StepOutcome, UnsupportedSink,
};
use beam_process::{
    DistSignal, MonitorKind, ProcessId, RefAllocator, RemotePid, RemoteRef, RemoteWatcherRef,
};
use beam_term::prelude::{make_atom as atom_term, make_small, AtomTable as Table};
use beam_term::AtomTable;

use crate::dist::DIST_READ_SLICE;

use super::handshake::is_would_block;
use super::lookup::{lookup_dist_port, lookup_dist_port_at};
use super::signals::{signal_node, unlink_id_term};

/// A toolchain-free loopback pair: the client dials a local listener that speaks
/// the **server** side of the handshake. Returns the client and the peer's driver
/// and socket.
fn loopback(
    cookie: &str,
    creation: u32,
) -> (DistClient, ConnectionDriver, TcpStream, AtomTable) {
    loopback_with_visibility(cookie, creation, false)
}
/// [`loopback`] with this node's **visibility** chosen (`-hidden` when `hidden`), so a
/// test can compare the flags the connector puts on the wire for both kinds of node.
fn loopback_with_visibility(
    cookie: &str,
    creation: u32,
    hidden: bool,
) -> (DistClient, ConnectionDriver, TcpStream, AtomTable) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let addr = listener.local_addr().expect("read the port");
    let mut atoms = Table::new();
    let our = atom_term(atoms.intern("client@localhost"));
    let client = DistClient::new(cookie.as_bytes().to_vec()).expect("client");
    client.set_hidden(hidden);
    client.set_identity("client@localhost", our, creation, cookie.as_bytes());
    client
        .connect_addr("peer@localhost", addr, &mut atoms)
        .expect("dial the loopback peer");
    let (stream, _) = listener.accept().expect("the client connected");
    stream
        .set_read_timeout(Some(DIST_READ_SLICE))
        .expect("read timeout");
    let driver = ConnectionDriver::server(b"peer@localhost", cookie.as_bytes(), creation);
    (client, driver, stream, atoms)
}

/// Drives both sides until each reports `Established` (bounded).
fn drive(
    client: &DistClient,
    driver: &mut ConnectionDriver,
    stream: &mut TcpStream,
    atoms: &mut AtomTable,
) {
    for _ in 0..200 {
        client.pump(atoms).expect("client pump");
        let mut buf = [0u8; 8192];
        match stream.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => driver.feed(&buf[..n]),
            Err(err) if is_would_block(&err) => {}
            Err(err) => panic!("peer read: {err}"),
        }
        loop {
            match driver.step() {
                Ok(StepOutcome::WouldBlock) => break,
                Ok(_) => {}
                Err(err) => panic!("peer step: {err}"),
            }
        }
        if driver.has_output() {
            let bytes = driver.take_output();
            stream.write_all(&bytes).expect("peer write");
            stream.flush().expect("peer flush");
        }
        if matches!(driver.state(), DriverState::Established)
            && client.is_connected("peer@localhost")
        {
            return;
        }
    }
    panic!("the loopback handshake never completed");
}

/// An external pid for `peer@localhost`, built on a throwaway heap.
fn remote_pid(atoms: &mut AtomTable, number: u32, creation: u32) -> RemotePid {
    let mut heap = TestHeap::new(16).expect("heap");
    let node = atom_term(atoms.intern("peer@localhost"));
    RemotePid::from_eterm(
        make_external_pid(&mut heap, node, number, 0, creation).expect("external pid"),
    )
    .expect("a remote pid")
}

/// The connector completes the `'N'` handshake against a server driver, and the
/// link reports itself established — the state-machine half, with no `erl`.
#[test]
fn the_client_handshake_completes_against_a_server_driver() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 7);
    assert!(!client.is_connected("peer@localhost"), "not connected yet");
    drive(&client, &mut driver, &mut stream, &mut atoms);
    assert_eq!(client.connected_nodes(), vec!["peer@localhost".to_string()]);
    assert!(client.last_error().is_none(), "no link was retired");
}

/// **Contract A.5 — this node's visibility, dialling half.** A node's visibility is a
/// property of the node, not of who opened the socket, so a link this runtime dials
/// advertises exactly what the accepted half advertises
/// ([`crate::dist::advertised_flags`]): the emulator default **plus**
/// `DFLAG_PUBLISHED` (`dist.h:30`, `dist_util.erl:152-160`) unless the node is
/// `-hidden`. The peer's `ConnectionDriver::client_flags` is its record of our `Name`
/// message, i.e. the bytes that crossed the socket.
#[test]
fn the_client_publishes_the_node_unless_it_is_hidden() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 5);
    drive(&client, &mut driver, &mut stream, &mut atoms);
    let published = DistFlags::from_bits(DFLAG_PUBLISHED);
    assert!(!client.is_hidden(), "a client is published unless `-hidden` says so");
    assert!(
        driver.client_flags().contains(published),
        "the peer read our flags as 0x{:x}, which omits DFLAG_PUBLISHED (0x{:x}) — \
         it would list this node only under `nodes(hidden)`",
        driver.client_flags().bits(),
        DFLAG_PUBLISHED
    );
    assert_eq!(
        driver.client_flags().bits(),
        (preferred_flags() | publish_flag(false, true)).bits(),
        "the advertised set is the emulator default plus exactly DFLAG_PUBLISHED"
    );
    assert!(
        driver.client_flags().contains(preferred_flags()),
        "every capability of the emulator default is still advertised"
    );
}

/// The `-hidden` half of the same rule: a hidden node's outbound link advertises the
/// emulator default **unchanged**, so the peer classifies it as hidden
/// (`publish_type/1`) and keeps it out of `nodes()` — while the link still establishes.
#[test]
fn a_hidden_client_withholds_the_published_bit() {
    let (client, mut driver, mut stream, mut atoms) = loopback_with_visibility("cook", 5, true);
    drive(&client, &mut driver, &mut stream, &mut atoms);
    assert!(client.is_hidden(), "`set_hidden(true)` takes effect");
    assert!(
        !driver.client_flags().contains(DistFlags::from_bits(DFLAG_PUBLISHED)),
        "the peer read the hidden node's flags as 0x{:x}, which still sets \
         DFLAG_PUBLISHED (0x{:x})",
        driver.client_flags().bits(),
        DFLAG_PUBLISHED
    );
    assert_eq!(
        driver.client_flags().bits(),
        preferred_flags().bits(),
        "a hidden node advertises the emulator default, unchanged"
    );
    assert_eq!(
        client.connected_nodes(),
        vec!["peer@localhost".to_string()],
        "a hidden node still connects: visibility changes who lists us, not whether \
         the link works"
    );
}
/// A `DOP_REG_SEND` written through the client's public send API is decoded by the
/// peer's driver as the same `{DOP_REG_SEND, From, Cookie, ToName} -- Msg` tuple —
/// the outbound message path, with no `erl`.
#[test]
fn a_reg_send_reaches_the_peer_decoded() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 3);
    drive(&client, &mut driver, &mut stream, &mut atoms);

    let mut msg_heap = TestHeap::new(64).expect("heap");
    let hello = atom_term(atoms.intern("hello"));
    let msg = msg_heap.tuple(&[hello, make_small(42)]).expect("build the message");
    let from = ProcessId::new(1, 1);
    client
        .send_reg_send("peer@localhost", "echo", from, msg, &mut atoms)
        .expect("send DOP_REG_SEND");

    // Drain the bytes the client wrote and decode them on the peer.
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).expect("peer read");
    driver.feed(&buf[..n]);
    driver.step().expect("peer step");
    let mut heap = TestHeap::new(1 << 12).expect("heap");
    let mut sink = UnsupportedSink;
    let dispatched = driver
        .dispatch_inbound(&mut heap, &mut atoms, &mut sink)
        .expect("classify");
    let delivery = dispatched
        .first()
        .map(|record| record.delivery.clone())
        .expect("one control message was written");
    match delivery {
        Delivery::RegSend { name, message, .. } => {
            assert_eq!(name, atom_term(atoms.intern("echo")));
            // The decoded message is the same `{hello,42}` tuple.
            let mut expected_heap = TestHeap::new(64).expect("heap");
            let hello = atom_term(atoms.intern("hello"));
            let expected = expected_heap
                .tuple(&[hello, make_small(42)])
                .expect("expected message");
            assert!(
                beam_term::compare::eq(message, expected, &atoms),
                "the decoded message differs from {hello:?},42"
            );
        }
        other => panic!("expected a DOP_REG_SEND, got {other:?}"),
    }
}

/// A routed `DistSignal::Exit` is materialised **synchronously**: dropping the
/// signal's source heap right after `route` returns cannot affect what is later
/// written to the wire.
#[test]
fn a_routed_exit_survives_its_source_heap() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 9);
    drive(&client, &mut driver, &mut stream, &mut atoms);

    let remote = remote_pid(&mut atoms, 5, 9);
    // The reason lives on a heap that is dropped the moment `route` returns.
    let mut reason_heap = TestHeap::new(64).expect("heap");
    let reason = {
        let boom = atom_term(atoms.intern("boom"));
        reason_heap
            .tuple(&[boom, make_small(17)])
            .expect("build the reason")
    };
    {
        let mut router = client.router();
        router
            .route(DistSignal::Exit {
                from: ProcessId::new(2, 0),
                to: remote,
                reason,
            })
            .expect("queue the exit");
    }
    // The reason's source heap is gone; the queued copy must be intact.
    drop(reason_heap);

    client.pump(&mut atoms).expect("flush the queued exit");
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).expect("peer read");
    driver.feed(&buf[..n]);
    driver.step().expect("peer step");
    let mut heap = TestHeap::new(1 << 12).expect("heap");
    let mut sink = UnsupportedSink;
    let dispatched = driver
        .dispatch_inbound(&mut heap, &mut atoms, &mut sink)
        .expect("classify");
    match dispatched.first().map(|r| r.delivery.clone()) {
        Some(Delivery::Exit { reason, .. }) => {
            let mut expected_heap = TestHeap::new(64).expect("heap");
            let boom = atom_term(atoms.intern("boom"));
            let expected = expected_heap
                .tuple(&[boom, make_small(17)])
                .expect("expected reason");
            assert!(
                beam_term::compare::eq(reason, expected, &atoms),
                "the decoded exit reason differs from {boom:?},17"
            );
        }
        other => panic!("expected a DOP_EXIT, got {other:?}"),
    }
}

/// A monitor signal addresses the peer's node and carries the **watcher first**
/// (the C's `TUPLE4(DOP_MONITOR_P, watcher, watched, ref)`), cross-checked against
/// the `live_interop` evidence.
#[test]
fn a_routed_monitor_uses_the_otp_argument_order() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 4);
    drive(&client, &mut driver, &mut stream, &mut atoms);

    let remote = remote_pid(&mut atoms, 7, 4);
    let watcher = ProcessId::new(3, 0);
    let reference = RefAllocator::new().next();
    let mut router = client.router();
    router
        .route(DistSignal::Monitor {
            watcher,
            target: remote,
            reference,
            kind: MonitorKind::Process,
        })
        .expect("queue the monitor");
    client.pump(&mut atoms).expect("flush the queued monitor");

    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).expect("peer read");
    driver.feed(&buf[..n]);
    driver.step().expect("peer step");
    let mut heap = TestHeap::new(1 << 12).expect("heap");
    let mut sink = UnsupportedSink;
    let dispatched = driver
        .dispatch_inbound(&mut heap, &mut atoms, &mut sink)
        .expect("classify");
    match dispatched.first().map(|r| r.delivery.clone()) {
        Some(Delivery::Monitor { watcher: w, watched, .. }) => {
            // The watcher is our local pid (element 0) as an *external* pid for the
            // client node, and the watched process is the remote pid (element 1).
            let expected = make_external_pid(
                &mut heap,
                atom_term(atoms.intern("client@localhost")),
                watcher.number() as u32,
                watcher.serial() as u32,
                4,
            )
            .expect("expected watcher pid");
            assert!(
                beam_term::compare::eq(w, expected, &atoms),
                "the decoded watcher is not the local pid as an external pid"
            );
            assert_eq!(
                RemotePid::from_eterm(watched)
                    .expect("the watched process is an external pid")
                    .number(),
                remote.number()
            );
        }
        other => panic!("expected a DOP_MONITOR_P, got {other:?}"),
    }
}

/// A signal for a node with no link is a typed error — never a silent drop.
#[test]
fn a_signal_without_a_link_is_a_typed_error() {
    let client = DistClient::new(b"cook".to_vec()).expect("client");
    let mut atoms = Table::new();
    let our = atom_term(atoms.intern("client@localhost"));
    client.set_identity("client@localhost", our, 1, b"cook");
    let remote = {
        let mut heap = TestHeap::new(16).expect("heap");
        let node = atom_term(atoms.intern("absent@localhost"));
        RemotePid::from_eterm(
            make_external_pid(&mut heap, node, 1, 0, 1).expect("external pid"),
        )
        .expect("a remote pid")
    };
    let mut router = client.router();
    let err = router
        .route(DistSignal::Link {
            from: ProcessId::new(1, 0),
            to: remote,
        })
        .expect_err("no link, so the route must fail");
    assert!(
        err.to_string().contains("no established connection"),
        "unexpected error: {err}"
    );
}
/// The `DOP_UNLINK_ID` id is encoded **checked**: the largest encodable id is exact and
/// one past it is the typed resource limit, never a wrapped immediate.
#[test]
fn an_unlink_id_is_encoded_checked() {
    let max = beam_term::MAX_SMALL as u64;
    assert_eq!(
        unlink_id_term(max).expect("the largest id still fits"),
        make_small(beam_term::MAX_SMALL),
        "the id at the bound is encoded verbatim"
    );
    assert!(
        matches!(unlink_id_term(max + 1), Err(BeamError::SystemLimit)),
        "one past the bound is refused"
    );
    // The whole upper half of the `u64` space is unrepresentable too (an `i64` overflow,
    // not just a small-integer overflow).
    assert!(
        matches!(unlink_id_term(u64::MAX), Err(BeamError::SystemLimit)),
        "u64::MAX cannot fit an i64"
    );
}
/// A routed unlink whose counter has left the small-integer range fails **typed** — the
/// signal is refused instead of carrying a wrapped id a peer would ack against the wrong
/// unlink. The counter is parked at the bound directly (reaching it by routing
/// `2^59` signals is not a test).
#[test]
fn a_routed_unlink_past_the_small_range_is_a_typed_system_limit() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 5);
    drive(&client, &mut driver, &mut stream, &mut atoms);
    let remote = remote_pid(&mut atoms, 1, 5);
    let mut inner = client.inner.borrow_mut();
    // Just below the bound: the id is encoded and the signal is queued.
    inner.next_unlink_id = beam_term::MAX_SMALL as u64;
    inner
        .encode_signal(DistSignal::Unlink {
            from: ProcessId::new(3, 0),
            to: remote,
        })
        .expect("an id at the bound encodes");
    // One past it: the encode is a typed refusal, and nothing wrapped is produced.
    inner.next_unlink_id = beam_term::MAX_SMALL as u64 + 1;
    assert!(
        matches!(
            inner.encode_signal(DistSignal::Unlink {
                from: ProcessId::new(3, 0),
                to: remote,
            }),
            Err(BeamError::SystemLimit)
        ),
        "an id past the bound is a typed refusal"
    );
}

/// The outbound `epmd` lookup speaks to the **configured** `epmd`, not the
/// well-known one: a fake `epmd` on an ephemeral loopback port answers the
/// `PORT2_REQ` (`'z'` + the peer name, 2-byte framed) with a hand-encoded
/// `PORT2_RESP` (`'w'`, result 0, port(2) node_type(1) protocol(1)
/// high_vsn(2) low_vsn(2) namelen(2) name extralen(2) extra — unframed, per
/// `epmd_srv.c:904-936`), and the lookup resolves exactly the port it advertised.
///
/// The address is the parameter of [`lookup_dist_port_at`] (what
/// [`lookup_dist_port`] supplies [`crate::dist::epmd_addr`] for) because the
/// process environment is global: this test never mutates `$ERL_EPMD_PORT`.
#[test]
fn the_dist_port_lookup_asks_the_configured_epmd_address() {
    /// The port the fake `epmd` claims the peer listens on — deliberately not
    /// the well-known `4369`, nor a port the fake itself is bound to.
    const ADVERTISED: u16 = 45_678;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a fake epmd");
    let addr = listener.local_addr().expect("read the fake epmd's port");
    let fake = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the lookup connected");
        let mut header = [0u8; 2];
        stream
            .read_exact(&mut header)
            .expect("the 2-byte frame length");
        let len = usize::from(u16::from_be_bytes(header));
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).expect("the framed request");
        assert_eq!(
            body.first().copied(),
            Some(b'z'),
            "the request must be a PORT2_REQ ('z')"
        );
        assert_eq!(&body[1..], b"peer", "the request names the peer");
        let mut reply = vec![b'w', 0];
        reply.extend_from_slice(&ADVERTISED.to_be_bytes());
        reply.push(77); // node_type: normal (`EPMD_NODE_NORMAL`)
        reply.push(0); // protocol: tcp/ip (`EPMD_PROTO_TCP`)
        reply.extend_from_slice(&7u16.to_be_bytes()); // high_vsn
        reply.extend_from_slice(&6u16.to_be_bytes()); // low_vsn
        reply.extend_from_slice(&4u16.to_be_bytes()); // namelen
        reply.extend_from_slice(b"peer");
        reply.extend_from_slice(&0u16.to_be_bytes()); // extralen
        stream.write_all(&reply).expect("answer the PORT2_REQ");
        stream.flush().expect("flush the PORT2_RESP");
    });
    let resolved = lookup_dist_port_at(&addr.to_string(), b"peer")
        .expect("the configured epmd answers the lookup");
    assert_eq!(
        resolved, ADVERTISED,
        "the lookup resolves the port the configured epmd advertised"
    );
    fake.join().expect("the fake epmd finished");
}

/// [`lookup_dist_port`] — the function [`DistClient::connect`] calls — resolves
/// against [`crate::dist::epmd_addr`] and keeps `connect`'s error contract: a name
/// nobody registers is a typed [`BeamError::Io`] (the `NotFound` of an answering
/// `epmd`, or a connection error when none is listening), never a fabricated port
/// and never a panic.
#[test]
fn an_unregistered_peer_is_a_typed_error_not_a_fabricated_port() {
    // Two honest outcomes, both typed: an answering `epmd` that does not know the
    // name (`Io(NotFound)`, what `connect`'s contract promises), or no `epmd`
    // listening on the configured address (a connection error).
    match lookup_dist_port(b"beam_rt_no_such_node_for_the_lookup") {
        Ok(port) => panic!("a fabricated distribution port {port}"),
        Err(BeamError::Io(err)) => match err.kind() {
            // An `epmd` answered the `PORT2_REQ` and does not know the name.
            ErrorKind::NotFound => assert!(
                err.to_string().contains("beam_rt_no_such_node_for_the_lookup"),
                "a not-registered answer names the node: {err}"
            ),
            // No `epmd` is listening on the configured address.
            kind => assert!(
                matches!(
                    kind,
                    ErrorKind::ConnectionRefused
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::TimedOut
                        | ErrorKind::AddrNotAvailable
                ),
                "unexpected lookup failure: {kind} ({err})"
            ),
        },
        Err(other) => panic!("expected a typed Io error, got {other:?}"),
    }
}

/// An external reference for `peer@localhost`, **decomposed** the way the watcher table
/// records one a peer minted in its `DOP_MONITOR_P` (`RemoteWatcherRef::Remote`).
fn remote_ref(atoms: &mut AtomTable, creation: u32, ids: &[u32]) -> RemoteRef {
    let mut heap = TestHeap::new(32).expect("heap");
    let node = atom_term(atoms.intern("peer@localhost"));
    let term = make_external_ref(&mut heap, node, ids, creation).expect("external reference");
    RemoteRef::from_eterm(term).expect("a remote reference")
}

/// **Contract A.5 — `DOP_MONITOR_P_EXIT`'s argument order and reference spelling.** The wire
/// tuple is `{DOP_MONITOR_P_EXIT, Watched(local), Watcher(remote), Ref, Reason}` — *watched
/// first*, the reverse of `DOP_MONITOR_P`'s `{watcher, watched, ref}`
/// (`erts_dsig_send_m_exit`, `erts/emulator/beam/dist.c:1516`) — and a reference the **peer**
/// minted goes back **verbatim**: its own node, its own creation, its own id words. The
/// watched pid therefore carries *this* node, the watcher the peer's, and the reason has been
/// copied onto the scratch heap (its source heap is dropped before any assertion).
#[test]
fn a_monitor_exit_names_the_watched_first_and_returns_the_peer_reference_verbatim() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 6);
    drive(&client, &mut driver, &mut stream, &mut atoms);

    let our_node = atom_term(atoms.intern("client@localhost"));
    let peer_node = atom_term(atoms.intern("peer@localhost"));
    let watcher = remote_pid(&mut atoms, 7, 9);
    let watched = ProcessId::new(3, 1);
    let peer_ids = [0u32, 0xaaaa_bbbb, 0xcccc_dddd];
    let peer_ref = remote_ref(&mut atoms, 9, &peer_ids);
    // The reason lives on a heap that is dropped the moment the encode returns: the scratch
    // heap must hold the copy (`copy_struct_simple`), exactly as for `DOP_EXIT`.
    let mut reason_heap = TestHeap::new(64).expect("heap");
    let reason = {
        let boom = atom_term(atoms.intern("boom"));
        reason_heap
            .tuple(&[boom, make_small(17)])
            .expect("build the reason")
    };
    let (dop, args) = {
        let mut inner = client.inner.borrow_mut();
        inner
            .encode_signal(DistSignal::MonitorExit {
                watched,
                watcher,
                reference: RemoteWatcherRef::Remote(peer_ref),
                reason,
            })
            .expect("encode DOP_MONITOR_P_EXIT")
    };
    drop(reason_heap);

    assert_eq!(dop, Dop::MonitorPExit, "the plain 5-tuple form is emitted");
    assert_eq!(
        args.len(),
        4,
        "watched, watcher, reference, reason — and nothing else"
    );

    // Element 0: the **watched** process, i.e. this node's own pid as an external pid.
    let (node, number, _serial, creation) =
        external_pid_parts(args[0]).expect("the watched element is an external pid");
    assert_eq!(node, our_node, "the watched pid carries this node");
    assert_eq!(creation, 6, "…and this node's creation");
    assert_eq!(
        u64::from(number),
        watched.number(),
        "…and the watched process's own number"
    );

    // Element 1: the watcher, verbatim as the table recorded it.
    let (node, number, _serial, creation) =
        external_pid_parts(args[1]).expect("the watcher element is an external pid");
    assert_eq!(node, peer_node, "the watcher pid carries the peer's node");
    assert_eq!(creation, 9, "…and the peer's creation");
    assert_eq!(number, watcher.number(), "…and the watcher's own number");

    // Element 2: the peer-minted reference, byte-for-byte its own `{node, creation, ids}` —
    // never this node, never this node's creation.
    let (node, ids, creation) =
        external_ref_parts(args[2]).expect("the reference element is an external reference");
    assert_eq!(node, peer_node, "the reference keeps the peer's node");
    assert_eq!(creation, 9, "the reference keeps the peer's creation");
    assert_eq!(ids, peer_ids, "and the peer's own id words");

    // Element 3: the reason, on the stable scratch heap.
    let mut expected_heap = TestHeap::new(64).expect("heap");
    let boom = atom_term(atoms.intern("boom"));
    let expected = expected_heap
        .tuple(&[boom, make_small(17)])
        .expect("expected reason");
    assert!(
        beam_term::compare::eq(args[3], expected, &atoms),
        "the copied reason is not {{boom,17}}"
    );
}

/// The other reference spelling: a reference **this** node minted goes out through the
/// monitor family's own `{node, creation, ids}` builder ([`monitor_ref_term`]), so the peer's
/// `{'DOWN', …}` matches the monitor it installed with that reference.
#[test]
fn a_locally_minted_monitor_exit_reference_uses_the_monitor_spelling() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 8);
    drive(&client, &mut driver, &mut stream, &mut atoms);

    let our_node = atom_term(atoms.intern("client@localhost"));
    let watcher = remote_pid(&mut atoms, 4, 8);
    let reference = RefAllocator::new().next();
    let reason = atom_term(atoms.intern("killed"));
    let (dop, args) = {
        let mut inner = client.inner.borrow_mut();
        inner
            .encode_signal(DistSignal::MonitorExit {
                watched: ProcessId::new(5, 0),
                watcher,
                reference: RemoteWatcherRef::Local(reference),
                reason,
            })
            .expect("encode DOP_MONITOR_P_EXIT")
    };

    assert_eq!(dop, Dop::MonitorPExit);
    assert_eq!(args.len(), 4);
    let (node, ids, creation) =
        external_ref_parts(args[2]).expect("the reference element is an external reference");
    assert_eq!(node, our_node, "a locally minted reference is this node's");
    assert_eq!(creation, 8, "…with this node's creation");
    let id = reference.id();
    assert_eq!(
        ids,
        [
            (id & u64::from((1u32 << 18) - 1)) as u32,
            (id & u64::from(0xFFFC_0000u32)) as u32,
            (id >> 32) as u32,
        ],
        "the C's `erts_set_ref_numbers` pack (num[0] = the 18-bit number, num[1] = its \
         bits 18..31, num[2] = the rest), so an inbound decode with this node's \
         LocalIdentity restores exactly this id"
    );
}

/// [`signal_node`] for **every** [`DistSignal`] variant — the routing key each variant's wire
/// tuple addresses. `MonitorExit` is the one that is *not* named by its `to`/`target` element:
/// its tuple leads with the locally **watched** pid, so the control is written to the
/// **watcher's** node.
#[test]
fn every_routed_signal_addresses_the_peer_that_must_receive_it() {
    let mut atoms = Table::new();
    let peer_node = atom_term(atoms.intern("peer@localhost"));
    let peer = remote_pid(&mut atoms, 1, 2);
    let reference = RefAllocator::new().next();
    let from = ProcessId::new(3, 0);
    let reason = make_small(0);
    let v = [
        ("link", DistSignal::Link { from, to: peer }),
        ("unlink", DistSignal::Unlink { from, to: peer }),
        ("exit", DistSignal::Exit { from, to: peer, reason }),
        ("exit2", DistSignal::Exit2 { from, to: peer, reason }),
        (
            "monitor",
            DistSignal::Monitor {
                watcher: from,
                target: peer,
                reference,
                kind: MonitorKind::Process,
            },
        ),
        (
            "demonitor",
            DistSignal::Demonitor {
                watcher: from,
                target: peer,
                reference,
            },
        ),
        (
            "monitor-exit",
            DistSignal::MonitorExit {
                watched: from,
                watcher: peer,
                reference: RemoteWatcherRef::Local(reference),
                reason,
            },
        ),
    ];
    for (label, signal) in v {
        assert_eq!(
            signal_node(&signal),
            peer_node,
            "{label} must address the peer's node"
        );
    }
}

/// A routed `MonitorExit` is queued work until the flush, then it is on the wire — and the
/// peer decodes it as the C's `DOP_MONITOR_P_EXIT` tuple, **watched first**. This is the
/// quiescence account's evidence: the queue is non-empty (so
/// [`DistClient::has_pending_outbound`] is `true`, so a pass may not declare a stall) between
/// `route` and `pump`, and empty (with the link's driver output drained) after it.
#[test]
fn a_queued_monitor_exit_is_pending_until_the_flush_and_decodes_watched_first() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 11);
    drive(&client, &mut driver, &mut stream, &mut atoms);
    assert!(
        !client.has_pending_outbound(),
        "an established, pumped link with nothing queued owes the wire nothing"
    );

    let our_node = atom_term(atoms.intern("client@localhost"));
    let peer_node = atom_term(atoms.intern("peer@localhost"));
    let watcher = remote_pid(&mut atoms, 6, 11);
    let watched = ProcessId::new(2, 0);
    let peer_ref = remote_ref(&mut atoms, 11, &[0, 1, 2]);
    let reason = atom_term(atoms.intern("noproc"));
    {
        let mut router = client.router();
        router
            .route(DistSignal::MonitorExit {
                watched,
                watcher,
                reference: RemoteWatcherRef::Remote(peer_ref),
                reason,
            })
            .expect("queue the monitor exit");
    }
    assert!(
        client.has_pending_outbound(),
        "a queued signal is work the following flush still owes"
    );

    client
        .pump(&mut atoms)
        .expect("flush the queued monitor exit");
    assert!(
        !client.has_pending_outbound(),
        "the flush emptied the queue and wrote the link's output"
    );

    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).expect("peer read");
    driver.feed(&buf[..n]);
    driver.step().expect("peer step");
    let mut heap = TestHeap::new(1 << 12).expect("heap");
    let mut sink = UnsupportedSink;
    let dispatched = driver
        .dispatch_inbound(&mut heap, &mut atoms, &mut sink)
        .expect("classify");
    match dispatched.first().map(|r| r.delivery.clone()) {
        Some(Delivery::MonitorExit {
            watched: w,
            watcher: wt,
            reference,
            ..
        }) => {
            let (node, number, _serial, _creation) =
                external_pid_parts(w).expect("the decoded watched element is an external pid");
            assert_eq!(node, our_node, "the first element is this node's own pid");
            assert_eq!(
                u64::from(number),
                watched.number(),
                "…the process that died"
            );
            let (node, number, _serial, _creation) =
                external_pid_parts(wt).expect("the decoded watcher element is an external pid");
            assert_eq!(node, peer_node, "the second element is the peer's pid");
            assert_eq!(number, watcher.number(), "…the watcher");
            let (node, ids, creation) =
                external_ref_parts(reference).expect("the decoded reference is external");
            assert_eq!((node, creation), (peer_node, 11), "the peer's own reference identity");
            assert_eq!(ids, [0u32, 1, 2], "…with its own id words");
        }
        other => panic!("expected a DOP_MONITOR_P_EXIT, got {other:?}"),
    }
}

/// A link retired **before** it completed its `'N'` handshake records **no** lost peer: no
/// process can hold a relationship with a peer that never became a node, so a failed dial is
/// the diagnostic in [`DistClient::last_error`] and never a `noconnection` the scheduler pass
/// would apply (`crate::dist::links::LostPeers`).
///
/// The peer in this fixture accepts the connection and hangs up without answering the `'N'`
/// frame — the shape a re-dial to a peer that has just gone away leaves behind.
#[test]
fn a_retirement_that_never_handshook_records_no_lost_peer() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let addr = listener.local_addr().expect("read the port");
    let mut atoms = Table::new();
    let our = atom_term(atoms.intern("client@localhost"));
    let client = DistClient::new(b"cook".to_vec()).expect("client");
    client.set_identity("client@localhost", our, 1, b"cook");
    let lost = crate::dist::links::new_lost_peers();
    client.use_lost_peers(std::rc::Rc::clone(&lost));
    client
        .connect_addr("peer@localhost", addr, &mut atoms)
        .expect("dial the loopback peer");
    let (stream, _) = listener.accept().expect("the client connected");
    drop(stream);

    for _ in 0..50 {
        client.pump(&mut atoms).expect("the client pump");
        if !client.has_links() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        !client.has_links(),
        "the link is retired rather than left half-open"
    );
    assert!(
        client.last_error().is_some(),
        "the retirement stays observable as a diagnostic"
    );
    assert!(
        lost.borrow().is_empty(),
        "a link that never handshook was never a node down, so nothing is recorded"
    );
}

/// **A lost outbound link records its peer.** A link that completed its `'N'` handshake and
/// then loses the peer (the socket closes) is a node that went down, so its `node@host` name
/// lands in the runtime's shared [`LostPeers`](crate::dist::links::LostPeers) recorder — the
/// record [`crate::Runtime::service_lost_dist_peers`] turns into OTP's `noconnection`
/// semantics.
///
/// The question the retirement asks is **`was_established`**, never the live state: the pump
/// that discovers the peer's EOF is what moves the driver to `Closed`, so a retirement that
/// asked `is_established()` would answer `false` for every dying link and no `noconnection`
/// would ever fire for a real `erl` peer's `init:stop()`.
#[test]
fn a_retirement_that_had_established_records_its_peer_as_lost() {
    let (client, mut driver, mut stream, mut atoms) = loopback("cook", 7);
    let lost = crate::dist::links::new_lost_peers();
    client.use_lost_peers(std::rc::Rc::clone(&lost));
    drive(&client, &mut driver, &mut stream, &mut atoms);
    assert!(client.is_connected("peer@localhost"), "the handshake completed");
    assert!(lost.borrow().is_empty(), "nothing was lost while the peer was alive");

    // The peer's node goes away: its socket closes, exactly as a dying `erl`'s does.
    drop(stream);
    for _ in 0..200 {
        client.pump(&mut atoms).expect("the client pump");
        if !client.has_links() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        !client.has_links(),
        "the dead link is retired rather than left half-open"
    );
    assert_eq!(
        *lost.borrow(),
        vec!["peer@localhost".to_string()],
        "a link that had established is a peer whose node went down"
    );
}
