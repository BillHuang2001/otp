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

//! The distribution **node-set, connector and Erlang-supervisor** floor:
//! `erlang:nodes/0,1,2`, `net_adm:ping/1`, `net_kernel:connect_node/1` and
//! `erl_distribution:start_link/0,1` (contract **A.5**).
//!
//! The node's *identity* (`is_alive/0`, `get_cookie/0`, `set_cookie/2`) lives in
//! [`super::node`]; this module owns the calls that read or drive the runtime's own
//! **link table** ([`Runtime::dist_connected_nodes`] / [`Runtime::dist_connect`]) -- state
//! no `beam_bifs` module can reach, exactly like the process/registry queries in
//! [`super::process`]. They are therefore served from the runtime-native floor, consulted
//! **before** the `beam_bifs` registry.
//!
//! `erl_distribution:start_link/0,1` is the odd one out: it is the Erlang-level distribution
//! **supervisor** `kernel_sup` starts as its `net_sup` child, and the floor answers it
//! `ignore` because this runtime's distribution is the *native* [`crate::dist::DistNode`],
//! not the `inet_tcp`/`prim_inet` stack that supervisor would run. Both keys are enumerated
//! in the parent's `floor_keys` list, so the rest of `erl_distribution` stays an ordinary
//! `.beam` body.
//!
//! # What the answers are on a non-distributed node (`nonode@nohost`)
//!
//! Milestone 1 runs a `-noshell` VM unless `-sname`/`-name` configured a node, and the
//! differential corpus pins that case end to end (`tools/differential`,
//! `corpus::wire_ids::node_identity_cases`):
//!
//! | Call | Answer |
//! |---|---|
//! | `nodes()`, `nodes(connected)`, `nodes(visible)`, `nodes(hidden)` | `[]` |
//! | `nodes(this)`, `nodes(known)` | `[nonode@nohost]` |
//! | `nodes(visible, #{connection_id => true})` | `[]` (no peers to describe) |
//! | `nodes(this, #{})` | `[{nonode@nohost, #{}}]` |
//! | `net_adm:ping(Node)` | the atom `pang`, for every `Node` |
//! | `net_kernel:connect_node(Node)` | the atom `ignored`, for every `Node` |
//!
//! `net_kernel:connect_node/1`'s `ignored` is the oracle's own answer for a node that is
//! not alive (`net_kernel.erl`'s `connect_node/1` clause tests `is_alive()`, measured on
//! the system `erl` both for `nonode@nohost` and for an arbitrary atom) -- *not* the
//! `false` a failed connection reports.
//!
//! # `nodes/1`'s argument grammar (`dist.c:nodes`)
//!
//! The C folds an atom argument into a one-element list and then folds the list's elements
//! into five flags: `visible`/`hidden`/`connected` select peers, `this`/`known` select the
//! local node. An element that is neither is `badarg`, and so is a non-list, non-atom
//! argument or an improper tail. The result is ordered **peers first, the local node
//! last**, de-duplicated in first-seen order (measured on the oracle: after connecting one
//! peer, `nodes([this,connected])` is `[peer, local]`).
//!
//! # `nodes/2`'s information map (`dist.c:6273-6276,6323-6351,6436-6484`)
//!
//! `nodes/2` is the same node set -- the **same** [`node_types`](RuntimeContext::nodes_1)
//! grammar -- answered as a list of `{Node, Info}` 2-tuples instead of bare node names.
//! The second argument selects what each `Info` says about its node and must be a **map**:
//!
//! * the only recognised keys are the atoms `connection_id` and `node_type`, each valued the
//!   atom `true` or `false` (the C's `switch (*p) { case am_true: … case am_false: … }`);
//! * the number of *recognised* keys must equal the map size, so **any**
//!   unrecognised key is `badarg` (`dist.c:6347-6348`);
//! * a non-map (`nodes(visible, [])` included) and a recognised key with any other value are
//!   `badarg` too.
//!
//! `Info` is a **flat** map holding `connection_id` (only when that option is `true`) then
//! `node_type` (only when that option is `true`), in that key order. `node_type`'s value is
//! the entry's own type atom -- `this` for the local node, `visible` for a peer.
//! `connection_id`'s value is the link's **local identity**
//! ([`Runtime::dist_connection_id`]) -- stable for that link's lifetime, a different integer
//! for a different peer -- or the atom `undefined` when the C's `dep->connection_id == ~0`,
//! which is the case for a `this` entry and for a peer whose link retired between the node
//! set and the lookup. An **empty** selected set answers `[]` (the C returns `NIL`), but the
//! options are validated **first**, so an invalid `Options` is still `badarg`.
//!
//! Measured on the oracle (OTP 29.1.1 / erts-17.1), all on a dormant node:
//! `nodes(this, #{})` is `[{nonode@nohost,#{}}]`, `nodes(this, #{connection_id => true})` is
//! `[{nonode@nohost,#{connection_id => undefined}}]`, `nodes(this, #{node_type => true})` is
//! `[{nonode@nohost,#{node_type => this}}]` and `nodes([this],
//! #{connection_id => true, node_type => true})` is
//! `[{nonode@nohost,#{connection_id => undefined,node_type => this}}]`.
//!
//! # The connector: which name is dialled, and through what
//!
//! `net_adm:ping/1` is the `net_adm.erl` wrapper over `net_kernel:connect_node/1`
//! (`pong` for `true`, `pang` for everything else) and `net_kernel:connect_node/1` is the
//! outbound half of contract A.5 -- it dials the peer through `epmd` and drives the client
//! side of the `'N'` handshake ([`Runtime::dist_connect`], `crate::dist_client`).
//!
//! Two details of the real connector are reproduced here rather than in `dist_client`:
//!
//! * **A full `name@host` is dialled through its short `name`.** This OTP release
//!   registers a node with `epmd` under its short name *whether it was started `-sname
//!   foo` or `-name foo@host`* (measured with `epmd -names`), and the connector resolves
//!   the name it is handed through `epmd` verbatim ([`crate::dist::NodeName::parse`] treats
//!   a `name@host` value as a long name). A full `name@host` therefore looked up nothing
//!   (`noport`), while the same node is reachable by its short name.
//! * **A dial failure is the BIF's own answer, not a raise.** `net_adm:ping/1` answers
//!   `pang` and `net_kernel:connect_node/1` answers `false` for *every* unreachable peer
//!   (`nosuchnode@nowhere`, a name `epmd` does not know, a peer that refuses the
//!   handshake) -- the typed error [`Runtime::dist_connect`] returns **is** that answer,
//!   never a swallowed failure (its diagnostic stays observable through
//!   [`crate::DistClient::last_error`]).
//!
//! # The gaps (typed, never a fabricated value)
//!
//! * A **non-atom** argument is [`BeamError::BadArg`] here, where the oracle raises
//!   `function_clause` (the `net_adm.erl`/`net_kernel.erl` clause guard). This runtime has
//!   no way to raise `function_clause` from a native BIF, so the honest typed rejection
//!   stands in for it.
//! * `nodes(known)` reports this runtime's own `this`; the oracle's connected peers and its
//!   extra `not_connected` entries (nodes another node knows) are not modelled.
//! * `hidden` links are not modelled, so `nodes(hidden)` is always `[]`.
//!
//! # File layout
//!
//! `mod.rs` keeps the module docs and the one helper four siblings share; the surface itself is
//! split by responsibility:
//!
//! * `nodes.rs` — the `erlang:nodes/0,1,2` node set, its argument grammar (`dist.c:nodes`) and
//!   `nodes/2`'s information map (with the `NodeInfoOptions`/`NodeEntryType` shapes it folds).
//! * `connector.rs` — the outbound half: `net_adm:ping/1`, `net_kernel:connect_node/1`,
//!   `erl_distribution:start_link/0,1` and the two peer-name helpers they share.
//! * `monitor.rs` — `erlang:monitor_node/2` (whose store and notifications live in
//!   [`crate::Runtime`]).
//! * `tests.rs` — the colocated suite.
//!
//! Helpers a sibling file calls carry `pub(super)`: inside a directory module that is exactly
//! the scope a private item of the former single-file `net` module had.

use crate::context::RuntimeContext;
use crate::dist::NONODE_NAME;

mod connector;
mod monitor;
mod nodes;
#[cfg(test)]
mod tests;

impl RuntimeContext<'_> {
    /// This runtime's own node name (`node()`), the name a peer argument is compared
    /// against and the string `erlang.erl`'s `set_cookie/2` guard compares its first
    /// argument against.
    ///
    /// It is the configured `node@host` when `-name`/`-sname` made this a node, and the
    /// fixed [`NONODE_NAME`] otherwise -- the very string [`crate::Runtime::local_node`] was
    /// interned from (see `Runtime::new`), so the two can never disagree.
    ///
    /// `pub(super)` because `super::node` (the identity surface) and the connector both read it:
    /// the scope the former single-file module gave it.
    pub(super) fn local_node_name(&self) -> String {
        match self.runtime.node_name() {
            Some(name) => name.to_string(),
            None => NONODE_NAME.to_string(),
        }
    }
}
