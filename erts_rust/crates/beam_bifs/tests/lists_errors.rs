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

//! The `lists` **argument-shape error classes**, pinned against the OTP 29 /
//! erts-17.1 oracle.
//!
//! `lists.erl` reports a list argument that is not a list (or whose tail is not) with
//! one of three reasons, and which one depends on the *clause shape* of the function —
//! never on a blanket `badarg`:
//!
//! * `error:function_clause` — the clause-set itself rejects the argument
//!   (`reverse/1`, `keymap/3`, an *improper* tail of `map/2`, `foldl/3`, `all/2` and
//!   `any/2`, an improper tail past `keysort/2`'s first two elements);
//! * `error:{case_clause, V}` — the walk `case`s the whole argument up front
//!   (`map/2`/`foldl/3`/`all/2`/`any/2` as a non-list) or clause-matches it against
//!   `[]`/`[_]`/`[X, Y | T]` (`keysort/2`);
//! * `error:{bad_generator, V}` — `filter/2` is a list comprehension, so its
//!   generator reports the value it cannot iterate (the whole argument for a non-list,
//!   the non-list **remainder** for an improper list).
//!
//! Every expectation below is the reason the oracle produced for the equivalent `erl`
//! call. The crate's own `badarg` cases (`member/2`, `keysearch/3`, `keysort/2`'s and
//! `keymap/3`'s `element/2`) are pinned too, so a regression cannot hide behind the
//! structured-reason path.
//!
//! A **fun** argument is checked first: every fun-taking `lists.erl` function whose
//! clause head carries `is_function(F, Arity)` (`map/2`, `foreach/2`, `filter/2`,
//! `foldl/3`, `foldr/3`, …) raises the bare `error:function_clause` for a non-fun or a
//! fun of the wrong arity — *before* the list argument is looked at, so the
//! list-shape reasons above are only reachable with a fun of the right arity.
//!
//! The fun-taking functions are exercised through a stub [`FunCaller`]: it answers
//! arity-1 calls with a configurable boolean *verdict* (so `all/2`/`any/2` can be made
//! to walk past their short-circuit) and any other arity with its last argument (the
//! accumulator).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, FunCaller};
use beam_codec::term::{tuple_arity, tuple_element};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, make_atom, make_small, AtomTable, Eterm, NIL};

/// The atoms the reasons and the probes need. `case_clause`/`bad_generator` are the
/// structured-reason tags; neither is a predefined emulator atom, so they are resolved
/// through [`BifContext::intern_atom`] (they are seeded here, which is the same thing
/// the runtime's `AtomInterner` would do).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "case_clause", "bad_generator", "a", "b", "c"]);
    atoms
}

/// The atom `name` in a freshly seeded table *and* the term naming it. Identical seed
/// sequences produce identical atom values, which is what lets a test name an expected
/// atom without threading a live context out of its closure.
fn atom(name: &str) -> (AtomTable, Eterm) {
    let atoms = seeded_atoms();
    let value = make_atom(atoms.lookup(name).expect("seeded"));
    (atoms, value)
}

/// A `FunCaller` that never inspects the fun: an arity-1 call answers the configured
/// *verdict* atom, any other arity answers its last argument (the accumulator).
struct StubFuns {
    verdict: Eterm,
}

