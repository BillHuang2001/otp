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

//! The `lists` **fun-argument guard** (`is_function(Fun, Arity)`) on the remaining
//! fun-taking BIFs, pinned against the OTP 29.1.1 / erts-17.1 oracle.
//!
//! Every fun-taking `lists.erl` function guards its clause head with
//! `is_function(Fun, Arity)`, so a **non-fun** or a fun of the **wrong arity** is the
//! bare `error:function_clause` — raised *before* the list argument is looked at, hence
//! for **every** list shape including `[]` (a wrong fun never gets to the fun call
//! layer). Without the guard the crate fabricated a success for the empty list —
//! `lists:all(5, [])` returned `true`, `lists:any(5, [])` returned `false`,
//! `lists:partition(5, [])` returned `{[],[]}`, `lists:takewhile(5, [])` returned `[]`,
//! `lists:sort(5, [])` returned `[]`, … — where the oracle raises.
//!
//! Two functions guard only **part** of their clause set, so the guard must *not* be
//! applied at entry (and the crate must not raise where the oracle does not):
//!
//! * `sort/2` guards only its `[]` and `[_]` clauses. Its `[X, Y | T]` clause is
//!   unguarded, so `lists:sort(5, [1,2])` reaches the **call layer** and is
//!   `error:{badfun,5}` — a different layer from the guard (the crate's stub
//!   `FunCaller` stands in for that layer here, so the observable is "the BIF does not
//!   reject").
//! * `zipwith/3` guards only its `([], [])` terminal clause:
//!   `lists:zipwith(5, [1], [2])` is `error:{badfun,5}`, while a length mismatch
//!   (`lists:zipwith(5, [], [1])`, `lists:zipwith(5, [1,2], [])`) fails the clause set
//!   itself and is `function_clause` whatever the fun is.
//!
//! The guard is a **clause head**, so it also precedes the `case`-based walks:
//! `lists:all(5, a)` is `function_clause`, *not* the `{case_clause, a}`
//! `lists:all(fun(_) -> true end, a)` raises. Both orderings are pinned below.
//!
//! # Oracle probe transcript (system `erl`, OTP 29.1.1 / erts-17.1)
//!
//! Each probe below is `try Expr of V -> {ok, V} catch C:R -> {C, R} end`, run through
//! `erl -noshell -eval '<Expr wrapped as a thunk>' -s init stop`:
//!
//! ```text
//! lists:all(5, [])                          => {error,function_clause}
//! lists:all(5, [1])                         => {error,function_clause}
//! lists:all(5, [1|2])                       => {error,function_clause}
//! lists:all(5, a)                           => {error,function_clause}
//! lists:all(fun(_) -> true end, a)          => {error,{case_clause,a}}
//! lists:all(fun(_, _) -> x end, a)          => {error,function_clause}
//! lists:all(fun(_) -> true end, [1|2])      => {error,function_clause}
//! lists:all(fun(_) -> false end, [1|2])     => {ok,false}
//! lists:all(fun(_) -> true end, [])         => {ok,true}
//! lists:any(5, [])                          => {error,function_clause}
//! lists:any(5, [1|2])                       => {error,function_clause}
//! lists:any(5, a)                           => {error,function_clause}
//! lists:any(fun(_) -> true end, [1|2])      => {ok,true}
//! lists:any(fun(_) -> false end, [1|2])     => {error,function_clause}
//! lists:any(fun(_) -> false end, [])        => {ok,false}
//! lists:flatmap(5, [])                      => {error,function_clause}
//! lists:flatmap(5, [1])                     => {error,function_clause}
//! lists:flatmap(fun(_) -> true end, [])     => {ok,[]}
//! lists:flatmap(fun(_) -> true end, a)      => {error,function_clause}
//! lists:takewhile(5, [])                    => {error,function_clause}
//! lists:takewhile(5, [1])                   => {error,function_clause}
//! lists:takewhile(fun(_) -> true end, [])   => {ok,[]}
//! lists:takewhile(fun(_) -> true end, a)    => {error,function_clause}
//! lists:dropwhile(5, [])                    => {error,function_clause}
//! lists:dropwhile(fun(_) -> true end, [])   => {ok,[]}
//! lists:dropwhile(fun(_) -> true end, a)    => {error,function_clause}
//! lists:partition(5, [])                    => {error,function_clause}
//! lists:partition(fun(_) -> true end, [])   => {ok,{[],[]}}
//! lists:partition(fun(_) -> true end, a)    => {error,function_clause}
//! lists:splitwith(5, [])                    => {error,function_clause}
//! lists:splitwith(fun(_) -> true end, [])   => {ok,{[],[]}}
//! lists:splitwith(fun(_) -> true end, a)    => {error,function_clause}
//! lists:mapfoldl(5, 0, [])                  => {error,function_clause}
//! lists:mapfoldl(5, 0, [1])                 => {error,function_clause}
//! lists:mapfoldl(5, 0, a)                   => {error,function_clause}
//! lists:mapfoldl(Fun2, 0, [])               => {ok,{[],0}}
//! lists:mapfoldr(5, 0, [])                  => {error,function_clause}
//! lists:mapfoldr(Fun2, 0, [])               => {ok,{[],0}}
//! lists:usort(5, [])                        => {error,function_clause}
//! lists:usort(5, [1])                       => {error,function_clause}
//! lists:usort(5, [1,2])                     => {error,function_clause}
//! lists:usort(fun(A, B) -> A =< B end, [])  => {ok,[]}
//! lists:sort(5, [])                         => {error,function_clause}
//! lists:sort(5, [1])                        => {error,function_clause}
//! lists:sort(5, a)                          => {error,function_clause}
//! lists:sort(5, [1,2])                      => {error,{badfun,5}}
//! lists:sort(5, [1,2,3])                    => {error,{badfun,5}}
//! lists:sort(fun(A, B) -> A =< B end, [])   => {ok,[]}
//! lists:zipwith(5, [], [])                  => {error,function_clause}
//! lists:zipwith(5, [], [1])                 => {error,function_clause}
//! lists:zipwith(5, [1,2], [])               => {error,function_clause}
//! lists:zipwith(5, a, a)                    => {error,function_clause}
//! lists:zipwith(5, [1], [2])                => {error,{badfun,5}}
//! lists:zipwith(fun(A, _B) -> A end, [], []) => {ok,[]}
//! ```
//!
//! (`Fun2` is `fun(X, A) -> {X, A} end`.) The same 54 probes are re-run against the
//! live oracle by [`the_guard_transcript_matches_the_live_oracle`], so the table above
//! cannot rot silently.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, FunCaller};
use beam_codec::term::{tuple_arity, tuple_element};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, make_atom, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// The atoms the reasons and the probes need. `case_clause` is not a predefined
/// emulator atom, so it is seeded here (the same thing the runtime's `AtomInterner`
/// would do).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "case_clause", "a", "f", "g"]);
    atoms
}

