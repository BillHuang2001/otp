//! The connection-level API: the handshake driver and a live distribution link.
//!
//! Drives the connect/challenge handshake (client and server roles) and then
//! sends/receives distributed `DOP_*` control messages over a
//! [`crate::transport::Transport`], including a [`std::net::TcpStream`] adapter.
//!
//! # The wire (ground truth: `erts/emulator/beam/dist.c`,
//! `lib/kernel/src/dist_util.erl`, `lib/erl_interface/src/connect/ei_connect.c`)
//!
//! * **Framing.** Every handshake message is a 2-byte big-endian length-prefixed
//!   packet ([`PacketHeaderSize::Two`]); once the connection is established every
//!   packet is 4-byte framed ([`PacketHeaderSize::Four`]). A **zero-length packet
//!   is a tick** and must be answered with a tick ([`crate::framing`]).
//! * **Client.** `Name` → `Status` (`"ok"` proceeds, `"nok…"` is refused) →
//!   `Challenge` → `ChallengeReply { challenge: ours, digest:
//!   md5(cookie ++ decimal(peer_challenge)) }` → `ChallengeAck` (verified against
//!   *our* challenge). That completes the OTP-23+ `'N'` handshake — no `Complement`
//!   is exchanged.
//! * **Server.** `Name` → `Status::Ok` → `Challenge` → `ChallengeReply`
//!   (verified) → `ChallengeAck`. Again no `Complement`: the `'c'` message exists
//!   only for the legacy `'n'` name (`dist_util.erl:recv_complement`,
//!   `ei_connect.c:1629`), which [`crate::handshake::Name`] does not decode.
//! * The digest is `md5(cookie ++ decimal(challenge))` — cookie first — computed
//!   by [`crate::auth`]. The nonce comes from [`gen_challenge`].
//! * **Established packets** are a distribution header followed by the control
//!   body. Because `DFLAG_DIST_HDR_ATOM_CACHE`/`DFLAG_FRAGMENTS` are negotiated,
//!   every outbound message carries a [`DistHeader`]. [`Connection::send_control`]
//!   routes every atom the control body references through the connection's
//!   outbound atom cache ([`crate::atom_cache::OutboundAtomCache`]): each atom is
//!   written as `ATOM_CACHE_REF` (`82`) + its header position, and the matching
//!   [`DistHeader`] — carrying one [`crate::dist_header::AtomCacheRef`] per atom,
//!   `New` the first time the connection sends it and `Cached` afterwards — is
//!   emitted ahead of the body. A message with no atoms therefore carries the empty
//!   header ([`DistHeader::empty`] ⇒ the exact bytes `131, 68, 0`). A payload larger
//!   than [`DIST_FRAGMENT_SIZE`] is split into `'E'`/`'F'` fragment frames
//!   ([`crate::fragment`]) when `DFLAG_FRAGMENTS` is negotiated (the header tail
//!   rides in the `'E'` frame). On receive a normal `'D'` header, an `'E'`/`'F'`
//!   fragment sequence, or the pass-through form (a lone `PASS_THROUGH` byte before
//!   the term stream) is accepted; the decoded header is applied to the receive atom
//!   cache so that payload `ATOM_CACHE_REF`s resolve.
//!
//! # The message term — the real `{DOP_*, …}` tuple wire form
//!
//! A distribution *control* message is the ETF tuple `{DOP_*, …}`
//! (`dist.c:2205`: `if (is_not_tuple(arg) || arityval(*tuple) < 1 ||
//! is_not_small(tuple[1])) goto invalid_message;`), and for the `send` opcodes the
//! message is a **separate term after the control tuple** (`dist.c`:
//! `/* {DOP_SEND, From, To} -- Message */`). Because a distribution header is
//! negotiated, the tuple and every term after it are encoded **body-form** — no
//! `VERSION_MAGIC` byte (`external.c:772`, `1231`), since the header already
//! carries the single `131`. An established packet is therefore
//!
//! ```text
//! 131 'D' NumberOfAtomCacheRefs [Flags] [refs]   'h'|'i' arity 'a' DOP control-term* [message]
//! └──────────── distribution header ──────────┘  └──────── body-form tuple + terms ───────┘
//! ```
//!
//! (`'h'`/`'i'` = `SMALL_TUPLE_EXT`/`LARGE_TUPLE_EXT`, `'a'` = `SMALL_INTEGER_EXT`,
//! so the opcode is a real small-integer term.)
//!
//! [`Connection::send_control`] builds that tuple image through
//! [`crate::control::encode_control`] (and [`Connection::recv`] decodes it back
//! through [`crate::control::decode_control`]). This module keeps only the
//! distribution-header/fragment envelope; [`crate::control`] owns the
//! control-term codec. The public [`ControlMessage`] is *flattened*:
//! [`ControlMessage::terms`] holds the control arguments followed, for a `send`
//! opcode, by the trailing message term (so
//! `{DOP_REG_SEND, From, Cookie, ToName} -- Message` arrives as
//! `From + Cookie + ToName + Message`). The arity rule lives in [`crate::control`]:
//! an **exact** [`Dop::arity`] control arguments, except the variable-arity
//! `DOP_ALTACT_SIG_SEND`.
//!
//! # Errors
//!
//! Only [`beam_common::BeamError`]: malformed/short/unknown wire data is
//! [`BeamError::BadArg`], a size beyond a wire ceiling is
//! [`BeamError::SystemLimit`], and a socket failure is [`BeamError::Io`]. A
//! **clean EOF** (the peer closed the connection) is reported as
//! [`BeamError::BadArg`], matching [`crate::transport::MemoryTransport`]'s
//! "short read" semantics. A refused handshake carries the peer's status text in
//! `Io(ErrorKind::ConnectionRefused)`.

