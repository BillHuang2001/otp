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

//! Integration tests for the **`fun`-taking `lists` BIFs** (`map/2`, `filter/2`,
//! `foldl/3`, `foldr/3`, `foreach/2`, `sort/2`, `usort/2`, `flatmap/2`, `zipwith/3`,
//! `mapfoldl/3`, `mapfoldr/3`, `all/2`, `any/2`, `takewhile/2`, `dropwhile/2`,
//! `partition/2`, `splitwith/2`, `keymap/3`).
//!
//! A `BifFn` cannot execute BEAM code, so these BIFs call their fun through the
//! public [`FunCaller`] seam. The tests therefore install a **mock** caller: the
//! "fun" argument is a small-integer sentinel naming the operation, and the mock
//! supplies the behaviour (a real `beam_rt` would resolve a closure reference
//! there). Every expected value is the value the OTP 29 / erts-17.1 `erl` oracle
//! produced for the equivalent Erlang fun, so the assertions pin the *oracle's*
//! semantics rather than this implementation's.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, FunCaller};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_codec::term::{cons_head, cons_tail, tuple_arity, tuple_element};
use beam_process::ProcessId;
use beam_term::{is_list, is_nil, is_small, make_atom, make_small, signed_value, AtomTable, Eterm, NIL};

/// `fun(X) -> X * X end`.
const SQUARE: i64 = 1;
/// `fun(X, Y) -> X + Y end` (also used as `fun(X, Acc) -> Acc + X end`).
const ADD: i64 = 2;
/// `fun(X) -> 2 * X end`.
const DOUBLE: i64 = 3;
/// `fun(X, Acc) -> [X | Acc] end`.
const CONS: i64 = 4;
/// `fun(X) -> X rem 2 =:= 0 end`.
const IS_EVEN: i64 = 5;
/// `fun(X) -> X < 3 end`.
const LT3: i64 = 6;
/// `fun(X) -> X > 2 end`.
const GT2: i64 = 7;
/// `fun(A, B) -> A =< B end` (the ordering fun `sort/2`/`usort/2` document).
const LESS_EQ: i64 = 8;
/// `fun(A, B) -> A < B end` — a *one-directional* comparator.
const LESS: i64 = 9;
/// `fun(X) -> [X, X] end`.
const DUPLICATE: i64 = 10;
/// `fun(X, Acc) -> {X * 10 + Acc, Acc + 1} end`.
const ACC_TEN: i64 = 11;
/// `fun(X) -> X end` — a fun whose result is neither a boolean nor a list.
const IDENTITY: i64 = 12;
/// A fun whose body raises `badarg`; the BIF must propagate it untouched.
const RAISES: i64 = 13;
/// `fun(X) -> X > 0 end`.
const POSITIVE: i64 = 14;
/// `fun({K1, _}, {K2, _}) -> K1 =< K2 end` — an ordering fun over `{Key, Value}`
/// pairs that compares the **keys** only, so two pairs with the same key are
/// mutually `Fun`-equivalent.
const KEY_LESS_EQ: i64 = 15;

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b", "c"]);
    atoms
}

/// The mock `FunCaller`: it recognises the sentinel fun term and computes the
/// operation's result on **its own** heap.
struct MockFuns {
    heap: &'static mut Heap,
    truth: Eterm,
    falsity: Eterm,
    calls: Vec<(i64, Vec<Eterm>)>,
}

impl MockFuns {
    fn new(atoms: &AtomTable) -> MockFuns {
        let atom = |name: &str| make_atom(atoms.lookup(name).expect("seeded atom"));
        // Leaked on purpose, for the same reason as the context heap in `with_mock`:
        // the funs that build terms (`CONS`, `DUPLICATE`, `ACC_TEN`) allocate here,
        // and the tests inspect those terms *after* `with_mock` has returned —
        // freeing this heap would make those reads a use-after-free.
        let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(8192).expect("mock heap")));
        MockFuns {
            heap,
            truth: atom("true"),
            falsity: atom("false"),
            calls: Vec::new(),
        }
    }

    fn boolean(&self, value: bool) -> Eterm {
        if value {
            self.truth
        } else {
            self.falsity
        }
    }
}

