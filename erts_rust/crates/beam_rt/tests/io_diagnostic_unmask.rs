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

//! The **diagnostic-unmasking** probe: the installed compiler's own `catch`-handler
//! diagnostic reaches the output instead of being replaced by `calling_self`.
//!
//! # Why this is the point of the `io` floor
//!
//! Every pass of the installed OTP compiler reports through a `catch` handler of the shape
//!
//! ```erlang
//! catch
//!     Class:Error:Stack ->
//!         #b_local{name=#b_literal{val=Name},arity=Arity} = Id,
//!         io:fwrite("Function: ~w/~w\n", [Name,Arity]),
//!         erlang:raise(Class, Error, Stack)
//! ```
//!
//! (`lib/compiler/src/beam_ssa_opt.erl:190-201`, `beam_ssa_bool.erl:143-151`,
//! `beam_ssa_type.erl:227-235`, `beam_core_to_ssa.erl:270-278`, …). `io:fwrite/2` used to
//! execute `io.erl`'s body, whose `o_request/2` posts the request to the caller's group
//! leader — itself, for this runtime's own root process — and gets `{error, calling_self}`
//! back, so the diagnostic call itself raised and the compiler's real reason was **masked**
//! as `calling_self`. The `io` floor (`src/native/io.rs`) serves `io:fwrite/2` directly, so
//! the `Function: <name>/<arity>` line is printed and the real reason propagates.
//!
//! # What is asserted: the invariant, not the outcome
//!
//! A small set of real stdlib modules is compiled **in process** under the built `beam_rt`
//! binary ([`CARGO_BIN_EXE_beam_rt`]) with
//! `[no_spawn_compiler_process,deterministic,no_docs,return_errors,return_warnings,{outdir,D}]`,
//! and **for every module** the probe asserts the invariant this suite exists to pin:
//!
//! 1. **the combined stdout+stderr contains no `calling_self`** — the io-device failure is
//!    exactly what used to replace the compiler's real reason, so its presence anywhere in a
//!    compiler run is the regression. This is the one thing that must **never** appear;
//! 2. the run rode a real in-process compile to a **reported** outcome — the probe's own
//!    `RETSAW` line names either `{ok, Module, …}` or `{error, …}` (see
//!    [`reported_outcome`]) — so the "no `calling_self`" claim is about a compile that
//!    actually ran rather than a probe that died early;
//! 3. **conditionally on that outcome being `{error, …}`**, a `Function: ` diagnostic line
//!    was printed. Only a *failing* compile has a `catch` handler to report, so this evidence
//!    is deliberately conditional: a module compiling cleanly prints no diagnostic and the
//!    check is simply skipped for it.
//!
//! Each module's source is also compiled by the **live oracle**, and the oracle must report
//! `{ok, Module, …}`: the inputs are demonstrably compilable, so a rust-side `{error, …}` is a
//! residual of this runtime rather than a broken input. A missing `erl` is a **loud** skip,
//! never a silent pass, and the oracle is discovered by filesystem only
//! (`tests/support/mod.rs`).
//!
//! # Why an invariant and not a fixed outcome
//!
//! Both outcomes are legitimate here, because this runtime's in-process compile is *not* yet
//! oracle-complete: `lib/stdlib/src/erl_tar.erl` currently fails in
//! `beam_core_to_ssa:function/2` (`{crash,core_to_ssa,badarg,…}`) — a **separate,
//! out-of-scope defect** in the compiler-runtime surface (a `maps:*` iteration-term
//! mis-rejection owned by `crates/beam_bifs`), *not* an io defect. An earlier version of this
//! probe asserted the `Function: ` line **unconditionally**, i.e. it depended on that foreign
//! defect still being present and would have started failing the moment it was fixed —
//! blocking the agent fixing it. The assertion is therefore re-pointed at the invariant the
//! `io` floor is responsible for, over a set that **spans both outcomes** today
//! ([`PROBED`]): at least one module that compiles cleanly on both runtimes, and at least one
//! that currently fails here. Whichever way the out-of-scope residual moves — closed entirely
//! (both modules compile), partially (one of each), or with the failure re-appearing elsewhere
//! — this probe keeps pinning "the compiler's diagnostics are not replaced by
//! `calling_self`" without constraining the outcome.
//!
//! # The probe budget
//!
//! A single in-process compile must stay well inside the shared
//! `support::PROBE_TIMEOUT` (30 s), because a probe killed at the bound is a test failure.
//! Measured on this host against a debug `beam_rt`: `math.erl` ≈ 0.9 s, `ordsets.erl` ≈ 1.2 s,
//! `erl_tar.erl` ≈ 4.8 s — whereas `string.erl` takes ≈ 20 s and `erl_lint.erl` ≈ 23 s, i.e.
//! margins too thin for a loaded CI host (they carry the same invariant but are not worth the
//! risk). The compilable module in [`PROBED`] is therefore one of the small ones, not
//! `string.erl`/`json.erl`.

