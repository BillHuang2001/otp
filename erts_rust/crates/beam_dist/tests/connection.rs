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

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_dist::connection::{Connection, TcpTransport};
use beam_dist::dist_header::DIST_FRAG_HEADER;
use beam_dist::flags::Dop;
use beam_dist::fragment::DIST_FRAGMENT_SIZE;
use beam_dist::transport::{Read, Write};
use beam_dist::{check_mandatory, preferred_flags};
use beam_term::prelude::{make_atom, make_small, Eterm, NIL};
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

    /// Whether any recorded block is a `'E'` fragment-header frame body
    /// (`VERSION_MAGIC + DIST_FRAG_HEADER`), i.e. an actual fragment was exchanged.
    fn saw_fragment_header(&self) -> bool {
        self.0.lock().expect("wire log").iter().any(|b| {
            b.len() >= 2 && b[0] == beam_codec::VERSION_MAGIC && b[1] == DIST_FRAG_HEADER
        })
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