/// The `i64` behind a small-integer argument of the mock (never reached with any
/// other term — the BIFs under test only ever hand the mock the elements of the
/// list they were given).
fn number(term: Eterm) -> i64 {
    assert!(is_small(term), "the mock is only called with small integers");
    signed_value(term)
}

/// The small-integer **key** of a `{Key, Value}` pair argument of the mock.
fn key(term: Eterm) -> i64 {
    number(tuple_element(term, 0).expect("a {Key, Value} pair"))
}

impl FunCaller for MockFuns {
    fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        let operation = number(fun);
        self.calls.push((operation, args.to_vec()));
        match operation {
            SQUARE => Ok(make_small(number(args[0]) * number(args[0]))),
            ADD => Ok(make_small(number(args[0]) + number(args[1]))),
            DOUBLE => Ok(make_small(2 * number(args[0]))),
            CONS => self.heap.cons(args[0], args[1]),
            IS_EVEN => Ok(self.boolean(number(args[0]) % 2 == 0)),
            LT3 => Ok(self.boolean(number(args[0]) < 3)),
            GT2 => Ok(self.boolean(number(args[0]) > 2)),
            LESS_EQ => Ok(self.boolean(number(args[0]) <= number(args[1]))),
            LESS => Ok(self.boolean(number(args[0]) < number(args[1]))),
            DUPLICATE => {
                let tail = self.heap.cons(args[0], NIL)?;
                self.heap.cons(args[0], tail)
            }
            ACC_TEN => {
                let (x, acc) = (number(args[0]), number(args[1]));
                let mapped = make_small(x * 10 + acc);
                let next = make_small(acc + 1);
                self.heap.tuple(&[mapped, next])
            }
            IDENTITY => Ok(args[0]),
            RAISES => Err(BeamError::BadArg),
            POSITIVE => Ok(self.boolean(number(args[0]) > 0)),
            KEY_LESS_EQ => Ok(self.boolean(key(args[0]) <= key(args[1]))),
            other => panic!("unknown mock fun {other}"),
        }
    }
}

/// Runs `f` with the default registry over a real heap + seeded atom table and the
/// mock fun caller installed, returning the closure's value and the recorded calls.
fn with_mock<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> (R, Vec<(i64, Vec<Eterm>)>) {
    let registry = default_registry();
    let atoms = seeded_atoms();
    // Leaked on purpose: the closure's results are `Eterm`s that point into this
    // heap, and the tests inspect them *after* `with_mock` returns. Freeing the
    // heap here would make those reads a use-after-free.
    let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(65_536).expect("test heap")));
    let mut mock = MockFuns::new(&atoms);
    let result = {
        let mut ctx =
            BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_fun_caller(&mut mock);
        f(&registry, &mut ctx)
    };
    (result, mock.calls)
}

/// The elements of a proper list.
fn elements(term: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        out.push(cons_head(cur).expect("a cons head"));
        cur = cons_tail(cur).expect("a cons tail");
    }
    assert!(is_nil(cur), "expected a proper list, got a term ending in {cur:?}");
    out
}

/// The small integers of a proper list.
fn ints(term: Eterm) -> Vec<i64> {
    elements(term).iter().copied().map(number).collect()
}

/// A proper list of small integers built on the context heap.
fn int_list(ctx: &mut BifContext<'_>, items: &[i64]) -> Eterm {
    let elems: Vec<Eterm> = items.iter().copied().map(make_small).collect();
    ctx.list(&elems).expect("the list fits")
}

#[test]
fn map_applies_the_fun_left_to_right() {
    let ((), calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let fun = make_small(SQUARE);
        let args = [fun, list];
        let mapped = reg.call("lists", "map", 2, ctx, &args).expect("lists:map/2");
        // Oracle: lists:map(fun(X) -> X * X end, [1,2,3]) => [1,4,9].
        assert_eq!(ints(mapped), vec![1, 4, 9]);
    });

    assert_eq!(calls.len(), 3, "the fun runs once per element");
    assert_eq!(
        calls,
        vec![
            (SQUARE, vec![make_small(1)]),
            (SQUARE, vec![make_small(2)]),
            (SQUARE, vec![make_small(3)]),
        ]
    );
}

