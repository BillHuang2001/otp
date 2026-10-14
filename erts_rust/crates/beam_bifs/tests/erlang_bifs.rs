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

//! Integration tests for the `erlang`/`erts_internal` core BIFs and the public
//! reason-mapping API, all driven through [`BifRegistry::call`].
//!
//! Each test builds a real [`BifContext`] over `beam_alloc::Heap` + a seeded
//! `beam_term::AtomTable` and drives the registry the way `beam_rt` will. Results
//! are asserted structurally — immediate terms by value, structured terms with
//! `beam_term::compare::eq`, booleans by resolving the `true`/`false` atom through
//! the atom table — never by hand-rolled bit inspection.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, SendHook};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, make_small, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b", "normal", "foo", "bad_generator", "shutdown"]);
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

/// The `(class, reason)` of a raise returned by a BIF call.
fn raise_of(result: Result<Eterm>) -> (ExceptionClass, ExitReason) {
    match result {
        Err(BeamError::Raise(raise)) => (raise.class, raise.reason),
        other => panic!("expected a raise, got {other:?}"),
    }
}

/// The live reason term attached to a raise, if any — the opaque
/// `beam_common::Raise::live_reason` slot, whose payload is the reason `Eterm`.
fn live_reason_of(result: Result<Eterm>) -> Option<Eterm> {
    match result {
        Err(BeamError::Raise(raise)) => raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<Eterm>().copied()),
        other => panic!("expected a raise, got {other:?}"),
    }
}

/// Asserts `result` is a raise of `class` whose reason is `ExitReason::Structured`
/// and whose live reason term downcasts to an `Eterm` structurally equal to
/// `expected` (a non-atom reason: a tuple like `{bad_generator,5}`, a bare `42`, …).
#[track_caller]
fn assert_structured(
    result: Result<Eterm>,
    ctx: &BifContext<'_>,
    class: ExceptionClass,
    expected: Eterm,
) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, class, "class");
            assert_eq!(raise.reason, ExitReason::Structured, "reason");
            assert_eq!(raise.reason.as_atom(), None, "as_atom");
            let live = *raise
                .live_reason()
                .expect("a structured reason carries its live term")
                .downcast_ref::<Eterm>()
                .expect("the payload is the live reason term");
            assert!(
                beam_term::compare::eq(live, expected, ctx.atoms()),
                "live reason differs from the expected reason term"
            );
        }
        other => panic!("expected a structured raise, got {other:?}"),
    }
}

