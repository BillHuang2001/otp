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

//! Integration tests for the `erlang` **list operators** — `'++'/2`, `'--'/2` and
//! their `append/2` / `subtract/2` aliases — driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) (the crate's public seam).
//!
//! Every expected value is pinned to the OTP 29 / erts-17.1 oracle, e.g.
//!
//! ```text
//! erl -noshell -eval 'io:format("~p~n",[[1,2]++[3]]), halt().'               % [1,2,3]
//! erl -noshell -eval 'io:format("~p~n",[[1,2]++x]), halt().'                 % [1,2|x]
//! erl -noshell -eval 'io:format("~p~n",[catch (x ++ [1])]), halt().'         % badarg
//! erl -noshell -eval 'io:format("~p~n",[[1,2,3]--[1]]), halt().'             % [2,3]
//! erl -noshell -eval 'io:format("~p~n",[[1.0]--[1]]), halt().'               % [1.0]
//! erl -noshell -eval 'io:format("~p~n",[[1]--[1.0]]), halt().'               % [1]
//! erl -noshell -eval 'io:format("~p~n",[[1,0,1]--[1,1]]), halt().'           % [0]
//! erl -noshell -eval 'io:format("~p~n",[catch ([1,2]--x)]), halt().'         % badarg
//! ```
//!
//! `erlang:append/2` and `erlang:subtract/2` are the *same* C functions as
//! `'++'/2` and `'--'/2` (`../../erts/emulator/beam/erl_bif_lists.c:350,1103`), so
//! each alias must agree with its operator on every input.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{compare::eq, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b", "x"]);
    atoms
}

/// The proper list `values` on the context heap.
fn ints(ctx: &mut BifContext<'_>, values: &[i64]) -> Eterm {
    let elems: Vec<Eterm> = values
        .iter()
        .map(|&v| ctx.int(v).expect("a small int fits"))
        .collect();
    ctx.list(&elems).expect("the list fits")
}

#[test]
fn plusplus_concatenates_and_uses_a_non_list_tail_verbatim() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let lhs = ints(&mut ctx, &[1, 2]);
    let rhs = ints(&mut ctx, &[3]);

    // `[1,2] ++ [3]` ⇒ `[1,2,3]`.
    let result = reg
        .call("erlang", "++", 2, &mut ctx, &[lhs, rhs])
        .expect("erlang:'++'/2");
    let expected = ints(&mut ctx, &[1, 2, 3]);
    assert!(eq(result, expected, &atoms));

    // `erlang:append/2` is the same BIF.
    let alias = reg
        .call("erlang", "append", 2, &mut ctx, &[lhs, rhs])
        .expect("erlang:append/2");
    assert!(eq(alias, expected, &atoms));

    // `[1,2] ++ x` ⇒ `[1,2|x]` — the right operand is used as the tail verbatim.
    let x = ctx.atom("x").expect("seeded");
    let result = reg
        .call("erlang", "++", 2, &mut ctx, &[lhs, x])
        .expect("erlang:'++'/2 with a non-list tail");
    let expected = ctx.cons(ctx.int(2).expect("fits"), x).expect("fits");
    let expected = ctx.cons(ctx.int(1).expect("fits"), expected).expect("fits");
    assert!(eq(result, expected, &atoms));

    // `[] ++ x` ⇒ `x` (the oracle's documented `[]` bug-compatibility), and
    // `[] ++ []` ⇒ `[]`.
    let result = reg
        .call("erlang", "++", 2, &mut ctx, &[NIL, x])
        .expect("[] ++ x");
    assert!(eq(result, x, &atoms));
    let result = reg
        .call("erlang", "++", 2, &mut ctx, &[NIL, NIL])
        .expect("[] ++ []");
    assert!(eq(result, NIL, &atoms));
}

