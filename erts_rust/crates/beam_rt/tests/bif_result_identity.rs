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
//! **The no-op BIF-result identity, on both routes** — the regression suite for the
//! copy-out short-circuit.
//!
//! # What is pinned
//!
//! `erts_debug:same/2` (`../beam_bifs/src/erts_internal.rs`, `same_2`) is **raw word
//! equality**, so it is the one observable that can tell a BIF result that was *handed through*
//! from the same value that was *copied*: OTP's `maps:put/3`, `maps:update/3` and
//! `maps:remove/2` answer the caller's own map when the operation is a no-op, and
//! `lists:reverse/1` answers `[]` for `[]` — the oracle then reports `true`, and a runtime that
//! re-copied the unchanged argument reports `false`. This suite runs the whole matrix under
//! **both** of the runtime's routes and cross-checks every row against the live `erl`:
//!
//! * the **`-eval`** route (`beam_rt -noshell -eval <expr>`) — the tree-walking evaluator, a
//!   **table-less** `RuntimeContext`, whose copy-out destination is the runtime-lifetime
//!   `Runtime::eval_heap` store (`../src/context/pool.rs::move_result_into_runtime_store`);
//! * the **compiled `run`** route (`beam_rt -noshell run <fixture>.beam m:f/0`) — a real
//!   scheduled process, whose destination is that process's own scanned young heap
//!   (`../src/context/calls.rs::move_result_into_process_heap`).
//!
//! Both destination questions are answered by the **one** containment definition in
//! `../src/context/calls/dest.rs` (`process_holds` / `store_holds` / `pool_holds`), reusing the
//! copier's own `Geometry::word_is_scanned` and the GC's `term_in_heap_block`; this suite is
//! what makes the two arms observably equal instead of only unit-tested.
//!
//! # Why a handed-through word is sound (the argument, once)
//!
//! A result that already stands on the region the caller's word is read from needs no copy:
//! on the process arm that region is the process's own young heap, which is scanned and
//! relocated by its collector — the handed-through word is a root like any other; on the
//! table-less arm it is `eval_heap`, a pointer-stable store nothing relocates. A container
//! the BIF *built* is a different matter: it stands on the pooled BIF frontier (`bif_heaps`),
//! which is rewound at the end of every call, so it takes the copy path unchanged — that is
//! what the two "…is a copy" rows below observe.
//!
//! # The literal form is NOT a usable case
//!
//! `erts_debug:same(maps:put(a, V, #{a => V}), #{a => V})` never exercises the copy-out: on
//! the `-eval` route the evaluator materializes each `#{a => V}` occurrence separately, so the
//! two maps are **distinct allocations** and both sides answer `false` (pinned in the `-eval`
//! test); in compiled code the compiler **shares** one literal term, so the answer is decided
//! by the `put_map_assoc` instruction's no-op rule instead of by this crate's door. Bound to a
//! variable it becomes the real no-op case (`put_bound`), which is what the matrix uses.
//!
//! # One documented cross-crate residual
//!
//! OTP's compiler rewrites a direct `maps:put/3` call into the **`put_map_assoc`
//! instruction**, which this runtime's engine implements without the C's no-op short-circuit
//! (`erts_maps_put` returns the input map when nothing changed). A compiled `M#{a => 1}` on a
//! map that already holds `a => 1` therefore answers `false` here where the oracle answers
//! `true`. That is `../beam_engine`'s instruction family, not this crate's door, so it is
//! pinned as a **characterization** row (`an_engine_opcode_gap_is_pinned`) rather than
//! asserted as parity — the fixture keeps `maps:put/3` a real `call_ext` by passing key and
//! value through a helper, which is why `put_bound` observes the BIF door on both routes.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
//!     --test bif_result_identity
//! ```
// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::path::Path;

/// The suite's banner in a loud skip.
const SUITE: &str = "bif_result_identity";

/// The fixture module compiled at test time by the system `erlc`.
const MODULE: &str = "bif_result_identity";

