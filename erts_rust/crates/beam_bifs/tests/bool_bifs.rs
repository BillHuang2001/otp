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

//! Integration tests for the boolean/guard operator BIFs that compiled code and the
//! guard compiler depend on: `erlang:'not'/1`, `'and'/2`, `'or'/2`, `'xor'/2`,
//! `is_function/2` and `is_record/1,2,3` — all driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call), i.e. exactly as `beam_rt`
//! will.
//!
//! Every expected value is pinned against the **system oracle** (OTP 29 /
//! erts-17.1). The semantics were read from the read-only ground truth
//! `erts/emulator/beam/erl_bif_op.c` — `and_2`/`or_2`/`xor_2`/`not_1` (lines 45-91),
//! `is_function_2`/`erl_is_function` (232-267), `is_record_1`/`_2`/`_3`
//! (279-350) — and then confirmed by running them under the real system, e.g.
//!
//! ```text
//! $ escript -e 'A = fun(M,F,As) -> apply(M,F,As) end,
//!              io:format("~p~n", [A(erlang, is_record, [{}, a, 0])])'   %% false
//! $ escript -e 'A = fun(M,F,As) -> apply(M,F,As) end,
//!              io:format("~p~n", [A(erlang, is_function, [fun() -> ok end, 0])])' %% true
//! ```
//!
//! the boolean operators themselves are reserved words in Erlang source, so they
//! were called as `erlang:'not'(true)`, `erlang:'and'(true, false)`, … (each
//! wrapped in `try/catch` to observe the `badarg`); `is_record/2,3` go through
//! `apply/3` because the compiler rewrites a *literal* call into a record-definition
//! lookup (`record a undefined` otherwise).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{atom_val, make_small, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b"]);
    atoms
}

/// The boolean value of a `true`/`false` atom term, resolved via `atoms`.
fn truth(atoms: &AtomTable, term: Eterm) -> bool {
    match atoms.name(atom_val(term)) {
        Some("true") => true,
        Some("false") => false,
        other => panic!("expected a boolean atom, got {other:?}"),
    }
}

/// Builds a local fun term of `arity`, returning it with the storage that owns its
/// words (the caller must keep that storage alive while the term is in use).
///
/// The frozen layout `[thing_word, entry, env…]` (`erl_fun.h`) and the header
/// `MAKE_FUN_HEADER(Arity, NumFree, External)` (`erl_term.h:417-424`: the arity at
/// bit 8, the environment size at bit 16, `TAG_PRIMARY_HEADER` = 0) are the ones
/// `beam_engine::terms::make_fun` writes; `erlang:is_function/2` reads only the
/// header word.
fn fun(arity: u32) -> (Box<[Eterm]>, Eterm) {
    let words: Box<[Eterm]> = vec![
        Eterm::from_raw(beam_term::FUN_SUBTAG | ((arity as usize) << 8)),
        Eterm::from_raw(0), // uniq  — replaces C's out-of-line `entry` pointer
        Eterm::from_raw(0), // index
    ]
    .into_boxed_slice();
    let term = beam_term::make_boxed(words.as_ptr());
    (words, term)
}

#[test]
fn strict_boolean_operators_match_the_oracle() {
    let reg = default_registry();
    let mut heap = Heap::new(1024).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let t = ctx.atom("true").expect("seeded");
    let f = ctx.atom("false").expect("seeded");

    // Oracle: erlang:'not'(true) = false, erlang:'not'(false) = true.
    assert!(!truth(
        &atoms,
        reg.call("erlang", "not", 1, &mut ctx, &[t])
            .expect("not true")
    ));
    assert!(truth(
        &atoms,
        reg.call("erlang", "not", 1, &mut ctx, &[f])
            .expect("not false")
    ));

    // The strict `and`/`or`/`xor` truth tables (`erl_bif_op.c:45-82`), all four
    // `true`/`false` combinations of each operator.
    let cases: [(&str, Eterm, Eterm, bool); 12] = [
        ("and", t, t, true),
        ("and", t, f, false),
        ("and", f, t, false),
        ("and", f, f, false),
        ("or", t, t, true),
        ("or", t, f, true),
        ("or", f, t, true),
        ("or", f, f, false),
        ("xor", t, t, false),
        ("xor", t, f, true),
        ("xor", f, t, true),
        ("xor", f, f, false),
    ];
    for (op, left, right, expected) in cases {
        let got = reg
            .call("erlang", op, 2, &mut ctx, &[left, right])
            .expect(op);
        assert_eq!(truth(&atoms, got), expected, "{op} truth table");
    }
}

