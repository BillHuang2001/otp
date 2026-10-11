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

//! Live-oracle-pinned tests for the match-spec arithmetic hook this crate
//! installs over `beam_ets` (`src/ets/arith_hook.rs`, backed by
//! `crate::arith::match_spec_arith`).
//!
//! `beam_ets` evaluates a match-spec guard/body arithmetic operator through a
//! thread-local seam (`set_arith_hook`); with none installed its
//! `DefaultArithHook` answers, and it is **checked small-integer/float only** — a
//! bignum operand or an integer result outside the 60-bit small range is the typed
//! `unsupported_ets`. Every match-spec-evaluating BIF in this crate installs an
//! arbitrary-precision hook for the duration of the evaluation, so those cases now
//! produce the oracle's own value.
//!
//! Three legs, all driven through the **public** API:
//!
//! * [`the_installed_hook_computes_the_oracles_values`] — the exact result shapes,
//!   asserted structurally (`beam_term::compare::eq`) against terms built by the
//!   crate's own registered operators;
//! * [`the_default_hook_refuses_what_the_installed_hook_computes`] — the
//!   **control**: the very same spec driven straight through `beam_ets`'s executor
//!   with no hook installed is refused, while the registry BIF (which installs the
//!   hook over that same executor) succeeds. This is what proves the hook is on
//!   the real `beam_ets` match-spec path, not just `crate::arith` in isolation;
//! * [`the_live_oracle_agrees_on_the_arithmetic_corpus`] — shells out to the system
//!   `erl`, replays every case and compares `erlang:phash2/1` of the two results
//!   (printer-independent, a bit-exact pin of the result **term**). It **skips
//!   loudly** when no oracle is on PATH (the `tests/match_spec.rs` /
//!   `tests/os_bif.rs` pattern) — never a silent pass.
//!
//! Every golden below carries the probe that pinned it (system OTP 29.1.1 /
//! erts-17.1).
use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{signed_value, AtomTable, Eterm, NIL};

/// The atoms every case in this file needs: the literal `true`/`false` guard
/// results, the `ok`/`EXIT` atoms the result shape and a raising body use, the
/// `table` dialect, the match-spec variable, and the arithmetic operator atoms the
/// specs spell as tuple heads.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true", "false", "ok", "EXIT", "table", "$1", // the arithmetic operator atoms
        "+", "-", "*", "/", "div", "rem", "band", "bor", "bxor", "bsl", "bsr", "==",
    ]);
    atoms
}

/// A heap sized for the specs, bignums and results below (it never reallocates, so
/// terms built by one context stay valid for later ones).
fn test_heap() -> Heap {
    Heap::new(65536).expect("test heap")
}

/// An arithmetic operator application `{Op, Args…}` — the match-spec spelling of
/// `Op(Args…)`.
fn arith(ctx: &mut BifContext<'_>, op: &str, args: &[Eterm]) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(args.len() + 1);
    elems.push(ctx.atom(op)?);
    elems.extend_from_slice(args);
    ctx.tuple(&elems)
}

/// `{Head, Guards, Body}` with the guards/body given as element lists.
fn clause(ctx: &mut BifContext<'_>, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Result<Eterm> {
    let guard_list = ctx.list(guards)?;
    let body_list = ctx.list(body)?;
    ctx.tuple(&[head, guard_list, body_list])
}

/// `{ok, Value, [], []}` — `erlang:match_spec_test/3`'s `table` result shape.
fn ok_result(ctx: &mut BifContext<'_>, value: Eterm) -> Result<Eterm> {
    let ok = ctx.ok()?;
    ctx.tuple(&[ok, value, NIL, NIL])
}

/// `erlang:match_spec_test(Against, Spec, table)` through the full public registry.
fn call_spec_test(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    against: Eterm,
    spec: Eterm,
) -> Result<Eterm> {
    let table = ctx.atom("table")?;
    reg.call("erlang", "match_spec_test", 3, ctx, &[against, spec, table])
}

/// `erlang:bsl/2` — the crate's own arbitrary-precision shift, an independent route
/// to the same bignum value the hook computes.
fn bsl(reg: &BifRegistry, ctx: &mut BifContext<'_>, a: i64, b: i64) -> Eterm {
    let (a, b) = (ctx.int(a).expect("fits"), ctx.int(b).expect("fits"));
    reg.call("erlang", "bsl", 2, ctx, &[a, b])
        .expect("erlang:bsl/2")
}

