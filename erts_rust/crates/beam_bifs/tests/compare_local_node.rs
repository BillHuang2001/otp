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
//! The comparison BIFs' **node-aware** term ordering — the mixed
//! internal/external `pid`/`port`/`ref` arms of `erts_cmp_compound`.
//!
//! An internal pid/port/ref word carries no node (it is *this* node, implicitly), so
//! the C compares such a pair against an external one field by field with
//! `erts_this_node` substituted (`erts/emulator/beam/utils.c:1751-1811`,
//! `:2219-2308`). `beam_term::compare::cmp_with_local_node` /
//! `cmp_arith_with_local_node` are that arm; `beam_bifs`' ordering BIFs supply the
//! node (`crate::compare::local_node_id`, the [`BifContext::local_node`] seam)
//! instead of falling back to the context-free **class ladder**.
//!
//! # Why this is not a cosmetic difference
//!
//! The ladder ranks `ExternalPid` (9) *before* `Pid` (10), `ExternalPort` (7) before
//! `Port` (8) and `ExternalRef` (4) before `Ref` (5), so without a node **every**
//! internal/external pair is ordered "external first, fields ignored". The oracle
//! orders by the **node name** first, which flips the verdict for a node sorting
//! after `nonode@nohost` — and makes an external pid naming the local node with the
//! same number/serial *exactly equal* to the internal one
//! (`<0.10.0> =:= binary_to_term(<<131,88,…>>)` is `true` on the oracle).
//!
//! Every test below pins **both** answers: the BIF's (node-aware) and
//! `beam_term::compare::cmp{,_arith}`'s (the documented no-context convention), so a
//! regression in either direction is caught.
//!
//! # Known divergence (owned by `beam_term`, not pinned against the oracle here)
//!
//! For a mixed **port** pair whose nodes are *equal*, the C hits
//! `ERTS_INTERNAL_ERROR("different nodes compared equal")` (`utils.c:1758`) — an
//! assertion the release build compiles out, after which control falls through into
//! the pid arm and compares port blocks as if they were pids. The oracle's answer
//! there is that accident, not a rule; `beam_term::compare::identity` documents a
//! total, antisymmetric same-node convention (compare the port numbers) instead, and
//! this file pins *that* (in `src/compare.rs`'s unit tests) rather than the accident.
//!
//! # The oracle pins
//!
//! The live cross-check shells out to the system `erl` and compares its transcript
//! against this crate's BIFs line by line; see [`ORACLE_EVAL`] for the probe. It
//! **fails loudly** when no oracle is available (the crate convention for the
//! integration files that shell out to `erl` — a missing oracle is not a pass).
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_codec::term as codec_term;
use beam_process::{PortId, ProcessId};
use beam_term::compare::{cmp, cmp_arith, eq as term_eq};
use beam_term::{atom_val, make_atom, signed_value, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// The atoms these tests resolve by name.
///
/// `nonode@nohost` is the node the no-hook [`BifContext::local_node`] fallback
/// reports, with creation `0` — exactly what a non-distributed `erl -noshell`
/// reports (`erlang:node()` = `nonode@nohost`, `erlang:system_info(creation)` = `0`).
/// The two foreign names bracket it lexicographically: `aaa@host` sorts **before**
/// it and `zzz@host` **after** it, so either direction of the node comparison can be
/// pinned. `value` is `lists:keysearch/3`'s result tag; `e`/`s` are the tuple tags
/// that tell a `lists:keyfind/3` walk's two candidates apart.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "nonode@nohost",
        "aaa@host",
        "zzz@host",
        "value",
        "e",
        "s",
    ]);
    atoms
}

/// A generous term-construction arena (each external container is a handful of
/// words).
fn heap() -> Heap {
    Heap::new(4096).expect("test heap")
}

/// The internal pid this crate's comparisons see as "self": `<0.10.0>`. The number is
/// arbitrary — nothing in the ordering depends on it — but it matches what the live
/// oracle probe reports for its own `self()` at startup (`<0.10.0>`), so the two
/// sides build the very same pair. The live test derives the oracle's actual values
/// instead of assuming them.
const SELF_NUM: u64 = 10;
/// `self()`'s serial (`<0.10.0>` ⇒ serial 0).
const SELF_SER: u64 = 0;
/// The internal port number used by the mixed-port pins (`#Port<0.7>`).
const PORT_NUM: u64 = 7;
/// The internal reference id used by the mixed-ref pins (`#Ref<0.0.0.9>`).
const REF_ID: u64 = 9;

/// The internal pid term `self()` stands for.
fn self_pid() -> Eterm {
    ProcessId::new(SELF_NUM, SELF_SER).to_eterm()
}

/// The internal pid handle matching [`self_pid`].
fn self_handle() -> ProcessId {
    ProcessId::new(SELF_NUM, SELF_SER)
}

/// The internal port term `#Port<0.7>`.
fn self_port() -> Eterm {
    PortId::new(PORT_NUM).to_eterm()
}

