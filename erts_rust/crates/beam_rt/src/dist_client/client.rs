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

//! [`DistClient`] — the runtime's client-side distribution registry — and the shared state every
//! half of it borrows ([`DistClientInner`]).
//!
//! The module-level contract (the handshake order, why a routed signal is materialised rather
//! than sent inline, the `DOP_*` argument order and the honest limits) is [`super`]'s docs. This
//! file owns the registry's public API, the outbound queue's drain/flush and the per-link
//! inbound dispatch; the socket-side state machine is
//! [`ClientLink`](super::handshake::ClientLink)'s.

use std::cell::{RefCell, RefMut};
use std::collections::VecDeque;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::rc::Rc;

use beam_alloc::Heap;
use beam_codec::term::{make_external_pid, make_external_ref};
use beam_common::{BeamError, Result};
use beam_dist::driver::ConnectionDriver;
use beam_dist::{Dop, InboundSink};
use beam_process::{DistSignal, DistSignalRouter, ProcessId, RemotePid, RemoteWatcherRef};
use beam_term::prelude::{make_atom, make_small, Eterm};
use beam_term::AtomTable;

use crate::dist::externalize::PayloadArena;
use crate::dist::links::no_link_error;
use crate::dist::{advertised_flags, DistConnection, DistLinks, NodeName, DIST_READ_SLICE};
use crate::error;

use super::handshake::ClientLink;
use super::lookup::{host_part, lookup_dist_port};
use super::DistRouter;
use super::signals::{monitor_ref_term, signal_node, unlink_id_term};

/// The words of the stable scratch heap the routed signals' terms are materialised on.
///
/// A routed signal carries at most a few pids, a reference and one reason term, so
/// this is generous; exceeding it is a typed error (`error:system_limit`), never a
/// silent drop.
const SCRATCH_WORDS: usize = 1 << 12;

/// The words of the arena one inbound control message is decoded on (reset per drain).
const DISPATCH_HEAP_WORDS: usize = 1 << 16;

/// A control message the [`DistSignalRouter`] queued for delivery.
///
/// `args` are `Eterm`s on [`DistClientInner::scratch`], so the whole queue is only
/// valid while that heap is not reset — which happens exactly when the queue empties.
struct Queued {
    /// The target peer's node atom term (the link key).
    node: Eterm,
    /// The control opcode.
    dop: Dop,
    /// The control message's arguments, already materialised on the scratch heap.
    args: Vec<Eterm>,
}

/// The shared state of the runtime's client half: the links, the outbound queue and
/// the two scratch heaps.
pub(crate) struct DistClientInner {
    /// This node's **advertised** name (the full `node@host`, as the handshake puts it
    /// on the wire). Refreshed by [`DistClient::set_identity`].
    our_name: Vec<u8>,
    /// This node's sysname atom term — what an external pid for this node carries.
    our_node: Eterm,
    /// This node's creation.
    creation: u32,
    /// The shared cookie the handshake digests use.
    cookie: Vec<u8>,
    /// **Both** distribution halves' links, and the one routing entry over them
    /// ([`DistLinks`]): the outbound links this client dialled plus the accepted cell
    /// [`crate::dist::DistNode`] owns, installed by [`DistClient::share_links_with`].
    links: DistLinks,
    /// Controls the router queued for delivery (their terms live on `scratch`).
    pending: VecDeque<Queued>,
    /// The stable heap the `pending` terms are materialised on. Reset **exactly** when
    /// `pending` empties, so a queued term can never dangle.
    scratch: Heap,
    /// The arena one inbound control message is decoded on (reset per drain; the
    /// delivery copies the term out immediately).
    dispatch_heap: Heap,
    /// The arena an outbound **payload** is re-materialised on when it carries one of this
    /// runtime's own pids/ports/references (see [`crate::dist::externalize`]). Every term
    /// it returns is consumed by the encoder inside the same send, which is the arena's
    /// lifetime contract.
    payloads: PayloadArena,
    /// Why the most recently retired link was dropped (recorded, never raised).
    last_error: Option<String>,
    /// Whether this node is **hidden** (`-hidden`): a link this client dials then
    /// advertises no `DFLAG_PUBLISHED`, exactly as the accepted half does
    /// ([`crate::dist::advertised_flags`]). Visibility belongs to the node, not to who
    /// dialled, so [`DistClient::set_hidden`] is set alongside
    /// [`crate::dist::DistNode::set_hidden`].
    hidden: bool,
    /// The next `DOP_UNLINK_ID` id (starts at `1`; a real receiver rejects `0`).
    pub(super) next_unlink_id: u64,
}

/// The runtime's **client-side** distribution registry (contract A.5, phase 2).
///
/// See the module docs. Every method is fallible with a typed
/// [`beam_common::BeamError`]; nothing panics on runtime data and nothing is
/// silently dropped.
pub struct DistClient {
    /// The links, the outbound queue and the scratch heaps — shared with the
    /// [`DistSignalRouter`] installed on the process table (see the module docs).
    pub(super) inner: Rc<RefCell<DistClientInner>>,
}