// The shared harness is compiled in whole, but this suite uses only its bounded runner and its
// toolchain discovery; the unused remainder is expected here — the same shape
// `tests/compile_file.rs` uses.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The probe's own marker, so the outcome assertion cannot be satisfied by an unrelated line.
const MARKER: &str = "RETSAW";

/// The diagnostic every crashing compiler pass prints from its `catch` handler
/// (`io:fwrite("Function: ~w/~w\n", [Name, Arity])`).
const DIAGNOSTIC: &str = "Function: ";

/// The io-device failure the `io` floor removed: the compiler's diagnostic call used to raise
/// it and thereby *replace* the pass's real reason. Its appearance anywhere in a compiler run
/// is the regression this suite pins.
const MASKING_REASON: &str = "calling_self";

// ---------------------------------------------------------------------------------------
// The probed modules
// ---------------------------------------------------------------------------------------

/// One probed stdlib module, with the reason it belongs to the set.
struct Probed {
    /// The source path relative to `$ERL_TOP`.
    source: &'static str,
    /// The module name `compile:file/2` reports in its result tuple.
    module: &'static str,
    /// What is known about this module's outcome today.
    today: &'static str,
}

/// **The probed set — at least one module per outcome.** It must span a module that compiles
/// cleanly on both runtimes *and* a module that currently fails here, so the invariant holds
/// whichever way the out-of-scope `maps:*` residual moves (see the module docs).
const PROBED: &[Probed] = &[
    Probed {
        source: "lib/stdlib/src/ordsets.erl",
        module: "ordsets",
        today: "compiles cleanly under both runtimes today (~1.2 s in process here) — its job is \
                to show the probe rides the compiler to a real reported result with no diagnostic \
                to print",
    },
    Probed {
        source: "lib/stdlib/src/erl_tar.erl",
        module: "erl_tar",
        today: "compiles on the oracle but NOT yet under this runtime (~4.8 s in process here; it \
                crashes in `beam_core_to_ssa:function/2` because of the separate, out-of-scope \
                `maps:*` iterator-term defect in `crates/beam_bifs`) — its job is to show a \
                compiler pass's own `Function: <name>/<arity>` diagnostic reaches the output",
    },
];

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and its raw stdout/stderr bytes.
struct Run {
    code: i32,
    out: Vec<u8>,
    err: Vec<u8>,
}

