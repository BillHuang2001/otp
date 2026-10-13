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

//! A **non-blocking, byte-fed, driven** distribution-connection state machine.
//!
//! The blocking [`crate::connection::Connection`] owns a
//! [`crate::transport::Transport`] and performs every read/write itself. That is
//! the right shape for one link on one thread, but a runtime that multiplexes
//! many links on a single event loop (e.g. `beam_rt` over `beam_io`'s
//! `epoll`/`mio` poller) must own the socket itself. [`ConnectionDriver`] is that
//! shape: it is **transport-agnostic and byte-fed** — the runtime reads bytes
//! from its socket and hands them over with [`ConnectionDriver::feed`], calls
//! [`ConnectionDriver::step`] to make all possible non-blocking progress, drains
//! the bytes it must write with [`ConnectionDriver::take_output`], and pulls the
//! decoded control messages with [`ConnectionDriver::next_message`]. It never
//! opens a socket.
//!
//! # Driving contract
//!
//! ```text
//! loop {
//!     let n = read(socket, &mut buf)?;         // the runtime's own I/O
//!     if n == 0 { driver.feed_eof(); }         // peer closed
//!     else      { driver.feed(&buf[..n]); }
//!
//!     let step_result = loop {
//!         match driver.step() {
//!             Ok(StepOutcome::WouldBlock) => break Ok(()),
//!             Ok(_) => {}
//!             Err(e) => break Err(e),
//!         }
//!     };
//!     // A refusing acceptor buffers its `not_allowed` status *before* returning
//!     // the error, so drain (and write) the output even on error.
//!     let out = driver.take_output();
//!     if !out.is_empty() { write(socket, &out)?; }
//!     step_result?;
//!
//!     while let Some(msg) = driver.next_message(&mut heap, &mut atoms)? {
//!         /* handle the DOP_* message */
//!     }
//!     match driver.state() { DriverState::Closed | DriverState::Failed => drop(driver, ), _ => {} }
//! }
//! ```
//!
//! [`ConnectionDriver::step`] drives the **incremental 2-byte-framed handshake**
//! first (client or server role, chosen at construction), then, once established,
//! parses **4-byte-framed packets**, answers ticks, reassembles `'E'`/`'F'`
//! fragments, and queues the payload bodies. [`ConnectionDriver::next_message`]
//! decodes the next queued body.
//!
//! # Reuse, not re-implementation
//!
//! Every byte of the wire is produced by the crate's existing codecs: the
//! handshake messages ([`crate::handshake`]), the MD5 challenge/response
//! ([`crate::auth`]), the packet-length framing ([`crate::framing`]), the
//! distribution header and fragment headers ([`crate::dist_header`]), message
//! fragmentation ([`crate::fragment`]), the atom caches ([`crate::atom_cache`])
//! and the control-message tuple ([`crate::control`]). The driver adds only the
//! *state machine* that stitches them together without blocking.
//!
//! # A note on where the distribution header is parsed
//!
//! [`ConnectionDriver::step`] deliberately has **no** [`beam_term::AtomTable`]
//! parameter (it must stay usable from a runtime loop that owns no interpreter
//! state), and decoding a distribution header interns the header's `NewAtomCacheRef`
//! atom texts — which must land in the *same* atom table the payload is later
//! decoded against (otherwise a payload `ATOM_CACHE_REF` could collide with an
//! inline atom's index and resolve to the wrong name). The header is therefore
//! parsed and applied to the receive atom cache in
//! [`ConnectionDriver::next_message`], which *does* receive the atom table, using
//! the exact rule [`crate::connection`] documents (normal `'D'`, the reassembled
//! fragment form, or the `PASS_THROUGH` form). `step` still performs a cheap
//! structural check of each packet's leading bytes so a garbage frame is rejected
//! immediately rather than queued.
//!
//! # Errors
//!
//! Every fallible method returns [`beam_common::Result`]. Malformed, truncated or
//! unknown wire data is [`BeamError::BadArg`]; the decoder never `unwrap()`s on
//! wire data and never panics. A protocol error is **fatal** for the link: the
//! driver moves to [`DriverState::Failed`] and further calls to `step` report
//! [`StepOutcome::Failed`] instead of resuming, so a handshake error can never
//! hang the caller. A peer's non-`Ok` [`Status`] refusal is classified through the
//! single mapping shared with the blocking API
//! ([`crate::handshake::status_error`]) — a `"nok"`/`"not_allowed"` refusal, an
//! `"alive"` peer and a `"named:"` name collision each surface the same typed `Io`
//! error (and `io::ErrorKind`) [`crate::connection`] would return.
//!
//! Conversely, a driver playing the **accepting** role that must refuse a peer
//! (a `Name` whose flags fail the mandatory check) writes the `"not_allowed"`
//! status into its output **before** returning the error: the framed bytes are
//! buffered, so [`ConnectionDriver::take_output`] still yields them after `step`
//! returned `Err` — the driver notifies the peer exactly as
//! [`crate::connection::accept_server`] does, so the rejected peer cannot block.

use std::collections::VecDeque;

use beam_alloc::Heap;
use beam_codec::AtomEncoding;
use beam_common::{BeamError, Result};
use beam_term::{AtomTable, Eterm};

