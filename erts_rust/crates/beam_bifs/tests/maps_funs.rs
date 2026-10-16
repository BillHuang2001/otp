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
//! caller: the "fun" argument is a **real** `EXPORT_EXT` fun term whose *function*
//! atom names the operation, and the mock supplies the behaviour (a real `beam_rt`
//! would resolve a closure reference there).
//!
//! A `fun`-taking `maps` BIF first applies the oracle's `is_function(Fun, Arity)`
//! clause-head guard ([`mock_fun`]), so the mock's fun must be a real fun of the
//! right arity — an opaque sentinel never reaches the mock, and the guard's own
//! `error:badarg` cases are pinned in `tests/maps_fun_guards.rs`.
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
use beam_codec::term::{cons_head, cons_tail, fun_function, make_fun_external, tuple_element};
use beam_process::ProcessId;
use beam_term::{
    atom_val, is_list, is_nil, is_small, make_atom, make_small, signed_value, AtomTable, Eterm,
};

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

/// The `(operation, true arity, function-atom)` triple of every mock fun.
///
/// The mock's fun argument is a **real** `EXPORT_EXT` fun term (see [`mock_fun`]):
/// a `fun`-taking `maps` BIF validates its fun with the oracle's
/// `is_function(Fun, Arity)` clause-head guard, so an opaque small-integer sentinel
/// would be rejected with the plain `error:badarg` before the mock is ever
/// consulted. The operation is therefore carried in the fun's *function* atom, which
/// [`MockFuns::operation`] decodes back through the seeded [`AtomTable`].
///
/// The arity is the one the equivalent Erlang fun has — `map/2`, `filter/2` and
/// `foreach/2` call the fun with two arguments and `fold/3` with three, so the fun
/// the mock is handed must match.
const MOCK_FUNS: [(i64, usize, &str); 5] = [
    (KEY_PLUS_VALUE, 2, "f_key_plus_value"),
    (VALUE_GT_15, 2, "f_value_gt_15"),
    (ACC_KEY_TIMES_VALUE, 3, "f_acc_key_times_value"),
    (KEY, 2, "f_key"),
    (RAISES, 2, "f_raises"),
];

/// The fun term the mock recognizes as `operation`: an `EXPORT_EXT` fun of the
/// operation's true arity whose function atom names it.
///
/// Built on the *context* heap (the BIF only hands the term straight back to the
/// [`FunCaller`], never copying it), so a test can build it mid-walk with nothing
/// but its `BifContext`.
fn mock_fun(ctx: &mut BifContext<'_>, operation: i64) -> Eterm {
    let (_, arity, name) =
        MOCK_FUNS.iter().find(|(id, _, _)| *id == operation).expect("a known mock operation");
    let module = atom_val(ctx.atom("mock").expect("seeded"));
    let function = atom_val(ctx.atom(name).expect("seeded"));
    make_fun_external(ctx.heap(), *arity, module, function).expect("the mock fun fits")
}

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "case_clause", "mock"]);
    atoms.seed(MOCK_FUNS.iter().map(|(_, _, name)| *name));
    // The map-or-iterator walk resolves `none` (its terminal marker) and the
    // structured `badmap` reason tag through the table, and the bad-iterator probes
    // use two more atoms as an unreadable path head/tail.
    atoms.seed(["none", "badmap", "foo", "bar"]);
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

/// The mock `FunCaller`: it decodes the operation a mock fun term names and computes
/// the operation's result. Every result here is an immediate, so the mock needs no heap.
struct MockFuns {
    /// The seeded atom table, kept to resolve a mock fun's *function* atom back to its
    /// operation after `with_mock` has returned.
    atoms: &'static AtomTable,
    truth: Eterm,
    falsity: Eterm,
    calls: Vec<(i64, Vec<Eterm>)>,
}

