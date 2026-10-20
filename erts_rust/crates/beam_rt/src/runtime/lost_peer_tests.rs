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
//! The scheduler pass's **peer-loss** step — [`Runtime::service_lost_dist_peers`] — and the
//! `noconnection` semantics a distribution link's death owes the process table (contract A.5).
//!
//! The step has four decisions, and this suite pins each of them at the seam rather than
//! through a two-node round trip:
//!
//! 1. a recorded peer that **no** link names any more fires one
//!    [`ProcessTable::node_down`](beam_process::ProcessTable::node_down) — a remote monitor's
//!    `{'DOWN', Ref, process, Peer, noconnection}`
//!    (`lost_peer_remote_monitor_fires_one_down_with_noconnection`) and a trapping link
//!    partner's `{'EXIT', Peer, noconnection}`
//!    (`lost_peer_trapping_link_partner_receives_exit_with_noconnection`);
//! 2. a peer that **is** still linked fires nothing — the re-connect guard, with a link-less
//!    peer recorded in the same call as the control
//!    (`node_loss_of_a_still_linked_peer_fires_nothing_while_a_linkless_one_fires`);
//! 3. a peer whose only link is still **handshaking** fires nothing either, because the guard
//!    reads [`DistLinks::link_names`](crate::dist::DistLinks) — every handshake state, not only
//!    established links (`node_loss_under_a_link_still_handshaking_fires_nothing`);
//! 4. the recorder's fast path and its atom decline: an empty recorder is `Ok(0)` and stays
//!    empty (`lost_peer_step_with_an_empty_recorder_is_an_ok_zero_no_op`), and a name the live
//!    atom table cannot resolve is a decline that fabricates nothing
//!    (`lost_peer_step_declines_a_name_the_atom_table_cannot_resolve`).
//!
//! A sibling suite rather than more lines in `runtime.rs` (the `dist_connect_tests.rs` /
//! `port/tests.rs` pattern): the fixtures drive a real `'N'` handshake over a loopback socket
//! pair, with no `erl` and no `epmd`. The recorded peer is noted directly
//! ([`note_lost_peer`]) — the step's input is the recorder, and *who records* is the
//! retirement sites' business (`crate::dist::DistNode` / `crate::dist_client`).

use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};

use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::StepOutcome;
use beam_process::{MonitorKind, ProcessId};

use super::*;
use crate::dist::links::note_lost_peer;
use crate::dist::DIST_READ_SLICE;

/// A peer no link of this suite ever reaches.
const PEER: &str = "peer@host.example";

/// A second peer, used as the control in the still-linked case: nothing links it, so its
/// recorded loss must fire.
const OTHER: &str = "other@host.example";

/// A name nothing interned — the atom decline's input.
const UNBOUND: &str = "never-seen@host.example";

/// The cookie the loopback handshake below digests with (the `-setcookie` of
/// [`node_runtime`]).
const COOKIE: &[u8] = b"cook";

/// A **node** runtime — a bound listener plus a distribution client — configured exactly as
/// `runtime/dist_connect_tests.rs` and `runtime/scheduler/tests.rs` configure one. The
/// listener is published to `epmd` only on the node's first pump, which no test here drives,
/// so the suite needs neither `epmd` nor `erl`.
fn node_runtime() -> Runtime {
    let mut config = RuntimeConfig::default();
    config.emulator.node_name = Some("unitloss".to_string());
    config
        .init_args
        .record("setcookie", vec!["cook".to_string()]);
    Runtime::new(config).expect("a node runtime")
}

/// A `ProcessTable` whose `SignalAtoms` come from the **runtime's own** atom table, so the
/// node terms this suite builds and the atoms `node_down` writes into a message share one
/// namespace (a name looked up in the runtime's table is the index the table compares).
fn peer_table(rt: &mut Runtime) -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(rt.code_server_atoms_mut()))
}

