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

//! Integration tests for the **`fun`-taking `maps` BIFs** (`map/2`, `fold/3`,
//! `filter/2`, `foreach/2`).
//!
//! A `BifFn` cannot execute BEAM code, so — exactly like the `lists` fun-taking
//! BIFs in `tests/lists_funs.rs` — these call their fun through the public
//! [`FunCaller`] seam ([`BifContext::call_fun`]). Each test installs a **mock**
//! caller: the "fun" argument is a small-integer sentinel naming the operation, and
//! the mock supplies the behaviour (a real `beam_rt` would resolve a closure
//! reference there).
//!
//! OTP documents map iteration order as **unspecified** and the oracle's own order
//! is neither insertion nor sorted order (`maps:keys(#{b=>2,a=>1,c=>3})` ⇒
//! `[c,a,b]`), so every assertion here is **order-insensitive**: a result map is
//! compared pair by pair *after sorting*, and a traversal only asserts the *set* of
//! `{Key, Value}` pairs the fun saw. Every expected value is what the OTP 29 /
//! erts-17.1 `erl` oracle produced for the equivalent Erlang fun.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, FunCaller};
use beam_common::{BeamError, Result};
use beam_codec::term::{cons_head, cons_tail, tuple_element};
use beam_process::ProcessId;
use beam_term::{is_list, is_nil, is_small, make_atom, make_small, signed_value, AtomTable, Eterm};

/// `fun(K, V) -> K + V end`.
const KEY_PLUS_VALUE: i64 = 1;
/// `fun(_K, V) -> V > 15 end`.
const VALUE_GT_15: i64 = 2;
/// `fun(K, V, Acc) -> Acc + K * V end`.
const ACC_KEY_TIMES_VALUE: i64 = 3;
/// `fun(K, _V) -> K end` — a fun whose result is neither `true` nor `false`.
const KEY: i64 = 4;
/// A fun whose body raises `badarg`; the BIF must propagate it untouched.
const RAISES: i64 = 5;

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "case_clause"]);
    atoms
}

/// Asserts `result` is the oracle's structured raise whose live reason term is
/// structurally equal to `expected`.
///
/// The full contract this crate now honours: `Err(BeamError::Raise(r))`,
/// `r.reason == ExitReason::Structured` (so `BeamError::as_atom()` is `None`) and
/// `r.live_reason()` carrying the real reason term.
#[track_caller]
fn assert_structured(result: Result<Eterm>, ctx: &BifContext<'_>, expected: Eterm) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, beam_common::ExceptionClass::Error, "class");
            assert_eq!(raise.reason, beam_common::ExitReason::Structured, "reason");
            assert_eq!(raise.reason.as_atom(), None, "as_atom");
            let live = *raise
                .live_reason()
                .expect("a structured reason carries its live term")
                .downcast_ref::<Eterm>()
                .expect("the payload is the live reason term");
            assert!(
                beam_term::compare::eq(live, expected, ctx.atoms()),
                "live reason differs from the expected reason tuple"
            );
        }
        other => panic!("expected a structured raise, got {other:?}"),
    }
}

/// The mock `FunCaller`: it recognises the sentinel fun term and computes the
/// operation's result. Every result here is an immediate, so the mock needs no heap.
struct MockFuns {
    truth: Eterm,
    falsity: Eterm,
    calls: Vec<(i64, Vec<Eterm>)>,
}

