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

//! Live interop: a real OTP `erl` node dials a Rust node and the crate's
//! **blocking** server handshake accepts it — and then **classifies the inbound
//! signal families the peer initiates**.
//!
//! `tests/live_accept.rs` already proves the crate's *accepting* half works, but
//! it drives it exclusively through the **non-blocking**
//! [`beam_dist::driver::ConnectionDriver`] state machine. The blocking entry
//! points — the free [`beam_dist::connection::accept_server`] and
//! [`beam_dist::connection::Connection::server`] — were only pinned hermetically
//! (`tests/connection.rs`, two in-process endpoints), never against a real OTP
//! peer. This test closes that gap:
//!
//! 1. [`blocking_accept_server_completes_the_n_handshake_with_a_real_erl_peer`]
//!    accepts a real peer with the free blocking `accept_server` and asserts the
//!    returned [`beam_dist::Handshake`] carries the peer's **full `node@host`**
//!    name and the accepting side's own advertised capability set.
//! 2. [`blocking_server_classifies_the_signal_families_a_real_erl_peer_initiates`]
//!    establishes the link with the blocking `Connection::server`, hands the peer
//!    this node's own external pid, and then proves the crate decodes **and
//!    classifies** the three `DOP_*` families a real peer initiates when it
//!    addresses that pid:
//!    * `Pid ! {hi, Seq}`          → [`beam_dist::Delivery::Send`] (`DOP_SEND` /
//!      `DOP_SEND_SENDER`, `dist.c:2529-2542`),
//!    * `erlang:link(Pid)`         → [`beam_dist::Delivery::Link`]
//!      (`{DOP_LINK, From, To}`, `dist.c:2224-2232` / `erts_dsig_send_link`,
//!      `dist.c:1453`),
//!    * `erlang:monitor(process, Pid)` → [`beam_dist::Delivery::Monitor`]
//!      (`{DOP_MONITOR_P, Watcher, Watched, Ref}`, `dist.c:2341-2346` /
//!      `erts_dsig_send_monitor`, `dist.c:1527`).
//!
//! ## The wire requirement this test pins
//!
//! As in `tests/live_accept.rs`, the Rust node's advertised name **must** be the
//! full `node@host`: OTP's connecting side
//! (`lib/kernel/src/dist_util.erl:recv_challenge_new/2`) matches the name in the
//! server's `'N'` challenge against the node it dialled with
//! `list_to_existing_atom/1`, and `send_name/4`/`send_challenge/2` always put
//! `atom_to_binary(Node)` (i.e. `node@host`) on the wire. A bare short name is
//! aborted by the peer.
//!
//! ## Why the pid the peer is handed matters
//!
//! The peer can only address this node if it holds a pid whose **node** field
//! matches the node-table entry the handshake installed
//! (`erlang:setnode(Node, Port, {Flags, Creation})`,
//! `dist_util.erl:do_setnode/1`; the lookup is `erts_find_dist_entry/1`,
//! `erts/emulator/beam/erl_node_tables.c:373`). This node therefore builds an
//! external pid (`beam_codec::term::make_external_pid`) whose node is the full
//! `node@host` it advertised and whose creation is the one `epmd` assigned its
//! registration, and sends it to the peer's registered process in the trailing
//! term of a `DOP_REG_SEND` (`{DOP_REG_SEND, From, Cookie, ToName} -- Message`,
//! `dist.c:2468`). That is exactly the `tests/live_accept.rs` pattern, for the
//! same reason.
//!
//! Every wait is bounded and the test skips loudly (never fakes a pass) when no
//! usable `erl` is on `PATH`, when `epmd` is unreachable, or when the peer never
//! dials the listener within the accept timeout. A family that simply does not
//! arrive is a **hard failure** with the collected evidence.

mod common;

use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_dist::epmd::{
    Alive2Req, EpmdClient, EPMD_DIST_HIGH, EPMD_DIST_LOW, EPMD_NODE_NORMAL, EPMD_PROTO_TCP,
};
use beam_dist::{accept_server, preferred_flags, Connection, Delivery, PeerVisibility, TcpTransport};
use beam_term::prelude::{make_atom, make_small, Eterm};
use beam_term::AtomTable;

