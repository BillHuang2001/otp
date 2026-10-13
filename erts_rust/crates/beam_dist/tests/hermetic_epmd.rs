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

//! A **hermetic `epmd`**: `beam_dist`'s port-mapper *client* driven against a
//! test-local `epmd` stub, so the whole `ALIVE2`/`PORT2`/`NAMES`/`STOP` request
//! path is exercised **without a host-run `epmd`**.
//!
//! # Why a stub, and why it is still oracle-validated
//!
//! `beam_dist::epmd` is a client only — the daemon is a separate ERTS program
//! (`erts/epmd/src/epmd_srv.c`), not a library this workspace can link — so the
//! crate's live coverage (`tests/epmd.rs`) needs a real `epmd` on the host and
//! skips when there is none. [`EpmdStub`] closes that gap: it binds an ephemeral
//! loopback port, speaks the daemon's wire protocol, and is queried through
//! [`EpmdClient::connect`] and the address-taking
//! [`lookup_at`](beam_dist::epmd::lookup_at) /
//! [`lookup_port_at`](beam_dist::epmd::lookup_port_at), which reach any `epmd`
//! address ([`beam_dist::epmd::lookup`]/[`lookup_port`] are the
//! [`EPMD_DEFAULT_ADDR`](beam_dist::epmd::EPMD_DEFAULT_ADDR)-pinned wrappers).
//!
//! The stub is an **independent** implementation of the wire, not a second call
//! into the crate under test: requests are hand-decoded and replies hand-encoded
//! from `epmd_srv.c`, so the round-trip in
//! [`the_crate_client_publishes_and_looks_up_a_node_against_a_hermetic_stub`]
//! cannot be self-consistent by construction. It is still validated against a real
//! ERTS client:
//! [`the_stub_accepts_a_real_erlang_node_and_the_crate_resolves_it`] points a
//! **real `erl` node** at the stub with `-epmd_port <stub_port>` (the daemon-port
//! override of `erl_epmd:get_epmd_port/0`, `erl_epmd.erl:373-380`) and then has
//! the crate's client resolve that registration. It skips honestly — and only —
//! when `erl` is absent.
//!
//! # Fidelity rules the stub mirrors (`erts/epmd/src/epmd_srv.c`)
//!
//! * A request is **2-byte big-endian length-framed**; a reply is written
//!   **unframed** (self-delimiting by tag) — `:1547` `reply()`.
//! * `ALIVE2_RESP` is `'v'` (32-bit creation) for a `highvsn >= 6` requester, `'y'`
//!   (16-bit) otherwise (`:850-859`); the result byte is `0` for an accepted
//!   registration and `1` for a refused one (a refusal still carries creation 99).
//! * A **non-`ALIVE`** connection is closed right after serving its single request
//!   (`:727-730`), so every lookup needs a fresh [`EpmdClient`].
//! * A **successful `ALIVE2_REQ`** sets the daemon's `keep` flag (`:870`), and the
//!   node is **de-registered when that socket closes** (`:641-645`
//!   `node_unreg_sock`) — what `tests/epmd.rs` observes live, too.
//! * `PORT2_RESP` carries **no creation field** (`:904-936`), so a lookup proves
//!   the port and advertised fields, never the creation.
//! * `STOP_REQ` answers the literal 7 bytes `"STOPPED"`/`"NOEXIST"` (`:1068-1110`);
//!   the stub is loopback-only, i.e. the daemon's `local_peer` requirement holds.
//! * An unknown tag is answered with **nothing** and a closed connection (`:1113`).
//! * `KILL_REQ` is **not** modelled: a real daemon answers `"OK"` *and
//!   terminates*, which a stub that keeps running cannot honour honestly.
//!   `DUMP_REQ` is omitted as unnecessary (`tests/epmd.rs` covers it), and the stub
//!   does not close a stopped node's `ALIVE` socket (`:1100`) — the effect the
//!   client asserts (the registration is gone) is identical.
//!
//! Every wait is bounded and `Drop` shuts the stub's threads down, so no
//! assertion in this file can hang a CI run.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_dist::epmd::{
    lookup_at, lookup_port_at, Alive2Req, Alive2Resp, EpmdClient, Port2Resp, StopResp,
    EPMD_ALIVE2_REQ, EPMD_ALIVE2_RESP, EPMD_ALIVE2_X_RESP, EPMD_DIST_HIGH, EPMD_DIST_LOW,
    EPMD_NAMES_REQ, EPMD_NODE_NORMAL, EPMD_PORT2_REQ, EPMD_PORT2_RESP, EPMD_PROTO_TCP,
    EPMD_STOP_REQ,
};
use beam_dist::TcpTransport;

