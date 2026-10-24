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

//! The `erlang:nodes/0,1,2` node set: the selectors, the `nodes/2` information map and the
//! `NodeInfoOptions`/`NodeEntryType` shapes they fold (`dist.c:nodes`). The contract each answer
//! keeps is [`super`]'s module docs.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, make_small, Eterm, NIL};

use crate::context::RuntimeContext;

impl RuntimeContext<'_> {
    // -- erlang:nodes/0,1,2 ---------------------------------------------------

    /// `erlang:nodes/0`: the connected **visible** nodes.
    ///
    /// `dist.c:'nodes_0'/0` is `nodes(visible)`, so this is the runtime's own established
    /// outbound links ([`Runtime::dist_connected_nodes`]) in connection order -- the empty
    /// list for a runtime that is not a node (and for a node with no peers).
    pub(in crate::native) fn nodes_0(&mut self) -> Result<Eterm> {
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
    pub(in crate::native) fn nodes_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
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
    /// A **peer** entry's `connection_id` is the link's local identity
    /// ([`Runtime::dist_connection_id`]); a peer whose link retired between the node set and
    /// the lookup answers the atom `undefined` (the C's `~0`), never an error and never a
    /// fabricated integer.
    pub(in crate::native) fn nodes_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
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
        let mut tuples: Vec<Eterm> = Vec::with_capacity(entries.len());
        for (node, kind) in entries.drain(..) {
            let mut pairs: Vec<(Eterm, Eterm)> = Vec::with_capacity(2);
            if info.connection_id {
                // `this` carries the C's `dep->connection_id == ~0`, which it renders as
                // the atom `undefined`; a peer carries its link's own local identity, and
                // a link that retired out from under the node set falls back to the same
                // `undefined` (never a raise, never a fabricated integer).
                let value = match kind {
                    NodeEntryType::This => None,
                    NodeEntryType::Visible => self
                        .atom_name(node)
                        .and_then(|peer| self.runtime.dist_connection_id(&peer)),
                };
                let value = match value {
                    Some(id) => make_small(id as i64),
                    None => self.atom("undefined")?,
                };
                pairs.push((connection_id_key, value));
            }
            if info.node_type {
                pairs.push((node_type_key, self.atom(kind.atom_name())?));
            }
            // The table handle is taken **per entry, after** this entry's interns: an
            // `atoms_handle` is an `Rc` clone and `atoms_mut` is `Rc::make_mut`, so an
            // intern taken while a handle is alive deep-clones the whole shared table (one
            // `Arc<str>` allocation + one hash-map insertion per atom) and drops the copy at
            // once. Holding it across the loop cloned the table once per interned name — two
            // clones per node entry. Taken last it is alive only for the map construction,
            // and the code server stays the table's sole owner while the names are appended.
            let atoms = self.runtime.code_server.atoms_handle();
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
