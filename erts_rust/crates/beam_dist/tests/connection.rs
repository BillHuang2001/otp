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

//! Hermetic loopback integration test for the connection-level API
//! ([`beam_dist::connection`]).
//!
//! A real `TcpListener` is bound on `127.0.0.1:0`; the distribution **server**
//! runs on a spawned thread (`accept` → [`Connection::server`]) and the **client**
//! runs on the test thread ([`TcpTransport::connect`] → [`Connection::client`]).
//! Together they drive the full connect/challenge handshake and then exchange
//! `DOP_*` control messages over the established 4-byte-framed link.
//!
//! No external process is involved: no `epmd`, no live Erlang node, no network
//! beyond the loopback interface. Every socket carries a finite read timeout
//! ([`TEST_TIMEOUT`]), so a wire bug surfaces as a failed assertion rather than a
//! hung suite.

use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use beam_alloc::Heap;
use beam_codec::{AtomEncoding, ATOM_CACHE_REF_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT};
use beam_common::BeamError;
use beam_dist::connection::{
    accept_server, accept_server_with_decision, connect_client, connect_to_node, Connection,
    TcpListener as DistListener, TcpTransport, CLIENT_CREATION,
};
use beam_dist::dist_header::{DistHeader, DIST_FRAG_HEADER, DIST_HEADER};
use beam_dist::flags::{DistFlags, Dop, DFLAG_PUBLISHED};
use beam_dist::fragment::DIST_FRAGMENT_SIZE;
use beam_dist::handshake::{
    publish_flag, AcceptDecision, Name, PeerVisibility, Status, StatusKind,
};
use beam_dist::transport::{MemoryTransport, Read, Write};
use beam_dist::{
    check_mandatory, preferred_flags, read_packet, write_packet, EpmdClient, PacketHeaderSize,
    Port2Resp,
};
use beam_term::prelude::{atom_val, make_atom, make_small, Eterm, NIL};
use beam_term::AtomTable;

/// The shared cookie both ends authenticate with.
const COOKIE: &[u8] = b"s3cr3t-cookie";
/// The connecting node's name.
const CLIENT_NAME: &[u8] = b"client@host";
/// The accepting node's name.
const SERVER_NAME: &[u8] = b"server@host";
/// The accepting node's creation.
const SERVER_CREATION: u32 = 0xdead_beef;

/// A finite read timeout, so a stalled or broken link fails instead of hanging.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A handle to the server half running on its own thread.
type ServerHandle = thread::JoinHandle<Result<Connection<TcpTransport>, BeamError>>;

/// Spawns the **server**: accepts one connection and completes the handshake.
fn spawn_server(listener: TcpListener) -> ServerHandle {
    thread::spawn(move || {
        let (stream, _) = listener.accept().map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        Connection::server(transport, SERVER_NAME, COOKIE, SERVER_CREATION)
    })
}

/// Binds a loopback listener, starts the server thread and connects the client.
///
/// Returns the live client connection and the server thread's handle (join it
/// with [`joined`] once the client side has finished talking).
fn connect_pair() -> (Connection<TcpTransport>, ServerHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    let server = spawn_server(listener);
    let client = Connection::client(
        TcpTransport::connect(&addr.to_string()).expect("connect to server"),
        CLIENT_NAME,
        COOKIE,
    )
    .expect("client handshake");
    (client, server)
}

/// Joins the server thread and unwraps its connection.
fn joined(handle: ServerHandle) -> Connection<TcpTransport> {
    handle.join().expect("server thread").expect("server handshake")
}

/// Spawns the **server** advertising an explicit capability flag set.
fn spawn_server_with(listener: TcpListener, flags: DistFlags) -> ServerHandle {
    thread::spawn(move || {
        let (stream, _) = listener.accept().map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        Connection::server_with_flags(transport, SERVER_NAME, COOKIE, SERVER_CREATION, flags)
    })
}

/// Binds a loopback listener, starts the server advertising `server_flags` and
/// connects a client advertising `client_flags` (both opt-in seams).
fn connect_pair_with(
    server_flags: DistFlags,
    client_flags: DistFlags,
) -> (Connection<TcpTransport>, ServerHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    let server = spawn_server_with(listener, server_flags);
    let client = Connection::client_with_flags(
        TcpTransport::connect(&addr.to_string()).expect("connect to server"),
        CLIENT_NAME,
        COOKIE,
        CLIENT_CREATION,
        client_flags,
    )
    .expect("client handshake");
    (client, server)
}

/// The body-form external term image of `term` (no `VERSION_MAGIC`).
///
/// `Eterm` pointers are heap-relative and therefore *differ* between the sender's
/// and the receiver's heap, so decoded terms are compared by their encoded image.
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

// ---------------------------------------------------------------------------
// A transport that records what actually crossed the socket, so a test can prove
// that a large message really was fragmented (rather than merely round-tripping).
// ---------------------------------------------------------------------------

/// One direction's worth of recorded transfers, shared with the server thread.
///
/// Every [`beam_dist::framing::write_packet`] emits the length prefix and then the
/// whole body in one `write_all`, and `read_packet` reads the prefix and then the
/// whole body — so one frame contributes two recorded blocks, the body being a
/// contiguous block that starts with the frame's first tag byte.
#[derive(Clone, Default)]
struct WireLog(Arc<Mutex<Vec<Vec<u8>>>>);

impl WireLog {
    fn new() -> WireLog {
        WireLog::default()
    }

    fn record(&self, bytes: &[u8]) {
        self.0.lock().expect("wire log").push(bytes.to_vec());
    }

