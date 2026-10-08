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

//! Oracle-pinned integration tests for `erlang:node/0,1` (registered by
//! `src/convert.rs`), driven through the **public** API only
//! ([`default_registry`] + [`BifRegistry::call`]).
//!
//! Ground truth (read-only) `../../erts/emulator/beam/dist.c:6242-6256`:
//!
//! ```c
//! BIF_RETTYPE node_1(BIF_ALIST_1) {
//!     /* NOTE: The JIT has its own implementation of this BIF. */
//!     if (is_not_node_container(BIF_ARG_1)) BIF_ERROR(BIF_P, BADARG);
//!     BIF_RET(node_container_node_name(BIF_ARG_1));
//! }
//! BIF_RETTYPE node_0(BIF_ALIST_0) { BIF_RET(erts_this_dist_entry->sysname); }
//! ```
//!
//! `is_node_container(x) = is_internal(x) || is_external(x)` and
//! `node_container_node_name(x)` is `internal_node_name(x) = erts_this_node->sysname`
//! for internal and `external_node_name(x) = external_node(x)->sysname` for external
//! (`erl_node_container_utils.h:74-94`).
//!
//! Every golden carries the probe that pinned it (system OTP 29.1.1 / erts-17.1).
//! The local cases go through the [`LocalNode`] seam (`BifContext::local_node`,
//! the identity `beam_rt` installs as `(nonode@nohost, 0)`); the external cases
//! read the node atom frozen into the term's `[header, node, next, data]` block
//! (built here with the safe `beam_codec::term` writers, the same layout the
//! oracle's `binary_to_term` produces). One live cross-check shells out to the
//! system `erl` and **skips loudly** when the oracle is absent.

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_codec::term as codec_term;
use beam_common::BeamError;
use beam_process::{PortId, ProcessId};
use beam_term::{atom_val, AtomTable, Eterm};

/// The atoms these tests resolve by name: the local node (which `node/0` and the
/// local arm of `node/1` look up) plus two foreign node names for the external
/// containers and the `LocalNode` stub.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "nonode@nohost",
        "remote@host",
        "other@host",
    ]);
    atoms
}

/// A generous heap: a ref term and the external-container blocks are a handful of
/// words each.
fn heap() -> Heap {
    Heap::new(4096).expect("test heap")
}

/// Runs `module:function/arity` through the full public registry.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    reg.call(module, function, arity, ctx, args)
}

/// The local node for `X` — `erlang:node/1` through the registry.
fn node_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, x: Eterm) -> Result<Eterm, BeamError> {
    call(reg, ctx, "erlang", "node", 1, &[x])
}

/// The local node — `erlang:node/0` through the registry.
fn local_node(reg: &BifRegistry, ctx: &mut BifContext<'_>) -> Result<Eterm, BeamError> {
    call(reg, ctx, "erlang", "node", 0, &[])
}

/// The name of an atom term, as the shared atom table spells it.
fn atom_name(atoms: &AtomTable, term: Eterm) -> String {
    atoms
        .name(atom_val(term))
        .expect("an interned atom name")
        .to_string()
}

/// The char list `"s"` as an Erlang term (`list_to_ref/1`'s argument shape).
fn charlist(ctx: &mut BifContext<'_>, s: &str) -> Eterm {
    let elems: Vec<Eterm> = s
        .bytes()
        .map(|b| ctx.int(i64::from(b)).expect("fits"))
        .collect();
    ctx.list(&elems).expect("the string fits")
}

