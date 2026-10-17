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

use std::rc::Rc;

use beam_bifs::bif_tab::BifTab;
use beam_bifs::{AtomInterner, BifFn, BifRegistry, BytesSink, LocalFunIdentity, LocalNode};
use beam_engine::RunOutcome;
use beam_loader::Operand;
use beam_term::{atom_val, is_small, make_atom, make_small, signed_value, Atom, AtomTable, NIL};

use super::*;
use crate::config::RuntimeConfig;
use crate::context::test_support::{
    compile_fixtures, find_on_path, resolve_erlc, skip_loudly,
};
use crate::native::test_support::remote_pid;
use crate::runtime::GcStatsSnapshot;
fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("the default runtime")
}

#[test]
fn dispatches_a_registered_erlang_bif() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    let sum = ctx
        .call_bif("erlang", "+", 2, &[make_small(2), make_small(3)])
        .expect("2 + 3");
    assert!(is_small(sum));
    assert_eq!(signed_value(sum), 5);
    assert_eq!(
        signed_value(
            ctx.call_bif("erlang", "*", 2, &[make_small(6), make_small(7)])
                .expect("6 * 7")
        ),
        42
    );
}

#[test]
fn a_bif_builds_an_atom_from_the_shared_table() {
    let mut rt = runtime();
    let a = rt.intern("a");
    let mut ctx = RuntimeContext::new(&mut rt);
    let is_atom = ctx
        .call_bif("erlang", "is_atom", 1, &[a])
        .expect("is_atom(a)");
    // `ctx` now owns a shared-sink handle (and therefore a destructor), so the runtime
    // is reached through the context that borrows it rather than the outer binding.
    assert_eq!(
        ctx.runtime.atoms().name(atom_val(is_atom)),
        Some("true"),
        "the atom the BIF built resolves through the shared table"
    );
}

/// **The `atom_name` seam resolves through the runtime's LIVE namespace, not the running
/// module's load-time snapshot** — the engine-side half of the load-time atom-snapshot gap.
///
/// [`Module::atom_name`](beam_loader::Module::atom_name) indexes only the module's **owned
/// load-time prefix clone** of the shared table (`CodeServer::load_bytes` hands
/// `lower::build_module` an `atoms().clone()`), so a module loaded *before* a name was interned
/// cannot name that atom — its id lies beyond the snapshot. The engine resolves a runtime atom
/// value (the `Mod` in `Mod:init(Args)` / `apply(M,F,A)`) through
/// [`ExecutionContext::atom_name`], and on the boot path `gen_server:init_it/6` names
/// `supervisor_bridge` — interned by a later boot-script directive — so the trait default
/// answered `None` and the call raised `badarg`.
///
/// This case interns a fresh name **after** loading a fixture and asserts both halves: the
/// module's own table still cannot name it (the premise), while the context's seam does (the
/// override). Removing the override makes the second assertion fail.
#[test]
fn the_atom_name_seam_names_an_atom_interned_after_the_module_loaded() {
    let mut rt = runtime();
    rt.load_module(GC_FLOATS_BEAM)
        .expect("the fixture loads into the code server");
    let module = rt
        .code_server()
        .module_handle("gc_floats")
        .expect("the fixture is cached under its own name");

    // Interned through the shared table, so its id is exactly one past the module's snapshot.
    let name = "an_atom_interned_after_gc_floats_loaded";
    let atom = rt.code_server_atoms_mut().intern(name);
    assert!(
        module.atom_name(atom).is_none(),
        "the name is beyond the module's load-time snapshot (the premise of the boot blocker)"
    );

    let ctx = RuntimeContext::new(&mut rt);
    assert_eq!(
        ExecutionContext::atom_name(&ctx, &module, atom).as_deref(),
        Some(name),
        "the seam resolves the runtime atom through the live shared namespace"
    );
}

#[test]
fn the_pending_interner_predicts_append_only_indices_and_dedups() {
    let mut interner = PendingInterner::new(10);
    assert_eq!(interner.intern("foo").raw(), 10);
    assert_eq!(interner.intern("bar").raw(), 11);
    // A duplicate within the same call reuses its predicted index.
    assert_eq!(interner.intern("foo").raw(), 10);
    assert_eq!(interner.intern("baz").raw(), 12);

    // Committing lands each recorded name at exactly the index it predicted. The table
    // must already hold `base` names (`AtomTable` assigns indices in insertion order).
    let mut atoms = AtomTable::new();
    for i in 0..10 {
        atoms.intern(&format!("pad{i}"));
    }
    assert_eq!(atoms.len(), 10);
    interner.commit(&mut atoms);
    assert_eq!(atoms.name(Atom::from_raw(10)), Some("foo"));
    assert_eq!(atoms.name(Atom::from_raw(11)), Some("bar"));
    assert_eq!(atoms.name(Atom::from_raw(12)), Some("baz"));
    assert_eq!(atoms.len(), 13);
}

#[test]
fn the_pending_interner_reports_emptiness_so_the_commit_can_be_skipped() {
    // `is_empty` is the guard `call_registered`/`try_call_nif` use to skip the
    // `Rc::make_mut` commit (`atoms_mut`) on a call that interned nothing -- otherwise the
    // whole shared `AtomTable` is deep-cloned on every registered BIF call. It must be
    // true before any name is recorded and false once one is.
    let mut interner = PendingInterner::new(0);
    assert!(interner.is_empty(), "a fresh interner has recorded nothing");
    interner.intern("first");
    assert!(!interner.is_empty(), "a recorded name is not empty");
    let mut atoms = AtomTable::new();
    interner.commit(&mut atoms);
    assert!(
        interner.is_empty(),
        "commit drains the recorded names, so the interner is empty again"
    );
}

#[test]
fn a_bif_interns_a_new_atom_into_the_shared_table() {
    let mut rt = runtime();
    let name = "a_fresh_atom_zzz";
    assert!(rt.atoms().lookup(name).is_none(), "the name starts unknown");
    let base = rt.atoms().len();
    let mut ctx = RuntimeContext::new(&mut rt);
    let image = terms::make_heap_binary(&mut ctx, name.as_bytes()).expect("a binary");
    let atom = ctx
        .call_bif("erlang", "binary_to_atom", 1, &[image])
        .expect("binary_to_atom/1 interns a new name");
    // The atom the BIF built resolves through the one shared table: the name was
    // committed after the call, at exactly the index the interner predicted.
    // (`ctx` holds a shared-sink handle with a destructor, so the runtime is reached
    // through the context that borrows it.)
    assert_eq!(ctx.runtime.atoms().name(atom_val(atom)), Some(name));
    assert_eq!(ctx.runtime.atoms().len(), base + 1);
}

#[test]
fn an_unknown_erlang_bif_raises_undef_not_a_panic() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    // `erlang` is the BIF namespace, never a loadable module: a name the release does
    // not have raises the oracle's bare `error:undef`, never a fabricated value.
    let error = ctx
        .call_bif("erlang", "nope", 0, &[])
        .expect_err("not a BIF");
    assert_eq!(error.as_atom(), Some("undef"), "{error}");
    assert!(ctx
        .call_bif("erlang", "+", 2, &[make_small(1)])
        .is_err(), "wrong arity");
}

#[test]
fn io_format_writes_to_the_installed_sink() {
    let mut rt = runtime();
    let format: Vec<Eterm> = "hi ~w\n".bytes().map(|b| make_small(i64::from(b))).collect();
    let mut sink = BytesSink::new();
    let result = {
        let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
        let format = terms::make_list(&mut ctx, &format).expect("a format list");
        let args = terms::make_list(&mut ctx, &[make_small(7)]).expect("an argument list");
        ctx.call_bif("io", "format", 2, &[format, args])
            .expect("io:format/2")
    };
    assert_eq!(rt.atoms().name(atom_val(result)), Some("ok"));
    assert_eq!(sink.as_str(), "hi 7\n");
}

#[test]
fn ets_inside_an_applied_fun_reaches_the_outer_table() {
    let mut rt = runtime();
    // `lists:map/2` applies its fun argument through `FunBridge`; the `ets:*` calls
    // inside that fun must reach the runtime's shared tables. Before `call_registered`
    // stopped holding the ETS cell for a re-entrancy-capable call, the nested
    // `ets:insert` raised `unsupported_bif` (the cell was already borrowed by the
    // outer `lists:map/2` call), never a wrong result — but never a right one either.
    let term = rt
        .eval(
            "begin T = ets:new(t, [set]), \
             lists:map(fun(X) -> ets:insert(T, {X, X*X}) end, [1,2,3]), \
             ets:lookup(T, 2) end",
        )
        .expect("ets:* inside a fun applied by lists:map/2");
    let printed = rt.format_term(term, None).expect("~p renders the lookup");
    assert_eq!(printed.trim_end(), "[{2,4}]");
}

#[test]
fn a_nested_io_format_reaches_the_callers_sink() {
    let mut rt = runtime();
    let mut sink = BytesSink::new();
    {
        let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
        // The `io:format/2` runs inside the fun `lists:map/2` applies; it must write to
        // the caller's sink (shared through `SharedSink`), not to stdout.
        crate::eval::eval(
            &mut ctx,
            "lists:map(fun(X) -> io:format(\"~w \", [X]) end, [1,2,3])",
        )
        .expect("io:format inside a fun applied by lists:map/2");
    }
    assert_eq!(sink.as_str(), "1 2 3 ");
}

#[test]
fn a_nested_call_without_a_fun_still_sees_the_outer_tables() {
    let mut rt = runtime();
    // The mirror case: a *non*-re-entrancy-capable BIF (`ets:new/2`, `ets:insert/2` here)
    // is given the shared tables, and a fun it does not apply cannot observe them.
    let term = rt
        .eval("begin T = ets:new(t, [set]), ets:insert(T, {1, 11}), ets:lookup(T, 1) end")
        .expect("a plain ets:* sequence");
    let printed = rt.format_term(term, None).expect("~p renders the lookup");
    assert_eq!(printed.trim_end(), "[{1,11}]");
}

#[test]
fn ets_foldl_reaches_the_shared_tables_with_a_fun_argument() {
    let mut rt = runtime();
    // `ets:foldl/3`/`ets:foldr/3` are the only fun-taking `ets:*` BIFs (`beam_bifs`'
    // `FUN_CALLER_BIFS` intersected with the `ets` namespace): they resolve the table out of
    // the shared `TableRegistry`, so `call_registered` must install the cell even though the
    // call is re-entrancy-capable. Before that, these calls raised `unsupported: bif`.
    //
    // An `ordered_set` is used so the traversal order is exact (ascending keys) and the
    // comparison is deterministic; a hash `set`'s iteration order is a documented *interior*
    // freedom of `beam_ets`, so a fold over one is order-insensitive by construction.
    let term = rt
        .eval(
            "begin T = ets:new(t, [ordered_set]), \
             [ets:insert(T, {X, X}) || X <- [1,2,3,4]], \
             ets:foldl(fun(X, A) -> element(2, X) + A end, 0, T) end",
        )
        .expect("ets:foldl/3 applies its fun and folds the table left-to-right");
    assert_eq!(signed_value(term), 10, "1+2+3+4");
}

#[test]
fn ets_foldr_reaches_the_shared_tables_with_a_fun_argument() {
    let mut rt = runtime();
    // As above, but the right-to-left direction: an `ordered_set` is walked keys-descending,
    // so prepending each object yields the ascending list — the oracle's own answer.
    let term = rt
        .eval(
            "begin T = ets:new(t, [ordered_set]), \
             [ets:insert(T, {X, X}) || X <- [1,2,3,4]], \
             ets:foldr(fun(X, A) -> [element(2, X) | A] end, [], T) end",
        )
        .expect("ets:foldr/3 applies its fun and folds the table right-to-left");
    let printed = rt
        .format_term(term, None)
        .expect("~p renders the folded list");
    assert_eq!(printed.trim_end(), "[1,2,3,4]");
}

#[test]
fn ets_fold_over_an_empty_table_answers_the_accumulator_without_calling_the_fun() {
    let mut rt = runtime();
    // The oracle's `do_foldl`/`do_foldr` answer `Accu` for `'$end_of_table'` without ever
    // applying the fun, so a completely unvalidated (0-arity) fun is accepted on an empty
    // table — the probe is that this must not raise `badarity`.
    let term = rt
        .eval(
            "begin T = ets:new(t, [set]), \
             {ets:foldl(fun() -> never end, acc0, T), \
              ets:foldr(fun() -> never end, acc1, T)} end",
        )
        .expect("an empty fold returns the accumulator and never calls the fun");
    let printed = rt
        .format_term(term, None)
        .expect("~p renders the two accumulators");
    assert_eq!(printed.trim_end(), "{acc0,acc1}");
}

#[test]
fn ets_inside_a_lists_map_fun_still_reaches_the_shared_tables() {
    let mut rt = runtime();
    // The re-entrancy guard is unweakened: `lists:map/2` reads no `TableRegistry`, so it
    // installs no ETS cell and the `ets:lookup/2` its fun performs borrows the free cell and
    // resolves. A blanket "install the cell for every re-entrant call" would break this.
    let term = rt
        .eval(
            "begin T = ets:new(t, [set]), ets:insert(T, {k, 1}), \
             lists:map(fun(X) -> ets:lookup(T, X) end, [k]) end",
        )
        .expect("ets:lookup inside a fun applied by lists:map/2");
    let printed = rt.format_term(term, None).expect("~p renders the lookup");
    assert_eq!(printed.trim_end(), "[[{k,1}]]");
}

#[test]
fn tables_capable_names_exactly_the_table_registry_seam_consumers() {
    // The two fun-folds: the only reentrant keys on the set, and the reason the cell is now
    // installed for a reentrant call at all.
    assert!(RuntimeContext::tables_capable("ets", "foldl", 3));
    assert!(RuntimeContext::tables_capable("ets", "foldr", 3));
    // The whole `ets` namespace resolves its handle through `ctx.tables()`.
    assert!(RuntimeContext::tables_capable("ets", "new", 2));
    assert!(RuntimeContext::tables_capable("ets", "lookup", 2));
    // The two `erts_internal` raw-walk aliases delegate into the `ets` family.
    assert!(RuntimeContext::tables_capable("erts_internal", "ets_raw_first", 1));
    assert!(RuntimeContext::tables_capable("erts_internal", "ets_raw_next", 2));
    // A fun-applying BIF that does NOT read the seam must not be gated on it — that is what
    // keeps `lists:map(fun(X) -> ets:lookup(T, X) end, L)` working.
    assert!(!RuntimeContext::tables_capable("lists", "map", 2));
    assert!(!RuntimeContext::tables_capable("maps", "map", 2));
    assert!(!RuntimeContext::tables_capable("lists", "foldl", 3));
    // Near misses on the `erts_internal` aliases fall back to the `ets`-namespace test.
    assert!(!RuntimeContext::tables_capable("erts_internal", "ets_raw_first", 2));
    assert!(!RuntimeContext::tables_capable("erts_internal", "ets_raw_next", 1));
}

/// **The nested-`ets:*`-inside-a-fold residual is closed**: the fold holds the shared
/// `TableRegistry` cell for its whole duration, so it **lends** the one live `&mut` through
/// `BifContext::call_fun_with_tables` and the nested `ets:lookup/2` resolves.
///
/// Oracle (`erl -noshell -eval '…' -s init stop`): the same expression prints `10`, exit 0.
/// Before the lend, the nested lookup failed its `try_borrow_mut` and raised
/// `unsupported: bif` (exit 3) — a typed capability gap, never a wrong result.
#[test]
fn ets_lookup_inside_an_ets_foldl_fun_reaches_the_folded_table() {
    let mut rt = runtime();
    let term = rt
        .eval(
            "begin T = ets:new(t, [ordered_set]), \
             [ets:insert(T, {X, X}) || X <- [1,2,3,4]], \
             ets:foldl(fun(X, A) -> \
                 A + element(2, hd(ets:lookup(T, element(1, X)))) end, 0, T) end",
        )
        .expect("an ets:lookup/2 inside the fun ets:foldl/3 applies");
    assert_eq!(signed_value(term), 10, "1+2+3+4, each read back from T");
}

#[test]
fn ets_lookup_inside_an_ets_foldr_fun_reaches_the_folded_table() {
    let mut rt = runtime();
    // The right-to-left twin: the accumulated list is built from the nested lookups.
    // Oracle (`erl -noshell -eval '…' -s init stop`, the identical expression): prints
    // `[1,2,3,4]`, exit 0.
    let term = rt
        .eval(
            "begin T = ets:new(t, [ordered_set]), \
             [ets:insert(T, {X, X}) || X <- [1,2,3,4]], \
             ets:foldr(fun(X, A) -> \
                 [element(2, hd(ets:lookup(T, element(1, X)))) | A] end, [], T) end",
        )
        .expect("an ets:lookup/2 inside the fun ets:foldr/3 applies");
    let printed = rt.format_term(term, None).expect("~p renders the folded list");
    assert_eq!(printed.trim_end(), "[1,2,3,4]");
}

/// **A lent registry is installed for *every* nested call, not only a reentrant one**, which is
/// what carries it through an intermediate that installs no cell of its own: the `lists:map/2`
/// inside a fold's fun (deliberately *not* on [`RuntimeContext::tables_capable`]) applies a
/// second fun whose `ets:lookup/2` must still resolve.
///
/// The innermost lookup is keyed on `element(1, X)`: `ets:foldl/3` hands its fun the stored
/// **object** (the whole tuple), and looking a tuple up as a key finds nothing, so `hd([])`
/// would raise `badarg` on **this** runtime and on the oracle alike.
///
/// Oracle (`erl -noshell -eval '…' -s init stop`, the identical expression): prints `100`,
/// exit 0 — so the shape crosses the `-eval` evaluator's fun-inside-a-fun boundary on both
/// runtimes and the assertion below is a real oracle cross-check.
#[test]
fn ets_lookup_survives_a_fold_fun_through_a_lists_map_fun() {
    let mut rt = runtime();
    let term = rt
        .eval(
            "begin T = ets:new(t, [ordered_set]), \
             [ets:insert(T, {X, X * 10}) || X <- [1,2,3,4]], \
             ets:foldl(fun(X, A) -> \
                 A + lists:sum(lists:map(fun(Y) -> \
                     element(2, hd(ets:lookup(T, Y))) end, [element(1, X)])) end, 0, T) end",
        )
        .expect("ets:lookup through a fold fun and a lists:map fun");
    assert_eq!(signed_value(term), 100, "(1+2+3+4)*10");
}

#[test]
fn the_arena_is_unbounded_in_milestone_one() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    assert_eq!(ctx.heap_available(), usize::MAX);
    let mut x = [make_small(0)];
    let mut stack = [make_small(0)];
    assert!(ctx.ensure_heap(u32::MAX, 1, &mut x, &mut stack).is_ok());
    assert!(!ctx.alloc_words(4).expect("arena allocates").is_null());
}

#[test]
fn the_registry_is_the_core_bif_set() {
    let rt = runtime();
    let core = BifRegistry::with_core_bifs();
    for (module, function, arity) in [
        ("erlang", "+", 2),
        ("lists", "reverse", 1),
        ("io", "format", 2),
    ] {
        assert!(rt.registry.contains(module, function, arity));
        // Value-for-value agreement with a freshly built core registry proves this
        // is the real `beam_bifs` one, not a hand-rolled subset.
        assert_eq!(
            rt.registry.lookup(module, function, arity).is_some(),
            core.lookup(module, function, arity).is_some()
        );
    }
    assert!(!rt.registry.contains("erlang", "nope", 0));
    assert!(!core.contains("erlang", "nope", 0));
}

// -----------------------------------------------------------------------
// the dispatch memo (`RuntimeContext::dispatch`)
// -----------------------------------------------------------------------
//
// `call_bif` answers a registered call out of `RuntimeContext::dispatch`'s memo instead of
// re-running the dispatch prologue (`registry.lookup` + the whole `uses_*` hook roster) on
// every call. A memo **hit** must therefore be identical to what the uncached prologue would
// have produced: the memo may only skip *work*, never change an answer. These cases pin that
// against the uncached oracle below — on a registered BIF, a hook-gated BIF of every arm, a
// name no registry owns, and a pair of keys that share a memo slot while carrying *different*
// answers (the eviction case, where a hit decided by anything weaker than the full key would
// return the wrong answer).
//
// Why a cached answer can never go stale (the soundness argument, also stated on
// `super::DispatchMemo`): every input the answer is built from is immutable for the life of
// the runtime. `beam_bifs`' `needs_*` rosters are `&'static` tables (the predicates are pure
// `binary_search` membership tests), and `Runtime::registry` is an `Rc<BifRegistry>` built once
// at boot — `BifRegistry::register` takes `&mut self`, is boot-time only, and nothing in this
// crate ever calls `Rc::make_mut`/`Rc::get_mut` on it. The one **runtime-mutable** dispatch
// input, the NIF registry that `erlang:load_nif/2` grows, is deliberately **not** memoized:
// `RuntimeContext::call_bif` still runs `try_call_nif` first on every call, so a library loaded
// mid-slice wins immediately (`../../tests/nif.rs` loads a real `.so` and pins that precedence).
// The memo lives on the context, so it is exactly as long-lived as one run (`call_bif`);
// a fresh context starts empty and recomputes on first use.

/// Keys spanning the answer space the memo has to keep apart: registered arithmetic, one
/// registered key for each hook-gated arm, a registered `RuntimeHooks` key, long-standing
/// stdlib/ETS registrations and names no registry owns.
///
/// Longer than [`DISPATCH_MEMO_SLOTS`], so a full pass over them evicts continuously — the
/// churn case below is not vacuous — and it contains at least one same-slot pair whose two
/// answers differ (asserted, not assumed).
const DISPATCH_PROBE_KEYS: &[(&str, &str, u32)] = &[
    ("erlang", "+", 2),
    ("erlang", "*", 2),
    ("erlang", "put", 2),
    ("erlang", "whereis", 1),
    ("erlang", "load_nif", 2),
    ("records", "get_definition", 2),
    ("erlang", "send", 2),
    ("erlang", "get_module_info", 1),
    ("persistent_term", "get", 1),
    ("erlang", "start_timer", 3),
    ("erlang", "term_to_binary", 1),
    ("erlang", "memory", 0),
    ("lists", "reverse", 1),
    ("ets", "new", 2),
    ("erlang", "nope", 0),
    ("nosuchmod", "nope", 3),
];

/// The answer the **uncached** dispatch prologue produces for `key`: the `beam_bifs` registry
/// implementation plus one fresh evaluation of every `uses_*` hook predicate — exactly the two
/// inputs [`RuntimeContext::dispatch`] memoizes, but recomputed from scratch here.
fn uncached_dispatch(rt: &Runtime, key: (&str, &str, u32)) -> (Option<BifFn>, DispatchHooks) {
    let (module, function, arity) = key;
    (
        rt.registry.lookup(module, function, arity),
        DispatchHooks::of(module, function, arity),
    )
}

/// A dispatch answer as a comparable value: `(is-registered, BIF address, hook roster)`.
///
/// The `BifFn` itself is not comparable (`fn` pointers have neither `Debug` nor `PartialEq`),
/// so the implementation is compared by address — two lookups of the same key necessarily
/// yield the same address.
type AnswerShape = (bool, Option<usize>, DispatchHooks);

fn shape(answer: (Option<BifFn>, DispatchHooks)) -> AnswerShape {
    (answer.0.is_some(), answer.0.map(|f| f as usize), answer.1)
}

#[test]
fn a_dispatch_memo_hit_answers_what_the_uncached_prologue_would() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    for &(module, function, arity) in DISPATCH_PROBE_KEYS {
        let expected = shape(uncached_dispatch(ctx.runtime, (module, function, arity)));
        // The first call is a miss: it computes the answer and memoizes it in the slot.
        let miss = ctx.dispatch(module, function, arity);
        assert_eq!(
            shape((miss.bif, miss.hooks)),
            expected,
            "the memo miss for {module}:{function}/{arity} must equal the uncached prologue"
        );
        let entry = ctx.dispatch_memo.slots[dispatch_slot(module, function, arity)]
            .as_ref()
            .expect("the miss memoized its answer");
        assert_eq!(
            (entry.module.as_str(), entry.function.as_str(), entry.arity),
            (module, function, arity),
            "the memoized entry is tagged with the key it answers"
        );
        // Every later call is a hit (until a collision evicts it): same answer, always.
        for _ in 0..3 {
            let hit = ctx.dispatch(module, function, arity);
            assert_eq!(
                shape((hit.bif, hit.hooks)),
                expected,
                "a memo hit for {module}:{function}/{arity} must equal the uncached prologue"
            );
        }
    }
}

#[test]
fn a_memoized_hook_roster_mirrors_the_beam_bifs_predicates_field_for_field() {
    // A hit returns the *memoized* roster, so a mis-mapped field would answer a different arm
    // than `beam_bifs` routes the key through — a wrong seam (and so a wrong result) rather
    // than a slow one. Compare the memoized roster to the raw predicates (not to
    // `DispatchHooks::of`, which built it) so a mapping bug cannot hide behind itself.
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    let mut any_claimed = false;
    let mut any_unclaimed = false;
    for &(module, function, arity) in DISPATCH_PROBE_KEYS {
        let hooks = ctx.dispatch(module, function, arity).hooks;
        assert_eq!(
            hooks.dict,
            beam_bifs::needs_dict_hook(module, function, arity),
            "{module}:{function}/{arity} dict arm"
        );
        assert_eq!(
            hooks.process_hook,
            beam_bifs::needs_process_hook(module, function, arity),
            "{module}:{function}/{arity} process arm"
        );
        assert_eq!(
            hooks.nif_loader,
            beam_bifs::needs_nif_loader(module, function, arity),
            "{module}:{function}/{arity} NIF-loader arm"
        );
        assert_eq!(
            hooks.record_source,
            beam_bifs::needs_record_source(module, function, arity),
            "{module}:{function}/{arity} record-source arm"
        );
        assert_eq!(
            hooks.send_hook,
            beam_bifs::needs_send_hook(module, function, arity),
            "{module}:{function}/{arity} send arm"
        );
        assert_eq!(
            hooks.module_info,
            beam_bifs::needs_module_info(module, function, arity),
            "{module}:{function}/{arity} module-info arm"
        );
        assert_eq!(
            hooks.persistent_term,
            beam_bifs::needs_persistent_term(module, function, arity),
            "{module}:{function}/{arity} persistent-term arm"
        );
        assert_eq!(
            hooks.runtime_hooks,
            beam_bifs::needs_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity} runtime-hooks arm"
        );
        // No probe key is `erlang:fun_info/1,2` — the gate's introspection half — so the
        // field-for-field comparison against the encoders' roster holds for this set (see
        // `the_fun_identity_gate_selects_the_image_and_introspection_bifs` for the
        // divergence the roster itself does not carry).
        assert_eq!(
            hooks.fun_identity,
            beam_bifs::needs_local_fun_identity(module, function, arity),
            "{module}:{function}/{arity} local-fun-identity gate"
        );
        assert_eq!(
            hooks.memory_hook,
            beam_bifs::needs_memory_hook(module, function, arity),
            "{module}:{function}/{arity} memory-hook gate"
        );
        any_claimed |= hooks != DispatchHooks::default();
        any_unclaimed |= hooks == DispatchHooks::default();
    }
    // A probe set that claimed no arm (or claimed every key) would make the mirror above
    // vacuous, so both sides of the mapping must occur.
    assert!(any_claimed, "the probe set must contain a hook-gated key");
    assert!(any_unclaimed, "the probe set must contain an ungated key");
}

#[test]
fn a_dispatch_memo_slot_collision_cannot_change_an_answer() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    // Every unordered pair of probe keys that share a memo slot while carrying **different**
    // answers. These are exactly the pairs an eviction could corrupt: with a 8-slot
    // direct-mapped memo the second key overwrites the first, so the first's next call has to
    // recompute — and a hit decided by anything weaker than the whole key (the slot, the
    // arity, a module-only compare) would return the evicted key's answer instead.
    let mut colliding: Vec<((&str, &str, u32), (&str, &str, u32))> = Vec::new();
    for (i, a) in DISPATCH_PROBE_KEYS.iter().enumerate() {
        for b in &DISPATCH_PROBE_KEYS[i + 1..] {
            if dispatch_slot(a.0, a.1, a.2) != dispatch_slot(b.0, b.1, b.2) {
                continue;
            }
            if shape(uncached_dispatch(ctx.runtime, *a)) != shape(uncached_dispatch(ctx.runtime, *b))
            {
                colliding.push((*a, *b));
            }
        }
    }
    // A concrete, stable instance of the situation, so the guard below can never pass
    // vacuously: `erlang:send/2` is registered and needs the delivery hook, while a name no
    // registry owns shares its slot carrying a completely different answer.
    assert_eq!(dispatch_slot("erlang", "send", 2), dispatch_slot("nosuchmod", "nope", 3));
    assert!(
        !colliding.is_empty(),
        "the probe set must contain a slot collision between two different answers"
    );
    for (a, b) in colliding {
        // Interleave the two keys: `b` evicts `a`, so the third call is a recompute. Every
        // answer must still equal the uncached prologue — an eviction costs work, never a
        // result.
        for (key, label) in [(a, "first"), (b, "colliding"), (a, "recomputed")] {
            let got = ctx.dispatch(key.0, key.1, key.2);
            assert_eq!(
                shape((got.bif, got.hooks)),
                shape(uncached_dispatch(ctx.runtime, key)),
                "the {label} answer for {}/{}/{} (slot {}) changed across a collision",
                key.0,
                key.1,
                key.2,
                dispatch_slot(key.0, key.1, key.2),
            );
        }
    }
}

#[test]
fn the_dispatch_memo_stays_correct_under_slot_churn() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    let expected: Vec<AnswerShape> = DISPATCH_PROBE_KEYS
        .iter()
        .map(|&key| shape(uncached_dispatch(ctx.runtime, key)))
        .collect();
    // The probe set is deliberately larger than the memo, so one pass over it evicts and
    // recomputes continuously and three passes make every hit/miss interleaving occur.
    assert!(
        DISPATCH_PROBE_KEYS.len() > DISPATCH_MEMO_SLOTS,
        "the probe set must not fit the memo, or no churn happens"
    );
    for pass in 0..3 {
        for (key, want) in DISPATCH_PROBE_KEYS.iter().zip(&expected) {
            let got = ctx.dispatch(key.0, key.1, key.2);
            assert_eq!(
                shape((got.bif, got.hooks)),
                *want,
                "pass {pass}: {}/{} answer changed under slot churn",
                key.0,
                key.1
            );
        }
    }
}

// -----------------------------------------------------------------------
// the receive/mailbox seam over the running process's real mailbox
// -----------------------------------------------------------------------

/// The committed mailbox fixture (regeneration recipe in `tests/fixtures/README.md`).
const MAILBOX_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/mailbox.beam"
);

/// The fixture loaded as an owned image, so it outlives the borrowing context
/// (the receive-seam cases need `Machine::run_export_outcome`, which borrows both).
fn mailbox_fixture() -> Module {
    beam_loader::load_file(MAILBOX_BEAM).expect("load mailbox.beam")
}

/// The fixture's pid term (the destination `self()` reports).
fn self_term(rt: &Runtime) -> Eterm {
    rt.self_pid().to_eterm()
}

#[test]
fn send_message_resolves_the_target_and_rejects_a_non_pid() {
    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut ctx = RuntimeContext::new(&mut rt);

    // A self-delivery is the `Enqueued` case (nothing is blocked).
    ctx.send_message(pid, make_small(7)).expect("deliver to self");

    // A non-pid destination is the typed `BadArg` — never a wrong delivery.
    assert!(matches!(
        ctx.send_message(make_small(7), make_small(7)),
        Err(BeamError::BadArg)
    ));

    // The message is in the running process's mailbox.
    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox.peek().expect("the delivered message").payload(), make_small(7));
}

#[test]
fn send_message_to_an_unknown_pid_is_the_tables_typed_error() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    // A pid whose (number, serial) names no live process: the table's typed not-found.
    let ghost = ProcessId::new(9999, 7).to_eterm();
    let error = ctx
        .send_message(ghost, make_small(1))
        .expect_err("no such process");
    assert!(
        matches!(error, BeamError::TermNotFound),
        "expected the table's typed not-found error, got {error:?}"
    );
}

// -----------------------------------------------------------------------
// Escaped off-heap refc binaries (a nested run's payload)
// -----------------------------------------------------------------------

/// The off-heap payload owner a `beam_codec::term::make_refc_binary` call needs — exactly the
/// role `beam_engine`'s `Machine::refc_binaries` plays for a **nested run**
/// ([`RuntimeContext::run_export_blocking`]/[`RuntimeContext::run_label_blocking`], a boot
/// `{apply, …}` directive, the `{get_until, …}` io-device continuation), whose short-lived
/// machine `beam_rt` drops the moment the run returns.
struct NestedRunPayloads(Vec<beam_alloc::RefcBinary>);

impl beam_codec::OffHeapBinaries for NestedRunPayloads {
    fn alloc_refc_payload(&mut self, bytes: &[u8]) -> Result<*mut Eterm> {
        let handle = beam_alloc::RefcBinary::new(bytes)?;
        let data = handle.data_ptr();
        self.0.push(handle);
        Ok(data.cast::<Eterm>())
    }
}

/// Builds the frozen 8-word `[BinRef][ErlSubBits]` pair over a fresh off-heap payload of 200
/// bytes — the genuine representation `bs_create_bin` produces above
/// [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`]. Returns the source heap (which owns the pair and
/// must outlive the term), the payload owner, its bytes and the term.
fn escaped_off_heap_binary() -> (beam_alloc::Heap, NestedRunPayloads, Vec<u8>, Eterm) {
    let bytes: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    assert!(bytes.len() > beam_term::refc::ERL_ONHEAP_BINARY_LIMIT);
    let mut heap = beam_alloc::Heap::new(16).expect("the source heap");
    let mut payloads = NestedRunPayloads(Vec::new());
    let term =
        beam_codec::term::make_refc_binary(&mut heap, &mut payloads, &bytes, 0, bytes.len() * 8)
            .expect("the source refc-binary term");
    assert_eq!(payloads.0.len(), 1, "one off-heap payload");
    (heap, payloads, bytes, term)
}

/// **The escaped-payload delivery, end to end.** A message carrying an off-heap refc binary whose
/// only owner was a **nested run's** machine — already dropped by the time the message is sent —
/// must still reach the receiver, and the runtime's escaped-payload store must resolve it by
/// **sharing** (not flattening) the payload.
///
/// This is the defect `A = base64:encode(<<…>>), X1 = binary:encode_hex(A), _ =
/// base64:encode(<<…>>), X2 = binary:encode_hex(A)` (`X1 == X2` on the oracle: the second
/// allocation used to reuse the first binary's freed block) and
/// `B = compile:forms(F, [no_spawn_compiler_process]), self() ! B` (`error:badarg`) pin, reproduced
/// without the compiler. The copy engine resolves a refc binary **by its data pointer** through the
/// copy's source table, and a sender holding no handles of its own (`erlang:send/2`/`'!'/2` through
/// the `SendBridge`, and every runtime-internal delivery) passes an empty one.
///
/// The two paths are observably distinct, and **both halves are asserted**:
///
/// * **flattened** (the store is empty) — `beam_message`'s `copy_refc_binary` last resort
///   materializes the window as a fresh **on-heap** bitstring, so the delivery *succeeds*, the
///   store stays empty and the queued message owns **no** off-heap handle;
/// * **shared** (the store holds the adopted handle) — the copy finds the handle, so the queued
///   message is byte-identical and owns its own reference to the **same** payload (the store's
///   handle survives at refcount 2).
#[test]
fn a_delivery_resolves_an_escaped_off_heap_payload_through_the_runtime_store() {
    let (_source_heap, payloads, bytes, term) = escaped_off_heap_binary();

    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut reference_heap = beam_alloc::Heap::new(64).expect("the reference heap");
    let reference =
        beam_codec::term::make_binary(&mut reference_heap, &bytes).expect("the reference");

    // -- FLATTENED: with the escaped-payload store empty the copy has no handle to share, so it
    //    materializes the window as a fresh on-heap binary instead of refusing the message. --
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message(pid, term)
            .expect("without a resolvable handle the delivery flattens rather than failing");
    }
    assert!(
        rt.eval_off_heap.binaries().is_empty(),
        "a flattened delivery registers nothing in the escaped-payload store"
    );
    assert_eq!(
        queued_binary_counts(&rt, rt.self_pid()),
        vec![0],
        "the flattened payload is an ordinary on-heap binary — the message owns no off-heap handle"
    );
    assert!(
        beam_term::compare::eq(queued_payloads(&rt, rt.self_pid())[0], reference, rt.atoms()),
        "the flattened binary reads back byte-identical to the payload"
    );

    // -- SHARED: the nested run returns and its machine is dropped — `beam_rt` adopts the handles
    //    first, so the store can now resolve (and share) the escaped payload. --
    rt.adopt_escaped_binaries(&payloads.0);
    drop(payloads);

    // The sender holds **no** handles of its own, yet the delivery must resolve the escaped
    // payload: the store's clone is the only thing keeping it alive.
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message(pid, term).expect("the escaped payload resolves");
    }
    let queued = queued_payloads(&rt, rt.self_pid());
    assert_eq!(queued.len(), 2, "both messages arrived");
    assert!(
        beam_term::compare::eq(queued[1], reference, rt.atoms()),
        "the delivered binary reads back byte-identical to the payload that escaped"
    );
    assert_eq!(
        queued_binary_counts(&rt, rt.self_pid())[1],
        1,
        "the shared delivery re-registered the payload's handle on the message"
    );
    assert_eq!(
        rt.eval_off_heap.binaries()[0].refcount(),
        2,
        "the receiver's message owns its own reference to the shared payload"
    );
}