impl DistClient {
    /// An empty client with no links.
    ///
    /// The accepted half starts as this client's **own** empty cell — enough for the unit
    /// suites and for a runtime that never installs a node's table; [`DistClient::share_links_with`]
    /// replaces it with the live one.
    ///
    /// # Errors
    ///
    /// The scratch heaps' own allocation failures.
    pub fn new(cookie: Vec<u8>) -> Result<DistClient> {
        Ok(DistClient {
            inner: Rc::new(RefCell::new(DistClientInner {
                our_name: Vec::new(),
                our_node: make_small(0),
                creation: 0,
                cookie,
                links: DistLinks::new(Rc::new(RefCell::new(Vec::new()))),
                pending: VecDeque::new(),
                scratch: Heap::new(SCRATCH_WORDS)?,
                dispatch_heap: Heap::new(DISPATCH_HEAP_WORDS)?,
                payloads: PayloadArena::new()?,
                last_error: None,
                hidden: false,
                next_unlink_id: 1,
            })),
        })
    }

    /// Installs the **accepted** link table this client routes through.
    ///
    /// The listening half ([`crate::dist::DistNode`]) owns the links a peer dialled into
    /// this runtime and pumps them from the scheduler pass; both halves then share one
    /// table ([`DistLinks`]), so an outbound `link`/`monitor`/`exit`/`unlink` or message
    /// reaches a peer **whichever side opened the socket**. The runtime calls this once,
    /// before the [`DistSignalRouter`] is installed on the process table.
    ///
    /// # Errors
    ///
    /// The shared cell's re-entrant borrow (never in practice: this runs before anything is
    /// pumped).
    pub(crate) fn share_links_with(
        &self,
        accepted: Rc<RefCell<Vec<DistConnection>>>,
    ) -> Result<()> {
        self.borrow_mut()?.links.accepted = accepted;
        Ok(())
    }

