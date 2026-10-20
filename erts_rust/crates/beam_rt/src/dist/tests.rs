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
//! The distribution module's unit suite: the `epmd` address, this node's advertised
//! visibility, the peer-node binding and the loopback fixtures over a real socket pair
//! (`DistConnection`'s handshake/refusal/close behaviour and `DistNode`'s accept,
//! bind and retirement paths).
//!
//! A separate file so the fixtures do not grow the module root (`dist.rs`) or the
//! production files beside it — the crate's `port/tests.rs` / `dist/links/tests.rs`
//! pattern. It is a child of `crate::dist`, so `use super::*;` reaches every private
//! item of the module root, and the two items it needs from the responsibility
//! submodules are `pub(super)` for exactly that reason (`is_would_block`,
//! `epmd_addr_from`).
use std::io::{Read, Write};
use std::net::TcpStream;

use beam_common::BeamError;
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::epmd::EPMD_DEFAULT_ADDR;
use beam_dist::{
    preferred_flags, publish_flag, AcceptDecision, DistFlags, PeerVisibility, Status,
};
use beam_term::prelude::{make_atom, AtomTable};

use super::connection::is_would_block;
use super::node::epmd_addr_from;
use super::*;

/// The cookie every loopback fixture below digests with.
const COOKIE: &[u8] = b"dist-unit-test-cookie";

/// A connected loopback pair, each end configured exactly like an accepted link
/// (`TCP_NODELAY` + the [`DIST_READ_SLICE`] read timeout), so a
/// [`DistConnection`] built over either end behaves as it does on a real link.
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

/// Whatever the peer end has sent so far (`b""` on the read slice's timeout) —
/// the raw wire view the refusal tests assert on.
fn read_available(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = [0u8; 512];
    match stream.read(&mut buf) {
        Ok(n) => buf[..n].to_vec(),
        Err(err) if is_would_block(&err) => Vec::new(),
        Err(err) => panic!("reading the peer end failed: {err}"),
    }
}