#[test]
fn a_non_boolean_operand_is_badarg() {
    let reg = default_registry();
    let mut heap = Heap::new(1024).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let t = ctx.atom("true").expect("seeded");
    let f = ctx.atom("false").expect("seeded");
    let ok = ctx.atom("ok").expect("seeded");
    let tuple = ctx.tuple(&[ok]).expect("fits");
    let float = ctx.make_float(1.0).expect("fits");

    // Oracle: `erlang:'not'(1)`, `erlang:'not'([])`, `erlang:'not'(atom)` and every
    // other non-boolean operand raise `error:badarg`.
    for bad in [ok, make_small(1), NIL, tuple, float] {
        assert!(
            matches!(
                reg.call("erlang", "not", 1, &mut ctx, &[bad]),
                Err(BeamError::BadArg)
            ),
            "erlang:'not'({bad:?}) must be badarg"
        );
        for good in [t, f] {
            for op in ["and", "or", "xor"] {
                assert!(
                    matches!(
                        reg.call("erlang", op, 2, &mut ctx, &[good, bad]),
                        Err(BeamError::BadArg)
                    ),
                    "erlang:'{op}'({good:?}, {bad:?}) must be badarg"
                );
                assert!(
                    matches!(
                        reg.call("erlang", op, 2, &mut ctx, &[bad, good]),
                        Err(BeamError::BadArg)
                    ),
                    "erlang:'{op}'({bad:?}, {good:?}) must be badarg"
                );
            }
        }
    }
}

#[test]
fn is_function_2_matches_the_oracle() {
    let reg = default_registry();
    let mut heap = Heap::new(1024).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // The storage must outlive every use of `fun0`/`fun2`.
    let (_storage0, fun0) = fun(0);
    let (_storage2, fun2) = fun(2);

    // Oracle: erlang:is_function(fun() -> ok end, 0) = true, (..., 1) = false;
    // erlang:is_function(fun(_X,_Y) -> ok end, 2) = true, (..., 0) = false.
    assert!(truth(
        &atoms,
        reg.call("erlang", "is_function", 2, &mut ctx, &[fun0, make_small(0)])
            .expect("arity 0")
    ));
    assert!(!truth(
        &atoms,
        reg.call("erlang", "is_function", 2, &mut ctx, &[fun0, make_small(1)])
            .expect("arity 1")
    ));
    assert!(truth(
        &atoms,
        reg.call("erlang", "is_function", 2, &mut ctx, &[fun2, make_small(2)])
            .expect("arity 2")
    ));
    assert!(!truth(
        &atoms,
        reg.call("erlang", "is_function", 2, &mut ctx, &[fun2, make_small(0)])
            .expect("arity 0")
    ));
    assert!(!truth(
        &atoms,
        reg.call("erlang", "is_function", 2, &mut ctx, &[fun2, make_small(3)])
            .expect("arity 3")
    ));

    // A non-fun is `false` for every valid arity (never `badarg`).
    for non_fun in [make_small(1), NIL, ctx.atom("ok").expect("seeded")] {
        assert!(!truth(
            &atoms,
            reg.call(
                "erlang",
                "is_function",
                2,
                &mut ctx,
                &[non_fun, make_small(0)]
            )
            .expect("a non-fun is false")
        ));
    }

    // Oracle: a *positive* bignum arity is valid but can never match (false), while a
    // *negative* bignum is `error:badarg` (`erl_is_function`, `erl_bif_op.c:250-256`).
    let big_pos = beam_codec::term::make_big(ctx.heap(), false, &[1, 1]).expect("the bignum fits");
    let big_neg = beam_codec::term::make_big(ctx.heap(), true, &[1, 1]).expect("the bignum fits");
    assert!(!truth(
        &atoms,
        reg.call("erlang", "is_function", 2, &mut ctx, &[fun0, big_pos])
            .expect("a positive bignum arity is false")
    ));
    assert!(matches!(
        reg.call("erlang", "is_function", 2, &mut ctx, &[fun0, big_neg]),
        Err(BeamError::BadArg)
    ));

    // Oracle: a negative small, a float, an atom and a list arity are all `badarg`.
    let float = ctx.make_float(0.0).expect("fits");
    for bad in [make_small(-1), float, ctx.atom("ok").expect("seeded"), NIL] {
        assert!(
            matches!(
                reg.call("erlang", "is_function", 2, &mut ctx, &[fun0, bad]),
                Err(BeamError::BadArg)
            ),
            "erlang:is_function(Fun, {bad:?}) must be badarg"
        );
    }
}