    /// Installs this node's identity (the name/atom/creation an outbound handshake
    /// and every external pid this client builds must carry).
    ///
    /// The runtime calls this while building itself
    /// ([`Runtime::new`](crate::Runtime)) and again immediately before
    /// [`DistClient::connect`], because the node's creation is only known for certain
    /// **after** `epmd` answered its `ALIVE2` registration (see
    /// [`crate::dist::DistNode::creation`]); the creation half is refreshed at that one
    /// point by [`DistClient::set_creation`] (see
    /// [`Runtime::sync_dist_creation`](crate::Runtime)).
    pub fn set_identity(&self, our_name: &str, our_node: Eterm, creation: u32, cookie: &[u8]) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.our_name = our_name.as_bytes().to_vec();
            inner.our_node = our_node;
            inner.creation = creation;
            inner.cookie = cookie.to_vec();
        }
    }

    /// This node's creation **as this client has it** — the value every externalised
    /// pid/port/reference and every outbound handshake this half builds carries.
    ///
    /// It must equal [`Runtime::node_creation`](crate::Runtime) (the node's live
    /// creation, which is what `term_to_binary/1` and the `'N'` handshake use), or this
    /// runtime would advertise one incarnation and stamp another — see
    /// [`Runtime::sync_dist_creation`](crate::Runtime), the point that keeps them equal.
    ///
    /// `None` while another borrow of the shared cell is in flight (a pump or a drain is
    /// running and is rewriting the value): an unreadable answer, never a fabricated `0`.
    /// `Some(0)` is the honest answer before [`DistClient::set_identity`] was called.
    pub fn creation(&self) -> Option<u32> {
        self.inner.try_borrow().ok().map(|inner| inner.creation)
    }
    /// Refreshes **only** the creation (the half `epmd` assigns), leaving the name, node
    /// atom and cookie untouched.
    ///
    /// The creation is the one identity field that changes after the runtime is built
    /// (`DistNode`'s `ALIVE2` registration), so this is the cheap re-install
    /// [`Runtime::sync_dist_creation`](crate::Runtime) runs at the pump that finalises it —
    /// no allocation, and a no-op (no borrow taken for a write) once the value already
    /// matches, which is every pump after the first.
    pub fn set_creation(&self, creation: u32) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            if inner.creation != creation {
                inner.creation = creation;
            }
        }
    }
    /// Marks this node **hidden** (`-hidden`) — or, with `false`, published (the
    /// default).    ///
    /// An outbound link then advertises [`crate::dist::advertised_flags`]'s set, which
    /// omits `DFLAG_PUBLISHED` for a hidden node, so a real peer records this runtime as
    /// [`beam_dist::PeerVisibility::Hidden`] and leaves it out of `nodes()` — the
    /// mirror of [`crate::dist::DistNode::set_hidden`] on the accepted half. Visibility
    /// is a property of the node, not of who dialled, so both halves must be told the
    /// same value.
    ///
    /// Additive: it only affects links dialled afterwards (the flags are baked into a
    /// link's driver when it is created), which is why the runtime sets it before any
    /// pump.
    pub fn set_hidden(&self, hidden: bool) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.hidden = hidden;
        }
    }

    /// Whether this node dials out as **hidden** ([`DistClient::set_hidden`]).
    pub fn is_hidden(&self) -> bool {
        self.inner.try_borrow().map(|inner| inner.hidden).unwrap_or(false)
    }

    /// The `DistSignalRouter` sharing this client's links and outbound queue.
    ///
    /// Installed on the runtime's process table ([`beam_process::ProcessTable::set_dist_router`])
    /// so the process layer's outbound link/monitor/exit signals reach the peer.
    pub fn router(&self) -> Box<dyn DistSignalRouter> {
        Box::new(DistRouter {
            inner: Rc::clone(&self.inner),
        })
    }

    /// Dials `peer_name` (a `-name`/`-sname` value) and opens an outbound link.
    ///
    /// The peer's distribution listen port is resolved through `epmd`
    /// ([`lookup_dist_port`]), on the `epmd` at [`crate::dist::epmd_addr`] — the same
    /// address this node's `ALIVE2` registration uses, so a private `epmd`
    /// (`$ERL_EPMD_PORT`) serves both; the socket is opened, and the connecting
    /// state machine is created. Nothing is sent yet — the first
    /// [`DistClient::pump`] writes the `Name` frame, and
    /// [`crate::Runtime::dist_connect`] drives it to completion.
    ///
    /// # Errors
    ///
    /// The `epmd` lookup's failure (a peer that is not registered is a typed
    /// `Io(NotFound)`, never a fabricated port), the socket's own failures, and an
    /// identity that was never installed ([`DistClient::set_identity`]).
    pub fn connect(&self, peer_name: &str, atoms: &mut AtomTable) -> Result<()> {
        let peer = NodeName::parse(peer_name);
        let port = lookup_dist_port(peer.epmd())?;
        let host = host_part(peer.full());
        // A peer on this host is reached over the loopback when its name does not
        // resolve (a `-sname` node's host part is the local hostname); a genuinely
        // remote name resolves through the resolver.
        let addr = match (host.as_str(), port).to_socket_addrs() {
            Ok(mut addrs) => match addrs.next() {
                Some(addr) => addr,
                None => SocketAddr::from(([127, 0, 0, 1], port)),
            },
            Err(_) => SocketAddr::from(([127, 0, 0, 1], port)),
        };
        self.connect_addr(peer_name, addr, atoms)
    }

    /// [`DistClient::connect`] against an already-resolved socket address.
    ///
    /// The `epmd`-free core (and the toolchain-free test seam): the peer's name is
    /// still what the link is keyed by, so a routed signal addresses it exactly as
    /// the `epmd` route would.
    ///
    /// # Errors
    ///
    /// The socket's failures, and an identity that was never installed.
    pub fn connect_addr(
        &self,
        peer_name: &str,
        addr: SocketAddr,
        atoms: &mut AtomTable,
    ) -> Result<()> {
        let peer = NodeName::parse(peer_name);
        let stream = TcpStream::connect(addr).map_err(BeamError::Io)?;
        stream.set_nodelay(true).map_err(BeamError::Io)?;
        stream
            .set_read_timeout(Some(DIST_READ_SLICE))
            .map_err(BeamError::Io)?;
        let node = make_atom(atoms.intern(peer.full()));
        let mut inner = self.borrow_mut()?;
        if inner.our_name.is_empty() {
            return Err(error::message(
                "distribution: connect called before the node identity was installed",
            ));
        }
        // A re-connect to the same node replaces the old link (never two live links to
        // one peer, which a real node would treat as a simultaneous connection).
        inner.links.outbound.retain(|link| link.node != node);
        // The flags this link advertises are the **node's** visibility
        // ([`advertised_flags`]): a normal node publishes itself to the peer it dials
        // exactly as it does to one that dials it, and a `-hidden` node never does.
        let driver = ConnectionDriver::client_with_flags(
            &inner.our_name,
            &inner.cookie,
            inner.creation,
            advertised_flags(inner.hidden),
        );
        inner.links.outbound.push(ClientLink {
            node,
            name: peer.full().to_string(),
            driver,
            stream,
            bytes_in: 0,
            bytes_out: 0,
        });
        Ok(())
    }

    /// Advances every link one non-blocking step and delivers the outbound queue.
    ///
    /// Reads what has arrived, feeds it to each driver, writes the driver's reply and
    /// drains any routed control queued by the [`DistSignalRouter`] (encoding it with
    /// the live `atoms`). A link that fails is retired and recorded in
    /// [`DistClient::last_error`] — never raised, so a hostile or broken peer can
    /// neither fail nor stall a scheduler pass.
    ///
    /// # Errors
    ///
    /// Only a link's own I/O failure surfaces, and only as the *recorded* retirement
    /// (this is `Ok` in every case the caller must ignore); the `Result` exists for the
    /// scratch-borrow and heap conditions.
    pub fn pump(&self, atoms: &mut AtomTable) -> Result<usize> {
        let mut inner = self.borrow_mut()?;
        let events = inner.step_links();
        let sent = inner.flush_pending(atoms)?;
        Ok(events + sent)
    }

    /// Routes every control message the **established** links decoded through `sink`,
    /// returning how many reached a local mailbox.
    ///
    /// The mirror of [`crate::dist::DistNode::dispatch_inbound`]: the driver owns the
    /// queue, the receive atom cache and **the link's own identity**, the sink owns the
    /// routing. Every queued control message is therefore validated against **that**
    /// link's peer/own names (the C's per-opcode `invalid_message` shape checks and the
    /// `dep != external_pid_dist_entry(from)` anti-spoof check, `dist.c`) before it can
    /// reach a mailbox — an inbound `DOP_*` from a peer this runtime dialled out to is
    /// checked with the identity of *that* connection, never a pass-wide one. A
    /// link-level failure (a decode/classification failure **or** a validation failure)
    /// retires that one link and records why — it is never raised.
    pub fn dispatch_inbound(
        &self,
        atoms: &mut AtomTable,
        sink: &mut dyn InboundSink,
    ) -> Result<usize> {
        let mut inner = self.borrow_mut()?;
        inner.dispatch(atoms, sink)
    }

    /// Whether an outbound link to `peer_name` completed its `'N'` handshake.
    pub fn is_connected(&self, peer_name: &str) -> bool {
        let full = NodeName::parse(peer_name).full().to_string();
        match self.inner.try_borrow() {
            Ok(inner) => inner
                .links
                .outbound
                .iter()
                .any(|link| link.name == full && link.is_established()),
            Err(_) => false,
        }
    }

    /// Whether **either half's** link to `peer_name` completed its `'N'` handshake.
    ///
    /// The union exists because a link a peer **dialled into** this runtime is as much a link
    /// to that node as one this runtime dialled — `erlang:nodes/0` and `net_adm:ping/1` must
    /// see a peer that connected *us*, not only the peers we opened.
    ///
    /// The name is normalized through [`NodeName::parse`] exactly as
    /// [`DistClient::is_connected`] does, and the membership test is over
    /// [`DistLinks::established_names`] — the one union rule, so the two halves can never be
    /// matched differently. A cell that cannot be borrowed answers `false`, the same
    /// discipline [`DistClient::is_connected`] keeps.
    pub fn is_connected_any_half(&self, peer_name: &str) -> bool {
        let full = NodeName::parse(peer_name).full().to_string();
        match self.inner.try_borrow() {
            Ok(inner) => inner.links.established_names().iter().any(|name| *name == full),
            Err(_) => false,
        }
    }

    /// The **local link identity** of the established link to `peer_name` — the integer
    /// `erlang:nodes/2` reports as `connection_id` (`dist.c`'s `dep->connection_id`).
    ///
    /// The identity lives in the one link table both halves share, so a peer that dialled
    /// **into** this runtime has one exactly like a peer this runtime dialled: the name is
    /// normalized through [`NodeName::parse`] exactly as [`DistClient::is_connected_any_half`]
    /// does and the table assigns the id on its first request
    /// ([`DistLinks::connection_id_for`]), which keeps it stable for that link's lifetime.
    /// A cell that cannot be borrowed answers `None` — the caller then renders the atom
    /// `undefined` rather than a fabricated integer, the same discipline
    /// [`DistClient::is_connected_any_half`] keeps.
    pub fn connection_id_for(&self, peer_name: &str) -> Option<u64> {
        let full = NodeName::parse(peer_name).full().to_string();
        match self.inner.try_borrow_mut() {
            Ok(mut inner) => inner.links.connection_id_for(&full),
            Err(_) => None,
        }
    }

    /// Whether an outbound link to the node named by `node` (its atom term) completed
    /// its `'N'` handshake.
    pub fn is_connected_to_node(&self, node: Eterm) -> bool {
        match self.inner.try_borrow() {
            Ok(inner) => inner.links.link_index_for_node(node).is_some(),
            Err(_) => false,
        }
    }

    /// The full `node@host` names of every peer this runtime has **dialled** and established
    /// a link to (the outbound half only — see [`crate::dist::DistLinks::established_names`]
    /// for the union with the links a peer opened).
    pub fn connected_nodes(&self) -> Vec<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner
                .links
                .outbound
                .iter()
                .filter(|link| link.is_established())
                .map(|link| link.name.clone())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The full `node@host` names of every peer this runtime has an **established** link to,
    /// **either** half: the peers it dialled first, then the peers that dialled into it (the
    /// union [`DistLinks::established_names`] reports).
    ///
    /// The union exists because a link a peer **opened into** this runtime reaches that node
    /// just as an outbound one does, so `erlang:nodes/0` — which reads this — must not hide
    /// the peers that connected to us. [`DistClient::connected_nodes`] stays the outbound-only
    /// view. A client cell that cannot be borrowed answers `[]`, matching
    /// [`DistClient::connected_nodes`]; an accepted cell that cannot be borrowed contributes no
    /// accepted name (the union rule's own discipline, [`DistLinks::established_names`]).
    pub fn all_connected_nodes(&self) -> Vec<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner.links.established_names(),
            Err(_) => Vec::new(),
        }
    }

    /// Whether this client holds **any** link, handshaking or established, in either half.
    ///
    /// This is the scheduler pass's quiescence query on the connecting half: a peer link —
    /// even one whose `'N'` handshake has not completed yet — is work the pass must keep
    /// pumping, because an inbound `DOP_*` (or the handshake's own completion) can deliver a
    /// message and wake a process parked on a bare `receive`. A runtime with no link has
    /// nothing here that could ever wake anything, so this answers `false` and the pass is
    /// free to declare its stall.
    ///
    /// The complement of [`crate::dist::DistNode::is_idle`]: a node is idle when it has no
    /// live link, and this is the same question asked of the one link table both halves
    /// share ([`crate::dist::DistLinks`]).
    ///
    /// A cell that cannot be borrowed is reported as **holding a link**: a borrowed cell
    /// means distribution work is in flight (a pump or the signal router holds it), so the
    /// caller must keep pumping rather than substantiate a stall it cannot justify.
    pub fn has_links(&self) -> bool {
        match self.inner.try_borrow() {
            Ok(inner) => !inner.links.is_idle(),
            Err(_) => true,
        }
    }

    /// Whether this client holds **no** link at all — [`DistClient::has_links`]'s
    /// complement, and the same "nothing in flight" answer
    /// [`crate::dist::DistNode::is_idle`] gives for the accepted links.
    ///
    /// A cell that cannot be borrowed is reported as **not** idle, for the same reason
    /// [`DistClient::has_links`] reports `true` there.
    pub fn is_idle(&self) -> bool {
        !self.has_links()
    }

    /// Whether this client still owes the wire **work it has already accepted** — a routed
    /// signal sitting in the outbound queue, or bytes a link's driver has not yet written.
    ///
    /// The scheduler pass's *narrow* companion of [`DistClient::has_links`]: a link is pending
    /// work in principle (an inbound `DOP_*` can wake a parked process), while this asks
    /// whether something concrete is **mid-flight** and must be pumped at least once more.
    ///
    /// A queued signal is work even though [`DistClientInner::queue_signal`] gated it on
    /// [`DistLinks::may_reach_node`]: the link can be **retired** between queueing and the
    /// flush (the peer dies, the socket errors), and such an item is then dropped at the flush
    /// with the typed reason recorded in [`DistClient::last_error`] — which still requires the
    /// flush to run, so the pass may not declare a stall before it. The buffered-output half
    /// covers the same window on the wire side: a control written by a send path that did not
    /// get to flush (the driver buffers, the socket write fails) stays pending here.
    ///
    /// O(link count), allocation-free (two slice scans plus one `try_borrow`), and **declines
    /// to `true`** when a cell cannot be borrowed: a borrowed cell means distribution work is
    /// in flight, so the caller must keep pumping rather than substantiate a stall it cannot
    /// justify — the same discipline [`DistClient::has_links`] keeps.
    pub fn has_pending_outbound(&self) -> bool {
        match self.inner.try_borrow() {
            Ok(inner) => inner.has_pending_outbound(),
            Err(_) => true,
        }
    }

    /// Why the most recently retired link was dropped, if any.
    pub fn last_error(&self) -> Option<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner.last_error.clone(),
            Err(_) => None,
        }
    }

    /// Sends `msg` from `from` to the process registered as `name` on `peer_name`
    /// (`{DOP_REG_SEND, From, Cookie, ToName} -- Msg`).
    ///
    /// # Errors
    ///
    /// A typed error when there is no **established** link to `peer_name` in either half (an
    /// unconnected node is never a silent drop and never a fabricated success), plus
    /// the encoding/socket failures.
    pub fn send_reg_send(
        &self,
        peer_name: &str,
        name: &str,
        from: ProcessId,
        msg: Eterm,
        atoms: &mut AtomTable,
    ) -> Result<()> {
        let node = make_atom(atoms.intern(NodeName::parse(peer_name).full()));
        let to_name = make_atom(atoms.intern(name));
        let empty = make_atom(atoms.intern(""));
        let mut inner = self.borrow_mut()?;
        let local = inner.local_pid_term(from)?;
        let msg = inner.outbound_payload(msg, atoms)?;
        inner
            .links
            .route_control(node, Dop::RegSend, &[local, empty, to_name, msg], atoms)
    }

    /// Sends `msg` from `from` to the **external pid** `to`
    /// (`{DOP_SEND_SENDER|DOP_SEND, From, To} -- Msg`).
    ///
    /// # Errors
    ///
    /// A typed error when `to` is not an external pid, when there is no established
    /// link to its node in either half, plus the encoding/socket failures.
    pub fn send_message_pid(
        &self,
        to: Eterm,
        from: ProcessId,
        msg: Eterm,
        atoms: &mut AtomTable,
    ) -> Result<()> {
        let remote = RemotePid::from_eterm(to)?;
        let mut inner = self.borrow_mut()?;
        let local = inner.local_pid_term(from)?;
        let to_term = remote.to_eterm(&mut inner.scratch)?;
        let msg = inner.outbound_payload(msg, atoms)?;
        // `send_message` selects `DOP_SEND_SENDER` when the peer negotiated
        // `DFLAG_SEND_SENDER` and `DOP_SEND` otherwise (`dist.c:1686`), so the choice
        // cannot be made here — it is the driver's.
        inner
            .links
            .route_message(remote.node(), local, to_term, msg, atoms)
    }

    /// Sends `msg` from `from` to the **process alias** `to`
    /// (`{DOP_ALIAS_SEND, From, To} -- Msg`, `dist.c:2563`).
    ///
    /// `to` is an **external reference** term naming the alias; its node decides which link
    /// carries the message, exactly as [`DistClient::send_message_pid`]'s does. The link
    /// must have negotiated `DFLAG_ALIAS` (see [`DistLinks::route_alias`]).
    ///
    /// # Errors
    ///
    /// A typed error when `to` is not an external reference, when there is no established
    /// link to its node in either half, when that link did not negotiate `DFLAG_ALIAS`,
    /// plus the encoding/socket failures.
    pub fn send_alias(
        &self,
        to: Eterm,
        from: ProcessId,
        msg: Eterm,
        atoms: &mut AtomTable,
    ) -> Result<()> {
        let mut inner = self.borrow_mut()?;
        let local = inner.local_pid_term(from)?;
        let msg = inner.outbound_payload(msg, atoms)?;
        inner.links.route_alias(to, local, msg, atoms)
    }

    /// Borrows the shared inner state, mapping a live borrow to a typed error (never a
    /// panic: the router may be invoked while the runtime holds the cell).
    fn borrow_mut(&self) -> Result<RefMut<'_, DistClientInner>> {
        self.inner
            .try_borrow_mut()
            .map_err(|_| error::message("distribution: the client is already in use"))
    }
}

