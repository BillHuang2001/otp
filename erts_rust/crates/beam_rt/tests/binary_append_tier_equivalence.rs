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

//! **Tier equivalence on a writable-accumulator binary append** — the executable's own
//! regression pin for the in-place payload republish.
//!
//! # The program
//!
//! The fixture is the *writable-accumulator* binary-append shape (`bench_binary_append`,
//! written to a scratch directory and compiled with the system `erlc`):
//!
//! ```erlang
//! run(N) -> run(N, input(8192), 0).
//! run(0, _Bin, Acc) -> Acc;
//! run(N, Bin, Acc) ->
//!     Out = expand(Bin),
//!     run(N - 1, Bin, Acc + byte_size(Out)).
//! ```
//!
//! `input/1` builds a 16 384-byte binary (two `N = 8192`-byte halves) and `expand/1` quadruples
//! every byte with a binary comprehension, so each round produces 65 536 bytes. **The writable
//! append is the comprehension's own accumulator**: ERTS's `bs_private_append` arm rewrites the
//! accumulator's `[BinRef][ErlSubBits]` pair in place instead of copying the binary per iteration,
//! and the binary is rebuilt from scratch every round (`run/1` itself only sums
//! `byte_size(Out)`), so the growth crosses the small-binary boundary and collections repeatedly.
//! At `N = 32` the answer is `32 × 65 536 = 2097152`.
//!
//! The answer is deterministic and independent of heap addresses, so every execution tier must
//! print it. That is the claim: **each tier this build serves produces the same stdout, exit code
//! 0 and empty stderr, and the tiers agree with each other** (equivalence, not only the constant).
//! The invocation is the benchmark driver's (`<beam_rt> -pa <dir> run <beam>
//! bench_binary_append:run/1 32`; `run` records its own scoped `-noshell`, so no banner is
//! printed), through the shared bounded harness ([`support::run_bounded_with_env`]) so a tier that
//! blocks can never hang the suite; the tier list is [`served_tiers`], which names the Cranelift
//! hot tier only in a build carrying this crate's `cranelift` Cargo feature. A tier that this
//! build does **not** serve is not part of the comparison and its (typed, exit-3) refusal is
//! never treated as a failure — that refusal has its own rows in `tests/tier_selection.rs`.
//!
//! # Why the accumulator can outlive a collection between two appends
//!
//! The accumulator is a `[BinRef][ErlSubBits]` pair, and the pair is **two separate heap
//! blocks** that a collection copies independently (`../beam_gc/src/copy.rs`, pinned by
//! `../beam_gc/tests/stress_cross_generation.rs`), so after a collection the two blocks need not
//! be adjacent. This program really does reach that state on the interpreter tier: the tier
//! charges the reduction budget per interpreted *instruction*, so the build pre-empts
//! mid-comprehension (the runner's park arm → `Runtime::scheduler_pass` →
//! `Machine::resume_outcome_with_tier` on the tier the process remembers) and a collection runs at
//! a heap-admission point between two `private_append`s. The in-place growth path then takes the
//! `Moved` arm of `InPlace::grow` and republishes the pair at the payload's new address, writing
//! only the **two** words that name it — the `BinRef`'s `val` and the `ErlSubBits`'s
//! `base_flags` (its flag bits preserved) — at each block's own live address, with adjacency
//! assumed nowhere (`../beam_engine/src/families/bitstrings/raw.rs::republish_over`).
//!
//! The engine-side pins for that rule are
//! `../beam_engine/src/families/bitstrings/raw_tests.rs`'s
//! `a_republish_writes_only_the_two_payload_words_and_leaves_a_split_pairs_neighbour_intact` (a
//! canary block planted directly behind the `BinRef`, byte-identical afterwards in a **debug and
//! a release** build) and `a_split_pairs_payload_move_keeps_the_next_append_in_place` (the *next*
//! append still lands in place, so the amortisation the shape measures survives the move). This
//! file is the same invariant at the CLI boundary, where the tiers can be compared to each other
//! rather than only to a constant.
// The one helper this binary uses is the shared bounded child runner; the rest of the shared
// harness folds away and would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The suite's name in the loud-skip banner.
const SUITE: &str = "binary_append_tier_equivalence";