/// The socket read timeout of every stub connection: it bounds every read *and*
/// is the granularity at which a worker notices the stub being dropped.
const STUB_READ_TIMEOUT: Duration = Duration::from_millis(250);
/// The retry interval of the stub's `wait_for_*` helpers.
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// How long to wait for the spawned node's `READY` line.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a positive observation (the node's `ALIVE2_REQ`
/// arriving, or the crate's client resolving it).
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a registration to vanish after its `ALIVE` socket closes.
const DEREGISTER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the spawned node stays up, so there is a live registration to query.
const NODE_LIFETIME_MS: u32 = 30_000;
/// The spawned node's cookie (`erl` requires one even when nothing distributes).
const NODE_COOKIE: &str = "hermetic_epmd_cookie";
/// The first creation the stub hands out. A real `epmd` never assigns creation
/// `0` (`net_kernel.erl:2645` even skips creations below `4`).
const FIRST_CREATION: u32 = 0x0100_0000;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The crate's `epmd` client publishes a node on the stub and resolves it from a
/// **second** connection — with no `erl` and no host `epmd` involved.
#[test]
fn the_crate_client_publishes_and_looks_up_a_node_against_a_hermetic_stub() {
    let stub = EpmdStub::start().expect("bind a hermetic epmd stub");
    let addr = stub.addr_string();
    let name = b"hermetic_node".to_vec();
    const NODE_PORT: u16 = 47_110;

    // ALIVE2_REQ: the crate's client registers the node.
    let req = alive2_req(&name, NODE_PORT);
    let (publisher, resp) = publish(&addr, &req).expect("the stub answers ALIVE2_REQ");
    assert!(resp.is_accepted(), "the stub refused the registration: {resp:?}");
    // A v6 requester (`EPMD_DIST_HIGH == 6`) is answered with the extended
    // 32-bit-creation form, exactly as `epmd_srv.c:850` selects the reply width.
    assert!(
        matches!(resp, Alive2Resp::Extended { ok: true, .. }),
        "a v6 ALIVE2_REQ must be answered 'v': {resp:?}"
    );
    let creation = resp.creation();
    assert_ne!(creation, 0, "a real epmd never assigns creation 0: {resp:?}");

    // The stub is an independent decoder of the very bytes the client wrote:
    // every field of the recorded registration is asserted, not just the tag.
    let record = stub.record(&name).expect("the stub recorded the node");
    assert_eq!(record.port, NODE_PORT, "the registered port");
    assert_eq!(record.node_type, EPMD_NODE_NORMAL, "the registered node type");
    assert_eq!(record.protocol, EPMD_PROTO_TCP, "the registered protocol");
    assert_eq!(record.high_vsn, EPMD_DIST_HIGH, "the advertised dist high");
    assert_eq!(record.low_vsn, EPMD_DIST_LOW, "the advertised dist low");
    assert!(record.extra.is_empty(), "the extra field is empty");
    assert_eq!(record.creation, creation, "the recorded creation is the answered one");

    // A second ALIVE2_REQ for the same live name is refused.
    let (_dup, dup) = publish(&addr, &req).expect("the stub answers the duplicate ALIVE2_REQ");
    assert!(dup.is_rejected(), "a name already taken must be refused: {dup:?}");
    assert!(dup.is_alive_conflict(), "the refusal is the alive-name conflict: {dup:?}");
    assert!(stub.record(&name).is_some(), "the refused duplicate must leave the live node alone");

    // PORT2_REQ: a fresh connection resolves the registration.
    let mut lookup = EpmdClient::connect(&addr).expect("connect a lookup client");
    let resolved = lookup.port_please(&name).expect("the stub answers PORT2_REQ");
    assert!(resolved.is_registered(), "the node must resolve: {resolved:?}");
    match &resolved {
        Port2Resp::Ok { port, node_type, protocol, high_vsn, low_vsn, name: reported, extra } => {
            assert_eq!(*port, NODE_PORT, "the resolved port");
            assert_eq!(*node_type, EPMD_NODE_NORMAL, "the resolved node type");
            assert_eq!(*protocol, EPMD_PROTO_TCP, "the resolved protocol");
            assert_eq!(*high_vsn, EPMD_DIST_HIGH, "the resolved dist high");
            assert_eq!(*low_vsn, EPMD_DIST_LOW, "the resolved dist low");
            assert_eq!(reported, &name, "the resolved name");
            assert!(extra.is_empty(), "the resolved extra field is empty");
        }
        Port2Resp::Error => panic!("PORT2_RESP reported the node as unregistered: {resolved:?}"),
    }
    // NOTE: `PORT2_RESP` has no creation field (`epmd_srv.c:904-936`), so the
    // creation can only be asserted on the `ALIVE2_RESP` side above.
    drop(lookup);

    // An unknown name is reported as not registered.
    let mut unknown = EpmdClient::connect(&addr).expect("connect another lookup client");
    let missing = unknown.port_please(b"no_such_hermetic_node").expect("the stub answers PORT2_REQ");
    assert!(missing.is_not_registered(), "an unknown name must not resolve: {missing:?}");
    assert!(matches!(missing, Port2Resp::Error), "the 'w'+1 reply is the Error variant");
    // `port_please` surfaces that reply as a *value*; the `Io(NotFound)` translation
    // lives in `lookup_port_at` (the address-taking form of `lookup_port`), which
    // this test drives below.
    drop(unknown);

    // Closing the ALIVE socket de-registers the node.
    drop(publisher);
    assert!(
        stub.wait_until_gone(&name, DEREGISTER_TIMEOUT),
        "the stub must de-register the node when its ALIVE socket closes"
    );
    let mut after = EpmdClient::connect(&addr).expect("connect a client after the drop");
    let gone = after.port_please(&name).expect("the stub answers PORT2_REQ");
    assert!(gone.is_not_registered(), "a de-registered node must not resolve: {gone:?}");
}

