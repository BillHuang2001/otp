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
//!   *our* challenge) → `Complement` when `DFLAG_HANDSHAKE_23` was negotiated.
//! * **Server.** `Name` → `Status::Ok` → `Challenge` → `ChallengeReply`
//!   (verified) → `ChallengeAck` → `Complement` when `DFLAG_HANDSHAKE_23` was
//!   negotiated.
//! * The digest is `md5(cookie ++ decimal(challenge))` — cookie first — computed
//!   by [`crate::auth`]. The nonce comes from [`gen_challenge`].
//! * **Established packets** are a distribution header followed by the control
//!   body. Because `DFLAG_DIST_HDR_ATOM_CACHE`/`DFLAG_FRAGMENTS` are negotiated,
//!   every outbound message carries a [`DistHeader`] — currently always the empty
//!   header ([`DistHeader::empty`] ⇒ the exact bytes `131, 68, 0`), because the
//!   *outbound* atom cache is not wired up (`beam_codec` never emits
//!   `ATOM_CACHE_REF`/`NEW_CACHE`; see [`crate::atom_cache`]). A payload larger
//!   than [`DIST_FRAGMENT_SIZE`] is split into `'E'`/`'F'` fragment frames
//!   ([`crate::fragment`]) when `DFLAG_FRAGMENTS` is negotiated. On receive a
//!   normal `'D'` header, an `'E'`/`'F'` fragment sequence, or the pass-through
//!   form (a lone `PASS_THROUGH` byte before the term stream) is accepted; the
//!   decoded header is applied to the receive atom cache so that payload
//!   `ATOM_CACHE_REF`s resolve.
//!
//! # The message term — and why the connection layer decodes the control body
//!
//! A distribution *control* message is the tuple `{DOP_*, …}` and, for the
//! `send` opcodes, the message is a **separate term after the control tuple**
//! (`dist.c`: `/* {DOP_SEND, From, To} -- Message */`). This crate's control codec
//! *flattens* that tuple into `dop_byte + term*`, so a send control arrives as
//! `dop + From + To + Message` — exactly [`Dop::arity`] control terms followed by
//! the message term. [`crate::control::decode_control`] enforces an **exact**
//! arity (its model is the control tuple alone), so it is used for the opcodes
//! that carry no message, while the send opcodes are decoded with the same
//! `beam_codec` body decoder plus the trailing term.
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
use beam_codec::{AtomCache, AtomEncoding, Decoder};
use beam_common::{BeamError, Result};
use beam_term::{AtomTable, Eterm};

use crate::atom_cache::ReceiveAtomCache;
use crate::auth::{gen_challenge, gen_digest, verify};
use crate::control::{decode_control, encode_control, ControlMessage};
use crate::dist_header::{DistHeader, DIST_HEADER};
use crate::flags::{
    DistFlags, Dop, COMPLEMENT_TAG, DFLAG_FRAGMENTS, DFLAG_HANDSHAKE_23, DFLAG_SEND_SENDER,
};
use crate::fragment::{split, Assembly, FragmentAssembler, DIST_FRAGMENT_SIZE};
use crate::framing::{
    is_tick, read_packet, write_packet, write_tick, PacketHeaderSize, MAX_PACKET_BODY,
};
use crate::handshake::{
    check_mandatory, preferred_flags, Challenge, ChallengeAck, ChallengeReply, Complement, Name,
    Status,
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
    /// The OTP-23+ handshake reads one frame after the `ChallengeAck`: normally
    /// that is the peer's `Complement` (`'c'`), but a peer that pipelines its
    /// first real packet leaves it here for [`Connection`] to deliver.
    pub buffered: Option<Vec<u8>>,
}

/// The creation a [`connect_client`] link advertises.
///
/// This API has no local node container, so the client's own creation is `0`; the
/// *peer's* creation is reported in [`Handshake::peer_creation`].
const CLIENT_CREATION: u32 = 0;

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
pub fn connect_client<T: Transport>(t: &mut T, our_name: &[u8], cookie: &[u8]) -> Result<Handshake> {
    connect_client_with(t, our_name, cookie, gen_challenge())
}

