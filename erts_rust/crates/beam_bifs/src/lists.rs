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

//! `lists` module BIFs.
//!
//! The pure subset of `lists.erl` (no `fun` argument): `member/2`, `reverse/1,2`,
//! `keyfind/3`, `append/1,2`, `nth/2`, `nthtail/2`, `last/1`, `sublist/2,3`,
//! `seq/2,3`, `flatten/1,2`, `sum/1`, `max/1`, `min/1`, `duplicate/2`, `delete/2`,
//! `keysort/2`, `sort/1`, `usort/1`, `zip/2`, `unzip/1` — plus the **`fun`-taking**
//! functions `map/2`, `filter/2`, `foldl/3`, `foldr/3`, `foreach/2`, `sort/2`,
//! `usort/2`, `flatmap/2`, `zipwith/3`, `mapfoldl/3`, `mapfoldr/3`, `all/2`,
//! `any/2`, `takewhile/2`, `dropwhile/2`, `partition/2`, `splitwith/2`,
//! `keymap/3`, which call their fun through the [`crate::FunCaller`] seam
//! ([`BifContext::call_fun`]) — see below.
//!
//! Ground truth (read-only): `../../lib/stdlib/src/lists.erl` (the semantics these
//! fast paths reproduce) and the `bif.tab` roster
//! (`../../erts/emulator/beam/bif.tab`:
//! `bif lists:member/2`, `bif lists:reverse/2`, `bif lists:keyfind/3`, …) plus the C
//! implementations in `../../erts/emulator/beam/erl_bif_lists.c`.
//!
//! # The `fun`-taking functions need an evaluator — the `FunCaller` seam
//!
//! A BIF is plain Rust and cannot execute BEAM code, so calling an Erlang fun goes
//! through [`BifContext::call_fun`], which delegates to the runtime's
//! [`crate::FunCaller`]. With no caller installed (Milestone-1 has no in-crate
//! evaluator) every `fun`-taking BIF here raises `unsupported_bif`; the whole
//! module stays usable and the interpreter runs the real `lists.erl` until
//! `beam_rt` installs the seam. A fun's own failures (`badarg`, a `badfun`, a BEAM
//! raise from its body) are propagated verbatim — this module never re-wraps them.
//!
//! The *shape* of each walk is nonetheless pinned to the oracle, because it decides
//! which fun calls happen:
//!
//! * left to right: `map/2`, `foreach/2`, `filter/2`, `foldl/3`, `all/2`, `any/2`,
//!   `takewhile/2`, `dropwhile/2`, `partition/2`, `splitwith/2`, `mapfoldl/3`,
//!   `flatmap/2`, `keymap/3`, `zipwith/3`;
//! * right to left: `foldr/3` (`Fun(Elem, AccIn)`) and `mapfoldr/3`;
//! * `all/2` and `any/2` **short-circuit**: the first `false`/`true` decides without
//!   calling the fun again (`all(P, [1,2,3])` with `P(2) = false` calls it twice);
//! * `takewhile/2`, `dropwhile/2` and `splitwith/2` stop at the first `false` and
//!   never touch the remainder — so an *improper* tail is only an error when the
//!   walk actually reaches it (`lists:takewhile(fun(_) -> false end, [1|2])` ⇒ `[]`,
//!   while `lists:takewhile(fun(_) -> true end, [1|2])` ⇒ an error), exactly as
//!   `takewhile_1`/`dropwhile_1`/`splitwith_1` behave.
//!
//! Every predicate result is read the way `lists.erl` reads it — a `case … of
//! true -> …; false -> … end` — so a **non-boolean** result is an error and not a
//! truthy value: `lists:filter` reports `{bad_filter, 1}` for
//! `fun(X) -> X end`, and `all/2`/`partition/2`/`takewhile/2`/`dropwhile/2` report
//! `case_clause`. Milestone-1 collapses all of these to
//! [`BeamError::BadArg`](beam_common::BeamError::BadArg) (see the errors section).
//!
//! # `sort/2` and `usort/2` — the ordering fun
//!
//! `Fun(A, B)` is the `=<` relation, so it is probed in **both** directions: only
//! `Fun(A,B) = true, Fun(B,A) = false` is a strict order; both-true is equivalence
//! (the stable sort then keeps the input order, `sort(U, [{1,a},{1,b}])` ⇒
//! `[{1,a},{1,b}]`) and so is the degenerate neither-true case (the oracle's own
//! `sort` keeps the input order for `fun(A,B) -> A < B end`). `usort/2` then drops
//! the *later* of each mutually-`Fun`-equivalent run, so a one-directional
//! comparator such as `fun(A,B) -> A < B end` removes nothing. The sort itself is a
//! local **stable merge sort** (not `slice::sort_by`): the comparator can raise, and
//! an Erlang ordering fun is user-supplied, so a degenerate one must not panic the
//! BIF.
//!
//! # Which of these are real BIFs?
//!
//! `bif.tab` only lists **five** `lists` BIFs: `member/2`, `reverse/2`,
//! `keymember/3`, `keysearch/3`, `keyfind/3`. Every other function here is a pure
//! Erlang function in `lists.erl` (exported, not a BIF). This module nonetheless
//! registers all of them under the `lists` module as Rust **fast paths**: the
//! names and arities are verbatim from the `lists.erl` export list, so a loader
//! that resolves them against this registry bypasses the interpreted Erlang with
//! identical observable results.
//!
//! The module also registers the four **list operators** of the `erlang` module —
//! `'++'/2` and `'--'/2` (`bif erlang:'++'/2 ebif_plusplus_2`,
//! `bif erlang:'--'/2 ebif_minusminus_2`) plus their `append/2` and `subtract/2`
//! aliases, which are the *same* C functions (`erl_bif_lists.c`'s `append`/
//! `subtract`) and therefore share one implementation each here.
//!
//! # Matching order — pinned against the OTP 29/erts-17.1 oracle
//!
//! The comparison order is *not* uniform across the module; each function was
//! probed against the oracle and reproduces its exact choice:
//!
//! * **Exact** (`=:=`, [`beam_term::compare::eq`]): `member/2`
//!   (`member(1, [1.0])` ⇒ `false`), `delete/2` (`delete(1.0, [1])` ⇒ `[1]`).
//! * **Arithmetic** (`==`, [`beam_term::compare::cmp_arith`]): `keyfind/3`
//!   (`keyfind(1.0, 1, [{1}])` ⇒ `{1}`), `sort/1`, `usort/1`, `max/1`, `min/1`,
//!   `keysort/2` (`sort([1.0, 1])` ⇒ `[1.0,1]`, `max([1.0,1])` ⇒ `1.0`).
//!
//! `sort/1`, `usort/1` and `keysort/2` are **stable**: elements that compare equal
//! keep their input order (`sort([1.0, 1])` ⇒ `[1.0,1]`, `sort([1, 1.0])` ⇒
//! `[1,1.0]`). `usort/1` removes the *later* of each arithmetically-equal run
//! (`usort([1.0, 1])` ⇒ `[1.0]`, `usort([1, 1.0])` ⇒ `[1]`).
//!
//! # Errors — `function_clause`, `badarg` or `badarith`
//!
//! The oracle distinguishes *argument* failures (`badarg` — e.g.
//! `lists:member(x, [a|b])`, `keysort(1, [b,c])`) from *no-clause* failures
//! (`error:function_clause` — e.g. `lists:nth(3, [a,b])`, `lists:last([])`,
//! `lists:zip([1], [2,3])`, `lists:nthtail(5, [1,2])`, `lists:sublist([1],0,1)`),
//! and this module reproduces both: a no-clause failure is the `function_clause`
//! constructor in `crate::error` (class `error`, the bare atom `function_clause` as
//! the reason — the arguments travel in the stacktrace), everything else is
//! [`BeamError::BadArg`](beam_common::BeamError::BadArg) — never a wrong result.
//! Arithmetic on a non-number (`sum([a])`) is
//! [`BeamError::BadArith`](beam_common::BeamError::BadArith). `sum/1` folds with the
//! `arith` module's exact `+`, so it promotes to a bignum exactly like the oracle; a
//! 60-bit overflow is
//! [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) only where the
//! arithmetic is *not* term arithmetic (`seq/2,3`'s bound check).
//!
//! # `is_list/1` in guards is *shallow*
//!
//! This OTP's `is_list/1` returns `true` for **any cons cell** — `is_list([a|b])`
//! is `true` — so `lists.erl` guards such as `sublist/2`'s or `flatten/2`'s accept
//! improper lists at the top level and only fail when the walk reaches a non-list
//! term. [`erlang_is_list`] reproduces that (`NIL` or a cons); the walk itself
//! then rejects an improper tail, exactly as `lists.erl` does.

use core::cmp::Ordering;

use beam_common::Result;
use beam_term::compare::{cmp_arith, eq};
use beam_term::{atom_val, signed_value, AtomTable, Eterm, NIL};

use crate::access::{
    car, cdr, is_atom, is_list, is_nil, is_small, is_tuple, tuple_element, tuple_size,
};
use crate::context::BifContext;
use crate::error::{function_clause, badarg, system_limit};
use crate::print::proper_list;
use crate::registry::BifRegistry;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The *guard* `is_list/1`: `true` for `NIL` or a cons cell.
///
/// Deliberately shallow — see the module docs. `beam_term::is_list` (wrapped as
/// [`crate::access::is_list`]) is `false` for `NIL` and only checks the outer
/// cell, so `is_nil(x) || is_list(x)` is the exact guard predicate.
#[inline]
fn erlang_is_list(x: Eterm) -> bool {
    is_nil(x) || is_list(x)
}

/// The `i64` value of a small integer argument, or `badarg`.
fn small_int(t: Eterm) -> Result<i64> {
    if is_small(t) {
        Ok(signed_value(t))
    } else {
        Err(badarg())
    }
}

/// Like [`small_int`], but a non-integer argument is the oracle's
/// `function_clause` (`nth`/`nthtail`/`sublist`/`seq`/`duplicate`/`keysort`
/// have no clause for a non-integer index or length).
fn small_int_clause(t: Eterm) -> Result<i64> {
    small_int(t).map_err(|_| function_clause())
}

/// Builds the proper list `elems ++ tail` on the context heap.
///
/// Conses the elements in reverse so a *head*-first cons order reproduces the
/// input order; `tail` may be any term (`lists:append/2` and `lists:reverse/2`
/// both allow a non-list tail).
fn build_list(ctx: &mut BifContext<'_>, elems: &[Eterm], tail: Eterm) -> Result<Eterm> {
    let mut acc = tail;
    for &elem in elems.iter().rev() {
        acc = ctx.cons(elem, acc)?;
    }
    Ok(acc)
}

/// Builds `reverse(elems) ++ tail` on the context heap.
///
/// Conses the elements in input order, which makes the *last* element end up
/// first — the `lists:reverse/2` shape (`reverse([1,2], T)` ⇒ `[2,1|T]`, and
/// `reverse([], T)` ⇒ `T`).
fn reverse_with_tail(ctx: &mut BifContext<'_>, elems: &[Eterm], tail: Eterm) -> Result<Eterm> {
    let mut acc = tail;
    for &elem in elems {
        acc = ctx.cons(elem, acc)?;
    }
    Ok(acc)
}

/// Copies `elems` and orders them by the **arithmetic** term order, stably.
///
/// `slice::sort_by` is stable, which is the `lists:sort/1` contract: two elements
/// equal under `==` keep their input order.
fn sort_by_arith(atoms: &AtomTable, elems: &[Eterm]) -> Vec<Eterm> {
    let mut sorted = elems.to_vec();
    sorted.sort_by(|a, b| cmp_arith(*a, *b, atoms));
    sorted
}

// ---------------------------------------------------------------------------
// Helpers for the `fun`-taking functions (the `FunCaller` seam)
// ---------------------------------------------------------------------------

/// The boolean value of a `true`/`false` **atom**, else `badarg`.
///
/// Every predicate read in this module goes through `case Pred(E) of true -> …;
/// false -> … end`, so a non-boolean result is an error rather than a truthy value
/// (the oracle reports `case_clause` or `{bad_filter, E}`; Milestone-1 reports
/// `badarg` — see the module docs).
fn boolean_result(ctx: &BifContext<'_>, result: Eterm) -> Result<bool> {
    if is_atom(result) {
        match ctx.atoms().name(atom_val(result)) {
            Some("true") => return Ok(true),
            Some("false") => return Ok(false),
            _ => {}
        }
    }
    Err(badarg())
}

/// Calls the predicate `fun` on `args` and reads its boolean result.
fn call_predicate(ctx: &mut BifContext<'_>, fun: Eterm, args: &[Eterm]) -> Result<bool> {
    let result = ctx.call_fun(fun, args)?;
    boolean_result(ctx, result)
}

/// The `Fun(A, B)` ordering of two elements for `sort/2`/`usort/2`.
///
/// `Fun` is the `=<` relation, so it is probed both ways: `true`/`false` is a
/// strict order, both-`true` is equivalence, and the degenerate neither-`true` case
/// is equivalence too (which keeps the input order of elements a one-directional
/// comparator such as `fun(A, B) -> A < B end` considers equal — the oracle's own
/// `sort/2` does the same).
fn fun_order(ctx: &mut BifContext<'_>, fun: Eterm, a: Eterm, b: Eterm) -> Result<Ordering> {
    if call_predicate(ctx, fun, &[a, b])? {
        if call_predicate(ctx, fun, &[b, a])? {
            Ok(Ordering::Equal)
        } else {
            Ok(Ordering::Less)
        }
    } else if call_predicate(ctx, fun, &[b, a])? {
        Ok(Ordering::Greater)
    } else {
        Ok(Ordering::Equal)
    }
}

/// A **stable** bottom-up merge sort with a fallible comparator.
///
/// `slice::sort_by` is unusable here: the comparator calls an Erlang fun (which can
/// raise), and `sort_by` additionally requires a total order — an Erlang ordering
/// fun is user-supplied, so a degenerate one must never panic a BIF. Elements the
/// comparator reports `Equal` keep their input order.
fn merge_sort_elems(
    elems: Vec<Eterm>,
    mut order: impl FnMut(Eterm, Eterm) -> Result<Ordering>,
) -> Result<Vec<Eterm>> {
    let len = elems.len();
    let mut source = elems;
    let mut width = 1;
    while width < len {
        let mut merged = Vec::with_capacity(len);
        let mut start = 0;
        while start < len {
            let middle = (start + width).min(len);
            let end = (start + 2 * width).min(len);
            let (mut left, mut right) = (start, middle);
            while left < middle && right < end {
                // `Greater` means the right run's element must come first; anything
                // else takes the left element, which is what makes the sort stable.
                if order(source[left], source[right])? == Ordering::Greater {
                    merged.push(source[right]);
                    right += 1;
                } else {
                    merged.push(source[left]);
                    left += 1;
                }
            }
            merged.extend_from_slice(&source[left..middle]);
            merged.extend_from_slice(&source[right..end]);
            start = end;
        }
        source = merged;
        width *= 2;
    }
    Ok(source)
}