impl MockFuns {
    fn new(atoms: &'static AtomTable) -> MockFuns {
        let atom = |name: &str| make_atom(atoms.lookup(name).expect("seeded atom"));
        MockFuns {
            atoms,
            truth: atom("true"),
            falsity: atom("false"),
            calls: Vec::new(),
        }
    }

    /// The operation the mock fun term `fun` names.
    ///
    /// Every fun argument reaching the mock is one [`mock_fun`] built, so its function
    /// atom is a seeded [`MOCK_FUNS`] name; anything else is a test bug, not an input
    /// the BIF could produce.
    fn operation(&self, fun: Eterm) -> i64 {
        let function = fun_function(fun).expect("a mock fun argument is an external fun");
        let name = self.atoms.name(function).expect("the mock fun's function atom is seeded");
        MOCK_FUNS
            .iter()
            .find(|(_, _, atom)| *atom == name)
            .map(|(id, _, _)| *id)
            .unwrap_or_else(|| panic!("unknown mock fun {name}"))
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
        let operation = self.operation(fun);
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
    // Leaked for the same reason as the heap: the mock resolves a mock fun's function
    // atom through this table *after* `with_mock` has returned, and the expected atom
    // values must be identical for every seed sequence.
    let atoms: &'static AtomTable = Box::leak(Box::new(seeded_atoms()));
    // Leaked on purpose: the closure's results are `Eterm`s that point into this
    // heap, and the tests inspect them *after* `with_mock` returns. Freeing the
    // heap here would make those reads a use-after-free.
    let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(65_536).expect("test heap")));
    let mut mock = MockFuns::new(atoms);
    let result = {
        let mut ctx =
            BifContext::new(heap, atoms, ProcessId::new(1, 0)).with_fun_caller(&mut mock);
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
        let fun = mock_fun(ctx, KEY_PLUS_VALUE);
        let mapped = reg.call("maps", "map", 2, ctx, &[fun, map]).expect("maps:map/2");
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
        let fun = mock_fun(ctx, KEY_PLUS_VALUE);
        let mapped = reg.call("maps", "map", 2, ctx, &[fun, empty]).expect("maps:map/2");
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
        let fun = mock_fun(ctx, ACC_KEY_TIMES_VALUE);
        reg.call("maps", "fold", 3, ctx, &[fun, make_small(0), map])
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
        let fun = mock_fun(ctx, ACC_KEY_TIMES_VALUE);
        reg.call("maps", "fold", 3, ctx, &[fun, make_small(100), map])
            .expect("maps:fold/3")
    });
    // Oracle: the same fold seeded with 100 => 150.
    assert_eq!(number(total), 150);
}

#[test]
fn fold_of_the_empty_map_returns_the_initial_accumulator() {
    let (acc, calls) = with_mock(|reg, ctx| {
        let empty = reg.call("maps", "new", 0, ctx, &[]).expect("maps:new/0");
        let fun = mock_fun(ctx, ACC_KEY_TIMES_VALUE);
        reg.call("maps", "fold", 3, ctx, &[fun, make_small(7), empty])
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
        let fun = mock_fun(ctx, VALUE_GT_15);
        let kept = reg.call("maps", "filter", 2, ctx, &[fun, map]).expect("maps:filter/2");
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
        let fun = mock_fun(ctx, VALUE_GT_15);
        let none = reg.call("maps", "filter", 2, ctx, &[fun, map]).expect("maps:filter/2");
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
        let fun = mock_fun(ctx, KEY);
        let result = reg.call("maps", "filter", 2, ctx, &[fun, map]);
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
        let fun = mock_fun(ctx, KEY_PLUS_VALUE);
        reg.call("maps", "foreach", 2, ctx, &[fun, map]).expect("maps:foreach/2")
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
        let fun = mock_fun(ctx, RAISES);
        match reg.call("maps", "map", 2, ctx, &[fun, map]) {
            Err(error) => assert!(matches!(error, BeamError::BadArg)),
            other => panic!("expected the fun's own error, got {other:?}"),
        }
    });
}

