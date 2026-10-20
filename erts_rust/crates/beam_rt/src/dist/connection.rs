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

//! One accepted distribution link and the driver that owns its protocol.
//!
//! [`DistConnection`] is the object a peer that **dialled into** this runtime gets: a
//! real socket plus a server-side [`ConnectionDriver`] (the `'N'` → `'s'`/`'S'` → `'r'` →
//! `'a'` handshake, then the established control-message stream). It owns the link's
//! bytes in **both** directions — every read, every write and the checked inbound drain
//! — while the listener that produced it, the accept loop and the `epmd` registration
//! live in the parent's `node` module, and the node→link routing table lives in the
//! sibling `links` module.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::{Dispatched, DistFlags, Dop, InboundSink, PeerVisibility};
use beam_term::prelude::Eterm;
use beam_term::AtomTable;

use super::DIST_READ_SLICE;

/// One live inbound distribution link and the driver that owns its protocol.
pub struct DistConnection {
    /// The accepting side's handshake/established state machine.
    driver: ConnectionDriver,
    /// The socket the driver's bytes travel over.
    stream: TcpStream,
    /// The peer's socket address (a diagnostic; the dist wire carries no address).
    peer_addr: SocketAddr,
    /// The peer's **node atom term** — the key a routed [`beam_process::DistSignal`] addresses.
    ///
    /// `None` until [`DistNode::bind_peer_nodes`](crate::dist::DistNode::bind_peer_nodes)
    /// interns [`DistConnection::peer_name`] into the runtime's live atom table; a link that
    /// never binds is simply not matched by the accepted half's lookup (see
    /// [`crate::dist::links`]).
    node: Option<Eterm>,
    /// Whether this link **ever** completed its `'N'` handshake.
    ///
    /// A latch, not a state: [`beam_dist::driver::DriverState`] leaves `Established` *before*
    /// the retirement site asks (a peer's EOF moves it to `Closed`, a protocol error to
    /// `Failed`), so a retirement that asked the live state would call every dead link
    /// "never handshook" — and a peer loss would never be recorded. See
    /// [`DistConnection::was_established`].
    was_established: bool,
    /// Bytes read from the peer so far.
    bytes_in: u64,
    /// Bytes written to the peer so far.
    bytes_out: u64,
}

impl DistConnection {
    /// Wraps an accepted socket in a **server-side** driver advertising
    /// `advertised` as this node's name and `flags` as this node's capability set —
    /// [`advertised_flags`](crate::dist::advertised_flags), so a normal node publishes
    /// itself (`DFLAG_PUBLISHED`) and a `-hidden` one does not.
    pub(super) fn new(stream: TcpStream, peer_addr: SocketAddr, advertised: &[u8], cookie: &[u8], creation: u32, flags: DistFlags) -> Result<DistConnection> {
        stream.set_nodelay(true).map_err(BeamError::Io)?;
        stream
            .set_read_timeout(Some(DIST_READ_SLICE))
            .map_err(BeamError::Io)?;
        Ok(DistConnection {
            driver: ConnectionDriver::server_with_flags(advertised, cookie, creation, flags),
            stream,
            peer_addr,
            node: None,
            was_established: false,
            bytes_in: 0,
            bytes_out: 0,
        })
    }

    /// A connection around an **explicit** driver, on a socket that must already be
    /// configured (`TCP_NODELAY`, the [`DIST_READ_SLICE`] read timeout).
    ///
    /// The test-only seam that lets a suite install a driver the accepting path never
    /// builds — e.g. one with an accept policy
    /// ([`ConnectionDriver::server_with_decision`]) or one whose peer flags fail the
    /// mandatory check, so a **refusal** (which `beam_dist` buffers into the driver's
    /// output *before* returning `Err`) can be driven over a real loopback socket.
    #[cfg(test)]
    pub(crate) fn from_parts(
        driver: ConnectionDriver,
        stream: TcpStream,
        peer_addr: SocketAddr,
    ) -> DistConnection {
        DistConnection {
            driver,
            stream,
            peer_addr,
            node: None,
            was_established: false,
            bytes_in: 0,
            bytes_out: 0,
        }
    }
    /// The link's protocol state ([`DriverState::Established`] once the `'N'`
    /// handshake has completed and been acknowledged).
    pub fn state(&self) -> DriverState {
        self.driver.state()
    }