/// The atom **term** for `name` in the runtime's one table, interning it if needed.
fn node_term(rt: &mut Runtime, name: &str) -> Eterm {
    make_atom(rt.code_server_atoms_mut().intern(name))
}

/// The atom term for a name the runtime's table already holds — `node_down`'s reason atom,
/// which the peer-loss step interns itself.
fn known_atom(rt: &mut Runtime, name: &str) -> Eterm {
    let atom = rt
        .code_server_atoms_mut()
        .lookup(name)
        .unwrap_or_else(|| panic!("the runtime's atom table holds `{name}`"));
    make_atom(atom)
}

/// An **external pid** term for a process on `node` — the shape a remote peer's pid arrives
/// as, built on the caller's `wire` heap so it outlives the assertions.
fn remote_pid(wire: &mut Heap, node: Eterm, number: u32) -> Eterm {
    beam_codec::term::make_external_pid(wire, node, number, 0, 1).expect("the wire heap has room")
}

/// Watches a remote pid on `peer` from `watcher`, returning the external pid term the monitor
/// names (on `wire`).
fn watch_remote(
    rt: &mut Runtime,
    table: &mut ProcessTable,
    wire: &mut Heap,
    watcher: ProcessId,
    peer: &str,
    number: u32,
) -> Eterm {
    let node = node_term(rt, peer);
    let target = remote_pid(wire, node, number);
    table
        .monitor_term(watcher, target, MonitorKind::Process)
        .expect("a remote monitor");
    target
}

/// The one queued payload of `recipient`, with the mailbox size asserted on the way.
fn sole_payload(table: &ProcessTable, recipient: ProcessId) -> Eterm {
    let mailbox = table
        .get(recipient)
        .expect("the recipient is live")
        .mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued").payload()
}

/// One bounded, non-blocking read of `stream`: the bytes that had arrived, or an empty vector
/// when the read would block. `runtime/dist_connect_tests.rs` carries its own copy for the
/// same reason every suite does: it is three lines of socket plumbing over that suite's peer.
fn read_once(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = [0u8; 8192];
    match stream.read(&mut buf) {
        Ok(n) => buf[..n].to_vec(),
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Vec::new(),
        Err(err) => panic!("peer read: {err}"),
    }
}

/// Feeds `bytes` to the peer's driver, steps it to quiescence and writes back what it produced
/// — one peer step of the `beam_dist` side of a loopback link.
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