/// The node-name atom term `name` (caller: `name` is seeded in `atoms`).
fn node_term(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).expect("a seeded node name"))
}

/// An external pid for `node`/`number`/`serial`/`creation` — the `NEW_PID_EXT` (`88`)
/// shape, built with the safe `beam_codec::term` writer (the frozen layout the
/// oracle's `binary_to_term` produces: the node atom and creation sit where the C
/// keeps its node pointer, and the block's arity is the C's).
fn external_pid(
    arena: &mut Heap,
    atoms: &AtomTable,
    node: &str,
    number: u32,
    serial: u32,
    creation: u32,
) -> Eterm {
    codec_term::make_external_pid(arena, node_term(atoms, node), number, serial, creation)
        .expect("the external pid block fits")
}

/// An external port (`NEW_PORT_EXT`, `89`).
fn external_port(
    arena: &mut Heap,
    atoms: &AtomTable,
    node: &str,
    number: u64,
    creation: u32,
) -> Eterm {
    codec_term::make_external_port(arena, node_term(atoms, node), number, creation)
        .expect("the external port block fits")
}

/// An external reference (`NEWER_REFERENCE_EXT`, `90`) whose id array is `ids`.
fn external_ref(
    arena: &mut Heap,
    atoms: &AtomTable,
    node: &str,
    ids: &[u32],
    creation: u32,
) -> Eterm {
    codec_term::make_external_ref(arena, node_term(atoms, node), ids, creation)
        .expect("the external ref block fits")
}

/// The internal (M1 four-word `[header, node, serial, id]`) reference with id `id`.
fn internal_ref(arena: &mut Heap, id: u64) -> Eterm {
    beam_codec::make_internal_ref(arena, id).expect("the ref id fits a small")
}

/// Runs `module:function/arity` through the full public registry, panicking on a
/// raise (every call below is a valid one).
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Eterm {
    reg.call(module, function, arity, ctx, args)
        .unwrap_or_else(|err| panic!("{module}:{function}/{arity} failed: {err:?}"))
}

/// The boolean value of a `true`/`false` atom term.
fn truth(atoms: &AtomTable, t: Eterm) -> bool {
    match atoms.name(atom_val(t)) {
        Some("true") => true,
        Some("false") => false,
        other => panic!("expected a boolean atom, got {other:?}"),
    }
}

/// `A < B` through the registry.
fn lt(reg: &BifRegistry, ctx: &mut BifContext<'_>, a: Eterm, b: Eterm) -> bool {
    let result = call(reg, ctx, "erlang", "<", 2, &[a, b]);
    truth(ctx.atoms(), result)
}

/// `A > B` through the registry.
fn gt(reg: &BifRegistry, ctx: &mut BifContext<'_>, a: Eterm, b: Eterm) -> bool {
    let result = call(reg, ctx, "erlang", ">", 2, &[a, b]);
    truth(ctx.atoms(), result)
}

/// `A == B` through the registry.
fn eq_arith(reg: &BifRegistry, ctx: &mut BifContext<'_>, a: Eterm, b: Eterm) -> bool {
    let result = call(reg, ctx, "erlang", "==", 2, &[a, b]);
    truth(ctx.atoms(), result)
}

/// `A =:= B` through the registry.
fn eq_exact(reg: &BifRegistry, ctx: &mut BifContext<'_>, a: Eterm, b: Eterm) -> bool {
    let result = call(reg, ctx, "erlang", "=:=", 2, &[a, b]);
    truth(ctx.atoms(), result)
}

/// `erts_internal:cmp_term(A, B)` through the registry.
fn cmp_term(reg: &BifRegistry, ctx: &mut BifContext<'_>, a: Eterm, b: Eterm) -> i64 {
    signed_value(call(reg, ctx, "erts_internal", "cmp_term", 2, &[a, b]))
}

/// The head of a non-empty proper list term.
fn head(list: Eterm) -> Eterm {
    codec_term::cons_head(list).expect("a cons cell")
}

/// The second element of a two-or-more-element proper list term.
fn second(list: Eterm) -> Eterm {
    codec_term::cons_head(codec_term::cons_tail(list).expect("a cons cell")).expect("a cons cell")
}

// ---------------------------------------------------------------------------
// `erlang:'<'/2` … `erts_internal:cmp_term/2` — the node decides
// ---------------------------------------------------------------------------