/// Walks `list` while `fun` holds, returning `(taken, rest)`.
///
/// `rest` is the first sublist whose head fails the predicate (or `NIL`). The walk
/// is **lazy**: it stops at the first `false` and never looks at the remainder, so
/// an improper tail is an error only when the walk actually reaches it — exactly
/// `lists.erl`'s `takewhile_1`/`dropwhile_1`/`splitwith_1`.
fn take_while_prefix(
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    list: Eterm,
) -> Result<(Vec<Eterm>, Eterm)> {
    let mut cur = list;
    let mut taken = Vec::new();
    loop {
        if is_nil(cur) {
            return Ok((taken, NIL));
        }
        if !is_list(cur) {
            return Err(function_clause());
        }
        let elem = car(cur);
        if !call_predicate(ctx, fun, &[elem])? {
            return Ok((taken, cur));
        }
        taken.push(elem);
        cur = cdr(cur);
    }
}

/// Splits the `{Mapped, Acc}` 2-tuple a `mapfoldl`/`mapfoldr` fun must return.
///
/// A fun returning anything else is the oracle's `badmatch`, which Milestone-1
/// reports as [`BeamError::BadArg`](beam_common::BeamError::BadArg).
fn map_fold_pair(result: Eterm) -> Result<(Eterm, Eterm)> {
    if !is_tuple(result) || tuple_size(result) != 2 {
        return Err(badarg());
    }
    Ok((tuple_element(result, 0), tuple_element(result, 1)))
}

/// `setelement(Index, Tuple, Value)` — a **new** tuple with element `Index`
/// (1-based) replaced, or `badarg` when `Tuple` is not a tuple or is too short.
fn replace_element(
    ctx: &mut BifContext<'_>,
    tuple: Eterm,
    index: usize,
    value: Eterm,
) -> Result<Eterm> {
    if !is_tuple(tuple) || index > tuple_size(tuple) {
        return Err(badarg());
    }
    let mut elems: Vec<Eterm> = (0..tuple_size(tuple))
        .map(|i| tuple_element(tuple, i))
        .collect();
    elems[index - 1] = value;
    ctx.tuple(&elems)
}

// ---------------------------------------------------------------------------
// lists:member/2  (a real BIF)
// ---------------------------------------------------------------------------

/// `member(Elem, List)` — `true` iff some element `=:=` `Elem`.
///
/// The list must be proper: a non-list or an improper tail is `badarg` (the C
/// `lists_member_2` reports `badarg` for both).
fn member(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let needle = args[0];
    let mut cur = args[1];
    if is_nil(cur) {
        return ctx.boolean(false);
    }
    while is_list(cur) {
        if eq(car(cur), needle, ctx.atoms()) {
            return ctx.boolean(true);
        }
        cur = cdr(cur);
    }
    if is_nil(cur) {
        ctx.boolean(false)
    } else {
        Err(badarg())
    }
}

// ---------------------------------------------------------------------------
// lists:reverse/1,2  (reverse/2 is a real BIF)
// ---------------------------------------------------------------------------

/// `reverse(List)` — a new list with the elements of `List` in reverse order.
fn reverse1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(badarg)?;
    reverse_with_tail(ctx, &elems, NIL)
}

/// `reverse(List1, Tail)` — `List1` reversed with the arbitrary `Tail` appended.
fn reverse2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let list = args[0];
    let tail = args[1];
    if is_nil(list) {
        return Ok(tail);
    }
    let elems = proper_list(list).ok_or_else(badarg)?;
    reverse_with_tail(ctx, &elems, tail)
}

// ---------------------------------------------------------------------------
// lists:keyfind/3  (a real BIF)
// ---------------------------------------------------------------------------

/// `keyfind(Key, N, TupleList)` — the first tuple whose `N`th element `==` `Key`,
/// else the atom `false`.
///
/// Non-tuple elements are skipped and a tuple shorter than `N` never matches (both
/// pinned against the oracle: `keyfind(x, 1, [a])` ⇒ `false`,
/// `keyfind(x, 2, [{a,10}])` ⇒ `false`). `N` must be a small integer `>= 1` and the
/// list proper, else `badarg`.
fn keyfind(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let key = args[0];
    let pos = small_int(args[1])?;
    if pos < 1 {
        return Err(badarg());
    }
    let pos = pos as usize;
    let mut list = args[2];
    while is_list(list) {
        let elem = car(list);
        list = cdr(list);
        if is_tuple(elem) && pos <= tuple_size(elem) {
            if cmp_arith(key, tuple_element(elem, pos - 1), ctx.atoms()) == Ordering::Equal {
                return Ok(elem);
            }
        }
    }
    if is_nil(list) {
        ctx.boolean(false)
    } else {
        Err(badarg())
    }
}

/// `keymember(Key, N, TupleList)` — `true` iff some tuple's `N`th element `==` `Key`.
///
/// The comparison and skip rules are exactly [`keyfind`]'s: arithmetic order, a
/// non-tuple element is skipped and a tuple shorter than `N` never matches. `N`
/// must be a small integer `>= 1` and the list proper, else `badarg` — but a match
/// short-circuits **before** any improper tail is reached
/// (`keymember(a, 1, [{a}|2])` ⇒ `true`).
fn keymember(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let key = args[0];
    let pos = small_int(args[1])?;
    if pos < 1 {
        return Err(badarg());
    }
    let pos = pos as usize;
    let mut list = args[2];
    while is_list(list) {
        let elem = car(list);
        list = cdr(list);
        if is_tuple(elem) && pos <= tuple_size(elem) {
            if cmp_arith(key, tuple_element(elem, pos - 1), ctx.atoms()) == Ordering::Equal {
                return ctx.boolean(true);
            }
        }
    }
    if is_nil(list) {
        ctx.boolean(false)
    } else {
        Err(badarg())
    }
}

/// `keysearch(Key, N, TupleList)` — `{value, Tuple}` for the first tuple whose
/// `N`th element `==` `Key`, else the atom `false`.
///
/// Same comparison, skip and short-circuit rules as [`keymember`]/[`keyfind`].
fn keysearch(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let key = args[0];
    let pos = small_int(args[1])?;
    if pos < 1 {
        return Err(badarg());
    }
    let pos = pos as usize;
    let mut list = args[2];
    while is_list(list) {
        let elem = car(list);
        list = cdr(list);
        if is_tuple(elem) && pos <= tuple_size(elem) {
            if cmp_arith(key, tuple_element(elem, pos - 1), ctx.atoms()) == Ordering::Equal {
                let tag = ctx.atom("value")?;
                return ctx.tuple(&[tag, elem]);
            }
        }
    }
    if is_nil(list) {
        ctx.boolean(false)
    } else {
        Err(badarg())
    }
}

// ---------------------------------------------------------------------------
// lists:append/1,2
// ---------------------------------------------------------------------------

/// `append(ListOfLists)` — all sublists concatenated.
///
/// `append([])` is `[]`, `append([E])` is `E` (whatever it is), and otherwise
/// `H ++ append(T)` — so every element but the last must be a proper list.
fn append1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(function_clause)?;
    let Some((last, front)) = elems.split_last() else {
        return Ok(NIL);
    };
    if front.is_empty() {
        return Ok(*last);
    }
    let mut acc = *last;
    for &head in front.iter().rev() {
        let head = proper_list(head).ok_or_else(badarg)?;
        acc = build_list(ctx, &head, acc)?;
    }
    Ok(acc)
}

/// `append(List1, List2)` — `List1 ++ List2`.
///
/// `List1` must be a proper list; `List2` is used as the tail verbatim
/// (`append([1,2], 3)` ⇒ `[1,2|3]`, while `append([1|2], [3])` ⇒ `badarg`).
fn append2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(badarg)?;
    build_list(ctx, &elems, args[1])
}

// ---------------------------------------------------------------------------
// The `erlang` list operators: `'++'/2`, `'--'/2` and the `append/2`,
// `subtract/2` aliases
// ---------------------------------------------------------------------------

/// `Lhs ++ Rhs` — `erlang:'++'/2`, and the same implementation is registered as
/// the `erlang:append/2` alias (`bif.tab` and `erl_bif_lists.c:350` both route
/// `append_2` to the very same `append` function).
///
/// `Lhs` must be a proper list, so an improper left operand is `badarg`
/// (`[1|2] ++ [3]` ⇒ `badarg`). `Rhs` is the tail **verbatim** — no check at all
/// — so `[1,2] ++ x` ⇒ `[1,2|x]` and `[] ++ x` ⇒ `x` (`erl_bif_lists.c:270`
/// documents the `[]` case as intentional bug-compatibility).
fn plusplus_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(badarg)?;
    build_list(ctx, &elems, args[1])
}

/// `Lhs -- Rhs` — `erlang:'--'/2`, and the same implementation is registered as
/// the `erlang:subtract/2` alias (`erl_bif_lists.c:1103`).
///
/// Removes from `Lhs` the **first** remaining occurrence of each element of `Rhs`
/// (in `Rhs` order), preserving `Lhs` order; an element of `Rhs` with no match is
/// ignored. Both operands must be proper lists (`[1|2] -- [1]` ⇒ `badarg`).
///
/// The match is **exact** (`=:=`): `erl_bif_lists.c` compares with `EQ` and its
/// red-black-tree path with `CMP_TERM`, documented there as "there is no type
/// coercion so the elements must match exactly". Verified against the oracle:
/// `[1.0] -- [1]` ⇒ `[1.0]` and `[1] -- [1.0]` ⇒ `[1]` (not `[]`).
fn minusminus_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut remaining = proper_list(args[0]).ok_or_else(badarg)?;
    let remove = proper_list(args[1]).ok_or_else(badarg)?;
    for item in remove {
        if let Some(at) = remaining
            .iter()
            .position(|&elem| eq(elem, item, ctx.atoms()))
        {
            remaining.remove(at);
        }
    }
    build_list(ctx, &remaining, NIL)
}

// ---------------------------------------------------------------------------
// lists:nth/2, lists:nthtail/2, lists:last/1
// ---------------------------------------------------------------------------

/// `nth(N, List)` — the `N`th element (1-based) of `List`.
fn nth(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut remaining = small_int_clause(args[0])?;
    if remaining < 1 {
        return Err(function_clause());
    }
    let mut cur = args[1];
    while remaining > 1 {
        if !is_list(cur) {
            return Err(function_clause());
        }
        cur = cdr(cur);
        remaining -= 1;
    }
    if !is_list(cur) {
        return Err(function_clause());
    }
    Ok(car(cur))
}

/// `nthtail(N, List)` — the sublist of `List` from position `N+1` on.
///
/// `nthtail(0, L)` is `L` itself (so a non-list `L` is `function_clause`), and the tail of
/// the `N`th cell is returned verbatim — thus `nthtail(1, [1|2])` ⇒ `2`.
fn nthtail(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let count = small_int_clause(args[0])?;
    if count < 0 {
        return Err(function_clause());
    }
    let mut cur = args[1];
    if count == 0 {
        return if erlang_is_list(cur) {
            Ok(cur)
        } else {
            Err(function_clause())
        };
    }
    let mut remaining = count;
    while remaining > 0 {
        if !is_list(cur) {
            return Err(function_clause());
        }
        cur = cdr(cur);
        remaining -= 1;
    }
    Ok(cur)
}

/// `last(List)` — the final element of a non-empty proper list.
fn last(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut cur = args[0];
    if !is_list(cur) {
        return Err(function_clause());
    }
    loop {
        let rest = cdr(cur);
        if is_nil(rest) {
            return Ok(car(cur));
        }
        if !is_list(rest) {
            return Err(function_clause());
        }
        cur = rest;
    }
}

// ---------------------------------------------------------------------------
// lists:sublist/2,3
// ---------------------------------------------------------------------------

/// The `sublist_2/2` worker: at most `len` elements of `list`, from position 1.
///
/// Iterative form of `sublist_2([H|T], L) when L > 0 -> ...; sublist_2(_, 0) -> [];
/// sublist_2(List, L) when is_list(List), L > 0 -> []` — a non-list term before
/// `len` is reached is `function_clause`.
fn sublist_prefix(ctx: &mut BifContext<'_>, list: Eterm, len: i64) -> Result<Eterm> {
    if len < 0 {
        return Err(function_clause());
    }
    let mut prefix = Vec::new();
    let mut cur = list;
    let mut remaining = len;
    while remaining > 0 {
        if is_list(cur) {
            prefix.push(car(cur));
            cur = cdr(cur);
            remaining -= 1;
        } else if is_nil(cur) {
            break;
        } else {
            return Err(function_clause());
        }
    }
    build_list(ctx, &prefix, NIL)
}

/// `sublist(List, Len)` — the first `Len` elements of `List`.
fn sublist2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let len = small_int_clause(args[1])?;
    if !erlang_is_list(args[0]) {
        return Err(function_clause());
    }
    sublist_prefix(ctx, args[0], len)
}

/// `sublist(List, Start, Len)` — `Len` elements of `List` from position `Start`.
///
/// `Start` must be `>= 1` and `Len` `>= 0`; running off the end of the list is not
/// an error (`sublist([1,2,3,4], 5, 2)` ⇒ `[]`).
fn sublist3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let start = small_int_clause(args[1])?;
    let len = small_int_clause(args[2])?;
    let mut cur = args[0];
    if start == 1 {
        if !erlang_is_list(cur) || len < 0 {
            return Err(function_clause());
        }
        return sublist_prefix(ctx, cur, len);
    }
    if start < 2 {
        return Err(function_clause());
    }
    // Skip `Start - 1` cells; running out of list yields `[]`.
    let mut remaining = start;
    while remaining >= 2 {
        if is_nil(cur) {
            return Ok(NIL);
        }
        if !is_list(cur) {
            return Err(function_clause());
        }
        cur = cdr(cur);
        remaining -= 1;
    }
    if !erlang_is_list(cur) || len < 0 {
        return Err(function_clause());
    }
    sublist_prefix(ctx, cur, len)
}

// ---------------------------------------------------------------------------
// lists:seq/2,3
// ---------------------------------------------------------------------------

/// `seq(From, To)` — `[From, From+1, …, To]`.
fn seq2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let first = small_int_clause(args[0])?;
    let last = small_int_clause(args[1])?;
    // The guard is `From - 1 =< To`; `From` is a 60-bit small, so this cannot
    // overflow `i64`.
    if first - 1 > last {
        return Err(function_clause());
    }
    let mut acc = NIL;
    let mut x = last;
    while x >= first {
        acc = ctx.cons(ctx.int(x)?, acc)?;
        if x == first {
            break;
        }
        x -= 1;
    }
    Ok(acc)
}

