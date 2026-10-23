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

//! The **`BEAM_RT_TIER` selection surface** of the executable: what the environment variable
//! does to a real `beam_rt` process, and why an *unbuilt* tier is an exit-3 capability gap
//! rather than a runtime failure.
//!
//! # The rule this file pins
//!
//! An execution tier has to be **built** to be **selected** (`src/runner/tier.rs`, *The Cranelift
//! tier is opt-in*). `BEAM_RT_TIER=cranelift` names the optional Cranelift hot tier, whose
//! backend exists only in a build with this crate's `cranelift` Cargo feature (default OFF,
//! forwarding `beam_engine/jit-cranelift`). In a build **without** it, the request is refused at
//! the **CLI boundary**, before any boot work: `drive` consults
//! `runner::unbuilt_tier_request()`, the typed gap reaches `error::unsupported_reason`, and the
//! process prints the stable one-line signal — `beam_rt: unsupported: <reason>`, the reason
//! naming the Cargo feature — and exits [`beam_rt::cli::EXIT_UNSUPPORTED`] (3).
//!
//! Why the boundary and not the first attempt: `BEAM_RT_TIER` is **process-wide**, so the boot
//! script's own processes are the first to be tiered, they die one at a time on the missing
//! backend, and the boot drive re-wraps the failure into the ordinary exit-1 diagnostic
//! (`beam_rt: exit <structured reason>`) — which names no feature and reads like a crash. The
//! startup check is what makes the refusal happen once, loudly and as the *capability gap* it
//! is.
//!
//! # The rows, and the negative controls
//!
//! | Row | Expectation |
//! |---|---|
//! | feature-OFF, `cranelift`, `run slice.beam slice:fib/1 10` | exit **3**, empty stdout, one `beam_rt: unsupported: …` line naming `cranelift` and the Cargo feature |
//! | feature-OFF, `cranelift`, `-version` | exit **0** and the banner: `-version` runs no code, so it selects no tier |
//! | every build, unset / `auto` / `baseline`, same `run` | `55`, exit 0 — the check must fire on **nothing** else |
//! | feature-ON, `cranelift`, same `run` | the startup check never answers (a built tier is not refused) **and the session completes**: `55`, exit 0, empty stderr — a parked or pre-empted process resumes on the tier |
//! | every build, the three CLI rows, once per **served** tier | identical stdout (`55` / `timeout` / `50`), exit 0, empty stderr and no `unsupported` line on `interpreter` and `baseline`, plus `cranelift` when the feature is built; the feature-off run is the negative control |
//!
//! The child is the **built** binary ([`CARGO_BIN_EXE_beam_rt`]) run through the crate's shared
//! bounded harness ([`support::run_bounded_with_env`], a 30 s probe bound), so a blocking child
//! can never hang the suite and the test process's own environment is never mutated (the crate
//! is `#![deny(unsafe_code)]`, and `set_var` is `unsafe` in edition 2024) — the variable is set
//! or **removed** on the child alone.
//!
//! # Three-tier equivalence, at the CLI boundary
//!
//! The last row is the executable's own equivalence claim, and it is deliberately *not*
//! feature-gated: a feature-off build serves two of the three tiers, so the same body is the
//! positive equivalence check there and the negative control in a `cranelift` build (it never
//! names a tier that build must refuse). The three rows are chosen for the three shapes a
//! session takes — a pure computation (`slice:fib/1 10`), a process that **parks** on a finite
//! `receive … after T` (`recv_after:after20/0`), and a **two-process** ping-pong that parks and
//! resumes 50 times (`pingpong:go/1 50`). Each tier's stdout is compared both to the fixture's
//! fixed answer **and** to the interpreter's own run of the same row, so the reference is a
//! real execution rather than a table.
//!
//! Its in-process half lives in `src/runner/tests.rs`
//! (`a_preempted_process_resumes_on_its_cranelift_tier`,
//! `a_receive_parked_process_resumes_on_its_cranelift_tier`); this row is the same property
//! through the whole binary, boot script included.

// This binary uses one helper of the shared harness (the bounded child runner), so the rest of it
// (the oracle probes, the two-VM session) folds away and would otherwise warn on the entry points
// it does not call.
#[allow(dead_code)]
mod support;