/// The atom `name` in a freshly seeded table *and* the term naming it. Identical seed
/// sequences produce identical atom values, which lets a test name an expected atom
/// without threading a live context out of its closure.
fn atom(name: &str) -> (AtomTable, Eterm) {
    let atoms = seeded_atoms();
    let value = make_atom(atoms.lookup(name).expect("seeded"));
    (atoms, value)
}

/// A `FunCaller` that never inspects the fun: every call answers the configured
/// *verdict* term. The functions under test read the verdict as a boolean (the
/// predicates, `sort/2`'s ordering fun) or hand it straight back (`zipwith/3`), so one
/// stub serves them all.
struct StubFuns {
    verdict: Eterm,
}

impl FunCaller for StubFuns {
    fn call_fun(&mut self, _fun: Eterm, _args: &[Eterm]) -> Result<Eterm> {
        Ok(self.verdict)
    }
}

/// Runs `f` over the default registry and a fresh context whose stub fun answers `true`
/// when `verdict` is set, else `false`.
fn run<R>(verdict: bool, f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let registry = default_registry();
    let atoms = seeded_atoms();
    // Leaked on purpose: the closure's results are `Eterm`s that point into this heap,
    // and the tests inspect them *after* `run` returns. Freeing it here would make
    // those reads a use-after-free.
    let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(65_536).expect("test heap")));
    let verdict = if verdict {
        make_atom(atoms.lookup("true").expect("seeded"))
    } else {
        make_atom(atoms.lookup("false").expect("seeded"))
    };
    let mut stub = StubFuns { verdict };
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_fun_caller(&mut stub);
    f(&registry, &mut ctx)
}