/// **The mailbox-retained SHARE route on the `erlang:send/2` BIF path.** A process that
/// **received** an off-heap refc payload (so the only owner became the receiving process's
/// [`MailboxState`] once the message was removed) and then **forwards** the received term with a
/// BIF-path send must still deliver it **shared**: the copy resolves the payload through the
/// sender's retained mailbox families ([`super::RuntimeContext::send_source`] over
/// `Runtime::source_payloads`).
///
/// This is the regression guard for the family union: reverting it to the escaped-store-only table
/// (`Runtime::off_heap_source`) leaves the store **empty** and the retained mailbox family out, so
/// the copy can only **flatten** and the forwarded message owns no off-heap handle.
///
/// This is the only SHARE route a `beam_rt`-only change can reach on that path: the sending
/// machine's own `refc_binaries()` is unobservable from here (see
/// [`super::RuntimeContext::send_source`]'s docs for why — the in-flight `Machine` is out of
/// `RunnerState.engines` for the slice and `ExecutionContext::call_bif`/`SendHook::send` carry no
/// handle parameter), so the test pins exactly the mailbox family.
///
/// The send is the **BIF** form (`erlang:send/2` through `SendBridge` →
/// `ExecutionContext::send_message`), never the compiled `Dest ! Msg` instruction — that
/// instruction's `send_message_with_off_heap` arm is the one seam that *does* carry the machine's
/// handles.
#[test]
fn a_bif_send_forwards_a_mailbox_retained_off_heap_payload_by_sharing_it() {
    let (_source_heap, payloads, bytes, term) = escaped_off_heap_binary();
    let mut rt = runtime();
    let pid = self_term(&rt);

    // 1. Receive: the delivery resolves the payload off the **sender's** handle slice, so the
    //    queued message owns a shared handle of its own.
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message_with_off_heap(pid, term, &payloads.0)
            .expect("the received payload is shared");
    }
    assert_eq!(
        queued_binary_counts(&rt, rt.self_pid()),
        vec![1],
        "the received message owns the shared payload's handle"
    );
    // The received (copied) term — what the process's code would bind and later forward.
    let received = queued_payloads(&rt, rt.self_pid())[0];

    // 2. Remove: the handle moves to the process's mailbox scan state, and — with the building
    //    run's handles dropped — that retained family is now the payload's **only** owner.
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.remove_mailbox_message(0)
            .expect("the receive removes the message");
    }
    drop(payloads);
    assert_eq!(
        rt.mailboxes
            .get(&rt.self_pid)
            .expect("the scan state")
            .retained_binaries()
            .len(),
        1,
        "the removed message left its off-heap handle on the scan state"
    );
    assert!(
        rt.eval_off_heap.binaries().is_empty(),
        "the runtime's escaped-payload store holds nothing — only the mailbox family can resolve"
    );

    // 3. Forward through the `erlang:send/2` BIF (the `SendBridge` route): the payload must be
    //    SHARED again, resolved through the retained mailbox family alone.
    let answer = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("erlang", "send", 2, &[pid, received])
            .expect("erlang:send/2 forwards the received term")
    };
    assert!(
        beam_term::compare::eq(answer, received, rt.atoms()),
        "the BIF send returns the message it sent"
    );
    let queued = queued_payloads(&rt, rt.self_pid());
    assert_eq!(queued.len(), 1, "the forwarded message arrived");
    let mut reference_heap = beam_alloc::Heap::new(64).expect("the reference heap");
    let reference =
        beam_codec::term::make_binary(&mut reference_heap, &bytes).expect("the reference");
    assert!(
        beam_term::compare::eq(queued[0], reference, rt.atoms()),
        "the forwarded binary reads back byte-identical to the received payload"
    );
    assert_eq!(
        queued_binary_counts(&rt, rt.self_pid()),
        vec![1],
        "the forwarded message shares the payload — it owns an off-heap handle"
    );
    assert_eq!(
        rt.mailboxes
            .get(&rt.self_pid)
            .expect("the scan state")
            .retained_binaries()[0]
            .refcount(),
        2,
        "the forwarded message owns its own reference to the mailbox-retained payload"
    );
}

/// The on-heap reference thing for `reference`, built on a pooled BIF heap — the production
/// route (`src/native/process.rs::reference_term`), so the term stays valid after the heap goes
/// back to the pool (a `beam_alloc::Heap` never resets).
fn alias_reference_term(rt: &mut Runtime, reference: MonitorRef) -> Eterm {
    let mut heap = rt.take_bif_heap().expect("a pooled BIF heap");
    let term = reference.to_eterm(&mut heap).expect("the ref thing");
    rt.put_bif_heap(heap);
    term
}

/// The payloads of every message queued for `pid`, oldest first.
fn queued_payloads(rt: &Runtime, pid: ProcessId) -> Vec<Eterm> {
    rt.processes
        .get(pid)
        .expect("the process is live")
        .mailbox()
        .iter()
        .map(|message| message.payload())
        .collect()
}

/// The number of off-heap **refc-binary** handles each message queued for `pid` owns, oldest
/// first.
///
/// The receiver-side copy hands every payload it **shares** to the message's off-heap list, and
/// nothing to a payload it **flattens** — so this is the observable that distinguishes a shared
/// distribution from a flattened one (`Message::binaries`).
fn queued_binary_counts(rt: &Runtime, pid: ProcessId) -> Vec<usize> {
    rt.processes
        .get(pid)
        .expect("the process is live")
        .mailbox()
        .iter()
        .map(|message| message.binaries().len())
        .collect()
}

/// The compiled `Dest ! Msg` operator (`gen_server:reply/2`'s `Alias ! {Tag, Reply}`) reaches
/// the alias **owner**, and a `reply_demonitor` alias is consumed by that delivery.
///
/// `Instr::Send` calls `ExecutionContext::send_message_with_off_heap` directly
/// (`../beam_engine/src/families/receive.rs:110-113`), so this is the seam that has to resolve a
/// reference destination — without it the reply could never reach its alias.
///
/// Oracle (system `erl`, OTP 29 / `erts-17.1`):
/// `A = alias(), A ! hi, receive X -> X end` ⇒ `hi`, and after the alias is consumed a second
/// reference send no longer resolves (that half is `beam_process`'s own contract,
/// `tests/alias.rs`).
#[test]
fn a_compiled_send_to_an_alias_reference_reaches_the_owner_and_consumes_the_alias() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let reference = rt
        .processes
        .create_alias(owner, beam_process::UnaliasMode::ReplyDemonitor)
        .expect("a live owner mints an alias");
    let to = alias_reference_term(&mut rt, reference);
    assert!(
        !ProcessId::is_process(to),
        "an alias is a reference term, never a pid"
    );

    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message_with_off_heap(to, make_small(7), &[])
            .expect("a send to a live alias is delivered to its owner");
        assert_eq!(
            queued_payloads(ctx.runtime, owner),
            vec![make_small(7)],
            "the alias send reached the owner's mailbox"
        );
    }

    assert_eq!(
        rt.processes.resolve_alias(reference),
        None,
        "a reply_demonitor alias is consumed by the message that went through it"
    );
}

/// An `explicit_unalias` alias **survives** a delivery — only `erlang:unalias/1` clears it (the
/// C's `handle_altact_msg` drops the alias only for `ERTS_ML_STATE_ALIAS_ONCE`).
#[test]
fn an_explicit_unalias_alias_survives_the_send() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let reference = rt
        .processes
        .create_alias(owner, beam_process::UnaliasMode::ExplicitUnalias)
        .expect("a live owner mints an alias");
    let to = alias_reference_term(&mut rt, reference);

    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message(to, make_small(1))
            .expect("a send to a live alias is delivered to its owner");
    }

    assert_eq!(queued_payloads(&rt, owner), vec![make_small(1)]);
    assert_eq!(
        rt.processes.resolve_alias(reference),
        Some(owner),
        "explicit_unalias is not a reply-time expiry"
    );
}

/// `erlang:send(Alias, Msg)` — the BIF route (the `SendHook` bridge) — resolves the same alias
/// and returns the message, as the oracle does.
#[test]
fn the_send_bif_delivers_through_an_alias_reference() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let reference = rt
        .processes
        .create_alias(owner, beam_process::UnaliasMode::ReplyDemonitor)
        .expect("a live owner mints an alias");
    let to = alias_reference_term(&mut rt, reference);

    {
        let mut ctx = RuntimeContext::new(&mut rt);
        let answer = ctx
            .call_bif("erlang", "send", 2, &[to, make_small(9)])
            .expect("erlang:send/2 to a live alias");
        assert_eq!(signed_value(answer), 9, "a send returns the message");
        assert_eq!(
            queued_payloads(ctx.runtime, owner),
            vec![make_small(9)],
            "the BIF send reached the owner's mailbox"
        );
    }

    assert_eq!(
        rt.processes.resolve_alias(reference),
        None,
        "the BIF delivery consumes a reply_demonitor alias too"
    );
}

/// `resolve_send_target`'s contract on both seams: a pid passes through with **nothing** to
/// consume, a live alias maps to its owner plus the reference to consume, and a reference that
/// is not a live alias is the typed `badarg` (both as the compiled send and as the BIF).
///
/// The non-alias reference here is a **table-minted** one (a plain `erlang:monitor/2`
/// reference). A bare `erlang:make_ref/0` reference is rejected too: the destination is
/// classified by **provenance** (see [`super::alias`]), so a bare reference is never treated
/// as an alias even when its id coincides with a live alias's.
#[test]
fn resolve_send_target_maps_pids_and_live_aliases_and_rejects_other_references() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let alias = rt
        .processes
        .create_alias(owner, beam_process::UnaliasMode::ReplyDemonitor)
        .expect("a live owner mints an alias");
    let alias_term = alias_reference_term(&mut rt, alias);
    // A reference from the table's own allocator that is **not** an alias: a monitor.
    let monitor = rt
        .processes
        .monitor_with_tag(owner, owner, beam_process::MonitorKind::Process, None)
        .expect("a monitor reference");
    let monitor_term = alias_reference_term(&mut rt, monitor);

    {
        let mut ctx = RuntimeContext::new(&mut rt);

        let (target, consume) = ctx
            .resolve_send_target(owner.to_eterm())
            .expect("a pid is a destination");
        assert_eq!(target, owner, "a pid passes through");
        assert_eq!(consume, None, "a pid has no alias to consume");

        let (target, consume) = ctx
            .resolve_send_target(alias_term)
            .expect("a live alias is a destination");
        assert_eq!(target, owner, "an alias resolves to its owner");
        assert_eq!(
            consume,
            Some(alias),
            "the reference must be consumed after delivery"
        );

        assert!(
            matches!(ctx.resolve_send_target(monitor_term), Err(BeamError::BadArg)),
            "a monitor reference is not an alias"
        );
        assert!(
            matches!(ctx.send_message(monitor_term, make_small(4)), Err(BeamError::BadArg)),
            "a monitor reference is not a compiled-send destination"
        );
        assert!(
            matches!(
                ctx.call_bif("erlang", "send", 2, &[monitor_term, make_small(4)]),
                Err(BeamError::BadArg)
            ),
            "a monitor reference is not a send-BIF destination"
        );

        assert_eq!(
            queued_payloads(ctx.runtime, owner),
            Vec::<Eterm>::new(),
            "no refused destination delivered anything"
        );
    }
}

/// Puts the runtime's **own** reference allocator onto `id`, so its next draw mints exactly `id`.
///
/// [`beam_process::RefAllocator`] is monotone from `0`, so advancing a fresh allocator `id` times
/// lands the *next* draw on `id`: the deterministic way to force the runtime's reference id space
/// and the process table's onto the same number (the collision HAZARD-2 covers).
fn force_next_reference_id(rt: &mut Runtime, id: u64) {
    let mut allocator = beam_process::RefAllocator::new();
    for _ in 0..id {
        allocator.next();
    }
    rt.refs = allocator;
}

/// The **id-collision regression guard** for the provenance bridge
/// ([`super::alias::alias_destination`]).
///
/// This runtime has **two independent** reference id spaces, both monotone `u64` from `0`, and both
/// encode the very same on-heap thing `[make_header(3, REF_SUBTAG), node=0, serial=0, id]`:
///
/// * [`Runtime::refs`](crate::Runtime) — fed by `erlang:make_ref/0`, the NIF `make_ref`, the
///   `erlang:send_after/3`/`start_timer` timer references and `seq_trace`;
/// * the [`ProcessTable`]'s own allocator — fed by every `erlang:monitor/2,3` and `erlang:alias/0,1`
///   (the `{alias, Mode}` monitor form included).
///
/// Resolving a `send` destination by its **decoded id** alone would therefore deliver a bare
/// `make_ref/0` (or a timer reference) to a live alias's owner. The destination is classified by
/// **provenance** first, so a bare runtime-minted reference is never an alias — even when its id
/// coincides with one. The test forces the coincidence deterministically (both allocators are put on
/// the same id), pins that the id-based lookup *would* have resolved (the pre-fix behaviour), and
/// then asserts that all three `send` entry points reject the colliding reference with the typed
/// `badarg` and deliver nothing.
#[test]
fn resolve_send_target_rejects_a_bare_reference_that_collides_with_a_live_alias() {
    let mut rt = runtime();
    let owner = rt.self_pid();

    // 1. A live alias minted through the **real BIF** (so the runtime's provenance registry is
    //    filled exactly as production fills it) from the process table's own allocator.
    let alias_term = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("erlang", "alias", 1, &[NIL])
            .expect("erlang:alias([])")
    };
    let alias = MonitorRef::from_eterm(alias_term).expect("an alias is a reference");

    // 2. Force the runtime's own allocator onto the alias's id before **each** mint, so the
    //    `make_ref/0` and the timer reference both carry exactly the alias's id.
    force_next_reference_id(&mut rt, alias.id());
    let bare_term = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("erlang", "make_ref", 0, &[])
            .expect("erlang:make_ref/0")
    };
    force_next_reference_id(&mut rt, alias.id());
    let timer_term = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif(
            "erlang",
            "send_after",
            3,
            &[make_small(600_000), owner.to_eterm(), make_small(1)],
        )
        .expect("erlang:send_after/3")
    };

    // 3. The coincidence is real: three references, one id — and the id-based lookup WOULD have
    //    resolved both colliding references to the alias (the pre-fix wrong delivery).
    let id_of = |term: Eterm| MonitorRef::from_eterm(term).expect("a reference").id();
    assert_eq!(id_of(bare_term), alias.id(), "the bare reference collides");
    assert_eq!(id_of(timer_term), alias.id(), "the timer reference collides");
    assert_eq!(
        rt.processes.resolve_alias(MonitorRef::from_eterm(bare_term).expect("a reference")),
        Some(owner),
        "a by-id lookup resolves the bare reference to the alias owner — the hazard being guarded"
    );
    assert_eq!(
        rt.processes.resolve_alias(MonitorRef::from_eterm(timer_term).expect("a reference")),
        Some(owner),
        "and the timer reference to the very same owner"
    );

    // 4. Provenance still tells the three apart.
    assert_eq!(
        rt.local_reference(alias_term),
        Some(Some(alias)),
        "the alias term's provenance is the alias reference"
    );
    assert_eq!(
        rt.local_reference(bare_term),
        Some(None),
        "a make_ref/0 term is a bare runtime-minted reference"
    );
    assert_eq!(
        rt.local_reference(timer_term),
        Some(None),
        "a timer reference is a bare runtime-minted reference"
    );

    // 5. Every send entry point rejects both colliding references and nothing is delivered.
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        assert!(
            matches!(ctx.resolve_send_target(bare_term), Err(BeamError::BadArg)),
            "a bare reference must not resolve as the live alias it collides with"
        );
        assert!(
            matches!(
                ctx.resolve_send_target(timer_term),
                Err(BeamError::BadArg)
            ),
            "a timer reference must not resolve as the live alias it collides with"
        );
        for (label, term) in [("make_ref/0", bare_term), ("timer", timer_term)] {
            assert!(
                matches!(ctx.send_message(term, make_small(9)), Err(BeamError::BadArg)),
                "the compiled send must reject the colliding {label} reference"
            );
            assert!(
                matches!(
                    ctx.call_bif("erlang", "send", 2, &[term, make_small(9)]),
                    Err(BeamError::BadArg)
                ),
                "the erlang:send/2 BIF must reject the colliding {label} reference"
            );
        }
        assert_eq!(
            queued_payloads(ctx.runtime, owner),
            Vec::<Eterm>::new(),
            "not one refused destination delivered anything to the alias owner"
        );
    }

    // 6. The live alias itself is untouched by the colliding references and still resolves.
    assert_eq!(
        rt.processes.resolve_alias(alias),
        Some(owner),
        "the alias is still live after the refusals"
    );
    let ctx = RuntimeContext::new(&mut rt);
    let (target, consume) = ctx
        .resolve_send_target(alias_term)
        .expect("the live alias still resolves");
    assert_eq!(target, owner, "the alias still names its owner");
    assert_eq!(consume, Some(alias), "and still yields the reference to consume");
}

/// The provenance classification changes **nothing** for the references the process table minted: a
/// live alias still resolves to its owner plus the reference to consume, and a plain
/// `erlang:monitor/2` reference is still the typed `badarg` on every send entry point.
#[test]
fn a_table_minted_alias_still_resolves_and_a_plain_monitor_reference_is_rejected() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let process = rt.intern("process");
    // Both references are minted through the **real BIFs**, so their provenance entries are exactly
    // the ones production writes (`alias_1`, `monitor_process`).
    let (alias_term, monitor_term) = {
        let mut ctx = RuntimeContext::new(&mut rt);
        let alias = ctx
            .call_bif("erlang", "alias", 1, &[NIL])
            .expect("erlang:alias([])");
        let monitor = ctx
            .call_bif("erlang", "monitor", 2, &[process, owner.to_eterm()])
            .expect("erlang:monitor(process, self())");
        (alias, monitor)
    };
    let alias = MonitorRef::from_eterm(alias_term).expect("an alias is a reference");
    let monitor = MonitorRef::from_eterm(monitor_term).expect("a monitor ref is a reference");
    assert_ne!(alias, monitor, "the table minted two distinct references");

    assert_eq!(
        rt.local_reference(alias_term),
        Some(Some(alias)),
        "an alias term is recorded as its alias reference"
    );
    assert_eq!(
        rt.local_reference(monitor_term),
        Some(Some(monitor)),
        "a monitor term is recorded as its monitor reference"
    );

    {
        let mut ctx = RuntimeContext::new(&mut rt);
        let (target, consume) = ctx
            .resolve_send_target(alias_term)
            .expect("a live alias is a destination");
        assert_eq!(target, owner, "an alias resolves to its owner");
        assert_eq!(consume, Some(alias), "a reply_demonitor alias must be consumed");

        assert!(
            matches!(
                ctx.resolve_send_target(monitor_term),
                Err(BeamError::BadArg)
            ),
            "a plain monitor reference is not an alias"
        );
        assert!(
            matches!(
                ctx.send_message(monitor_term, make_small(4)),
                Err(BeamError::BadArg)
            ),
            "a plain monitor reference is not a compiled-send destination"
        );
        assert!(
            matches!(
                ctx.call_bif("erlang", "send", 2, &[monitor_term, make_small(4)]),
                Err(BeamError::BadArg)
            ),
            "a plain monitor reference is not a send-BIF destination"
        );
        assert_eq!(
            queued_payloads(ctx.runtime, owner),
            Vec::<Eterm>::new(),
            "the refused monitor reference delivered nothing"
        );
    }
}

/// The four arms of the ONE provenance helper — [`Runtime::local_reference`] (and its
/// [`Runtime::is_known_local_reference`] predicate) — which the send path and the timer predicates
/// share:
///
/// * a `make_ref/0` term and a timer reference are **bare** runtime-minted references → `Some(None)`;
/// * a monitor/alias reference is → `Some(Some(reference))`;
/// * a term this runtime did **not** mint (a reference decoded from an external term — here
///   `binary_to_term(term_to_binary(Alias))`) has **unknown provenance** → `None`, so the
///   resolution falls back to the id the term encodes and still names the live alias.
#[test]
fn local_reference_classifies_all_four_provenance_arms() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let process = rt.intern("process");
    // Both table-minted references come from the **real BIFs**, so their provenance entries are the
    // ones production writes.
    let (alias_term, monitor_term) = {
        let mut ctx = RuntimeContext::new(&mut rt);
        let alias = ctx
            .call_bif("erlang", "alias", 1, &[NIL])
            .expect("erlang:alias([])");
        let monitor = ctx
            .call_bif("erlang", "monitor", 2, &[process, owner.to_eterm()])
            .expect("erlang:monitor(process, self())");
        (alias, monitor)
    };
    let alias = MonitorRef::from_eterm(alias_term).expect("an alias is a reference");
    let monitor = MonitorRef::from_eterm(monitor_term).expect("a monitor ref is a reference");

    let (bare_term, timer_term, copy_term) = {
        let mut ctx = RuntimeContext::new(&mut rt);
        let bare = ctx
            .call_bif("erlang", "make_ref", 0, &[])
            .expect("erlang:make_ref/0");
        let timer = ctx
            .call_bif(
                "erlang",
                "send_after",
                3,
                &[make_small(600_000), owner.to_eterm(), make_small(1)],
            )
            .expect("erlang:send_after/3");
        // A **copy** of the alias: `binary_to_term(term_to_binary(Alias))` materializes a fresh
        // reference term the runtime never minted, so it carries the alias's id but no provenance.
        let image = ctx
            .call_bif("erlang", "term_to_binary", 1, &[alias_term])
            .expect("term_to_binary of the alias");
        let copy = ctx
            .call_bif("erlang", "binary_to_term", 1, &[image])
            .expect("binary_to_term of the alias image");
        (bare, timer, copy)
    };

    assert_eq!(
        rt.local_reference(bare_term),
        Some(None),
        "make_ref/0 is a bare runtime-minted reference"
    );
    assert_eq!(
        rt.local_reference(timer_term),
        Some(None),
        "a timer reference is a bare runtime-minted reference"
    );
    assert_eq!(
        rt.local_reference(alias_term),
        Some(Some(alias)),
        "an alias term is recorded as its alias reference"
    );
    assert_eq!(
        rt.local_reference(monitor_term),
        Some(Some(monitor)),
        "a monitor term is recorded as its monitor reference"
    );
    assert_eq!(
        rt.local_reference(copy_term),
        None,
        "a decoded copy was not minted here, so its provenance is unknown"
    );
    assert!(
        rt.is_known_local_reference(alias_term),
        "an alias reference is a local reference"
    );
    assert!(
        !rt.is_known_local_reference(copy_term),
        "a decoded copy is not a reference this runtime minted"
    );
    assert_eq!(
        MonitorRef::from_eterm(copy_term).expect("a reference").id(),
        alias.id(),
        "the copy nonetheless names the same id"
    );

    // Unknown provenance falls back to the id the term encodes — the `binary_to_term`'d alias route
    // must still resolve the live alias it names (this is the fallback arm the class-first
    // classification deliberately keeps).
    let ctx = RuntimeContext::new(&mut rt);
    let (target, consume) = ctx
        .resolve_send_target(copy_term)
        .expect("a decoded copy of a live alias still resolves");
    assert_eq!(target, owner, "the decoded copy names the alias owner");
    assert_eq!(
        consume,
        Some(alias),
        "and yields the reference to consume, exactly as the original term does"
    );
}

/// **HAZARD-2 — a known divergence from the oracle that `beam_rt` cannot close.**
///
/// This runtime has two **independent** reference id spaces
/// ([`resolve_send_target_rejects_a_bare_reference_that_collides_with_a_live_alias`]) and both mint
/// the very same on-heap thing `[make_header(3, REF_SUBTAG), 0, 0, id]`. Term comparison
/// ([`beam_term::compare::eq`] — `=:=`) compares that payload, so a bare `erlang:make_ref/0`
/// reference and a monitor/alias reference that happen to carry the **same id** are `=:=` here. The
/// oracle has **one** id space, so it can never produce such a pair: this is a divergence, not
/// desirable behaviour.
///
/// It is **not closable from `beam_rt`**: the provenance registry that tells the two mints apart is
/// keyed by the term *word* (which is why the send path is already correct), while `=:=` is a
/// `beam_term` operation over the payload `[hdr, 0, 0, id]`. Closing it needs the two allocators
/// unified onto one id space — or aliases keyed by the reference term — which is **owned by
/// `../beam_process/`** (`RefAllocator`/`MonitorRef` vs `Runtime::refs`).
#[test]
fn a_bare_reference_and_a_monitor_reference_with_equal_ids_are_exactly_equal() {
    let mut rt = runtime();
    // A live alias minted through the real BIF, from the process table's own allocator.
    let alias_term = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("erlang", "alias", 1, &[NIL])
            .expect("erlang:alias([])")
    };
    let alias = MonitorRef::from_eterm(alias_term).expect("an alias is a reference");

    // Put the runtime's own allocator on the alias's id, then mint a bare `make_ref/0` reference.
    force_next_reference_id(&mut rt, alias.id());
    let bare_term = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("erlang", "make_ref", 0, &[])
            .expect("erlang:make_ref/0")
    };

    // The two mints carry the same id ...
    assert_eq!(
        MonitorRef::from_eterm(bare_term).expect("a reference").id(),
        alias.id(),
        "the two references carry the same id"
    );
    // ... and compare `=:=` (the divergence): `beam_term` compares the `[hdr, 0, 0, id]` payload.
    assert!(
        beam_term::compare::eq(bare_term, alias_term, rt.atoms()),
        "HAZARD-2: a bare make_ref/0 and a monitor/alias reference with the same id compare =:= \
         here — a divergence from the oracle's single id space, not closable from beam_rt"
    );
    // The arithmetic `==` ordering agrees with `=:=` for two reference terms.
    assert!(
        beam_term::compare::cmp_arith(bare_term, alias_term, rt.atoms()).is_eq(),
        "the arithmetic == ordering agrees with =:= for two references"
    );
    // Provenance still tells them apart — which is what keeps the *send* path correct.
    assert_eq!(
        rt.local_reference(alias_term),
        Some(Some(alias)),
        "the alias's provenance is still the alias reference"
    );
    assert_eq!(
        rt.local_reference(bare_term),
        Some(None),
        "the bare reference's provenance is still 'not an alias'"
    );
}

#[test]
fn receive_next_peeks_and_the_scan_operations_position_the_mailbox() {
    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut ctx = RuntimeContext::new(&mut rt);

    // An empty mailbox is `Ok(None)` (not an error) — that is what makes the
    // compiled code jump to the `wait` that blocks.
    assert_eq!(ctx.receive_next().expect("empty mailbox"), None);

    ctx.send_message(pid, make_small(1)).expect("first");
    ctx.send_message(pid, make_small(2)).expect("second");

    // `loop_rec` peeks: repeated calls yield the same candidate.
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));

    // `loop_rec_end` advances past it.
    ctx.receive_skip().unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(2)));

    // `remove_message` unlinks the candidate and repositions at the first message.
    ctx.receive_remove().unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));

    // `timeout` resets the scan (already first here).
    ctx.receive_reset().unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
}

#[test]
fn remove_message_unlinks_only_the_matched_candidate() {
    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut ctx = RuntimeContext::new(&mut rt);
    ctx.send_message(pid, make_small(1)).unwrap();
    ctx.send_message(pid, make_small(2)).unwrap();
    ctx.send_message(pid, make_small(3)).unwrap();

    ctx.receive_skip().unwrap(); // candidate `2`
    ctx.receive_remove().unwrap();

    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    let left: Vec<Eterm> = mailbox.iter().map(beam_message::Message::payload).collect();
    assert_eq!(left, vec![make_small(1), make_small(3)]);
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
}

#[test]
fn an_empty_mailbox_blocks_and_the_plain_entry_is_the_typed_blocked_error() {
    // Mirrors the reference wait-on-an-empty-mailbox case: the engine reports `Wait`,
    // and the non-blocking entry folds that into the documented typed error.
    let image = mailbox_fixture();
    let wait = image.atom_table.lookup("wait").expect("wait/0");
    let mut rt = runtime();
    let mut machine = Machine::new();

    let outcome = {
        let mut ctx = RuntimeContext::new(&mut rt);
        machine
            .run_export_outcome(&image, wait, 0, &[], &mut ctx)
            .expect("wait/0")
    };
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "an empty mailbox suspends the receive, got {outcome:?}"
    );

    // The non-blocking entry folds the suspension into the documented typed error.
    let error = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.run_export(&image, "wait", 0, &[])
            .expect_err("run_export is non-blocking")
    };
    assert_eq!(error.to_string(), beam_engine::blocked().to_string());
}

#[test]
fn a_pump_resumes_a_cross_module_receive_whose_mailbox_the_pump_filled() {
    // `run_export` is **blocking-aware**: a cross-module callee whose bare `receive`
    // (`mailbox:wait/0` is `receive X -> X end`) finds an empty mailbox is driven by the
    // scheduler — the very `Runtime::pump_scheduler` the `-eval` evaluator's `receive`
    // drives — rather than folded into the typed `beam_engine::blocked()` gap.
    //
    // The message the pump delivers into the runtime's own mailbox (the process `run_export`
    // runs as) arrives by the same deliver+wake path a runnable sender's `send` takes, so
    // the suspended callee really re-scans a mailbox the pump changed: an `erlang:send_after/3`
    // deadline owed to ourselves is fired by the pass (`Runtime::fire_due_timers`). Without
    // the pump this exact call is the typed `blocked()` error (see the sibling case above and
    // the slice case below).
    let image = mailbox_fixture();
    let mut rt = runtime();
    let me = self_term(&rt);
    let message = make_small(42);

    let mut ctx = RuntimeContext::new(&mut rt);
    assert!(
        ctx.may_pump_scheduler(),
        "a plain context may advance the scheduler between resumes"
    );
    // A 0 ms deadline to our own mailbox. It has not fired yet, so the mailbox is still
    // empty when the callee suspends and the value can only come from the pump.
    let _reference = ctx
        .call_bif("erlang", "send_after", 3, &[make_small(0), me, message])
        .expect("arm a 0 ms timer to the runtime's own mailbox");

    let result = ctx
        .run_export(&image, "wait", 0, &[])
        .expect("the pump delivers the timed message and the receive resumes");
    assert_eq!(
        result, message,
        "the resumed receive took the message the pump delivered"
    );

    // The resume removed the matched message.
    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    assert!(mailbox.is_empty(), "the receive removed the message");
}

#[test]
fn a_slice_context_that_may_not_pump_reports_the_typed_blocked_gap() {
    // A context built by `RuntimeContext::for_process` is itself **inside** a scheduler
    // slice, so it must never re-enter the scheduler (that would run a second process from
    // within a slice). A cross-module callee that suspends on an empty mailbox therefore
    // keeps the plain non-blocking entry's typed `beam_engine::blocked()` capability gap —
    // never a wrong result and never a hang.
    let image = mailbox_fixture();
    let mut rt = runtime();
    let mut table = beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut beam_term::prelude::AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("spawn a process");

    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    assert!(
        !ctx.may_pump_scheduler(),
        "a slice context is inside a pass and may not re-enter the scheduler"
    );
    let error = ctx
        .run_export(&image, "wait", 0, &[])
        .expect_err("an empty mailbox with nothing runnable is the typed gap");
    assert_eq!(error.to_string(), beam_engine::blocked().to_string());
}

#[test]
fn a_suspended_receive_resumes_against_the_runtime_stored_scan_position() {
    // The point of storing the scan state per pid: the resume happens through a
    // **fresh context**, and it still finds the position where the receive blocked.
    let image = mailbox_fixture();
    let wait = image.atom_table.lookup("wait").expect("wait/0");
    let mut rt = runtime();
    let mut machine = Machine::new();

    let outcome = {
        let mut ctx = RuntimeContext::new(&mut rt);
        machine
            .run_export_outcome(&image, wait, 0, &[], &mut ctx)
            .expect("wait/0")
    };
    let (func, pc) = match outcome {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("expected a wait, got {other:?}"),
    };

    // Deliver a message to the runtime's own mailbox (a `receive` waiting on it).
    let pid = rt.self_pid();
    rt.processes
        .deliver_simple(pid, pid.to_eterm(), make_small(42))
        .expect("deliver");

    let mut ctx = RuntimeContext::new(&mut rt);
    let resumed = machine
        .resume_outcome(&image, func, pc, &mut ctx)
        .expect("resume at the wait target");
    assert_eq!(resumed, RunOutcome::Returned(make_small(42)));
    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    assert!(
        mailbox.is_empty(),
        "the resumed receive removed the matched message"
    );
}

#[test]
fn scan_skips_a_non_matching_head_and_keeps_it_queued() {
    // Mirrors the reference `scan_skips_a_non_matching_head_before_matching`.
    let image = mailbox_fixture();
    let mut rt = runtime();
    let result = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.run_export(&image, "scan", 0, &[]).expect("scan/0")
    };
    assert_eq!(result, make_small(7), "the second message matched");

    let mailbox = rt
        .processes
        .get(rt.self_pid())
        .expect("the runtime process")
        .mailbox();
    assert_eq!(mailbox.len(), 1, "only the matched message was removed");
    assert_eq!(
        mailbox.peek().expect("the scanned-past head").payload(),
        make_small(42)
    );
}

#[test]
fn a_marker_reserved_on_an_empty_queue_is_the_shared_undefined_atom() {
    let mut rt = runtime();
    // The atom `receive_marker_reserve` interns is the C `am_undefined`.
    let undefined = rt.intern("undefined");
    let mut ctx = RuntimeContext::new(&mut rt);

    let marker = ctx.receive_marker_reserve().expect("reserve");
    assert_eq!(marker, undefined, "an empty queue has no position to record");
    assert!(!is_small(marker), "`undefined` is an atom, not a marker id");

    // Binding it is a silent no-op, and using the reference cannot move the scan.
    ctx.receive_marker_bind(marker, make_small(1)).unwrap();
    ctx.receive_marker_use(make_small(1)).unwrap();
    assert_eq!(ctx.receive_next().unwrap(), None);
}

#[test]
fn the_marker_prologue_repositions_the_scan() {
    // Mirrors the reference `the_mailbox_repositions_the_scan_at_a_marker`, driven
    // through the real context (so it exercises `RuntimeContext::mailbox_parts`).
    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut ctx = RuntimeContext::new(&mut rt);
    ctx.send_message(pid, make_small(1)).unwrap();

    ctx.receive_skip().unwrap(); // scan past `1`
    let marker = ctx.receive_marker_reserve().expect("reserve");
    assert!(is_small(marker), "a recorded marker is a small integer");
    ctx.receive_marker_bind(marker, make_small(999)).unwrap();

    // A later message (self-delivered through the same context).
    ctx.send_message(pid, make_small(2)).unwrap();

    ctx.receive_reset().unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
    ctx.receive_marker_use(make_small(999)).unwrap();
    assert_eq!(
        ctx.receive_next().unwrap(),
        Some(make_small(2)),
        "the marker moved the scan past the first message"
    );

    ctx.receive_marker_clear(make_small(999)).unwrap();
    ctx.receive_reset().unwrap();
    ctx.receive_marker_use(make_small(999)).unwrap();
    assert_eq!(
        ctx.receive_next().unwrap(),
        Some(make_small(1)),
        "a cleared marker no longer repositions the scan"
    );
}

#[test]
fn for_process_resolves_the_mailbox_against_the_supplied_table() {
    // The scheduler-pass seam: a context running a pid in a table it owns separately
    // from the runtime, with the scan state still keyed by pid in the runtime.
    let mut rt = runtime();
    let mut table = beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut beam_term::prelude::AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("spawn a process");
    let undefined = rt.intern("undefined");

    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    assert_eq!(ctx.pid(), pid);
    // Empty: `Ok(None)` from the external table's process, `undefined` marker.
    assert_eq!(ctx.receive_next().unwrap(), None);
    assert_eq!(ctx.receive_marker_reserve().unwrap(), undefined);

    // A self-delivery lands in the external table's process mailbox.
    ctx.send_message(pid.to_eterm(), make_small(5)).unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(5)));

    // `erlang:self/0` reports the context's pid, not the runtime's own.
    let self_term = ctx.call_bif("erlang", "self", 0, &[]).expect("self/0");
    assert_eq!(self_term, pid.to_eterm());

    // Drop the context to release its borrow of `table`, then inspect the mailbox.
    drop(ctx);
    assert_eq!(table.get(pid).expect("the process").mailbox().len(), 1);
}