impl FunCaller for StubFuns {
    fn call_fun(&mut self, _fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        if args.len() == 1 {
            Ok(self.verdict)
        } else {
            Ok(*args.last().expect("a call always has arguments"))
        }
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

/// Asserts the bare `error:badarg`.
fn assert_badarg(result: Result<Eterm>) {
    assert!(matches!(result, Err(BeamError::BadArg)), "expected badarg, got {result:?}");
}

/// A stand-in for a fun argument that is **not** a fun: the stub `FunCaller` never
/// inspects it, and it only ever reaches a BIF whose clause head does *not* guard on
/// the fun (`all/2`/`any/2` case the list first, the explicit-`badarg` BIFs never look
/// at a fun). Every BIF with an `is_function(F, Arity)` head guard takes [`real_fun`]
/// instead — a non-fun fails that guard with `error:function_clause` before the list is
/// walked.
fn a_fun() -> Eterm {
    make_small(0)
}

/// A **real** `EXPORT_EXT` fun term of arity `arity`, so a fun-*ness*/*arity* check
/// (the `[]` clause's `is_function(Fun, 1)` guard) observes the true arity — unlike
/// [`a_fun`], which is an opaque small-int sentinel.
fn real_fun(ctx: &mut BifContext<'_>, arity: usize) -> Eterm {
    let module = atom_val(ctx.atom("a").expect("seeded"));
    let function = atom_val(ctx.atom("b").expect("seeded"));
    beam_codec::term::make_fun_external(ctx.heap(), arity, module, function)
        .expect("the external fun fits")
}

/// The improper list `[head | tail]` on `ctx`'s heap.
fn improper(ctx: &mut BifContext<'_>, head: i64, tail: Eterm) -> Eterm {
    let head = ctx.int(head).expect("a small int fits");
    ctx.cons(head, tail).expect("the cons fits")
}

#[test]
fn map_of_a_non_list_is_case_clause_and_of_an_improper_tail_function_clause() {
    // Oracle: lists:map(fun(X) -> X end, a) => error:{case_clause, a}. The fun is a
    // **real** arity-1 fun, so `map/2`'s clause-head guard `is_function(F, 1)` passes
    // and the failure is the walk's own `case`.
    let (_, case_clause) = atom("case_clause");
    let (result, non_list) = run(true, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        let fun = real_fun(ctx, 1);
        (reg.call("lists", "map", 2, ctx, &[fun, non_list]), non_list)
    });
    assert_tagged(result, case_clause, non_list);

    // Oracle: lists:map(fun(X) -> X end, [1|2]) => error:function_clause — the
    // `map_1/2` worker has no clause for the non-list tail.
    let result = run(true, |reg, ctx| {
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        let fun = real_fun(ctx, 1);
        reg.call("lists", "map", 2, ctx, &[fun, list])
    });
    assert_function_clause(result);

    // A first argument that is **not** an arity-1 fun fails the clause head
    // `map(F, List) when is_function(F, 1)` *before* the list argument is ever
    // inspected, so the reason is the bare `error:function_clause` for **every** list
    // shape — the `{case_clause, V}`/`function_clause` split above is unreachable with
    // a non-fun. Probed against the oracle:
    //   lists:map(5, [1])                  => error:function_clause
    //   lists:map(5, x)                    => error:function_clause
    //   lists:map(5, [])                   => error:function_clause
    //   lists:map(fun(X, Y) -> X end, [1]) => error:function_clause
    //   lists:map(fun(X, Y) -> X end, x)   => error:function_clause
    //   lists:map(fun() -> 1 end, [1])     => error:function_clause
    //   lists:map({a, b}, x)               => error:function_clause
    for list in [0, 1, 2] {
        let result = run(true, |reg, ctx| {
            let arg = match list {
                0 => NIL,
                1 => ctx.list(&[ctx.int(1).expect("fits")]).expect("fits"),
                _ => ctx.atom("a").expect("seeded"),
            };
            let non_fun = ctx.int(5).expect("fits");
            reg.call("lists", "map", 2, ctx, &[non_fun, arg])
        });
        assert_function_clause(result);
    }

    // A fun of the *wrong* arity fails it too, again for both list shapes.
    for list in [false, true] {
        let result = run(true, |reg, ctx| {
            let arg = if list {
                ctx.list(&[ctx.int(1).expect("fits")]).expect("fits")
            } else {
                ctx.atom("a").expect("seeded")
            };
            let fun = real_fun(ctx, 2);
            reg.call("lists", "map", 2, ctx, &[fun, arg])
        });
        assert_function_clause(result);
    }
}

#[test]
fn foldl_of_a_non_list_is_case_clause_and_of_an_improper_tail_function_clause() {
    let (_, case_clause) = atom("case_clause");

    // Oracle: lists:foldl(fun(X, A) -> X + A end, 0, a) => error:{case_clause, a} —
    // with a **real** arity-2 fun, so the clause-head guard `is_function(F, 2)` passes
    // and `foldl/3`'s own `case` reports.
    let (result, non_list) = run(true, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        let zero = ctx.int(0).expect("fits");
        let fun = real_fun(ctx, 2);
        let result = reg.call("lists", "foldl", 3, ctx, &[fun, zero, non_list]);
        (result, non_list)
    });
    assert_tagged(result, case_clause, non_list);