/// Runs one `-eval` statement under the system `erl`; `None` (with a loud
/// message) when the oracle is unavailable, so the cross-check skips instead of
/// failing.
fn oracle_eval(statement: &str) -> Option<String> {
    match Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output()
    {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// node/0
// ---------------------------------------------------------------------------

#[test]
fn node_0_is_the_local_node() {
    // ORACLE: `erl -noshell -eval 'io:format("~p~n",[erlang:node()])' -s init stop`
    //         => `nonode@nohost`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let node = local_node(&reg, &mut ctx).expect("the atom is seeded");
    assert_eq!(atom_name(&atoms, node), "nonode@nohost");
}

// ---------------------------------------------------------------------------
// node/1 — internal (local) containers
// ---------------------------------------------------------------------------

#[test]
fn node_of_a_local_pid_port_and_ref_is_the_local_node() {
    // ORACLE:
    //   `io:format("~p~n",[erlang:node(self())])`                        => nonode@nohost
    //   `io:format("~p~n",[erlang:node(erlang:list_to_pid("<0.1.0>"))])`  => nonode@nohost
    //   `P=open_port({spawn,"cat"},[]), io:format("~p~n",[erlang:node(P)])` => nonode@nohost
    //   `io:format("~p~n",[erlang:node(erlang:make_ref())])`              => nonode@nohost
    //   `io:format("~p~n",[erlang:node(erlang:list_to_ref("#Ref<0.0.0.1>"))])` => nonode@nohost
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // An internal pid immediate (`make_internal_pid`).
    let pid = ProcessId::new(123, 7).to_eterm();
    // An internal port immediate (`make_internal_port`).
    let port = PortId::new(7).to_eterm();
    // An internal reference — the M1 on-heap ref thing, built by the registered
    // `erlang:list_to_ref/1` (its textual form is the one the oracle accepts).
    let ref_text = charlist(&mut ctx, "#Ref<0.0.0.1>");
    let reference =
        call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[ref_text]).expect("a ref text");

    let expected = local_node(&reg, &mut ctx).expect("the atom is seeded");
    for (label, term) in [("pid", pid), ("port", port), ("ref", reference)] {
        let node = node_of(&reg, &mut ctx, term).expect("a node container");
        assert_eq!(
            atom_name(&atoms, node),
            "nonode@nohost",
            "node of a local {label}"
        );
        // All three agree with `node/0` — they resolve through one helper.
        assert_eq!(
            node.raw(),
            expected.raw(),
            "node/1 of a local {label} mirrors node/0"
        );
    }
}

// ---------------------------------------------------------------------------
// node/1 — external containers
// ---------------------------------------------------------------------------

#[test]
fn node_of_an_external_container_is_the_embedded_node() {
    // ORACLE (the images are exactly what the writers below lay out):
    //   `P=binary_to_term(<<131,88,100,0,11,"remote@host",0,0,0,1,0,0,0,0,0,0,0,0>>),
    //    io:format("~p~n",[erlang:node(P)])`                                 => remote@host
    //   `P=binary_to_term(<<131,89,100,0,11,"remote@host",0,0,0,7,0,0,0,0>>),
    //    io:format("~p~n",[erlang:node(P)])`                                 => remote@host
    //   `R=binary_to_term(<<131,90,0,1,100,0,11,"remote@host",0,0,0,0,0,0,0,1>>),
    //    io:format("~p~n",[erlang:node(R)])`                                 => remote@host
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A second name proves the answer is read from the term, not the local node.
    let remote = ctx.atom("remote@host").expect("seeded");
    let other = ctx.atom("other@host").expect("seeded");

    let external_pid =
        codec_term::make_external_pid(ctx.heap(), remote, 1, 0, 0).expect("the block fits");
    let external_port = codec_term::make_external_port(ctx.heap(), remote, 7, 0).expect("fits");
    let external_ref =
        codec_term::make_external_ref(ctx.heap(), remote, &[1], 0).expect("the block fits");

    for (label, term, expected) in [
        ("pid", external_pid, "remote@host"),
        ("port", external_port, "remote@host"),
        ("ref", external_ref, "remote@host"),
    ] {
        let node = node_of(&reg, &mut ctx, term).expect("a node container");
        assert_eq!(
            atom_name(&atoms, node),
            expected,
            "node of an external {label}"
        );
    }

    // The node slot is what is returned — a different embedded name comes back.
    let other_pid =
        codec_term::make_external_pid(ctx.heap(), other, 2, 0, 0).expect("the block fits");
    let node = node_of(&reg, &mut ctx, other_pid).expect("a node container");
    assert_eq!(atom_name(&atoms, node), "other@host");
}

// ---------------------------------------------------------------------------
// node/1 — non-containers
// ---------------------------------------------------------------------------

#[test]
fn node_rejects_a_non_container_with_badarg() {
    // ORACLE: `erlang:node(42)` / `erlang:node({a})` / `erlang:node(foo)` /
    //         `erlang:node(1.0)` / `erlang:node(<<>>)` / `erlang:node([])` /
    //         `erlang:node(#{})` are all `error:badarg`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // Constructors are hoisted into `let` bindings: passing `ctx.<ctor>(..)`
    // inline to a call that also takes `&mut ctx` would not borrow-check.
    let seven = ctx.int(7).expect("fits");
    let five = ctx.int(5).expect("fits");
    let tuple = ctx.tuple(&[seven]).expect("fits");
    let nil = ctx.list(&[]).expect("fits");
    let improv = ctx.cons(seven, five).expect("fits");
    let empty_bin = codec_term::make_binary(ctx.heap(), &[]).expect("fits");
    let float = ctx.make_float(1.0).expect("fits");
    let atom = ctx.atom("true").expect("seeded");
    let empty_map = call(&reg, &mut ctx, "maps", "new", 0, &[]).expect("maps:new/0");

    for (label, term) in [
        ("integer", seven),
        ("tuple", tuple),
        ("atom", atom),
        ("float", float),
        ("binary", empty_bin),
        ("nil", nil),
        ("improper list", improv),
        ("map", empty_map),
    ] {
        assert!(
            matches!(node_of(&reg, &mut ctx, term), Err(BeamError::BadArg)),
            "node/1 of a {label} must be badarg"
        );
    }
}

// ---------------------------------------------------------------------------
// The LocalNode seam
// ---------------------------------------------------------------------------

/// A minimal [`LocalNode`] test double answering a fixed identity.
struct StubNode {
    node: Option<beam_term::Atom>,
}

impl LocalNode for StubNode {
    fn local_node(&mut self) -> Option<(beam_term::Atom, u32)> {
        self.node.map(|node| (node, 7))
    }
}

#[test]
fn node_uses_the_local_node_hook_for_internal_containers_and_node_0() {
    // The oracle has no undistributed node whose name is not `nonode@nohost`, so
    // this seam behaviour is pinned to `dist.c:6247` (`node_container_node_name`)
    // and `dist.c:6255` (`erts_this_dist_entry->sysname`), not to a probe: both
    // `node/0` and the internal arm read the runtime's own node name.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let node_atom = atoms.lookup("remote@host").expect("seeded");
    let pid = ProcessId::new(123, 7).to_eterm();
    let self_pid = ProcessId::new(1, 0);

    {
        let mut hook = StubNode {
            node: Some(node_atom),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, self_pid).with_local_node(&mut hook);

        assert_eq!(
            atom_name(&atoms, local_node(&reg, &mut ctx).expect("hooked")),
            "remote@host"
        );
        assert_eq!(
            atom_name(
                &atoms,
                node_of(&reg, &mut ctx, pid).expect("a node container")
            ),
            "remote@host"
        );
    }

    // A hook that does not know the identity yields the typed `TermNotFound`
    // (never a fabricated atom) — even though `nonode@nohost` *is* interned here.
    {
        let mut unknown = StubNode { node: None };
        let mut ctx = BifContext::new(&mut heap, &atoms, self_pid).with_local_node(&mut unknown);
        assert!(matches!(
            node_of(&reg, &mut ctx, pid),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            local_node(&reg, &mut ctx),
            Err(BeamError::TermNotFound)
        ));
    }
}

#[test]
fn node_without_a_hook_needs_the_nonode_atom() {
    // No hook: `BifContext::local_node` falls back to `(nonode@nohost, 0)` *when
    // that atom is interned*. An unseeded table leaves the identity unknown, so
    // the local arms report the typed `TermNotFound` (never a fabricated atom);
    // a non-container is still `badarg` (it never needs the identity).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = AtomTable::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let pid = ProcessId::new(1, 0).to_eterm();
    assert!(matches!(
        node_of(&reg, &mut ctx, pid),
        Err(BeamError::TermNotFound)
    ));
    assert!(matches!(
        local_node(&reg, &mut ctx),
        Err(BeamError::TermNotFound)
    ));

    let bad = ctx.int(42).expect("fits");
    assert!(matches!(
        node_of(&reg, &mut ctx, bad),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// Arity
// ---------------------------------------------------------------------------

#[test]
fn node_arity_mismatch_is_badarity() {
    // `node/0` and `node/1` exist, so a call whose argument count contradicts the
    // resolved arity is `BadArity` (the registry's check, `registry.rs`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "node", 1, &[]),
        Err(BeamError::BadArity)
    ));
    let pid = ProcessId::new(1, 0).to_eterm();
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "node", 0, &[pid]),
        Err(BeamError::BadArity)
    ));
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------