/// `seq(From, To, Incr)` — the arithmetic sequence from `From` to `To`.
fn seq3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (from, to, incr) = if is_small(args[0]) && is_small(args[1]) && is_small(args[2]) {
        (
            Some(signed_value(args[0])),
            Some(signed_value(args[1])),
            Some(signed_value(args[2])),
        )
    } else {
        (None, None, None)
    };
    if let (Some(from), Some(to), Some(incr)) = (from, to, incr) {
        let ascending = incr > 0 && from - incr <= to;
        let descending = incr < 0 && from - incr >= to;
        if ascending || descending {
            // `N = (To - From + Incr) div Incr` — truncation toward zero, exactly
            // as Erlang's `div` (and Rust's `/` for integers) does.
            let count = (to - from + incr) / incr;
            return seq_loop(ctx, count, incr, from);
        }
        if incr == 0 && from == to {
            // `seq(Same, Same, 0) -> [Same]`.
            return ctx.list(&[args[0]]);
        }
    }
    Err(badarg())
}

/// The `seq_loop/4` worker: `N` terms from `first`, stepping by `incr`.
fn seq_loop(ctx: &mut BifContext<'_>, count: i64, incr: i64, first: i64) -> Result<Eterm> {
    if count <= 0 {
        return Ok(NIL);
    }
    let mut x = incr
        .checked_mul(count - 1)
        .and_then(|v| v.checked_add(first))
        .ok_or_else(system_limit)?;
    let mut acc = NIL;
    let mut remaining = count;
    while remaining > 0 {
        acc = ctx.cons(ctx.int(x)?, acc)?;
        remaining -= 1;
        if remaining == 0 {
            break;
        }
        x = x.checked_sub(incr).ok_or_else(system_limit)?;
    }
    Ok(acc)
}

// ---------------------------------------------------------------------------
// lists:flatten/1,2
// ---------------------------------------------------------------------------

/// Collects the non-list leaves of `root` in traversal order.
///
/// An explicit work stack rather than recursion, so a deeply nested list cannot
/// overflow the Rust stack (`lists:flatten` is depth-unbounded in the oracle: the
/// BEAM stack grows). A non-list term reached before the traversal finishes is
/// `function_clause`, matching `do_flatten/2`'s missing clause.
fn flatten_leaves(root: Eterm) -> Result<Vec<Eterm>> {
    let mut leaves = Vec::new();
    let mut work = vec![root];
    while let Some(term) = work.pop() {
        if is_nil(term) {
            continue;
        }
        if !is_list(term) {
            return Err(function_clause());
        }
        let head = car(term);
        // The tail is visited *after* the head, so push it first.
        work.push(cdr(term));
        if erlang_is_list(head) {
            work.push(head);
        } else {
            leaves.push(head);
        }
    }
    Ok(leaves)
}

/// `flatten(DeepList)` — all levels of `DeepList` spliced into one list.
fn flatten1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let leaves = flatten_leaves(args[0])?;
    build_list(ctx, &leaves, NIL)
}

/// `flatten(DeepList, Tail)` — as above, with the arbitrary `Tail` appended.
fn flatten2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !erlang_is_list(args[0]) || !erlang_is_list(args[1]) {
        return Err(function_clause());
    }
    let leaves = flatten_leaves(args[0])?;
    build_list(ctx, &leaves, args[1])
}

// ---------------------------------------------------------------------------
// lists:sum/1, lists:max/1, lists:min/1
// ---------------------------------------------------------------------------

/// `sum(List)` — the fold `Sum + H` over a proper list of numbers (`sum([])` ⇒ `0`).
///
/// The addition is [`crate::arith::add_terms`] — the very `+` the oracle's
/// `lists.erl` folds with — so an integer sum that leaves the 60-bit small range
/// **promotes to a bignum**, exactly as real Erlang does
/// (`lists:sum([576460752303423487, 1])` ⇒ `576460752303423488`), and a bignum
/// *element* is summed exactly.
///
/// # Errors
///
/// [`BeamError::BadArith`](beam_common::BeamError::BadArith) for a non-number
/// (`sum([a])` ⇒ `badarith`), and `function_clause` when `List` is not a proper list.
fn sum(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut acc = ctx.int(0)?;
    let mut cur = args[0];
    while is_list(cur) {
        acc = crate::arith::add_terms(ctx, acc, car(cur))?;
        cur = cdr(cur);
    }
    if !is_nil(cur) {
        return Err(function_clause());
    }
    Ok(acc)
}

/// Walks a non-empty proper list, keeping the element that beats the running
/// extreme. `better(candidate, best)` decides whether the candidate replaces it.
fn extreme(ctx: &mut BifContext<'_>, list: Eterm, better: fn(Ordering) -> bool) -> Result<Eterm> {
    if !is_list(list) {
        return Err(function_clause());
    }
    let mut best = car(list);
    let mut cur = cdr(list);
    while is_list(cur) {
        let candidate = car(cur);
        if better(cmp_arith(candidate, best, ctx.atoms())) {
            best = candidate;
        }
        cur = cdr(cur);
    }
    if !is_nil(cur) {
        return Err(function_clause());
    }
    Ok(best)
}

/// `max(List)` — the first element `>=` all others (arithmetic order).
fn max(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    extreme(ctx, args[0], |order| order == Ordering::Greater)
}

/// `min(List)` — the first element `<=` all others (arithmetic order).
fn min(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    extreme(ctx, args[0], |order| order == Ordering::Less)
}

// ---------------------------------------------------------------------------
// lists:duplicate/2, lists:delete/2
// ---------------------------------------------------------------------------

/// `duplicate(N, Elem)` — a list of `N` copies of `Elem`.
///
/// A count past the heap's capacity surfaces as the heap's allocation error
/// (there is no separate `system_limit` guard, mirroring the engine's reset-and-
/// retry contract for term allocation).
fn duplicate(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let count = small_int_clause(args[0])?;
    if count < 0 {
        return Err(function_clause());
    }
    let mut acc = NIL;
    for _ in 0..count {
        acc = ctx.cons(args[1], acc)?;
    }
    Ok(acc)
}

/// `delete(Elem, List)` — `List` with the first element `=:=` `Elem` removed.
///
/// The removed position returns its *tail* verbatim (`delete(a, [a|b])` ⇒ `b`),
/// exactly as `delete(Item, [Item|Rest]) -> Rest` does.
fn delete(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let item = args[0];
    let mut cur = args[1];
    let mut prefix = Vec::new();
    loop {
        if is_nil(cur) {
            return build_list(ctx, &prefix, NIL);
        }
        if !is_list(cur) {
            return Err(function_clause());
        }
        let head = car(cur);
        if eq(head, item, ctx.atoms()) {
            let rest = cdr(cur);
            return build_list(ctx, &prefix, rest);
        }
        prefix.push(head);
        cur = cdr(cur);
    }
}

// ---------------------------------------------------------------------------
// lists:keysort/2, lists:sort/1, lists:usort/1
// ---------------------------------------------------------------------------

/// `element(N, Tuple)` for a keysort key, or `badarg` when the term is not a
/// tuple with at least `n` elements.
fn key_element(term: Eterm, n: usize) -> Result<Eterm> {
    if !is_tuple(term) || n > tuple_size(term) {
        return Err(badarg());
    }
    Ok(tuple_element(term, n - 1))
}

/// `keysort(N, TupleList)` — a stable sort on the `N`th element of each tuple.
///
/// Lists of fewer than two elements are returned unchanged *without* touching a
/// key (so `keysort(2, [{b}])` ⇒ `[{b}]`), matching `lists.erl`'s single-element
/// shortcut.
fn keysort(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let index = small_int_clause(args[0])?;
    if index < 1 {
        return Err(function_clause());
    }
    let index = index as usize;
    let list = args[1];
    let elems = proper_list(list).ok_or_else(badarg)?;
    if elems.len() < 2 {
        return Ok(list);
    }
    let mut keys = Vec::with_capacity(elems.len());
    for &elem in &elems {
        keys.push(key_element(elem, index)?);
    }
    let sorted = {
        let atoms = ctx.atoms();
        let mut order: Vec<usize> = (0..elems.len()).collect();
        order.sort_by(|&a, &b| cmp_arith(keys[a], keys[b], atoms));
        order.into_iter().map(|k| elems[k]).collect::<Vec<Eterm>>()
    };
    ctx.list(&sorted)
}

/// `sort(List)` — a stable sort in the arithmetic term order.
fn sort(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(function_clause)?;
    let sorted = sort_by_arith(ctx.atoms(), &elems);
    ctx.list(&sorted)
}

/// `usort(List)` — `sort/1` with arithmetically-equal neighbours collapsed.
fn usort(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(function_clause)?;
    let sorted = sort_by_arith(ctx.atoms(), &elems);
    let mut unique: Vec<Eterm> = Vec::with_capacity(sorted.len());
    for &elem in &sorted {
        let duplicate = match unique.last() {
            Some(&previous) => cmp_arith(previous, elem, ctx.atoms()) == Ordering::Equal,
            None => false,
        };
        if !duplicate {
            unique.push(elem);
        }
    }
    ctx.list(&unique)
}

// ---------------------------------------------------------------------------
// lists:zip/2, lists:unzip/1
// ---------------------------------------------------------------------------

/// `zip(List1, List2)` — `{X, Y}` pairs; both lists must have equal length.
fn zip(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut left = args[0];
    let mut right = args[1];
    let mut pairs = Vec::new();
    loop {
        match (is_list(left), is_list(right)) {
            (true, true) => {
                pairs.push(ctx.tuple(&[car(left), car(right)])?);
                left = cdr(left);
                right = cdr(right);
            }
            (false, false) if is_nil(left) && is_nil(right) => break,
            _ => return Err(function_clause()),
        }
    }
    ctx.list(&pairs)
}

/// `unzip(List)` — splits a list of 2-tuples into `{Firsts, Seconds}`.
fn unzip(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut cur = args[0];
    let mut firsts = Vec::new();
    let mut seconds = Vec::new();
    while is_list(cur) {
        let elem = car(cur);
        cur = cdr(cur);
        if !is_tuple(elem) || tuple_size(elem) != 2 {
            return Err(function_clause());
        }
        firsts.push(tuple_element(elem, 0));
        seconds.push(tuple_element(elem, 1));
    }
    if !is_nil(cur) {
        return Err(function_clause());
    }
    let firsts = ctx.list(&firsts)?;
    let seconds = ctx.list(&seconds)?;
    ctx.tuple(&[firsts, seconds])
}

// ---------------------------------------------------------------------------
// The `fun`-taking functions (via the `FunCaller` seam)
// ---------------------------------------------------------------------------

/// `map(Fun, List)` — `Fun` applied to every element, left to right.
///
/// # Errors
///
/// `badarg` when `List` is not a proper list (the oracle raises `case_clause` for a
/// non-list and `function_clause` for an improper tail); otherwise propagates
/// whatever `Fun` raises through [`BifContext::call_fun`].
fn map2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let elems = proper_list(args[1]).ok_or_else(badarg)?;
    let mut mapped = Vec::with_capacity(elems.len());
    for elem in elems {
        mapped.push(ctx.call_fun(fun, &[elem])?);
    }
    build_list(ctx, &mapped, NIL)
}

/// `foreach(Fun, List)` — `Fun` called for its side effects, left to right; `ok`.
fn foreach2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let elems = proper_list(args[1]).ok_or_else(function_clause)?;
    for elem in elems {
        ctx.call_fun(fun, &[elem])?;
    }
    ctx.ok()
}

/// `filter(Pred, List)` — the elements for which `Pred` returns `true`, in order.
fn filter2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let elems = proper_list(args[1]).ok_or_else(badarg)?;
    let mut kept = Vec::new();
    for elem in elems {
        if call_predicate(ctx, fun, &[elem])? {
            kept.push(elem);
        }
    }
    build_list(ctx, &kept, NIL)
}

/// `foldl(Fun, Acc0, List)` — `Acc = Fun(Elem, Acc)`, left to right.
fn foldl3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut acc = args[1];
    for elem in proper_list(args[2]).ok_or_else(badarg)? {
        acc = ctx.call_fun(fun, &[elem, acc])?;
    }
    Ok(acc)
}

/// `foldr(Fun, Acc0, List)` — `Acc = Fun(Elem, Acc)`, **right to left**.
fn foldr3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut acc = args[1];
    for &elem in proper_list(args[2]).ok_or_else(function_clause)?.iter().rev() {
        acc = ctx.call_fun(fun, &[elem, acc])?;
    }
    Ok(acc)
}

/// `all(Pred, List)` — whether `Pred` holds for every element.
///
/// **Short-circuits**: the first `false` decides and no further element is visited.
/// The walk is lazy for the same reason, so — like `all/2` in `lists.erl` — it can
/// answer before an improper tail is ever reached.
fn all2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut cur = args[1];
    while is_list(cur) {
        let elem = car(cur);
        if !call_predicate(ctx, fun, &[elem])? {
            return ctx.boolean(false);
        }
        cur = cdr(cur);
    }
    if !is_nil(cur) {
        return Err(badarg());
    }
    ctx.boolean(true)
}

/// `any(Pred, List)` — whether `Pred` holds for some element.
///
/// **Short-circuits**: the first `true` decides and no further element is visited.
fn any2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut cur = args[1];
    while is_list(cur) {
        let elem = car(cur);
        if call_predicate(ctx, fun, &[elem])? {
            return ctx.boolean(true);
        }
        cur = cdr(cur);
    }
    if !is_nil(cur) {
        return Err(badarg());
    }
    ctx.boolean(false)
}

/// `flatmap(Fun, List)` — `Fun(Elem)` (a list) applied to every element, the
/// results concatenated left to right.
///
/// # Errors
///
/// `function_clause` when `List` is not proper, or `badarg` when a `Fun` result is
/// not a proper list (`List ++ Rest` is `badarg` in the oracle).
fn flatmap2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let elems = proper_list(args[1]).ok_or_else(function_clause)?;
    let mut flat: Vec<Eterm> = Vec::new();
    for elem in elems {
        let result = ctx.call_fun(fun, &[elem])?;
        let sub = proper_list(result).ok_or_else(badarg)?;
        flat.extend_from_slice(&sub);
    }
    build_list(ctx, &flat, NIL)
}

/// `takewhile(Pred, List)` — the longest prefix whose elements satisfy `Pred`.
fn takewhile2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (taken, _rest) = take_while_prefix(ctx, args[0], args[1])?;
    build_list(ctx, &taken, NIL)
}

/// `dropwhile(Pred, List)` — `List` with its longest `Pred`-satisfying prefix
/// removed (the remainder is returned as-is, sharing its tail).
fn dropwhile2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (_taken, rest) = take_while_prefix(ctx, args[0], args[1])?;
    Ok(rest)
}

