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

//! The **foreign-function** workloads: real functions from the target projects, compiled from
//! their **read-only checkouts** at test time.
//!
//! Two entry points, four cases, two loaded artifacts:
//!
//! | entry point | case | function | in | out |
//! |---|---|---|---|---|
//! | [`foreign_cowboy_bstr_decomposition`] | `bstr_to_lower` | `cowboy_bstr:to_lower/1` | 30 B | 30 B |
//! | [`foreign_cowboy_bstr_decomposition`] | `bstr_capitalize_token` | `cowboy_bstr:capitalize_token/1` | 30 B | 30 B |
//! | [`foreign_emqx_base62_decomposition`] | `emqx_base62_encode` | `emqx_base62:encode/1` | 12 B | 16 B |
//! | [`foreign_emqx_base62_decomposition`] | `emqx_base62_decode` | `emqx_base62:decode/1` | 15 B | 11 B |
//!
//! # The sources are staged, never copied into the repository
//!
//! [`super::compile_staged`] copies the project's own `.erl` file into a temp directory and
//! compiles it with the system `erlc +deterministic`, so what runs is exactly the upstream
//! code — this repository stores no copy that could drift. The checkouts are located through an
//! env override first, then a default root:
//!
//! * `BENCH_COWBOY_REPO` (default `/home/bill/Source/cowboy`) → `src/cowboy_bstr.erl`,
//! * `BENCH_EMQX_REPO` (default `/home/bill/Source/emqx`) → `apps/emqx/src/emqx_base62.erl`.
//!
//! A missing checkout, a missing source file and an unspawnable `erlc` are all **loud skips**:
//! the entry point prints `SKIP: …` with the reason and reports no figures, which is this
//! harness' convention (`erlc` is an external tool, never a gate).
//!
//! # One artifact, two cases
//!
//! `cowboy_bstr` exports **both** Cowboy cases, so the module is loaded **once** and both cases
//! are measured against that one loaded artifact (one [`Machine`]/[`CountingContext`] pair per
//! case, per batch — the batch lifecycle of [`super::run_calls`] is what keeps memory flat).
//! `emqx_base62` likewise serves its two cases from one artifact.
//!
//! # What each case needs from the host
//!
//! * `to_lower`, `capitalize_token` are **BIF-free** Erlang: a binary comprehension/append plus
//!   a `select_val` character table.
//! * `encode` is **not**: it calls `erlang:iolist_to_binary/1` once per four encoded characters
//!   (3 static call sites; 4 calls for this 12-byte input), and the base62 character tables do
//!   small-integer `+`/`-`. [`super::counting::CountingContext`]'s own floor serves exactly that
//!   set, so the case reports a real `bif_calls` figure and its histogram instead of asserting
//!   `bif_calls == 0`.
//! * `decode` additionally needs `erlang:bit_size/1` — in a guard (`bit_size(Rest) >= 8`) and
//!   for `bit_size(Acc) rem 8`, the latter over the bitstring accumulator (which `erlc -S`
//!   types as a `t_bs_context`). The floor serves it through the bitstring family's own
//!   frozen-layout reader ([`crate::families::bitstrings::bit_size`]), which reads heap bits, an
//!   off-heap refc window and a live match context exactly as C's `erts_bin_bits_size` does and
//!   refuses a non-bitstring instead of substituting a size, so the case reports a real figure.

use std::path::PathBuf;
use std::sync::Arc;

use beam_loader::Module;
use beam_term::Eterm;

use crate::jit::Tier;
use crate::machine::Machine;
use crate::terms::make_heap_binary;

use super::counting::CountingContext;
use super::{
    atom_named, baseline_census, compile_staged, env_usize, instr_per_call, measure_result,
    report_generic, selected_tiers, Measurement, ReportSpec, ResultSnapshot,
};

/// Default measured calls.
const DEFAULT_ITERS: usize = 20_000;
/// Default calls per fresh machine/host batch.
const DEFAULT_BATCH: usize = 500;
/// Default untimed warmup calls.
const DEFAULT_WARMUP: usize = 2_000;

/// One foreign function under measurement: a fixed input and its verified expected result.
struct Case {
    /// The `workload = …` label.
    label: &'static str,
    /// The exported function name to call.
    function: &'static str,
    /// The one fixed argument, built as a heap binary by the batch's `prepare`.
    input: &'static [u8],
    /// The verified result; see the module docs for how each was computed.
    expected: &'static [u8],
    /// `true` when the function must dispatch no BIF at all.
    bif_free: bool,
}