#[test]
fn arithmetic_bifs() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    assert_eq!(
        reg.call("erlang", "+", 2, &mut ctx, &[make_small(2), make_small(3)])
            .expect("2 + 3"),
        make_small(5)
    );
    assert_eq!(
        reg.call("erlang", "-", 2, &mut ctx, &[make_small(3), make_small(5)])
            .expect("3 - 5"),
        make_small(-2)
    );
    assert_eq!(
        reg.call("erlang", "*", 2, &mut ctx, &[make_small(4), make_small(6)])
            .expect("4 * 6"),
        make_small(24)
    );
    assert_eq!(
        reg.call("erlang", "div", 2, &mut ctx, &[make_small(7), make_small(2)])
            .expect("7 div 2"),
        make_small(3)
    );
    assert_eq!(
        reg.call("erlang", "rem", 2, &mut ctx, &[make_small(7), make_small(2)])
            .expect("7 rem 2"),
        make_small(1)
    );
    assert_eq!(
        reg.call("erlang", "abs", 1, &mut ctx, &[make_small(-5)])
            .expect("abs(-5)"),
        make_small(5)
    );

    // `/` always yields a float: 7 / 2 = 3.5.
    let quotient = reg
        .call("erlang", "/", 2, &mut ctx, &[make_small(7), make_small(2)])
        .expect("7 / 2");
    let three_and_a_half = ctx.make_float(3.5).expect("the float fits");
    assert!(beam_term::compare::eq(quotient, three_and_a_half, ctx.atoms()));

    // `1 + a` and `1 div 0` are `badarith`.
    let a = ctx.atom("a").expect("seeded");
    assert!(matches!(
        reg.call("erlang", "+", 2, &mut ctx, &[make_small(1), a]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(
        reg.call("erlang", "div", 2, &mut ctx, &[make_small(1), make_small(0)]),
        Err(BeamError::BadArith)
    ));
}

#[test]
fn comparison_bifs_and_cmp_term() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let one = make_small(1);
    let one_f = ctx.make_float(1.0).expect("the float fits");

    // Arithmetic vs. exact equality: `1 == 1.0` but not `1 =:= 1.0`.
    assert!(truth(&atoms, reg.call("erlang", "==", 2, &mut ctx, &[one, one_f]).expect("==")));
    assert!(!truth(&atoms, reg.call("erlang", "/=", 2, &mut ctx, &[one, one_f]).expect("/=")));
    assert!(!truth(&atoms, reg.call("erlang", "=:=", 2, &mut ctx, &[one, one_f]).expect("=:=")));
    assert!(truth(&atoms, reg.call("erlang", "=/=", 2, &mut ctx, &[one, one_f]).expect("=/=")));

    // Ordering is the arithmetic order: `1 < 1.0` is false, `1 =< 1.0` is true.
    assert!(!truth(&atoms, reg.call("erlang", "<", 2, &mut ctx, &[one, one_f]).expect("<")));
    assert!(truth(&atoms, reg.call("erlang", "=<", 2, &mut ctx, &[one, one_f]).expect("=<")));
    assert!(truth(&atoms, reg.call("erlang", ">=", 2, &mut ctx, &[one, one_f]).expect(">=")));
    assert!(!truth(&atoms, reg.call("erlang", ">", 2, &mut ctx, &[one, one_f]).expect(">")));

    // Cross-type order is the term order: an atom sorts before a tuple.
    let a = ctx.atom("a").expect("seeded");
    let tup = ctx.tuple(&[a]).expect("fits");
    assert!(truth(&atoms, reg.call("erlang", "<", 2, &mut ctx, &[a, tup]).expect("a < {a}")));
    assert!(truth(&atoms, reg.call("erlang", ">", 2, &mut ctx, &[tup, a]).expect("{a} > a")));

    // `erts_internal:cmp_term/2` is the exact order, returning -1 | 0 | 1.
    assert_eq!(
        reg.call("erts_internal", "cmp_term", 2, &mut ctx, &[one, one_f]).expect("cmp_term"),
        make_small(-1)
    );
    assert_eq!(
        reg.call("erts_internal", "cmp_term", 2, &mut ctx, &[one_f, one]).expect("cmp_term"),
        make_small(1)
    );
    assert_eq!(
        reg.call("erts_internal", "cmp_term", 2, &mut ctx, &[one, one]).expect("cmp_term"),
        make_small(0)
    );
}

