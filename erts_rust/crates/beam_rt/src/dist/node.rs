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

//! The node itself: the bound listener, the `epmd` registration, the accept loop and the
//! per-link drivetrain.
//!
//! [`DistNode`] is what a `-name`/`-sname` runtime *is*: it binds a wildcard distribution
//! listener ([`DIST_LISTEN_ADDR`]), publishes itself to `epmd` with one `ALIVE2_REQ` on
//! the first pump, accepts inbound connections non-blocking and drives each accepted link
//! (a [`DistConnection`], see the sibling `connection` module) through the server `'N'`
//! handshake and then its checked inbound drain. The `epmd` address is [`epmd_addr`], so
//! `$ERL_EPMD_PORT` selects a private `epmd`.
//!
//! Every failure here is **recorded, never raised**: a link that breaks, a message that
//! cannot be decoded or a peer that forges its identity retires that one link
//! ([`DistNode::last_link_error`]) and the scheduler pass carries on; an `epmd` that is
//! absent or refuses leaves the node bound-but-unregistered
//! ([`DistNode::registration_error`]) rather than failing a runtime that merely passed
//! `-sname`/`-name`.

use std::cell::RefCell;
use std::io::ErrorKind;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_dist::connection::TcpTransport;
use beam_dist::epmd::{Alive2Req, Alive2Resp, EpmdClient, EPMD_DEFAULT_ADDR};
use beam_dist::TcpListener;
use beam_dist::{preferred_flags, publish_flag, DeliveryOutcome, DistFlags, InboundSink};
use beam_term::prelude::make_atom;
use beam_term::AtomTable;

use crate::error;

use super::connection::{is_would_block, DistConnection};
use super::{
    DistConnectionSnapshot, DistLinks, DIST_DISPATCH_HEAP_WORDS, DIST_LISTEN_ADDR,
};

