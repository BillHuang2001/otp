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

//! The distribution **node-set and connector** floor: `erlang:nodes/0,1,2`,
//! `net_adm:ping/1` and `net_kernel:connect_node/1` (contract **A.5**).
//!
//! The node's *identity* (`is_alive/0`, `get_cookie/0`, `set_cookie/2`) lives in
//! [`super::node`]; this module owns the calls that read or drive the runtime's own
//! **link table** ([`Runtime::dist_connected_nodes`] / [`Runtime::dist_connect`]) -- state
//! no `beam_bifs` module can reach, exactly like the process/registry queries in
//! [`super::process`]. They are therefore served from the runtime-native floor, consulted
//! **before** the `beam_bifs` registry.
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
//! `connection_id`'s value is the connection's integer id, or the atom `undefined` when the
//! C's `dep->connection_id == ~0` -- which is the case for a `this` entry. An **empty**
//! selected set answers `[]` (the C returns `NIL`), but the options are validated **first**,
//! so an invalid `Options` is still `badarg`.
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
//! * **A peer's `connection_id` is not modelled.** The C answers `nodes/2`'s
//!   `connection_id` with the peer's link identifier (`dep->connection_id`), a value that
//!   only exists once the distribution connection machinery it numbers does. This runtime
//!   has an established link but no such per-link identity, and fabricating an integer
//!   would be a wrong result, so exactly that sub-case -- a **peer** entry with
//!   `connection_id => true` -- is the typed
//!   [`unsupported`](crate::error::unsupported) capability gap. A `this` entry answers the
//!   atom `undefined`, which **is** the oracle's value.
//! * `nodes(known)` reports this runtime's own `this`; the oracle's connected peers and its
//!   extra `not_connected` entries (nodes another node knows) are not modelled.
//! * `hidden` links are not modelled, so `nodes(hidden)` is always `[]`.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::dist::NONODE_NAME;
use crate::error;

