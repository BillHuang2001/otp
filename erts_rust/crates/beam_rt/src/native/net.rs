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

//! The distribution **node-set and connector** floor: `erlang:nodes/0,1`,
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
//! * `nodes/2`'s options-map form is not served (the floor owns `nodes/0,1` only).
//! * `nodes(known)` reports this runtime's own `this` plus its connected peers; the
//!   oracle's extra `not_connected` entries (nodes another node knows) are not modelled.
//! * `hidden` links are not modelled, so `nodes(hidden)` is always `[]`.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::dist::NONODE_NAME;

impl RuntimeContext<'_> {
    // -- erlang:nodes/0,1 -----------------------------------------------------

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
        let mut result: Vec<Eterm> = Vec::new();
        let mut want_local = false;
        for name in &types {
            match name.as_str() {
                "visible" | "connected" => {
                    for node in self.connected_node_terms()? {
                        if !result.contains(&node) {
                            result.push(node);
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
            if !result.contains(&node) {
                result.push(node);
            }
        }
        terms::make_list(self, &result)
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