use common::{erl_hostname, free_port, run_id, ErlangNode, MESSAGE_TIMEOUT};

/// The shared cookie for every link this test builds.
const COOKIE: &str = "rustdistacceptblockingcookie";

/// The name the peer registers a process under, and the name this side addresses
/// its `DOP_REG_SEND` to (an atom on both sides).
const PEER_NAME: &str = "blkpeer";

/// The tag of the message the peer sends to our pid.
const SIGNAL_TAG: &str = "hi";

/// The pid number/serial this node uses for the process it names in outbound
/// `DOP_*` control messages. Any stable pair works — nothing on the wire ties it
/// to a real local process — but it must be fixed so the pid in an inbound
/// signal can be compared byte-for-byte with the one that was sent.
const SELF_PID_NUMBER: u32 = 4242;
const SELF_PID_SERIAL: u32 = 7;

/// Bounds the wait for the peer's inbound TCP connection.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
/// The read timeout used while the blocking handshake runs.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The socket read timeout once the link is established: it bounds every blocking
/// `recv`, so the classification loop can re-check its own deadline.
const READ_SLICE: Duration = Duration::from_millis(250);
/// Bounds the wait for the three inbound signal families.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Accepts one connection within `timeout` (the listener is non-blocking).
fn accept_within(listener: &TcpListener, timeout: Duration) -> Option<TcpStream> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((sock, _)) => return Some(sock),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("accept failed: {err:?}"),
        }
    }
    None
}

/// Binds a distribution listener and publishes `short_name` to `epmd`.
///
/// The listener binds `0.0.0.0` (not loopback): the peer resolves the node name's
/// host part to a real interface, so a loopback-only listener is never dialled.
/// The returned [`EpmdClient`] must outlive the link — dropping it de-registers
/// the node.
///
/// Unlike `tests/live_accept.rs` (which panics on an `epmd` failure), an
/// unreachable `epmd` is reported as a **loud skip**: it is an environment
/// precondition, not a property of the crate under test.
///
/// The **node creation `epmd` assigned** is returned alongside it: it is what this
/// node must advertise in its `'N'` challenge *and* what its own outbound pids
/// must carry, so the pid's node-table lookup on the peer matches the connection
/// the handshake installed.
fn publish(short_name: &str) -> Option<(TcpListener, u16, EpmdClient<TcpTransport>, u32)> {
    let listener = TcpListener::bind("0.0.0.0:0").expect("bind the distribution listener");
    listener.set_nonblocking(true).expect("non-blocking listener");
    let port = listener.local_addr().expect("listener address").port();

    let mut epmd = match EpmdClient::connect("127.0.0.1:4369") {
        Ok(client) => client,
        Err(err) => {
            eprintln!("SKIP: epmd is unreachable at 127.0.0.1:4369: {err:?}");
            return None;
        }
    };
    let resp = match epmd.publish(&Alive2Req {
        port,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: EPMD_DIST_HIGH,
        low_vsn: EPMD_DIST_LOW,
        name: short_name.as_bytes().to_vec(),
        extra: vec![],
    }) {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("SKIP: epmd refused the ALIVE2_REQ for `{short_name}`: {err:?}");
            return None;
        }
    };
    if !resp.is_accepted() {
        eprintln!("SKIP: epmd did not accept the registration of `{short_name}` (got {resp:?})");
        return None;
    }
    Some((listener, port, epmd, resp.creation()))
}

/// The ETF body image of `term` under `atoms` (the atom *names* are written, so
/// two images are byte-identical iff the terms are).
fn term_image(term: Eterm, atoms: &AtomTable) -> Option<Vec<u8>> {
    beam_codec::encode_term_body(term, atoms).ok()
}

