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

//! The **distributed node** (contract A.5, Phase 1): a real `epmd` registration,
//! a bound distribution listener and the accepting half of the `'N'` handshake.
//!
//! This module is the runtime's *listening* half of Erlang distribution. It is
//! deliberately the smallest thing that makes `beam_rt` a **reachable node**:
//!
//! 1. **Bind** a distribution listener (`0.0.0.0:0`, the oracle's default
//!    wildcard bind; the OS picks the port).
//! 2. **Register** the node with `epmd` ([`beam_dist::epmd`]) over an `ALIVE2_REQ`
//!    carrying the *short* node name and the bound port. The `ALIVE2` socket is
//!    kept open for the node's whole life — closing it is what de-registers the
//!    node (`epmd_srv.c` `node_unreg_sock`). The `epmd` address is [`epmd_addr`], so
//!    `$ERL_EPMD_PORT` selects a private `epmd`.
//! 3. **Accept** inbound connections and drive each through
//!    [`beam_dist::ConnectionDriver`] as the **server** (`'N'` → `'s'`/`'S'` →
//!    `'r'` → `'a'`), so a remote node completes the handshake with this runtime.
//!
//! # Why the acceptor, not the connector
//!
//! A node is *registered* as soon as it has a listener: the **connecting** side
//! resolves the *acceptor's* port through `epmd` and opens the socket
//! (`dist.c`'s `net_kernel` connector + `erl_epmd`). So the runtime needs no
//! knowledge of a peer's port — the peer knows ours. The connector half (and the
//! `net_kernel` `{setup,…}`/`{nodeup,…}` protocol on top of an established link)
//! is Stage-5 work and is *not* implemented here; a link this runtime accepts is
//! handshaked and then carries an inbound `send`/`reg_send` into the addressed
//! local process's mailbox (see [`DistNode::dispatch_inbound`]).
//!
//! # Driving
//!
//! [`DistNode::pump`] is called from the runtime's scheduler pass
//! ([`crate::runtime::Runtime::pump_dist`]), next to the port half. The sockets
//! are **non-blocking** for `accept` and carry a short read timeout
//! ([`DIST_READ_SLICE`]) so one pump can never stall the scheduler: a pump reads
//! what has arrived, feeds it to the driver, writes the driver's reply and moves
//! on. No second poller is started — a node with no link costs one `accept`
//! (`WouldBlock`) per pass. [`DistNode::dispatch_inbound`] then drains what every
//! **established** link decoded and routes the `send` family into the mailboxes
//! (the scheduler pass's distribution-delivery half).
//!
//! # Honest limits (Milestone 1)
//!
//! * **Best-effort `epmd` registration.** No `epmd` (or a refused registration)
//!   leaves the node *bound but unregistered*: the listener still accepts, and
//!   the failure is recorded in [`DistNode::registration_error`] instead of
//!   breaking a runtime that merely passed `-sname`/`-name` (the differential
//!   `flags/*` cases must keep agreeing). Registration is attempted **once**,
//!   on the first pump.
//! * **`-sname`/`-name` are derived from the configured name.** A value
//!   containing `@` is a long name (used verbatim, as the oracle registers it); a
//!   bare value is a short name, so the node name is `<value>@<hostname>` and the
//!   `epmd` name is `<value>`. What goes on the **wire** is always the full
//!   `node@host` (see [`NodeName`]) — a bare advertised name is refused by a real
//!   peer. (`inet:gethostname/0`'s host is read from
//!   `/proc/sys/kernel/hostname`, falling back to `$HOSTNAME`.)
//! * **Inbound control messages are delivered for the message and signal families.** An
//!   **established** link's decoded `DOP_SEND*`/`DOP_REG_SEND*`/`DOP_ALIAS_SEND*` control
//!   messages — and the plain **message** form of `DOP_ALTACT_SIG_SEND` — are routed to the
//!   addressed **local** process's mailbox by [`DistNode::dispatch_inbound`] through the
//!   [`LocalDeliverySink`] (the runtime side of `beam_dist`'s [`InboundSink`] seam), as are
//!   the inbound **signal** families `DOP_LINK`, `DOP_EXIT*`, `DOP_UNLINK_ID[_ACK]`,
//!   `DOP_MONITOR_P_EXIT*` and `DOP_GROUP_LEADER` — each through `beam_process`'s route-free
//!   *receipt* primitives (see [`LocalDeliverySink`]'s type docs for the honest routed /
//!   partially-routed / unrouted split). What genuinely has no local seam —
//!   `DOP_MONITOR_P`/`DOP_DEMONITOR_P` and the distributed-spawn family — is reported as the
//!   honest [`DeliveryOutcome::Unsupported`], with the missing `beam_process` API named at its
//!   match arm; never a fabricated or mis-addressed delivery. A message the runtime
//!   cannot decode, validate or classify **retires that one link** (recorded in
//!   [`DistNode::last_link_error`]); it never fails a scheduler pass.
//! * **Every inbound delivery is validated against its own link.** Each link drains
//!   through [`ConnectionDriver::dispatch_inbound_checked`], so a delivery is checked
//!   against **that** link's peer/own identity (the C's per-opcode `invalid_message`
//!   shape checks plus the `dep != external_pid_dist_entry(from)` anti-spoof check,
//!   `dist.c`) before it can reach a mailbox. The check cannot live in the sink — one
//!   [`LocalDeliverySink`] is built per pass and shared by every link — so it is the
//!   per-link driver that performs it; a **validation failure is a link-level `Err`**,
//!   i.e. it retires (kills) that one link and is recorded in
//!   [`DistNode::last_link_error`], exactly as the C's `invalid_message` arm does. A
//!   forged `From` naming another node, or a malformed party term, is therefore never
//!   delivered and never merely ignored.
//! * **A broken link is dropped, not raised.** A protocol error, a write
//!   failure or a failed per-link inbound validation retires that one link and is
//!   recorded in [`DistNode::last_link_error`]; it never aborts the runtime.

