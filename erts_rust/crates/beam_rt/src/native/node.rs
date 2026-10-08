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

//! The `erlang` node-identity floor: `is_alive/0`, `get_cookie/0`, `set_cookie/2` and
//! `nodes/0,1` (contract **A.5**, the distribution identity surface).
//!
//! The oracle serves these from three different places -- `nodes/0,1` and `is_alive/0` are
//! `dist.c` BIFs, `get_cookie/0` is an `erlang.erl` wrapper over `auth:get_cookie/0`, and
//! `set_cookie/2` is an `erlang.erl` wrapper over `auth:set_cookie/2` -- but every one of
//! them reads the same runtime-owned state: this node's name (`Runtime::local_node`), the
//! cookie (`Runtime::cookie`) and whether this VM is a live distributed node
//! (`Runtime::is_alive`). They therefore live on the runtime-native floor, consulted
//! **before** the `beam_bifs` registry, exactly like the process/registry queries in
//! [`super::process`] -- the distribution table they read is runtime state no BIF module can
//! reach.
//!
//! # What the answers are on a non-distributed node (`nonode@nohost`)
//!
//! Milestone 1 runs a `-noshell` VM unless `-sname`/`-name` configured a node, and the
//! differential corpus pins that case end to end (`tools/differential`,
//! `corpus::wire_ids::node_identity_cases`):
//!
//! | Call | Answer |
//! |---|---|
//! | `is_alive()` | `false` |
//! | `get_cookie()` | the atom `nocookie` -- never the cookie the host resolved, because `auth:get_cookie/0` answers `nocookie` unless the node is alive |
//! | `nodes()`, `nodes(connected)`, `nodes(visible)`, `nodes(hidden)` | `[]` |
//! | `nodes(this)`, `nodes(known)` | `[nonode@nohost]` |
//! | `set_cookie(node(), Cookie)` | raises `error:badarg` (the `erlang.erl` guard rejects `nonode@nohost`) |
//! | `set_cookie(OtherNode, 123)` | raises `error:badarg` (the cookie is not an atom) |
//! | `set_cookie(OtherNode, an_atom_cookie)` | raises `error:distribution_not_started` |
//!
//! # `nodes/1`'s argument grammar (`dist.c:nodes`)
//!
//! The C folds an atom argument into a one-element list and then folds the list's elements
//! into five flags (`visible`/`hidden`/`connected` select peers, `this`/`known` select the
//! local node); an element that is neither is `badarg`, and so is a non-list, non-atom
//! argument or an improper tail. This runtime holds **no** live peer connections, so the
//! peer-selecting types always answer `[]` and only `this`/`known` contribute anything --
//! the local node atom, de-duplicated in first-seen order.
//!
//! # `set_cookie/2`'s ordering (`erlang.erl:12670-12674`)
//!
//! The `erlang.erl` clause guard is a single conjunction -- `Node =/= nonode@nohost`,
//! `is_atom(Node)`, `is_atom(Cookie)` -- so a **non-atom** node *or* cookie is `badarg`
//! before the node is ever compared, and only a well-typed pair reaches
//! `auth:set_cookie/2` (which raises `distribution_not_started` when this VM has no
//! distribution). This module reproduces that order: type-check both arguments first, then
//! compare the node against this runtime's own, then require a **live** node before storing
//! the cookie.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_term::{is_atom, is_list, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::dist::NONODE_NAME;
use crate::error;

/// The `error` reason `set_cookie/2` raises for a well-typed call naming a node other than
/// this runtime's own -- the reason `auth:set_cookie/2` gives when there is no distribution
/// to talk to.
const DISTRIBUTION_NOT_STARTED: &str = "distribution_not_started";