use std::net::TcpStream;
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::AtomEncoding;
use beam_common::{BeamError, Result};
use beam_term::{AtomTable, Eterm};

use crate::atom_cache::{OutboundAtomCache, ReceiveAtomCache};
use crate::auth::{gen_challenge, gen_digest, verify};
use crate::control::{decode_control, encode_control_cached, ControlMessage};
use crate::dist_header::{DistHeader, DIST_HEADER};
use crate::flags::{DistFlags, Dop, DFLAG_FRAGMENTS, DFLAG_SEND_SENDER};
use crate::fragment::{split, Assembly, FragmentAssembler, DIST_FRAGMENT_SIZE};
use crate::framing::{
    is_tick, read_packet, write_packet, write_tick, PacketHeaderSize, MAX_PACKET_BODY,
};
use crate::handshake::{
    check_mandatory, preferred_flags, Challenge, ChallengeAck, ChallengeReply, Name, Status,
};
use crate::transport::{Read, Transport, Write};

/// The default read timeout a [`TcpTransport::connect`] sets, so a stalled peer
/// can never hang a caller (or a test) forever.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The handshake packets' length-prefix width (`ei_connect.c`: every connect
/// handshake message is 2-byte framed).
const HANDSHAKE_FRAMING: PacketHeaderSize = PacketHeaderSize::Two;

/// The established connection's length-prefix width (`packet_parser.c`).
const ESTABLISHED_FRAMING: PacketHeaderSize = PacketHeaderSize::Four;

/// `PASS_THROUGH` (`erts/emulator/beam/external.c:56`): the single byte written
/// in place of a distribution header when neither the header atom cache nor
/// fragmentation was negotiated.
const PASS_THROUGH: u8 = b'p';

/// The atom-text encoding used for the distribution header and the control body.
///
/// `DFLAG_UTF8_ATOMS` is part of the mandatory flag set, so a conforming peer
/// always negotiates it; the codec's own default is the same.
const ATOM_ENCODING: AtomEncoding = AtomEncoding::Utf8;

// ---------------------------------------------------------------------------
// The `TcpStream` transport adapter.
// ---------------------------------------------------------------------------

/// A [`crate::transport::Transport`] over a [`std::net::TcpStream`].
///
/// The adapter is deliberately thin: it maps `std::io` failures onto
/// [`BeamError`] and otherwise hands the bytes straight to the framed protocol
/// layer. A **clean EOF** is reported as [`BeamError::BadArg`] (a short read, the
/// same rule as [`crate::transport::MemoryTransport`]); every other socket error
/// — including a read timeout (`WouldBlock`/`TimedOut`) — is
/// [`BeamError::Io`].
#[derive(Debug)]
pub struct TcpTransport {
    stream: TcpStream,
}