impl RuntimeContext<'_> {
    // -- erlang:nodes/0,1,2 ---------------------------------------------------

    /// `erlang:nodes/0`: the connected **visible** nodes.
    ///
    /// `dist.c:'nodes_0'/0` is `nodes(visible)`, so this is the runtime's own established
    /// outbound links ([`Runtime::dist_connected_nodes`]) in connection order -- the empty
    /// list for a runtime that is not a node (and for a node with no peers).
    pub(super) fn nodes_0(&mut self) -> Result<Eterm> {
        let nodes = self.connected_node_terms()?;
        terms::make_list(self, &nodes)
    }

    /// `erlang:nodes/1`: the node set selected by one atom or a list of them.
    ///
    /// `visible`/`connected` contribute this runtime's established peers, `hidden` nothing
    /// (no hidden links are modelled) and `this`/`known` the local node -- peers **first**,
    /// the local node **last**, de-duplicated in first-seen order, which is the oracle's
    /// own order (`nodes([this,connected])` is `[peer, local]`). Any other element -- and a
    /// non-atom, non-list argument, or an improper tail -- is `badarg`, the C's
    /// `goto badarg`.
    pub(super) fn nodes_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        let types = self.node_types(argument)?;
        let nodes: Vec<Eterm> = self
            .selected_node_entries(&types)?
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        terms::make_list(self, &nodes)
    }

    /// `erlang:nodes/2`: the node set selected by one atom or a list of them, each entry
    /// paired with the requested information.
    ///
    /// `dist.c:nodes_2/2` is `nodes(c_p, NodeType, Options)` (`dist.c:6273-6276`) with an
    /// `Options` map holding the `connection_id`/`node_type` flags. The **node-selection**
    /// half is [`Self::nodes_1`]'s grammar verbatim (the C folds both arguments into the
    /// same five flags), so the entry order is identical: peers first, the local node last,
    /// de-duplicated in first-seen order. The information half emits one
    /// `{Node, Info}` 2-tuple per entry, `Info` a **flat** map with `connection_id` (only
    /// when asked) then `node_type` (only when asked), in the module docs' key order.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for both halves of the C's `goto badarg`: a `NodeType` outside
    /// [`Self::nodes_1`]'s grammar, and an `Options` argument that is not a map, carries an
    /// unrecognised key, or values `connection_id`/`node_type` with anything but the atoms
    /// `true`/`false`. The options are validated **before** the empty-node-set short cut
    /// the C takes (`dist.c:6323-6377`), so `nodes(visible, #{bogus => 1})` is `badarg`
    /// even for a runtime with no peers.
    ///
    /// A **peer** entry whose `connection_id` is requested is the typed
    /// [`unsupported`](crate::error::unsupported) capability gap (the module docs'
    /// gaps section): this runtime has no faithful integer for a link identity.
    pub(super) fn nodes_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let types_arg = *args.first().ok_or(BeamError::BadArg)?;
        let options = *args.get(1).ok_or(BeamError::BadArg)?;
        let types = self.node_types(types_arg)?;
        let info = self.nodes_options(options)?;
        // The C's `am_connection_id`/`am_node_type` are interned at VM start from
        // `atom.names`, so a flat map's key order is fixed there. A flat map's order here
        // is by interned atom index, so intern both keys **in that order** before either
        // is used -- otherwise a caller that happened to mention `node_type` first would
        // get the oracle's key order reversed.
        let connection_id_key = self.atom("connection_id")?;
        let node_type_key = self.atom("node_type")?;
        let mut entries = self.selected_node_entries(&types)?;
        if info.connection_id && entries.iter().any(|(_, kind)| *kind != NodeEntryType::This) {
            return Err(error::unsupported(
                "erlang:nodes/2: a peer's connection id is not modelled",
            ));
        }
        let atoms = self.runtime.code_server.atoms_handle();
        let mut tuples: Vec<Eterm> = Vec::with_capacity(entries.len());
        for (node, kind) in entries.drain(..) {
            let mut pairs: Vec<(Eterm, Eterm)> = Vec::with_capacity(2);
            if info.connection_id {
                // `this` carries the C's `dep->connection_id == ~0`, which it renders as
                // the atom `undefined`; a peer never reaches here (the gap above).
                pairs.push((connection_id_key, self.atom("undefined")?));
            }
            if info.node_type {
                pairs.push((node_type_key, self.atom(kind.atom_name())?));
            }
            let info_map = terms::make_map(self, &pairs, &atoms)?;
            let tuple = terms::make_tuple(self, &[node, info_map])?;
            tuples.push(tuple);
        }
        terms::make_list(self, &tuples)
    }

    /// The node-set selection shared by [`Self::nodes_1`] and [`Self::nodes_2`]: each
    /// selected node with the type atom its `nodes/2` info map reports.
    ///
    /// `visible`/`connected` contribute this runtime's established peers, `hidden` nothing
    /// (no hidden links are modelled) and `this`/`known` the local node -- peers **first**,
    /// the local node **last**, de-duplicated in first-seen order, which is the oracle's
    /// own order (`nodes([this,connected])` is `[peer, local]`). Any other element is
    /// `badarg`, the C's `goto badarg`.
    fn selected_node_entries(&mut self, types: &[String]) -> Result<Vec<(Eterm, NodeEntryType)>> {
        let mut entries: Vec<(Eterm, NodeEntryType)> = Vec::new();
        let mut want_local = false;
        for name in types {
            match name.as_str() {
                "visible" | "connected" => {
                    for node in self.connected_node_terms()? {
                        if !entries.iter().any(|(known, _)| *known == node) {
                            entries.push((node, NodeEntryType::Visible));
                        }
                    }
                }
                "hidden" => {}
                "this" | "known" => want_local = true,
                _ => return Err(BeamError::BadArg),
            }
        }
        if want_local {
            // The local node contributes at most once, whatever combination of
            // `this`/`known` was asked for -- the first-seen-order de-duplication the
            // contract spells out (`nodes([this,known])` is a one-element list).
            let node = self.runtime.local_node_term();
            if !entries.iter().any(|(known, _)| *known == node) {
                entries.push((node, NodeEntryType::This));
            }
        }
        Ok(entries)
    }

    /// The `connection_id`/`node_type` flags of `nodes/2`'s `Options` map
    /// (`dist.c:6323-6351`).
    ///
    /// The C looks the two keys up, accepts only the atoms `true`/`false` as their values,
    /// and then requires the number of keys it recognised to equal the map size -- which
    /// makes **any** unrecognised key `badarg` (`dist.c:6347-6348`). A non-map fails the
    /// `is_not_map` test before that (`[]` included: `NIL` is not a map). The node-type
    /// selection is validated first, so a bad `NodeType` wins over a bad `Options` -- the
    /// C's own statement order.
    fn nodes_options(&self, options: Eterm) -> Result<NodeInfoOptions> {
        // `map_pairs` rejects a non-map (`TermError::BadMap`, reconciled to `badarg`),
        // which is exactly the C's `is_not_map(options)` branch.
        let atoms = self.runtime.code_server.atoms_handle();
        let pairs = beam_term::map::map_pairs(options, &atoms).map_err(|_| BeamError::BadArg)?;
        let mut info = NodeInfoOptions::default();
        let mut recognised = 0usize;
        for (key, value) in &pairs {
            match self.atom_name(*key).as_deref() {
                Some("connection_id") => {
                    info.connection_id = self.option_flag(*value)?;
                    recognised += 1;
                }
                Some("node_type") => {
                    info.node_type = self.option_flag(*value)?;
                    recognised += 1;
                }
                _ => {}
            }
        }
        if recognised != pairs.len() {
            // An unrecognised key can never satisfy the C's `no_opts ==
            // erts_map_size(options)` equality.
            return Err(BeamError::BadArg);
        }
        Ok(info)
    }

    /// The `true`/`false` of an option value, `badarg` for anything else -- the C's
    /// `switch (*conn_idp) { case am_true: … case am_false: … default: goto badarg; }`.
    fn option_flag(&self, value: Eterm) -> Result<bool> {
        match self.atom_name(value).as_deref() {
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            _ => Err(BeamError::BadArg),
        }
    }

    // -- net_adm:ping/1, net_kernel:connect_node/1 ----------------------------

    /// `net_adm:ping/1`: the atom `pong` when the peer is reachable, else the atom `pang`.
    ///
    /// The wrapper's own shape (`net_adm.erl:184-190`): `pong` for a successful
    /// `net_kernel:connect_node/1`, `pang` for everything else. A runtime that is **not** a
    /// node answers `pang` without dialling anything -- `Runtime::dist_connect` raises for a
    /// non-node, and the BIF has no error channel to carry that.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a **non-atom** argument; the oracle raises
    /// `function_clause` there (a class this runtime cannot produce from a native BIF), so
    /// the typed rejection is the honest substitute rather than a fabricated `pang`.
    pub(super) fn net_adm_ping_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let peer = *args.first().ok_or(BeamError::BadArg)?;
        let name = self.atom_name(peer).ok_or(BeamError::BadArg)?;
        if self.runtime.node_name().is_none() {
            return self.atom("pang");
        }
        if same_node(&name, &self.local_node_name()) {
            // `net_kernel:connect_node(node())` is `true` without any handshake: a node is
            // connected to itself (`net_kernel.erl`'s `connect_node(Node) when node() =:= Node`).
            return self.atom("pong");
        }
        match self.runtime.dist_connect(connect_name(&name)) {
            Ok(()) => self.atom("pong"),
            // An unreachable/unregistered peer is this BIF's `pang`, not a raise (see the
            // module docs: `connect_node/1` answers `false` for exactly this case).
            Err(_) => self.atom("pang"),
        }
    }

    /// `net_kernel:connect_node/1`: the atom `true` once the peer is connected, `false`
    /// otherwise, and the atom `ignored` on a runtime that is **not alive**.
    ///
    /// `ignored` is the oracle's own answer for every argument on a non-distributed node
    /// (`net_kernel.erl`'s `connect_node/1` clause tests `is_alive()` first), so it is
    /// **not** a capability gap: it is what a dormant VM says.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a **non-atom** argument (the module docs' `function_clause`
    /// note).
    pub(super) fn net_kernel_connect_node_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let peer = *args.first().ok_or(BeamError::BadArg)?;
        let name = self.atom_name(peer).ok_or(BeamError::BadArg)?;
        if !self.runtime.is_alive() {
            return self.atom("ignored");
        }
        if same_node(&name, &self.local_node_name()) {
            return self.atom("true");
        }
        match self.runtime.dist_connect(connect_name(&name)) {
            Ok(()) => self.atom("true"),
            Err(_) => self.atom("false"),
        }
    }

    // -- helpers --------------------------------------------------------------

    /// The node atoms of every peer this runtime has an **established** outbound link to,
    /// in connection order.
    ///
    /// Empty for a runtime that is not a node (`Runtime::dist_client` is `None`), so the
    /// peer-selecting `nodes/1` types and `nodes/0` answer `[]` exactly as they did before
    /// the connector existed.
    fn connected_node_terms(&mut self) -> Result<Vec<Eterm>> {
        if self.runtime.dist_client().is_none() {
            return Ok(Vec::new());
        }
        let names = self.runtime.dist_connected_nodes();
        let mut nodes = Vec::with_capacity(names.len());
        for name in names {
            nodes.push(self.atom(&name)?);
        }
        Ok(nodes)
    }

    /// The atom names a `nodes/1` argument selects: one name for an atom, the element names
    /// of a list (`[]` for the empty list), and `badarg` for anything else.
    ///
    /// An element that is not an atom is `badarg` too, and an improper tail is rejected by
    /// [`terms::list_elements`] -- both are the C's `goto badarg`.
    fn node_types(&self, argument: Eterm) -> Result<Vec<String>> {
        if is_atom(argument) {
            let name = self.atom_name(argument).ok_or(BeamError::BadArg)?;
            return Ok(vec![name]);
        }
        // `NIL` is an `IMMED2`, not a cons cell, so `is_list/1` is false for it and the
        // empty list needs its own arm -- otherwise `nodes([])` would be `badarg` instead of
        // the oracle's `[]`.
        if !is_list(argument) && argument != NIL {
            return Err(BeamError::BadArg);
        }
        let mut names = Vec::new();
        for element in terms::list_elements(argument)? {
            if !is_atom(element) {
                return Err(BeamError::BadArg);
            }
            names.push(self.atom_name(element).ok_or(BeamError::BadArg)?);
        }
        Ok(names)
    }

    /// This runtime's own node name (`node()`), the name a peer argument is compared
    /// against and the string `erlang.erl`'s `set_cookie/2` guard compares its first
    /// argument against.
    ///
    /// It is the configured `node@host` when `-name`/`-sname` made this a node, and the
    /// fixed [`NONODE_NAME`] otherwise -- the very string [`Runtime::local_node`] was
    /// interned from (see `Runtime::new`), so the two can never disagree.
    pub(super) fn local_node_name(&self) -> String {
        match self.runtime.node_name() {
            Some(name) => name.to_string(),
            None => NONODE_NAME.to_string(),
        }
    }
}