/// The asymmetry that motivates the whole change: for an external pid at a node
/// sorting **after** `nonode@nohost`, the node-aware order says `self() < Ext`
/// (`true`) while the context-free class ladder says the opposite (`false`, because
/// it ranks `ExternalPid` before `Pid` and never looks at the node).
///
/// ORACLE: `erl -noshell`, `self()` = `<0.10.0>`, `node()` = `nonode@nohost`,
/// `erlang:system_info(creation)` = `0`,
/// `Ext = binary_to_term(<<131,88,100,8:16,"zzz@host",10:32,0:32,0:32>>)` ⇒
/// `self() < Ext` = `true`, `Ext < self()` = `false`, `self() =:= Ext` = `false`,
/// `erts_internal:cmp_term(self(), Ext)` = `-1`.
#[test]
fn an_external_pid_at_a_later_node_name_sorts_after_the_local_one() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let mut dest = heap();
    let me = self_pid();
    let ext = external_pid(
        &mut arena,
        &atoms,
        "zzz@host",
        SELF_NUM as u32,
        SELF_SER as u32,
        0,
    );
    let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());

    assert!(lt(&reg, &mut ctx, me, ext), "the node name decides: nonode@nohost < zzz@host");
    assert!(!gt(&reg, &mut ctx, me, ext), "the converse must not hold");
    assert!(!eq_arith(&reg, &mut ctx, me, ext), "different nodes are not ==");
    assert!(!eq_exact(&reg, &mut ctx, me, ext), "…nor =:=");
    assert_eq!(cmp_term(&reg, &mut ctx, me, ext), -1);
    assert_eq!(cmp_term(&reg, &mut ctx, ext, me), 1, "antisymmetric");

    // The documented no-context convention answers the opposite — which is what the
    // BIFs would answer if they forgot to supply the node.
    assert_eq!(
        cmp_arith(me, ext, &atoms),
        core::cmp::Ordering::Greater,
        "the plain class ladder ranks ExternalPid before Pid"
    );
    assert_eq!(cmp(me, ext, &atoms), core::cmp::Ordering::Greater);
}

/// The mirror image: a node name sorting **before** `nonode@nohost` makes the
/// external pid the *smaller* one — the verdict follows the **name**, not a fixed
/// internal-vs-external rank. (The ladder happens to agree here, which is why the
/// `zzz@host` case above is the load-bearing one.)
///
/// ORACLE (same probe, node `aaa@host`): `self() < Ext` = `false`,
/// `Ext < self()` = `true`, `erts_internal:cmp_term(self(), Ext)` = `1`.
#[test]
fn an_external_pid_at_an_earlier_node_name_sorts_before_the_local_one() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let mut dest = heap();
    let me = self_pid();
    let ext = external_pid(
        &mut arena,
        &atoms,
        "aaa@host",
        SELF_NUM as u32,
        SELF_SER as u32,
        0,
    );
    let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());

    assert!(!lt(&reg, &mut ctx, me, ext), "aaa@host < nonode@nohost, so Ext sorts first");
    assert!(gt(&reg, &mut ctx, me, ext));
    assert_eq!(cmp_term(&reg, &mut ctx, me, ext), 1);
    assert_eq!(cmp_term(&reg, &mut ctx, ext, me), -1);
    // The two node names flip the verdict, which no fixed rank could do.
    assert_eq!(
        cmp_arith(me, ext, &atoms),
        core::cmp::Ordering::Greater,
        "the ladder cannot see the name"
    );
}

/// With the node **names** equal the creation decides (`CMP_NODES`' second field) —
/// again where the ladder has nothing to say.
///
/// ORACLE (`Ext` = `nonode@nohost` with creation `7`): `self() < Ext` = `true`,
/// `self() =:= Ext` = `false`, `erts_internal:cmp_term(self(), Ext)` = `-1`.
#[test]
fn a_later_creation_at_the_local_node_name_sorts_the_external_pid_after() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let mut dest = heap();
    let me = self_pid();
    let ext = external_pid(
        &mut arena,
        &atoms,
        "nonode@nohost",
        SELF_NUM as u32,
        SELF_SER as u32,
        7,
    );
    let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());

    assert!(lt(&reg, &mut ctx, me, ext), "creation 0 < 7 decides the pair");
    assert!(!eq_exact(&reg, &mut ctx, me, ext));
    assert_eq!(cmp_term(&reg, &mut ctx, me, ext), -1);
    assert_eq!(
        cmp_arith(me, ext, &atoms),
        core::cmp::Ordering::Greater,
        "the ladder ignores creation"
    );
}

/// An external pid naming the **local** node with `self()`'s own number and serial is
/// *exactly equal* to it — `==` and `=:=` both `true`, `cmp_term` `0`. The class
/// ladder cannot express that at all (two different classes are never equal).
///
/// ORACLE (`Ext` = `nonode@nohost`, creation `0`, `self()`'s number/serial):
/// `self() =:= Ext` = `true`, `self() == Ext` = `true`,
/// `erts_internal:cmp_term(self(), Ext)` = `0`, and
/// `term_to_binary(self()) =:= term_to_binary(Ext)` = `true`.
#[test]
fn an_external_pid_at_the_local_node_with_the_same_fields_is_exactly_equal() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let mut dest = heap();
    let me = self_pid();
    let ext = external_pid(
        &mut arena,
        &atoms,
        "nonode@nohost",
        SELF_NUM as u32,
        SELF_SER as u32,
        0,
    );
    let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());

    assert!(eq_arith(&reg, &mut ctx, me, ext), "same node, serial, number ⇒ ==");
    assert!(eq_exact(&reg, &mut ctx, me, ext), "…and =:=");
    assert!(!lt(&reg, &mut ctx, me, ext));
    assert!(!gt(&reg, &mut ctx, me, ext));
    assert_eq!(cmp_term(&reg, &mut ctx, me, ext), 0);
    assert_eq!(cmp_term(&reg, &mut ctx, ext, me), 0);
    // The ladder cannot see it: two different classes are never equal.
    assert_eq!(cmp(me, ext, &atoms), core::cmp::Ordering::Greater);
    assert_eq!(cmp_arith(me, ext, &atoms), core::cmp::Ordering::Greater);
}