impl TcpTransport {
    /// Connects to `addr` (a `host:port` pair) and sets
    /// [`DEFAULT_READ_TIMEOUT`] plus `TCP_NODELAY` on the socket.
    ///
    /// The read timeout is what keeps a stalled or failed handshake from blocking
    /// forever; clear it with [`set_read_timeout(None)`](Self::set_read_timeout)
    /// for a deliberately blocking link.
    pub fn connect(addr: &str) -> Result<TcpTransport> {
        let stream = TcpStream::connect(addr).map_err(BeamError::Io)?;
        let mut transport = TcpTransport::from_stream(stream);
        transport.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
        transport.set_nodelay(true)?;
        Ok(transport)
    }

    /// Wraps an already-connected stream.
    ///
    /// The stream's existing settings are left untouched, so the caller decides
    /// about timeouts (see [`set_read_timeout`](Self::set_read_timeout)).
    pub fn from_stream(stream: TcpStream) -> TcpTransport {
        TcpTransport { stream }
    }

    /// Sets (or with `None` clears) the socket read timeout.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.stream.set_read_timeout(timeout).map_err(BeamError::Io)
    }

    /// Enables/disables `TCP_NODELAY`.
    pub fn set_nodelay(&mut self, nodelay: bool) -> Result<()> {
        self.stream.set_nodelay(nodelay).map_err(BeamError::Io)
    }

    /// The underlying stream.
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// Consumes the adapter, returning the stream.
    pub fn into_inner(self) -> TcpStream {
        self.stream
    }
}

impl Read for TcpTransport {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        match std::io::Read::read_exact(&mut self.stream, buf) {
            Ok(()) => Ok(()),
            // A clean EOF is a short read, exactly as `MemoryTransport` reports it.
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(BeamError::BadArg),
            Err(err) => Err(BeamError::Io(err)),
        }
    }
}

impl Write for TcpTransport {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        std::io::Write::write_all(&mut self.stream, buf).map_err(BeamError::Io)
    }

    fn flush(&mut self) -> Result<()> {
        std::io::Write::flush(&mut self.stream).map_err(BeamError::Io)
    }
}

// ---------------------------------------------------------------------------
// The handshake.
// ---------------------------------------------------------------------------

/// The outcome of a completed connect/challenge handshake.
///
/// `negotiated` is the **peer's** validated capability set
/// (its flags after [`check_mandatory`], exactly as `recv_challenge`/`recv_name`
/// compute it): it governs how this side encodes outbound messages, e.g.
/// `DFLAG_SEND_SENDER` selects `DOP_SEND_SENDER` over `DOP_SEND`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Handshake {
    /// The connecting side's advertised capability flags.
    pub client_flags: DistFlags,
    /// The accepting side's advertised capability flags.
    pub server_flags: DistFlags,
    /// The peer's validated flags (see the struct docs).
    pub negotiated: DistFlags,
    /// The peer's node creation.
    pub peer_creation: u32,
    /// The peer's node name.
    pub peer_name: Vec<u8>,
    /// A post-handshake packet body read while draining the handshake, if any.
    ///
    /// Always `None` for the OTP-23 `'N'` handshake (no `Complement` is
    /// exchanged), retained for a peer that pipelines a real packet right after
    /// the ack.
    pub buffered: Option<Vec<u8>>,
}

/// The default creation a [`connect_client`] link advertises.
///
/// This API has no local node container, so the client's own creation defaults to
/// `0`; the *peer's* creation is reported in [`Handshake::peer_creation`]. A peer
/// that negotiated `DFLAG_BIG_CREATION` may reject creation `0`, so
/// [`connect_client_with_creation`] / [`Connection::client_with_creation`] accept an
/// explicit creation instead.
pub const CLIENT_CREATION: u32 = 0;

/// Writes one 2-byte-framed handshake message and flushes.
fn send_handshake<T: Transport>(transport: &mut T, body: &[u8]) -> Result<()> {
    write_packet(transport, HANDSHAKE_FRAMING, body)?;
    transport.flush()
}

/// Encodes a handshake message body through its own codec.
fn encode_handshake<F>(encode: F) -> Result<Vec<u8>>
where
    F: FnOnce(&mut crate::transport::MemoryTransport) -> Result<()>,
{
    // `transport.rs` has no `Write` impl for `Vec<u8>`; `MemoryTransport` is the
    // crate's in-memory byte sink, so it doubles as the scratch buffer here.
    let mut sink = crate::transport::MemoryTransport::new();
    encode(&mut sink)?;
    Ok(sink.into_written())
}

