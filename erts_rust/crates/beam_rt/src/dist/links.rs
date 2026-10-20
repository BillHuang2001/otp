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
use crate::hasher::FastHashMap;

/// The largest connection id this table assigns.
///
/// The id is the local link identity `erlang:nodes/2` reports, so it is materialised as a
/// term; staying inside the 60-bit small-integer range keeps it a plain integer (the ERTS
/// value is an opaque `UWord` no peer ever compares, and a node never has 2^59 live links).
const MAX_CONNECTION_ID: u64 = (1 << 59) - 1;

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
    /// The **local link identity** [`DistLinks::connection_id_for`] hands out —
    /// `erlang:nodes/2`'s `connection_id` — keyed by the link's `node@host` name.
    ///
    /// An id is minted on its first request and dropped only when the link it numbers is
    /// gone ([`DistLinks::connection_id_for`] prunes it), so it is stable for one link's
    /// whole lifetime and never reused for a re-established one (that is a new
    /// connection).
    connection_ids: FastHashMap<String, u64>,
    /// The next id to mint; starts at `1` (the C's `connection_id` is never `0` for a link
    /// and `~0` — the atom `undefined` — for `this`).
    next_connection_id: u64,
}

impl DistLinks {
    /// A table over `accepted` with no outbound link yet.
    pub(crate) fn new(accepted: Rc<RefCell<Vec<DistConnection>>>) -> DistLinks {
        DistLinks {
            outbound: Vec::new(),
            accepted,
            connection_ids: FastHashMap::default(),
            next_connection_id: 1,
        }
    }