/// The fixture module (`-module(bench_binary_append)` names the file `bench_binary_append.erl`).
const MODULE: &str = "bench_binary_append";

/// How many rounds `run/1` is driven with: `32 × 65 536` bytes.
const ROUNDS: &str = "32";

/// The answer every tier must print (trimmed): `32 × 4 × 2 × 8192`.
const EXPECTED: &str = "2097152";

/// The fixture source, verbatim: the writable-accumulator binary-append shape.
const FIXTURE_ERL: &str = r#"-module(bench_binary_append).
-export([input/1, expand/1, run/1]).

input(N) ->
    <<16#00112233445566778899aabbccddeeff:N/unit:8,
      16#ffeeddccbbaa99887766554433221100:N/unit:8>>.

expand(Bin) -> << <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>.

run(N) -> run(N, input(8192), 0).

run(0, _Bin, Acc) -> Acc;
run(N, Bin, Acc) ->
    Out = expand(Bin),
    run(N - 1, Bin, Acc + byte_size(Out)).
"#;

/// One finished `beam_rt` invocation.
struct Run {
    /// The process exit code (`-1` when it was killed by a signal).
    code: i32,
    /// Everything the child wrote to stdout.
    stdout: String,
    /// Everything the child wrote to stderr.
    stderr: String,
}