/// Builds the error for a handshake the peer refused (`"nok…"` / `"snamed:"` /
/// `"sok_simultaneous"`), carrying the status text.
fn status_error(status: &Status) -> BeamError {
    match status {
        Status::Error(text) => BeamError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!(
                "distribution handshake refused by peer: {}",
                String::from_utf8_lossy(text)
            ),
        )),
        other => BeamError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("distribution handshake: unexpected status {other:?}"),
        )),
    }
}

/// Drives the **client** side of the connect/challenge handshake.
///
/// `our_name` and `cookie` are this node's name and its shared cookie;
/// `t` carries the 2-byte-framed handshake messages. On success the link is
/// established: the peer's `Complement` (if any) has been consumed and any extra
/// frame is returned in [`Handshake::buffered`].
///
/// The client advertises [`CLIENT_CREATION`] as its own creation; use
/// [`connect_client_with_creation`] to advertise an explicit one.
pub fn connect_client<T: Transport>(t: &mut T, our_name: &[u8], cookie: &[u8]) -> Result<Handshake> {
    connect_client_with(t, our_name, cookie, gen_challenge())
}

/// [`connect_client`] advertising an explicit node `creation`.
///
/// A peer with `DFLAG_BIG_CREATION` may reject [`CLIENT_CREATION`] (`0`); this entry
/// point sends the given `creation` in the `Name` frame instead. Everything else is
/// identical to [`connect_client`].
pub fn connect_client_with_creation<T: Transport>(
    t: &mut T,
    our_name: &[u8],
    cookie: &[u8],
    creation: u32,
) -> Result<Handshake> {
    connect_client_core(t, our_name, cookie, gen_challenge(), creation)
}

/// [`connect_client`] with an explicit challenge nonce (the testable core).
fn connect_client_with<T: Transport>(
    t: &mut T,
    our_name: &[u8],
    cookie: &[u8],
    our_challenge: u32,
) -> Result<Handshake> {
    connect_client_core(t, our_name, cookie, our_challenge, CLIENT_CREATION)
}

/// The connect/challenge client handshake with both knobs explicit: the challenge
/// nonce (`our_challenge`) and the advertised creation (`creation`).
fn connect_client_core<T: Transport>(
    t: &mut T,
    our_name: &[u8],
    cookie: &[u8],
    our_challenge: u32,
    creation: u32,
) -> Result<Handshake> {
    let our_flags = preferred_flags();

    // 1. Name.
    let body = encode_handshake(|w| {
        Name { flags: our_flags, creation, name: our_name.to_vec() }.encode(w)
    })?;
    send_handshake(t, &body)?;

    // 2. Status (`"ok"` proceeds).
    let status = Status::decode(&read_packet(t, HANDSHAKE_FRAMING)?)?;
    if status != Status::Ok {
        return Err(status_error(&status));
    }

    // 3. Challenge: the peer's flags, nonce, creation and name.
    let challenge = Challenge::decode(&read_packet(t, HANDSHAKE_FRAMING)?)?;
    let negotiated = check_mandatory(challenge.flags)?;

    // 4. ChallengeReply: our nonce plus the digest over the *peer's* challenge.
    let body = encode_handshake(|w| {
        ChallengeReply {
            challenge: our_challenge,
            digest: gen_digest(cookie, challenge.challenge),
        }
        .encode(w)
    })?;
    send_handshake(t, &body)?;

    // 5. ChallengeAck: the peer proves it knows the cookie and our nonce.
    let ack = ChallengeAck::decode(&read_packet(t, HANDSHAKE_FRAMING)?)?;
    if !verify(cookie, our_challenge, &ack.digest) {
        return Err(BeamError::BadArg);
    }

    // No complement is exchanged after the ack. The `'c'` complement exists only
    // for the legacy `'n'` name (which carries 32-bit flags and no creation): an
    // acceptor that received `'n'` then reads the initiator's complement
    // (`dist_util.erl:recv_complement`, `ei_connect.c:1629`). This crate speaks the
    // OTP-23+ `'N'` name, whose 64-bit flags and creation already carry everything,
    // so a real `erl` node sends *nothing* here — reading a frame would block
    // (verified against a live node, `tests/live_interop.rs`).
    Ok(Handshake {
        client_flags: our_flags,
        server_flags: challenge.flags,
        negotiated,
        peer_creation: challenge.creation,
        peer_name: challenge.name,
        buffered: None,
    })
}