#[test]
fn map_of_the_empty_list_calls_nothing() {
    let (mapped, calls) = with_mock(|reg, ctx| {
        let args = [make_small(SQUARE), NIL];
        reg.call("lists", "map", 2, ctx, &args).expect("lists:map/2")
    });
    assert_eq!(mapped, NIL);
    assert!(calls.is_empty());
}

#[test]
fn map_rejects_a_non_list_and_an_improper_list() {
    let ((), calls) = with_mock(|reg, ctx| {
        let non_list = ctx.atom("a").expect("seeded");
        let improper = ctx.cons(make_small(1), make_small(2)).expect("fits");
        for bad in [non_list, improper] {
            let args = [make_small(SQUARE), bad];
            // The oracle raises `case_clause`/`function_clause`; Milestone-1 reports
            // `badarg` (documented in the module docs).
            assert!(matches!(
                reg.call("lists", "map", 2, ctx, &args),
                Err(BeamError::BadArg)
            ));
        }
    });
    assert!(calls.is_empty(), "no element of a rejected list is visited");

    // A *proper* list of non-integers is **not** rejected by `map/2` itself: the walk
    // reaches the fun, whose own failure is what surfaces (oracle:
    // `lists:map(fun(X) -> X * X end, [a])` raises the fun body's `badarith`). With
    // an identity fun the accepted case is observable — oracle:
    // `lists:map(fun(X) -> X end, [a])` => `[a]`.
    let (mapped, calls) = with_mock(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let list = ctx.list(&[a]).expect("fits");
        let args = [make_small(IDENTITY), list];
        reg.call("lists", "map", 2, ctx, &args).expect("lists:map/2")
    });
    let a = make_atom(seeded_atoms().lookup("a").expect("seeded"));
    assert_eq!(elements(mapped), vec![a]);
    assert_eq!(calls, vec![(IDENTITY, vec![a])]);
}

#[test]
fn foreach_visits_every_element_and_returns_ok() {
    let (result, calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let args = [make_small(DOUBLE), list];
        reg.call("lists", "foreach", 2, ctx, &args)
            .expect("lists:foreach/2")
    });
    // Oracle: lists:foreach/2 => ok.
    assert_eq!(result, make_atom(seeded_atoms().lookup("ok").expect("seeded")));
    assert_eq!(calls.len(), 3);
}

#[test]
fn filter_keeps_the_true_elements_in_order() {
    let (kept, calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3, 4]);
        let args = [make_small(IS_EVEN), list];
        reg.call("lists", "filter", 2, ctx, &args)
            .expect("lists:filter/2")
    });
    // Oracle: lists:filter(fun(X) -> X rem 2 =:= 0 end, [1,2,3,4]) => [2,4].
    assert_eq!(ints(kept), vec![2, 4]);
    assert_eq!(calls.len(), 4);
}

#[test]
fn filter_rejects_a_non_boolean_predicate_result() {
    let ((), calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1]);
        let args = [make_small(IDENTITY), list];
        // Oracle: `{bad_filter, 1}` — an error, never a truthy value.
        assert!(matches!(
            reg.call("lists", "filter", 2, ctx, &args),
            Err(BeamError::BadArg)
        ));
    });
    assert_eq!(calls.len(), 1);
}