/// `lookup_at`/`lookup_port_at` aim the one-shot `PORT2_REQ` lookup at an
/// **arbitrary** `epmd`, so the `Io(NotFound)` translation is reachable against a
/// daemon that is not on [`EPMD_DEFAULT_ADDR`] — here the hermetic stub.
#[test]
fn address_taking_lookups_resolve_a_node_on_a_non_default_epmd() {
    let stub = EpmdStub::start().expect("bind a hermetic epmd stub");
    let addr = stub.addr_string();
    let name = b"at_node".to_vec();
    const NODE_PORT: u16 = 47_120;

    // Publish the node, keeping the ALIVE socket alive for the lookups below
    // (`epmd` de-registers a node when its ALIVE socket closes).
    let (_publisher, resp) =
        publish(&addr, &alive2_req(&name, NODE_PORT)).expect("the stub answers ALIVE2_REQ");
    assert!(resp.is_accepted(), "the stub refused the registration: {resp:?}");

    // `lookup_at` resolves the name against the stub's own (non-default) address,
    // and the stub is an independent decoder of the bytes the client wrote.
    let resolved = lookup_at(&addr, &name).expect("the stub answers PORT2_REQ");
    assert!(resolved.is_registered(), "the node must resolve: {resolved:?}");
    match &resolved {
        Port2Resp::Ok { port, name: reported, .. } => {
            assert_eq!(*port, NODE_PORT, "the resolved port");
            assert_eq!(reported, &name, "the resolved name");
        }
        Port2Resp::Error => panic!("the node must resolve: {resolved:?}"),
    }

    // `lookup_port_at` reduces that very reply to its port on a fresh connection.
    assert_eq!(
        lookup_port_at(&addr, &name).expect("the stub answers PORT2_REQ"),
        NODE_PORT,
        "lookup_port_at must return the registered port"
    );

    // The `Io(NotFound)` translation (never a silent port `0`) is now reachable
    // against a non-default `epmd`: an unknown name is a typed error.
    match lookup_port_at(&addr, b"no_such_node_at") {
        Err(BeamError::Io(err)) => assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "an unknown name must be Io(NotFound): {err}"
        ),
        other => panic!("an unknown name must be Io(NotFound), not {other:?}"),
    }
    assert!(lookup_at(&addr, b"no_such_node_at").expect("the stub answers PORT2_REQ").is_not_registered());
}
/// `NAMES_REQ`/`STOP_REQ` against the stub, hermetically.
#[test]
fn the_stub_lists_and_unregisters_nodes_hermetically() {
    let stub = EpmdStub::start().expect("bind a hermetic epmd stub");
    let addr = stub.addr_string();

    // Two registrations, each on its own `ALIVE` connection (kept alive below).
    let (_keep_a, resp_a) = publish(&addr, &alive2_req(b"hermetic_a", 40_001))
        .expect("the stub answers ALIVE2_REQ for hermetic_a");
    let (_keep_b, resp_b) = publish(&addr, &alive2_req(b"hermetic_b", 40_002))
        .expect("the stub answers ALIVE2_REQ for hermetic_b");
    assert!(
        resp_a.is_accepted() && resp_b.is_accepted(),
        "both nodes register: {resp_a:?} {resp_b:?}"
    );

    // NAMES_RESP: the stub's own port plus one line per registered node (the
    // `name <name> at port <port>\n` line `epmd_srv.c:963-966` emits).
    let mut lister = EpmdClient::connect(&addr).expect("connect a names client");
    let names = lister.names().expect("the stub answers NAMES_REQ");
    assert_eq!(names.epmd_port, u32::from(stub.addr().port()), "NAMES_RESP reports the daemon's port");
    assert_eq!(
        names.names,
        vec!["hermetic_a".to_string(), "hermetic_b".to_string()],
        "NAMES_RESP lists every registered node"
    );
    drop(lister);

    // STOP_REQ for a registered node: "STOPPED" and the registration is gone.
    let mut stopper = EpmdClient::connect(&addr).expect("connect a stop client");
    let stopped = stopper.stop(b"hermetic_a").expect("the stub answers STOP_REQ");
    assert!(stopped.is_stopped(), "a registered node must answer STOPPED: {stopped:?}");
    assert!(stub.record(b"hermetic_a").is_none(), "STOP_REQ unregisters the node");
    drop(stopper);

    // STOP_REQ for an unknown node: "NOEXIST", never a fabricated success.
    let mut stopper = EpmdClient::connect(&addr).expect("connect a second stop client");
    let absent = stopper.stop(b"hermetic_missing").expect("the stub answers STOP_REQ");
    assert!(matches!(absent, StopResp::NoExist), "an unknown name answers NOEXIST: {absent:?}");
    drop(stopper);

    // The listed node set shrank accordingly.
    let mut lister = EpmdClient::connect(&addr).expect("connect a second names client");
    let remaining = lister.names().expect("the stub answers NAMES_REQ");
    assert_eq!(remaining.names, vec!["hermetic_b".to_string()], "hermetic_a is gone");
}