#[test]
fn plusplus_rejects_a_non_list_or_improper_left_operand() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let x = ctx.atom("x").expect("seeded");
    let rhs = ints(&mut ctx, &[3]);
    let improper = ctx.cons(ctx.int(1).expect("fits"), x).expect("fits");

    // `x ++ [3]` and `[1|x] ++ [3]` are both `badarg`.
    for bad in [x, improper] {
        assert!(matches!(
            reg.call("erlang", "++", 2, &mut ctx, &[bad, rhs]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn minusminus_removes_the_first_exact_match_of_each_right_hand_element() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `[1,2,3] -- [1]` ⇒ `[2,3]`.
    let three = ints(&mut ctx, &[1, 2, 3]);
    let one = ints(&mut ctx, &[1]);
    let result = reg
        .call("erlang", "--", 2, &mut ctx, &[three, one])
        .expect("erlang:'--'/2");
    let expected = ints(&mut ctx, &[2, 3]);
    assert!(eq(result, expected, &atoms));

    // `erlang:subtract/2` is the same BIF.
    let alias = reg
        .call("erlang", "subtract", 2, &mut ctx, &[three, one])
        .expect("erlang:subtract/2");
    assert!(eq(alias, expected, &atoms));

    // Only the *first* occurrence is removed per right-hand element:
    // `[1,0,1] -- [1,1]` ⇒ `[0]`.
    let lhs = ints(&mut ctx, &[1, 0, 1]);
    let rhs = ints(&mut ctx, &[1, 1]);
    let result = reg
        .call("erlang", "--", 2, &mut ctx, &[lhs, rhs])
        .expect("erlang:'--'/2");
    let expected = ints(&mut ctx, &[0]);
    assert!(eq(result, expected, &atoms));

    // `[1,2] -- []` ⇒ `[1,2]`, `[] -- [1]` ⇒ `[]`, and an unmatched right-hand
    // element changes nothing (`[1,2] -- [3,4]` ⇒ `[1,2]`).
    let two = ints(&mut ctx, &[1, 2]);
    let empty = ctx.list(&[]).expect("fits");
    let result = reg
        .call("erlang", "--", 2, &mut ctx, &[two, empty])
        .expect("erlang:'--'/2 with []");
    assert!(eq(result, two, &atoms));
    let result = reg
        .call("erlang", "--", 2, &mut ctx, &[NIL, one])
        .expect("[] -- [1]");
    assert!(eq(result, NIL, &atoms));
    let unmatched = ints(&mut ctx, &[3, 4]);
    let result = reg
        .call("erlang", "--", 2, &mut ctx, &[two, unmatched])
        .expect("erlang:'--'/2 with no match");
    assert!(eq(result, two, &atoms));

    // The match is **exact** (`=:=`), not arithmetic: `[1.0] -- [1]` ⇒ `[1.0]` and
    // `[1] -- [1.0]` ⇒ `[1]` (oracle-verified).
    let one_f = ctx.make_float(1.0).expect("fits");
    let floats = ctx.list(&[one_f]).expect("fits");
    let result = reg
        .call("erlang", "--", 2, &mut ctx, &[floats, one])
        .expect("erlang:'--'/2");
    assert!(eq(result, floats, &atoms));
    let result = reg
        .call("erlang", "--", 2, &mut ctx, &[one, floats])
        .expect("erlang:'--'/2");
    assert!(eq(result, one, &atoms));
}

#[test]
fn minusminus_rejects_a_non_list_operand_on_either_side() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let one = ints(&mut ctx, &[1]);
    let x = ctx.atom("x").expect("seeded");
    let improper = ctx.cons(ctx.int(1).expect("fits"), x).expect("fits");

    // `x -- [1]`, `[1|x] -- [1]`, `[1] -- x` and `[1] -- [1|x]` are all `badarg`.
    for bad in [x, improper] {
        assert!(matches!(
            reg.call("erlang", "--", 2, &mut ctx, &[bad, one]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            reg.call("erlang", "--", 2, &mut ctx, &[one, bad]),
            Err(BeamError::BadArg)
        ));
    }
}