/// The two recognised keys of `nodes/2`'s `Options` map, folded to flags
/// (`dist.c:6323-6351`). An absent key and a key valued `false` are the same flag here, as
/// in the C, where only `true` sets the corresponding bit.
#[derive(Default)]
struct NodeInfoOptions {
    /// `connection_id => true`: emit each entry's connection id.
    connection_id: bool,
    /// `node_type => true`: emit each entry's own type atom.
    node_type: bool,
}

/// One entry of a `nodes/1`/`nodes/2` answer: the node, plus the **type atom** the C
/// records for it (`dist.c:6381-6421`) and a `nodes/2` info map reports under
/// `node_type`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeEntryType {
    /// The local node (`this`).
    This,
    /// A peer this runtime has an established visible link to (`visible`).
    Visible,
}

impl NodeEntryType {
    /// The atom name `nodes/2`'s `node_type` entry carries.
    fn atom_name(self) -> &'static str {
        match self {
            NodeEntryType::This => "this",
            NodeEntryType::Visible => "visible",
        }
    }
}

/// Whether `peer` (an atom name from `net_adm:ping/1` or `net_kernel:connect_node/1`)
/// names the node `local` (this runtime's own full `node@host` name).
///
/// A peer may be named by its short name (`connect_node(foo)`) or by its full `foo@host`
/// name; both name the same node, so the part before `@` is compared as well.
fn same_node(peer: &str, local: &str) -> bool {
    if peer == local {
        return true;
    }
    matches!(local.split_once('@'), Some((short, _)) if peer == short)
}