use crate::atom_cache::{OutboundAtomCache, ReceiveAtomCache};
use crate::auth::{gen_challenge, gen_digest, verify};
use crate::connection::Handshake;
use crate::control::{decode_control, encode_control_cached, ControlMessage};
use crate::delivery::{Delivery, Dispatched, InboundSink};
use crate::dist_header::{DistHeader, DIST_HEADER};
use crate::flags::{DistFlags, Dop, DFLAG_FRAGMENTS, DFLAG_SEND_SENDER};
use crate::fragment::{split, Assembly, FragmentAssembler, DIST_FRAGMENT_SIZE};
use crate::framing::{
    is_tick, read_length, read_packet, write_packet, write_tick, PacketHeaderSize,
};
use crate::handshake::{
    check_mandatory, preferred_flags, publish_type, status_error, AcceptDecision, Challenge,
    ChallengeAck, ChallengeReply, Name, PeerVisibility, Status,
};
use crate::transport::{MemoryTransport, Read, Write};

/// The handshake packets' length-prefix width (`ei_connect.c`: every connect
/// handshake message is 2-byte framed).
const HANDSHAKE_FRAMING: PacketHeaderSize = PacketHeaderSize::Two;

/// The established link's length-prefix width (`packet_parser.c`).
const ESTABLISHED_FRAMING: PacketHeaderSize = PacketHeaderSize::Four;

/// `PASS_THROUGH` (`erts/emulator/beam/external.c:56`): the single byte written
/// in place of a distribution header when neither the header atom cache nor
/// fragmentation was negotiated.
const PASS_THROUGH: u8 = b'p';

/// The atom-text encoding used for the distribution header and the control body.
/// `DFLAG_UTF8_ATOMS` is a mandatory flag, so a conforming peer always negotiates
/// it; the codec's own default is the same.
const ATOM_ENCODING: AtomEncoding = AtomEncoding::Utf8;

/// The driver's link state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DriverState {
    /// The connect/challenge handshake is in progress.
    Handshaking,
    /// The handshake completed; `DOP_*` messages flow.
    Established,
    /// The peer closed the connection (a clean EOF) — a terminal state.
    Closed,
    /// A protocol error made the link unusable — a terminal state.
    Failed,
}

/// What one [`ConnectionDriver::step`] call accomplished.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StepOutcome {
    /// No progress was possible without more input bytes; the runtime should read
    /// from its socket again (or do other work). Nothing was consumed or produced.
    WouldBlock,
    /// The handshake advanced but is not yet complete.
    Handshaking,
    /// The handshake completed on this call; the link is now established.
    Established,
    /// Established-phase progress: a packet was consumed, a tick was answered, or
    /// a message body was queued.
    Progress,
    /// The peer closed the connection (a clean EOF).
    Closed,
    /// The link was already unusable (a previous protocol error).
    Failed,
}

/// Which side of the handshake this driver plays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// The connecting side (`Name` → `Status` → `Challenge` → `ChallengeReply` →
    /// `ChallengeAck`).
    Client,
    /// The accepting side (`Name` → `Status::Ok` + `Challenge` → `ChallengeReply`
    /// → `ChallengeAck`).
    Server,
}

/// The handshake sub-state (one transition per `step`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HandshakePhase {
    /// Client: emit the `Name` frame.
    ClientSendName,
    /// Client: await the peer's `Status` frame.
    ClientAwaitStatus,
    /// Client: await the peer's `Challenge` frame.
    ClientAwaitChallenge,
    /// Client: await the peer's `ChallengeAck` frame.
    ClientAwaitAck,
    /// Server: await the client's `Name` frame.
    ServerAwaitName,
    /// Server: await the client's `ChallengeReply` frame.
    ServerAwaitReply,
}

/// The result of one handshake transition.
enum HsProgress {
    /// A transition happened; attempt the next one.
    More,
    /// The current phase needs a packet that is not yet fully buffered.
    NeedInput,
    /// The handshake completed.
    Established,
    /// The peer closed the connection.
    Closed,
}

/// The result of one established-phase transition.
enum EstProgress {
    /// A packet was consumed; attempt the next one.
    More,
    /// No whole packet is buffered yet.
    NeedInput,
    /// The peer closed the connection.
    Closed,
}

/// A non-blocking, byte-fed distribution-connection state machine.
///
/// Construct one with [`ConnectionDriver::client`] or [`ConnectionDriver::server`]
/// (mirroring [`crate::connection::Connection::client`] /
/// [`crate::connection::Connection::server`]) and drive it as described in the
/// module docs. The driver holds no socket: the runtime owns the I/O.
pub struct ConnectionDriver {
    /// Which handshake role this driver plays.
    role: Role,
    /// The current handshake phase.
    phase: HandshakePhase,
    /// The link state.
    state: DriverState,

    /// This node's name (advertised in `Name`/`Challenge`).
    our_name: Vec<u8>,
    /// The shared cookie used for the challenge digests.
    cookie: Vec<u8>,
    /// This node's creation (advertised in `Name`/`Challenge`).
    creation: u32,
    /// The capability flags this node offers ([`preferred_flags`]).
    our_flags: DistFlags,
    /// This node's challenge nonce (`gen_challenge`, or an explicit value).
    our_challenge: u32,