/// The runtime's distribution node: a bound listener, its `epmd` registration
/// and every accepted link.
///
/// See the [`crate::dist`] module docs for what this does and does not do in
/// Milestone 1.
pub struct DistNode {
    /// The node's name in its three wire forms.
    name: super::NodeName,
    /// The `epmd`-assigned (or defaulted) node creation.
    creation: u32,
    /// The bound listen port.
    port: u16,
    /// The listener; non-blocking, so `accept` never stalls a pump.
    listener: std::net::TcpListener,
    /// The shared cookie the handshake digests use.
    cookie: Vec<u8>,
    /// The accepted links, **shared** with [`DistLinks`] — the one table both distribution
    /// halves route through (see [`crate::dist::links`]). The node pumps and mutates it;
    /// the routing entry only borrows it to write a frame.
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
    /// Whether this node is **hidden** (`-hidden`): its handshake then omits
    /// `DFLAG_PUBLISHED`, so a peer lists it only in
    /// `nodes(hidden)`/`nodes(connected)` (see [`advertised_flags`]). Set through
    /// [`DistNode::set_hidden`] before the first pump; `false` — a **published** node —
    /// by default.
    hidden: bool,
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
        let name = super::NodeName::parse(configured);
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
            hidden: false,
            dispatch_heap: Heap::new(DIST_DISPATCH_HEAP_WORDS)?,
        })
    }

    /// Marks this node **hidden** (`-hidden`) — or, with `false`, published (the
    /// default).
    ///
    /// A hidden node's `'N'` handshake omits `DFLAG_PUBLISHED`, exactly as
    /// `dist_util.erl:152-160`'s `publish_flag/3` does for a `-hidden` node, so a real
    /// peer records it as `beam_dist::PeerVisibility::Hidden` and leaves it out of
    /// `nodes()`. Visibility is a property of the node, so the connecting half
    /// ([`crate::dist_client::DistClient::set_hidden`]) must be told the same value.
    ///
    /// Additive: a node built by [`DistNode::start`] is published. It only affects links
    /// **accepted afterwards** (the flags are baked into each link's driver at accept
    /// time), which is why the runtime sets it before the first
    /// [`pump`](Self::pump).
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    /// Whether this node advertises itself as **hidden** ([`DistNode::set_hidden`]).
    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// The capability flags this node advertises in its handshake
    /// ([`advertised_flags`] of [`DistNode::is_hidden`]).
    pub fn advertised_flags(&self) -> DistFlags {
        advertised_flags(self.hidden)
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

    /// Test-only: sets the creation exactly as [`DistNode::register_once`] does when `epmd`
    /// answers its `ALIVE2` registration.
    ///
    /// Registration is the **only** production writer of this field and it needs a live
    /// `epmd` daemon, which a hermetic test has no way to provide — so the identity tests that
    /// must observe the creation *changing* (and the `Runtime` sync point that keeps the
    /// outbound half's copy equal to it) drive it through here. Same floor as the production
    /// write (`creation.max(1)`: `0` is the undistributed value, never a node's).
    #[cfg(test)]
    pub(crate) fn set_creation_for_test(&mut self, creation: u32) {
        self.creation = creation.max(1);
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
    /// **accepted** (see [`crate::dist::links`]).
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
        for _ in 0..super::MAX_ACCEPTS_PER_PUMP {
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
    /// **established** link through `sink` (the runtime's [`beam_dist::InboundSink`] —
    /// see [`LocalDeliverySink`](crate::dist::LocalDeliverySink)), returning how many
    /// were actually delivered to a local mailbox.
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
        // The decode-side identity: a pid/port/reference naming **this** node at **this**
        // creation decodes to the internal term (`beam_dist::control::LocalIdentity` — the
        // C's `dec_is_this_node`, `external.c:3123-3129`, applied to the control arguments
        // *and* the trailing message). It is the ONE place the demotion happens; the sink
        // only classifies (`LocalDeliverySink::resolve`).
        //
        // Refreshed per drain rather than installed once per link: an `Atom` is a
        // **table-local index** (`beam_term::atom`), and a copy-on-write table clone can
        // give the same name a different index, which would leave a stored identity
        // silently inert (it can never mis-match — `LocalIdentity`'s own docs — but it
        // would demote nothing). Interning a name already present is one hash lookup, and
        // the atom table passed here is the very one the decode uses.
        let identity = beam_dist::control::LocalIdentity::intern(
            atoms,
            self.name.advertised(),
            self.creation,
        )?;
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
            connections[index].set_local_identity(identity);
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
                    advertised_flags(self.hidden),
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
    /// bound-but-unregistered listener (see the [`crate::dist`] module docs).
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
/// while the pump drains it (see [`crate::dist::links`]) — the borrow is already held, so the
/// caller passes the slice and the `last_link_error` slot instead of `&mut DistNode`.
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
pub(super) fn epmd_addr_from(value: Option<&str>) -> String {
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

/// The capability flags this node advertises in its `'N'` handshake: the emulator
/// default ([`preferred_flags`]) **plus** `DFLAG_PUBLISHED` unless the node is
/// **hidden** (`-hidden`).
///
/// A real node publishes itself per peer: `dist_util.erl:152-160`'s `publish_flag/3`
/// ORs `DFLAG_PUBLISHED` (`dist.h:30`) into the flags this side advertises unless the
/// node was started `-hidden` (`net_kernel.erl:hidden_argument/0` reads exactly that
/// command-line argument). The bit is what makes a peer list this node in `nodes()`:
/// without it every real peer classifies this runtime as *hidden*
/// ([`beam_dist::handshake::publish_type`], `dist_util.erl:917-923`) and it shows up
/// only in `nodes(hidden)`/`nodes(connected)`.
///
/// Visibility is a property of the **node**, not of who dialled, so this one set is
/// what both halves advertise — the acceptor ([`DistConnection`]) and the connector
/// ([`crate::dist_client::DistClient`]).
pub(crate) fn advertised_flags(hidden: bool) -> DistFlags {
    preferred_flags() | publish_flag(hidden, true)
}
