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

//! Integration tests for the **`lists` no-clause failure** — `error:function_clause`
//! — versus the failures that stay `badarg`/`badarith`.
//!
//! `lists.erl` distinguishes two families of argument failures, and erts-17.1
//! reports them with different reasons (verified against the oracle):
//!
//! * a **no-clause** failure — the arguments match no clause at all — raises
//!   `error:function_clause`, a bare atom whose arguments live in the *stacktrace*
//!   (`../../erts/emulator/beam/beam_common.c`), e.g. `lists:nth(3, [1,2])`,
//!   `lists:last([])`, `lists:zip([1], [2,3])`;
//! * an **explicit** argument rejection — `badarg`, e.g. `lists:member(a, foo)`,
//!   `lists:keyfind(a, 1, foo)`, `lists:seq(0, 1, 0)`;
//! * non-numeric arithmetic — `badarith`, e.g. `lists:sum([a])`.
//!
//! The registry adapts to [`BifRegistry::call`](beam_bifs::BifRegistry::call), so
//! these run through the crate's public seam only (`beam_bifs::*` plus its declared
//! dependency crates). Every expected outcome below was pinned against the OTP 29 /
//! erts-17.1 oracle.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b", "foo"]);
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

/// Asserts the `error:function_clause` raise (class `error`, the bare atom reason).
fn assert_function_clause(result: Result<Eterm>) {
    assert!(
        matches!(&result, Err(BeamError::Raise(r))
            if r.class == ExceptionClass::Error
                && r.reason == ExitReason::atom("function_clause")),
        "expected function_clause, got {result:?}"
    );
}

#[test]
fn no_clause_failures_raise_error_function_clause() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let foo = ctx.atom("foo").expect("seeded");
    let one_two = ints(&mut ctx, &[1, 2]);
    let one = ints(&mut ctx, &[1]);
    let two_three = ints(&mut ctx, &[2, 3]);
    let improper = ctx.cons(ctx.int(1).expect("fits"), ctx.int(2).expect("fits")).expect("fits");

    // (module, function, arity, args) — each a `lists` no-clause failure.
    let cases: [(&str, u32, Vec<Eterm>); 8] = [
        // `lists:nth(3, [1,2])` — the list is too short.
        ("nth", 2, vec![ctx.int(3).expect("fits"), one_two]),
        // `lists:last([])` — no element to return.
        ("last", 1, vec![ctx.list(&[]).expect("fits")]),
        // `lists:zip([1], [2,3])` — the lengths differ.
        ("zip", 2, vec![one, two_three]),
        // `lists:sublist([1], 0, 1)` — `Start` is below 1.
        ("sublist", 3, vec![one, ctx.int(0).expect("fits"), ctx.int(1).expect("fits")]),
        // `lists:flatten(foo)` — the root is not a list.
        ("flatten", 1, vec![foo]),
        // `lists:max([])` — no element to compare.
        ("max", 1, vec![ctx.list(&[]).expect("fits")]),
        // `lists:seq(3, 1)` — the range is empty (ascending form).
        ("seq", 2, vec![ctx.int(3).expect("fits"), ctx.int(1).expect("fits")]),
        // `lists:sum([1|2])` — the walk reaches an improper tail.
        ("sum", 1, vec![improper]),
    ];

    for (function, arity, args) in cases {
        let result = reg.call("lists", function, arity, &mut ctx, &args);
        assert_function_clause(result);
    }
}

#[test]
fn explicit_argument_rejections_stay_badarg() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let foo = ctx.atom("foo").expect("seeded");

    // `lists:member(a, foo)` — a non-list haystack is an explicit `badarg`.
    assert!(matches!(
        reg.call("lists", "member", 2, &mut ctx, &[a, foo]),
        Err(BeamError::BadArg)
    ));
    // `lists:keyfind(a, 1, foo)` — a non-list tuple list is `badarg`.
    let one = ctx.int(1).expect("fits");
    assert!(matches!(
        reg.call("lists", "keyfind", 3, &mut ctx, &[a, one, foo]),
        Err(BeamError::BadArg)
    ));
    // `lists:seq(0, 1, 0)` — a zero step has no clause of its own, but the oracle
    // reports `badarg` (not `function_clause`).
    let zero_step = [
        ctx.int(0).expect("fits"),
        ctx.int(1).expect("fits"),
        ctx.int(0).expect("fits"),
    ];
    assert!(matches!(
        reg.call("lists", "seq", 3, &mut ctx, &zero_step),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn non_numeric_sum_is_badarith() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let list = ctx.list(&[a]).expect("fits");
    // `lists:sum([a])` — `0 + a` is `badarith`.
    assert!(matches!(
        reg.call("lists", "sum", 1, &mut ctx, &[list]),
        Err(BeamError::BadArith)
    ));
}

/// Proves the registry actually holds the two functions the assertions above call,
/// so a typo cannot make them pass via an `undef` raise.
#[test]
fn the_registry_holds_the_lists_functions_under_test() {
    let reg = default_registry();
    for (name, arity) in [("nth", 2), ("last", 1), ("zip", 2), ("sublist", 3), ("flatten", 1),
                          ("max", 1), ("seq", 2), ("sum", 1), ("member", 2), ("keyfind", 3)]
    {
        assert!(reg.contains("lists", name, arity), "missing lists:{name}/{arity}");
    }
}