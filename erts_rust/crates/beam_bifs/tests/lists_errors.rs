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
//! The fun-taking functions are exercised through a stub [`FunCaller`]: it answers
//! arity-1 calls with a configurable boolean *verdict* (so `all/2`/`any/2` can be made
//! to walk past their short-circuit) and any other arity with its last argument (the
//! accumulator).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, FunCaller};
use beam_codec::term::{tuple_arity, tuple_element};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{make_atom, make_small, AtomTable, Eterm, NIL};

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

/// A stand-in for a fun argument: the stub `FunCaller` never inspects it.
fn a_fun() -> Eterm {
    make_small(0)
}

/// The improper list `[head | tail]` on `ctx`'s heap.
fn improper(ctx: &mut BifContext<'_>, head: i64, tail: Eterm) -> Eterm {
    let head = ctx.int(head).expect("a small int fits");
    ctx.cons(head, tail).expect("the cons fits")
}

#[test]
fn map_of_a_non_list_is_case_clause_and_of_an_improper_tail_function_clause() {
    // Oracle: lists:map(fun(X) -> X end, a) => error:{case_clause, a}.
    let (_, case_clause) = atom("case_clause");
    let (result, non_list) = run(true, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        (reg.call("lists", "map", 2, ctx, &[a_fun(), non_list]), non_list)
    });
    assert_tagged(result, case_clause, non_list);

    // Oracle: lists:map(fun(X) -> X end, [1|2]) => error:function_clause — the
    // `map_1/2` worker has no clause for the non-list tail.
    let result = run(true, |reg, ctx| {
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        reg.call("lists", "map", 2, ctx, &[a_fun(), list])
    });
    assert_function_clause(result);
}

#[test]
fn foldl_of_a_non_list_is_case_clause_and_of_an_improper_tail_function_clause() {
    let (_, case_clause) = atom("case_clause");

    // Oracle: lists:foldl(F, 0, a) => error:{case_clause, a}.
    let (result, non_list) = run(true, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        let zero = ctx.int(0).expect("fits");
        let result = reg.call("lists", "foldl", 3, ctx, &[a_fun(), zero, non_list]);
        (result, non_list)
    });
    assert_tagged(result, case_clause, non_list);

    // Oracle: lists:foldl(F, 0, [1|2]) => error:function_clause.
    let result = run(true, |reg, ctx| {
        let zero = ctx.int(0).expect("fits");
        let list = improper(ctx, 1, ctx.int(2).expect("fits"));
        reg.call("lists", "foldl", 3, ctx, &[a_fun(), zero, list])
    });
    assert_function_clause(result);
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

    // Oracle: lists:filter(fun(_) -> true end, a) => error:{bad_generator, a}.
    let (result, non_list) = run(true, |reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        (reg.call("lists", "filter", 2, ctx, &[a_fun(), non_list]), non_list)
    });
    assert_tagged(result, bad_generator, non_list);

    // Oracle: lists:filter(P, [1|2]) => {bad_generator, 2} — the non-list *remainder*,
    // not the argument.
    let (result, two) = run(true, |reg, ctx| {
        let two = ctx.int(2).expect("fits");
        let list = improper(ctx, 1, two);
        (reg.call("lists", "filter", 2, ctx, &[a_fun(), list]), two)
    });
    assert_tagged(result, bad_generator, two);

    // Oracle: lists:filter(P, [1,2|3]) => {bad_generator, 3}.
    let (result, three) = run(true, |reg, ctx| {
        let three = ctx.int(3).expect("fits");
        let list = improper(ctx, 1, improper(ctx, 2, three));
        (reg.call("lists", "filter", 2, ctx, &[a_fun(), list]), three)
    });
    assert_tagged(result, bad_generator, three);

    // The empty list filters to itself and calls nothing.
    let result = run(true, |reg, ctx| reg.call("lists", "filter", 2, ctx, &[a_fun(), NIL]));
    assert_eq!(result.expect("the empty list filters to itself"), NIL);
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
        let list = improper(ctx, 1, improper(ctx, 2, ctx.int(3).expect("fits")));
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
fn the_explicit_argument_rejections_stay_badarg() {
    // `member/2`, `keymember/3` and `keysearch/3` are real `bif.tab` BIFs: a non-list
    // haystack/tuple list is the C's explicit `badarg`, never a no-clause failure
    // (oracle-verified).
    for name in ["member", "keymember", "keysearch"] {
        let result = run(true, |reg, ctx| {
            let a = ctx.atom("a").expect("seeded");
            reg.call("lists", name, 2, ctx, &[a, a])
        });
        assert_badarg(result);
    }

    // `keyfind/3` likewise (arity 3).
    let result = run(true, |reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let one = ctx.int(1).expect("fits");
        reg.call("lists", "keyfind", 3, ctx, &[a, one, a])
    });
    assert_badarg(result);
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