    // Oracle: lists:foldl(fun(X, A) -> X + A end, 0, [1|2]) => error:function_clause.
    let result = run(true, |reg, ctx| {
        let zero = ctx.int(0).expect("fits");
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        let fun = real_fun(ctx, 2);
        reg.call("lists", "foldl", 3, ctx, &[fun, zero, list])
    });
    assert_function_clause(result);

    // A first argument that is not an arity-2 fun fails the clause head
    // `foldl(F, Accu, List) when is_function(F, 2)` before the list argument is
    // touched — the bare `error:function_clause` whatever the list shape. Probed:
    //   lists:foldl(5, 0, [1])                => error:function_clause
    //   lists:foldl(5, 0, a)                  => error:function_clause
    //   lists:foldl(fun(X) -> X end, 0, [1])  => error:function_clause
    //   lists:foldl(fun(X, Y, Z) -> X end, 0, [1]) => error:function_clause
    for list in [0, 1, 2] {
        let result = run(true, |reg, ctx| {
            let arg = match list {
                0 => NIL,
                1 => ctx.list(&[ctx.int(1).expect("fits")]).expect("fits"),
                _ => ctx.atom("a").expect("seeded"),
            };
            let zero = ctx.int(0).expect("fits");
            let non_fun = ctx.int(5).expect("fits");
            reg.call("lists", "foldl", 3, ctx, &[non_fun, zero, arg])
        });
        assert_function_clause(result);
    }

    // An arity-1 fun fails it too, for both a proper list and a non-list.
    for list in [false, true] {
        let result = run(true, |reg, ctx| {
            let arg = if list {
                ctx.list(&[ctx.int(1).expect("fits")]).expect("fits")
            } else {
                ctx.atom("a").expect("seeded")
            };
            let zero = ctx.int(0).expect("fits");
            let fun = real_fun(ctx, 1);
            reg.call("lists", "foldl", 3, ctx, &[fun, zero, arg])
        });
        assert_function_clause(result);
    }
}

#[test]
fn all_and_any_of_a_non_list_are_case_clause() {
    let (_, case_clause) = atom("case_clause");

    // Oracle: lists:all(fun(_) -> true end, a) => error:{case_clause, a}.
    let (result, non_list) = run(true, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        (reg.call("lists", "all", 2, ctx, &[a_fun(), non_list]), non_list)
    });
    assert_tagged(result, case_clause, non_list);

    // Oracle: lists:any(fun(_) -> false end, a) => error:{case_clause, a}.
    let (result, non_list) = run(false, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        (reg.call("lists", "any", 2, ctx, &[a_fun(), non_list]), non_list)
    });
    assert_tagged(result, case_clause, non_list);
}