/// The fixture: one zero-arity function per row, all off the same shape.
///
/// `put_via_helper/3` exists so the compiler keeps `maps:put/3` a **`call_ext`**: a direct
/// `maps:put(K, V, M)` is rewritten into the `put_map_assoc` instruction (OTP's own maps
/// optimization), and that opcode's no-op behaviour is `beam_engine`'s (see the residual row
/// `opcode_put_bound/0`).
const FIXTURE: &str = r#"
-module(bif_result_identity).

-export([put_bound/0, put_changed/0, remove_absent/0, remove_present/0,
         reverse_of_empty/0, same_on_a_bound_term/0, binary_part_of_the_whole/0,
         fresh_list_is_a_copy/0, rebuilt_map_is_a_copy/0, opcode_put_bound/0,
         put_via_helper/3]).

%% A named 3-arity call: the compiler leaves this `maps:put/3` as a call_ext, so the
%% runtime's BIF door (not the `put_map_assoc` opcode) answers it.
put_via_helper(Key, Value, Map) -> maps:put(Key, Value, Map).

put_bound() ->
    Map = maps:from_list([{a, 1}]),
    erts_debug:same(put_via_helper(a, 1, Map), Map).

put_changed() ->
    Map = maps:from_list([{a, 1}]),
    erts_debug:same(put_via_helper(a, 2, Map), Map).

remove_absent() ->
    Map = maps:from_list([{a, 1}]),
    erts_debug:same(maps:remove(zz, Map), Map).

remove_present() ->
    Map = maps:from_list([{a, 1}]),
    erts_debug:same(maps:remove(a, Map), Map).

reverse_of_empty() ->
    erts_debug:same(lists:reverse([]), []).

same_on_a_bound_term() ->
    Term = maps:from_list([{N, N * N} || N <- lists:seq(1, 40)]),
    erts_debug:same(Term, Term).

binary_part_of_the_whole() ->
    Bin = <<1, 2, 3>>,
    erts_debug:same(binary:part(Bin, 0, byte_size(Bin)), Bin).

fresh_list_is_a_copy() ->
    erts_debug:same(lists:seq(1, 3), [1, 2, 3]).

rebuilt_map_is_a_copy() ->
    Map = maps:from_list([{a, 1}]),
    erts_debug:same(maps:filter(fun(_K, V) -> V =:= 1 end, Map), Map).