/// The mixed **port** pair: the C compares the node **only** (it never compares port
/// numbers — `utils.c:1751-1764`), so a foreign node name decides the pair whatever
/// the numbers are.
///
/// ORACLE (`P = erlang:list_to_port("#Port<0.7>")`,
/// `Ext = binary_to_term(<<131,89,100,8:16,"zzz@host",7:64,0:32>>)`):
/// `P < Ext` = `true`, `P =:= Ext` = `false`,
/// `erts_internal:cmp_term(P, Ext)` = `-1`.
#[test]
fn a_mixed_port_pair_is_decided_by_the_node_alone() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let mut dest = heap();
    let me = self_port();
    let same_number = external_port(&mut arena, &atoms, "zzz@host", PORT_NUM, 0);
    // A *different* number at the same foreign node: the node still decides.
    let other_number = external_port(&mut arena, &atoms, "zzz@host", 999, 0);
    let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());

    for (label, ext) in [("same number", same_number), ("different number", other_number)] {
        assert!(lt(&reg, &mut ctx, me, ext), "port, {label}: the node decides");
        assert!(!eq_arith(&reg, &mut ctx, me, ext), "port, {label}");
        assert_eq!(cmp_term(&reg, &mut ctx, me, ext), -1, "port, {label}");
        // The ladder answers the opposite (ExternalPort 7 before Port 8).
        assert_eq!(
            cmp_arith(me, ext, &atoms),
            core::cmp::Ordering::Greater,
            "port, {label}: the plain class ladder"
        );
    }
}

/// The mixed **reference** pair: the node first, then the ids as one big integer
/// (`utils.c:2239-2287`) — and with the node names and the ids equal the pair *is*
/// `Equal`, which the oracle confirms.
///
/// ORACLE (`R = erlang:list_to_ref("#Ref<0.0.0.9>")`): with
/// `Ext = binary_to_term(<<131,90,1:16,100,8:16,"zzz@host",0:32,9:32>>)` —
/// `R < Ext` = `true`, `erts_internal:cmp_term(R, Ext)` = `-1`; with `Ext`'s node
/// `nonode@nohost` — `R =:= Ext` = `true`, `erts_internal:cmp_term(R, Ext)` = `0`.
#[test]
fn a_mixed_reference_pair_uses_the_node_and_then_the_ids() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let mut dest = heap();
    let me = internal_ref(&mut arena, REF_ID);
    let foreign = external_ref(&mut arena, &atoms, "zzz@host", &[REF_ID as u32], 0);
    let same = external_ref(&mut arena, &atoms, "nonode@nohost", &[REF_ID as u32], 0);
    let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());

    assert!(lt(&reg, &mut ctx, me, foreign), "ref: the node decides first");
    assert_eq!(cmp_term(&reg, &mut ctx, me, foreign), -1);
    assert_eq!(
        cmp_arith(me, foreign, &atoms),
        core::cmp::Ordering::Greater,
        "ref: the plain class ladder"
    );

    // Same node, same ids ⇒ Equal (the C's `ref_common` id tail).
    assert!(eq_exact(&reg, &mut ctx, me, same), "ref: same node + same id ⇒ =:=");
    assert_eq!(cmp_term(&reg, &mut ctx, me, same), 0);
    assert_eq!(cmp_term(&reg, &mut ctx, same, me), 0);
}

// ---------------------------------------------------------------------------
// The `None`-local context, and hook precedence
// ---------------------------------------------------------------------------

/// With no local node at all — no [`LocalNode`] hook **and** the `nonode@nohost`
/// atom absent from the context's table, so [`BifContext::local_node`] answers
/// `None` — every pair keeps the context-free class ladder, i.e. exactly the
/// behaviour before the node was supplied. `zzz@host` is the case that would flip if
/// the node were invented rather than reasoned about.
#[test]
fn a_context_without_a_local_node_keeps_the_class_ladder() {
    let reg = default_registry();
    let mut atoms = AtomTable::new();
    // Deliberately *without* `nonode@nohost`: `local_node()` then has no fallback.
    atoms.seed(["true", "false", "zzz@host"]);
    let mut arena = heap();
    let mut dest = heap();
    let me = self_pid();
    let ext = external_pid(
        &mut arena,
        &atoms,
        "zzz@host",
        SELF_NUM as u32,
        SELF_SER as u32,
        0,
    );
    let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());

    assert_eq!(ctx.local_node(), None, "no hook and no nonode@nohost atom");
    // The ladder: ExternalPid (9) before Pid (10) — so `self() < Ext` is false.
    assert!(!lt(&reg, &mut ctx, me, ext));
    assert!(gt(&reg, &mut ctx, me, ext));
    assert_eq!(cmp_term(&reg, &mut ctx, me, ext), 1);
    assert!(!eq_exact(&reg, &mut ctx, me, ext));
    assert_eq!(cmp(me, ext, &atoms), core::cmp::Ordering::Greater);
}