#[test]
fn all_and_any_reach_an_improper_tail_only_without_a_short_circuit() {
    let (_, truth) = atom("true");
    let (_, falsity) = atom("false");

    // A `true` verdict keeps `all/2` walking, so it reaches the improper tail:
    // oracle: lists:all(fun(_) -> true end, [1|2]) => error:function_clause.
    let result = run(true, |reg, ctx| {
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        reg.call("lists", "all", 2, ctx, &[a_fun(), list])
    });
    assert_function_clause(result);

    // A `false` verdict lets `all/2` answer before the tail is ever visited:
    // oracle: lists:all(fun(_) -> false end, [1|2]) => false.
    let result = run(false, |reg, ctx| {
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        reg.call("lists", "all", 2, ctx, &[a_fun(), list])
    });
    assert_eq!(result.expect("short-circuits to false"), falsity);

    // `any/2` is the mirror image: a `false` verdict walks on to the tail…
    let result = run(false, |reg, ctx| {
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        reg.call("lists", "any", 2, ctx, &[a_fun(), list])
    });
    assert_function_clause(result);

    // …and a `true` verdict short-circuits: oracle => true.
    let result = run(true, |reg, ctx| {
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        reg.call("lists", "any", 2, ctx, &[a_fun(), list])
    });
    assert_eq!(result.expect("short-circuits to true"), truth);
}

#[test]
fn filter_reports_the_bad_generator_value() {
    let (_, bad_generator) = atom("bad_generator");

    // Oracle: lists:filter(fun(_) -> true end, a) => error:{bad_generator, a} — the
    // fun is a **real** arity-1 fun, so the clause-head guard `is_function(Pred, 1)`
    // passes and the comprehension's generator reports.
    let (result, non_list) = run(true, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        let fun = real_fun(ctx, 1);
        (reg.call("lists", "filter", 2, ctx, &[fun, non_list]), non_list)
    });
    assert_tagged(result, bad_generator, non_list);

    // Oracle: lists:filter(fun(_) -> true end, [1|2]) => {bad_generator, 2} — the
    // non-list *remainder*, not the argument.
    let (result, two) = run(true, |reg, ctx| {
        let two = ctx.int(2).expect("fits");
        let list = improper(ctx, 1, two);
        let fun = real_fun(ctx, 1);
        (reg.call("lists", "filter", 2, ctx, &[fun, list]), two)
    });
    assert_tagged(result, bad_generator, two);

    // Oracle: lists:filter(fun(_) -> true end, [1,2|3]) => {bad_generator, 3}.
    let (result, three) = run(true, |reg, ctx| {
        let three = ctx.int(3).expect("fits");
        let inner = improper(ctx, 2, three);
        let list = improper(ctx, 1, inner);
        let fun = real_fun(ctx, 1);
        (reg.call("lists", "filter", 2, ctx, &[fun, list]), three)
    });
    assert_tagged(result, bad_generator, three);

    // The empty list filters to itself and calls nothing.
    let result = run(true, |reg, ctx| {
        let fun = real_fun(ctx, 1);
        reg.call("lists", "filter", 2, ctx, &[fun, NIL])
    });
    assert_eq!(result.expect("the empty list filters to itself"), NIL);

    // A first argument that is not an arity-1 fun fails the clause head
    // `filter(Pred, List) when is_function(Pred, 1)` *before* the generator is
    // entered, so the `{bad_generator, V}` shape is unreachable with a non-fun — the
    // reason is the bare `error:function_clause`. Probed:
    //   lists:filter(5, [1])                 => error:function_clause
    //   lists:filter(5, x)                   => error:function_clause
    //   lists:filter(5, [])                  => error:function_clause
    //   lists:filter(fun(X, Y) -> X end, [1]) => error:function_clause
    //   lists:filter(fun(X, Y) -> X end, x)  => error:function_clause
    for list in [0, 1, 2] {
        let result = run(true, |reg, ctx| {
            let arg = match list {
                0 => NIL,
                1 => ctx.list(&[ctx.int(1).expect("fits")]).expect("fits"),
                _ => ctx.atom("a").expect("seeded"),
            };
            let non_fun = ctx.int(5).expect("fits");
            reg.call("lists", "filter", 2, ctx, &[non_fun, arg])
        });
        assert_function_clause(result);
    }

    // A fun of the wrong arity fails it too, again for both list shapes.
    for list in [false, true] {
        let result = run(true, |reg, ctx| {
            let arg = if list {
                ctx.list(&[ctx.int(1).expect("fits")]).expect("fits")
            } else {
                ctx.atom("a").expect("seeded")
            };
            let fun = real_fun(ctx, 2);
            reg.call("lists", "filter", 2, ctx, &[fun, arg])
        });
        assert_function_clause(result);
    }
}