/// [`connect_client`] with an explicit challenge nonce (the testable core).
fn connect_client_with<T: Transport>(
    t: &mut T,
    our_name: &[u8],
    cookie: &[u8],
    our_challenge: u32,
) -> Result<Handshake> {
    let our_flags = preferred_flags();

    // 1. Name.
    let body = encode_handshake(|w| {
        Name { flags: our_flags, creation: CLIENT_CREATION, name: our_name.to_vec() }.encode(w)
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

    // 6. OTP 23+ complement. The peer sends it after the ack; a pipelined data
    //    packet instead is buffered for the connection.
    let mut buffered = None;
    if negotiated.contains(DistFlags::from_bits(DFLAG_HANDSHAKE_23)) {
        let frame = read_packet(t, HANDSHAKE_FRAMING)?;
        if frame.first().copied() == Some(COMPLEMENT_TAG) {
            // Validate the shape; the high flag word is not otherwise needed here.
            let _complement = Complement::decode(&frame)?;
        } else {
            buffered = Some(frame);
        }
    }

    Ok(Handshake {
        client_flags: our_flags,
        server_flags: challenge.flags,
        negotiated,
        peer_creation: challenge.creation,
        peer_name: challenge.name,
        buffered,
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

    // 6. OTP 23+ complement.
    if negotiated.contains(DistFlags::from_bits(DFLAG_HANDSHAKE_23)) {
        let body = encode_handshake(|w| {
            Complement { flags_high: (our_flags.bits() >> 32) as u32, creation }.encode(w)
        })?;
        send_handshake(t, &body)?;
    }

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
    pub fn client(transport: T, name: &[u8], cookie: &[u8]) -> Result<Connection<T>> {
        let mut transport = transport;
        let handshake = connect_client(&mut transport, name, cookie)?;
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
    /// message term last — see [`Connection::send_message`]).
    pub fn send_control(&mut self, dop: Dop, terms: &[Eterm], atoms: &AtomTable) -> Result<()> {
        let control = encode_control(dop, terms, atoms)?;
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
        decode_message_body(payload, heap, atoms, self.recv_cache.table())
    }

    /// Records the outbound payload, prepending the distribution header and
    /// fragmenting when needed.
    fn send_payload(&mut self, control: &[u8], atoms: &AtomTable) -> Result<()> {
        // Only the empty header is emitted: the outbound atom cache is not wired
        // up, so the message never carries refs.
        let mut sink = crate::transport::MemoryTransport::new();
        DistHeader::empty().encode(&mut sink, atoms, ATOM_ENCODING)?;
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

/// Whether a control opcode's message carries the message term after the control
/// tuple (`dist.c`: `/* {DOP_SEND, From, To} -- Message */`).
fn carries_message(dop: Dop) -> bool {
    matches!(
        dop,
        Dop::Send
            | Dop::SendTt
            | Dop::SendSender
            | Dop::SendSenderTt
            | Dop::RegSend
            | Dop::RegSendTt
            | Dop::AliasSend
            | Dop::AliasSendTt
            | Dop::AltactSigSend
    )
}

/// Decodes a control body (`dop` byte + body-form terms).
///
/// The opcodes that carry no message use [`decode_control`], which enforces the
/// exact arity of the control tuple. A `send` opcode is followed by the message
/// term, so its terms are decoded with the same `beam_codec` body decoder and only
/// the *minimum* arity is enforced — `decode_control` cannot be reused there
/// because it requires an exact match.
fn decode_message_body(
    payload: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    cache: &AtomCache,
) -> Result<ControlMessage> {
    let opcode = *payload.first().ok_or(BeamError::BadArg)?;
    let dop = Dop::from_u8(opcode).ok_or(BeamError::BadArg)?;
    if !carries_message(dop) {
        return decode_control(payload, heap, atoms, Some(cache));
    }

    let mut decoder = Decoder::new_body(&payload[1..]).with_atom_cache(cache);
    let mut terms = Vec::new();
    while decoder.remaining() > 0 {
        terms.push(decoder.decode(heap, atoms)?);
    }
    if terms.len() < dop.arity() {
        return Err(BeamError::BadArg);
    }
    Ok(ControlMessage { dop, terms })
}

/// The largest body an established packet can advertise
/// (`framing::MAX_PACKET_BODY`); re-exported here so callers of this module can
/// reason about the bound without reaching into [`crate::framing`].
pub const MAX_ESTABLISHED_PACKET_BODY: usize = MAX_PACKET_BODY;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MemoryTransport;
    use beam_term::prelude::{make_small, NIL};

    const COOKIE: &[u8] = b"a-test-cookie";
    const CLIENT: &[u8] = b"client@host";
    const SERVER: &[u8] = b"server@host";

    /// Frames `body` with the given length prefix.
    fn framed(body: &[u8], hdr: PacketHeaderSize) -> Vec<u8> {
        let mut sink = MemoryTransport::new();
        write_packet(&mut sink, hdr, body).expect("frame");
        sink.into_written()
    }

    /// Encodes a message body through its own codec.
    fn encoded<F>(encode: F) -> Vec<u8>
    where
        F: FnOnce(&mut MemoryTransport) -> Result<()>,
    {
        let mut sink = MemoryTransport::new();
        encode(&mut sink).expect("encode");
        sink.into_written()
    }

    /// Reads one handshake frame out of a byte reader.
    fn read_handshake_frame(reader: &mut MemoryTransport) -> Vec<u8> {
        read_packet(reader, HANDSHAKE_FRAMING).expect("read frame")
    }

    // -----------------------------------------------------------------------
    // connect_client
    // -----------------------------------------------------------------------

    /// The scripted server half of a successful client handshake.
    fn client_script(our_challenge: u32, peer_challenge: u32, creation: u32) -> Vec<u8> {
        let server_flags = preferred_flags();
        let mut input = Vec::new();
        input.extend(framed(&encoded(|w| Status::Ok.encode(w)), HANDSHAKE_FRAMING));
        input.extend(framed(
            &encoded(|w| {
                Challenge {
                    flags: server_flags,
                    challenge: peer_challenge,
                    creation,
                    name: SERVER.to_vec(),
                }
                .encode(w)
            }),
            HANDSHAKE_FRAMING,
        ));
        input.extend(framed(
            &encoded(|w| ChallengeAck { digest: gen_digest(COOKIE, our_challenge) }.encode(w)),
            HANDSHAKE_FRAMING,
        ));
        input.extend(framed(
            &encoded(|w| {
                Complement { flags_high: (server_flags.bits() >> 32) as u32, creation }.encode(w)
            }),
            HANDSHAKE_FRAMING,
        ));
        input
    }

    #[test]
    fn client_drives_the_full_handshake() {
        let our_challenge = 0x1234_5678;
        let peer_challenge = 0xdead_beef;
        let mut transport =
            MemoryTransport::with_input(client_script(our_challenge, peer_challenge, 41));

        let hs = connect_client_with(&mut transport, CLIENT, COOKIE, our_challenge)
            .expect("handshake");

        assert_eq!(hs.client_flags, preferred_flags());
        assert_eq!(hs.server_flags, preferred_flags());
        assert_eq!(hs.negotiated, check_mandatory(preferred_flags()).unwrap());
        assert_eq!(hs.peer_creation, 41);
        assert_eq!(hs.peer_name, SERVER);
        assert_eq!(hs.buffered, None);

        // The client sent Name then ChallengeReply, both 2-byte framed.
        let mut output = MemoryTransport::with_input(transport.into_written());
        assert_eq!(
            Name::decode(&read_handshake_frame(&mut output)).unwrap(),
            Name { flags: preferred_flags(), creation: CLIENT_CREATION, name: CLIENT.to_vec() }
        );
        let reply = ChallengeReply::decode(&read_handshake_frame(&mut output)).unwrap();
        assert_eq!(reply.challenge, our_challenge);
        assert_eq!(reply.digest, gen_digest(COOKIE, peer_challenge));
        assert_eq!(output.input_remaining(), 0);
    }

    #[test]
    fn client_buffers_a_post_handshake_packet() {
        let our_challenge = 0x0abc_def0;
        let mut input = client_script(our_challenge, 7, 3);
        // Replace the complement with a real (opaque) packet body.
        input.truncate(input.len() - (framed(&encoded(|w| Complement { flags_high: 0, creation: 3 }.encode(w)), HANDSHAKE_FRAMING)).len());
        input.extend(framed(&[1, 2, 3], HANDSHAKE_FRAMING));

        let mut transport = MemoryTransport::with_input(input);
        let hs = connect_client_with(&mut transport, CLIENT, COOKIE, our_challenge)
            .expect("handshake");
        assert_eq!(hs.buffered, Some(vec![1, 2, 3]));
    }

    #[test]
    fn client_rejects_a_refused_status() {
        let input = framed(&encoded(|w| Status::Error(b"nok".to_vec()).encode(w)), HANDSHAKE_FRAMING);
        let mut transport = MemoryTransport::with_input(input);
        let err = connect_client_with(&mut transport, CLIENT, COOKIE, 1).unwrap_err();
        match err {
            BeamError::Io(io) => {
                assert_eq!(io.kind(), std::io::ErrorKind::ConnectionRefused);
                assert!(io.to_string().contains("nok"), "the status text is carried: {io}");
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn client_rejects_a_bad_ack_digest() {
        let our_challenge = 0x5555_aaaa;
        let server_flags = preferred_flags();
        let mut input = Vec::new();
        input.extend(framed(&encoded(|w| Status::Ok.encode(w)), HANDSHAKE_FRAMING));
        input.extend(framed(
            &encoded(|w| {
                Challenge {
                    flags: server_flags,
                    challenge: 9,
                    creation: 2,
                    name: SERVER.to_vec(),
                }
                .encode(w)
            }),
            HANDSHAKE_FRAMING,
        ));
        input.extend(framed(
            &encoded(|w| ChallengeAck { digest: [0u8; 16] }.encode(w)),
            HANDSHAKE_FRAMING,
        ));

        let mut transport = MemoryTransport::with_input(input);
        assert!(matches!(
            connect_client_with(&mut transport, CLIENT, COOKIE, our_challenge),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn client_rejects_a_peer_missing_mandatory_flags() {
        let mut input = Vec::new();
        input.extend(framed(&encoded(|w| Status::Ok.encode(w)), HANDSHAKE_FRAMING));
        input.extend(framed(
            &encoded(|w| {
                Challenge {
                    flags: DistFlags::EMPTY,
                    challenge: 9,
                    creation: 2,
                    name: SERVER.to_vec(),
                }
                .encode(w)
            }),
            HANDSHAKE_FRAMING,
        ));
        let mut transport = MemoryTransport::with_input(input);
        assert!(matches!(
            connect_client_with(&mut transport, CLIENT, COOKIE, 1),
            Err(BeamError::BadArg)
        ));
    }

    // -----------------------------------------------------------------------
    // accept_server
    // -----------------------------------------------------------------------

    #[test]
    fn server_drives_the_full_handshake() {
        let our_challenge = 0x0bad_f00d;
        let peer_challenge = 0x0102_0304;
        let mut input = Vec::new();
        input.extend(framed(
            &encoded(|w| {
                Name { flags: preferred_flags(), creation: 9, name: CLIENT.to_vec() }.encode(w)
            }),
            HANDSHAKE_FRAMING,
        ));
        input.extend(framed(
            &encoded(|w| {
                ChallengeReply {
                    challenge: peer_challenge,
                    digest: gen_digest(COOKIE, our_challenge),
                }
                .encode(w)
            }),
            HANDSHAKE_FRAMING,
        ));

        let mut transport = MemoryTransport::with_input(input);
        let hs = accept_server_with(&mut transport, SERVER, COOKIE, 77, our_challenge)
            .expect("handshake");

        assert_eq!(hs.client_flags, preferred_flags());
        assert_eq!(hs.server_flags, preferred_flags());
        assert_eq!(hs.negotiated, check_mandatory(preferred_flags()).unwrap());
        assert_eq!(hs.peer_creation, 9);
        assert_eq!(hs.peer_name, CLIENT);
        assert_eq!(hs.buffered, None);

        // The server sent Status::Ok, Challenge, ChallengeAck and the Complement.
        let mut output = MemoryTransport::with_input(transport.into_written());
        assert_eq!(Status::decode(&read_handshake_frame(&mut output)).unwrap(), Status::Ok);
        assert_eq!(
            Challenge::decode(&read_handshake_frame(&mut output)).unwrap(),
            Challenge {
                flags: preferred_flags(),
                challenge: our_challenge,
                creation: 77,
                name: SERVER.to_vec(),
            }
        );
        assert_eq!(
            ChallengeAck::decode(&read_handshake_frame(&mut output)).unwrap().digest,
            gen_digest(COOKIE, peer_challenge)
        );
        assert_eq!(
            Complement::decode(&read_handshake_frame(&mut output)).unwrap(),
            Complement { flags_high: (preferred_flags().bits() >> 32) as u32, creation: 77 }
        );
        assert_eq!(output.input_remaining(), 0);
    }

    #[test]
    fn server_rejects_a_bad_reply_digest() {
        let mut input =
            framed(&encoded(|w| Name { flags: preferred_flags(), creation: 9, name: CLIENT.to_vec() }.encode(w)), HANDSHAKE_FRAMING);
        input.extend(framed(
            &encoded(|w| ChallengeReply { challenge: 3, digest: [0xffu8; 16] }.encode(w)),
            HANDSHAKE_FRAMING,
        ));
        let mut transport = MemoryTransport::with_input(input);
        assert!(matches!(
            accept_server_with(&mut transport, SERVER, COOKIE, 1, 42),
            Err(BeamError::BadArg)
        ));
    }

    // -----------------------------------------------------------------------
    // Established-connection framing: header, pass-through, tick, fragments.
    // -----------------------------------------------------------------------

    /// A connection over a memory transport whose input is pre-loaded, with a
    /// synthetic completed handshake.
    fn connection(input: Vec<u8>) -> Connection<MemoryTransport> {
        let flags = preferred_flags();
        Connection::from_handshake(
            MemoryTransport::with_input(input),
            Handshake {
                client_flags: flags,
                server_flags: flags,
                negotiated: flags,
                peer_creation: 0,
                peer_name: Vec::new(),
                buffered: None,
            },
        )
    }

    #[test]
    fn recv_decodes_a_normal_distribution_header_packet() {
        let atoms = AtomTable::new();
        let control = encode_control(Dop::Link, &[make_small(1), make_small(2)], &atoms).unwrap();
        let mut body = vec![beam_codec::VERSION_MAGIC, DIST_HEADER, 0];
        body.extend_from_slice(&control);

        let mut conn = connection(framed(&body, ESTABLISHED_FRAMING));
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let msg = conn.recv(&mut heap, &mut atoms).unwrap();
        assert_eq!(msg.dop, Dop::Link);
        assert_eq!(msg.terms, vec![make_small(1), make_small(2)]);
    }

    #[test]
    fn recv_accepts_the_pass_through_form() {
        let atoms = AtomTable::new();
        let control = encode_control(Dop::Link, &[make_small(3), make_small(4)], &atoms).unwrap();
        let mut body = vec![PASS_THROUGH];
        body.extend_from_slice(&control);

        let mut conn = connection(framed(&body, ESTABLISHED_FRAMING));
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let msg = conn.recv(&mut heap, &mut atoms).unwrap();
        assert_eq!(msg.dop, Dop::Link);
        assert_eq!(msg.terms, vec![make_small(3), make_small(4)]);
    }

    #[test]
    fn recv_answers_a_tick_and_delivers_the_next_message() {
        let atoms = AtomTable::new();
        let control = encode_control(Dop::Link, &[make_small(5), make_small(6)], &atoms).unwrap();
        let mut body = vec![beam_codec::VERSION_MAGIC, DIST_HEADER, 0];
        body.extend_from_slice(&control);

        let mut input = framed(&[], ESTABLISHED_FRAMING); // the tick
        input.extend(framed(&body, ESTABLISHED_FRAMING));
        let mut conn = connection(input);

        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let msg = conn.recv(&mut heap, &mut atoms).unwrap();
        assert_eq!(msg.terms, vec![make_small(5), make_small(6)]);
        assert_eq!(conn.ticks_answered(), 1);

        // The answer is a zero-length packet.
        let mut written = MemoryTransport::with_input(conn.into_inner().into_written());
        assert!(is_tick(&read_packet(&mut written, ESTABLISHED_FRAMING).unwrap()));
        assert_eq!(written.input_remaining(), 0);
    }

    #[test]
    fn recv_reassembles_a_fragment_sequence() {
        let atoms = AtomTable::new();
        let control = encode_control(Dop::Link, &[make_small(7), make_small(8)], &atoms).unwrap();
        let frames = split(3, &[0], &control, 4).unwrap();
        assert!(frames.len() > 1, "the payload must span fragments");

        let mut input = Vec::new();
        for frame in &frames {
            input.extend(framed(frame, ESTABLISHED_FRAMING));
        }
        let mut conn = connection(input);
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        let msg = conn.recv(&mut heap, &mut atoms).unwrap();
        assert_eq!(msg.dop, Dop::Link);
        assert_eq!(msg.terms, vec![make_small(7), make_small(8)]);
    }

    #[test]
    fn recv_rejects_a_message_interleaved_with_a_fragment_sequence() {
        let atoms = AtomTable::new();
        let control = encode_control(Dop::Link, &[make_small(1), make_small(2)], &atoms).unwrap();
        let frames = split(4, &[0], &control, 4).unwrap();

        let mut input = framed(&frames[0], ESTABLISHED_FRAMING);
        let mut plain = vec![beam_codec::VERSION_MAGIC, DIST_HEADER, 0];
        plain.extend_from_slice(&control);
        input.extend(framed(&plain, ESTABLISHED_FRAMING));

        let mut conn = connection(input);
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
    }

    #[test]
    fn recv_rejects_an_unknown_message_shape() {
        let mut conn = connection(framed(&[0x00, 0x00], ESTABLISHED_FRAMING));
        let mut heap = Heap::new(64).unwrap();
        let mut atoms = AtomTable::new();
        assert!(matches!(conn.recv(&mut heap, &mut atoms), Err(BeamError::BadArg)));
    }

    #[test]
    fn send_message_selects_the_negotiated_send_opcode() {
        let atoms = AtomTable::new();
        let mut conn = connection(Vec::new());
        conn.send_message(make_small(1), make_small(2), NIL, &atoms).unwrap();

        let mut written = MemoryTransport::with_input(conn.into_inner().into_written());
        let body = read_packet(&mut written, ESTABLISHED_FRAMING).unwrap();
        assert_eq!(&body[..3], &[beam_codec::VERSION_MAGIC, DIST_HEADER, 0]);
        assert_eq!(body[3], Dop::SendSender.as_u8());
    }
}