    /// The negotiated handshake outcome, once the handshake completes.
    handshake: Option<Handshake>,
    /// Scratch: the connecting side's advertised flags (from `Name`).
    hs_client_flags: DistFlags,
    /// Scratch: the accepting side's advertised flags (from `Challenge`).
    hs_server_flags: DistFlags,
    /// Scratch: the peer's validated flags (`check_mandatory`).
    hs_negotiated: DistFlags,
    /// Scratch: the peer's node creation.
    hs_peer_creation: u32,
    /// Scratch: the peer's node name.
    hs_peer_name: Vec<u8>,

    /// The accepting side's accept policy (server role only), consulted after the
    /// peer's `Name` passes the mandatory check. `None` for a client driver and for
    /// a server constructed without a policy — the default, which always answers
    /// the `"ok"` status.
    decide: Option<Box<dyn Fn(&Name) -> AcceptDecision + Send + Sync>>,
    /// Raw socket bytes fed by the runtime, not yet consumed.
    input: Vec<u8>,
    /// Bytes the runtime must write to its socket (handshake frames, tick replies,
    /// message envelopes).
    output: Vec<u8>,
    /// Whether the runtime has signalled end-of-stream on the read side.
    eof: bool,

    /// Reassembles `'E'`/`'F'` fragment sequences.
    fragments: FragmentAssembler,
    /// Applies inbound distribution headers so payload atom-cache refs resolve.
    recv_cache: ReceiveAtomCache,
    /// Classifies outbound atoms and yields the distribution header's refs.
    outbound: OutboundAtomCache,
    /// The next fragment sequence id (never `0`).
    next_seq_id: u64,

    /// Complete (defragmented) inbound message bodies awaiting decode.
    inbound: VecDeque<Vec<u8>>,
    /// How many ticks this link has answered (a diagnostic).
    ticks_answered: u64,
}

impl ConnectionDriver {
    /// Creates a **client** driver advertising `creation` as its own node
    /// creation, with a fresh [`gen_challenge`] nonce.
    ///
    /// This mirrors [`crate::connection::Connection::client`] (which performs the
    /// blocking handshake); the returned driver has not yet emitted anything — the
    /// first [`step`](Self::step) writes the `Name` frame to the output.
    pub fn client(our_name: &[u8], cookie: &[u8], creation: u32) -> ConnectionDriver {
        ConnectionDriver::client_with_challenge(our_name, cookie, creation, gen_challenge())
    }

    /// [`ConnectionDriver::client`] with an explicit challenge nonce (the testable
    /// / deterministic core).
    pub fn client_with_challenge(
        our_name: &[u8],
        cookie: &[u8],
        creation: u32,
        our_challenge: u32,
    ) -> ConnectionDriver {
        ConnectionDriver::new(
            Role::Client,
            HandshakePhase::ClientSendName,
            our_name,
            cookie,
            creation,
            our_challenge,
            preferred_flags(),
            None,
        )
    }

    /// [`ConnectionDriver::client`] advertising an explicit **capability flag set**.
    ///
    /// The opt-in seam for a *published* node (default: [`preferred_flags`], which
    /// omits `DFLAG_PUBLISHED`, `dist.h:30`/`dist.h:118-129`). See
    /// [`crate::connection::connect_client_with_flags`] and
    /// [`crate::handshake::publish_flag`].
    pub fn client_with_flags(
        our_name: &[u8],
        cookie: &[u8],
        creation: u32,
        flags: DistFlags,
    ) -> ConnectionDriver {
        ConnectionDriver::new(
            Role::Client,
            HandshakePhase::ClientSendName,
            our_name,
            cookie,
            creation,
            gen_challenge(),
            flags,
            None,
        )
    }

    /// Creates a **server** driver advertising `creation` as its own node
    /// creation, with a fresh [`gen_challenge`] nonce.
    ///
    /// This mirrors [`crate::connection::Connection::server`].
    ///
    /// **`our_name` must be this node's full `node@host` name.** OTP's connecting
    /// side matches the name inside the server's `'N'` challenge against the node
    /// it dialled (`lib/kernel/src/dist_util.erl:recv_challenge_new/2`, which
    /// compares `list_to_existing_atom(Ns)` with the target node); both
    /// `send_name/4` and `send_challenge/2` put the full `node@host` on the wire.
    /// A host-less (bare short) name is therefore refused by the peer, which shuts
    /// the socket down — the caller sees that abort as a short read
    /// ([`BeamError::BadArg`](beam_common::BeamError::BadArg)) on its next read.
    /// See `tests/live_accept.rs`.
    pub fn server(our_name: &[u8], cookie: &[u8], creation: u32) -> ConnectionDriver {
        ConnectionDriver::server_with_challenge(our_name, cookie, creation, gen_challenge())
    }

    /// [`ConnectionDriver::server`] with an explicit challenge nonce (the testable
    /// / deterministic core).
    pub fn server_with_challenge(
        our_name: &[u8],
        cookie: &[u8],
        creation: u32,
        our_challenge: u32,
    ) -> ConnectionDriver {
        ConnectionDriver::new(
            Role::Server,
            HandshakePhase::ServerAwaitName,
            our_name,
            cookie,
            creation,
            our_challenge,
            preferred_flags(),
            None,
        )
    }