#[test]
fn reverse1_rejects_a_non_list_and_a_bad_tail_but_reverse2_stays_badarg() {
    // Oracle: lists:reverse(a) => error:function_clause.
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        reg.call("lists", "reverse", 1, ctx, &[a])
    });
    assert_function_clause(result);

    // Oracle: lists:reverse([1|2]) => error:function_clause (a one-element improper
    // list matches none of the four clauses)…
    let result = run(true, |reg, ctx| {
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        reg.call("lists", "reverse", 1, ctx, &[list])
    });
    assert_function_clause(result);

    // …but from two elements on the clause hands its tail to the `reverse/2` BIF:
    // oracle: lists:reverse([1,2|3]) => error:badarg.
    let result = run(true, |reg, ctx| {
        let three = ctx.int(3).expect("fits");
        let inner = improper(ctx, 2, three);
        let list = improper(ctx, 1, inner);
        reg.call("lists", "reverse", 1, ctx, &[list])
    });
    assert_badarg(result);

    // `reverse/2` is the checking BIF: a non-list is `badarg`.
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        reg.call("lists", "reverse", 2, ctx, &[a, NIL])
    });
    assert_badarg(result);

    // A proper list still reverses.
    let result = run(true, |reg, ctx| {
        let list = ctx.list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")]).expect("fits");
        reg.call("lists", "reverse", 1, ctx, &[list])
    });
    assert!(result.is_ok(), "a proper list still reverses");
}

#[test]
fn keysort_reports_case_clause_function_clause_and_badarg() {
    let (_, case_clause) = atom("case_clause");

    // Oracle: lists:keysort(1, a) => error:{case_clause, a}.
    let (result, non_list) = run(true, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        let one = ctx.int(1).expect("fits");
        (reg.call("lists", "keysort", 2, ctx, &[one, non_list]), non_list)
    });
    assert_tagged(result, case_clause, non_list);

    // Oracle: lists:keysort(1, [1|2]) => error:{case_clause, [1|2]} — the argument
    // itself, not its tail.
    let (result, list) = run(true, |reg, ctx| {
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        let one = ctx.int(1).expect("fits");
        (reg.call("lists", "keysort", 2, ctx, &[one, list]), list)
    });
    assert_tagged(result, case_clause, list);

    // Two elements are enough for the `keysplit_1`/`keysort_1` workers, so an improper
    // tail *past* the first two elements is a no-clause failure instead:
    // oracle: lists:keysort(1, [{2,b},{1,a}|3]) => error:function_clause.
    let result = run(true, |reg, ctx| {
        let b = ctx.atom("b").expect("seeded");
        let a = ctx.atom("a").expect("seeded");
        let two_b = ctx.tuple(&[ctx.int(2).expect("fits"), b]).expect("fits");
        let one_a = ctx.tuple(&[ctx.int(1).expect("fits"), a]).expect("fits");
        let list = ctx.cons(two_b, ctx.int(3).expect("fits")).expect("fits");
        let list = ctx.cons(one_a, list).expect("fits");
        let one = ctx.int(1).expect("fits");
        reg.call("lists", "keysort", 2, ctx, &[one, list])
    });
    assert_function_clause(result);

    // A non-positive `Index` fails the `is_integer(I), I > 0` guard.
    let result = run(true, |reg, ctx| {
        let zero = ctx.int(0).expect("fits");
        reg.call("lists", "keysort", 2, ctx, &[zero, NIL])
    });
    assert_function_clause(result);

    // A non-integer `Index` fails the same guard.
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        reg.call("lists", "keysort", 2, ctx, &[a, NIL])
    });
    assert_function_clause(result);

    // `element/2` on a non-tuple element stays `badarg`:
    // oracle: lists:keysort(1, [a,b]) => error:badarg.
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let list = ctx.list(&[a, b]).expect("fits");
        let one = ctx.int(1).expect("fits");
        reg.call("lists", "keysort", 2, ctx, &[one, list])
    });
    assert_badarg(result);
}