    /// Whether the handshake has completed (the link is usable).
    pub fn is_established(&self) -> bool {
        matches!(self.driver.state(), DriverState::Established)
    }

    /// Whether the handshake **ever** completed — the question a retirement must ask.
    ///
    /// [`DistConnection::is_established`] is the *live* state, and by the time a link is
    /// retired it is always gone: the pump that discovered the death is what moved the driver
    /// out of `Established` (`Closed` on the peer's EOF, `Failed` on a protocol error). A
    /// retirement that asked the live state would therefore classify **every** dead link as
    /// one that never handshook, record no peer loss, and silently drop OTP's `noconnection`
    /// semantics — while a link that really never handshook stays `false` here, which is the
    /// distinction the recorder needs.
    pub fn was_established(&self) -> bool {
        self.was_established
    }

    /// Whether the link can still make progress (handshaking or established).
    pub fn is_live(&self) -> bool {
        matches!(
            self.driver.state(),
            DriverState::Handshaking | DriverState::Established
        )
    }

    /// The peer's node name from the `'N'`/`'N'` handshake, once known.
    pub fn peer_name(&self) -> Option<String> {
        self.driver
            .peer_name()
            .map(|name| String::from_utf8_lossy(name).into_owned())
    }

    /// The peer's **node atom term**, once
    /// [`DistNode::bind_peer_nodes`](crate::dist::DistNode::bind_peer_nodes) has interned
    /// [`DistConnection::peer_name`] into the runtime's live atom table.
    ///
    /// This is what makes the accepted half of the routing table an **exact** node match:
    /// a routed control addresses its peer by atom term, and an accepted link that has
    /// bound its own answers with the very term the caller holds. A retired link is gone
    /// from the table, so its binding is irrelevant — nothing outside the link holds it.
    pub(crate) fn node_term(&self) -> Option<Eterm> {
        self.node
    }

    /// Binds this link's peer node atom term (see [`DistConnection::node_term`]).
    ///
    /// The caller interns the peer's `node@host` name in the runtime's one live atom table
    /// and hands the resulting term in; binding never re-interns and never looks a name up.
    pub(crate) fn bind_node(&mut self, atom: Eterm) {
        self.node = Some(atom);
    }
    /// Installs this runtime's **decode-side identity** on the link's driver
    /// ([`beam_dist::control::LocalIdentity`]).
    ///
    /// With it installed, an inbound pid/port/reference whose node **and** creation are
    /// this node's decodes to the **internal** term (the C's `dec_is_this_node`,
    /// `external.c:3123-3129`), which is what lets the sink route a reflected pid and an
    /// inbound alias reference. Refreshed by every drain
    /// ([`DistNode::dispatch_inbound`](crate::dist::DistNode::dispatch_inbound)), because
    /// the atom it carries is table-local.
    pub(crate) fn set_local_identity(&mut self, identity: beam_dist::control::LocalIdentity) {
        self.driver.set_local_identity(identity);
    }

    /// The identity installed by [`DistConnection::set_local_identity`], or `None`
    /// (the default) — the observable proof a drain really installed one. The colocated
    /// suites read it; production code installs and never asks, which is why it carries the
    /// `dead_code` allowance.
    #[allow(dead_code)]
    pub(crate) fn local_identity(&self) -> Option<beam_dist::control::LocalIdentity> {
        self.driver.local_identity()
    }

    /// The peer's node creation, once the handshake advertised it.
    pub fn peer_creation(&self) -> u32 {
        self.driver.peer_creation()
    }

    /// The **peer's** validated capability flags, once the handshake completed
    /// (`beam_dist::Handshake::negotiated`; [`DistFlags::EMPTY`] before then).
    ///
    /// This is what governs outbound opcode choice on the link — e.g. whether
    /// `DFLAG_ALIAS` allows a `DOP_ALIAS_SEND` (see
    /// [`DistLinks::route_alias`](crate::dist::links::DistLinks::route_alias)) or
    /// `DFLAG_SEND_SENDER` a `DOP_SEND_SENDER` (`dist.c:1686`).
    pub(crate) fn negotiated(&self) -> DistFlags {
        self.driver.negotiated()
    }