/// Reports whether `term` is a pid, without `unsafe`: a pid is the only term the
/// ETF encoder writes behind a `NEW_PID_EXT` (`88`) tag, so its body image's
/// first byte identifies it.
fn is_pid(term: Eterm, atoms: &AtomTable) -> bool {
    term_image(term, atoms)
        .and_then(|bytes| bytes.first().copied())
        .map(|tag| tag == beam_codec::NEW_PID_EXT)
        .unwrap_or(false)
}

/// The ETF body image of the `{SIGNAL_TAG, seq}` tuple, built on a scratch
/// heap/atom table so it can be compared with a term decoded into another table.
fn signal_image(seq: i64) -> Vec<u8> {
    let mut heap = Heap::new(4096).expect("scratch heap");
    let mut atoms = AtomTable::new();
    let term = heap
        .tuple(&[make_atom(atoms.intern(SIGNAL_TAG)), make_small(seq)])
        .expect("the signal tuple fits a scratch heap");
    term_image(term, &atoms).expect("the signal tuple encodes")
}

/// Reports whether every signal family this test requires has been classified.
fn has_all_families(deliveries: &[Delivery]) -> bool {
    let (mut send, mut link, mut monitor) = (false, false, false);
    for delivery in deliveries {
        match delivery {
            Delivery::Send { .. } => send = true,
            Delivery::Link { .. } => link = true,
            Delivery::Monitor { .. } => monitor = true,
            _ => {}
        }
    }
    send && link && monitor
}

/// A peer that only establishes the distribution link: it dials `target` and
/// reports whether `net_kernel:connect_node/1` answered `true` (it needs only the
/// distribution link, not a message round trip — `net_adm:ping/1` would block on
/// a `$gen_call` round trip).
fn spawn_connect_only_peer(peer_short: &str, target: &str) -> Option<ErlangNode> {
    let eval = format!(
        "R = net_kernel:connect_node('{target}'),\n\
         io:format(\"CONN=~p~n\", [R]),\n\
         timer:sleep(20000),\n\
         io:format(\"PEER_DONE~n\", []),\n\
         halt()."
    );
    let port = free_port();
    ErlangNode::start(peer_short, COOKIE, port, &eval).ok()
}

/// Starts a `-sname` peer that first registers a `PEER_NAME` process and then
/// dials `target`.
///
/// The registered process, on receiving the `{Pid, Seq}` trailing term of our
/// `DOP_REG_SEND`, performs exactly the three actions this test classifies,
/// printing a marker for each so the peer's own stdout is on record:
/// `Pid ! {hi, Seq}`, `link(Pid)` and `monitor(process, Pid)`.
fn spawn_signal_peer(peer_short: &str, target: &str) -> Option<ErlangNode> {
    let eval = format!(
        "register({PEER_NAME}, spawn(fun Loop() ->\n\
             receive\n\
                 {{Pid, Seq}} when is_pid(Pid) ->\n\
                     io:format(\"PEER_GOT_PID ~p~n\", [Pid]),\n\
                     Pid ! {{{SIGNAL_TAG}, Seq}},\n\
                     io:format(\"PEER_SENT {{{SIGNAL_TAG},~p}} TO_PID ~p~n\", [Seq, is_pid(Pid)]),\n\
                     link(Pid),\n\
                     io:format(\"PEER_LINKED ~p~n\", [Pid]),\n\
                     Ref = monitor(process, Pid),\n\
                     io:format(\"PEER_MONITORED ~p REF ~p~n\", [Pid, Ref])\n\
             end\n\
         end)),\n\
         R = net_kernel:connect_node('{target}'),\n\
         io:format(\"CONN=~p~n\", [R]),\n\
         timer:sleep(30000),\n\
         io:format(\"PEER_DONE~n\", []),\n\
         halt()."
    );
    // Pin a distinct listen port so a concurrent test cannot collide on it.
    let port = free_port();
    ErlangNode::start(peer_short, COOKIE, port, &eval).ok()
}