/// A **real** `EXPORT_EXT` fun term of arity `arity`, so the `is_function(Fun, Arity)`
/// guard observes the true arity (a small-integer sentinel never reaches the fun-call
/// layer — the guard rejects it first).
fn real_fun(ctx: &mut BifContext<'_>, arity: usize) -> Eterm {
    let module = atom_val(ctx.atom("f").expect("seeded"));
    let function = atom_val(ctx.atom("g").expect("seeded"));
    beam_codec::term::make_fun_external(ctx.heap(), arity, module, function)
        .expect("the external fun fits")
}

/// The improper list `[head | tail]` on `ctx`'s heap.
fn improper(ctx: &mut BifContext<'_>, head: i64, tail: Eterm) -> Eterm {
    let head = ctx.int(head).expect("a small int fits");
    ctx.cons(head, tail).expect("the cons fits")
}

/// The live reason term of a structured `error` raise.
fn live_reason(result: &Result<Eterm>) -> Eterm {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, ExceptionClass::Error, "the exception class");
            assert_eq!(
                raise.reason,
                ExitReason::Structured,
                "a term-carrying reason is `ExitReason::Structured`"
            );
            *raise
                .live_reason()
                .expect("a structured reason carries its term")
                .downcast_ref::<Eterm>()
                .expect("the payload is the live reason term")
        }
        other => panic!("expected a structured raise, got {other:?}"),
    }
}

/// Asserts the structured reason `{Tag, Value}`.
fn assert_tagged(result: Result<Eterm>, tag: Eterm, value: Eterm) {
    let reason = live_reason(&result);
    assert_eq!(tuple_arity(reason).expect("the reason is a tuple"), 2, "reason {reason:?}");
    assert_eq!(tuple_element(reason, 0).expect("the tag"), tag, "reason tag");
    assert_eq!(tuple_element(reason, 1).expect("the value"), value, "reason value");
}

/// Asserts the bare `error:function_clause` raise (no live-reason term).
fn assert_function_clause(result: Result<Eterm>) {
    assert!(
        matches!(&result, Err(BeamError::Raise(r))
            if r.class == ExceptionClass::Error
                && r.reason == ExitReason::atom("function_clause")
                && r.live_reason().is_none()),
        "expected error:function_clause, got {result:?}"
    );
}

/// Asserts that the call did **not** raise the guard's `function_clause` (the fun
/// reached the stub call layer).
fn assert_not_function_clause(result: &Result<Eterm>) {
    assert!(
        !matches!(result, Err(BeamError::Raise(r))
            if r.reason == ExitReason::atom("function_clause")),
        "the unguarded clause must not reject the fun, got {result:?}"
    );
}

/// The argument shape `0`/`1`/`2`/`3` of the empty list / `[1]` / a non-list atom /
/// the improper list `[1 | 2]`.
fn shaped_arg(ctx: &mut BifContext<'_>, shape: u8) -> Eterm {
    match shape {
        0 => NIL,
        1 => ctx.list(&[ctx.int(1).expect("fits")]).expect("fits"),
        2 => ctx.atom("a").expect("seeded"),
        _ => {
            let tail = ctx.int(2).expect("fits");
            improper(ctx, 1, tail)
        }
    }
}

// ---------------------------------------------------------------------------
// The arity-1 predicates: all/2, any/2, flatmap/2, takewhile/2, dropwhile/2,
// partition/2, splitwith/2
// ---------------------------------------------------------------------------