#[test]
fn foldl_and_foldr_walk_in_opposite_directions() {
    let (sum, foldl_calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let args = [make_small(ADD), make_small(0), list];
        reg.call("lists", "foldl", 3, ctx, &args)
            .expect("lists:foldl/3")
    });
    // Oracle: lists:foldl(fun(X, A) -> A + X end, 0, [1,2,3]) => 6, calling
    // Fun(1, 0), Fun(2, 1), Fun(3, 3) — `Fun(Elem, Acc)` with the *running*
    // accumulator, left to right.
    assert_eq!(number(sum), 6);
    assert_eq!(
        foldl_calls,
        vec![
            (ADD, vec![make_small(1), make_small(0)]),
            (ADD, vec![make_small(2), make_small(1)]),
            (ADD, vec![make_small(3), make_small(3)]),
        ],
        "`foldl` calls Fun(Elem, Acc) left to right with the running accumulator"
    );

    let (built, foldr_calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let args = [make_small(CONS), NIL, list];
        reg.call("lists", "foldr", 3, ctx, &args)
            .expect("lists:foldr/3")
    });
    // Oracle: lists:foldr(fun(X, A) -> [X | A] end, [], [1,2,3]) => [1,2,3],
    // calling Fun(3, []), Fun(2, [3]), Fun(1, [2,3]) — right to left.
    assert_eq!(ints(built), vec![1, 2, 3]);
    assert_eq!(
        foldr_calls.iter().map(|(_, args)| number(args[0])).collect::<Vec<_>>(),
        vec![3, 2, 1],
        "`foldr` reaches the last element first"
    );
    assert_eq!(foldr_calls.len(), 3, "the fun runs once per element");
    // The accumulator threaded into the *first* call is the seed; the later ones are
    // the fun's own list results (built on the mock's heap, so only their identity
    // is asserted here).
    assert_eq!(foldr_calls[0].1[1], NIL, "the last element folds into the seed");
    assert_ne!(foldr_calls[1].1[1], NIL, "…and each earlier call gets the running accumulator");
}

#[test]
fn foldl_returns_the_accumulator_for_the_empty_list() {
    let (acc, calls) = with_mock(|reg, ctx| {
        let args = [make_small(ADD), make_small(41), NIL];
        reg.call("lists", "foldl", 3, ctx, &args)
            .expect("lists:foldl/3")
    });
    assert_eq!(number(acc), 41);
    assert!(calls.is_empty());
}

#[test]
fn all_and_any_short_circuit() {
    // `all(fun(X) -> X > 2 end, [1,2,3])` decides on the very first element.
    let (result, calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let args = [make_small(GT2), list];
        reg.call("lists", "all", 2, ctx, &args).expect("lists:all/2")
    });
    assert_eq!(result, make_atom(seeded_atoms().lookup("false").expect("seeded")));
    assert_eq!(calls.len(), 1, "`all/2` stops at the first false");

    // `any(fun(X) -> X < 3 end, [1,2,3])` decides on the very first element.
    let (result, calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let args = [make_small(LT3), list];
        reg.call("lists", "any", 2, ctx, &args).expect("lists:any/2")
    });
    assert_eq!(result, make_atom(seeded_atoms().lookup("true").expect("seeded")));
    assert_eq!(calls.len(), 1, "`any/2` stops at the first true");

    // Oracle: lists:all(fun(X) -> X > 0 end, [1,2,3]) => true; the empty list
    // answers without calling the fun at all.
    let (result, calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let args = [make_small(POSITIVE), list];
        let all = reg.call("lists", "all", 2, ctx, &args).expect("lists:all/2");
        let args = [make_small(IS_EVEN), NIL];
        let empty = reg.call("lists", "any", 2, ctx, &args).expect("lists:any/2");
        (all, empty)
    });
    assert_eq!(result.0, make_atom(seeded_atoms().lookup("true").expect("seeded")));
    assert_eq!(result.1, make_atom(seeded_atoms().lookup("false").expect("seeded")));
    assert_eq!(calls.len(), 3, "the empty list never calls the fun");
}