#[test]
fn type_test_bifs() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let one = make_small(1);
    let a = ctx.atom("a").expect("seeded");
    let t_true = ctx.atom("true").expect("seeded");

    // `is_list([])` is true (the cons-cell predicate is not enough by itself).
    assert!(truth(&atoms, reg.call("erlang", "is_list", 1, &mut ctx, &[NIL]).expect("is_list")));
    assert!(truth(&atoms, reg.call("erlang", "is_integer", 1, &mut ctx, &[one]).expect("is_integer")));
    assert!(!truth(&atoms, reg.call("erlang", "is_float", 1, &mut ctx, &[one]).expect("is_float")));

    let one_f = ctx.make_float(1.0).expect("the float fits");
    assert!(truth(&atoms, reg.call("erlang", "is_float", 1, &mut ctx, &[one_f]).expect("is_float")));
    assert!(truth(&atoms, reg.call("erlang", "is_number", 1, &mut ctx, &[one_f]).expect("is_number")));

    // `is_pid(self())`.
    let self_pid = reg.call("erlang", "self", 0, &mut ctx, &[]).expect("self");
    assert!(truth(&atoms, reg.call("erlang", "is_pid", 1, &mut ctx, &[self_pid]).expect("is_pid")));

    // `is_boolean/1` accepts only the atoms `true`/`false`.
    assert!(truth(&atoms, reg.call("erlang", "is_boolean", 1, &mut ctx, &[t_true]).expect("is_boolean")));
    assert!(!truth(&atoms, reg.call("erlang", "is_boolean", 1, &mut ctx, &[a]).expect("is_boolean")));

    assert!(truth(&atoms, reg.call("erlang", "is_atom", 1, &mut ctx, &[a]).expect("is_atom")));
    let tup = ctx.tuple(&[a]).expect("fits");
    assert!(truth(&atoms, reg.call("erlang", "is_tuple", 1, &mut ctx, &[tup]).expect("is_tuple")));

    // The M1 term set has no binaries/maps/funs/ports/refs; the BIFs are total.
    for name in ["is_binary", "is_bitstring", "is_map", "is_function", "is_port", "is_reference"] {
        assert!(
            !truth(&atoms, reg.call("erlang", name, 1, &mut ctx, &[NIL]).unwrap_or_else(|e| panic!("{name}: {e:?}"))),
            "{name}([]) should be false"
        );
    }
}

