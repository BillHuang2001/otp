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
//! * **Inbound control messages are delivered for the `send` family.** An
//!   **established** link's decoded `DOP_SEND*`/`DOP_REG_SEND*` control messages
//!   are routed to the addressed **local** process's mailbox by
//!   [`DistNode::dispatch_inbound`] through the [`LocalDeliverySink`] (the runtime
//!   side of `beam_dist`'s [`InboundSink`] seam). Every other classified delivery
//!   (aliases, links, exit and monitor signals, the distributed-spawn family) has
//!   no correct local seam in this runtime and is reported as the honest
//!   [`DeliveryOutcome::Unsupported`] — never a fabricated or mis-addressed
//!   delivery. A message the runtime cannot decode or classify **retires that one
//!   link** (recorded in [`DistNode::last_link_error`]); it never fails a scheduler
//!   pass.
//! * **A broken link is dropped, not raised.** A protocol error or a write
//!   failure retires that one link and is recorded in
//!   [`DistNode::last_link_error`]; it never aborts the runtime.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_dist::connection::TcpTransport;
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::epmd::{Alive2Req, Alive2Resp, EpmdClient, EPMD_DEFAULT_ADDR};
use beam_dist::{Delivery, DeliveryOutcome, Dispatched, InboundSink};
use beam_dist::TcpListener;
use beam_process::{PidTarget, ProcessId, ProcessTable};
use beam_term::prelude::{atom_val, is_atom, make_atom, Atom, Eterm};
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

    /// Decodes, classifies and routes every control message this link has queued,
    /// returning one [`Dispatched`] record per message in arrival order.
    ///
    /// A thin forwarder to [`ConnectionDriver::dispatch_inbound`]: the driver owns
    /// the queue and the receive atom cache, the sink owns the routing (see
    /// [`LocalDeliverySink`]). An `Err` is a link-level decode/classification
    /// failure — the caller retires the link and records the reason.
    fn dispatch_inbound(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
        sink: &mut dyn InboundSink,
    ) -> Result<Vec<Dispatched>> {
        self.driver.dispatch_inbound(heap, atoms, sink)
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
    /// (`beam_dist`'s `BadArg` from a truncated/malformed control message) — retires
    /// **that one link** and records the reason in [`DistNode::last_link_error`],
    /// exactly as [`DistNode::pump`] does; it is never propagated, so a hostile or
    /// broken peer can neither fail nor stall a scheduler pass. The existing links
    /// (and their already-dispatched messages) are untouched.
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
/// Only the **`send` family** is routed: a `DOP_SEND*` to a local pid and a
/// `DOP_REG_SEND*` to a registered local name
/// ([`Delivery::Send`]/[`Delivery::RegSend`]). Every other classified delivery —
/// process aliases ([`Delivery::AliasSend`]), the link/exit family
/// ([`Delivery::Link`]/[`Delivery::Exit`]/[`Delivery::Exit2`]), the monitor family
/// ([`Delivery::Monitor`]/[`Delivery::Demonitor`]/[`Delivery::MonitorExit`]), the
/// unlink-id handshake ([`Delivery::UnlinkId`]/[`Delivery::UnlinkIdAck`]), the
/// group-leader change ([`Delivery::GroupLeader`]) and the recognised-but-
/// unclassified opcodes ([`Delivery::Other`]) — is reported as
/// [`DeliveryOutcome::Unsupported`]: `beam_process`'s link/monitor/party
/// structures only accept *local* peers, so there is no correct seam to install a
/// remote counterpart and a fabricated one would be far worse than an honest gap.
///
/// Nothing is ever delivered to the *wrong* process: an addressed pid that is not
/// a process of this node (another node, or a stale incarnation of this one) is
/// [`DeliveryOutcome::NoSuchProcess`], never a lookup by number alone.
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
            // Every other delivery has no correct local seam here (see the type
            // docs): the honest `Unsupported`, never a fabricated/mis-addressed
            // delivery.
            _ => Ok(DeliveryOutcome::Unsupported),
        }
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
            // answers the peer with nothing, `dist.c:2506-2509`).
            None => Ok(DeliveryOutcome::NoSuchProcess),
        }
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
    fn enqueue(
        &mut self,
        pid: ProcessId,
        from: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        if !self.table.contains(pid) {
            // The pid decoded to a slot this table does not hold (a dead process, or
            // a number that was never ours).
            return Ok(DeliveryOutcome::NoSuchProcess);
        }
        match self.table.deliver_simple(pid, from, message) {
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