/// Runs the built `beam_rt` with `args` on the `tier` named by `BEAM_RT_TIER`.
///
/// The variable is set on the **child alone** (the test process's own environment is never
/// mutated — the crate is `#![deny(unsafe_code)]` and `set_var` is `unsafe` in edition 2024), and
/// a child that outlives [`support::PROBE_TIMEOUT`] is a loud panic rather than a skipped run.
fn beam_rt(args: &[&str], tier: &str) -> Run {
    let output = support::run_bounded_with_env(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        args,
        &[("BEAM_RT_TIER", Some(tier))],
    )
    .unwrap_or_else(|| {
        panic!(
            "`beam_rt {}` with BEAM_RT_TIER={tier} could not answer (it outlived the {}s probe \
             bound or could not be spawned)",
            args.join(" "),
            support::PROBE_TIMEOUT.as_secs()
        )
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The tiers **this build** serves, in the order the test drives them.
///
/// `interpreter` and `baseline` exist in every build; the Cranelift hot tier exists only with
/// this crate's `cranelift` Cargo feature (default OFF), which is why the list is computed from
/// `cfg!` rather than being a constant — a feature-off build never asks for a tier it must
/// refuse.
fn served_tiers() -> Vec<&'static str> {
    let mut tiers = vec!["interpreter", "baseline"];
    if cfg!(feature = "cranelift") {
        tiers.push("cranelift");
    }
    tiers
}

/// The compiler: `erlc` on `$PATH`, or `None` (the caller then skips **loudly**).
///
/// Filesystem discovery only — a probe must never *run* `erlc` to find out whether it exists
/// (`tests/support/mod.rs`).
fn resolve_erlc() -> Option<PathBuf> {
    support::find_on_path("erlc")
}

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding the fixture's `.erl` and `.beam`.
    dir: PathBuf,
    /// The compiled `.beam`.
    beam: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test, yet a
        // cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Writes [`FIXTURE_ERL`] into a fresh scratch directory and compiles it there, or `None` when it
/// cannot be compiled (the caller then returns early — **loudly**).
///
/// The directory is unique per call (pid + a process-wide counter) and holds only the fixture
/// module, so it is a safe single `-pa`.
fn build_fixture(erlc: &Path) -> Option<Fixture> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_binary_append_tier_equivalence_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{MODULE}.erl"));
    if let Err(error) =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&source, FIXTURE_ERL))
    {
        support::skip_loudly(SUITE, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(SUITE, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            SUITE,
            &format!(
                "`erlc -o {} {}` exited with {:?}: {}",
                dir.display(),
                source.display(),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        );
        return None;
    }
    let beam = dir.join(format!("{MODULE}.beam"));
    if !beam.is_file() {
        support::skip_loudly(
            SUITE,
            &format!("`erlc` produced no {}", beam.display()),
        );
        return None;
    }
    Some(Fixture { dir, beam })
}

/// The CLI argument vector: `-pa <dir> run <dir>/bench_binary_append.beam
/// bench_binary_append:run/1 32`.
fn fixture_args(fixture: &Fixture) -> Vec<String> {
    let dir = fixture
        .dir
        .to_str()
        .unwrap_or_else(|| panic!("the fixture dir is not UTF-8: {}", fixture.dir.display()));
    let beam = fixture
        .beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", fixture.beam.display()));
    vec![
        "-pa".to_string(),
        dir.to_string(),
        "run".to_string(),
        beam.to_string(),
        format!("{MODULE}:run/1"),
        ROUNDS.to_string(),
    ]
}

/// The first non-blank line of the child's stderr, or `<empty>`.
///
/// The panic hook's output starts with a blank line, so a plain `lines().next()` would report an
/// empty diagnostic for a child that did panic.
fn stderr_head(run: &Run) -> String {
    match run.stderr.lines().find(|line| !line.trim().is_empty()) {
        Some(line) => line.trim_end().to_string(),
        None => "<empty>".to_string(),
    }
}

/// The per-tier outcome table, so a failure names **which** tier diverged and what every other
/// tier answered in the same run.
fn report(outcomes: &[(&str, Run)]) -> String {
    let mut table = String::new();
    for (tier, run) in outcomes {
        table.push_str(&format!(
            "\n  {tier}: exit={} stdout={:?} stderr={:?}",
            run.code,
            run.stdout.trim(),
            stderr_head(run)
        ));
    }
    table
}

/// Prints every tier's **whole** stderr, before anything is asserted, so a reproducing run shows
/// each child's own diagnostic (a debug build's `raw.rs` assertion, a release build's typed
/// `unsupported` line) even when the first tier already failed.
fn dump_stderr(outcomes: &[(&str, Run)]) {
    for (tier, run) in outcomes {
        if !run.stderr.is_empty() {
            eprintln!("[{SUITE}] {tier}: stderr:\n{}", run.stderr.trim_end());
        }
    }
}

/// **Every served tier prints the writable-accumulator answer identically.**
///
/// Each tier is run as its own child process, the per-tier outcomes are collected and printed
/// **before** anything is asserted (so a reader sees every tier's answer even when the first one
/// already fails), and then each tier is asserted independently: exit code 0, stdout trimmed to
/// [`EXPECTED`], empty stderr. The last claim is the equivalence one — every tier's raw stdout is
/// compared to the reference interpreter's — because a shared constant alone cannot catch two
/// tiers agreeing on a wrong value.
///
/// The interpreter is the reference, and it is the interesting leg: it pre-empts and collects
/// between two appends, so its run is what exercises the pair republish. Every other served tier
/// is compared to its raw stdout as well as to the fixed answer.
#[test]
fn every_served_tier_prints_the_same_writable_accumulator_answer() {
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(SUITE, "no `erlc` on $PATH");
        return;
    };
    let Some(fixture) = build_fixture(&erlc) else {
        return;
    };
    let args = fixture_args(&fixture);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();

    let outcomes: Vec<(&str, Run)> = served_tiers()
        .into_iter()
        .map(|tier| (tier, beam_rt(&borrowed, tier)))
        .collect();
    for (tier, run) in &outcomes {
        eprintln!(
            "[{SUITE}] {tier}: exit={} stdout={:?} stderr={:?}",
            run.code,
            run.stdout.trim(),
            stderr_head(run)
        );
    }
    dump_stderr(&outcomes);
    let table = report(&outcomes);

    for (tier, run) in &outcomes {
        assert_eq!(
            run.code, 0,
            "`{MODULE}:run/1 {ROUNDS}` must complete on the {tier} tier, exit 0: {table}"
        );
        assert_eq!(
            run.stdout.trim(),
            EXPECTED,
            "the {tier} tier must print {EXPECTED}: {table}"
        );
        assert!(
            run.stderr.is_empty(),
            "the {tier} tier must be silent on stderr: {table}"
        );
    }

    let (_, reference) = &outcomes[0];
    for (tier, run) in &outcomes[1..] {
        assert_eq!(
            run.stdout, reference.stdout,
            "the {tier} tier must print what the interpreter reference printed: {table}"
        );
    }
}