#[test]
fn for_process_reports_a_missing_process_as_the_typed_capability_gap() {
    let mut rt = runtime();
    let mut table = beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut beam_term::prelude::AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("spawn a process");

    // Remove the process *before* building the context: a receive-seam call must be
    // the typed `unsupported` capability gap, never a panic and never a wrong result.
    table.take(pid).expect("take the process");
    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    let error = ctx.receive_next().expect_err("no mailbox for the pid");
    assert_eq!(
        crate::error::unsupported_reason(&error).as_deref(),
        Some(format!("receive: process {pid:?} has no mailbox").as_str())
    );
}

#[test]
fn the_record_source_predicate_selects_only_its_own_bif() {
    // Exactly one of the five mutually exclusive bridges is installed per call, so the
    // predicates must be pairwise disjoint for the name each one claims.
    assert!(RuntimeContext::uses_record_source(
        "records",
        "get_definition",
        2
    ));
    for (module, function, arity) in [
        ("records", "get", 2),
        ("records", "get_definition", 1),
        ("recs", "get_definition", 2),
        ("erlang", "load_nif", 2),
        ("erlang", "get", 1),
        ("erlang", "send", 2),
    ] {
        assert!(
            !RuntimeContext::uses_record_source(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
    assert!(RuntimeContext::uses_record_source(
        "records",
        "get_definition",
        2
    ));
    // It is not the NIF-loading arm either: the two bridges are mutually exclusive.
    assert!(!RuntimeContext::uses_nif_loader(
        "records",
        "get_definition",
        2
    ));
}

#[test]
fn the_send_predicate_selects_only_the_message_sending_bifs() {
    // The fifth bridge (`SendBridge`, the delivery seam) is installed for the two names
    // `beam_bifs` registers under **one** implementation — `erlang:send/2` and
    // `erlang:'!'/2` (`../beam_bifs/src/proc.rs`) — and nothing else.
    assert!(RuntimeContext::uses_send_hook("erlang", "send", 2));
    assert!(RuntimeContext::uses_send_hook("erlang", "!", 2));
    for (module, function, arity) in [
        ("erlang", "send", 1),
        ("erlang", "send_after", 3),
        ("erlang", "send_nosuspend", 2),
        ("erlang", "self", 0),
        ("erlang", "!", 1),
        ("erlang", "!", 3),
        ("lists", "send", 2),
        ("erlang", "get", 1),
        ("erlang", "load_nif", 2),
        ("records", "get_definition", 2),
    ] {
        assert!(
            !RuntimeContext::uses_send_hook(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
    // Pairwise disjoint with the other four arms: `send/2` needs the delivery bridge (it
    // borrows the whole runtime) and neither a dictionary, a NIF loader, a record source
    // nor the fun caller.
    for (function, arity) in [("send", 2), ("!", 2)] {
        assert!(!RuntimeContext::uses_dict("erlang", function, arity));
        assert!(!RuntimeContext::uses_nif_loader("erlang", function, arity));
        assert!(!RuntimeContext::uses_record_source("erlang", function, arity));
    }
}

#[test]
fn the_memory_hook_gate_selects_only_the_two_memory_bifs() {
    // `RuntimeMemory::new` folds every loaded module's `.beam` size (one `stat` syscall
    // each), so it must be built **only** for the two BIFs that read the `MemoryHook`
    // seam. A too-narrow gate would turn `erlang:memory/0,1` into the typed
    // `unsupported_bif` gap; a too-wide one would put the ~10 us scan back on the
    // arithmetic hot path.
    assert!(RuntimeContext::uses_memory_hook("erlang", "memory", 0));
    assert!(RuntimeContext::uses_memory_hook("erlang", "memory", 1));
    for (module, function, arity) in [
        ("erlang", "memory", 2),
        ("erlang", "memory", 3),
        ("erlang", "processes", 0),
        ("erts_internal", "memory", 0),
        ("erlang", "-", 2),
        ("erlang", "+", 2),
        ("lists", "memory", 1),
    ] {
        assert!(
            !RuntimeContext::uses_memory_hook(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
}

#[test]
fn the_fun_identity_gate_selects_the_image_and_introspection_bifs() {
    // The `LocalFunIdentity` seam has two consumer families. The **encoders**: the two
    // `beam_bifs` code paths that reach `binconv::encode_body`'s first statement —
    // `term_to_binary/1,2` directly and `external_size/1,2` through `plain_image_of`. And
    // the **introspection** pair `erlang:fun_info/1,2`, which reads the seam for a fun's
    // `name`. Every other registered BIF must skip the `Arc` clone + `PathBuf` allocation
    // the bridge does.
    for (function, arity) in [
        ("term_to_binary", 1),
        ("term_to_binary", 2),
        ("external_size", 1),
        ("external_size", 2),
        ("fun_info", 1),
        ("fun_info", 2),
    ] {
        assert!(
            RuntimeContext::uses_fun_identity("erlang", function, arity),
            "erlang:{function}/{arity}"
        );
    }
    // The introspection half is *wider* than `beam_bifs`' own roster, which documents that
    // crate's seam-readers (the two encoders) only: the gate is the beam_rt side of the
    // seam, so a `fun_info` call still gets the bridge installed. That divergence is the
    // contract, not a drift — the roster test below compares the gate to the roster's
    // predicate only on the roster's own keys.
    assert!(!beam_bifs::needs_local_fun_identity("erlang", "fun_info", 1));
    assert!(!beam_bifs::needs_local_fun_identity("erlang", "fun_info", 2));
    // The widened gate is what the dispatch memo records, so the bridge really is installed
    // on the introspection arm (`DispatchHooks::of` reads the gate, not the roster).
    assert!(DispatchHooks::of("erlang", "fun_info", 1).fun_identity);
    assert!(DispatchHooks::of("erlang", "fun_info", 2).fun_identity);
    for (module, function, arity) in [
        ("erlang", "term_to_binary", 0),
        ("erlang", "term_to_binary", 3),
        ("erlang", "external_size", 0),
        ("erlang", "external_size", 3),
        ("erlang", "binary_to_term", 1),
        ("erlang", "-", 2),
        ("lists", "term_to_binary", 1),
        // `fun_info/2` is the only arity pair the introspection half claims, and it is
        // `erlang`'s alone. `fun_info_mfa/1` is deliberately not claimed: it rejects every
        // non-`External` fun before reading any seam, so it never calls the accessor.
        ("erlang", "fun_info", 0),
        ("erlang", "fun_info", 3),
        ("erlang", "fun_info_mfa", 1),
        ("erts_internal", "fun_info", 1),
        ("lists", "fun_info", 1),
    ] {
        assert!(
            !RuntimeContext::uses_fun_identity(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
}

#[test]
fn the_local_node_identity_is_nonode_at_nohost_with_creation_zero() {
    // The runtime's boot seed interns the node-name atom, so the identity resolves and
    // `term_to_binary/1` of an internal pid can emit `NEW_PID_EXT` (88).
    let rt = runtime();
    let expected = rt.atoms().lookup("nonode@nohost").expect("seeded at boot");
    let mut identity = NodeIdentity {
        node: rt.atoms().lookup("nonode@nohost"),
    };
    assert_eq!(identity.local_node(), Some((expected, 0)));

    // A table without the atom answers `None` — an unencodable id, never a
    // fabricated node.
    let mut identity = NodeIdentity { node: None };
    assert_eq!(identity.local_node(), None);
}

#[test]
fn term_to_binary_of_an_internal_pid_is_a_new_pid_ext() {
    // End to end through the real dispatch: the `LocalNode` hook installed by
    // `call_registered` turns the runtime's own pid into the oracle's `NEW_PID_EXT`
    // (`88`) image. Oracle: `binary:at(term_to_binary(self()),1)` ⇒ `88`.
    let mut rt = runtime();
    let pid = rt.self_pid();
    let mut ctx = RuntimeContext::new(&mut rt);
    let image = ctx
        .call_bif("erlang", "term_to_binary", 1, &[pid.to_eterm()])
        .expect("term_to_binary(self())");
    let tag = ctx
        .call_bif("binary", "at", 2, &[image, make_small(1)])
        .expect("binary:at/2");
    assert_eq!(signed_value(tag), 88);
}

/// The `LOCAL_EXT` (`121`) image: `term_to_binary(Term, [local])` is live on every arm, because
/// [`NodeIdentity`] now answers [`LocalNode::local_node_hash`].
///
/// Oracle (`erl -noshell -eval 'B = term_to_binary({ok,17},[local]),
/// io:format("~p~n",[{binary:at(B,0),binary:at(B,1),binary_to_term(B)}])' -s init stop`):
/// prints `{131,121,{ok,17}}`, exit 0; a second call of the same expression is `=:=` the first.
/// Before the change `binary:at(B, 1)` raised the typed `unsupported_bif` gap (exit 3),
/// because the seam's default `local_node_hash` was `None`.
#[test]
fn term_to_binary_local_is_a_real_image_end_to_end() {
    let mut rt = runtime();
    let term = rt
        .eval(
            "begin B = term_to_binary({ok, 17}, [local]), \
             {binary:at(B, 0), binary:at(B, 1), binary_to_term(B), \
              B =:= term_to_binary({ok, 17}, [local])} end",
        )
        .expect("term_to_binary({ok,17},[local])");
    let printed = rt.format_term(term, None).expect("~p renders the probe tuple");
    assert_eq!(printed.trim_end(), "{131,121,{ok,17},true}");
}

/// The hash is stable for the whole OS process, so two **separate** `rt.eval` calls render the
/// same `~p` string (the [`super::node_hash::node_hash`] `OnceLock` pin).
#[test]
fn a_local_image_is_stable_across_two_evaluations() {
    let mut rt = runtime();
    let first = rt
        .eval("term_to_binary({ok, 17}, [local])")
        .expect("the first [local] image");
    let second = rt
        .eval("term_to_binary({ok, 17}, [local])")
        .expect("the second [local] image");
    assert_eq!(
        rt.format_term(first, None).expect("~p renders the first image"),
        rt.format_term(second, None).expect("~p renders the second image"),
        "the per-OS-process hash must not drift between calls"
    );
}

/// `LOCAL_EXT` replaces the internal node atom with `NIL_EXT`, and the decoder demotes it back to
/// the internal id — so an internal pid survives the local round trip.
///
/// Oracle: `binary_to_term(term_to_binary(self(),[local])) =:= self()` prints `true`.
#[test]
fn a_local_image_round_trips_an_internal_pid() {
    let mut rt = runtime();
    let term = rt
        .eval(
            "begin P = term_to_binary(self(), [local]), \
             {binary:at(P, 0), binary:at(P, 1), binary_to_term(P) =:= self()} end",
        )
        .expect("term_to_binary(self(),[local])");
    let printed = rt.format_term(term, None).expect("~p renders the probe tuple");
    assert_eq!(printed.trim_end(), "{131,121,true}");
}

/// [`super::node_hash::block_hash`] is a real mixing function: deterministic for the same
/// `(bytes, seed)` and different for a different input or a different seed.
#[test]
fn block_hash_is_deterministic_and_genuinely_mixing() {
    let seed = 0x1234_5678;
    assert_eq!(node_hash::block_hash(b"hostname42", seed), node_hash::block_hash(b"hostname42", seed));
    assert_ne!(node_hash::block_hash(b"hostname42", seed), node_hash::block_hash(b"hostname43", seed));
    assert_ne!(node_hash::block_hash(b"hostname42", seed), node_hash::block_hash(b"hostname42", seed ^ 1));
    assert_ne!(node_hash::block_hash(b"", 7), node_hash::block_hash(b"", 8));
}

/// [`super::node_hash::node_hash`] answers the SAME value on repeated calls and across two
/// independent runtimes in one OS process — the per-process `OnceLock`.
#[test]
fn node_hash_is_stable_within_an_os_process() {
    let first = node_hash::node_hash();
    assert_eq!(first, node_hash::node_hash(), "the same value on a repeat call");
    let rt = runtime();
    assert_eq!(node_hash::node_hash(), first, "a second runtime reports the same hash");
    assert_eq!(
        NodeIdentity { node: rt.atoms().lookup("nonode@nohost") }
            .local_node_hash(),
        Some(first),
        "the LocalNode seam answers the same per-process hash"
    );
}
#[test]
fn the_local_fun_identity_hook_is_a_clean_miss_when_the_answer_is_unknown() {
    let mut rt = runtime();
    let node = rt.atoms().lookup("nonode@nohost");
    // A boxed non-fun term: the runtime's own `term_to_binary(7)` image (a heap bitstring).
    let image = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("erlang", "term_to_binary", 1, &[make_small(7)])
            .expect("term_to_binary(7)")
    };
    // `beam_bifs` consults this hook for **every** `term_to_binary` argument, so an
    // immediate (`7`, `[]`) and a boxed non-fun (the image) must all be clean misses —
    // never an error, never a fabricated identity. With no calling module (the `-eval`
    // context) there is nothing to derive from at all.
    let mut hook = FunIdentityBridge::new(rt.code_server(), None, node);
    // `image` is a binary on the pooled, pointer-stable BIF heap, so it outlives `ctx`.
    for term in [make_small(7), NIL, image] {
        assert_eq!(
            hook.local_fun_identity(term).expect("a miss is never an error"),
            None,
            "a non-local-fun term must be a clean no-op"
        );
    }
}

#[test]
fn the_runtime_hooks_predicate_selects_only_its_own_bifs() {
    // The `RuntimeHooksBridge` arm is one of the mutually exclusive bridges installed per
    // call. Its gate is neither the list of keys the bridge **serves** nor a list kept here:
    // it is `beam_bifs`' own `RUNTIME_HOOKS_BIFS` roster (every BIF registered through the
    // `runtime_bif!` macro, each reading the single `RuntimeHooks` seam), so the two sides
    // cannot drift. The keys the bridge serves are the timer family, the loaded-export
    // check, the link set, the dynamic-tracing tag families, the code-store/loading probes,
    // the internal process predicates and `notsup` probe, the trace-cleaner/error-logger
    // constants, the `code:*` coverage surface and `erlang:statistics/1`; every other key of
    // the roster answers the same typed capability gap the *defaulted* seam reports (the
    // served-key behaviour is pinned by the `the_runtime_hooks_bridge_serves_*` tests).
    let served = [
        ("erlang", "start_timer", 3),
        ("erlang", "start_timer", 4),
        ("erlang", "send_after", 4),
        ("erlang", "cancel_timer", 2),
        ("erlang", "function_exported", 3),
        ("erlang", "link", 1),
        ("erlang", "unlink", 1),
        ("erlang", "dt_spread_tag", 1),
        ("erlang", "dt_restore_tag", 1),
        ("erlang", "dt_get_tag", 0),
        ("erlang", "dt_get_tag_data", 0),
        ("erlang", "dt_put_tag", 1),
        ("erlang", "dt_append_vm_tag_data", 1),
        ("erlang", "dt_prepend_vm_tag_data", 1),
        ("erlang", "system_profile", 0),
        ("erlang", "check_old_code", 1),
        ("erlang", "finish_loading", 1),
        ("erlang", "call_on_load_function", 1),
        ("erlang", "has_prepared_code_on_load", 1),
        ("erlang", "finish_after_on_load", 2),
        ("erts_internal", "is_system_process", 1),
        ("erts_internal", "is_process_executing_dirty", 1),
        ("erts_internal", "erase_persistent_terms", 0),
        ("erts_literal_area_collector", "release_area_switch", 0),
        ("erts_trace_cleaner", "check", 0),
        ("error_logger", "warning_map", 0),
        ("code", "coverage_support", 0),
        ("code", "get_coverage_mode", 0),
        ("code", "set_coverage_mode", 1),
        ("erlang", "statistics", 1),
    ];
    for (module, function, arity) in served {
        assert!(
            RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity}"
        );
        assert!(
            beam_bifs::RUNTIME_HOOKS_BIFS.contains(&(module, function, arity)),
            "{module}:{function}/{arity} is served by the bridge but absent from the roster"
        );
    }
    // Roster keys the bridge does **not** serve are claimed too — that is the point of
    // gating on the whole roster: `beam_bifs` can never route a key through the seam without
    // the bridge being installed. The reasons these are not in `served` are per-key gaps.
    for (module, function, arity) in [
        ("erlang", "read_timer", 1),
        ("erlang", "system_info", 1),
    ] {
        assert!(
            RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity} reads the seam, so the bridge must be installed"
        );
        assert!(
            !served.contains(&(module, function, arity)),
            "{module}:{function}/{arity} is not one of the keys the bridge serves"
        );
    }
    // Near misses — every one of them from `beam_bifs`' own must-miss set for this table:
    // a sibling arity of a roster key, an unrelated module's function of the same name, and
    // a key that belongs to another seam entirely (the memory hook, the ETS registry, the
    // module-info seam).
    for (module, function, arity) in [
        ("erlang", "start_timer", 2),
        ("erlang", "nodes", 3),
        ("erlang", "memory", 0),
        ("lists", "reverse", 1),
        ("ets", "lookup", 2),
        ("erlang", "get_module_info", 1),
    ] {
        assert!(
            !RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
    // Pairwise disjoint with every other mutually exclusive arm: each roster key needs
    // `&mut Runtime` plus a `&mut ProcessTable`, so none of the other bridges may claim it.
    for (function, arity) in served.iter().map(|(_, function, arity)| (*function, *arity)) {
        assert!(!RuntimeContext::uses_dict("erlang", function, arity));
        assert!(!RuntimeContext::uses_process_hook("erlang", function, arity));
        assert!(!RuntimeContext::uses_nif_loader("erlang", function, arity));
        assert!(!RuntimeContext::uses_record_source("erlang", function, arity));
        assert!(!RuntimeContext::uses_send_hook("erlang", function, arity));
        assert!(!RuntimeContext::uses_module_info("erlang", function, arity));
        assert!(!RuntimeContext::uses_persistent_term(
            "erlang", function, arity
        ));
    }
    // And symmetrically: none of the other arms' keys is claimed by this predicate.
    for (module, function, arity) in [
        ("erlang", "send", 2),
        ("erlang", "get", 1),
        ("erlang", "load_nif", 2),
        ("erlang", "get_module_info", 1),
        ("erlang", "whereis", 1),
        ("records", "get_definition", 2),
        ("persistent_term", "get", 1),
    ] {
        assert!(
            !RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
}

/// A `(module, function, arity)` BIF key.
type HookKey = (&'static str, &'static str, u32);

/// A `needs_*` / `uses_*` hook predicate over a BIF key.
type HookPred = fn(&str, &str, u32) -> bool;

/// One `beam_bifs` hook roster: its name, its keys, its own predicate, the `beam_rt` gate
/// that must delegate to it (`None` where this crate installs the seam on **every** arm, so
/// there is no gate: the port provider and the local-node identity) and the keys the
/// predicate must **not** claim — the same near misses `beam_bifs`' own `hook_sets` tests
/// pin.
struct HookRoster {
    name: &'static str,
    table: &'static [HookKey],
    needs: HookPred,
    gate: Option<HookPred>,
    misses: &'static [HookKey],
}

/// Every exported hook roster, so the agreement and disjointness assertions below are
/// written once against `beam_bifs`' own tables rather than a second copy of the lists.
fn hook_rosters() -> [HookRoster; 13] {
    [
        HookRoster {
            name: "MEMORY_HOOK_BIFS",
            table: beam_bifs::MEMORY_HOOK_BIFS,
            needs: beam_bifs::needs_memory_hook,
            gate: Some(RuntimeContext::uses_memory_hook as HookPred),
            misses: &[
                ("erlang", "memory", 2),
                ("erlang", "system_info", 1),
                ("lists", "memory", 1),
                ("erlang", "binary_to_term", 1),
            ],
        },
        HookRoster {
            name: "LOCAL_FUN_IDENTITY_BIFS",
            table: beam_bifs::LOCAL_FUN_IDENTITY_BIFS,
            needs: beam_bifs::needs_local_fun_identity,
            gate: Some(RuntimeContext::uses_fun_identity as HookPred),
            misses: &[
                ("erlang", "term_to_binary", 0),
                ("erlang", "term_to_binary", 3),
                ("erlang", "external_size", 0),
                ("erlang", "binary_to_term", 1),
                ("lists", "term_to_binary", 1),
                ("erlang", "-", 2),
            ],
        },
        HookRoster {
            name: "DICT_HOOK_BIFS",
            table: beam_bifs::DICT_HOOK_BIFS,
            needs: beam_bifs::needs_dict_hook,
            gate: Some(RuntimeContext::uses_dict as HookPred),
            misses: &[
                ("erlang", "put", 3),
                ("erlang", "erase", 2),
                ("erlang", "memory", 0),
                ("proc_lib", "put", 2),
                ("erlang", "process_info", 1),
            ],
        },
        HookRoster {
            name: "PROCESS_HOOK_BIFS",
            table: beam_bifs::PROCESS_HOOK_BIFS,
            needs: beam_bifs::needs_process_hook,
            gate: Some(RuntimeContext::uses_process_hook as HookPred),
            misses: &[
                ("erlang", "whereis", 0),
                ("erlang", "process_info", 3),
                ("erlang", "group_leader", 1),
                ("erlang", "nodes", 0),
                ("erlang", "get_module_info", 1),
            ],
        },
        HookRoster {
            name: "PORT_HOOK_BIFS",
            table: beam_bifs::PORT_HOOK_BIFS,
            needs: beam_bifs::needs_port_hook,
            // No gate: `call_registered` installs the port provider on every arm.
            gate: None,
            misses: &[
                ("erlang", "open_port", 1),
                ("erlang", "port_info", 3),
                ("erlang", "list_to_port", 1),
                ("erlang", "port_to_list", 1),
                ("erts_internal", "port_close", 1),
            ],
        },
        HookRoster {
            name: "SEND_HOOK_BIFS",
            table: beam_bifs::SEND_HOOK_BIFS,
            needs: beam_bifs::needs_send_hook,
            gate: Some(RuntimeContext::uses_send_hook as HookPred),
            misses: &[
                ("erlang", "send", 1),
                ("erlang", "self", 0),
                ("erlang", "!", 1),
                ("maps", "send", 2),
            ],
        },
        HookRoster {
            name: "NIF_LOADER_BIFS",
            table: beam_bifs::NIF_LOADER_BIFS,
            needs: beam_bifs::needs_nif_loader,
            gate: Some(RuntimeContext::uses_nif_loader as HookPred),
            misses: &[
                ("erlang", "load_nif", 1),
                ("erlang", "load_nif", 3),
                ("code", "load_nif", 2),
            ],
        },
        HookRoster {
            name: "RECORD_SOURCE_BIFS",
            table: beam_bifs::RECORD_SOURCE_BIFS,
            needs: beam_bifs::needs_record_source,
            gate: Some(RuntimeContext::uses_record_source as HookPred),
            misses: &[
                ("records", "get_definition", 1),
                ("records", "get", 2),
                ("records", "is_exported", 1),
                ("erlang", "get_definition", 2),
            ],
        },
        HookRoster {
            name: "MODULE_INFO_BIFS",
            table: beam_bifs::MODULE_INFO_BIFS,
            needs: beam_bifs::needs_module_info,
            gate: Some(RuntimeContext::uses_module_info as HookPred),
            misses: &[
                ("erlang", "get_module_info", 0),
                ("erlang", "get_module_info", 3),
                ("code", "get_module_info", 1),
            ],
        },
        HookRoster {
            name: "PERSISTENT_TERM_BIFS",
            table: beam_bifs::PERSISTENT_TERM_BIFS,
            needs: beam_bifs::needs_persistent_term,
            gate: Some(RuntimeContext::uses_persistent_term as HookPred),
            misses: &[
                ("persistent_term", "put", 3),
                ("persistent_term", "erase", 0),
                ("persistent_term", "put_new", 3),
                ("persistent_term", "keys", 0),
                ("erlang", "persistent_term", 1),
            ],
        },
        HookRoster {
            name: "FUN_CALLER_BIFS",
            table: beam_bifs::FUN_CALLER_BIFS,
            needs: beam_bifs::needs_fun_caller,
            // No gate: the fun caller is the fallback arm, installed for every key no gated
            // arm claims (`Self::uses_fun_caller` does not exist).
            gate: None,
            misses: &[
                ("lists", "map", 3),
                ("lists", "foldl", 2),
                ("lists", "usort", 1),
                ("lists", "sort", 1),
                ("lists", "reverse", 1),
                ("maps", "next", 1),
                ("maps", "merge", 2),
                ("sets", "map", 2),
            ],
        },
        HookRoster {
            name: "LOCAL_NODE_BIFS",
            table: beam_bifs::LOCAL_NODE_BIFS,
            needs: beam_bifs::needs_local_node,
            // No gate: the local-node identity is installed on every arm.
            gate: None,
            misses: &[
                ("erlang", "node", 2),
                ("erlang", "term_to_binary", 0),
                ("erlang", "list_to_pid", 1),
                ("erlang", "local_node", 0),
                ("erlang", "open_port", 2),
            ],
        },
        HookRoster {
            name: "RUNTIME_HOOKS_BIFS",
            table: beam_bifs::RUNTIME_HOOKS_BIFS,
            needs: beam_bifs::needs_runtime_hooks,
            gate: Some(RuntimeContext::uses_runtime_hooks as HookPred),
            misses: &[
                ("erlang", "start_timer", 2),
                ("erlang", "nodes", 3),
                ("erlang", "memory", 0),
                ("lists", "reverse", 1),
                ("ets", "lookup", 2),
                ("erlang", "get_module_info", 1),
            ],
        },
    ]
}

#[test]
fn the_rt_gates_delegate_to_the_exported_hook_predicates() {
    let rosters = hook_rosters();
    // Every key every roster claims, plus every near miss: the gate comparison below runs on
    // all of them, so a `beam_rt` gate can neither claim a key `beam_bifs` does not route
    // through its seam nor drop one it does.
    let keys: Vec<HookKey> = rosters
        .iter()
        .flat_map(|roster| roster.table.iter().chain(roster.misses.iter()).copied())
        .collect();
    for roster in &rosters {
        for key in roster.table {
            let (module, function, arity) = *key;
            assert!(
                (roster.needs)(module, function, arity),
                "{} must claim {module}:{function}/{arity}",
                roster.name
            );
        }
        for key in roster.misses {
            let (module, function, arity) = *key;
            assert!(
                !(roster.needs)(module, function, arity),
                "{} must reject {module}:{function}/{arity}",
                roster.name
            );
        }
        // The `beam_rt` gate agrees with the exported predicate on every key any roster
        // names — so a gate can neither claim a key `beam_bifs` does not route through its
        // seam nor drop one it does (`beam_bifs`' own `hook_sets` tests pin the tables
        // themselves against each module's `register`). A gate may still be **wider** than
        // its roster outside this key universe: the `local-fun-identity` gate also covers
        // `erlang:fun_info/1,2`, which the roster does not list — see
        // `the_fun_identity_gate_selects_the_image_and_introspection_bifs`. (`fun_info/1,2`
        // is in no roster, so the comparison below never meets it.)
        let Some(gate) = roster.gate else { continue };
        for key in &keys {
            let (module, function, arity) = *key;
            assert_eq!(
                gate(module, function, arity),
                (roster.needs)(module, function, arity),
                "the {} gate drifted from `beam_bifs` on {module}:{function}/{arity}",
                roster.name
            );
        }
    }
}

#[test]
fn the_mutually_exclusive_dispatch_arms_are_pairwise_disjoint() {
    // `call_registered` installs exactly **one** arm per call — first match wins — so two
    // arms claiming the same key would silently starve the later one's seam (a fun-taking
    // BIF routed to the dict arm would lose its fun caller, and so on). The nine arms are
    // the eight gated bridges plus the fallback (the fun caller).
    let arms: [(&str, HookPred); 9] = [
        ("DICT_HOOK_BIFS", beam_bifs::needs_dict_hook),
        ("PROCESS_HOOK_BIFS", beam_bifs::needs_process_hook),
        ("NIF_LOADER_BIFS", beam_bifs::needs_nif_loader),
        ("RECORD_SOURCE_BIFS", beam_bifs::needs_record_source),
        ("SEND_HOOK_BIFS", beam_bifs::needs_send_hook),
        ("MODULE_INFO_BIFS", beam_bifs::needs_module_info),
        ("PERSISTENT_TERM_BIFS", beam_bifs::needs_persistent_term),
        ("RUNTIME_HOOKS_BIFS", beam_bifs::needs_runtime_hooks),
        ("FUN_CALLER_BIFS", beam_bifs::needs_fun_caller),
    ];
    // Every roster key and every near miss is a probe: a key two arms share would surface
    // here even if neither arm's own table lists it.
    let rosters = hook_rosters();
    let keys: Vec<HookKey> = rosters
        .iter()
        .flat_map(|roster| roster.table.iter().chain(roster.misses.iter()).copied())
        .collect();
    for key in &keys {
        let (module, function, arity) = *key;
        let claimants: Vec<&str> = arms
            .iter()
            .filter(|(_, predicate)| predicate(module, function, arity))
            .map(|(name, _)| *name)
            .collect();
        assert!(
            claimants.len() <= 1,
            "{module}:{function}/{arity} is claimed by more than one mutually exclusive arm: {claimants:?}"
        );
    }
}

#[test]
fn the_runtime_hooks_bridge_serves_the_timer_and_export_families() {
    // End to end through the real dispatch: `erlang:start_timer/3` schedules on the
    // runtime's timer heap and answers the timer reference (the oracle's bare `#Ref<>`,
    // not `{ok, Ref}`), and `erlang:cancel_timer/2` accepts that same reference — so the
    // reference really indexes the runtime's timer heap rather than being fabricated.
    let mut rt = runtime();
    let me = rt.self_pid().to_eterm();
    let hi = rt.intern("hi");
    let ok = rt.intern("ok");
    let erlang = rt.intern("erlang");
    let start_timer = rt.intern("start_timer");
    let true_atom = rt.intern("true");
    let false_atom = rt.intern("false");
    let mut ctx = RuntimeContext::new(&mut rt);
    let reference = ctx
        .call_bif("erlang", "start_timer", 3, &[make_small(60_000), me, hi])
        .expect("erlang:start_timer/3");
    let cancelled = ctx
        .call_bif("erlang", "cancel_timer", 2, &[reference, NIL])
        .expect("erlang:cancel_timer/2");
    assert_eq!(cancelled, ok, "the default option list answers `ok`");
    // `erlang:function_exported/3` answers from this runtime's real registry (the
    // arity-shifted variant is not exported, so it answers `false`).
    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[erlang, start_timer, make_small(3)]
        )
        .expect("function_exported/3"),
        true_atom
    );
    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[erlang, start_timer, make_small(5)]
        )
        .expect("function_exported/3"),
        false_atom
    );
    // A key this bridge does **not** serve still answers a typed capability gap — never a
    // fabricated value. The gate claims `beam_bifs`' whole `RUNTIME_HOOKS_BIFS` roster, so
    // `read_timer/1` reaches the bridge (rather than the registry's defaulted seam) and the
    // bridge's fallback arm names the key in the reason instead of the bare `bif`.
    let error = ctx
        .call_bif("erlang", "read_timer", 1, &[reference])
        .expect_err("read_timer/1 is not served by this bridge");
    assert_eq!(
        crate::error::unsupported_reason(&error).as_deref(),
        Some("erlang:read_timer/1")
    );
}

// ---------------------------------------------------------------------------
// `erlang:link/1` / `erlang:unlink/1`: the pid-taking front door on a **remote** pid.
// ---------------------------------------------------------------------------

/// `erlang:link/1` and `erlang:unlink/1` with a **remote** pid: both answer the atom
/// `true`, and the relationship is recorded where the rest of the runtime reads it.
///
/// Oracle (OTP 29/`erts-17.1`, measured against a real `erl -sname` peer):
/// `erlang:link(RemotePid)` ⇒ `true` and `erlang:unlink(RemotePid)` ⇒ `true`. Before the
/// front-door change both answered the blanket capability gap
/// `unsupported: erlang:link/1 on a remote pid: distribution is not implemented`.
///
/// A runtime with no `-name`/`-sname` installs **no** `DistSignalRouter` on its process
/// table (`Runtime::new`), so `ProcessTable::route` is a no-op and the local record is the
/// whole observable effect — which is exactly the oracle's answer for this BIF, never a
/// fabricated success on top of a silently dropped signal. A non-pid stays `badarg`.
#[test]
fn the_link_and_unlink_front_doors_serve_a_remote_pid() {
    let mut rt = runtime();
    let me = rt.self_pid();
    let true_atom = rt.intern("true");
    // The term's words live on `_remote_heap`, so the heap must outlive every use below.
    let (_remote_heap, remote) = remote_pid(&mut rt, "peer@localhost");
    assert!(
        !rt.processes().has_dist_router(),
        "a runtime with no -name/-sname installs no router"
    );

    {
        let mut ctx = RuntimeContext::new(&mut rt);
        assert_eq!(
            ctx.call_bif("erlang", "link", 1, &[remote])
                .expect("erlang:link(RemotePid)"),
            true_atom,
            "link/1 on a remote pid answers `true`"
        );
    }
    assert_eq!(
        rt.processes().remote_links(me).len(),
        1,
        "the remote link is recorded on the caller"
    );
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        assert_eq!(
            ctx.call_bif("erlang", "unlink", 1, &[remote])
                .expect("erlang:unlink(RemotePid)"),
            true_atom,
            "unlink/1 on a remote pid answers `true`"
        );
    }
    assert!(
        rt.processes().remote_links(me).is_empty(),
        "the remote link is dropped again"
    );

    // The `badarg` rule for a non-pid is unchanged by the remote arm.
    let mut ctx = RuntimeContext::new(&mut rt);
    for function in ["link", "unlink"] {
        let error = ctx
            .call_bif("erlang", function, 1, &[make_small(1)])
            .expect_err("a non-pid is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{function}(1): {error}");
    }
}

/// The same two BIFs on a runtime that **is** a node but holds no established link to the
/// peer's node: the router's own typed transport error surfaces — never the stale blanket
/// `distribution is not implemented` gap, and never a fabricated `true`.
///
/// Construction only binds the listener; `epmd` is touched by the *listening* half's first
/// pump, which this case never runs. A sandbox that cannot bind a listener skips loudly
/// (the same rule `src/info/dist.rs`'s node case applies).
#[test]
fn the_link_and_unlink_front_doors_report_the_routers_own_error_with_no_link() {
    let config = RuntimeConfig::default().with_node_name("unitlink@localhost");
    let Ok(mut rt) = Runtime::new(config) else {
        eprintln!(
            "SKIP the_link_and_unlink_front_doors_report_the_routers_own_error_with_no_link: \
             could not bind a distribution listener"
        );
        return;
    };
    assert!(
        rt.processes().has_dist_router(),
        "a node installs the client's DistSignalRouter on the process table"
    );
    let (_remote_heap, remote) = remote_pid(&mut rt, "peer@localhost");
    let mut ctx = RuntimeContext::new(&mut rt);

    for function in ["link", "unlink"] {
        let error = ctx
            .call_bif("erlang", function, 1, &[remote])
            .expect_err("a remote pid with no link is an error, never a fabricated `true`");
        let reason = error.to_string();
        assert!(
            reason.contains("no established connection to node"),
            "{function}/1 must surface the router's own transport error, got {reason:?}"
        );
        assert!(
            !reason.contains("distribution is not implemented"),
            "{function}/1 must not fall back to the stale blanket gap, got {reason:?}"
        );
        assert!(
            crate::error::unsupported_reason(&error).is_none(),
            "{function}/1 on a connectable remote pid is an ordinary error, not a capability \
             gap: {reason:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// `erlang:function_exported/3`'s membership source.
//
// The C answer is `true` iff the module is **loaded and exports** `F/A` **or** `M:F/A` is
// an **emulator builtin** (`erts/emulator/beam/bif.c:4409-4424`: `erts_find_function` then
// `erts_is_builtin`). The builtin half is the *declared* `bif.tab` surface plus the extras
// the emulator registers in the `erlang` module — **never** the whole `beam_bifs` registry,
// which also carries this runtime's stdlib *fast paths* (implementations of real *Erlang*
// functions). Oracle answers (system `erl`, OTP 29/`erts-17.1`):
// `function_exported(erlang,memory,1)` `true`, `(erlang,system_info,1)` `true`,
// `(erlang,nonexistent,1)` `false`, `(lists,member,2)` `true`,
// `(mnesia,start,0)` `false` and `(string,chars,1)` `false`.
// ---------------------------------------------------------------------------

/// Prints a loud skip banner — a missing prerequisite is never a silent pass.
fn skip_builtin_case(banner: &str, why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt {banner} SKIPPED ===============");
    eprintln!("{why}. NOT a pass.");
    eprintln!("=========================================================");
    eprintln!();
}

/// The parsed real `bif.tab`, or `None` after a **loud skip** line.
///
/// A bare container without an OTP source tree cannot answer the declared-builtin cases,
/// so those tests skip visibly rather than passing.
fn bif_tab_or_skip(banner: &str) -> Option<&'static BifTab> {
    let tab = declared_bif_tab();
    if tab.is_none() {
        skip_builtin_case(
            banner,
            "erts/emulator/beam/bif.tab is not present in this checkout",
        );
    }
    tab
}

/// A stdlib **fast path** (`lists:map/2`) is NOT a builtin: with `lists` unloaded the answer
/// must be `false`. This is the regression test for the wrong-value bug (the registry holds
/// 640 keys, 79 of them stdlib fast paths, so `BifRegistry::contains` alone answered `true`).
#[test]
fn function_exported_does_not_report_a_stdlib_fast_path_as_exported() {
    let mut rt = runtime();
    // The premise, pinned: `lists:map/2` really is a registered key of this runtime and
    // `lists` really is not loaded, so the answer can only come from the builtin half.
    assert!(
        rt.registry.contains("lists", "map", 2),
        "lists:map/2 is a registry fast path"
    );
    assert!(
        rt.module("lists").is_none(),
        "lists must not be loaded in an un-booted runtime"
    );
    let lists = rt.intern("lists");
    let map = rt.intern("map");
    let member = rt.intern("member");
    let true_atom = rt.intern("true");
    let false_atom = rt.intern("false");
    // A declared `bif.tab` BIF of the same unloaded module still answers `true`; without a
    // readable table that half of the case cannot be asserted (the skip line is already out).
    let has_tab = bif_tab_or_skip("function_exported_does_not_report_a_stdlib_fast_path").is_some();
    let mut ctx = RuntimeContext::new(&mut rt);

    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[lists, map, make_small(2)]
        )
        .expect("function_exported(lists, map, 2)"),
        false_atom,
        "a stdlib fast path is not an emulator builtin"
    );

    if has_tab {
        assert_eq!(
            ctx.call_bif(
                "erlang",
                "function_exported",
                3,
                &[lists, member, make_small(2)]
            )
            .expect("function_exported(lists, member, 2)"),
            true_atom,
            "lists:member/2 is declared as a bif.tab BIF"
        );
    }
}

/// A **declared** `bif.tab` builtin answers `true` with the module unloaded; a name the
/// table does not declare (and no loaded module exports) answers `false`.
#[test]
fn function_exported_reports_a_declared_bif_tab_builtin() {
    let Some(_) = bif_tab_or_skip("function_exported_reports_a_declared_bif_tab_builtin") else {
        return;
    };
    let mut rt = runtime();
    assert!(
        rt.module("erlang").is_none(),
        "the un-booted runtime has no loaded `erlang` module, so the declared half decides"
    );
    let erlang = rt.intern("erlang");
    let system_info = rt.intern("system_info");
    let nonexistent = rt.intern("nonexistent");
    let true_atom = rt.intern("true");
    let false_atom = rt.intern("false");
    let mut ctx = RuntimeContext::new(&mut rt);

    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[erlang, system_info, make_small(1)]
        )
        .expect("function_exported(erlang, system_info, 1)"),
        true_atom,
        "erlang:system_info/1 is declared in bif.tab"
    );
    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[erlang, nonexistent, make_small(1)]
        )
        .expect("function_exported(erlang, nonexistent, 1)"),
        false_atom,
        "erlang:nonexistent/1 is neither declared nor registered"
    );
}

/// The **undeclared** `erlang`-namespace extras are builtins too: `erlang:memory/1` is not
/// in `bif.tab`, yet the emulator serves it and the oracle answers `true`.
#[test]
fn function_exported_reports_an_undeclared_erlang_builtin() {
    let mut rt = runtime();
    assert!(
        rt.registry.contains("erlang", "memory", 1),
        "erlang:memory/1 is registered in the `erlang` namespace"
    );
    assert!(
        rt.module("erlang").is_none(),
        "the un-booted runtime has no loaded `erlang` module"
    );
    // The premise, pinned against the real table when it is readable.
    if let Some(tab) = bif_tab_or_skip("function_exported_reports_an_undeclared_erlang_builtin") {
        assert!(
            !tab.contains("erlang", "memory", 1),
            "erlang:memory/1 must stay undeclared, or this test stops exercising the extras"
        );
    }
    let erlang = rt.intern("erlang");
    let memory = rt.intern("memory");
    let true_atom = rt.intern("true");
    let mut ctx = RuntimeContext::new(&mut rt);

    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[erlang, memory, make_small(1)]
        )
        .expect("function_exported(erlang, memory, 1)"),
        true_atom,
        "the `erlang`-namespace extras are emulator builtins"
    );
}

/// The predicate itself, exercised with an **absent** `bif.tab`: the declared half is
/// unavailable, so a non-`erlang` registry key is never claimed as a builtin, while the
/// `erlang`-namespace extras and the real table's declarations keep working.
#[test]
fn a_builtin_predicate_without_bif_tab_never_claims_a_foreign_builtin() {
    let rt = runtime();
    assert!(
        rt.registry.contains("lists", "map", 2),
        "lists:map/2 is a registry fast path"
    );
    // The absent-table fallback: no declared half, so a stdlib fast path is not a builtin…
    assert!(
        !is_builtin(None, &rt.registry, "lists", "map", 2),
        "without bif.tab a non-`erlang` fast path must never be reported as a builtin"
    );
    // …the `erlang`-namespace extras are still honoured…
    assert!(is_builtin(None, &rt.registry, "erlang", "memory", 1));
    // …and an `erlang` key the registry does not hold is still `false`.
    assert!(!is_builtin(None, &rt.registry, "erlang", "nonexistent", 1));
    // The readable-table path honours both halves.
    if let Some(tab) = declared_bif_tab() {
        assert!(is_builtin(Some(tab), &rt.registry, "lists", "member", 2));
        assert!(!is_builtin(Some(tab), &rt.registry, "lists", "map", 2));
        assert!(is_builtin(Some(tab), &rt.registry, "erlang", "system_info", 1));
    }
}

/// The loaded-export half still works: a module that really is loaded and exports `F/A`
/// answers `true`, while an unloaded module (and an unknown function of a loaded one)
/// answers `false`.
#[test]
fn function_exported_sees_a_loaded_modules_real_exports() {
    let mut rt = runtime();
    if let Err(error) = rt.code_server.add_system_paths() {
        skip_builtin_case(
            "function_exported_sees_a_loaded_modules_real_exports",
            &format!("no system code path ({error})"),
        );
        return;
    }
    rt.load_code_module("lists").expect("stdlib lists loads");
    let lists = rt.intern("lists");
    let map = rt.intern("map");
    let no_such = rt.intern("no_such_fun_xyz");
    let mnesia = rt.intern("mnesia");
    let start = rt.intern("start");
    let true_atom = rt.intern("true");
    let false_atom = rt.intern("false");
    let mut ctx = RuntimeContext::new(&mut rt);

    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[lists, map, make_small(2)]
        )
        .expect("function_exported(lists, map, 2)"),
        true_atom,
        "the loaded `lists` module really exports map/2"
    );
    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[lists, no_such, make_small(1)]
        )
        .expect("function_exported(lists, no_such_fun_xyz, 1)"),
        false_atom,
        "a function the loaded module does not export answers false"
    );
    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[mnesia, start, make_small(0)]
        )
        .expect("function_exported(mnesia, start, 0)"),
        false_atom,
        "an unloaded module answers false"
    );
}
#[test]
fn the_runtime_hooks_bridge_serves_the_dynamic_tag_and_statistics_arms() {
    // The dynamic-tracing tag pair `file:call/2` brackets every file-server round trip with
    // (`lib/kernel/src/file.erl:3040-3043`). With dynamic tracing compiled out the oracle's
    // two BIFs are pure argument checks (`erts/emulator/beam/bif.c:6080-6112` / `6162-6167`):
    // `dt_spread_tag/1` accepts `true` and `false` and answers `true`, while
    // `dt_restore_tag/1` accepts **only** `true`. Both are cross-checked against the live
    // oracle by `tests/runtime_hooks.rs`.
    let mut rt = runtime();
    let true_atom = rt.intern("true");
    let false_atom = rt.intern("false");
    let run_queue = rt.intern("run_queue");
    let run_queues = rt.intern("run_queues");
    let run_queue_lengths = rt.intern("run_queue_lengths");
    let run_queue_lengths_all = rt.intern("run_queue_lengths_all");
    let total_run_queue_lengths = rt.intern("total_run_queue_lengths");
    let total_run_queue_lengths_all = rt.intern("total_run_queue_lengths_all");
    let scheduler_wall_time_all = rt.intern("scheduler_wall_time_all");
    let undefined = rt.intern("undefined");
    let active_tasks = rt.intern("active_tasks");
    let microstate_accounting = rt.intern("microstate_accounting");
    let wall_clock = rt.intern("wall_clock");
    let runtime_key = rt.intern("runtime");
    let garbage_collection = rt.intern("garbage_collection");
    let atom_count = rt.intern("atom_count");
    // Captured before the context takes its `&mut Runtime` borrow: a fresh runtime queues
    // nothing, so this is the run-queue figure `statistics(run_queue)` must reproduce.
    let queued = rt.run_queue_len();
    let mut ctx = RuntimeContext::new(&mut rt);

    assert_eq!(queued, 0, "a fresh runtime queues no process");
    for tag in [true_atom, false_atom] {
        assert_eq!(
            ctx.call_bif("erlang", "dt_spread_tag", 1, &[tag])
                .expect("dt_spread_tag/1"),
            true_atom,
            "dt_spread_tag/1 accepts both booleans and answers `true`"
        );
    }
    assert_eq!(
        ctx.call_bif("erlang", "dt_restore_tag", 1, &[true_atom])
            .expect("dt_restore_tag(true)"),
        true_atom
    );
    // A non-boolean is `badarg` for the spreader; for the restorer `false` is one too.
    for (function, argument) in [
        ("dt_spread_tag", make_small(42)),
        ("dt_spread_tag", NIL),
        ("dt_restore_tag", false_atom),
        ("dt_restore_tag", make_small(42)),
    ] {
        let error = ctx
            .call_bif("erlang", function, 1, &[argument])
            .expect_err("a rejected tag is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{function}: {error}");
    }

    // `statistics(run_queue)` reads this runtime's **real** run queue, so it reproduces the
    // accessor's own figure rather than a constant.
    assert_eq!(
        ctx.call_bif("erlang", "statistics", 1, &[run_queue])
            .expect("statistics(run_queue)"),
        make_small(queued as i64)
    );

    // The whole run-queue family reads the same real queue: `run_queues` is the per-queue
    // **tuple** (one element — this runtime has one run queue and no dirty schedulers),
    // `run_queue_lengths`/`_all` the per-queue **list**, and `total_*` the sum.
    let value = ctx
        .call_bif("erlang", "statistics", 1, &[run_queues])
        .expect("statistics(run_queues)");
    assert_eq!(terms::tuple_arity(value).expect("a 1-tuple"), 1, "one real run queue");
    assert_eq!(
        terms::tuple_element(value, 0).expect("the queue length"),
        make_small(queued as i64)
    );
    for key in [run_queue_lengths, run_queue_lengths_all] {
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect("statistics(run_queue_lengths[_all])");
        assert_eq!(
            terms::list_elements(value).expect("a proper list"),
            vec![make_small(queued as i64)],
            "one element per real run queue"
        );
    }
    for key in [total_run_queue_lengths, total_run_queue_lengths_all] {
        assert_eq!(
            ctx.call_bif("erlang", "statistics", 1, &[key])
                .expect("statistics(total_run_queue_lengths[_all])"),
            make_small(queued as i64),
            "the sum over the one real run queue"
        );
    }

    // `scheduler_wall_time_all` is the `_all` sibling of `scheduler_wall_time`: the oracle's
    // own "not enabled" answer, exactly as this runtime never enables it.
    assert_eq!(
        ctx.call_bif("erlang", "statistics", 1, &[scheduler_wall_time_all])
            .expect("statistics(scheduler_wall_time_all)"),
        undefined,
        "never enabled, so `undefined` is exact"
    );

    // `garbage_collection` is the C's `{GCs, WordsReclaimed, 0}` shape, read from this
    // runtime's own node-global GC counters. A fresh runtime has run no collection yet — the
    // real-counter direction (a collection that really ran, and the words it reclaimed) is
    // pinned by
    // `statistics_garbage_collection_reads_the_runtimes_own_collector_counters`.
    let value = ctx
        .call_bif("erlang", "statistics", 1, &[garbage_collection])
        .expect("statistics(garbage_collection)");
    assert_eq!(terms::tuple_arity(value).expect("a 3-tuple"), 3);
    assert_eq!(terms::tuple_element(value, 0).expect("GCs"), make_small(0));
    assert_eq!(terms::tuple_element(value, 1).expect("WordsReclaimed"), make_small(0));
    assert_eq!(
        terms::tuple_element(value, 2).expect("the reserved third element"),
        make_small(0),
        "the C's `gather_gc_info_result/1` always answers 0 here"
    );

    // A valid statistics key this runtime cannot measure is the typed capability gap (never
    // a fabricated number); a name that is not a statistics key at all is the oracle's
    // `badarg`. `active_tasks*` needs the per-run-queue "a scheduler is executing from this
    // queue" state `beam_process::Scheduler` does not keep, and `microstate_accounting` the
    // MSACC per-thread machinery; the counters it *can* measure are served (see
    // `the_served_statistics_counters_read_the_runtimes_own_state`).
    for (key, name) in [
        (active_tasks, "active_tasks"),
        (microstate_accounting, "microstate_accounting"),
    ] {
        let error = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect_err("an unmeasurable statistics key is an honest gap");
        let expected = format!("erlang:statistics/1: the `{name}` counter is not implemented");
        assert_eq!(
            crate::error::unsupported_reason(&error).as_deref(),
            Some(expected.as_str()),
            "the gap names the counter"
        );
    }
    for key in [atom_count, make_small(1)] {
        let error = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect_err("an unknown statistics key is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

    // `statistics(wall_clock)` is a real monotonic clock: `{Total, SinceLastRead}`, both
    // whole milliseconds. Sleeping between two reads must advance the total and make the
    // delta positive — a fabricated or frozen counter would fail this.
    let wall_clock_pair = |ctx: &mut RuntimeContext<'_>| {
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[wall_clock])
            .expect("statistics(wall_clock)");
        assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2);
        let total = terms::tuple_element(value, 0).expect("the total");
        let since = terms::tuple_element(value, 1).expect("the delta");
        assert!(is_small(total) && is_small(since), "two small integers");
        (signed_value(total), signed_value(since))
    };
    let (first_total, first_since) = wall_clock_pair(&mut ctx);
    assert!(first_total >= 0 && first_since >= 0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (second_total, second_since) = wall_clock_pair(&mut ctx);
    assert!(
        second_total > first_total,
        "the wall clock advanced: {second_total} > {first_total}"
    );
    assert!(
        second_since > 0,
        "the delta since the previous read is real: {second_since}"
    );

    // `statistics(runtime)` is served from the runtime's one long-lived CPU-time clock
    // (`beam_io::cpu_time::RuntimeClock`): `{Total, SinceLastCall}` in **milliseconds of user
    // CPU time** (the C's `getrusage(RUSAGE_SELF)` measurement), so both elements are small
    // integers. The clock's anchor starts at 0, so the very first read answers two equal
    // elements — exactly the C's `runtime_prev.data.user = 0` semantics.
    let runtime_pair = |ctx: &mut RuntimeContext<'_>| {
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[runtime_key])
            .expect("statistics(runtime)");
        assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2);
        let total = terms::tuple_element(value, 0).expect("the total");
        let since = terms::tuple_element(value, 1).expect("the delta");
        assert!(is_small(total) && is_small(since), "two small integers");
        (signed_value(total), signed_value(since))
    };
    let (cpu_total, cpu_since) = runtime_pair(&mut ctx);
    assert_eq!(
        cpu_since, cpu_total,
        "the first read's delta is the whole total (the clock anchor starts at 0)"
    );
    assert!(cpu_total >= 0, "a CPU-time reading is non-negative");

    // A valid statistics key this runtime cannot measure is the typed capability gap (never
    // a fabricated number); a name that is not a statistics key at all is the oracle's
    // `badarg`. These two are asserted above, next to the served statistics arms.
}