    /// [`ConnectionDriver::server`] advertising an explicit **capability flag set**.
    ///
    /// The opt-in seam for a *published* acceptor (default: [`preferred_flags`], a
    /// **hidden** node). See [`crate::connection::accept_server_with_flags`].
    pub fn server_with_flags(
        our_name: &[u8],
        cookie: &[u8],
        creation: u32,
        flags: DistFlags,
    ) -> ConnectionDriver {
        ConnectionDriver::new(
            Role::Server,
            HandshakePhase::ServerAwaitName,
            our_name,
            cookie,
            creation,
            gen_challenge(),
            flags,
            None,
        )
    }

    /// [`ConnectionDriver::server`] consulting a caller-supplied **accept decision**.
    ///
    /// The non-blocking counterpart of
    /// [`crate::connection::accept_server_with_decision`]: `decide` is consulted
    /// once, with the peer's decoded [`Name`], after the mandatory flag check has
    /// passed (the order a real node uses — `dist_util.erl:286` runs `check_dflags`
    /// before `mark_pending/1`).
    ///
    /// * [`AcceptDecision::Ok`] behaves **exactly** as
    ///   [`server`](Self::server)/[`server_with_flags`](Self::server_with_flags) —
    ///   the peer receives the byte-identical `"ok"` status frame.
    /// * [`AcceptDecision::Refuse`]`(status)` buffers that status 2-byte-framed into
    ///   the driver's output (drained by [`take_output`](Self::take_output) — the
    ///   bytes survive even though [`step`](Self::step) then returns `Err`), moves the
    ///   link to [`DriverState::Failed`] and returns [`status_error`]`(&status)`.
    ///   The statuses a real accepting node sends here are [`Status::nok`]
    ///   (`dist_util.erl:343`), [`Status::alive`] (`dist_util.erl:386`) and
    ///   [`Status::not_allowed`] (`dist_util.erl:286`).
    ///
    /// The closure is shared (`Fn`, not `FnOnce`) because the driver is a reusable
    /// state machine; it is boxed so a driver with a policy and one without have the
    /// same type.
    pub fn server_with_decision(
        our_name: &[u8],
        cookie: &[u8],
        creation: u32,
        flags: DistFlags,
        decide: Box<dyn Fn(&Name) -> AcceptDecision + Send + Sync>,
    ) -> ConnectionDriver {
        ConnectionDriver::new(
            Role::Server,
            HandshakePhase::ServerAwaitName,
            our_name,
            cookie,
            creation,
            gen_challenge(),
            flags,
            Some(decide),
        )
    }

    /// The shared constructor (`our_flags` is the capability set this node
    /// advertises — [`preferred_flags`] unless a `*_with_flags` constructor
    /// overrides it; `decide` is the server-role accept policy, `None` for the
    /// always-`"ok"` default).
    fn new(
        role: Role,
        phase: HandshakePhase,
        our_name: &[u8],
        cookie: &[u8],
        creation: u32,
        our_challenge: u32,
        our_flags: DistFlags,
        decide: Option<Box<dyn Fn(&Name) -> AcceptDecision + Send + Sync>>,
    ) -> ConnectionDriver {
        ConnectionDriver {
            role,
            phase,
            state: DriverState::Handshaking,
            our_name: our_name.to_vec(),
            cookie: cookie.to_vec(),
            creation,
            our_flags,
            our_challenge,
            handshake: None,
            hs_client_flags: DistFlags::EMPTY,
            hs_server_flags: DistFlags::EMPTY,
            hs_negotiated: DistFlags::EMPTY,
            hs_peer_creation: 0,
            hs_peer_name: Vec::new(),
            decide,
            input: Vec::new(),
            output: Vec::new(),
            eof: false,
            fragments: FragmentAssembler::new(),
            recv_cache: ReceiveAtomCache::new(),
            outbound: OutboundAtomCache::new(),
            next_seq_id: 1,
            inbound: VecDeque::new(),
            ticks_answered: 0,
        }
    }

    /// Appends raw socket bytes to the driver's input buffer.
    ///
    /// The driver never reads a socket itself: the runtime reads whatever it can
    /// and forwards the bytes here, then calls [`step`](Self::step).
    pub fn feed(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
    }

    /// Signals that the runtime's read side reached end-of-stream (a clean EOF,
    /// `read` returning `0`).
    ///
    /// The next [`step`](Self::step) at a frame boundary reports
    /// [`StepOutcome::Closed`]; a partial frame left in the buffer is a truncated
    /// frame and yields [`BeamError::BadArg`].
    pub fn feed_eof(&mut self) {
        self.eof = true;
    }