    /// How the peer's own advertised flags classify it — [`PeerVisibility::Normal`]
    /// or [`PeerVisibility::Hidden`] (`dist_util.erl:publish_type/1`) — or `None`
    /// before the handshake completes.
    ///
    /// **Recorded, never refused.** Visibility is a *classification* of a peer's
    /// flags, not a permission this runtime could check: both ends of a real OTP
    /// connection may advertise themselves hidden (`dist_util.erl:publish_type/1`
    /// answers `hidden` for any peer that did not set `DFLAG_PUBLISHED`, and
    /// `-hidden` is a per-node choice), and this runtime's own choice is
    /// [`DistNode::set_hidden`](crate::dist::DistNode::set_hidden)
    /// ([`advertised_flags`](crate::dist::advertised_flags) — published unless
    /// `-hidden`). A hidden peer is therefore the *expected* case for a hidden node and
    /// is accepted exactly like a normal one; the classification is only made observable
    /// here, per link.
    pub fn peer_visibility(&self) -> Option<PeerVisibility> {
        self.driver.peer_visibility()
    }

    /// The peer's socket address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Bytes read from / written to the peer so far.
    pub fn bytes(&self) -> (u64, u64) {
        (self.bytes_in, self.bytes_out)
    }

    /// Whether the driver holds bytes it still has to send (a diagnostic; a pump
    /// always flushes them).
    pub fn has_pending_output(&self) -> bool {
        self.driver.has_output()
    }

    /// Writes whatever the driver has queued to the socket (the link's one output path).
    ///
    /// # Errors
    ///
    /// The socket's own write failure, which the caller turns into a retired link.
    pub(crate) fn flush_output(&mut self) -> Result<()> {
        if self.driver.has_output() {
            let bytes = self.driver.take_output();
            self.stream.write_all(&bytes).map_err(BeamError::Io)?;
            self.stream.flush().map_err(BeamError::Io)?;
            self.bytes_out += bytes.len() as u64;
        }
        Ok(())
    }

    /// Encodes one `DOP_*` control on this link's driver and writes it — the accepting
    /// half's outbound path, reached through
    /// [`DistLinks::route_control`](crate::dist::links::DistLinks::route_control) so a
    /// peer that dialled *this* runtime is served exactly like one it dialled.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] before the handshake completed (the driver's own contract),
    /// the encoder's failures and the socket's write failure.
    pub(crate) fn send_control(&mut self, dop: Dop, args: &[Eterm], atoms: &AtomTable) -> Result<()> {
        self.driver.send_control(dop, args, atoms)?;
        self.flush_output()
    }

    /// Encodes one message on this link and writes it; the driver picks
    /// `DOP_SEND_SENDER`/`DOP_SEND` from the negotiated flags.
    ///
    /// # Errors
    ///
    /// As [`DistConnection::send_control`].
    pub(crate) fn send_message(
        &mut self,
        from: Eterm,
        to: Eterm,
        msg: Eterm,
        atoms: &AtomTable,
    ) -> Result<()> {
        self.driver.send_message(from, to, msg, atoms)?;
        self.flush_output()
    }
    /// Shuts the link's socket down in **both** directions.
    ///
    /// A link is never left half-open: the retirement helper behind
    /// [`DistNode::pump`](crate::dist::DistNode::pump) (and behind
    /// [`DistNode::dispatch_inbound`](crate::dist::DistNode::dispatch_inbound)) shuts the
    /// socket down before the connection drops, so the peer observes the closure
    /// immediately instead of holding a socket whose other end is merely unreferenced.
    ///
    /// # Errors
    ///
    /// A genuine `shutdown(2)` failure. The *already-closed* answers — `ENOTCONN`
    /// ([`ErrorKind::NotConnected`], what Linux returns for a socket that is shut
    /// down or was never connected) and the connection-lost family a peer's reset
    /// produces — are **not** errors: the post-condition the caller asked for (the
    /// socket is closed) already holds.
    pub fn close(&mut self) -> Result<()> {
        match self.stream.shutdown(std::net::Shutdown::Both) {
            Ok(()) => Ok(()),
            Err(err) if is_already_closed(&err) => Ok(()),
            Err(err) => Err(BeamError::Io(err)),
        }
    }