/// A [`LocalNode`] hook **wins over** the `nonode@nohost` fallback: with a hook
/// reporting `(zzz@host, 0)`, an external pid at `nonode@nohost` (creation 0) is no
/// longer equal to `self()` — the hook's node is the local one now, and it sorts
/// after `nonode@nohost`; an external pid at `zzz@host` *is* equal.
#[test]
fn the_local_node_hook_decides_instead_of_the_fallback() {
    /// A `LocalNode` stub naming one node.
    struct Stub {
        node: beam_term::Atom,
        creation: u32,
    }
    impl LocalNode for Stub {
        fn local_node(&mut self) -> Option<(beam_term::Atom, u32)> {
            Some((self.node, self.creation))
        }
    }

    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let me = self_pid();
    let num = SELF_NUM as u32;
    let ser = SELF_SER as u32;
    let at_nonode = external_pid(&mut arena, &atoms, "nonode@nohost", num, ser, 0);
    let at_hook_node = external_pid(&mut arena, &atoms, "zzz@host", num, ser, 0);

    {
        let mut dest = heap();
        let mut hook = Stub {
            node: atoms.lookup("zzz@host").expect("seeded"),
            creation: 0,
        };
        let mut ctx =
            BifContext::new(&mut dest, &atoms, self_handle()).with_local_node(&mut hook);
        // The fallback would have said `Equal`; the hook's node says otherwise.
        assert!(!eq_exact(&reg, &mut ctx, me, at_nonode));
        // The hook's node is `zzz@host`, and `nonode@nohost` sorts *before* it, so the
        // external pid sorts first: `self() < Ext` is false, `Ext < self()` is true.
        assert!(!lt(&reg, &mut ctx, me, at_nonode), "zzz@host > nonode@nohost");
        assert!(gt(&reg, &mut ctx, me, at_nonode), "nonode@nohost < zzz@host");
        assert_eq!(cmp_term(&reg, &mut ctx, me, at_nonode), 1);
    }
    {
        // The hook's *creation* is used too: at the hook's own node the pair is Equal.
        let mut dest = heap();
        let mut hook = Stub {
            node: atoms.lookup("zzz@host").expect("seeded"),
            creation: 0,
        };
        let mut ctx =
            BifContext::new(&mut dest, &atoms, self_handle()).with_local_node(&mut hook);
        assert!(eq_exact(&reg, &mut ctx, me, at_hook_node));
        assert_eq!(cmp_term(&reg, &mut ctx, me, at_hook_node), 0);
    }
}

// ---------------------------------------------------------------------------
// Every ordering `lists` / `erlang` function that can see a pid, port or ref
// ---------------------------------------------------------------------------

/// The `lists` ordering functions thread the resolved node through their whole walk
/// or sort (resolved once per call, not per element) — pinned on the `zzz@host` pair,
/// where the node-aware and ladder answers differ.
///
/// ORACLE (`Ext` = `zzz@host`, creation 0, `self()` = `<0.10.0>`):
/// `lists:sort([self(),Ext])` = `[self(),Ext]`, `length(lists:usort([self(),Ext]))`
/// = `2`, `lists:min([self(),Ext])` = `self()`, `lists:max([self(),Ext])` = `Ext`,
/// `lists:keysort(1, [{self(),s},{Ext,e}])` = `[{self(),s},{Ext,e}]`,
/// `erlang:min(self(),Ext)` = `self()`, `erlang:max(self(),Ext)` = `Ext`.
#[test]
fn list_ordering_functions_use_the_node_aware_order() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let mut dest = heap();
    let me = self_pid();
    let ext = external_pid(
        &mut arena,
        &atoms,
        "zzz@host",
        SELF_NUM as u32,
        SELF_SER as u32,
        0,
    );
    let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());

    let list = ctx.list(&[me, ext]).expect("fits");
    let sorted = call(&reg, &mut ctx, "lists", "sort", 1, &[list]);
    assert_eq!(head(sorted), me, "sort/1 puts the internal pid first");
    assert_eq!(second(sorted), ext, "sort/1 keeps the input's second element");

    let unique = call(&reg, &mut ctx, "lists", "usort", 1, &[list]);
    assert_eq!(second(unique), ext, "usort/1 does not collapse unequal nodes");

    let min = call(&reg, &mut ctx, "lists", "min", 1, &[list]);
    let max = call(&reg, &mut ctx, "lists", "max", 1, &[list]);
    assert_eq!(min, me, "lists:min/1 of a node-ordered pair");
    assert_eq!(max, ext, "lists:max/1 of a node-ordered pair");

    // `lists:keysort/2` orders its index vector with the same comparator.
    let e = ctx.atom("e").expect("seeded");
    let s = ctx.atom("s").expect("seeded");
    let pair_me = ctx.tuple(&[me, s]).expect("fits");
    let pair_ext = ctx.tuple(&[ext, e]).expect("fits");
    let pairs = ctx.list(&[pair_me, pair_ext]).expect("fits");
    let one = ctx.int(1).expect("fits");
    let keysorted = call(&reg, &mut ctx, "lists", "keysort", 2, &[one, pairs]);
    assert_eq!(head(keysorted), pair_me, "keysort/2 orders by the key");
    assert_eq!(second(keysorted), pair_ext);

    // `erlang:min/2` / `erlang:max/2` (the `convert.rs` site), both operand orders.
    assert_eq!(call(&reg, &mut ctx, "erlang", "min", 2, &[me, ext]), me);
    assert_eq!(call(&reg, &mut ctx, "erlang", "min", 2, &[ext, me]), me);
    assert_eq!(call(&reg, &mut ctx, "erlang", "max", 2, &[me, ext]), ext);
    assert_eq!(call(&reg, &mut ctx, "erlang", "max", 2, &[ext, me]), ext);
}