impl DistClientInner {
    /// Whether this client holds outbound work the next flush owes ([`DistClient::has_pending_outbound`]).
    ///
    /// Both halves are asked, because a routed control may travel on either one
    /// ([`DistLinks::route_control`]): the outbound links live in this cell, the accepted one
    /// behind the shared `Rc<RefCell<…>>` the node pumps.
    fn has_pending_outbound(&self) -> bool {
        if !self.pending.is_empty() {
            return true;
        }
        if self
            .links
            .outbound
            .iter()
            .any(|link| link.driver.has_output())
        {
            return true;
        }
        match self.links.accepted.try_borrow() {
            Ok(accepted) => accepted.iter().any(|link| link.has_pending_output()),
            // A borrowed accepted cell is a pump (or a route) in progress: pending work.
            Err(_) => true,
        }
    }

    /// Steps every **outbound** link, retiring the ones that died, and returns the event count.
    ///
    /// Only the outbound half is stepped here: the accepted links are the listening node's
    /// ([`crate::dist::DistNode::pump`]) and are driven by it in the same pass.
    fn step_links(&mut self) -> usize {
        let mut events = 0;
        let mut index = 0;
        while index < self.links.outbound.len() {
            let outcome = self.links.outbound[index].pump();
            match outcome {
                Ok(progress) => {
                    events += progress;
                    if self.links.outbound[index].is_live() {
                        index += 1;
                    } else {
                        self.retire(index, BeamError::BadArg);
                    }
                }
                Err(err) => self.retire(index, err),
            }
        }
        events
    }