/// The seven arity-1 predicate functions this guard extension reaches beyond the
/// original five (`map/2`, `foreach/2`, `filter/2`, `foldl/3`, `foldr/3`).
const ARITY_1_PREDICATES: [&str; 7] =
    ["all", "any", "flatmap", "takewhile", "dropwhile", "partition", "splitwith"];

#[test]
fn arity_1_predicates_reject_a_non_fun_on_every_list_shape() {
    // Probed for each of the seven (the `lists:all`/`lists:any` rows):
    //   F(5, [])   => error:function_clause
    //   F(5, [1])  => error:function_clause
    //   F(5, a)    => error:function_clause
    //   F(5, [1|2]) => error:function_clause   (an improper tail is not reached: the
    //                                            clause head rejects the fun first)
    // The guard is the clause head, so it fires before the list is inspected — for
    // `[]` the fun would never have been called, and the empty-list cases fabricated
    // a value (`true`/`false`/`[]`/`{[],[]}`) before the guard landed.
    for name in ARITY_1_PREDICATES {
        for shape in [0u8, 1, 2, 3] {
            let result = run(true, |reg, ctx| {
                let arg = shaped_arg(ctx, shape);
                let non_fun = ctx.int(5).expect("fits");
                reg.call("lists", name, 2, ctx, &[non_fun, arg])
            });
            assert_function_clause(result);
        }
    }
}

#[test]
fn arity_1_predicates_reject_a_wrong_arity_fun() {
    // `lists:all(fun(_, _) -> x end, a) => error:function_clause` (probed): the guard
    // checks the arity, not just fun-ness, again for every list shape — including the
    // improper list `[1|2]`, which the guard rejects before the walk can reach it.
    for name in ARITY_1_PREDICATES {
        for shape in [0u8, 1, 2, 3] {
            let result = run(true, |reg, ctx| {
                let arg = shaped_arg(ctx, shape);
                let fun = real_fun(ctx, 2);
                reg.call("lists", name, 2, ctx, &[fun, arg])
            });
            assert_function_clause(result);
        }
    }
}

#[test]
fn arity_1_predicates_accept_a_real_arity_1_fun_on_the_empty_list() {
    // The empty list never calls the fun, so a matching arity is the only requirement
    // (probed: `lists:all(fun(_) -> true end, [])` => true,
    // `lists:any(fun(_) -> false end, [])` => false, the other five => their empty
    // result `[]`/`{[],[]}`).
    for name in ARITY_1_PREDICATES {
        let result = run(true, |reg, ctx| {
            let fun = real_fun(ctx, 1);
            reg.call("lists", name, 2, ctx, &[fun, NIL])
        });
        assert!(result.is_ok(), "lists:{name}/2 accepts a real arity-1 fun on []");
    }

    // `all/2` and `any/2` answer their identity element.
    let (_, truth) = atom("true");
    let result = run(false, |reg, ctx| {
        let fun = real_fun(ctx, 1);
        reg.call("lists", "all", 2, ctx, &[fun, NIL])
    });
    assert_eq!(result.expect("all/2 of [] is true"), truth, "lists:all(_, []) => true");

    let (_, falsity) = atom("false");
    let result = run(true, |reg, ctx| {
        let fun = real_fun(ctx, 1);
        reg.call("lists", "any", 2, ctx, &[fun, NIL])
    });
    assert_eq!(result.expect("any/2 of [] is false"), falsity, "lists:any(_, []) => false");
}