#[test]
fn without_a_fun_caller_the_maps_fun_bifs_are_a_typed_unsupported() {
    let registry = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let map = int_map(&registry, &mut ctx, &[(1, 10)]);

    let cases: [(&str, u32, i64, Vec<Eterm>); 4] = [
        ("map", 2, KEY_PLUS_VALUE, vec![map]),
        ("fold", 3, ACC_KEY_TIMES_VALUE, vec![make_small(0), map]),
        ("filter", 2, VALUE_GT_15, vec![map]),
        ("foreach", 2, KEY_PLUS_VALUE, vec![map]),
    ];
    for (name, arity, operation, tail) in cases {
        // A **valid** fun (the right arity) with no caller installed is the typed
        // `unsupported_bif` gap — the guard passes, the seam is missing.
        let fun = mock_fun(&mut ctx, operation);
        let mut args = vec![fun];
        args.extend(tail);
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
    let fun = mock_fun(&mut ctx, ACC_KEY_TIMES_VALUE);
    let acc = registry
        .call("maps", "fold", 3, &mut ctx, &[fun, make_small(41), empty])
        .expect("maps:fold/3");
    assert_eq!(number(acc), 41);
}

// ---------------------------------------------------------------------------
// `MapOrIter`: a map OR a `maps` iterator
// ---------------------------------------------------------------------------
//
// `maps.erl` gives each of these four functions a `map(Fun, Map) when is_map(Map)`
// clause and a `map(Fun, Iter) when is_function(Fun, 2)` clause that walks anything
// else through `try_next/2` — so a `maps:iterator/1,2` result is a first-class
// argument (`beam_ssa_recv:plan_clears/2` relies on it:
// `maps:fold(F, #{}, maps:iterator(UsageMap, ordered))`). A bad iterator is the
// structured `error:{badmap, Iter}` naming the **original** argument, and `none` is a
// valid empty iterator while `[]` is not.

/// The shapes a `MapOrIter` argument can take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// The map term itself (the `is_map(Map)` clause).
    Map,
    /// `maps:iterator/1` — `[0 | Map]`.
    Path,
    /// The `maps:iterator(Map, ordered)` shape — `[Keys | Map]`.
    Ordered,
    /// The `{Key, Value, NextIter}` look-ahead `maps:next/1` returns, which is itself
    /// a valid iterator (`try_next/2`'s first clause).
    Next,
}