    /// Reads whatever has arrived, advances the handshake and writes the
    /// driver's reply. Returns how many protocol events took effect.
    ///
    /// A read timeout ([`DIST_READ_SLICE`], or a clean `WouldBlock`) is not an
    /// event and not an error: the link simply had nothing to say.
    ///
    /// The driver's buffered output is flushed on **both** outcomes. `beam_dist`
    /// frames a refusal status (`"not_allowed"` on a mandatory-flag failure, an
    /// [`beam_dist::AcceptDecision::Refuse`] status) into the driver's output and
    /// *then* returns `Err`, so the bytes a peer is waiting for must still reach its
    /// socket: the step's error is therefore propagated only **after** the flush, and
    /// only a genuine write failure (which the caller turns into a retired link) can
    /// suppress it.
    pub(super) fn pump(&mut self) -> Result<usize> {
        let mut events = 0;
        let mut buf = [0u8; 8192];
        match self.stream.read(&mut buf) {
            Ok(0) => {
                // A clean EOF: the peer closed; the next step reports `Closed`.
                self.driver.feed_eof();
                events += 1;
            }
            Ok(n) => {
                self.bytes_in += n as u64;
                self.driver.feed(&buf[..n]);
                events += 1;
            }
            Err(err) if is_would_block(&err) => {}
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(BeamError::Io(err)),
        }
        // A step that reports an error may still have framed a refusal into the
        // output buffer, so the outcome is held rather than propagated with `?`.
        let stepped = self.driver.step();
        // The establishment latch is set **here**, before the driver can leave `Established`
        // again: the retirement sites read it (see [`DistConnection::was_established`]). One
        // `step` can establish *and* consume the whole inbound buffer, so an outcome that
        // reports the establishment (`Established`) or progress on an established link
        // (`Progress`) counts even when a same-call EOF already moved the state on.
        self.was_established |= self.is_established()
            || matches!(
                stepped.as_ref(),
                Ok(beam_dist::StepOutcome::Established | beam_dist::StepOutcome::Progress)
            );
        if let Ok(outcome) = &stepped {
            match outcome {
                beam_dist::StepOutcome::Progress
                | beam_dist::StepOutcome::Handshaking
                | beam_dist::StepOutcome::Established => events += 1,
                beam_dist::StepOutcome::Failed | beam_dist::StepOutcome::Closed => events += 1,
                beam_dist::StepOutcome::WouldBlock => {}
            }
        }
        if self.driver.has_output() {
            self.flush_output()?;
            events += 1;
        }
        stepped?;
        Ok(events)
    }

    /// Decodes, **validates + classifies** and routes every control message this
    /// link has queued, returning one [`Dispatched`] record per message in arrival
    /// order.
    ///
    /// A thin forwarder to [`ConnectionDriver::dispatch_inbound_checked`]: the driver
    /// owns the queue, the receive atom cache **and the link's own identity** (its
    /// `peer_name()`/`our_name()`), so every delivery is validated against *this*
    /// link — the C's per-opcode `invalid_message` shape checks and the per-link
    /// `dep != external_pid_dist_entry(from)` anti-spoof check (`dist.c`) — while the
    /// sink owns the routing (see [`LocalDeliverySink`](crate::dist::LocalDeliverySink)).
    /// The link identity is interned into the **same** atom table the message was
    /// decoded against, so the node comparison is a raw term `==`.
    ///
    /// The identity check deliberately lives here rather than in the sink: one
    /// [`LocalDeliverySink`](crate::dist::LocalDeliverySink) is built per scheduler pass
    /// and shared by **every** link, so it has no peer to compare a `From` against; only
    /// the per-link driver does.
    ///
    /// An `Err` is a **link-level** failure — a decode/classification failure *or* a
    /// validation failure (a malformed message, or a forged one whose named node is
    /// not this link's peer) — and the caller retires the link and records the reason,
    /// exactly as the C kills the connection on `invalid_message`.
    pub(super) fn dispatch_inbound(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
        sink: &mut dyn InboundSink,
    ) -> Result<Vec<Dispatched>> {
        self.driver.dispatch_inbound_checked(heap, atoms, sink)
    }
}

/// A read that timed out (or would have blocked) is "no data", never an error.
///
/// Shared with the parent's `node` module (the non-blocking `accept`) and with the
/// module's unit suite (`src/dist/tests.rs`).
pub(super) fn is_would_block(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// A `shutdown(2)` whose socket is **already** gone — the answer a shut-down,
/// never-connected or peer-reset socket gives (`ENOTCONN` is
/// [`ErrorKind::NotConnected`] on Linux) — is the post-condition
/// [`DistConnection::close`] exists to establish, never a failure.
fn is_already_closed(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::NotConnected
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::BrokenPipe
    )
}