#[test]
fn arity_1_predicates_keep_the_oracle_list_shape_reason_with_a_real_fun() {
    // With a matching-arity fun the guard passes and the *list shape* decides — the
    // split the guard makes unreachable for a non-fun.
    // Probed: `lists:all(fun(_) -> true end, a)`  => {case_clause,a},
    //         `lists:any(fun(_) -> false end, a)` => {case_clause,a}.
    let (_, case_clause) = atom("case_clause");
    for name in ["all", "any"] {
        let (result, non_list) = run(true, |reg, ctx| {
            let non_list = ctx.atom("a").expect("seeded");
            let fun = real_fun(ctx, 1);
            (reg.call("lists", name, 2, ctx, &[fun, non_list]), non_list)
        });
        assert_tagged(result, case_clause, non_list);
    }

    // The remaining five clause-match the list directly (or their worker does), so a
    // non-list is the bare `function_clause`.
    // Probed: `lists:flatmap(fun(_) -> true end, a)` \
    //         `lists:takewhile(fun(_) -> true end, a)` \
    //         `lists:dropwhile(fun(_) -> true end, a)` \
    //         `lists:partition(fun(_) -> true end, a)` \
    //         `lists:splitwith(fun(_) -> true end, a)`  => {error,function_clause}.
    for name in ["flatmap", "takewhile", "dropwhile", "partition", "splitwith"] {
        let result = run(true, |reg, ctx| {
            let non_list = ctx.atom("a").expect("seeded");
            let fun = real_fun(ctx, 1);
            reg.call("lists", name, 2, ctx, &[fun, non_list])
        });
        assert_function_clause(result);
    }
}

// ---------------------------------------------------------------------------
// The arity-2 folds: mapfoldl/3, mapfoldr/3
// ---------------------------------------------------------------------------

#[test]
fn mapfold_rejects_a_non_fun_and_a_wrong_arity_fun() {
    for name in ["mapfoldl", "mapfoldr"] {
        // Probed: `lists:mapfoldl(5, 0, [])`/`(5, 0, [1])`/`(5, 0, a)` =>
        // {error,function_clause}, and the same for `mapfoldr/3`.
        for shape in [0u8, 1, 2] {
            let result = run(true, |reg, ctx| {
                let arg = shaped_arg(ctx, shape);
                let zero = ctx.int(0).expect("fits");
                let non_fun = ctx.int(5).expect("fits");
                reg.call("lists", name, 3, ctx, &[non_fun, zero, arg])
            });
            assert_function_clause(result);
        }

        // A fun of the wrong arity fails the same guard.
        for shape in [0u8, 1, 2] {
            let result = run(true, |reg, ctx| {
                let arg = shaped_arg(ctx, shape);
                let zero = ctx.int(0).expect("fits");
                let fun = real_fun(ctx, 1);
                reg.call("lists", name, 3, ctx, &[fun, zero, arg])
            });
            assert_function_clause(result);
        }

        // A real arity-2 fun passes it: `lists:mapfoldl(Fun2, 0, [])` => {[],0}, and
        // `lists:mapfoldr(Fun2, 0, [])` => {[],0} (probed; `[]` means the fun is never
        // called, so the stub never runs).
        let result = run(true, |reg, ctx| {
            let zero = ctx.int(0).expect("fits");
            let fun = real_fun(ctx, 2);
            reg.call("lists", name, 3, ctx, &[fun, zero, NIL])
        });
        assert!(result.is_ok(), "lists:{name}/3 accepts a real arity-2 fun on []");
    }
}

// ---------------------------------------------------------------------------
// usort/2 — every clause guarded
// ---------------------------------------------------------------------------

#[test]
fn usort_rejects_a_non_fun_for_every_clause() {
    // Probed: `lists:usort(5, [])`/`(5, [1])`/`(5, [1,2])` => {error,function_clause}
    // (every `usort/2` clause carries the guard) — and a non-list is the same
    // no-clause failure: `lists:usort(5, a)` => {error,function_clause}.
    for shape in [0u8, 1, 2, 3] {
        let result = run(true, |reg, ctx| {
            let arg = match shape {
                2 => ctx.list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")]).expect("fits"),
                other => shaped_arg(ctx, other),
            };
            let non_fun = ctx.int(5).expect("fits");
            reg.call("lists", "usort", 2, ctx, &[non_fun, arg])
        });
        assert_function_clause(result);
    }

    // A wrong-arity fun fails the guard too.
    let result = run(true, |reg, ctx| {
        let arg = ctx.list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")]).expect("fits");
        let fun = real_fun(ctx, 1);
        reg.call("lists", "usort", 2, ctx, &[fun, arg])
    });
    assert_function_clause(result);

    // A real arity-2 fun passes it: `lists:usort(fun(A, B) -> A =< B end, [])` => [].
    let result = run(true, |reg, ctx| {
        let fun = real_fun(ctx, 2);
        reg.call("lists", "usort", 2, ctx, &[fun, NIL])
    });
    assert!(result.is_ok(), "lists:usort/2 accepts a real arity-2 fun");
}