#[test]
fn keymap_splits_function_clause_from_element_badarg() {
    // The argument is not a list at all — no clause matches:
    // oracle: lists:keymap(F, 1, a) => function_clause,
    //         lists:keymap(F, 1, {a,b}) => function_clause,
    //         lists:keymap(F, 2, a) => function_clause.
    for (index, tuple_arg) in [(1, false), (1, true), (2, false)] {
        let result = run(true, |reg, ctx| {
            let index = ctx.int(index).expect("fits");
            let list = if tuple_arg {
                let a = ctx.atom("a").expect("seeded");
                let b = ctx.atom("b").expect("seeded");
                ctx.tuple(&[a, b]).expect("fits")
            } else {
                ctx.atom("a").expect("seeded")
            };
            reg.call("lists", "keymap", 3, ctx, &[a_fun(), index, list])
        });
        assert_function_clause(result);
    }

    // An improper tail the walk reaches: neither clause matches.
    // Oracle: lists:keymap(F, 1, [{a,b}|2]) => error:function_clause.
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let pair = ctx.tuple(&[a, b]).expect("fits");
        let list = ctx.cons(pair, ctx.int(2).expect("fits")).expect("fits");
        let one = ctx.int(1).expect("fits");
        reg.call("lists", "keymap", 3, ctx, &[a_fun(), one, list])
    });
    assert_function_clause(result);

    // A non-positive `Index` only fails the `[]` clause's guard, so the same call on a
    // *non-empty* list dies inside `element/2` instead:
    // oracle: lists:keymap(F, 0, []) => function_clause,
    //         lists:keymap(F, 0, [{a,b}]) => badarg.
    let result = run(true, |reg, ctx| {
        let zero = ctx.int(0).expect("fits");
        reg.call("lists", "keymap", 3, ctx, &[a_fun(), zero, NIL])
    });
    assert_function_clause(result);

    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let pair = ctx.tuple(&[a, b]).expect("fits");
        let list = ctx.list(&[pair]).expect("fits");
        let zero = ctx.int(0).expect("fits");
        reg.call("lists", "keymap", 3, ctx, &[a_fun(), zero, list])
    });
    assert_badarg(result);

    // A short tuple and a non-tuple element are `badarg`:
    // oracle: lists:keymap(F, 2, [{a}]) => badarg,
    //         lists:keymap(F, 1, [{a,b},2]) => badarg.
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let pair = ctx.tuple(&[a]).expect("fits");
        let list = ctx.list(&[pair]).expect("fits");
        let two = ctx.int(2).expect("fits");
        reg.call("lists", "keymap", 3, ctx, &[a_fun(), two, list])
    });
    assert_badarg(result);

    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let pair = ctx.tuple(&[a, b]).expect("fits");
        let two = ctx.int(2).expect("fits");
        let list = ctx.list(&[pair, two]).expect("fits");
        let one = ctx.int(1).expect("fits");
        reg.call("lists", "keymap", 3, ctx, &[a_fun(), one, list])
    });
    assert_badarg(result);

    // A well-formed walk still succeeds (the stub fun returns the verdict atom).
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let pair = ctx.tuple(&[a, b]).expect("fits");
        let list = ctx.list(&[pair]).expect("fits");
        let one = ctx.int(1).expect("fits");
        reg.call("lists", "keymap", 3, ctx, &[a_fun(), one, list])
    });
    assert!(result.is_ok(), "a well-formed tuple list still maps");
}