/// A name no other test (or re-run) collides with on a shared `epmd`.
fn unique_node(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "beamrt_{tag}_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// `$ERL_EPMD_PORT` unset (or empty) means the well-known default: the mapping is
/// pinned on [`epmd_addr_from`], the pure core, because the process environment is
/// global and this crate's tests never mutate it.
#[test]
fn epmd_addr_defaults_to_the_well_known_loopback_address() {
    assert_eq!(epmd_addr_from(None), EPMD_DEFAULT_ADDR);
    assert_eq!(epmd_addr_from(Some("")), EPMD_DEFAULT_ADDR);
    assert_eq!(
        epmd_addr_from(Some("   ")),
        EPMD_DEFAULT_ADDR,
        "a blank value is an unset value"
    );
    assert_eq!(EPMD_DEFAULT_ADDR, "127.0.0.1:4369");
}

/// A bare number is a **port** on the local loopback (the `ERL_EPMD_PORT=4370`
/// form of a private `epmd`).
#[test]
fn epmd_addr_reads_a_bare_number_as_a_loopback_port() {
    assert_eq!(epmd_addr_from(Some("4370")), "127.0.0.1:4370");
    assert_eq!(epmd_addr_from(Some(" 5370 ")), "127.0.0.1:5370");
}

/// A value containing `:` is already a `host:port` pair and is used verbatim (the
/// form [`beam_dist::epmd::EpmdClient::connect`] takes).
#[test]
fn epmd_addr_uses_a_host_port_pair_verbatim() {
    assert_eq!(epmd_addr_from(Some("10.0.0.5:4370")), "10.0.0.5:4370");
    assert_eq!(
        epmd_addr_from(Some("epmd.internal.example:4370")),
        "epmd.internal.example:4370"
    );
}

/// The environment-reading wrapper agrees with the pure core: with
/// `ERL_EPMD_PORT` absent (the normal case) it is the default. Read-only — the
/// variable is never set by a test, so no other test in this binary can race.
#[test]
fn epmd_addr_reads_the_environment_when_the_variable_is_absent() {
    if std::env::var_os("ERL_EPMD_PORT").is_some() {
        eprintln!("SKIP: ERL_EPMD_PORT is set in this environment");
        return;
    }
    assert_eq!(epmd_addr(), EPMD_DEFAULT_ADDR);
}

/// A peer whose flags fail the mandatory v6 gate is refused with a framed
/// `"not_allowed"` status, and those bytes must reach the peer's socket **even
/// though** `ConnectionDriver::step` reports the refusal as an error: the driver
/// buffers the status *before* returning `Err`, so `pump` flushes the driver's
/// output on that path too.
#[test]
fn a_mandatory_flag_mismatch_still_writes_the_not_allowed_status() {
    let (server_stream, mut client_stream) = socket_pair();
    let peer_addr = server_stream.peer_addr().expect("the peer's address");

    // The peer sends a `'N'` name carrying **no** flags at all, so it misses
    // `DFLAG_DIST_MANDATORY` (the OTP-23+ gate) and is refused.
    let mut peer = ConnectionDriver::client_with_flags(
        b"peer@host.example",
        COOKIE,
        1,
        DistFlags::EMPTY,
    );
    peer.step().expect("the peer frames its `'N'` message");
    let name_frame = peer.take_output();
    assert!(!name_frame.is_empty(), "the `'N'` message is on the wire");
    client_stream
        .write_all(&name_frame)
        .expect("write the `'N'` message to the acceptor");

    let mut server = DistConnection::from_parts(
        ConnectionDriver::server(b"acceptor@host.example", COOKIE, 1),
        server_stream,
        peer_addr,
    );
    // A frame may straddle reads: pump until the driver refuses the peer.
    let mut refusal = None;
    for _ in 0..8 {
        match server.pump() {
            Ok(_) => continue,
            Err(err) => {
                refusal = Some(err);
                break;
            }
        }
    }
    let err = refusal.expect("the acceptor refuses a peer missing the mandatory flags");
    assert!(
        matches!(err, BeamError::BadArg),
        "the mandatory check's typed refusal: {err}"
    );
    assert!(!server.is_live(), "a refused link is in the terminal `Failed` state");
    assert!(
        !server.has_pending_output(),
        "the refusal frame was flushed rather than dropped with the link"
    );

    let received = read_available(&mut client_stream);
    assert!(
        !received.is_empty(),
        "the peer must receive the refusal status, not a silent close"
    );
    assert_eq!(
        received.len() - 2,
        usize::from(u16::from_be_bytes([received[0], received[1]])),
        "the status is one 2-byte-framed handshake message: {received:?}"
    );
    assert!(
        received.windows(11).any(|window| window == b"not_allowed"),
        "the refusal is the oracle's `not_allowed` status: {received:?}"
    );
}

/// The same flush rule for a **policy** refusal (`AcceptDecision::Refuse`, the
/// `nok`/`alive` statuses a real acceptor sends): the status is buffered before
/// `step` returns `Err`, so it must still be written.
#[test]
fn a_policy_refusal_still_writes_its_status() {
    let (server_stream, mut client_stream) = socket_pair();
    let peer_addr = server_stream.peer_addr().expect("the peer's address");

    let mut peer = ConnectionDriver::client(b"peer@host.example", COOKIE, 1);
    peer.step().expect("the peer frames its `'N'` message");
    let name_frame = peer.take_output();
    client_stream
        .write_all(&name_frame)
        .expect("write the `'N'` message to the acceptor");

    let mut server = DistConnection::from_parts(
        ConnectionDriver::server_with_decision(
            b"acceptor@host.example",
            COOKIE,
            1,
            preferred_flags(),
            Box::new(|_| AcceptDecision::Refuse(Status::nok())),
        ),
        server_stream,
        peer_addr,
    );
    let mut refusal = None;
    for _ in 0..8 {
        match server.pump() {
            Ok(_) => continue,
            Err(err) => {
                refusal = Some(err);
                break;
            }
        }
    }
    refusal.expect("the accept policy refuses this peer");
    assert!(!server.is_live(), "the refused link is terminal");
    assert!(!server.has_pending_output(), "the refusal frame was flushed");

    let received = read_available(&mut client_stream);
    assert!(
        received.len() >= 2 && received.windows(3).any(|window| window == b"nok"),
        "the peer receives the framed `nok` status: {received:?}"
    );
}

/// Both ends of a real OTP connection may advertise themselves hidden, so a
/// hidden peer is **recorded** ([`DistConnection::peer_visibility`]), never
/// refused: here a published peer and a deliberately hidden acceptor
/// ([`ConnectionDriver::server`], the emulator default) complete the handshake, each
/// classifying the other. The *accepting* half's own visibility comes from
/// [`advertised_flags`] — see
/// [`the_acceptor_publishes_the_node_unless_it_is_hidden`].
#[test]
fn a_hidden_and_a_published_peer_both_establish_and_are_recorded() {
    let (server_stream, client_stream) = socket_pair();
    let client_addr = client_stream.peer_addr().expect("the client's address");
    let server_addr = server_stream.peer_addr().expect("the server's address");

    let mut server = DistConnection::from_parts(
        ConnectionDriver::server(b"acceptor@host.example", COOKIE, 1),
        server_stream,
        client_addr,
    );
    // The peer publishes itself (`DFLAG_PUBLISHED`), the acceptor does not.
    let mut client = DistConnection::from_parts(
        ConnectionDriver::client_with_flags(
            b"peer@host.example",
            COOKIE,
            1,
            preferred_flags() | publish_flag(false, true),
        ),
        client_stream,
        server_addr,
    );
    assert_eq!(
        server.peer_visibility(),
        None,
        "nothing is classified before the handshake completes"
    );

    for _ in 0..10 {
        if client.is_established() && server.is_established() {
            break;
        }
        client.pump().expect("the client's handshake step");
        server.pump().expect("the acceptor's handshake step");
    }

    assert!(
        client.is_established() && server.is_established(),
        "a hidden peer is accepted like any other (client={:?}, server={:?})",
        client.state(),
        server.state()
    );
    assert_eq!(
        server.peer_visibility(),
        Some(PeerVisibility::Normal),
        "the peer set `DFLAG_PUBLISHED`"
    );
    assert_eq!(
        client.peer_visibility(),
        Some(PeerVisibility::Hidden),
        "an acceptor whose driver advertises `preferred_flags` (the emulator default) \
         omits `DFLAG_PUBLISHED`"
    );
    // The rest of the peer identity stays truthful.
    assert_eq!(server.peer_name().as_deref(), Some("peer@host.example"));
    assert_eq!(client.peer_name().as_deref(), Some("acceptor@host.example"));
    assert_eq!(server.peer_creation(), 1);
    assert_eq!(client.peer_creation(), 1);
}

/// A peer that dials `node`'s listener and completes the `'N'` handshake: the socket it speaks
/// over and the driver holding its side of the protocol, both still live.
///
/// The node accepts and drives the link exactly as it does in production, so the returned
/// pair can go on to make the peer **disappear** (drop the socket) and observe what the node
/// records — see [`an_accepted_link_that_dies_records_its_peer_as_lost`].
fn dial_and_establish(node: &mut DistNode) -> (TcpStream, ConnectionDriver) {
    let mut client = TcpStream::connect(("127.0.0.1", node.port()))
        .expect("connect the node's listener");
    client.set_nodelay(true).expect("TCP_NODELAY");
    client
        .set_read_timeout(Some(DIST_READ_SLICE))
        .expect("the link's read slice");
    // The accept, and (best-effort) the `epmd` registration.
    node.pump().expect("the accept");
    // The peer publishes itself, so the acceptor's classification of it is exercised
    // too (a hidden peer is equally acceptable — see the test above).
    let mut peer = ConnectionDriver::client_with_flags(
        b"peer@host.example",
        COOKIE,
        1,
        preferred_flags() | publish_flag(false, true),
    );
    for _ in 0..200 {
        node.pump().expect("the accepted link's pump");
        let mut buf = [0u8; 4096];
        match client.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => peer.feed(&buf[..n]),
            Err(err) if is_would_block(&err) => {}
            Err(err) => panic!("the peer's read: {err}"),
        }
        loop {
            match peer.step() {
                Ok(beam_dist::StepOutcome::WouldBlock) => break,
                Ok(_) => {}
                Err(err) => panic!("the peer's step: {err}"),
            }
        }
        if peer.has_output() {
            let bytes = peer.take_output();
            client.write_all(&bytes).expect("the peer's write");
            client.flush().expect("the peer's flush");
        }
        if matches!(peer.state(), DriverState::Established)
            && node
                .links()
                .borrow()
                .first()
                .is_some_and(DistConnection::is_established)
        {
            return (client, peer);
        }
    }
    panic!("the loopback 'N' handshake never completed");
}

