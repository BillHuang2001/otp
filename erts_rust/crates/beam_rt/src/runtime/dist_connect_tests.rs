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
//! The **connect-on-demand** half of the three outbound distribution send entries
//! ([`Runtime::dist_send_message_pid`]/[`Runtime::dist_send_reg_send`]/
//! [`Runtime::dist_send_alias`], contract A.5).
//!
//! ERTS sets a distribution entry up for a node a message addresses, so a send to a peer this
//! runtime has not dialled yet **delivers** instead of being refused (`dist.c`'s
//! `erts_get_dist_entry`). The runtime's [`Runtime::dist_ensure_link`] is that step, and these
//! tests pin its three binding properties:
//!
//! * a runtime that is **not** a node has no client to dial and keeps its exact typed refusals
//!   (the differential corpus pins that byte for byte);
//! * a peer an **established** link already reaches is never dialled again — the live link
//!   carries the frame and keeps its connection id;
//! * a peer that cannot be reached still answers the established-link refusal, after exactly
//!   one bounded attempt, and never mints a link.
//!
//! A sibling suite rather than more lines in `runtime.rs` (the `port/tests.rs` /
//! `runtime/scheduler/tests.rs` pattern): the fixture below drives a real `'N'` handshake over
//! a loopback socket pair, with no `erl` and no `epmd`.
use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::time::Duration;

use beam_alloc::Heap;
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::StepOutcome;
use beam_term::prelude::make_small;

use super::*;
use crate::config::RuntimeConfig;
use crate::dist::DIST_READ_SLICE;

/// A `-noshell` runtime: no `-name`/`-sname`, so it is `nonode@nohost` and has no client.
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// A **node** runtime (a bound listener plus a distribution client), configured exactly as the
/// scheduler suite configures one.
fn node_runtime() -> Runtime {
    let mut config = RuntimeConfig::default();
    config.emulator.node_name = Some("unitcli".to_string());
    config
        .init_args
        .record("setcookie", vec!["cook".to_string()]);
    Runtime::new(config).expect("a node runtime")
}

/// An **external pid** on `node` (a heap-owned 4-word thing, so the term must not outlive the
/// returned heap).
fn external_pid(rt: &mut Runtime, node: &str, number: u32) -> (Heap, Eterm) {
    let mut heap = Heap::new(64).expect("a heap for the destination term");
    let node = make_atom(rt.code_server_atoms_mut().intern(node));
    let pid = beam_codec::term::make_external_pid(&mut heap, node, number, 0, 1)
        .expect("an external pid");
    (heap, pid)
}

/// One bounded, non-blocking read of `stream`: the bytes that had arrived, or an empty vector
/// when the read would block (`DIST_READ_SLICE`). The fixture `runtime/scheduler/tests.rs` and
/// `dist/links/tests.rs` each carry their own copy for the same reason: it is two lines of
/// socket plumbing, and every suite's peer is a different driver.
fn read_once(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = [0u8; 8192];
    match stream.read(&mut buf) {
        Ok(n) => buf[..n].to_vec(),
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Vec::new(),
        Err(err) => panic!("peer read: {err}"),
    }
}

/// Feeds `bytes` to the peer's driver, steps it to quiescence and writes back whatever it
/// produced — one peer step of the `beam_dist` side of a loopback link.
fn step_peer(driver: &mut ConnectionDriver, stream: &mut TcpStream, bytes: &[u8]) {
    use std::io::Write;

    if !bytes.is_empty() {
        driver.feed(bytes);
    }
    loop {
        match driver.step() {
            Ok(StepOutcome::WouldBlock) => break,
            Ok(_) => {}
            Err(err) => panic!("peer step: {err}"),
        }
    }
    if driver.has_output() {
        let out = driver.take_output();
        stream.write_all(&out).expect("peer write");
        stream.flush().expect("peer flush");
    }
}

