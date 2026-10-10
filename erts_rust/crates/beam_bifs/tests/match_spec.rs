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

//! Oracle-pinned integration tests for `erlang:match_spec_test/3`, driven through
//! the **public** API only (`default_registry()` + `BifRegistry::call`).
//!
//! Every golden below carries the probe that pinned it (system OTP 29.1.1 /
//! erts-17.1). Two independent layers:
//!
//! * [`static_goldens`]-style tests compare the produced term against an
//!   explicitly built expectation with `beam_term::compare::eq` (atoms and small
//!   integers are read with `beam_term::{atom_val, signed_value}`);
//! * [`the_live_oracle_agrees_on_the_whole_corpus`] shells out to the system
//!   `erl`, replays every case and compares `erlang:phash2/1` of the two results
//!   (the hash is bit-exact against the oracle and printer-independent, so it
//!   pins the result **term** rather than a rendering). It **skips loudly** when
//!   the oracle is absent (the `tests/os_bif.rs` pattern).
use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{signed_value, AtomTable, Eterm, NIL};

/// The atoms every case in this file needs, plus the `error`/`ok` atoms the
/// result shapes use.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true", "false", "ok", "error", "warning", "table", "trace", "bogus",
        "_", "$_", "$$", "$1", "$2", "$3", "a", "b", "x", "bad", "foo",
        "seven", "other", "==", "is_integer",
    ]);
    atoms
}

/// A heap sized for the specs and results below (it never reallocates, so terms
/// built by one context stay valid for later ones).
fn test_heap() -> Heap {
    Heap::new(65536).expect("test heap")
}

/// Calls `erlang:match_spec_test/3` through the full public registry.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    term: Eterm,
    spec: Eterm,
    dialect: Eterm,
) -> Result<Eterm> {
    reg.call("erlang", "match_spec_test", 3, ctx, &[term, spec, dialect])
}

/// `true` when `err` is the `error:<reason>` raise.
fn is_raise_atom(err: &BeamError, reason: &'static str) -> bool {
    matches!(err, BeamError::Raise(raise) if raise.reason == beam_common::ExitReason::atom(reason))
}

/// `true` when `err` is the plain `badarg` failure (the engine raises it as
/// `error:badarg`).
fn is_badarg(err: &BeamError) -> bool {
    matches!(err, BeamError::BadArg)
}

/// A char list term (`"…"`) built byte by byte — the C's lint payload is an
/// **int list**, never a binary (`db_format_dmc_err_info`, line 3179).
fn char_list(ctx: &mut BifContext<'_>, text: &str) -> Result<Eterm> {
    let mut chars = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        chars.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&chars)
}

/// `{error, [{error, "Message"}]}` — the C's failed-compile result.
fn lint_result(ctx: &mut BifContext<'_>, message: &str) -> Result<Eterm> {
    let severity = ctx.atom("error")?;
    let text = char_list(ctx, message)?;
    let entry = ctx.tuple(&[severity, text])?;
    let errors = ctx.list(&[entry])?;
    let tag = ctx.atom("error")?;
    ctx.tuple(&[tag, errors])
}

/// `{Head, Guards, Body}` with the guards/body given as element lists.
fn clause(
    ctx: &mut BifContext<'_>,
    head: Eterm,
    guards: &[Eterm],
    body: &[Eterm],
) -> Result<Eterm> {
    let guard_list = ctx.list(guards)?;
    let body_list = ctx.list(body)?;
    ctx.tuple(&[head, guard_list, body_list])
}

// ---------------------------------------------------------------------------
// The table dialect: valid specs
// ---------------------------------------------------------------------------

