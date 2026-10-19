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

//! The **one** node→link table: both distribution halves in a single registry, and the
//! **one** routing entry that writes an outbound control message on whichever link reaches
//! a node.
//!
//! A distributed runtime holds links of two kinds:
//!
//! * the links it **dialled** — [`crate::dist_client::DistClient`]'s outbound
//!   [`ClientLink`]s, owned by the client's own `Rc<RefCell<…>>` cell (a peer, its node
//!   atom term and the connecting side of the `'N'` handshake);
//! * the links a peer **dialled into** it — [`crate::dist::DistNode`]'s accepted
//!   [`DistConnection`]s, whose peer is only *named* (`peer_name()`), never keyed by an
//!   already-interned atom.
//!
//! Both are links to a node, so a locally-initiated `link`/`monitor`/`exit`/`unlink` or a
//! plain message must reach the peer **whichever side opened the socket**: when a system
//! `erl` dials `beam_rt`, the runtime's only link *is* an accepted one, and a table that
//! only knows the dialled half leaves every outbound signal unroutable.
//!
//! # Why the accepted half is a shared cell
//!
//! [`crate::dist::DistNode`] owns the accepted links and pumps them from the scheduler
//! pass, while the routing entry is reached from paths that hold no `DistNode` at all —
//! the [`DistSignalRouter`](beam_process::DistSignalRouter) installed on the **process
//! table** (which the runtime *moves out of itself* for the duration of a pass, so it
//! cannot borrow the runtime) and the runtime's own `dist_send_*` API. The accepted links
//! are therefore held as one `Rc<RefCell<Vec<DistConnection>>>` shared by the node and by
//! this table: the node pumps and mutates it, the routing entry borrows it only to write
//! one frame. The two never run at the same time (both are driven sequentially from one
//! scheduler pass, and the pump's borrows are released before the pass's send paths run),
//! and every borrow here is a `try_borrow`/`try_borrow_mut` — a re-entrant borrow is a
//! typed error or a documented no-op, never a panic.
//!
//! # One established link per node
//!
//! A node never holds two live links to the same peer — a real peer treats that as a
//! simultaneous connection and kills one. The outbound half enforces it at dial time
//! (a re-connect replaces the old link, see `DistClient::connect_addr`), so the routing
//! entry looks the outbound half up **first** and only then the accepted half; a node
//! with an outbound link never needs the accepted lookup, and a node whose only link was
//! accepted has no outbound entry at all.
//!
//! # Naming an accepted link's peer
//!
//! An accepted [`DistConnection`] knows its peer only as the `node@host` name its `'N'`
//! handshake carried, while a routed signal addresses the peer by an **atom term**. The
//! two are matched through the runtime's one live [`AtomTable`] — the node term's own name
//! is read out of the table and compared with `peer_name()` — which is why every lookup
//! here takes `atoms`. The atom table is live exactly where the routing entry runs (the
//! `flush` half of the client's pump, and the runtime's `dist_send_*` API), which is also
//! why a routed signal is *queued* rather than encoded while the router runs.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_dist::{DistFlags, Dop, PeerVisibility};
use beam_term::prelude::{atom_val, is_atom, Eterm};
use beam_term::AtomTable;

use crate::dist::DistConnection;
use crate::dist_client::ClientLink;
use crate::error;

/// Both halves of this runtime's distribution links, and the routing entry over them.
///
/// See the module docs for why the accepted half is a shared cell and why the outbound
/// half is looked up first.
pub(crate) struct DistLinks {
    /// The links this runtime **dialled** (the connecting half, owned by
    /// [`crate::dist_client::DistClientInner`]).
    pub(crate) outbound: Vec<ClientLink>,
    /// The links a peer dialled **into** this runtime (the accepting half) — the very same
    /// cell [`crate::dist::DistNode`] pumps and mutates.
    pub(crate) accepted: Rc<RefCell<Vec<DistConnection>>>,
}

impl DistLinks {
    /// A table over `accepted` with no outbound link yet.
    pub(crate) fn new(accepted: Rc<RefCell<Vec<DistConnection>>>) -> DistLinks {
        DistLinks {
            outbound: Vec::new(),
            accepted,
        }
    }

    /// The index of the **established** outbound link keyed by `node`, if any.
    ///
    /// A link still handshaking is not a route — it carries no `DOP_*` yet, and
    /// [`beam_dist::ConnectionDriver::send_control`] refuses one — so it is skipped here
    /// and a queued control for that node simply waits for a later flush.
    pub(crate) fn link_index_for_node(&self, node: Eterm) -> Option<usize> {
        self.outbound
            .iter()
            .position(|link| link.node == node && link.is_established())
    }