/// A node runtime with an **established** outbound link to `peer@localhost` over a loopback
/// socket, returned with the peer's driver and its end of the socket.
fn connected_node(peer: &str) -> (Runtime, ConnectionDriver, TcpStream) {
    let mut rt = node_runtime();
    let creation = rt.dist_node().expect("a configured node").creation();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let addr = listener.local_addr().expect("the loopback port");
    // `rt.code_server` (not the `code_server_atoms_mut` method): the two fields are disjoint,
    // so the client borrow and the atom table can be held at once.
    rt.dist_client
        .as_ref()
        .expect("a node has a client")
        .connect_addr(peer, addr, rt.code_server.atoms_mut())
        .expect("dial the loopback peer");
    let (mut stream, _) = listener.accept().expect("the client connected");
    stream
        .set_read_timeout(Some(DIST_READ_SLICE))
        .expect("a bounded read");
    let mut driver = ConnectionDriver::server(peer.as_bytes(), b"cook", creation);
    for _ in 0..500 {
        rt.pump_dist_client()
            .expect("the client pump never fails a pass");
        let bytes = read_once(&mut stream);
        step_peer(&mut driver, &mut stream, &bytes);
        if matches!(driver.state(), DriverState::Established) && rt.dist_is_connected(peer) {
            return (rt, driver, stream);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the loopback 'N' handshake never completed");
}

/// **A runtime that is not a node keeps its exact refusals.** With no `-name`/`-sname` there is
/// no distribution client, so [`Runtime::dist_ensure_link`] is a no-op: each of the three send
/// entries answers the same `distribution: this runtime is not a distributed node
/// (no -name/-sname)` it always has — never a dial attempt, and never a different error.
#[test]
fn a_non_distributed_runtime_refuses_every_outbound_send_without_dialling() {
    let mut rt = runtime();
    assert!(
        rt.node_name().is_none(),
        "a -noshell runtime is `nonode@nohost` (the premise)"
    );
    assert!(
        rt.dist_client().is_none(),
        "no client, so nothing can be dialled"
    );
    let from = rt.self_pid();
    let (_pid_heap, remote_pid) = external_pid(&mut rt, "peer@host", 7);
    let (_ref_heap, remote_ref) = {
        let mut heap = Heap::new(64).expect("a heap for the alias term");
        let node = make_atom(rt.code_server_atoms_mut().intern("peer@host"));
        let reference = beam_codec::term::make_external_ref(&mut heap, node, &[5], 1)
            .expect("an external reference");
        (heap, reference)
    };
    let message = make_small(42);
    let outcomes = [
        (
            "reg_send",
            rt.dist_send_reg_send("peer@host", "collector", from, message),
        ),
        (
            "message_pid",
            rt.dist_send_message_pid(remote_pid, from, message),
        ),
        ("alias", rt.dist_send_alias(remote_ref, from, message)),
    ];
    for (entry, result) in outcomes {
        let error = result.expect_err(entry);
        assert!(
            error
                .to_string()
                .contains("this runtime is not a distributed node"),
            "dist_send_{entry} keeps its typed refusal for a runtime with no client: {error}"
        );
    }
}

/// **An established link carries the frame, and is never dialled again.** The peer answers our
/// loopback handshake, so [`Runtime::dist_is_connected`] is already true when the send runs:
/// `dist_ensure_link` must skip the dial (a re-connect would replace the live link with a new
/// connection, i.e. mint a different `connection_id`) and the frame must reach the wire on that
/// one link.
#[test]
fn an_established_link_carries_the_frame_and_is_not_dialled_again() {
    const PEER: &str = "peer@localhost";
    let (mut rt, mut driver, mut stream) = connected_node(PEER);
    let id_before = rt
        .dist_connection_id(PEER)
        .expect("an established link has a connection id");
    let from = rt.self_pid();
    let (_heap, remote) = external_pid(&mut rt, PEER, 7);
    let mut message_heap = Heap::new(64).expect("a heap for the message");
    let hello = make_atom(rt.code_server_atoms_mut().intern("hello"));
    let message = message_heap
        .tuple(&[hello, make_small(7)])
        .expect("the message tuple");
    rt.dist_send_message_pid(remote, from, message)
        .expect("an established link carries the message without dialling anything");
    assert_eq!(
        rt.dist_connection_id(PEER),
        Some(id_before),
        "the send re-used the live link: a re-connect would have replaced it (a new \
         connection id, and briefly no established link at all)"
    );
    assert_eq!(
        rt.dist_connected_nodes(),
        vec![PEER.to_string()],
        "the send opened no second link"
    );
    // The frame is on the wire after one client pump — and it is the `DOP_SEND(_SENDER)` the
    // peer decodes, so the message really travelled on that link.
    rt.pump_dist_client()
        .expect("the client pump never fails a pass");
    let bytes = read_once(&mut stream);
    assert!(
        !bytes.is_empty(),
        "the message reaches the wire on the established link"
    );
    step_peer(&mut driver, &mut stream, &bytes);
    assert!(
        matches!(driver.state(), DriverState::Established),
        "the peer's link survives the frame"
    );
}

/// **The two decisions `dist_ensure_link` makes before it dials.** `epmd` registers a node under
/// its **short** name whatever it was started with, and this runtime's `epmd` lookup queries the
/// local `epmd` — so a peer on this host is dialled by the short name that `epmd` knows it by,
/// while a peer on another host is left verbatim (dialling its short name would key the link to
/// `short@<this host>`, a node nobody addressed). And a peer naming this runtime's **own** node
/// is never dialled at all.
#[test]
fn the_dial_name_is_the_epmd_key_of_a_local_peer_and_the_own_node_is_never_dialled() {
    let rt = node_runtime();
    let own = rt
        .node_name()
        .expect("a configured node knows its name")
        .to_string();
    let (short, host) = own
        .split_once('@')
        .expect("a node name is `name@host` by construction");
    assert_eq!(
        rt.dist_dial_name(&own),
        short,
        "a peer on this host is dialled by the short name `epmd` registers"
    );
    assert_eq!(
        rt.dist_dial_name(&format!("peer@{host}")),
        "peer",
        "another node on this host: the same `epmd` key"
    );
    assert_eq!(
        rt.dist_dial_name("peer@elsewhere.example"),
        "peer@elsewhere.example",
        "a peer on another host keeps its full name: its short name would key the link to \
         `peer@<this host>`"
    );
    assert_eq!(
        rt.dist_dial_name("peer"),
        "peer",
        "a bare short name is dialled verbatim"
    );
    assert!(
        rt.dist_peer_is_local(&own) && rt.dist_peer_is_local(short),
        "both the full and the short spelling of this runtime's own node are recognised"
    );
    assert!(
        !rt.dist_peer_is_local(&format!("peer@{host}")),
        "a peer on this host is not this node"
    );
    // A runtime that is not a node has no name to match, so nothing is ever "its own node".
    let plain = runtime();
    assert!(
        !plain.dist_peer_is_local("nonode@nohost"),
        "with no node configured there is no own-node name to skip"
    );
}

/// **A peer the connector cannot reach is still the established-link refusal, after one bounded
/// attempt.** The dial happens (that is the new behaviour), its failure is not raised as the
/// send's error and no link is minted, so the caller sees exactly the typed
/// `distribution: no established connection to node <peer>` — never a silent drop.
#[test]
fn an_unreachable_peer_answers_the_established_link_refusal_after_one_attempt() {
    const PEER: &str = "no-such-node-4c1f@localhost";
    let mut rt = node_runtime();
    assert!(
        rt.dist_client().is_some(),
        "the premise: this runtime IS a node, so the send does attempt a connection"
    );
    let from = rt.self_pid();
    let (_heap, remote) = external_pid(&mut rt, PEER, 7);
    let error = rt
        .dist_send_message_pid(remote, from, make_small(1))
        .expect_err("a node no epmd knows cannot be reached");
    assert!(
        error
            .to_string()
            .contains("no established connection to node no-such-node-4c1f@localhost"),
        "the send reports the missing link, not the connector's own failure: {error}"
    );
    assert!(
        rt.dist_connected_nodes().is_empty(),
        "a failed attempt mints no link"
    );
}