/// Drives the **server** side of the connect/challenge handshake.
///
/// `our_name`/`creation` identify this node and `cookie` is the shared secret.
/// The server advertises [`preferred_flags`];
/// an outbound `Complement` is sent when `DFLAG_HANDSHAKE_23` was negotiated.
pub fn accept_server<T: Transport>(
    t: &mut T,
    our_name: &[u8],
    cookie: &[u8],
    creation: u32,
) -> Result<Handshake> {
    accept_server_with(t, our_name, cookie, creation, gen_challenge())
}

/// [`accept_server`] with an explicit challenge nonce (the testable core).
fn accept_server_with<T: Transport>(
    t: &mut T,
    our_name: &[u8],
    cookie: &[u8],
    creation: u32,
    our_challenge: u32,
) -> Result<Handshake> {
    let our_flags = preferred_flags();

    // 1. Name: the client's flags, creation and name.
    let name = Name::decode(&read_packet(t, HANDSHAKE_FRAMING)?)?;
    let negotiated = check_mandatory(name.flags)?;

    // 2. Status::Ok.
    let body = encode_handshake(|w| Status::Ok.encode(w))?;
    send_handshake(t, &body)?;

    // 3. Challenge: our flags, nonce, creation and name.
    let body = encode_handshake(|w| {
        Challenge {
            flags: our_flags,
            challenge: our_challenge,
            creation,
            name: our_name.to_vec(),
        }
        .encode(w)
    })?;
    send_handshake(t, &body)?;

    // 4. ChallengeReply: verify the client proves it knows the cookie and our nonce.
    let reply = ChallengeReply::decode(&read_packet(t, HANDSHAKE_FRAMING)?)?;
    if !verify(cookie, our_challenge, &reply.digest) {
        return Err(BeamError::BadArg);
    }

    // 5. ChallengeAck: prove *we* know the cookie and the client's nonce.
    let body = encode_handshake(|w| {
        ChallengeAck { digest: gen_digest(cookie, reply.challenge) }.encode(w)
    })?;
    send_handshake(t, &body)?;

    // No complement is sent: it is only used to answer the legacy `'n'` name (see
    // `connect_client_with`), and `crate::handshake::Name` only decodes `'N'`.
    Ok(Handshake {
        client_flags: name.flags,
        server_flags: our_flags,
        negotiated,
        peer_creation: name.creation,
        peer_name: name.name,
        buffered: None,
    })
}

// ---------------------------------------------------------------------------
// The established connection.
// ---------------------------------------------------------------------------

/// An established distribution link: the handshake is complete and control
/// messages flow with 4-byte framing and a distribution header.
///
/// Construct one with [`Connection::client`] / [`Connection::server`]; both
/// perform the handshake on the supplied transport.
pub struct Connection<T: Transport> {
    /// The framed byte transport.
    transport: T,
    /// The negotiated capabilities and peer identity.
    handshake: Handshake,
    /// Reassembles `'E'`/`'F'` fragment sequences.
    fragments: FragmentAssembler,
    /// Applies inbound distribution headers so payload atom-cache refs resolve.
    recv_cache: ReceiveAtomCache,
    /// Classifies outbound atoms and yields the distribution header's refs, so
    /// payload atoms are written as `ATOM_CACHE_REF` (`82`).
    outbound: OutboundAtomCache,
    /// The next fragment sequence id (never `0`: a real ERTS receiver reads `0`
    /// as "not fragmented").
    next_seq_id: u64,
    /// A packet body read ahead of the protocol (the handshake's `buffered` frame).
    pending: Option<Vec<u8>>,
    /// How many ticks this link has answered (a diagnostic).
    ticks_answered: u64,
}

impl<T: Transport> Connection<T> {
    /// Connects to a peer and completes the handshake (client role).
    ///
    /// The client advertises [`CLIENT_CREATION`] as its own creation; use
    /// [`Connection::client_with_creation`] to advertise an explicit one.
    pub fn client(transport: T, name: &[u8], cookie: &[u8]) -> Result<Connection<T>> {
        Connection::client_with_creation(transport, name, cookie, CLIENT_CREATION)
    }