    /// The index of the **established** accepted link bound to `node`, if any.
    ///
    /// The comparison is the peer's own **node atom term** ([`DistConnection::node_term`]),
    /// bound by [`crate::dist::DistNode::bind_peer_nodes`] from the `node@host` name its
    /// `'N'` handshake carried — so an accepted link is matched **exactly**, by the same
    /// term the caller addresses, and a node no accepted link names has no index here. An
    /// unbound link (still handshaking when the pass last bound) is not a route.
    ///
    /// A cell that cannot be borrowed is reported as **no link**: this is a pure query (the
    /// routing entry re-borrows and reports the typed error itself), and a query must never
    /// panic.
    pub(crate) fn accepted_index_for_node(&self, node: Eterm) -> Option<usize> {
        let accepted = self.accepted.try_borrow().ok()?;
        accepted
            .iter()
            .position(|connection| connection.is_established() && connection.node_term() == Some(node))
    }

    /// Whether an **established** link to `node` exists in either half.
    pub(crate) fn has_established_link_to(&self, node: Eterm) -> bool {
        self.link_index_for_node(node).is_some() || self.accepted_index_for_node(node).is_some()
    }

    /// Whether **any** link (either half) names `node`, whatever its handshake state.
    ///
    /// The distinction from [`DistLinks::has_established_link_to`] is what keeps a queued
    /// control alive while its link is still handshaking: the node is reachable, just not
    /// yet usable. The accepted half is matched by its bound node term, exactly like
    /// [`DistLinks::accepted_index_for_node`].
    pub(crate) fn names_node(&self, node: Eterm) -> bool {
        self.outbound.iter().any(|link| link.node == node)
            || match self.accepted.try_borrow() {
                Ok(accepted) => accepted
                    .iter()
                    .any(|connection| connection.node_term() == Some(node)),
                Err(_) => false,
            }
    }

    /// Whether a link that could reach `node` exists at all, **without** an atom table.
    ///
    /// This is the [`DistSignalRouter`](beam_process::DistSignalRouter)'s gate: the router
    /// installed on the process table holds no atoms, so it compares **node atom terms** —
    /// an outbound link's own `node` and an accepted link's bound
    /// [`DistConnection::node_term`] — exactly, on both halves. Both are interned in the
    /// runtime's one live table, so the term a routed signal addresses is the term a link
    /// answers with; no name resolution is needed and none is skipped.
    ///
    /// A node with no link in either half is therefore refused **here**, with the typed
    /// [`no_link_error`] — never accepted and dropped by a later flush (the A.5 contract: a
    /// signal for a node with no link is a typed error, never a silent drop).
    ///
    /// A cell that cannot be borrowed is reported as **reachable**: a borrowed table means
    /// distribution work is in flight, so the signal must be queued rather than refused.
    pub(crate) fn may_reach_node(&self, node: Eterm) -> bool {
        if self.outbound.iter().any(|link| link.node == node) {
            return true;
        }
        match self.accepted.try_borrow() {
            Ok(accepted) => accepted
                .iter()
                .any(|connection| connection.node_term() == Some(node)),
            Err(_) => true,
        }
    }
    /// Whether this table holds **any** link, either half, in any state.
    ///
    /// The scheduler pass's quiescence query: a link that exists but has not finished its
    /// handshake is still work the pass must keep pumping (its completion, or an inbound
    /// `DOP_*`, can wake a parked process). A cell that cannot be borrowed is reported as
    /// **holding a link** — a borrowed table means distribution work is in flight.
    pub(crate) fn has_any(&self) -> bool {
        if !self.outbound.is_empty() {
            return true;
        }
        match self.accepted.try_borrow() {
            Ok(accepted) => !accepted.is_empty(),
            Err(_) => true,
        }
    }

    /// Whether this table holds **no** link at all — [`DistLinks::has_any`]'s complement.
    pub(crate) fn is_idle(&self) -> bool {
        !self.has_any()
    }

    /// Whether **any** link, either half, completed its handshake.
    pub(crate) fn has_established(&self) -> bool {
        !self.established_names().is_empty()
    }