/// `statistics(garbage_collection)` reads the runtime's **node-global** GC counters
/// ([`Runtime::gc_stats`]), read through the **real dispatch** (the
/// [`RuntimeHooksBridge`](crate::context) arm) as `(GCs, WordsReclaimed, Reserved)`.
///
/// A fresh runtime has run no collection at all — the C's own `{0, 0, 0}` shape — and a
/// collection that really ran through [`Runtime::serve_gc_request`] moves **both** served
/// figures by its own outcome: the cumulative collection count and the words that collection
/// really reclaimed. It is deliberately **not** read from the live processes' collectors (which
/// are empty of collections precisely when the workload's collections are the interesting
/// ones); the runtime keeps the node-global totals itself. The same contract is pinned at the
/// runtime level by `src/runtime/gc.rs`'s
/// `statistics_garbage_collection_reads_the_node_global_collection_counters`.
///
/// No oracle comparison here: the crate's test strategy forbids comparing live GC counters
/// against the oracle, so every figure below is this runtime's own.
#[test]
fn statistics_garbage_collection_reads_the_runtimes_own_collector_counters() {
    let mut rt = runtime();
    let garbage_collection = rt.intern("garbage_collection");

    // The served key, through the one real dispatch, as a 3-tuple of small integers.
    let read = |rt: &mut Runtime| -> (Eterm, Eterm, Eterm) {
        let mut ctx = RuntimeContext::new(rt);
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[garbage_collection])
            .expect("statistics(garbage_collection) is a served key");
        assert_eq!(terms::tuple_arity(value).expect("a 3-tuple"), 3);
        (
            terms::tuple_element(value, 0).expect("GCs"),
            terms::tuple_element(value, 1).expect("WordsReclaimed"),
            terms::tuple_element(value, 2).expect("the reserved third element"),
        )
    };

    // A fresh runtime has collected nothing: all three elements are real zeros.
    let (gcs, words, reserved) = read(&mut rt);
    assert_eq!(gcs, make_small(0), "a fresh runtime has collected nothing");
    assert_eq!(words, make_small(0), "and so reclaimed nothing");
    assert_eq!(reserved, make_small(0), "the C's reserved third element");

    // A real collection through the forced-GC latch: `serve_gc_request` takes a **standalone**
    // `ProcessTable` and collects the live process's own young heap. The heap carries an
    // unrooted (garbage) tuple, which the collection frees rather than copies.
    let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("a process");
    table
        .get_mut(pid)
        .expect("the process is live")
        .heap_mut()
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("a scratch tuple on the young heap");
    rt.request_gc(pid);
    assert!(
        rt.serve_gc_request(&mut table, pid),
        "the forced collection really ran"
    );

    let stats = rt.gc_stats();
    assert!(stats.collections >= 1, "a collection really ran: {stats:?}");
    assert!(
        stats.reclaimed > 0,
        "the unrooted tuple was garbage the collector reclaimed: {stats:?}"
    );

    // The served answer is exactly the runtime's own cumulative counters.
    let (gcs, words, reserved) = read(&mut rt);
    assert_eq!(
        signed_value(gcs),
        i64::try_from(stats.collections).expect("a small collection count"),
        "the runtime's own cumulative collection count"
    );
    assert_eq!(
        signed_value(words),
        i64::try_from(stats.reclaimed).expect("a small reclaimed-word count"),
        "the words those collections really reclaimed"
    );
    assert_eq!(reserved, make_small(0), "the C's reserved third element");
}

/// The counters this runtime keeps for `erlang:statistics/1` are read from its **own real
/// state**: the node-global reduction/context-switch counters the runner and the scheduler
/// charge, and the port subsystem's byte counters. Every figure is a real measurement — a
/// fabricated or frozen counter would fail these assertions.
#[test]
fn the_served_statistics_counters_read_the_runtimes_own_state() {
    let mut rt = runtime();
    // The two cumulative counters are credited by their real charge sites
    // (`Runtime::note_reductions`/`note_context_switches`, driven end to end by the
    // scheduler unit tests); here the anchor and read semantics are pinned directly.
    rt.note_reductions(7);
    rt.note_context_switches(3);
    let reductions = rt.intern("reductions");
    let exact_reductions = rt.intern("exact_reductions");
    let context_switches = rt.intern("context_switches");
    let io = rt.intern("io");
    let scheduler_wall_time = rt.intern("scheduler_wall_time");
    let input = rt.intern("input");
    let output = rt.intern("output");
    let undefined = rt.intern("undefined");
    let mut ctx = RuntimeContext::new(&mut rt);

    // A `{Total, SinceLast}` read of a served counter key.
    let pair = |ctx: &mut RuntimeContext<'_>, key: Eterm| {
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect("a served statistics key");
        assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2);
        let total = terms::tuple_element(value, 0).expect("the total");
        let since = terms::tuple_element(value, 1).expect("the delta");
        assert!(is_small(total) && is_small(since), "two small integers");
        (signed_value(total), signed_value(since))
    };

    // `reductions`: the first read's delta is the whole total (the anchor starts at zero) and
    // the very next read of the same key is a zero delta.
    assert_eq!(pair(&mut ctx, reductions), (7, 7));
    assert_eq!(pair(&mut ctx, reductions), (7, 0));
    // `exact_reductions` names the same quantity but keeps its **own** anchor, so it has not
    // been read yet — and reading it does not consume `reductions`' cursor.
    assert_eq!(pair(&mut ctx, exact_reductions), (7, 7));
    assert_eq!(pair(&mut ctx, reductions), (7, 0));
    assert_eq!(pair(&mut ctx, exact_reductions), (7, 0));
    // `context_switches`, same rule.
    assert_eq!(pair(&mut ctx, context_switches), (3, 3));
    assert_eq!(pair(&mut ctx, context_switches), (3, 0));

    // `scheduler_wall_time` is the oracle's own "not enabled" answer.
    assert_eq!(
        ctx.call_bif("erlang", "statistics", 1, &[scheduler_wall_time])
            .expect("scheduler_wall_time"),
        undefined
    );

    // `io` is `{{input, In}, {output, Out}}`; this runtime has written nothing to a port and
    // read nothing from one, so both byte counts are the honest zero.
    let value = ctx
        .call_bif("erlang", "statistics", 1, &[io])
        .expect("statistics(io)");
    let in_pair = terms::tuple_element(value, 0).expect("the input pair");
    let out_pair = terms::tuple_element(value, 1).expect("the output pair");
    assert_eq!(terms::tuple_element(in_pair, 0).expect("the tag"), input);
    assert_eq!(terms::tuple_element(out_pair, 0).expect("the tag"), output);
    assert_eq!(
        signed_value(terms::tuple_element(in_pair, 1).expect("the input count")),
        0
    );
    assert_eq!(
        signed_value(terms::tuple_element(out_pair, 1).expect("the output count")),
        0
    );
}

/// `erts_internal:scheduler_wall_time/1` is the C's `atomic_xchg` on the node-global
/// `sched_wall_time` switch (`erts/emulator/beam/bif.c:5357-5366`): **any** argument is
/// accepted (only the atom `true` enables — every other term, `false`/`foo`/an integer,
/// disables, never a `badarg`) and the answer is the state held *before* the call, as the
/// atom `true`/`false`.
///
/// Oracle-verified (OTP 29/`erts-17.1`), one VM, in order —
/// `scheduler_wall_time(true)` → `false`, `(false)` → `true`, `(true)` → `false`,
/// `(foo)` → `true`, `(foo)` → `false`; on a fresh VM each of `(true)`/`(false)`/`(foo)`/`(1)`
/// answers `false` (the switch starts disabled). `kernel_refc`'s
/// `resource(scheduler_wall_time, Enable)` (`lib/kernel/src/kernel_refc.erl:141-142`) is the
/// boot-path caller and `kernel_sup` starts it `permanent`, so a raise here tears `kernel_sup`
/// down with `start_error`.
///
/// The switch is **recorded but never sampled**: `erlang:statistics(scheduler_wall_time)`
/// keeps answering `undefined` even while armed — this runtime measures no per-scheduler wall
/// time, so a fabricated `{SchedulerId, ActiveTime, TotalTime}` tuple list would be a wrong
/// result (`tests/runtime_hooks.rs` cross-checks the disabled answer against the live oracle).
#[test]
fn the_runtime_hooks_bridge_serves_the_scheduler_wall_time_switch() {
    let mut rt = runtime();
    let on = rt.intern("true");
    let off = rt.intern("false");
    let other = rt.intern("foo");
    let undefined = rt.intern("undefined");
    let statistics_key = rt.intern("scheduler_wall_time");
    let mut ctx = RuntimeContext::new(&mut rt);

    let call = |ctx: &mut RuntimeContext<'_>, arg: Eterm| {
        ctx.call_bif("erts_internal", "scheduler_wall_time", 1, &[arg])
            .expect("scheduler_wall_time/1 accepts any argument")
    };
    let stat = |ctx: &mut RuntimeContext<'_>| {
        ctx.call_bif("erlang", "statistics", 1, &[statistics_key])
            .expect("statistics(scheduler_wall_time)")
    };

    // Force the switch off first so the sequence is independent of any preceding call.
    let _ = call(&mut ctx, off);

    // The exchange answers the *previous* state: enabling a disabled switch answers `false`,
    // disabling an enabled one answers `true`.
    assert_eq!(call(&mut ctx, on), off, "arming answers the previous (disabled) state");
    assert_eq!(call(&mut ctx, off), on, "disarming answers the previous (armed) state");
    assert_eq!(call(&mut ctx, on), off);

    // A non-`true` term disables exactly like `false` — never a `badarg`.
    assert_eq!(call(&mut ctx, other), on, "`foo` disabled the armed switch");
    assert_eq!(call(&mut ctx, other), off, "and is now the previous, disabled state");
    // An integer is a disabling request too, not a `badarg`.
    assert_eq!(call(&mut ctx, make_small(1)), off);
    assert_eq!(call(&mut ctx, on), off, "re-arming a disabled switch");
    // Still armed: a second `true` answers the previous (armed) state, which proves the switch
    // really took the `true` above.
    assert_eq!(call(&mut ctx, on), on, "the switch really is armed");

    // The switch is recorded but drives no sampling: the statistics reader answers `undefined`
    // while armed (here) and while disabled (below).
    assert_eq!(
        stat(&mut ctx),
        undefined,
        "no per-scheduler wall time is measured even while the switch is armed"
    );
    let _ = call(&mut ctx, off);
    assert_eq!(stat(&mut ctx), undefined);
}
/// The guarded call-site imports, and the **scope** of the refusal each one forces
/// ([`callee_observes_its_call_site`]): a module importing `erlang:load_nif/2` keeps **every**
/// function off the in-machine route (a local call inside the module reaches the loader and a
/// local `call` never re-consults the seam), while the fun-image BIFs
/// (`erlang:term_to_binary/1,2`, `erlang:external_size/1,2`) refuse only the callee **function**
/// whose own lowered body issues the call — everything else about that module is entered
/// in-machine. Every other import is ignored.
#[test]
fn the_call_site_and_guarded_imports_keep_a_callee_off_the_in_machine_route() {
    /// A synthetic module importing exactly `imports` (as `{module, function, arity}`).
    fn module_importing(imports: &[(&str, &str, u32)]) -> Module {
        let mut module = Module::default();
        for (name, function, arity) in imports {
            let module_atom = module.atom_table.intern(name);
            let function_atom = module.atom_table.intern(function);
            module.imports.push(beam_loader::Import {
                module: module_atom,
                function: function_atom,
                arity: *arity,
            });
        }
        module
    }

    /// The seam's verdict for `module`'s `function`/`arity`, interning the name on demand.
    fn refused(module: &mut Module, function: &str, arity: u32) -> bool {
        let function_atom = module.atom_table.intern(function);
        callee_observes_its_call_site(module, function_atom, arity)
    }

    /// A `name`/`arity` function over exactly `code` (the guard reads neither label, so both are
    /// `0`).
    fn function_over(module: &mut Module, name: &str, arity: u32, code: Vec<Instr>) -> Function {
        let name = module.atom_table.intern(name);
        Function {
            func_label: Label(0),
            entry_label: Label(0),
            module: module.name,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels: std::collections::BTreeMap::new(),
        }
    }

    // ---- the MODULE-wide scope: `erlang:load_nif/2` ---------------------------------------
    // Refused for every function of the module, whether or not the callee's own body issues the
    // call: a local `call` inside the module reaches the loader without re-consulting the seam,
    // so nothing narrower is sound.
    let mut load_nif = module_importing(&[("erlang", "load_nif", 2)]);
    for (function, arity) in [("load", 1), ("init", 0), ("other", 3)] {
        assert!(
            refused(&mut load_nif, function, arity),
            "load_nif/2 keeps the whole module off the in-machine route ({function}/{arity})"
        );
    }
    // The same *names* in a foreign module, a different arity of the same name, and every
    // unrelated import are all not the runtime's guarded BIFs — for **any** callee function.
    for ordinary in [
        ("erlang", "load_nif", 1),
        ("erlang", "load_nif", 3),
        ("erlang", "term_to_binary", 0),
        ("erlang", "term_to_binary", 3),
        ("erlang", "external_size", 0),
        ("erlang", "external_size", 3),
        ("some_module", "load_nif", 2),
        ("some_module", "term_to_binary", 1),
        ("erlang", "+", 2),
        ("erlang", "send", 2),
    ] {
        assert!(
            !refused(&mut module_importing(&[ordinary]), "any", 0),
            "{ordinary:?} must not keep a module off the in-machine route"
        );
    }

    // ---- the PER-FUNCTION scope: the fun-image BIFs ---------------------------------------
    // A module importing one but defining no function of the callee's name/arity keeps the
    // module-wide verdict — the conservative fallback that never admits what the guard refused
    // before.
    for fun_image in [
        ("erlang", "term_to_binary", 1),
        ("erlang", "term_to_binary", 2),
        ("erlang", "external_size", 1),
        ("erlang", "external_size", 2),
    ] {
        let mut module = module_importing(&[fun_image]);
        assert!(
            refused(&mut module, "encode", 1),
            "{fun_image:?}: an unlocatable callee function keeps the module-wide verdict"
        );
    }

    // One import, two functions: only the function whose **own** body issues the call is
    // refused, and every instruction form carrying the import counts.
    for observing in [
        Instr::CallExt {
            arity: 1,
            target: ImportRef(0),
        },
        Instr::CallExtOnly {
            arity: 1,
            target: ImportRef(0),
        },
        Instr::CallExtLast {
            arity: 1,
            target: ImportRef(0),
            deallocate: 0,
        },
        Instr::Bif0 {
            bif: ImportRef(0),
            dst: Operand::X(0),
        },
        Instr::Bif {
            arity: 1,
            bif: ImportRef(0),
            fail: Label(0),
            args: vec![Operand::X(0)],
            dst: Operand::X(0),
        },
        Instr::GcBif {
            arity: 1,
            bif: ImportRef(0),
            fail: Label(0),
            live: 1,
            args: vec![Operand::X(0)],
            dst: Operand::X(0),
        },
    ] {
        let mut module = module_importing(&[("erlang", "term_to_binary", 1)]);
        let enc = function_over(&mut module, "enc", 1, vec![observing.clone()]);
        let plain = function_over(&mut module, "plain", 0, Vec::new());
        module.functions.push(enc);
        module.functions.push(plain);
        assert!(
            refused(&mut module, "enc", 1),
            "{observing:?} in the callee's own body refuses the callee"
        );
        assert!(
            !refused(&mut module, "plain", 0),
            "a non-observing function of the same module is entered in-machine"
        );
    }

    // An import-carrying instruction whose `ImportRef` is out of range names no import at all,
    // so it must not panic and must not refuse (a malformed image never fabricates a refusal).
    let mut malformed = module_importing(&[("erlang", "term_to_binary", 1)]);
    let bad = function_over(
        &mut malformed,
        "enc",
        1,
        vec![Instr::CallExt {
            arity: 1,
            target: ImportRef(7),
        }],
    );
    malformed.functions.push(bad);
    assert!(
        !refused(&mut malformed, "enc", 1),
        "an out-of-range ImportRef observes nothing"
    );

    // `load_nif` dominates a module importing both, in **either** order: the whole module is
    // refused even for a function whose own body issues nothing.
    for imports in [
        [("erlang", "load_nif", 2), ("erlang", "term_to_binary", 1)],
        [("erlang", "term_to_binary", 1), ("erlang", "load_nif", 2)],
    ] {
        let mut module = module_importing(&imports);
        let plain = function_over(&mut module, "plain", 0, Vec::new());
        module.functions.push(plain);
        assert!(
            refused(&mut module, "plain", 0),
            "{imports:?}: load_nif/2 dominates the fun-image scope"
        );
    }

    // The scan is "any import matches", not "the first does", and an empty import list observes
    // nothing.
    let mut mixed = module_importing(&[
        ("erlang", "+", 2),
        ("lists", "map", 2),
        ("erlang", "load_nif", 2),
    ]);
    assert!(refused(&mut mixed, "load", 1));
    let mut unrelated = module_importing(&[
        ("erlang", "+", 2),
        ("lists", "map", 2),
        ("erlang", "term_to_binary", 1),
    ]);
    let enc = function_over(
        &mut unrelated,
        "enc",
        1,
        vec![Instr::CallExt {
            arity: 1,
            target: ImportRef(2),
        }],
    );
    unrelated.functions.push(enc);
    assert!(
        refused(&mut unrelated, "enc", 1),
        "the guarded import is found after unrelated ones"
    );
    assert!(!refused(&mut module_importing(&[]), "any", 0));
}

/// The `obs_split` fixture: one module, two functions — `enc/1` issues
/// `erlang:term_to_binary/1` in its **own** body, `plain/0` does not.
const SEAM_OBS_SPLIT_SRC: &str = "-module(obs_split).
-export([plain/0, enc/1]).
plain() -> ok.
enc(Term) -> term_to_binary(Term).
";

/// **The guard's scope on a real `.beam`: per-function for the fun-image BIFs.** The cross-module
/// seam admits the non-observing function of a module that imports `erlang:term_to_binary/1`
/// while still refusing the function whose own body issues the call. That narrowing is what lets
/// `compile:forms/2`/`compile:file/2` be entered in-machine (the A.9 self-hosted-build blocker)
/// instead of folding a parked `receive` into the typed `blocked()` gap on the nested,
/// non-pumping `call_bif` route.
///
/// The fixture is compiled for real, so the case also pins the two halves the per-function guard
/// scans: the compiler really records the call as an `ImpT` import **and** as an import-carrying
/// instruction in that one function's lowered body.
#[test]
fn the_call_seam_admits_a_non_observing_function_of_a_fun_image_module() {
    let Some(erlc) = resolve_erlc() else {
        skip_loudly(
            "per-function call-site scope",
            "no `erlc` (and none beside $ERTS_ORACLE either)",
        );
        return;
    };
    let dir = compile_fixtures(
        &erlc,
        "call_site_scope",
        &[("obs_split", SEAM_OBS_SPLIT_SRC)],
    );

    let mut rt = runtime();
    rt.add_code_path(dir.path());
    let mut ctx = RuntimeContext::new(&mut rt);

    // `plain/0` first: it is uncached here, so the seam loads the module on demand — which also
    // caches it for the `enc/1` consultations below (the arm the JIT's second consultation
    // takes). Both functions answer the same on both consultations (the seam is idempotent).
    for consultation in 1..=2 {
        assert!(
            ctx.resolve_external_call("obs_split", "plain", 0)
                .expect("the seam reports a routing, never an error")
                .is_some(),
            "consultation {consultation}: a non-observing function is entered in-machine"
        );
        assert!(
            ctx.resolve_external_call("obs_split", "enc", 1)
                .expect("the seam reports a routing, never an error")
                .is_none(),
            "consultation {consultation}: the function whose own body issues \
             `erlang:term_to_binary/1` keeps the synchronous route"
        );
    }

    // The structural half, read straight off the loaded IR: the module really imports the BIF
    // (which is what arms the per-function check at all), and the import-carrying instruction
    // really sits in `enc/1`'s own lowered body and nowhere else.
    let module = ctx
        .runtime
        .code_server()
        .module_handle("obs_split")
        .expect("the seam's on-demand load cached the fixture");
    assert_eq!(
        call_site_reading_imports(&module),
        CallSiteReadingImports::FunImage,
        "the fixture imports a guarded fun-image BIF (and not `load_nif/2`)"
    );
    for (function, arity) in [("enc", 1)] {
        let atom = module
            .atom_table
            .lookup(function)
            .expect("the fixture's function is interned");
        let body = module
            .function(atom, arity)
            .expect("the function is defined");
        assert!(
            function_calls_a_fun_image_bif(&module, body),
            "`{function}/{arity}`'s own lowered body carries the `term_to_binary/1` import"
        );
    }
    let plain = module
        .atom_table
        .lookup("plain")
        .expect("`plain` is interned");
    let plain_body = module.function(plain, 0).expect("`plain/0` is defined");
    assert!(
        !function_calls_a_fun_image_bif(&module, plain_body),
        "`plain/0`'s body issues no guarded fun-image call"
    );
}