/// The capability flags the **accepting** half actually put on the wire, as the
/// connecting peer reads them: a real [`DistNode`] and a peer driver run the `'N'`
/// handshake against each other over a loopback socket, and the peer's record of the
/// acceptor's `Challenge` flags (`ConnectionDriver::server_flags`) is returned.
///
/// This is the wire truth rather than the constructor's input: it is what a system
/// `erl` classifies with `publish_type/1` when it decides whether to list this node in
/// `nodes()`.
fn acceptor_flags_as_the_peer_read_them(node: &mut DistNode) -> DistFlags {
    let (_client, peer) = dial_and_establish(node);
    assert_eq!(
        node.links()
            .borrow()
            .first()
            .and_then(|link| link.peer_visibility()),
        Some(PeerVisibility::Normal),
        "the peer published ITSELF; our own flags are the separate question"
    );
    peer.server_flags()
}

/// **A lost accepted link records its peer.** [`DistNode::pump`] retires a link whose peer
/// hung up and the retirement records that peer's `node@host` name in the runtime's shared
/// [`LostPeers`](super::links::LostPeers) recorder — the record
/// [`crate::Runtime::service_lost_dist_peers`] turns into OTP's `noconnection` semantics.
///
/// The question the retirement asks is **`was_established`**, never the live state: the pump
/// that discovers the peer's EOF is what moves the driver to `Closed`, so a retirement that
/// asked `is_established()` would see `false` for exactly the link whose loss must be
/// recorded — and a real `erl` peer's `init:stop()` would deliver nothing at all.
#[test]
fn an_accepted_link_that_dies_records_its_peer_as_lost() {
    use super::links::{new_lost_peers, take_lost_peers};
    let name = unique_node("lostpeer");
    let mut node = DistNode::start(&name, COOKIE.to_vec()).expect("the listener binds");
    let lost: super::links::LostPeers = new_lost_peers();
    node.use_lost_peers(std::rc::Rc::clone(&lost));
    let (client, peer) = dial_and_establish(&mut node);
    assert!(
        node.links()
            .borrow()
            .first()
            .is_some_and(DistConnection::was_established),
        "the link completed its 'N' handshake"
    );
    assert!(take_lost_peers(&lost).is_empty(), "nothing was lost yet");

    // The peer's whole node goes away: its socket closes, exactly as a dying `erl`'s does.
    drop(client);
    drop(peer);
    for _ in 0..200 {
        node.pump().expect("the accepted link's pump");
        if node.links().borrow().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        node.links().borrow().is_empty(),
        "the dead link is retired rather than left half-open"
    );
    assert_eq!(
        take_lost_peers(&lost),
        vec!["peer@host.example".to_string()],
        "a link that had established is a peer whose node went down — the record the \
         peer-loss drain turns into `noconnection`"
    );
}