    /// The `node@host` names of every peer holding an **established** link, outbound first
    /// then accepted (the union of both halves).
    pub(crate) fn established_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .outbound
            .iter()
            .filter(|link| link.is_established())
            .map(|link| link.name.clone())
            .collect();
        if let Ok(accepted) = self.accepted.try_borrow() {
            names.extend(
                accepted
                    .iter()
                    .filter(|connection| connection.is_established())
                    .filter_map(|connection| connection.peer_name()),
            );
        }
        names
    }

    /// A human-readable label for `node`, **without** an atom table: the `node@host` name
    /// an outbound link keys it by, else a rendering of the raw term.
    ///
    /// This is the router's form (it holds no atoms). The flush, which does hold the live
    /// table, uses [`DistLinks::node_label_in`] and gets the atom's own name too.
    pub(crate) fn node_label(&self, node: Eterm) -> String {
        match self.outbound.iter().find(|link| link.node == node) {
            Some(link) => link.name.clone(),
            None => format!("{node:?}"),
        }
    }

    /// [`DistLinks::node_label`] with the live atom table: an outbound link's own name
    /// first, then the atom's name from `atoms`, then the raw term.
    pub(crate) fn node_label_in(&self, node: Eterm, atoms: &AtomTable) -> String {
        if let Some(link) = self.outbound.iter().find(|link| link.node == node) {
            return link.name.clone();
        }
        match node_name(node, atoms) {
            Some(name) => name.to_string(),
            None => self.node_label(node),
        }
    }
    /// Encodes one `DOP_*` control for `node` and **writes it**: the outbound link first,
    /// then an accepted link, else the typed refusal.
    ///
    /// This is the **one** routing entry — every outbound control this runtime emits (a
    /// routed `link`/`monitor`/`exit`/`unlink`, a `DOP_REG_SEND`, the runtime's
    /// `dist_send_*` API) goes through it, so a peer reached by an accepted link is served
    /// exactly like one this runtime dialled.
    ///
    /// # Errors
    ///
    /// The typed `distribution: no established connection to node <label>` when neither
    /// half holds an established link to `node` (a link that is merely handshaking
    /// included — the caller may keep the control queued for a later flush), the accepted
    /// cell's re-entrant-borrow refusal, and the encode/socket failures of the link itself.
    pub(crate) fn route_control(
        &mut self,
        node: Eterm,
        dop: Dop,
        args: &[Eterm],
        atoms: &AtomTable,
    ) -> Result<()> {
        if let Some(index) = self.link_index_for_node(node) {
            return self.outbound[index].send_control(dop, args, atoms);
        }
        if let Some(index) = self.accepted_index_for_node(node) {
            let mut accepted = self.borrow_accepted_mut()?;
            return (*accepted)[index].send_control(dop, args, atoms);
        }
        Err(no_link_error(&self.node_label_in(node, atoms)))
    }

    /// Encodes one **message** for `node` and writes it, on the same link search as
    /// [`DistLinks::route_control`].
    ///
    /// The opcode is not chosen here: [`beam_dist::ConnectionDriver::send_message`] picks
    /// `DOP_SEND_SENDER` when the peer negotiated `DFLAG_SEND_SENDER` and `DOP_SEND`
    /// otherwise (`dist.c:1686`), per link.
    ///
    /// # Errors
    ///
    /// As [`DistLinks::route_control`].
    pub(crate) fn route_message(
        &mut self,
        node: Eterm,
        from: Eterm,
        to: Eterm,
        msg: Eterm,
        atoms: &AtomTable,
    ) -> Result<()> {
        if let Some(index) = self.link_index_for_node(node) {
            return self.outbound[index].send_message(from, to, msg, atoms);
        }
        if let Some(index) = self.accepted_index_for_node(node) {
            let mut accepted = self.borrow_accepted_mut()?;
            return (*accepted)[index].send_message(from, to, msg, atoms);
        }
        Err(no_link_error(&self.node_label_in(node, atoms)))
    }

    /// Encodes one **alias message** for the node named by the alias reference `to` and
    /// writes it, on the same link search as [`DistLinks::route_message`].
    ///
    /// `to` is an external **reference** term (`NEWER_REFERENCE_EXT`): a process alias is a
    /// reference whose node slot names the peer the alias lives on, so the destination node
    /// is read out of the term itself ([`beam_codec::term::external_ref_parts`]) and needs
    /// no separate argument. `from` and `to` are already external terms prepared by the
    /// caller; the wire form is `{DOP_ALIAS_SEND, From, To} -- Msg` (`dist.c:2563`), so the
    /// argument order is `[from, to]`.
    ///
    /// # The `DFLAG_ALIAS` gate
    ///
    /// `DOP_ALIAS_SEND` is only meaningful to a peer that offered `DFLAG_ALIAS`. OTP 29
    /// advertises it in both its **default** and its **addable** flag masks
    /// (`DFLAG_DIST_DEFAULT`/`DFLAG_DIST_ADDABLE`) and does **not** make it mandatory
    /// (`DFLAG_DIST_MANDATORY` omits it), so a peer may legitimately establish a link
    /// without it. Such a link is the typed
    /// `distribution: the link to <label> did not negotiate DFLAG_ALIAS` — never a silent
    /// drop and never a bare [`BeamError::BadArg`], because the caller (`erlang:send/2` on
    /// an alias) must be able to tell an unreachable alias from a protocol shortfall.
    ///
    /// # Errors
    ///
    /// The typed refusal above, the typed
    /// `distribution: an alias send needs an external reference destination` when `to` is
    /// not an external reference, [`no_link_error`] when neither half holds an established
    /// link to the reference's node, the accepted cell's re-entrant-borrow refusal, and the
    /// encode/socket failures of the link itself.
    pub(crate) fn route_alias(
        &mut self,
        to: Eterm,
        from: Eterm,
        msg: Eterm,
        atoms: &AtomTable,
    ) -> Result<()> {
        let node = match beam_codec::term::external_ref_parts(to) {
            Ok((node, _ids, _creation)) => node,
            Err(_) => {
                return Err(error::message(
                    "distribution: an alias send needs an external reference destination",
                ))
            }
        };
        let label = self.node_label_in(node, atoms);
        if let Some(index) = self.link_index_for_node(node) {
            if !negotiated_alias(self.outbound[index].negotiated()) {
                return Err(alias_flag_error(&label));
            }
            return self.outbound[index].send_control(Dop::AliasSend, &[from, to, msg], atoms);
        }
        if let Some(index) = self.accepted_index_for_node(node) {
            let mut accepted = self.borrow_accepted_mut()?;
            let link = &mut (*accepted)[index];
            if !negotiated_alias(link.negotiated()) {
                return Err(alias_flag_error(&label));
            }
            return link.send_control(Dop::AliasSend, &[from, to, msg], atoms);
        }
        Err(no_link_error(&label))
    }

    /// Borrows the accepted cell for a write, mapping a re-entrant borrow to a typed error
    /// (never a panic: the routing entry can be reached while a pass holds the cell).
    fn borrow_accepted_mut(&self) -> Result<std::cell::RefMut<'_, Vec<DistConnection>>> {
        self.accepted
            .try_borrow_mut()
            .map_err(|_| error::message("distribution: the accepted link table is already in use"))
    }
}