/// The blocking **free function** [`accept_server`]: a real `erl` node dials the
/// Rust listener and the crate completes the OTP-23 `'N'` handshake on the
/// accepting side without the non-blocking driver.
///
/// Ground truth for the assertions: `lib/kernel/src/dist_util.erl`
/// (`send_name/4`, `send_challenge/2`, `recv_challenge_ack/2`) and
/// `erts/emulator/beam/dist.c` (`dist_util.erl:do_setnode/1` installs the node
/// table entry from the advertised `{Flags, Creation}`).
#[test]
fn blocking_accept_server_completes_the_n_handshake_with_a_real_erl_peer() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable `erl` on PATH");
        return;
    };
    let id = run_id();
    let short = format!("rustblkh{id}");
    let advertised = format!("{short}@{host}");
    let peer_short = format!("blkh{id}");
    let peer_full = format!("{peer_short}@{host}");

    // 1. The Rust node's distribution listener + its `epmd` registration.
    let Some((listener, _port, epmd, creation)) = publish(&short) else {
        return;
    };

    // 2. A real `erl` peer dials the Rust node.
    let Some(peer) = spawn_connect_only_peer(&peer_short, &advertised) else {
        eprintln!("SKIP: could not spawn an `erl` peer");
        drop(epmd);
        return;
    };

    let Some(sock) = accept_within(&listener, ACCEPT_TIMEOUT) else {
        eprintln!("SKIP: the peer never dialled the Rust listener");
        drop(epmd);
        return;
    };
    sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).expect("handshake read timeout");
    sock.set_nodelay(true).expect("TCP_NODELAY");
    let mut transport = TcpTransport::from_stream(sock);

    // 3. [BLOCKING] The free `accept_server` — no driver, no poll loop.
    let handshake =
        match accept_server(&mut transport, advertised.as_bytes(), COOKIE.as_bytes(), creation) {
            Ok(handshake) => handshake,
            Err(err) => panic!(
                "the blocking `accept_server` handshake failed against a real `erl` peer \
                 (advertised name `{advertised}`, creation {creation:#010x}): {err:?}; \
                 peer stderr: {}",
                peer.stderr_text(),
            ),
        };

    // [LIVE] The peer's `'N'` name carries its FULL `node@host` and its creation.
    assert_eq!(
        handshake.peer_name,
        peer_full.as_bytes(),
        "the blocking accept must report the real peer's full `node@host` name",
    );
    // [LIVE] The accepting side advertised its own default capability set (the
    // `accept_server` entry point offers exactly `preferred_flags()` — a *hidden*
    // node, `dist.h:118-129`).
    assert_eq!(
        handshake.server_flags,
        preferred_flags(),
        "the blocking `accept_server` must advertise exactly `preferred_flags()`",
    );
    // [LIVE] A real default `-sname` node publishes itself
    // (`net_kernel:publish_on_node/1` is true by default), so its `DFLAG_PUBLISHED`
    // bit is set and this side classifies it `normal`
    // (`dist_util.erl:publish_type/1`).
    assert_eq!(
        handshake.peer_visibility,
        PeerVisibility::Normal,
        "a real default `-sname` node advertises DFLAG_PUBLISHED, so it is a normal node",
    );
    // [LIVE] No `Complement` frame trails the OTP-23 `'N'` handshake.
    assert!(
        handshake.buffered.is_none(),
        "the OTP-23 `'N'` handshake exchanges no complement; buffered: {:?}",
        handshake.buffered,
    );

    // 4. [LIVE] The peer's own kernel considers the link established.
    let conn = peer
        .wait_for(|line| line.starts_with("CONN="), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            format!(
                "the peer never reported its connect result; stderr: {}",
                peer.stderr_text()
            )
        });
    assert_eq!(
        conn, "CONN=true",
        "a real `erl` node must complete the blocking `accept_server` handshake",
    );

    eprintln!(
        "LIVE: blocking `accept_server` accepted `{peer_full}` (creation \
         {:#010x} advertised by this node, peer creation {:#010x}); peer said \
         `{conn}`",
        creation,
        handshake.peer_creation,
    );

    drop(epmd);
}