    /// Makes all possible non-blocking progress and reports what happened.
    ///
    /// Drives the incremental handshake (2-byte-framed) and, once established,
    /// consumes 4-byte-framed packets, answers ticks, reassembles fragments,
    /// validates each packet's leading bytes and queues the bodies. Output the
    /// runtime must write accumulates in the driver; drain it with
    /// [`take_output`](Self::take_output).
    ///
    /// Returns [`StepOutcome::WouldBlock`] when no progress is possible without
    /// more input; a protocol error sets [`DriverState::Failed`] and is returned as
    /// [`Err`].
    pub fn step(&mut self) -> Result<StepOutcome> {
        let mut progressed = false;
        let mut just_established = false;
        loop {
            match self.state {
                DriverState::Closed => return Ok(StepOutcome::Closed),
                DriverState::Failed => return Ok(StepOutcome::Failed),
                DriverState::Handshaking => match self.handshake_step() {
                    Err(err) => {
                        self.state = DriverState::Failed;
                        return Err(err);
                    }
                    Ok(HsProgress::More) => progressed = true,
                    Ok(HsProgress::Established) => {
                        self.state = DriverState::Established;
                        just_established = true;
                    }
                    Ok(HsProgress::NeedInput) => {
                        return Ok(if just_established {
                            StepOutcome::Established
                        } else if progressed {
                            StepOutcome::Handshaking
                        } else {
                            StepOutcome::WouldBlock
                        });
                    }
                    Ok(HsProgress::Closed) => {
                        self.state = DriverState::Closed;
                        return Ok(StepOutcome::Closed);
                    }
                },
                DriverState::Established => match self.established_step() {
                    Err(err) => {
                        self.state = DriverState::Failed;
                        return Err(err);
                    }
                    Ok(EstProgress::More) => progressed = true,
                    Ok(EstProgress::NeedInput) => {
                        return Ok(if just_established {
                            StepOutcome::Established
                        } else if progressed {
                            StepOutcome::Progress
                        } else {
                            StepOutcome::WouldBlock
                        });
                    }
                    Ok(EstProgress::Closed) => {
                        self.state = DriverState::Closed;
                        return Ok(StepOutcome::Closed);
                    }
                },
            }
        }
    }

    /// Drains the bytes the runtime must write to its socket.
    ///
    /// Returns an empty `Vec` when there is nothing to write.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Whether there are bytes waiting in the output buffer.
    pub fn has_output(&self) -> bool {
        !self.output.is_empty()
    }

    /// Decodes the next queued control message, or returns `None` when none is
    /// pending.
    ///
    /// The body's distribution header is parsed and applied to the receive atom
    /// cache here (see the module docs), so the payload's `ATOM_CACHE_REF`s resolve
    /// against `atoms`. `atoms` must be the same table across calls (the cached atom
    /// indices are table-local).
    pub fn next_message(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
    ) -> Result<Option<ControlMessage>> {
        let body = match self.inbound.pop_front() {
            Some(body) => body,
            None => return Ok(None),
        };
        let (header, offset) = parse_header(&body, atoms)?;
        self.recv_cache.apply(&header)?;
        let payload = body.get(offset..).ok_or(BeamError::BadArg)?;
        let message = decode_control(payload, heap, atoms, Some(self.recv_cache.table()))?;
        Ok(Some(message))
    }

    /// Decodes **and classifies** the next queued inbound control message, or
    /// returns `None` when none is pending.
    ///
    /// This is [`next_message`](Self::next_message) followed by
    /// [`Delivery::classify`] — the semantic view a runtime routes.
    pub fn next_delivery(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
    ) -> Result<Option<Delivery>> {
        match self.next_message(heap, atoms)? {
            Some(message) => Ok(Some(Delivery::classify(&message)?)),
            None => Ok(None),
        }
    }

    /// Decodes, classifies and **routes** every queued inbound control message
    /// through `sink`, returning a [`Dispatched`] record per message in arrival
    /// order.
    ///
    /// This is the inbound delivery seam: the caller (e.g. `beam_rt`) supplies an
    /// [`InboundSink`] over its process table/mailbox. Draining all queued messages
    /// matches the driver's own `step` loop. A classification failure (a
    /// message-carrying opcode with no trailing message) is returned as `Err` and
    /// does **not** alter the link state; messages already dispatched stay
    /// dispatched.
    pub fn dispatch_inbound(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
        sink: &mut dyn InboundSink,
    ) -> Result<Vec<Dispatched>> {
        let mut dispatched = Vec::new();
        while let Some(delivery) = self.next_delivery(heap, atoms)? {
            let outcome = sink.deliver(&delivery)?;
            dispatched.push(Dispatched { delivery, outcome });
        }
        Ok(dispatched)
    }
    /// Encodes and queues one `DOP_*` control message.
    ///
    /// Every referenced atom is routed through the outbound atom cache
    /// ([`encode_control_cached`]), the matching [`DistHeader`] is prepended, and a
    /// payload larger than [`DIST_FRAGMENT_SIZE`] is split into `'E'`/`'F'` frames
    /// when `DFLAG_FRAGMENTS` was negotiated (the same envelope
    /// [`crate::connection::Connection`] builds). A message carrying no atoms
    /// yields the exact empty-header bytes `131, 68, 0`.
    ///
    /// Returns [`BeamError::BadArg`] when called before the link is established.
    pub fn send_control(&mut self, dop: Dop, terms: &[Eterm], atoms: &AtomTable) -> Result<()> {
        if self.state != DriverState::Established {
            return Err(BeamError::BadArg);
        }
        let control = encode_control_cached(dop, terms, atoms, Some(&mut self.outbound))?;
        let header = self.outbound.take_header();
        let mut sink = MemoryTransport::new();
        header.encode(&mut sink, atoms, ATOM_ENCODING)?;
        let header = sink.into_written();

        let fragment = self.negotiated().contains(DistFlags::from_bits(DFLAG_FRAGMENTS))
            && header.len().saturating_add(control.len()) > DIST_FRAGMENT_SIZE;

        if fragment {
            // The header tail (`NumberOfAtomCacheRefs + Flags + AtomCacheRefs`) rides
            // in the `'E'` frame, ahead of the first payload chunk.
            let tail = header.get(2..).ok_or(BeamError::BadArg)?;
            let seq_id = self.next_seq_id;
            self.next_seq_id = self.next_seq_id.checked_add(1).filter(|id| *id != 0).unwrap_or(1);
            for frame in split(seq_id, tail, &control, DIST_FRAGMENT_SIZE)? {
                self.frame(ESTABLISHED_FRAMING, &frame)?;
            }
        } else {
            let mut body = header;
            body.extend_from_slice(&control);
            self.frame(ESTABLISHED_FRAMING, &body)?;
        }
        Ok(())
    }