#[test]
fn node_goldens_agree_with_the_oracle() {
    // One `erl` process per statement; each prints the `~p` of `node/1`. The
    // non-container probes wrap the call in `try` so the *reason* is printed
    // (`catch` would print the whole `{'EXIT',{badarg,[...]}}` tuple).
    let external_pid = "erlang:node(binary_to_term(<<131,88,100,0,11,\"remote@host\",\
                        0,0,0,1,0,0,0,0,0,0,0,0>>))";
    let external_port = "erlang:node(binary_to_term(<<131,89,100,0,11,\"remote@host\",\
                         0,0,0,7,0,0,0,0>>))";
    let external_ref = "erlang:node(binary_to_term(<<131,90,0,1,100,0,11,\"remote@host\",\
                        0,0,0,0,0,0,0,1>>))";

    let cases: [(&str, &str); 9] = [
        ("erlang:node()", "nonode@nohost"),
        (
            "erlang:node(erlang:list_to_pid(\"<0.1.0>\"))",
            "nonode@nohost",
        ),
        ("erlang:node(erlang:make_ref())", "nonode@nohost"),
        (
            "erlang:node(erlang:list_to_ref(\"#Ref<0.0.0.1>\"))",
            "nonode@nohost",
        ),
        (external_pid, "remote@host"),
        (external_port, "remote@host"),
        (external_ref, "remote@host"),
        ("try erlang:node(42) catch error:R -> R end", "badarg"),
        ("try erlang:node({a}) catch error:R -> R end", "badarg"),
    ];

    for (expr, expected) in cases {
        let statement = format!("io:format(\"~p~n\",[{expr}])");
        let Some(output) = oracle_eval(&statement) else {
            return; // no oracle — the loud skip above is the report
        };
        assert_eq!(output.trim(), expected, "oracle disagreed for `{expr}`");
    }
}