// ---------------------------------------------------------------------------------------
// The cross-module call seam vs the GC float guard (contract A.9's E0 route)
// ---------------------------------------------------------------------------------------
/// The float-using fixture (`tests/fixtures/gc_floats.beam`) — its code really lowers a
/// float-family instruction, which is the property this case needs.
const GC_FLOATS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/gc_floats.beam"
);

/// **The seam enters a float-using callee in-machine.** [`RuntimeContext::resolve_external_call`]
/// used to refuse a module whose code touches the machine's float-register file, which routed
/// the call onto the synchronous `call_bif` → `run_export_blocking` path: that path builds a
/// **nested** machine whose admission point is never armed, so a `receive` in the callee cannot
/// suspend a scheduled slice and folds into the typed `blocked()` gap — exactly the A.9 E0
/// blocker (`compile:file/2`, whose code lowers `fconv`).
///
/// It no longer refuses: the door the engine reaches on a slice's own machine is
/// [`ExecutionContext::ensure_heap_full`], which publishes each register file up to its
/// pointer-bearing prefix — the FR file included, so every live float register (a boxed float
/// term) is inside the bound — as a root (`crate::context::gc::publish_roots_full`), so a
/// collection over a callee that left a float register live relocates it instead of dangling it
/// — for any module.
///
/// The caller-module float test (`module_uses_floats`) still exists, but only the **narrow**
/// [`ExecutionContext::ensure_heap`] door consults it (that door cannot see the FR
/// file); the two full doors apply none. The module is therefore still *not* float-free — the
/// property this case needs — and its narrow-door refusal is pinned by
/// [`the_narrow_door_still_refuses_a_float_using_call_site`].
#[test]
fn the_call_seam_enters_a_float_using_module_in_machine() {
    let mut rt = runtime();
    rt.load_module(GC_FLOATS_BEAM)
        .expect("the float fixture loads into the code server");
    let floats = rt
        .code_server()
        .module_handle("gc_floats")
        .expect("the fixture is cached under its own name");
    assert!(
        !rt.module_is_float_free("gc_floats", &floats),
        "the fixture lowers a float-family instruction, so the GC doors still refuse to \
         collect over it"
    );
    assert!(
        super::gc::module_uses_floats(&floats),
        "the property the guard keys on is the lowered float-family instruction"
    );

    let mut ctx = RuntimeContext::new(&mut rt);
    let resolved = ctx
        .resolve_external_call("gc_floats", "loop", 2)
        .expect("the seam reports a routing, never an error");
    let (module, label) = resolved.expect(
        "a float-using callee is entered in-machine: the call site, not the callee's \
         instruction set, is refused or admitted",
    );
    assert!(
        Arc::ptr_eq(&module, &floats),
        "the seam hands back the code server's own handle (the engine dedups contexts on it)"
    );
    let function = floats.atom_table.lookup("loop").expect("loop is interned");
    assert_eq!(
        Some(label),
        floats.export(function, 2),
        "the resolved label is the callee's own export entry"
    );

    // The remaining refusals are unchanged: an export the callee does not have is still
    // `Ok(None)`, so the synchronous path reports the oracle's `undef` byte for byte.
    assert!(
        ctx.resolve_external_call("gc_floats", "loop", 3)
            .expect("the seam reports a routing")
            .is_none(),
        "an arity the callee does not export is refused"
    );
}
// ---------------------------------------------------------------------------------------
// The cross-module call seam's idempotence (the observer guard on the cached arm)
// ---------------------------------------------------------------------------------------

/// The `idle` fixture module: a callee that imports nothing the seam guards, so it is entered
/// in-machine like any ordinary `.beam` body.
const SEAM_IDLE_SRC: &str = "-module(idle).
-export([go/0]).
go() -> ok.
";

/// The `obs` fixture module: a callee whose **own** code imports `erlang:load_nif/2`, whose
/// refusal is **module-wide** ([`callee_observes_its_call_site`]) — every function of the module
/// keys on it, which is exactly what makes it an observer of its call site.
const SEAM_OBS_SRC: &str = "-module(obs).
-export([load/1]).
load(Path) -> erlang:load_nif(Path, 0).
";

/// **The cross-module seam is idempotent for an already-cached observer callee.**
///
/// The native baseline tier resolves one `call_ext*` **twice** — its `jit_ext_call` arm asks
/// [`RuntimeContext::resolve_external_call`] and, when it declines, the whole instruction is
/// re-run by `Machine::step_instruction`, which asks the same seam again (see
/// [`crate::runner::tier`]). The call-site guard ([`callee_observes_its_call_site`]) must
/// therefore hold on the **already-cached** arm too: a callee that the *first* consultation
/// loaded (and thereby cached) must still be refused the second time.
///
/// While the guard lived only on the on-demand-load arm, the second consultation answered
/// `Ok(Some(..))` and the engine entered the callee in-machine under the **caller's** module
/// identity — for a module whose own `init/1` runs `erlang:load_nif/2`, that identity is what
/// the loader validates `ErlNifEntry.name` against, so the load failed with `bad_lib`. The
/// `obs` fixture here is that shape (its `erlang:load_nif/2` import is what the guard keys on).
///
/// `idle` is the control the fix must leave **unchanged**: a non-observer callee is still
/// entered in-machine, on both consultations.
#[test]
fn the_call_seam_is_idempotent_for_a_cached_observer_callee() {
    let Some(erlc) = resolve_erlc() else {
        skip_loudly(
            "cached observer callee",
            "no `erlc` (and none beside $ERTS_ORACLE either)",
        );
        return;
    };
    let dir = compile_fixtures(
        &erlc,
        "observer_idempotence",
        &[("idle", SEAM_IDLE_SRC), ("obs", SEAM_OBS_SRC)],
    );

    let mut rt = runtime();
    rt.add_code_path(dir.path());
    let mut ctx = RuntimeContext::new(&mut rt);

    // The first consultation finds `obs` uncached, loads it on demand and refuses it (it is an
    // observer); the load is what caches it, so the **second** consultation takes the
    // already-cached arm — the arm the fix added the guard to, and the one that used to admit
    // it. Both answers must be the same refusal.
    for consultation in 1..=2 {
        assert!(
            ctx.resolve_external_call("obs", "load", 1)
                .expect("the seam reports a routing, never an error")
                .is_none(),
            "consultation {consultation} of an observer callee must be refused: entering a \
             module whose own `erlang:load_nif/2` runs under the caller's identity is the \
             `bad_lib` bug the seam is idempotent to prevent"
        );
    }

    // The control: a non-observer callee is entered in-machine, on both consultations the JIT
    // makes — the fix must not have widened the refusal to ordinary modules.
    for consultation in 1..=2 {
        assert!(
            ctx.resolve_external_call("idle", "go", 0)
                .expect("the seam reports a routing, never an error")
                .is_some(),
            "consultation {consultation} of a non-observer callee must be admitted in-machine"
        );
    }
}

// ---------------------------------------------------------------------------------------
// The cross-module call seam vs a BUILT-IN NIF library (contract A.7's `prim_*` family)
// ---------------------------------------------------------------------------------------

/// The non-built-in control for the exemption: the **same** `erlang:load_nif/2` import the
/// built-in library carries, under a name no static NIF table resolves — so the module-wide
/// refusal must still stand for it.
const SEAM_NOT_BUILTIN_SRC: &str = "-module(not_builtin_nif).
-export([init/1]).
init(Path) -> erlang:load_nif(Path, #{}).
";

/// **A built-in NIF library is entered in-machine even though it imports `erlang:load_nif/2`.**
///
/// `prim_tty` is ERTS's *statically linked* NIF library: `erlang:load_nif/2` resolves it from
/// ERTS's static NIF table (`is_static_nif_module`, `erts/emulator/beam/erl_nif.c`) instead of
/// `dlopen`ing a shared object, so this runtime's `NifLoader::load_nif` answers the oracle's
/// `ok` without a `dlopen` and **without registering an owning module** — no
/// `current_module`-dependent identity exists for the module-wide guard to protect. Keeping the
/// refusal would buy nothing and would route the library's Erlang wrappers (`load/0`, `init/1`,
/// `writer/1`, `reader/1`, …) onto the synchronous non-pumping `call_bif` path, where `init/1`'s
/// blocking `receive` folds into the typed `blocked()` gap.
///
/// Both directions are pinned so the exemption cannot silently widen:
///
/// * the **real system** `prim_tty` resolves to its own `init/1` export label — and its loaded
///   IR is asserted to really carry the guard-arming `erlang:load_nif/2` import, so the case
///   proves the *exemption* rather than a vanished import;
/// * a **non-built-in** module with the identical import stays `Ok(None)`, keeping the
///   synchronous route that sets `current_module` correctly (its `load_nif` really does register
///   a library under the module identity).
///
/// The built-in half needs a system Erlang installation (a bare container skips loudly); the
/// control half needs `erlc` to compile its fixture.
#[test]
fn the_call_seam_enters_a_built_in_nif_library_in_machine() {
    let mut rt = runtime();

    // A built-in NIF library's Erlang wrappers are entered in-machine rather than through the
    // synchronous `call_bif` route. A bare container without an
    // OTP installation skips **loudly**; once the system code path resolves, the module must be
    // there — so a seam regression below still fails rather than skips.
    if let Err(error) = rt.code_server.add_system_paths() {
        skip_builtin_case(
            "the_call_seam_enters_a_built_in_nif_library_in_machine",
            &format!("no system code path ({error})"),
        );
        return;
    }
    assert!(
        rt.find_module_file("prim_tty").is_some(),
        "the system code path resolved but carries no `prim_tty.beam`"
    );
    // `RuntimeContext` owns a sink handle (and a destructor), so its borrow of the runtime ends
    // with this block — the control half below re-borrows the same runtime.
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for consultation in 1..=2 {
            assert!(
                ctx.resolve_external_call("prim_tty", "init", 1)
                    .expect("the seam reports a routing, never an error")
                    .is_some(),
                "consultation {consultation}: a built-in NIF library's Erlang wrapper is entered \
                 in-machine — its `erlang:load_nif/2` registers nothing, so its call site cannot \
                 be read"
            );
        }
        let (module, label) = ctx
            .resolve_external_call("prim_tty", "init", 1)
            .expect("the seam reports a routing, never an error")
            .expect("the built-in library is admitted");

        // The exemption is what admitted it: the guard's own verdict on this very module is
        // `LoadNif`, i.e. a refusal of **every** function — read straight off the loaded IR, so
        // the case cannot pass because the import went missing.
        assert_eq!(
            call_site_reading_imports(&module),
            CallSiteReadingImports::LoadNif,
            "the real `prim_tty` imports `erlang:load_nif/2`, which the guard refuses module-wide"
        );
        let init = module
            .atom_table
            .lookup("init")
            .expect("`init` is interned in the module's own table");
        assert!(
            callee_observes_its_call_site(&module, init, 1),
            "the guard itself refuses `prim_tty:init/1` — only the built-in exemption admits it"
        );
        // The seam hands back the code server's own handle and the callee's own export label.
        let cached = ctx
            .runtime
            .code_server()
            .module_handle("prim_tty")
            .expect("the seam's on-demand load cached the system module");
        assert!(
            Arc::ptr_eq(&module, &cached),
            "the seam hands back the code server's own handle (the engine dedups contexts on it)"
        );
        assert_eq!(
            Some(label),
            module.export(init, 1),
            "the resolved label is `prim_tty:init/1`'s own export entry"
        );
    }

    // The control direction: the identical import under a name no static NIF table resolves
    // keeps the module-wide refusal.
    let Some(erlc) = resolve_erlc() else {
        skip_loudly(
            "built-in NIF library exemption",
            "no `erlc` (and none beside $ERTS_ORACLE either)",
        );
        return;
    };
    let dir = compile_fixtures(
        &erlc,
        "builtin_nif_exemption",
        &[("not_builtin_nif", SEAM_NOT_BUILTIN_SRC)],
    );
    rt.add_code_path(dir.path());
    let mut ctx = RuntimeContext::new(&mut rt);
    assert!(
        ctx.resolve_external_call("not_builtin_nif", "init", 1)
            .expect("the seam reports a routing, never an error")
            .is_none(),
        "a non-built-in module importing `erlang:load_nif/2` keeps the module-wide refusal: \
         its `load_nif` registers a library under the module identity, which an in-machine \
         callee would run under the caller's"
    );
    let control = ctx
        .runtime
        .code_server()
        .module_handle("not_builtin_nif")
        .expect("the seam's on-demand load cached the control fixture");
    assert_eq!(
        call_site_reading_imports(&control),
        CallSiteReadingImports::LoadNif,
        "the control carries the very same guarded import as the built-in library"
    );
}

// ---------------------------------------------------------------------------------------
// The in-machine fun-apply seam (`resolve_local_fun`) and its external-fun arm
// ---------------------------------------------------------------------------------------

/// `efwait:wait/0` materializes an **external** fun (`fun efcallee:block/1`, the `EXPORT_EXT`
/// encoding) and hands it to a **second** module, so the compiler cannot inline the apply: the
/// `call_fun` instruction lives in `efhelper:call1/2`. The spawned `efwait:sender/2` is the
/// child that delivers the message the callee waits for.
const EFWAIT_SRC: &str = r#"
-module(efwait).
-export([wait/0, sender/2]).

wait() ->
    spawn(efwait, sender, [self(), 42]),
    efhelper:call1(fun efcallee:block/1, dummy).

sender(To, Msg) -> To ! Msg.
"#;

/// The module the `call_fun` lives in — a **distinct** module is what defeats the compiler's
/// fun inlining, so the external fun is genuinely applied with `call_fun`.
const EFHELPER_SRC: &str = r#"
-module(efhelper).
-export([call1/2]).

call1(F, X) -> F(X).
"#;

/// The external fun's callee: its body blocks in a bare `receive`, so the process can only
/// finish if the engine entered the callee **in-machine** (the suspension is the *process's*)
/// and the scheduler resumed it when the sender's message arrived.
const EFCALLEE_SRC: &str = r#"
-module(efcallee).
-export([block/1]).

block(_) -> receive X -> X end.
"#;

/// **An external `fun M:F/A` whose callee blocks is entered in-machine from a scheduled slice.**
///
/// `RuntimeContext::resolve_local_fun` resolves an **external** fun (`fun M:F/A`, `EXPORT_EXT`)
/// to its callee module's export label, so the engine's `call_fun` arm enters the callee body
/// in the **calling** machine — exactly as C's `call_fun` dispatches to the fun's code in the
/// same process. The workload drives the CLI's `run` route
/// ([`Runtime::run_scheduled`](crate::runtime::Runtime::run_scheduled)): a real scheduled
/// process applies `fun efcallee:block/1` from a foreign module, the callee's bare `receive`
/// finds an empty mailbox and [suspends](beam_engine::RunOutcome::Wait) the *process*, and a
/// later scheduler pass — the sender child the process spawned — delivers the message that
/// resumes it.
///
/// Without the external-fun arm the resolver answers `Ok(None)`, `call_fun` falls back to the
/// value-returning `apply_fun`, whose nested `Machine` cannot re-enter the scheduler
/// (`may_pump_scheduler() == false` on a slice), so the `receive` folds into the uncatchable
/// `blocked()` capability gap and the process dies instead of returning. Asserting the
/// oracle-shaped value `42` is what rules that out — a wrong rung on the seam is an error or a
/// hang, never this value.
///
/// The final block reads the seam's own answer directly (the external fun resolves to
/// `efcallee:block/1`'s export label), pinning the resolution the end-to-end run depends on.
#[test]
fn an_external_fun_whose_callee_blocks_resumes_on_a_scheduled_slice() {
    // `run_scheduled` boots the runtime on demand, which resolves the system code path (natively
    // from the filesystem, else `$ERTS_STDLIB_DIR` or the system `erl`). The guard below is
    // conservative — it skips when neither `$ERTS_STDLIB_DIR` nor an `erl` on `$PATH` is
    // available — and this test needs the compiler too. A missing toolchain is a loud skip,
    // never a silent pass.
    if std::env::var_os("ERTS_STDLIB_DIR").is_none() && find_on_path("erl").is_none() {
        skip_loudly(
            "external fun apply",
            "no `erl` on $PATH and no $ERTS_STDLIB_DIR",
        );
        return;
    }
    let Some(erlc) = resolve_erlc() else {
        skip_loudly(
            "external fun apply",
            "no `erlc` (and none beside $ERTS_ORACLE either)",
        );
        return;
    };
    let dir = compile_fixtures(
        &erlc,
        "external_fun_apply",
        &[
            ("efwait", EFWAIT_SRC),
            ("efhelper", EFHELPER_SRC),
            ("efcallee", EFCALLEE_SRC),
        ],
    );

    let mut rt = runtime();
    rt.add_code_path(dir.path());

    // The scheduled route: a real process, a real run queue, real passes.
    let result = rt
        .run_scheduled(dir.join("efwait.beam"), "wait", 0, &[])
        .expect(
            "an external fun's blocking callee must suspend the scheduled process and be \
             resumed — a missing in-machine entry folds this into the typed `blocked()` gap",
        );
    assert_eq!(
        result,
        make_small(42),
        "the resumed receive took the message the sender child delivered"
    );

    // The seam's own outcome, read directly: the external fun resolves to `efcallee:block/1`'s
    // export label, which is exactly what makes the engine enter the callee in the scheduled
    // machine. `efcallee` is cached by the run above, so the resolution needs no code path.
    let module_atom = atom_val(rt.intern("efcallee"));
    let function_atom = atom_val(rt.intern("block"));
    let mut heap = beam_alloc::Heap::new(16).expect("a scratch heap");
    let fun = beam_codec::term::make_fun_external(&mut heap, 1, module_atom, function_atom)
        .expect("the external fun builds");
    let mut ctx = RuntimeContext::new(&mut rt);
    let (callee, label) = ctx
        .resolve_local_fun(fun, &[make_small(0)])
        .expect("the seam reports a routing, never an error")
        .expect("an external fun resolves to its callee's export label");
    assert_eq!(
        callee.atom_name(callee.name),
        Some("efcallee"),
        "the seam names the callee module the fun's MFA carries"
    );
    let block = callee
        .atom_table
        .lookup("block")
        .expect("`block` is interned in the callee's own table");
    assert_eq!(
        callee.export(block, 1),
        Some(label),
        "the resolved label is the callee's own export entry (what `call_ext` would enter)"
    );
}

/// A `make_fun3` ([`beam_codec::FunKind::Local`]) fun term plus the storage owning its words.
///
/// The frozen layout is `[header, uniq, index, env…]` with
/// `header = FUN_SUBTAG | arity << 8 | num_free << 16` (`beam_term::fun`). The returned
/// `Box<[Eterm]>` owns the words and must outlive the term (the boxed word points into it).
fn local_fun(arity: u32, uniq: u64, index: u32, env: &[Eterm]) -> (Box<[Eterm]>, Eterm) {
    let header = beam_term::FUN_SUBTAG
        | ((arity as usize) << 8)
        | ((env.len() as usize) << beam_term::fun::FUN_HEADER_ENV_SIZE_OFFS);
    let mut words: Vec<Eterm> = vec![
        Eterm::from_raw(header),
        Eterm::from_raw(uniq as usize),
        Eterm::from_raw(index as usize),
    ];
    words.extend_from_slice(env);
    let words = words.into_boxed_slice();
    let term = beam_term::make_boxed(words.as_ptr());
    (words, term)
}

/// **The seam's honest misses: the fun kinds it must NOT claim.**
///
/// [`ExecutionContext::resolve_local_fun`] enters a fun in-machine only for the two kinds it
/// can resolve: a `.beam` **local** (`make_fun3`) closure (through
/// [`RuntimeContext::local_fun_target`]) and an **external** fun (`fun M:F/A`, through
/// [`RuntimeContext::external_fun_target`]). Everything else answers `Ok(None)`, so the engine
/// keeps the value-returning [`ExecutionContext::apply_fun`] fallback for it — the seam must
/// never fabricate an in-machine entry.
///
/// Two shapes the resolver must decline, one per fun kind:
///
/// * the `-eval` evaluator's own closure — a [`beam_codec::FunKind::Local`] fun stamped with the
///   evaluator's sentinel `uniq` ([`crate::eval::EVAL_FUN_UNIQ`]). Only the evaluator can run its
///   body, so `local_fun_target` declines it by that stamp alone (before it searches the loaded
///   module set) and `external_fun_target` declines it because it is not an external fun;
/// * an external fun naming a module this runtime cannot load — the cross-module seam's own
///   refusal, so no export label is invented and the fun stays on the synchronous dynamic-call
///   route.
#[test]
fn resolve_local_fun_declines_the_fun_kinds_it_cannot_claim() {
    let mut rt = runtime();
    let mut heap = beam_alloc::Heap::new(32).expect("a scratch heap");

    // 1. The `-eval` evaluator's closure: a local fun stamped with the evaluator's sentinel
    //    `uniq`, which `local_fun_target` declines up front. No module defines it, so a resolver
    //    that claimed it would be entering the wrong code (or failing the descriptor lookup).
    let (_eval_words, eval_closure) = local_fun(0, crate::eval::EVAL_FUN_UNIQ, 0, &[]);

    // 2. An external fun naming a module this runtime does not have: the cross-module resolver
    //    refuses the (failing) load, so no export label is fabricated.
    let module = atom_val(rt.intern("no_such_module_zzz"));
    let function = atom_val(rt.intern("no_such_function"));
    let unknown = beam_codec::term::make_fun_external(&mut heap, 1, module, function)
        .expect("the external fun builds");

    let mut ctx = RuntimeContext::new(&mut rt);
    assert!(
        ctx.resolve_local_fun(eval_closure, &[])
            .expect("the seam reports a routing, never an error")
            .is_none(),
        "the `-eval` evaluator's own closure has no in-machine entry, so `apply_fun` stays its \
         fallback"
    );
    assert!(
        ctx.resolve_local_fun(unknown, &[make_small(1)])
            .expect("the seam reports a routing, never an error")
            .is_none(),
        "an external fun whose module cannot be loaded keeps the synchronous dynamic-call route"
    );
}

// ---------------------------------------------------------------------------
// The `RuntimeHooksBridge` arms beyond the timer/tag families.
//
// Each test drives the real dispatch (`RuntimeContext::call_bif`), so the arm is reached
// through `call_registered`'s gate. The oracle-cross-checked answers (OTP 29 / `erts-17.1`)
// are pinned alongside adversarial arguments: a rejected argument is the oracle's `badarg`,
// a facility this runtime lacks is the typed `unsupported` gap (never a wrong index, never a
// fabricated value), and a wrong arity misses the arm and falls through to the seam's
// defaulted gap.
// ---------------------------------------------------------------------------

/// `erlang:dt_get_tag/0`, `dt_get_tag_data/0`, `dt_put_tag/1` (all the atom `undefined`) and
/// `dt_append_vm_tag_data/1` / `dt_prepend_vm_tag_data/1` (the argument, unchanged).
#[test]
fn the_runtime_hooks_bridge_serves_the_dynamic_tracing_tag_family() {
    let mut rt = runtime();
    let undefined = rt.intern("undefined");
    let me = rt.self_pid().to_eterm();
    let mut scratch = beam_alloc::Heap::new(64).expect("a scratch heap");
    let tuple = scratch
        .tuple(&[make_small(1), undefined])
        .expect("a two-tuple");
    let mut ctx = RuntimeContext::new(&mut rt);

    for function in ["dt_get_tag", "dt_get_tag_data"] {
        assert_eq!(
            ctx.call_bif("erlang", function, 0, &[]).expect(function),
            undefined,
            "{function}/0 answers the atom `undefined`"
        );
    }
    // `dt_put_tag/1` ignores whatever it is handed.
    for argument in [undefined, make_small(42), NIL, me] {
        assert_eq!(
            ctx.call_bif("erlang", "dt_put_tag", 1, &[argument])
                .expect("dt_put_tag/1"),
            undefined,
            "dt_put_tag/1 ignores its argument"
        );
    }
    // The VM-probe helpers apply no type check at all: the argument comes back unchanged.
    for function in ["dt_append_vm_tag_data", "dt_prepend_vm_tag_data"] {
        for argument in [make_small(42), undefined, NIL, tuple] {
            assert_eq!(
                ctx.call_bif("erlang", function, 1, &[argument])
                    .expect(function),
                argument,
                "{function}/1 returns its argument unchanged"
            );
        }
    }
    // A wrong arity is not a `RuntimeHooks` roster key at all, so the bridge is never
    // installed for it and the call falls through to the ordinary namespace path — a bare
    // `error:undef`, never a neighbouring arity's behaviour. (`needs_runtime_hooks` is
    // per `(module, function, arity)`, so only the listed arity reaches the seam.)
    for (function, arity) in [("dt_get_tag", 1u32), ("dt_put_tag", 0), ("dt_get_tag_data", 2)] {
        let error = ctx
            .call_bif("erlang", function, arity, &[make_small(1)])
            .expect_err("an unlisted arity is not a RuntimeHooks key");
        assert_eq!(
            error.as_atom(),
            Some("undef"),
            "{function}/{arity} is outside the roster, so no bridge is installed: {error}"
        );
    }
}