// ---------------------------------------------------------------------------
// sort/2 — only the [] and [_] clauses are guarded
// ---------------------------------------------------------------------------

#[test]
fn sort_guards_only_its_empty_and_singleton_clauses() {
    // Probed: `lists:sort(5, [])` and `lists:sort(5, [1])` => {error,function_clause}
    // (those two clauses carry `when is_function(Fun, 2)`), and a non-list is the same
    // no-clause failure: `lists:sort(5, a)` => {error,function_clause}.
    for shape in [0u8, 1, 2] {
        let result = run(true, |reg, ctx| {
            let arg = shaped_arg(ctx, shape);
            let non_fun = ctx.int(5).expect("fits");
            reg.call("lists", "sort", 2, ctx, &[non_fun, arg])
        });
        assert_function_clause(result);
    }

    // A wrong-arity fun fails the `[]` clause's guard.
    let result = run(true, |reg, ctx| {
        let fun = real_fun(ctx, 1);
        reg.call("lists", "sort", 2, ctx, &[fun, NIL])
    });
    assert_function_clause(result);

    // ≥ 2 elements match the **unguarded** `[X, Y | T]` clause, so the BIF must not
    // reject the non-fun: the oracle reaches the *call* layer instead —
    // `lists:sort(5, [1,2])` => {error,{badfun,5}}. The stub `FunCaller` stands in for
    // that layer, so the observable here is "the guard did not fire".
    for len in [2, 3] {
        let result = run(true, |reg, ctx| {
            let elems: Vec<Eterm> = (1..=len).map(|n| ctx.int(n).expect("fits")).collect();
            let list = ctx.list(&elems).expect("fits");
            let non_fun = ctx.int(5).expect("fits");
            reg.call("lists", "sort", 2, ctx, &[non_fun, list])
        });
        assert_not_function_clause(&result);
        assert!(result.is_ok(), "the unguarded clause reaches the (stub) call layer");
    }

    // A real arity-2 fun passes the empty list's guard:
    // `lists:sort(fun(A, B) -> A =< B end, [])` => [].
    let result = run(true, |reg, ctx| {
        let fun = real_fun(ctx, 2);
        reg.call("lists", "sort", 2, ctx, &[fun, NIL])
    });
    assert!(result.is_ok(), "lists:sort/2 accepts a real arity-2 fun on []");
}

// ---------------------------------------------------------------------------
// zipwith/3 — only the ([], []) clause is guarded
// ---------------------------------------------------------------------------