#[test]
fn keymap_empty_list_requires_an_arity_1_fun() {
    // The `[]` clause's guard is `is_integer(Index), Index >= 1, is_function(Fun, 1)`,
    // so an empty list with a *valid* `Index` still rejects a fun of the wrong arity
    // (or a non-fun) — the crate returned `[]` unconditionally before this guard was
    // modelled. Probed:
    //   lists:keymap(fun(X) -> X end, 1, [])     => []
    //   lists:keymap(non_fun, 1, [])             => error:function_clause
    //   lists:keymap(fun(_, _) -> ok end, 1, []) => error:function_clause
    let result = run(true, |reg, ctx| {
        let one = ctx.int(1).expect("fits");
        let fun = real_fun(ctx, 1);
        reg.call("lists", "keymap", 3, ctx, &[fun, one, NIL])
    });
    assert_eq!(result.expect("an arity-1 fun maps [] to []"), NIL);

    // A non-fun (the opaque stub sentinel) fails the guard.
    let result = run(true, |reg, ctx| {
        let one = ctx.int(1).expect("fits");
        reg.call("lists", "keymap", 3, ctx, &[a_fun(), one, NIL])
    });
    assert_function_clause(result);

    // A fun of arity 2 fails it too.
    let result = run(true, |reg, ctx| {
        let one = ctx.int(1).expect("fits");
        let fun = real_fun(ctx, 2);
        reg.call("lists", "keymap", 3, ctx, &[fun, one, NIL])
    });
    assert_function_clause(result);

    // ...but the guard is inert once the walk has consumed an element: a non-empty
    // list matches the *unguarded* first clause, so the same arity-2 fun is applied
    // (here by the stub caller) rather than rejected. Probed: the fun-ness guard
    // never fires for a non-empty list.
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let pair = ctx.tuple(&[a, b]).expect("fits");
        let list = ctx.list(&[pair]).expect("fits");
        let one = ctx.int(1).expect("fits");
        let fun = real_fun(ctx, 2);
        reg.call("lists", "keymap", 3, ctx, &[fun, one, list])
    });
    assert!(result.is_ok(), "a non-empty list never consults the [] clause's guard");
}

#[test]
fn the_explicit_argument_rejections_stay_badarg() {
    // `member/2`, `keymember/3`, `keysearch/3` and `keyfind/3` are real `bif.tab`
    // BIFs: a non-list haystack/tuple list is the C's explicit `badarg`, never a
    // no-clause failure (oracle-verified). The tuple-list walkers take an extra
    // `Index` argument (arity 3); `member/2` is the arity-2 form.
    for (name, arity) in [("member", 2u32), ("keymember", 3), ("keysearch", 3), ("keyfind", 3)] {
        let result = run(true, |reg, ctx| {
            let a = ctx.atom("a").expect("seeded");
            let args = if arity == 2 {
                vec![a, a]
            } else {
                let one = ctx.int(1).expect("fits");
                vec![a, one, a]
            };
            reg.call("lists", name, arity, ctx, &args)
        });
        assert_badarg(result);
    }
}

#[test]
fn the_registry_holds_every_function_under_test() {
    let registry = default_registry();
    for (name, arity) in [
        ("map", 2),
        ("foldl", 3),
        ("all", 2),
        ("any", 2),
        ("filter", 2),
        ("reverse", 1),
        ("reverse", 2),
        ("keysort", 2),
        ("keymap", 3),
        ("member", 2),
        ("keyfind", 3),
        ("keymember", 3),
        ("keysearch", 3),
    ] {
        assert!(registry.contains("lists", name, arity), "missing lists:{name}/{arity}");
    }
}