/// The stub closes a connection whose request it does not understand, without
/// inventing a reply.
#[test]
fn the_stub_closes_an_unknown_request_without_replying() {
    let stub = EpmdStub::start().expect("bind a hermetic epmd stub");
    let mut stream = TcpStream::connect(stub.addr()).expect("connect to the stub");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("bound the read");

    // A well-formed 2-byte frame carrying a tag no request uses.
    stream.write_all(&[0, 1, b'q']).expect("write the framed request");
    stream.flush().expect("flush the framed request");

    // `epmd_srv.c`'s `default:` arm answers nothing; the connection just closes.
    let mut byte = [0u8; 1];
    let read = stream.read(&mut byte).expect("the stub must close, not stall");
    assert_eq!(read, 0, "an unknown tag must produce no reply, got {byte:?}");
}

/// **Oracle cross-check.** A real `erl` node registers with the stub through
/// `-epmd_port`, and the crate's client resolves that registration.
///
/// The stub is started first, so nothing here depends on a daemon on the default
/// port. Skips (and returns) only when there is no usable `erl`.
#[test]
fn the_stub_accepts_a_real_erlang_node_and_the_crate_resolves_it() {
    if !erl_available() {
        eprintln!(
            "hermetic_epmd: SKIP — no usable `erl` on PATH; the stub round-trip and the \
             hermetic tests still run"
        );
        return;
    }

    let stub = EpmdStub::start().expect("bind a hermetic epmd stub");
    let addr = stub.addr_string();
    let dist_port = free_port();
    let short_name = unique_node_name();

    let node = match ErlNode::start(&short_name, NODE_COOKIE, dist_port, stub.addr().port()) {
        Ok(node) => node,
        Err(err) => {
            eprintln!("hermetic_epmd: SKIP — could not spawn `erl` ({err})");
            return;
        }
    };

    // `-eval` runs after boot, i.e. after distribution (and so the epmd
    // registration) has been set up.
    assert!(
        node.wait_for(|line| line.contains("READY"), READY_TIMEOUT).is_some(),
        "the node never printed READY; stderr:\n{}",
        node.stderr_text()
    );

    // (a) The stub's hand-written parser matched a REAL ERTS client: the node
    //     registered the pinned dist port under its short name.
    let record = stub.wait_for_record(short_name.as_bytes(), OBSERVE_TIMEOUT).unwrap_or_else(|| {
        panic!("the stub never saw {short_name}'s ALIVE2_REQ; stderr:\n{}", node.stderr_text())
    });
    assert_eq!(record.port, dist_port, "the node registers its pinned dist port");
    assert_eq!(record.node_type, EPMD_NODE_NORMAL, "`-sname` registers a normal node");
    assert_eq!(record.protocol, EPMD_PROTO_TCP, "the node registers over tcp/ip");
    assert_eq!(record.extra, Vec::<u8>::new(), "the node advertises no extra field");
    assert!(
        record.high_vsn >= 6 && record.low_vsn <= record.high_vsn,
        "the node advertises a well-formed version interval: {record:?}"
    );

    // (b) The crate's client resolves that registration (PORT2_REQ/PORT2_RESP).
    let mut client_error = String::from("no reply yet");
    let resolved = poll_until(
        || match lookup_once(&addr, short_name.as_bytes()) {
            Ok(resp) if resp.is_registered() => Some(resp),
            Ok(other) => {
                client_error = format!("{other:?}");
                None
            }
            Err(err) => {
                client_error = format!("{err:?}");
                None
            }
        },
        OBSERVE_TIMEOUT,
    )
    .unwrap_or_else(|| {
        panic!(
            "the crate's client never resolved {short_name} (last: {client_error}); stderr:\n{}",
            node.stderr_text()
        )
    });
    match &resolved {
        Port2Resp::Ok { port, node_type, .. } => {
            assert_eq!(*port, dist_port, "the crate must resolve the node's pinned port");
            assert_eq!(*node_type, EPMD_NODE_NORMAL, "the resolved node type");
        }
        Port2Resp::Error => panic!("PORT2_RESP reported the live node as unregistered: {resolved:?}"),
    }

    // (c) The registration is LIVE: a second ALIVE2_REQ for the same name is
    //     refused, which is what the real daemon does for an occupied name
    //     (`epmd_srv.c` "node name already occupied").
    let (_dup, duplicate) = publish(&addr, &alive2_req(short_name.as_bytes(), dist_port))
        .expect("the stub answers the duplicate ALIVE2_REQ");
    assert!(duplicate.is_alive_conflict(), "the live node keeps its name: {duplicate:?}");
    assert!(stub.record(short_name.as_bytes()).is_some(), "the node's registration survives it");

    // Killing the node closes its ALIVE socket, which de-registers it.
    drop(node);
    assert!(
        stub.wait_until_gone(short_name.as_bytes(), DEREGISTER_TIMEOUT),
        "the stub must de-register {short_name} once the node is gone"
    );
}

// ---------------------------------------------------------------------------
// The in-process `epmd` stub
// ---------------------------------------------------------------------------