    /// Removes the outbound link at `index` and records why.
    fn retire(&mut self, index: usize, err: BeamError) {
        let link = self.links.outbound.remove(index);
        self.last_error = Some(format!("{}: {err}", link.name));
    }

    /// The **wire form** of an outbound message payload: `msg` itself when it carries no
    /// identifier of this runtime's own (the gate's answer — nothing is copied and nothing
    /// is allocated), else a copy whose internal pids/ports/references are the external
    /// terms the wire carries.
    ///
    /// This is the **one** externalising step of the outbound path — every message family
    /// ([`DistClient::send_reg_send`], [`DistClient::send_message_pid`],
    /// [`DistClient::send_alias`]) funnels its payload through it — and it is what makes the
    /// canonical `Peer ! {reply, self()}` idiom sendable: `beam_dist` encodes every control
    /// with no local node identity of its own, so an internal pid has no wire form there
    /// (see [`crate::dist::externalize`]).
    ///
    /// # Errors
    ///
    /// The arena's allocation failures and the codec's own refusal for a payload the wire
    /// has no image for (a `make_fun3` local fun). A payload is never silently altered: a
    /// rewrite that cannot be completed is a typed error, exactly as an unencodable payload
    /// is today.
    fn outbound_payload(&mut self, msg: Eterm, atoms: &mut AtomTable) -> Result<Eterm> {
        self.payloads
            .externalize(msg, atoms, self.our_node, self.creation)
    }