/// `maps:iterator(M)` through the registry — the `[0 | M]` improper cons.
fn iterator(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Eterm {
    reg.call("maps", "iterator", 1, ctx, &[map]).expect("maps:iterator/1")
}

/// `maps:next(maps:iterator(M))` — the `{Key, Value, NextIter}` look-ahead.
fn look_ahead(reg: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> Eterm {
    let iter = iterator(reg, ctx, map);
    reg.call("maps", "next", 1, ctx, &[iter]).expect("maps:next/1")
}

/// `map` in the requested shape: the map, its `[0 | M]` iterator, a `[Keys | M]`
/// ordered iterator over the map's own stored-order keys, or the `{K,V,Iter}`
/// look-ahead.
fn shaped(reg: &BifRegistry, ctx: &mut BifContext<'_>, shape: Shape, map: Eterm) -> Eterm {
    match shape {
        Shape::Map => map,
        Shape::Path => iterator(reg, ctx, map),
        Shape::Ordered => {
            // `maps:iterator(M, ordered)` is `[KeysOf(M) | M]`: the walk order is then
            // the map's own stored order, so all four shapes must agree exactly.
            let keys = reg.call("maps", "keys", 1, ctx, &[map]).expect("maps:keys/1");
            ctx.cons(keys, map).expect("fits")
        }
        Shape::Next => look_ahead(reg, ctx, map),
    }
}

/// Runs `maps:<function>` with `fun` and `arg` (`init` is `fold/3`'s accumulator).
fn run_fun_bif(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    function: &str,
    fun: Eterm,
    init: i64,
    arg: Eterm,
) -> Result<Eterm> {
    match function {
        "fold" => reg.call("maps", "fold", 3, ctx, &[fun, make_small(init), arg]),
        other => reg.call("maps", other, 2, ctx, &[fun, arg]),
    }
}

/// The `(Key, Value)` pairs the mock was called with, in **visit** order.
fn visited_in_order(calls: &[(i64, Vec<Eterm>)]) -> Vec<(i64, i64)> {
    calls
        .iter()
        .map(|(_, args)| (number(args[0]), number(args[1])))
        .collect()
}

/// The `{Key, Value}` pairs of a map **or an iterator** as small integers, in **visit**
/// order (`maps:to_list/1` accepts both, so it is the order-sensitive view).
fn pairs_in_order(reg: &BifRegistry, ctx: &mut BifContext<'_>, arg: Eterm) -> Vec<(i64, i64)> {
    let list = reg.call("maps", "to_list", 1, ctx, &[arg]).expect("maps:to_list/1");
    elements(list)
        .into_iter()
        .map(|pair| {
            (
                number(tuple_element(pair, 0).expect("a key")),
                number(tuple_element(pair, 1).expect("a value")),
            )
        })
        .collect()
}

/// Every one of the four `fun`-taking BIFs walks a map, a `[0 | M]` iterator, a
/// `[Keys | M]` ordered iterator and a `{K,V,Iter}` look-ahead **identically** —
/// same result, same pairs, and the same visit order (the map's own stored order).
#[test]
fn every_fun_bif_walks_a_map_and_every_iterator_shape_alike() {
    for shape in [Shape::Map, Shape::Path, Shape::Ordered, Shape::Next] {
        let ((stored, mapped, mapped_size, total, kept, kept_size, ok), calls) =
            with_mock(|reg, ctx| {
                let map = int_map(reg, ctx, &[(1, 10), (2, 20), (3, 30)]);
                let stored = pairs_in_order(reg, ctx, map);
                let arg = shaped(reg, ctx, shape, map);
                let add = mock_fun(ctx, KEY_PLUS_VALUE);
                let acc = mock_fun(ctx, ACC_KEY_TIMES_VALUE);
                let gt = mock_fun(ctx, VALUE_GT_15);
                let mapped = run_fun_bif(reg, ctx, "map", add, 0, arg).expect("maps:map/2");
                let total = run_fun_bif(reg, ctx, "fold", acc, 0, arg).expect("maps:fold/3");
                let kept = run_fun_bif(reg, ctx, "filter", gt, 0, arg).expect("maps:filter/2");
                let ok = run_fun_bif(reg, ctx, "foreach", add, 0, arg).expect("maps:foreach/2");
                (
                    stored,
                    pairs(reg, ctx, mapped),
                    size(reg, ctx, mapped),
                    number(total),
                    pairs(reg, ctx, kept),
                    size(reg, ctx, kept),
                    ok,
                )
            });
        // Oracle: maps:map(fun(K,V) -> K+V end, #{1=>10,2=>20,3=>30}) => #{1=>11,2=>22,3=>33};
        // maps:fold(fun(K,V,Acc) -> Acc+K*V end, 0, …) => 140;
        // maps:filter(fun(_K,V) -> V > 15 end, …) => #{2=>20,3=>30}.
        assert_eq!(mapped, vec![(1, 11), (2, 22), (3, 33)], "map/2 via {shape:?}");
        assert_eq!(mapped_size, 3, "map/2 via {shape:?}");
        assert_eq!(total, 140, "fold/3 via {shape:?}");
        assert_eq!(kept, vec![(2, 20), (3, 30)], "filter/2 via {shape:?}");
        assert_eq!(kept_size, 2, "filter/2 via {shape:?}");
        assert_eq!(
            ok,
            make_atom(seeded_atoms().lookup("ok").expect("seeded")),
            "foreach/2 via {shape:?}"
        );
        // Each of the four traversals visited every pair once, in the map's stored
        // order — `filter`'s rejected pair included (`maps.erl:747`).
        let mut expected = Vec::new();
        for _ in 0..4 {
            expected.extend_from_slice(&stored);
        }
        assert_eq!(visited_in_order(&calls), expected, "visit order via {shape:?}");
    }
}

/// The **path** — not the map's storage order — decides the walk order: `[Keys | M]`
/// visits `Keys` in the path's own order, exactly as the oracle's
/// `maps:iterator(M, ordered)` / `(M, reversed)` / `(M, CmpFun)` do.
#[test]
fn the_ordered_path_decides_the_walk_order() {
    let ((), calls) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10), (2, 20), (3, 30)]);
        let acc = mock_fun(ctx, ACC_KEY_TIMES_VALUE);
        for keys in [[1i64, 2, 3], [3, 2, 1]] {
            let path: Vec<Eterm> = keys.iter().map(|&k| make_small(k)).collect();
            let path = ctx.list(&path).expect("fits");
            let arg = ctx.cons(path, map).expect("fits");
            let total = run_fun_bif(reg, ctx, "fold", acc, 0, arg).expect("maps:fold/3");
            assert_eq!(number(total), 140);
        }
    });
    // Oracle: maps:iterator(M, ordered) visits 1,2,3 and maps:iterator(M, reversed)
    // visits 3,2,1 — the two paths are the reverse of each other here.
    assert_eq!(
        visited_in_order(&calls),
        vec![(1, 10), (2, 20), (3, 30), (3, 30), (2, 20), (1, 10)]
    );
}

