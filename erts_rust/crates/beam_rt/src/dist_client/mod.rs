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
//! (`beam_dist::epmd::lookup_port_at`, aimed at [`crate::dist::epmd_addr`]) and the
//! inbound classifier (`ConnectionDriver::dispatch_inbound_checked`). What this module
//! adds is the *driving state* the runtime owns: a socket per peer, a bounded
//! non-blocking pump, a failure-recorded retirement policy and the [`DistSignalRouter`]
//! implementation the process table's outbound signals are routed through.
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
//! DistSignal::MonitorExit -> {DOP_MONITOR_P_EXIT, Watched(local), Watcher(remote), Ref, Reason}
//!                                                                             dist.c:1516
//! ```
//!
//! `MonitorExit` is the **one** variant whose tuple is *watched*-first — the reverse of the
//! monitor family's `{watcher, watched, ref}` — because it is the mirror of an inbound
//! `DOP_MONITOR_P` (`erts_dsig_send_m_exit`, `dist.c:1499-1522`); it is also the one variant
//! routed to the node of the element that is **not** its subject (`signals.rs`'s `signal_node`).
//! Its `Ref` is written in the spelling the watcher table recorded
//! ([`beam_process::RemoteWatcherRef`]): a peer-minted external reference goes back verbatim
//! (`{its node, its creation, its ids}`), a locally minted one through the monitor family's own
//! `{this node, this creation, ids}` builder.
//!
//! **The plain 5-tuple form is emitted unconditionally** — the `DFLAG_EXIT_PAYLOAD` preference
//! for `DOP_PAYLOAD_MONITOR_P_EXIT` with the reason as the message body (`dist.c:1511-1519`) is
//! *not* reproduced here. Two reasons: [`crate::dist::advertised_flags`] describes what this
//! runtime **understands inbound** (it does handle `DOP_PAYLOAD_MONITOR_P_EXIT`, see
//! `crate::dist::inbound`'s `remote_monitor_down`), not what it emits; and the plain form is
//! legal on **every** peer that supports `DFLAG_DIST_MONITOR` — a mandatory-set flag
//! (`dist.c:1504-1508` only skips the send when even that is missing) — so no per-link gate is
//! needed and the 4-tuple form buys nothing measurable on this path.
//!
//! The unlink id is minted here (a monotonic non-zero small integer, `client.rs`'s
//! `next_unlink_id`): a real receiver deletes the link by `from` and uses the id only for its
//! acknowledgement handshake (`erl_proc_sig_queue.c:6695-6726`), so any non-zero id is accepted
//! and effective.
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
//!
//! # File layout
//!
//! `mod.rs` keeps the module docs and the frozen façade; the implementation is split by
//! responsibility, so no file grows past the workspace's size convention:
//!
//! * `client.rs` — [`DistClient`] (the public registry and its API) together with the shared
//!   state every half of it borrows ([`DistClientInner`]: the links cell, the outbound queue and
//!   the three scratch heaps), the queue's drain/flush and the per-link inbound dispatch. The two
//!   types stay in one file because every one of the registry's methods is a borrow of that
//!   state, so drawing a file boundary between them would only widen its private fields.
//! * `handshake.rs` — [`ClientLink`]: the connecting side's handshake/established state machine
//!   over its own socket, and the non-blocking read/step/write round `pump` drives.
//! * `router.rs` — [`DistRouter`], the [`DistSignalRouter`] implementation the runtime installs
//!   on its process table.
//! * `signals.rs` — the `DOP_*` argument materialisation of a routed signal (the external
//!   pid/reference builders and the checked `DOP_UNLINK_ID` id).
//! * `lookup.rs` — the `epmd` `PORT2_REQ` port lookup and the `node@host` name helpers.
//! * `tests.rs` — the colocated suite.
//!
//! Helpers a sibling file calls carry `pub(super)`: inside a directory module that is exactly
//! the scope a private item of the former single-file `dist_client` module had.

use std::time::Duration;

mod client;
mod handshake;
mod lookup;
mod router;
mod signals;
#[cfg(test)]
mod tests;

pub use client::DistClient;
pub(crate) use client::DistClientInner;
pub(crate) use handshake::ClientLink;
pub(crate) use router::DistRouter;

/// How long [`crate::Runtime::dist_connect`] drives the handshake before giving up.
///
/// The connector handshake is a handful of round trips on an already-open socket, so
/// this bounds a stuck peer without a scheduler pass ever waiting on it (the pass
/// uses [`DistClient::pump`], which never blocks beyond one read slice).
pub const DIST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