impl RuntimeContext<'_> {
    // -- erlang:is_alive/0 ----------------------------------------------------

    /// `erlang:is_alive/0`: whether this VM is a **live** distributed node.
    ///
    /// "Alive" is the C's `is_alive()`: a node that bound its listener *and* was accepted by
    /// `epmd`, so a remote node can reach it ([`Runtime::is_alive`]). A `-noshell` VM (and a
    /// `-sname` VM whose registration failed) answers the atom `false`.
    pub(super) fn is_alive_0(&mut self) -> Result<Eterm> {
        let answer = if self.runtime.is_alive() {
            "true"
        } else {
            "false"
        };
        self.atom(answer)
    }

    // -- erlang:get_cookie/0 --------------------------------------------------

    /// `erlang:get_cookie/0`: the cookie atom of this node, or the atom `nocookie` when the
    /// node is not alive.
    ///
    /// The liveness gate is the oracle's own (`erlang.erl`'s `get_cookie/0` delegates to
    /// `auth:get_cookie/0`, which answers `nocookie` for any node that is not alive), so a
    /// host-resolved cookie file is **never** leaked through this BIF on a dormant node.
    pub(super) fn get_cookie_0(&mut self) -> Result<Eterm> {
        if !self.runtime.is_alive() {
            return self.atom("nocookie");
        }
        let cookie = String::from_utf8_lossy(self.runtime.cookie()).into_owned();
        self.atom(&cookie)
    }

    // -- erlang:set_cookie/2 --------------------------------------------------

    /// `erlang:set_cookie(Node, Cookie)` -- see the module docs for the four outcomes.
    ///
    /// * `badarg` when either argument is not an atom, **before** the node is compared;
    /// * `distribution_not_started` for a well-typed pair naming a node other than this
    ///   runtime's own;
    /// * `badarg` for this runtime's own node name on a VM that is not alive;
    /// * otherwise the cookie bytes are installed ([`Runtime::set_cookie_bytes`]) and the
    ///   atom `true` is returned.
    pub(super) fn set_cookie_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let node = *args.first().ok_or(BeamError::BadArg)?;
        let cookie = *args.get(1).ok_or(BeamError::BadArg)?;
        // The two type checks come first: `erlang.erl`'s guard is one conjunction, so a
        // non-atom cookie is `badarg` even when the node is a remote one.
        let node_name = self.atom_name(node).ok_or(BeamError::BadArg)?;
        let cookie_name = self.atom_name(cookie).ok_or(BeamError::BadArg)?;
        if node_name != self.local_node_name() {
            return Err(error::message(DISTRIBUTION_NOT_STARTED));
        }
        if !self.runtime.is_alive() {
            return Err(BeamError::BadArg);
        }
        self.runtime.set_cookie_bytes(cookie_name.into_bytes());
        self.atom("true")
    }

    // -- erlang:nodes/0,1 -----------------------------------------------------

    /// `erlang:nodes/0`: the connected **visible** nodes -- always `[]` here.
    ///
    /// `dist.c:'nodes_0'/0` is `nodes(visible)`, and this runtime holds no live peer
    /// connections (a link is never surfaced as a node), so the honest answer is the empty
    /// list regardless of whether a node is configured.
    pub(super) fn nodes_0(&mut self) -> Result<Eterm> {
        terms::make_list(self, &[])
    }

    /// `erlang:nodes/1`: the node set selected by one atom or a list of them.
    ///
    /// `visible`/`hidden`/`connected` select peers (there are none, so they contribute
    /// nothing); `this`/`known` select the local node. Any other element -- and a non-atom,
    /// non-list argument, or an improper tail -- is `badarg`, the C's `goto badarg`.
    pub(super) fn nodes_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let argument = *args.first().ok_or(BeamError::BadArg)?;
        let types = self.node_types(argument)?;
        let mut result: Vec<Eterm> = Vec::new();
        for name in &types {
            match name.as_str() {
                "visible" | "hidden" | "connected" => {}
                "this" | "known" => {
                    // The local node contributes at most once, whatever combination of
                    // `this`/`known` was asked for -- the first-seen-order de-duplication the
                    // objective's contract spells out (`nodes([this,known])` is a
                    // one-element list).
                    let node = self.runtime.local_node_term();
                    if !result.contains(&node) {
                        result.push(node);
                    }
                }
                _ => return Err(BeamError::BadArg),
            }
        }
        terms::make_list(self, &result)
    }

    // -- helpers --------------------------------------------------------------

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

    /// This runtime's own node name (`node()`), the name `erlang.erl`'s `set_cookie/2` guard
    /// compares the first argument against.
    ///
    /// It is the configured `node@host` when `-name`/`-sname` made this a node, and the fixed
    /// [`NONODE_NAME`] otherwise -- the very string [`Runtime::local_node`] was interned from
    /// (see `Runtime::new`), so the two can never disagree.
    fn local_node_name(&self) -> String {
        match self.runtime.node_name() {
            Some(name) => name.to_string(),
            None => NONODE_NAME.to_string(),
        }
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

    /// A runtime built from CLI switches through the **real** parser, the way
    /// `tests/dist.rs::node_runtime` does it.
    fn node_runtime(args: &[&str]) -> Result<Runtime> {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let config = crate::parse(&args)?;
        Runtime::new(config)
    }

    /// Runs `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are
    /// dropped before `rt` is borrowed again.
    fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
        let mut sink = NullSink;
        let mut ctx = RuntimeContext::with_output(rt, &mut sink);
        f(&mut ctx)
    }

    /// Calls an `erlang` BIF through the runtime's `ExecutionContext` seam.
    fn call(rt: &mut Runtime, name: &str, args: &[Eterm]) -> Result<Eterm> {
        with_context(rt, |ctx| ctx.call_bif("erlang", name, args.len() as u32, args))
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
    fn is_alive_is_false_without_distribution() {
        let mut rt = runtime();
        assert!(!rt.is_alive(), "a -noshell runtime is not a node");
        let term = call(&mut rt, "is_alive", &[]).expect("erlang:is_alive/0");
        assert_eq!(rt.atom_name_of(term), Some("false"));
    }

    #[test]
    fn get_cookie_is_nocookie_without_distribution() {
        let mut rt = runtime();
        // A host-resolved cookie must never leak through the BIF on a dormant node: even
        // with bytes installed, the answer stays `nocookie` while the node is not alive.
        rt.set_cookie_bytes(b"a_test_cookie".to_vec());
        let term = call(&mut rt, "get_cookie", &[]).expect("erlang:get_cookie/0");
        assert_eq!(rt.atom_name_of(term), Some("nocookie"));
    }

    #[test]
    fn nodes_0_and_the_peer_types_are_the_empty_list() {
        let mut rt = runtime();
        let term = call(&mut rt, "nodes", &[]).expect("erlang:nodes/0");
        assert_eq!(rendered(&mut rt, term), "[]");
        for name in ["connected", "visible", "hidden"] {
            let selector = rt.intern(name);
            let term =
                call(&mut rt, "nodes", &[selector]).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(rendered(&mut rt, term), "[]", "nodes({name})");
        }
    }

    #[test]
    fn nodes_this_and_known_are_the_local_node() {
        for name in ["this", "known"] {
            let mut rt = runtime();
            let selector = rt.intern(name);
            let term =
                call(&mut rt, "nodes", &[selector]).unwrap_or_else(|e| panic!("{name}: {e}"));
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
        let term = call(&mut rt, "nodes", &[empty]).expect("nodes([])");
        assert_eq!(rendered(&mut rt, term), "[]");

        // `[this, connected]` and its permutation are the local node, exactly once.
        for elements in [vec![this, connected], vec![connected, this]] {
            let list = build(&mut rt, |ctx| {
                terms::make_list(ctx, &elements).expect("a list")
            });
            let term = call(&mut rt, "nodes", &[list]).expect("nodes([...])");
            assert_eq!(rendered(&mut rt, term), "[nonode@nohost]");
        }

        // A repeated selector stays a one-element list (first-seen-order de-duplication).
        let repeated = build(&mut rt, |ctx| {
            terms::make_list(ctx, &[this, known, this]).expect("a list")
        });
        let term = call(&mut rt, "nodes", &[repeated]).expect("nodes([this,known,this])");
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
                call(&mut rt, "nodes", &[selector]).is_err(),
                "nodes({name}) must raise badarg"
            );
        }
        // A non-atom, non-list argument; a tuple; and a list with a bad element.
        let mut rt = runtime();
        assert!(call(&mut rt, "nodes", &[make_small(42)]).is_err());
        let tuple = build(&mut rt, |ctx| {
            terms::make_tuple(ctx, &[make_small(1)]).expect("a tuple")
        });
        assert!(call(&mut rt, "nodes", &[tuple]).is_err());
        let this = rt.intern("this");
        let bad_element = build(&mut rt, |ctx| {
            terms::make_list(ctx, &[this, make_small(42)]).expect("a list")
        });
        assert!(call(&mut rt, "nodes", &[bad_element]).is_err());
    }

    #[test]
    fn nodes_of_an_improper_list_is_badarg() {
        let mut rt = runtime();
        let improper = rt.eval("[this|bogus_type]").expect("an improper list");
        assert!(
            call(&mut rt, "nodes", &[improper]).is_err(),
            "an improper tail is badarg"
        );
    }

    #[test]
    fn set_cookie_raises_the_corpus_shapes() {
        // `wire_ids/set_cookie_local_node_is_badarg`
        let mut rt = runtime();
        let before = rt.cookie().to_vec();
        let node = rt.local_node_term();
        let cookie = rt.intern("a_test_cookie");
        let error = call(&mut rt, "set_cookie", &[node, cookie])
            .expect_err("set_cookie(node(), Cookie) on a dormant node");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
        assert_eq!(rt.cookie(), before, "a rejected call stores nothing");

        // `wire_ids/set_cookie_bad_cookie_type`
        let mut rt = runtime();
        let node = rt.local_node_term();
        let error = call(&mut rt, "set_cookie", &[node, make_small(123)])
            .expect_err("set_cookie(node(), 123)");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");

        // `wire_ids/set_cookie_non_atom_node_badarg`
        let mut rt = runtime();
        let text = rt.eval("\"str\"").expect("the string \"str\"");
        let cookie = rt.intern("a_test_cookie");
        let error = call(&mut rt, "set_cookie", &[text, cookie]).expect_err("a non-atom node");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

    #[test]
    fn set_cookie_on_another_node_reports_the_missing_distribution() {
        // `wire_ids/set_cookie_other_node_requires_distribution`: a well-typed pair naming
        // another node reaches `auth:set_cookie/2`, which raises for a VM with no
        // distribution.
        let mut rt = runtime();
        let other = rt.intern("someother@node");
        let cookie = rt.intern("a_test_cookie");
        let error =
            call(&mut rt, "set_cookie", &[other, cookie]).expect_err("set_cookie(other, Cookie)");
        // `error::message` builds an `error`-class raise whose reason is the atom; the
        // reason is an owned `String`, so `BeamError::as_atom()` (the `&'static str` lookup)
        // answers `None` while `Display` -- the CLI/-eval rendering -- names it exactly as the
        // oracle's `~w` does.
        assert_eq!(error.as_atom(), None, "{error}");
        assert_eq!(error.to_string(), "error distribution_not_started");

        // `wire_ids/set_cookie_other_node_bad_cookie_type`: the guard is one conjunction, so
        // the non-atom cookie is `badarg`, not the distribution reason.
        let mut rt = runtime();
        let other = rt.intern("someother@node");
        let error = call(&mut rt, "set_cookie", &[other, make_small(123)])
            .expect_err("set_cookie(other, 123)");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

    #[test]
    fn set_cookie_stores_the_cookie_on_a_live_node() {
        // The success path needs a node `epmd` accepted, which needs a reachable `epmd`.
        // Without one the registration fails and this runtime is not alive -- the case the
        // tests above pin. The probe is a loud skip, never a silent pass.
        let Ok(mut rt) = node_runtime(&[
            "-noshell",
            "-sname",
            &format!("beamrtnc{}", std::process::id()),
            "-setcookie",
            "probe_cookie",
            "-eval",
            "ok.",
        ]) else {
            eprintln!("SKIP set_cookie_stores_the_cookie_on_a_live_node: the node did not start");
            return;
        };
        let _ = rt.pump_dist();
        if !rt.is_alive() {
            eprintln!(
                "SKIP set_cookie_stores_the_cookie_on_a_live_node: no `epmd` accepted the \
                 registration, so this runtime is not alive"
            );
            return;
        }
        let node = rt.local_node_term();
        let cookie = rt.intern("a_live_cookie");
        let answer = call(&mut rt, "set_cookie", &[node, cookie]).expect("the live set_cookie/2");
        assert_eq!(rt.atom_name_of(answer), Some("true"));
        assert_eq!(rt.cookie(), b"a_live_cookie");
        // A live node's `get_cookie/0` really answers the installed cookie.
        let got = call(&mut rt, "get_cookie", &[]).expect("erlang:get_cookie/0");
        assert_eq!(rt.atom_name_of(got), Some("a_live_cookie"));
        // And the node name is the configured one, not `nonode@nohost`.
        assert_ne!(rt.local_node_term(), rt.intern("nonode@nohost"));
    }
}