impl Run {
    /// stdout as a lossy string (the compiler's diagnostics are ASCII).
    fn out_str(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    /// stdout and stderr as one lossy string — the diagnostic print and a CLI diagnostic may
    /// land on either stream, and the assertion is about **content**, not about which stream
    /// carried it.
    fn combined(&self) -> String {
        let mut all = self.out_str();
        all.push_str(&String::from_utf8_lossy(&self.err));
        all
    }
}

/// Runs `program args` under the shared bounded runner (`/dev/null` stdin, killed at
/// `support::PROBE_TIMEOUT`), failing loudly when the probe could not be run at all.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: output.stdout,
        err: output.stderr,
    }
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// `erl -noshell -eval <expr> -s init stop` through the oracle (bounded: the node always gets
/// a terminator, and the shared runner kills it at the bound).
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    run(oracle, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

// ---------------------------------------------------------------------------------------
// Temporary directories, Erlang literals and the source tree
// ---------------------------------------------------------------------------------------

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_io_diagnostic_<pid>_<n>`, unique per run.
    fn new() -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_io_diagnostic_{}_{unique}",
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

/// The **Erlang string literal** for `path`: the temporary names this suite builds are ASCII,
/// which Rust's `Debug` and Erlang's string syntax quote identically.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII temporary path"))
}

/// The OTP source tree (`$ERL_TOP`, else the workspace root three levels up from this crate).
fn source_root() -> PathBuf {
    match std::env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

// ---------------------------------------------------------------------------------------
// The probe and its outcome
// ---------------------------------------------------------------------------------------

/// `compile:file(Source, [no_spawn_compiler_process, deterministic, no_docs, return_errors,
/// return_warnings, {outdir, Dir}])`, then the result — the same option list the oracle is
/// given, so the only difference between the two runs is the runtime.
///
/// `MARKER` is the probe's own marker so the outcome assertion cannot be satisfied by an
/// unrelated line. `no_docs` keeps a real module's artifact independent of the `Docs` chunk and
/// `return_errors`/`return_warnings` make the compiler **return** the failure instead of
/// exiting, so the probe always reaches its `halt(0)`.
fn compile_probe(src: &Path, outdir: &Path) -> String {
    format!(
        "R = compile:file({}, [no_spawn_compiler_process, deterministic, no_docs, \
         return_errors, return_warnings, {{outdir, {}}}]),\n\
         io:format(\"{MARKER} ~p~n\", [R]),\n\
         halt(0).",
        literal(src),
        literal(outdir)
    )
}

/// How the in-process compile ended, as reported by the probe's own `MARKER` line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    /// `{ok, Module, …}` — the module compiled (with or without warnings).
    Compiled,
    /// `{error, …}` — the compiler reported a failure (the real reason, not a masked one).
    Errored,
}

/// Classifies the probe's result line for `module`, or `None` when the probe never reported
/// one — the case in which the "no `calling_self`" claim would be vacuous and the probe must
/// fail loudly instead.
///
/// Both outcomes are accepted **by design** (see the module docs): only the `calling_self`
/// invariant is fixed.
fn reported_outcome(text: &str, module: &str) -> Option<Outcome> {
    if text.contains(&format!("{MARKER} {{ok,{module},")) {
        Some(Outcome::Compiled)
    } else if text.contains(&format!("{MARKER} {{error")) {
        Some(Outcome::Errored)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------
// The probe
// ---------------------------------------------------------------------------------------

/// **The diagnostic-unmasking probe.** For every module in [`PROBED`] — a set spanning a
/// cleanly-compiling module and one that currently fails here — the oracle must compile it,
/// and this runtime's in-process compile must report a real outcome while **never** producing
/// `calling_self`. See the module docs for why the invariant, not the outcome, is asserted.
#[test]
fn the_compilers_own_diagnostic_is_not_masked_as_calling_self() {
    let Some(oracle) = support::require_oracle("io_diagnostic") else {
        return;
    };
    let root = source_root();

    // Discover every probed source up front: a missing one is a **loud** skip, never a silent
    // pass (and never a mid-set partial run presented as a success).
    let sources: Vec<PathBuf> = PROBED.iter().map(|probe| root.join(probe.source)).collect();
    for (probe, src) in PROBED.iter().zip(&sources) {
        if !src.is_file() {
            support::skip_loudly(
                "io_diagnostic",
                &format!(
                    "no real stdlib source to compile at {} — this module is in the probed set \
                     because it {}",
                    src.display(),
                    probe.today
                ),
            );
            return;
        }
    }

    let dir = TempDir::new();
    for (probe, src) in PROBED.iter().zip(&sources) {
        let rust_out = dir.0.join(format!("rust_{}", probe.module));
        let oracle_out = dir.0.join(format!("oracle_{}", probe.module));
        // `compile` does not create the output directory.
        for out in [&rust_out, &oracle_out] {
            fs::create_dir_all(out).expect("the output directory");
        }

        // ---- the oracle: the source really is compilable ----
        let theirs = oracle_eval(&oracle, &compile_probe(src, &oracle_out));
        assert_eq!(
            theirs.code, 0,
            "{}: the oracle must complete the probe: exit {} — {}",
            probe.source,
            theirs.code,
            theirs.combined()
        );
        assert!(
            theirs
                .out_str()
                .starts_with(&format!("{MARKER} {{ok,{},", probe.module)),
            "{}: the source must be compilable on the oracle, so a rust-side `{{error, …}}` is a \
             residual of this runtime rather than a broken input — the oracle printed {:?}",
            probe.source,
            theirs.out_str()
        );

        // ---- this runtime: the probe must reach a reported outcome, with no masked reason ----
        let ours = beam_rt_eval(&compile_probe(src, &rust_out));
        assert_eq!(
            ours.code, 0,
            "{}: the probe must reach its own halt(0) — exit {} — {}",
            probe.source,
            ours.code,
            ours.combined()
        );
        let combined = ours.combined();
        eprintln!(
            "in-process compile of {} under beam_rt printed:\n{combined}",
            probe.source
        );

        // (1) THE INVARIANT. `calling_self` is exactly what the compiler's diagnostic call
        // used to raise, replacing the pass's real reason with its own.
        assert!(
            !combined.contains(MASKING_REASON),
            "{}: the compiler's diagnostic call must not raise `{MASKING_REASON}` — the io \
             device floor serves `io:fwrite/2` natively, so a compiler pass's diagnostic print \
             reaches the output and its real reason propagates. Captured output: {:?}",
            probe.source,
            combined
        );

        // (2) The run really rode a compile to a reported result — either outcome is fine, but
        // a run that reported neither would make the invariant above vacuous. `today`: {}.
        let Some(outcome) = reported_outcome(&combined, probe.module) else {
            panic!(
                "{}: the in-process compile must report `{MARKER} {{ok,{},…}}` or \
                 `{MARKER} {{error, …}}` so the `{MASKING_REASON}` check is not vacuous — it \
                 reported neither, which means `compile:file/2` never produced a result here. \
                 Captured output: {:?}",
                probe.source, probe.module, combined
            );
        };

        // (3) CONDITIONAL on a failure: the compiler's own `Function: <name>/<arity>` line
        // must have been printed. A cleanly compiling module has no crash for a pass's `catch`
        // handler to report, so nothing is required of it (and nothing is asserted).
        if outcome == Outcome::Errored {
            assert!(
                combined.contains(DIAGNOSTIC),
                "{}: the compile failed, so a compiler pass's own `{DIAGNOSTIC}<name>/<arity>` \
                 diagnostic must reach the output — it did not. Captured output: {:?}",
                probe.source,
                combined
            );
        }
    }
}
