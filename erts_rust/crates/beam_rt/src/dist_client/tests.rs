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
use beam_codec::term::make_external_pid;
use beam_common::BeamError;
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::flags::DFLAG_PUBLISHED;
use beam_dist::{
    preferred_flags, publish_flag, Delivery, DistFlags, StepOutcome, UnsupportedSink,
};
use beam_process::{DistSignal, MonitorKind, ProcessId, RefAllocator, RemotePid};
use beam_term::prelude::{make_atom as atom_term, make_small, AtomTable as Table};
use beam_term::AtomTable;

use crate::dist::DIST_READ_SLICE;

use super::handshake::is_would_block;
use super::lookup::{lookup_dist_port, lookup_dist_port_at};
use super::signals::unlink_id_term;

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