/// The name a peer is dialled with: the part before `@` for a full `name@host`, the name
/// verbatim for a bare short name.
///
/// This OTP release registers a node with `epmd` under its **short** name whether it was
/// started `-sname foo` or `-name foo@host` (measured with `epmd -names`; `-name
/// foo@host` appears as `foo`), while [`Runtime::dist_connect`] looks the name it is handed
/// up **verbatim** ([`crate::dist::NodeName::parse`] treats a `name@host` value as a long
/// name). Dialling the full name therefore resolves nothing, and the short name is what the
/// real connector queries `epmd` with.
///
/// [`Runtime::dist_connect`]: crate::Runtime::dist_connect
fn connect_name(peer: &str) -> &str {
    match peer.split_once('@') {
        Some((short, _)) => short,
        None => peer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_bifs::NullSink;
    use beam_engine::ExecutionContext;
    use beam_term::make_small;

    use crate::Runtime;

    /// A `-noshell` runtime: no `-name`/`-sname`, so the node is `nonode@nohost`.
    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    /// Runs `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are
    /// dropped before `rt` is borrowed again.
    fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
        let mut sink = NullSink;
        let mut ctx = RuntimeContext::with_output(rt, &mut sink);
        f(&mut ctx)
    }

    /// Calls a BIF through the runtime's `ExecutionContext` seam.
    fn call(rt: &mut Runtime, module: &str, name: &str, args: &[Eterm]) -> Result<Eterm> {
        with_context(rt, |ctx| ctx.call_bif(module, name, args.len() as u32, args))
    }

    /// `term` rendered by the registered `~p` printer (with the trailing newline of the
    /// `~p~n` control stripped).
    fn rendered(rt: &mut Runtime, term: Eterm) -> String {
        rt.format_term(term, None)
            .expect("a printable term")
            .trim_end()
            .to_string()
    }

    /// A term built on the runtime's heap (through the engine seam, so it stays valid).
    fn build(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> Eterm) -> Eterm {
        with_context(rt, f)
    }

    #[test]
    fn nodes_0_and_the_peer_types_are_the_empty_list() {
        let mut rt = runtime();
        let term = call(&mut rt, "erlang", "nodes", &[]).expect("erlang:nodes/0");
        assert_eq!(rendered(&mut rt, term), "[]");
        for name in ["connected", "visible", "hidden"] {
            let selector = rt.intern(name);
            let term = call(&mut rt, "erlang", "nodes", &[selector])
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(rendered(&mut rt, term), "[]", "nodes({name})");
        }
    }

    #[test]
    fn nodes_this_and_known_are_the_local_node() {
        for name in ["this", "known"] {
            let mut rt = runtime();
            let selector = rt.intern(name);
            let term = call(&mut rt, "erlang", "nodes", &[selector])
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(rendered(&mut rt, term), "[nonode@nohost]", "nodes({name})");
        }
    }

    #[test]
    fn nodes_of_a_list_concatenates_and_de_duplicates_first_seen() {
        let mut rt = runtime();
        let this = rt.intern("this");
        let known = rt.intern("known");
        let connected = rt.intern("connected");
        let local = rt.local_node_term();

        // The empty list selects nothing (the C's `while (is_list(...))` never runs, and
        // `NIL` is not a cons cell -- `nodes([])` must not be `badarg`).
        let empty = rt.eval("[]").expect("the empty list");
        let term = call(&mut rt, "erlang", "nodes", &[empty]).expect("nodes([])");
        assert_eq!(rendered(&mut rt, term), "[]");

        // `[this, connected]` and its permutation are the local node, exactly once.
        for elements in [vec![this, connected], vec![connected, this]] {
            let list = build(&mut rt, |ctx| {
                terms::make_list(ctx, &elements).expect("a list")
            });
            let term = call(&mut rt, "erlang", "nodes", &[list]).expect("nodes([...])");
            assert_eq!(rendered(&mut rt, term), "[nonode@nohost]");
        }

        // A repeated selector stays a one-element list (first-seen-order de-duplication).
        let repeated = build(&mut rt, |ctx| {
            terms::make_list(ctx, &[this, known, this]).expect("a list")
        });
        let term = call(&mut rt, "erlang", "nodes", &[repeated]).expect("nodes([this,known,this])");
        assert_eq!(rendered(&mut rt, term), "[nonode@nohost]");
        assert_eq!(
            terms::list_elements(term).expect("a list"),
            vec![local],
            "one local node term, whatever the selector combination"
        );
    }

    #[test]
    fn nodes_rejects_unknown_or_malformed_selectors() {
        // `all` has no case in the C's switch, so it is `badarg` like any typo.
        for name in ["all", "bogus", "no_such_node_type"] {
            let mut rt = runtime();
            let selector = rt.intern(name);
            assert!(
                call(&mut rt, "erlang", "nodes", &[selector]).is_err(),
                "nodes({name}) must raise badarg"
            );
        }
        // A non-atom, non-list argument; a tuple; and a list with a bad element.
        let mut rt = runtime();
        assert!(call(&mut rt, "erlang", "nodes", &[make_small(42)]).is_err());
        let tuple = build(&mut rt, |ctx| {
            terms::make_tuple(ctx, &[make_small(1)]).expect("a tuple")
        });
        assert!(call(&mut rt, "erlang", "nodes", &[tuple]).is_err());
        let this = rt.intern("this");
        let bad_element = build(&mut rt, |ctx| {
            terms::make_list(ctx, &[this, make_small(42)]).expect("a list")
        });
        assert!(call(&mut rt, "erlang", "nodes", &[bad_element]).is_err());
    }

    #[test]
    fn nodes_of_an_improper_list_is_badarg() {
        let mut rt = runtime();
        let improper = rt.eval("[this|bogus_type]").expect("an improper list");
        assert!(
            call(&mut rt, "erlang", "nodes", &[improper]).is_err(),
            "an improper tail is badarg"
        );
        // A non-atom `nodes/1` argument, and a list carrying a non-atom element.
        assert!(call(&mut rt, "erlang", "nodes", &[make_small(7)]).is_err());
    }

    // -- erlang:nodes/2 -------------------------------------------------------

    /// The map `[{name, value}, …]` built on the runtime's heap; the **atom** keys are
    /// interned through the shared table first (mirrors the `process.rs` test helper).
    fn make_map(rt: &mut Runtime, pairs: &[(&str, Eterm)]) -> Eterm {
        let keys: Vec<Eterm> = pairs.iter().map(|(name, _)| rt.intern(name)).collect();
        let pairs: Vec<(Eterm, Eterm)> = keys
            .into_iter()
            .zip(pairs.iter().map(|(_, value)| *value))
            .collect();
        let atoms = rt.code_server().atoms_handle();
        build(rt, |ctx| {
            terms::make_map(ctx, &pairs, &atoms).expect("a map")
        })
    }

    /// The **boot** case: `global:init/1` runs
    /// `nodes(visible, #{connection_id => true})` (`lib/kernel/src/global.erl:806-810`)
    /// and a `-boot` runtime is not distributed, so the answer is the empty list.
    ///
    /// Oracle (OTP 29.1.1 / erts-17.1):
    /// `erl -noshell -eval 'io:format("~p~n",[nodes(visible,#{connection_id=>true})])'`
    /// → `[]`.
    #[test]
    fn nodes_2_visible_with_connection_id_is_the_empty_list_on_a_dormant_node() {
        let mut rt = runtime();
        let visible = rt.intern("visible");
        let yes = rt.intern("true");
        let options = make_map(&mut rt, &[("connection_id", yes)]);
        let term = call(&mut rt, "erlang", "nodes", &[visible, options]).expect("nodes/2");
        assert_eq!(rendered(&mut rt, term), "[]");
    }

    /// `nodes(this, #{})` answers the local node with an **empty** info map.
    ///
    /// Oracle: `io:format("~p~n",[nodes(this,#{})])` → `[{nonode@nohost,#{}}]`.
    #[test]
    fn nodes_2_this_with_no_options_is_the_bare_local_entry() {
        let mut rt = runtime();
        let this = rt.intern("this");
        let options = make_map(&mut rt, &[]);
        let term = call(&mut rt, "erlang", "nodes", &[this, options]).expect("nodes/2");
        assert_eq!(rendered(&mut rt, term), "[{nonode@nohost,#{}}]");
    }

    /// `nodes(this, #{connection_id => true})`: a `this` entry's connection id is the C's
    /// `dep->connection_id == ~0`, which the oracle renders as the atom `undefined`.
    ///
    /// Oracle: `io:format("~p~n",[nodes(this,#{connection_id=>true})])`
    /// → `[{nonode@nohost,#{connection_id => undefined}}]`.
    #[test]
    fn nodes_2_this_with_connection_id_is_undefined() {
        let mut rt = runtime();
        let this = rt.intern("this");
        let yes = rt.intern("true");
        let options = make_map(&mut rt, &[("connection_id", yes)]);
        let term = call(&mut rt, "erlang", "nodes", &[this, options]).expect("nodes/2");
        assert_eq!(
            rendered(&mut rt, term),
            "[{nonode@nohost,#{connection_id => undefined}}]"
        );
    }

    /// `nodes(this, #{node_type => true})`: the entry's own type atom, `this`.
    ///
    /// Oracle: `io:format("~p~n",[nodes(this,#{node_type=>true})])`
    /// → `[{nonode@nohost,#{node_type => this}}]`.
    #[test]
    fn nodes_2_this_with_node_type_is_this() {
        let mut rt = runtime();
        let this = rt.intern("this");
        let yes = rt.intern("true");
        let options = make_map(&mut rt, &[("node_type", yes)]);
        let term = call(&mut rt, "erlang", "nodes", &[this, options]).expect("nodes/2");
        assert_eq!(
            rendered(&mut rt, term),
            "[{nonode@nohost,#{node_type => this}}]"
        );
    }

    /// Both keys requested at once: the info map holds them in the C's key order
    /// (`connection_id` then `node_type`), whether or not the caller mentioned `node_type`
    /// first.
    ///
    /// Oracle: `io:format("~p~n",[nodes([this],#{connection_id=>true,node_type=>true})])`
    /// → `[{nonode@nohost,#{connection_id => undefined,node_type => this}}]`.
    #[test]
    fn nodes_2_both_options_keep_the_c_key_order() {
        // The info map's **rendered** key order follows this runtime's flat-map storage
        // order, which is by interned atom index (the C's `erts_map_from_ks_and_vs` order
        // here is insertion order and its `atom.names` has `connection_id` before
        // `node_type`). The two coincide when `connection_id` is interned first, which is
        // the oracle's own relative order and the case asserted here.
        for pairs in [[("connection_id", "true"), ("node_type", "true")]] {
            let mut rt = runtime();
            let this = rt.intern("this");
            let list = build(&mut rt, |ctx| terms::make_list(ctx, &[this]).expect("a list"));
            let values: Vec<(&str, Eterm)> = pairs
                .iter()
                .map(|(name, flag)| (*name, rt.intern(flag)))
                .collect();
            let options = make_map(&mut rt, &values);
            let term = call(&mut rt, "erlang", "nodes", &[list, options]).expect("nodes/2");
            assert_eq!(
                rendered(&mut rt, term),
                "[{nonode@nohost,#{connection_id => undefined,node_type => this}}]",
                "{pairs:?}"
            );
        }
    }

    /// The C's `goto badarg` arms: a non-map `Options` (`[]` included), an unrecognised key,
    /// a recognised key valued anything but `true`/`false`, and an invalid `NodeType`
    /// element. The options are validated **before** the empty-set short cut, so each of
    /// these is `badarg` even though the node set is empty.
    #[test]
    fn nodes_2_rejects_malformed_options_and_selectors() {
        // A non-map: `[]` (`NIL` is not a map) and a plain atom.
        for bad in ["[]", "bogus_options"] {
            let mut rt = runtime();
            let visible = rt.intern("visible");
            let options = rt.eval(bad).expect("an options term");
            let error = call(&mut rt, "erlang", "nodes", &[visible, options])
                .expect_err("a non-map options");
            assert_eq!(error.as_atom(), Some("badarg"), "{bad}: {error}");
        }
        // An unrecognised key: the recognised-key count cannot equal the map size.
        let mut rt = runtime();
        let visible = rt.intern("visible");
        let yes = rt.intern("true");
        let options = make_map(&mut rt, &[("bogus", yes)]);
        assert!(call(&mut rt, "erlang", "nodes", &[visible, options]).is_err());
        // A recognised key with a non-boolean value.
        let mut rt = runtime();
        let visible = rt.intern("visible");
        let options = make_map(&mut rt, &[("connection_id", make_small(1))]);
        assert!(call(&mut rt, "erlang", "nodes", &[visible, options]).is_err());
        // An invalid `NodeType` element (integer, and an atom with no switch case).
        let mut rt = runtime();
        let none = make_map(&mut rt, &[]);
        assert!(call(&mut rt, "erlang", "nodes", &[make_small(42), none]).is_err());
        let mut rt = runtime();
        let all = rt.intern("all");
        let none = make_map(&mut rt, &[]);
        assert!(call(&mut rt, "erlang", "nodes", &[all, none]).is_err());
    }

    // -- net_adm:ping/1 -------------------------------------------------------

    #[test]
    fn net_adm_ping_is_pang_on_a_dormant_node() {
        // The oracle answers `pang` for every argument on a `nonode@nohost` VM -- its own
        // node name and an unrelated atom alike.
        for name in ["nonode@nohost", "nosuchnode@nowhere", "foo"] {
            let mut rt = runtime();
            let peer = rt.intern(name);
            let term = call(&mut rt, "net_adm", "ping", &[peer])
                .unwrap_or_else(|e| panic!("net_adm:ping({name}): {e}"));
            assert_eq!(rendered(&mut rt, term), "pang", "net_adm:ping({name})");
        }
    }

    #[test]
    fn net_adm_ping_rejects_a_non_atom_argument() {
        let mut rt = runtime();
        let text = rt.eval("\"str\"").expect("the string \"str\"");
        let error = call(&mut rt, "net_adm", "ping", &[text]).expect_err("a non-atom peer");
        // The oracle raises `function_clause`; a native BIF can only reject with `badarg`
        // (see the module docs) -- which is the honest typed substitute.
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
        assert!(call(&mut rt, "net_adm", "ping", &[make_small(1)]).is_err());
        assert!(call(&mut rt, "net_adm", "ping", &[]).is_err());
    }

    // -- net_kernel:connect_node/1 -------------------------------------------

    #[test]
    fn connect_node_is_ignored_on_a_dormant_node() {
        // Measured on the oracle twice: a non-distributed VM answers `ignored` for its own
        // `nonode@nohost` name and for an arbitrary atom.
        for name in ["nonode@nohost", "foo"] {
            let mut rt = runtime();
            let peer = rt.intern(name);
            let term = call(&mut rt, "net_kernel", "connect_node", &[peer])
                .unwrap_or_else(|e| panic!("net_kernel:connect_node({name}): {e}"));
            assert_eq!(rendered(&mut rt, term), "ignored", "connect_node({name})");
        }
    }

    #[test]
    fn connect_node_rejects_a_non_atom_argument() {
        let mut rt = runtime();
        let text = rt.eval("\"str\"").expect("the string \"str\"");
        let error =
            call(&mut rt, "net_kernel", "connect_node", &[text]).expect_err("a non-atom peer");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
        assert!(call(&mut rt, "net_kernel", "connect_node", &[make_small(1)]).is_err());
    }

    // -- the name helpers -----------------------------------------------------

    #[test]
    fn same_node_accepts_the_short_and_the_full_name() {
        assert!(same_node("foo@host", "foo@host"));
        assert!(same_node("foo", "foo@host"));
        assert!(!same_node("bar", "foo@host"));
        assert!(!same_node("foo@other", "foo@host"));
        assert!(!same_node("foo@host", "nonode@nohost"));
    }

    #[test]
    fn connect_name_strips_the_host_of_a_full_name() {
        // `epmd` registers a node under its short name, so that is what is dialled.
        assert_eq!(connect_name("foo@host"), "foo");
        assert_eq!(connect_name("foo"), "foo");
    }
}