#[test]
fn table_valid_specs_return_the_oracle_ok_shapes() {
    // ORACLE probes (`io:format("~w")` of `erlang:match_spec_test(T, S, table)`):
    //   (7, [{'$1',[],['$1']}])                          => {ok,7,[],[]}
    //   (7, [{'$1',[{'==','$1',7}],['$1']}])             => {ok,7,[],[]}
    //   (7, [{'$1',[{'==','$1',8}],['$1']}])             => {ok,false,[],[]}
    //   (7, [{'$1',[],[a,b]}])                           => {ok,b,[],[]}
    //   (8, [{7,[],[ok]}])                               => {ok,false,[],[]}
    //   ({a,1}, [{{a,'$1'},[],['$1']}])                  => {ok,1,[],[]}
    //   ({a,1}, [{{'$1','$2'},[],['$_']}])               => {ok,{a,1},[],[]}
    //   (9, [{'$1',[{'==','$1',7}],[seven]},{'$1',[],[other]}]) => {ok,other,[],[]}
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let table = ctx.atom("table").expect("seeded");
    let ok = ctx.atom("ok").expect("seeded");
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    let seven = ctx.int(7).expect("fits");
    let eight = ctx.int(8).expect("fits");
    let nine = ctx.int(9).expect("fits");
    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let eq = ctx.atom("==").expect("seeded");
    let seven_atom = ctx.atom("seven").expect("seeded");
    let other = ctx.atom("other").expect("seeded");
    let whole = ctx.atom("$_").expect("seeded");
    let guard_ok = ctx.tuple(&[eq, v1, seven]).expect("fits");

    // `[{'$1',[],['$1']}]` against 7 — the head binds everything, the body
    // returns it.
    let guards_empty = ctx.list(&[]).expect("fits");
    let body_v1 = ctx.list(&[v1]).expect("fits");
    let c = ctx.tuple(&[v1, guards_empty, body_v1]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, seven, spec, table).expect("match");
    let want = ctx.tuple(&[ok, seven, NIL, NIL]).expect("fits");
    assert!(beam_term::compare::eq(got, want, &atoms), "{{ok,7,[],[]}}");

    // The same clause with a holding guard.
    let guards_ok = ctx.list(&[guard_ok]).expect("fits");
    let c = ctx.tuple(&[v1, guards_ok, body_v1]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, seven, spec, table).expect("match");
    assert!(
        beam_term::compare::eq(got, want, &atoms),
        "{{ok,7,[],[]}} with a guard"
    );

    // A failing guard is the C's non-match sentinel: the atom `false`.
    let guard_fail = ctx.tuple(&[eq, v1, eight]).expect("fits");
    let guards_fail = ctx.list(&[guard_fail]).expect("fits");
    let c = ctx.tuple(&[v1, guards_fail, body_v1]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, seven, spec, table).expect("match");
    let false_atom = ctx.atom("false").expect("seeded");
    let want = ctx.tuple(&[ok, false_atom, NIL, NIL]).expect("fits");
    assert!(
        beam_term::compare::eq(got, want, &atoms),
        "{{ok,false,[],[]}} on a failed guard"
    );

    // A body is a sequence: the value of the **last** expression is returned.
    let body_ab = ctx.list(&[a, b]).expect("fits");
    let c = ctx.tuple(&[v1, guards_empty, body_ab]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, seven, spec, table).expect("match");
    let want = ctx.tuple(&[ok, b, NIL, NIL]).expect("fits");
    assert!(beam_term::compare::eq(got, want, &atoms), "{{ok,b,[],[]}}");

    // A head that cannot match (the literal `7` against the integer `8`).
    let body_ok = ctx.list(&[ok]).expect("fits");
    let c = ctx.tuple(&[seven, guards_empty, body_ok]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, eight, spec, table).expect("match");
    let want = ctx.tuple(&[ok, false_atom, NIL, NIL]).expect("fits");
    assert!(
        beam_term::compare::eq(got, want, &atoms),
        "{{ok,false,[],[]}} on a non-matching head"
    );

    // A tuple head that binds its second element.
    let pair_obj = ctx.tuple(&[a, seven]).expect("fits");
    let tup_head = ctx.tuple(&[a, v1]).expect("fits");
    let c = ctx.tuple(&[tup_head, guards_empty, body_v1]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, pair_obj, spec, table).expect("match");
    let want = ctx.tuple(&[ok, seven, NIL, NIL]).expect("fits");
    assert!(
        beam_term::compare::eq(got, want, &atoms),
        "{{ok,1,[],[]}} from a tuple head"
    );

    // `'$_'` in a body is the whole matched object.
    let head_pair = ctx.tuple(&[v1, v2]).expect("fits");
    let body_whole = ctx.list(&[whole]).expect("fits");
    let c = ctx.tuple(&[head_pair, guards_empty, body_whole]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, pair_obj, spec, table).expect("match");
    let want = ctx.tuple(&[ok, pair_obj, NIL, NIL]).expect("fits");
    assert!(
        beam_term::compare::eq(got, want, &atoms),
        "{{ok,{{a,1}},[],[]}} from an '$_' body"
    );

    // Two clauses: only the second guard holds, so its body value is returned.
    let body_seven = ctx.list(&[seven_atom]).expect("fits");
    let body_other = ctx.list(&[other]).expect("fits");
    let c1 = ctx.tuple(&[v1, guards_ok, body_seven]).expect("fits");
    let c2 = ctx.tuple(&[v1, guards_empty, body_other]).expect("fits");
    let spec = ctx.list(&[c1, c2]).expect("fits");
    let got = call(&reg, &mut ctx, nine, spec, table).expect("match");
    let want = ctx.tuple(&[ok, other, NIL, NIL]).expect("fits");
    assert!(
        beam_term::compare::eq(got, want, &atoms),
        "{{ok,other,[],[]}} from the second clause"
    );
}