/// The atom `none` is a valid `MapOrIter` holding no pair: `fold` answers `Init`,
/// `map`/`filter` `#{}`, `foreach` `ok` — and the fun is never called.
#[test]
fn none_is_a_valid_empty_iterator_for_every_fun_bif() {
    let ((mapped, mapped_size, total, kept, kept_size, ok), calls) = with_mock(|reg, ctx| {
        let none = ctx.atom("none").expect("seeded");
        let add = mock_fun(ctx, KEY_PLUS_VALUE);
        let acc = mock_fun(ctx, ACC_KEY_TIMES_VALUE);
        let gt = mock_fun(ctx, VALUE_GT_15);
        let mapped = run_fun_bif(reg, ctx, "map", add, 0, none).expect("maps:map/2");
        let total = run_fun_bif(reg, ctx, "fold", acc, 41, none).expect("maps:fold/3");
        let kept = run_fun_bif(reg, ctx, "filter", gt, 0, none).expect("maps:filter/2");
        let ok = run_fun_bif(reg, ctx, "foreach", add, 0, none).expect("maps:foreach/2");
        (
            pairs(reg, ctx, mapped),
            size(reg, ctx, mapped),
            number(total),
            pairs(reg, ctx, kept),
            size(reg, ctx, kept),
            ok,
        )
    });
    // Oracle: maps:map(_Fun, none) => #{}; maps:fold(_Fun, 41, none) => 41;
    // maps:filter(_Pred, none) => #{}; maps:foreach(_Fun, none) => ok.
    assert!(mapped.is_empty());
    assert_eq!(mapped_size, 0);
    assert_eq!(total, 41);
    assert!(kept.is_empty());
    assert_eq!(kept_size, 0);
    assert_eq!(ok, make_atom(seeded_atoms().lookup("ok").expect("seeded")));
    assert!(calls.is_empty(), "`none` holds no pair to visit");
}

/// `maps:iterator(#{}` is likewise a valid empty iterator (its chain terminates at
/// `none` immediately).
#[test]
fn the_empty_maps_iterator_is_a_valid_empty_iterator() {
    let (total, calls) = with_mock(|reg, ctx| {
        let empty = reg.call("maps", "new", 0, ctx, &[]).expect("maps:new/0");
        let arg = iterator(reg, ctx, empty);
        let acc = mock_fun(ctx, ACC_KEY_TIMES_VALUE);
        let total = run_fun_bif(reg, ctx, "fold", acc, 9, arg).expect("maps:fold/3");
        number(total)
    });
    // Oracle: maps:fold(_Fun, 9, maps:iterator(#{})) => 9.
    assert_eq!(total, 9);
    assert!(calls.is_empty());
}