#[test]
fn is_record_1_2_and_3_match_the_oracle() {
    let reg = default_registry();
    let mut heap = Heap::new(1024).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let rec = ctx.tuple(&[a, make_small(1)]).expect("fits"); // {a, 1}
    let one = ctx.tuple(&[a]).expect("fits"); // {a}
    let empty = ctx.tuple(&[]).expect("fits"); // {}

    // `is_record/1` is the native-record test, so a tuple is never a record
    // (oracle: erlang:is_record({a,b}) = false).
    assert!(!truth(
        &atoms,
        reg.call("erlang", "is_record", 1, &mut ctx, &[rec])
            .expect("is_record/1")
    ));
    assert!(!truth(
        &atoms,
        reg.call("erlang", "is_record", 1, &mut ctx, &[a])
            .expect("is_record/1")
    ));

    // `is_record/2`: arity >= 1 and `element(1) =:= Tag`. The oracle answers (in
    // order) true, false, true, false, false, false.
    for (term, tag, expected) in [
        (rec, a, true),
        (rec, b, false),
        (one, a, true),
        (empty, a, false),
        (NIL, a, false),
        (make_small(1), a, false),
    ] {
        let got = reg
            .call("erlang", "is_record", 2, &mut ctx, &[term, tag])
            .expect("is_record/2");
        assert_eq!(truth(&atoms, got), expected, "is_record({term:?}, {tag:?})");
    }

    // `is_record/3` with a small-integer size: the exact tuple arity. The oracle
    // answers true, false, false, true, false, false.
    for (term, size, expected) in [
        (rec, 2, true),
        (rec, 1, false),
        (rec, 3, false),
        (one, 1, true),
        (empty, 0, false), // erlang:is_record({}, a, 0) = false
        (rec, -1, false),
    ] {
        let got = reg
            .call(
                "erlang",
                "is_record",
                3,
                &mut ctx,
                &[term, a, make_small(size)],
            )
            .expect("is_record/3");
        assert_eq!(
            truth(&atoms, got),
            expected,
            "is_record({term:?}, a, {size})"
        );
    }

    // `is_record/3` with an *atom* size is the native-record form; a tuple is not a
    // record, so it is `false` (oracle: erlang:is_record({a,b}, a, foo) = false).
    for term in [rec, NIL, make_small(1)] {
        assert!(!truth(
            &atoms,
            reg.call("erlang", "is_record", 3, &mut ctx, &[term, a, b])
                .expect("the native-record form")
        ));
    }

    // Oracle: a non-atom *tag* is `error:badarg` for both arities, and a size that is
    // neither a small integer nor an atom is `error:badarg`.
    let float = ctx.make_float(1.0).expect("fits");
    let big = beam_codec::term::make_big(ctx.heap(), false, &[1, 1]).expect("the bignum fits");
    for tag in [make_small(1), NIL, float, big] {
        assert!(
            matches!(
                reg.call("erlang", "is_record", 2, &mut ctx, &[rec, tag]),
                Err(BeamError::BadArg)
            ),
            "erlang:is_record({{a,1}}, {tag:?}) must be badarg"
        );
        assert!(
            matches!(
                reg.call(
                    "erlang",
                    "is_record",
                    3,
                    &mut ctx,
                    &[rec, tag, make_small(2)]
                ),
                Err(BeamError::BadArg)
            ),
            "erlang:is_record({{a,1}}, {tag:?}, 2) must be badarg"
        );
    }
    for size in [float, NIL, big] {
        assert!(
            matches!(
                reg.call("erlang", "is_record", 3, &mut ctx, &[rec, a, size]),
                Err(BeamError::BadArg)
            ),
            "erlang:is_record({{a,1}}, a, {size:?}) must be badarg"
        );
    }
}

#[test]
fn the_boolean_and_guard_operators_are_registered() {
    let reg = default_registry();
    for (name, arity) in [
        ("not", 1),
        ("and", 2),
        ("or", 2),
        ("xor", 2),
        ("is_function", 2),
        ("is_record", 1),
        ("is_record", 2),
        ("is_record", 3),
    ] {
        assert!(
            reg.contains("erlang", name, arity),
            "missing erlang:{name}/{arity}"
        );
        assert!(
            reg.lookup("erlang", name, arity).is_some(),
            "no impl for erlang:{name}/{arity}"
        );
    }

    // `andalso`/`orelse` are compiler-lowered to jumps, not BIFs — they must stay
    // unregistered.
    assert!(!reg.contains("erlang", "andalso", 2));
    assert!(!reg.contains("erlang", "orelse", 2));

    // A freshly built core registry carries them too.
    let core = BifRegistry::with_core_bifs();
    for (name, arity) in [("xor", 2), ("is_record", 3), ("is_function", 2)] {
        assert!(core.contains("erlang", name, arity), "{name}/{arity}");
    }
}