/// An accepted link's observable state at the moment it was read.
///
/// [`crate::dist::DistNode::connections`] answers one of these per accepted link instead of
/// a borrow of the live table: the table is shared with the routing entry (see the module
/// docs), so it cannot be handed out by reference. A snapshot is a **diagnostic** — it is
/// empty while the table is being mutated (a pump in flight), and it never affects routing.
#[derive(Clone, Debug)]
pub struct DistConnectionSnapshot {
    /// Whether the link completed its `'N'` handshake.
    established: bool,
    /// The peer's `node@host` name, once the handshake advertised it.
    peer_name: Option<String>,
    /// The peer's node creation.
    peer_creation: u32,
    /// The peer's socket address.
    peer_addr: SocketAddr,
    /// Bytes read from / written to the peer so far.
    bytes: (u64, u64),
    /// How the peer's own flags classify it.
    peer_visibility: Option<PeerVisibility>,
}

impl DistConnectionSnapshot {
    /// The snapshot of one live connection.
    fn of(connection: &DistConnection) -> DistConnectionSnapshot {
        DistConnectionSnapshot {
            established: connection.is_established(),
            peer_name: connection.peer_name(),
            peer_creation: connection.peer_creation(),
            peer_addr: connection.peer_addr(),
            bytes: connection.bytes(),
            peer_visibility: connection.peer_visibility(),
        }
    }