/// A foreign module staged from a read-only checkout, plus the cases it serves.
struct Program {
    /// The module name `erlc` produces (and the artifact is loaded as).
    module: &'static str,
    /// The env var naming the checkout root.
    repo_env: &'static str,
    /// The checkout root when the env var is unset.
    repo_default: &'static str,
    /// The source file, relative to the checkout root.
    source: &'static str,
    /// The temp-directory tag the staged compile uses.
    tag: &'static str,
    /// The cases this module serves.
    cases: &'static [Case],
}

/// Cowboy's `cowboy_bstr` (`https://github.com/ninenines/cowboy`, ISC licence, © Loïc Hoguin).
static COWBOY: Program = Program {
    module: "cowboy_bstr",
    repo_env: "BENCH_COWBOY_REPO",
    repo_default: "/home/bill/Source/cowboy",
    source: "src/cowboy_bstr.erl",
    tag: "foreign_cowboy_bstr",
    cases: &[
        Case {
            label: "bstr_to_lower",
            function: "to_lower",
            input: b"The Quick Brown Fox 0123456789",
            expected: b"the quick brown fox 0123456789",
            bif_free: true,
        },
        Case {
            label: "bstr_capitalize_token",
            function: "capitalize_token",
            input: b"the-quick-brown-fox-0123456789",
            expected: b"The-Quick-Brown-Fox-0123456789",
            bif_free: true,
        },
    ],
};

/// EMQX's `emqx_base62` (`https://github.com/emqx/emqx`, Apache-2.0, © EMQ Technologies).
static EMQX: Program = Program {
    module: "emqx_base62",
    repo_env: "BENCH_EMQX_REPO",
    repo_default: "/home/bill/Source/emqx",
    source: "apps/emqx/src/emqx_base62.erl",
    tag: "foreign_emqx_base62",
    cases: &[
        Case {
            label: "emqx_base62_encode",
            function: "encode",
            input: &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            expected: b"AQIDBAUGBwgJCgsM",
            // `iolist_to_binary/1` once per four encoded characters (4 calls here) plus the
            // base62 character tables' small-int `+`/`-`.
            bif_free: false,
        },
        Case {
            label: "emqx_base62_decode",
            function: "decode",
            input: b"ABCDEFGHIJKLMNO",
            expected: &[0, 16, 131, 16, 81, 135, 32, 146, 139, 48, 222],
            // `bit_size/1` (a guard and the accumulator's size, over a match context) plus the
            // floor's small-int `rem`/`-` -- see the module docs.
            bif_free: false,
        },
    ],
};

/// Decompose Cowboy's `cowboy_bstr` on every selected tier: `to_lower/1` and
/// `capitalize_token/1` against **one** loaded artifact.
///
/// `#[ignore]`d: it is a measurement, not a gate. Run it with
/// `-- --ignored --nocapture foreign_cowboy_bstr_decomposition`.
#[test]
#[ignore = "measurement harness"]
fn foreign_cowboy_bstr_decomposition() {
    decompose(&COWBOY);
}

/// Decompose EMQX's `emqx_base62` on every selected tier: `encode/1` and `decode/1` against
/// **one** loaded artifact.
///
/// `#[ignore]`d: it is a measurement, not a gate. Run it with
/// `-- --ignored --nocapture foreign_emqx_base62_decomposition`.
#[test]
#[ignore = "measurement harness"]
fn foreign_emqx_base62_decomposition() {
    decompose(&EMQX);
}