/// A node runtime with an **established** outbound link to `peer` over a loopback socket pair,
/// returned with the peer's driver and its end of the socket.
///
/// The fixture `runtime/dist_connect_tests.rs::connected_node` drives the same handshake; it is
/// copied here because that helper is private to its module.
fn establish_outbound(rt: &mut Runtime, peer: &str) -> (ConnectionDriver, TcpStream) {
    let creation = rt.dist_node().expect("a configured node").creation();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let addr = listener.local_addr().expect("the loopback port");
    // `rt.code_server` (not the `code_server_atoms_mut` method): the two fields are
    // disjoint, so the client borrow and the atom table can be held at once.
    rt.dist_client
        .as_ref()
        .expect("a node has a client")
        .connect_addr(peer, addr, rt.code_server.atoms_mut())
        .expect("dial the loopback peer");
    let (mut stream, _) = listener.accept().expect("the client connected");
    stream
        .set_read_timeout(Some(DIST_READ_SLICE))
        .expect("a bounded read");
    let mut driver = ConnectionDriver::server(peer.as_bytes(), COOKIE, creation);
    for _ in 0..500 {
        rt.pump_dist_client().expect("the client pump");
        let bytes = read_once(&mut stream);
        step_peer(&mut driver, &mut stream, &bytes);
        if matches!(driver.state(), DriverState::Established) && rt.dist_is_connected(peer) {
            return (driver, stream);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("the loopback handshake never completed");
}

/// A node runtime with an outbound link to `peer` that has **not** completed its handshake —
/// the shape a re-connect leaves behind (`DistClient::connect_addr` replaces the established
/// link with a fresh one). The peer end is accepted and then simply held open, so nothing ever
/// answers the `'N'` frame.
fn dial_handshaking(rt: &mut Runtime, peer: &str) -> (TcpStream, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let addr = listener.local_addr().expect("the loopback port");
    // `rt.code_server` (not the `code_server_atoms_mut` method): the two fields are
    // disjoint, so the client borrow and the atom table can be held at once.
    rt.dist_client
        .as_ref()
        .expect("a node has a client")
        .connect_addr(peer, addr, rt.code_server.atoms_mut())
        .expect("dial the loopback peer");
    let (stream, _) = listener.accept().expect("the client connected");
    stream
        .set_read_timeout(Some(DIST_READ_SLICE))
        .expect("a bounded read");
    (stream, listener)
}

// -- the fired signals -------------------------------------------------------

/// A recorded peer no link names fires **one** `node_down`: the watcher's remote monitor
/// becomes `{'DOWN', Ref, process, PeerPid, noconnection}`, the fired record is gone, and a
/// blocked watcher is woken by the delivery. The step answers `1` and leaves the recorder
/// empty, so the next pass is the allocation-free no-op.
#[test]
fn lost_peer_remote_monitor_fires_one_down_with_noconnection() -> Result<()> {
    let mut rt = node_runtime();
    let mut table = peer_table(&mut rt);
    let watcher = table.spawn_default()?;
    let mut wire = Heap::new(64)?;
    let target = watch_remote(&mut rt, &mut table, &mut wire, watcher, PEER, 11);
    table
        .get_mut(watcher)
        .expect("the watcher is live")
        .set_state(ProcessState::Waiting);

    note_lost_peer(&rt.lost_dist_peers, PEER);
    assert_eq!(
        rt.service_lost_dist_peers(&mut table)?,
        1,
        "one recorded peer, one node down"
    );

    assert!(
        rt.lost_dist_peers.borrow().is_empty(),
        "the record was consumed by the drain, never left to fire twice"
    );
    assert!(
        table.remote_monitors(watcher).is_empty(),
        "the monitor that fired is discharged from the watcher's tree"
    );
    assert_eq!(
        table.drain_woken(),
        vec![watcher],
        "a blocked watcher is woken by the DOWN, not left parked"
    );

    let payload = sole_payload(&table, watcher);
    assert_eq!(terms::tuple_arity(payload)?, 5, "the DOWN is a 5-tuple");
    assert_eq!(terms::tuple_element(payload, 0)?, known_atom(&mut rt, "DOWN"));
    assert_eq!(
        terms::tuple_element(payload, 2)?,
        known_atom(&mut rt, "process")
    );
    assert_eq!(
        RemotePid::from_eterm(terms::tuple_element(payload, 3)?)?,
        RemotePid::from_eterm(target)?,
        "the body names the peer's own pid"
    );
    assert_eq!(
        terms::tuple_element(payload, 4)?,
        known_atom(&mut rt, "noconnection"),
        "the reason is `noconnection`, never a substituted atom"
    );

    assert_eq!(
        rt.service_lost_dist_peers(&mut table)?,
        0,
        "a drained recorder is the fast path"
    );
    Ok(())
}

/// A recorded peer no link names fires **one** `node_down` for a **link** too: a trapping
/// partner survives with `{'EXIT', PeerPid, noconnection}`, the message's sender is the node
/// atom itself (the C's `from_tag`), and the link record is gone.
#[test]
fn lost_peer_trapping_link_partner_receives_exit_with_noconnection() -> Result<()> {
    let mut rt = node_runtime();
    let mut table = peer_table(&mut rt);
    let partner = table.spawn_default()?;
    table
        .get_mut(partner)
        .expect("the partner is live")
        .set_trap_exit(true);
    let mut wire = Heap::new(64)?;
    let node = node_term(&mut rt, PEER);
    let target = remote_pid(&mut wire, node, 21);
    table.record_remote_link(partner, RemotePid::from_eterm(target)?)?;

    note_lost_peer(&rt.lost_dist_peers, PEER);
    assert_eq!(
        rt.service_lost_dist_peers(&mut table)?,
        1,
        "one recorded peer, one node down"
    );

    assert!(
        table.contains(partner),
        "a trapping partner survives the node's death"
    );
    assert!(
        table.remote_links(partner).is_empty(),
        "the link to the lost node fired and went"
    );
    let message = table
        .get(partner)
        .expect("the partner is live")
        .mailbox()
        .peek()
        .expect("one message is queued");
    assert_eq!(
        message.from(),
        node,
        "the message is sent by the node atom (`from_tag`)"
    );
    let payload = message.payload();
    assert_eq!(terms::tuple_arity(payload)?, 3, "the EXIT is a 3-tuple");
    assert_eq!(terms::tuple_element(payload, 0)?, known_atom(&mut rt, "EXIT"));
    assert_eq!(
        RemotePid::from_eterm(terms::tuple_element(payload, 1)?)?,
        RemotePid::from_eterm(target)?,
        "the body names the partner that went down with the node"
    );
    assert_eq!(
        terms::tuple_element(payload, 2)?,
        known_atom(&mut rt, "noconnection"),
        "a link's node-down reason is exactly `noconnection`"
    );
    Ok(())
}

// -- the guards --------------------------------------------------------------

/// The **re-connect guard**: of two peers recorded in the same call, the one an established
/// link still names fires nothing — its monitor stays armed and no message is queued — while
/// the one no link names fires. The control is what makes the guard a decision rather than a
/// blanket no-op.
#[test]
fn node_loss_of_a_still_linked_peer_fires_nothing_while_a_linkless_one_fires() -> Result<()> {
    let mut rt = node_runtime();
    let (peer_driver, peer_stream) = establish_outbound(&mut rt, PEER);
    assert!(rt.dist_is_connected(PEER), "the loopback link established");
    assert!(
        rt.dist_link_names()
            .expect("the client cell is readable")
            .iter()
            .any(|name| name == PEER),
        "the established link names the peer"
    );

    let mut table = peer_table(&mut rt);
    let watcher = table.spawn_default()?;
    let mut wire = Heap::new(64)?;
    let linked = watch_remote(&mut rt, &mut table, &mut wire, watcher, PEER, 31);
    let unlinked = watch_remote(&mut rt, &mut table, &mut wire, watcher, OTHER, 32);

    note_lost_peer(&rt.lost_dist_peers, PEER);
    note_lost_peer(&rt.lost_dist_peers, OTHER);
    assert_eq!(
        rt.service_lost_dist_peers(&mut table)?,
        1,
        "only the peer no link names is lost"
    );

    let monitors = table.remote_monitors(watcher);
    assert_eq!(
        monitors.len(),
        1,
        "the still-linked peer's monitor is untouched"
    );
    assert_eq!(
        monitors[0].remote,
        RemotePid::from_eterm(linked)?,
        "and it is the linked peer's monitor that survived"
    );

    let payload = sole_payload(&table, watcher);
    assert_eq!(
        RemotePid::from_eterm(terms::tuple_element(payload, 3)?)?,
        RemotePid::from_eterm(unlinked)?,
        "the one delivery is the link-less peer's DOWN"
    );
    assert!(
        rt.lost_dist_peers.borrow().is_empty(),
        "both records were consumed — a re-connect is a non-event, not a deferred loss"
    );
    assert_eq!(
        rt.service_lost_dist_peers(&mut table)?,
        0,
        "and nothing is pending for the next pass"
    );
    drop((peer_driver, peer_stream));
    Ok(())
}

/// A peer whose only link is **still handshaking** is not lost either: the guard reads
/// [`DistLinks::link_names`](crate::dist::DistLinks), which lists every link whatever its
/// handshake state — so the replacement a re-dial leaves behind keeps the peer alive even
/// though no link can carry a `DOP_*` yet.
#[test]
fn node_loss_under_a_link_still_handshaking_fires_nothing() -> Result<()> {
    let mut rt = node_runtime();
    let (_peer_stream, _listener) = dial_handshaking(&mut rt, PEER);
    assert!(
        !rt.dist_is_connected(PEER),
        "the link has not completed its handshake"
    );
    assert!(
        rt.dist_link_names()
            .expect("the client cell is readable")
            .iter()
            .any(|name| name == PEER),
        "a handshaking link still names the peer it is trying to reach"
    );

    let mut table = peer_table(&mut rt);
    let watcher = table.spawn_default()?;
    let mut wire = Heap::new(64)?;
    watch_remote(&mut rt, &mut table, &mut wire, watcher, PEER, 41);

    note_lost_peer(&rt.lost_dist_peers, PEER);
    assert_eq!(
        rt.service_lost_dist_peers(&mut table)?,
        0,
        "a peer a link still names — even one still handshaking — is not lost"
    );
    assert_eq!(
        table.remote_monitors(watcher).len(),
        1,
        "the monitor is still armed"
    );
    assert!(
        table
            .get(watcher)
            .expect("the watcher is live")
            .mailbox()
            .peek()
            .is_none(),
        "nothing was delivered"
    );
    Ok(())
}

// -- the fast path and the decline -------------------------------------------

/// Nothing recorded is `Ok(0)` and stays nothing recorded — the shape every non-distributed run
/// and every runtime whose peers are all alive takes, and the one the pass pays for on each
/// round (one borrow and one emptiness test, no allocation).
#[test]
fn lost_peer_step_with_an_empty_recorder_is_an_ok_zero_no_op() -> Result<()> {
    let mut rt = node_runtime();
    let mut table = peer_table(&mut rt);
    assert!(rt.lost_dist_peers.borrow().is_empty(), "a fresh runtime");
    assert_eq!(rt.service_lost_dist_peers(&mut table)?, 0);
    assert_eq!(
        rt.service_lost_dist_peers(&mut table)?,
        0,
        "the no-op repeats forever without growing anything"
    );
    assert!(
        rt.lost_dist_peers.borrow().is_empty(),
        "the recorder is still empty"
    );

    // A runtime that is not a node at all: no listener, no client, so there is nothing that
    // could have been lost and the step keeps answering `0`.
    let mut plain = Runtime::with_defaults().expect("the default runtime");
    assert!(plain.dist_node().is_none() && plain.dist_client().is_none());
    let mut plain_table = ProcessTable::new(SignalAtoms::from_table(plain.code_server_atoms_mut()));
    assert_eq!(plain.service_lost_dist_peers(&mut plain_table)?, 0);
    assert!(plain.lost_dist_peers.borrow().is_empty());
    Ok(())
}

/// A recorded name the live atom table cannot resolve is a **decline**: no atom is fabricated,
/// nothing is delivered and nothing panics — the step answers `0` and the record is consumed.
#[test]
fn lost_peer_step_declines_a_name_the_atom_table_cannot_resolve() -> Result<()> {
    let mut rt = node_runtime();
    let mut table = peer_table(&mut rt);
    assert!(
        rt.code_server_atoms_mut().lookup(UNBOUND).is_none(),
        "the fixture's name is interned by nothing"
    );

    note_lost_peer(&rt.lost_dist_peers, UNBOUND);
    assert_eq!(
        rt.service_lost_dist_peers(&mut table)?,
        0,
        "an unresolvable name names no node to bring down"
    );
    assert!(
        rt.code_server_atoms_mut().lookup(UNBOUND).is_none(),
        "the decline interns nothing — never a fabricated atom"
    );
    assert!(
        rt.lost_dist_peers.borrow().is_empty(),
        "and the record was consumed rather than retried for ever"
    );
    Ok(())
}
