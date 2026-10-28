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
//! **A stdlib fast path's raise carries no `error_info` extension** — the host half of the
//! synthesized BIF stacktrace item's shape rule.
//!
//! # The two shapes
//!
//! C's `save_stacktrace` makes the failing **BIF** the innermost frame of a raise's saved
//! trace (`../../erts/emulator/beam/beam_common.c:877-1063`) and attaches
//! `[{error_info, #{module => FormatModule}}]` — but **only on the `bif_mfa` path**, i.e. for a
//! BIF the `bif.tab` surface *declares*. A `beam_bifs` **stdlib fast path** is a registry key
//! the table does not declare: it stands in for an Erlang **source** function, so it is not a
//! BIF frame and may not borrow the BIF classification.
//!
//! The engine embeds no BIF table, so the discriminator is the host seam
//! `ExecutionContext::bif_kind`; `beam_rt` answers it from `beam_bifs`'s census
//! (`bif_tab::Census::fast_paths`) through `crate::bif_kind`. This suite pins that wiring
//! **end to end on real bytecode**, on both execution tiers, against the live system oracle.
//!
//! # The measured rows (system oracle, OTP 29.1.2 / erts-17.1.1)
//!
//! `lists:seq/3` is a census **fast path** (no `bif.tab` row) and raises `badarg` — a
//! non-`function_clause` reason, so the engine's reason arm cannot mask the classification:
//!
//! ```text
//! try lists:seq(1, 2, 0) catch error:R:S -> {R, hd(S)} end
//!   oracle      {badarg,{lists,seq,[1,2,0],
//!                         [{file,"lists.erl"},{line,541},
//!                          {error_info,#{module => erl_stdlib_errors}}]}}
//!   rust before {badarg,{lists,seq,[1,2,0],
//!                         [{error_info,#{module => erl_stdlib_errors}}]}}
//!   rust after  {badarg,{lists,seq,[1,2,0],[]}}
//! ```
//!
//! `erlang:hd/1` is a declared `bif.tab` BIF (`ubif`), and its item is unchanged and
//! oracle-identical on both sides:
//!
//! ```text
//! try erlang:hd(1) catch error:R:S -> {R, hd(S)} end
//!   oracle      {badarg,{erlang,hd,[1],[{error_info,#{module => erl_erts_errors}}]}}
//!   rust before {badarg,{erlang,hd,[1],[{error_info,#{module => erl_erts_errors}}]}}
//!   rust after  {badarg,{erlang,hd,[1],[{error_info,#{module => erl_erts_errors}}]}}
//! ```
//!
//! # The measured residual (honest, and not this crate's to close)
//!
//! The oracle's fast-path item still carries **more** than this runtime's: the source location
//! (`{file,"lists.erl"},{line,541}`) and an extension its own `error_with_info` opted into.
//! Both come from the *Erlang source* of `lists:seq/3`, which ran on the oracle and stands
//! behind the registry key here; the source-location pair the engine cannot derive, and a
//! per-key census cannot see whether a given source body asks for `error_info`. So the census
//! answer removes a classification this runtime **fabricated** (it named an extension on a frame
//! that is not a BIF) while leaving the residual gap that motivated the engine's rule. The
//! property this suite pins is the one the seam owns: the fast path is not a BIF frame.
//!
//! # Toolchain
//!
//! The fixture is compiled at test time by the system `erlc` (never committed as a `.beam`);
//! a missing toolchain is a **loud skip** ([`support::skip_loudly`]), never a silent pass.
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
//!     --test bif_kind_stacktrace
//! ```
use std::path::Path;
use beam_rt::{Runtime, RuntimeConfig};
mod support;
/// The suite's banner in a loud skip.
const SUITE: &str = "bif_kind_stacktrace";
/// The fixture module.
const MODULE: &str = "bifkind";
/// The fast-path row, `~w`-rendered — the **exact** answer this runtime must give.
///
/// An empty location: the census says the key is not a `bif.tab` BIF, so no `error_info`
/// extension may be attached (before the wiring the row was
/// `[{error_info,#{module => erl_stdlib_errors}}]`).
const FAST_PATH_ANSWER: &str = "{badarg,{lists,seq,[1,2,0],[]}}";
/// The fixture's source.
///
/// Both probes catch `error:R:S` and answer `{R, hd(S)}` — the innermost stacktrace item,
/// which is the only place the extension lives.
const FIXTURE_ERL: &str = "\
-module(bifkind).
-export([fast_path/0, declared/0]).

%% A `beam_bifs` stdlib fast path: `lists:seq/3` is `lists.erl` source on the oracle, never a
%% `bif.tab` BIF, and it raises `badarg` (not `function_clause`, so the engine's reason arm
%% does not mask the classification).
fast_path() ->
    try lists:seq(1, 2, 0) of
        Value -> {unexpected, Value}
    catch
        error:Reason:Stack -> {Reason, hd(Stack)}
    end.

%% A declared `bif.tab` BIF (`ubif erlang:hd/1`): the innermost item is the BIF itself with the
%% closed table's `erl_erts_errors` extension.
declared() ->
    try erlang:hd(1) of
        Value -> {unexpected, Value}
    catch
        error:Reason:Stack -> {Reason, hd(Stack)}
    end.