    /// Sends a message to `to` from `from`.
    ///
    /// Selects `DOP_SEND_SENDER` when the peer negotiated `DFLAG_SEND_SENDER` and
    /// `DOP_SEND` otherwise (`dist.c:1686`), with `msg` as the trailing message
    /// term — see [`crate::connection::Connection::send_message`].
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

    /// Queues a tick (a zero-length packet) for the runtime to write.
    pub fn send_tick(&mut self) -> Result<()> {
        write_tick(&mut VecSink(&mut self.output), ESTABLISHED_FRAMING)
    }

    /// The current link state.
    pub fn state(&self) -> DriverState {
        self.state
    }

    /// The peer's validated capability flags, once established (the flags that
    /// govern outbound encoding — see [`Handshake::negotiated`]).
    ///
    /// [`DistFlags::EMPTY`] before the handshake completes.
    pub fn negotiated(&self) -> DistFlags {
        self.handshake.as_ref().map_or(DistFlags::EMPTY, |h| h.negotiated)
    }

    /// The connecting side's advertised flags, once established.
    pub fn client_flags(&self) -> DistFlags {
        self.handshake.as_ref().map_or(DistFlags::EMPTY, |h| h.client_flags)
    }

    /// The accepting side's advertised flags, once established.
    pub fn server_flags(&self) -> DistFlags {
        self.handshake.as_ref().map_or(DistFlags::EMPTY, |h| h.server_flags)
    }

    /// The peer's node name, once established.
    pub fn peer_name(&self) -> Option<&[u8]> {
        self.handshake.as_ref().map(|h| h.peer_name.as_slice())
    }

    /// The peer's node creation, once established (`0` before).
    pub fn peer_creation(&self) -> u32 {
        self.handshake.as_ref().map_or(0, |h| h.peer_creation)
    }

    /// How the peer's own flags classify it (`dist_util.erl:publish_type/1` — see
    /// [`Handshake::peer_visibility`]), or `None` before the handshake completes.
    pub fn peer_visibility(&self) -> Option<PeerVisibility> {
        self.handshake.as_ref().map(|h| h.peer_visibility)
    }

    /// The full handshake outcome, once established.
    pub fn handshake(&self) -> Option<&Handshake> {
        self.handshake.as_ref()
    }

    /// How many ticks this link has answered (a diagnostic).
    pub fn ticks_answered(&self) -> u64 {
        self.ticks_answered
    }

    /// Whether the runtime has signalled end-of-stream on the read side.
    pub fn is_eof(&self) -> bool {
        self.eof
    }

