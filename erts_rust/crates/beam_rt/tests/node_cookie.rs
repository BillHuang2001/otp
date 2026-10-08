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

//! The `erlang` node/cookie identity surface (contract **A.5**) at the executable boundary.
//!
//! The runtime-native floor (`beam_rt::native`, `src/native/node.rs`) serves `is_alive/0`,
//! `get_cookie/0`, `set_cookie/2` and `nodes/0,1` from this runtime's own node table. This
//! suite pins their **observable** answers through the real `beam_rt` binary and cross-checks
//! every one of them against the live system Erlang.
//!
//! # Why two probe forms
//!
//! The differential probe -- `try (E) of V -> io:format("ok ~w~n", [V]), halt(0) catch C:R ->
//! io:format("exception ~w ~w~n", [C, R]), halt(1) end.` -- is the corpus's own. It pins the
//! exception rendering *and* the exit code, which is exactly what this family's raises are
//! about (`tools/differential`, `corpus::wire_ids::node_identity_cases`).
//!
//! The shared `support::oracle_eval` helper only collects a probe that exits `0` (it hands
//! the child `-s init stop` and considers a non-zero exit a failure), so the **oracle
//! comparison** uses a second, exit-0 form of the same call -- `io:format("~w~n", [try (E)
//! catch C:R -> {C,R} end])` -- in which an uncaught raise renders as `{Class,Reason}`. Every
//! golden below is the **oracle's** own line, re-measured against the live system Erlang, and
//! the suite additionally re-measures it at run time whenever `erl` is present.
//!
//! A missing `erl` is a **loud skip** of the oracle half only; the `beam_rt` half always runs.

#[allow(dead_code)]
mod support;

use std::process::{Command, Output};

/// One node/cookie probe: the exact expression, the oracle's differential-probe line and exit
/// code, and the oracle's exit-0 line for the cross-check form.
struct Probe {
    /// The Erlang expression under test.
    expr: &'static str,
    /// The differential probe's stdout line (`ok ...` / `exception ...`), the corpus's shape.
    differential: &'static str,
    /// The differential probe's exit code (`0` for a value, `1` for a BEAM exception).
    exit_code: i32,
    /// The exit-0 cross-check line: `~w` of the value, or `{Class,Reason}` of the raise.
    exit_zero: &'static str,
}

/// The oracle's recorded answers on a `-noshell` VM with **no distribution started**
/// (OTP 29.1.1 / `erts-17.1`), re-measured for every entry.
const PROBES: &[Probe] = &[
    // -- is_alive/0, get_cookie/0 ---------------------------------------------
    Probe {
        expr: "erlang:is_alive()",
        differential: "ok false",
        exit_code: 0,
        exit_zero: "false",
    },
    Probe {
        expr: "erlang:get_cookie()",
        differential: "ok nocookie",
        exit_code: 0,
        exit_zero: "nocookie",
    },
    // -- nodes/0 and the peer-selecting types ---------------------------------
    Probe {
        expr: "erlang:nodes()",
        differential: "ok []",
        exit_code: 0,
        exit_zero: "[]",
    },
    Probe {
        expr: "erlang:nodes(connected)",
        differential: "ok []",
        exit_code: 0,
        exit_zero: "[]",
    },
    Probe {
        expr: "erlang:nodes(visible)",
        differential: "ok []",
        exit_code: 0,
        exit_zero: "[]",
    },
    Probe {
        expr: "erlang:nodes(hidden)",
        differential: "ok []",
        exit_code: 0,
        exit_zero: "[]",
    },
    // -- nodes(this|known): the local node ------------------------------------
    Probe {
        expr: "erlang:nodes(this)",
        differential: "ok [nonode@nohost]",
        exit_code: 0,
        exit_zero: "[nonode@nohost]",
    },
    Probe {
        expr: "erlang:nodes(known)",
        differential: "ok [nonode@nohost]",
        exit_code: 0,
        exit_zero: "[nonode@nohost]",
    },
    // -- the list form: concatenation, de-duplicated in first-seen order ------
    Probe {
        expr: "erlang:nodes([])",
        differential: "ok []",
        exit_code: 0,
        exit_zero: "[]",
    },
    Probe {
        expr: "erlang:nodes([this,connected])",
        differential: "ok [nonode@nohost]",
        exit_code: 0,
        exit_zero: "[nonode@nohost]",
    },
    Probe {
        expr: "erlang:nodes([connected,this])",
        differential: "ok [nonode@nohost]",
        exit_code: 0,
        exit_zero: "[nonode@nohost]",
    },
    Probe {
        expr: "erlang:nodes([this,this])",
        differential: "ok [nonode@nohost]",
        exit_code: 0,
        exit_zero: "[nonode@nohost]",
    },
    // -- nodes/1's badarg cases -----------------------------------------------
    Probe {
        expr: "erlang:nodes(all)",
        differential: "exception error badarg",
        exit_code: 1,
        exit_zero: "{error,badarg}",
    },
    Probe {
        expr: "erlang:nodes(bogus)",
        differential: "exception error badarg",
        exit_code: 1,
        exit_zero: "{error,badarg}",
    },
    Probe {
        expr: "erlang:nodes(42)",
        differential: "exception error badarg",
        exit_code: 1,
        exit_zero: "{error,badarg}",
    },
    Probe {
        expr: "erlang:nodes([this,42])",
        differential: "exception error badarg",
        exit_code: 1,
        exit_zero: "{error,badarg}",
    },
    // -- set_cookie/2: the corpus-pinned forms --------------------------------
    Probe {
        expr: "erlang:set_cookie(node(), a_test_cookie)",
        differential: "exception error badarg",
        exit_code: 1,
        exit_zero: "{error,badarg}",
    },
    Probe {
        expr: "erlang:set_cookie(node(), 123)",
        differential: "exception error badarg",
        exit_code: 1,
        exit_zero: "{error,badarg}",
    },
    Probe {
        expr: "erlang:set_cookie(\"str\", a_test_cookie)",
        differential: "exception error badarg",
        exit_code: 1,
        exit_zero: "{error,badarg}",
    },
    Probe {
        expr: "erlang:set_cookie('someother@node', 123)",
        differential: "exception error badarg",
        exit_code: 1,
        exit_zero: "{error,badarg}",
    },
    Probe {
        expr: "erlang:set_cookie('someother@node', a_test_cookie)",
        differential: "exception error distribution_not_started",
        exit_code: 1,
        exit_zero: "{error,distribution_not_started}",
    },
];