/// One registration the stub holds.
#[derive(Clone, Debug)]
struct StubNode {
    /// The registered (short) node name.
    name: Vec<u8>,
    /// The node's listen port.
    port: u16,
    node_type: u8,
    protocol: u8,
    high_vsn: u16,
    low_vsn: u16,
    extra: Vec<u8>,
    /// The creation the stub assigned (also returned in its `ALIVE2_RESP`).
    creation: u32,
    /// The connection that registered this node: closing it de-registers the node.
    owner: u64,
}

/// The fields of an `ALIVE2_REQ`, hand-decoded by the stub.
#[derive(Clone, Debug)]
struct Alive2Fields {
    port: u16,
    node_type: u8,
    protocol: u8,
    high_vsn: u16,
    low_vsn: u16,
    name: Vec<u8>,
    extra: Vec<u8>,
}

/// What the stub does with a connection after it served one request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Serving {
    /// Hold the (idle) socket open, as the daemon's `keep` flag does, so the
    /// node's death can be detected.
    KeepAlive,
    /// Close the connection after the reply, as the daemon does for every
    /// non-`ALIVE` request (and for an unknown tag, where it does not reply).
    Close,
}

/// The state shared by the stub's accept thread and its connection workers.
struct StubState {
    /// The port the stub listens on (what `NAMES_RESP` reports).
    port: u16,
    /// The registrations, in registration order (so `NAMES_RESP` is stable).
    nodes: Mutex<Vec<StubNode>>,
    /// Set by [`EpmdStub`]'s `Drop`: every worker polls it, so the stub always
    /// shuts down promptly instead of wedging a test.
    stop: AtomicBool,
    next_conn: AtomicU64,
    next_creation: AtomicU32,
}

impl StubState {
    /// A fresh, empty registry for a stub listening on `port`.
    fn new(port: u16) -> StubState {
        StubState {
            port,
            nodes: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            next_conn: AtomicU64::new(1),
            next_creation: AtomicU32::new(FIRST_CREATION),
        }
    }

    /// The id of a new connection.
    fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers `fields` on behalf of connection `owner`, returning the assigned
    /// creation or `Err(())` when the name is already taken by a live node (the
    /// daemon's `result = 1`).
    fn register(&self, owner: u64, fields: &Alive2Fields) -> Result<u32, ()> {
        // The daemon rejects an empty name and one longer than 255 bytes
        // (`epmd_srv.c:899-902`); UTF-8 validity is not checked here.
        if fields.name.is_empty() || fields.name.len() > 255 {
            return Err(());
        }
        let mut nodes = lock(&self.nodes);
        if nodes.iter().any(|node| node.name == fields.name) {
            return Err(());
        }
        let creation = self.next_creation.fetch_add(1, Ordering::Relaxed);
        nodes.push(StubNode {
            name: fields.name.clone(),
            port: fields.port,
            node_type: fields.node_type,
            protocol: fields.protocol,
            high_vsn: fields.high_vsn,
            low_vsn: fields.low_vsn,
            extra: fields.extra.clone(),
            creation,
            owner,
        });
        Ok(creation)
    }

    /// Drops every registration owned by the (now closed) connection `owner` —
    /// the daemon's `node_unreg_sock` (`epmd_srv.c:643`).
    fn unregister_owner(&self, owner: u64) {
        lock(&self.nodes).retain(|node| node.owner != owner);
    }

    /// Drops the registration named `name`, reporting whether there was one
    /// (`STOP_REQ`, `epmd_srv.c:1089`).
    fn unregister_name(&self, name: &[u8]) -> bool {
        let mut nodes = lock(&self.nodes);
        match nodes.iter().position(|node| node.name == name) {
            Some(index) => {
                nodes.remove(index);
                true
            }
            None => false,
        }
    }

    /// The registration named `name`, if any.
    fn find(&self, name: &[u8]) -> Option<StubNode> {
        lock(&self.nodes).iter().find(|node| node.name == name).cloned()
    }

    /// Every registration, in registration order.
    fn all(&self) -> Vec<StubNode> {
        lock(&self.nodes).clone()
    }
}

/// A minimal in-process `epmd` **for the tests in this file only**.
///
/// It binds an ephemeral loopback port, accepts connections on a background
/// thread, and serves each connection on its own worker (a registered node's
/// socket stays open, so a single-threaded server would block every later
/// lookup). Every read is bounded by [`STUB_READ_TIMEOUT`], and `Drop` stops and
/// joins the workers, so a failing assertion can neither hang nor leak a thread.
struct EpmdStub {
    addr: SocketAddr,
    state: Arc<StubState>,
    accept: Option<JoinHandle<()>>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl EpmdStub {
    /// Binds `127.0.0.1:0` and starts serving.
    fn start() -> std::io::Result<EpmdStub> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let state = Arc::new(StubState::new(addr.port()));
        let workers = Arc::new(Mutex::new(Vec::new()));

        let accept_state = Arc::clone(&state);
        let accept_workers = Arc::clone(&workers);
        let accept = thread::spawn(move || {
            // `incoming()` ends when `accept` fails; the stub's own `Drop` also
            // wakes this loop with a self-connect.
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if accept_state.stop.load(Ordering::SeqCst) {
                    break;
                }
                let worker_state = Arc::clone(&accept_state);
                let worker = thread::spawn(move || serve_connection(stream, worker_state));
                lock(&accept_workers).push(worker);
            }
        });

