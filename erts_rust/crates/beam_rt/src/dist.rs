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
//!    node (`epmd_srv.c` `node_unreg_sock`).
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

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::term::external_ref_parts;
use beam_common::{BeamError, Result};
use beam_dist::connection::TcpTransport;
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::epmd::{Alive2Req, Alive2Resp, EpmdClient, EPMD_DEFAULT_ADDR};
use beam_dist::{AltactSigFlags, Delivery, DeliveryOutcome, Dispatched, InboundSink};
use beam_dist::TcpListener;
use beam_process::{MonitorKind, MonitorRef, PidTarget, ProcessId, ProcessTable, RemotePid, RemoteRef};
use beam_term::prelude::{
    atom_val, is_atom, is_immediate, is_small, make_atom, signed_value, Atom, Eterm,
};
use beam_term::AtomTable;

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
            bytes_in: 0,
            bytes_out: 0,
        })
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

    /// The peer's node creation, once the handshake advertised it.
    pub fn peer_creation(&self) -> u32 {
        self.driver.peer_creation()
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

    /// Reads whatever has arrived, advances the handshake and writes the
    /// driver's reply. Returns how many protocol events took effect.
    ///
    /// A read timeout ([`DIST_READ_SLICE`], or a clean `WouldBlock`) is not an
    /// event and not an error: the link simply had nothing to say.
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
        let outcome = self.driver.step()?;
        match &outcome {
            beam_dist::StepOutcome::Progress
            | beam_dist::StepOutcome::Handshaking
            | beam_dist::StepOutcome::Established => events += 1,
            beam_dist::StepOutcome::Failed | beam_dist::StepOutcome::Closed => events += 1,
            beam_dist::StepOutcome::WouldBlock => {}
        }
        if self.driver.has_output() {
            let bytes = self.driver.take_output();
            self.stream.write_all(&bytes).map_err(BeamError::Io)?;
            self.stream.flush().map_err(BeamError::Io)?;
            self.bytes_out += bytes.len() as u64;
            events += 1;
        }
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
    /// The accepted links.
    connections: Vec<DistConnection>,
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
            connections: Vec::new(),
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

    /// The accepted links, in accept order.
    pub fn connections(&self) -> &[DistConnection] {
        &self.connections
    }

    /// Whether the node has no live link.
    pub fn is_idle(&self) -> bool {
        self.connections.is_empty()
    }

    /// Accepts whatever connections are pending, advances every live link and
    /// registers with `epmd` on the first call. Returns how many protocol events
    /// took effect (an accepted link, a handshake step, bytes written).
    ///
    /// # Errors
    ///
    /// Only the accept path's own failures. A *link*'s failure never propagates:
    /// the link is retired and the reason recorded in
    /// [`last_link_error`](Self::last_link_error), so a hostile or broken peer
    /// can neither fail nor stall the scheduler.
    pub fn pump(&mut self) -> Result<usize> {
        self.register_once();
        let mut events = 0;
        for _ in 0..MAX_ACCEPTS_PER_PUMP {
            match self.accept_one()? {
                Some(connection) => {
                    self.connections.push(connection);
                    events += 1;
                }
                None => break,
            }
        }
        let mut index = 0;
        while index < self.connections.len() {
            let outcome = self.connections[index].pump();
            match outcome {
                Ok(progress) => events += progress,
                Err(err) => {
                    // The link is broken (a protocol error, a dead socket). Retire
                    // it and keep the reason observable — the runtime carries on.
                    self.retire(index, err);
                    continue;
                }
            }
            if self.connections[index].is_live() {
                index += 1;
            } else {
                let err = BeamError::BadArg;
                self.retire(index, err);
            }
        }
        Ok(events)
    }

    /// Retires the link at `index` (removing it) and records why.
    fn retire(&mut self, index: usize, err: BeamError) {
        let connection = self.connections.remove(index);
        self.last_link_error = Some(match connection.peer_name() {
            Some(peer) => format!("{peer}: {err}"),
            None => format!("{}: {err}", connection.peer_addr()),
        });
    }

    /// Whether at least one accepted link has completed the `'N'` handshake.
    ///
    /// The cheap guard the scheduler pass uses to skip the inbound-delivery half
    /// entirely (and never allocate the decode arena) for a node with no usable
    /// link — the common case.
    pub fn has_established_link(&self) -> bool {
        self.connections.iter().any(DistConnection::is_established)
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
    /// that fails the per-link [`Delivery::classify_validated`] checks (a forged
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
        let mut index = 0;
        while index < self.connections.len() {
            if !self.connections[index].is_established() {
                index += 1;
                continue;
            }
            // The decode arena (`self.dispatch_heap`) and the link
            // (`self.connections[index]`) are disjoint fields, so both are borrowed
            // for the duration of the decode only.
            let outcome = {
                let heap = &mut self.dispatch_heap;
                self.connections[index].dispatch_inbound(heap, atoms, sink)
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
                Err(err) => self.retire(index, err),
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
        let mut client = match EpmdClient::connect(EPMD_DEFAULT_ADDR) {
            Ok(client) => client,
            Err(err) => {
                self.registration_error = Some(format!("epmd unreachable: {err}"));
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

/// The creation a node advertises before `epmd` has assigned one.
const DEFAULT_CREATION: u32 = 1;

// The `epmd` node type / protocol / version vocabulary a normal tcp/ip node
// registers with (`epmd.h`), re-exported by the peer protocol crate so the two
// can never drift.
use beam_dist::epmd::{EPMD_DIST_HIGH, EPMD_DIST_LOW, EPMD_NODE_NORMAL, EPMD_PROTO_TCP};

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

// ---------------------------------------------------------------------------
// Inbound delivery: the runtime side of `beam_dist`'s `InboundSink` seam
// ---------------------------------------------------------------------------

/// The runtime's [`InboundSink`]: routes a classified inbound [`Delivery`] from a
/// distribution link to the addressed **local** process.
///
/// This is `beam_dist`'s "production implementation belongs to `beam_rt`" seam
/// (`beam_dist::delivery`): the value/protocol crate classifies the control
/// message, this sink resolves the addressee against *this* runtime's process
/// table and enqueues the message through `beam_process`'s own delivery path (so a
/// blocked receiver is woken on the table's wake list and resumed by the
/// scheduler, exactly as a port or timer message wakes it).
///
/// # Scope (honest)
///
/// **Routed** — the ordinary message family, which is what a peer's `!`/`erlang:send`
/// produces:
///
/// * [`Delivery::Send`] (`DOP_SEND*`) to a local pid — an internal pid immediate, or
///   the **external** pid shape the distribution decoder materialises for a pid on this
///   node (`dist.c:2512-2561`);
/// * [`Delivery::RegSend`] (`DOP_REG_SEND*`) to a registered local name
///   (`dist.c:2463-2510`);
/// * [`Delivery::AliasSend`] (`DOP_ALIAS_SEND*`) to a live local alias — a process
///   reference alias (`dist.c:2563-2588`; `erl_proc_sig_queue.c:2318-2326`,
///   `:6036-6049`), with the C's `reply_demonitor` alias consumed when the message is
///   really delivered;
/// * the same three targets when they arrive as the **message** form of
///   [`Delivery::AltactSigSend`] (`DOP_ALTACT_SIG_SEND` with neither
///   `ERTS_DOP_ALTACT_SIG_FLG_PRIO` nor `..._EXIT`, `dist.c:2590-2697`).
///
/// **Routed** — the inbound *signal* families, through the route-free **receipt**
/// primitives `beam_process` exposes (each installs/drops the local record and echoes
/// nothing, the C's `break; /* Done */`):
///
/// * [`Delivery::Link`] (`DOP_LINK`, `dist.c:2224`);
/// * [`Delivery::Exit`]/[`Delivery::Exit2`] and their payload forms
///   (`DOP_EXIT`/`DOP_EXIT2`, `dist.c:2760`/`:2824`), with `untrappable` selecting the
///   link-exit vs `erlang:exit/2` family;
/// * [`Delivery::UnlinkIdAck`] (the ack **consumption** half of the OTP-26+ handshake);
/// * [`Delivery::Monitor`]/[`Delivery::Demonitor`] (`DOP_MONITOR_P`/`DOP_DEMONITOR_P`,
///   `dist.c:2341`/`:2415`): a peer's monitor of a local pid-or-name is recorded as a
///   [`RemoteWatcher`](beam_process::RemoteWatcher) keyed by the whole **peer-minted wire
///   reference** ([`RemoteRef`](beam_process::RemoteRef)), and its demonitor removes exactly
///   that record (see [`LocalDeliverySink::monitor`]);
/// * [`Delivery::MonitorExit`]/[`Delivery::PayloadMonitorExit`]
///   (`DOP_MONITOR_P_EXIT`, `dist.c:2700-2714`) for a DOWN whose reference is one
///   **this** node minted (see [`LocalDeliverySink::wire_monitor_ref_id`]);
/// * [`Delivery::GroupLeader`] (`DOP_GROUP_LEADER`, `dist.c:2889`).
///
/// **Partially routed** — [`Delivery::UnlinkId`]: the local link-record removal IS
/// applied, but an `id != 0` additionally owes a `DOP_UNLINK_ID_ACK` this sink has no
/// channel to emit, so that case reports the typed
/// [`DeliveryOutcome::Unsupported`] for the wire half (see its arm).
///
/// **Not routed** — the distributed-spawn family and [`Delivery::Other`] are reported as
/// [`DeliveryOutcome::Unsupported`], with the still-missing sibling API named at each
/// match arm. (The `DOP_MONITOR_P_EXIT`-on-local-death echo a recorded
/// [`RemoteWatcher`](beam_process::RemoteWatcher) would owe its peer is a separate
/// **outbound** concern, still unimplemented and out of this sink's scope.)
///
/// Every unrouted arm is *structural*, not incidental: routing one through the nearest
/// *locally-initiated* `beam_process` entry point (`link_term`/`monitor_term`/`…`) would
/// emit an outbound `DOP_*` the C's inbound receipt arm never sends. Guessing at the
/// semantics — or echoing the mirrored opcode — is far worse than an honest gap.
///
/// Nothing is ever delivered to the *wrong* process: an addressed pid that is not
/// a process of this node (another node, or a stale incarnation of this one) is
/// [`DeliveryOutcome::NoSuchProcess`], never a lookup by number alone; a delivery
/// outside its opcode's wire contract (a non-pid `DOP_SEND` target, a non-atom
/// `DOP_REG_SEND` name, a non-reference `DOP_ALIAS_SEND` target) is
/// [`DeliveryOutcome::Unsupported`]; and a well-formed delivery whose recipient does
/// not exist is dropped ([`DeliveryOutcome::NoSuchProcess`] /
/// [`DeliveryOutcome::Dropped`]) exactly as the C's silent-drop arms do.
pub struct LocalDeliverySink<'a> {
    /// The runtime's process table the delivery is enqueued into.
    table: &'a mut ProcessTable,
    /// This node's full `node@host` name, as the interned atom term a decoded
    /// external pid carries in its node slot.
    local_node: Eterm,
    /// This node's creation (the incarnation the pid must belong to).
    local_creation: u32,
}

impl<'a> LocalDeliverySink<'a> {
    /// A sink over `table` for a node named by `local_node` (its interned atom) with
    /// the given `local_creation`.
    pub fn new(table: &'a mut ProcessTable, local_node: Atom, local_creation: u32) -> Self {
        LocalDeliverySink {
            table,
            local_node: make_atom(local_node),
            local_creation,
        }
    }
}

impl InboundSink for LocalDeliverySink<'_> {
    fn deliver(&mut self, delivery: &Delivery) -> Result<DeliveryOutcome> {
        match delivery {
            // --- routed: the ordinary message family (see the type docs) -----
            Delivery::Send {
                from,
                to,
                message,
                ..
            } => self.deliver_to_pid(*from, *to, *message),
            Delivery::RegSend {
                from,
                name,
                message,
                ..
            } => self.deliver_to_name(*from, *name, *message),
            Delivery::AliasSend {
                from,
                to,
                message,
                ..
            } => self.deliver_to_alias(*from, *to, *message),
            Delivery::AltactSigSend {
                flags,
                from,
                to,
                message,
                ..
            } => self.deliver_altact(*flags, *from, *to, *message),

            // --- routed: the link / exit / unlink / monitor-down signal family --
            //
            // Every family below goes through the **route-free receipt** half of the
            // distribution seam — the `ProcessTable` entry points documented under
            // "inbound (receipt) distribution primitives" (`ptab.rs`), each of which
            // installs (or drops) the local record and then follows the C's
            // `break; /* Done */` (`dist.c:2258`), never echoing the opcode it
            // received. Routing them through the nearest *locally-initiated* entry
            // point (`link_term`/`unlink_term`/`monitor_term`/`demonitor`/
            // `send_exit_term`) would instead put a `DOP_*` the C's inbound arm never
            // sends on the wire through the installed `DistSignalRouter`.
            Delivery::Link { from, to } => self.record_link(*from, *to),

            // Exit signals from a remote sender (`DOP_EXIT`/`DOP_EXIT2`, `dist.c:2760`
            // /`:2824`, and their payload forms). The C family selects the trap rule,
            // and `untrappable` carries exactly that split: `DOP_EXIT`/
            // `DOP_PAYLOAD_EXIT` are **link exits** (the recorded incoming remote link
            // is removed first, and with no such link the signal is ignored outright —
            // `erl_proc_sig_queue.c:4674-4680`), while `DOP_EXIT2`/`DOP_PAYLOAD_EXIT2`
            // are the untrappable `erlang:exit/2` family.
            //
            // The C answers a dead `to` with a `DOP_EXIT` carrying `noproc`
            // (`dist.c:2269-2275`); that reply has no channel on this sink, so a
            // non-live `to` is the typed `NoSuchProcess` instead (see the type docs).
            Delivery::Exit { from, to, reason, .. } => {
                self.exit_signal(*from, *to, *reason, false)
            }
            Delivery::Exit2 { from, to, reason, .. } => {
                self.exit_signal(*from, *to, *reason, true)
            }
            Delivery::PayloadExit { from, to, payload, .. } => {
                self.exit_signal(*from, *to, *payload, false)
            }
            Delivery::PayloadExit2 { from, to, payload, .. } => {
                self.exit_signal(*from, *to, *payload, true)
            }

            // The OTP-26+ unlink-id handshake (`DOP_UNLINK_ID`/`DOP_UNLINK_ID_ACK`,
            // `dist.c:2284`/`:2315`).
            //
            // `DOP_UNLINK_ID_ACK` **consumes** an ack: it resolves a pending unlink
            // this node initiated, so it owes no reply and is `Delivered` once `to` is
            // a live local process. (`unlink_ack_from_remote` is the route-free
            // counterpart of `unlink_term`.) The pending-op bookkeeping it would
            // resolve does not exist: `beam_rt`'s `dist_client` mints the unlink id on
            // the fly and keeps no record of it — a gap *there*, recorded in
            // `../CONTEXT.md`, not a routing gap here.
            //
            // `DOP_UNLINK_ID` is the half that **must answer**. The C removes the link
            // and then sends `DOP_UNLINK_ID_ACK` unconditionally
            // (`reply_dist_unlink_ack`, `erl_proc_sig_queue.c:2753-2760`; nothing is
            // sent only when `id == 0`). The local link-record removal IS applied
            // (`unlink_from_remote`), but the `ACK` **cannot be emitted**: `InboundSink`
            // carries no reply channel and the drains count only `Delivered` (see the
            // type docs), so this reports the typed `Unsupported` for the wire half
            // rather than claiming an ack the runtime cannot produce. The `id == 0`
            // form owes no ack and is `Delivered`.
            Delivery::UnlinkId { id, from, to } => self.unlink_id(*id, *from, *to),
            Delivery::UnlinkIdAck { id, from, to } => self.unlink_ack(*id, *from, *to),

            // A **remote watcher** monitoring a local process/name
            // (`DOP_MONITOR_P`/`DOP_DEMONITOR_P`, `dist.c:2341`/`:2415`).
            //
            // The C's inbound arms only install (respectively, remove) the **local record**:
            // `DOP_MONITOR_P` creates the monitor and queues the local monitor signal, then
            // `break; /* done */` (`dist.c:2341-2381`), and `DOP_DEMONITOR_P` sends the local
            // demonitor signal and answers nothing (`dist.c:2415-2460`). Neither echoes its own
            // opcode, so neither may go through a *locally-initiated* entry point
            // (`monitor_term`/`demonitor`, which route a `DOP_*`).
            //
            // The reference a **peer** puts on the wire is one *it* minted — a full external
            // reference whose identity is `(node, creation, ids[0..n])`. It is decomposed into a
            // `RemoteRef` and the record is keyed by the whole identity, so two distinct peer
            // monitors of one local process never collapse (a replaced watcher, or a `demonitor`
            // that cancels the wrong monitor). `beam_process::RemoteRef::from_eterm` is the safe
            // decoder — the `unsafe` block reader stays confined to `beam_process` — and a term
            // that is not a well-formed external reference is outside this opcode's wire contract:
            // `Unsupported`.
            Delivery::Monitor {
                watcher,
                watched,
                reference,
            } => self.monitor(*watcher, *watched, *reference),
            Delivery::Demonitor {
                watcher,
                watched,
                reference,
            } => self.demonitor(*watcher, *watched, *reference),

            // A monitor **DOWN** from a remote target (`DOP_MONITOR_P_EXIT` /
            // `DOP_PAYLOAD_MONITOR_P_EXIT`, `dist.c:2700-2714`). This is the **inverse**
            // of a monitor *this* node initiated: the reference on the wire is the one
            // `dist_client` put into the outgoing `DOP_MONITOR_P` (an external reference
            // on **our** node, `{our_node, our_creation, [0, id >> 32, id]}`), which the
            // peer echoes back verbatim. `wire_monitor_ref_id` therefore recovers the
            // very `MonitorRef` the outgoing record was keyed by, so the recorded
            // `RemoteMonitor` is found and the DOWN matches its monitor exactly.
            //
            // A DOWN whose `watched` is a registered **name** (the C's `dist.c:2722`
            // accepts an external pid *or* an atom) is not representable: the receipt
            // API takes the resolved `RemotePid`. That stays a typed `Unsupported`.
            Delivery::MonitorExit {
                watched,
                watcher,
                reference,
                reason,
            } => self.monitor_down(*watched, *watcher, *reference, *reason),
            Delivery::PayloadMonitorExit {
                watched,
                watcher,
                reference,
                payload,
            } => self.monitor_down(*watched, *watcher, *reference, *payload),

            // `DOP_GROUP_LEADER` (`dist.c:2889`): installs the remote `from` as the
            // local `to`'s group leader. `set_group_leader_from_remote` records it as a
            // `PidTarget::Remote` — readable back through `ProcessTable::group_leader`
            // — and routes nothing, exactly as the C's arm does.
            Delivery::GroupLeader { from, to } => self.group_leader(*from, *to),

            // The distributed-spawn family (`DOP_SPAWN_REQUEST`/`DOP_SPAWN_REPLY`,
            // `dist.c:2902-3093`): a `beam_rt` CLI process neither serves spawn
            // requests nor holds spawn reservations, so there is no responder. Needs a
            // `beam_process` spawn-responder/reservation API — a `DOP_SPAWN_REPLY`
            // sender plus the reservation bookkeeping a `DOP_SPAWN_REQUEST` would key
            // into — of which none exists.
            Delivery::SpawnRequest { .. } | Delivery::SpawnReply { .. } => {
                Ok(DeliveryOutcome::Unsupported)
            }

            // `DOP_UNLINK` is not a v6 opcode the C accepts at all: it falls into the
            // `invalid_message` arm and **kills the connection** (`dist.c:2277-2282`).
            // A `DOP_UNLINK` reaching this sink is therefore a protocol violation; the
            // caller (the link driver) owns the connection-level answer, so the sink
            // reports the honest "recognised, not routed" and never a fabricated one.
            Delivery::Other(_) => Ok(DeliveryOutcome::Unsupported),
        }
    }
}

/// The mailbox **envelope**'s sender word for a message classified off the wire.
///
/// [`ProcessTable::deliver_simple`] stores `from` in the queue entry's envelope, and
/// that slot must be an **immediate**: `beam_message` gathers only each queued
/// message's *payload* as a GC root, so a non-immediate word left there is not
/// relocated and dangles into the evacuated from-space the moment the owning process
/// collects (`beam_message/src/message.rs`'s `debug_assert_enqueue_terms`). An
/// external pid classified off the wire is exactly such a word: the distribution
/// decoder materialises it in the dispatch arena, which [`DistNode::dispatch_inbound`]
/// **resets on every drain** — and `Runtime::dispatch_inbound`'s caller-supplied arena
/// is reset by its caller. Passing the classified `from` straight through therefore
/// stored a dangling envelope.
///
/// The C's own rule is exact (`erl_message.c:307`): `ERL_MESSAGE_FROM(mp) =
/// dist_ext->dep->sysname` — the **peer node atom**, never the sender pid (the pid is
/// used only for per-sender signal ordering, `erl_message.c:324-329`). This derives
/// exactly that from an external-pid sender's node slot: an atom term, i.e. an
/// immediate, safe to hold for the message's whole queued life. The plain `DOP_SEND*`
/// forms need no derivation — the peer puts the empty atom `''` there
/// (`dist.c:2535-2542`) — and an internal pid is already an immediate.
///
/// `None` means no faithful immediate word exists: a boxed sender that is neither an
/// internal nor an external pid (outside both opcodes' wire contract) or a
/// pathological external pid whose node slot is not an atom immediate. The delivery is
/// then refused rather than enqueued with a term that would dangle.
///
/// **Typed divergence from the C:** for a plain `DOP_SEND*` the C still stores the peer's
/// *sysname*, while this keeps the wire's `''`. Both are immediates and neither is ever
/// read as a process identity by this runtime (nothing here resolves a mailbox
/// envelope), so the divergence is inert — and deriving the sysname would need the
/// peer's node name plumbed down to the sink, which this seam does not carry.
fn envelope_from(from: Eterm) -> Option<Eterm> {
    if is_immediate(from) {
        return Some(from);
    }
    match PidTarget::from_eterm(from) {
        Ok(PidTarget::Remote(remote)) => {
            let node = remote.node();
            // The pid block's node slot is an atom term (an immediate); refuse anything
            // else rather than store a word the GC walk cannot keep alive.
            (is_immediate(node) && is_atom(node)).then_some(node)
        }
        _ => None,
    }
}

/// Where an addressed pid term points (see [`LocalDeliverySink::resolve`]).
enum Target {
    /// A local process in this node's table.
    Local(ProcessId),
    /// Not a process of *this* node (another node, or a stale incarnation).
    Foreign,
    /// Not a pid at all — outside the wire contract for this opcode.
    NotAPid,
}

impl LocalDeliverySink<'_> {
    /// Routes a `DOP_SEND*` to the addressed local pid.
    ///
    /// The C's receipt (`dist.c:2512-2561`) rejects a non-pid `to` as an
    /// `invalid_message` and **silently drops** a message whose `to` is not a live local
    /// process (a dead pid, or a stale incarnation of this node) — it answers the peer
    /// with nothing at all.
    fn deliver_to_pid(
        &mut self,
        from: Eterm,
        to: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        match self.resolve(to) {
            Target::Local(pid) => self.enqueue(pid, from, message),
            Target::Foreign => Ok(DeliveryOutcome::NoSuchProcess),
            Target::NotAPid => Ok(DeliveryOutcome::Unsupported),
        }
    }

    /// Routes a `DOP_REG_SEND*` to the addressed registered local name.
    fn deliver_to_name(
        &mut self,
        from: Eterm,
        name: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        if !is_atom(name) {
            // `DOP_REG_SEND`'s `ToName` is an atom (`dist.c:2486`); anything else is
            // outside the contract for this opcode.
            return Ok(DeliveryOutcome::Unsupported);
        }
        match self.table.whereis(atom_val(name)) {
            Some(pid) => self.enqueue(pid, from, message),
            // A name nobody holds drops the message (the oracle drops it too — it
            // answers the peer with nothing, `dist.c:2506-2509`). A name registered to
            // a *port* is the same silent drop in the C, and `ProcessTable::whereis`
            // resolves process names only, so it lands here too.
            None => Ok(DeliveryOutcome::NoSuchProcess),
        }
    }

    /// Routes a `DOP_ALIAS_SEND*` through this node's **reference alias** table.
    ///
    /// The C's receipt is `erts_proc_sig_send_dist_altact_msg` (`dist.c:2563-2588`
    /// → `erl_proc_sig_queue.c:2293-2412`): `To` must be a reference (`is_not_ref(to)`
    /// ⇒ `invalid_message`), the reference's own embedded owner pid must be an
    /// **internal** pid — a reference whose owner is not local is dropped outright
    /// (`:2318-2326`) — and the alias is then resolved inside the owner's monitor tree,
    /// where a reference that is not a live alias is **silently dropped**
    /// (`:6036-6046`: *"Not an alias (never has been, not anymore, or not yet); drop
    /// message…"*). The peer is answered with nothing in every one of those cases.
    ///
    /// A message really delivered through an `ALIAS_ONCE` (`reply_demonitor`) alias
    /// **consumes** it (`erl_proc_sig_queue.c:6047-6064`), so that is what happens here —
    /// after, and only after, a successful enqueue.
    fn deliver_to_alias(
        &mut self,
        from: Eterm,
        to: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        let Ok(reference) = MonitorRef::from_eterm(to) else {
            // Not an internal reference thing: outside the opcode's contract
            // (`dist.c:2577`), so neither a delivery nor a drop is claimed for it.
            //
            // A wire-borne reference currently arrives in the **external** form — the
            // control-message decoder installs no local-node identity
            // (`beam_dist::control::decode_control`), so the codec's local-node
            // demotion never runs — and an external reference is not a readable alias
            // key here. That is a `beam_dist` handoff, recorded rather than worked
            // around: see the module's escalation notes.
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(owner) = self.table.resolve_alias(reference) else {
            // Never an alias, its rule already fired, or its owner has died: the C
            // drops the message and tells the peer nothing.
            return Ok(DeliveryOutcome::Dropped);
        };
        let outcome = self.enqueue(owner, from, message)?;
        if outcome == DeliveryOutcome::Delivered {
            // The reply-alias consumption (`erl_proc_sig_queue.c:6047-6049`); a
            // `demonitor`/`explicit_unalias` alias survives the delivery, which the
            // table's own mode check enforces.
            self.table.alias_reply_delivered(reference)?;
        }
        Ok(outcome)
    }

    /// Routes the sub-cases of `DOP_ALTACT_SIG_SEND` that land in the ordinary
    /// message paths above.
    ///
    /// The C reads the flag word and dispatches accordingly
    /// (`dist.c:2662-2696`): `..._EXIT` is an **exit signal** (the trailing term is
    /// the reason), otherwise a **message**; `..._ALIAS` says `To` is a reference,
    /// `..._NAME` that it is a registered name, and neither that it is a pid.
    /// `..._PRIO` makes the signal jump the mailbox queue.
    ///
    /// Only the plain **message** forms are routed ([`Self::deliver_to_pid`] /
    /// [`Self::deliver_to_name`] / [`Self::deliver_to_alias`]) — a priority signal
    /// needs a priority queue this mailbox has no seam for, and an exit signal needs the
    /// `apply_remote_exit_signal` named on [`Delivery::Exit`]'s arm. Both stay typed
    /// gaps rather than being delivered as ordinary messages (which would silently
    /// reorder a priority signal and turn an untrappable exit into a message).
    fn deliver_altact(
        &mut self,
        flags: AltactSigFlags,
        from: Eterm,
        to: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        if flags.contains(AltactSigFlags::EXIT) {
            // `ERTS_DOP_ALTACT_SIG_FLG_EXIT`: the trailing term is an exit *reason*
            // (`dist.c:2687-2692`), with the `DOP_EXIT2` semantics — untrappable, so it
            // must not be enqueued as a message.
            return Ok(DeliveryOutcome::Unsupported);
        }
        if flags.contains(AltactSigFlags::PRIO) {
            // `ERTS_DOP_ALTACT_SIG_FLG_PRIO`: the signal must jump the mailbox queue
            // (`dist.c:2684`/`:2693-2696`), which the ordinary enqueue here cannot do.
            return Ok(DeliveryOutcome::Unsupported);
        }
        if flags.contains(AltactSigFlags::ALIAS) {
            // `To` is a reference (the C rejects `ALIAS` together with `NAME`,
            // `dist.c:2666-2669`; the classifier has already applied that rule).
            return self.deliver_to_alias(from, to, message);
        }
        if flags.contains(AltactSigFlags::NAME) {
            // `To` is a registered name; an unregistered one is a silent drop
            // (`dist.c:2670-2680`).
            return self.deliver_to_name(from, to, message);
        }
        // Otherwise `To` is a pid (`dist.c:2682-2685`).
        self.deliver_to_pid(from, to, message)
    }

    /// Classifies an addressed pid term as a process of this node.
    ///
    /// An **internal** pid immediate is a local process by construction. The
    /// distribution decoder materialises a pid as an **external** pid thing
    /// (`beam_codec`), so a destination on this node arrives as an external pid
    /// whose node atom and creation match ours — and only then is it looked up (by
    /// number *and* serial, never number alone).
    fn resolve(&self, term: Eterm) -> Target {
        match PidTarget::from_eterm(term) {
            Ok(PidTarget::Local(pid)) => Target::Local(pid),
            Ok(PidTarget::Remote(remote))
                if remote.node() == self.local_node
                    && remote.creation() == self.local_creation =>
            {
                Target::Local(ProcessId::new(
                    u64::from(remote.number()),
                    u64::from(remote.serial()),
                ))
            }
            Ok(PidTarget::Remote(_)) => Target::Foreign,
            Err(_) => Target::NotAPid,
        }
    }

    /// Enqueues `message` (from `from`) into `pid`'s mailbox, waking a blocked
    /// receiver through the process table's own wake list.
    ///
    /// The envelope's sender word is derived by [`envelope_from`] — shared by every
    /// routed family, because the rule is the message envelope's, not the opcode's.
    fn enqueue(
        &mut self,
        pid: ProcessId,
        from: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        let Some(envelope) = envelope_from(from) else {
            // No immediate sender word exists for this delivery, and storing the
            // classified word would leave a dangling envelope (see [`envelope_from`]).
            // Refuse it: nothing is delivered, nothing is mis-delivered.
            return Ok(DeliveryOutcome::Unsupported);
        };
        if !self.table.contains(pid) {
            // The pid decoded to a slot this table does not hold (a dead process, or
            // a number that was never ours).
            return Ok(DeliveryOutcome::NoSuchProcess);
        }
        match self.table.deliver_simple(pid, envelope, message) {
            Ok(_) => Ok(DeliveryOutcome::Delivered),
            // The receiver's heap could not hold the message even after a
            // collection (this runtime has no inbound heap-fragment delivery). Drop
            // it rather than mis-deliver it, and keep the link up: one oversized
            // message from a peer must not tear the link down. Reported honestly as
            // `Dropped`.
            Err(BeamError::Alloc(_)) => Ok(DeliveryOutcome::Dropped),
            // Anything else is an unexpected table invariant break — surface it (the
            // caller retires the offending link) rather than swallowing it.
            Err(err) => Err(err),
        }
    }

    // --- the routed signal families (route-free receipt) ---------------------

    /// Applies an inbound `DOP_LINK`: records the remote `from` as linked to the local
    /// `to`. **Record-only** — the C's arm ends `break; /* Done */` (`dist.c:2258`) and
    /// sends no `DOP_LINK` back.
    fn record_link(&mut self, from: Eterm, to: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            // `DOP_LINK`'s `From` is an external pid (`dist.c:2234`); the per-link
            // validator already refuses anything else, so this only catches the
            // unchecked call.
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(self.table.record_remote_link(pid, remote))
    }

    /// Applies an inbound `DOP_EXIT`/`DOP_EXIT2` (and their payload forms) to the local
    /// `to`, with `untrappable` selecting the C's operation family (see the `Exit` arm).
    fn exit_signal(
        &mut self,
        from: Eterm,
        to: Eterm,
        reason: Eterm,
        untrappable: bool,
    ) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(
            self.table
                .apply_remote_exit_signal(pid, remote, reason, untrappable),
        )
    }

    /// Applies an inbound `DOP_UNLINK_ID`: drops the recorded remote link and reports
    /// what the **wire** half owes (see the `UnlinkId` arm).
    ///
    /// `unlink_from_remote` hands back the id an ack is required for: `Some(id)` iff
    /// `id != 0`, which is exactly the C's split (`reply_dist_unlink_ack` sends nothing
    /// for `id == 0`, `erl_proc_sig_queue.c:2753-2760`). The link is dropped in both
    /// cases; only the `id != 0` case additionally owes a `DOP_UNLINK_ID_ACK`, which
    /// this sink has no channel to emit, so it is reported `Unsupported`.
    fn unlink_id(&mut self, id: Eterm, from: Eterm, to: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(id) = small_uint(id) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        match self.table.unlink_from_remote(pid, remote, id) {
            // `id == 0`: no ack owed, the unlink is complete locally.
            Ok(None) => Ok(DeliveryOutcome::Delivered),
            // The link is dropped, but the ack this node owes cannot be emitted.
            Ok(Some(_)) => Ok(DeliveryOutcome::Unsupported),
            Err(BeamError::TermNotFound) => Ok(DeliveryOutcome::NoSuchProcess),
            Err(err) => Err(err),
        }
    }

    /// Applies an inbound `DOP_UNLINK_ID_ACK`: validates the addressed local process
    /// and lets the table consume the ack. It routes nothing (the C answers nothing).
    fn unlink_ack(&mut self, id: Eterm, from: Eterm, to: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(id) = small_uint(id) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(self.table.unlink_ack_from_remote(pid, remote, id))
    }

    /// Applies an inbound `DOP_GROUP_LEADER`: the remote `from` becomes the local `to`'s
    /// group leader. Routes nothing.
    fn group_leader(&mut self, from: Eterm, to: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(self.table.set_group_leader_from_remote(pid, remote))
    }

    /// Applies an inbound `DOP_MONITOR_P` — `{DOP_MONITOR_P, Remote(watcher), Local
    /// pid-or-name(watched), Ref}` (`dist.c:2341`): records the `watcher`'s monitor of the local
    /// `watched`. Routes nothing (the C's arm ends `break; /* done */` once the local monitor
    /// signal is queued).
    ///
    /// `watched` is the C's `local pid or name` slot: an internal pid (or an external pid of
    /// *this* node, the shape the decoder materialises) resolves through
    /// [`LocalDeliverySink::resolve_live`], and a registered **name** through the table's name
    /// registry (`erts_whereis_name_to_id`, `dist.c:2353-2360`) — an unregistered name is the
    /// honest [`DeliveryOutcome::NoSuchProcess`] (the C replies `noproc`, a wire artifact this
    /// sink owes no channel for).
    ///
    /// The wire `watcher` must be an external pid and the wire `Ref` a peer-minted external
    /// reference; anything else is outside this opcode's wire contract and reported
    /// [`DeliveryOutcome::Unsupported`]. The reference is decomposed into a
    /// [`RemoteRef`](beam_process::RemoteRef) and stored whole, so the record is faithful — a
    /// later demonitor matches exactly this reference (see [`LocalDeliverySink::demonitor`]).
    ///
    /// `DOP_MONITOR_P` is a **process** monitor on the wire, so the record's kind is
    /// [`MonitorKind::Process`](beam_process::MonitorKind).
    fn monitor(&mut self, watcher: Eterm, watched: Eterm, reference: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_watched(watched) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(watcher) = remote_pid(watcher) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        // A term that is not a well-formed external reference (`is_not_ref` in the C,
        // `dist.c:2350`) is outside the opcode's contract; the per-link validator owns the
        // connection-level answer.
        let Ok(reference) = RemoteRef::from_eterm(reference) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(
            self.table
                .monitor_from_remote_ref(pid, watcher, reference, MonitorKind::Process),
        )
    }

    /// Applies an inbound `DOP_DEMONITOR_P` — `{DOP_DEMONITOR_P, Remote(watcher), Local
    /// pid-or-name(watched), Ref}` (`dist.c:2415`): removes the `watch`er's record under
    /// `reference`. Routes nothing (the C's arm answers nothing).
    ///
    /// The same resolution as [`LocalDeliverySink::monitor`]: `watched` is a local pid / this
    /// node's external pid / a registered name; the wire `watcher` must be an external pid and the
    /// wire `Ref` a peer-minted external reference. Removal matches the reference by its **whole**
    /// decomposed identity, so it cancels exactly the monitor the matching `DOP_MONITOR_P`
    /// installed, never a different one.
    fn demonitor(
        &mut self,
        watcher: Eterm,
        watched: Eterm,
        reference: Eterm,
    ) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_watched(watched) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        // The demonitor key is `(watched, ref)` — the C still validates the `watcher` is an
        // external pid on the peer's dist entry (`dist.c:2424`), so a bogus one is outside the
        // wire contract rather than a silent removal.
        if remote_pid(watcher).is_none() {
            return Ok(DeliveryOutcome::Unsupported);
        }
        let Ok(reference) = RemoteRef::from_eterm(reference) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(self.table.demonitor_from_remote_ref(pid, reference))
    }

    /// Resolves the `watched` slot of the monitor family: a local pid (or this node's external
    /// pid) through [`LocalDeliverySink::resolve_live`], or a registered **name** through the
    /// table's process-name registry.
    ///
    /// An unregistered name, a pid of another node and a stale incarnation of this one are all
    /// [`DeliveryOutcome::NoSuchProcess`] — the C replies `noproc` for the first two
    /// (`dist.c:2353-2362`, `erl_proc_sig_queue.c`) and drops the stale-pid case. A term that is
    /// neither a pid nor an atom is outside the wire contract: [`DeliveryOutcome::Unsupported`].
    fn resolve_watched(&self, watched: Eterm) -> std::result::Result<ProcessId, DeliveryOutcome> {
        if is_atom(watched) {
            return match self.table.whereis(atom_val(watched)) {
                Some(pid) => Ok(pid),
                None => Err(DeliveryOutcome::NoSuchProcess),
            };
        }
        self.resolve_live(watched)
    }
    /// Applies an inbound `DOP_MONITOR_P_EXIT`/`DOP_PAYLOAD_MONITOR_P_EXIT`: resolves the
    /// **local** `watcher`'s outgoing remote monitor under `reference` and delivers the
    /// `{'DOWN', …}` — without routing any `DOP_DEMONITOR_P` back (the C's arm queues
    /// only the local DOWN signal).
    ///
    /// The reference is looked up *through the watcher's own record*, exactly as the C
    /// resolves the DOWN through the watcher's monitor tree and **drops** the signal when
    /// the lookup misses (`erl_proc_sig_queue.c:6355-6396`) — so an unmatched DOWN is the
    /// honest `Dropped`, never a fabricated one. Using the record's own `MonitorRef` also
    /// avoids inventing one: the wire id is only the lookup key, never a term.
    fn monitor_down(
        &mut self,
        watched: Eterm,
        watcher: Eterm,
        reference: Eterm,
        reason: Eterm,
    ) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(watcher) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(watched) else {
            // A **name**-targeted DOWN (an atom `watched`) is outside what the receipt
            // API can express — it takes the resolved `RemotePid` (see the arm).
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(wanted) = self.wire_monitor_ref_id(reference) else {
            // A reference this node did not mint cannot match an outgoing record.
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(record) = self
            .table
            .remote_monitors(pid)
            .iter()
            .find(|monitor| monitor.reference.id() == wanted)
            .copied()
        else {
            return Ok(DeliveryOutcome::Dropped);
        };
        receipt(self.table.remote_monitor_down(
            pid,
            remote,
            record.reference,
            reason,
        ))
    }

    /// Resolves an addressed pid term to a **live** local process, or the honest outcome
    /// for one that is not.
    ///
    /// A pid on another node, or a stale incarnation of this one, is never looked up by
    /// its slot number alone — [`Self::resolve`] classifies it [`Target::Foreign`], and a
    /// local slot this table does not hold is the same `NoSuchProcess`. A non-pid (for
    /// this opcode) is outside the wire contract: `Unsupported`.
    fn resolve_live(&self, to: Eterm) -> std::result::Result<ProcessId, DeliveryOutcome> {
        match self.resolve(to) {
            Target::Local(pid) if self.table.contains(pid) => Ok(pid),
            Target::Local(_) | Target::Foreign => Err(DeliveryOutcome::NoSuchProcess),
            Target::NotAPid => Err(DeliveryOutcome::Unsupported),
        }
    }

    /// Recovers the `MonitorRef` **id** of a reference that came back off the wire and
    /// was minted by *this* runtime — the exact inverse of the pack
    /// [`crate::dist_client`]'s `monitor_ref_term` puts on the wire.
    ///
    /// A monitor this node initiates on a remote process records a
    /// [`RemoteMonitor`](beam_process::RemoteMonitor) under a `MonitorRef` and routes a
    /// `DOP_MONITOR_P` whose `Ref` element is `{our_node, our_creation, [0, id >> 32,
    /// id]}` (three 32-bit words, most significant first, leading word zeroed). The peer
    /// echoes that identical reference image in the `DOP_MONITOR_P_EXIT` it sends when the
    /// monitored process dies, and the control-message decoder materialises it as an
    /// **external** reference (`beam_dist::control::decode_control` installs no
    /// local-node identity, so the codec's local-node demotion never runs).
    ///
    /// This undoes that packing exactly, and only for a reference this node minted: the
    /// node and creation must be **this** node's and the leading id word zero. Anything
    /// else — in particular a reference a **peer** minted, whose node is not ours — is
    /// `None`, never a guessed id: such an identity has no faithful `u64` image (see the
    /// `Monitor`/`Demonitor` arm).
    fn wire_monitor_ref_id(&self, term: Eterm) -> Option<u64> {
        let (node, ids, creation) = external_ref_parts(term).ok()?;
        if node != self.local_node || creation != self.local_creation {
            return None;
        }
        let (high, low) = match ids {
            [0, high, low] => (*high, *low),
            _ => return None,
        };
        Some((u64::from(high) << 32) | u64::from(low))
    }
}

/// The sink outcome of a **route-free receipt** call: `Delivered` on success, or the
/// honest `NoSuchProcess` when the table reports the addressed local party is not live
/// (every receipt primitive reports that as `BeamError::TermNotFound`).
fn receipt(result: Result<()>) -> Result<DeliveryOutcome> {
    match result {
        Ok(()) => Ok(DeliveryOutcome::Delivered),
        Err(BeamError::TermNotFound) => Ok(DeliveryOutcome::NoSuchProcess),
        Err(err) => Err(err),
    }
}

/// The remote pid an **external** pid thing names, or `None` for anything else.
fn remote_pid(term: Eterm) -> Option<RemotePid> {
    RemotePid::from_eterm(term).ok()
}

/// The `u64` value of a non-negative **small** integer term, or `None` (a bignum or a
/// non-number id is outside this safe path).
fn small_uint(term: Eterm) -> Option<u64> {
    if !is_small(term) {
        return None;
    }
    u64::try_from(signed_value(term)).ok()
}

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
}