";
/// A runtime over the default configuration.
fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("the default runtime")
}
/// This runtime's `~w` rendering of `MODULE:<function>()` — the same control the oracle probe
/// uses, so the two strings are directly comparable.
fn runtime_answer(runtime: &mut Runtime, beam: &Path, function: &str) -> String {
    let term = runtime
        .run_function(beam, function, 0, &[])
        .unwrap_or_else(|e| panic!("{MODULE}:{function}/0 did not run: {e:?}"));
    runtime
        .format_term_w(term, None)
        .unwrap_or_else(|e| panic!("{MODULE}:{function}/0's result did not print: {e:?}"))
        .trim_end()
        .to_string()
}
/// The oracle's `~w` rendering of `MODULE:<function>()`, or `None` after a loud skip.
fn oracle_answer(oracle: &Path, dir: &Path, function: &str) -> Option<String> {
    let expr = format!("io:format(\"~w~n\", [{MODULE}:{function}()]), halt(0).");
    match support::oracle_eval(oracle, Some(dir), &expr) {
        Some(answer) => Some(answer),
        None => {
            support::skip_loudly(SUITE, "the oracle could not answer the probe");
            None
        }
    }
}
/// The compiled fixture plus the resolved oracle.
fn fixture() -> Option<(support::ErlangFixture, std::path::PathBuf)> {
    let oracle = support::require_oracle(SUITE)?;
    let fixture = support::ErlangFixture::compile(SUITE, "stacktrace", MODULE, FIXTURE_ERL)?;
    Some((fixture, oracle))
}
#[test]
fn a_stdlib_fast_path_raise_carries_no_error_info_extension() {
    let Some((fixture, oracle)) = fixture() else {
        return;
    };
    let mut runtime = runtime();
    let ours = runtime_answer(&mut runtime, &fixture.beam(), "fast_path");
    let Some(theirs) = oracle_answer(&oracle, fixture.dir(), "fast_path") else {
        return;
    };
    eprintln!("fast path  rust: {ours}\nfast path oracle: {theirs}");
    // The wiring's property: the registry key is not a `bif.tab` BIF, so its raise carries no
    // BIF classification.
    assert!(
        !ours.contains("error_info"),
        "a `lists:seq/3` raise must not carry a fabricated `error_info` extension: {ours}"
    );
    // The runtime's own item: the fast path, the **real** argument list, an empty location.
    assert_eq!(
        ours, FAST_PATH_ANSWER,
        "the fast-path head item names the call with the real arguments"
    );
    // The oracle's own item names the same call with the same arguments — the reason, the
    // module, the function and the arglist all agree. Its location carries the source position
    // and the extension `lists.erl`'s `error_with_info` opted into (the documented residual at
    // the top of this file), so the location list is deliberately not compared.
    assert!(
        theirs.contains("{badarg,{lists,seq,[1,2,0],"),
        "the oracle names the same call in its innermost item: {theirs}"
    );
}
#[test]
fn a_declared_bif_raise_keeps_its_error_info_extension() {
    let Some((fixture, oracle)) = fixture() else {
        return;
    };
    let mut runtime = runtime();
    let ours = runtime_answer(&mut runtime, &fixture.beam(), "declared");
    let Some(theirs) = oracle_answer(&oracle, fixture.dir(), "declared") else {
        return;
    };
    eprintln!("declared  rust: {ours}\ndeclared oracle: {theirs}");
    assert!(
        ours.contains("{error_info,#{module => erl_erts_errors}}"),
        "a declared `bif.tab` BIF keeps the closed table's extension: {ours}"
    );
    // Byte-identical to the oracle for the declared row — the classification changed nothing
    // here (the pre-seam behaviour is preserved exactly where it was right).
    assert_eq!(ours, theirs, "the declared-BIF row is oracle-identical");
}
#[test]
fn the_scheduled_path_classifies_identically_on_both_tiers() {
    let Some((fixture, _oracle)) = fixture() else {
        return;
    };
    // The `run` action drives the **scheduled** path (`Runtime::run_scheduled`), whose engine
    // context is the runner's `BudgetContext` wrapper — the second `ExecutionContext` this
    // runtime builds. Its `bif_kind` forward is what makes a spawned process answer like the
    // single-pass `-eval`/`run_function` context above; without it the wrapper would take the
    // engine's `BifKind::Unknown` default and fabricate the extension again. The tier is set on
    // the **child's** environment only (the test process is never mutated), so the two rows also
    // pin that a tier change cannot change the classification.
    let beam = fixture.beam();
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8"));
    let target = format!("{MODULE}:fast_path/0");
    let mut observed: Vec<String> = Vec::new();
    for tier in [None, Some("interpreter")] {
        let output = support::run_bounded_with_env(
            std::path::Path::new(env!("CARGO_BIN_EXE_beam_rt")),
            &["run", beam, &target],
            &[("BEAM_RT_TIER", tier)],
        )
        .unwrap_or_else(|| {
            panic!("`beam_rt run {beam} {target}` (BEAM_RT_TIER={tier:?}) could not answer")
        });
        let stdout = String::from_utf8_lossy(&output.stdout).trim_end().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "`beam_rt run {beam} {target}` (BEAM_RT_TIER={tier:?}) exited {:?}: {stderr}",
            output.status.code()
        );
        observed.push(stdout);
    }
    assert_eq!(
        observed[0], observed[1],
        "both tiers cook the fast-path item identically"
    );
    assert!(
        !observed[0].contains("error_info"),
        "the scheduled path fabricates no extension: {observed:?}"
    );
    assert_eq!(
        observed[0], FAST_PATH_ANSWER,
        "the scheduled path's fast-path head item is the same shape as the single-pass one"
    );
}