use std::path::Path;

/// The `beam_loader` oracle fixture (reused by relative path, never copied): `slice:fib/1 10` is
/// `55`, the smallest end-to-end answer this runtime computes.
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// The committed finite-`receive` fixture (`erlc +deterministic recv_after.erl`):
/// `recv_after:after20/0` parks ~20 ms on an empty mailbox, then prints `timeout`. It is the
/// equivalence row that makes a **resume** part of the session.
const RECV_AFTER_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/recv_after.beam"
);

/// The committed two-process ping-pong fixture (`erlc +deterministic pingpong.erl`):
/// `pingpong:go/1 50` empties the run queue between every round trip, so the driver process
/// parks and resumes fifty times.
const PINGPONG_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pingpong.beam");

/// The feature's own name in the diagnostic the startup check prints (used by the feature-off
/// rows only, because that is the only build where the check fires).
#[cfg(not(feature = "cranelift"))]
const FEATURE: &str = "cranelift";

/// One finished `beam_rt` invocation.
struct Run {
    /// The process exit code (`-1` when it was killed by a signal).
    code: i32,
    /// Everything the child wrote to stdout.
    stdout: String,
    /// Everything the child wrote to stderr.
    stderr: String,
}

/// Runs the built `beam_rt` with `args` and `tier` as its `BEAM_RT_TIER` (`None` **removes** the
/// variable, so that row is the unset default rather than whatever this process carries).
fn beam_rt(args: &[&str], tier: Option<&str>) -> Run {
    let output = support::run_bounded_with_env(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        args,
        &[("BEAM_RT_TIER", tier)],
    )
    .unwrap_or_else(|| {
        panic!(
            "`beam_rt {}` with BEAM_RT_TIER={tier:?} outlived the {}s probe bound",
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

/// The vertical-slice argument vector: `-noshell run <slice.beam> slice:fib/1 10`.
///
/// `-noshell` is the convention for a stdout-asserting invocation in this crate (a booted
/// kernel prints the OTP banner without it), and the fixture is passed by absolute path because
/// the test process's working directory is the crate directory.
fn fib_args() -> [&'static str; 5] {
    ["-noshell", "run", SLICE_BEAM, "slice:fib/1", "10"]
}

/// The tiers **this build** serves, in the order the equivalence row drives them.
///
/// `interpreter` and `baseline` exist in every build; the Cranelift hot tier exists only with
/// this crate's `cranelift` Cargo feature (default OFF), which is why the list is computed from
/// `cfg!` rather than being a constant. A feature-off build therefore never *names* the tier its
/// startup check must refuse — the same test body is the negative control there.
fn served_tiers() -> Vec<&'static str> {
    let mut tiers = vec!["interpreter", "baseline"];
    if cfg!(feature = "cranelift") {
        tiers.push("cranelift");
    }
    tiers
}

/// The three CLI rows the equivalence test drives: a label (for failure messages), the argument
/// vector, and the exact stdout the committed fixture prints.
///
/// They cover the three shapes a session takes: a pure computation, a process parked on a finite
/// `receive … after T`, and a two-process ping-pong that parks and resumes between every round
/// trip. The fib row reuses [`fib_args`] so the slice's argument vector has one definition.
fn equivalence_rows() -> Vec<(&'static str, Vec<&'static str>, &'static str)> {
    vec![
        ("slice:fib/1 10", fib_args().to_vec(), "55\n"),
        (
            "recv_after:after20/0",
            vec!["-noshell", "run", RECV_AFTER_BEAM, "recv_after:after20/0"],
            "timeout\n",
        ),
        (
            "pingpong:go/1 50",
            vec!["-noshell", "run", PINGPONG_BEAM, "pingpong:go/1", "50"],
            "50\n",
        ),
    ]
}

/// **Feature off: naming the Cranelift tier is refused at the CLI boundary with exit 3.**
///
/// The refusal is the *capability gap* signal — one `beam_rt: unsupported: …` line on stderr, no
/// usage text, and nothing on stdout — and its reason names the Cargo feature the caller has to
/// rebuild with, which is the whole point of deciding this before any boot work. A process that
/// had booted first would have died per process on the missing backend and been re-wrapped into
/// an exit-1 diagnostic naming nothing.
#[cfg(not(feature = "cranelift"))]
#[test]
fn an_unbuilt_cranelift_request_is_an_unsupported_gap_with_exit_code_three() {
    let run = beam_rt(&fib_args(), Some("cranelift"));
    assert_eq!(
        run.code,
        beam_rt::cli::EXIT_UNSUPPORTED,
        "an unbuilt tier is a capability gap (3), not a runtime error (1): stderr={:?}",
        run.stderr
    );
    assert!(
        run.stdout.is_empty(),
        "nothing runs, so nothing is printed: {:?}",
        run.stdout
    );
    let unsupported = run
        .stderr
        .lines()
        .find(|line| line.starts_with("beam_rt: unsupported:"))
        .unwrap_or_else(|| {
            panic!(
                "the refusal is the stable unsupported signal, not a crash: {:?}",
                run.stderr
            )
        });
    assert!(
        run.stderr.lines().count() == 1,
        "exactly one diagnostic line, no usage text: {:?}",
        run.stderr
    );
    assert!(
        unsupported.contains(FEATURE),
        "the reason names the tier that is missing: {unsupported:?}"
    );
    assert!(
        unsupported.contains("Cargo feature"),
        "the reason names what to rebuild with: {unsupported:?}"
    );
    assert!(
        unsupported.contains("BEAM_RT_TIER"),
        "the reason names the setting that triggered it: {unsupported:?}"
    );
    assert!(
        !unsupported.contains("exit "),
        "the refusal is not the boot drive's re-wrapped exit diagnostic: {unsupported:?}"
    );
}

/// **Feature off: `-version` still exits 0 under an unbuilt-tier request.**
///
/// `-version` is the one action that runs no code, so it selects no tier (and boots nothing) —
/// the startup check is inside the same `!Version` guard the `-config` boot failure uses, so the
/// banner is unaffected. This is the row that keeps the check from becoming a blunt refusal of
/// the whole command line.
#[cfg(not(feature = "cranelift"))]
#[test]
fn version_still_answers_under_an_unbuilt_cranelift_request() {
    let run = beam_rt(&["-version"], Some("cranelift"));
    assert_eq!(run.code, 0, "stderr={:?}", run.stderr);
    assert!(
        run.stdout.contains("30.0-rc0") && run.stdout.contains("erts-17.1"),
        "the banner is printed: {:?}",
        run.stdout
    );
    assert!(
        run.stderr.is_empty(),
        "-version reports no diagnostic: {:?}",
        run.stderr
    );
}

/// **Every build: the startup check fires on nothing but an unbuilt `cranelift` request.**
///
/// The negative control for the row above — the unset default, the explicit `auto` and the
/// explicit `baseline` all select the native baseline tier, which every build serves, so the
/// slice still prints `55` and exits 0. If the check ever refused one of these, a working
/// invocation would be turned into a capability gap.
#[test]
fn the_served_tiers_still_run_the_slice() {
    for tier in [None, Some("auto"), Some("baseline"), Some("interpreter")] {
        let run = beam_rt(&fib_args(), tier);
        assert_eq!(
            run.code, 0,
            "BEAM_RT_TIER={tier:?} selects a served tier: stderr={:?}",
            run.stderr
        );
        assert_eq!(
            run.stdout, "55\n",
            "BEAM_RT_TIER={tier:?} computes fib(10): {:?}",
            run.stdout
        );
        assert!(
            run.stderr.is_empty(),
            "BEAM_RT_TIER={tier:?} is silent on stderr: {:?}",
            run.stderr
        );
    }
}

/// **Feature on: a built tier is never refused by the startup check.**
///
/// The check reads the same Cargo feature the engine's entry points do, so in a `cranelift` build
/// it answers `None` and the invocation proceeds — all the way to its own answer. A parked or
/// pre-empted process resumes **on the tier it was selected for**
/// (`Machine::resume_outcome_with_tier` dispatching to the Cranelift resume entry over the
/// backend's retained artifact store, an address that is not an enterable block start advancing
/// by the driver's per-instruction interpreter step), so the session completes rather than
/// reporting a gap. This row pins only the startup check's silence — the completion itself, on
/// all three tiers, is
/// [`every_served_tier_runs_the_cli_rows_identically`]'s subject.
#[cfg(feature = "cranelift")]
#[test]
fn a_built_cranelift_tier_is_not_refused_at_startup() {
    let run = beam_rt(&fib_args(), Some("cranelift"));
    assert!(
        !run.stderr.contains("rebuild with the `cranelift` Cargo feature"),
        "a `cranelift` build has the tier, so the startup check must not fire: {:?}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("beam_rt: unsupported: BEAM_RT_TIER=cranelift"),
        "the startup check's reason is absent in a build that serves the tier: {:?}",
        run.stderr
    );
}

/// **Every build: each served tier answers the three CLI rows identically.**
///
/// The rule is three-tier equivalence at the *CLI* boundary: for a pure computation, a process
/// parked on a finite `receive … after T`, and a two-process ping-pong that parks and resumes
/// between round trips, every tier this build serves must produce the fixture's own answer, an
/// exit code of 0, an empty stderr and no `unsupported` line anywhere. The interpreter is the
/// reference implementation, so its run of each row is captured first and every other tier is
/// compared to that run's stdout as well as to the fixed answer — a table alone cannot catch two
/// tiers agreeing on a wrong value.
///
/// The test is deliberately **not** feature-gated: one body executes in both builds. With the
/// `cranelift` feature off, [`served_tiers`] names only `interpreter`/`baseline`, so this is the
/// negative control (it never asks for a tier that build must refuse — the refusal has its own
/// feature-off rows above). With it on, the hot tier participates and `recv_after`/`pingpong`
/// make a resume part of the comparison; the in-process half of that property is
/// `src/runner/tests.rs`'s `a_preempted_process_resumes_on_its_cranelift_tier` and
/// `a_receive_parked_process_resumes_on_its_cranelift_tier`.
///
/// [`support::PROBE_TIMEOUT`] needs no tier awareness: measured on a dev-profile build on a
/// *shared* host, one `run` invocation costs ≈0.29–0.48 s on the interpreter tier, ≈0.34–0.55 s
/// on the baseline tier and ≈1.21–1.94 s on the Cranelift tier (the hot tier eagerly compiles
/// every module the boot script touches, so its figure is the whole session's) — the spread is
/// two measurements of the same binary under different host load, and even its top end sits
/// ~16× under the 30 s bound. Re-measure before tightening it: these are dev figures, and
/// absolute wall times on this host are not stable.
#[test]
fn every_served_tier_runs_the_cli_rows_identically() {
    for (label, args, expected) in equivalence_rows() {
        // The reference run first: the comparison is against a real execution of the same row.
        let reference = beam_rt(&args, Some("interpreter"));
        assert_eq!(
            reference.code, 0,
            "{label} must run on the reference interpreter tier: stdout={:?} stderr={:?}",
            reference.stdout, reference.stderr
        );
        assert_eq!(
            reference.stdout, expected,
            "{label} prints its fixture's answer on the interpreter tier: stderr={:?}",
            reference.stderr
        );
        assert!(
            reference.stderr.is_empty(),
            "{label} is silent on stderr on the interpreter tier: {:?}",
            reference.stderr
        );

        for tier in served_tiers() {
            let run = beam_rt(&args, Some(tier));
            assert_eq!(
                run.code, 0,
                "{label} must complete on the {tier} tier: stdout={:?} stderr={:?}",
                run.stdout, run.stderr
            );
            assert_eq!(
                run.stdout, expected,
                "{label} must print its fixture's answer on the {tier} tier: stderr={:?}",
                run.stderr
            );
            assert_eq!(
                run.stdout, reference.stdout,
                "{label} on the {tier} tier must agree with the interpreter reference: \
                 stderr={:?}",
                run.stderr
            );
            assert!(
                run.stderr.is_empty(),
                "{label} must be silent on stderr on the {tier} tier: {:?}",
                run.stderr
            );
            assert!(
                !run.stdout.contains("unsupported") && !run.stderr.contains("unsupported"),
                "{label} on the {tier} tier must not report a capability gap: \
                 stdout={:?} stderr={:?}",
                run.stdout,
                run.stderr
            );
        }
    }
}