/// `net_kernel:dflag_unicode_io/1` — the io-device unicode fast-path probe.
///
/// Oracle-verified (OTP 29/`erts-17.1`): a **local** pid answers the atom `true` (the
/// C's `net_kernel_dflag_unicode_io_1` short-circuits for a pid on *this* node,
/// `dist.c:6693-6709`); anything that is not a pid is `badarg`. A remote pid is the
/// typed capability gap (no peer-flag accessor) rather than a fabricated `true`. A wrong
/// arity is outside the roster, so no bridge is installed (bare `error:undef`).
#[test]
fn the_runtime_hooks_bridge_serves_the_dflag_unicode_io_probe() {
    let mut rt = runtime();
    let true_atom = rt.intern("true");
    let me = rt.self_pid().to_eterm();
    let mut ctx = RuntimeContext::new(&mut rt);

    assert_eq!(
        ctx.call_bif("net_kernel", "dflag_unicode_io", 1, &[me])
            .expect("dflag_unicode_io/1 on a local pid"),
        true_atom,
        "a local pid answers `true`"
    );
    for argument in [make_small(42), NIL] {
        let error = ctx
            .call_bif("net_kernel", "dflag_unicode_io", 1, &[argument])
            .expect_err("a non-pid is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }
    let error = ctx
        .call_bif("net_kernel", "dflag_unicode_io", 0, &[])
        .expect_err("an unlisted arity is not a RuntimeHooks key");
    assert_eq!(error.as_atom(), Some("undef"), "{error}");
}

/// `erlang:system_profile/0`, `erlang:check_old_code/1`, `erlang:finish_loading/1` and the
/// three `badarg`-always on-load-hook primitives.
#[test]
fn the_runtime_hooks_bridge_serves_the_code_store_and_loading_probes() {
    let mut rt = runtime();
    let undefined = rt.intern("undefined");
    let false_atom = rt.intern("false");
    let ok = rt.intern("ok");
    let some_module = rt.intern("some_module");
    let mut scratch = beam_alloc::Heap::new(16).expect("a scratch heap");
    let one_item = scratch.cons(some_module, NIL).expect("a one-element list");
    let mut ctx = RuntimeContext::new(&mut rt);

    // No profiling subsystem runs, so the oracle's `system_profile/0` answers `undefined`.
    assert_eq!(
        ctx.call_bif("erlang", "system_profile", 0, &[])
            .expect("system_profile/0"),
        undefined
    );

    // `check_old_code/1`: any atom answers `false` (this runtime never replaces a live
    // module); anything that is not an atom is `badarg`.
    for argument in [some_module, ok] {
        assert_eq!(
            ctx.call_bif("erlang", "check_old_code", 1, &[argument])
                .expect("check_old_code/1"),
            false_atom
        );
    }
    for argument in [make_small(42), NIL] {
        let error = ctx
            .call_bif("erlang", "check_old_code", 1, &[argument])
            .expect_err("a non-atom is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

    // `finish_loading/1`: only the empty list is accepted; the non-empty list a real
    // `code:load_binary` would pass, and any non-list, are `badarg`.
    assert_eq!(
        ctx.call_bif("erlang", "finish_loading", 1, &[NIL])
            .expect("finish_loading([])"),
        ok
    );
    for argument in [one_item, make_small(42), some_module] {
        let error = ctx
            .call_bif("erlang", "finish_loading", 1, &[argument])
            .expect_err("anything but [] is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

    // The on-load-hook primitives always answer `badarg` on this BEAMASM build: they need an
    // internal prepared-code term only `prepare_loading/2` can mint.
    let trio: [(&str, u32, [beam_term::Eterm; 2]); 3] = [
        ("call_on_load_function", 1, [one_item, NIL]),
        ("has_prepared_code_on_load", 1, [ok, NIL]),
        ("finish_after_on_load", 2, [ok, NIL]),
    ];
    for (function, arity, args) in trio {
        let error = ctx
            .call_bif("erlang", function, arity, &args[..arity as usize])
            .expect_err("the on-load-hook primitives are always badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{function}/{arity}: {error}");
    }
}

/// `erts_internal:is_system_process/1` / `is_process_executing_dirty/1`,
/// `erts_literal_area_collector:release_area_switch/0`, `erts_trace_cleaner:check/0`,
/// `error_logger:warning_map/0` and the `code:*` coverage-mode surface.
#[test]
fn the_runtime_hooks_bridge_serves_the_internal_notsup_and_constants() {
    let mut rt = runtime();
    let false_atom = rt.intern("false");
    let true_atom = rt.intern("true");
    let none = rt.intern("none");
    let line = rt.intern("line");
    let call = rt.intern("call");
    let warning = rt.intern("warning");
    let me = rt.self_pid().to_eterm();
    // The term's words live on `_remote_heap`, so the heap must outlive every use below.
    let (_remote_heap, remote) = remote_pid(&mut rt, "elsewhere@host");
    let mut ctx = RuntimeContext::new(&mut rt);

    for function in ["is_system_process", "is_process_executing_dirty"] {
        assert_eq!(
            ctx.call_bif("erts_internal", function, 1, &[me])
                .expect(function),
            false_atom,
            "{function}/1 answers `false` for a local pid"
        );
        // A remote pid needs the peer's process record (distribution, contract A.5), so it
        // is the typed gap rather than a fabricated `false`.
        let error = ctx
            .call_bif("erts_internal", function, 1, &[remote])
            .expect_err("a remote pid is a typed gap");
        shared_gap(
            &error,
            &format!(
                "erts_internal:{function}/1 on a remote pid: distribution is not implemented"
            ),
        );
        // A term that is not a pid is `badarg`.
        let error = ctx
            .call_bif("erts_internal", function, 1, &[make_small(42)])
            .expect_err("a non-pid is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{function}: {error}");
    }

    // `release_area_switch/0` **raises** `error notsup` (the facility is not compiled in) —
    // never the bare atom.
    let error = ctx
        .call_bif("erts_literal_area_collector", "release_area_switch", 0, &[])
        .expect_err("release_area_switch/0 raises");
    let raise = error.into_raise().expect("a raise, not a reason variant");
    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
    assert_eq!(raise.reason.as_atom(), Some("notsup"));

    // The trace-cleaner and error-logger constants.
    assert_eq!(
        ctx.call_bif("erts_trace_cleaner", "check", 0, &[])
            .expect("check/0"),
        false_atom
    );
    assert_eq!(
        ctx.call_bif("error_logger", "warning_map", 0, &[])
            .expect("warning_map/0"),
        warning
    );

    // The `code:*` coverage surface: support is `true`, the current mode `none`, and
    // `set_coverage_mode/1` accepts only `none`/`line`, answering the previous `none`.
    assert_eq!(
        ctx.call_bif("code", "coverage_support", 0, &[])
            .expect("coverage_support/0"),
        true_atom
    );
    assert_eq!(
        ctx.call_bif("code", "get_coverage_mode", 0, &[])
            .expect("get_coverage_mode/0"),
        none
    );
    for mode in [none, line] {
        assert_eq!(
            ctx.call_bif("code", "set_coverage_mode", 1, &[mode])
                .expect("set_coverage_mode/1"),
            none
        );
    }
    for bad in [call, make_small(1), NIL] {
        let error = ctx
            .call_bif("code", "set_coverage_mode", 1, &[bad])
            .expect_err("an unknown coverage mode is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }
}

/// `erts_internal:erase_persistent_terms/0` answers `true` **and** really clears the
/// runtime's own persistent-term store.
#[test]
fn the_runtime_hooks_bridge_erases_the_persistent_term_store() {
    let mut rt = runtime();
    let key = rt.intern("a_persistent_key");
    let value = rt.intern("a_persistent_value");
    let default = rt.intern("the_default");
    let ok = rt.intern("ok");
    let true_atom = rt.intern("true");
    let mut ctx = RuntimeContext::new(&mut rt);

    // Store a term through the `PersistentTerm` arm and read it back.
    assert_eq!(
        ctx.call_bif("persistent_term", "put", 2, &[key, value])
            .expect("put/2"),
        ok
    );
    assert_eq!(
        ctx.call_bif("persistent_term", "get", 2, &[key, default])
            .expect("get/2"),
        value
    );

    // The reset hook answers `true` and clears the store: the same lookup now answers the
    // default, and `get/1` — which raises for an absent key — is `badarg`.
    assert_eq!(
        ctx.call_bif("erts_internal", "erase_persistent_terms", 0, &[])
            .expect("erase_persistent_terms/0"),
        true_atom
    );
    assert_eq!(
        ctx.call_bif("persistent_term", "get", 2, &[key, default])
            .expect("get/2 after the erase"),
        default
    );
    let error = ctx
        .call_bif("persistent_term", "get", 1, &[key])
        .expect_err("an absent key is badarg");
    assert_eq!(error.as_atom(), Some("badarg"), "{error}");
}

/// Assert `error` is the typed capability gap whose reason is `expected` (the reason the
/// `unsupported_reason` classifier strips the leading token from).
fn shared_gap(error: &BeamError, expected: &str) {
    assert_eq!(
        crate::error::unsupported_reason(error).as_deref(),
        Some(expected),
        "{error}"
    );
}

// ---------------------------------------------------------------------------------------
// The GC root-set hand-off: each register file's pointer-bearing prefix and the whole Erlang stack
// ---------------------------------------------------------------------------------------

/// The integer-only fixture (`tests/fixtures/gc_ints.beam`), used here for one property
/// only: its code contains **no** float-family instruction, so the module guard
/// ([`super::gc::module_uses_floats`]) lets a collection run over it. The bytes are never
/// executed by these cases.
const GC_INTS_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/gc_ints.beam");

/// The call site's module name — a module the rig has loaded (so [`module_may_be_collected`]
/// answers `true` from the real code server, not from a stubbed cache).
///
/// [`module_may_be_collected`]: super::gc::module_may_be_collected
const GC_CALL_SITE: &str = "gc_ints";

/// The rig the two root-hand-off cases share: a runtime with the float-free fixture loaded
/// as the call site, plus a process living in a **caller-supplied** [`ProcessTable`] — the
/// shape a scheduler slice has ([`RuntimeContext::for_process`]), and the one where a
/// collection is a legitimate admission point.
fn gc_root_set_rig() -> (Runtime, ProcessTable, ProcessId) {
    let mut rt = runtime();
    rt.load_module(GC_INTS_BEAM)
        .expect("the float-free fixture loads into the code server");
    let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("spawn a process");
    (rt, table, pid)
}

/// Builds the two terms the root-set hand-off cases need on `pid`'s **own young heap**:
///
/// * `beyond` — the term that will sit in `x[1]` while the engine declares only `x[0]`
///   live (`live = 1`); and
/// * `fr_only` — the term that will be reachable **only** through the machine's FR file.
///
/// Both are two-element tuples of small integers, so their relocation is checked by value
/// (`{7, 8}` / `{9, 10}`), never by address arithmetic.
fn gc_root_terms(table: &mut ProcessTable, pid: ProcessId) -> (Eterm, Eterm) {
    let process = table.get_mut(pid).expect("the spawned process");
    let heap = process.heap_mut();
    let beyond = heap
        .tuple(&[make_small(7), make_small(8)])
        .expect("the young heap builds {7,8}");
    let fr_only = heap
        .tuple(&[make_small(9), make_small(10)])
        .expect("the young heap builds {9,10}");
    (beyond, fr_only)
}

/// Assert `relocated` is `expected`'s tuple **at its new address** — the term really moved
/// (a copied term never keeps its from-space address) and the copy is intact.
fn assert_relocated_tuple(relocated: Eterm, stale: Eterm, expected: [i64; 2], what: &str) {
    assert_ne!(
        relocated, stale,
        "{what}: the term was left pointing into the from-space — a collection moved the \
         heap without relocating it"
    );
    assert_eq!(
        terms::tuple_arity(relocated).unwrap_or_else(|e| panic!("{what}: {e}")),
        2,
        "{what}: the relocated word is not the two-element tuple it was"
    );
    for (index, value) in expected.iter().enumerate() {
        assert_eq!(
            terms::tuple_element(relocated, index).expect("in range"),
            make_small(*value),
            "{what}: element {index} of the relocated tuple"
        );
    }
}

/// **The hole-1 regression guard: a collection must root the WHOLE root set.**
///
/// The engine's `live` count is only a *hint* — a slot it declares dead at one admission
/// point can still be read later in the same run — and the FR file holds `Eterm` term slots
/// at Milestone 1, so a float register is a root like any other. Both are invisible to the
/// narrow `ensure_heap` door (it receives only an x prefix and the stack), so a collection it
/// drove would move the heap out from under them.
///
/// The case therefore drives the **full-root** admission point
/// ([`ExecutionContext::ensure_heap_full`]) with `live = 1` while a live term sits in `x[1]`
/// and a second lives only in `fr[0]`. A forced collection runs; both must come back
/// relocated (a different word, still the same tuple). A runtime that publishes only
/// `x[0..live)` and discards the FR file leaves both words stale, which is exactly the
/// failure this asserts against.
#[test]
fn a_admission_point_roots_every_x_register_and_the_whole_fr_file() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    // `x[1]` is a live term the engine declares DEAD (`live = 1`); `fr[0]` is reachable
    // through the FR file alone. Neither is in the narrow door's published root set.
    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }

    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "the forced collection really ran; {after:?}"
    );
    assert_eq!(
        after.halted, before.halted,
        "the collection and its scatter completed; {after:?}"
    );

    assert_relocated_tuple(x[1], beyond, [7, 8], "x[1] (beyond the `live` hint)");
    assert_relocated_tuple(fr[0], fr_only, [9, 10], "fr[0] (reachable only through the FR file)");
}

/// **`fr_count` is an exact region length, and a region mismatch still halts the collection.**
///
/// The runner's park arm bounds the published FR prefix by `Machine::fr_mark()` (the live
/// leading slots) and the resume replays that same count, so
/// [`super::gc::publish_roots_full`] / [`super::gc::scatter_roots_full`] take a **count**
/// rather than a flag. Three properties are load-bearing, and this case drives the helpers
/// directly to pin all three without a scheduler:
///
/// * a publish of `fr_count` words puts exactly the stack followed by those leading FR words
///   onto the process stack — the caller's FR view is the machine's whole 1024-slot file, so
///   `fr_count` (here `2` of a four-word view) is what bounds the region, never the slice
///   length;
/// * the matching scatter writes **only** the leading `fr_count` FR slots back and leaves the
///   slots above it untouched (a machine whose mark proved them `NIL` must not have them
///   overwritten by the relocated stack's tail);
/// * a relocated region whose length disagrees with `stack + fr_count` is reported as `false`
///   — the caller's "halt this process's collection" — rather than a mis-sized write back.
#[test]
fn the_fr_count_bounds_the_published_region_and_a_mismatch_still_halts() {
    let mut table =
        ProcessTable::new(beam_process::SignalAtoms::from_table(&mut AtomTable::new()));
    let pid = table.spawn_default().expect("spawn a process");

    // A four-slot FR view with two live leading slots.
    let mut x = [make_small(1), make_small(2), NIL, NIL];
    let mut stack = [make_small(3), make_small(4)];
    let mut fr = [make_small(5), make_small(6), NIL, NIL];

    {
        let process = table.get_mut(pid).expect("the spawned process");
        assert!(
            super::gc::publish_roots_full(
                process,
                &RootSet::new(&mut x, &mut stack, &mut fr, 1),
                2
            ),
            "a publish with an in-range count reports the region it wrote"
        );
        assert_eq!(
            process.registers().stack().as_slice(),
            &[make_small(3), make_small(4), make_small(5), make_small(6)][..],
            "the published region is the whole stack followed by exactly `fr_count` FR words"
        );
    }

    // The scatter is the exact inverse for the same count: the published region splits back
    // into the stack and the two **leading** FR slots.
    {
        let process = table.get(pid).expect("the process is live");
        let mut back = [NIL, NIL];
        let mut out_fr = [make_small(99), make_small(99), make_small(99), make_small(99)];
        assert!(
            super::gc::scatter_roots_full(
                process,
                &mut RootSet::new(&mut x, &mut back, &mut out_fr, 1),
                2
            ),
            "the scatter accepts the region the matching publish wrote"
        );
        assert_eq!(
            out_fr,
            [make_small(5), make_small(6), make_small(99), make_small(99)],
            "only the leading `fr_count` FR slots are written back"
        );
    }

    // A relocated region one word longer than `stack + fr_count` (what a resume passing a count
    // the park did not use would see) must be refused, never written through.
    {
        let process = table.get_mut(pid).expect("the process is live");
        process.registers_mut().stack_mut().push(make_small(7));
        let mut back = [NIL, NIL];
        assert!(
            !super::gc::scatter_roots_full(
                process,
                &mut RootSet::new(&mut x, &mut back, &mut fr, 1),
                2
            ),
            "a relocated stack longer than stack + fr_count halts the collection"
        );
    }
}

/// **The same guard for the collecting allocation seam.**
///
/// [`ExecutionContext::alloc_words_rooted`] is the engine's term-building admission point:
/// when the running process's young heap cannot serve `n` raw words it collects over the
/// handed-in root set and retries. The root set it receives is the machine's whole
/// [`RootSet`], so the same two roots must survive: an x register beyond the engine's `live`
/// hint, and a term reachable only through the FR file.
///
/// The door is driven by a **forced** collection request (`erlang:garbage_collect/0,1`), which
/// **bypasses** the young-heap bump — and, with it, the no-relocation
/// [grow-and-retry](super::gc::alloc_words_rooted) that meets an unforced capacity refusal. That
/// is what makes this the collecting path: the nursery is filled with garbage first, so the
/// collection really has to evacuate something before the retry can be served from it.
#[test]
fn a_collecting_allocation_roots_every_x_register_and_the_whole_fr_file() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    // Fill the nursery with unreachable tuples so the post-collection retry is a real one.
    {
        let process = table.get_mut(pid).expect("the spawned process");
        let filler = [make_small(1)];
        while process.heap().available() >= 16 {
            process
                .heap_mut()
                .tuple(&filler)
                .expect("the nursery has room for a two-word tuple");
        }
    }

    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.alloc_words_rooted(16, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("a rooted allocation");
    }

    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "the refused bump collected over the handed-in roots; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "{after:?}");
    assert_eq!(
        after.fallbacks, before.fallbacks,
        "the collection emptied the nursery, so the retry was served from the young heap \
         (no arena fallback); {after:?}"
    );

    assert_relocated_tuple(x[1], beyond, [7, 8], "x[1] (beyond the `live` hint)");
    assert_relocated_tuple(fr[0], fr_only, [9, 10], "fr[0] (reachable only through the FR file)");
}

// ---------------------------------------------------------------------------------------
// The GC root-set hand-off, hole 2: a root in a region the collector does not scan
// ---------------------------------------------------------------------------------------

/// Builds the pooled-BIF-heap shape the `erl_error:format_exception` repro hit: a **cons
/// cell built on the runtime's pooled BIF heap** (a region the copying collector classifies
/// as `Target::Foreign` — it leaves the pointer in place and does not follow it) whose `car`
/// aliases a term on `pid`'s **own young heap**.
///
/// Returns `(foreign, child)`:
///
/// * `child` — a two-element tuple `{7,8}` on the process's young heap (a term the collector
///   *would* relocate); and
/// * `foreign` — the one-element list `[child]` whose cons cell lives in `rt.bif_heaps[0]`.
///
/// The list's own address is in neither the young nor the mature space of `pid`, so it is
/// exactly the root [`super::gc::root_is_unscanned`] must flag. The pool heap lives in the
/// runtime (index `0` is never popped in these tests), so the cons cell stays valid.
fn foreign_root_aliasing_a_young_child(
    rt: &mut Runtime,
    table: &mut ProcessTable,
    pid: ProcessId,
) -> (Eterm, Eterm) {
    let child = table
        .get_mut(pid)
        .expect("the spawned process")
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("the young heap builds {7,8}");
    let foreign = rt
        .bif_heaps
        .first_mut()
        .expect("the runtime always owns its pooled BIF heap")
        .cons(child, NIL)
        .expect("the pooled BIF heap builds the cons cell");
    (foreign, child)
}

/// **The hole-2 regression guard: a collection is DECLINED while a live machine root is a
/// container in a region the collector does not scan.**
///
/// A copying collector relocates exactly the words in its *from* set (here the process's
/// young space) and classifies every other address as `Target::Foreign`
/// (`../../beam_gc/src/copy.rs`): it leaves such a pointer where it is **and does not follow
/// it**, so a young word the foreign container points *at* is neither kept alive by it nor
/// updated when it moves — the term goes stale.
///
/// The `erl_error:format_exception` repro hit exactly this: a registered BIF built its result
/// on the runtime's pooled BIF heap (`../context/mod.rs`'s `call_registered` heart) and left
/// it in `x[0]` when the process's minor collection ran. This case reproduces the shape
/// directly at the admission point: `x[0]` holds a cons cell built on `rt.bif_heaps[0]` whose
/// `car` is a young `{7,8}`, and a **forced** collection asks the runtime to move that heap.
///
/// The fix declines the move: `gc_stats().collections` must **not** advance, the forced
/// request must stay armed (it is consumed only by a collection that is committed to), and
/// the root must remain the valid `[child]` list — its `car` still reads back the intact
/// `{7,8}` from the young heap it aliases. Without the valve the collection runs and the
/// `collections` assertion is the one that fires.
#[test]
fn a_collection_is_declined_when_a_live_root_lives_in_an_unscanned_region() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (foreign, _child) = foreign_root_aliasing_a_young_child(&mut rt, &mut table, pid);

    // The predicate on its own: the cons cell's address is in no region this process's
    // collector scans (neither the young space nor the mature space).
    {
        let process = table.get(pid).expect("the spawned process");
        assert!(
            super::gc::root_is_unscanned(process, foreign),
            "a cons cell built on the pooled BIF heap is not a young/mature-heap pointer, so \
             the root is unscanned"
        );
        // `descend = true`: the reused scratch plus an unconditional descent is the walk in
        // its fullest form, so the assertion below is about the predicate, not the gate.
        let mut scratch = super::gc_walk::WalkScratch::default();
        assert!(
            super::gc::any_root_is_unscanned(&mut scratch, process, [foreign].iter(), true),
            "`any_root_is_unscanned` must agree with the single-term predicate"
        );
    }

    let mut x = [foreign, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();

    // The decline: the forced collection was NOT run.
    assert_eq!(
        after.collections, before.collections,
        "the collection must be declined while a live root is a container in an unscanned \
         region — it would relocate this heap's young words out from under that root; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "nothing failed; {after:?}");
    assert!(
        rt.gc_request_pending(pid),
        "a declined collection must leave the forced request armed for the runner's park arm"
    );

    // The heap kept its terms, so the root is still the valid `[child]` list and its `car`
    // still reads the intact young `{7,8}` it aliases.
    assert_eq!(
        x[0], foreign,
        "the declined collection moved nothing, so the root word is unchanged"
    );
    assert!(
        beam_term::compare::eq(x[0], foreign, rt.atoms()),
        "the root is structurally the same list after the decline"
    );
    let head = terms::cons_head(x[0]).expect("the root is a cons cell");
    assert!(
        beam_term::compare::eq(terms::cons_tail(x[0]).expect("a cons cell"), NIL, rt.atoms()),
        "the root is exactly the one-element list `[child]`"
    );
    assert_eq!(
        terms::tuple_arity(head).expect("the `car` is a tuple"),
        2,
        "the aliased young child is still the two-element tuple"
    );
    assert_eq!(
        terms::tuple_element(head, 0).expect("in range"),
        make_small(7),
        "element 0 of the aliased child survived the declined collection"
    );
    assert_eq!(
        terms::tuple_element(head, 1).expect("in range"),
        make_small(8),
        "element 1 of the aliased child survived the declined collection"
    );
}

/// **The valve's non-firing guard: an ordinary young-only root set is never declined.**
///
/// A root set made only of immediates and pointers into the process's **own young heap** is
/// fully scanned, so the valve must not fire: `any_root_is_unscanned` is `false` and the
/// forced admission-point collection really runs and relocates both terms (the x register
/// beyond the engine's `live` hint *and* the FR-only term). This is the guard against the
/// valve over-firing and quietly disabling collection for every ordinary workload.
#[test]
fn an_ordinary_young_only_root_set_is_never_declined() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    {
        let process = table.get(pid).expect("the spawned process");
        // `descend = true`: the strongest form of the valve, so "not declined" here really
        // means "nothing reachable in the scanned graph is foreign" — not just the per-word
        // root phase.
        let mut scratch = super::gc_walk::WalkScratch::default();
        assert!(
            !super::gc::any_root_is_unscanned(
                &mut scratch,
                process,
                x.iter().chain(stack.iter()).chain(fr.iter()),
                true
            ),
            "immediates and own-young-heap pointers are all in scanned regions, so no root \
             may be reported as unscanned"
        );
    }

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "the valve must not fire for a young-only root set: the forced collection really ran; \
         {after:?}"
    );
    assert_eq!(after.halted, before.halted, "{after:?}");

    assert_relocated_tuple(x[1], beyond, [7, 8], "x[1] (beyond the `live` hint)");
    assert_relocated_tuple(fr[0], fr_only, [9, 10], "fr[0] (reachable only through the FR file)");
}

// ---------------------------------------------------------------------------------------
// The GC root-set hand-off, hole 3: a foreign container NESTED below a scanned one
// ---------------------------------------------------------------------------------------
//
// The per-word phase classifies a **root word**; it cannot see a foreign container that a
// *scanned* container reaches. `super::gc_walk`'s two-phase check closes that hole with a
// gated descent, and the gate is `Runtime::foreign_reachable(pid)` — the sticky bit every
// event that can put a foreign container into the process's graph opens (a BIF/floor/NIF
// result, an arena fallback, a spawn argument, a delivered fragment, the escape store). A
// test of the NESTED shape must therefore **open the gate first**, exactly as a real BIF
// result would; with the gate closed the descent is skipped by design and the test would be
// vacuous.

/// Builds the depth-≥2 shape the **gated descent** exists to find: `layers` container levels on
/// `pid`'s **scanned** young heap over a container built on the runtime's pooled BIF heap,
/// whose own child aliases a term on `pid`'s young heap.
///
/// Returns `(root, leaf)`:
///
/// * `leaf` — a two-element tuple `{7,8}` on the process's own young heap; and
/// * `root` — `layers` young-heap container levels over a `rt.bif_heaps[0]` one-element tuple
///   holding `leaf`, wrapped in a cons cell (so the shape is a list `[ … ]`).
///
/// The root's own top-level address is a young-heap pointer, so the valve's per-word phase
/// calls it safe; only the descent reaches the BIF-heap container below.
fn nested_foreign_root(
    rt: &mut Runtime,
    table: &mut ProcessTable,
    pid: ProcessId,
    layers: usize,
) -> (Eterm, Eterm) {
    let leaf = table
        .get_mut(pid)
        .expect("the spawned process")
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("the young heap builds {7,8}");
    let mut node = rt
        .bif_heaps
        .first_mut()
        .expect("the runtime always owns its pooled BIF heap")
        .tuple(&[leaf])
        .expect("the pooled BIF heap builds the foreign container");
    for _ in 0..layers {
        node = table
            .get_mut(pid)
            .expect("the spawned process")
            .heap_mut()
            .tuple(&[node])
            .expect("the young heap builds a scanned layer");
    }
    let root = table
        .get_mut(pid)
        .expect("the spawned process")
        .heap_mut()
        .cons(node, NIL)
        .expect("the young heap builds the root cons cell");
    (root, leaf)
}

/// The **clean** twin of [`nested_foreign_root`]: the same `layers`-deep nesting, but every
/// child is a container on the process's **own young heap** — the fully-scanned graph a
/// collection must still be allowed to move.
fn nested_scanned_root(table: &mut ProcessTable, pid: ProcessId, layers: usize) -> (Eterm, Eterm) {
    let leaf = table
        .get_mut(pid)
        .expect("the spawned process")
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("the young heap builds {7,8}");
    let mut node = leaf;
    for _ in 0..layers {
        node = table
            .get_mut(pid)
            .expect("the spawned process")
            .heap_mut()
            .tuple(&[node])
            .expect("the young heap builds a scanned layer");
    }
    let root = table
        .get_mut(pid)
        .expect("the spawned process")
        .heap_mut()
        .cons(node, NIL)
        .expect("the young heap builds the root cons cell");
    (root, leaf)
}

/// Walks `root` down through `steps` one-element container levels — starting at the cons
/// head, asserting each layer's shape — and returns the word the last level's element 0 names.
///
/// A [`nested_foreign_root`] of `n` scanned levels needs `n + 1` steps (the `n` young layers
/// plus the pooled-BIF-heap one); a [`nested_scanned_root`] of `n` levels needs `n`.
fn nested_leaf(root: Eterm, steps: usize, what: &str) -> Eterm {
    let mut node = terms::cons_head(root).unwrap_or_else(|e| panic!("{what}: {e}"));
    for step in 0..steps {
        assert_eq!(
            terms::tuple_arity(node).unwrap_or_else(|e| panic!("{what}: {e}")),
            1,
            "{what}: layer {step} is a one-element tuple"
        );
        node = terms::tuple_element(node, 0).expect("in range");
    }
    node
}

/// Asserts `leaf` is the intact `{7,8}` every fixture here nests at the bottom of the graph.
fn assert_leaf(leaf: Eterm, what: &str) {
    assert_eq!(
        terms::tuple_arity(leaf).unwrap_or_else(|e| panic!("{what}: {e}")),
        2,
        "{what}: the leaf is a two-element tuple"
    );
    assert_eq!(
        terms::tuple_element(leaf, 0).expect("in range"),
        make_small(7),
        "{what}: leaf element 0"
    );
    assert_eq!(
        terms::tuple_element(leaf, 1).expect("in range"),
        make_small(8),
        "{what}: leaf element 1"
    );
}

/// **The hole-3 regression guard (one scanned level): a foreign container NESTED inside a
/// scanned container must decline the collection too.**
///
/// The root is a **young-heap** cons cell — a word the collector scans and relocates — and its
/// element is a one-element tuple built on the runtime's pooled BIF heap, whose element is a
/// tuple on the process's own young heap. The root's own address is scanned, so the valve's
/// cheap per-word phase calls the root set clean; only a descent through the scanned cons
/// reaches the BIF-heap container. Before the gated descent existed the collection ran and
/// moved the nursery out from under the BIF-heap container's child, which then read the dead
/// from-space word.
///
/// The gate is opened first (`Runtime::note_foreign_reachable`), exactly as
/// `RuntimeContext::open_gate_for_foreign_result` opens it for a real BIF result. The assert
/// is on real observable state: `gc_stats().collections` must not advance, the forced request
/// must stay armed for the runner's park arm, and the root — nothing moved — must still read
/// back the intact nested `{7,8}`.
#[test]
fn the_valve_declines_a_foreign_container_nested_in_a_scanned_container() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (root, leaf) = nested_foreign_root(&mut rt, &mut table, pid, 1);

    // The cheap phase alone cannot see the hole: the root word is a young-heap pointer.
    {
        let process = table.get(pid).expect("the spawned process");
        let mut scratch = super::gc_walk::WalkScratch::default();
        assert!(
            !super::gc::any_root_is_unscanned(&mut scratch, process, [root].iter(), false),
            "the per-word phase classifies the root's own (scanned) address, so it alone \
             reports the graph clean — which is exactly why the descent is needed"
        );
    }

    // The event that opens the gate in production: a foreign container entered the graph.
    rt.note_foreign_reachable(pid);
    assert!(rt.foreign_reachable(pid), "the descent gate is open");

    let mut x = [root, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();

    assert_eq!(
        after.collections, before.collections,
        "the gated descent must decline the collection: a container on the pooled BIF heap is \
         reachable below the scanned root, and moving this heap would leave its young child \
         pointing into the evacuated from-space; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "nothing failed; {after:?}");
    assert!(
        rt.gc_request_pending(pid),
        "a declined collection must leave the forced request armed for the runner's park arm"
    );

    // Nothing moved, so the root (and everything below it) is still readable, and the word
    // the BIF-heap container aliases is still the intact young `{7,8}`.
    assert_eq!(
        x[0], root,
        "the declined collection moved nothing, so the root word is unchanged"
    );
    let leaf_read_back = nested_leaf(x[0], 2, "x[0] after the decline");
    assert_eq!(leaf_read_back, leaf, "the aliased young leaf keeps its address");
    assert_leaf(leaf_read_back, "the aliased young leaf after the decline");
}

/// **The hole-3 regression guard (two scanned levels): the descent is transitive.**
///
/// The same shape as [`the_valve_declines_a_foreign_container_nested_in_a_scanned_container`],
/// with **two** scanned container levels above the pooled-BIF-heap one — the root cons, a
/// young tuple, a second young tuple, then the foreign container. A walk that only classified
/// the root's *direct* children would miss it, so this pins the descent as a genuine graph
/// traversal rather than a one-level expansion.
#[test]
fn the_valve_declines_a_deeply_nested_foreign_container() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (root, _leaf) = nested_foreign_root(&mut rt, &mut table, pid, 2);

    // Still clean to the cheap phase — both intermediate layers are young-heap pointers.
    {
        let process = table.get(pid).expect("the spawned process");
        let mut scratch = super::gc_walk::WalkScratch::default();
        assert!(
            !super::gc::any_root_is_unscanned(&mut scratch, process, [root].iter(), false),
            "the per-word phase sees only the scanned root address"
        );
    }

    rt.note_foreign_reachable(pid);

    let mut x = [root, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();

    assert_eq!(
        after.collections, before.collections,
        "the descent must reach a foreign container two scanned levels down; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "nothing failed; {after:?}");
    assert!(
        rt.gc_request_pending(pid),
        "a declined collection must leave the forced request armed"
    );
    assert_eq!(x[0], root, "the declined collection moved nothing");
    assert_leaf(
        nested_leaf(x[0], 3, "x[0] after the decline"),
        "the deeply nested leaf after the decline",
    );
}

/// **The non-firing guard for the descent: a fully-scanned nested graph must STILL collect —
/// with the gate open, and with it closed.**
///
/// The valve must not become an unconditional decline. Two halves:
///
/// * **gate OPEN** — the `O(live objects)` descent really runs over the nested graph and finds
///   nothing foreign, because every layer is on the process's own young heap. The forced
///   collection must run and relocate the whole chain.
/// * **gate CLOSED** — a fresh pid whose gate nothing has opened skips the descent (that is the
///   gate's contract) and decides on the cheap per-word phase alone, which is clean here; the
///   collection must run just the same. This is the guard against a "fix" that made the door
///   decline whenever the gate is closed.
#[test]
fn the_valve_collects_when_the_nested_graph_is_fully_scanned() {
    // --- gate OPEN: the descent runs and must not fire on a fully-scanned graph. ---
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (root, _leaf) = nested_scanned_root(&mut table, pid, 2);
    rt.note_foreign_reachable(pid);
    assert!(
        rt.foreign_reachable(pid),
        "the descent really is enabled for this pid"
    );

    let mut x = [root, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "an all-scanned nested graph must still be collected over even with the descent \
         enabled; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "{after:?}");
    assert!(
        !rt.gc_request_pending(pid),
        "the committed collection consumed the forced request"
    );
    assert_ne!(
        x[0], root,
        "the collection really moved the nested graph (the root was relocated)"
    );
    assert_leaf(
        nested_leaf(x[0], 2, "x[0] after the collection"),
        "the fully-scanned nested leaf after the collection",
    );

    // --- gate CLOSED: the descent is skipped and the cheap phase alone decides. ---
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (root, _leaf) = nested_scanned_root(&mut table, pid, 2);
    assert!(
        !rt.foreign_reachable(pid),
        "a fresh pid's descent gate is closed until an event opens it"
    );

    let mut x = [root, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "with the gate closed the descent is skipped and the cheap phase — clean here — lets \
         the collection run; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "{after:?}");
    assert_ne!(x[0], root, "the collection really moved the nested graph");
    assert_leaf(
        nested_leaf(x[0], 2, "x[0] after the closed-gate collection"),
        "the fully-scanned nested leaf after the closed-gate collection",
    );
}

/// **The same guard on the collecting allocation seam.**
///
/// [`ExecutionContext::alloc_words_rooted`] is the second site the fix reconciles: when the
/// running process's young bump fails it collects over the handed-in root set — but a root in
/// an unscanned region makes that move unsound, so the valve must decline the collection and
/// take the pointer-stable arena fallback instead (`Runtime::gc_stats().fallbacks` advances,
/// `collections` does not).
///
/// The door is driven by a **forced** collection request, which **bypasses** the young-heap bump
/// (and with it the no-relocation grow-and-retry an unforced capacity refusal takes — a growth
/// collects nothing, so it is safe against any root set and would never reach this valve). The
/// nursery is filled with garbage as well, so the collection the request asks for is a real one
/// the valve has to refuse.
#[test]
fn a_rooted_allocation_is_declined_when_a_live_root_lives_in_an_unscanned_region() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (foreign, _child) = foreign_root_aliasing_a_young_child(&mut rt, &mut table, pid);

    // Fill the nursery with unreachable tuples so a 16-word bump cannot be served.
    {
        let process = table.get_mut(pid).expect("the spawned process");
        let filler = [make_small(1)];
        while process.heap().available() >= 16 {
            process
                .heap_mut()
                .tuple(&filler)
                .expect("the nursery has room for a two-word tuple");
        }
    }

    let mut x = [foreign, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        let block = ctx
            .alloc_words_rooted(16, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the declined collection falls back to the arena, which never errors");
        assert!(!block.is_null(), "a block was really handed out");
    }
    let after = rt.gc_stats();

    assert_eq!(
        after.collections, before.collections,
        "the refused bump must NOT collect over a root set holding a container in an unscanned \
         region; {after:?}"
    );
    assert_eq!(
        after.fallbacks,
        before.fallbacks + 1,
        "the declined collection takes the pointer-stable arena fallback; {after:?}"
    );
    assert_eq!(
        x[0], foreign,
        "the arena fallback moved nothing, so the unscanned root is unchanged"
    );
    let head = terms::cons_head(x[0]).expect("the root is still the cons cell");
    assert_eq!(
        terms::tuple_element(head, 0).expect("in range"),
        make_small(7),
        "the aliased young child is still intact"
    );
}

/// **An unforced rooted allocation whose bump hits a capacity refusal is served by the
/// no-relocation growth, never by the valve.**
///
/// This is the counterpart of
/// [`a_rooted_allocation_is_declined_when_a_live_root_lives_in_an_unscanned_region`]: the same
/// rig, the same filled nursery and the same **live unscanned root** in `x[0]`, but **no forced
/// request**. The bump's capacity refusal is met by [`Process::grow_nursery`] — which retires the
/// space it replaces without moving a single live term, so it is safe against any root set — and
/// the retried bump serves the block. The unscanned root therefore costs nothing here: no
/// collection runs (the roots are never consulted), no arena fallback is taken, and every word —
/// the unscanned root included — stays exactly where it was.
///
/// What the growth does leave behind is [`Process::collection_debt`]: the retired space can only
/// be reclaimed by a collection, which the next admission point's occupancy test then forces.
#[test]
fn a_rooted_allocation_whose_bump_is_refused_grows_the_nursery_instead_of_declining() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (foreign, _child) = foreign_root_aliasing_a_young_child(&mut rt, &mut table, pid);

    // Fill the nursery with unreachable tuples so a 16-word bump cannot be served.
    {
        let process = table.get_mut(pid).expect("the spawned process");
        let filler = [make_small(1)];
        while process.heap().available() >= 16 {
            process
                .heap_mut()
                .tuple(&filler)
                .expect("the nursery has room for a two-word tuple");
        }
        assert!(
            process.heap().available() < 16,
            "the nursery is full: the bump really is refused"
        );
        assert!(!process.collection_debt(), "no growth has been taken yet");
    }

    let mut x = [foreign, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    let block = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.alloc_words_rooted(16, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the grown young heap serves the retried bump")
    };
    let after = rt.gc_stats();

    assert!(!block.is_null(), "a block was really handed out");
    assert_eq!(
        after.collections, before.collections,
        "the growth collects nothing, so no collection runs: {after:?}"
    );
    assert_eq!(
        after.fallbacks, before.fallbacks,
        "the block came from the process's own heap, not the pointer-stable arena: {after:?}"
    );
    assert_eq!(
        after.halted, before.halted,
        "nothing failed, so collection is not halted; {after:?}"
    );
    assert_eq!(
        x[0], foreign,
        "nothing moved, so the unscanned root is unchanged"
    );
    let process = table.get(pid).expect("the spawned process");
    assert!(
        process.collection_debt(),
        "the growth left the retired space behind as collection debt, which the next admission \
         point's occupancy test turns into a real collection"
    );
    assert!(
        process.heap().available() >= 16,
        "the retried bump really fitted in the grown space: {} words available",
        process.heap().available()
    );
}

/// **The collecting allocation seam serves a forced `garbage_collect` request that a bump would
/// have answered on its own.**
///
/// `erlang:garbage_collect/0,1` (`erts_internal:garbage_collect/1`) answers the oracle's `true`
/// but performs no collection itself: it records **one** request (`Runtime::request_gc`) and the
/// process's own root set is collected over at the sites that hold it. [`alloc_words_rooted`] is
/// one of them, and for a process whose only allocation goes through this seam it is the *only*
/// one — before the fix the request was carried, silently, all the way to the pass boundary,
/// where the runner's park arm no longer had the machine to collect over.
///
/// The nursery is deliberately **not** filled: the bump this call would reach succeeds on its
/// own, so a collection here can only be the forced request's. The block still comes back from
/// the process's own young heap (the collection emptied the nursery and then served the retry),
/// and both roots really moved.
#[test]
fn the_collecting_allocation_seam_serves_a_forced_request_a_bump_would_have_answered() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    let before = rt.gc_stats();
    assert!(
        !rt.gc_request_pending(pid),
        "the rig starts with no request armed; {before:?}"
    );
    rt.request_gc(pid);
    let block = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.alloc_words_rooted(4, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("a rooted allocation")
    };
    let after = rt.gc_stats();

    assert_eq!(
        after.collections,
        before.collections + 1,
        "the forced request really ran a collection, though the young bump would have succeeded; \
         {after:?}"
    );
    assert_eq!(after.halted, before.halted, "{after:?}");
    assert!(
        !rt.gc_request_pending(pid),
        "the committed collection consumed the forced request; {after:?}"
    );
    assert_eq!(
        after.fallbacks, before.fallbacks,
        "the collection emptied the nursery, so the retry was served from the young heap (no arena \
         fallback); {after:?}"
    );
    assert!(!block.is_null(), "a block was really handed out");
    assert!(
        table
            .get(pid)
            .expect("the process is still in the table")
            .collector()
            .young_contains(block as *const Eterm),
        "the retried block is served from the process's own young nursery — an arena block would \
         fail this region check"
    );

    assert_relocated_tuple(x[1], beyond, [7, 8], "x[1] (beyond the `live` hint)");
    assert_relocated_tuple(fr[0], fr_only, [9, 10], "fr[0] (reachable only through the FR file)");
}

/// **The forced request is consumed EXACTLY once: a second allocation on the same rig does not
/// collect.**
///
/// The first call answered the pending `garbage_collect`, so the second must take the ordinary
/// path — the nursery's bump, which the now-empty nursery can serve — and leave `collections`
/// alone. A seam that peeked the request without consuming it (or re-armed it) would collect on
/// every allocation while the flag stayed set, the opposite failure of the one the fix closes.
#[test]
fn the_collecting_allocation_seam_consumes_the_forced_request_exactly_once() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.alloc_words_rooted(4, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the first rooted allocation");
    }
    let after_first = rt.gc_stats();
    assert_eq!(
        after_first.collections,
        before.collections + 1,
        "the first call served the forced request; {after_first:?}"
    );

    // The same rig, a second call: the request is spent, so this is an ordinary young-heap bump.
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.alloc_words_rooted(4, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the second rooted allocation");
    }
    let after_second = rt.gc_stats();
    assert_eq!(
        after_second.collections, after_first.collections,
        "the request was already spent, so the second call takes the ordinary young-heap bump and \
         does not collect; {after_second:?}"
    );
    assert_eq!(
        after_second.fallbacks, after_first.fallbacks,
        "the second call is a plain bump, so it takes no arena fallback either; {after_second:?}"
    );
}

/// **A forced request the valve DECLINES stays armed for the runner's park arm.**
///
/// The collecting allocation seam is exactly the site the runner's park arm falls back on, so a
/// request refused here — an unscanned root would make the move unsound — must NOT be consumed:
/// the park arm serves it later, where the machine's root set is provably at rest. The nursery is
/// filled with unreachable tuples so the ordinary (non-forced) refusal path is exercised too, but
/// here the request **bypasses** the bump and it is the valve that declines.
#[test]
fn the_collecting_allocation_seam_leaves_a_declined_forced_request_armed() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (foreign, _child) = foreign_root_aliasing_a_young_child(&mut rt, &mut table, pid);

    // Fill the nursery with unreachable tuples so a 16-word bump cannot be served — the shape the
    // ordinary refusal takes, so the case would exercise the same decline without the request.
    {
        let process = table.get_mut(pid).expect("the spawned process");
        let filler = [make_small(1)];
        while process.heap().available() >= 16 {
            process
                .heap_mut()
                .tuple(&filler)
                .expect("the nursery has room for a two-word tuple");
        }
    }

    let mut x = [foreign, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        let block = ctx
            .alloc_words_rooted(16, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the declined collection falls back to the arena, which never errors");
        assert!(!block.is_null(), "a block was really handed out");
    }
    let after = rt.gc_stats();

    assert_eq!(
        after.collections, before.collections,
        "the valve must decline the forced collection while a live root is a container in a \
         region the collector does not scan; {after:?}"
    );
    assert_eq!(
        after.fallbacks,
        before.fallbacks + 1,
        "the declined collection takes the pointer-stable arena fallback; {after:?}"
    );
    assert!(
        rt.gc_request_pending(pid),
        "a declined forced request must stay armed: the runner's park arm is where it can still be \
         served, and consuming it here would silently drop the caller's `garbage_collect`"
    );
    assert_eq!(
        x[0], foreign,
        "the arena fallback moved nothing, so the unscanned root is unchanged"
    );
    let head = terms::cons_head(x[0]).expect("the root is still the cons cell");
    assert_eq!(
        terms::tuple_element(head, 0).expect("in range"),
        make_small(7),
        "the aliased young child is still intact"
    );
}

/// The **normal** path: a machine that returns ends the drive immediately, however short the
/// cap is — the deadline is a safety net, never the ordinary exit.
#[test]
fn a_returned_blocking_machine_ends_the_pump_without_the_cap() {
    let value = super::pump_until_returned(
        std::time::Duration::ZERO,
        RunOutcome::Returned(make_small(7)),
        |_f, _p| panic!("a returned machine never pumps"),
    )
    .expect("a returned machine yields its value");
    assert_eq!(signed_value(value), 7);
}

/// A `step` that reports **nothing ran** (`Ok(None)` — a context that may not pump, or a
/// drained queue) keeps the engine's honest typed `blocked` gap, and does so before the cap
/// (checked first) could fire.
#[test]
fn a_step_that_reports_nothing_ran_keeps_the_typed_blocked_gap() {
    let error = super::pump_until_returned(
        super::BLOCKING_PUMP_DEADLINE,
        RunOutcome::Wait { func: 0, pc: 0 },
        |_f, _p| Ok(None),
    )
    .expect_err("nothing ran is the blocked gap");
    assert_eq!(error.to_string(), beam_engine::blocked().to_string());
}

// -----------------------------------------------------------------------
// the nested run's own finite `receive … after T` deadline
// -----------------------------------------------------------------------

/// The committed `receive … after T` fixture (regeneration recipe in
/// `tests/fixtures/README.md`): every export pins one literal-timeout opcode shape.
const RECV_AFTER_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/recv_after.beam"
);

/// A **nested run** — the [`RuntimeContext::run_export_blocking`] route the `-eval` driver
/// and the fun bridge use — really delivers the timeout value of a finite
/// `receive … after T` that only its own deadline can satisfy.
///
/// The engine arms **no** timer for a `wait_timeout`, and the machine a nested run drives is
/// not a scheduled process, so no scheduler pass ever fires this deadline: with an empty run
/// queue the pump reports `0` on every round. [`super::RuntimeContext::pump_once`] therefore
/// recovers the deadline through
/// [`Machine::wait_deadline`](beam_engine::Machine::wait_deadline) and waits it out itself —
/// without that, this very call is the typed `blocked()` gap. Oracle:
/// `recv_after:after20()` ⇒ `timeout`.
#[test]
fn a_nested_finite_receive_delivers_its_timeout() {
    let image = beam_loader::load_file(RECV_AFTER_BEAM).expect("load recv_after.beam");
    // The fixture's own `timeout` atom is the one its code stream carries, so the term the
    // machine returns is exactly this atom.
    let expected = image
        .atom_table
        .lookup("timeout")
        .expect("the fixture names `timeout`");
    let mut rt = runtime();

    let started = Instant::now();
    let value = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.run_export(&image, "after20", 0, &[])
            .expect("the nested run waits its own deadline out")
    };
    let elapsed = started.elapsed();

    assert_eq!(
        value,
        make_atom(expected),
        "the `after` body ran: the timeout value, never the typed `blocked()` gap"
    );
    assert!(
        elapsed >= Duration::from_millis(20),
        "the timeout is never delivered before its 20 ms delay has really elapsed \
         (took {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the wait ends at the deadline rather than spinning out the safety cap \
         (took {elapsed:?})"
    );
}

/// The negative control for the case above: a parked `receive … after infinity` compiles to a
/// bare `wait`, so [`Machine::wait_deadline`](beam_engine::Machine::wait_deadline) reports
/// `delay_ms: None` — no deadline to wait for. The unchanged pump rule therefore stands: one
/// pump that reports nothing ran is the honest typed `blocked()` gap, never a wait for a
/// deadline that does not exist (and never a hang).
#[test]
fn a_nested_infinite_receive_keeps_the_typed_blocked_gap() {
    let image = beam_loader::load_file(RECV_AFTER_BEAM).expect("load recv_after.beam");
    let mut rt = runtime();
    let error = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.run_export(&image, "wait_forever", 0, &[])
            .expect_err("`after infinity` has no deadline to wait for")
    };
    assert_eq!(error.to_string(), beam_engine::blocked().to_string());
}

/// The **other** half of the finite-`after` branch: a message that arrives before the deadline
/// wins the race, and the abandoned deadline is not also fired.
///
/// Oracle: a receiver parked in `receive X -> X after 2000 -> timeout end` that is sent a
/// message returns **that message** — the send cancels the deadline, so the answer is neither
/// `timeout` nor a wait for the full 2000 ms. The message reaches the very mailbox the nested
/// run scans through the pump itself (`erlang:send_after/3`'s due deadline is fired by the
/// scheduler pass), so this pins the "a pump that reported progress resumes the machine and a
/// non-`Wait` outcome is returned at once" half of
/// [`super::RuntimeContext::pump_once`] on the finite path.
#[test]
fn a_message_beats_a_nested_finite_receive_deadline() {
    let image = beam_loader::load_file(RECV_AFTER_BEAM).expect("load recv_after.beam");
    let mut rt = runtime();
    let me = self_term(&rt);
    let message = make_small(42);

    let mut ctx = RuntimeContext::new(&mut rt);
    // 5 ms — far below `after2000/0`'s 2000 ms deadline, so the message can only win it.
    let _reference = ctx
        .call_bif("erlang", "send_after", 3, &[make_small(5), me, message])
        .expect("arm a 5 ms timer to the runtime's own mailbox");

    let started = Instant::now();
    let value = ctx
        .run_export(&image, "after2000", 0, &[])
        .expect("the pump delivers the timed message and the receive takes it");
    let elapsed = started.elapsed();

    assert_eq!(value, message, "the message cancelled the deadline");
    assert!(
        elapsed < Duration::from_millis(1000),
        "the 2000 ms deadline was abandoned, not waited out (took {elapsed:?})"
    );
    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    assert!(mailbox.is_empty(), "the receive removed the message");
}

// ---------------------------------------------------------------------------------------
// The installed `CollectionGate` host seam: the SAME valve, at the `beam_process` signal sites
// ---------------------------------------------------------------------------------------
//
// The signal sites (a monitor `'DOWN'`, an exit propagation, an applied trap reason) run inside
// `beam_process::ProcessTable` with only `&mut ProcessTable` in hand, so they cannot consult the
// runtime's unscanned-root valve; `Runtime::new` installs `RuntimeCollectionGate` on the
// runtime's table as their half of exactly that decision. The case below pins the seam end to
// end, through a real signal site, for both its firing and non-firing behaviour.

/// **The installed gate declines a signal-driven relocation of a foreign-reachable receiver.**
///
/// The receiver's **gathered** roots reach a container at depth ≥ 2 on the pooled BIF heap (a
/// foreign container below the process's own scanned young layers) — the shape only the
/// *descent* can see. An exit signal whose reason is larger than the receiver's whole nursery
/// must then be delivered **without** collecting or moving that heap: the enqueue-time copy is
/// refused and the message is built into a heap fragment instead, so the notification is never
/// lost.
#[test]
fn the_installed_gate_declines_a_signal_driven_relocation_of_a_foreign_reachable_receiver() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    // A foreign container at depth >= 2 below a SCANNED root: the pooled-BIF-heap tuple is
    // reachable only through the process's own young layers, so the cheap per-word phase cannot
    // see it and the descent is what flags the receiver.
    let (root, _leaf) = nested_foreign_root(&mut rt, &mut table, pid, 2);
    table
        .get_mut(pid)
        .expect("the receiver is live")
        .registers_mut()
        .extra_roots_mut()
        .push(root);
    // The production gate, exactly as `Runtime::new` installs it on the runtime's own table.
    table.set_collection_gate(Box::new(super::gc_walk::RuntimeCollectionGate::new()));
    assert!(table.has_collection_gate(), "the host gate is installed");

    // The seam itself, called directly: the answer for this receiver is `true`.
    {
        let process = table.get(pid).expect("the receiver is live");
        let mut gate = super::gc_walk::RuntimeCollectionGate::new();
        assert!(
            <super::gc_walk::RuntimeCollectionGate as beam_process::CollectionGate>::declines_relocation(
                &mut gate,
                process
            ),
            "a container at depth >= 2 on the pooled BIF heap is reachable from the receiver's \
             gathered roots (its extra roots), so the gate must decline"
        );
    }

    // A reason LARGER than the receiver's whole nursery: without a decline the enqueue-time copy
    // would collect and grow the receiver's heap.
    let signaller = table.spawn_default().expect("a signaller");
    table.get_mut(pid).expect("live").set_trap_exit(true);
    let capacity = table
        .get(pid)
        .expect("live")
        .collector()
        .young()
        .capacity_words();
    let mut src = beam_alloc::Heap::new(capacity + 8).expect("the source arena allocates");
    let elems: Vec<Eterm> = (0..capacity).map(|i| make_small(i as i64)).collect();
    let reason = src.tuple(&elems).expect("the oversized reason fits the arena");

    let minor_before = table.get(pid).expect("live").collector().minor_gcs();
    let base_before = table.get(pid).expect("live").collector().young().base();
    let mailbox_before = table.get(pid).expect("live").mailbox().len();

    table
        .send_exit(signaller, pid, reason)
        .expect("the trapping receiver takes the signal");

    let r = table.get(pid).expect("the receiver survived");
    assert_eq!(
        r.collector().minor_gcs(),
        minor_before,
        "the installed gate declined the signal-driven relocation"
    );
    assert_eq!(
        r.collector().young().base(),
        base_before,
        "a declined signal must not move or grow the nursery"
    );
    assert_eq!(
        r.mailbox().len(),
        mailbox_before + 1,
        "the notification is NOT lost: it is enqueued fragment-resident"
    );
    assert!(
        !r.is_relocation_exempt(),
        "the signal route leaves the receiver's own exemption flag alone"
    );
    // The new message is fragment-resident and carries the exit tuple whole.
    let exit_msg = {
        let process = table.get_mut(pid).expect("live");
        let mut last = None;
        while let Some(message) = process.receive(|_| true) {
            last = Some(message);
        }
        last.expect("the exit message is in the mailbox")
    };
    assert!(
        exit_msg.fragments().is_some(),
        "the declining gate's fallback enqueued a fragment-resident message"
    );
    let payload = exit_msg.payload();
    assert_eq!(
        terms::tuple_arity(payload).expect("the exit message is a tuple"),
        3,
        "the exit message is the three-tuple EXIT/From/Reason"
    );
    assert_eq!(
        terms::tuple_element(payload, 1).expect("in range"),
        signaller.to_eterm(),
        "the signaller is the exit sender"
    );

    // --- the non-firing guard: an identical receiver with NOTHING foreign reachable takes the
    // ordinary collect-and-grow arm, so the gate is not an unconditional decline. ---
    let clean = table.spawn_default().expect("a clean receiver");
    table.get_mut(clean).expect("live").set_trap_exit(true);
    let minor_before = table.get(clean).expect("live").collector().minor_gcs();
    table
        .send_exit(signaller, clean, reason)
        .expect("the clean receiver takes the signal");
    let r = table.get(clean).expect("the clean receiver survived");
    assert!(
        r.collector().minor_gcs() > minor_before,
        "with nothing foreign reachable the gate must NOT fire: the ordinary arm collects"
    );
    assert_eq!(
        r.mailbox().len(),
        1,
        "the signal is delivered onto the receiver's own (grown) heap"
    );
}

/// The seam call the latch cases below drive, spelled once: whether `gate` declines relocating
/// `pid`'s heap, asked through the same trait entry point a `beam_process` signal site uses.
fn gate_declines(
    gate: &mut super::gc_walk::RuntimeCollectionGate,
    table: &ProcessTable,
    pid: ProcessId,
) -> bool {
    let process = table.get(pid).expect("the receiver is live");
    <super::gc_walk::RuntimeCollectionGate as beam_process::CollectionGate>::declines_relocation(
        gate, process,
    )
}

/// Pushes the depth-≥2 foreign shape below `pid`'s **own** scanned layers into the roots
/// `Process::collect` gathers (`registers_mut().extra_roots_mut()`), so a gate over `pid`
/// is asked about exactly the graph only the descent can classify.
fn make_receiver_foreign_reachable(
    rt: &mut Runtime,
    table: &mut ProcessTable,
    pid: ProcessId,
) -> Eterm {
    let (root, _leaf) = nested_foreign_root(rt, table, pid, 2);
    table
        .get_mut(pid)
        .expect("the receiver is live")
        .registers_mut()
        .extra_roots_mut()
        .push(root);
    root
}

/// **The gate is `O(1)` while the pid's descent latch is clear: it answers `false` WITHOUT
/// descending.**
///
/// The receiver carries the same shape the installed-gate case uses — a pooled-BIF-heap container
/// two scanned young-heap levels below its gathered extra roots, which the cheap per-word phase
/// cannot see. Two answers are compared over the **identical** graph, differing only by the latch
/// bit:
///
/// * a **clear** shared cell returns `false` and the gate's descent counter does **not** move, so
///   the answer came from the `O(1)` bit test rather than from a walk that happened to find
///   nothing; and
/// * with the pid's bit **set** (`latch.borrow_mut().insert(pid)`, what
///   `Runtime::note_foreign_reachable` does) the very same receiver answers `true` after exactly
///   one descent — proving the descent *would* have found the hit, i.e. the clear-latch answer
///   really was a skipped descent.
#[test]
fn the_gate_skips_the_descent_while_the_latch_is_clear() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let _root = make_receiver_foreign_reachable(&mut rt, &mut table, pid);

    // --- a CLEAR latch: `false`, with no descent at all. ---
    let latch = super::gc_walk::ForeignLatch::default();
    let mut gate = super::gc_walk::RuntimeCollectionGate::from(Rc::clone(&latch));
    assert!(
        !gate_declines(&mut gate, &table, pid),
        "a clear latch means no container from an unscanned region ever entered this process's \
         reachable graph, so the gate must answer `false`"
    );
    assert_eq!(
        gate.descents(),
        0,
        "the clear latch must skip the descent outright — a `false` that came from a walk would \
         be a different (and arbitrarily expensive) claim"
    );

    // --- the SAME receiver, with the bit SET: the descent runs and reaches the hit. ---
    latch.borrow_mut().insert(pid);
    assert!(
        gate_declines(&mut gate, &table, pid),
        "with the bit open the descent runs over the identical graph and reaches the \
         pooled-BIF-heap container, so the gate must decline"
    );
    assert_eq!(
        gate.descents(),
        1,
        "the open latch really descended exactly once"
    );
}