    /// [`Connection::client`] advertising an explicit node `creation`.
    ///
    /// A peer with `DFLAG_BIG_CREATION` may reject [`CLIENT_CREATION`] (`0`); this
    /// constructor sends `creation` in the `Name` frame instead.
    pub fn client_with_creation(
        transport: T,
        name: &[u8],
        cookie: &[u8],
        creation: u32,
    ) -> Result<Connection<T>> {
        let mut transport = transport;
        let handshake = connect_client_with_creation(&mut transport, name, cookie, creation)?;
        Ok(Connection::from_handshake(transport, handshake))
    }

    /// Accepts a peer and completes the handshake (server role).
    pub fn server(
        transport: T,
        name: &[u8],
        cookie: &[u8],
        creation: u32,
    ) -> Result<Connection<T>> {
        let mut transport = transport;
        let handshake = accept_server(&mut transport, name, cookie, creation)?;
        Ok(Connection::from_handshake(transport, handshake))
    }

    /// Builds a connection over a finished handshake.
    fn from_handshake(transport: T, mut handshake: Handshake) -> Connection<T> {
        let pending = handshake.buffered.take();
        Connection {
            transport,
            handshake,
            fragments: FragmentAssembler::new(),
            recv_cache: ReceiveAtomCache::new(),
            outbound: OutboundAtomCache::new(),
            next_seq_id: 1,
            pending,
            ticks_answered: 0,
        }
    }

    /// The negotiated flags that govern outbound encoding (the peer's validated
    /// set — see [`Handshake::negotiated`]).
    pub fn negotiated(&self) -> DistFlags {
        self.handshake.negotiated
    }

    /// The connecting side's advertised flags.
    pub fn client_flags(&self) -> DistFlags {
        self.handshake.client_flags
    }

    /// The accepting side's advertised flags.
    pub fn server_flags(&self) -> DistFlags {
        self.handshake.server_flags
    }

    /// The peer's node name.
    pub fn peer_name(&self) -> &[u8] {
        &self.handshake.peer_name
    }

    /// The peer's node creation.
    pub fn peer_creation(&self) -> u32 {
        self.handshake.peer_creation
    }

    /// The full handshake outcome.
    pub fn handshake(&self) -> &Handshake {
        &self.handshake
    }

    /// How many ticks this link has answered (a diagnostic).
    pub fn ticks_answered(&self) -> u64 {
        self.ticks_answered
    }

    /// Consumes the connection, returning the transport.
    pub fn into_inner(self) -> T {
        self.transport
    }

    /// Sends one `DOP_*` control message.
    ///
    /// The terms are exactly the control message's arguments ([`Dop::arity`] of
    /// them for the non-`send` opcodes; a `send` opcode additionally carries the
    /// message term last — see [`Connection::send_message`]). They are encoded on
    /// the wire as the real ETF tuple `{DOP_*, …}` (see the module docs).
    pub fn send_control(&mut self, dop: Dop, terms: &[Eterm], atoms: &AtomTable) -> Result<()> {
        let control = encode_control_cached(dop, terms, atoms, Some(&mut self.outbound))?;
        self.send_payload(&control, atoms)
    }

    /// Sends a message to `to` from `from`.
    ///
    /// Selects `DOP_SEND_SENDER` when the peer negotiated `DFLAG_SEND_SENDER` and
    /// `DOP_SEND` otherwise (`dist.c:1686`), with `msg` as the trailing message
    /// term after the `{DOP, From, To}` control arguments.
    pub fn send_message(
        &mut self,
        from: Eterm,
        to: Eterm,
        msg: Eterm,
        atoms: &AtomTable,
    ) -> Result<()> {
        let dop = if self.negotiated().contains(DistFlags::from_bits(DFLAG_SEND_SENDER)) {
            Dop::SendSender
        } else {
            Dop::Send
        };
        self.send_control(dop, &[from, to, msg], atoms)
    }

    /// Sends a tick (a zero-length packet).
    pub fn send_tick(&mut self) -> Result<()> {
        write_tick(&mut self.transport, ESTABLISHED_FRAMING)?;
        self.transport.flush()
    }