#[test]
fn zipwith_guards_only_its_empty_empty_clause() {
    // Probed: `lists:zipwith(5, [], [])` => {error,function_clause} (the terminal
    // clause guards `is_function(F, 2)`), and the length mismatches fail the clause set
    // itself — `lists:zipwith(5, [], [1])`, `lists:zipwith(5, [1,2], [])` => the same
    // {error,function_clause}, as does `lists:zipwith(5, a, a)`.
    for (left, right) in [(0u8, 0u8), (0, 1), (2, 0), (2, 2)] {
        let result = run(true, |reg, ctx| {
            let non_fun = ctx.int(5).expect("fits");
            // Two distinct, non-empty lists for the `(2, 2)` shape.
            let left_arg = if left == 2 {
                ctx.list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")]).expect("fits")
            } else {
                shaped_arg(ctx, if left == 0 { 0 } else { 2 })
            };
            let right_arg = if right == 2 {
                ctx.list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")]).expect("fits")
            } else {
                shaped_arg(ctx, right)
            };
            reg.call("lists", "zipwith", 3, ctx, &[non_fun, left_arg, right_arg])
        });
        assert_function_clause(result);
    }

    // A wrong-arity fun fails the `([], [])` guard.
    let result = run(true, |reg, ctx| {
        let fun = real_fun(ctx, 1);
        reg.call("lists", "zipwith", 3, ctx, &[fun, NIL, NIL])
    });
    assert_function_clause(result);

    // Equal-length non-empty lists match the **unguarded** first clause, so the BIF
    // must not reject the non-fun: the oracle reaches the call layer —
    // `lists:zipwith(5, [1], [2])` => {error,{badfun,5}}.
    let result = run(true, |reg, ctx| {
        let one = ctx.list(&[ctx.int(1).expect("fits")]).expect("fits");
        let two = ctx.list(&[ctx.int(2).expect("fits")]).expect("fits");
        let non_fun = ctx.int(5).expect("fits");
        reg.call("lists", "zipwith", 3, ctx, &[non_fun, one, two])
    });
    assert_not_function_clause(&result);
    assert!(result.is_ok(), "the unguarded clause reaches the (stub) call layer");

    // A real arity-2 fun passes the guard:
    // `lists:zipwith(fun(A, _B) -> A end, [], [])` => [].
    let result = run(true, |reg, ctx| {
        let fun = real_fun(ctx, 2);
        reg.call("lists", "zipwith", 3, ctx, &[fun, NIL, NIL])
    });
    assert!(result.is_ok(), "lists:zipwith/3 accepts a real arity-2 fun on []");
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

/// The oracle probe transcript — `(expression, the reason the system `erl` answered as
/// `{ok, V}` / `{error, R}`)`. This is the table the module docs record; the live
/// cross-check below re-derives the right-hand column.
const ORACLE_PROBES: &[(&str, &str)] = &[
    ("lists:all(5, [])", "{error,function_clause}"),
    ("lists:all(5, [1])", "{error,function_clause}"),
    ("lists:all(5, [1|2])", "{error,function_clause}"),
    ("lists:all(5, a)", "{error,function_clause}"),
    ("lists:all(fun(_) -> true end, a)", "{error,{case_clause,a}}"),
    ("lists:all(fun(_, _) -> x end, a)", "{error,function_clause}"),
    ("lists:all(fun(_) -> true end, [1|2])", "{error,function_clause}"),
    ("lists:all(fun(_) -> false end, [1|2])", "{ok,false}"),
    ("lists:all(fun(_) -> true end, [])", "{ok,true}"),
    ("lists:any(5, [])", "{error,function_clause}"),
    ("lists:any(5, [1|2])", "{error,function_clause}"),
    ("lists:any(5, a)", "{error,function_clause}"),
    ("lists:any(fun(_) -> true end, [1|2])", "{ok,true}"),
    ("lists:any(fun(_) -> false end, [1|2])", "{error,function_clause}"),
    ("lists:any(fun(_) -> false end, [])", "{ok,false}"),
    ("lists:flatmap(5, [])", "{error,function_clause}"),
    ("lists:flatmap(5, [1])", "{error,function_clause}"),
    ("lists:flatmap(fun(_) -> true end, [])", "{ok,[]}"),
    ("lists:flatmap(fun(_) -> true end, a)", "{error,function_clause}"),
    ("lists:takewhile(5, [])", "{error,function_clause}"),
    ("lists:takewhile(5, [1])", "{error,function_clause}"),
    ("lists:takewhile(fun(_) -> true end, [])", "{ok,[]}"),
    ("lists:takewhile(fun(_) -> true end, a)", "{error,function_clause}"),
    ("lists:dropwhile(5, [])", "{error,function_clause}"),
    ("lists:dropwhile(fun(_) -> true end, [])", "{ok,[]}"),
    ("lists:dropwhile(fun(_) -> true end, a)", "{error,function_clause}"),
    ("lists:partition(5, [])", "{error,function_clause}"),
    ("lists:partition(fun(_) -> true end, [])", "{ok,{[],[]}}"),
    ("lists:partition(fun(_) -> true end, a)", "{error,function_clause}"),
    ("lists:splitwith(5, [])", "{error,function_clause}"),
    ("lists:splitwith(fun(_) -> true end, [])", "{ok,{[],[]}}"),
    ("lists:splitwith(fun(_) -> true end, a)", "{error,function_clause}"),
    ("lists:mapfoldl(5, 0, [])", "{error,function_clause}"),
    ("lists:mapfoldl(5, 0, [1])", "{error,function_clause}"),
    ("lists:mapfoldl(5, 0, a)", "{error,function_clause}"),
    ("lists:mapfoldl(fun(X, A) -> {X, A} end, 0, [])", "{ok,{[],0}}"),
    ("lists:mapfoldr(5, 0, [])", "{error,function_clause}"),
    ("lists:mapfoldr(fun(X, A) -> {X, A} end, 0, [])", "{ok,{[],0}}"),
    ("lists:usort(5, [])", "{error,function_clause}"),
    ("lists:usort(5, [1])", "{error,function_clause}"),
    ("lists:usort(5, [1,2])", "{error,function_clause}"),
    ("lists:usort(fun(A, B) -> A =< B end, [])", "{ok,[]}"),
    ("lists:sort(5, [])", "{error,function_clause}"),
    ("lists:sort(5, [1])", "{error,function_clause}"),
    ("lists:sort(5, a)", "{error,function_clause}"),
    ("lists:sort(5, [1,2])", "{error,{badfun,5}}"),
    ("lists:sort(5, [1,2,3])", "{error,{badfun,5}}"),
    ("lists:sort(fun(A, B) -> A =< B end, [])", "{ok,[]}"),
    ("lists:zipwith(5, [], [])", "{error,function_clause}"),
    ("lists:zipwith(5, [], [1])", "{error,function_clause}"),
    ("lists:zipwith(5, [1,2], [])", "{error,function_clause}"),
    ("lists:zipwith(5, a, a)", "{error,function_clause}"),
    ("lists:zipwith(5, [1], [2])", "{error,{badfun,5}}"),
    ("lists:zipwith(fun(A, _B) -> A end, [], [])", "{ok,[]}"),
];

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` and the sibling test files use.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs lists_fun_guards] $ERTS_ORACLE='{}' is not an existing \
             executable — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Runs every probe through one `erl` invocation and returns the `{ok, V}`/`{error, R}`
/// terms it printed, one per line.
fn oracle_probe_lines(oracle: &Path, probes: &[(&str, &str)]) -> Vec<String> {
    let mut eval = String::from(
        "P = fun(E) -> R = try E() of V -> {ok, V} catch C:S -> {C, S} end, \
         io:format(\"~p~n\", [R]) end, ",
    );
    for (expr, _) in probes {
        eval.push_str("P(fun() -> ");
        eval.push_str(expr);
        eval.push_str(" end), ");
    }
    eval.push_str("ok");

    let output = Command::new(oracle)
        .args(["-noshell", "-eval", &eval, "-s", "init", "stop"])
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Every expectation this file pins is the live oracle's own answer. Skips loudly when
/// no oracle is available.
#[test]
fn the_guard_transcript_matches_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("======= beam_bifs lists_fun_guards: live oracle SKIPPED =======");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass — set");
        eprintln!("$ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("===============================================================");
        eprintln!();
        return;
    };

    let lines = oracle_probe_lines(&oracle, ORACLE_PROBES);
    assert_eq!(
        lines.len(),
        ORACLE_PROBES.len(),
        "the oracle printed {} lines for {} probes",
        lines.len(),
        ORACLE_PROBES.len()
    );
    for ((expr, expected), actual) in ORACLE_PROBES.iter().zip(lines.iter()) {
        assert_eq!(actual, expected, "the oracle answered {actual} for `{expr}`");
    }
}