impl MockFuns {
    fn new(atoms: &AtomTable) -> MockFuns {
        let atom = |name: &str| make_atom(atoms.lookup(name).expect("seeded atom"));
        MockFuns {
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

/// The `i64` behind a small-integer argument of the mock (every pair this suite
/// uses has small-integer keys and values).
fn number(term: Eterm) -> i64 {
    assert!(is_small(term), "the mock is only called with small integers");
    signed_value(term)
}

impl FunCaller for MockFuns {
    fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        let operation = number(fun);
        self.calls.push((operation, args.to_vec()));
        match operation {
            KEY_PLUS_VALUE => Ok(make_small(number(args[0]) + number(args[1]))),
            VALUE_GT_15 => Ok(self.boolean(number(args[1]) > 15)),
            ACC_KEY_TIMES_VALUE => Ok(make_small(number(args[2]) + number(args[0]) * number(args[1]))),
            KEY => Ok(args[0]),
            RAISES => Err(BeamError::BadArg),
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

/// A flat map of small-integer pairs, built through `maps:from_list/1`.
fn int_map(reg: &BifRegistry, ctx: &mut BifContext<'_>, pairs: &[(i64, i64)]) -> Eterm {
    let mut tuples = Vec::with_capacity(pairs.len());
    for &(key, value) in pairs {
        tuples.push(ctx.tuple(&[make_small(key), make_small(value)]).expect("fits"));
    }
    let list = ctx.list(&tuples).expect("fits");
    reg.call("maps", "from_list", 1, ctx, &[list])
        .expect("maps:from_list/1")
}

/// The `{Key, Value}` pairs of a map as small integers, **sorted** — the
/// order-insensitive view every assertion here uses.
fn pairs(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Vec<(i64, i64)> {
    let list = reg
        .call("maps", "to_list", 1, ctx, &[map])
        .expect("maps:to_list/1");
    let mut out: Vec<(i64, i64)> = elements(list)
        .into_iter()
        .map(|pair| {
            (
                number(tuple_element(pair, 0).expect("a key")),
                number(tuple_element(pair, 1).expect("a value")),
            )
        })
        .collect();
    out.sort_unstable();
    out
}

/// The `(Key, Value)` pairs the mock was actually called with, **sorted**.
fn observed(calls: &[(i64, Vec<Eterm>)]) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = calls
        .iter()
        .map(|(_, args)| (number(args[0]), number(args[1])))
        .collect();
    out.sort_unstable();
    out
}

/// `maps:size/1` of `map` (also asserts the term really is a map).
fn size(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> i64 {
    number(reg.call("maps", "size", 1, ctx, &[map]).expect("maps:size/1"))
}

#[test]
fn map_rebuilds_every_value_and_keeps_every_key() {
    let ((mapped, mapped_size), calls) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10), (2, 20)]);
        let mapped = reg
            .call("maps", "map", 2, ctx, &[make_small(KEY_PLUS_VALUE), map])
            .expect("maps:map/2");
        (pairs(reg, ctx, mapped), size(reg, ctx, mapped))
    });
    // Oracle: maps:map(fun(K,V) -> K+V end, #{1=>10,2=>20}) => #{1=>11,2=>22}
    // (order unspecified), so the map keeps its two keys with one new value each.
    assert_eq!(mapped, vec![(1, 11), (2, 22)]);
    assert_eq!(mapped_size, 2);
    // The fun saw every pair exactly once, as `(Key, Value)`.
    assert_eq!(calls.len(), 2);
    assert_eq!(observed(&calls), vec![(1, 10), (2, 20)]);
}

#[test]
fn map_of_the_empty_map_calls_nothing() {
    let ((mapped, mapped_size), calls) = with_mock(|reg, ctx| {
        let empty = reg.call("maps", "new", 0, ctx, &[]).expect("maps:new/0");
        let mapped = reg
            .call("maps", "map", 2, ctx, &[make_small(KEY_PLUS_VALUE), empty])
            .expect("maps:map/2");
        (pairs(reg, ctx, mapped), size(reg, ctx, mapped))
    });
    // Oracle: maps:map(_Fun, #{}) => #{}.
    assert!(mapped.is_empty());
    assert_eq!(mapped_size, 0);
    assert!(calls.is_empty(), "the empty map has no pair to visit");
}

#[test]
fn fold_visits_every_pair_and_threads_the_accumulator() {
    let (total, calls) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10), (2, 20)]);
        reg.call(
            "maps",
            "fold",
            3,
            ctx,
            &[make_small(ACC_KEY_TIMES_VALUE), make_small(0), map],
        )
        .expect("maps:fold/3")
    });
    // Oracle: maps:fold(fun(K,V,Acc) -> Acc + K*V end, 0, #{1=>10,2=>20}) => 50.
    // The accumulator is threaded into each call, so the total is order-insensitive
    // even though the traversal order is not fixed.
    assert_eq!(number(total), 50);
    assert_eq!(calls.len(), 2);
    assert_eq!(observed(&calls), vec![(1, 10), (2, 20)]);

    let (total, _) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10), (2, 20)]);
        reg.call(
            "maps",
            "fold",
            3,
            ctx,
            &[make_small(ACC_KEY_TIMES_VALUE), make_small(100), map],
        )
        .expect("maps:fold/3")
    });
    // Oracle: the same fold seeded with 100 => 150.
    assert_eq!(number(total), 150);
}

#[test]
fn fold_of_the_empty_map_returns_the_initial_accumulator() {
    let (acc, calls) = with_mock(|reg, ctx| {
        let empty = reg.call("maps", "new", 0, ctx, &[]).expect("maps:new/0");
        reg.call(
            "maps",
            "fold",
            3,
            ctx,
            &[make_small(ACC_KEY_TIMES_VALUE), make_small(7), empty],
        )
        .expect("maps:fold/3")
    });
    // Oracle: maps:fold(_Fun, 7, #{}) => 7.
    assert_eq!(number(acc), 7);
    assert!(calls.is_empty());
}

