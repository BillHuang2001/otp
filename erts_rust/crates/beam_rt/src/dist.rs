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
//!   [`LocalDeliverySink`] (the runtime side of `beam_dist`'s [`beam_dist::InboundSink`] seam), as are
//!   the inbound **signal** families `DOP_LINK`, `DOP_EXIT*`, `DOP_UNLINK_ID[_ACK]`,
//!   `DOP_MONITOR_P_EXIT*` and `DOP_GROUP_LEADER` — each through `beam_process`'s route-free
//!   *receipt* primitives (see [`LocalDeliverySink`]'s type docs for the honest routed /
//!   partially-routed / unrouted split). What genuinely has no local seam —
//!   `DOP_MONITOR_P`/`DOP_DEMONITOR_P` and the distributed-spawn family — is reported as the
//!   honest [`beam_dist::DeliveryOutcome::Unsupported`], with the missing `beam_process` API
//!   named at its match arm; never a fabricated or mis-addressed delivery. A message the
//!   runtime cannot decode, validate or classify **retires that one link** (recorded in
//!   [`DistNode::last_link_error`]); it never fails a scheduler pass.
//! * **Every inbound delivery is validated against its own link.** Each link drains
//!   through [`beam_dist::ConnectionDriver::dispatch_inbound_checked`], so a delivery is
//!   checked against **that** link's peer/own identity (the C's per-opcode `invalid_message`
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
//!
//! # File layout (the module root is a façade)
//!
//! The responsibility split lives in this directory: `name.rs` owns [`NodeName`] and
//! the host-name derivation, `connection.rs` owns one accepted link
//! ([`DistConnection`]) and its protocol driver, `node.rs` owns the listener, the
//! `epmd` registration and the accept/pump/inbound-drain loop ([`DistNode`],
//! [`epmd_addr`]), `links.rs` is the one node→link routing table, `inbound.rs` is the
//! inbound delivery sink and `externalize.rs` the outbound local-id rewrites. This
//! file re-exports the whole public surface, so `crate::dist::X` keeps resolving
//! exactly as before.

use std::time::Duration;

mod connection;
pub(crate) mod externalize;
mod inbound;
pub(crate) mod links;
mod name;
mod node;

pub use connection::DistConnection;
pub use inbound::LocalDeliverySink;
pub(crate) use links::DistLinks;
pub use links::DistConnectionSnapshot;
pub use name::NodeName;
pub use node::{epmd_addr, DistNode};
pub(crate) use node::advertised_flags;

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
mod tests;