#[test]
fn takewhile_dropwhile_and_splitwith_are_lazy() {
    let ((taken, dropped, split), _) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3, 1]);
        let take = reg
            .call("lists", "takewhile", 2, ctx, &[make_small(LT3), list])
            .expect("lists:takewhile/2");
        let drop = reg
            .call("lists", "dropwhile", 2, ctx, &[make_small(LT3), list])
            .expect("lists:dropwhile/2");
        let split = reg
            .call("lists", "splitwith", 2, ctx, &[make_small(LT3), list])
            .expect("lists:splitwith/2");
        (take, drop, split)
    });
    // Oracle: takewhile => [1,2]; dropwhile => [3,1]; splitwith => {[1,2],[3,1]}.
    assert_eq!(ints(taken), vec![1, 2]);
    assert_eq!(ints(dropped), vec![3, 1]);
    assert_eq!(tuple_arity(split).expect("a 2-tuple"), 2);
    assert_eq!(ints(tuple_element(split, 0).expect("taken")), vec![1, 2]);
    assert_eq!(ints(tuple_element(split, 1).expect("rest")), vec![3, 1]);

    // The walk stops at the first `false` and never touches the improper tail.
    let ((), calls) = with_mock(|reg, ctx| {
        let improper = ctx.cons(make_small(1), make_small(2)).expect("fits");
        for name in ["takewhile", "dropwhile", "splitwith"] {
            let args = [make_small(GT2), improper];
            let result = reg.call("lists", name, 2, ctx, &args).expect("stops early");
            if name == "dropwhile" {
                // Oracle: lists:dropwhile(fun(X) -> X > 2 end, [1|2]) => [1|2].
                assert_eq!(result, improper);
            }
        }
    });
    assert_eq!(calls.len(), 3, "one predicate call per BIF");

    // A predicate that holds past the last proper element does reach the tail,
    // which has no clause — `error:function_clause` (verified against the oracle:
    // `lists:takewhile(fun(X) -> X < 3 end, [1|2])` etc.).
    let ((), _) = with_mock(|reg, ctx| {
        let improper = ctx.cons(make_small(1), make_small(2)).expect("fits");
        for name in ["takewhile", "dropwhile", "splitwith"] {
            let args = [make_small(LT3), improper];
            assert!(matches!(
                reg.call("lists", name, 2, ctx, &args),
                Err(BeamError::Raise(r))
                    if r.class == ExceptionClass::Error
                        && r.reason == ExitReason::atom("function_clause")
            ));
        }
    });
}

#[test]
fn partition_splits_in_order() {
    let (result, calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3, 4]);
        let args = [make_small(GT2), list];
        reg.call("lists", "partition", 2, ctx, &args)
            .expect("lists:partition/2")
    });
    // Oracle: lists:partition(fun(X) -> X > 2 end, [1,2,3,4]) => {[3,4],[1,2]}.
    assert_eq!(ints(tuple_element(result, 0).expect("satisfied")), vec![3, 4]);
    assert_eq!(ints(tuple_element(result, 1).expect("not satisfied")), vec![1, 2]);
    assert_eq!(calls.len(), 4);
}

#[test]
fn flatmap_concatenates_and_rejects_a_non_list_result() {
    let (flat, calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2]);
        let args = [make_small(DUPLICATE), list];
        reg.call("lists", "flatmap", 2, ctx, &args)
            .expect("lists:flatmap/2")
    });
    // Oracle: lists:flatmap(fun(X) -> [X,X] end, [1,2]) => [1,1,2,2].
    assert_eq!(ints(flat), vec![1, 1, 2, 2]);
    assert_eq!(calls.len(), 2);

    let ((), _) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1]);
        let args = [make_small(IDENTITY), list];
        // Oracle: `List ++ Rest` with a non-list is `badarg`.
        assert!(matches!(
            reg.call("lists", "flatmap", 2, ctx, &args),
            Err(BeamError::BadArg)
        ));
    });
}

