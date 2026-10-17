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

//! The **connecting (client) half** of Erlang distribution (contract A.5, phase 2):
//! a registry of *outbound* links this runtime dials, the routing of the process
//! layer's outbound link/monitor/exit signals onto them, and the sending of
//! messages to a peer.
//!
//! [`crate::dist`] is the runtime's **listening** half (bind a listener, register
//! with `epmd`, accept, drive the server side of the `'N'` handshake, route inbound
//! `DOP_*` into local mailboxes). This module is its mirror: the runtime *dials* a
//! peer it resolved through `epmd`, drives the **client** side of the same `'N'`
//! handshake, and carries a two-way `DOP_*` session on the resulting link.
//!
//! # This is wiring, not new protocol code
//!
//! Every byte of the wire already lives in the sibling `beam_dist` crate — the
//! connector handshake (`beam_dist::driver::ConnectionDriver::client`), the
//! control-message codec (`send_control`), the `epmd` `PORT2_REQ` lookup
//! (`beam_dist::epmd::lookup_port`) and the inbound classifier
//! (`ConnectionDriver::dispatch_inbound_checked`). What this module adds is the *driving
//! state* the runtime owns: a socket per peer, a bounded non-blocking pump, a
//! failure-recorded retirement policy and the [`DistSignalRouter`] implementation
//! the process table's outbound signals are routed through.
//!
//! # The handshake order (OTP's `handshake_we_started/1`)
//!
//! A connector sends `$N`NAME, receives `$s`STATUS, receives `$N`CHALLENGE, sends
//! `$r`REPLY and receives `$a`ACK; the steady state is 4-byte framing
//! (`lib/kernel/src/dist_util.erl:431-740`), and the challenge digest is
//! `md5(Cookie ++ integer_to_list(Challenge))` (`dist_util.erl:548`). The driver
//! implements exactly that sequence; this module only feeds it bytes and drains its
//! output.
//!
//! # There is **no** `{setup,…}` / `{nodeup,…}` control message
//!
//! A recurring guess about the distribution protocol is that a connector must
//! exchange a `{setup,…}`-style message (or a `nodeup`/`nodedown` pair) after the
//! handshake before the link carries traffic. **That protocol does not exist.**
//!
//! * The only `{setup,…}` in OTP is `net_kernel.erl:2287`, inside `verbose/3` — a
//!   `?dbg`-gated **debug trace** that is printed, never sent.
//! * Node up/down are purely each side's **local bookkeeping**: `mark_nodeup`
//!   (`net_kernel.erl:1468`) flips local state for the node that completed the
//!   handshake, and `beam_dist`'s own `delivery` module documents that nothing on
//!   the wire carries them (`DOP_*` has no node-up/down opcode — see
//!   `beam_dist::delivery`'s module docs).
//! * Consequently the link is **immediately usable for `DOP_*`** the moment the
//!   final `$a`ACK is seen: a peer holding the connection can `{Name, Us} ! Msg`
//!   with no further exchange. This module therefore does *not* implement such a
//!   message; [`DistClient::connect`] returns as soon as the driver reports
//!   [`DriverState::Established`].
//!
//! # Driving, and why a routed signal is *materialised* rather than sent inline
//!
//! [`DistClient::pump`] is called from the runtime's scheduler pass
//! ([`crate::runtime::Runtime::pump_dist_client`]). The sockets are non-blocking
//! with a short read timeout ([`crate::dist::DIST_READ_SLICE`]), so one pump reads
//! what has arrived, feeds it to the driver, writes the driver's reply and moves on
//! — a runtime with no link costs nothing here.
//!
//! The [`DistSignalRouter`] seam is installed on the runtime's **process table**,
//! which the runtime *moves out of itself* for the duration of a scheduler pass
//! (`Runtime::take_processes`/`put_processes`). A router living inside the table
//! therefore cannot borrow the runtime, and it cannot reach the runtime's atom
//! table either — yet [`DistSignalRouter::route`] must serialise **synchronously**
//! (its `reason` may live in the dying process's heap, which the caller may drop
//! the moment `route` returns). The router and the runtime therefore share one
//! `Rc<RefCell<…>>` cell ([`DistClient`]), and `route` only *materialises*: the
//! signal's terms are written onto a stable scratch heap ([`DistClientInner::scratch`])
//! and appended to a small outbound queue. Encoding (which needs the live atom
//! table) and the socket write happen in [`DistClient::pump`], the one place the
//! runtime's atom table is live. The scratch heap is reset exactly when the queue
//! empties, so no queued term can dangle.
//!
//! # The `DOP_*` argument order of the routed signals
//!
//! [`DistSignal`]'s variants carry two [`RemotePid`]s, a [`MonitorRef`] and (for
//! the exit family) a reason term. The wire tuples are the C's, taken from
//! `erts/emulator/beam/dist.c` — the ground truth, cross-checked against
//! `beam_dist/tests/live_interop.rs`:
//!
//! ```text
//! DistSignal::Link      -> {DOP_LINK,       From(local), To(remote)}         dist.c:1456
//! DistSignal::Unlink    -> {DOP_UNLINK_ID,  Id, From(local), To(remote)}     dist.c:1472
//! DistSignal::Monitor   -> {DOP_MONITOR_P,  Watcher(local), Watched(remote), Ref}  dist.c:1543
//! DistSignal::Demonitor -> {DOP_DEMONITOR_P,Watcher(local), Watched(remote), Ref}  dist.c:1566
//! DistSignal::Exit      -> {DOP_EXIT,       From(local), To(remote), Reason} dist.c:1456 & 1747
//! DistSignal::Exit2     -> {DOP_EXIT2,      From(local), To(remote), Reason} dist.c:1800
//! ```
//!
//! **Correction to a sibling's inline gloss.** `beam_process::remote`'s module
//! table glosses the monitor family as `&[to_term, watcher.to_eterm(), ref_term]`
//! and the unlink as `&[from, to]`; both differ from the C — the monitor family's
//! first element is the **watcher** (the sender's local pid, `dist.c:1543-1545`:
//! `TUPLE4(DOP_MONITOR_P, watcher, watched, ref)`), and `DOP_UNLINK_ID` carries a
//! leading **id** (`dist.c:1472`). This module follows `dist.c` (and the
//! `live_interop` evidence), because a real peer must never be mis-served. The
//! unlink id is minted here (a monotonic non-zero small integer): a real receiver
//! deletes the link by `from` and uses the id only for its acknowledgement
//! handshake (`erl_proc_sig_queue.c:6695-6726`), so any non-zero id is accepted and
//! effective.
//!
//! # Honest limits
//!
//! * **No auto-connect.** A routed signal for a node this runtime has no link to is
//!   a typed [`BeamError`] (never a silent drop and never a fabricated success); the
//!   caller connects with [`DistClient::connect`] first.
//! * **A link failure is recorded, not raised.** A protocol or I/O failure retires
//!   that one link and records why in [`DistClient::last_error`]; it never fails a
//!   scheduler pass.
//! * **The outbound queue is bounded by its scratch heap.** A signal whose terms do
//!   not fit is a typed error; nothing is silently discarded.