        Ok(EpmdStub { addr, state, accept: Some(accept), workers })
    }

    /// The address the stub listens on.
    fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The stub's address as the `host:port` pair the client's `connect` wants.
    fn addr_string(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }

    /// The registration named `name`, if the stub currently holds one.
    fn record(&self, name: &[u8]) -> Option<StubNode> {
        self.state.find(name)
    }

    /// Waits (bounded) for the registration named `name` to appear.
    fn wait_for_record(&self, name: &[u8], timeout: Duration) -> Option<StubNode> {
        poll_until(|| self.state.find(name), timeout)
    }

    /// Waits (bounded) for the registration named `name` to disappear.
    fn wait_until_gone(&self, name: &[u8], timeout: Duration) -> bool {
        poll_until(|| self.record(name).is_none().then_some(()), timeout).is_some()
    }
}

impl Drop for EpmdStub {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop: it is sitting in `accept`, so the self-connect
        // is what lets it observe the stop flag and return.
        if let Ok(waker) = TcpStream::connect(self.addr) {
            let _ = waker.shutdown(Shutdown::Both);
        }
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        // Every worker notices the stop flag at its next read timeout, so the
        // joins below are bounded by `STUB_READ_TIMEOUT` (they run concurrently).
        let workers: Vec<JoinHandle<()>> = lock(&self.workers).drain(..).collect();
        for worker in workers {
            let _ = worker.join();
        }
    }
}

/// Serves one stub connection until its peer (or the stub) is done with it.
fn serve_connection(mut stream: TcpStream, state: Arc<StubState>) {
    let _ = stream.set_read_timeout(Some(STUB_READ_TIMEOUT));
    let _ = stream.set_nodelay(true);

    let owner = state.next_conn_id();
    loop {
        let Some(body) = read_request(&mut stream, &state.stop) else {
            break;
        };
        if serve_request(&body, &mut stream, &state, owner) == Serving::Close {
            break;
        }
        // The registration succeeded: from here the daemon only watches for the
        // socket closing (any further byte is "more than expected on ALIVE
        // socket" and also ends the registration, `epmd_srv.c:653-659`).
        wait_for_close(&mut stream, &state.stop);
        break;
    }
    // Closing the ALIVE socket de-registers everything it registered.
    state.unregister_owner(owner);
}

/// Serves one framed request, returning what the connection should do next.
fn serve_request(body: &[u8], stream: &mut TcpStream, state: &StubState, owner: u64) -> Serving {
    match body.first().copied() {
        Some(EPMD_ALIVE2_REQ) => {
            let Some(fields) = parse_alive2(body) else {
                return Serving::Close;
            };
            match state.register(owner, &fields) {
                Ok(creation) => {
                    let reply = encode_alive2_resp(true, creation, fields.high_vsn);
                    if write_reply(stream, &reply).is_err() {
                        return Serving::Close;
                    }
                    Serving::KeepAlive
                }
                Err(()) => {
                    // `epmd_srv.c:848`: a refused registration still carries
                    // creation 99, so the reply keeps its width.
                    let reply = encode_alive2_resp(false, 99, fields.high_vsn);
                    let _ = write_reply(stream, &reply);
                    Serving::Close
                }
            }
        }
        Some(EPMD_PORT2_REQ) => {
            let Some(name) = body.get(1..) else {
                return Serving::Close;
            };
            let reply = encode_port2_resp(state.find(name).as_ref());
            let _ = write_reply(stream, &reply);
            Serving::Close
        }
        Some(EPMD_NAMES_REQ) => {
            let reply = encode_names_resp(state.port, &state.all());
            let _ = write_reply(stream, &reply);
            Serving::Close
        }
        Some(EPMD_STOP_REQ) => {
            let Some(name) = body.get(1..) else {
                return Serving::Close;
            };
            // The daemon requires a name (`epmd_srv.c:1079-1083`).
            if name.is_empty() {
                return Serving::Close;
            }
            let reply = encode_stop_resp(state.unregister_name(name));
            let _ = write_reply(stream, &reply);
            Serving::Close
        }
        // `epmd_srv.c`'s `default:` arm: nothing is written, the connection just
        // closes.
        _ => Serving::Close,
    }
}

/// Reads one 2-byte-framed request body, or `None` when the peer closed the
/// connection (or the stub is shutting down). A zero-length frame is not a
/// request and closes the connection (`epmd_srv.c:697` reads the same framing).
fn read_request(stream: &mut TcpStream, stop: &AtomicBool) -> Option<Vec<u8>> {
    let mut header = [0u8; 2];
    read_full(stream, &mut header, stop)?;
    let len = usize::from(u16::from_be_bytes(header));
    if len == 0 {
        return None;
    }
    let mut body = vec![0u8; len];
    read_full(stream, &mut body, stop)?;
    Some(body)
}