/// An argument that is neither a map nor a walkable iterator is the structured
/// `error:{badmap, Arg}` naming the **original** argument — never a sub-iterator and
/// never a fabricated empty result. `[]` is not an iterator: only `none` terminates.
#[test]
fn a_non_iterator_argument_is_the_structured_badmap_naming_the_original() {
    let ((), _) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10)]);
        let empty_list = ctx.list(&[]).expect("fits"); // `[]`
        let five = make_small(5); // `5`
        let foo = ctx.atom("foo").expect("seeded");
        let atom_path = ctx.cons(foo, map).expect("fits"); // `[foo | Map]`
        let bad_tail = ctx.cons(make_small(1), five).expect("fits"); // `[1 | 5]`
        let badmap = ctx.atom("badmap").expect("seeded");
        for arg in [empty_list, five, atom_path, bad_tail] {
            let reason = ctx.tuple(&[badmap, arg]).expect("fits");
            for (function, operation) in [
                ("map", KEY_PLUS_VALUE),
                ("fold", ACC_KEY_TIMES_VALUE),
                ("filter", VALUE_GT_15),
                ("foreach", KEY_PLUS_VALUE),
            ] {
                let fun = mock_fun(ctx, operation);
                assert_structured(run_fun_bif(reg, ctx, function, fun, 0, arg), ctx, reason);
            }
        }
    });
}

/// An **improper** ordered path tail (`[[foo | bar] | Map]`) is the same structured
/// `{badmap, Arg}` (`ordered_badarg` in the C, `maps.erl`'s `next/1` `badarg`).
#[test]
fn an_improper_ordered_path_is_the_structured_badmap_naming_the_original() {
    let ((), _) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10), (2, 20)]);
        let foo = ctx.atom("foo").expect("seeded");
        let bar = ctx.atom("bar").expect("seeded");
        let path = ctx.cons(foo, bar).expect("fits"); // `[foo | bar]`
        let arg = ctx.cons(path, map).expect("fits");
        let reason = ctx
            .tuple(&[ctx.atom("badmap").expect("seeded"), arg])
            .expect("fits");
        for (function, operation) in [
            ("map", KEY_PLUS_VALUE),
            ("fold", ACC_KEY_TIMES_VALUE),
            ("filter", VALUE_GT_15),
            ("foreach", KEY_PLUS_VALUE),
        ] {
            let fun = mock_fun(ctx, operation);
            assert_structured(run_fun_bif(reg, ctx, function, fun, 0, arg), ctx, reason);
        }
    });
}

/// The `is_function(Fun, Arity)` clause-head guard still fires **before** the
/// `MapOrIter` argument is looked at, so a bad fun is the plain `error:badarg` even
/// when the second argument is not a map or an iterator either
/// (oracle: `maps:fold(5, 0, not_a_map)` ⇒ `error:badarg`).
#[test]
fn a_bad_fun_still_wins_over_a_bad_iterator_argument() {
    let ((), _) = with_mock(|reg, ctx| {
        let not_an_iterator = make_small(7);
        for function in ["map", "fold", "filter", "foreach"] {
            match run_fun_bif(reg, ctx, function, make_small(5), 0, not_an_iterator) {
                Err(BeamError::BadArg) => {}
                other => panic!("maps:{function} with a non-fun must be badarg, got {other:?}"),
            }
        }
    });
}