use std::cell::{RefCell, RefMut};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::rc::Rc;
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::term::{make_external_pid, make_external_ref};
use beam_common::{BeamError, Result};
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::{Dop, InboundSink, StepOutcome};
use beam_process::{DistSignal, DistSignalRouter, MonitorRef, ProcessId, RemotePid};
use beam_term::prelude::{make_atom, make_small, try_make_small, Eterm};
use beam_term::AtomTable;

use crate::dist::{NodeName, DIST_READ_SLICE};
use crate::error;

/// The words of the stable scratch heap the routed signals' terms are materialised on.
///
/// A routed signal carries at most a few pids, a reference and one reason term, so
/// this is generous; exceeding it is a typed error (`error:system_limit`), never a
/// silent drop.
const SCRATCH_WORDS: usize = 1 << 12;

/// The words of the arena one inbound control message is decoded on (reset per drain).
const DISPATCH_HEAP_WORDS: usize = 1 << 16;

/// How long [`crate::Runtime::dist_connect`] drives the handshake before giving up.
///
/// The connector handshake is a handful of round trips on an already-open socket, so
/// this bounds a stuck peer without a scheduler pass ever waiting on it (the pass
/// uses [`DistClient::pump`], which never blocks beyond one read slice).
pub const DIST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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

/// One outbound distribution link: the connecting state machine and its socket.
struct ClientLink {
    /// The peer's node atom term — the key a routed signal addresses.
    node: Eterm,
    /// The peer's full `node@host` name (diagnostic and [`DistClient::connected_nodes`]).
    name: String,
    /// The connecting side's handshake/established state machine.
    driver: ConnectionDriver,
    /// The socket the driver's bytes travel over.
    stream: TcpStream,
    /// Bytes read from the peer so far.
    bytes_in: u64,
    /// Bytes written to the peer so far.
    bytes_out: u64,
}

impl ClientLink {
    /// Whether the handshake completed and the link carries `DOP_*`.
    fn is_established(&self) -> bool {
        matches!(self.driver.state(), DriverState::Established)
    }

    /// Whether the link can still make progress (handshaking or established).
    fn is_live(&self) -> bool {
        matches!(
            self.driver.state(),
            DriverState::Handshaking | DriverState::Established
        )
    }