    /// An **external pid** term for a local process: the `From`/`Watcher` element a
    /// routed signal or an outbound message must carry, so the peer can tell which
    /// node the pid lives on.
    fn local_pid_term(&mut self, pid: ProcessId) -> Result<Eterm> {
        make_external_pid(
            &mut self.scratch,
            self.our_node,
            pid.number() as u32,
            pid.serial() as u32,
            self.creation,
        )
    }

    /// Drains the outbound queue: every control whose node has an **established** link (in
    /// **either** half — see [`DistLinks`]) is encoded with the live `atoms` and written
    /// through the one routing entry; one whose link has not finished handshaking stays
    /// queued. The scratch heap is reset exactly when the queue empties, so no queued term
    /// can dangle.
    ///
    /// Both gates are **exact** node-term matches on both halves
    /// ([`DistLinks::names_node`]/[`DistLinks::has_established_link_to`]), the same terms
    /// [`DistLinks::may_reach_node`] gated the router's `route` with. The `names_node`
    /// branch is therefore a belt-and-braces guard, not the normal path: it fires only for a
    /// control that was queued while its link was alive and whose link died before the
    /// flush — the queue may not grow for ever, so such a control is dropped with the typed
    /// reason recorded in [`DistClientInner::last_error`] (the router already told its
    /// caller `Ok`, so recording is the only honest place the loss can surface).
    fn flush_pending(&mut self, atoms: &mut AtomTable) -> Result<usize> {
        if self.pending.is_empty() {
            return Ok(0);
        }
        let queued = std::mem::take(&mut self.pending);
        let mut retry: VecDeque<Queued> = VecDeque::new();
        let mut sent = 0;
        for item in queued {
            if !self.links.names_node(item.node) {
                // The link it waited for was retired: nothing in either half names that
                // node any more, so the control can never be delivered. Record the typed
                // refusal and drop it rather than growing the queue for ever.
                let label = self.links.node_label_in(item.node, atoms);
                self.last_error = Some(format!("{label}: {}", no_link_error(&label)));
                continue;
            }
            if !self.links.has_established_link_to(item.node) {
                // A link that names the node exists but has not completed its handshake
                // yet: keep it queued, bounded by the scratch heap.
                retry.push_back(item);
                continue;
            }
            match self
                .links
                .route_control(item.node, item.dop, &item.args, atoms)
            {
                Ok(()) => sent += 1,
                Err(err) => {
                    let label = self.links.node_label_in(item.node, atoms);
                    self.last_error = Some(format!("{label}: {err}"));
                }
            }
        }
        self.pending = retry;
        if self.pending.is_empty() {
            self.scratch.reset();
        }
        Ok(sent)
    }
    /// Decodes, **validates + classifies** and routes every control message the
    /// established links queued, using one scratch arena reset per drain.
    ///
    /// The checked drain ([`ConnectionDriver::dispatch_inbound_checked`]) is what makes
    /// each link validate against its **own** peer/own identity: the driver builds the
    /// [`beam_dist::LinkIdentity`] from its own handshake names, interned into the same
    /// table the message was decoded against, so a forged or malformed control message
    /// is an `Err` — which retires that one link (recorded in
    /// [`DistClientInner::last_error`]) instead of being mis-routed.
    ///
    /// Only the **outbound** half is drained here: the accepted links are the listening
    /// node's ([`crate::dist::DistNode::dispatch_inbound`]).
    fn dispatch(&mut self, atoms: &mut AtomTable, sink: &mut dyn InboundSink) -> Result<usize> {
        self.dispatch_heap.reset();
        // This node's **decode-side identity** (`beam_dist::control::LocalIdentity`), built
        // in the table this drain decodes against: an inbound pid/port/reference naming us
        // at our creation decodes to the internal term (the C's `dec_is_this_node`,
        // `external.c:3123-3129`), for the control arguments and the trailing message alike.
        //
        // Rebuilt per drain, not stored: an `Atom` is a table-local index, so an identity
        // interned into an earlier (possibly cloned) table could be silently inert. An
        // empty `our_name` means no identity was ever installed — then nothing is installed
        // here either, rather than naming the empty atom.
        let identity = if self.our_name.is_empty() {
            None
        } else {
            Some(beam_dist::control::LocalIdentity::intern(
                atoms,
                &self.our_name,
                self.creation,
            )?)
        };
        let mut delivered = 0;
        let mut index = 0;
        while index < self.links.outbound.len() {
            if !self.links.outbound[index].is_established() {
                index += 1;
                continue;
            }
            if let Some(identity) = identity {
                self.links.outbound[index].driver.set_local_identity(identity);
            }
            // The decode arena and the link are disjoint fields, so both are borrowed
            // for the duration of the decode only.
            let outcome = {
                let heap = &mut self.dispatch_heap;
                self.links.outbound[index]
                    .driver
                    .dispatch_inbound_checked(heap, atoms, sink)
            };
            match outcome {
                Ok(dispatched) => {
                    delivered += dispatched
                        .iter()
                        .filter(|record| record.outcome == beam_dist::DeliveryOutcome::Delivered)
                        .count();
                    index += 1;
                }
                Err(err) => self.retire(index, err),
            }
        }
        Ok(delivered)
    }