use std::cell::RefCell;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::time::Duration;

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_dist::connection::TcpTransport;
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::epmd::{Alive2Req, Alive2Resp, EpmdClient, EPMD_DEFAULT_ADDR};
use beam_dist::{DeliveryOutcome, Dispatched, Dop, InboundSink, PeerVisibility};
use beam_dist::TcpListener;
use beam_term::prelude::{make_atom, Eterm};
use beam_term::AtomTable;

use crate::error;

mod inbound;
pub use inbound::LocalDeliverySink;

pub(crate) mod links;
pub(crate) use links::DistLinks;
pub use links::DistConnectionSnapshot;
/// The address the distribution listener binds: the wildcard on an OS-chosen
/// port, exactly the oracle's default (`inet_dist_use_interface` unset).
pub const DIST_LISTEN_ADDR: &str = "0.0.0.0:0";

/// The read timeout of an accepted link: one short slice per pump, so a link that
/// has nothing to say never stalls the scheduler pass (the same order of
/// magnitude as the port poller's slice).
pub const DIST_READ_SLICE: Duration = Duration::from_millis(25);

/// The largest number of connections accepted in one pump (the accept loop is
/// drained, but a flood is bounded per pass).
const MAX_ACCEPTS_PER_PUMP: usize = 8;

/// The scratch heap used to decode an inbound control message (words).
///
/// One decoded message at a time is materialised here; the delivery immediately
/// **copies** it into the receiver's own heap, so the arena is reset between
/// drains. It is sized to hold a full non-fragmented distribution message
/// ([`beam_dist::DIST_FRAGMENT_SIZE`]); a larger reassembled fragmented message
/// that does not fit retires the link with a recorded reason, exactly as any
/// other undecodable message does (see [`DistNode::last_link_error`]) — a bounded,
/// honest Milestone-1 limit, never a wrong delivery.
const DIST_DISPATCH_HEAP_WORDS: usize = 1 << 16;

/// The node's name, split into the three forms the distribution wire needs.
///
/// * `full` — the name this node *is* (`node@host`); what
///   `erlang:node/0` reports for a live node.
/// * `advertised` — the name put on the wire in `Name`/`Challenge`: **always** the
///   full `node@host`, for a long node (`-name`) *and* a short node (`-sname`) —
///   exactly what OTP sends. OTP's connecting side validates the name inside the
///   server's `'N'`/challenge message with `list_to_existing_atom(Ns)` and compares
///   it against the node it dialled, which is always the full `node@host`
///   (`dist_util.erl`'s `recv_challenge_new/2`; both `send_name/4` and
///   `send_challenge/2` put `atom_to_binary(Node)` on the wire). A bare short name
///   is therefore refused: the peer aborts the socket mid-handshake and the
///   accepting side observes a short read.
/// * `epmd` — the name `epmd` is told and a `PORT2_REQ` looks up: the short name
///   for a short node, the full name verbatim for a long node.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NodeName {
    /// The full `node@host` name.
    full: String,
    /// The name advertised in the `'N'`/`'s'` handshake messages: the full
    /// `node@host` name (short names are *not* advertised bare).
    advertised: Vec<u8>,
    /// The name registered with `epmd`.
    epmd: Vec<u8>,
    /// Whether this is a short name (`-sname`).
    short: bool,
}