    /// Writes whatever the driver has queued to the socket.
    fn flush_output(&mut self) -> Result<()> {
        if self.driver.has_output() {
            let bytes = self.driver.take_output();
            self.stream.write_all(&bytes).map_err(BeamError::Io)?;
            self.stream.flush().map_err(BeamError::Io)?;
            self.bytes_out += bytes.len() as u64;
        }
        Ok(())
    }

    /// Reads whatever has arrived, advances the handshake and writes the reply.
    ///
    /// A read timeout (or a clean `WouldBlock`) is "no data": not an event and not an
    /// error. Returns how many protocol events took effect.
    fn pump(&mut self) -> Result<usize> {
        let mut events = 0;
        let mut buf = [0u8; 8192];
        match self.stream.read(&mut buf) {
            Ok(0) => {
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
        loop {
            match self.driver.step() {
                // No progress is possible without more input.
                Ok(StepOutcome::WouldBlock) => break,
                // A dead link: the caller retires it.
                Ok(StepOutcome::Failed) | Ok(StepOutcome::Closed) => {
                    events += 1;
                    break;
                }
                Ok(_) => events += 1,
                Err(err) => {
                    // A refusing/misbehaving peer may have buffered a status frame
                    // before the failure — drain it (the driver's contract) and then
                    // report the failure so the caller retires the link.
                    self.flush_output()?;
                    return Err(err);
                }
            }
        }
        self.flush_output()?;
        Ok(events)
    }
}

/// A read that timed out (or would have blocked) is "no data", never an error.
fn is_would_block(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// The shared state of the runtime's client half: the links, the outbound queue and
/// the two scratch heaps.
struct DistClientInner {
    /// This node's **advertised** name (the full `node@host`, as the handshake puts it
    /// on the wire). Refreshed by [`DistClient::set_identity`].
    our_name: Vec<u8>,
    /// This node's sysname atom term — what an external pid for this node carries.
    our_node: Eterm,
    /// This node's creation.
    creation: u32,
    /// The shared cookie the handshake digests use.
    cookie: Vec<u8>,
    /// The outbound links, one per peer node.
    links: Vec<ClientLink>,
    /// Controls the router queued for delivery (their terms live on `scratch`).
    pending: VecDeque<Queued>,
    /// The stable heap the `pending` terms are materialised on. Reset **exactly** when
    /// `pending` empties, so a queued term can never dangle.
    scratch: Heap,
    /// The arena one inbound control message is decoded on (reset per drain; the
    /// delivery copies the term out immediately).
    dispatch_heap: Heap,
    /// Why the most recently retired link was dropped (recorded, never raised).
    last_error: Option<String>,
    /// The next `DOP_UNLINK_ID` id (starts at `1`; a real receiver rejects `0`).
    next_unlink_id: u64,
}

/// The runtime's **client-side** distribution registry (contract A.5, phase 2).
///
/// See the module docs. Every method is fallible with a typed
/// [`beam_common::BeamError`]; nothing panics on runtime data and nothing is
/// silently dropped.
pub struct DistClient {
    /// The links, the outbound queue and the scratch heaps — shared with the
    /// [`DistSignalRouter`] installed on the process table (see the module docs).
    inner: Rc<RefCell<DistClientInner>>,
}

impl DistClient {
    /// An empty client with no links.
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
                links: Vec::new(),
                pending: VecDeque::new(),
                scratch: Heap::new(SCRATCH_WORDS)?,
                dispatch_heap: Heap::new(DISPATCH_HEAP_WORDS)?,
                last_error: None,
                next_unlink_id: 1,
            })),
        })
    }

    /// Installs this node's identity (the name/atom/creation an outbound handshake
    /// and every external pid this client builds must carry).
    ///
    /// The runtime calls this immediately before [`DistClient::connect`], because the
    /// node's creation is only known for certain **after** `epmd` answered its
    /// `ALIVE2` registration (see [`crate::dist::DistNode::creation`]).
    pub fn set_identity(&self, our_name: &str, our_node: Eterm, creation: u32, cookie: &[u8]) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.our_name = our_name.as_bytes().to_vec();
            inner.our_node = our_node;
            inner.creation = creation;
            inner.cookie = cookie.to_vec();
        }
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
    /// ([`beam_dist::epmd::lookup_port`]); the socket is opened, and the connecting
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
        let port = beam_dist::epmd::lookup_port(peer.epmd())?;
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
        inner.links.retain(|link| link.node != node);
        let driver = ConnectionDriver::client(&inner.our_name, &inner.cookie, inner.creation);
        inner.links.push(ClientLink {
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
                .iter()
                .any(|link| link.name == full && link.is_established()),
            Err(_) => false,
        }
    }

    /// Whether an outbound link to the node named by `node` (its atom term) completed
    /// its `'N'` handshake.
    pub fn is_connected_to_node(&self, node: Eterm) -> bool {
        match self.inner.try_borrow() {
            Ok(inner) => inner
                .link_index_for_node(node)
                .is_some_and(|index| inner.links[index].is_established()),
            Err(_) => false,
        }
    }

    /// The full `node@host` names of every peer this runtime has **established** a link to.
    pub fn connected_nodes(&self) -> Vec<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner
                .links
                .iter()
                .filter(|link| link.is_established())
                .map(|link| link.name.clone())
                .collect(),
            Err(_) => Vec::new(),
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
    /// A typed error when there is no **established** link to `peer_name` (an
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
        inner.send_to_node(node, Dop::RegSend, &[local, empty, to_name, msg], atoms)
    }

    /// Sends `msg` from `from` to the **external pid** `to`
    /// (`{DOP_SEND_SENDER|DOP_SEND, From, To} -- Msg`).
    ///
    /// # Errors
    ///
    /// A typed error when `to` is not an external pid, when there is no established
    /// link to its node, plus the encoding/socket failures.
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
        // `send_message` selects `DOP_SEND_SENDER` when the peer negotiated
        // `DFLAG_SEND_SENDER` and `DOP_SEND` otherwise (`dist.c:1686`), so the choice
        // cannot be made here — it is the driver's.
        inner.send_message_to_node(remote.node(), local, to_term, msg, atoms)
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
    /// Steps every link, retiring the ones that died, and returns the event count.
    fn step_links(&mut self) -> usize {
        let mut events = 0;
        let mut index = 0;
        while index < self.links.len() {
            let outcome = self.links[index].pump();
            match outcome {
                Ok(progress) => {
                    events += progress;
                    if self.links[index].is_live() {
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

    /// Removes the link at `index` and records why.
    fn retire(&mut self, index: usize, err: BeamError) {
        let link = self.links.remove(index);
        self.last_error = Some(format!("{}: {err}", link.name));
    }

    /// The index of the link to `node`, if any.
    fn link_index_for_node(&self, node: Eterm) -> Option<usize> {
        self.links.iter().position(|link| link.node == node)
    }

    /// A human-readable label for `node`: its `node@host` name if a link (established
    /// or still handshaking) is keyed by it, else a rendering of the raw term.
    fn node_label(&self, node: Eterm) -> String {
        self.links
            .iter()
            .find(|link| link.node == node)
            .map(|link| link.name.clone())
            .unwrap_or_else(|| format!("{node:?}"))
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

    /// Encodes `msg` (from `from` to `to`, both already external pids) and writes it to
    /// the node `node`'s link, selecting the send opcode from the negotiated flags.
    fn send_message_to_node(
        &mut self,
        node: Eterm,
        from: Eterm,
        to: Eterm,
        msg: Eterm,
        atoms: &mut AtomTable,
    ) -> Result<()> {
        let Some(index) = self.link_index_for_node(node) else {
            let label = self.node_label(node);
            return Err(no_link_error(&label));
        };
        self.send_on_link(index, |link, atoms| link.driver.send_message(from, to, msg, atoms), atoms)
    }

    /// Encodes one control message and writes it to the node `node`'s link.
    fn send_to_node(
        &mut self,
        node: Eterm,
        dop: Dop,
        args: &[Eterm],
        atoms: &mut AtomTable,
    ) -> Result<()> {
        let Some(index) = self.link_index_for_node(node) else {
            let label = self.node_label(node);
            return Err(no_link_error(&label));
        };
        self.send_on_link(
            index,
            |link, atoms| link.driver.send_control(dop, args, atoms),
            atoms,
        )
    }

    /// Runs `encode` against the link at `index` (which must be established) and writes
    /// the resulting bytes to its socket.
    fn send_on_link<F>(
        &mut self,
        index: usize,
        encode: F,
        _atoms: &AtomTable,
    ) -> Result<()>
    where
        F: FnOnce(&mut ClientLink, &AtomTable) -> Result<()>,
    {
        let link = &mut self.links[index];
        if !link.is_established() {
            return Err(error::message(format!(
                "distribution: the link to {} has not completed its handshake",
                link.name
            )));
        }
        encode(link, _atoms)?;
        link.flush_output()
    }

    /// Drains the outbound queue: every control whose node has an **established** link
    /// is encoded (with the live `atoms`) and written; one whose link has not finished
    /// handshaking stays queued. The scratch heap is reset exactly when the queue
    /// empties, so no queued term can dangle.
    fn flush_pending(&mut self, atoms: &mut AtomTable) -> Result<usize> {
        if self.pending.is_empty() {
            return Ok(0);
        }
        let queued = std::mem::take(&mut self.pending);
        let mut retry: VecDeque<Queued> = VecDeque::new();
        let mut sent = 0;
        for item in queued {
            match self.link_index_for_node(item.node) {
                Some(index) if self.links[index].is_established() => {
                    let link = &mut self.links[index];
                    match link.driver.send_control(item.dop, &item.args, atoms) {
                        Ok(()) => match link.flush_output() {
                            Ok(()) => sent += 1,
                            Err(err) => self.last_error = Some(format!("{}: {err}", link.name)),
                        },
                        Err(err) => self.last_error = Some(format!("{}: {err}", link.name)),
                    }
                }
                // No link yet (or one still handshaking): keep it queued, bounded by the
                // scratch heap.
                _ => retry.push_back(item),
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
    fn dispatch(&mut self, atoms: &mut AtomTable, sink: &mut dyn InboundSink) -> Result<usize> {
        self.dispatch_heap.reset();
        let mut delivered = 0;
        let mut index = 0;
        while index < self.links.len() {
            if !self.links[index].is_established() {
                index += 1;
                continue;
            }
            // The decode arena and the link are disjoint fields, so both are borrowed
            // for the duration of the decode only.
            let outcome = {
                let heap = &mut self.dispatch_heap;
                self.links[index]
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
    fn queue_signal(&mut self, signal: DistSignal) -> Result<()> {
        let node = signal_node(&signal);
        if self.link_index_for_node(node).is_none() {
            let label = self.node_label(node);
            return Err(no_link_error(&label));
        }
        let (dop, args) = self.encode_signal(signal)?;
        self.pending.push_back(Queued { node, dop, args });
        Ok(())
    }

    /// Writes the signal's control arguments onto the scratch heap and returns the
    /// opcode plus the arguments (see the module docs for the `DOP_*` order).
    fn encode_signal(&mut self, signal: DistSignal) -> Result<(Dop, Vec<Eterm>)> {
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
        }
    }
}

/// The node a routed signal addresses.
fn signal_node(signal: &DistSignal) -> Eterm {
    match signal {
        DistSignal::Link { to, .. }
        | DistSignal::Unlink { to, .. }
        | DistSignal::Exit { to, .. }
        | DistSignal::Exit2 { to, .. } => to.node(),
        DistSignal::Monitor { target, .. } | DistSignal::Demonitor { target, .. } => target.node(),
    }
}

/// The `{DOP_DEMONITOR_P, …, Ref}`/`{DOP_MONITOR_P, …, Ref}` reference element.
///
/// The reference **must** go on the wire as an **external** reference
/// (`NEWER_REFERENCE_EXT`, `{node, creation, ids}`), never as the in-heap internal ref
/// thing `MonitorRef::to_eterm` builds: an internal ref's `node` slot is the M1
/// substitution `make_small(0)` (a *small integer*, not an atom — see
/// [`beam_process::MonitorRef::to_eterm`]), which the ETF encoder cannot render as a
/// node name. `beam_dist`'s own live test
/// (`beam_dist/tests/live_interop.rs:1063`) sends exactly this external form to a real
/// `erl` peer, which accepts it and echoes it back in `DOP_MONITOR_P_EXIT`.
///
/// The three 32-bit identifier words carry the reference id, most significant first —
/// the same word count OTP uses for a distribution reference (`ERTS_REF_NUMBERS`).
fn monitor_ref_term(
    heap: &mut Heap,
    reference: MonitorRef,
    node: Eterm,
    creation: u32,
) -> Result<Eterm> {
    let id = reference.id();
    let ids = [0u32, (id >> 32) as u32, id as u32];
    make_external_ref(heap, node, &ids, creation)
}

/// The `DOP_UNLINK_ID` id element: a **checked** small integer.
///
/// The C's `DOP_UNLINK_ID` carries a small integer the receiver echoes back in its
/// `DOP_UNLINK_ID_ACK` (`erl_proc_sig_queue.c`), so the id has to be encodable as an
/// immediate — the wire's whole id slot. `make_small` is unchecked (it only
/// `debug_assert!`s its range), so casting a counter into it would panic in a debug build
/// and, in release, wrap into a **different** — possibly negative — id, which a peer would
/// then ack against the wrong unlink. An id past `MIN_SMALL..=MAX_SMALL` is therefore
/// refused with the crate's typed resource-limit error
/// ([`BeamError::SystemLimit`]), exactly as reference-id exhaustion is in
/// `beam_process::MonitorRef::to_eterm`.
///
/// Only reachable in principle: the counter starts at `1` and one id is spent per routed
/// unlink signal, so a node would need `MAX_SMALL` unlinks before it reached the bound.
fn unlink_id_term(id: u64) -> Result<Eterm> {
    // `MIN_SMALL` needs no test: a `u64` id is never negative, and `i64::try_from` rejects
    // the whole upper half of the `u64` space.
    let signed = i64::try_from(id).map_err(|_| BeamError::SystemLimit)?;
    try_make_small(signed).map_err(|_| BeamError::SystemLimit)
}

/// A typed "this runtime has no link to that node" error.
///
/// Never a silent drop: the caller (a link/monitor operation, or a dying process's
/// termination) can see exactly why the signal did not reach the wire.
fn no_link_error(label: &str) -> BeamError {
    error::message(format!(
        "distribution: no established connection to node {label}"
    ))
}

/// The host part of a `node@host` name.
fn host_part(full: &str) -> String {
    match full.split_once('@') {
        Some((_, host)) => host.to_string(),
        None => full.to_string(),
    }
}

/// The [`DistSignalRouter`] implementation over a shared [`DistClientInner`].
struct DistRouter {
    /// The shared client state installed on the runtime's process table.
    inner: Rc<RefCell<DistClientInner>>,
}

impl DistSignalRouter for DistRouter {
    fn route(&mut self, signal: DistSignal) -> Result<()> {
        let mut inner = self
            .inner
            .try_borrow_mut()
            .map_err(|_| error::message("distribution: the client is already in use"))?;
        inner.queue_signal(signal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap as TestHeap;
    use beam_dist::{Delivery, UnsupportedSink};
    use beam_process::{MonitorKind, RefAllocator};
    use beam_term::prelude::{make_atom as atom_term, AtomTable as Table};

    /// A toolchain-free loopback pair: the client dials a local listener that speaks
    /// the **server** side of the handshake. Returns the client and the peer's driver
    /// and socket.
    fn loopback(
        cookie: &str,
        creation: u32,
    ) -> (DistClient, ConnectionDriver, TcpStream, AtomTable) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener.local_addr().expect("read the port");
        let mut atoms = Table::new();
        let our = atom_term(atoms.intern("client@localhost"));
        let client = DistClient::new(cookie.as_bytes().to_vec()).expect("client");
        client.set_identity("client@localhost", our, creation, cookie.as_bytes());
        client
            .connect_addr("peer@localhost", addr, &mut atoms)
            .expect("dial the loopback peer");
        let (stream, _) = listener.accept().expect("the client connected");
        stream
            .set_read_timeout(Some(DIST_READ_SLICE))
            .expect("read timeout");
        let driver = ConnectionDriver::server(b"peer@localhost", cookie.as_bytes(), creation);
        (client, driver, stream, atoms)
    }

    /// Drives both sides until each reports `Established` (bounded).
    fn drive(
        client: &DistClient,
        driver: &mut ConnectionDriver,
        stream: &mut TcpStream,
        atoms: &mut AtomTable,
    ) {
        for _ in 0..200 {
            client.pump(atoms).expect("client pump");
            let mut buf = [0u8; 8192];
            match stream.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => driver.feed(&buf[..n]),
                Err(err) if is_would_block(&err) => {}
                Err(err) => panic!("peer read: {err}"),
            }
            loop {
                match driver.step() {
                    Ok(StepOutcome::WouldBlock) => break,
                    Ok(_) => {}
                    Err(err) => panic!("peer step: {err}"),
                }
            }
            if driver.has_output() {
                let bytes = driver.take_output();
                stream.write_all(&bytes).expect("peer write");
                stream.flush().expect("peer flush");
            }
            if matches!(driver.state(), DriverState::Established)
                && client.is_connected("peer@localhost")
            {
                return;
            }
        }
        panic!("the loopback handshake never completed");
    }

    /// An external pid for `peer@localhost`, built on a throwaway heap.
    fn remote_pid(atoms: &mut AtomTable, number: u32, creation: u32) -> RemotePid {
        let mut heap = TestHeap::new(16).expect("heap");
        let node = atom_term(atoms.intern("peer@localhost"));
        RemotePid::from_eterm(
            make_external_pid(&mut heap, node, number, 0, creation).expect("external pid"),
        )
        .expect("a remote pid")
    }

    /// The connector completes the `'N'` handshake against a server driver, and the
    /// link reports itself established — the state-machine half, with no `erl`.
    #[test]
    fn the_client_handshake_completes_against_a_server_driver() {
        let (client, mut driver, mut stream, mut atoms) = loopback("cook", 7);
        assert!(!client.is_connected("peer@localhost"), "not connected yet");
        drive(&client, &mut driver, &mut stream, &mut atoms);
        assert_eq!(client.connected_nodes(), vec!["peer@localhost".to_string()]);
        assert!(client.last_error().is_none(), "no link was retired");
    }

    /// A `DOP_REG_SEND` written through the client's public send API is decoded by the
    /// peer's driver as the same `{DOP_REG_SEND, From, Cookie, ToName} -- Msg` tuple —
    /// the outbound message path, with no `erl`.
    #[test]
    fn a_reg_send_reaches_the_peer_decoded() {
        let (client, mut driver, mut stream, mut atoms) = loopback("cook", 3);
        drive(&client, &mut driver, &mut stream, &mut atoms);

        let mut msg_heap = TestHeap::new(64).expect("heap");
        let hello = atom_term(atoms.intern("hello"));
        let msg = msg_heap.tuple(&[hello, make_small(42)]).expect("build the message");
        let from = ProcessId::new(1, 1);
        client
            .send_reg_send("peer@localhost", "echo", from, msg, &mut atoms)
            .expect("send DOP_REG_SEND");

        // Drain the bytes the client wrote and decode them on the peer.
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).expect("peer read");
        driver.feed(&buf[..n]);
        driver.step().expect("peer step");
        let mut heap = TestHeap::new(1 << 12).expect("heap");
        let mut sink = UnsupportedSink;
        let dispatched = driver
            .dispatch_inbound(&mut heap, &mut atoms, &mut sink)
            .expect("classify");
        let delivery = dispatched
            .first()
            .map(|record| record.delivery.clone())
            .expect("one control message was written");
        match delivery {
            Delivery::RegSend { name, message, .. } => {
                assert_eq!(name, atom_term(atoms.intern("echo")));
                // The decoded message is the same `{hello,42}` tuple.
                let mut expected_heap = TestHeap::new(64).expect("heap");
                let hello = atom_term(atoms.intern("hello"));
                let expected = expected_heap
                    .tuple(&[hello, make_small(42)])
                    .expect("expected message");
                assert!(
                    beam_term::compare::eq(message, expected, &atoms),
                    "the decoded message differs from {hello:?},42"
                );
            }
            other => panic!("expected a DOP_REG_SEND, got {other:?}"),
        }
    }

    /// A routed `DistSignal::Exit` is materialised **synchronously**: dropping the
    /// signal's source heap right after `route` returns cannot affect what is later
    /// written to the wire.
    #[test]
    fn a_routed_exit_survives_its_source_heap() {
        let (client, mut driver, mut stream, mut atoms) = loopback("cook", 9);
        drive(&client, &mut driver, &mut stream, &mut atoms);

        let remote = remote_pid(&mut atoms, 5, 9);
        // The reason lives on a heap that is dropped the moment `route` returns.
        let mut reason_heap = TestHeap::new(64).expect("heap");
        let reason = {
            let boom = atom_term(atoms.intern("boom"));
            reason_heap
                .tuple(&[boom, make_small(17)])
                .expect("build the reason")
        };
        {
            let mut router = client.router();
            router
                .route(DistSignal::Exit {
                    from: ProcessId::new(2, 0),
                    to: remote,
                    reason,
                })
                .expect("queue the exit");
        }
        // The reason's source heap is gone; the queued copy must be intact.
        drop(reason_heap);

        client.pump(&mut atoms).expect("flush the queued exit");
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).expect("peer read");
        driver.feed(&buf[..n]);
        driver.step().expect("peer step");
        let mut heap = TestHeap::new(1 << 12).expect("heap");
        let mut sink = UnsupportedSink;
        let dispatched = driver
            .dispatch_inbound(&mut heap, &mut atoms, &mut sink)
            .expect("classify");
        match dispatched.first().map(|r| r.delivery.clone()) {
            Some(Delivery::Exit { reason, .. }) => {
                let mut expected_heap = TestHeap::new(64).expect("heap");
                let boom = atom_term(atoms.intern("boom"));
                let expected = expected_heap
                    .tuple(&[boom, make_small(17)])
                    .expect("expected reason");
                assert!(
                    beam_term::compare::eq(reason, expected, &atoms),
                    "the decoded exit reason differs from {boom:?},17"
                );
            }
            other => panic!("expected a DOP_EXIT, got {other:?}"),
        }
    }

    /// A monitor signal addresses the peer's node and carries the **watcher first**
    /// (the C's `TUPLE4(DOP_MONITOR_P, watcher, watched, ref)`), cross-checked against
    /// the `live_interop` evidence.
    #[test]
    fn a_routed_monitor_uses_the_otp_argument_order() {
        let (client, mut driver, mut stream, mut atoms) = loopback("cook", 4);
        drive(&client, &mut driver, &mut stream, &mut atoms);

        let remote = remote_pid(&mut atoms, 7, 4);
        let watcher = ProcessId::new(3, 0);
        let reference = RefAllocator::new().next();
        let mut router = client.router();
        router
            .route(DistSignal::Monitor {
                watcher,
                target: remote,
                reference,
                kind: MonitorKind::Process,
            })
            .expect("queue the monitor");
        client.pump(&mut atoms).expect("flush the queued monitor");

        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).expect("peer read");
        driver.feed(&buf[..n]);
        driver.step().expect("peer step");
        let mut heap = TestHeap::new(1 << 12).expect("heap");
        let mut sink = UnsupportedSink;
        let dispatched = driver
            .dispatch_inbound(&mut heap, &mut atoms, &mut sink)
            .expect("classify");
        match dispatched.first().map(|r| r.delivery.clone()) {
            Some(Delivery::Monitor { watcher: w, watched, .. }) => {
                // The watcher is our local pid (element 0) as an *external* pid for the
                // client node, and the watched process is the remote pid (element 1).
                let expected = make_external_pid(
                    &mut heap,
                    atom_term(atoms.intern("client@localhost")),
                    watcher.number() as u32,
                    watcher.serial() as u32,
                    4,
                )
                .expect("expected watcher pid");
                assert!(
                    beam_term::compare::eq(w, expected, &atoms),
                    "the decoded watcher is not the local pid as an external pid"
                );
                assert_eq!(
                    RemotePid::from_eterm(watched)
                        .expect("the watched process is an external pid")
                        .number(),
                    remote.number()
                );
            }
            other => panic!("expected a DOP_MONITOR_P, got {other:?}"),
        }
    }

    /// A signal for a node with no link is a typed error — never a silent drop.
    #[test]
    fn a_signal_without_a_link_is_a_typed_error() {
        let client = DistClient::new(b"cook".to_vec()).expect("client");
        let mut atoms = Table::new();
        let our = atom_term(atoms.intern("client@localhost"));
        client.set_identity("client@localhost", our, 1, b"cook");
        let remote = {
            let mut heap = TestHeap::new(16).expect("heap");
            let node = atom_term(atoms.intern("absent@localhost"));
            RemotePid::from_eterm(
                make_external_pid(&mut heap, node, 1, 0, 1).expect("external pid"),
            )
            .expect("a remote pid")
        };
        let mut router = client.router();
        let err = router
            .route(DistSignal::Link {
                from: ProcessId::new(1, 0),
                to: remote,
            })
            .expect_err("no link, so the route must fail");
        assert!(
            err.to_string().contains("no established connection"),
            "unexpected error: {err}"
        );
    }
    /// The `DOP_UNLINK_ID` id is encoded **checked**: the largest encodable id is exact and
    /// one past it is the typed resource limit, never a wrapped immediate.
    #[test]
    fn an_unlink_id_is_encoded_checked() {
        let max = beam_term::MAX_SMALL as u64;
        assert_eq!(
            unlink_id_term(max).expect("the largest id still fits"),
            make_small(beam_term::MAX_SMALL),
            "the id at the bound is encoded verbatim"
        );
        assert!(
            matches!(unlink_id_term(max + 1), Err(BeamError::SystemLimit)),
            "one past the bound is refused"
        );
        // The whole upper half of the `u64` space is unrepresentable too (an `i64` overflow,
        // not just a small-integer overflow).
        assert!(
            matches!(unlink_id_term(u64::MAX), Err(BeamError::SystemLimit)),
            "u64::MAX cannot fit an i64"
        );
    }
    /// A routed unlink whose counter has left the small-integer range fails **typed** — the
    /// signal is refused instead of carrying a wrapped id a peer would ack against the wrong
    /// unlink. The counter is parked at the bound directly (reaching it by routing
    /// `2^59` signals is not a test).
    #[test]
    fn a_routed_unlink_past_the_small_range_is_a_typed_system_limit() {
        let (client, mut driver, mut stream, mut atoms) = loopback("cook", 5);
        drive(&client, &mut driver, &mut stream, &mut atoms);
        let remote = remote_pid(&mut atoms, 1, 5);
        let mut inner = client.inner.borrow_mut();
        // Just below the bound: the id is encoded and the signal is queued.
        inner.next_unlink_id = beam_term::MAX_SMALL as u64;
        inner
            .encode_signal(DistSignal::Unlink {
                from: ProcessId::new(3, 0),
                to: remote,
            })
            .expect("an id at the bound encodes");
        // One past it: the encode is a typed refusal, and nothing wrapped is produced.
        inner.next_unlink_id = beam_term::MAX_SMALL as u64 + 1;
        assert!(
            matches!(
                inner.encode_signal(DistSignal::Unlink {
                    from: ProcessId::new(3, 0),
                    to: remote,
                }),
                Err(BeamError::SystemLimit)
            ),
            "an id past the bound is a typed refusal"
        );
    }
}