    /// Receives one control message, decoding its terms into `heap`.
    ///
    /// A received tick is answered with a tick and reading continues; a fragment
    /// sequence is reassembled in full; the inbound distribution header is applied
    /// to the receive atom cache before the control body is decoded, so payload
    /// `ATOM_CACHE_REF`s resolve.
    ///
    /// The returned [`ControlMessage::terms`] holds the control arguments and, for
    /// a `send` opcode, the trailing message term (see the module docs).
    pub fn recv(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<ControlMessage> {
        let body = self.read_message_body()?;
        let (header, offset) = parse_header(&body, atoms)?;
        self.recv_cache.apply(&header)?;
        let payload = body.get(offset..).ok_or(BeamError::BadArg)?;
        decode_control(payload, heap, atoms, Some(self.recv_cache.table()))
    }

    /// Records the outbound payload, prepending the distribution header and
    /// fragmenting when needed.
    ///
    /// The header is the one the outbound atom cache accumulated while the payload
    /// was encoded ([`OutboundAtomCache::take_header`] — which also starts the next
    /// message); it carries a ref for every atom the payload wrote as
    /// `ATOM_CACHE_REF` (`82`). A payload that referenced no atoms yields the empty
    /// header (`131, 68, 0`).
    fn send_payload(&mut self, control: &[u8], atoms: &AtomTable) -> Result<()> {
        let header = self.outbound.take_header();
        let mut sink = crate::transport::MemoryTransport::new();
        header.encode(&mut sink, atoms, ATOM_ENCODING)?;
        let header = sink.into_written();

        let fragment = self.negotiated().contains(DistFlags::from_bits(DFLAG_FRAGMENTS))
            && header.len().saturating_add(control.len()) > DIST_FRAGMENT_SIZE;

        if fragment {
            // The header tail (`NumberOfAtomCacheRefs + Flags + AtomCacheRefs`)
            // rides in the `'E'` frame, ahead of the first payload chunk.
            let tail = header.get(2..).ok_or(BeamError::BadArg)?;
            let seq_id = self.next_seq_id;
            self.next_seq_id = self.next_seq_id.checked_add(1).filter(|id| *id != 0).unwrap_or(1);
            for frame in split(seq_id, tail, control, DIST_FRAGMENT_SIZE)? {
                write_packet(&mut self.transport, ESTABLISHED_FRAMING, &frame)?;
            }
        } else {
            let mut body = header;
            body.extend_from_slice(control);
            write_packet(&mut self.transport, ESTABLISHED_FRAMING, &body)?;
        }
        self.transport.flush()
    }

    /// Reads the next complete (defragmented) message body.
    ///
    /// Ticks are answered transparently; a fragment sequence is buffered until its
    /// last frame arrives.
    fn read_message_body(&mut self) -> Result<Vec<u8>> {
        loop {
            let packet = match self.pending.take() {
                Some(packet) => packet,
                None => read_packet(&mut self.transport, ESTABLISHED_FRAMING)?,
            };
            if is_tick(&packet) {
                self.send_tick()?;
                self.ticks_answered += 1;
                continue;
            }
            match self.fragments.push(&packet)? {
                Assembly::NotFragmented => {
                    if self.fragments.is_pending() {
                        // Strict channel ordering: a message may not interleave a
                        // fragment sequence (`erl_ext_dist.md`).
                        return Err(BeamError::BadArg);
                    }
                    return Ok(packet);
                }
                Assembly::Pending => continue,
                Assembly::Complete(body) => return Ok(body),
            }
        }
    }
}

/// Splits a received message body into its distribution header and the offset at
/// which the control body begins.
///
/// Accepts the normal `'D'` header, the reassembled form produced by the fragment
/// assembler (also `'D'`), and the pass-through form (`external.c`: a lone
/// `PASS_THROUGH` byte, optionally followed by the term's own `VERSION_MAGIC`).
fn parse_header(body: &[u8], atoms: &mut AtomTable) -> Result<(DistHeader, usize)> {
    match body.first().copied() {
        Some(beam_codec::VERSION_MAGIC) if body.get(1).copied() == Some(DIST_HEADER) => {
            DistHeader::decode_tail(body, 2, atoms, ATOM_ENCODING)
        }
        Some(PASS_THROUGH) => {
            let offset = if body.get(1).copied() == Some(beam_codec::VERSION_MAGIC) { 2 } else { 1 };
            Ok((DistHeader::empty(), offset))
        }
        _ => Err(BeamError::BadArg),
    }
}

/// The largest body an established packet can advertise
/// (`framing::MAX_PACKET_BODY`); re-exported here so callers of this module can
/// reason about the bound without reaching into [`crate::framing`].
pub const MAX_ESTABLISHED_PACKET_BODY: usize = MAX_PACKET_BODY;

#[cfg(test)]
#[path = "connection_tests.rs"]
mod tests;