    /// Whether the *exact* byte block `wanted` was recorded, i.e. one
    /// `write_all`/`read_exact` carried exactly those bytes.
    ///
    /// [`beam_dist::framing::read_packet`]/`write_packet` transfer the length
    /// prefix and the whole body as two separate calls, so a frame body is one
    /// recorded block — which makes this an exact frame-body pin.
    fn contains(&self, wanted: &[u8]) -> bool {
        self.0.lock().expect("wire log").iter().any(|b| b.as_slice() == wanted)
    }
    /// Whether any recorded block is a `'E'` fragment-header frame body
    /// (`VERSION_MAGIC + DIST_FRAG_HEADER`), i.e. an actual fragment was exchanged.
    fn saw_fragment_header(&self) -> bool {
        self.0.lock().expect("wire log").iter().any(|b| {
            b.len() >= 2 && b[0] == beam_codec::VERSION_MAGIC && b[1] == DIST_FRAG_HEADER
        })
    }

    /// The recorded frame body that carries a `'D'` distribution header (an
    /// established packet), if one was exchanged.
    fn distribution_body(&self) -> Option<Vec<u8>> {
        self.0
            .lock()
            .expect("wire log")
            .iter()
            .find(|b| b.len() >= 2 && b[0] == beam_codec::VERSION_MAGIC && b[1] == DIST_HEADER)
            .cloned()
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

// ---------------------------------------------------------------------------
// (a) The handshake establishes an agreed link.
// ---------------------------------------------------------------------------

#[test]
fn handshake_establishes_an_agreed_link() {
    let (client, server) = connect_pair();
    let server = joined(server);

    // Both ends advertise `preferred_flags()` and each validates the peer's set;
    // `negotiated` is therefore the same validated set on both sides.
    let agreed = check_mandatory(preferred_flags()).expect("the preferred set is conforming");
    assert_eq!(client.client_flags(), preferred_flags());
    assert_eq!(client.server_flags(), preferred_flags());
    assert_eq!(server.client_flags(), preferred_flags());
    assert_eq!(server.server_flags(), preferred_flags());
    assert_eq!(client.negotiated(), agreed);
    assert_eq!(server.negotiated(), agreed);

    // Each end learns the other's name and creation.
    assert_eq!(client.peer_name(), SERVER_NAME);
    assert_eq!(server.peer_name(), CLIENT_NAME);
    assert_eq!(client.peer_creation(), SERVER_CREATION);
    assert_eq!(server.peer_creation(), 0);

    // The link starts with no ticks exchanged.
    assert_eq!(client.ticks_answered(), 0);
    assert_eq!(server.ticks_answered(), 0);

    // GOLDEN-VECTOR: no `DFLAG_PUBLISHED` is advertised by either default
    // constructor (the emulator's `erts_dflags.default`, `dist.h:118-129`), so each
    // side classifies the other as hidden.
    assert_eq!(client.peer_visibility(), PeerVisibility::Hidden);
    assert_eq!(server.peer_visibility(), PeerVisibility::Hidden);
}

// ---------------------------------------------------------------------------
// (a2) The opt-in flag seam: a *published* node advertises DFLAG_PUBLISHED and the
//      peer classifies it normal, while every default stays byte-identical.
// ---------------------------------------------------------------------------

#[test]
fn opt_in_flags_are_advertised_and_peers_are_classified() {
    // A published acceptor: exactly `preferred_flags()` plus the published bit
    // (`dist_util.erl:199-200` / `publish_flag/3`).
    let published = preferred_flags() | publish_flag(false, true);
    assert_eq!(published.bits(), preferred_flags().bits() | DFLAG_PUBLISHED);
    assert_ne!(published, preferred_flags());

    let (client, server) = connect_pair_with(published, preferred_flags());
    let server = joined(server);

    // The server advertised the opt-in set on both transcriptions; the client kept
    // its default.
    assert_eq!(client.server_flags(), published);
    assert_eq!(server.server_flags(), published);
    assert_eq!(client.client_flags(), preferred_flags());
    assert_eq!(server.client_flags(), preferred_flags());

    // Each side classifies the OTHER purely from the other's own DFLAG_PUBLISHED
    // (`dist_util.erl:publish_type/1`): the published server reads as normal, the
    // unpublished client as hidden.
    assert_eq!(client.peer_visibility(), PeerVisibility::Normal);
    assert_eq!(server.peer_visibility(), PeerVisibility::Hidden);
    assert_eq!(client.handshake().peer_visibility, PeerVisibility::Normal);
    assert_eq!(server.handshake().peer_visibility, PeerVisibility::Hidden);

    // The flag is the ONLY difference: what the server advertised equals
    // `preferred_flags()` with the published bit OR'd in, and nothing else moved.
    assert!(published.contains(DistFlags::from_bits(DFLAG_PUBLISHED)));
    assert_eq!(
        published.bits() & !DFLAG_PUBLISHED,
        preferred_flags().bits(),
        "the opt-in must add exactly DFLAG_PUBLISHED and change nothing else",
    );

    // And the default pair (no opt-in) remains byte-identically hidden on both
    // ends — the seam changes nothing when it is not used.
    let (client, server) = connect_pair();
    let server = joined(server);
    assert_eq!(client.server_flags(), preferred_flags());
    assert_eq!(client.client_flags(), preferred_flags());
    assert_eq!(client.peer_visibility(), PeerVisibility::Hidden);
    assert_eq!(server.peer_visibility(), PeerVisibility::Hidden);
}

// ---------------------------------------------------------------------------
// (b) A control message reaches the peer and decodes equal.
// ---------------------------------------------------------------------------

#[test]
fn a_non_message_control_reaches_the_server() {
    let (mut client, server) = connect_pair();
    let mut server = joined(server);

    let mut atoms = AtomTable::new();
    let mut server_heap = Heap::new(4096).expect("server heap");

    // `{DOP_REG_SEND, From, Cookie, ToName}` (`dist.c:2476`): an exact-arity
    // control carrying immediates only.
    let from = make_atom(atoms.intern("from"));
    let cookie = make_atom(atoms.intern("cookie"));
    let to_name = make_atom(atoms.intern("to_name"));

    client
        .send_control(Dop::RegSend, &[from, cookie, to_name], &atoms)
        .expect("send DOP_REG_SEND");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv DOP_REG_SEND");

    assert_eq!(got.dop, Dop::RegSend);
    assert_eq!(got.terms, vec![from, cookie, to_name]);
}

// ---------------------------------------------------------------------------
// (c) A `send` message round-trips both ways, including a boxed term.
// ---------------------------------------------------------------------------

#[test]
fn a_send_message_with_a_tuple_round_trips_both_ways() {
    let (mut client, server) = connect_pair();
    let mut server = joined(server);

    let mut atoms = AtomTable::new();
    let mut client_heap = Heap::new(4096).expect("client heap");
    let mut server_heap = Heap::new(4096).expect("server heap");

    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));