/// `partition(Pred, List)` — `{Satisfied, NotSatisfied}`, both order-preserving.
fn partition2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let elems = proper_list(args[1]).ok_or_else(function_clause)?;
    let mut satisfied = Vec::new();
    let mut not_satisfied = Vec::new();
    for elem in elems {
        if call_predicate(ctx, fun, &[elem])? {
            satisfied.push(elem);
        } else {
            not_satisfied.push(elem);
        }
    }
    let satisfied = build_list(ctx, &satisfied, NIL)?;
    let not_satisfied = build_list(ctx, &not_satisfied, NIL)?;
    ctx.tuple(&[satisfied, not_satisfied])
}

/// `splitwith(Pred, List)` — `{takewhile(Pred, List), dropwhile(Pred, List)}` in a
/// single pass (`splitwith_1`).
fn splitwith2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (taken, rest) = take_while_prefix(ctx, args[0], args[1])?;
    let taken = build_list(ctx, &taken, NIL)?;
    ctx.tuple(&[taken, rest])
}

/// `mapfoldl(Fun, Acc0, List)` — `{Mapped, Acc}` with `{M, Acc1} = Fun(Elem, Acc)`
/// applied left to right; the mapped values are in element order.
fn mapfoldl3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut acc = args[1];
    let elems = proper_list(args[2]).ok_or_else(function_clause)?;
    let mut mapped = Vec::with_capacity(elems.len());
    for elem in elems {
        let (mapped_elem, next_acc) = map_fold_pair(ctx.call_fun(fun, &[elem, acc])?)?;
        mapped.push(mapped_elem);
        acc = next_acc;
    }
    let mapped = build_list(ctx, &mapped, NIL)?;
    ctx.tuple(&[mapped, acc])
}

/// `mapfoldr(Fun, Acc0, List)` — `{Mapped, Acc}` with `Fun(Elem, Acc)` applied
/// **right to left**; the mapped values are still returned in *element* order
/// (`mapfoldr_1` conses without reversing its accumulator).
fn mapfoldr3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut acc = args[1];
    let elems = proper_list(args[2]).ok_or_else(function_clause)?;
    let mut mapped = vec![NIL; elems.len()];
    for (index, &elem) in elems.iter().enumerate().rev() {
        let (mapped_elem, next_acc) = map_fold_pair(ctx.call_fun(fun, &[elem, acc])?)?;
        mapped[index] = mapped_elem;
        acc = next_acc;
    }
    let mapped = build_list(ctx, &mapped, NIL)?;
    ctx.tuple(&[mapped, acc])
}

/// `zipwith(Fun, List1, List2)` — `[Fun(X, Y) | …]`, element by element.
///
/// The two lists must have the **same length**: `zipwith/3` is
/// `zipwith(F, Xs, Ys, fail)` in `lists.erl`, whose `fail` mode has no clause for
/// a length mismatch, so `zipwith(F, [1,2,3], [10,20])` raises `function_clause`
/// exactly like `zip/2`. The `trim`/`{pad, …}` behaviours live in `zipwith/4`, which
/// this module does not register.
fn zipwith3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let mut left = args[1];
    let mut right = args[2];
    let mut zipped = Vec::new();
    loop {
        match (is_list(left), is_list(right)) {
            (true, true) => {
                zipped.push(ctx.call_fun(fun, &[car(left), car(right)])?);
                left = cdr(left);
                right = cdr(right);
            }
            (false, false) if is_nil(left) && is_nil(right) => break,
            _ => return Err(function_clause()),
        }
    }
    build_list(ctx, &zipped, NIL)
}

/// `sort(Fun, List)` — a **stable** sort in the `Fun` (`=<`) order.
fn sort2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let elems = proper_list(args[1]).ok_or_else(function_clause)?;
    let sorted = merge_sort_elems(elems, |a, b| fun_order(ctx, fun, a, b))?;
    ctx.list(&sorted)
}

/// `usort(Fun, List)` — the oracle's **exact** `usort/2` algorithm.
///
/// `lists.erl` implements `usort/2` in pure Erlang as a bottom-up merge sort whose
/// runs come from the `ufsplit_1`/`ufsplit_1_1`/`ufsplit_2` family and are merged by
/// `ufmergel`/`rufmergel`/`ufmerge2_*`/`rufmerge2_*`. The run structure and which of
/// two `Fun`-equivalent elements a merge keeps are *observable* for a degenerate
/// (non-total) comparator — `lists:usort(fun(_, _) -> false end, [1,2,3])` is
/// `[3,2,1]` and `lists:usort(fun(_, _) -> true end, [1,2,3])` is `[1]` — so the
/// sort-then-collapse-adjacent-equivalents shape of `usort/1`'s path does not
/// reproduce it. This is a faithful port (validated case-for-case against the oracle
/// over ~10 700 comparator/list combinations, degenerate comparators included).
///
/// The Erlang is tail-recursive; each family is an explicit loop here, and every
/// `Fun` call happens in the oracle's own order. List values are `Vec<Eterm>` in
/// Erlang order (index 0 is the head); the `M`/`R` accumulators the Erlang builds by
/// prepending are held *reversed* so the prepend is a `push`.
fn usort2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let elems = proper_list(args[1]).ok_or_else(function_clause)?;
    let sorted = usort_1_entry(ctx, fun, &elems)?;
    ctx.list(&sorted)
}

/// `usort/2`'s head clauses (`[_]`/`[]` are returned as they are) and its
/// `usort_1(Fun, X, L)` call.
fn usort_1_entry(ctx: &mut BifContext<'_>, fun: Eterm, list: &[Eterm]) -> Result<Vec<Eterm>> {
    if list.len() < 2 {
        return Ok(list.to_vec());
    }
    // `usort_1/3`, iterative because `usort_1(Fun, X, L)` is tail-recursive.
    let x = list[0];
    let mut at = 1;
    loop {
        let y = list[at];
        if call_predicate(ctx, fun, &[x, y])? {
            if call_predicate(ctx, fun, &[y, x])? {
                // `X` equal to `Y`: drop `Y` and continue with the rest.
                at += 1;
                if at == list.len() {
                    return Ok(vec![x]);
                }
            } else {
                // `ufsplit_1(Y, X, Fun, L, [], [])`.
                return ufsplit_1(ctx, fun, y, x, &list[at + 1..], Vec::new(), Vec::new(), None);
            }
        } else {
            // `ufsplit_2(Y, L, Fun, [X])`.
            return ufsplit_2(ctx, fun, y, &list[at + 1..], vec![x]);
        }
    }
}

/// The Erlang run `[Y, X | R]` from `r`, which holds `R` reversed.
fn run_head_pair(r: &[Eterm], x: Eterm, y: Eterm) -> Vec<Eterm> {
    let mut run = Vec::with_capacity(r.len() + 2);
    run.push(y);
    run.push(x);
    run.extend(r.iter().rev().copied());
    run
}

/// The Erlang list `R` from `r`, which holds `R` reversed.
fn rev_erl(r: &[Eterm]) -> Vec<Eterm> {
    r.iter().rev().copied().collect()
}

/// The C `lists:reverse(List, M)` where `m` holds `M` reversed.
fn rev_append_tail(list: &[Eterm], m: &[Eterm]) -> Vec<Eterm> {
    let mut out: Vec<Eterm> = list.iter().rev().copied().collect();
    out.extend(m.iter().rev().copied());
    out
}

/// `ufsplit_1/6` **and** its `ufsplit_1_1/7` continuation as one loop (`stage_two`
/// is `Some(S)` in the `ufsplit_1_1` state and `None` in `ufsplit_1`).
///
/// # Errors
///
/// Propagates whatever `Fun` raises through [`BifContext::call_fun`].
fn ufsplit_1(
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    mut y: Eterm,
    mut x: Eterm,
    list: &[Eterm],
    mut r: Vec<Eterm>,
    mut rs: Vec<Vec<Eterm>>,
    mut stage_two: Option<Eterm>,
) -> Result<Vec<Eterm>> {
    let mut at = 0;
    loop {
        if at < list.len() {
            let z = list[at];
            at += 1;
            if call_predicate(ctx, fun, &[y, z])? {
                if call_predicate(ctx, fun, &[z, y])? {
                    // `Z` equal to `Y`: neither advances.
                } else {
                    // `Y` < `Z`: `ufsplit_1(Z, Y, L, [X | R], Rs)` — the pair
                    // shifts: new `Y` is `Z`, new `X` is the old `Y`, and the old
                    // `X` is prepended to `R`. Omitting `x = y` duplicates `X`.
                    r.push(x);
                    x = y;
                    y = z;
                }
            } else if call_predicate(ctx, fun, &[x, z])? {
                if call_predicate(ctx, fun, &[z, x])? {
                    // `Z` equal to `X`.
                } else {
                    // `Z` < `X`: `ufsplit_1(Y, Z, L, [X | R], Rs)`.
                    r.push(x);
                    x = z;
                }
            } else if let Some(s) = stage_two {
                // `ufsplit_1_1/7`: the run needs a third element.
                if call_predicate(ctx, fun, &[s, z])? {
                    if call_predicate(ctx, fun, &[z, s])? {
                        // `Z` equal to `S`.
                    } else {
                        // `ufsplit_1(Z, S, L, [], [[Y, X | R] | Rs])`.
                        let run = run_head_pair(&r, x, y);
                        rs.insert(0, run);
                        y = z;
                        x = s;
                        r = Vec::new();
                        stage_two = None;
                    }
                } else {
                    // `ufsplit_1(S, Z, L, [], [[Y, X | R] | Rs])`.
                    let run = run_head_pair(&r, x, y);
                    rs.insert(0, run);
                    y = s;
                    x = z;
                    r = Vec::new();
                    stage_two = None;
                }
            } else if r.is_empty() {
                // `false when R == [] -> ufsplit_1(Y, X, L, [Z], Rs)`.
                r = vec![z];
            } else {
                // `ufsplit_1_1(Y, X, L, R, Rs, Z)`.
                stage_two = Some(z);
            }
        } else if let Some(s) = stage_two {
            // `rufmergel([[S], [Y, X | R] | Rs], [], Fun)`.
            let run = run_head_pair(&r, x, y);
            let mut runs = vec![vec![s], run];
            runs.append(&mut rs);
            return rufmergel(ctx, fun, runs, Vec::new());
        } else {
            // `rufmergel([[Y, X | R] | Rs], [], Fun)`.
            let run = run_head_pair(&r, x, y);
            let mut runs = vec![run];
            runs.append(&mut rs);
            return rufmergel(ctx, fun, runs, Vec::new());
        }
    }
}

/// `ufsplit_2/4` — the descending split (`Fun(Y, Z)` false keeps `Y`).
///
/// # Errors
///
/// Propagates whatever `Fun` raises through [`BifContext::call_fun`].
fn ufsplit_2(
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    mut y: Eterm,
    list: &[Eterm],
    mut r: Vec<Eterm>,
) -> Result<Vec<Eterm>> {
    let mut at = 0;
    loop {
        if at < list.len() {
            let z = list[at];
            at += 1;
            if call_predicate(ctx, fun, &[y, z])? {
                if call_predicate(ctx, fun, &[z, y])? {
                    // `Z` equal to `Y`: keep `Y`.
                } else {
                    // `ufsplit_1(Z, Y, Fun, L, [], [lists:reverse(R, [])])`. `r`
                    // already holds `R` reversed, so `lists:reverse(R)` is `r`.
                    return ufsplit_1(ctx, fun, z, y, &list[at..], Vec::new(), vec![r], None);
                }
            } else {
                r.push(y);
                y = z;
            }
        } else {
            // `ufsplit_2(Y, [], _Fun, R) -> [Y | R]`.
            r.push(y);
            return Ok(rev_erl(&r));
        }
    }
}

/// `ufmergel/3` and `rufmergel/3` run in opposite directions, so they alternate:
/// each consumes pairs of runs into `acc` and then hands the (halved) run list to
/// the other, with `lists:reverse(L, [])` at every hand-off.
///
/// # Errors
///
/// Propagates whatever `Fun` raises through [`BifContext::call_fun`].
fn ufmergel(
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    mut runs: Vec<Vec<Eterm>>,
    mut acc: Vec<Vec<Eterm>>,
) -> Result<Vec<Eterm>> {
    loop {
        if runs.len() >= 2 {
            let first = runs.remove(0);
            let second = runs.remove(0);
            let merged = ufmerge2_2(ctx, fun, first[0], &first[1..], &second)?;
            acc.insert(0, merged);
        } else if runs.len() == 1 {
            let last = runs.pop().expect("one run remains");
            if acc.is_empty() {
                // `ufmergel([L], [], _Fun) -> L`.
                return Ok(last);
            }
            // `ufmergel([L], Acc, Fun) -> rufmergel([lists:reverse(L) | Acc], [], Fun)`.
            acc.insert(0, rev_erl(&last));
            return rufmergel(ctx, fun, acc, Vec::new());
        } else {
            // `ufmergel([], Acc, Fun) -> rufmergel(Acc, [], Fun)`.
            return rufmergel(ctx, fun, acc, Vec::new());
        }
    }
}

/// See [`ufmergel`].
///
/// # Errors
///
/// Propagates whatever `Fun` raises through [`BifContext::call_fun`].
fn rufmergel(
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    mut runs: Vec<Vec<Eterm>>,
    mut acc: Vec<Vec<Eterm>>,
) -> Result<Vec<Eterm>> {
    loop {
        if runs.len() >= 2 {
            let first = runs.remove(0);
            let head = first[0];
            let tail = first[1..].to_vec();
            let second = runs.remove(0);
            let merged = rufmerge2_1(ctx, fun, &second, head, &tail, Vec::new())?;
            acc.insert(0, merged);
        } else if runs.len() == 1 {
            let last = runs.pop().expect("one run remains");
            acc.insert(0, rev_erl(&last));
            return ufmergel(ctx, fun, acc, Vec::new());
        } else {
            return ufmergel(ctx, fun, acc, Vec::new());
        }
    }
}