/// `lists:keyfind/3`, `keymember/3` and `keysearch/3` compare their key with the
/// arithmetic order, so a mixed pair that is now `Equal` changes their answer —
/// pinned on the local-node pair (`nonode@nohost`, creation 0), where `self()` and
/// the decoded external pid are the same term to the oracle.
///
/// ORACLE (`Ext` = `nonode@nohost`, creation 0, `self()`'s number/serial):
/// `lists:keyfind(self(),1,[{Ext,e},{self(),s}])` = `{Ext,e}` (the first entry
/// matches), `lists:keymember(self(),1,[{Ext,e}])` = `true`,
/// `lists:keysearch(self(),1,[{Ext,e}])` = `{value,{Ext,e}}`; with `Ext`'s node
/// `zzz@host` they answer the `s`/`false` variants instead.
#[test]
fn list_key_lookup_sees_the_node_aware_equality() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let me = self_pid();
    let num = SELF_NUM as u32;
    let ser = SELF_SER as u32;
    let equal_ext = external_pid(&mut arena, &atoms, "nonode@nohost", num, ser, 0);
    let foreign = external_pid(&mut arena, &atoms, "zzz@host", num, ser, 0);

    {
        let mut dest = heap();
        let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());
        let e = ctx.atom("e").expect("seeded");
        let s = ctx.atom("s").expect("seeded");
        let one = ctx.int(1).expect("fits");
        // The equal case: the FIRST entry (holding the external pid) matches.
        let pair_ext_e = ctx.tuple(&[equal_ext, e]).expect("fits");
        let pair_me_s = ctx.tuple(&[me, s]).expect("fits");
        let entries = ctx.list(&[pair_ext_e, pair_me_s]).expect("fits");
        let found = call(&reg, &mut ctx, "lists", "keyfind", 3, &[me, one, entries]);
        assert_eq!(found, pair_ext_e, "keyfind/3 finds the equal external pid");

        let single = ctx.list(&[pair_ext_e]).expect("fits");
        let member = call(&reg, &mut ctx, "lists", "keymember", 3, &[me, one, single]);
        assert!(truth(&atoms, member), "keymember/3 on the equal pair");
        let searched = call(&reg, &mut ctx, "lists", "keysearch", 3, &[me, one, single]);
        let value = ctx.atom("value").expect("seeded");
        let expected = ctx.tuple(&[value, pair_ext_e]).expect("fits");
        // `Eterm`'s `PartialEq` is raw-word identity and these are two distinct tuple
        // blocks, so the structural `=:=` pinned by `term_eq` is the right predicate.
        assert!(
            term_eq(searched, expected, &atoms),
            "keysearch/3 ⇒ {{value, Tuple}}"
        );
    }
    {
        // The foreign-node case: nothing matches, so the walk continues to the second
        // entry, and a one-entry list has no match at all.
        let mut dest = heap();
        let mut ctx = BifContext::new(&mut dest, &atoms, self_handle());
        let e = ctx.atom("e").expect("seeded");
        let s = ctx.atom("s").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let foreign_e = ctx.tuple(&[foreign, e]).expect("fits");
        let mine_s = ctx.tuple(&[me, s]).expect("fits");
        let entries = ctx.list(&[foreign_e, mine_s]).expect("fits");
        let found = call(&reg, &mut ctx, "lists", "keyfind", 3, &[me, one, entries]);
        assert_eq!(found, mine_s, "a foreign node is not equal, so the walk continues");
        let single = ctx.list(&[foreign_e]).expect("fits");
        let member = call(&reg, &mut ctx, "lists", "keymember", 3, &[me, one, single]);
        assert!(!truth(&atoms, member), "nothing matches a foreign node");
        let searched = call(&reg, &mut ctx, "lists", "keysearch", 3, &[me, one, single]);
        let false_atom = ctx.atom("false").expect("seeded");
        let is_false = call(&reg, &mut ctx, "erlang", "==", 2, &[searched, false_atom]);
        assert!(truth(&atoms, is_false), "keysearch/3 ⇒ false");
        // `erlang:min/2` on the *equal* pair keeps the first argument (the oracle's
        // `min(A, B) when A > B -> B; min(A, _) -> A`).
        assert_eq!(call(&reg, &mut ctx, "erlang", "min", 2, &[me, mine_s]), me);
    }
}