/// The fun for a pair runs **before** the `next/1` that yields the following pair
/// (`maps.erl:948`'s `[{K, Fun(K, V)} | map_1(Fun, try_next(Iter, Tag), Tag)]`), so a
/// fun that raises wins over a continuation that cannot be read — and the fun was
/// applied to the pairs before the failure.
#[test]
fn the_fun_runs_before_the_next_walk_step() {
    // A look-ahead `{1, 10, [foo | Map]}` whose continuation is not walkable.
    let ((), calls) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10)]);
        let bogus = ctx.cons(ctx.atom("foo").expect("seeded"), map).expect("fits");
        let iter = ctx
            .tuple(&[make_small(1), make_small(10), bogus])
            .expect("fits");
        let raises = mock_fun(ctx, RAISES);
        match run_fun_bif(reg, ctx, "map", raises, 0, iter) {
            Err(BeamError::BadArg) => {}
            other => panic!("the fun's own raise must win, got {other:?}"),
        }
    });
    assert_eq!(
        calls.len(),
        1,
        "the fun ran for the first pair before the unreadable continuation was walked"
    );

    // The same interleaving witness through `filter/2`'s non-boolean predicate result:
    // the reason is the fun's `{case_clause, 1}`, not the unreadable continuation's
    // `{badmap, _}`.
    let ((), calls) = with_mock(|reg, ctx| {
        let map = int_map(reg, ctx, &[(1, 10)]);
        let bogus = ctx.cons(ctx.atom("foo").expect("seeded"), map).expect("fits");
        let iter = ctx
            .tuple(&[make_small(1), make_small(10), bogus])
            .expect("fits");
        let key = mock_fun(ctx, KEY);
        let expected = ctx
            .tuple(&[ctx.atom("case_clause").expect("seeded"), make_small(1)])
            .expect("fits");
        assert_structured(run_fun_bif(reg, ctx, "filter", key, 0, iter), ctx, expected);
    });
    assert_eq!(calls.len(), 1);
}

/// A >32-key HAMT iterator round trip: `fold/3` over `maps:iterator/1` of a 40-pair
/// map visits every pair exactly once (the 32-pair first chunk plus its tail), and
/// `map/2` over the iterator rebuilds the same map as `map/2` over the map itself.
#[test]
fn fold_over_a_hamt_iterator_visits_every_pair_once() {
    let (total, calls) = with_mock(|reg, ctx| {
        let many: Vec<(i64, i64)> = (1..=40).map(|n| (n, n)).collect();
        let hamt = int_map(reg, ctx, &many);
        let arg = iterator(reg, ctx, hamt);
        let acc = mock_fun(ctx, ACC_KEY_TIMES_VALUE);
        let total = run_fun_bif(reg, ctx, "fold", acc, 0, arg).expect("maps:fold/3");
        number(total)
    });
    // Oracle: maps:fold(fun(K,V,Acc) -> Acc + K*V end, 0, maps:iterator(M)) with
    // M = maps:from_list([{N,N} || N <- lists:seq(1,40)]) => lists:sum([N*N || N <-
    // lists:seq(1,40)]) = 22140.
    assert_eq!(total, 22140);
    assert_eq!(calls.len(), 40, "every pair exactly once");
    assert_eq!(observed(&calls), (1..=40).map(|n| (n, n)).collect::<Vec<_>>());

    let ((via_iter, via_map), _) = with_mock(|reg, ctx| {
        let many: Vec<(i64, i64)> = (1..=40).map(|n| (n, n)).collect();
        let hamt = int_map(reg, ctx, &many);
        let arg = iterator(reg, ctx, hamt);
        let add = mock_fun(ctx, KEY_PLUS_VALUE);
        let mapped_iter = run_fun_bif(reg, ctx, "map", add, 0, arg).expect("maps:map/2");
        let mapped_map = run_fun_bif(reg, ctx, "map", add, 0, hamt).expect("maps:map/2");
        (pairs(reg, ctx, mapped_iter), pairs(reg, ctx, mapped_map))
    });
    assert_eq!(via_iter, via_map, "the iterator must rebuild the same map");
    assert_eq!(via_iter.len(), 40);
    assert_eq!(via_iter[0], (1, 2));
}