#[test]
fn zipwith_pairs_and_rejects_a_length_mismatch() {
    let (zipped, calls) = with_mock(|reg, ctx| {
        let left = int_list(ctx, &[1, 2]);
        let right = int_list(ctx, &[10, 20]);
        let args = [make_small(ADD), left, right];
        reg.call("lists", "zipwith", 3, ctx, &args)
            .expect("lists:zipwith/3")
    });
    assert_eq!(ints(zipped), vec![11, 22]);
    assert_eq!(calls.len(), 2);

    let ((), _) = with_mock(|reg, ctx| {
        let left = int_list(ctx, &[1, 2, 3]);
        let right = int_list(ctx, &[10, 20]);
        let args = [make_small(ADD), left, right];
        // `lists:zipwith/3` is `zipwith(F, Xs, Ys, fail)`, whose `fail` mode has no
        // clause for a length mismatch — *it does not stop at the shorter list*.
        // Verified against the oracle: `lists:zipwith(fun(X,Y) -> X+Y end, [1,2], [3])`
        // raises `error:function_clause` at `zipwith/4` (lists.erl:965), which this
        // BIF reproduces. The `trim`/`{pad, …}` behaviours live in `zipwith/4`.
        assert!(matches!(
            reg.call("lists", "zipwith", 3, ctx, &args),
            Err(BeamError::Raise(r))
                if r.class == ExceptionClass::Error
                    && r.reason == ExitReason::atom("function_clause")
        ));
        let empty = [make_small(ADD), NIL, NIL];
        assert_eq!(
            reg.call("lists", "zipwith", 3, ctx, &empty)
                .expect("both empty"),
            NIL
        );
    });
}

#[test]
fn mapfoldl_and_mapfoldr_accumulate_in_opposite_directions() {
    let (left, left_calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let args = [make_small(ACC_TEN), make_small(0), list];
        reg.call("lists", "mapfoldl", 3, ctx, &args)
            .expect("lists:mapfoldl/3")
    });
    // Oracle: lists:mapfoldl(fun(X, A) -> {X*10+A, A+1} end, 0, [1,2,3])
    //         => {[10,21,32], 3}.
    assert_eq!(ints(tuple_element(left, 0).expect("mapped")), vec![10, 21, 32]);
    assert_eq!(number(tuple_element(left, 1).expect("acc")), 3);
    assert_eq!(
        left_calls.iter().map(|(_, args)| number(args[0])).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    let (right, right_calls) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1, 2, 3]);
        let args = [make_small(ACC_TEN), make_small(0), list];
        reg.call("lists", "mapfoldr", 3, ctx, &args)
            .expect("lists:mapfoldr/3")
    });
    // Oracle: the fun is applied right to left, but the mapped values come back in
    // *element* order: {[12,21,30], 3}.
    assert_eq!(ints(tuple_element(right, 0).expect("mapped")), vec![12, 21, 30]);
    assert_eq!(number(tuple_element(right, 1).expect("acc")), 3);
    assert_eq!(
        right_calls.iter().map(|(_, args)| number(args[0])).collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
}