// ---------------------------------------------------------------------------
// The live oracle cross-check
// ---------------------------------------------------------------------------

/// The `-eval` program the oracle runs.
///
/// It derives `self()`'s number and serial from `erlang:pid_to_list/1` (so both
/// sides build the *same* pid), prints them, then prints one
/// `tag lt gt =:= cmp_term` line per probe — `lt` is `A < B`, `gt` is `B < A`,
/// `cmp_term` is `erts_internal:cmp_term(A, B)`. The external containers are
/// hand-built ETF images (`NEW_PID_EXT` `88`, `NEW_PORT_EXT` `89`,
/// `NEWER_REFERENCE_EXT` `90`), which is exactly how a real external pid/port/ref
/// enters the runtime.
const ORACLE_EVAL: &str = concat!(
    "Self = self(), ",
    "\"<0.\" ++ R0 = erlang:pid_to_list(Self), ",
    "[NumS, SerS0] = string:split(R0, \".\"), ",
    "Ser = list_to_integer(lists:droplast(SerS0)), ",
    "Num = list_to_integer(NumS), ",
    "io:format(\"num ~w~nser ~w~nport ~w~n\", [Num, Ser, 7]), ",
    "Probe = fun(Tag, A, B) -> ",
    "io:format(\"~s ~w ~w ~w ~w~n\", ",
    "[Tag, A < B, B < A, A =:= B, erts_internal:cmp_term(A, B)]) end, ",
    "Pid = fun(Node, Cre) -> NB = list_to_binary(Node), ",
    "binary_to_term(<<131,88,100,(byte_size(NB)):16,NB/binary,",
    "Num:32,Ser:32,Cre:32>>) end, ",
    "Port = fun(Node, N, Cre) -> NB = list_to_binary(Node), ",
    "binary_to_term(<<131,89,100,(byte_size(NB)):16,NB/binary,",
    "N:64,Cre:32>>) end, ",
    "Ref = fun(Node, Id, Cre) -> NB = list_to_binary(Node), ",
    "binary_to_term(<<131,90,1:16,100,(byte_size(NB)):16,NB/binary,",
    "Cre:32,Id:32>>) end, ",
    "Probe(\"pid_zzz\", Self, Pid(\"zzz@host\", 0)), ",
    "Probe(\"pid_aaa\", Self, Pid(\"aaa@host\", 0)), ",
    "Probe(\"pid_creation\", Self, Pid(\"nonode@nohost\", 7)), ",
    "Probe(\"pid_same\", Self, Pid(\"nonode@nohost\", 0)), ",
    "P = erlang:list_to_port(\"#Port<0.7>\"), ",
    "Probe(\"port_zzz\", P, Port(\"zzz@host\", 7, 0)), ",
    "Probe(\"port_aaa\", P, Port(\"aaa@host\", 7, 0)), ",
    "R = erlang:list_to_ref(\"#Ref<0.0.0.9>\"), ",
    "Probe(\"ref_zzz\", R, Ref(\"zzz@host\", 9, 0)), ",
    "Probe(\"ref_same\", R, Ref(\"nonode@nohost\", 9, 0)), ",
    "ok",
);