    /// Materialises one routed [`DistSignal`] onto the scratch heap and appends it to
    /// the outbound queue.
    ///
    /// **Synchronous materialisation is the contract**: the signal's `reason` may live
    /// in the dying process's heap, and the caller may drop that heap the moment this
    /// returns — so every term is written onto the stable scratch heap here.
    ///
    /// The gate is [`DistLinks::may_reach_node`], an **exact node-term** match on both
    /// halves: the outbound links key on their node atom term and the accepted links on the
    /// peer term [`crate::dist::DistNode::bind_peer_nodes`] bound for them, so a signal for
    /// a node **no** link reaches is the typed refusal here — never a queued control that a
    /// later flush would have to drop (the A.5 contract: a signal for a node with no link is
    /// a typed error, never a silent drop).
    pub(super) fn queue_signal(&mut self, signal: DistSignal) -> Result<()> {
        let node = signal_node(&signal);
        if !self.links.may_reach_node(node) {
            let label = self.links.node_label(node);
            return Err(no_link_error(&label));
        }
        let (dop, args) = self.encode_signal(signal)?;
        self.pending.push_back(Queued { node, dop, args });
        Ok(())
    }

    /// Writes the signal's control arguments onto the scratch heap and returns the
    /// opcode plus the arguments (see the module docs for the `DOP_*` order).
    ///
    /// # `DOP_MONITOR_P_EXIT` is emitted in the plain 5-tuple form
    ///
    /// The C prefers `DOP_PAYLOAD_MONITOR_P_EXIT` — 4 arguments with the reason as the
    /// trailing message body — when the **peer link** advertised `DFLAG_EXIT_PAYLOAD`
    /// (`dist.c:1511-1519`). That choice is deliberately not reproduced: the flags this
    /// runtime advertises ([`crate::dist::advertised_flags`]) describe what it *understands
    /// inbound*, not what it emits, and the plain `TUPLE5` form is legal on every peer that
    /// supports `DFLAG_DIST_MONITOR` (a **mandatory-set** flag — the C only skips the send
    /// when even that is absent, `dist.c:1504-1508`). So the plain form needs no per-link
    /// gate, cannot pick a shape a peer misreads, and the 4-tuple form buys nothing
    /// measurable here.
    pub(super) fn encode_signal(&mut self, signal: DistSignal) -> Result<(Dop, Vec<Eterm>)> {
        let our_node = self.our_node;
        let creation = self.creation;
        let next_id = self.next_unlink_id;
        self.next_unlink_id = self.next_unlink_id.wrapping_add(1).max(1);
        let foreign = |pid: ProcessId| {
            (
                pid.number() as u32,
                pid.serial() as u32,
            )
        };
        let heap = &mut self.scratch;
        let local = |heap: &mut Heap, pid: ProcessId| -> Result<Eterm> {
            let (number, serial) = foreign(pid);
            make_external_pid(heap, our_node, number, serial, creation)
        };
        match signal {
            DistSignal::Link { from, to } => Ok((
                Dop::Link,
                vec![local(heap, from)?, to.to_eterm(heap)?],
            )),
            DistSignal::Unlink { from, to } => Ok((
                Dop::UnlinkId,
                vec![
                    unlink_id_term(next_id)?,
                    local(heap, from)?,
                    to.to_eterm(heap)?,
                ],
            )),
            DistSignal::Monitor {
                watcher,
                target,
                reference,
                ..
            } => Ok((
                Dop::MonitorP,
                vec![
                    local(heap, watcher)?,
                    target.to_eterm(heap)?,
                    monitor_ref_term(heap, reference, our_node, creation)?,
                ],
            )),
            DistSignal::Demonitor {
                watcher,
                target,
                reference,
            } => Ok((
                Dop::DemonitorP,
                vec![
                    local(heap, watcher)?,
                    target.to_eterm(heap)?,
                    monitor_ref_term(heap, reference, our_node, creation)?,
                ],
            )),
            DistSignal::Exit { from, to, reason } => Ok((
                Dop::Exit,
                vec![
                    local(heap, from)?,
                    to.to_eterm(heap)?,
                    beam_message::copy_struct_simple(reason, heap)?,
                ],
            )),
            DistSignal::Exit2 { from, to, reason } => Ok((
                Dop::Exit2,
                vec![
                    local(heap, from)?,
                    to.to_eterm(heap)?,
                    beam_message::copy_struct_simple(reason, heap)?,
                ],
            )),
            // `{DOP_MONITOR_P_EXIT, Watched(local), Watcher(remote), Ref, Reason}` — the
            // C's `erts_dsig_send_m_exit` (`dist.c:1516`). **Watched first**, the reverse of
            // `DOP_MONITOR_P`'s `{watcher, watched, ref}`, because the tuple mirrors the
            // inbound `DOP_MONITOR_P` this node is answering.
            DistSignal::MonitorExit {
                watched,
                watcher,
                reference,
                reason,
            } => {
                let watched_term = local(heap, watched)?;
                let watcher_term = watcher.to_eterm(heap)?;
                let reference_term = match reference {
                    // The peer minted this reference, so it goes back **verbatim** — its own
                    // node, its own creation and its own id words. Substituting this node (or
                    // this node's creation) would name a reference the peer does not have, and
                    // its `{'DOWN', Ref, …}` would then match nothing.
                    RemoteWatcherRef::Remote(remote) => {
                        make_external_ref(heap, remote.node(), remote.ids(), remote.creation())?
                    }
                    // This node minted it, so it is spelled exactly as the monitor family
                    // spells one, through the same builder (`{node, creation, ids}`).
                    RemoteWatcherRef::Local(local_ref) => {
                        monitor_ref_term(heap, local_ref, our_node, creation)?
                    }
                };
                Ok((
                    Dop::MonitorPExit,
                    vec![
                        watched_term,
                        watcher_term,
                        reference_term,
                        beam_message::copy_struct_simple(reason, heap)?,
                    ],
                ))
            }
        }
    }
}