opcode_put_bound() ->
    Map = maps:from_list([{a, 1}]),
    erts_debug:same(Map#{a => 1}, Map).
"#;

/// One row of the matrix, run on **both** routes.
struct Row {
    /// The row's name in a failure message.
    name: &'static str,
    /// The `-eval` expression as a **sub-expression** (the suite adds the printer wrapper).
    eval: &'static str,
    /// The compiled fixture's zero-arity function for the same shape.
    fun: &'static str,
    /// The oracle's answer — what this runtime must reproduce on **both** routes.
    expected: &'static str,
}

/// The matrix. Every `expected` was measured on the oracle (OTP 29.1.2 / erts-17.1.1) and is
/// asserted unconditionally, so the suite still discriminates on a host without `erl`.
const ROWS: &[Row] = &[
    Row {
        name: "a bound no-op maps:put/3 hands the map through",
        eval: "M = #{a => 1}, erts_debug:same(maps:put(a, 1, M), M)",
        fun: "put_bound",
        expected: "true",
    },
    Row {
        name: "a changed maps:put/3 builds a new map",
        eval: "M = #{a => 1}, erts_debug:same(maps:put(a, 2, M), M)",
        fun: "put_changed",
        expected: "false",
    },
    Row {
        name: "removing an absent key hands the map through",
        eval: "M = #{a => 1}, erts_debug:same(maps:remove(zz, M), M)",
        fun: "remove_absent",
        expected: "true",
    },
    Row {
        name: "removing a present key builds a new map",
        eval: "M = #{a => 1}, erts_debug:same(maps:remove(a, M), M)",
        fun: "remove_present",
        expected: "false",
    },
    Row {
        name: "reversing the empty list answers the empty list",
        eval: "erts_debug:same(lists:reverse([]), [])",
        fun: "reverse_of_empty",
        expected: "true",
    },
    Row {
        name: "a bound term is identical to itself",
        eval: "T = maps:from_list([{N, N * N} || N <- lists:seq(1, 40)]), \
               erts_debug:same(T, T)",
        fun: "same_on_a_bound_term",
        expected: "true",
    },
    Row {
        name: "a whole binary:part/3 is a **new** sub-binary",
        eval: "B = <<1, 2, 3>>, erts_debug:same(binary:part(B, 0, byte_size(B)), B)",
        fun: "binary_part_of_the_whole",
        expected: "false",
    },
    // The two "the copy still happens" controls: a result the BIF **built** stands on the
    // pooled frontier, so it must leave a fresh copy (a blanket no-op short-circuit would
    // answer `true` here).
    Row {
        name: "a freshly built list is a copy, not the literal",
        eval: "erts_debug:same(lists:seq(1, 3), [1, 2, 3])",
        fun: "fresh_list_is_a_copy",
        expected: "false",
    },
    Row {
        name: "a rebuilt map is a copy, not the argument",
        eval: "M = #{a => 1}, erts_debug:same(maps:filter(fun(_K, V) -> V =:= 1 end, M), M)",
        fun: "rebuilt_map_is_a_copy",
        expected: "false",
    },
];

/// Runs `expr` on this runtime's **`-eval`** route and answers its trimmed stdout.
fn runtime_eval(expr: &str, code_dir: Option<&Path>) -> String {
    let mut args: Vec<&str> = vec!["-noshell"];
    if let Some(dir) = code_dir {
        args.push("-pa");
        args.push(
            dir.to_str()
                .unwrap_or_else(|| panic!("the fixture directory is not UTF-8: {}", dir.display())),
        );
    }
    args.push("-eval");
    args.push(expr);
    let observed = support::run_runtime(&args)
        .unwrap_or_else(|| panic!("the runtime did not answer {expr:?} within the probe bound"));
    assert_eq!(
        observed.code, 0,
        "the runtime must answer {expr:?} with exit 0\nstdout: {:?}\nstderr: {:?}",
        observed.stdout, observed.stderr
    );
    assert_eq!(
        observed.stderr, "",
        "a successful probe writes nothing on stderr: {expr:?}"
    );
    observed.stdout.trim().to_string()
}

/// Runs `expr` under the **oracle**, or `None` after the caller's loud skip when no `erl` is
/// installed.
fn ask_oracle(oracle: &Path, expr: &str, code_dir: Option<&Path>) -> Option<String> {
    match support::oracle_eval(oracle, code_dir, expr) {
        Some(answer) => Some(answer),
        None => {
            support::skip_loudly(SUITE, &format!("the oracle did not answer {expr:?}"));
            None
        }
    }
}

/// The `-eval` wrapper: bindings must sit inside `begin … end` because they land in
/// `io:format/2`'s argument list (the `-eval` grammar rejects `io:format("~w", [M = #{}])` on
/// both runtimes, so an unwrapped row would compare two `badarg`s).
fn eval_probe(row: &Row) -> String {
    format!(
        "io:format(\"~w~n\", [begin {} end]), halt(0).",
        row.eval
    )
}

/// The compiled route's probe expression (the oracle side; this runtime is driven by `run`).
fn compiled_probe(row: &Row) -> String {
    format!("io:format(\"~w~n\", [{MODULE}:{}()]), halt(0).", row.fun)
}

/// Runs the compiled fixture's `fun` through this runtime's **`run`** action.
fn runtime_run(fixture: &support::ErlangFixture, fun: &str) -> String {
    let beam = fixture.beam();
    let target = format!("{MODULE}:{fun}/0");
    let observed = support::run_runtime(&[
        "-noshell",
        "run",
        beam.to_str()
            .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display())),
        &target,
    ])
    .unwrap_or_else(|| panic!("the runtime did not finish `run {target}` within the probe bound"));
    assert_eq!(
        observed.code, 0,
        "`run {target}` must exit 0\nstdout: {:?}\nstderr: {:?}",
        observed.stdout, observed.stderr
    );
    observed.stdout.trim().to_string()
}