    // Client → server: `DFLAG_SEND_SENDER` is negotiated, so `send_message` picks
    // `DOP_SEND_SENDER` and the message term follows `{DOP, From, To}`.
    let payload = client_heap
        .tuple(&[make_atom(atoms.intern("hello")), make_small(42), NIL])
        .expect("build payload tuple");
    client.send_message(from, to, payload, &atoms).expect("send message");

    let got = server.recv(&mut server_heap, &mut atoms).expect("recv message");
    assert_eq!(got.dop, Dop::SendSender);
    assert_eq!(got.terms.len(), 3, "From, To and the message term");
    assert_eq!(got.terms[0], from);
    assert_eq!(got.terms[1], to);
    assert_eq!(body_image(got.terms[2], &atoms), body_image(payload, &atoms));

    // Server → client: the reply is decoded with the client's heap.
    let reply = server_heap
        .tuple(&[make_atom(atoms.intern("world")), make_small(7)])
        .expect("build reply tuple");
    server.send_message(to, from, reply, &atoms).expect("send reply");

    let got = client.recv(&mut client_heap, &mut atoms).expect("recv reply");
    assert_eq!(got.dop, Dop::SendSender);
    assert_eq!(got.terms[0], to);
    assert_eq!(got.terms[1], from);
    assert_eq!(body_image(got.terms[2], &atoms), body_image(reply, &atoms));
}

// ---------------------------------------------------------------------------
// (d) A wrong cookie fails the handshake — with an error, never a hang.
// ---------------------------------------------------------------------------

#[test]
fn a_wrong_cookie_fails_the_handshake_without_hanging() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    let server = spawn_server(listener);

    let outcome = Connection::client(
        TcpTransport::connect(&addr.to_string()).expect("connect to server"),
        CLIENT_NAME,
        b"the-wrong-cookie",
    );
    let err = match outcome {
        Ok(_) => panic!("a wrong cookie must not establish a link"),
        Err(err) => err,
    };
    match err {
        // A clean EOF: the server dropped the link after rejecting our digest
        // (the client's next read is a short read).
        BeamError::BadArg => {}
        // A torn-down socket is also a legitimate rejection — but a *timeout*
        // would mean the handshake hung, which must never happen.
        BeamError::Io(io) => assert_ne!(
            io.kind(),
            std::io::ErrorKind::WouldBlock,
            "the handshake must not hang: {io}"
        ),
        other => panic!("unexpected handshake failure: {other:?}"),
    }

    // The server side failed too: it could not verify the reply digest.
    let server_outcome = server.join().expect("server thread");
    assert!(matches!(server_outcome, Err(BeamError::BadArg)), "the server must reject the reply");
}

// ---------------------------------------------------------------------------
// (d2) A clean peer close at a frame boundary is distinct from malformed data.
// ---------------------------------------------------------------------------

#[test]
fn a_clean_peer_close_at_a_frame_boundary_reports_eof_not_bad_arg() {
    let (mut client, server) = connect_pair();
    // Join the server and drop its connection: the underlying socket closes
    // cleanly (a FIN), with no half-written frame in flight. The client's next
    // frame read therefore starts at a boundary and reads no byte at all.
    drop(joined(server));

    let mut heap = Heap::new(4096).expect("heap");
    let mut atoms = AtomTable::new();
    match client.recv(&mut heap, &mut atoms) {
        // "The peer went away" — reported as `UnexpectedEof`, NOT the `BadArg`
        // that malformed/truncated wire data yields.
        Err(BeamError::Io(err)) => assert_eq!(
            err.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "a clean peer close must surface as UnexpectedEof: {err}"
        ),
        other => panic!("a clean peer close must be UnexpectedEof, not {other:?}"),
    }
}
// ---------------------------------------------------------------------------
// (e) A message larger than a fragment round-trips (fragmentation both ways).
// ---------------------------------------------------------------------------