/// `$ERTS_ORACLE` when it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` and the sibling integration files use.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        return is_executable(&program).then_some(program);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("erl"))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The oracle's transcript: `num`/`ser`/`port`, then one probe line each.
///
/// # Panics
///
/// When no oracle is available or it fails — a missing oracle is **not** a pass
/// (the crate's fail-loud convention for live-oracle integration files).
fn oracle_lines() -> Vec<String> {
    let Some(oracle) = resolve_oracle() else {
        panic!(
            "no system `erl` oracle found ($ERTS_ORACLE unset or not executable, and no \
             `erl` on $PATH): this file's live cross-check cannot run. Install Erlang/OTP \
             or set $ERTS_ORACLE."
        );
    };
    let output = Command::new(&oracle)
        .args(["-noshell", "-eval", ORACLE_EVAL, "-s", "init", "stop"])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{ORACLE_EVAL}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The `TAG lt gt =:= cmp_term` line this crate's BIFs produce for the pair `(a, b)`.
fn probe_line(reg: &BifRegistry, ctx: &mut BifContext<'_>, tag: &str, a: Eterm, b: Eterm) -> String {
    let less = lt(reg, ctx, a, b);
    let greater = lt(reg, ctx, b, a);
    let equal = eq_exact(reg, ctx, a, b);
    let cmp = cmp_term(reg, ctx, a, b);
    format!("{tag} {less} {greater} {equal} {cmp}")
}

/// The value after the leading label of a `label value` line.
fn field(line: &str, label: &str) -> String {
    let mut parts = line.split_whitespace();
    let head = parts.next().unwrap_or_else(|| panic!("empty line, expected {label:?}"));
    assert_eq!(head, label, "unexpected label in {line:?}");
    parts
        .next()
        .unwrap_or_else(|| panic!("no value in {line:?}"))
        .to_string()
}

/// The live cross-check: the oracle's probe transcript and this crate's must be
/// **identical** — the same `self()`, the same hand-built external containers, the
/// same `<`, `=:=` and `erts_internal:cmp_term/2` answers.
///
/// The transcript pins, in order: the node name deciding (both directions), the
/// creation deciding, the "same node ⇒ equal" case, the mixed port pair (node only —
/// the port numbers are never compared) and both mixed reference cases.
#[test]
fn comparison_bifs_match_the_live_oracle() {
    let theirs = oracle_lines();
    assert_eq!(
        theirs.len(),
        3 + 8,
        "the oracle printed an unexpected number of lines: {theirs:?}"
    );

    // The oracle's own `self()` fields and port number — the values both sides use.
    let num: u32 = field(&theirs[0], "num").parse().expect("a pid number");
    let ser: u32 = field(&theirs[1], "ser").parse().expect("a pid serial");
    let port: u64 = field(&theirs[2], "port").parse().expect("a port number");

    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut arena = heap();
    let mut dest = heap();
    let me = ProcessId::new(u64::from(num), u64::from(ser)).to_eterm();
    let pid_zzz = external_pid(&mut arena, &atoms, "zzz@host", num, ser, 0);
    let pid_aaa = external_pid(&mut arena, &atoms, "aaa@host", num, ser, 0);
    let pid_creation = external_pid(&mut arena, &atoms, "nonode@nohost", num, ser, 7);
    let pid_same = external_pid(&mut arena, &atoms, "nonode@nohost", num, ser, 0);
    let my_port = PortId::new(port).to_eterm();
    let port_zzz = external_port(&mut arena, &atoms, "zzz@host", port, 0);
    let port_aaa = external_port(&mut arena, &atoms, "aaa@host", port, 0);
    let my_ref = internal_ref(&mut arena, REF_ID);
    let ref_zzz = external_ref(&mut arena, &atoms, "zzz@host", &[REF_ID as u32], 0);
    let ref_same = external_ref(&mut arena, &atoms, "nonode@nohost", &[REF_ID as u32], 0);
    let mut ctx = BifContext::new(&mut dest, &atoms, ProcessId::new(u64::from(num), u64::from(ser)));

    let ours: Vec<String> = [
        ("pid_zzz", me, pid_zzz),
        ("pid_aaa", me, pid_aaa),
        ("pid_creation", me, pid_creation),
        ("pid_same", me, pid_same),
        ("port_zzz", my_port, port_zzz),
        ("port_aaa", my_port, port_aaa),
        ("ref_zzz", my_ref, ref_zzz),
        ("ref_same", my_ref, ref_same),
    ]
    .into_iter()
    .map(|(tag, a, b)| probe_line(&reg, &mut ctx, tag, a, b))
    .collect();

    let their_probes = &theirs[3..];
    assert_eq!(
        their_probes.len(),
        ours.len(),
        "the oracle printed {} probe lines, this crate {}\n--- oracle ---\n{}\n--- rust ---\n{}",
        their_probes.len(),
        ours.len(),
        their_probes.join("\n"),
        ours.join("\n"),
    );
    for (theirs_line, ours_line) in their_probes.iter().zip(ours.iter()) {
        assert_eq!(theirs_line.as_str(), ours_line.as_str(), "probe transcript line");
    }

    // Spot-pin the load-bearing oracle values, so a regression is readable even
    // though the whole transcript was compared above (OTP 29.1.1 / erts-17.1; the
    // three trailing fields are `lt`, `gt`, `=:=`, then `cmp_term`).
    assert_eq!(theirs[3], "pid_zzz true false false -1", "nonode@nohost < zzz@host");
    assert_eq!(theirs[4], "pid_aaa false true false 1", "aaa@host < nonode@nohost");
    assert_eq!(theirs[5], "pid_creation true false false -1", "creation 0 < 7");
    assert_eq!(theirs[6], "pid_same false false true 0", "same node ⇒ =:=");
    assert_eq!(theirs[7], "port_zzz true false false -1", "the port's node decides");
    assert_eq!(theirs[8], "port_aaa false true false 1");
    assert_eq!(theirs[9], "ref_zzz true false false -1", "the ref's node decides first");
    assert_eq!(theirs[10], "ref_same false false true 0", "same node + same id ⇒ =:=");
}