/// Stage, load and measure every case of `program` on every selected tier.
fn decompose(program: &Program) {
    let repo = match std::env::var(program.repo_env) {
        Ok(root) => PathBuf::from(root),
        Err(_) => PathBuf::from(program.repo_default),
    };
    let source = repo.join(program.source);
    if !source.is_file() {
        println!(
            "SKIP: {} is not a file (set {} to the checkout root) -- the {} decomposition did not \
             run",
            source.display(),
            program.repo_env,
            program.module
        );
        return;
    }
    let beam = match compile_staged(program.module, &source, program.tag) {
        Ok(beam) => beam,
        Err(why) => {
            println!(
                "SKIP: {why} -- the {} decomposition did not run",
                program.module
            );
            return;
        }
    };
    let module: Arc<Module> = match beam_loader::load_file(&beam) {
        Ok(module) => Arc::new(module),
        Err(error) => {
            println!(
                "SKIP: load {}: {error:?} -- the {} decomposition did not run",
                beam.display(),
                program.module
            );
            return;
        }
    };
    let iters = env_usize("BENCH_ITERS", DEFAULT_ITERS);
    let batch = env_usize("BENCH_BATCH", DEFAULT_BATCH);
    let warmup = env_usize("BENCH_WARMUP", DEFAULT_WARMUP);
    let tiers = selected_tiers();

    println!("module = {} ({} cases, ONE loaded artifact)", program.module, program.cases.len());
    println!("source = {}", source.display());
    println!("beam = {}", beam.display());
    println!("note = both/all cases run against one `load_file`, so the per-case figures are the");
    println!("note = engine's, not a re-load's; each case still gets a fresh Machine per batch.");
    println!("note = instr_per_call is the INTERPRETER's reduce-hook count (a property of the");
    println!("note = code, so it is reported for both tiers); every *_per_call count is");
    println!("note = deterministic, while min/median ns_per_call are wall clock on a shared host.");

    let mut first_measured: Option<(&'static str, PathBuf)> = None;
    for case in program.cases {
        let entry = atom_named(&module, case.function);
        let input = case.input;
        let prepare = move |ctx: &mut CountingContext,
                            _machine: &mut Machine|
              -> beam_common::Result<Eterm> { make_heap_binary(ctx.inner_mut(), input) };
        let label = format!("{}:{}", program.module, case.label);

        let mut measured: Vec<(Tier, Measurement)> = Vec::new();
        let mut skipped = false;
        for tier in &tiers {
            match measure_result(&module, entry, *tier, iters, batch, warmup, &prepare) {
                Ok(phase) => measured.push((*tier, phase)),
                Err(error) => {
                    report_skip(program, case, *tier, &error);
                    skipped = true;
                    break;
                }
            }
        }
        if skipped {
            continue;
        }

        let interpreter = measured
            .iter()
            .find(|(tier, _)| *tier == Tier::Interpreter)
            .map(|(_, phase)| phase);
        let per_call_instr = instr_per_call(interpreter, &module, entry, batch, &prepare);
        let expect = |measurement: &Measurement| {
            measurement.snapshot == ResultSnapshot::Binary(case.expected.to_vec())
        };
        let spec = ReportSpec {
            workload: &label,
            unit: None,
            expect: &expect,
            expected: show(case.expected),
            bif_free: case.bif_free,
        };
        for (tier, phase) in &measured {
            report_generic(phase, *tier, per_call_instr, &spec);
        }
        if let [(_, first), (_, second)] = measured.as_slice() {
            super::assert_tiers_agree(first, second, &label);
        }
        if first_measured.is_none() {
            first_measured = Some((case.function, beam.clone()));
        }
    }

    // One census pass for the module's FIRST successful case (the baseline tier's own counters
    // are per loaded artifact, not per export, so repeating it per case would only re-measure
    // the same compile). The reload inside `baseline_census` is what makes its static census
    // non-empty: the per-thread block cache would otherwise answer from an artifact compiled
    // with recording off.
    if tiers.contains(&Tier::BaselineJit) {
        if let Some((function, beam)) = first_measured {
            println!(
                "note = the census block below is the baseline tier's per-thread counters for \
                 {function}/1 only"
            );
            println!("note = (they are per loaded artifact; the other cases' figures are above).");
            let case = program
                .cases
                .iter()
                .find(|case| case.function == function)
                .expect("the census case is one of the program's cases");
            let input = case.input;
            let prepare = move |ctx: &mut CountingContext,
                                _machine: &mut Machine|
                  -> beam_common::Result<Eterm> { make_heap_binary(ctx.inner_mut(), input) };
            baseline_census(&beam, function, iters, batch, &prepare, None);
        }
    }
}

/// Print the `SKIP: …` block for a case the harness could not serve, naming the reason.
fn report_skip(
    program: &Program,
    case: &Case,
    tier: Tier,
    error: &beam_common::BeamError,
) {
    println!("---");
    println!("workload = {}:{} ({}->{} bytes)", program.module, case.label, case.input.len(), case.expected.len());
    println!("tier = {}", tier.name());
    println!("skipped = true");
    println!("skip_reason = {error:?}");
    println!(
        "note = the harness' BIF floor does not model a BIF this function dispatches, so no \
         figure is reported (a half-modelled BIF could turn a guard failure into a wrong result)."
    );
}

/// A byte-slice rendering for the assertion message: the string when it is UTF-8, else the bytes.
fn show(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => format!("{text:?}"),
        Err(_) => format!("{bytes:?}"),
    }
}