#[test]
fn a_message_larger_than_a_fragment_round_trips() {
    let (mut client, server) = connect_pair();
    let mut server = joined(server);

    let mut atoms = AtomTable::new();
    let mut client_heap = Heap::new(1 << 17).expect("client heap");
    let mut server_heap = Heap::new(1 << 17).expect("server heap");

    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));

    // Each list element is an `INTEGER_EXT` (5 bytes), so 14_000 elements encode
    // to ~70 KB — comfortably past `DIST_FRAGMENT_SIZE` (64 KiB) and therefore
    // split into an `'E'` first fragment plus `'F'` continuations.
    let big = big_list(&mut client_heap, 14_000, 1_000);
    let image = body_image(big, &atoms);
    assert!(
        image.len() > DIST_FRAGMENT_SIZE,
        "the payload must exceed one fragment, got {} bytes",
        image.len()
    );

    // Client → server.
    client.send_message(from, to, big, &atoms).expect("send big message");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv big message");
    assert_eq!(got.dop, Dop::SendSender);
    assert_eq!(body_image(got.terms[2], &atoms), image);

    // Server → client (the server's sender fragments independently).
    let back = big_list(&mut server_heap, 14_000, 1_000);
    let back_image = body_image(back, &atoms);
    server.send_message(to, from, back, &atoms).expect("send big reply");
    let got = client.recv(&mut client_heap, &mut atoms).expect("recv big reply");
    assert_eq!(got.terms[0], to);
    assert_eq!(got.terms[1], from);
    assert_eq!(body_image(got.terms[2], &atoms), back_image);
}

// ---------------------------------------------------------------------------
// (f) A tick is answered transparently and the stream continues.
// ---------------------------------------------------------------------------

#[test]
fn a_tick_is_answered_transparently() {
    let (mut client, server) = connect_pair();
    let mut server = joined(server);

    let mut atoms = AtomTable::new();
    let mut client_heap = Heap::new(4096).expect("client heap");
    let mut server_heap = Heap::new(4096).expect("server heap");

    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));

    // The client sends a bare tick, then a real message. The server must answer
    // the tick with a tick and still deliver the message that follows it.
    client.send_tick().expect("send tick");
    client
        .send_control(Dop::Link, &[from, to], &atoms)
        .expect("send DOP_LINK");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv past the tick");
    assert_eq!(got.dop, Dop::Link);
    assert_eq!(got.terms, vec![from, to]);
    assert_eq!(server.ticks_answered(), 1, "the tick was answered");

    // The test thread's client still has the server's tick answer queued; its next
    // `recv` absorbs it (answering in turn) and returns the following message.
    server.send_control(Dop::Link, &[to, from], &atoms).expect("send reply");
    let got = client.recv(&mut client_heap, &mut atoms).expect("recv past the tick answer");
    assert_eq!(got.dop, Dop::Link);
    assert_eq!(got.terms, vec![to, from]);
    assert_eq!(client.ticks_answered(), 1, "the server's tick answer was absorbed");
}

// ---------------------------------------------------------------------------
// The message above really is split into `'E'`/`'F'` fragment frames on the wire.
// ---------------------------------------------------------------------------

#[test]
fn a_large_message_is_fragmented_on_the_wire_both_ways() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");

    let server_reads = WireLog::new();
    let server_writes = WireLog::new();
    let client_reads = WireLog::new();
    let client_writes = WireLog::new();

    let sr = server_reads.clone();
    let sw = server_writes.clone();
    let server = thread::spawn(move || -> beam_common::Result<()> {
        let (stream, _) = listener.accept().map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        let mut conn = Connection::server(
            SpyTransport { inner: transport, reads: sr, writes: sw },
            SERVER_NAME,
            COOKIE,
            SERVER_CREATION,
        )?;

        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(1 << 17).expect("server heap");
        let from = make_atom(atoms.intern("from"));
        let to = make_atom(atoms.intern("to"));

        // Receive the client's oversized message, then fragment one back.
        let got = conn.recv(&mut heap, &mut atoms)?;
        assert_eq!(got.dop, Dop::SendSender);
        let big = big_list(&mut heap, 14_000, 1_000);
        conn.send_message(to, from, big, &atoms)?;
        Ok(())
    });

    let mut conn = Connection::client(
        SpyTransport {
            inner: TcpTransport::connect(&addr.to_string()).expect("connect"),
            reads: client_reads.clone(),
            writes: client_writes.clone(),
        },
        CLIENT_NAME,
        COOKIE,
    )
    .expect("client handshake");

    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 17).expect("client heap");
    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));

    let big = big_list(&mut heap, 14_000, 1_000);
    let image = body_image(big, &atoms);
    assert!(image.len() > DIST_FRAGMENT_SIZE, "the payload must exceed one fragment");
    conn.send_message(from, to, big, &atoms).expect("send big message");

    let reply = conn.recv(&mut heap, &mut atoms).expect("recv big reply");
    assert_eq!(reply.dop, Dop::SendSender);
    assert_eq!(reply.terms[0], to);
    assert_eq!(reply.terms[1], from);
    assert!(
        body_image(reply.terms[2], &atoms).len() > DIST_FRAGMENT_SIZE,
        "the reply must exceed one fragment too"
    );

    server.join().expect("server thread").expect("server side");

    // Both directions really carried `'E'` fragment-header frames.
    assert!(client_writes.saw_fragment_header(), "the client must fragment its large message");
    assert!(server_reads.saw_fragment_header(), "the server must receive 'E' fragments");
    assert!(server_writes.saw_fragment_header(), "the server must fragment its large reply");
    assert!(client_reads.saw_fragment_header(), "the client must receive 'E' fragments");
}

// ---------------------------------------------------------------------------
// (g) The outbound atom cache: atoms cross as header refs + `82` payload bytes.
// ---------------------------------------------------------------------------