#[test]
fn filter_keeps_only_the_pairs_the_predicate_accepts() {
    let ((kept, kept_size), calls) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10), (2, 20)]);
        let kept = reg
            .call("maps", "filter", 2, ctx, &[make_small(VALUE_GT_15), map])
            .expect("maps:filter/2");
        (pairs(reg, ctx, kept), size(reg, ctx, kept))
    });
    // Oracle: maps:filter(fun(_K,V) -> V > 15 end, #{1=>10,2=>20}) => #{2=>20}.
    assert_eq!(kept, vec![(2, 20)]);
    assert_eq!(kept_size, 1);
    assert_eq!(calls.len(), 2, "every pair is tested");

    // A predicate that rejects everything yields the empty map
    // (oracle: maps:filter(fun(_K,V) -> V > 15 end, #{1=>10,2=>11}) => #{}).
    let ((none, none_size), _) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10), (2, 11)]);
        let none = reg
            .call("maps", "filter", 2, ctx, &[make_small(VALUE_GT_15), map])
            .expect("maps:filter/2");
        (pairs(reg, ctx, none), size(reg, ctx, none))
    });
    assert!(none.is_empty());
    assert_eq!(none_size, 0);
}

#[test]
fn filter_rejects_a_non_boolean_predicate_result() {
    let ((), calls) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10)]);
        // Oracle: maps:filter(fun(K,_V) -> K end, #{1=>10}) => error:{case_clause,1}
        // (the fun's own non-boolean result is the `case_clause` value).
        //   erl -noshell -eval 'try maps:filter(fun(K,_V) -> K end, #{1=>10}) catch C:R ->
        //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{case_clause,1}
        let result = reg.call("maps", "filter", 2, ctx, &[make_small(KEY), map]);
        let case_clause = ctx.atom("case_clause").expect("seeded");
        let expected = ctx.tuple(&[case_clause, make_small(1)]).expect("fits");
        assert_structured(result, ctx, expected);
    });
    assert_eq!(calls.len(), 1);
}

#[test]
fn foreach_visits_every_pair_and_returns_ok() {
    let (result, calls) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10), (2, 20)]);
        reg.call("maps", "foreach", 2, ctx, &[make_small(KEY_PLUS_VALUE), map])
            .expect("maps:foreach/2")
    });
    // Oracle: maps:foreach/2 => ok.
    assert_eq!(result, make_atom(seeded_atoms().lookup("ok").expect("seeded")));
    assert_eq!(calls.len(), 2);
    assert_eq!(observed(&calls), vec![(1, 10), (2, 20)]);
}

#[test]
fn a_failing_fun_is_propagated_verbatim() {
    let ((), _) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10)]);
        match reg.call("maps", "map", 2, ctx, &[make_small(RAISES), map]) {
            Err(error) => assert!(matches!(error, BeamError::BadArg)),
            other => panic!("expected the fun's own error, got {other:?}"),
        }
    });
}

#[test]
fn without_a_fun_caller_the_maps_fun_bifs_are_a_typed_unsupported() {
    let registry = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1024).expect("test heap");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let map = int_map(&registry, &mut ctx, &[(1, 10)]);

    let cases: [(&str, u32, Vec<Eterm>); 4] = [
        ("map", 2, vec![make_small(KEY_PLUS_VALUE), map]),
        ("fold", 3, vec![make_small(ACC_KEY_TIMES_VALUE), make_small(0), map]),
        ("filter", 2, vec![make_small(VALUE_GT_15), map]),
        ("foreach", 2, vec![make_small(KEY_PLUS_VALUE), map]),
    ];
    for (name, arity, args) in cases {
        match registry.call("maps", name, arity, &mut ctx, &args) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("maps:{name}/{arity} should need a FunCaller, got {other:?}"),
        }
    }

    // The empty map never calls the fun, so these legitimately answer without a
    // caller (oracle: maps:map(_Fun, #{}) => #{}, maps:fold(_Fun, Init, #{}) => Init).
    let empty = registry.call("maps", "new", 0, &mut ctx, &[]).expect("maps:new/0");
    assert_eq!(size(&registry, &mut ctx, empty), 0);
    let acc = registry
        .call(
            "maps",
            "fold",
            3,
            &mut ctx,
            &[make_small(ACC_KEY_TIMES_VALUE), make_small(41), empty],
        )
        .expect("maps:fold/3");
    assert_eq!(number(acc), 41);
}