/// The complementary case: an accepted link retired **before** its `'N'` handshake completed
/// records **no** peer loss — no process can hold a relationship with a peer that never
/// became a node, and the reason stays the diagnostic
/// [`DistNode::last_link_error`] reports.
#[test]
fn an_accepted_link_that_never_handshook_records_no_lost_peer() {
    use super::links::{new_lost_peers, take_lost_peers};
    let name = unique_node("silentpeer");
    let mut node = DistNode::start(&name, COOKIE.to_vec()).expect("the listener binds");
    let lost: super::links::LostPeers = new_lost_peers();
    node.use_lost_peers(std::rc::Rc::clone(&lost));
    let client = TcpStream::connect(("127.0.0.1", node.port()))
        .expect("connect the node's listener");
    node.pump().expect("the accept");
    assert_eq!(node.links().borrow().len(), 1, "the link was accepted");
    assert!(
        !node.links().borrow()[0].was_established(),
        "the peer never sent an `'N'` frame"
    );
    drop(client);
    for _ in 0..200 {
        node.pump().expect("the accepted link's pump");
        if node.links().borrow().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(node.links().borrow().is_empty(), "the link is retired");
    assert!(
        node.last_link_error().is_some(),
        "the retirement stays observable as a diagnostic"
    );
    assert!(
        take_lost_peers(&lost).is_empty(),
        "a link that never handshook was never a node down, so nothing is recorded"
    );
}

/// **Contract A.5 — this node's visibility.** A published node ORs
/// `DFLAG_PUBLISHED` (`dist.h:30`) into the flags it advertises, exactly as
/// `dist_util.erl:152-160`'s `publish_flag/3` does for a node that was not started
/// `-hidden`. The bit is what makes a real peer list this runtime in `nodes()`:
/// without it every peer classifies it as *hidden* and only `nodes(hidden)`/
/// `nodes(connected)` show the link.
///
/// The assertion is on the peer's view of the acceptor's `Challenge`, i.e. the bytes
/// that actually crossed the socket, not on a locally computed set.
#[test]
fn the_acceptor_publishes_the_node_unless_it_is_hidden() {
    let published = DistFlags::from_bits(beam_dist::flags::DFLAG_PUBLISHED);
    let name = unique_node("published");
    let mut node = DistNode::start(&name, COOKIE.to_vec()).expect("the listener binds");
    assert!(!node.is_hidden(), "a node is published unless `-hidden` says otherwise");
    assert!(
        node.advertised_flags().contains(published),
        "the node's own flag set carries DFLAG_PUBLISHED: 0x{:x}",
        node.advertised_flags().bits()
    );
    let peer_view = acceptor_flags_as_the_peer_read_them(&mut node);
    assert!(
        peer_view.contains(published),
        "the peer read the acceptor's flags as 0x{:x}, which omits DFLAG_PUBLISHED \
         (0x{:x}) — such a node is hidden to every real peer",
        peer_view.bits(),
        beam_dist::flags::DFLAG_PUBLISHED
    );
    assert_eq!(
        peer_view.bits(),
        (preferred_flags() | publish_flag(false, true)).bits(),
        "the advertised set is the emulator default plus exactly DFLAG_PUBLISHED"
    );
    assert!(
        peer_view.contains(preferred_flags()),
        "every capability of the emulator default is still advertised"
    );
}

/// The `-hidden` half: a node started hidden advertises the emulator default
/// **unchanged** — no `DFLAG_PUBLISHED` — so a real peer records it as
/// [`PeerVisibility::Hidden`] and leaves it out of `nodes()` (`-hidden` is a
/// per-node choice, `dist_util.erl:152-160`).
#[test]
fn a_hidden_node_withholds_the_published_bit() {
    let name = unique_node("hidden");
    let mut node = DistNode::start(&name, COOKIE.to_vec()).expect("the listener binds");
    node.set_hidden(true);
    assert!(node.is_hidden(), "`set_hidden(true)` takes effect");
    assert!(
        !node
            .advertised_flags()
            .contains(DistFlags::from_bits(beam_dist::flags::DFLAG_PUBLISHED)),
        "a hidden node's own flag set omits DFLAG_PUBLISHED: 0x{:x}",
        node.advertised_flags().bits()
    );
    let peer_view = acceptor_flags_as_the_peer_read_them(&mut node);
    assert!(
        !peer_view.contains(DistFlags::from_bits(beam_dist::flags::DFLAG_PUBLISHED)),
        "the peer read the hidden acceptor's flags as 0x{:x}, which still sets \
         DFLAG_PUBLISHED (0x{:x})",
        peer_view.bits(),
        beam_dist::flags::DFLAG_PUBLISHED
    );
    assert_eq!(
        peer_view.bits(),
        preferred_flags().bits(),
        "a hidden node advertises the emulator default, unchanged"
    );
    // Back to published: the switch is a plain property, reversible before a pump.
    node.set_hidden(false);
    assert!(!node.is_hidden(), "`set_hidden(false)` publishes the node again");
}

/// An **established** accepted link is bound to its peer's node atom term, which is
/// what makes the routing table's accepted half an exact node match: the link learns
/// its peer only as `node@host` bytes, so the term has to be interned into the live
/// table once, and binding is idempotent thereafter.
#[test]
fn bind_peer_nodes_gives_every_established_accepted_link_its_peer_term() {
    let name = unique_node("bind");
    let mut node = DistNode::start(&name, COOKIE.to_vec()).expect("the listener binds");
    let mut client = TcpStream::connect(("127.0.0.1", node.port()))
        .expect("connect the node's listener");
    client.set_nodelay(true).expect("TCP_NODELAY");
    client
        .set_read_timeout(Some(DIST_READ_SLICE))
        .expect("the link's read slice");
    // The accept, and (best-effort) the `epmd` registration.
    node.pump().expect("the accept");

    let mut peer = ConnectionDriver::client(b"peer@host.example", COOKIE, 1);
    for _ in 0..200 {
        node.pump().expect("the accepted link's pump");
        let mut buf = [0u8; 4096];
        match client.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => peer.feed(&buf[..n]),
            Err(err) if is_would_block(&err) => {}
            Err(err) => panic!("the peer's read: {err}"),
        }
        loop {
            match peer.step() {
                Ok(beam_dist::StepOutcome::WouldBlock) => break,
                Ok(_) => {}
                Err(err) => panic!("the peer's step: {err}"),
            }
        }
        if peer.has_output() {
            let bytes = peer.take_output();
            client.write_all(&bytes).expect("the peer's write");
            client.flush().expect("the peer's flush");
        }
        let established = node
            .links()
            .borrow()
            .first()
            .is_some_and(|connection| connection.is_established());
        if established {
            break;
        }
    }

    let mut atoms = AtomTable::new();
    assert_eq!(
        node.links().borrow().first().and_then(|c| c.node_term()),
        None,
        "an accepted link is unbound until the scheduler pass binds it"
    );
    assert_eq!(
        node.bind_peer_nodes(&mut atoms).expect("the first bind"),
        1,
        "the one established accepted link is bound"
    );
    let expected = make_atom(atoms.intern("peer@host.example"));
    assert_eq!(
        node.links().borrow().first().and_then(|c| c.node_term()),
        Some(expected),
        "the bound term is the peer's own node atom, interned in the live table"
    );
    assert_eq!(
        node.bind_peer_nodes(&mut atoms).expect("the second bind"),
        0,
        "binding is idempotent: an already-bound link is skipped"
    );
}

