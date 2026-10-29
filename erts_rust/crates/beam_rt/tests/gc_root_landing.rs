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
//! **The exception-landing register set must survive a collection.**
//!
//! # What this pins
//!
//! The engine lands an exception the way C's `handle_error` does: four words are written into
//! the register file, `x(0) = THE_NON_VALUE`, `x(1) = reason`, `x(2) = trace`,
//! `x(3) = class`, and the compiled handler's `try_case` **requires** `x(0)` to still be
//! `THE_NON_VALUE` when it runs.
//! `THE_NON_VALUE` is `make_header(0, FLOAT_SUBTAG)` — the bare header word **`0x18`** — in a
//! debug build (`0` in release).
//!
//! A word whose primary tag is `HEADER` is not a term, but the frozen `is_boxed`/`is_list` bit
//! tricks answer `true` for it (bit 0 is clear), so a root classifier built on those tricks
//! dereferenced `0x18` as a cons cell. The reduction-boundary door publishes
//! `RootSet::ALL_LIVE` (all 1024 x slots) and the driver charges reductions *before* the
//! instruction body, so a slice that pre-empts on the first handler instruction hands the valve
//! exactly this register set — a real workload reaches it, which is why this suite drives the
//! binary instead of asserting the classifier alone.
//!
//! The classifier now dispatches on `primary_tag` (`src/context/gc.rs::is_pointer_bearing`), and
//! `src/context/gc/valve_bounds.rs` pins the unit-level half (inert verdict, byte-identical
//! round trip through a collection). This suite pins the whole stack: a compiled module whose
//! tail-recursive loop raises and **catches** once per iteration must answer like the oracle at
//! every depth, on both execution tiers, because the landing window recurs on every iteration.
//!
//! Three BIF shapes are driven: a **stdlib fast path** (`binary:at/2` over a non-binary,
//! `lists:max([])`), which is a `beam_bifs` registry key the `bif.tab` surface does not declare,
//! and a **declared `bif.tab` BIF** (`erlang:float/1`), which raises through the other host
//! route. Both converge on the same landing, so both are pinned.
//!
//! A missing toolchain (`erl`/`erlc`) is a **loud skip**, never a silent pass.
#[allow(dead_code)]
mod support;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};
/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
/// The probe module: one export per raise shape, each a tail-recursive loop that catches the
/// raise and recurses, so every iteration lands an exception in the register file.
///
/// `at/1` reaches the `binary:at/2` fast path (its first argument is a list, so it is `badarg`),
/// `fastmax/1` reaches `lists:max/1` (an empty list is a `function_clause`) and `decl/1` reaches
/// `erlang:float/1`, a declared `bif.tab` BIF.
const FIXTURE_SRC: &str = "\
-module(pfp).
-export([at/1, fastmax/1, decl/1]).
at(0) -> ok;
at(N) -> try binary:at([no,binary], bad_pos) catch error:_:_ -> ok end, at(N - 1).
fastmax(0) -> ok;
fastmax(N) -> try lists:max([]) catch error:_:_ -> ok end, fastmax(N - 1).
decl(0) -> ok;
decl(N) -> try erlang:float(foo) catch error:_:_ -> ok end, decl(N - 1).
";
// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------
/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}
impl Run {
    /// The finished probe described by one collected child `output`.
    fn from_output(output: Output) -> Run {
        Run {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
    }
}
/// Runs `program args` under the shared bounded runner (`/dev/null` stdin, killed at
/// `support::PROBE_TIMEOUT`), failing loudly when the probe could not be run at all.
fn run_with_env(program: &Path, args: &[&str], env: &[(&str, Option<&str>)]) -> Run {
    let output: Output = support::run_bounded_with_env(program, args, env).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Run::from_output(output)
}
/// `beam_rt <args>` on the requested execution tier through the built binary.
///
/// `tier` is the value of `BEAM_RT_TIER`, or `None` for the default (native baseline) tier —
/// removal rather than a value, so an inherited setting in the test runner's environment cannot
/// silently reroute the default-tier leg.
fn beam_rt_tier(args: &[&str], tier: Option<&str>) -> Run {
    run_with_env(Path::new(BINARY), args, &[("BEAM_RT_TIER", tier)])
}
// ---------------------------------------------------------------------------------------
// Temporary directories and the fixture
// ---------------------------------------------------------------------------------------
/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);
impl TempDir {
    /// Creates `$TMPDIR/beam_rt_gc_root_landing_<pid>_<n>`, unique per test and per run.
    fn new() -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_gc_root_landing_{}_{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Compiles [`FIXTURE_SRC`] with the system `erlc` into a private scratch directory, or reports
/// a loud skip when `erlc` is absent.
fn compile_fixture(banner: &str) -> Option<TempDir> {
    let Some(erlc) = support::find_on_path("erlc") else {
        support::skip_loudly(banner, "no `erlc` on $PATH");
        return None;
    };
    let dir = TempDir::new();
    let src = dir.0.join("pfp.erl");
    fs::write(&src, FIXTURE_SRC).expect("write the fixture");
    let built = run_with_env(&erlc, &["-o", &dir.0.to_string_lossy(), &src.to_string_lossy()], &[]);
    assert_eq!(built.code, 0, "`erlc` must build the fixture: {}", built.err);
    assert!(
        dir.0.join("pfp.beam").is_file(),
        "`erlc` wrote no pfp.beam in {}",
        dir.0.display()
    );
    Some(dir)
}
/// The depths every shape is driven at.
///
/// `2000` is the depth the fault was measured at (a slice boundary lands in the landing window
/// within the first few hundred iterations); `1999`/`1998` are its immediate neighbours, which
/// answered before the fix and must keep answering; `20000` walks far past the first boundary and
/// `1000` stops short of it, so a fix that merely moved the cliff cannot pass.
const DEPTHS: &[u32] = &[1000, 1998, 1999, 2000, 20000];
/// The two execution tiers' `BEAM_RT_TIER` values: the default (native baseline) tier and the
/// interpreter, which faults at a *different* depth because the landing is shared but the
/// reduction accounting per slice is not.
const TIERS: &[(&str, Option<&str>)] = &[("default", None), ("interpreter", Some("interpreter"))];
// ---------------------------------------------------------------------------------------
// The regression itself
// ---------------------------------------------------------------------------------------
/// **A caught raise in a tail-recursive loop answers `ok` at every depth, on both tiers.**
///
/// The pre-fix failure was `exit 139` (SIGSEGV, empty stdout) at depth 2000 on the default tier
/// and from depth 1000 on the interpreter tier. A signal is reported by this harness as
/// `code == -1`, so the assertion names the crash explicitly instead of merely "not zero".
#[test]
fn a_caught_raise_loop_survives_a_collection_at_every_depth() {
    let Some(dir) = compile_fixture("gc_root_landing") else {
        return;
    };
    let beam = dir.0.join("pfp.beam");
    for (shape, tier_label, tier) in SHAPES
        .iter()
        .flat_map(|shape| TIERS.iter().map(move |(label, tier)| (*shape, *label, *tier)))
    {
        for &depth in DEPTHS {
            let spec = format!("pfp:{shape}/1");
            let n = depth.to_string();
            let run = beam_rt_tier(&["-noshell", "run", &beam.to_string_lossy(), &spec, &n], tier);
            assert_ne!(
                run.code, -1,
                "`pfp:{shape}/1 {depth}` on the {tier_label} tier died of a signal \
                 (the GC root classifier faulted); stderr: {}",
                run.err
            );
            assert_eq!(
                run.code, 0,
                "`pfp:{shape}/1 {depth}` on the {tier_label} tier exited {}; stderr: {}",
                run.code, run.err
            );
            assert_eq!(
                run.out, "ok",
                "`pfp:{shape}/1 {depth}` on the {tier_label} tier printed {:?}",
                run.out
            );
            assert_eq!(
                run.err, "",
                "`pfp:{shape}/1 {depth}` on the {tier_label} tier wrote to stderr"
            );
        }
    }
}
/// The three raise shapes the loop drives: a `binary:at/2` fast path, a `lists:max/1` fast path
/// and the declared `bif.tab` BIF `erlang:float/1`.
const SHAPES: &[&str] = &["at", "fastmax", "decl"];
/// **The same loops answer like the oracle.** The oracle has no landing hazard, so its `ok` at
/// the faulting depth is the ground truth the assertions above reproduce.
#[test]
fn the_oracle_answers_ok_at_the_same_depths() {
    let Some(oracle) = support::require_oracle("gc_root_landing") else {
        return;
    };
    let Some(dir) = compile_fixture("gc_root_landing") else {
        return;
    };
    for &depth in DEPTHS {
        let expr = format!("io:format(\"~p~n\",[pfp:at({depth})]), halt(0).");
        let answer = support::oracle_eval(&oracle, Some(&dir.0), &expr);
        assert_eq!(
            answer.as_deref(),
            Some("ok"),
            "the oracle must answer `ok` for `pfp:at({depth})`"
        );
    }
}
/// **The `-eval` route agrees too.** It arms no collection ([`crate::runner`]'s note on the
/// stale-term SIGSEGV), so it never reached the fault — but a fix must not make the two routes
/// disagree about this shape either.
#[test]
fn the_eval_route_agrees_with_the_run_route() {
    let Some(dir) = compile_fixture("gc_root_landing") else {
        return;
    };
    let code_dir = dir.0.to_string_lossy().into_owned();
    for (shape, tier_label, tier) in SHAPES
        .iter()
        .flat_map(|shape| TIERS.iter().map(move |(label, tier)| (*shape, *label, *tier)))
    {
        let expr = format!("pfp:{shape}(20000), halt(0).");
        let run = beam_rt_tier(&["-noshell", "-pa", &code_dir, "-eval", &expr], tier);
        assert_eq!(
            run.code, 0,
            "`-eval pfp:{shape}(20000)` on the {tier_label} tier exited {}; stderr: {}",
            run.code, run.err
        );
        assert_eq!(run.out, "", "a `-eval` expression prints nothing");
    }
}