/// **The gate reads the host's LIVE latch cell, never a snapshot taken at construction.**
///
/// The gate is built over a clone of one shared cell, and a host mutates it through a handle the
/// gate does not own — exactly the shape `Runtime::note_foreign_reachable` produces. The same gate
/// instance must therefore change its answer when the cell changes underneath it: `false` with no
/// descent while the bit is clear, then `true` after one descent once the bit is set.
#[test]
fn the_gates_latch_is_the_shared_cell_a_host_mutates() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let _root = make_receiver_foreign_reachable(&mut rt, &mut table, pid);

    let latch = super::gc_walk::ForeignLatch::default();
    let mut gate = super::gc_walk::RuntimeCollectionGate::from(Rc::clone(&latch));
    assert!(
        !gate_declines(&mut gate, &table, pid),
        "the latch starts clear, so the gate cannot descend"
    );
    assert_eq!(gate.descents(), 0, "no descent ran for the clear bit");

    // The host's mutation, through the handle the gate only holds a clone of.
    latch.borrow_mut().insert(pid);

    assert!(
        gate_declines(&mut gate, &table, pid),
        "the same gate instance must see the host's mutation and descend"
    );
    assert_eq!(
        gate.descents(),
        1,
        "the gate consulted the live shared cell and ran the descent once"
    );
}

/// **The non-firing twin: an OPEN latch is not an unconditional decline.**
///
/// The change must not be read as "a latched gate always declines". With the pid's bit set over a
/// **fully-scanned** nested graph (every layer on the receiver's own young heap, in the gathered
/// extra roots), the gate really descends — its counter moves — and finds nothing foreign, so a
/// relocation is allowed. This pins the one-way soundness argument: the bit gates only whether the
/// descent *runs*, never what it answers.
#[test]
fn the_latched_gate_still_allows_a_fully_scanned_receiver() {
    let (_rt, mut table, pid) = gc_root_set_rig();
    let (root, _leaf) = nested_scanned_root(&mut table, pid, 2);
    table
        .get_mut(pid)
        .expect("the receiver is live")
        .registers_mut()
        .extra_roots_mut()
        .push(root);

    let latch = super::gc_walk::ForeignLatch::default();
    latch.borrow_mut().insert(pid);
    let mut gate = super::gc_walk::RuntimeCollectionGate::from(Rc::clone(&latch));

    assert!(
        !gate_declines(&mut gate, &table, pid),
        "an open latch over a fully-scanned graph must still allow the relocation"
    );
    assert_eq!(
        gate.descents(),
        1,
        "the open latch really ran the descent — the `false` is the descent's own answer, not a \
         skipped one"
    );
}

// ---------------------------------------------------------------------------------------------
// A registered BIF's container result is MOVED onto the running process's own (SCANNED) young
// heap and the pooled BIF heap is then REWOUND.
// ---------------------------------------------------------------------------------------------

/// Builds the list `[a, b, c]` on `pid`'s **own young heap**.
///
/// A BIF's *arguments* come from the caller's heap, so a case exercising the result move must not
/// hand the BIF a list built on the pooled BIF heap (that would test the wrong region).
fn young_int_list(table: &mut ProcessTable, pid: ProcessId, values: &[i64]) -> Eterm {
    let heap = table.get_mut(pid).expect("the process").heap_mut();
    let mut list = NIL;
    for value in values.iter().rev() {
        list = heap
            .cons(make_small(*value), list)
            .expect("the young heap builds the list");
    }
    list
}

/// **A registered BIF's container result is re-homed onto the process's own young heap, and the
/// pooled BIF heap is rewound.**
///
/// Two halves of the same decision, asserted together because either alone would be a weaker
/// claim:
///
/// * the result's address lies in the process's **young space** — the region the copying
///   collector scans — so [`super::gc::root_is_unscanned`] no longer classifies it as a foreign
///   container; and
/// * the pooled heap's `used()` is back at its pre-call mark (0 for a fresh pool), so the pool
///   serves the next call from word 0 instead of marching toward its capacity.
///
/// Before the copy-out the result stayed on the pool: it was foreign, so the unscanned-root valve
/// declined every collection while it was live in an x register, and the pool's bump pointer only
/// ever advanced.
#[test]
fn a_bif_container_result_is_moved_onto_the_process_heap_and_the_pool_is_rewound() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let pool_before = rt.bif_heaps.first().expect("the pool owns a heap").used();
    assert_eq!(pool_before, 0, "the pooled heap starts untouched");

    let list = young_int_list(&mut table, pid, &[1, 2, 3]);
    let tuple = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.call_bif("erlang", "list_to_tuple", 1, &[list])
            .expect("list_to_tuple([1,2,3])")
    };

    assert_eq!(
        terms::tuple_arity(tuple).expect("the result is a tuple"),
        3,
        "the BIF really built a container"
    );
    let process = table.get(pid).expect("the process");
    assert!(
        !super::gc::root_is_unscanned(process, tuple),
        "the BIF result must live in a region the collector scans"
    );
    assert!(
        process
            .collector()
            .young_contains(beam_term::list::ptr_val(tuple) as *const Eterm),
        "the result's pointer is inside the process's young nursery"
    );

    assert_eq!(rt.bif_heaps.len(), 1, "the one pooled heap came back");
    assert_eq!(
        rt.bif_heaps[0].used(),
        pool_before,
        "the pooled BIF heap must be rewound once the result has been copied out"
    );
}

/// **An immediate result rewinds the pool without paying a copy.**
///
/// The arithmetic/immediate hot path must not become a copier: an immediate is neither boxed nor
/// a cons, so nothing reachable can live on the pooled heap and the call is free to rewind it
/// outright. The case pins both halves — a plain small integer comes back unchanged, and the pool
/// is at its base afterwards.
#[test]
fn an_immediate_result_rewinds_the_pool_without_a_copy() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let sum = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.call_bif("erlang", "+", 2, &[make_small(2), make_small(3)])
            .expect("2 + 3")
    };
    assert!(is_small(sum), "the result is an immediate");
    assert_eq!(signed_value(sum), 5);
    assert_eq!(
        rt.bif_heaps.first().expect("the pool owns a heap").used(),
        0,
        "an immediate result allocates nothing on the pool, so the pool is rewound"
    );
}

/// **An ESCAPING arm neither moves the result nor rewinds the pool.**
///
/// The seven-arm `safe_to_rewind` gate exists because six of the `DispatchHooks` arms let a term
/// the BIF built on `ctx.heap()` escape into state that outlives the call. The case drives one of
/// them — the process-dictionary arm — with a BIF that **does** allocate on the BIF heap:
/// `erlang:get_keys/1` builds its key list there. The stored term (`7`) is a small integer, so
/// the observable proof is the *result*: it stays on the pooled heap — its address is outside
/// every region the process's collector scans — and the pool's `used()` is **not** reset.
///
/// The witness is the result's **region**, not `root_is_unscanned`'s verdict: the key list is a
/// cons of immediates and so, under the valve's forwarding fact, it is an *inert* foreign
/// container that never declines a collection by itself. What must not happen is the copy-out —
/// a moved result would sit in the process's young space instead.
#[test]
fn an_escaping_arm_neither_moves_the_result_nor_rewinds_the_pool() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    // The process dictionary holds one pair, so `get_keys/1` really builds a one-element list.
    table
        .get_mut(pid)
        .expect("the process")
        .dict_mut()
        .put(make_small(7), make_small(11));

    let keys = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.call_bif("erlang", "get_keys", 1, &[make_small(11)])
            .expect("get_keys(11)")
    };

    // The result is the key list `[7]` — a cons cell built on the pooled BIF heap.
    assert!(
        beam_term::list::is_list(keys),
        "get_keys/1 builds a list on `ctx.heap()`"
    );
    assert_eq!(terms::cons_head(keys).expect("a cons cell"), make_small(7));
    assert_eq!(
        terms::cons_tail(keys).expect("a cons cell"),
        NIL,
        "the key list is exactly `[7]`"
    );

    let process = table.get(pid).expect("the process");
    // The witness is the result's REGION, not `root_is_unscanned`'s verdict: `[7]` is a cons of
    // immediates, so under the valve's forwarding fact it is an *inert* foreign container and
    // never declines a collection by itself. A copy-out would instead have put it in the
    // process's young space.
    assert!(
        !process
            .collector()
            .young_contains(beam_term::list::ptr_val(keys) as *const Eterm),
        "the `dict` arm is an escaping seam, so the result must NOT have been moved onto the \
         process's scanned heap"
    );
    assert!(
        rt.bif_heaps.first().expect("the pool owns a heap").used() > 0,
        "the pooled heap must NOT be rewound: the escaping arm forbids it"
    );
    // The term the arm let escape is still valid (nothing was rewound).
    assert_eq!(
        terms::cons_head(keys).expect("the cons cell is still readable"),
        make_small(7)
    );
}

/// **A PARTIALLY-USED pool is rewound to the CALLER's mark, not to the heap's base.**
///
/// The pool is shared, so a heap can reach a call already holding words another caller still
/// owns: the native floor's reference minting, trace and the ETS-owner sweep all pop a heap and
/// put it back **without** resetting it. The call therefore reads `mark = heap.used()` before it
/// runs and gives back exactly `[mark..used)`.
///
/// The case pins that with a retained `{1,2}` another caller built on the pool (3 words) and a
/// `list_to_tuple/1` call (which builds a 4-word tuple there):
///
/// * the pool's offset afterwards is that caller's `mark` — not `mark + 4` (the pre-fix behaviour:
///   the heap arrived dirty, so the call refused to rewind at all and the bump pointer marched
///   toward the pool's capacity) and not `0` (a rewind to the base, which would recycle the
///   words another caller owns);
/// * the words below the mark are intact — the retained tuple still reads back `{1,2}`;
/// * the copy-out is unaffected: the result is the *moved* container on the process's own
///   scanned young heap.
#[test]
fn a_partially_used_pool_is_rewound_to_the_callers_mark() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    // A term a previous caller owns, built on the pooled BIF heap and left there: a `{1,2}` tuple
    // costs a one-word header plus two element words, so the heap reaches this call at
    // `used() == 3`. The heap stays in the pool (index 0 is never popped by this rig), so the term
    // is exactly the "live word below the mark" the rewind must keep.
    let retained = rt
        .bif_heaps
        .first_mut()
        .expect("the runtime always owns its pooled BIF heap")
        .tuple(&[make_small(1), make_small(2)])
        .expect("the pooled BIF heap builds the retained tuple");
    let mark = rt.bif_heaps.first().expect("the pool owns a heap").used();
    assert_eq!(mark, 3, "a `{{1,2}}` tuple is a header word plus two element words");

    let list = young_int_list(&mut table, pid, &[4, 5, 6]);
    let tuple = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.call_bif("erlang", "list_to_tuple", 1, &[list])
            .expect("list_to_tuple([4,5,6])")
    };

    assert_eq!(
        terms::tuple_arity(tuple).expect("the result is a tuple"),
        3,
        "the BIF really built a container"
    );
    // The copy-out is unaffected by the (previously disqualifying) dirty pool: the result really
    // left the pool for the process's scanned young nursery.
    let process = table.get(pid).expect("the process");
    assert!(
        process.collector().young_contains(beam_term::list::ptr_val(tuple) as *const Eterm),
        "the result's pointer is inside the process's young nursery"
    );

    assert_eq!(rt.bif_heaps.len(), 1, "the one pooled heap came back");
    assert_eq!(
        rt.bif_heaps[0].used(),
        mark,
        "the pool must be rewound to the CALLER's mark: this call's own words `[mark..used)` are \
         freed, the words another caller owns below `mark` are kept"
    );
    // The retained words are untouched: the term the previous caller owns still reads back.
    assert_eq!(
        terms::tuple_arity(retained).expect("the retained tuple is still readable"),
        2,
        "the words below the mark must survive the rewind"
    );
    assert_eq!(
        terms::tuple_element(retained, 0).expect("in range"),
        make_small(1)
    );
    assert_eq!(
        terms::tuple_element(retained, 1).expect("in range"),
        make_small(2)
    );
}

/// **The escaping-arm guard is independent of the mark: a DIRTY pool is not rewound either.**
///
/// The seven-arm `safe_to_rewind` gate exists because six of the `DispatchHooks` arms let a term
/// the BIF built on `ctx.heap()` escape into state that outlives the call, which a
/// `reset_to(mark)` would recycle just as a `reset()` would. The case drives the `dict` arm
/// (`erlang:get_keys/1` builds its key list on the pool) over a pool that is **partially used**
/// — a retained `{1,2}` below the mark — and asserts the offset stays **above** the mark.
#[test]
fn an_escaping_arm_does_not_rewind_a_partially_used_pool() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let retained = rt
        .bif_heaps
        .first_mut()
        .expect("the runtime always owns its pooled BIF heap")
        .tuple(&[make_small(1), make_small(2)])
        .expect("the pooled BIF heap builds the retained tuple");
    let mark = rt.bif_heaps.first().expect("the pool owns a heap").used();
    assert_eq!(mark, 3, "a `{{1,2}}` tuple is a header word plus two element words");

    // The process dictionary holds one pair, so `get_keys/1` really builds a one-element list.
    table
        .get_mut(pid)
        .expect("the process")
        .dict_mut()
        .put(make_small(7), make_small(11));

    let keys = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.call_bif("erlang", "get_keys", 1, &[make_small(11)])
            .expect("get_keys(11)")
    };

    // The result is the key list `[7]` — a cons cell built on the pooled BIF heap.
    assert!(
        beam_term::list::is_list(keys),
        "get_keys/1 builds a list on `ctx.heap()`"
    );
    let process = table.get(pid).expect("the process");
    // As above: the region is the witness, not the valve's verdict — `[7]` forwards no scanned
    // word, so an inert foreign cons can never decline a collection on its own.
    assert!(
        !process
            .collector()
            .young_contains(beam_term::list::ptr_val(keys) as *const Eterm),
        "the `dict` arm is an escaping seam, so the result must NOT have been moved onto the \
         process's scanned heap"
    );
    assert!(
        rt.bif_heaps.first().expect("the pool owns a heap").used() > mark,
        "the escaping arm must NOT rewind — not even to the caller's mark"
    );
    // Both the escaped term and the retained words are still valid.
    assert_eq!(terms::cons_head(keys).expect("the cons cell"), make_small(7));
    assert_eq!(
        terms::cons_tail(keys).expect("a cons cell"),
        NIL,
        "the key list is exactly `[7]`"
    );
    assert_eq!(
        terms::tuple_arity(retained).expect("the retained tuple is still readable"),
        2
    );
    assert_eq!(
        terms::tuple_element(retained, 0).expect("in range"),
        make_small(1)
    );
}

/// **Item #10b — the collection the valve used to decline now RUNS.**
///
/// The same rig as the hole-2 guard
/// ([`a_collection_is_declined_when_a_live_root_lives_in_an_unscanned_region`]), with the
/// hand-built pooled-heap root replaced by a **real registered-BIF result**: `list_to_tuple/1`'s
/// tuple now lands on the process's own young heap, so a forced admission-point collection over
/// it must run (`gc_stats().collections` advances) and relocate it. With the result left on the
/// pool the valve would have declined and the counter would not move.
///
/// Repeated over several rounds on purpose: that is the shape the peak-RSS problem had, a churn
/// loop whose every result used to be an immutable foreign root and so disabled collection for
/// the rest of the process's life. Every round must collect, and every round must relocate the
/// result intact.
#[test]
fn a_bif_result_on_the_process_heap_lets_the_churn_loop_be_collected() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    const ROUNDS: usize = 8;
    for round in 0..ROUNDS {
        let list = young_int_list(&mut table, pid, &[1, 2, 3]);
        let tuple = {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            ctx.call_bif("erlang", "list_to_tuple", 1, &[list])
                .expect("list_to_tuple/1")
        };

        let mut x = [tuple, NIL, NIL, NIL];
        let mut stack = [NIL];
        let mut fr = [NIL];
        let before = rt.gc_stats();
        rt.request_gc(pid);
        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            ctx.current_module = Some(GC_CALL_SITE.to_string());
            ctx.arm_gc();
            ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
                .expect("the heap-admission point never fails the program");
        }
        let after = rt.gc_stats();
        assert!(
            after.collections > before.collections,
            "round {round}: the forced collection must RUN — the BIF result is on the scanned \
             heap, so the unscanned-root valve has nothing to decline; {after:?}"
        );
        assert!(
            after.reclaimed > before.reclaimed,
            "round {round}: the collection really reclaimed the dead list words, counted from the \
             collection's own outcome at the door site; {after:?}"
        );
        assert_eq!(
            after.halted, before.halted,
            "round {round}: nothing failed; {after:?}"
        );

        // The relocated result is still the tuple, at its (new) address.
        assert_eq!(
            terms::tuple_arity(x[0]).expect("the relocated word is a tuple"),
            3,
            "round {round}: the relocated result"
        );
        for (index, value) in [1i64, 2, 3].iter().enumerate() {
            assert_eq!(
                terms::tuple_element(x[0], index).expect("in range"),
                make_small(*value),
                "round {round}: element {index} of the relocated result"
            );
        }
    }
}

/// **The full door collects over a float-using call site.** The full door publishes the FR file
/// up to its pointer-bearing prefix ([`super::gc::publish_roots_full`]), so a live float register
/// is relocated like any x register or stack word and the caller-module float guard would only be
/// a redundant conservative extra: the door applies **no** float guard at all. A float-using
/// call site therefore collects exactly like a float-free one, and the FR-only root comes back
/// relocated.
#[test]
fn the_full_door_collects_over_a_float_using_call_site() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    rt.load_module(GC_FLOATS_BEAM)
        .expect("the float fixture loads into the code server");
    assert!(
        !rt.module_is_float_free(
            "gc_floats",
            &rt.code_server()
                .module_handle("gc_floats")
                .expect("cached under its own name")
        ),
        "the fixture really is float-using, so the test would be vacuous otherwise"
    );
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some("gc_floats".to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "a float-using call site is collected over at the full door; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "the scatter completed; {after:?}");
    assert_relocated_tuple(x[1], beyond, [7, 8], "x[1] (float-using call site)");
    assert_relocated_tuple(
        fr[0],
        fr_only,
        [9, 10],
        "fr[0] — the FR word the full door publishes",
    );
}

/// **A run with no caller module is still refused at the full door.** A fun entry and the
/// `-eval` evaluator have no `current_module`; the door has no machine to name, so it leaves
/// the heap alone rather than guessing. Nothing about the float guard is involved: this refusal
/// survives the guard's removal.
#[test]
fn the_full_door_still_refuses_a_run_with_no_caller_module() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let mut x = [NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        assert!(ctx.current_module.is_none(), "no caller module is set");
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    assert_eq!(
        rt.gc_stats().collections,
        before.collections,
        "an unnamed call site is never collected over"
    );
}

/// **The narrow door still refuses a float-using call site — and collects over a float-free
/// one.**
///
/// The narrow [`ExecutionContext::ensure_heap`] door receives only an x prefix and the stack,
/// so it can never see the machine's FR file: the caller-module float guard is load-bearing
/// there and must survive the full doors' removal of it. The second half drives the **same**
/// door with the same (still-armed) forced request and a float-free call site, so the refusal
/// above is about the float guard and not about the door being inert.
#[test]
fn the_narrow_door_still_refuses_a_float_using_call_site() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    rt.load_module(GC_FLOATS_BEAM)
        .expect("the float fixture loads into the code server");
    let mut x = [NIL, NIL];
    let mut stack = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some("gc_floats".to_string());
        ctx.arm_gc();
        ctx.ensure_heap(0, 1, &mut x, &mut stack)
            .expect("the heap-admission point never fails the program");
    }
    assert_eq!(
        rt.gc_stats().collections,
        before.collections,
        "the narrow door must not collect over a float-using call site"
    );
    assert!(
        rt.gc_request_pending(pid),
        "the refused forced request stays armed for the runner's park arm"
    );

    // The same door, the same armed request, a float-free call site: it collects.
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap(0, 1, &mut x, &mut stack)
            .expect("the heap-admission point never fails the program");
    }
    assert_eq!(
        rt.gc_stats().collections,
        before.collections + 1,
        "the narrow door does collect over a float-free call site"
    );
}

/// **The collection debt forces the collection the occupancy test would not.**
///
/// [`Process::grow_nursery`] is the no-relocation growth seam: it retires the young space it
/// replaces instead of freeing it, so the nursery becomes a chain only a collection can
/// evacuate. With `heap_need = 0` the occupancy test can never fire on a fresh nursery
/// (`available_words() < 0` is false), so without the debt term the door would return and the
/// chain would grow for as long as the load keeps enlarging it. The case drives the full door
/// with `heap_need = 0` on a process whose nursery really carries a retired space.
#[test]
fn the_collection_debt_forces_a_collection_the_occupancy_test_would_not() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let mut x = [NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    // The door with no debt: an occupancy test of zero words is never a trigger, and no
    // request is pending, so this is a no-op.
    let before = rt.gc_stats();
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    assert_eq!(
        rt.gc_stats().collections,
        before.collections,
        "an empty nursery with no debt is not collected over"
    );
    assert!(
        !table.get(pid).expect("the process").should_collect(0),
        "the occupancy test alone says there is room, so the debt term is what decides"
    );

    // Create the debt through the real growth seam, then re-drive the same door with the same
    // zero-word request. The request must exceed the current space, or `grow_nursery` is a
    // no-op by construction.
    let over = table
        .get(pid)
        .expect("the process")
        .collector()
        .young()
        .capacity_words()
        + 64;
    table
        .get_mut(pid)
        .expect("the process")
        .grow_nursery(over)
        .expect("the nursery grows");
    assert!(
        table.get(pid).expect("the process").collection_debt(),
        "the growth retired a space, which is the debt"
    );
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "the retired-space chain forces the collection; {after:?}"
    );
    assert!(
        !table.get(pid).expect("the process").collection_debt(),
        "the collection collapsed the chain and settled the debt"
    );
}

/// The Erlang source of the `ets_copy` **allocate-and-abandon** loop the A/B/C fixtures share,
/// mirroring `tools/bench/src/workloads/copy_bound.rs`'s `ets_copy` case (`payload/0` +
/// `ets:insert/2` + `ets:lookup/2`, one table and one key per process so the table never
/// grows).
///
/// The bound-variable pattern `[{k, T}]` is a real equality test — a corrupted copy raises —
/// and each validated round-trip adds 1, so `run(N)` returns exactly `N` on both runtimes.
/// `value_expr` is the payload's `b` value and `extra_key` an optional additional map entry:
/// they are the **only** differences between the three fixtures, so a divergence in GC
/// behaviour can only come from the term the payload holds.
fn gc_ab_module(module: &str, value_expr: &str, extra_key: &str) -> String {
    format!(
        r#"-module({module}).
-export([run/1]).

payload(X) ->
    #{{a => [1,2,3,4,5,6,7,8,9,10],
      b => {{ok, <<"hello world">>, {value_expr}}},
      c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
      d => "a char list",
      e => -42{extra_key}}}.

run(N) ->
    Tab = ets:new({module}, [set, public]),
    R = run(N, Tab, 0),
    ets:delete(Tab),
    R.

run(0, _Tab, Acc) -> Acc;
run(N, Tab, Acc) ->
    T = payload(N),
    true = ets:insert(Tab, {{k, T}}),
    case ets:lookup(Tab, k) of
        [{{k, T}}] -> run(N - 1, Tab, Acc + 1)
    end.
"#
    )
}

/// One fixture run at one loop count: the loop's value, the runtime's GC counters afterwards and
/// the unscanned-root valve's tallies over the run.
struct GcAbRun {
    /// The value the loop returned.
    value: Eterm,
    /// The runtime's GC counters at the end of the run.
    stats: GcStatsSnapshot,
    /// The valve's per-door tallies over the run — the evidence that whatever the collection
    /// *cadence* is, the valve did not withhold a collection.
    counters: gc::valve_counters::ValveCounters,
}