/// Fills `buf` from `stream`, retrying over read timeouts; `None` means the peer
/// closed the connection (or the stub was asked to stop).
///
/// Reading byte-run-wise (rather than with `read_exact`) is what makes a timeout
/// harmless: `Read::read_exact` would discard a partial read.
fn read_full(stream: &mut TcpStream, buf: &mut [u8], stop: &AtomicBool) -> Option<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return None,
            Ok(read) => filled += read,
            Err(err) if is_quiet(err.kind()) => {
                if stop.load(Ordering::SeqCst) {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    Some(())
}

/// Holds an `ALIVE` socket open until its peer closes it (or sends anything at
/// all, which the daemon also treats as the end of the registration), polling the
/// stop flag so the stub can always shut down.
fn wait_for_close(stream: &mut TcpStream, stop: &AtomicBool) {
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            // The node died (`node_unreg_sock`), or sent more than expected.
            Ok(_) => return,
            Err(err) if is_quiet(err.kind()) => {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Writes one reply body. A reply is never length-framed (`epmd_srv.c:1547`).
fn write_reply(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    stream.write_all(body)?;
    stream.flush()
}

/// Hand-decodes an `ALIVE2_REQ` body (`epmd_srv.c:820-847`).
fn parse_alive2(body: &[u8]) -> Option<Alive2Fields> {
    let mut wire = Wire::new(body);
    if wire.u8()? != EPMD_ALIVE2_REQ {
        return None;
    }
    let port = wire.u16()?;
    let node_type = wire.u8()?;
    let protocol = wire.u8()?;
    let high_vsn = wire.u16()?;
    let low_vsn = wire.u16()?;
    let namelen = usize::from(wire.u16()?);
    let name = wire.take(namelen)?.to_vec();
    let extralen = usize::from(wire.u16()?);
    let extra = wire.take(extralen)?.to_vec();
    // Trailing bytes are not an `ALIVE2_REQ` (`epmd` reads the whole packet).
    if !wire.is_done() {
        return None;
    }
    Some(Alive2Fields { port, node_type, protocol, high_vsn, low_vsn, name, extra })
}

/// Hand-encodes an `ALIVE2_RESP` (`epmd_srv.c:849-859`): `'v' + result(1) +
/// creation(4)` for a `highvsn >= 6` requester, `'y' + result(1) + creation(2)`
/// otherwise.
fn encode_alive2_resp(accepted: bool, creation: u32, request_high_vsn: u16) -> Vec<u8> {
    let result = if accepted { 0u8 } else { 1u8 };
    if request_high_vsn >= 6 {
        let mut out = Vec::with_capacity(6);
        out.push(EPMD_ALIVE2_X_RESP);
        out.push(result);
        out.extend_from_slice(&creation.to_be_bytes());
        out
    } else {
        let mut out = Vec::with_capacity(4);
        out.push(EPMD_ALIVE2_RESP);
        out.push(result);
        out.extend_from_slice(&(creation as u16).to_be_bytes());
        out
    }
}

/// Hand-encodes a `PORT2_RESP` (`epmd_srv.c:904-936`): `'w' + result(1)`, and on
/// success `port(2) node_type(1) protocol(1) high_vsn(2) low_vsn(2) namelen(2)
/// name extralen(2) extra`. There is deliberately **no creation** field.
fn encode_port2_resp(node: Option<&StubNode>) -> Vec<u8> {
    let Some(node) = node else {
        return vec![EPMD_PORT2_RESP, 1];
    };
    let mut out = Vec::with_capacity(14 + node.name.len() + node.extra.len());
    out.push(EPMD_PORT2_RESP);
    out.push(0);
    out.extend_from_slice(&node.port.to_be_bytes());
    out.push(node.node_type);
    out.push(node.protocol);
    out.extend_from_slice(&node.high_vsn.to_be_bytes());
    out.extend_from_slice(&node.low_vsn.to_be_bytes());
    out.extend_from_slice(&(node.name.len() as u16).to_be_bytes());
    out.extend_from_slice(&node.name);
    out.extend_from_slice(&(node.extra.len() as u16).to_be_bytes());
    out.extend_from_slice(&node.extra);
    out
}

/// Hand-encodes a `NAMES_RESP` (`epmd_srv.c:946-976`): the daemon's own port as a
/// big-endian `u32`, then `name <name> at port <port>\n` per registered node.
fn encode_names_resp(port: u16, nodes: &[StubNode]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&u32::from(port).to_be_bytes());
    for node in nodes {
        out.extend_from_slice(
            format!("name {} at port {}\n", String::from_utf8_lossy(&node.name), node.port)
                .as_bytes(),
        );
    }
    out
}

/// The `STOP_REQ` reply texts (`epmd_srv.c:1091-1109`).
fn encode_stop_resp(removed: bool) -> Vec<u8> {
    if removed {
        b"STOPPED".to_vec()
    } else {
        b"NOEXIST".to_vec()
    }
}

/// A bounds-checked cursor over an already-received request body: running past the
/// end yields `None` — never a panic — so a truncated request is rejected rather
/// than mis-sliced.
struct Wire<'a> {
    body: &'a [u8],
    pos: usize,
}

impl<'a> Wire<'a> {
    /// Starts a cursor at the front of `body`.
    fn new(body: &'a [u8]) -> Wire<'a> {
        Wire { body, pos: 0 }
    }

    /// Reads one byte.
    fn u8(&mut self) -> Option<u8> {
        let byte = *self.body.get(self.pos)?;
        self.pos += 1;
        Some(byte)
    }

    /// Reads a big-endian `u16`.
    fn u16(&mut self) -> Option<u16> {
        let bytes = self.take(2)?;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Reads the next `n` bytes (all of them, or `None`).
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.body.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// Whether the whole body was consumed.
    fn is_done(&self) -> bool {
        self.pos == self.body.len()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether an I/O failure just means "no bytes available right now" (a
/// non-blocking read or a socket read timeout) rather than a real failure.
fn is_quiet(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
    )
}

/// Locks a mutex, tolerating poisoning (a panic elsewhere must not turn a stub
/// helper into a second panic).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Polls `probe` until it yields `Some`, or `timeout` elapses.
fn poll_until<T, F: FnMut() -> Option<T>>(mut probe: F, timeout: Duration) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// The `ALIVE2_REQ` this crate's client sends for a `-sname`-style node.
fn alive2_req(name: &[u8], port: u16) -> Alive2Req {
    Alive2Req {
        port,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: EPMD_DIST_HIGH,
        low_vsn: EPMD_DIST_LOW,
        name: name.to_vec(),
        extra: Vec::new(),
    }
}

/// Opens a fresh client to `addr` and publishes `req`.
///
/// The client is returned alongside the reply because a registration lives
/// exactly as long as its `ALIVE` socket: dropping it de-registers the node.
fn publish(
    addr: &str,
    req: &Alive2Req,
) -> Result<(EpmdClient<TcpTransport>, Alive2Resp), BeamError> {
    let mut client = EpmdClient::connect(addr)?;
    let resp = client.publish(req)?;
    Ok((client, resp))
}

/// Resolves `name` over a **fresh** connection: the stub (like the real daemon,
/// `epmd_srv.c:727-730`) closes a non-`ALIVE` connection after one request.
fn lookup_once(addr: &str, name: &[u8]) -> Result<Port2Resp, BeamError> {
    let mut client = EpmdClient::connect(addr)?;
    client.port_please(name)
}

/// A free loopback TCP port (a small TOCTOU window the node's own bind absorbs by
/// failing loudly).
fn free_port() -> u16 {
    for _ in 0..16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
        let port = listener.local_addr().expect("read the ephemeral port").port();
        drop(listener);
        if port != 0 {
            return port;
        }
    }
    panic!("could not obtain a free loopback port");
}

/// A process-unique short node name, so a concurrent test in the same binary
/// cannot collide with this one's registration.
fn unique_node_name() -> String {
    static RUN: AtomicU32 = AtomicU32::new(0);
    format!("hstub{}_{}", std::process::id(), RUN.fetch_add(1, Ordering::Relaxed))
}

/// Whether a usable `erl` is on `PATH`.
fn erl_available() -> bool {
    Command::new("erl")
        .args(["-noshell", "-eval", "halt()."])
        .stdin(Stdio::null())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The `erl` child harness (this file owns its own: no shared fixture changes)
// ---------------------------------------------------------------------------

/// A spawned `erl` node whose stdout/stderr are read on background threads and
/// which is killed and reaped on `Drop`.
struct ErlNode {
    child: Child,
    /// The node's stdout lines.
    lines: Receiver<String>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    /// Everything the node wrote to stderr so far.
    stderr: Arc<Mutex<String>>,
}

impl ErlNode {
    /// Starts a `-sname` node that registers with the `epmd` on `epmd_port` (the
    /// `-epmd_port` override of `erl_epmd:get_epmd_port/0`, `erl_epmd.erl:373-380`)
    /// and listens for distribution on `dist_port`.
    ///
    /// `-start_epmd false` keeps `erlexec` from spawning a daemon on the default
    /// port (`erlexec.c:1133`), so a test run neither needs nor leaves behind a
    /// host `epmd`.
    fn start(name: &str, cookie: &str, dist_port: u16, epmd_port: u16) -> std::io::Result<ErlNode> {
        let eval =
            format!("io:format(\"READY~n\", []), timer:sleep({NODE_LIFETIME_MS}), halt().");
        let mut child = Command::new("erl")
            .arg("-sname")
            .arg(name)
            .arg("-setcookie")
            .arg(cookie)
            .args(["-epmd_port", &epmd_port.to_string()])
            .args(["-start_epmd", "false"])
            .arg("-kernel")
            .arg("inet_dist_listen_min")
            .arg(dist_port.to_string())
            .arg("inet_dist_listen_max")
            .arg(dist_port.to_string())
            .arg("-noshell")
            .arg("-eval")
            .arg(eval)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        let (tx, lines) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        let stderr_thread = thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                match line {
                    Ok(line) => lock(&stderr_sink).push_str(&format!("{line}\n")),
                    Err(_) => break,
                }
            }
        });

        Ok(ErlNode {
            child,
            lines,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr,
        })
    }

    /// Waits (bounded) for a stdout line satisfying `pred`.
    fn wait_for<F: Fn(&str) -> bool>(&self, pred: F, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    if pred(&line) {
                        return Some(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// The node's stderr collected so far (for failure diagnostics).
    fn stderr_text(&self) -> String {
        lock(&self.stderr).clone()
    }
}

impl Drop for ErlNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}