#[test]
fn the_outbound_atom_cache_round_trips_over_a_real_link() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");

    let server_reads = WireLog::new();
    let server_writes = WireLog::new();
    let sr = server_reads.clone();
    let sw = server_writes.clone();
    let server = thread::spawn(move || -> beam_common::Result<Vec<String>> {
        let (stream, _) = listener.accept().map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        let mut conn = Connection::server(
            SpyTransport { inner: transport, reads: sr, writes: sw },
            SERVER_NAME,
            COOKIE,
            SERVER_CREATION,
        )?;
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(4096).expect("server heap");
        let got = conn.recv(&mut heap, &mut atoms)?;
        assert_eq!(got.dop, Dop::RegSend);
        // Resolve the control-argument atoms back to their names.
        Ok(got
            .terms
            .iter()
            .map(|term| atoms.name(atom_val(*term)).unwrap_or_default().to_string())
            .collect())
    });

    // The client is spied on so the test can inspect the real wire image.
    let client_reads = WireLog::new();
    let client_writes = WireLog::new();
    let mut client = Connection::client_with_creation(
        SpyTransport {
            inner: TcpTransport::connect(&addr.to_string()).expect("connect"),
            reads: client_reads,
            writes: client_writes.clone(),
        },
        CLIENT_NAME,
        COOKIE,
        42,
    )
    .expect("client handshake");

    let mut atoms = AtomTable::new();
    let from = make_atom(atoms.intern("from"));
    let cookie = make_atom(atoms.intern("cookie"));
    let to_name = make_atom(atoms.intern("to_name"));

    client
        .send_control(Dop::RegSend, &[from, cookie, to_name], &atoms)
        .expect("send DOP_REG_SEND");

    // The receiver resolves the payload's `82` refs through the header it applied.
    let names = server.join().expect("server thread").expect("server side");
    assert_eq!(names, vec!["from", "cookie", "to_name"]);

    // ... and the wire really carried one `New` ref per atom and a `82, index`
    // payload whose indices are the header ref positions.
    let body = client_writes.distribution_body().expect("the client sent a 'D' header");
    let mut scratch = AtomTable::new();
    let (header, offset) = DistHeader::decode_tail(&body, 2, &mut scratch, AtomEncoding::Utf8)
        .expect("decode the distribution header");
    assert_eq!(header.refs.len(), 3, "one ref per atom");
    assert!(header.refs.iter().all(beam_dist::dist_header::AtomCacheRef::is_new));
    assert_eq!(
        &body[offset..],
        &[
            SMALL_TUPLE_EXT,
            4,
            SMALL_INTEGER_EXT,
            Dop::RegSend.as_u8(),
            ATOM_CACHE_REF_EXT,
            0,
            ATOM_CACHE_REF_EXT,
            1,
            ATOM_CACHE_REF_EXT,
            2,
        ]
    );
}

#[test]
fn client_with_creation_advertises_its_creation_to_the_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    let server = spawn_server(listener);

    let client = Connection::client_with_creation(
        TcpTransport::connect(&addr.to_string()).expect("connect to server"),
        CLIENT_NAME,
        COOKIE,
        42,
    )
    .expect("client handshake");
    let server = joined(server);

    assert_eq!(server.peer_creation(), 42, "the server sees the client's creation");
    assert_eq!(client.peer_creation(), SERVER_CREATION, "and vice versa");
}

// ---------------------------------------------------------------------------
// (h) The crate's own listener adapter + the typed `DOP_*` send helpers.
// ---------------------------------------------------------------------------

/// Binds loopback, starts the server thread over the crate's own
/// [`DistListener`], and connects the client.
///
/// Returns the live client connection and the server thread's handle (join it
/// with [`joined`]).
fn connect_pair_over_listener() -> (Connection<TcpTransport>, ServerHandle) {
    let listener = DistListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    let server = thread::spawn(move || {
        let transport = listener.accept()?;
        Connection::server(transport, SERVER_NAME, COOKIE, SERVER_CREATION)
    });
    let client = Connection::client(
        TcpTransport::connect(&addr.to_string()).expect("connect to server"),
        CLIENT_NAME,
        COOKIE,
    )
    .expect("client handshake");
    (client, server)
}

#[test]
fn the_listener_adapter_binds_accepts_and_into_inner_round_trips() {
    let listener = DistListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");
    assert_ne!(addr.port(), 0, "port 0 must resolve to a real, free port");

    // `into_inner` hands back the standard listener unchanged.
    let std_listener = listener.into_inner();
    assert_eq!(std_listener.local_addr().expect("local_addr"), addr);
}

#[test]
fn a_nonblocking_listener_reports_would_block_instead_of_hanging() {
    let listener = DistListener::bind("127.0.0.1:0").expect("bind loopback");
    listener.set_nonblocking(true).expect("set_nonblocking(true)");

    // No pending connection ⇒ the accept must fail fast rather than block.
    match listener.accept() {
        Err(BeamError::Io(err)) => assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock),
        other => panic!("expected WouldBlock, got {other:?}"),
    }

    listener.set_nonblocking(false).expect("set_nonblocking(false)");
}