/// Runs `<path>:<function>/1` at `n` on a **fresh** runtime through the scheduled route
/// ([`Runtime::with_defaults`] + [`Runtime::run_scheduled`], the same shape as
/// `tests/rss_bound.rs::the_churn_loop_really_collects`) and returns the value plus the GC
/// counters and the valve's tallies, so a run's `collections`/`pinned` can be compared across
/// loop counts.
fn gc_ab_run(path: &std::path::Path, function: &str, n: i64) -> GcAbRun {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    gc::valve_counters::reset();
    let value = rt
        .run_scheduled(path, function, 1, &[make_small(n)])
        .expect("the loop runs to completion on the scheduled path");
    GcAbRun {
        value,
        stats: rt.gc_stats(),
        counters: gc::valve_counters::snapshot(),
    }
}

/// **A float-using long loop collects like a float-free one: the post-slice float-register
/// reaction is scoped to the inter-slice window, never a permanent pin** (`src/runner/run.rs`).
///
/// **Regression pin for the removed `post_slice_float_pin` plateau** — the name describes the
/// defect this case rules out, and the assertions below now pin the fixed behaviour.
///
/// The reaction used to be a **permanent** runtime-level process pin, applied through a
/// `Runtime::pin_process/1` entry this crate no longer has (the superseded reaction this case
/// rules out) — so the first slice that ended with a live FR slot made
/// `Runtime::gc_process_enabled` answer `false` for the rest of the process's life: a float-using
/// loop collected only inside its first slice, its nursery kept every space it ever allocated, and
/// its `collections` **plateaued** however large the loop got. The reaction is now the **per-slice
/// relocation exemption** the runner applies (`Process::set_relocation_exempt`, opened at the
/// slice's end when the FR prefix is live and cleared by the next slice entry) — so the process
/// collects again at its own heap-admission points and `Runtime::gc_stats().pinned` stays **0**.
///
/// This case is the **deterministic** half of the A/B experiment (no RSS involved): the same
/// three `ets_copy` allocate-and-abandon loops, differing only in the payload's `b` value, are
/// driven at two loop counts ~8x apart on the scheduled route and their
/// [`Runtime::gc_stats`] counters compared.
///
/// * `gcab_free` — float-free (`X`).
/// * `gcab_lit`  — `gcab_free` verbatim **plus a single `3.5` float literal** in the payload. It is
///   **not** float-using in the IR: the compiler folds a data-structure float literal into the
///   **literal area** (`{move,{literal,#{...3.5...}},{x,0}}`), so the module contains no float-family
///   instruction, never writes the FR file, and never opens the exemption. Its row is the empirical
///   refutation of "any module with a float literal writes FR slot 0".
/// * `gcab_flt`  — genuinely float-using: the payload computes `math:sqrt(float(X))`, so the module
///   contains `fconv`/`fmul` instructions and its slices end with `fr_mark() > 0`. The float
///   expression is deliberately **literal-free** (`math:sqrt(float(X))`, *not* `float(X) * 3.5`):
///   a float **operation** writes the FR file, whereas a float **literal** is folded into the
///   literal area and never writes FR. That literal-area shape is no longer a collection confound
///   in its own right — `beam_loader`'s literal-area decode now tags the area at load time
///   (`beam_term::literal::tag_literal_area`), so a live literal root is a scanned literal rather
///   than an untagged container the unscanned-root valve would decline — but keeping the row
///   literal-free anyway leaves the experiment's variable exactly one: the FR reaction.
///
/// The case asserts that all three rows **scale with `N` and pin nothing** (`pinned` 0
/// everywhere, each larger run collecting strictly more than its smaller twin — the float-using
/// row included, the property the old permanent pin destroyed).
///
/// **What the counts are, and what a failure here points at.** The pin's own signature is an
/// **exactly flat** pair — the same measurement twice — and `pinned == 0` plus a strictly larger
/// count at the larger `N` rules it out on every row. The *magnitude* of the counts is a
/// different question, and today only two rows carry the oracle's own cadence: on the system ERTS
/// (OTP 29.1.1) the same three sources report `minor_gcs` **1250 / 10 000** (`gcab_free`),
/// **381 / 3 043** (`gcab_lit`) and **406 / 3 244** (`gcab_flt`) at N = 5 000 / 40 000 — linear in
/// `N` in every row (ratio ≈ 8 over the 8x step), read through
/// `process_info(P, garbage_collection)`. The float-using row instead reads **9 → 11**, so this
/// case does not assert the oracle's magnitude there: the cause is the collector's
/// **post-collection nursery sizing**, which is `beam_gc`'s (`Nursery::collapse` ←
/// `HeapPolicy::young_after_collection`) and not this valve's or the runner's — the counters below
/// prove the valve declined nothing and `pinned` is 0 on both runs. The C applies two rules to
/// one heap, `major_collection`'s pre-copy young-usage sizing (`erts/emulator/beam/erl_gc.c:1866`)
/// **and** `adjust_after_fullsweep`'s shrink to `2 * need_after` off the post-copy live set once
/// `4 * need_after < HEAP_SIZE` (`erl_gc.c:2062-2098`); a collapse fed only the pre-copy occupancy
/// leaves a fullsweep-installed nursery sticky at whatever the growth seam last stepped it to.
/// See `IMPLEMENTATION.md`'s *What the doors do NOT decide*.
///
/// A missing `erlc` is a **loud skip**, never a silent pass; the fixtures are compiled from
/// these in-file sources into `$TMPDIR` (nothing is committed under `tests/fixtures/`).
#[test]
fn the_post_slice_float_pin_plateaus_a_float_using_loops_collections() {
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("gc-ab", "erlc was not found beside $ERTS_ORACLE or on $PATH");
        return;
    };
    let src_a = gc_ab_module("gcab_free", "X", "");
    let src_b = gc_ab_module("gcab_lit", "X", ",\n      f => 3.5");
    let src_c = gc_ab_module("gcab_flt", "math:sqrt(float(X))", "");
    let dir = compile_fixtures(
        &erlc,
        "gc_ab",
        &[
            ("gcab_free", src_a.as_str()),
            ("gcab_lit", src_b.as_str()),
            ("gcab_flt", src_c.as_str()),
        ],
    );

    // ~8x apart: every row's collection count must scale over the step. A permanently pinned
    // float-using loop's could not move at all — the two counts were the same measurement twice.
    const SMALL: i64 = 5_000;
    const LARGE: i64 = 40_000;

    let mut rows: Vec<(&str, GcAbRun, GcAbRun)> = Vec::new();
    for module in ["gcab_free", "gcab_lit", "gcab_flt"] {
        let path = dir.join(&format!("{module}.beam"));
        let small = gc_ab_run(&path, "run", SMALL);
        let large = gc_ab_run(&path, "run", LARGE);
        assert!(
            is_small(small.value) && is_small(large.value),
            "{module}: run/1 returns a small integer"
        );
        eprintln!(
            "gc-ab {module}: N={SMALL} -> value {} collections {} major {} pinned {} \
             fallbacks {} | N={LARGE} -> value {} collections {} major {} pinned {} fallbacks {}",
            signed_value(small.value),
            small.stats.collections,
            small.stats.major,
            small.stats.pinned,
            small.stats.fallbacks,
            signed_value(large.value),
            large.stats.collections,
            large.stats.major,
            large.stats.pinned,
            large.stats.fallbacks,
        );
        rows.push((module, small, large));
    }
    let row = |name: &str| -> &(&str, GcAbRun, GcAbRun) {
        rows.iter()
            .find(|(module, _, _)| *module == name)
            .unwrap_or_else(|| panic!("the {name} row was measured"))
    };
    let (_, a_small, a_large) = row("gcab_free");
    let (_, b_small, b_large) = row("gcab_lit");
    let (_, c_small, c_large) = row("gcab_flt");

    // Determinism first: a run cannot have silently done less work (which would make every
    // comparison below meaningless).
    for (module, small, large) in &rows {
        assert_eq!(
            signed_value(small.value),
            SMALL,
            "{module}: run({SMALL}) == {SMALL}"
        );
        assert_eq!(
            signed_value(large.value),
            LARGE,
            "{module}: run({LARGE}) == {LARGE}"
        );
    }

    // A — float-free: never pinned, and its collections SCALE with the loop count.
    assert_eq!(
        a_small.stats.pinned, 0,
        "a float-free module's slice never ends with a live FR slot; {:?}",
        a_small.stats
    );
    assert!(
        a_large.stats.collections > a_small.stats.collections * 4,
        "the float-free loop's collections must scale with N ({SMALL} -> {}, {LARGE} -> {})",
        a_small.stats.collections,
        a_large.stats.collections
    );

    // B — the literal-only payload: float-FREE in the IR, so it behaves exactly like A. This
    // is the empirical refutation of the "a float literal writes FR slot 0" premise.
    assert_eq!(
        b_small.stats.pinned, 0,
        "a payload float LITERAL folds into the literal area — the module is float-free, so \
         it is never pinned; {:?}",
        b_small.stats
    );
    assert!(
        b_large.stats.collections > b_small.stats.collections * 4,
        "the literal-float loop is float-free and collects across its whole run \
         ({SMALL} -> {}, {LARGE} -> {})",
        b_small.stats.collections,
        b_large.stats.collections
    );

    // C — genuinely float-using: the module contains float-family instructions (`fconv`/`fmul`),
    // and the post-slice reaction is a **per-slice** window exemption (never a permanent pin), so
    // the row is never pinned and its collections SCALE with N exactly like the float-free rows.
    // The row that discriminates the *pin* itself is the committed `gc_floats` control below: this
    // compiled row's float arithmetic completes inside its slices, so it was never pinned even
    // under the old reaction (`pinned` reads 0 there too).
    assert_eq!(
        c_small.stats.pinned, 0,
        "the float-using loop's post-slice reaction is a per-slice window exemption, never a \
         permanent pin; {:?}",
        c_small.stats
    );
    assert!(
        c_large.stats.collections > c_small.stats.collections,
        "the float-using loop must collect MORE as N grows — a permanent pin would plateau it at \
         one identical count ({SMALL} -> {}, {LARGE} -> {})",
        c_small.stats.collections,
        c_large.stats.collections
    );
    // ...and the reaction is never what withholds a collection: on both runs the valve declined
    // nothing and nothing fell back, so the row's cadence is the collector's own.
    for (label, run) in [("gcab_flt small", c_small), ("gcab_flt large", c_large)] {
        assert_eq!(
            run.counters.fallbacks_at(gc::valve_counters::FallbackSite::ValveDeclined),
            0,
            "{label}: no allocation fell back because the valve declined"
        );
        assert_eq!(
            run.counters.ensure_heap_full.hits, 0,
            "{label}: the full admission door declined no collection"
        );
    }

    // The committed float-arithmetic fixture (`tests/fixtures/gc_floats.beam`, `l1/1`
    // accumulates `math:sqrt(float(N))` in one double) is the control for the committed image —
    // and the row that actually exercises the post-slice FR reaction: its slices end with a live
    // FR prefix, so under the old permanent pin it was pinned on **both** runs with *identical*
    // collections at the ~8x larger N — the plateau this case is named after, and the state this
    // test fails in should the permanent pin ever return. Scoped to the inter-slice window, it is
    // never pinned and its collections grow instead.
    let f_small = gc_ab_run(std::path::Path::new(GC_FLOATS_BEAM), "l1", SMALL);
    let f_large = gc_ab_run(std::path::Path::new(GC_FLOATS_BEAM), "l1", LARGE);
    eprintln!(
        "gc-ab gc_floats:l1/1: N={SMALL} -> collections {} pinned {} | N={LARGE} -> \
         collections {} pinned {}",
        f_small.stats.collections,
        f_small.stats.pinned,
        f_large.stats.collections,
        f_large.stats.pinned
    );
    assert_eq!(
        f_small.stats.pinned, 0,
        "a float-using pid is never pinned — the inter-slice window is the whole reaction"
    );
    assert_eq!(f_large.stats.pinned, 0, "and the ~8x larger run pins nothing either");
    assert!(
        f_large.stats.collections > f_small.stats.collections,
        "a float-using loop's collections must grow with N, not plateau on a pin \
         ({SMALL} -> {}, {LARGE} -> {})",
        f_small.stats.collections,
        f_large.stats.collections
    );
}

/// **A live float term in the FR file is a SCANNED root, so this node's unscanned-root valve
/// does not decline it.**
///
/// The engine materialises an FR slot's float through
/// [`beam_engine::terms::make_float`]/`make_float_rooted`, which allocates on the **executing
/// process's own young heap** (this engine's FR model holds *terms*, not raw doubles), so the
/// word the full door publishes from `fr[0]` is classified as a scanned root by
/// [`root_is_unscanned`](super::gc::root_is_unscanned). The valve is therefore **not** what
/// would stop a float-using loop from collecting — a live FR is an ordinary scanned root at the
/// full door's FR-safe root set. This case proves the valve half directly, so the valve and the
/// runner's per-slice float-register reaction cannot be confused.
#[test]
fn a_live_float_term_in_fr_is_a_scanned_root() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    rt.load_module(GC_FLOATS_BEAM)
        .expect("the float fixture loads into the code server");
    assert!(
        !rt.module_is_float_free(
            "gc_floats",
            &rt.code_server()
                .module_handle("gc_floats")
                .expect("cached under its own name")
        ),
        "the fixture really is float-using, so the case would be vacuous otherwise"
    );

    // Build the float on the process's OWN young heap, exactly as the engine's FR writers do.
    let float = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        beam_engine::terms::make_float(&mut ctx, 3.5).expect("the float term is built")
    };
    assert_eq!(
        beam_engine::terms::float_value(float).expect("a boxed float term"),
        3.5
    );
    assert!(
        !super::gc::root_is_unscanned(table.get(pid).expect("the process"), float),
        "an FR float lives on the process's own heap, so it is a scanned root — never a \
         foreign container the valve would decline"
    );

    // And the full door really collects with it published from `fr[0]`.
    let mut x = [make_small(0), NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [float];
    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some("gc_floats".to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "a float-using call site with a live FR float is collected over at the full door; \
         {after:?}"
    );
    assert_ne!(fr[0], float, "the collection relocated the FR float");
    assert_eq!(
        beam_engine::terms::float_value(fr[0]).expect("still a boxed float term"),
        3.5,
        "the relocated FR float is intact"
    );
}

/// The **constant-payload** twin of [`gc_ab_module`]: the `copy_bound/ets_copy` shape exactly
/// as `tools/bench/src/workloads/copy_bound.rs` writes it — `payload/0` takes **no** argument, so
/// the compiler folds it into the module's **literal area** and the loop's `test_heap` call site
/// finds that literal live in `x(0)`.
///
/// `gc_ab_module`'s `payload(N)/1` cannot fold (the payload holds the loop counter), so the two
/// fixtures are the discriminating pair: **the only difference between them is whether the term
/// the loop leaves live in a register is a literal-area pointer or a young-heap one**.
fn gc_ab_const_module(module: &str) -> String {
    format!(
        r#"-module({module}).
-export([run/1]).
payload() ->
    #{{a => [1,2,3,4,5,6,7,8,9,10],
      b => {{ok, <<"hello world">>, 3.5}},
      c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
      d => "a char list",
      e => -42}}.
run(N) ->
    Tab = ets:new({module}, [set, public]),
    R = run(N, Tab, 0),
    ets:delete(Tab),
    R.
run(0, _Tab, Acc) -> Acc;
run(N, Tab, Acc) ->
    T = payload(),
    true = ets:insert(Tab, {{k, T}}),
    case ets:lookup(Tab, k) of
        [{{k, T}}] -> run(N - 1, Tab, Acc + 1)
    end.
"#
    )
}
/// **A tagged literal-area root passes the valve: the constant-payload loop collects and its
/// `collections` scale with `N`.**
///
/// The loader's `LiteralArea::decode` literal-tags the decoded arena with
/// [`beam_term::literal::tag_literal_area`] — the safe form of the C load-time step
/// `erts_set_literal_tag` (`erts/emulator/beam/erl_term.c:36`) — so every boxed/list word a
/// module's `LitT` chunk lowers to, and the literal roots the engine is handed, carry the
/// literal bit **set**.
///
/// [`super::gc::root_is_unscanned`] classifies a term by the C's own `erts_is_literal` rule (a
/// bare bit test), so a **tagged** literal-area pointer in a root is recognized as a literal —
/// an immutable, never-moved region the collector does not scan — rather than as a container in
/// a region the collector does not scan, and the valve passes. The loop's `test_heap`/
/// `test_heap_full` sites therefore collect on schedule.
///
/// `gclit`'s `payload/0` takes no argument, so the compiler folds it into the module's literal
/// area and the loop leaves a literal-area pointer live in `x(0)`; its `collections` are
/// non-zero and scale with `N`. The heap-built twin ([`gc_ab_module`]'s `payload(N)/1`) cannot
/// fold — the payload holds the loop counter — so it builds on the young heap; this test is the
/// discriminating pair, **the two fixtures differing in exactly one way: whether the term the
/// loop leaves live in a register is a literal-area pointer or a young-heap one**. Now that the
/// literal is tagged, the two are indistinguishable in collection count, which is the point.
#[test]
fn a_tagged_literal_root_is_scanned_and_the_loop_collects() {
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("gc-literal-root", "erlc was not found beside $ERTS_ORACLE or on $PATH");
        return;
    };
    let dir = compile_fixtures(
        &erlc,
        "gc_literal_root",
        &[
            ("gclit", &gc_ab_const_module("gclit")),
            ("gcab_free", &gc_ab_module("gcab_free", "X", "")),
        ],
    );
    // The discriminating pair, at a small and a large loop count.
    let lit_small = gc_ab_run(&dir.join("gclit.beam"), "run", 1_000);
    let lit_large = gc_ab_run(&dir.join("gclit.beam"), "run", 40_000);
    let heap_small = gc_ab_run(&dir.join("gcab_free.beam"), "run", 1_000);
    let heap_large = gc_ab_run(&dir.join("gcab_free.beam"), "run", 40_000);
    // Both loops compute the same answer; neither is corrupted — a literal root only affects
    // *retention*, never the result.
    assert_eq!(signed_value(lit_small.value), 1_000);
    assert_eq!(signed_value(lit_large.value), 40_000);
    assert_eq!(signed_value(heap_large.value), 40_000);
    // The control: a heap-built payload leaves no literal root, so the debt term fires and the
    // collections really scale.
    assert_eq!(heap_small.stats.pinned, 0);
    assert!(
        heap_large.stats.collections > heap_small.stats.collections * 4,
        "the heap-payload twin must collect: small={} large={}",
        heap_small.stats.collections,
        heap_large.stats.collections
    );
    // Not the post-slice float pin: the constant-payload loop is float-free, so it is never
    // pinned and never falls back — `collections` alone is the discriminator.
    assert_eq!(
        lit_large.stats.pinned, 0,
        "not the post-slice float pin: the constant-payload loop is float-free",
    );
    assert_eq!(lit_large.stats.fallbacks, 0);
    // The pin: a tagged literal-area root is recognized as a literal, so the valve passes and
    // the loop's heap-admission points really collect. (Before the loader tagged the arena the
    // same loop's live literal root read as an unscanned container and every collection was
    // declined, leaving `collections == 0`.)
    assert!(
        lit_large.stats.collections > 0,
        "a tagged literal root must collect (a literal is a never-moved region the collector \
         does not scan, so the valve passes): lit_large={} lit_small={}",
        lit_large.stats.collections,
        lit_small.stats.collections,
    );
    // And the literal-rooted loop keeps collecting as `N` grows: strictly more at the ~40x larger
    // count, with the valve declining nothing on either run (the per-run tallies prove the loop's
    // cadence is **not** valve-bound — the counts themselves are the collector's, see the doc's
    // *post-collection nursery sizing* note).
    assert!(
        lit_large.stats.collections > lit_small.stats.collections,
        "the literal-rooted loop must not plateau (a declined collection would leave it flat): \
         lit_small={} lit_large={} (heap_small={} heap_large={})",
        lit_small.stats.collections,
        lit_large.stats.collections,
        heap_small.stats.collections,
        heap_large.stats.collections,
    );
    for (label, run) in [("lit_small", &lit_small), ("lit_large", &lit_large)] {
        assert_eq!(
            run.counters.fallbacks_at(gc::valve_counters::FallbackSite::ValveDeclined),
            0,
            "{label}: no allocation fell back because the valve declined"
        );
        assert_eq!(
            run.counters.ensure_heap_full.hits, 0,
            "{label}: the full admission door declined no collection, so the loop's count is the \
             collector's sizing and never this valve's"
        );
    }
}

/// The **copy-bound two-process ping-pong** (`srprof`): a worker echoes the payload back and the
/// driver compares it, `N` times.
///
/// It is the workload whose profile the valve's per-word phase dominates, and it differs from the
/// `ets_copy` fixtures above in exactly the way that matters: every iteration **sends** the
/// payload to the worker and **receives** it back, so the delivery path (not just a local heap
/// build) is in the loop. Its result is the iteration count.
const SRPROF_SOURCE: &str = r#"-module(srprof).
-export([plain/1]).
payload() ->
    #{a => [1,2,3,4,5,6,7,8,9,10],
      b => {ok, <<"hello world">>, 3.5},
      c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
      d => "a char list",
      e => -42}.
plain(N) ->
    W = spawn(fun() -> worker() end),
    R = plain(N, W, 0),
    W ! stop, R.
plain(0, _W, Acc) -> Acc;
plain(N, W, Acc) ->
    T = payload(),
    W ! {self(), T},
    receive
        {W, T2} -> case T2 of T -> plain(N - 1, W, Acc + 1) end
    after 30000 -> exit(message_timeout)
    end.
worker() ->
    receive
        {From, T} -> From ! {self(), T}, worker();
        stop -> ok
    end.
"#;

/// One ping-pong measurement: the result, the runtime's own GC counters and the valve's tallies.
struct PingPongRun {
    /// The iteration count the workload returned.
    value: i64,
    /// The runtime's own collection counters at the end of the run.
    stats: GcStatsSnapshot,
    /// The unscanned-root valve's per-door tallies over the run.
    counters: gc::valve_counters::ValveCounters,
}

/// Runs `srprof:plain/1` at `n` on a **fresh** runtime through the scheduled route, with the
/// valve's test-only tallies zeroed immediately before the run.
fn ping_pong_run(path: &std::path::Path, n: i64) -> PingPongRun {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    gc::valve_counters::reset();
    let value = rt
        .run_scheduled(path, "plain", 1, &[make_small(n)])
        .expect("the ping-pong runs to completion on the scheduled path");
    PingPongRun {
        value: signed_value(value),
        stats: rt.gc_stats(),
        counters: gc::valve_counters::snapshot(),
    }
}

/// Renders one door's tallies for the test's diagnostic line.
fn door_line(door: gc::valve_counters::Door, counts: &gc::valve_counters::DoorCounts) -> String {
    format!(
        "{door:?}: reached={} occupancy={} debt={} forced={} latch_open={} hits={} \
         [machine_root={} process_root={} descent={}] descents={} collections={} machine_words={}",
        counts.reached,
        counts.occupancy,
        counts.debt,
        counts.forced,
        counts.latch_open,
        counts.hits,
        counts.machine_root_hits,
        counts.process_root_hits,
        counts.descent_hits,
        counts.descents,
        counts.collections,
        counts.machine_words,
    )
}

/// Renders the arena-fallback attribution for the test's diagnostic line.
fn fallback_line(counters: &gc::valve_counters::ValveCounters) -> String {
    use gc::valve_counters::FallbackSite;
    let sites: Vec<String> = FallbackSite::ALL
        .iter()
        .map(|site| format!("{site:?}={}", counters.fallbacks_at(*site)))
        .collect();
    format!("fallbacks={} [{}]", counters.fallbacks, sites.join(" "))
}

/// **The copy-bound ping-pong's per-iteration cost is the valve's per-word classification phase,
/// and the counters say why it runs at every heap-admission point: the driver's nursery is full
/// and never drains (no collection is ever taken at its door), so the occupancy short-circuit in
/// front of the valve never fires, and the valve then *declines* the collection at (essentially)
/// every reach — always on the same side, a **machine root word** that lies outside every region
/// the collector scans.**
///
/// The entry point of that self-sustaining loop is the **rooted allocation seam**
/// ([`gc::valve_counters::Door::AllocWordsRooted`]): the young heap's capacity refusal there used
/// to go straight to the valve and, on a decline, to the pointer-stable arena
/// ([`Runtime::alloc_words`](crate::runtime::Runtime)) — and the engine writes that arena block
/// into an x register, where it is permanent poison: the collector only relocates words that point
/// *into* the young/mature heap, so an arena pointer in a root slot is never relocated away and
/// every later admission point must decline. With the seam's no-relocation
/// **grow-and-retry** (`Process::grow_nursery`, the same one [`gc::alloc_words`] already used) the
/// refusal is met by a growth instead, the block stays on the process's own reclaimable heap, and
/// the circle never starts.
///
/// This is a **structural, deterministic** pin (never a timing): it drives the workload `perf`
/// profiles at a small and a large iteration count and reads [`gc::valve_counters`], whose tallies
/// are counts of calls and of root words — facts about the runtime, identical on an idle and on a
/// loaded host.
///
/// The measured chain, in the order the counters establish it:
///
/// 1. the engine's two full-root doors are the only ones reached: the `test_heap`/`allocate_heap`
///    admission point ([`gc::valve_counters::Door::EnsureHeapFull`]) and — only when the young-heap
///    bump cannot serve a term construction — the rooted allocation seam
///    ([`gc::valve_counters::Door::AllocWordsRooted`]). The narrow door and the runner's park arm
///    have no call site for this workload (all-zero tallies);
/// 2. the full-root door is reached when its occupancy test or the growth seam's **collection
///    debt** says so, and it **commits**: `collections` scales with `N` (≈ 1 per 20 iterations)
///    while the declines collapse to a small fraction of the reaches. The debt is non-zero because
///    a young-heap refusal is now met by a growth;
/// 3. the declines that remain are still attributed to a **machine root** word — the honest answer
///    for a term from a region the collector does not scan (a [`beam_message`] fragment, the other
///    unscanned region this route creates) — never to a process root or to the descent;
/// 4. the rooted seam is therefore **essentially never reached** (`reached` ≈ 0, against ≈ 1 per
///    iteration before the growth path existed): the capacity refusal it used to turn into a
///    decline is served by the growth, so no arena block ever enters a register;
/// 5. the arena fallbacks collapse with it — from ≈ 1 per iteration, all of them attributed to a
///    refused rooted collection ([`gc::valve_counters::FallbackSite::ValveDeclined`]), to
///    (essentially) zero and **flat** in `N`;
/// 6. each reach's per-word classification covers only the door's **published** region — each
///    register file's pointer-bearing prefix plus the whole Erlang stack
///    ([`gc::valve_counters::DoorCounts::published_words`]) — while
///    [`gc::valve_counters::DoorCounts::machine_words`] still records the **declared** figure, the
///    whole x-register file plus the whole FR file, ~2 Ki words per reach. What collapsed is the
///    number of reaches, so the workload's total classification work is down to a few hundred
///    words per **iteration**.
///
/// A missing `erlc` is a **loud skip**, never a silent pass.
#[test]
fn the_copy_bound_ping_pong_grows_the_nursery_instead_of_declining_the_valve() {
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("srprof-valve", "erlc was not found beside $ERTS_ORACLE or on $PATH");
        return;
    };
    let dir = compile_fixtures(&erlc, "srprof_valve", &[("srprof", SRPROF_SOURCE)]);
    let path = dir.join("srprof.beam");

    const SMALL: i64 = 200;
    const MID: i64 = 15_000;
    const LARGE: i64 = 30_000;
    let small = ping_pong_run(&path, SMALL);
    let mid = ping_pong_run(&path, MID);
    let large = ping_pong_run(&path, LARGE);

    // Determinism first: the workload really ran both counts, so every ratio below is a
    // statement about work done, not about a run that stopped early.
    assert_eq!(small.value, SMALL, "the small run returned its iteration count");
    assert_eq!(mid.value, MID, "the mid run returned its iteration count");
    assert_eq!(large.value, LARGE, "the large run returned its iteration count");

    for (label, n, run) in [("small", SMALL, &small), ("mid", MID, &mid), ("large", LARGE, &large)] {
        eprintln!(
            "srprof-valve {label} (N={n}): value={} collections={} major={} fallbacks={} \
             pinned={} halted={} {}",
            run.value,
            run.stats.collections,
            run.stats.major,
            run.stats.fallbacks,
            run.stats.pinned,
            run.stats.halted,
            fallback_line(&run.counters),
        );
        for (door, counts) in run.counters.doors() {
            eprintln!("srprof-valve {label}: {}", door_line(door, &counts));
        }
        match run.counters.first_decline {
            Some(first) => eprintln!(
                "srprof-valve {label}: first_decline door={:?} slice={:?} index={} live_len={}",
                first.door, first.slice, first.index, first.live_len,
            ),
            None => eprintln!("srprof-valve {label}: first_decline none"),
        }
    }

    let full = &large.counters.ensure_heap_full;
    let rooted = &large.counters.alloc_words_rooted;
    let reached = full.reached + rooted.reached;

    // (1) The two engine doors are the only ones reached, and the valve is reached **rarely**
    //     relative to the iteration count: the nursery really drains, so the occupancy short-circuit
    //     (or the growth seam's debt) is what lets the door through — never a permanently full heap.
    //     The bound is deliberately loose so the pin states the mechanism, not a fragile exact count.
    let other = large.counters.ensure_heap.reached + large.counters.serve_gc_request.reached;
    assert_eq!(other, 0, "the engine reaches only the two full-root doors ({other} others)");
    assert!(
        reached as i64 * 8 < LARGE,
        "the valve must be reached rarely — at most an eighth of the iterations: reached={reached} \
         for N={LARGE} (full={} rooted={})",
        full.reached,
        rooted.reached,
    );
    assert!(reached > 0, "the door must still be reached: the workload allocates");
    // (2) Every reach had the occupancy test pass or the growth seam's collection debt outstanding:
    //     nothing is reached "for free", and the debt is the evidence that a young-heap refusal is
    //     being met by a growth rather than by a decline.
    assert!(
        full.occupancy + full.debt >= full.reached,
        "every full-root reach had occupancy or debt: reached={} occupancy={} debt={}",
        full.reached,
        full.occupancy,
        full.debt,
    );
    assert!(full.occupancy > 0, "the occupancy test still lets the door through");
    // The growth seam's collection debt only exists when a reach actually fell through the valve
    // to a young-heap refusal that a growth met. With the valve's forwarding fact this workload
    // no longer declines at all (`hits == 0` below), so nothing is refused and no debt is left:
    // the circle cannot start, which is a stronger answer than breaking it. Pin the disjunction
    // rather than dropping the claim, so a regression that reintroduces declines must also
    // produce the debt again.
    if full.hits > 0 {
        assert!(
            full.debt > 0,
            "a decline that a no-collect growth met must leave collection debt at the full-root \
             door: debt={} hits={}",
            full.debt,
            full.hits,
        );
    } else {
        assert_eq!(
            full.debt, 0,
            "nothing was declined and so nothing needed a growth: debt={}",
            full.debt,
        );
    }
    assert_eq!(full.forced, 0, "the workload requests no collection");
    // (3) The door really **collects** — the circle is broken at its entry point: the collection is
    //     no longer declined at (almost) every reach, and what remains is a small fraction of the
    //     iterations. The declines that do remain are still machine roots (the honest answer for a
    //     term from a region the collector does not scan), never a process root or a descent.
    assert!(
        full.hits as i64 * 8 < LARGE,
        "the full-root door's declines must collapse to a small fraction of the iterations: \
         hits={} for N={LARGE} (was ≈1 per iteration)",
        full.hits,
    );
    assert!(
        full.collections as i64 * 40 > LARGE,
        "the full-root door must really collect, at a rate that scales with N: collections={} for \
         N={LARGE}",
        full.collections,
    );
    assert!(
        full.descents > 0,
        "a committed collection runs the descent once the latch is open: descents={}",
        full.descents,
    );
    assert_eq!(
        full.machine_root_hits, full.hits,
        "every remaining full-root decline is attributable to a machine root word"
    );
    assert_eq!(
        full.process_root_hits, 0,
        "no decline comes from a process root (the mailbox/dictionary/signal roots are clean)"
    );
    assert_eq!(
        full.descent_hits, 0,
        "no decline needed the O(live objects) descent: phase 1 finds the unscanned root itself"
    );
    // (4) The rooted allocation seam is (essentially) never reached at all: the capacity refusal it
    //     used to turn into a decline is met by the no-relocation growth, so the driver's payload
    //     build lands on the process's own reclaimable heap rather than on the pointer-stable
    //     arena — which is exactly what keeps an unscanned word out of the machine's registers.
    assert!(
        rooted.reached * 100 <= LARGE as usize,
        "the rooted seam must be reached at most ~1% of the iterations: reached={} for N={LARGE}",
        rooted.reached,
    );
    assert_eq!(
        rooted.descents, 0,
        "the rooted seam runs no descent: it is not the door the collections come from"
    );
    // The arena fallbacks are (essentially) gone: a refused rooted collection no longer sends its
    // allocation to the pointer-stable arena, so no arena block is ever written into a register and
    // the full-root door's roots stay scannable.
    use gc::valve_counters::FallbackSite;
    assert_eq!(
        large.counters.fallbacks,
        large.stats.fallbacks as usize,
        "the tally must agree with the runtime's own arena-fallback counter"
    );
    assert!(
        large.counters.fallbacks as i64 * 1000 < LARGE,
        "the arena fallbacks must collapse from ~1 per iteration to a vanishing fraction: {} for \
         N={LARGE}",
        large.counters.fallbacks,
    );
    assert!(
        large.counters.fallbacks_at(FallbackSite::ValveDeclined) as i64 * 1000 < LARGE,
        "no allocation should reach the arena through a refused rooted collection: {} of {} for \
         N={LARGE}",
        large.counters.fallbacks_at(FallbackSite::ValveDeclined),
        large.counters.fallbacks,
    );
    assert_eq!(
        large.counters.fallbacks_at(FallbackSite::PostCollection),
        0,
        "the rooted seam never reaches its post-collection bump: it is never reached at all"
    );
    // ...and they no longer grow with the iteration count: the flat, vanishing rate is the whole
    // point (the circled shape's fallbacks scaled with `N`).
    assert!(
        large.counters.fallbacks <= small.counters.fallbacks,
        "the arena fallbacks must not scale with the iteration count: small={} large={}",
        small.counters.fallbacks,
        large.counters.fallbacks,
    );
    // The collections the runtime runs are the driver's own now (its heap really drains), and they
    // stay a small fraction of the iterations rather than one per admission point.
    assert!(
        large.stats.collections > 0 && large.stats.collections * 16 < LARGE as u64,
        "collections must be real but rare next to the iterations: {} for N={LARGE}",
        large.stats.collections,
    );
    // (5) `machine_words` is the door's **declared** per-reach figure — the whole x file (1024)
    //     plus the whole FR file (1024) plus the Erlang stack — so it remains the right per-reach
    //     bound for the assertion below. The words the per-word phase actually classifies and the
    //     door hands over are the pointer-bearing prefix of each register file plus the whole
    //     stack (`published_words`). What collapsed is the number of reaches, and with it the
    //     workload's total classification work.
    const MIN_WORDS_PER_REACH: usize = 2_000;
    assert!(
        full.machine_words >= full.reached * MIN_WORDS_PER_REACH,
        "each reach must classify the whole x + FR file: full={}/{}",
        full.machine_words,
        full.reached,
    );
    let total_words = full.machine_words + rooted.machine_words;
    assert!(
        total_words * 100 < (LARGE as usize) * 40_000,
        "the per-word phase must classify far fewer words per iteration than a reach at every \
         admission point (~4 Ki): {total_words} for N={LARGE}"
    );
    // ...and it still scales with the iteration count, through the door's own reach count (a fixed
    // number of reaches per collection, and the collections are linear in `N`).
    let small_reached =
        small.counters.ensure_heap_full.reached + small.counters.alloc_words_rooted.reached;
    assert!(
        reached > small_reached * 10,
        "the valve's reach count must scale with the iteration count: small={small_reached} \
         large={reached}",
    );
    let mid_reached = mid.counters.ensure_heap_full.reached + mid.counters.alloc_words_rooted.reached;
    assert!(
        reached > mid_reached * 3 / 2 && reached < mid_reached * 3,
        "the valve's reach count must be linear in the iteration count: mid={mid_reached} \
         large={reached} for {MID} -> {LARGE}",
    );
    eprintln!(
        "srprof-valve totals: reached={reached} (~{:.3}/iteration) machine_words={total_words} \
         (~{:.0} words/iteration) fallbacks={}",
        reached as f64 / LARGE as f64,
        total_words as f64 / LARGE as f64,
        large.counters.fallbacks,
    );
}