/// The differential probe the corpus uses, terminated so the runtime (and the oracle) exits
/// with the exception's code.
fn differential_probe(expr: &str) -> String {
    format!(
        "try ({expr}) of V -> io:format(\"ok ~w~n\", [V]), halt(0) \
         catch C:R -> io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
    )
}

/// The exit-`0` form `support::oracle_eval` can collect: the value, or `{Class,Reason}`.
fn exit_zero_probe(expr: &str) -> String {
    format!("io:format(\"~w~n\", [try ({expr}) catch C:R -> {{C,R}} end]), halt(0).")
}

/// Runs the built `beam_rt` binary with `args`.
fn beam_rt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// The **exact** answers the corpus pins, measured on the real binary: the probe's stdout,
/// its stderr (empty -- a capability gap and a genuine exception are different things, and
/// neither writes a diagnostic for a `-eval` probe) and its exit code.
#[test]
fn the_node_cookie_probes_match_the_corpus_goldens() {
    for probe in PROBES {
        let output = beam_rt(&["-noshell", "-eval", &differential_probe(probe.expr)]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            stdout.trim_end(),
            probe.differential,
            "{}: stdout",
            probe.expr
        );
        assert_eq!(
            output.status.code(),
            Some(probe.exit_code),
            "{}: exit code (stderr: {stderr})",
            probe.expr
        );
        assert_eq!(stderr, "", "{}: stderr", probe.expr);
    }
}

/// The same probes evaluated by the live **oracle**, compared to the same goldens. Skips
/// **loudly** when no `erl` is available (its own exit-0 form is used, see the module docs).
#[test]
fn the_node_cookie_goldens_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("node_cookie") else {
        return;
    };
    let mut compared = 0usize;
    for probe in PROBES {
        let Some(rendered) = support::oracle_eval(&oracle, None, &exit_zero_probe(probe.expr))
        else {
            eprintln!(
                "SKIP {} in the live-oracle comparison: the erl probe could not answer",
                probe.expr
            );
            continue;
        };
        assert_eq!(
            rendered.trim(),
            probe.exit_zero,
            "{}: the recorded oracle answer changed",
            probe.expr
        );
        compared += 1;
    }
    assert_eq!(
        compared,
        PROBES.len(),
        "every probe must have been compared against the live oracle"
    );
}

/// `get_cookie/0` must not leak the cookie the **host** resolved: the differential corpus
/// cannot see the host's `$HOME/.erlang.cookie`, but a node that is not alive answers
/// `nocookie` whatever that file holds -- proved here by asking the runtime directly with a
/// cookie installed.
#[test]
fn get_cookie_is_nocookie_without_a_live_node() {
    use beam_bifs::NullSink;
    use beam_engine::ExecutionContext;
    use beam_rt::{Runtime, RuntimeContext};

    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    assert!(!runtime.is_alive(), "a -noshell runtime is not a node");
    runtime.set_cookie_bytes(b"a_test_cookie".to_vec());

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
    let term = ctx
        .call_bif("erlang", "get_cookie", 0, &[])
        .expect("erlang:get_cookie/0");
    drop(ctx);
    assert_eq!(
        runtime.atom_name_of(term),
        Some("nocookie"),
        "an installed cookie is invisible while the node is not alive"
    );
}