#[test]
fn term_ops_bifs() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ten = make_small(10);
    let twenty = make_small(20);

    // element/2 is 1-based.
    let pair = ctx.tuple(&[ten, twenty]).expect("fits");
    assert_eq!(
        reg.call("erlang", "element", 2, &mut ctx, &[make_small(1), pair]).expect("element"),
        ten
    );
    assert_eq!(
        reg.call("erlang", "element", 2, &mut ctx, &[make_small(2), pair]).expect("element"),
        twenty
    );

    // tuple_size/1 and size/1 on a tuple.
    let triple = ctx
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("fits");
    assert_eq!(
        reg.call("erlang", "tuple_size", 1, &mut ctx, &[triple]).expect("tuple_size"),
        make_small(3)
    );
    assert_eq!(
        reg.call("erlang", "size", 1, &mut ctx, &[triple]).expect("size"),
        make_small(3)
    );

    // length/1, hd/1, tl/1.
    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let ab = ctx.list(&[a, b]).expect("fits");
    assert_eq!(
        reg.call("erlang", "length", 1, &mut ctx, &[ab]).expect("length"),
        make_small(2)
    );
    assert_eq!(reg.call("erlang", "hd", 1, &mut ctx, &[ab]).expect("hd"), a);
    let tail = reg.call("erlang", "tl", 1, &mut ctx, &[ab]).expect("tl");
    let expected_tail = ctx.list(&[b]).expect("fits");
    assert!(beam_term::compare::eq(tail, expected_tail, ctx.atoms()));

    // setelement/3 returns a new tuple; the original is unchanged.
    let replaced = reg
        .call("erlang", "setelement", 3, &mut ctx, &[make_small(1), pair, make_small(9)])
        .expect("setelement");
    // `setelement(1, {10,20}, 9)` is `{9,20}`.
    let expected_pair = ctx.tuple(&[make_small(9), twenty]).expect("fits");
    assert!(beam_term::compare::eq(replaced, expected_pair, ctx.atoms()));
    assert!(beam_term::compare::eq(pair, ctx.tuple(&[ten, twenty]).expect("fits"), ctx.atoms()));

    // list_to_tuple/1 and tuple_to_list/1 round-trip.
    let as_tuple = reg.call("erlang", "list_to_tuple", 1, &mut ctx, &[ab]).expect("list_to_tuple");
    let expected_tuple = ctx.tuple(&[a, b]).expect("fits");
    assert!(beam_term::compare::eq(as_tuple, expected_tuple, ctx.atoms()));
    let as_list = reg.call("erlang", "tuple_to_list", 1, &mut ctx, &[pair]).expect("tuple_to_list");
    let expected_list = ctx.list(&[ten, twenty]).expect("fits");
    assert!(beam_term::compare::eq(as_list, expected_list, ctx.atoms()));

    // The badarg cases.
    let one_elem = ctx.tuple(&[a]).expect("fits");
    assert!(matches!(
        reg.call("erlang", "element", 2, &mut ctx, &[make_small(0), one_elem]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("erlang", "hd", 1, &mut ctx, &[NIL]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("erlang", "tl", 1, &mut ctx, &[NIL]),
        Err(BeamError::BadArg)
    ));
    let improper = ctx.cons(a, b).expect("fits");
    assert!(matches!(
        reg.call("erlang", "length", 1, &mut ctx, &[improper]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn self_and_send_bifs() {
    let reg = default_registry();
    let mut heap = Heap::new(512).expect("test heap");
    let atoms = seeded_atoms();

    // `self/0` returns the injected pid.
    let pid = ProcessId::new(7, 3);
    let mut ctx = BifContext::new(&mut heap, &atoms, pid);
    assert_eq!(reg.call("erlang", "self", 0, &mut ctx, &[]).expect("self"), pid.to_eterm());

    // Without a `SendHook`, `send/2` reports `unsupported_bif` (never a silent drop).
    let dest = ProcessId::new(2, 0).to_eterm();
    assert_eq!(
        raise_of(reg.call("erlang", "send", 2, &mut ctx, &[dest, make_small(42)])),
        (ExceptionClass::Error, ExitReason::atom("unsupported_bif"))
    );
}

/// A `SendHook` that records every delivery (the public seam `beam_rt` implements).
#[derive(Default)]
struct RecordingSender {
    sent: Vec<(Eterm, Eterm)>,
}

impl SendHook for RecordingSender {
    fn send(&mut self, to: Eterm, term: Eterm) -> Result<()> {
        self.sent.push((to, term));
        Ok(())
    }
}

#[test]
fn send_with_a_hook_delivers_and_returns_the_message() {
    let reg = default_registry();
    let mut heap = Heap::new(512).expect("test heap");
    let atoms = seeded_atoms();
    let dest = ProcessId::new(2, 0).to_eterm();
    let msg = make_small(42);

    let mut sender = RecordingSender::default();
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_sender(&mut sender);
        assert_eq!(
            reg.call("erlang", "send", 2, &mut ctx, &[dest, msg]).expect("send"),
            msg
        );
        // `"!"/2` is the same implementation, spelled as the operator BIF.
        assert_eq!(
            reg.call("erlang", "!", 2, &mut ctx, &[dest, msg]).expect("!"),
            msg
        );
    }
    assert_eq!(sender.sent, vec![(dest, msg), (dest, msg)]);
}

#[test]
fn raise_bifs_set_the_class_and_public_builders_agree() {
    let reg = default_registry();
    let mut heap = Heap::new(256).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");

    // `error/1`, `exit/1` and `throw/1` set the error/exit/throw class.
    assert_eq!(
        raise_of(reg.call("erlang", "error", 1, &mut ctx, &[a])),
        (ExceptionClass::Error, ExitReason::atom("a"))
    );
    assert_eq!(
        raise_of(reg.call("erlang", "exit", 1, &mut ctx, &[a])),
        (ExceptionClass::Exit, ExitReason::atom("a"))
    );
    assert_eq!(
        raise_of(reg.call("erlang", "throw", 1, &mut ctx, &[a])),
        (ExceptionClass::Throw, ExitReason::atom("a"))
    );

    // `error/2` ignores its second argument in M1 (documented simplification).
    assert_eq!(
        raise_of(reg.call("erlang", "error", 2, &mut ctx, &[a, make_small(1)],)),
        (ExceptionClass::Error, ExitReason::atom("a"))
    );

    // An **atom** reason keeps its exact `ExitReason` and carries **no** live slot.
    assert_eq!(live_reason_of(reg.call("erlang", "error", 1, &mut ctx, &[a])), None);
    assert_eq!(live_reason_of(reg.call("erlang", "exit", 1, &mut ctx, &[a])), None);
    assert_eq!(live_reason_of(reg.call("erlang", "throw", 1, &mut ctx, &[a])), None);

    // A **non-atom** reason is `ExitReason::Structured` and carries the real reason
    // term in the opaque `Raise::live_reason` slot.
    // Oracle: erlang:error({bad_generator,5}) => error:{bad_generator,5}
    //   erl -noshell -eval 'try erlang:error({bad_generator,5}) catch C:R ->
    //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{bad_generator,5}
    let bad_generator = ctx.atom("bad_generator").expect("seeded");
    let tuple = ctx.tuple(&[bad_generator, make_small(5)]).expect("fits");
    assert_structured(
        reg.call("erlang", "error", 1, &mut ctx, &[tuple]),
        &ctx,
        ExceptionClass::Error,
        tuple,
    );
    // Oracle: erlang:throw(42) => throw:42.
    //   erl -noshell -eval 'try erlang:throw(42) catch C:R ->
    //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  throw:42
    assert_structured(
        reg.call("erlang", "throw", 1, &mut ctx, &[make_small(42)]),
        &ctx,
        ExceptionClass::Throw,
        make_small(42),
    );
    // Oracle: erlang:exit({shutdown,1}) => exit:{shutdown,1}.
    //   erl -noshell -eval 'try erlang:exit({shutdown,1}) catch C:R ->
    //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  exit:{shutdown,1}
    let shutdown = ctx.atom("shutdown").expect("seeded");
    let tuple = ctx.tuple(&[shutdown, make_small(1)]).expect("fits");
    assert_structured(
        reg.call("erlang", "exit", 1, &mut ctx, &[tuple]),
        &ctx,
        ExceptionClass::Exit,
        tuple,
    );

    // The public raise builders produce the same `Raise` values.
    let reason = ExitReason::atom("boom");
    for build in [
        beam_bifs::raise_error(reason.clone()),
        beam_bifs::raise_exit(reason.clone()),
        beam_bifs::raise_throw(reason.clone()),
    ] {
        match build {
            BeamError::Raise(raise) => assert_eq!(raise.reason, reason),
            other => panic!("expected a raise, got {other:?}"),
        }
    }
}

/// `erlang:raise/3` (`raise_3`, `bif.c:1549-1696`): a **non-atom** reason is
/// raised as the argument term itself (carried in the opaque
/// `Raise::live_reason` slot), while an invalid argument shape is **returned** as
/// the atom `badarg` — never raised.
///
/// Every expectation below is pinned against the OTP 29 oracle (`erl -noshell
/// -eval`, erts-17.1), which needs `try F() of X -> {returned,X} catch C:R ->
/// {raised,C,R} end` — a bare `catch` cannot observe the *returned* atom.
#[test]
fn raise_three_matches_the_oracle_reason_and_badarg_shapes() {
    let reg = default_registry();
    let mut heap = Heap::new(512).expect("test heap");
    let mut atoms = seeded_atoms();
    atoms.seed(["error", "exit", "throw", "badarg", "x"]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let error = ctx.atom("error").expect("seeded");
    let exit = ctx.atom("exit").expect("seeded");
    let throw = ctx.atom("throw").expect("seeded");
    let badarg = ctx.atom("badarg").expect("seeded");
    let normal = ctx.atom("normal").expect("seeded");
    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let x = ctx.atom("x").expect("seeded");
    let shutdown = ctx.atom("shutdown").expect("seeded");
    let foo = ctx.atom("foo").expect("seeded");
    let empty = ctx.list(&[]).expect("fits");

    // Oracle: raise(error,{a,b},[])         => error:{a,b}
    //         raise(exit,{shutdown,x},[])   => exit:{shutdown,x}
    //         raise(throw,{a},[])           => throw:{a}
    //         raise(error,42,[])            => error:42
    let pair = ctx.tuple(&[a, b]).expect("fits");
    assert_structured(
        reg.call("erlang", "raise", 3, &mut ctx, &[error, pair, empty]),
        &ctx,
        ExceptionClass::Error,
        pair,
    );
    let shutdown_pair = ctx.tuple(&[shutdown, x]).expect("fits");
    assert_structured(
        reg.call("erlang", "raise", 3, &mut ctx, &[exit, shutdown_pair, empty]),
        &ctx,
        ExceptionClass::Exit,
        shutdown_pair,
    );
    let single = ctx.tuple(&[a]).expect("fits");
    assert_structured(
        reg.call("erlang", "raise", 3, &mut ctx, &[throw, single, empty]),
        &ctx,
        ExceptionClass::Throw,
        single,
    );
    assert_structured(
        reg.call("erlang", "raise", 3, &mut ctx, &[error, make_small(42), empty]),
        &ctx,
        ExceptionClass::Error,
        make_small(42),
    );

    // An **atom** reason keeps its exact `ExitReason` and carries **no** live slot:
    // raise(error,foo,[]) => error:foo and raise(exit,normal,[]) => exit:normal.
    assert_eq!(
        raise_of(reg.call("erlang", "raise", 3, &mut ctx, &[error, foo, empty])),
        (ExceptionClass::Error, ExitReason::atom("foo"))
    );
    assert_eq!(
        live_reason_of(reg.call("erlang", "raise", 3, &mut ctx, &[error, foo, empty])),
        None
    );
    assert_eq!(
        raise_of(reg.call("erlang", "raise", 3, &mut ctx, &[exit, normal, empty])),
        (ExceptionClass::Exit, ExitReason::Normal)
    );
    assert_eq!(
        live_reason_of(reg.call("erlang", "raise", 3, &mut ctx, &[exit, normal, empty])),
        None
    );

    // An unknown class atom or a non-atom class is the **returned** atom `badarg`
    // (`bif.c:1694-1695` ends every failure path in `return am_badarg`).
    // Oracle: raise(foo,foo,[]) => {returned,badarg}; raise(1,foo,[]) => {returned,badarg}.
    for args in [[foo, foo, empty], [make_small(1), foo, empty]] {
        assert_eq!(
            reg.call("erlang", "raise", 3, &mut ctx, &args).expect("the badarg atom is returned"),
            badarg,
            "raise/3 {args:?}"
        );
    }
    // A stacktrace that is not a proper list of valid frames is `badarg` too.
    // Oracle: raise(error,foo,[42]) => {returned,badarg}; raise(error,foo,42) => the same.
    // (`[]` is a valid, empty stacktrace and raises normally.)
    let bad_frame = ctx.list(&[make_small(42)]).expect("fits");
    for trace in [bad_frame, make_small(42)] {
        assert_eq!(
            reg.call("erlang", "raise", 3, &mut ctx, &[error, foo, trace])
                .expect("the badarg atom is returned"),
            badarg,
            "stacktrace {trace:?}"
        );
    }
}

#[test]
fn reason_mapping_round_trips() {
    let mut atoms = seeded_atoms();

    // Atom term -> ExitReason: a well-known reason maps to its dedicated variant.
    let normal = beam_term::make_atom(atoms.intern("normal"));
    assert_eq!(beam_bifs::exit_reason_from_term(normal, &atoms), ExitReason::Normal);

    // ExitReason -> term -> ExitReason round-trips for `normal` and an arbitrary atom.
    for reason in [ExitReason::Normal, ExitReason::atom("foo")] {
        let term = beam_bifs::reason_term_from_exit_reason(&reason, &mut atoms).expect("an atom");
        assert!(beam_term::is_atom(term));
        assert_eq!(beam_bifs::exit_reason_from_term(term, &atoms), reason);
    }

    // A non-atom reason term is `Structured`; `Structured` has no term (documented gap).
    assert_eq!(beam_bifs::exit_reason_from_term(make_small(1), &atoms), ExitReason::Structured);
    assert!(matches!(
        beam_bifs::reason_term_from_exit_reason(&ExitReason::Structured, &mut atoms),
        Err(BeamError::BadArg)
    ));
}