/// The blocking [`Connection::server`] link: this node hands the real peer its own
/// external pid and then proves the crate **decodes and classifies** the three
/// inbound signal families the peer initiates against it.
///
/// Each assertion is tagged `[LIVE]` (observed against a real OTP node during this
/// run) or `[BYTE]` (a value pinned from the OTP sources).
#[test]
fn blocking_server_classifies_the_signal_families_a_real_erl_peer_initiates() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable `erl` on PATH");
        return;
    };
    let id = run_id();
    let short = format!("rustblks{id}");
    let advertised = format!("{short}@{host}");
    let peer_short = format!("blks{id}");
    let peer_full = format!("{peer_short}@{host}");
    let seq: i64 = 1;

    // 1. The Rust node's distribution listener + its `epmd` registration (whose
    //    assigned creation this node must both advertise and use in its pids).
    let Some((listener, _port, epmd, creation)) = publish(&short) else {
        return;
    };

    // 2. A real `erl` peer dials the Rust node.
    let Some(peer) = spawn_signal_peer(&peer_short, &advertised) else {
        eprintln!("SKIP: could not spawn an `erl` peer");
        drop(epmd);
        return;
    };

    let Some(sock) = accept_within(&listener, ACCEPT_TIMEOUT) else {
        eprintln!("SKIP: the peer never dialled the Rust listener");
        drop(epmd);
        return;
    };

    // The blocking handshake wants a generous read timeout; the post-handshake
    // classification loop wants a small one. `TcpStream::try_clone` duplicates the
    // descriptor and `SO_RCVTIMEO` is a socket-level option, so retuning it on the
    // clone retunes the very socket the transport reads from.
    sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).expect("handshake read timeout");
    sock.set_nodelay(true).expect("TCP_NODELAY");
    let control = sock.try_clone().expect("duplicate the accepted socket");

    // 3. [BLOCKING] The blocking `Connection::server` (a thin wrapper over
    //    `accept_server`, `connection.rs`).
    let mut conn = match Connection::server(
        TcpTransport::from_stream(sock),
        advertised.as_bytes(),
        COOKIE.as_bytes(),
        creation,
    ) {
        Ok(conn) => conn,
        Err(err) => panic!(
            "the blocking `Connection::server` handshake failed against a real `erl` peer \
             (advertised name `{advertised}`, creation {creation:#010x}): {err:?}; \
             peer stderr: {}",
            peer.stderr_text(),
        ),
    };

    // [LIVE] The blocking handshake reports the peer's full `node@host`.
    assert_eq!(
        conn.peer_name(),
        peer_full.as_bytes(),
        "the blocking `Connection::server` must report the real peer's full `node@host` name",
    );
    assert_eq!(
        conn.peer_visibility(),
        PeerVisibility::Normal,
        "a real default `-sname` node is published, hence a normal (visible) node",
    );
    // The `control` duplicate refers to the very same socket the transport reads
    // from, so retuning it bounds every post-handshake `recv`.
    control
        .set_read_timeout(Some(READ_SLICE))
        .expect("retune the established link's read timeout");

    // 4. Hand the peer this node's own external pid, in the trailing term of a
    //    `DOP_REG_SEND` to its registered name — the only way the peer can learn a
    //    pid that routes back here.
    let mut out_heap = Heap::new(1 << 16).expect("outbound term heap");
    let mut out_atoms = AtomTable::new();
    let node_term = make_atom(out_atoms.intern(&advertised));
    let our_pid = beam_codec::term::make_external_pid(
        &mut out_heap,
        node_term,
        SELF_PID_NUMBER,
        SELF_PID_SERIAL,
        creation,
    )
    .expect("the external pid fits the outbound heap");
    let cookie_term = make_atom(out_atoms.intern(""));
    let name_term = make_atom(out_atoms.intern(PEER_NAME));
    let payload = out_heap
        .tuple(&[our_pid, make_small(seq)])
        .expect("the `{OurPid, Seq}` tuple fits the outbound heap");
    conn.send_reg_send(our_pid, cookie_term, name_term, payload, &out_atoms)
        .expect("the `DOP_REG_SEND` must encode onto the established link");
    let our_pid_image = term_image(our_pid, &out_atoms).expect("our pid encodes");

    // 5. [LIVE] Collect the inbound deliveries until every family is in (or the
    //    deadline passes). Every `recv` is bounded by the socket read timeout, so
    //    an absent family can never hang the test.
    let mut heap = Heap::new(1 << 20).expect("inbound term heap");
    let mut atoms = AtomTable::new();
    let mut deliveries: Vec<Delivery> = Vec::new();

    let deadline = Instant::now() + SIGNAL_TIMEOUT;
    while Instant::now() < deadline && !has_all_families(&deliveries) {
        match conn.recv_delivery(&mut heap, &mut atoms) {
            Ok(delivery) => {
                eprintln!("LIVE inbound: {delivery:?}");
                deliveries.push(delivery);
            }
            Err(BeamError::Io(err))
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Nothing in flight right now: re-check the deadline.
            }
            // A peer that halts right after sending closes the socket; whatever
            // arrived is already collected, so stop and let the assertions judge.
            Err(BeamError::BadArg) => break,
            Err(err) => panic!(
                "an inbound `recv_delivery` failed on an established link: {err:?}; \
                 peer stderr: {}",
                peer.stderr_text(),
            ),
        }
    }

    // 6. A family that did not arrive is a hard failure, with the evidence.
    let missing = || -> ! {
        panic!(
            "the real peer was handed pid {} (creation {creation:#010x}) and did not \
             produce all three inbound signal families within {SIGNAL_TIMEOUT:?}. \
             Classified deliveries: {deliveries:?}. Peer stdout tail: {:?}; peer stderr: {}",
            String::from_utf8_lossy(&our_pid_image),
            peer.wait_for(|line| line.starts_with("PEER_"), Duration::from_millis(500)),
            peer.stderr_text(),
        )
    };

    let send = deliveries.iter().find_map(|delivery| match delivery {
        Delivery::Send { from, to, message, .. } => Some((*from, *to, *message)),
        _ => None,
    });
    let link = deliveries.iter().find_map(|delivery| match delivery {
        Delivery::Link { from, to } => Some((*from, *to)),
        _ => None,
    });
    let monitor = deliveries.iter().find_map(|delivery| match delivery {
        Delivery::Monitor { watcher, watched, reference } => Some((*watcher, *watched, *reference)),
        _ => None,
    });
    if send.is_none() || link.is_none() || monitor.is_none() {
        missing();
    }
    let (send_from, send_to, send_msg) = send.expect("checked above");
    let (link_from, link_to) = link.expect("checked above");
    let (mon_watcher, mon_watched, _mon_ref) = monitor.expect("checked above");

    // [LIVE] `Pid ! {hi, Seq}` — `{DOP_SEND, From, To} -- Message` (`dist.c:2529`,
    // or `{DOP_SEND_SENDER, From, To}` when the sender pid was negotiated,
    // `dist.c:2494`). `Delivery::Send` covers both forms.
    assert_eq!(
        term_image(send_to, &atoms).as_deref(),
        Some(our_pid_image.as_slice()),
        "the delivered message must be addressed to the exact pid this node handed the peer",
    );
    assert_eq!(
        term_image(send_msg, &atoms).as_deref(),
        Some(signal_image(seq).as_slice()),
        "the delivered message must be the `{{{SIGNAL_TAG},{seq}}}` tuple the peer sent",
    );
    // `from` carries the sender pid for `DOP_SEND_SENDER` (the peer negotiated the
    // default flag set, which includes `DFLAG_SEND_SENDER`) and the empty atom
    // `''` for the plain `DOP_SEND` form (`dist.c:2535-2542`).
    assert!(
        is_pid(send_from, &atoms) || send_from == make_atom(atoms.intern("")),
        "`Delivery::Send::from` must be the sender pid or the empty atom, got image {:?}",
        term_image(send_from, &atoms).and_then(|b| b.first().copied()),
    );

    // [LIVE] `erlang:link(Pid)` — `{DOP_LINK, From, To}` (`dist.c:2224-2232`):
    // the peer's pid links to the pid this node handed it.
    assert!(
        is_pid(link_from, &atoms),
        "`Delivery::Link::from` must be the real peer's pid",
    );
    assert_eq!(
        term_image(link_to, &atoms).as_deref(),
        Some(our_pid_image.as_slice()),
        "`Delivery::Link::to` must be the exact pid this node handed the peer",
    );

    // [LIVE] `erlang:monitor(process, Pid)` — `{DOP_MONITOR_P, Watcher, Watched,
    // Ref}` (`dist.c:2341-2346`): the peer watches the pid this node handed it.
    assert!(
        is_pid(mon_watcher, &atoms),
        "`Delivery::Monitor::watcher` must be the real peer's pid",
    );
    assert_eq!(
        term_image(mon_watched, &atoms).as_deref(),
        Some(our_pid_image.as_slice()),
        "`Delivery::Monitor::watched` must be the exact pid this node handed the peer",
    );

    // [BYTE] Each family was classified into the typed variant this crate
    // documents — the same `Delivery` an `InboundSink` would be handed (the
    // blocking counterpart of `ConnectionDriver::next_delivery`).
    assert!(
        deliveries.iter().any(|d| matches!(d, Delivery::Send { .. })),
        "the collected deliveries must include a classified `Delivery::Send`: {deliveries:?}",
    );
    assert!(
        deliveries.iter().any(|d| matches!(d, Delivery::Link { .. })),
        "the collected deliveries must include a classified `Delivery::Link`: {deliveries:?}",
    );
    assert!(
        deliveries.iter().any(|d| matches!(d, Delivery::Monitor { .. })),
        "the collected deliveries must include a classified `Delivery::Monitor`: {deliveries:?}",
    );

    // 7. [LIVE] The peer's own stdout is the on-record evidence that it actually
    //    performed all three actions against the pid it was handed.
    let got_pid = peer
        .wait_for(|line| line.starts_with("PEER_GOT_PID"), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            format!(
                "the peer never reported receiving the pid; stderr: {}",
                peer.stderr_text()
            )
        });
    let sent = peer.wait_for(|line| line.starts_with("PEER_SENT"), MESSAGE_TIMEOUT);
    let linked = peer
        .wait_for(|line| line.starts_with("PEER_LINKED"), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            format!(
                "the peer never reported `link(Pid)`; stderr: {}",
                peer.stderr_text()
            )
        });
    let monitored = peer
        .wait_for(|line| line.starts_with("PEER_MONITORED"), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            format!(
                "the peer never reported `monitor(process, Pid)`; stderr: {}",
                peer.stderr_text()
            )
        });
    assert!(
        sent.is_some(),
        "the peer must have reported `Pid ! {{{SIGNAL_TAG},{seq}}}`; stderr: {}",
        peer.stderr_text(),
    );
    // `~p` renders a remote pid as `<NodeRef.Num.Serial>`, so the node's *text*
    // name is not in the line — but the pid's number/serial are the ones this node
    // built, which is the decisive evidence the peer decoded our pid.
    assert!(
        got_pid.contains(&format!(".{SELF_PID_NUMBER}.{SELF_PID_SERIAL}>")),
        "the peer must have received the pid this node built (number {SELF_PID_NUMBER}, \
         serial {SELF_PID_SERIAL}); peer line: {got_pid:?}",
    );

    eprintln!(
        "LIVE: blocking `Connection::server` accepted `{peer_full}` and classified all three \
         signal families the peer initiated against our pid (creation {creation:#010x}): \
         `Delivery::Send` (message `{{{SIGNAL_TAG},{seq}}}`), `Delivery::Link`, \
         `Delivery::Monitor`; peer said: {got_pid:?} / {sent:?} / {linked:?} / {monitored:?}",
    );

    drop(epmd);
}