impl NodeName {
    /// Splits a configured `-name`/`-sname` value into its wire forms.
    ///
    /// A value containing `@` is a long name (verbatim); a bare value is a short
    /// name, whose host part is this host's name (`inet:gethostname/0`). The
    /// **advertised** form is the full `node@host` name in both cases (only
    /// `epmd` is told the short name of a short node) — see [`NodeName`].
    pub fn parse(configured: &str) -> NodeName {
        if configured.contains('@') {
            NodeName {
                full: configured.to_string(),
                advertised: configured.as_bytes().to_vec(),
                epmd: configured.as_bytes().to_vec(),
                short: false,
            }
        } else {
            let full = format!("{configured}@{}", local_hostname());
            NodeName {
                advertised: full.as_bytes().to_vec(),
                full,
                epmd: configured.as_bytes().to_vec(),
                short: true,
            }
        }
    }

    /// The full `node@host` node name.
    pub fn full(&self) -> &str {
        &self.full
    }

    /// The name advertised in the handshake: the **full** `node@host` name, for a
    /// long node (`-name`) and a short node (`-sname`) alike.
    pub fn advertised(&self) -> &[u8] {
        &self.advertised
    }

    /// The name `epmd` registers and a `PORT2_REQ` looks up.
    pub fn epmd(&self) -> &[u8] {
        &self.epmd
    }

    /// Whether this is a short (`-sname`) name.
    pub fn is_short(&self) -> bool {
        self.short
    }
}

/// This host's name, the host part of a short node name — what
/// `inet:gethostname/0` answers.
fn local_hostname() -> String {
    if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let host = contents.trim();
        if !host.is_empty() {
            return host.to_string();
        }
    }
    match std::env::var("HOSTNAME") {
        Ok(host) if !host.is_empty() => host,
        _ => "localhost".to_string(),
    }
}

/// One live inbound distribution link and the driver that owns its protocol.
pub struct DistConnection {
    /// The accepting side's handshake/established state machine.
    driver: ConnectionDriver,
    /// The socket the driver's bytes travel over.
    stream: TcpStream,
    /// The peer's socket address (a diagnostic; the dist wire carries no address).
    peer_addr: SocketAddr,
    /// The peer's **node atom term** — the key a routed [`crate::dist::DistSignal`] addresses.
    ///
    /// `None` until [`DistNode::bind_peer_nodes`] interns [`DistConnection::peer_name`] into
    /// the runtime's live atom table; a link that never binds is simply not matched by the
    /// accepted half's lookup (see [`crate::dist::links`]).
    node: Option<Eterm>,
    /// Bytes read from the peer so far.
    bytes_in: u64,
    /// Bytes written to the peer so far.
    bytes_out: u64,
}