    /// Every accepted link's snapshot, in accept order (empty while the table is borrowed).
    pub(crate) fn of_table(accepted: &Rc<RefCell<Vec<DistConnection>>>) -> Vec<DistConnectionSnapshot> {
        match accepted.try_borrow() {
            Ok(accepted) => accepted.iter().map(DistConnectionSnapshot::of).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Whether the handshake has completed (the link is usable).
    pub fn is_established(&self) -> bool {
        self.established
    }

    /// The peer's node name from the `'N'` handshake, once known.
    pub fn peer_name(&self) -> Option<String> {
        self.peer_name.clone()
    }

    /// The peer's node creation, once the handshake advertised it.
    pub fn peer_creation(&self) -> u32 {
        self.peer_creation
    }

    /// The peer's socket address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Bytes read from / written to the peer so far.
    pub fn bytes(&self) -> (u64, u64) {
        self.bytes
    }

    /// How the peer's own advertised flags classify it — [`PeerVisibility::Normal`] or
    /// [`PeerVisibility::Hidden`] — or `None` before the handshake completes.
    pub fn peer_visibility(&self) -> Option<PeerVisibility> {
        self.peer_visibility
    }
}

/// The `node@host` name of an atom **term**, or `None` for a non-atom or an atom this
/// table does not know.
fn node_name(node: Eterm, atoms: &AtomTable) -> Option<&str> {
    if !is_atom(node) {
        return None;
    }
    atoms.name(atom_val(node))
}

/// A typed "this runtime has no link to that node" error.
///
/// Never a silent drop: the caller (a link/monitor operation, or a dying process's
/// termination) can see exactly why the signal did not reach the wire.
pub(crate) fn no_link_error(label: &str) -> BeamError {
    error::message(format!(
        "distribution: no established connection to node {label}"
    ))
}

/// Whether a link's negotiated flags allow [`Dop::AliasSend`] (`DFLAG_ALIAS`).
///
/// The one place the per-link alias capability is read, so the gate cannot be spelled
/// differently on the outbound and the accepted half.
pub(crate) fn negotiated_alias(flags: DistFlags) -> bool {
    flags.contains(DistFlags::from_bits(beam_dist::flags::DFLAG_ALIAS))
}

/// A typed "the link to `label` did not negotiate `DFLAG_ALIAS`" error.
///
/// A peer may establish a link without the flag — `DFLAG_ALIAS` is in OTP 29's default and
/// addable masks but not in its mandatory set — so an alias send over such a link is a
/// protocol shortfall the caller must see, not a silent drop and not a bare `BadArg`.
pub(crate) fn alias_flag_error(label: &str) -> BeamError {
    error::message(format!(
        "distribution: the link to {label} did not negotiate DFLAG_ALIAS"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read, Write};
    use std::net::TcpStream;

    use beam_alloc::Heap;
    use beam_codec::term::{make_external_pid, make_external_ref};
    use beam_dist::driver::{ConnectionDriver, DriverState};
    use beam_dist::{Delivery, StepOutcome, UnsupportedSink};
    use beam_process::{ProcessId, RemotePid};
    use beam_term::prelude::{make_atom, make_small, AtomTable};

    use crate::dist::DIST_READ_SLICE;
    use crate::dist_client::DistClient;

    /// The cookie every loopback fixture below digests with.
    const COOKIE: &[u8] = b"dist-links-unit-cookie";

    /// The peer that plays the *connecting* role in an accepted-link fixture.
    const PEER_NAME: &str = "peer@host.example";

    /// This runtime's own node name in an accepted-link fixture.
    const ACCEPTOR_NAME: &str = "acceptor@host.example";

    /// A connected loopback pair, each end configured exactly like a real link
    /// (`TCP_NODELAY` + the [`DIST_READ_SLICE`] read timeout) — the `dist` suite's fixture,
    /// copied because its own helper is private to that module.
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

    /// An **established** accepted link, returned with the peer's driver and socket.
    ///
    /// The runtime side is a real [`DistConnection`] over one end of a loopback pair (the
    /// production `ConnectionDriver::server`), the peer the connecting side of the same
    /// `'N'` handshake — so the link is established by exactly the bytes a real peer sends,
    /// with no `erl` and no `epmd`.
    fn establishing_accepted() -> (DistConnection, ConnectionDriver, TcpStream) {
        establishing_accepted_with_peer_flags(beam_dist::preferred_flags())
    }

    /// [`establishing_accepted`] with an explicit **peer** (connecting-side) flag set, so a
    /// fixture can withhold a non-mandatory capability — `DFLAG_ALIAS` in particular.
    fn establishing_accepted_with_peer_flags(
        peer_flags: beam_dist::DistFlags,
    ) -> (DistConnection, ConnectionDriver, TcpStream) {
        let (server_stream, mut peer_stream) = socket_pair();
        let peer_addr = server_stream.peer_addr().expect("the peer's address");
        let mut connection = DistConnection::from_parts(
            ConnectionDriver::server(ACCEPTOR_NAME.as_bytes(), COOKIE, 1),
            server_stream,
            peer_addr,
        );
        let mut peer =
            ConnectionDriver::client_with_flags(PEER_NAME.as_bytes(), COOKIE, 1, peer_flags);
        for _ in 0..200 {
            connection.pump().expect("the accepted link's pump");
            read_into(&mut peer, &mut peer_stream);
            step_peer(&mut peer, &mut peer_stream);
            if connection.is_established() && matches!(peer.state(), DriverState::Established) {
                return (connection, peer, peer_stream);
            }
        }
        panic!("the loopback 'N' handshake never completed");
    }

    /// Binds an established accepted link's peer **node atom term**, exactly as
    /// [`crate::dist::DistNode::bind_peer_nodes`] does from the scheduler pass: the peer's
    /// `node@host` name is interned in the live table and the resulting term handed to the
    /// link. Returns the bound term so a test can address the peer by it.
    fn bind_peer(connection: &mut DistConnection, atoms: &mut AtomTable) -> Eterm {
        let peer = connection
            .peer_name()
            .expect("an established link knows its peer's name");
        let atom = make_atom(atoms.intern(&peer));
        connection.bind_node(atom);
        atom
    }

    /// A toolchain-free **outbound** pair: a [`DistClient`] that dialled a loopback listener
    /// speaking the server side of the handshake, established and ready to route. The
    /// `dist_client` suite's fixture, copied for the same reason as [`socket_pair`].
    fn dialled_outbound() -> (DistClient, ConnectionDriver, TcpStream, AtomTable) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener.local_addr().expect("read the port");
        let mut atoms = AtomTable::new();
        let our = make_atom(atoms.intern(ACCEPTOR_NAME));
        let client = DistClient::new(COOKIE.to_vec()).expect("client");
        client.set_identity(ACCEPTOR_NAME, our, 1, COOKIE);
        client
            .connect_addr(PEER_NAME, addr, &mut atoms)
            .expect("dial the loopback peer");
        let (mut stream, _) = listener.accept().expect("the client connected");
        stream
            .set_read_timeout(Some(DIST_READ_SLICE))
            .expect("read timeout");
        let mut peer = ConnectionDriver::server(PEER_NAME.as_bytes(), COOKIE, 1);
        for _ in 0..200 {
            client.pump(&mut atoms).expect("client pump");
            read_into(&mut peer, &mut stream);
            step_peer(&mut peer, &mut stream);
            if matches!(peer.state(), DriverState::Established) && client.is_connected(PEER_NAME) {
                return (client, peer, stream, atoms);
            }
        }
        panic!("the loopback handshake never completed");
    }

    /// Feeds the peer's driver whatever the link wrote.
    fn read_into(peer: &mut ConnectionDriver, stream: &mut TcpStream) {
        let mut buf = [0u8; 8192];
        match stream.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => peer.feed(&buf[..n]),
            Err(err) if is_would_block(&err) => {}
            Err(err) => panic!("the peer's read: {err}"),
        }
    }

    /// Steps the peer's driver to quiescence and writes whatever it wants to say.
    fn step_peer(peer: &mut ConnectionDriver, stream: &mut TcpStream) {
        loop {
            match peer.step() {
                Ok(StepOutcome::WouldBlock) => break,
                Ok(_) => {}
                Err(err) => panic!("the peer's step: {err}"),
            }
        }
        if peer.has_output() {
            let bytes = peer.take_output();
            stream.write_all(&bytes).expect("the peer's write");
            stream.flush().expect("the peer's flush");
        }
    }

    /// What the peer decoded from the bytes the link wrote (`DOP_*` classification only;
    /// the per-link identity check is a different seam).
    ///
    /// `heap` is the **caller's**: every term a `Delivery` carries is materialized there, so
    /// it must outlive the assertions (a heap owned by this helper would be dropped on
    /// return, leaving the returned terms naming recycled memory).
    fn decoded(
        peer: &mut ConnectionDriver,
        stream: &mut TcpStream,
        atoms: &mut AtomTable,
        heap: &mut Heap,
    ) -> Vec<Delivery> {
        read_into(peer, stream);
        peer.step().expect("the peer's step");
        let mut sink = UnsupportedSink;
        peer.dispatch_inbound(heap, atoms, &mut sink)
            .expect("classify the control message the link wrote")
            .into_iter()
            .map(|record| record.delivery)
            .collect()
    }

    /// A read that timed out (or would have blocked) is "no data", never an error.
    fn is_would_block(err: &std::io::Error) -> bool {
        matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
    }

    /// A message tuple `{hello, N}` on `heap`.
    fn hello(heap: &mut Heap, atoms: &mut AtomTable, n: i64) -> Eterm {
        let tag = make_atom(atoms.intern("hello"));
        heap.tuple(&[tag, make_small(n)]).expect("the message tuple")
    }

    /// The routing entry finds a link this runtime **dialled**: a `DOP_REG_SEND` written
    /// through it arrives at the peer as the same `{DOP_REG_SEND, From, Cookie, ToName} --
    /// Msg` control message.
    #[test]
    fn the_routing_entry_finds_an_outbound_link() {
        let (client, mut peer, mut stream, mut atoms) = dialled_outbound();
        let mut heap = Heap::new(64).expect("the message heap");
        let msg = hello(&mut heap, &mut atoms, 42);
        client
            .send_reg_send(PEER_NAME, "echo", ProcessId::new(1, 1), msg, &mut atoms)
            .expect("route the DOP_REG_SEND over the outbound link");

        let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
        match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
            Some(Delivery::RegSend { name, message, .. }) => {
                assert_eq!(
                    *name,
                    make_atom(atoms.intern("echo")),
                    "the routed control names the registered process"
                );
                let mut expected_heap = Heap::new(64).expect("the expected heap");
                let expected = hello(&mut expected_heap, &mut atoms, 42);
                assert!(
                    beam_term::compare::eq(*message, expected, &atoms),
                    "the routed message reaches the outbound peer intact"
                );
            }
            other => panic!("expected a DOP_REG_SEND on the outbound link, got {other:?}"),
        }
    }