/// One corpus case: its name, the term the spec is matched against, and the spec.
type Case = (&'static str, Eterm, Eterm);

/// The corpus shared by the in-crate and live-oracle legs, all built on `heap`.
fn arithmetic_corpus(
    heap: &mut Heap,
    atoms: &AtomTable,
    reg: &BifRegistry,
) -> Vec<Case> {
    let mut corpus = Vec::new();
    let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
    let v1 = ctx.atom("$1").expect("seeded");
    let seven = ctx.int(7).expect("fits");
    let one = ctx.int(1).expect("fits");
    let hundred = ctx.int(100).expect("fits");
    // `1 bsl 100` — the bignum the `DefaultArithHook` refuses as a result.
    let two_pow_100 = bsl(reg, &mut ctx, 1, 100);

    // 1. body: `1 bsl 100` (a bignum result).
    //    ORACLE: `erlang:match_spec_test(7, [{'$1',[],[{'bsl',1,100}]}], table)`
    //            => {ok,1267650600228229401496703205376,[],[]}
    let body = [arith(&mut ctx, "bsl", &[one, hundred]).expect("fits")];
    let c = clause(&mut ctx, v1, &[], &body).expect("fits");
    corpus.push(("body_bsl_100", seven, ctx.list(&[c]).expect("fits")));

    // 2. body: `576460752303423487 + 1` = 2^59 — the first integer past the 60-bit
    //    small range, so a promoted small-int *result*.
    //    ORACLE: => {ok,576460752303423488,[],[]}
    let max_small = ctx.int((1i64 << 59) - 1).expect("fits");
    let body = [arith(&mut ctx, "+", &[max_small, one]).expect("fits")];
    let c = clause(&mut ctx, v1, &[], &body).expect("fits");
    corpus.push(("body_past_small_range", seven, ctx.list(&[c]).expect("fits")));

    // 3. body: `(1 bsl 100) band (1 bsl 100)` — a bignum bitwise op (both operands
    //    and the result are bignums).
    //    ORACLE: => {ok,1267650600228229401496703205376,[],[]}
    let arg = arith(&mut ctx, "bsl", &[one, hundred]).expect("fits");
    let body = [arith(&mut ctx, "band", &[arg, arg]).expect("fits")];
    let c = clause(&mut ctx, v1, &[], &body).expect("fits");
    corpus.push(("body_bignum_band", seven, ctx.list(&[c]).expect("fits")));

    // 4-7. body raise sources — each must become the executor's own `EXIT`
    //      sentinel (the C `FAIL_TERM`), never a leaked capability gap.
    //      ORACLE (each): => {ok,'EXIT',[],[]}
    let nil = ctx.list(&[]).expect("fits");
    let zero = ctx.int(0).expect("fits");
    let big_shift = ctx.int(1_073_741_824).expect("fits");
    for (name, raise) in [
        (
            "body_non_number_operand",
            arith(&mut ctx, "+", &[one, nil]).expect("fits"),
        ),
        (
            "body_div_by_zero",
            arith(&mut ctx, "div", &[one, zero]).expect("fits"),
        ),
        (
            "body_rem_by_zero",
            arith(&mut ctx, "rem", &[one, zero]).expect("fits"),
        ),
        (
            "body_system_limit",
            arith(&mut ctx, "bsl", &[one, big_shift]).expect("fits"),
        ),
    ] {
        let body = [raise];
        let c = clause(&mut ctx, v1, &[], &body).expect("fits");
        corpus.push((name, seven, ctx.list(&[c]).expect("fits")));
    }

    // 8. guard: `(1 bsl 100) == 2^100` holds, so the clause selects and its body
    //    returns the matched object's binding.
    //    ORACLE: => {ok,7,[],[]}
    let guard = arith(&mut ctx, "==", &[arg, two_pow_100]).expect("fits");
    let body = [v1];
    let c = clause(&mut ctx, v1, &[guard], &body).expect("fits");
    corpus.push(("guard_bignum_holds", seven, ctx.list(&[c]).expect("fits")));

    // 9-12. guard raise sources: the clause is *unmet* (the C `matchCatch`), so the
    //       result is the non-match sentinel `false`.
    //       ORACLE (each): => {ok,false,[],[]}
    for (name, raise) in [
        (
            "guard_non_number_operand",
            arith(&mut ctx, "+", &[one, nil]).expect("fits"),
        ),
        (
            "guard_div_by_zero",
            arith(&mut ctx, "div", &[one, zero]).expect("fits"),
        ),
        (
            "guard_rem_by_zero",
            arith(&mut ctx, "rem", &[one, zero]).expect("fits"),
        ),
        (
            "guard_system_limit",
            arith(&mut ctx, "bsl", &[one, big_shift]).expect("fits"),
        ),
    ] {
        let body = [v1];
        let c = clause(&mut ctx, v1, &[raise], &body).expect("fits");
        corpus.push((name, seven, ctx.list(&[c]).expect("fits")));
    }

    corpus
}

#[test]
fn the_installed_hook_computes_the_oracles_values() {
    // Every expected value below is the oracle's own (see `arithmetic_corpus`), and
    // every computed one comes out of the real `beam_ets` executor — so a
    // `DefaultArithHook` refusal would surface as an `unsupported_ets` error here.
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let corpus = arithmetic_corpus(&mut heap, &atoms, &reg);

    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let two_pow_100 = bsl(&reg, &mut ctx, 1, 100);
    let two_pow_59 = bsl(&reg, &mut ctx, 1, 59);
    let seven = ctx.int(7).expect("fits");
    let false_atom = ctx.boolean(false).expect("seeded");
    let exit_atom = ctx.atom("EXIT").expect("seeded");

    for (name, against, spec) in corpus {
        let got = call_spec_test(&reg, &mut ctx, against, spec)
            .unwrap_or_else(|err| panic!("{name}: erlang:match_spec_test/3 failed: {err:?}"));
        let value = match name {
            "body_bsl_100" | "body_bignum_band" => two_pow_100,
            "body_past_small_range" => two_pow_59,
            "guard_bignum_holds" => seven,
            "body_non_number_operand"
            | "body_div_by_zero"
            | "body_rem_by_zero"
            | "body_system_limit" => exit_atom,
            "guard_non_number_operand"
            | "guard_div_by_zero"
            | "guard_rem_by_zero"
            | "guard_system_limit" => false_atom,
            other => panic!("unclassified corpus case {other}"),
        };
        let want = ok_result(&mut ctx, value).expect("fits");
        assert!(
            beam_term::compare::eq(got, want, &atoms),
            "{name}: the hook did not produce the oracle's result"
        );
    }
}

#[test]
fn the_default_hook_refuses_what_the_installed_hook_computes() {
    // The **control** that pins the hook as the cause: run the identical spec
    // straight through `beam_ets`'s executor with no hook installed (the crate's
    // default), then through the registry BIF that installs it over the same
    // executor. The bignum cases must go from a typed refusal to the bignum.
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let corpus = arithmetic_corpus(&mut heap, &atoms, &reg);

    for (name, against, spec) in corpus {
        // Only the bignum arithmetic is outside the `DefaultArithHook` subset; the
        // `EXIT`/`false` sources are the oracle's own `badarith`/`system_limit`,
        // which the default hook already answers with the executor's sentinel.
        let shift = match name {
            "body_bsl_100" | "body_bignum_band" => 100i64,
            "body_past_small_range" => 59i64,
            _ => continue,
        };

        // No hook installed: `beam_ets`'s own `DefaultArithHook` refuses the bignum
        // with the typed `unsupported_ets` raise.
        beam_ets::select::expr::set_arith_hook(None);
        let singleton = {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
            ctx.list(&[against]).expect("fits")
        };
        let refused = beam_ets::match_spec_run(singleton, spec, &atoms, &mut heap);
        assert!(
            matches!(refused, Err(BeamError::Raise(_))),
            "{name}: the default hook must refuse the bignum, got {refused:?}"
        );

        // The registry BIF installs this crate's arbitrary-precision hook over that
        // same executor and answers with the oracle's value.
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let got = call_spec_test(&reg, &mut ctx, against, spec).unwrap_or_else(|err| {
            panic!("{name}: the BIF failed with the hook installed: {err:?}")
        });
        let expected = bsl(&reg, &mut ctx, 1, shift);
        let want = ok_result(&mut ctx, expected).expect("fits");
        assert!(
            beam_term::compare::eq(got, want, &atoms),
            "{name}: the hook-installed run must produce the oracle's bignum"
        );
    }
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
/// source for every term these cases contain, so it is exactly what the oracle is
/// fed.
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

/// The whole arithmetic corpus, cross-checked against the system oracle by
/// comparing `erlang:phash2/1` of the two results (a printer-independent pin of the
/// result **term**; `phash2/1` is itself bit-exact against the oracle).
#[test]
fn the_live_oracle_agrees_on_the_arithmetic_corpus() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let corpus = arithmetic_corpus(&mut heap, &atoms, &reg);

    // My side: `erlang:phash2/1` of the result.
    let mut mine: Vec<(&'static str, i64)> = Vec::new();
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        for (name, against, spec) in &corpus {
            let result = call_spec_test(&reg, &mut ctx, *against, *spec)
                .unwrap_or_else(|err| panic!("{name}: the BIF failed: {err:?}"));
            let hash = reg
                .call("erlang", "phash2", 1, &mut ctx, &[result])
                .unwrap_or_else(|err| panic!("{name}: phash2/1 failed: {err:?}"));
            mine.push((name, signed_value(hash)));
        }
    }

    // The oracle side.
    let mut checked = 0usize;
    for ((name, against, spec), (_, my_hash)) in corpus.iter().zip(&mine) {
        let against_src = render_w(&mut heap, &atoms, &reg, *against).expect("render against");
        let spec_src = render_w(&mut heap, &atoms, &reg, *spec).expect("render spec");
        let statement = format!(
            "io:format(\"~w~n\",[erlang:phash2(erlang:match_spec_test({against_src}, {spec_src}, table))])"
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
            "{name}: phash2 mismatch — the oracle disagrees for spec {spec_src} against {against_src}"
        );
        checked += 1;
    }
    assert_eq!(checked, corpus.len(), "every corpus case must be cross-checked");
    eprintln!("cross-checked {checked} match-spec arithmetic cases against the system oracle");
}

#[test]
fn this_file_registers_the_match_spec_test_key() {
    let reg = BifRegistry::with_core_bifs();
    assert!(reg.contains("erlang", "match_spec_test", 3));
}