    /// The **local link identity** of the established link to `name` — the integer ERTS
    /// reports as `nodes/2`'s `connection_id` (`dist.c`'s `dep->connection_id`).
    ///
    /// `name` is a peer's `node@host` name, matched against
    /// [`DistLinks::established_names`] (the one union rule both halves share), so a peer
    /// that dialled **into** this runtime has an identity exactly like one this runtime
    /// dialled. The id is assigned on the first request for a live link and kept in
    /// [`DistLinks::connection_ids`] until that link is gone: a caller asks twice and sees
    /// the same integer, which is what makes the value an identity rather than a
    /// per-call number. Names no longer established are pruned on every call, so the map
    /// is bounded by the number of live links; a **re-established** link gets a fresh id,
    /// because it genuinely is a new connection.
    ///
    /// `None` when no established link matches — the caller renders the C's `~0`, the atom
    /// `undefined`, and never a fabricated integer.
    pub(crate) fn connection_id_for(&mut self, name: &str) -> Option<u64> {
        let established = self.established_names();
        self.connection_ids
            .retain(|known, _| established.iter().any(|live| live == known));
        if !established.iter().any(|live| live == name) {
            return None;
        }
        if let Some(id) = self.connection_ids.get(name) {
            return Some(*id);
        }
        let id = self.next_connection_id;
        self.next_connection_id = if id >= MAX_CONNECTION_ID { 1 } else { id + 1 };
        self.connection_ids.insert(name.to_string(), id);
        Some(id)
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

    /// The `node@host` names of **every** link either half holds, whatever its handshake
    /// state — outbound first, then accepted.
    ///
    /// [`DistLinks::established_names`] answers "who can carry a `DOP_*` right now"; this
    /// answers "which peers is this runtime still connected to or still trying to connect
    /// to", which is what makes a **re-connect** a non-event for peer-loss handling
    /// ([`crate::Runtime::service_lost_dist_peers`]). A re-dial replaces an established
    /// outbound link with one that is still handshaking
    /// ([`crate::dist_client::DistClient`]'s `connect_addr`), so the old link's retirement
    /// records a lost peer while the replacement — not yet established — is the only link the
    /// table holds: the name is still listed here, and the loss must therefore not fire a
    /// `noconnection`.
    ///
    /// A cell that cannot be borrowed contributes no accepted name (the union rule's own
    /// discipline, [`DistLinks::established_names`]): a borrow in flight is work, never a
    /// claim about the peer.
    pub(crate) fn link_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.outbound.iter().map(|link| link.name.clone()).collect();
        if let Ok(accepted) = self.accepted.try_borrow() {
            names.extend(accepted.iter().filter_map(|connection| connection.peer_name()));
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

/// The **shared peer-loss recorder**: the `node@host` names of peers whose distribution link
/// was retired as **dead**, waiting for the scheduler pass to apply OTP's `noconnection`
/// semantics ([`crate::Runtime::service_lost_dist_peers`]).
///
/// One cell is owned by the [`Runtime`](crate::Runtime) and handed to **both** distribution
/// halves ([`crate::dist::DistNode::use_lost_peers`] and
/// [`crate::dist_client::DistClient::use_lost_peers`]), because either half may be the one
/// that observes a peer disappear: the accepting half pumps the links a peer dialled in, the
/// connecting half the links this runtime dialled.
///
/// The retirement sites **record**, they do not apply: they run inside a pump, which does not
/// hold the process table (a scheduler pass *moves* it out of the runtime). The pass drains
/// this recorder and calls [`beam_process::ProcessTable::node_down`], which is what makes a
/// remote monitor's `{'DOWN', Ref, process, Peer, noconnection}` and a remote link's
/// `{'EXIT', Peer, noconnection}` (trapping) or termination (non-trapping) fire — the
/// `noconnection` reason the C's `net_kernel` produces when a connection is lost.
///
/// A **graceful** shutdown never records ([`crate::dist::DistNode`]'s drop,
/// [`crate::dist_client::DistClient`]'s `close`): losing a link because this runtime is going
/// away is not the peer going down.
pub(crate) type LostPeers = Rc<RefCell<Vec<String>>>;

/// A fresh, empty peer-loss recorder.
pub(crate) fn new_lost_peers() -> LostPeers {
    Rc::new(RefCell::new(Vec::new()))
}

/// Whether no peer loss is pending — the scheduler step's allocation-free early test.
///
/// A recorder that cannot be borrowed has a record (or a drain) in flight, which is work in
/// progress: reported as "nothing pending" so the pass keeps its hands off it.
pub(crate) fn no_lost_peers(lost: &LostPeers) -> bool {
    match lost.try_borrow() {
        Ok(lost) => lost.is_empty(),
        Err(_) => true,
    }
}

/// Records `name` as a peer whose link was **retired as dead**.
///
/// Called by both halves' retirement sites *after* asking whether the retired link had
/// completed its `'N'` handshake — a link that never handshook was never a node down, so it
/// is never recorded (nothing on the process table can name such a peer anyway).
///
/// A recorder that cannot be borrowed means a drain is running; the retirement sites cannot
/// run inside one (a drain happens in the scheduler pass, between the pumps), and a `RefCell`
/// panic on runtime data is not an option, so a re-entrant borrow records nothing.
pub(crate) fn note_lost_peer(lost: &LostPeers, name: &str) {
    if let Ok(mut lost) = lost.try_borrow_mut() {
        lost.push(name.to_string());
    }
}

/// Takes every recorded lost peer out of `lost`, leaving it empty — the drain's half of
/// [`note_lost_peer`].
///
/// Allocation-free (the vector is moved out, the recorder is left with an empty one) and an
/// unreadable recorder (a record in flight) yields nothing, so those names stay recorded for
/// the next pass instead of being lost.
pub(crate) fn take_lost_peers(lost: &LostPeers) -> Vec<String> {
    match lost.try_borrow_mut() {
        Ok(mut lost) => std::mem::take(&mut *lost),
        Err(_) => Vec::new(),
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

/// The link table's suite — the routing entry over both halves, the connected-node union
/// and the typed refusals (see `src/dist/links/tests.rs`). A separate file so the loopback
/// socket fixtures do not grow this one.
#[cfg(test)]
mod tests;