#[test]
fn sort2_sorts_stably_and_usort2_collapses_equivalent_runs() {
    let (sorted, _) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[3, 1, 2]);
        let args = [make_small(LESS_EQ), list];
        reg.call("lists", "sort", 2, ctx, &args).expect("lists:sort/2")
    });
    assert_eq!(ints(sorted), vec![1, 2, 3]);

    let (unique, _) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[3, 1, 2, 1]);
        let args = [make_small(LESS_EQ), list];
        reg.call("lists", "usort", 2, ctx, &args).expect("lists:usort/2")
    });
    // Oracle: lists:usort(fun(A,B) -> A =< B end, [3,1,2,1]) => [1,2,3]. `usort/2`
    // collapses only elements that are *mutually* `Fun`-equivalent, and under `=<`
    // that means arithmetically equal — so the duplicate `1` disappears while the
    // distinct `3` stays.
    assert_eq!(ints(unique), vec![1, 2, 3]);

    // Stability: `{1, a}` and `{1, b}` have equal keys, so `Fun` considers them
    // equivalent and the stable sort keeps the input order
    // (oracle: lists:sort(F, [{1,a},{1,b}]) => [{1,a},{1,b}], where
    // F = fun({K1,_},{K2,_}) -> K1 =< K2 end).
    let ((stable, reordered), _) = with_mock(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let one_a = ctx.tuple(&[make_small(1), a]).expect("fits");
        let one_b = ctx.tuple(&[make_small(1), b]).expect("fits");
        let two_a = ctx.tuple(&[make_small(2), a]).expect("fits");
        let equal_keys = ctx.list(&[one_a, one_b]).expect("fits");
        let out_of_order = ctx.list(&[two_a, one_b]).expect("fits");
        let stable = reg
            .call("lists", "sort", 2, ctx, &[make_small(KEY_LESS_EQ), equal_keys])
            .expect("lists:sort/2");
        let reordered = reg
            .call("lists", "sort", 2, ctx, &[make_small(KEY_LESS_EQ), out_of_order])
            .expect("lists:sort/2");
        (stable, reordered)
    });
    let order = elements(stable);
    assert_eq!(order.len(), 2);
    assert_eq!(number(tuple_element(order[0], 0).expect("key")), 1);
    assert_eq!(number(tuple_element(order[1], 0).expect("key")), 1);
    let a = make_atom(seeded_atoms().lookup("a").expect("seeded"));
    let b = make_atom(seeded_atoms().lookup("b").expect("seeded"));
    assert_eq!(tuple_element(order[0], 1).expect("value"), a);
    assert_eq!(tuple_element(order[1], 1).expect("value"), b);
    // The keys do decide the order: oracle
    // `lists:sort(F, [{2,a},{1,b}])` => `[{1,b},{2,a}]`.
    let order = elements(reordered);
    assert_eq!(order.len(), 2);
    assert_eq!(number(tuple_element(order[0], 0).expect("key")), 1);
    assert_eq!(number(tuple_element(order[1], 0).expect("key")), 2);

    // `usort/2` *does* collapse a mutually `Fun`-equivalent run — here the second
    // pair of the sorted ([{1,a},{1,b}]) has the same key as the first
    // (oracle: lists:usort(F, [{1,a},{1,b}]) => [{1,a}]).
    let (unique, _) = with_mock(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let first = ctx.tuple(&[make_small(1), a]).expect("fits");
        let second = ctx.tuple(&[make_small(1), b]).expect("fits");
        let list = ctx.list(&[first, second]).expect("fits");
        let args = [make_small(KEY_LESS_EQ), list];
        reg.call("lists", "usort", 2, ctx, &args).expect("lists:usort/2")
    });
    let order = elements(unique);
    assert_eq!(order.len(), 1);
    assert_eq!(tuple_element(order[0], 1).expect("value"), a);

    // A *one-directional* comparator (`fun(A, B) -> A < B end`) considers distinct
    // elements non-equivalent in both directions, so `usort/2` removes nothing
    // (oracle: lists:usort(fun(A,B) -> A < B end, [3,1,2,1]) => [1,1,2,3]).
    let (unique, _) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[3, 1, 2, 1]);
        let args = [make_small(LESS), list];
        reg.call("lists", "usort", 2, ctx, &args).expect("lists:usort/2")
    });
    assert_eq!(ints(unique), vec![1, 1, 2, 3]);

    let (empty, _) = with_mock(|reg, ctx| {
        let args = [make_small(LESS_EQ), NIL];
        let sorted = reg.call("lists", "sort", 2, ctx, &args).expect("lists:sort/2");
        let args = [make_small(LESS_EQ), NIL];
        let unique = reg.call("lists", "usort", 2, ctx, &args).expect("lists:usort/2");
        (sorted, unique)
    });
    assert_eq!(empty.0, NIL);
    assert_eq!(empty.1, NIL);
}

#[test]
fn keymap_maps_only_the_indexed_tuple_element() {
    let (mapped, calls) = with_mock(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let first = ctx.tuple(&[a, make_small(1)]).expect("fits");
        let second = ctx.tuple(&[b, make_small(2)]).expect("fits");
        let list = ctx.list(&[first, second]).expect("fits");
        let args = [make_small(DOUBLE), make_small(2), list];
        reg.call("lists", "keymap", 3, ctx, &args)
            .expect("lists:keymap/3")
    });
    // Oracle: lists:keymap(fun(V) -> V * 2 end, 2, [{a,1},{b,2}]) => [{a,2},{b,4}].
    let pairs = elements(mapped);
    assert_eq!(pairs.len(), 2);
    assert_eq!(number(tuple_element(pairs[0], 1).expect("value")), 2);
    assert_eq!(number(tuple_element(pairs[1], 1).expect("value")), 4);
    assert_eq!(calls.len(), 2);

    // A non-tuple (or too-short tuple) element is `badarg` — and `element/2` runs
    // *before* the fun, so that element is never passed to it.
    let ((), calls) = with_mock(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let good = ctx.tuple(&[a, make_small(1)]).expect("fits");
        let bad = ctx.atom("b").expect("seeded");
        let list = ctx.list(&[good, bad]).expect("fits");
        let args = [make_small(DOUBLE), make_small(2), list];
        assert!(matches!(
            reg.call("lists", "keymap", 3, ctx, &args),
            Err(BeamError::BadArg)
        ));
    });
    assert_eq!(calls.len(), 1, "only the well-formed tuple reached the fun");
}