impl DistConnection {
    /// Wraps an accepted socket in a **server-side** driver advertising
    /// `advertised` as this node's name.
    fn new(stream: TcpStream, peer_addr: SocketAddr, advertised: &[u8], cookie: &[u8], creation: u32) -> Result<DistConnection> {
        stream.set_nodelay(true).map_err(BeamError::Io)?;
        stream
            .set_read_timeout(Some(DIST_READ_SLICE))
            .map_err(BeamError::Io)?;
        Ok(DistConnection {
            driver: ConnectionDriver::server(advertised, cookie, creation),
            stream,
            peer_addr,
            node: None,
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

    /// The peer's **node atom term**, once [`DistNode::bind_peer_nodes`] has interned
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
    /// The peer's node creation, once the handshake advertised it.
    pub fn peer_creation(&self) -> u32 {
        self.driver.peer_creation()
    }

    /// How the peer's own advertised flags classify it — [`PeerVisibility::Normal`]
    /// or [`PeerVisibility::Hidden`] (`dist_util.erl:publish_type/1`) — or `None`
    /// before the handshake completes.
    ///
    /// **Recorded, never refused.** Visibility is a *classification* of a peer's
    /// flags, not a permission this runtime could check: both ends of a real OTP
    /// connection may advertise themselves hidden (`dist_util.erl:publish_type/1`
    /// answers `hidden` for any peer that did not set `DFLAG_PUBLISHED`, and
    /// `-hidden` is a per-node choice), and this runtime itself advertises
    /// [`beam_dist::preferred_flags`], which omits that bit. A hidden peer is
    /// therefore the *expected* case for a hidden node and is accepted exactly like a
    /// normal one; the classification is only made observable here, per link.
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
    /// half's outbound path, reached through [`DistLinks::route_control`] so a peer that
    /// dialled *this* runtime is served exactly like one it dialled.
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
    /// A link is never left half-open: [`DistNode::retire`] and
    /// [`DistNode::pump`](DistNode::pump)'s dead-link branch shut the socket down
    /// before the connection drops, so the peer observes the closure immediately
    /// instead of holding a socket whose other end is merely unreferenced.
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
    fn pump(&mut self) -> Result<usize> {
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
    /// sink owns the routing (see [`LocalDeliverySink`]). The link identity is
    /// interned into the **same** atom table the message was decoded against, so the
    /// node comparison is a raw term `==`.
    ///
    /// The identity check deliberately lives here rather than in the sink: one
    /// [`LocalDeliverySink`] is built per scheduler pass and shared by **every** link,
    /// so it has no peer to compare a `From` against; only the per-link driver does.
    ///
    /// An `Err` is a **link-level** failure — a decode/classification failure *or* a
    /// validation failure (a malformed message, or a forged one whose named node is
    /// not this link's peer) — and the caller retires the link and records the reason,
    /// exactly as the C kills the connection on `invalid_message`.
    fn dispatch_inbound(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
        sink: &mut dyn InboundSink,
    ) -> Result<Vec<Dispatched>> {
        self.driver.dispatch_inbound_checked(heap, atoms, sink)
    }
}

/// A read that timed out (or would have blocked) is "no data", never an error.
fn is_would_block(err: &std::io::Error) -> bool {
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

/// The runtime's distribution node: a bound listener, its `epmd` registration
/// and every accepted link.
///
/// See the module docs for what this does and does not do in Milestone 1.
pub struct DistNode {
    /// The node's name in its three wire forms.
    name: NodeName,
    /// The `epmd`-assigned (or defaulted) node creation.
    creation: u32,
    /// The bound listen port.
    port: u16,
    /// The listener; non-blocking, so `accept` never stalls a pump.
    listener: std::net::TcpListener,
    /// The shared cookie the handshake digests use.
    cookie: Vec<u8>,
    /// The accepted links, **shared** with [`DistLinks`] — the one table both distribution
    /// halves route through (see [`links`]). The node pumps and mutates it; the routing
    /// entry only borrows it to write a frame.
    connections: Rc<RefCell<Vec<DistConnection>>>,
    /// The live `ALIVE2` socket — dropping it de-registers the node.
    registration: Option<EpmdClient<TcpTransport>>,
    /// Whether `epmd` accepted the registration.
    registered: bool,
    /// Why the registration did not happen (recorded, never swallowed).
    registration_error: Option<String>,
    /// Whether the (single) registration attempt has been made.
    registration_attempted: bool,
    /// Why the most recently retired link was dropped (recorded, never raised).
    last_link_error: Option<String>,
    /// The scratch arena an inbound control message is decoded on by
    /// [`DistNode::dispatch_inbound`] (reset per drain — the delivery copies the
    /// term out immediately). See [`DIST_DISPATCH_HEAP_WORDS`].
    dispatch_heap: Heap,
}

impl DistNode {
    /// Binds the distribution listener for `configured` (a `-name`/`-sname`
    /// value) with `cookie` and returns the node, **not yet registered**: the
    /// first [`pump`](Self::pump) publishes it to `epmd`.
    ///
    /// # Errors
    ///
    /// The listener's own failures (an unbindable address — the runtime then
    /// refuses to start rather than pretending to be distributed).
    pub fn start(configured: &str, cookie: Vec<u8>) -> Result<DistNode> {
        let name = NodeName::parse(configured);
        let listener = TcpListener::bind(DIST_LISTEN_ADDR)?;
        let port = listener.local_addr()?.port();
        // `beam_dist::TcpListener` wraps the standard listener but only exposes
        // `set_nonblocking`; the pump needs the standard one so an accept can be
        // polled without blocking a scheduler pass.
        let listener = listener.into_inner();
        listener.set_nonblocking(true).map_err(BeamError::Io)?;
        Ok(DistNode {
            name,
            creation: DEFAULT_CREATION,
            port,
            listener,
            cookie,
            connections: Rc::new(RefCell::new(Vec::new())),
            registration: None,
            registered: false,
            registration_error: None,
            registration_attempted: false,
            last_link_error: None,
            dispatch_heap: Heap::new(DIST_DISPATCH_HEAP_WORDS)?,
        })
    }

    /// The node's full `node@host` name.
    pub fn node_name(&self) -> &str {
        self.name.full()
    }

    /// The name advertised in the handshake: the full `node@host` name, for a
    /// `-sname` node as well as a `-name` node.
    pub fn advertised_name(&self) -> &[u8] {
        self.name.advertised()
    }

    /// The name `epmd` was told / a `PORT2_REQ` looks up.
    pub fn epmd_name(&self) -> &[u8] {
        self.name.epmd()
    }

    /// Whether this is a short (`-sname`) node.
    pub fn is_short_name(&self) -> bool {
        self.name.is_short()
    }

    /// The bound distribution listen port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The node's creation (the `epmd`-assigned value once registered).
    pub fn creation(&self) -> u32 {
        self.creation
    }

    /// Whether `epmd` accepted the registration.
    pub fn is_registered(&self) -> bool {
        self.registered
    }

    /// Whether the runtime is a live distributed node: registered with `epmd`
    /// (so a remote node can reach it) and listening.
    pub fn is_alive(&self) -> bool {
        self.registered
    }

    /// Why the registration did not happen — `None` when it succeeded or has not
    /// been attempted yet.
    pub fn registration_error(&self) -> Option<&str> {
        self.registration_error.as_deref()
    }

    /// The accepted links' address handle — the very cell the routing table
    /// ([`DistLinks`]) borrows, shared so an outbound `DOP_*` can reach a link this runtime
    /// **accepted** (see [`links`]).
    pub(crate) fn links(&self) -> Rc<RefCell<Vec<DistConnection>>> {
        Rc::clone(&self.connections)
    }

    /// Gives every **established** accepted link its peer's **node atom term**, interned
    /// into `atoms` — the binding that makes the accepted half of [`DistLinks`] an **exact**
    /// node match.
    ///
    /// A [`DistConnection`] learns its peer only as the `node@host` bytes its `'N'`
    /// handshake carried ([`DistConnection::peer_name`]), while a routed control addresses
    /// its peer by an **atom term**. Interned here, in the runtime's one live table, the
    /// link can then answer [`DistConnection::node_term`] with the very term the caller
    /// holds — so a control for a node with **no** link on either half is the typed
    /// refusal from the routing entry itself, instead of a queued control that a later
    /// flush silently drops.
    ///
    /// Called from the scheduler pass once the atom table is live
    /// ([`crate::runtime::Runtime::bind_dist_peers`]); it is idempotent — a link already
    /// bound is skipped, so a steady-state pass pays one borrow and one `is_some` per link.
    /// Returns how many links were bound by **this** call.
    ///
    /// # Errors
    ///
    /// The shared link table's re-entrant borrow (never in practice: the pass binds between
    /// the node's pump and the client's flush, holding neither).
    pub(crate) fn bind_peer_nodes(&mut self, atoms: &mut AtomTable) -> Result<usize> {
        let cell = self.links();
        let mut connections = cell
            .try_borrow_mut()
            .map_err(|_| error::message("distribution: the accepted link table is already in use"))?;
        let mut bound = 0;
        for connection in connections.iter_mut() {
            if connection.node_term().is_some() || !connection.is_established() {
                continue;
            }
            let Some(name) = connection.peer_name() else {
                // An established link whose handshake carried no readable peer name has
                // nothing to intern; it stays unbound (and therefore unmatched) rather
                // than being bound to a fabricated node.
                continue;
            };
            let atom = make_atom(atoms.intern(&name));
            connection.bind_node(atom);
            bound += 1;
        }
        Ok(bound)
    }
    /// A **diagnostic snapshot** of the accepted links, in accept order.
    ///
    /// The table is shared with the routing entry, so it cannot be lent out by reference;
    /// each entry is the link's observable state at the moment it was read. The snapshot is
    /// empty while the table is being mutated (a pump in flight) — it never affects routing.
    pub fn connections(&self) -> Vec<DistConnectionSnapshot> {
        DistConnectionSnapshot::of_table(&self.connections)
    }

    /// Whether the node has no live link.
    ///
    /// A cell that cannot be borrowed counts as holding a link (work in flight), so a
    /// scheduler pass never declares a stall it cannot justify.
    pub fn is_idle(&self) -> bool {
        DistLinks::new(self.links()).is_idle()
    }

    /// Accepts whatever connections are pending, advances every live link and
    /// registers with `epmd` on the first call. Returns how many protocol events
    /// took effect (an accepted link, a handshake step, bytes written).
    ///
    /// # Errors
    ///
    /// The accept path's own failures, and the shared link table's re-entrant borrow (see
    /// [`DistNode::links`]). A *link*'s failure never propagates: the link is retired and
    /// the reason recorded in [`last_link_error`](Self::last_link_error), so a hostile or
    /// broken peer can neither fail nor stall the scheduler.
    pub fn pump(&mut self) -> Result<usize> {
        self.register_once();
        let mut events = 0;
        // The cell is cloned out of `self` first, so the borrow of the table does not tie
        // `self` (which `accept_one`/`register_once` still need mutably).
        let cell = self.links();
        let mut connections = cell
            .try_borrow_mut()
            .map_err(|_| error::message("distribution: the accepted link table is already in use"))?;
        for _ in 0..MAX_ACCEPTS_PER_PUMP {
            match self.accept_one()? {
                Some(connection) => {
                    connections.push(connection);
                    events += 1;
                }
                None => break,
            }
        }
        let mut index = 0;
        while index < connections.len() {
            let outcome = connections[index].pump();
            match outcome {
                Ok(progress) => events += progress,
                Err(err) => {
                    // The link is broken (a protocol error, a dead socket). Retire
                    // it and keep the reason observable — the runtime carries on.
                    retire_at(&mut connections, &mut self.last_link_error, index, err);
                    continue;
                }
            }
            if connections[index].is_live() {
                index += 1;
            } else {
                // A terminal driver state (`Failed`/`Closed`) is a dead link: retire
                // it — which shuts its socket down first, so the peer sees the
                // closure instead of a half-open socket — and carry on.
                let err = BeamError::BadArg;
                retire_at(&mut connections, &mut self.last_link_error, index, err);
            }
        }
        Ok(events)
    }
    /// Whether at least one accepted link has completed the `'N'` handshake.
    ///
    /// The cheap guard the scheduler pass uses to skip the inbound-delivery half
    /// entirely (and never allocate the decode arena) for a node with no usable
    /// link — the common case. A table that cannot be borrowed counts as having one.
    pub fn has_established_link(&self) -> bool {
        DistLinks::new(self.links()).has_established()
    }
    /// Decodes, classifies and **routes** every control message queued on every
    /// **established** link through `sink` (the runtime's [`InboundSink`] — see
    /// [`LocalDeliverySink`]), returning how many were actually delivered to a
    /// local mailbox.
    ///
    /// This is the scheduler pass's **inbound-delivery half**, driven next to
    /// [`DistNode::pump`] once both the process table and the runtime are live (see
    /// `crate::runtime::Runtime::dispatch_dist_inbound`). The decode arena is reset
    /// per drain; the sink copies each delivered term into the receiver's own heap
    /// immediately.
    ///
    /// A **link-level** failure — a message this runtime cannot decode or classify
    /// (`beam_dist`'s `BadArg` from a truncated/malformed control message), **or** one
    /// that fails the per-link [`beam_dist::Delivery::classify_validated`] checks (a forged
    /// control message whose named node/party identity is not this link's own) —
    /// retires **that one link** and records the reason in
    /// [`DistNode::last_link_error`], exactly as [`DistNode::pump`] does and exactly
    /// as the C's `invalid_message` arm kills the connection (`dist.c`); it is never
    /// propagated, so a hostile or broken peer can neither fail nor stall a scheduler
    /// pass. The existing links (and their already-dispatched messages) are untouched.
    ///
    /// # Errors
    ///
    /// Never: the returned `Result` exists only to keep the scheduler-pass call
    /// site uniform (this method reports at most zero-link/zero-message progress).
    pub fn dispatch_inbound(
        &mut self,
        atoms: &mut AtomTable,
        sink: &mut dyn InboundSink,
    ) -> Result<usize> {
        // A fresh arena per drain: nothing decoded here outlives it (the delivery
        // copies out), so the previous drain's arena is reclaimed wholesale.
        self.dispatch_heap.reset();
        let mut delivered = 0;
        let cell = self.links();
        let mut connections = cell
            .try_borrow_mut()
            .map_err(|_| error::message("distribution: the accepted link table is already in use"))?;
        let mut index = 0;
        while index < connections.len() {
            if !connections[index].is_established() {
                index += 1;
                continue;
            }
            // The decode arena (`self.dispatch_heap`) and the link
            // (`connections[index]`) are separate objects, so both are borrowed
            // for the duration of the decode only.
            let outcome = {
                let heap = &mut self.dispatch_heap;
                connections[index].dispatch_inbound(heap, atoms, sink)
            };
            match outcome {
                Ok(dispatched) => {
                    // Only a real mailbox delivery can have woken a blocked
                    // receiver, so only those count towards the pass's `delivered`
                    // tally (`Resume`-on-send is the caller's next step).
                    delivered += dispatched
                        .iter()
                        .filter(|record| record.outcome == DeliveryOutcome::Delivered)
                        .count();
                    index += 1;
                }
                // A decode/classification failure retires just this link (recorded,
                // never raised) — the scheduler pass carries on.
                Err(err) => retire_at(&mut connections, &mut self.last_link_error, index, err),
            }
        }
        Ok(delivered)
    }

    /// Accepts one pending connection, or `None` when none is waiting.
    ///
    /// # Errors
    ///
    /// A genuine `accept` failure (the listener's own `Io` error).
    fn accept_one(&mut self) -> Result<Option<DistConnection>> {
        match self.listener.accept() {
            Ok((stream, peer_addr)) => {
                let connection = DistConnection::new(
                    stream,
                    peer_addr,
                    self.name.advertised(),
                    &self.cookie,
                    self.creation,
                )?;
                Ok(Some(connection))
            }
            Err(err) if is_would_block(&err) => Ok(None),
            Err(err) if err.kind() == ErrorKind::Interrupted => Ok(None),
            Err(err) => Err(BeamError::Io(err)),
        }
    }

    /// Publishes the node to `epmd` — once, on the first pump.
    ///
    /// A failure is **recorded**, never raised: a runtime that merely passed
    /// `-sname`/`-name` without an `epmd` daemon keeps working with a
    /// bound-but-unregistered listener (see the module docs).
    fn register_once(&mut self) {
        if self.registration_attempted {
            return;
        }
        self.registration_attempted = true;
        let request = Alive2Req {
            port: self.port,
            node_type: EPMD_NODE_NORMAL,
            protocol: EPMD_PROTO_TCP,
            high_vsn: EPMD_DIST_HIGH,
            low_vsn: EPMD_DIST_LOW,
            name: self.name.epmd().to_vec(),
            extra: Vec::new(),
        };
        let addr = epmd_addr();
        let mut client = match EpmdClient::connect(&addr) {
            Ok(client) => client,
            Err(err) => {
                self.registration_error = Some(format!("epmd unreachable ({addr}): {err}"));
                return;
            }
        };
        match client.publish(&request) {
            Ok(response) => {
                let (ok, creation) = match response {
                    Alive2Resp::Legacy { ok, creation } => (ok, u32::from(creation)),
                    Alive2Resp::Extended { ok, creation } => (ok, creation),
                };
                if ok {
                    self.creation = creation.max(1);
                    self.registered = true;
                    // Keeping the ALIVE2 socket is what keeps the registration:
                    // dropping it de-registers the node.
                    self.registration = Some(client);
                } else {
                    self.registration_error = Some("epmd refused the registration".to_string());
                }
            }
            Err(err) => self.registration_error = Some(format!("epmd refused: {err}")),
        }
    }

    /// The reason the most recently retired link was dropped, if any.
    pub fn last_link_error(&self) -> Option<&str> {
        self.last_link_error.as_deref()
    }
}

/// Retires `connections[index]` (removing it), shutting its socket down first and recording
/// why in `last_link_error`.
///
/// The socket is shut down **before** the connection drops, so a link is never left
/// half-open from the peer's point of view. The retirement reason stays the driver's (or the
/// socket's) failure; a `shutdown(2)` failure is appended rather than swallowed, since the
/// link is gone either way.
///
/// A free function rather than a method because the table is a borrowed `Rc<RefCell<…>>`
/// while the pump drains it (see [`links`]) — the borrow is already held, so the caller
/// passes the slice and the `last_link_error` slot instead of `&mut DistNode`.
fn retire_at(
    connections: &mut Vec<DistConnection>,
    last_link_error: &mut Option<String>,
    index: usize,
    err: BeamError,
) {
    let mut connection = connections.remove(index);
    let closed = connection.close();
    let reason = match connection.peer_name() {
        Some(peer) => format!("{peer}: {err}"),
        None => format!("{}: {err}", connection.peer_addr()),
    };
    *last_link_error = Some(match closed {
        Ok(()) => reason,
        Err(close_err) => format!("{reason} (close: {close_err})"),
    });
}
/// The creation a node advertises before `epmd` has assigned one.
const DEFAULT_CREATION: u32 = 1;

// The `epmd` node type / protocol / version vocabulary a normal tcp/ip node
// registers with (`epmd.h`), re-exported by the peer protocol crate so the two
// can never drift.
use beam_dist::epmd::{EPMD_DIST_HIGH, EPMD_DIST_LOW, EPMD_NODE_NORMAL, EPMD_PROTO_TCP};

/// The `epmd` address this node registers with: `$ERL_EPMD_PORT` when set, else
/// the well-known [`EPMD_DEFAULT_ADDR`] (`127.0.0.1:4369`).
///
/// OTP honours `ERL_EPMD_PORT` so a node can use a private `epmd` (or an `epmd` on a
/// non-default port), and this helper is the runtime's one reading of it: it drives
/// the node's [`DistNode::register_once`] `ALIVE2` registration here, and it is the
/// address the connecting half's `PORT2_REQ` lookups resolve a peer's port on (see
/// `crate::dist_client`), so a node and its lookups always agree on which `epmd`
/// they speak to.
///
/// * a bare number `N` is a **port** on the local loopback (`127.0.0.1:N`, the
///   `ERL_EPMD_PORT=4370` form);
/// * a value containing `:` is a `host:port` pair and is used verbatim (what
///   [`beam_dist::epmd::EpmdClient::connect`] accepts);
/// * an unset or empty value is [`EPMD_DEFAULT_ADDR`].
pub fn epmd_addr() -> String {
    match std::env::var("ERL_EPMD_PORT") {
        Ok(value) => epmd_addr_from(Some(value.as_str())),
        Err(_) => epmd_addr_from(None),
    }
}

/// [`epmd_addr`]'s single definition over an explicit value — the testable core,
/// kept pure because the process environment is global (surrounding whitespace is
/// trimmed, so an empty or blank value is "unset").
fn epmd_addr_from(value: Option<&str>) -> String {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return EPMD_DEFAULT_ADDR.to_string();
    }
    if value.contains(':') {
        value.to_string()
    } else {
        format!("127.0.0.1:{value}")
    }
}

/// Resolves the cookie the handshake digests use: `-setcookie` from the command
/// line, else `$HOME/.erlang.cookie`, else the documented Milestone-1 default.
pub fn resolve_cookie(init_args: &crate::config::InitArgs) -> Vec<u8> {
    if let Some(occurrences) = init_args.get("setcookie") {
        if let Some(values) = occurrences.first() {
            if let Some(value) = values.first() {
                return value.as_bytes().to_vec();
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = std::path::Path::new(&home).join(".erlang.cookie");
        if let Ok(contents) = std::fs::read_to_string(path) {
            let cookie = contents.trim();
            if !cookie.is_empty() {
                return cookie.as_bytes().to_vec();
            }
        }
    }
    DEFAULT_COOKIE.to_vec()
}

/// The cookie used when neither `-setcookie` nor a cookie file is available.
///
/// OTP's own `auth.c` reads `$HOME/.erlang.cookie` and fails distribution when it
/// is missing; this runtime instead uses a fixed, documented value, so a node
/// started without a cookie is reachable only by a peer that agrees on the same
/// default (a wrong cookie simply fails the handshake — never a false success).
const DEFAULT_COOKIE: &[u8] = b"nocookie";

/// The fixed local-node name of a runtime with **no** node (`-name`/`-sname` absent).
///
/// The distribution decoder materialises an addressed pid as an **external** pid
/// whose node slot is the peer's atom, so a runtime that is not a node needs *some*
/// node name to compare against. This is the same identity
/// [`crate::context`]'s node bridge hands `beam_codec` — the oracle's
/// `nonode@nohost` (it is seeded into the core atom table at boot), with creation
/// `0`. A pid addressed to any real node is therefore a foreign process to a
/// non-node runtime, exactly as it should be.
pub const NONODE_NAME: &str = "nonode@nohost";

#[cfg(test)]
mod tests {
    use super::*;

    /// A short (`-sname`) node advertises the **full** `node@host` on the wire while
    /// telling `epmd` the short name: OTP's connecting side compares the challenge
    /// name against the node it dialled (`dist_util.erl`'s `recv_challenge_new/2`),
    /// so a bare advertised name makes a real peer abort the socket.
    #[test]
    fn a_short_name_advertises_the_full_node_name() {
        let name = NodeName::parse("mynode");
        assert!(name.is_short());
        assert_eq!(name.full(), format!("mynode@{}", local_hostname()));
        assert_eq!(
            name.advertised(),
            name.full().as_bytes(),
            "the advertised name is the full `node@host`, never the bare short name"
        );
        assert_eq!(name.epmd(), b"mynode", "epmd is told the short name");
    }

    /// A long (`-name`) node advertises and registers its name verbatim.
    #[test]
    fn a_long_name_advertises_and_registers_verbatim() {
        let name = NodeName::parse("mynode@some.host.example");
        assert!(!name.is_short());
        assert_eq!(name.full(), "mynode@some.host.example");
        assert_eq!(name.advertised(), b"mynode@some.host.example");
        assert_eq!(name.epmd(), b"mynode@some.host.example");
    }

    use beam_dist::{preferred_flags, publish_flag, AcceptDecision, DistFlags, Status};

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
    /// refused: here a published client and this runtime's own hidden
    /// ([`preferred_flags`]) acceptor complete the handshake, each classifying the
    /// other.
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
            "this runtime advertises `preferred_flags`, which omits `DFLAG_PUBLISHED`"
        );
        // The rest of the peer identity stays truthful.
        assert_eq!(server.peer_name().as_deref(), Some("peer@host.example"));
        assert_eq!(client.peer_name().as_deref(), Some("acceptor@host.example"));
        assert_eq!(server.peer_creation(), 1);
        assert_eq!(client.peer_creation(), 1);
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

        let mut atoms = beam_term::prelude::AtomTable::new();
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
}