    /// Whether this driver plays the connecting (client) handshake role.
    pub fn is_client(&self) -> bool {
        self.role == Role::Client
    }
    /// One handshake transition.
    fn handshake_step(&mut self) -> Result<HsProgress> {
        match self.phase {
            HandshakePhase::ClientSendName => {
                let name = Name {
                    flags: self.our_flags,
                    creation: self.creation,
                    name: self.our_name.clone(),
                };
                let body = encode_handshake_body(|w| name.encode(w))?;
                self.frame(HANDSHAKE_FRAMING, &body)?;
                self.phase = HandshakePhase::ClientAwaitStatus;
                Ok(HsProgress::More)
            }
            HandshakePhase::ClientAwaitStatus => {
                let body = match self.take_frame(HANDSHAKE_FRAMING)? {
                    Some(body) => body,
                    None => return self.stall_hs(),
                };
                let status = Status::decode(&body)?;
                // Anything other than `"ok"` aborts, classified by the single
                // status -> error mapping shared with the blocking client
                // ([`crate::handshake::status_error`]): `"nok"`/`"not_allowed"`
                // refusals, an `"alive"` peer, and the two statuses a real node
                // continues on but this client cannot (`"ok_simultaneous"`/
                // `"named:"`) all become the same typed `Io` error (and the same
                // `io::ErrorKind`) the blocking API produces.
                if status != Status::Ok {
                    return Err(status_error(&status));
                }
                self.phase = HandshakePhase::ClientAwaitChallenge;
                Ok(HsProgress::More)
            }
            HandshakePhase::ClientAwaitChallenge => {
                let body = match self.take_frame(HANDSHAKE_FRAMING)? {
                    Some(body) => body,
                    None => return self.stall_hs(),
                };
                let challenge = Challenge::decode(&body)?;
                self.hs_negotiated = check_mandatory(challenge.flags)?;
                self.hs_server_flags = challenge.flags;
                self.hs_client_flags = self.our_flags;
                self.hs_peer_creation = challenge.creation;
                let peer_challenge = challenge.challenge;
                self.hs_peer_name = challenge.name;
                let reply = ChallengeReply {
                    challenge: self.our_challenge,
                    digest: gen_digest(&self.cookie, peer_challenge),
                };
                let body = encode_handshake_body(|w| reply.encode(w))?;
                self.frame(HANDSHAKE_FRAMING, &body)?;
                self.phase = HandshakePhase::ClientAwaitAck;
                Ok(HsProgress::More)
            }
            HandshakePhase::ClientAwaitAck => {
                let body = match self.take_frame(HANDSHAKE_FRAMING)? {
                    Some(body) => body,
                    None => return self.stall_hs(),
                };
                let ack = ChallengeAck::decode(&body)?;
                if !verify(&self.cookie, self.our_challenge, &ack.digest) {
                    return Err(BeamError::BadArg);
                }
                self.establish();
                Ok(HsProgress::Established)
            }
            HandshakePhase::ServerAwaitName => {
                let body = match self.take_frame(HANDSHAKE_FRAMING)? {
                    Some(body) => body,
                    None => return self.stall_hs(),
                };
                let name = Name::decode(&body)?;
                // A peer whose flags fail the mandatory check (the v6 version gate)
                // is refused. A real accepting node *tells the peer* before dropping
                // the link (`dist_util.erl:286`: `check_dflags` ->
                // `send_status(HSData, not_allowed)`); the framed `"not_allowed"`
                // status is buffered into `output` here (drained by
                // [`take_output`](Self::take_output) — the bytes survive even though
                // this returns `Err`) so the rejecting driver emits the same wire
                // bytes the blocking acceptor does (`crate::connection`'s
                // `send_refusal`), and the rejected peer reads a status frame
                // instead of blocking for one that never comes.
                let negotiated = match check_mandatory(name.flags) {
                    Ok(negotiated) => negotiated,
                    Err(refusal) => {
                        let status = encode_handshake_body(|w| Status::not_allowed().encode(w))?;
                        self.frame(HANDSHAKE_FRAMING, &status)?;
                        return Err(refusal);
                    }
                };
                // The local accept policy, consulted only after the mandatory check
                // (a real node runs `check_dflags` first, `dist_util.erl:286`). A
                // refusal is buffered into `output` *before* this returns `Err`, so
                // the framed status still reaches the peer through `take_output` —
                // exactly the `not_allowed` branch above.
                let decision = match &self.decide {
                    Some(decide) => decide(&name),
                    None => AcceptDecision::Ok,
                };
                if let AcceptDecision::Refuse(status) = decision {
                    let body = encode_handshake_body(|w| status.encode(w))?;
                    self.frame(HANDSHAKE_FRAMING, &body)?;
                    return Err(status_error(&status));
                }
                self.hs_negotiated = negotiated;
                self.hs_client_flags = name.flags;
                self.hs_server_flags = self.our_flags;
                self.hs_peer_creation = name.creation;
                self.hs_peer_name = name.name;

                let status = encode_handshake_body(|w| Status::Ok.encode(w))?;
                self.frame(HANDSHAKE_FRAMING, &status)?;
                let challenge = Challenge {
                    flags: self.our_flags,
                    challenge: self.our_challenge,
                    creation: self.creation,
                    name: self.our_name.clone(),
                };
                let body = encode_handshake_body(|w| challenge.encode(w))?;
                self.frame(HANDSHAKE_FRAMING, &body)?;
                self.phase = HandshakePhase::ServerAwaitReply;
                Ok(HsProgress::More)
            }
            HandshakePhase::ServerAwaitReply => {
                let body = match self.take_frame(HANDSHAKE_FRAMING)? {
                    Some(body) => body,
                    None => return self.stall_hs(),
                };
                let reply = ChallengeReply::decode(&body)?;
                if !verify(&self.cookie, self.our_challenge, &reply.digest) {
                    return Err(BeamError::BadArg);
                }
                let ack = ChallengeAck { digest: gen_digest(&self.cookie, reply.challenge) };
                let body = encode_handshake_body(|w| ack.encode(w))?;
                self.frame(HANDSHAKE_FRAMING, &body)?;
                self.establish();
                Ok(HsProgress::Established)
            }
        }
    }

    /// Finalises the handshake: builds the [`Handshake`] outcome.
    fn establish(&mut self) {
        // The peer's own flags: the client's `Challenge` carries the server's set
        // and the server's `Name` carries the client's — exactly the split
        // `dist_util.erl` uses to classify `other_flags` (`publish_type/1`).
        let peer_flags = if self.role == Role::Client {
            self.hs_server_flags
        } else {
            self.hs_client_flags
        };
        self.handshake = Some(Handshake {
            client_flags: self.hs_client_flags,
            server_flags: self.hs_server_flags,
            negotiated: self.hs_negotiated,
            peer_creation: self.hs_peer_creation,
            peer_name: std::mem::take(&mut self.hs_peer_name),
            peer_visibility: publish_type(peer_flags),
            buffered: None,
        });
    }