#[test]
fn table_all_bindings_body_is_the_oracle_list() {
    // ORACLE: `erlang:match_spec_test({a,1}, [{{'$1','$2'},[],['$$']}], table)`
    // is `{ok,[a,1],[],[]}` — `'$$'` is the bound values in **ascending variable
    // order**, so `{{'$2','$1'},[],['$$']}` is `{ok,[1,a],[],[]}` (both probed).
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let table = ctx.atom("table").expect("seeded");
    let ok = ctx.atom("ok").expect("seeded");
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    let all = ctx.atom("$$").expect("seeded");
    let a = ctx.atom("a").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let pair_obj = ctx.tuple(&[a, one]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[all]).expect("fits");

    let head = ctx.tuple(&[v1, v2]).expect("fits");
    let c = ctx.tuple(&[head, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, pair_obj, spec, table).expect("match");
    let bindings = ctx.list(&[a, one]).expect("fits");
    let want = ctx.tuple(&[ok, bindings, NIL, NIL]).expect("fits");
    assert!(
        beam_term::compare::eq(got, want, &atoms),
        "{{ok,[a,1],[],[]}} for '${{$}}'"
    );

    // The reordered head binds `'$2'` to `a` and `'$1'` to `1`, and the body
    // still renders `[1,a]` (ascending by index).
    let head = ctx.tuple(&[v2, v1]).expect("fits");
    let c = ctx.tuple(&[head, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let got = call(&reg, &mut ctx, pair_obj, spec, table).expect("match");
    let bindings = ctx.list(&[one, a]).expect("fits");
    let want = ctx.tuple(&[ok, bindings, NIL, NIL]).expect("fits");
    assert!(
        beam_term::compare::eq(got, want, &atoms),
        "{{ok,[1,a],[],[]}} for a reordered head"
    );
}

// ---------------------------------------------------------------------------
// The table dialect: the reproduced structural lint diagnostics
// ---------------------------------------------------------------------------

#[test]
fn table_structural_lint_errors_are_the_oracle_errors() {
    // ORACLE probes (the `~w` of the 2-tuple; the payload is an int list):
    //   (7, bad)      => {error,[{error,"Match programs are not in a list."}]}
    //   (7, [])       => {error,[{error,"Match programs are not in a list."}]}
    //   (7, [{'$1',[],['$1']}|x]) => {error,[{error,"Match programs are not in a
    //                                     proper list."}]}
    //   (7, [bad])    => {error,[{error,"Match program part is not a tuple of
    //                                     arity 3."}]}
    //   (7, [{'$1',[],['$1']},bad]) => the same message.
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let table = ctx.atom("table").expect("seeded");
    let seven = ctx.int(7).expect("fits");
    let v1 = ctx.atom("$1").expect("seeded");
    let bad = ctx.atom("bad").expect("seeded");
    let x = ctx.atom("x").expect("seeded");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[v1]).expect("fits");
    let c = ctx.tuple(&[v1, guards, body]).expect("fits");

    for (name, spec, message) in [
        (
            "a non-list spec",
            bad,
            "Match programs are not in a list.",
        ),
        ("[] is not a list", NIL, "Match programs are not in a list."),
        (
            "an improper list",
            ctx.cons(c, x).expect("fits"),
            "Match programs are not in a proper list.",
        ),
        (
            "a clause that is not a 3-tuple",
            ctx.list(&[bad]).expect("fits"),
            "Match program part is not a tuple of arity 3.",
        ),
        (
            "a later clause that is not a 3-tuple",
            ctx.list(&[c, bad]).expect("fits"),
            "Match program part is not a tuple of arity 3.",
        ),
    ] {
        let got = call(&reg, &mut ctx, seven, spec, table).expect("lint result");
        let want = lint_result(&mut ctx, message).expect("fits");
        assert!(
            beam_term::compare::eq(got, want, &atoms),
            "{name}: expected the oracle's {message:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[test]
fn a_type_other_than_table_or_trace_is_badarg() {
    // ORACLE: `erlang:match_spec_test(7, [{'$1',[],['$1']}], bogus)` and
    //         `erlang:match_spec_test(7, [{'$1',[],['$1']}], 42)` both raise
    //         `error:badarg` (`erl_db_util.c:6062`).
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let seven = ctx.int(7).expect("fits");
    let v1 = ctx.atom("$1").expect("seeded");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[v1]).expect("fits");
    let c = ctx.tuple(&[v1, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");

    let bogus = ctx.atom("bogus").expect("seeded");
    let err = call(&reg, &mut ctx, seven, spec, bogus).expect_err("bogus type");
    assert!(is_badarg(&err), "bogus type: {err:?}");

    let forty_two = ctx.int(42).expect("fits");
    let err = call(&reg, &mut ctx, seven, spec, forty_two).expect_err("non-atom type");
    assert!(is_badarg(&err), "non-atom type: {err:?}");

    // The dialect is checked **before** the spec: a bad type with an invalid
    // spec is still `badarg`, never a lint tuple (the C compares argument 3
    // first, `erl_db_util.c:6051-6062`).
    let err = call(&reg, &mut ctx, seven, bogus, bogus).expect_err("both bad");
    assert!(is_badarg(&err), "bad type and spec: {err:?}");
}

// ---------------------------------------------------------------------------
// The honest capability gaps
// ---------------------------------------------------------------------------

#[test]
fn the_trace_dialect_is_a_typed_gap() {
    // ORACLE: `erlang:match_spec_test([7,8], [{'$1',[],['$1']}], trace)` is
    //         `{ok,true,[],[]}` and `erlang:match_spec_test(7, …, trace)` is
    //         `error:badarg`. The trace dialect needs the `DCOMP_TRACE` programme
    //         and the `erts_match_set_run_trace` runner, neither of which this
    //         runtime has — so the whole dialect is refused, for a matching term
    //     and a non-matching one alike, rather than guessed at.
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let trace = ctx.atom("trace").expect("seeded");
    let seven = ctx.int(7).expect("fits");
    let v1 = ctx.atom("$1").expect("seeded");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[v1]).expect("fits");
    let c = ctx.tuple(&[v1, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");

    // A **scalar** `Against` is refused by the C's argument check *before* the
    // dialect is compiled (`if (trace && !(is_list(against) || against == NIL))`,
    // `erl_db_util.c:6078-6080`), so it is `error:badarg`, not the typed gap; only
    // the list form below reaches the (still missing) trace runner.
    let err = call(&reg, &mut ctx, seven, spec, trace).expect_err("trace");
    assert!(is_badarg(&err), "trace scalar: {err:?}");

    let list = ctx.list(&[seven]).expect("fits");
    let err = call(&reg, &mut ctx, list, spec, trace).expect_err("trace list");
    assert!(is_raise_atom(&err, "unsupported_bif"), "trace list: {err:?}");
}

#[test]
fn semantic_lint_errors_are_a_typed_gap() {
    // ORACLE: `erlang:match_spec_test(7, [{'$1',[],['$2']}], table)` is
    //   `{error,[{error,"Variable $2 is unbound."}]}` and
    //   `erlang:match_spec_test(7, [{'$1',[{foo,'$1'}],['$1']}], table)` is
    //   `{error,[{error,"Function foo/1 does_not_exist."}]}`.
    //
    // The C's semantic diagnostics are a large, term-embedding, accumulating set
    // (see the module docs); rebuilding them faithfully is a separate port, so
    // the runtime reports the typed capability gap instead of guessing a message
    // the oracle might not print.
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let table = ctx.atom("table").expect("seeded");
    let seven = ctx.int(7).expect("fits");
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    let foo = ctx.atom("foo").expect("seeded");
    let guards = ctx.list(&[]).expect("fits");

    // An unbound `'$2'` in the body.
    let body = ctx.list(&[v2]).expect("fits");
    let c = ctx.tuple(&[v1, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let err = call(&reg, &mut ctx, seven, spec, table).expect_err("unbound var");
    assert!(is_raise_atom(&err, "unsupported_bif"), "unbound: {err:?}");

    // An unknown guard function.
    let call_term = ctx.tuple(&[foo, v1]).expect("fits");
    let guards = ctx.list(&[call_term]).expect("fits");
    let body = ctx.list(&[v1]).expect("fits");
    let c = ctx.tuple(&[v1, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let err = call(&reg, &mut ctx, seven, spec, table).expect_err("unknown guard");
    assert!(is_raise_atom(&err, "unsupported_bif"), "guard: {err:?}");
}

#[test]
fn a_head_pattern_with_a_map_is_a_typed_gap() {
    // ORACLE probes (all `table`, all mismatched by a literal `=:=` comparison):
    //   (#{a => 1, b => 2}, [{#{a => 1}, [], [ok]}])       => {ok,ok,[],[]}
    //   (#{a => 1}, [{#{a => '$2'}, [], ['$2']}])          => {ok,1,[],[]}
    //   (#{a => 1}, [{#{'$1' => '$2'}, [], ['$1']}])       => {error,[{error,
    //                                      "Variable found in map key."}]}
    //   ({x, #{a => 1}}, [{{x, #{a => 1}}, [], [ok]}])     => {ok,ok,[],[]}
    // `beam_ets::pattern` compares a head map as an exact literal, so every one
    // of these would be answered *wrongly* — the runtime refuses instead.
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let table = ctx.atom("table").expect("seeded");
    let v1 = ctx.atom("$1").expect("seeded");
    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let x = ctx.atom("x").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[v1]).expect("fits");
    let obj = ctx.tuple(&[x, v1]).expect("fits");

    // `#{a => '$1'}` — a map **value** is a binding sub-pattern for the C.
    let map_head = beam_codec::term::make_flatmap(ctx.heap(), &[(a, v1)]).expect("fits");
    let c = ctx.tuple(&[map_head, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let err = call(&reg, &mut ctx, obj, spec, table).expect_err("map value pattern");
    assert!(is_raise_atom(&err, "unsupported_bif"), "map value: {err:?}");

    // `#{a => 1}` — even an all-literal map may under-match (the C matches a
    // superset object).
    let map_head = beam_codec::term::make_flatmap(ctx.heap(), &[(a, one)]).expect("fits");
    let c = ctx.tuple(&[map_head, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let err = call(&reg, &mut ctx, obj, spec, table).expect_err("literal map head");
    assert!(is_raise_atom(&err, "unsupported_bif"), "literal map: {err:?}");

    // A map nested inside a tuple head is caught too.
    let inner = beam_codec::term::make_flatmap(ctx.heap(), &[(b, one)]).expect("fits");
    let nested_head = ctx.tuple(&[x, inner]).expect("fits");
    let c = ctx.tuple(&[nested_head, guards, body]).expect("fits");
    let spec = ctx.list(&[c]).expect("fits");
    let err = call(&reg, &mut ctx, obj, spec, table).expect_err("nested map head");
    assert!(is_raise_atom(&err, "unsupported_bif"), "nested map: {err:?}");
}

#[test]
fn this_module_registers_its_key() {
    let reg = BifRegistry::with_core_bifs();
    assert!(reg.contains("erlang", "match_spec_test", 3));
}

// ---------------------------------------------------------------------------
// The live differential cross-check
// ---------------------------------------------------------------------------

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

/// Renders `term` with `~w` onto a capture sink. `~w` output is valid Erlang
/// source for every term these cases contain, so it is exactly what the oracle
/// is fed.
fn render_w(heap: &mut Heap, atoms: &AtomTable, reg: &BifRegistry, term: Eterm) -> Option<String> {
    let mut sink = BytesSink::new();
    {
        let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let tilde = ctx.int(126).ok()?;
        let w = ctx.int(119).ok()?;
        let fmt = ctx.list(&[tilde, w]).ok()?;
        let args = ctx.list(&[term]).ok()?;
        reg.call("io", "format", 2, &mut ctx, &[fmt, args]).ok()?;
    }
    Some(sink.as_str())
}

/// The whole corpus, cross-checked against the system oracle by comparing
/// `erlang:phash2/1` of the two results (a printer-independent pin of the result
/// **term**; `phash2/1` is itself bit-exact against the oracle).
#[test]
fn the_live_oracle_agrees_on_the_whole_corpus() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();

    // Build the corpus.
    let mut corpus: Vec<(&'static str, Eterm, Eterm)> = Vec::new();
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let v1 = ctx.atom("$1").expect("seeded");
        let v2 = ctx.atom("$2").expect("seeded");
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let x = ctx.atom("x").expect("seeded");
        let bad = ctx.atom("bad").expect("seeded");
        let seven = ctx.int(7).expect("fits");
        let eight = ctx.int(8).expect("fits");
        let nine = ctx.int(9).expect("fits");
        let one = ctx.int(1).expect("fits");
        let eq = ctx.atom("==").expect("seeded");
        let seven_atom = ctx.atom("seven").expect("seeded");
        let other = ctx.atom("other").expect("seeded");
        let whole = ctx.atom("$_").expect("seeded");
        let all = ctx.atom("$$").expect("seeded");

        // 1. `[{'$1',[],['$1']}]` — a match.
        let c = clause(&mut ctx, v1, &[], &[v1]).expect("fits");
        corpus.push(("match", seven, ctx.list(&[c]).expect("fits")));

        // 2/3. A holding and a failing guard.
        let ok_guard = ctx.tuple(&[eq, v1, seven]).expect("fits");
        let c = clause(&mut ctx, v1, &[ok_guard], &[v1]).expect("fits");
        corpus.push(("guard_holds", seven, ctx.list(&[c]).expect("fits")));
        let bad_guard = ctx.tuple(&[eq, v1, eight]).expect("fits");
        let c = clause(&mut ctx, v1, &[bad_guard], &[v1]).expect("fits");
        corpus.push(("guard_fails", seven, ctx.list(&[c]).expect("fits")));

        // 4. A multi-expression body returns the last value.
        let c = clause(&mut ctx, v1, &[], &[a, b]).expect("fits");
        corpus.push(("body_sequence", seven, ctx.list(&[c]).expect("fits")));

        // 5. A non-matching head.
        let c = clause(&mut ctx, seven, &[], &[a]).expect("fits");
        corpus.push(("non_matching_head", eight, ctx.list(&[c]).expect("fits")));

        // 6. A tuple head that binds.
        let pair_obj = ctx.tuple(&[a, one]).expect("fits");
        let head = ctx.tuple(&[a, v1]).expect("fits");
        let c = clause(&mut ctx, head, &[], &[v1]).expect("fits");
        corpus.push(("tuple_head", pair_obj, ctx.list(&[c]).expect("fits")));

        // 7. `'$_'` and 8. `'$$'`.
        let head = ctx.tuple(&[v1, v2]).expect("fits");
        let c = clause(&mut ctx, head, &[], &[whole]).expect("fits");
        corpus.push(("whole_object", pair_obj, ctx.list(&[c]).expect("fits")));
        let c = clause(&mut ctx, head, &[], &[all]).expect("fits");
        corpus.push(("all_bindings", pair_obj, ctx.list(&[c]).expect("fits")));

        // 9. Two clauses.
        let c1 = clause(&mut ctx, v1, &[ok_guard], &[seven_atom]).expect("fits");
        let c2 = clause(&mut ctx, v1, &[], &[other]).expect("fits");
        corpus.push(("two_clauses", nine, ctx.list(&[c1, c2]).expect("fits")));

        // 10-13. The structural lint diagnostics.
        corpus.push(("lint_atom_spec", seven, bad));
        corpus.push(("lint_nil_spec", seven, NIL));
        let c = clause(&mut ctx, v1, &[], &[v1]).expect("fits");
        corpus.push((
            "lint_improper_spec",
            seven,
            ctx.cons(c, x).expect("fits"),
        ));
        corpus.push(("lint_bad_clause", seven, ctx.list(&[bad]).expect("fits")));
    }

    // My side: `erlang:phash2/1` of the result.
    let mut mine: Vec<(&'static str, i64)> = Vec::new();
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let table = ctx.atom("table").expect("seeded");
        for (name, term, spec) in &corpus {
            let result = call(&reg, &mut ctx, *term, *spec, table)
                .unwrap_or_else(|err| panic!("{name}: the BIF failed: {err:?}"));
            let hash = reg
                .call("erlang", "phash2", 1, &mut ctx, &[result])
                .unwrap_or_else(|err| panic!("{name}: phash2/1 failed: {err:?}"));
            mine.push((name, signed_value(hash)));
        }
    }

    // The oracle side.
    let mut checked = 0usize;
    for ((name, term, spec), (_, my_hash)) in corpus.iter().zip(&mine) {
        let term_src = render_w(&mut heap, &atoms, &reg, *term).expect("render term");
        let spec_src = render_w(&mut heap, &atoms, &reg, *spec).expect("render spec");
        let statement = format!(
            "io:format(\"~w~n\",[erlang:phash2(erlang:match_spec_test({term_src}, {spec_src}, table))])"
        );
        let Some(stdout) = oracle_eval(&statement) else {
            eprintln!("SKIP: {name}: the oracle is unavailable");
            return;
        };
        let oracle_hash: i64 = match stdout.trim().parse() {
            Ok(value) => value,
            Err(err) => {
                eprintln!("SKIP: {name}: unparsable oracle output {stdout:?} ({err})");
                return;
            }
        };
        assert_eq!(
            *my_hash, oracle_hash,
            "{name}: phash2 mismatch — the oracle disagrees for term {term_src} / spec {spec_src}"
        );
        checked += 1;
    }
    assert_eq!(checked, corpus.len(), "every corpus case must be cross-checked");
    eprintln!("cross-checked {checked} cases against the system oracle");
}