/// `close()` is deterministic and idempotent: the first call shuts the socket
/// down (the peer reads EOF) and a second call — whose `shutdown(2)` answers
/// `ENOTCONN` — is still `Ok`, never an error.
#[test]
fn close_shuts_the_socket_down_and_is_idempotent() {
    let (server_stream, mut client_stream) = socket_pair();
    let peer_addr = server_stream.peer_addr().expect("the peer's address");
    let mut connection = DistConnection::from_parts(
        ConnectionDriver::server(b"acceptor@host.example", COOKIE, 1),
        server_stream,
        peer_addr,
    );
    connection.close().expect("the first close shuts the socket down");
    assert!(
        read_available(&mut client_stream).is_empty(),
        "the peer sees no data, only the closure"
    );
    let mut buf = [0u8; 16];
    assert_eq!(
        client_stream.read(&mut buf).expect("the peer's read after the shutdown"),
        0,
        "the peer reads a clean EOF, never a half-open socket"
    );
    connection
        .close()
        .expect("a second close is a no-op, not an `ENOTCONN` error");
}

/// A link the driver refuses on the wire is retired **and** its socket is shut
/// down: the peer reads EOF and the node records why the link went.
#[test]
fn retiring_a_broken_link_shuts_its_socket_down_so_the_peer_sees_eof() {
    let name = unique_node("retire");
    let mut node = DistNode::start(&name, COOKIE.to_vec()).expect("the listener binds");
    let mut client = TcpStream::connect(("127.0.0.1", node.port()))
        .expect("connect the node's listener");
    client
        .set_read_timeout(Some(DIST_READ_SLICE))
        .expect("the link's read slice");
    client.set_nodelay(true).expect("TCP_NODELAY");

    // The first pump accepts the connection and (best-effort) registers with
    // `epmd`; the link then waits for the peer's `'N'` message.
    node.pump().expect("the accept");
    node.pump().expect("the accepted link's first read");
    assert_eq!(node.connections().len(), 1, "the link is live");

    // A one-byte handshake body that is not a `'N'` name: the acceptor's
    // `Name::decode` fails, so the link is retired.
    client
        .write_all(&[0, 1, 0xff])
        .expect("write a malformed handshake frame");
    node.pump().expect("a broken link never fails the pump");
    assert!(
        node.connections().is_empty(),
        "the malformed link was retired: {:?}",
        node.last_link_error()
    );
    assert!(
        node.last_link_error().is_some(),
        "the retirement reason is recorded, never swallowed"
    );

    let mut buf = [0u8; 16];
    assert_eq!(
        client.read(&mut buf).expect("the peer's read after the retirement"),
        0,
        "the retired link's socket was shut down, so the peer reads EOF"
    );
}