    /// One established-phase transition.
    fn established_step(&mut self) -> Result<EstProgress> {
        let packet = match try_take_packet(&mut self.input, ESTABLISHED_FRAMING)? {
            Some(packet) => packet,
            None => return self.stall_est(),
        };
        if is_tick(&packet) {
            self.queue_tick()?;
            self.ticks_answered += 1;
            return Ok(EstProgress::More);
        }
        match self.fragments.push(&packet)? {
            Assembly::NotFragmented => {
                if self.fragments.is_pending() {
                    // Strict channel ordering: a message may not interleave a fragment
                    // sequence (`erl_ext_dist.md`).
                    return Err(BeamError::BadArg);
                }
                validate_header_prefix(&packet)?;
                self.inbound.push_back(packet);
                Ok(EstProgress::More)
            }
            Assembly::Pending => Ok(EstProgress::More),
            Assembly::Complete(body) => {
                validate_header_prefix(&body)?;
                self.inbound.push_back(body);
                Ok(EstProgress::More)
            }
        }
    }

    /// Takes one length-framed packet out of the input buffer.
    fn take_frame(&mut self, hdr: PacketHeaderSize) -> Result<Option<Vec<u8>>> {
        try_take_packet(&mut self.input, hdr)
    }

    /// The handshake outcome when a frame is not yet fully buffered: `NeedInput`
    /// normally, `Closed` at a clean EOF, or a truncated-frame error at EOF with a
    /// partial frame buffered.
    fn stall_hs(&self) -> Result<HsProgress> {
        if self.eof {
            if self.input.is_empty() {
                Ok(HsProgress::Closed)
            } else {
                Err(BeamError::BadArg)
            }
        } else {
            Ok(HsProgress::NeedInput)
        }
    }

    /// The established outcome when no whole packet is buffered (see [`stall_hs`]).
    ///
    /// [`stall_hs`]: Self::stall_hs
    fn stall_est(&self) -> Result<EstProgress> {
        if self.eof {
            if self.input.is_empty() {
                Ok(EstProgress::Closed)
            } else {
                Err(BeamError::BadArg)
            }
        } else {
            Ok(EstProgress::NeedInput)
        }
    }

    /// Writes one length-framed packet into the output buffer.
    fn frame(&mut self, hdr: PacketHeaderSize, body: &[u8]) -> Result<()> {
        write_packet(&mut VecSink(&mut self.output), hdr, body)
    }

    /// Queues a tick reply into the output buffer.
    fn queue_tick(&mut self) -> Result<()> {
        write_tick(&mut VecSink(&mut self.output), ESTABLISHED_FRAMING)
    }
}

/// Encodes a handshake message body through its own codec, into a scratch buffer.
fn encode_handshake_body<F>(encode: F) -> Result<Vec<u8>>
where
    F: FnOnce(&mut MemoryTransport) -> Result<()>,
{
    let mut sink = MemoryTransport::new();
    encode(&mut sink)?;
    Ok(sink.into_written())
}

/// Tries to take one length-framed packet from the front of `buf`, reusing the
/// crate's [`crate::framing`] codec.
///
/// Returns `None` when the buffer does not yet hold a whole packet (the runtime
/// must feed more bytes); otherwise removes and returns the body. A **zero-length**
/// body is a tick (an empty `Vec`).
fn try_take_packet(buf: &mut Vec<u8>, hdr: PacketHeaderSize) -> Result<Option<Vec<u8>>> {
    let hdr_bytes = hdr.bytes();
    if buf.len() < hdr_bytes {
        return Ok(None);
    }
    let len = {
        let mut prefix = SliceReader { bytes: &buf[..hdr_bytes], pos: 0 };
        read_length(&mut prefix, hdr)?
    };
    let total = hdr_bytes.checked_add(len).ok_or(BeamError::SystemLimit)?;
    if buf.len() < total {
        return Ok(None);
    }
    let body = {
        let mut reader = SliceReader { bytes: &buf[..total], pos: 0 };
        read_packet(&mut reader, hdr)?
    };
    buf.drain(..total);
    Ok(Some(body))
}

/// A cheap structural check of a complete (defragmented) packet body, so a
/// garbage first byte is rejected by [`ConnectionDriver::step`] rather than
/// queued: the body must start with a normal `'D'` header or the `PASS_THROUGH`
/// byte. The full header is parsed later, in
/// [`ConnectionDriver::next_message`] (see the module docs).
fn validate_header_prefix(body: &[u8]) -> Result<()> {
    match body.first().copied() {
        Some(beam_codec::VERSION_MAGIC) if body.get(1).copied() == Some(DIST_HEADER) => Ok(()),
        Some(PASS_THROUGH) => Ok(()),
        _ => Err(BeamError::BadArg),
    }
}

/// Splits a complete (defragmented) message body into its distribution header and
/// the offset at which the control body begins.
///
/// Accepts the normal `'D'` header, the reassembled fragment form (also `'D'`),
/// and the pass-through form (a lone `PASS_THROUGH` byte, optionally followed by
/// the term's own `VERSION_MAGIC`) — the same rule `crate::connection` documents.
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

/// A [`Write`] sink that appends to a `Vec<u8>` (the driver's output buffer).
struct VecSink<'a>(&'a mut Vec<u8>);

impl Write for VecSink<'_> {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.0.extend_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A [`Read`] over an in-memory slice (a prefix of the driver's input buffer).
struct SliceReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Read for SliceReader<'_> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        let end = self.pos.checked_add(buf.len()).ok_or(BeamError::BadArg)?;
        let src = self.bytes.get(self.pos..end).ok_or(BeamError::BadArg)?;
        buf.copy_from_slice(src);
        self.pos = end;
        Ok(())
    }
}