/// The typed send helpers put the opcode and its arguments in the
/// [`Dop::arity`] order the C receive path validates (`dist.c`), and the peer
/// decodes every one of them.
#[test]
fn the_typed_send_helpers_round_trip_over_the_listener() {
    let (mut client, server) = connect_pair_over_listener();
    let mut server = joined(server);

    assert_eq!(client.peer_name(), SERVER_NAME);
    assert_eq!(server.peer_name(), CLIENT_NAME);

    let mut atoms = AtomTable::new();
    let mut client_heap = Heap::new(8192).expect("client heap");
    let mut server_heap = Heap::new(8192).expect("server heap");

    let from = make_atom(atoms.intern("from"));
    let to = make_atom(atoms.intern("to"));
    let cookie = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern("registered"));
    let reason = make_atom(atoms.intern("normal"));
    let reference = make_small(17);
    let message = client_heap
        .tuple(&[make_atom(atoms.intern("hello")), make_small(42)])
        .expect("build the message tuple");

    // `{DOP_REG_SEND, From, Cookie, ToName} -- Message` — arity 3 *plus* the
    // trailing message term (`dist.c:2468`).
    client
        .send_reg_send(from, cookie, to_name, message, &atoms)
        .expect("send_reg_send");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv DOP_REG_SEND");
    assert_eq!(got.dop, Dop::RegSend);
    assert_eq!(got.terms.len(), 4, "From, Cookie, ToName and the message");
    assert_eq!(&got.terms[..3], &[from, cookie, to_name]);
    assert_eq!(body_image(got.terms[3], &atoms), body_image(message, &atoms));

    // `{DOP_LINK, From, To}` — arity 2.
    client.send_link(from, to, &atoms).expect("send_link");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv DOP_LINK");
    assert_eq!(got.dop, Dop::Link);
    assert_eq!(got.terms, vec![from, to]);

    // `{DOP_EXIT, From, To, Reason}` — arity 3.
    client.send_exit(from, to, reason, &atoms).expect("send_exit");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv DOP_EXIT");
    assert_eq!(got.dop, Dop::Exit);
    assert_eq!(got.terms, vec![from, to, reason]);

    // `{DOP_MONITOR_P, Remote, Local, Ref}` — arity 3.
    client.send_monitor_p(from, to, reference, &atoms).expect("send_monitor_p");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv DOP_MONITOR_P");
    assert_eq!(got.dop, Dop::MonitorP);
    assert_eq!(got.terms, vec![from, to, reference]);

    // `{DOP_DEMONITOR_P, Remote, Local, Ref}` — arity 3.
    client.send_demonitor_p(from, to, reference, &atoms).expect("send_demonitor_p");
    let got = server.recv(&mut server_heap, &mut atoms).expect("recv DOP_DEMONITOR_P");
    assert_eq!(got.dop, Dop::DemonitorP);
    assert_eq!(got.terms, vec![from, to, reference]);
}

// ---------------------------------------------------------------------------
// (i) The handshake's ERROR paths, over a real loopback socket.
//
// A refusing acceptor must *tell the peer* before dropping the link, otherwise
// the connecting node blocks forever waiting for a status frame that never comes
// (`dist_util.erl:286` replies `not_allowed`). The client's finite read timeout
// ([`TEST_TIMEOUT`]) turns a missing notification into a failed assertion rather
// than a hung suite.
// ---------------------------------------------------------------------------

/// Sends a raw 2-byte-framed handshake message body to `transport` and flushes.
fn send_handshake_body(transport: &mut TcpTransport, body: &[u8]) {
    write_packet(transport, PacketHeaderSize::Two, body).expect("write the handshake frame");
    transport.flush().expect("flush the handshake frame");
}

/// Encodes a handshake message body through its own codec.
fn encode_body<F>(encode: F) -> Vec<u8>
where
    F: FnOnce(&mut MemoryTransport) -> beam_common::Result<()>,
{
    let mut sink = MemoryTransport::new();
    encode(&mut sink).expect("encode a valid handshake body");
    sink.into_written()
}

#[test]
fn a_refusing_acceptor_notifies_the_peer_instead_of_hanging_it() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");

    // The acceptor: a `Name` whose flag word is empty fails the mandatory check
    // (the v6 version gate), so it must reply `not_allowed` and then error.
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        accept_server(&mut transport, SERVER_NAME, COOKIE, SERVER_CREATION)
    });

    let mut client = TcpTransport::connect(&addr.to_string()).expect("connect to server");
    client.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
    send_handshake_body(
        &mut client,
        &encode_body(|w| {
            Name { flags: DistFlags::EMPTY, creation: 0, name: CLIENT_NAME.to_vec() }.encode(w)
        }),
    );

    // The notification must arrive promptly — a timeout here is the regression.
    let frame = read_packet(&mut client, PacketHeaderSize::Two)
        .expect("the refused peer must receive a status frame, not block");
    assert_eq!(frame, b"snot_allowed");
    let status = Status::decode(&frame).expect("decode the refusal status");
    assert_eq!(status.kind(), StatusKind::NotAllowed);
    assert_eq!(status.error_kind(), Some(StatusKind::NotAllowed));

    let outcome = server.join().expect("server thread");
    assert!(
        matches!(outcome, Err(BeamError::BadArg)),
        "the acceptor must refuse the empty flag word with BadArg"
    );
}

#[test]
fn a_bogus_handshake_tag_is_rejected_without_a_hang() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        accept_server(&mut transport, SERVER_NAME, COOKIE, SERVER_CREATION)
    });

    let mut client = TcpTransport::connect(&addr.to_string()).expect("connect to server");
    client.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
    // A first frame that is not a `Name`: the acceptor drops the link, matching
    // `dist_util.erl:749-750` (`?shutdown2({no_node, Socket}, …)`).
    send_handshake_body(&mut client, b"not-a-name");

    let outcome = server.join().expect("server thread");
    assert!(
        matches!(outcome, Err(BeamError::BadArg)),
        "a bogus tag must be a typed error, not a hang"
    );
}