#[test]
fn a_failing_fun_is_propagated_verbatim() {
    let ((), _) = with_mock(|reg, ctx| {
        let list = int_list(ctx, &[1]);
        let args = [make_small(RAISES), list];
        match reg.call("lists", "map", 2, ctx, &args) {
            Err(error) => assert!(matches!(error, BeamError::BadArg)),
            other => panic!("expected the fun's own error, got {other:?}"),
        }
    });
}

#[test]
fn without_a_fun_caller_every_fun_taking_bif_is_a_typed_unsupported() {
    let registry = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1024).expect("test heap");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    // Two 2-tuples, so that *every* BIF below reaches its fun: `sort/2`/`usort/2`
    // need a second element to compare, and `keymap/3` needs a tuple with an
    // element 2 so its `element/2` check passes before the fun call.
    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let first = ctx.tuple(&[a, make_small(1)]).expect("fits");
    let second = ctx.tuple(&[b, make_small(2)]).expect("fits");
    let list = ctx.list(&[first, second]).expect("fits");

    let cases: [(&str, u32, Vec<Eterm>); 18] = [
        ("map", 2, vec![make_small(SQUARE), list]),
        ("filter", 2, vec![make_small(IS_EVEN), list]),
        ("foldl", 3, vec![make_small(ADD), make_small(0), list]),
        ("foldr", 3, vec![make_small(CONS), NIL, list]),
        ("foreach", 2, vec![make_small(DOUBLE), list]),
        ("sort", 2, vec![make_small(LESS_EQ), list]),
        ("usort", 2, vec![make_small(LESS_EQ), list]),
        ("flatmap", 2, vec![make_small(DUPLICATE), list]),
        ("zipwith", 3, vec![make_small(ADD), list, list]),
        ("mapfoldl", 3, vec![make_small(ACC_TEN), make_small(0), list]),
        ("mapfoldr", 3, vec![make_small(ACC_TEN), make_small(0), list]),
        ("all", 2, vec![make_small(IS_EVEN), list]),
        ("any", 2, vec![make_small(IS_EVEN), list]),
        ("takewhile", 2, vec![make_small(LT3), list]),
        ("dropwhile", 2, vec![make_small(LT3), list]),
        ("partition", 2, vec![make_small(GT2), list]),
        ("splitwith", 2, vec![make_small(LT3), list]),
        ("keymap", 3, vec![make_small(DOUBLE), make_small(2), list]),
    ];
    for (name, arity, args) in cases {
        match registry.call("lists", name, arity, &mut ctx, &args) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("lists:{name}/{arity} should need a FunCaller, got {other:?}"),
        }
    }

    // A list of fewer than two elements needs no comparison, so `sort/2`/`usort/2`
    // legitimately answer without a `FunCaller` (oracle: `lists:sort(F, [])` => [],
    // `lists:sort(F, [X])` => `[X]` — the ordering fun is never applied). That is
    // precisely why the cases above use a two-element list.
    let single = ctx.list(&[make_small(1)]).expect("fits");
    for name in ["sort", "usort"] {
        let sorted = registry
            .call("lists", name, 2, &mut ctx, &[make_small(LESS_EQ), single])
            .expect("no comparison needed");
        assert_eq!(ints(sorted), vec![1]);
        assert_eq!(
            registry
                .call("lists", name, 2, &mut ctx, &[make_small(LESS_EQ), NIL])
                .expect("no comparison needed"),
            NIL
        );
    }
}