/// The `-eval` route: every row answers the oracle's value.
#[test]
fn the_eval_route_reproduces_the_oracles_no_op_identity() {
    let oracle = support::require_oracle(SUITE);
    for row in ROWS {
        let probe = eval_probe(row);
        let ours = runtime_eval(&probe, None);
        assert_eq!(
            ours, row.expected,
            "`-eval` must answer the oracle's {} for {}\nprobe: {probe}",
            row.expected, row.name
        );
        if let Some(oracle) = oracle.as_deref() {
            if let Some(theirs) = ask_oracle(oracle, &probe, None) {
                assert_eq!(
                    ours, theirs,
                    "the `-eval` route must agree with the live oracle for {}\nprobe: {probe}",
                    row.name
                );
            }
        }
    }
    // The **literal form** is not a usable no-op case, and on this route the reason is
    // visible: the evaluator materializes each `#{a => V}` occurrence separately, so the two
    // maps are distinct allocations and both sides answer `false`. (In compiled code the
    // compiler shares one literal term, which is why this row cannot be route-shared — it
    // would then observe `put_map_assoc`, not the BIF door.)
    let literal = "io:format(\"~w~n\", [begin V = 1, \
                   erts_debug:same(maps:put(a, V, #{a => V}), #{a => V}) end]), halt(0).";
    let ours = runtime_eval(literal, None);
    assert_eq!(
        ours, "false",
        "the literal form is two distinct map allocations on the `-eval` route"
    );
    if let Some(oracle) = oracle.as_deref() {
        if let Some(theirs) = ask_oracle(oracle, literal, None) {
            assert_eq!(ours, theirs, "the literal form is `false` on the oracle too");
        }
    }
}

/// The compiled `run` route: every row answers the oracle's value, on a real process whose
/// destination is its own young heap.
#[test]
fn the_compiled_route_reproduces_the_oracles_no_op_identity() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let Some(fixture) = support::ErlangFixture::compile(SUITE, "bif_result_identity", MODULE, FIXTURE)
    else {
        return;
    };
    for row in ROWS {
        let ours = runtime_run(&fixture, row.fun);
        assert_eq!(
            ours, row.expected,
            "`run {}` must answer the oracle's {} for {}\n(if this row regressed to `false` on a \
             no-op shape, check whether the fixture's `maps:put/3` call is still a call_ext)",
            row.fun, row.expected, row.name
        );
        let probe = compiled_probe(row);
        if let Some(theirs) = ask_oracle(&oracle, &probe, Some(fixture.dir())) {
            assert_eq!(
                ours, theirs,
                "the compiled route must agree with the live oracle for {}\nprobe: {probe}",
                row.name
            );
        }
    }
}

/// The documented **cross-crate residual**: OTP's `put_map_assoc` keeps the input map's
/// identity for a value-identical put (`erts_maps_put`), this runtime's engine does not — it
/// lives in `../beam_engine`'s map family, outside this crate's door.
///
/// Pinned as a characterization row so the difference is a recorded, visible fact rather than
/// a surprise: when the engine grows the short-circuit this test fails on the `false` and its
/// message says the gap closed.
#[test]
fn an_engine_opcode_gap_is_pinned() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let Some(fixture) = support::ErlangFixture::compile(SUITE, "bif_result_identity", MODULE, FIXTURE)
    else {
        return;
    };
    let ours = runtime_run(&fixture, "opcode_put_bound");
    let probe = compiled_probe(&Row {
        name: "a compiled value-identical map update (`put_map_assoc`)",
        eval: "",
        fun: "opcode_put_bound",
        expected: "",
    });
    let Some(theirs) = ask_oracle(&oracle, &probe, Some(fixture.dir())) else {
        return;
    };
    assert_eq!(
        theirs, "true",
        "the oracle keeps the map's identity for `M#{{a => 1}}` when `a => 1` is already there"
    );
    assert_eq!(
        ours, "false",
        "the `put_map_assoc` no-op short-circuit landed — `beam_engine`'s map family now matches \
         erts_maps_put: move this row into `ROWS` with `expected = \"true\"` and drop the \
         residual paragraph from this suite's docs"
    );
}