#[test]
fn a_connecting_client_surfaces_a_peer_status_refusal() {
    // The peer reads the client's `Name` then replies with a refusal status; the
    // client's typed error must carry the shared mapping's `io::ErrorKind`
    // (`beam_dist::handshake::status_error`), the same the non-blocking driver
    // produces, and must not hang waiting for a further frame.
    let cases = [
        (Status::nok(), std::io::ErrorKind::ConnectionRefused),
        (Status::not_allowed(), std::io::ErrorKind::PermissionDenied),
        (Status::Error(b"alive".to_vec()), std::io::ErrorKind::ConnectionAborted),
        (
            Status::Named { name: b"taken@host".to_vec(), creation: 5 },
            std::io::ErrorKind::AlreadyExists,
        ),
        (Status::Simultaneous, std::io::ErrorKind::ConnectionAborted),
        (Status::Error(b"weird".to_vec()), std::io::ErrorKind::InvalidData),
    ];

    for (status, expected) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        let status_for_peer = status.clone();

        let peer = thread::spawn(move || -> beam_common::Result<()> {
            let (stream, _) = listener.accept().map_err(BeamError::Io)?;
            let mut transport = TcpTransport::from_stream(stream);
            transport.set_read_timeout(Some(TEST_TIMEOUT))?;
            // Consume the client's `Name`, then answer with the refusal status.
            let _name = read_packet(&mut transport, PacketHeaderSize::Two)?;
            let body = encode_body(|w| status_for_peer.encode(w));
            send_handshake_body(&mut transport, &body);
            Ok(())
        });

        let mut client = TcpTransport::connect(&addr.to_string()).expect("connect to server");
        client.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
        let err = connect_client(&mut client, CLIENT_NAME, COOKIE)
            .expect_err("a non-ok status must refuse the client");
        match err {
            BeamError::Io(io) => assert_eq!(io.kind(), expected, "refusal status {status:?}"),
            other => panic!("expected an Io error for {status:?}, got {other:?}"),
        }

        peer.join().expect("peer thread").expect("the peer wrote its refusal");
    }
}// ---------------------------------------------------------------------------
// (j) `epmd not_registered`: `connect_to_node` surfaces the lookup failure.
// ---------------------------------------------------------------------------

/// A node name that cannot be registered with `epmd` in this run (unique per
/// process and per attempt), so the `not_registered` path is exercised.
fn unregistered_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("t2a1185-nobody-{}-{nanos}", std::process::id())
}

#[test]
fn connect_to_a_node_not_registered_with_epmd_reports_not_found() {
    let name = unregistered_name();

    // The `epmd` client is owned by `src/epmd.rs`; what this crate's connect path
    // relies on is that `lookup_port` maps a `PORT2_RESP` error result to
    // `Io(NotFound)` — never a panic and never a silent port `0`.
    match beam_dist::epmd::lookup_port(name.as_bytes()) {
        Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
        // No reachable `epmd` in this sandbox: skip loudly, never a faked pass.
        Err(BeamError::Io(err)) => {
            eprintln!("skipping: no reachable epmd ({err}); not_registered untested");
            return;
        }
        Ok(port) => panic!("the freshly generated name {name:?} must not be registered ({port})"),
        Err(other) => panic!("unexpected lookup error: {other:?}"),
    }

    // `connect_to_node` returns that very error, before opening any socket.
    match connect_to_node("127.0.0.1", name.as_bytes(), CLIENT_NAME, COOKIE) {
        Err(BeamError::Io(err)) => assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "connect_to_node must surface the epmd not_registered mapping"
        ),
        Ok(_) => panic!("a node that is not registered must not connect"),
        Err(other) => panic!("unexpected connect error: {other:?}"),
    }
}

#[test]
fn an_epmd_error_reply_decodes_to_the_not_registered_path() {
    // Hermetic companion to the live test above: `epmd` answers an unknown name
    // with a `PORT2_RESP` carrying a non-zero result byte, which the codec decodes
    // to `Port2Resp::Error` — the exact value `epmd::lookup_port` (and therefore
    // `connect_to_node`) turns into `Io(NotFound)`. The reply bytes come from the
    // `epmd` codec itself, so no live `epmd` is needed.
    let mut sink = MemoryTransport::new();
    Port2Resp::Error.encode(&mut sink).expect("encode the failure reply");
    let mut client = EpmdClient::new(MemoryTransport::with_input(sink.into_written()));
    assert_eq!(client.port_please(b"nobody").expect("decode the reply"), Port2Resp::Error);
}

// ---------------------------------------------------------------------------
// (k) The acceptor's accept policy (`accept_server_with_decision` /
//     `Connection::server_with_decision`).
//
// The default acceptors proceed on every peer that passes the mandatory check —
// `dist_util.erl:326`'s `send_status(HSData, ok)` — and a caller that owns a
// net-kernel-style pending-connection registry can refuse one. A refusal is
// notified to the peer *before* the error is returned (`dist_util.erl:286`,`:343`,
// `:386`), so the peer reads a status frame instead of blocking for one that
// never comes. Every test below is hermetic: a loopback socket, no `erl`.
// ---------------------------------------------------------------------------

/// Spawns an **acceptor that applies `decide`** over one accepted connection.
fn spawn_deciding_server(
    listener: TcpListener,
    decide: impl FnOnce(&Name) -> AcceptDecision + Send + 'static,
) -> ServerHandle {
    thread::spawn(move || {
        let (stream, _) = listener.accept().map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        Connection::server_with_decision(
            transport,
            SERVER_NAME,
            COOKIE,
            SERVER_CREATION,
            preferred_flags(),
            decide,
        )
    })
}

/// Connects to `addr`, runs the **client** handshake through a [`SpyTransport`]
/// and returns the recorded read blocks alongside the outcome — so a test can pin
/// the acceptor's status frame *and* observe the connection (or typed error) the
/// client ended up with.
fn spy_client_handshake(
    addr: &SocketAddr,
) -> (WireLog, beam_common::Result<Connection<SpyTransport>>) {
    let reads = WireLog::new();
    let mut transport = TcpTransport::connect(&addr.to_string()).expect("connect to server");
    transport.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
    let spy = SpyTransport { inner: transport, reads: reads.clone(), writes: WireLog::new() };
    let outcome = Connection::client(spy, CLIENT_NAME, COOKIE);
    (reads, outcome)
}