    /// The routing entry finds a link a peer **dialled into** this runtime — the regression
    /// this table exists for: when a system `erl` dials `beam_rt`, the runtime's only link is
    /// an accepted one, and an outbound `DOP_REG_SEND` must still reach it.
    #[test]
    fn the_routing_entry_finds_an_accepted_link() {
        let (mut connection, mut peer, mut stream) = establishing_accepted();
        let mut atoms = AtomTable::new();
        // The link's peer node term, bound exactly as the scheduler pass binds it — the
        // accepted half is an exact node-term match, so an unbound link is no route.
        let node = bind_peer(&mut connection, &mut atoms);
        assert_eq!(node, make_atom(atoms.intern(PEER_NAME)));
        let accepted = Rc::new(RefCell::new(vec![connection]));
        let mut links = DistLinks::new(Rc::clone(&accepted));
        let mut heap = Heap::new(64).expect("the message heap");
        let from = make_external_pid(
            &mut heap,
            make_atom(atoms.intern(PEER_NAME)),
            1,
            0,
            1,
        )
        .expect("the sender's external pid");
        let empty = make_atom(atoms.intern(""));
        let to_name = make_atom(atoms.intern("echo"));
        let msg = hello(&mut heap, &mut atoms, 7);
        links
            .route_control(node, Dop::RegSend, &[from, empty, to_name, msg], &atoms)
            .expect("the routing entry finds the accepted link");

        let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
        match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
            Some(Delivery::RegSend { name, message, .. }) => {
                assert_eq!(
                    *name,
                    make_atom(atoms.intern("echo")),
                    "the routed control names the registered process"
                );
                let mut expected_heap = Heap::new(64).expect("the expected heap");
                let expected = hello(&mut expected_heap, &mut atoms, 7);
                assert!(
                    beam_term::compare::eq(*message, expected, &atoms),
                    "the routed message reaches the accepted peer intact"
                );
            }
            other => panic!("expected a DOP_REG_SEND on the accepted link, got {other:?}"),
        }
    }

    /// A node with no link in **either** half is the typed refusal, for the control entry
    /// and the message entry alike — never a silent drop and never a fabricated success.
    #[test]
    fn a_node_with_no_link_is_a_typed_error() {
        let mut links = DistLinks::new(Rc::new(RefCell::new(Vec::new())));
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("absent@host.example"));

        let err = links
            .route_control(node, Dop::Link, &[make_small(1), make_small(2)], &atoms)
            .expect_err("no link on either half, so the control must be refused");
        assert!(
            err.to_string()
                .contains("no established connection to node absent@host.example"),
            "the control entry reports the typed refusal: {err}"
        );

        let err = links
            .route_message(node, make_small(1), make_small(2), make_small(3), &atoms)
            .expect_err("no link on either half, so the message must be refused");
        assert!(
            err.to_string()
                .contains("no established connection to node absent@host.example"),
            "the message entry reports the typed refusal: {err}"
        );
    }

    /// The runtime's [`DistSignalRouter`](beam_process::DistSignalRouter) reaches an accepted
    /// link: a routed `DistSignal::Exit` is queued by the shared cell's table and written as a
    /// `DOP_EXIT` the accepted peer decodes.
    #[test]
    fn the_router_reaches_an_accepted_link() {
        let (mut connection, mut peer, mut stream) = establishing_accepted();
        let mut atoms = AtomTable::new();
        let bound = bind_peer(&mut connection, &mut atoms);
        let accepted = Rc::new(RefCell::new(vec![connection]));
        let our = make_atom(atoms.intern(ACCEPTOR_NAME));
        let client = DistClient::new(COOKIE.to_vec()).expect("client");
        client.set_identity(ACCEPTOR_NAME, our, 1, COOKIE);
        client
            .share_links_with(Rc::clone(&accepted))
            .expect("install the node's accepted links");

        let mut heap = Heap::new(64).expect("the reason heap");
        let remote = RemotePid::from_eterm(
            make_external_pid(&mut heap, bound, 5, 0, 1).expect("the target's external pid"),
        )
        .expect("a remote target pid");
        let boom = make_atom(atoms.intern("boom"));
        let reason = heap
            .tuple(&[boom, make_small(17)])
            .expect("the exit reason");
        let mut router = client.router();
        router
            .route(beam_process::DistSignal::Exit {
                from: ProcessId::new(2, 0),
                to: remote,
                reason,
            })
            .expect("the router accepts a signal for an accepted link's node");
        client.pump(&mut atoms).expect("flush the queued exit");

        let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
        match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
            Some(Delivery::Exit { reason, .. }) => {
                let mut expected_heap = Heap::new(64).expect("the expected heap");
                let expected = expected_heap
                    .tuple(&[boom, make_small(17)])
                    .expect("the expected reason");
                assert!(
                    beam_term::compare::eq(*reason, expected, &atoms),
                    "the routed exit reason reaches the accepted peer intact"
                );
            }
            other => panic!("expected a DOP_EXIT on the accepted link, got {other:?}"),
        }
    }

    /// A routed signal for a node **no link reaches** is the typed refusal even when
    /// another link is present — and it is refused **by the router**, not accepted and
    /// silently dropped by the later flush (contract A.5: a signal for a node with no link
    /// is a typed error, never a silent drop).
    ///
    /// The fixture holds one **established accepted** link bound to peer
    /// `peer@host.example`, and addresses `other@host.example`: an accepted half that only
    /// knew "some link exists" would have answered `Ok` here, queued the control, and let
    /// `flush_pending` drop it with nothing but a recorded reason to show for it.
    #[test]
    fn a_routed_signal_for_an_unlinked_node_is_a_typed_error_even_with_another_link_present() {
        let (mut connection, _peer, _stream) = establishing_accepted();
        let mut atoms = AtomTable::new();
        let bound = bind_peer(&mut connection, &mut atoms);
        assert_eq!(
            bound,
            make_atom(atoms.intern(PEER_NAME)),
            "the accepted link is bound to its own peer"
        );
        let accepted = Rc::new(RefCell::new(vec![connection]));
        let our = make_atom(atoms.intern(ACCEPTOR_NAME));
        let client = DistClient::new(COOKIE.to_vec()).expect("client");
        client.set_identity(ACCEPTOR_NAME, our, 1, COOKIE);
        client
            .share_links_with(Rc::clone(&accepted))
            .expect("install the node's accepted links");

        let mut heap = Heap::new(64).expect("the reason heap");
        let other = make_atom(atoms.intern("other@host.example"));
        let remote =
            RemotePid::from_eterm(make_external_pid(&mut heap, other, 5, 0, 1).expect("the pid"))
                .expect("a remote target pid");
        let reason = make_atom(atoms.intern("normal"));
        let mut router = client.router();
        let err = router
            .route(beam_process::DistSignal::Exit {
                from: ProcessId::new(2, 0),
                to: remote,
                reason,
            })
            .expect_err("no link reaches other@host.example, so the signal must be refused");
        assert!(
            err.to_string()
                .contains("no established connection to node"),
            "the router reports the typed refusal: {err}"
        );
        assert_eq!(
            client.pump(&mut atoms).expect("pump the client"),
            0,
            "nothing was queued for the peer"
        );
        assert!(
            client.last_error().is_none(),
            "the refusal happened at the router, not as a silent flush drop: {:?}",
            client.last_error()
        );
    }

    /// An alias send reaches an accepted peer as `{DOP_ALIAS_SEND, From, To} -- Msg`
    /// (`dist.c:2563`): the destination node is read out of the alias **reference** term
    /// itself, so the routing entry needs no separate node argument.
    ///
    /// The link's peer advertised `DFLAG_ALIAS` (it is in OTP 29's default mask), so the
    /// gate passes and the opcode is written; the peer decodes the very reference term the
    /// caller held (compared structurally, `beam_term::compare::eq`) and the message.
    #[test]
    fn an_alias_send_writes_a_dop_alias_send() {
        let (mut connection, mut peer, mut stream) = establishing_accepted();
        let mut atoms = AtomTable::new();
        let node = bind_peer(&mut connection, &mut atoms);
        let accepted = Rc::new(RefCell::new(vec![connection]));
        let mut links = DistLinks::new(Rc::clone(&accepted));
        let mut heap = Heap::new(64).expect("the alias heap");
        let to = make_external_ref(&mut heap, node, &[7, 11], 5).expect("the alias reference");
        let from = make_external_pid(&mut heap, make_atom(atoms.intern(ACCEPTOR_NAME)), 9, 0, 1)
            .expect("the sender's external pid");
        let msg = hello(&mut heap, &mut atoms, 3);
        links
            .route_alias(to, from, msg, &atoms)
            .expect("the routing entry writes the DOP_ALIAS_SEND");

        let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
        match decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).first() {
            Some(Delivery::AliasSend {
                to: got_to,
                message,
                ..
            }) => {
                assert!(
                    beam_term::compare::eq(*got_to, to, &atoms),
                    "the alias reference reaches the peer intact (same node/ids/creation)"
                );
                let mut expected_heap = Heap::new(64).expect("the expected heap");
                let expected = hello(&mut expected_heap, &mut atoms, 3);
                assert!(
                    beam_term::compare::eq(*message, expected, &atoms),
                    "the routed message reaches the accepted peer intact"
                );
            }
            other => panic!("expected a DOP_ALIAS_SEND, got {other:?}"),
        }
    }

    /// A link whose peer did **not** offer `DFLAG_ALIAS` refuses the alias send with the
    /// typed `did not negotiate DFLAG_ALIAS` — never a silent drop, never a bare `BadArg`.
    ///
    /// OTP 29 advertises `DFLAG_ALIAS` in its default **and** addable masks but does not
    /// make it mandatory, so a peer may establish a link without it: the fixture's peer
    /// offers only the mandatory set, which still completes the `'N'` handshake.
    #[test]
    fn an_alias_send_without_the_alias_flag_is_a_typed_error() {
        let (mut connection, mut peer, mut stream) =
            establishing_accepted_with_peer_flags(beam_dist::DistFlags::from_bits(
                beam_dist::flags::DFLAG_DIST_MANDATORY,
            ));
        assert!(
            !negotiated_alias(connection.negotiated()),
            "the fixture's link did not negotiate DFLAG_ALIAS"
        );
        let mut atoms = AtomTable::new();
        let node = bind_peer(&mut connection, &mut atoms);
        let accepted = Rc::new(RefCell::new(vec![connection]));
        let mut links = DistLinks::new(Rc::clone(&accepted));
        let mut heap = Heap::new(64).expect("the alias heap");
        let to = make_external_ref(&mut heap, node, &[1, 2], 1).expect("the alias reference");
        let from = make_external_pid(&mut heap, make_atom(atoms.intern(ACCEPTOR_NAME)), 2, 0, 1)
            .expect("the sender's external pid");
        let msg = hello(&mut heap, &mut atoms, 5);

        let err = links
            .route_alias(to, from, msg, &atoms)
            .expect_err("a link without DFLAG_ALIAS must refuse an alias send");
        assert!(
            err.to_string().contains("DFLAG_ALIAS"),
            "the refusal names the missing capability: {err}"
        );
        assert!(
            err.to_string()
                .contains("the link to peer@host.example did not negotiate DFLAG_ALIAS"),
            "the refusal names the link's node: {err}"
        );
        let mut decode_heap = Heap::new(1 << 12).expect("the decode heap");
        assert!(
            decoded(&mut peer, &mut stream, &mut atoms, &mut decode_heap).is_empty(),
            "nothing was written to the peer"
        );
    }
}