/// `ufmerge2_2/5` and `ufmerge2_1/6` as one loop: `State::Two(h1)` is
/// `ufmerge2_2(H1, T1, T2, M)` and `State::One(h2, hdm)` is
/// `ufmerge2_1(T1, H2, T2, M, HdM)`. Elements from the first run are kept and a
/// `Fun`-equivalent `H2` is dropped (`HdM` names the last element kept).
///
/// # Errors
///
/// Propagates whatever `Fun` raises through [`BifContext::call_fun`].
fn ufmerge2_2(
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    h1: Eterm,
    t1: &[Eterm],
    t2: &[Eterm],
) -> Result<Vec<Eterm>> {
    /// Which of the two mutually tail-recursive functions we are in.
    enum State {
        /// `ufmerge2_2(H1, …)`.
        Two(Eterm),
        /// `ufmerge2_1(…, H2, …, HdM)`.
        One(Eterm, Eterm),
    }
    let mut m: Vec<Eterm> = Vec::new();
    let mut at1 = 0;
    let mut at2 = 0;
    let mut state = State::Two(h1);
    loop {
        match state {
            State::Two(current) => {
                if let Some(&h2) = t2.get(at2) {
                    at2 += 1;
                    if call_predicate(ctx, fun, &[current, h2])? {
                        m.push(current);
                        state = State::One(h2, current);
                    } else {
                        m.push(h2);
                        state = State::Two(current);
                    }
                } else {
                    // `lists:reverse(T1, [H1 | M])`.
                    m.push(current);
                    return Ok(rev_append_tail(&t1[at1..], &m));
                }
            }
            State::One(h2, hdm) => {
                if let Some(&h1n) = t1.get(at1) {
                    at1 += 1;
                    if call_predicate(ctx, fun, &[h1n, h2])? {
                        m.push(h1n);
                        state = State::One(h2, h1n);
                    } else if call_predicate(ctx, fun, &[h2, hdm])? {
                        // `HdM` equal to `H2`: drop `H2`.
                        state = State::Two(h1n);
                    } else {
                        m.push(h2);
                        state = State::Two(h1n);
                    }
                } else {
                    if !call_predicate(ctx, fun, &[h2, hdm])? {
                        m.push(h2);
                    }
                    return Ok(rev_append_tail(&t2[at2..], &m));
                }
            }
        }
    }
}

/// `rufmerge2_1/5` and `rufmerge2_2/6` as one loop: the descending merge (used when
/// the runs were collected while `Fun` reported descending order).
///
/// # Errors
///
/// Propagates whatever `Fun` raises through [`BifContext::call_fun`].
fn rufmerge2_1(
    ctx: &mut BifContext<'_>,
    fun: Eterm,
    t1: &[Eterm],
    h2: Eterm,
    t2: &[Eterm],
    m: Vec<Eterm>,
) -> Result<Vec<Eterm>> {
    /// Which of the two mutually tail-recursive functions we are in.
    enum State {
        /// `rufmerge2_1(…, H2, …)`.
        One(Eterm),
        /// `rufmerge2_2(H1, …, H2M)`.
        Two(Eterm, Eterm),
    }
    let mut m = m;
    let mut at1 = 0;
    let mut at2 = 0;
    let mut state = State::One(h2);
    loop {
        match state {
            State::One(h2c) => {
                if let Some(&h1) = t1.get(at1) {
                    at1 += 1;
                    if call_predicate(ctx, fun, &[h1, h2c])? {
                        state = State::Two(h1, h2c);
                    } else {
                        m.push(h1);
                        state = State::One(h2c);
                    }
                } else {
                    // `lists:reverse(T2, [H2 | M])`.
                    m.push(h2c);
                    return Ok(rev_append_tail(&t2[at2..], &m));
                }
            }
            State::Two(h1, h2m) => {
                if let Some(&h2c) = t2.get(at2) {
                    at2 += 1;
                    if call_predicate(ctx, fun, &[h1, h2c])? {
                        m.push(h2m);
                        state = State::Two(h1, h2c);
                    } else if call_predicate(ctx, fun, &[h2m, h1])? {
                        // `H2M` equal to `H1`: drop `H2M`.
                        m.push(h1);
                        state = State::One(h2c);
                    } else {
                        m.push(h2m);
                        m.push(h1);
                        state = State::One(h2c);
                    }
                } else {
                    if call_predicate(ctx, fun, &[h2m, h1])? {
                        m.push(h1);
                    } else {
                        m.push(h2m);
                        m.push(h1);
                    }
                    return Ok(rev_append_tail(&t1[at1..], &m));
                }
            }
        }
    }
}

/// `keymap(Fun, Index, TupleList)` — `Fun` applied to the `Index`th element of
/// every tuple, the result replacing it.
///
/// # Errors
///
/// `badarg` when `TupleList` is not a proper list, or an element is not a tuple
/// with at least `Index` elements (`element/2` runs *before* `Fun`, so the fun is
/// never called for such an element), or `Index` is not a small integer `>= 1`.
fn keymap3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let fun = args[0];
    let index = small_int(args[1])?;
    if index < 1 {
        return Err(badarg());
    }
    let index = index as usize;
    let elems = proper_list(args[2]).ok_or_else(function_clause)?;
    let mut mapped = Vec::with_capacity(elems.len());
    for elem in elems {
        if !is_tuple(elem) || index > tuple_size(elem) {
            return Err(badarg());
        }
        let updated = ctx.call_fun(fun, &[tuple_element(elem, index - 1)])?;
        mapped.push(replace_element(ctx, elem, index, updated)?);
    }
    build_list(ctx, &mapped, NIL)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Registers the `lists` module BIFs.
///
/// 45 `lists` keys: the five real `bif.tab` BIFs (`member/2`, `reverse/2`,
/// `keyfind/3`, `keymember/3`, `keysearch/3`), the pure-Erlang `lists.erl`
/// functions as Rust fast paths, and the 18 `fun`-taking functions (which need the
/// [`crate::FunCaller`] seam).
///
/// Also registers the four **`erlang`** list operators — `'++'/2`, `'--'/2` and
/// their `append/2`, `subtract/2` aliases (`bif.tab` `bif erlang:'++'/2
/// ebif_plusplus_2`, `bif erlang:'--'/2 ebif_minusminus_2`).
pub(crate) fn register(reg: &mut BifRegistry) {
    // The `erlang` list operators (`erl_bif_lists.c`; `append`/`subtract` are the
    // same C functions as `++`/`--`, so the aliases share the implementation).
    reg.register("erlang", "++", 2, plusplus_2);
    reg.register("erlang", "append", 2, plusplus_2);
    reg.register("erlang", "--", 2, minusminus_2);
    reg.register("erlang", "subtract", 2, minusminus_2);
    reg.register("lists", "member", 2, member);
    reg.register("lists", "reverse", 1, reverse1);
    reg.register("lists", "reverse", 2, reverse2);
    reg.register("lists", "keyfind", 3, keyfind);
    reg.register("lists", "keymember", 3, keymember);
    reg.register("lists", "keysearch", 3, keysearch);
    reg.register("lists", "append", 1, append1);
    reg.register("lists", "append", 2, append2);
    reg.register("lists", "nth", 2, nth);
    reg.register("lists", "nthtail", 2, nthtail);
    reg.register("lists", "last", 1, last);
    reg.register("lists", "sublist", 2, sublist2);
    reg.register("lists", "sublist", 3, sublist3);
    reg.register("lists", "seq", 2, seq2);
    reg.register("lists", "seq", 3, seq3);
    reg.register("lists", "flatten", 1, flatten1);
    reg.register("lists", "flatten", 2, flatten2);
    reg.register("lists", "sum", 1, sum);
    reg.register("lists", "max", 1, max);
    reg.register("lists", "min", 1, min);
    reg.register("lists", "duplicate", 2, duplicate);
    reg.register("lists", "delete", 2, delete);
    reg.register("lists", "keysort", 2, keysort);
    reg.register("lists", "sort", 1, sort);
    reg.register("lists", "usort", 1, usort);
    reg.register("lists", "zip", 2, zip);
    reg.register("lists", "unzip", 1, unzip);
    // The `fun`-taking functions: they call their fun through `BifContext::call_fun`.
    reg.register("lists", "map", 2, map2);
    reg.register("lists", "filter", 2, filter2);
    reg.register("lists", "foldl", 3, foldl3);
    reg.register("lists", "foldr", 3, foldr3);
    reg.register("lists", "foreach", 2, foreach2);
    reg.register("lists", "sort", 2, sort2);
    reg.register("lists", "usort", 2, usort2);
    reg.register("lists", "flatmap", 2, flatmap2);
    reg.register("lists", "zipwith", 3, zipwith3);
    reg.register("lists", "mapfoldl", 3, mapfoldl3);
    reg.register("lists", "mapfoldr", 3, mapfoldr3);
    reg.register("lists", "all", 2, all2);
    reg.register("lists", "any", 2, any2);
    reg.register("lists", "takewhile", 2, takewhile2);
    reg.register("lists", "dropwhile", 2, dropwhile2);
    reg.register("lists", "partition", 2, partition2);
    reg.register("lists", "splitwith", 2, splitwith2);
    reg.register("lists", "keymap", 3, keymap3);
}

#[cfg(test)]
mod tests {
    //! Unit tests over a real `beam_alloc::Heap` and a seeded `beam_term::AtomTable`.
    //!
    //! Every expected value below was taken from the OTP 29 / erts-17.1 oracle, so
    //! the assertions pin the *oracle's* behaviour rather than this implementation's.

    use super::*;

    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    /// Calls a `lists` BIF on freshly built argument terms.
    ///
    /// The arguments are evaluated into a temporary slice *before* the call, so
    /// `call!(ctx, sort, [ints(ctx, &[2, 1])])` — an argument built on the very
    /// context the BIF is called with — keeps the `&mut ctx` borrows disjoint.
    macro_rules! call {
        ($ctx:expr, $f:expr, [$($arg:expr),* $(,)?]) => {{
            let args: Vec<Eterm> = vec![$($arg),*];
            $f($ctx, &args)
        }};
    }

    /// Calls a BIF and asserts the `~w` rendering of its result.
    macro_rules! show {
        ($ctx:expr, $f:expr, [$($arg:expr),* $(,)?], $expected:expr $(,)?) => {{
            let got = call!($ctx, $f, [$($arg),*]).expect("the BIF should succeed");
            assert_eq!(pretty($ctx, got), $expected);
        }};
    }

    /// Calls a BIF and asserts that it fails with `badarg`.
    macro_rules! rejects {
        ($ctx:expr, $f:expr, [$($arg:expr),* $(,)?]) => {{
            assert_badarg(call!($ctx, $f, [$($arg),*]));
        }};
    }