#[test]
fn the_default_acceptor_answers_exactly_ok_and_completes() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    let server = spawn_server(listener);

    let (reads, outcome) = spy_client_handshake(&addr);
    let client = outcome.expect("client handshake");
    let server = joined(server);

    // A spied client lets the test pin the acceptor's status frame, which is the
    // only frame whose body is exactly `sok`.
    assert!(reads.contains(b"sok"), "the default acceptor's status frame is exactly `sok`");
    assert!(
        !reads.contains(b"sok_simultaneous"),
        "the default acceptor must not claim a simultaneous connect"
    );
    assert_eq!(client.peer_name(), SERVER_NAME);
    assert_eq!(server.peer_name(), CLIENT_NAME);
    assert_eq!(client.negotiated(), server.negotiated());
}

#[test]
fn an_ok_accept_decision_is_byte_identical_to_the_default() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    let server = spawn_deciding_server(listener, |name| {
        // The decision sees the peer's decoded `Name`, after the mandatory check.
        assert_eq!(name.name, CLIENT_NAME, "the policy is consulted with the peer's Name");
        assert_eq!(name.creation, CLIENT_CREATION);
        AcceptDecision::Ok
    });

    let (reads, outcome) = spy_client_handshake(&addr);
    let client = outcome.expect("client handshake");
    let server = joined(server);

    // `AcceptDecision::Ok` proceeds with the byte-identical `"ok"` status frame.
    assert!(reads.contains(b"sok"), "an Ok decision answers exactly `sok`");
    assert_eq!(server.peer_name(), CLIENT_NAME);
    assert_eq!(client.peer_name(), SERVER_NAME);
}

#[test]
fn a_refusing_accept_decision_notifies_the_peer_and_aborts_the_client() {
    use std::io::ErrorKind;

    // The statuses a real accepting node sends before dropping a peer it will not
    // accept, and the `io::ErrorKind` the shared mapping gives the connecting side
    // (`beam_dist::handshake::status_error`).
    let cases = [
        (Status::alive(), &b"salive"[..], ErrorKind::ConnectionAborted),
        (Status::nok(), &b"snok"[..], ErrorKind::ConnectionRefused),
    ];

    for (status, wire, expected) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");

        let refused = status.clone();
        let server = spawn_deciding_server(listener, move |name| {
            assert_eq!(name.name, CLIENT_NAME, "the policy sees the peer's Name");
            AcceptDecision::Refuse(refused.clone())
        });

        let (reads, outcome) = spy_client_handshake(&addr);

        // The connecting client surfaces the shared mapping's typed error...
        match outcome {
            Err(BeamError::Io(io)) => assert_eq!(io.kind(), expected, "refusal {status:?}"),
            Err(other) => panic!("expected an Io error for {status:?}, got {other:?}"),
            Ok(_) => panic!("a refused handshake must not produce a connection ({status:?})"),
        }
        // ...and the refusal really reached it — 2-byte-framed and flushed *before*
        // the acceptor returned its error, so the peer never blocks.
        assert!(reads.contains(wire), "the refusal body {wire:?} must reach the peer");
        assert!(reads.contains(&[0, wire.len() as u8]), "the refusal is 2-byte length framed");
        assert_eq!(
            Status::decode(wire).expect("decode the refusal").kind(),
            status.kind(),
            "the wire text must classify as the refused status"
        );

        let outcome = server.join().expect("server thread");
        match outcome {
            Err(BeamError::Io(io)) => {
                assert_eq!(io.kind(), expected, "the acceptor's own error for {status:?}")
            }
            Err(other) => panic!(
                "the acceptor must refuse {status:?} with the shared Io mapping: {other:?}"
            ),
            Ok(_) => panic!("the acceptor must refuse {status:?}, not accept"),
        }
    }
}

#[test]
fn a_peer_failing_the_mandatory_check_never_reaches_the_decision() {
    // The mandatory check still runs first (`dist_util.erl:286`), so a `Name` whose
    // flag word is empty is refused as `not_allowed` and the accept policy is *not*
    // consulted — the ordering the seam promises.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");

    let consulted = Arc::new(Mutex::new(false));
    let consulted_in_server = consulted.clone();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(TEST_TIMEOUT))?;
        accept_server_with_decision(
            &mut transport,
            SERVER_NAME,
            COOKIE,
            SERVER_CREATION,
            preferred_flags(),
            move |_name| {
                *consulted_in_server.lock().expect("consulted flag") = true;
                AcceptDecision::Ok
            },
        )
    });

    let mut client = TcpTransport::connect(&addr.to_string()).expect("connect to server");
    client.set_read_timeout(Some(TEST_TIMEOUT)).expect("read timeout");
    send_handshake_body(
        &mut client,
        &encode_body(|w| {
            Name { flags: DistFlags::EMPTY, creation: 0, name: CLIENT_NAME.to_vec() }.encode(w)
        }),
    );

    let frame = read_packet(&mut client, PacketHeaderSize::Two)
        .expect("the refused peer must receive a status frame, not block");
    assert_eq!(frame, b"snot_allowed", "the mandatory check runs before the decision");

    let outcome = server.join().expect("server thread");
    assert!(matches!(outcome, Err(BeamError::BadArg)), "got {outcome:?}");
    assert!(
        !*consulted.lock().expect("consulted flag"),
        "a peer that fails the mandatory check must never reach the accept policy"
    );
}