    /// Calls a BIF and asserts that it fails with `error:function_clause` — the
    /// `lists.erl` no-clause failure the oracle reports for these argument problems.
    macro_rules! rejects_clause {
        ($ctx:expr, $f:expr, [$($arg:expr),* $(,)?]) => {{
            assert_function_clause(call!($ctx, $f, [$($arg),*]));
        }};
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true", "false", "ok", "a", "b", "c", "d", "e", "f", "g", "h", "tail", "x",
            "value",
        ]);
        atoms
    }

    /// Runs `f` with a `BifContext` over a `capacity_words`-word test heap.
    fn with_heap<R>(capacity_words: usize, f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(capacity_words).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        with_heap(65_536, f)
    }

    /// A mock [`crate::FunCaller`] for the ordering funs of `sort/2`/`usort/2`: the
    /// "fun" term is a small-integer sentinel naming the *exact* Erlang comparator
    /// the oracle was probed with. `true`/`false` come from the seeded atom table
    /// (the runtime's boolean atoms are not at fixed indices).
    struct OrderFuns {
        truth: Eterm,
        falsity: Eterm,
    }
    /// `fun(_, _) -> false end` — a completely degenerate comparator.
    const ALWAYS_FALSE: i64 = 100;
    /// `fun(_, _) -> true end` — the other degenerate comparator.
    const ALWAYS_TRUE: i64 = 101;
    /// `fun(A, B) -> A =< B end` — the expected total (ascending) order.
    const ORDER_ASC: i64 = 102;
    /// `fun(A, B) -> A >= B end` — the reverse (descending) order.
    const ORDER_DESC: i64 = 103;
    /// `fun(A, B) -> A < B end` — a non-total strict ascending order.
    const ORDER_LT: i64 = 104;
    /// `fun(A, B) -> A > B end` — a non-total strict descending order.
    const ORDER_GT: i64 = 105;
    impl crate::FunCaller for OrderFuns {
        fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
            let a = beam_term::signed_value(args[0]);
            let b = beam_term::signed_value(args[1]);
            let result = match beam_term::signed_value(fun) {
                ALWAYS_FALSE => false,
                ALWAYS_TRUE => true,
                ORDER_ASC => a <= b,
                ORDER_DESC => a >= b,
                ORDER_LT => a < b,
                ORDER_GT => a > b,
                other => panic!("unknown sentinel fun {other}"),
            };
            Ok(if result { self.truth } else { self.falsity })
        }
    }

    /// Runs `f` with a `BifContext` that has the ordering-fun mock installed.
    fn with_order_funs<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(65_536).expect("test heap");
        let atoms = atoms();
        let truth = beam_term::make_atom(atoms.lookup("true").expect("seeded atom"));
        let falsity = beam_term::make_atom(atoms.lookup("false").expect("seeded atom"));
        let mut caller = OrderFuns { truth, falsity };
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_fun_caller(&mut caller);
        f(&mut ctx)
    }

    /// Calls `usort/2` with the sentinel comparator `id` on `list` and renders the
    /// result with `~w`.
    fn show_usort(ctx: &mut BifContext<'_>, id: i64, list: Eterm) -> String {
        let args = vec![make_small(id), list];
        let out = usort2(ctx, &args).expect("usort/2 should succeed");
        pretty(ctx, out)
    }

    /// Renders `term` with the `~w` printer, for readable assertions.
    fn pretty(ctx: &mut BifContext<'_>, term: Eterm) -> String {
        crate::print::format_term(ctx, term, false).expect("term is printable")
    }

    /// A proper list of small integers.
    fn ints(ctx: &mut BifContext<'_>, items: &[i64]) -> Eterm {
        let elems: Vec<Eterm> = items.iter().copied().map(make_small).collect();
        ctx.list(&elems).expect("the list fits")
    }

    /// A proper list of the seeded atoms named in `names`.
    fn atoms_list(ctx: &mut BifContext<'_>, names: &[&str]) -> Eterm {
        let elems: Vec<Eterm> = names
            .iter()
            .map(|name| ctx.atom(name).expect("seeded"))
            .collect();
        ctx.list(&elems).expect("the list fits")
    }

    /// `{First, Second}` on the context heap.
    fn tuple2(ctx: &mut BifContext<'_>, first: Eterm, second: Eterm) -> Eterm {
        ctx.tuple(&[first, second]).expect("the tuple fits")
    }

    fn assert_badarg(result: Result<Eterm>) {
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "expected badarg, got {result:?}"
        );
    }

    /// Asserts that `result` is the `error:function_clause` raise a `lists.erl`
    /// no-clause failure produces (class `error`, reason the bare atom).
    fn assert_function_clause(result: Result<Eterm>) {
        assert!(
            matches!(&result, Err(BeamError::Raise(r))
                if r.class == beam_common::ExceptionClass::Error
                    && r.reason == beam_common::ExitReason::atom("function_clause")),
            "expected function_clause, got {result:?}"
        );
    }

    fn assert_badarith(result: Result<Eterm>) {
        assert!(
            matches!(result, Err(BeamError::BadArith)),
            "expected badarith, got {result:?}"
        );
    }

    #[test]
    fn member_uses_exact_equality_and_needs_a_proper_list() {
        with_ctx(|ctx| {
            let one = make_small(1);
            let one_f = ctx.make_float(1.0).unwrap();
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();

            for haystack in [ints(ctx, &[1]), ints(ctx, &[2, 1]), ints(ctx, &[3, 2, 1])] {
                show!(ctx, member, [one, haystack], "true");
            }
            show!(ctx, member, [one, NIL], "false");
            // Exact, not arithmetic: `1 =:= 1.0` is false.
            let floats = ctx.list(&[one_f]).unwrap();
            show!(ctx, member, [one, floats], "false");
            show!(ctx, member, [one_f, floats], "true");
            show!(ctx, member, [a, atoms_list(ctx, &["b", "a"])], "true");
            show!(ctx, member, [a, atoms_list(ctx, &["b", "c"])], "false");

            // Compound elements compare structurally and exactly too.
            let inner_int = ints(ctx, &[1]);
            let inner_two = ints(ctx, &[2]);
            let haystack = ctx.list(&[inner_int, inner_two]).unwrap();
            show!(ctx, member, [inner_int, haystack], "true");
            let floats_nested = ctx.list(&[floats]).unwrap();
            show!(ctx, member, [inner_int, floats_nested], "false");

            // A non-list or an improper tail is `badarg` (the `lists_member_2` path).
            let improper = ctx.cons(a, b).unwrap();
            rejects!(ctx, member, [b, improper]);
            rejects!(ctx, member, [a, a]);
        });
    }

    #[test]
    fn reverse_appends_an_arbitrary_tail() {
        with_ctx(|ctx| {
            show!(ctx, reverse1, [ints(ctx, &[1, 2, 3])], "[3,2,1]");
            show!(ctx, reverse1, [NIL], "[]");
            show!(ctx, reverse2, [ints(ctx, &[1, 2]), NIL], "[2,1]");

            // `reverse([], Tail)` is `Tail` itself — it is never checked to be a list.
            let tail = ctx.atom("tail").unwrap();
            assert_eq!(call!(ctx, reverse2, [NIL, tail]).unwrap(), tail);
            show!(ctx, reverse2, [ints(ctx, &[1, 2]), tail], "[2,1|tail]");
            let tuple_tail = ctx.tuple(&[tail]).unwrap();
            show!(ctx, reverse2, [ints(ctx, &[1, 2]), tuple_tail], "[2,1|{tail}]");

            let improper = ctx.cons(make_small(1), ctx.atom("b").unwrap()).unwrap();
            rejects!(ctx, reverse1, [improper]);
            rejects!(ctx, reverse1, [ctx.atom("a").unwrap()]);
            rejects!(ctx, reverse2, [improper, NIL]);
            rejects!(ctx, reverse2, [ctx.atom("a").unwrap(), NIL]);
        });
    }

    #[test]
    fn append_flattens_one_level_and_keeps_the_tail_verbatim() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let three = make_small(3);
            let one_two = ints(ctx, &[1, 2]);

            show!(ctx, append2, [one_two, ints(ctx, &[3])], "[1,2,3]");
            show!(ctx, append2, [ints(ctx, &[1, 2]), three], "[1,2|3]");
            assert_eq!(call!(ctx, append2, [NIL, three]).unwrap(), three);
            rejects!(ctx, append2, [ctx.cons(make_small(1), a).unwrap(), ints(ctx, &[3])]);
            rejects!(ctx, append2, [a, NIL]);

            // append/1: `append([E])` is `E` whatever it is, so a one-element list
            // never checks anything.
            show!(ctx, append1, [NIL], "[]");
            assert_eq!(call!(ctx, append1, [ctx.list(&[a]).unwrap()]).unwrap(), a);
            assert_eq!(call!(ctx, append1, [ctx.list(&[three]).unwrap()]).unwrap(), three);
            let three_list = ints(ctx, &[3]);
            let nested = ctx.list(&[one_two, three_list]).unwrap();
            show!(ctx, append1, [nested], "[1,2,3]");
            let scalar_tail = ctx.list(&[one_two, three]).unwrap();
            show!(ctx, append1, [scalar_tail], "[1,2|3]");
            let one_list = ints(ctx, &[1]);
            let empty_head = ctx.list(&[NIL, one_list]).unwrap();
            show!(ctx, append1, [empty_head], "[1]");
            show!(ctx, append1, [ctx.list(&[NIL, three]).unwrap()], "3");
            // Every element but the last must be a proper list.
            rejects!(ctx, append1, [ints(ctx, &[1, 2])]);
            rejects!(ctx, append1, [atoms_list(ctx, &["a", "a"])]);
            rejects_clause!(ctx, append1, [ctx.cons(make_small(1), a).unwrap()]);
        });
    }

    #[test]
    fn nth_nthtail_and_last_walk_the_expected_number_of_cells() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let c = ctx.atom("c").unwrap();
            let abc = atoms_list(ctx, &["a", "b", "c"]);
            let improper = ctx.cons(a, b).unwrap();

            assert_eq!(call!(ctx, nth, [make_small(1), abc]).unwrap(), a);
            assert_eq!(call!(ctx, nth, [make_small(2), abc]).unwrap(), b);
            assert_eq!(call!(ctx, nth, [make_small(3), abc]).unwrap(), c);
            assert_eq!(call!(ctx, nth, [make_small(1), improper]).unwrap(), a);
            for n in [0, 4, -1] {
                rejects_clause!(ctx, nth, [make_small(n), abc]);
            }
            rejects_clause!(ctx, nth, [make_small(2), improper]);
            rejects_clause!(ctx, nth, [make_small(1), NIL]);
            rejects_clause!(ctx, nth, [make_small(1), a]);
            rejects_clause!(ctx, nth, [ctx.make_float(1.0).unwrap(), abc]);
            rejects_clause!(ctx, nth, [a, abc]);

            // nthtail/2: `nthtail(0, L)` is `L`; the Nth tail is returned verbatim.
            assert_eq!(call!(ctx, nthtail, [make_small(0), NIL]).unwrap(), NIL);
            assert_eq!(call!(ctx, nthtail, [make_small(0), abc]).unwrap(), abc);
            show!(ctx, nthtail, [make_small(1), abc], "[b,c]");
            show!(ctx, nthtail, [make_small(2), abc], "[c]");
            show!(ctx, nthtail, [make_small(3), abc], "[]");
            assert_eq!(call!(ctx, nthtail, [make_small(1), improper]).unwrap(), b);
            rejects_clause!(ctx, nthtail, [make_small(2), improper]);
            rejects_clause!(ctx, nthtail, [make_small(4), abc]);
            rejects_clause!(ctx, nthtail, [make_small(1), NIL]);
            rejects_clause!(ctx, nthtail, [make_small(-1), abc]);
            rejects_clause!(ctx, nthtail, [ctx.make_float(1.0).unwrap(), abc]);
            rejects_clause!(ctx, nthtail, [make_small(0), a]);
            rejects_clause!(ctx, nthtail, [make_small(1), a]);

            // last/1.
            assert_eq!(call!(ctx, last, [abc]).unwrap(), c);
            assert_eq!(call!(ctx, last, [ints(ctx, &[7])]).unwrap(), make_small(7));
            rejects_clause!(ctx, last, [NIL]);
            rejects_clause!(ctx, last, [improper]);
            rejects_clause!(ctx, last, [a]);
        });
    }

    #[test]
    fn sublist_handles_ranges_and_running_off_the_end() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let improper = ctx.cons(make_small(1), ctx.atom("b").unwrap()).unwrap();

            show!(ctx, sublist2, [ints(ctx, &[1, 2, 3, 4]), make_small(2)], "[1,2]");
            show!(ctx, sublist2, [ints(ctx, &[1, 2, 3]), make_small(5)], "[1,2,3]");
            show!(ctx, sublist2, [ints(ctx, &[1, 2, 3]), make_small(0)], "[]");
            show!(ctx, sublist2, [improper, make_small(0)], "[]");
            rejects_clause!(ctx, sublist2, [improper, make_small(2)]);
            rejects_clause!(ctx, sublist2, [ints(ctx, &[1, 2, 3]), make_small(-1)]);
            rejects_clause!(ctx, sublist2, [ints(ctx, &[1, 2, 3]), ctx.make_float(1.0).unwrap()]);
            rejects_clause!(ctx, sublist2, [a, make_small(2)]);

            show!(ctx, sublist3, [ints(ctx, &[1, 2, 3, 4]), make_small(1), make_small(2)], "[1,2]");
            show!(ctx, sublist3, [ints(ctx, &[1, 2, 3, 4]), make_small(2), make_small(2)], "[2,3]");
            show!(ctx, sublist3, [ints(ctx, &[1, 2, 3, 4]), make_small(3), make_small(9)], "[3,4]");
            show!(ctx, sublist3, [ints(ctx, &[1, 2, 3, 4]), make_small(5), make_small(2)], "[]");
            show!(ctx, sublist3, [ints(ctx, &[1, 2]), make_small(3), make_small(1)], "[]");
            show!(ctx, sublist3, [ints(ctx, &[1, 2, 3]), make_small(1), make_small(0)], "[]");
            show!(ctx, sublist3, [ints(ctx, &[1, 2, 3, 4]), make_small(2), make_small(0)], "[]");
            rejects_clause!(ctx, sublist3, [ints(ctx, &[1, 2, 3, 4]), make_small(0), make_small(2)]);
            rejects_clause!(ctx, sublist3, [ints(ctx, &[1, 2, 3, 4]), make_small(2), make_small(-1)]);
            rejects_clause!(ctx, sublist3, [improper, make_small(1), make_small(2)]);
            rejects_clause!(ctx, sublist3, [ints(ctx, &[1, 2, 3]), ctx.make_float(1.0).unwrap(), make_small(1)]);
        });
    }

    #[test]
    fn seq_builds_arithmetic_sequences() {
        with_ctx(|ctx| {
            show!(ctx, seq2, [make_small(1), make_small(10)], "[1,2,3,4,5,6,7,8,9,10]");
            show!(ctx, seq2, [make_small(4), make_small(4)], "[4]");
            show!(ctx, seq2, [make_small(1), make_small(0)], "[]");
            rejects_clause!(ctx, seq2, [make_small(5), make_small(1)]);
            rejects_clause!(ctx, seq2, [ctx.make_float(1.0).unwrap(), make_small(5)]);
            rejects_clause!(ctx, seq2, [ctx.atom("a").unwrap(), make_small(5)]);
            let thousand = call!(ctx, seq2, [make_small(1), make_small(1000)]).unwrap();
            assert_eq!(proper_list(thousand).expect("proper list").len(), 1000);

            show!(ctx, seq3, [make_small(1), make_small(20), make_small(3)], "[1,4,7,10,13,16,19]");
            show!(ctx, seq3, [make_small(0), make_small(5), make_small(2)], "[0,2,4]");
            show!(ctx, seq3, [make_small(5), make_small(1), make_small(-1)], "[5,4,3,2,1]");
            // `To` exceeding the range is not an error, it just truncates.
            show!(ctx, seq3, [make_small(1), make_small(5), make_small(7)], "[1]");
            show!(ctx, seq3, [make_small(1), make_small(0), make_small(1)], "[]");
            show!(ctx, seq3, [make_small(10), make_small(6), make_small(4)], "[]");
            show!(ctx, seq3, [make_small(1), make_small(1), make_small(0)], "[1]");
            rejects!(ctx, seq3, [make_small(1), make_small(2), make_small(0)]);
            rejects!(ctx, seq3, [make_small(1), make_small(5), make_small(-1)]);
            rejects!(ctx, seq3, [make_small(1), make_small(5), ctx.make_float(1.0).unwrap()]);
            rejects!(ctx, seq3, [ctx.atom("a").unwrap(), make_small(5), make_small(1)]);
        });
    }

    #[test]
    fn flatten_splices_every_level_and_appends_the_tail() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let c = ctx.atom("c").unwrap();

            show!(ctx, flatten1, [NIL], "[]");
            show!(ctx, flatten1, [ints(ctx, &[1, 2, 3])], "[1,2,3]");
            show!(ctx, flatten1, [ctx.list(&[NIL]).unwrap()], "[]");
            let inner = atoms_list(ctx, &["b"]);
            show!(ctx, flatten1, [ctx.list(&[a, inner]).unwrap()], "[a,b]");
            let one_two = ints(ctx, &[1, 2]);
            let just_three = ints(ctx, &[3]);
            let flat = ctx.list(&[one_two, just_three]).unwrap();
            show!(ctx, flatten1, [flat], "[1,2,3]");
            let four = ints(ctx, &[4]);
            let deep_inner = ctx.list(&[make_small(3), four]).unwrap();
            let deep = ctx.list(&[one_two, deep_inner]).unwrap();
            show!(ctx, flatten1, [deep], "[1,2,3,4]");
            let empty = ctx.list(&[NIL]).unwrap();
            show!(ctx, flatten1, [ctx.list(&[empty]).unwrap()], "[]");
            rejects_clause!(ctx, flatten1, [a]);
            // A non-list *tail* reached by the walk has no `do_flatten/2` clause.
            let improper = ctx.cons(b, c).unwrap();
            rejects_clause!(ctx, flatten1, [ctx.list(&[a, improper]).unwrap()]);

            // flatten/2: only the *top* of the tail is checked, the rest is verbatim.
            show!(ctx, flatten2, [ctx.list(&[a, inner]).unwrap(), atoms_list(ctx, &["c"])], "[a,b,c]");
            show!(ctx, flatten2, [ctx.list(&[a]).unwrap(), improper], "[a,b|c]");
            let b_list = atoms_list(ctx, &["b"]);
            let nested_tail = ctx.list(&[b_list]).unwrap();
            show!(ctx, flatten2, [ctx.list(&[a]).unwrap(), nested_tail], "[a,[b]]");
            show!(ctx, flatten2, [flat, NIL], "[1,2,3]");
            rejects_clause!(ctx, flatten2, [a, NIL]);
            rejects_clause!(ctx, flatten2, [ctx.list(&[a]).unwrap(), a]);
            rejects_clause!(ctx, flatten2, [flat, b]);
        });
    }

    #[test]
    fn flatten_walks_deeply_nested_lists_without_recursing() {
        with_heap(400_000, |ctx| {
            // 40_000 levels of nesting must not overflow the Rust stack.
            let a = ctx.atom("a").unwrap();
            let mut nested = ctx.cons(a, NIL).unwrap();
            for _ in 0..40_000 {
                nested = ctx.cons(nested, NIL).unwrap();
            }
            let flat = call!(ctx, flatten1, [nested]).unwrap();
            assert_eq!(pretty(ctx, flat), "[a]");

            // A 20_000-element flat list round-trips unchanged (length only: the
            // rendering would be far bigger than the assertion needs).
            let long: Vec<Eterm> = (0..20_000).map(make_small).collect();
            let long = ctx.list(&long).unwrap();
            let flat = call!(ctx, flatten1, [long]).unwrap();
            assert_eq!(proper_list(flat).expect("proper list").len(), 20_000);
        });
    }

    #[test]
    fn sum_accumulates_ints_and_floats_the_erlang_way() {
        with_ctx(|ctx| {
            show!(ctx, sum, [NIL], "0");
            show!(ctx, sum, [ints(ctx, &[1, 2, 3])], "6");

            // A float anywhere in the list turns the whole sum into a float.
            let two_f = ctx.make_float(2.0).unwrap();
            let mixed = ctx.list(&[make_small(1), two_f, make_small(3)]).unwrap();
            show!(ctx, sum, [mixed], "6.0");
            let one_five = ctx.make_float(1.5).unwrap();
            let two_five = ctx.make_float(2.5).unwrap();
            let halves = ctx.list(&[one_five, two_five]).unwrap();
            show!(ctx, sum, [halves], "4.0");

            assert_badarith(call!(ctx, sum, [atoms_list(ctx, &["a"])]));
            rejects_clause!(ctx, sum, [ctx.cons(make_small(1), make_small(2)).unwrap()]);
            rejects_clause!(ctx, sum, [ctx.atom("a").unwrap()]);

            // A sum that leaves the 60-bit small range **promotes to a bignum**: the
            // fold uses the exact `+` (`MAX_SMALL` is 576460752303423487, so adding 1
            // crosses it). Oracle: lists:sum([576460752303423487,1]) => 576460752303423488,
            // lists:sum([-576460752303423488,-1]) => -576460752303423489.
            show!(ctx, sum, [ints(ctx, &[beam_term::MAX_SMALL, 1])], "576460752303423488");
            show!(
                ctx,
                sum,
                [ints(ctx, &[beam_term::MIN_SMALL, -1])],
                "-576460752303423489"
            );

            // A bignum *element* is summed exactly too (oracle:
            // lists:sum([1 bsl 70]) => 1180591620717411303424).
            let two_to_70 = beam_codec::term::make_big(ctx.heap(), false, &[0, 64]).unwrap();
            let single_big = ctx.list(&[two_to_70]).unwrap();
            show!(ctx, sum, [single_big], "1180591620717411303424");
        });
    }

    #[test]
    fn max_and_min_keep_the_first_extreme() {
        with_ctx(|ctx| {
            let f1 = ctx.make_float(1.0).unwrap();

            show!(ctx, max, [ints(ctx, &[1, 2, 3])], "3");
            show!(ctx, max, [ints(ctx, &[3, 2, 1])], "3");
            show!(ctx, min, [ints(ctx, &[1, 2, 3])], "1");
            show!(ctx, max, [atoms_list(ctx, &["a", "b"])], "b");
            show!(ctx, min, [atoms_list(ctx, &["a", "b"])], "a");

            // Elements equal under `==` never replace the running extreme, so the
            // *first* of a tie wins — and int/float ties are arithmetic ties.
            let float_first = ctx.list(&[f1, make_small(1)]).unwrap();
            let int_first = ctx.list(&[make_small(1), f1]).unwrap();
            show!(ctx, max, [float_first], "1.0");
            show!(ctx, max, [int_first], "1");
            show!(ctx, min, [float_first], "1.0");
            show!(ctx, min, [int_first], "1");

            // Cross-type: the term order puts atoms before tuples.
            let tup = ctx.tuple(&[ctx.atom("b").unwrap()]).unwrap();
            show!(ctx, max, [ctx.list(&[ctx.atom("a").unwrap(), tup]).unwrap()], "{b}");
            show!(ctx, min, [ctx.list(&[ctx.atom("a").unwrap(), tup]).unwrap()], "a");

            // A bignum element is ordered exactly (oracle:
            // lists:max([1, 1 bsl 70]) => 1180591620717411303424).
            let two_to_70 = beam_codec::term::make_big(ctx.heap(), false, &[0, 64]).unwrap();
            let with_big = ctx.list(&[make_small(1), two_to_70]).unwrap();
            show!(ctx, max, [with_big], "1180591620717411303424");
            show!(ctx, min, [with_big], "1");

            rejects_clause!(ctx, max, [NIL]);
            rejects_clause!(ctx, min, [NIL]);
            rejects_clause!(ctx, max, [ctx.cons(make_small(1), make_small(2)).unwrap()]);
            rejects_clause!(ctx, min, [ctx.atom("a").unwrap()]);
        });
    }

    #[test]
    fn duplicate_and_delete_edge_cases() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();

            show!(ctx, duplicate, [make_small(0), a], "[]");
            show!(ctx, duplicate, [make_small(3), a], "[a,a,a]");
            rejects_clause!(ctx, duplicate, [make_small(-1), a]);
            rejects_clause!(ctx, duplicate, [ctx.make_float(1.0).unwrap(), a]);

            // The removed position yields its tail verbatim.
            let improper = ctx.cons(a, b).unwrap();
            assert_eq!(call!(ctx, delete, [a, improper]).unwrap(), b);
            show!(ctx, delete, [a, atoms_list(ctx, &["a", "a"])], "[a]");
            show!(ctx, delete, [a, atoms_list(ctx, &["b", "a", "c"])], "[b,c]");
            show!(ctx, delete, [ctx.atom("d").unwrap(), atoms_list(ctx, &["a", "b"])], "[a,b]");
            rejects_clause!(ctx, delete, [b, improper]);

            // Exact, not arithmetic: `1 =:= 1.0` is false.
            let f1 = ctx.make_float(1.0).unwrap();
            show!(ctx, delete, [f1, ints(ctx, &[1])], "[1]");
            show!(ctx, delete, [make_small(1), ctx.list(&[f1]).unwrap()], "[1.0]");
        });
    }

    #[test]
    fn keyfind_uses_arithmetic_equality_and_skips_non_tuples() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let x = ctx.atom("x").unwrap();
            let first = tuple2(ctx, a, make_small(1));
            let second = tuple2(ctx, b, make_small(2));
            let tuples = ctx.list(&[first, second]).unwrap();

            assert_eq!(call!(ctx, keyfind, [a, make_small(1), tuples]).unwrap(), first);
            assert_eq!(call!(ctx, keyfind, [b, make_small(1), tuples]).unwrap(), second);
            // The key is compared with the Nth element, not the first.
            assert_eq!(call!(ctx, keyfind, [make_small(2), make_small(2), tuples]).unwrap(), second);
            // Arithmetic (`==`) keys, not exact ones — in both directions.
            let f1 = ctx.make_float(1.0).unwrap();
            let int_one = ctx.tuple(&[make_small(1)]).unwrap();
            let float_one = ctx.tuple(&[f1]).unwrap();
            show!(ctx, keyfind, [f1, make_small(1), ctx.list(&[int_one]).unwrap()], "{1}");
            show!(ctx, keyfind, [make_small(1), make_small(1), ctx.list(&[float_one]).unwrap()], "{1.0}");
            // A tuple shorter than the position is skipped, as is a non-tuple.
            show!(ctx, keyfind, [x, make_small(3), ctx.list(&[first]).unwrap()], "false");
            show!(ctx, keyfind, [x, make_small(1), atoms_list(ctx, &["a", "b"])], "false");
            show!(ctx, keyfind, [x, make_small(1), NIL], "false");
            rejects!(ctx, keyfind, [x, make_small(0), tuples]);
            rejects!(ctx, keyfind, [x, make_small(1), x]);
            rejects!(ctx, keyfind, [x, ctx.make_float(1.0).unwrap(), tuples]);
            rejects!(ctx, keyfind, [x, make_small(1), ctx.cons(first, b).unwrap()]);
        });
    }

    #[test]
    fn keymember_and_keysearch_match_the_oracle() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let x = ctx.atom("x").unwrap();
            let one = make_small(1);
            let first = tuple2(ctx, a, one);
            let second = tuple2(ctx, b, make_small(2));
            let tuples = ctx.list(&[first, second]).unwrap();
            let one_short = ctx.tuple(&[a]).unwrap();

            // keymember/3 — `true` iff some tuple's Nth element `==` the key.
            show!(ctx, keymember, [a, one, tuples], "true");
            // Non-tuple elements are skipped; a tuple shorter than N never matches.
            show!(ctx, keymember, [a, one, ctx.list(&[x, first]).unwrap()], "true");
            show!(ctx, keymember, [a, make_small(2), ctx.list(&[first]).unwrap()], "false");
            show!(ctx, keymember, [a, make_small(3), ctx.list(&[first]).unwrap()], "false");
            show!(ctx, keymember, [a, one, NIL], "false");
            // Arithmetic (`==`) key match: `1.0` finds `1`.
            let one_f = ctx.make_float(1.0).unwrap();
            let int_one = ctx.tuple(&[make_small(1)]).unwrap();
            show!(ctx, keymember, [one_f, one, ctx.list(&[int_one]).unwrap()], "true");
            // A match short-circuits *before* an improper tail is ever reached.
            show!(ctx, keymember, [a, one, ctx.cons(one_short, make_small(2)).unwrap()], "true");

            // A non-integer index, an index below 1, or a non-list are `badarg`.
            rejects!(ctx, keymember, [a, make_small(0), ctx.list(&[one_short]).unwrap()]);
            rejects!(ctx, keymember, [a, a, ctx.list(&[one_short]).unwrap()]);
            rejects!(ctx, keymember, [a, one, x]);
            // A match that *is* never found leaves the improper tail exposed: `badarg`.
            rejects!(ctx, keymember, [b, one, ctx.cons(one_short, make_small(2)).unwrap()]);

            // keysearch/3 — `{value, Tuple}` for the first match, else the atom `false`.
            show!(ctx, keysearch, [a, one, tuples], "{value,{a,1}}");
            show!(ctx, keysearch, [a, make_small(2), ctx.list(&[first]).unwrap()], "false");
            show!(ctx, keysearch, [a, one, NIL], "false");
            show!(ctx, keysearch, [one_f, one, ctx.list(&[int_one]).unwrap()], "{value,{1}}");
            show!(ctx, keysearch, [a, one, ctx.cons(one_short, make_small(2)).unwrap()], "{value,{a}}");
            rejects!(ctx, keysearch, [a, make_small(0), ctx.list(&[one_short]).unwrap()]);
            rejects!(ctx, keysearch, [a, a, ctx.list(&[one_short]).unwrap()]);
            rejects!(ctx, keysearch, [a, one, x]);
        });
    }

    #[test]
    fn sort_and_usort_are_stable_and_deduplicate_arithmetically() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();

            show!(ctx, sort, [NIL], "[]");
            show!(ctx, sort, [ints(ctx, &[1])], "[1]");
            show!(ctx, sort, [ints(ctx, &[2, 1])], "[1,2]");
            // The term order: numbers < atoms < tuples < lists.
            let tup = ctx.tuple(&[b]).unwrap();
            let lst = atoms_list(ctx, &["c"]);
            let mixed = ctx.list(&[a, make_small(1), tup, lst]).unwrap();
            show!(ctx, sort, [mixed], "[1,a,{b},[c]]");
            // Lists compare element-wise; a prefix sorts before its extension.
            let one = ints(ctx, &[1]);
            let one_zero = ints(ctx, &[1, 0]);
            let two = ints(ctx, &[2]);
            let nested = ctx.list(&[two, one_zero, one]).unwrap();
            show!(ctx, sort, [nested], "[[1],[1,0],[2]]");
            rejects_clause!(ctx, sort, [ctx.cons(make_small(1), make_small(2)).unwrap()]);
            rejects_clause!(ctx, sort, [a]);

            // Stable: arithmetically equal elements keep their input order.
            let f1 = ctx.make_float(1.0).unwrap();
            show!(ctx, sort, [ctx.list(&[f1, make_small(1)]).unwrap()], "[1.0,1]");
            show!(ctx, sort, [ctx.list(&[make_small(1), f1]).unwrap()], "[1,1.0]");

            // usort/1 drops the *later* member of each equal run.
            show!(ctx, usort, [NIL], "[]");
            show!(ctx, usort, [ints(ctx, &[2, 1, 2])], "[1,2]");
            show!(ctx, usort, [atoms_list(ctx, &["a", "b", "a"])], "[a,b]");
            show!(ctx, usort, [ints(ctx, &[1, 1, 1])], "[1]");
            show!(ctx, usort, [ctx.list(&[f1, make_small(1), f1]).unwrap()], "[1.0]");
            show!(ctx, usort, [ctx.list(&[make_small(1), f1]).unwrap()], "[1]");
            let f1_list = ctx.list(&[f1]).unwrap();
            show!(ctx, usort, [ctx.list(&[one, f1_list, one]).unwrap()], "[[1]]");
            rejects_clause!(ctx, usort, [a]);
        });
    }

    #[test]
    fn keysort_sorts_on_the_nth_element_and_stays_stable() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let b_one = tuple2(ctx, b, make_small(1));
            let a_two = tuple2(ctx, a, make_small(2));
            let ab = ctx.list(&[b_one, a_two]).unwrap();

            show!(ctx, keysort, [make_small(1), ab], "[{a,2},{b,1}]");
            show!(ctx, keysort, [make_small(2), ctx.list(&[a_two, b_one]).unwrap()], "[{b,1},{a,2}]");

            // Lists shorter than two elements come back untouched — no key access,
            // so even an out-of-range position is accepted.
            let a_one = tuple2(ctx, a, make_small(1));
            show!(ctx, keysort, [make_small(1), NIL], "[]");
            show!(ctx, keysort, [make_small(99), ctx.list(&[a_one]).unwrap()], "[{a,1}]");

            // Stable on keys that compare equal — including int/float keys.
            let key1 = tuple2(ctx, make_small(1), a);
            let key1_f = ctx.make_float(1.0).unwrap();
            let key1_float = tuple2(ctx, key1_f, b);
            show!(ctx, keysort, [make_small(1), ctx.list(&[key1, key1]).unwrap()], "[{1,a},{1,a}]");
            show!(ctx, keysort, [make_small(1), ctx.list(&[key1_float, key1]).unwrap()], "[{1.0,b},{1,a}]");

            rejects_clause!(ctx, keysort, [make_small(0), ctx.list(&[key1]).unwrap()]);
            rejects_clause!(ctx, keysort, [make_small(-1), ctx.list(&[key1, key1]).unwrap()]);
            rejects_clause!(ctx, keysort, [ctx.make_float(1.0).unwrap(), NIL]);
            // `element(N, X)` on a non-tuple or a too-short tuple is badarg.
            let b_only = ctx.tuple(&[b]).unwrap();
            rejects!(ctx, keysort, [make_small(2), ctx.list(&[a_two, b_only]).unwrap()]);
            rejects!(ctx, keysort, [make_small(1), atoms_list(ctx, &["b", "c"])]);
            rejects!(ctx, keysort, [make_small(1), ctx.cons(a_two, b).unwrap()]);
            rejects!(ctx, keysort, [make_small(1), a]);
        });
    }

    #[test]
    fn zip_and_unzip_round_trip_and_reject_mismatches() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let ones = ints(ctx, &[1, 2]);
            let ab = atoms_list(ctx, &["a", "b"]);
            let pairs = call!(ctx, zip, [ones, ab]).unwrap();

            show!(ctx, zip, [ints(ctx, &[1, 2]), atoms_list(ctx, &["a", "b"])], "[{1,a},{2,b}]");
            show!(ctx, zip, [NIL, NIL], "[]");
            assert_eq!(pretty(ctx, pairs), "[{1,a},{2,b}]");
            show!(ctx, unzip, [pairs], "{[1,2],[a,b]}");
            show!(ctx, unzip, [NIL], "{[],[]}");

            rejects_clause!(ctx, zip, [ints(ctx, &[1]), ints(ctx, &[2, 3])]);
            rejects_clause!(ctx, zip, [ctx.cons(make_small(1), make_small(2)).unwrap(), ints(ctx, &[3])]);
            rejects_clause!(ctx, zip, [ints(ctx, &[1]), ctx.cons(make_small(2), make_small(3)).unwrap()]);
            rejects_clause!(ctx, zip, [NIL, a]);
            rejects_clause!(ctx, zip, [a, NIL]);

            let single = ctx.tuple(&[a]).unwrap();
            rejects_clause!(ctx, unzip, [ctx.list(&[single]).unwrap()]);
            let triple = ctx.tuple(&[a, make_small(1), b]).unwrap();
            rejects_clause!(ctx, unzip, [ctx.list(&[triple]).unwrap()]);
            let pair = tuple2(ctx, a, make_small(1));
            rejects_clause!(ctx, unzip, [ctx.cons(pair, b).unwrap()]);
            rejects_clause!(ctx, unzip, [a]);
        });
    }

    #[test]
    fn register_installs_every_pure_lists_function() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        let expected = [
            ("append", 1),
            ("append", 2),
            ("delete", 2),
            ("duplicate", 2),
            ("flatten", 1),
            ("flatten", 2),
            ("keyfind", 3),
            ("keymember", 3),
            ("keysearch", 3),
            ("keysort", 2),
            ("last", 1),
            ("max", 1),
            ("member", 2),
            ("min", 1),
            ("nth", 2),
            ("nthtail", 2),
            ("reverse", 1),
            ("reverse", 2),
            ("seq", 2),
            ("seq", 3),
            ("sort", 1),
            ("sublist", 2),
            ("sublist", 3),
            ("sum", 1),
            ("unzip", 1),
            ("usort", 1),
            ("zip", 2),
            // The `fun`-taking functions (`lists.erl` exports backed by the
            // `FunCaller` seam).
            ("all", 2),
            ("any", 2),
            ("dropwhile", 2),
            ("filter", 2),
            ("flatmap", 2),
            ("foldl", 3),
            ("foldr", 3),
            ("foreach", 2),
            ("keymap", 3),
            ("map", 2),
            ("mapfoldl", 3),
            ("mapfoldr", 3),
            ("partition", 2),
            ("sort", 2),
            ("splitwith", 2),
            ("takewhile", 2),
            ("usort", 2),
            ("zipwith", 3),
        ];
        for (name, arity) in expected {
            assert!(reg.contains("lists", name, arity), "missing lists:{name}/{arity}");
        }
        // The `erlang` list operators registered alongside the `lists` module.
        for (name, arity) in [("++", 2), ("--", 2), ("append", 2), ("subtract", 2)] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), expected.len() + 4);
    }

    #[test]
    fn erlang_list_operators_concatenate_and_subtract() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        with_ctx(|ctx| {
            // `[1,2] ++ [3]` ⇒ `[1,2,3]`.
            let lhs = ints(ctx, &[1, 2]);
            let rhs = ints(ctx, &[3]);
            let sum = reg
                .call("erlang", "++", 2, ctx, &[lhs, rhs])
                .expect("++/2");
            let expected = ints(ctx, &[1, 2, 3]);
            assert!(beam_term::compare::eq(sum, expected, ctx.atoms()));

            // `append/2` is the same implementation.
            let alias = reg
                .call("erlang", "append", 2, ctx, &[lhs, rhs])
                .expect("append/2");
            assert!(beam_term::compare::eq(alias, sum, ctx.atoms()));

            // `[1,2] ++ x` ⇒ `[1,2|x]`; `[] ++ x` ⇒ `x` (the oracle's documented
            // `[]` special case).
            let x = ctx.atom("x").unwrap();
            let improper_tail = reg
                .call("erlang", "++", 2, ctx, &[lhs, x])
                .expect("++/2 with a non-list tail");
            let expected = ctx.cons(ctx.int(2).unwrap(), x).unwrap();
            let expected = ctx.cons(ctx.int(1).unwrap(), expected).unwrap();
            assert!(beam_term::compare::eq(improper_tail, expected, ctx.atoms()));
            let nil_concat = reg
                .call("erlang", "++", 2, ctx, &[NIL, x])
                .expect("[] ++ x");
            assert!(beam_term::compare::eq(nil_concat, x, ctx.atoms()));

            // A non-list or improper **left** operand is `badarg`.
            let improper_left = ctx.cons(ctx.int(1).unwrap(), x).unwrap();
            for bad in [x, improper_left] {
                assert!(matches!(
                    reg.call("erlang", "++", 2, ctx, &[bad, rhs]),
                    Err(BeamError::BadArg)
                ));
            }

            // `[1,2,3] -- [1]` ⇒ `[2,3]`.
            let three = ints(ctx, &[1, 2, 3]);
            let one = ints(ctx, &[1]);
            let diff = reg
                .call("erlang", "--", 2, ctx, &[three, one])
                .expect("--/2");
            let expected = ints(ctx, &[2, 3]);
            assert!(beam_term::compare::eq(diff, expected, ctx.atoms()));

            // `subtract/2` is the same implementation.
            let alias = reg
                .call("erlang", "subtract", 2, ctx, &[three, one])
                .expect("subtract/2");
            assert!(beam_term::compare::eq(alias, diff, ctx.atoms()));

            // The match is exact (`=:=`): `1.0` does not remove `1` and vice versa.
            let one_f = ctx.make_float(1.0).unwrap();
            let floats = ctx.list(&[one_f]).unwrap();
            let one = ints(ctx, &[1]);
            let kept_float = reg
                .call("erlang", "--", 2, ctx, &[floats, one])
                .expect("--/2");
            assert!(beam_term::compare::eq(kept_float, floats, ctx.atoms()));
            let kept_int = reg
                .call("erlang", "--", 2, ctx, &[one, floats])
                .expect("--/2");
            assert!(beam_term::compare::eq(kept_int, one, ctx.atoms()));

            // A non-list operand on **either** side is `badarg`.
            let improper = ctx.cons(ctx.int(1).unwrap(), x).unwrap();
            for args in [[improper, one], [one, improper], [x, one], [one, x]] {
                assert!(matches!(
                    reg.call("erlang", "--", 2, ctx, &args),
                    Err(BeamError::BadArg)
                ));
            }
        });
    }

    #[test]
    fn the_registry_dispatches_the_lists_module() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        with_ctx(|ctx| {
            let list = ints(ctx, &[2, 1]);
            let sorted = reg.call("lists", "sort", 1, ctx, &[list]).expect("sort/1");
            assert_eq!(pretty(ctx, sorted), "[1,2]");
            // A registered key with a mismatched argument count is `BadArity`.
            assert!(matches!(
                reg.call("lists", "sort", 1, ctx, &[list, list]),
                Err(BeamError::BadArity)
            ));
        });
    }
    #[test]
    fn usort_with_a_total_comparator_matches_the_oracle() {
        with_order_funs(|ctx| {
            // `lists:usort(fun(A,B) -> A =< B end, [3,1,2,1,2])` ⇒ `[1,2,3]`.
            let l = ints(ctx, &[3, 1, 2, 1, 2]);
            assert_eq!(show_usort(ctx, ORDER_ASC, l), "[1,2,3]");
            // The reverse comparator: `[3,2,1]`.
            let l = ints(ctx, &[3, 1, 2, 1, 2]);
            assert_eq!(show_usort(ctx, ORDER_DESC, l), "[3,2,1]");
            // A list with duplicates: `lists:usort(fun(A,B)->A=<B end, [4,2,1,4])`
            // ⇒ `[1,2,4]` (the run keeps `1` as the representative).
            let l = ints(ctx, &[4, 2, 1, 4]);
            assert_eq!(show_usort(ctx, ORDER_ASC, l), "[1,2,4]");
            // The oracle's head clauses and the many-element path.
            assert_eq!(show_usort(ctx, ORDER_ASC, NIL), "[]");
            let l = ints(ctx, &[1]);
            assert_eq!(show_usort(ctx, ORDER_ASC, l), "[1]");
            let l = ints(ctx, &[2, 1]);
            assert_eq!(show_usort(ctx, ORDER_ASC, l), "[1,2]");
            let l = ints(ctx, &[5, 4, 3, 2, 1]);
            assert_eq!(show_usort(ctx, ORDER_ASC, l), "[1,2,3,4,5]");
        });
    }

    #[test]
    fn usort_with_a_degenerate_comparator_reproduces_the_oracle() {
        with_order_funs(|ctx| {
            // The three `lists.erl` degenerate-comparator results, verbatim.
            // `lists:usort(fun(_,_) -> false end, [1,2,3])` ⇒ `[3,2,1]`.
            let l = ints(ctx, &[1, 2, 3]);
            assert_eq!(show_usort(ctx, ALWAYS_FALSE, l), "[3,2,1]");
            // `lists:usort(fun(_,_) -> true end, [1,2,3])` ⇒ `[1]`.
            let l = ints(ctx, &[1, 2, 3]);
            assert_eq!(show_usort(ctx, ALWAYS_TRUE, l), "[1]");
            // `lists:usort(fun(_,_) -> true end, [3,1,2])` ⇒ `[3]`.
            let l = ints(ctx, &[3, 1, 2]);
            assert_eq!(show_usort(ctx, ALWAYS_TRUE, l), "[3]");
            // …and the shorter degenerate shapes.
            assert_eq!(show_usort(ctx, ALWAYS_FALSE, NIL), "[]");
            let l = ints(ctx, &[1]);
            assert_eq!(show_usort(ctx, ALWAYS_FALSE, l), "[1]");
            let l = ints(ctx, &[1, 2]);
            assert_eq!(show_usort(ctx, ALWAYS_FALSE, l), "[2,1]");
            let l = ints(ctx, &[1, 2, 3, 4, 5]);
            assert_eq!(show_usort(ctx, ALWAYS_FALSE, l), "[5,4,3,2,1]");
        });
    }

    /// `usort/2` must agree with the oracle for the four total/strict numeric
    /// comparators over the six pinning lists. Oracle (erts-17.1):
    ///
    /// ```text
    /// le [1,2,3]   -> [1,2,3]      le [3,2,1]   -> [1,2,3]
    /// le [1,1,1]   -> [1]          le [2,1,2,1] -> [1,2]
    /// le []        -> []           le [7]       -> [7]
    /// lt [1,2,3]   -> [1,2,3]      lt [3,2,1]   -> [1,2,3]
    /// lt [1,1,1]   -> [1,1,1]      lt [2,1,2,1] -> [1,1,2,2]
    /// lt []        -> []           lt [7]       -> [7]
    /// ge [1,2,3]   -> [3,2,1]      ge [3,2,1]   -> [3,2,1]
    /// ge [1,1,1]   -> [1]          ge [2,1,2,1] -> [2,1]
    /// ge []        -> []           ge [7]       -> [7]
    /// gt [1,2,3]   -> [3,2,1]      gt [3,2,1]   -> [3,2,1]
    /// gt [1,1,1]   -> [1,1,1]      gt [2,1,2,1] -> [2,2,1,1]
    /// gt []        -> []           gt [7]       -> [7]
    /// ```
    #[test]
    fn usort_with_each_comparator_matches_the_oracle() {
        let cases: &[(i64, &[i64], &str)] = &[
            (ORDER_ASC, &[1, 2, 3], "[1,2,3]"),
            (ORDER_ASC, &[3, 2, 1], "[1,2,3]"),
            (ORDER_ASC, &[1, 1, 1], "[1]"),
            (ORDER_ASC, &[2, 1, 2, 1], "[1,2]"),
            (ORDER_ASC, &[], "[]"),
            (ORDER_ASC, &[7], "[7]"),
            (ORDER_LT, &[1, 2, 3], "[1,2,3]"),
            (ORDER_LT, &[3, 2, 1], "[1,2,3]"),
            (ORDER_LT, &[1, 1, 1], "[1,1,1]"),
            (ORDER_LT, &[2, 1, 2, 1], "[1,1,2,2]"),
            (ORDER_LT, &[], "[]"),
            (ORDER_LT, &[7], "[7]"),
            (ORDER_DESC, &[1, 2, 3], "[3,2,1]"),
            (ORDER_DESC, &[3, 2, 1], "[3,2,1]"),
            (ORDER_DESC, &[1, 1, 1], "[1]"),
            (ORDER_DESC, &[2, 1, 2, 1], "[2,1]"),
            (ORDER_DESC, &[], "[]"),
            (ORDER_DESC, &[7], "[7]"),
            (ORDER_GT, &[1, 2, 3], "[3,2,1]"),
            (ORDER_GT, &[3, 2, 1], "[3,2,1]"),
            (ORDER_GT, &[1, 1, 1], "[1,1,1]"),
            (ORDER_GT, &[2, 1, 2, 1], "[2,2,1,1]"),
            (ORDER_GT, &[], "[]"),
            (ORDER_GT, &[7], "[7]"),
        ];
        with_order_funs(|ctx| {
            for &(cmp, items, expected) in cases {
                let l = ints(ctx, items);
                assert_eq!(
                    show_usort(ctx, cmp, l),
                    expected,
                    "usort/2 comparator {cmp} over {items:?}"
                );
            }
        });
    }

    #[test]
    fn usort_rejects_a_non_proper_list() {
        with_order_funs(|ctx| {
            let improper = ctx
                .cons(make_small(1), ctx.atom("a").expect("seeded"))
                .expect("the cons fits");
            let args = vec![make_small(ORDER_ASC), improper];
            rejects_clause!(ctx, usort2, [args[0], args[1]]);
        });
    }
}
